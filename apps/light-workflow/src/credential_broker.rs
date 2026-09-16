//! Workflow-owned renewal. A durable claim precedes the single issuer request;
//! an abandoned claim is fenced and revoked, never replayed with its old token.
use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, AeadCore, OsRng, Payload},
};
use light_client::unattended::{ProviderFailure, RenewedCredential, UnattendedProvider};
use oauth_workflow_contract::IssuerGrant;
use serde_json::Value;
use sqlx::PgPool;
use std::{collections::BTreeMap, sync::Arc};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrokerSettings {
    pub callback_uri: String,
    #[serde(default)]
    pub legacy_long_lived_app_keys: Vec<light_security::token_purpose::LegacyLongLivedAppKey>,
    #[serde(default)]
    pub callback_tls: Option<CallbackTls>,
    pub database_url_file: std::path::PathBuf,
    pub keyring_file: std::path::PathBuf,
    pub provider: light_client::unattended::UnattendedProviderConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CallbackTls {
    pub address: std::net::SocketAddr,
    pub certificate_file: std::path::PathBuf,
    pub private_key_file: std::path::PathBuf,
}

/// Keyring contents are loaded only from the private mount and never become
/// part of runtime configuration, provenance, or diagnostic serialization.
pub async fn open(
    settings: &BrokerSettings,
    dir: &std::path::Path,
) -> Result<CredentialBroker, BrokerError> {
    use base64::Engine as _;
    let callback = url::Url::parse(&settings.callback_uri).map_err(|_| BrokerError::Evidence)?;
    if callback.scheme() != "https"
        || callback.host_str().is_none()
        || !callback.username().is_empty()
        || callback.password().is_some()
        || callback.fragment().is_some()
    {
        return Err(BrokerError::Evidence);
    }
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct Keyring {
        active_key_id: String,
        keys: BTreeMap<String, String>,
    }
    let bytes = tokio::fs::read(dir.join(&settings.keyring_file))
        .await
        .map_err(|_| BrokerError::Store)?;
    let ring: Keyring = serde_json::from_slice(&bytes).map_err(|_| BrokerError::Evidence)?;
    let keys = ring
        .keys
        .into_iter()
        .map(|(id, value)| {
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(value)
                .map_err(|_| BrokerError::Evidence)?;
            Ok((id, decoded.try_into().map_err(|_| BrokerError::Evidence)?))
        })
        .collect::<Result<BTreeMap<String, [u8; 32]>, BrokerError>>()?;
    let keys = CredentialKeys::new(ring.active_key_id, keys)?;
    let url = tokio::fs::read_to_string(dir.join(&settings.database_url_file))
        .await
        .map_err(|_| BrokerError::Store)?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(url.trim())
        .await
        .map_err(|_| BrokerError::Store)?;
    sqlx::query("SELECT grant_id FROM workflow_secret.grant_t LIMIT 0")
        .execute(&pool)
        .await
        .map_err(|_| BrokerError::Store)?;
    let provider = UnattendedProvider::new(settings.provider.clone(), dir)
        .await
        .map_err(|_| BrokerError::Evidence)?;
    // A store with pre-existing unbound records cannot prove which issuer
    // client owns them. Do not adopt or reconcile those records implicitly.
    sqlx::query("INSERT INTO workflow_secret.identity_t(singleton,issuer,client_id,token_url) SELECT true,$1,$2,$3 WHERE NOT EXISTS(SELECT 1 FROM workflow_secret.enrollment_t) AND NOT EXISTS(SELECT 1 FROM workflow_secret.grant_t) ON CONFLICT DO NOTHING")
        .bind(&settings.provider.issuer).bind(&settings.provider.client_id).bind(&settings.provider.token_url)
        .execute(&pool).await.map_err(|_| BrokerError::Store)?;
    let matches: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workflow_secret.identity_t WHERE singleton AND issuer=$1 AND client_id=$2 AND token_url=$3)")
        .bind(&settings.provider.issuer).bind(&settings.provider.client_id).bind(&settings.provider.token_url)
        .fetch_one(&pool).await.map_err(|_| BrokerError::Store)?;
    if !matches {
        return Err(BrokerError::Evidence);
    }
    Ok(CredentialBroker::new(pool, Arc::new(provider), keys))
}

#[derive(Debug, thiserror::Error)]
pub enum BrokerError {
    #[error("credential store unavailable")]
    Store,
    #[error("credential evidence rejected")]
    Evidence,
    #[error("renewal already in progress")]
    Busy,
    #[error("user reauthorization required")]
    Reauthorize,
    #[error("issuer request not sent; retry later")]
    Retryable,
    #[error("requesting run is inactive or expired")]
    RunInactive,
}

/// Keys come from a private secret mount. No key or credential Debug output.
pub struct CredentialKeys {
    active: String,
    keys: BTreeMap<String, [u8; 32]>,
}
impl CredentialKeys {
    pub fn new(active: String, keys: BTreeMap<String, [u8; 32]>) -> Result<Self, BrokerError> {
        if !keys.contains_key(&active) || active.is_empty() {
            return Err(BrokerError::Evidence);
        }
        Ok(Self { active, keys })
    }
    fn seal(&self, grant: Uuid, generation: i64, secret: &str) -> Result<Vec<u8>, BrokerError> {
        let cipher = Aes256Gcm::new_from_slice(&self.keys[&self.active])
            .map_err(|_| BrokerError::Evidence)?;
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let aad = format!("workflow-refresh:{grant}:{generation}:{}", self.active);
        let encrypted = cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: secret.as_bytes(),
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| BrokerError::Evidence)?;
        Ok([nonce.as_slice(), encrypted.as_slice()].concat())
    }
    fn open(
        &self,
        key: &str,
        grant: Uuid,
        generation: i64,
        bytes: &[u8],
    ) -> Result<String, BrokerError> {
        if bytes.len() < 28 {
            return Err(BrokerError::Evidence);
        }
        let cipher = Aes256Gcm::new_from_slice(self.keys.get(key).ok_or(BrokerError::Evidence)?)
            .map_err(|_| BrokerError::Evidence)?;
        let aad = format!("workflow-refresh:{grant}:{generation}:{key}");
        let plain = cipher
            .decrypt(
                Nonce::from_slice(&bytes[..12]),
                Payload {
                    msg: &bytes[12..],
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| BrokerError::Evidence)?;
        String::from_utf8(plain).map_err(|_| BrokerError::Evidence)
    }
}

pub struct CredentialBroker {
    pool: PgPool,
    provider: Arc<UnattendedProvider>,
    keys: CredentialKeys,
    boot: Uuid,
}

/// Safe to return to the browser: no refresh token or PKCE verifier.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrollmentChallenge {
    pub authorization_url: String,
    pub enrollment_id: Uuid,
    pub state: String,
    pub code_challenge: String,
}

impl CredentialBroker {
    /// Acquire a broker-bound credential from existing Portal scope authorization.
    /// The one-time code and PKCE material never leave this backend coordinator.
    pub async fn acquire_for_user(
        &self,
        authorization: &str,
        host: Uuid,
        user: Uuid,
        callback: &str,
        scope: &str,
        binding: Value,
        expires: chrono::DateTime<chrono::Utc>,
    ) -> Result<Uuid, BrokerError> {
        use aes_gcm::aead::rand_core::RngCore;
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
        use sha2::{Digest, Sha256};
        if !binding.is_object()
            || binding.as_object().is_some_and(|b| b.is_empty())
            || scope.trim().is_empty()
            || expires <= chrono::Utc::now()
        {
            return Err(BrokerError::Evidence);
        }
        let mut random = [0u8; 32];
        OsRng.fill_bytes(&mut random);
        let verifier = URL_SAFE_NO_PAD.encode(random);
        OsRng.fill_bytes(&mut random);
        let state = URL_SAFE_NO_PAD.encode(random);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let id = Uuid::new_v4();
        let sealed = self.keys.seal(id, 0, &verifier)?;
        let hash = hex::encode(Sha256::digest(state.as_bytes()));
        sqlx::query("INSERT INTO workflow_secret.enrollment_t
          (enrollment_id,host_id,user_id,state_hash,key_id,ciphertext,callback_uri,scope,binding,grant_expires_at,expires_at,state)
          VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,clock_timestamp()+interval '5 minutes','PREPARING')")
            .bind(id).bind(host).bind(user).bind(hash).bind(&self.keys.active).bind(sealed).bind(callback)
            .bind(scope).bind(&binding).bind(expires).execute(&self.pool).await.map_err(|_|BrokerError::Store)?;
        let code = self.provider.acquire_code(authorization, &serde_json::json!({
            "clientId":self.provider.client_id(),"enrollmentId":id,"hostId":host,"codeChallenge":challenge,
            "callbackUri":callback,"state":state,"scope":scope,"binding":binding,"expiresAt":expires
        })).await;
        let code = match code {
            Ok(code) => code,
            Err(_) => {
                sqlx::query("UPDATE workflow_secret.enrollment_t SET state='REAUTHORIZATION_REQUIRED',revocation_pending=true WHERE enrollment_id=$1")
                    .bind(id).execute(&self.pool).await.map_err(|_|BrokerError::Store)?;
                return Err(BrokerError::Reauthorize);
            }
        };
        sqlx::query("UPDATE workflow_secret.enrollment_t SET state='READY' WHERE enrollment_id=$1 AND state='PREPARING'")
            .bind(id).execute(&self.pool).await.map_err(|_|BrokerError::Store)?;
        self.complete_enrollment(&state, &code, host, user).await
    }

    /// Revoke durably before contacting the issuer. Failure to reach the issuer
    /// leaves a recoverable revocation obligation and never reopens local use.
    pub async fn revoke_grant(
        &self,
        grant: Uuid,
        host: Uuid,
        user: Uuid,
    ) -> Result<(), BrokerError> {
        let mut tx = self.pool.begin().await.map_err(|_| BrokerError::Store)?;
        let changed = sqlx::query("UPDATE workflow_secret.grant_t SET state='REVOKED',revocation_pending=true,renewal_id=NULL,owner_boot=NULL,renewal_deadline=NULL WHERE grant_id=$1 AND host_id=$2 AND user_id=$3")
            .bind(grant).bind(host).bind(user).execute(&mut *tx).await.map_err(|_| BrokerError::Store)?.rows_affected();
        if changed != 1 {
            return Err(BrokerError::Evidence);
        }
        sqlx::query("UPDATE workflow_secret.run_t SET active=false WHERE grant_id=$1")
            .bind(grant)
            .execute(&mut *tx)
            .await
            .map_err(|_| BrokerError::Store)?;
        sqlx::query("UPDATE workflow_secret.renewal_t SET result='REAUTHORIZATION_REQUIRED',finished_at=clock_timestamp() WHERE grant_id=$1 AND result IS NULL")
            .bind(grant).execute(&mut *tx).await.map_err(|_| BrokerError::Store)?;
        tx.commit().await.map_err(|_| BrokerError::Store)?;
        self.reconcile_revocations().await
    }

    /// OAuth state is unguessable and bound to the stored initiator; the code
    /// must additionally redeem using the backend-only PKCE verifier.
    pub async fn complete_browser_callback(
        &self,
        state: &str,
        code: &str,
    ) -> Result<Uuid, BrokerError> {
        use sha2::{Digest, Sha256};
        if state.len() != 43 || code.len() > 512 {
            return Err(BrokerError::Evidence);
        }
        let hash = hex::encode(Sha256::digest(state.as_bytes()));
        let (host,user)=sqlx::query_as::<_,(Uuid,Uuid)>("SELECT host_id,user_id FROM workflow_secret.enrollment_t WHERE state_hash=$1 AND state='READY' AND expires_at>clock_timestamp()")
            .bind(hash).fetch_optional(&self.pool).await.map_err(|_|BrokerError::Store)?.ok_or(BrokerError::Reauthorize)?;
        self.complete_enrollment(state, code, host, user).await
    }
    /// The issuer independently validates the original user token, tenant and
    /// client ceiling. Its login step records explicit consent before redemption.
    pub async fn begin_enrollment(
        &self,
        user_authorization: &str,
        host: Uuid,
        user: Uuid,
        callback: &str,
        scope: &str,
        binding: Value,
        expires: chrono::DateTime<chrono::Utc>,
    ) -> Result<EnrollmentChallenge, BrokerError> {
        use aes_gcm::aead::rand_core::RngCore;
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
        use sha2::{Digest, Sha256};
        if !binding.is_object() || binding.as_object().is_some_and(|b| b.is_empty()) {
            return Err(BrokerError::Evidence);
        }
        let mut random = [0u8; 32];
        OsRng.fill_bytes(&mut random);
        let verifier = URL_SAFE_NO_PAD.encode(random);
        OsRng.fill_bytes(&mut random);
        let state = URL_SAFE_NO_PAD.encode(random);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let id = Uuid::new_v4();
        let sealed = self.keys.seal(id, 0, &verifier)?;
        let hash = hex::encode(Sha256::digest(state.as_bytes()));
        sqlx::query("INSERT INTO workflow_secret.enrollment_t
          (enrollment_id,host_id,user_id,state_hash,key_id,ciphertext,callback_uri,scope,binding,grant_expires_at,expires_at,state)
          VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,clock_timestamp()+interval '5 minutes','PREPARING')")
            .bind(id).bind(host).bind(user).bind(hash).bind(&self.keys.active).bind(sealed).bind(callback)
            .bind(scope).bind(&binding).bind(expires).execute(&self.pool).await.map_err(|_|BrokerError::Store)?;
        let result=self.provider.begin_enrollment(user_authorization,&serde_json::json!({
            "clientId":self.provider.client_id(),"enrollmentId":id,"hostId":host,"codeChallenge":challenge,
            "callbackUri":callback,"state":state,"scope":scope,"binding":binding,"expiresAt":expires
        })).await;
        if result.is_err() {
            sqlx::query("UPDATE workflow_secret.enrollment_t SET state='REAUTHORIZATION_REQUIRED',revocation_pending=true WHERE enrollment_id=$1")
                .bind(id).execute(&self.pool).await.map_err(|_|BrokerError::Store)?;
            return Err(BrokerError::Reauthorize);
        }
        sqlx::query("UPDATE workflow_secret.enrollment_t SET state='READY' WHERE enrollment_id=$1 AND state='PREPARING'")
            .bind(id).execute(&self.pool).await.map_err(|_|BrokerError::Store)?;
        Ok(EnrollmentChallenge {
            authorization_url: self.provider.authorization_url(id, &state),
            enrollment_id: id,
            state,
            code_challenge: challenge,
        })
    }

    pub async fn complete_enrollment(
        &self,
        state: &str,
        code: &str,
        host: Uuid,
        user: Uuid,
    ) -> Result<Uuid, BrokerError> {
        use sha2::{Digest, Sha256};
        let hash = hex::encode(Sha256::digest(state.as_bytes()));
        let row=sqlx::query_as::<_,(Uuid,String,Vec<u8>,String,String,Value,chrono::DateTime<chrono::Utc>)>(
            "UPDATE workflow_secret.enrollment_t SET state='REDEEMING',owner_boot=$4
             WHERE state_hash=$1 AND host_id=$2 AND user_id=$3 AND state='READY' AND expires_at>clock_timestamp()
             RETURNING enrollment_id,key_id,ciphertext,callback_uri,scope,binding,grant_expires_at")
            .bind(hash).bind(host).bind(user).bind(self.boot).fetch_optional(&self.pool).await.map_err(|_|BrokerError::Store)?
            .ok_or(BrokerError::Reauthorize)?;
        let (id, key, ciphertext, callback, scope, binding, expires) = row;
        let verifier = self.keys.open(&key, id, 0, &ciphertext)?;
        let response = self.provider.redeem(code, &verifier, &callback).await;
        let credential = match response {
            Err(ProviderFailure::NotSent) => {
                let changed = sqlx::query("UPDATE workflow_secret.enrollment_t SET state='READY',owner_boot=NULL
                    WHERE enrollment_id=$1 AND state='REDEEMING' AND owner_boot=$2 AND expires_at>clock_timestamp() AND NOT revocation_pending")
                    .bind(id).bind(self.boot).execute(&self.pool).await.map_err(|_| BrokerError::Store)?.rows_affected();
                return Err(if changed == 1 {
                    BrokerError::Retryable
                } else {
                    BrokerError::Reauthorize
                });
            }
            Ok(c)
                if c.issuer_grant.grant_id == id
                    && c.issuer_grant.host_id == host
                    && c.issuer_grant.user_id == user
                    && c.issuer_grant.binding == binding
                    && c.issuer_grant.scope == scope
                    && c.issuer_grant.expires_at == expires =>
            {
                c
            }
            _ => {
                sqlx::query("UPDATE workflow_secret.enrollment_t SET state='REAUTHORIZATION_REQUIRED',revocation_pending=true WHERE enrollment_id=$1")
                    .bind(id).execute(&self.pool).await.map_err(|_|BrokerError::Store)?;
                self.provider.revoke(&id.to_string()).await.ok();
                return Err(BrokerError::Reauthorize);
            }
        };
        let mut tx = self.pool.begin().await.map_err(|_| BrokerError::Store)?;
        let changed=sqlx::query("UPDATE workflow_secret.enrollment_t SET state='COMPLETED',ciphertext=''::bytea
          WHERE enrollment_id=$1 AND state='REDEEMING' AND owner_boot=$2 AND expires_at>clock_timestamp()")
            .bind(id).bind(self.boot).execute(&mut *tx).await.map_err(|_|BrokerError::Store)?.rows_affected();
        if changed != 1 {
            drop(tx);
            self.provider.revoke(&id.to_string()).await.ok();
            return Err(BrokerError::Reauthorize);
        }
        self.store_enrolled(&credential, &mut tx).await?;
        tx.commit().await.map_err(|_| BrokerError::Store)?;
        Ok(id)
    }
}

impl CredentialBroker {
    pub fn new(pool: PgPool, provider: Arc<UnattendedProvider>, keys: CredentialKeys) -> Self {
        Self {
            pool,
            provider,
            keys,
            boot: Uuid::new_v4(),
        }
    }

    /// Called only after the enrollment coordinator validates the expected
    /// issuer grant against its persisted consent and PKCE enrollment record.
    async fn store_enrolled(
        &self,
        credential: &RenewedCredential,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<(), BrokerError> {
        let g = &credential.issuer_grant;
        if g.status != "ACTIVE"
            || g.generation != 1
            || g.client_id.to_string() != self.provider.client_id()
        {
            return Err(BrokerError::Evidence);
        }
        let ciphertext = self
            .keys
            .seal(g.grant_id, g.generation, &credential.refresh_token)?;
        sqlx::query("INSERT INTO workflow_secret.grant_t
          (grant_id,host_id,user_id,binding,issuer_grant,generation,expires_at,state,key_id,ciphertext)
          VALUES($1,$2,$3,$4,$5,$6,$7,'ACTIVE',$8,$9)")
            .bind(g.grant_id).bind(g.host_id).bind(g.user_id).bind(&g.binding)
            .bind(serde_json::to_value(g).map_err(|_|BrokerError::Evidence)?)
            .bind(g.generation).bind(g.expires_at).bind(&self.keys.active).bind(ciphertext)
            .execute(&mut **tx).await.map_err(|_|BrokerError::Store)?;
        Ok(())
    }

    /// Only the trusted run-admission path may call this after authorizing the
    /// run definition. No caller-supplied grant reference alone grants access.
    pub async fn bind_run(
        &self,
        run: Uuid,
        grant: Uuid,
        host: Uuid,
        user: Uuid,
        binding: &Value,
        expires: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), BrokerError> {
        let changed = sqlx::query(
            "INSERT INTO workflow_secret.run_t
          (run_id,grant_id,host_id,user_id,binding,expires_at)
          SELECT $1,grant_id,host_id,user_id,binding,$6 FROM workflow_secret.grant_t
          WHERE grant_id=$2 AND host_id=$3 AND user_id=$4 AND binding=$5
            AND state='ACTIVE' AND expires_at >= $6 AND $6 > clock_timestamp()
          ON CONFLICT(run_id) DO UPDATE SET run_id=EXCLUDED.run_id
          WHERE workflow_secret.run_t.grant_id=EXCLUDED.grant_id
            AND workflow_secret.run_t.host_id=EXCLUDED.host_id AND workflow_secret.run_t.user_id=EXCLUDED.user_id
            AND workflow_secret.run_t.binding=EXCLUDED.binding AND workflow_secret.run_t.active
            AND workflow_secret.run_t.expires_at=EXCLUDED.expires_at",
        )
        .bind(run)
        .bind(grant)
        .bind(host)
        .bind(user)
        .bind(binding)
        .bind(expires)
        .execute(&self.pool)
        .await
        .map_err(|_| BrokerError::Store)?
        .rows_affected();
        if changed != 1 {
            return Err(BrokerError::Evidence);
        }
        Ok(())
    }

    /// Bind a child to the parent's already accepted consent evidence. The
    /// child publication is authorized by the stored parent action; it cannot
    /// substitute another grant, tenant or user binding.
    pub async fn inherit_run(
        &self,
        parent_run: Uuid,
        child_run: Uuid,
        host: Uuid,
        user: Uuid,
        expires: chrono::DateTime<chrono::Utc>,
    ) -> Result<(), BrokerError> {
        let changed = sqlx::query(
            "INSERT INTO workflow_secret.run_t(run_id,grant_id,host_id,user_id,binding,expires_at)
             SELECT $2,r.grant_id,r.host_id,r.user_id,r.binding,LEAST($5,r.expires_at,g.expires_at)
             FROM workflow_secret.run_t r JOIN workflow_secret.grant_t g USING(grant_id)
             WHERE r.run_id=$1 AND r.host_id=$3 AND r.user_id=$4 AND r.active
               AND r.expires_at>clock_timestamp() AND g.state='ACTIVE' AND g.expires_at>clock_timestamp()
               AND $5>clock_timestamp()
             ON CONFLICT(run_id) DO UPDATE SET run_id=EXCLUDED.run_id
             WHERE workflow_secret.run_t.grant_id=EXCLUDED.grant_id
               AND workflow_secret.run_t.host_id=EXCLUDED.host_id
               AND workflow_secret.run_t.user_id=EXCLUDED.user_id
               AND workflow_secret.run_t.binding=EXCLUDED.binding
               AND workflow_secret.run_t.expires_at=EXCLUDED.expires_at
               AND workflow_secret.run_t.active",
        )
        .bind(parent_run)
        .bind(child_run)
        .bind(host)
        .bind(user)
        .bind(expires)
        .execute(&self.pool)
        .await
        .map_err(|_| BrokerError::Store)?
        .rows_affected();
        if changed != 1 {
            return Err(BrokerError::Evidence);
        }
        Ok(())
    }

    pub async fn renew_for_run(
        &self,
        run: Uuid,
        host: Uuid,
        user: Uuid,
    ) -> Result<RenewedCredential, BrokerError> {
        let mut tx = self.pool.begin().await.map_err(|_| BrokerError::Store)?;
        let row=sqlx::query_as::<_,(Uuid,i64,String,Vec<u8>,Value,String)>(
            "SELECT g.grant_id,g.generation,g.key_id,g.ciphertext,g.issuer_grant,g.state
             FROM workflow_secret.grant_t g JOIN workflow_secret.run_t r USING(grant_id)
             WHERE r.run_id=$1 AND r.host_id=$2 AND r.user_id=$3 AND r.active
               AND r.expires_at>clock_timestamp() AND g.expires_at>clock_timestamp()
               AND r.binding=g.binding AND r.host_id=g.host_id AND r.user_id=g.user_id FOR UPDATE OF g")
            .bind(run).bind(host).bind(user).fetch_optional(&mut *tx).await.map_err(|_|BrokerError::Store)?
            .ok_or(BrokerError::Reauthorize)?;
        let (grant, generation, key, ciphertext, evidence, state) = row;
        if state == "RENEWING" {
            return Err(BrokerError::Busy);
        }
        if state != "ACTIVE" {
            return Err(BrokerError::Reauthorize);
        }
        let prior: IssuerGrant =
            serde_json::from_value(evidence).map_err(|_| BrokerError::Evidence)?;
        if prior.grant_id != grant
            || prior.host_id != host
            || prior.user_id != user
            || prior.generation != generation
            || prior.status != "ACTIVE"
        {
            return Err(BrokerError::Evidence);
        }
        let refresh = self.keys.open(&key, grant, generation, &ciphertext)?;
        let attempt = Uuid::new_v4();
        sqlx::query(
            "UPDATE workflow_secret.grant_t SET state='RENEWING',renewal_id=$2,owner_boot=$3,
          renewal_deadline=clock_timestamp()+interval '30 seconds' WHERE grant_id=$1",
        )
        .bind(grant)
        .bind(attempt)
        .bind(self.boot)
        .execute(&mut *tx)
        .await
        .map_err(|_| BrokerError::Store)?;
        sqlx::query("INSERT INTO workflow_secret.renewal_t(renewal_id,grant_id,generation) VALUES($1,$2,$3)")
            .bind(attempt).bind(grant).bind(generation).execute(&mut *tx).await.map_err(|_|BrokerError::Store)?;
        tx.commit().await.map_err(|_| BrokerError::Store)?;
        let response = self.provider.refresh(&refresh).await;
        let credential = match response {
            Err(ProviderFailure::NotSent) => {
                let mut tx = self.pool.begin().await.map_err(|_| BrokerError::Store)?;
                let changed = sqlx::query("UPDATE workflow_secret.grant_t SET state='ACTIVE',renewal_id=NULL,owner_boot=NULL,renewal_deadline=NULL
                    WHERE grant_id=$1 AND renewal_id=$2 AND owner_boot=$3 AND generation=$4 AND state='RENEWING'
                    AND renewal_deadline>clock_timestamp() AND expires_at>clock_timestamp() AND NOT revocation_pending")
                    .bind(grant).bind(attempt).bind(self.boot).bind(generation).execute(&mut *tx).await.map_err(|_| BrokerError::Store)?.rows_affected();
                if changed != 1 {
                    return Err(BrokerError::Reauthorize);
                }
                sqlx::query("UPDATE workflow_secret.renewal_t SET result='NOT_SENT',finished_at=clock_timestamp() WHERE renewal_id=$1 AND result IS NULL")
                    .bind(attempt).execute(&mut *tx).await.map_err(|_| BrokerError::Store)?;
                tx.commit().await.map_err(|_| BrokerError::Store)?;
                return Err(BrokerError::Retryable);
            }
            Ok(c)
                if same_grant(&prior, &c.issuer_grant)
                    && c.issuer_grant.generation == generation + 1 =>
            {
                c
            }
            _ => {
                self.fence(grant, attempt).await?;
                return Err(BrokerError::Reauthorize);
            }
        };
        let sealed = self
            .keys
            .seal(grant, generation + 1, &credential.refresh_token)?;
        let mut tx = self.pool.begin().await.map_err(|_| BrokerError::Store)?;
        let changed=sqlx::query("UPDATE workflow_secret.grant_t SET state='ACTIVE',generation=$4,
          ciphertext=$5,key_id=$6,issuer_grant=$7,renewal_id=NULL,owner_boot=NULL,renewal_deadline=NULL
          WHERE grant_id=$1 AND renewal_id=$2 AND owner_boot=$3 AND state='RENEWING'
            AND renewal_deadline>clock_timestamp() AND expires_at>clock_timestamp() AND NOT revocation_pending")
            .bind(grant).bind(attempt).bind(self.boot).bind(generation+1).bind(sealed).bind(&self.keys.active)
            .bind(serde_json::to_value(&credential.issuer_grant).map_err(|_|BrokerError::Evidence)?)
            .execute(&mut *tx).await.map_err(|_|BrokerError::Store)?.rows_affected();
        if changed != 1 {
            drop(tx);
            self.fence(grant, attempt).await?;
            return Err(BrokerError::Reauthorize);
        }
        sqlx::query("UPDATE workflow_secret.renewal_t SET result='ROTATED',finished_at=clock_timestamp() WHERE renewal_id=$1")
            .bind(attempt).execute(&mut *tx).await.map_err(|_|BrokerError::Store)?;
        // Persist the shared rotation regardless of this run's cancellation.
        // Lock only for delivery authorization; a canceled run never receives it.
        let run_active = sqlx::query_scalar::<_, Uuid>(
            "SELECT run_id FROM workflow_secret.run_t
            WHERE run_id=$1 AND grant_id=$2 AND host_id=$3 AND user_id=$4 AND binding=$5
            AND active AND expires_at>clock_timestamp() FOR SHARE",
        )
        .bind(run)
        .bind(grant)
        .bind(host)
        .bind(user)
        .bind(&prior.binding)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_| BrokerError::Store)?
        .is_some();
        tx.commit().await.map_err(|_| BrokerError::Store)?;
        if !run_active {
            return Err(BrokerError::RunInactive);
        }
        Ok(credential)
    }

    async fn fence(&self, grant: Uuid, attempt: Uuid) -> Result<(), BrokerError> {
        let mut tx = self.pool.begin().await.map_err(|_| BrokerError::Store)?;
        sqlx::query("UPDATE workflow_secret.grant_t SET state='REAUTHORIZATION_REQUIRED',revocation_pending=true,
          renewal_id=NULL,owner_boot=NULL,renewal_deadline=NULL WHERE grant_id=$1 AND renewal_id=$2 AND state='RENEWING'")
            .bind(grant).bind(attempt).execute(&mut *tx).await.map_err(|_|BrokerError::Store)?;
        sqlx::query(
            "UPDATE workflow_secret.renewal_t SET result='UNCERTAIN',finished_at=clock_timestamp()
          WHERE renewal_id=$1 AND result IS NULL",
        )
        .bind(attempt)
        .execute(&mut *tx)
        .await
        .map_err(|_| BrokerError::Store)?;
        tx.commit().await.map_err(|_| BrokerError::Store)?;
        self.reconcile_revocations().await
    }

    /// Transient store failures must not terminate the managed recovery task or
    /// close admission for unrelated workflows. Configuration is validated at startup.
    pub async fn recovery_tick(&self) -> Result<(), BrokerError> {
        match self.recover().await {
            Err(BrokerError::Store) => {
                tracing::warn!("credential recovery store unavailable; retrying on next tick");
                Ok(())
            }
            result => result,
        }
    }

    /// Run on startup and periodically. A lost response or crashed owner never
    /// permits another rotation with the persisted, potentially consumed token.
    pub async fn recover(&self) -> Result<(), BrokerError> {
        sqlx::query("UPDATE workflow_secret.enrollment_t SET state='REAUTHORIZATION_REQUIRED',revocation_pending=true
          WHERE state IN ('PREPARING','READY','REDEEMING') AND expires_at<=clock_timestamp()")
            .execute(&self.pool).await.map_err(|_|BrokerError::Store)?;
        let pending = sqlx::query_scalar::<_, Uuid>(
            "SELECT enrollment_id FROM workflow_secret.enrollment_t WHERE revocation_pending",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|_| BrokerError::Store)?;
        for id in pending {
            if self.provider.revoke(&id.to_string()).await.is_ok() {
                sqlx::query("UPDATE workflow_secret.enrollment_t SET revocation_pending=false WHERE enrollment_id=$1")
                    .bind(id).execute(&self.pool).await.map_err(|_|BrokerError::Store)?;
            }
        }
        let stale = sqlx::query_as::<_, (Uuid, Uuid)>(
            "SELECT grant_id,renewal_id FROM workflow_secret.grant_t
          WHERE state='RENEWING' AND renewal_deadline<=clock_timestamp()",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|_| BrokerError::Store)?;
        for (grant, attempt) in stale {
            self.fence(grant, attempt).await?;
        }
        self.reconcile_revocations().await
    }
    async fn reconcile_revocations(&self) -> Result<(), BrokerError> {
        let grants = sqlx::query_scalar::<_, Uuid>(
            "SELECT grant_id FROM workflow_secret.grant_t WHERE revocation_pending",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|_| BrokerError::Store)?;
        for grant in grants {
            if self.provider.revoke(&grant.to_string()).await.is_ok() {
                sqlx::query(
                    "UPDATE workflow_secret.grant_t SET revocation_pending=false WHERE grant_id=$1",
                )
                .bind(grant)
                .execute(&self.pool)
                .await
                .map_err(|_| BrokerError::Store)?;
            }
        }
        Ok(())
    }
}

fn same_grant(a: &IssuerGrant, b: &IssuerGrant) -> bool {
    a.grant_id == b.grant_id
        && a.auth_host_id == b.auth_host_id
        && a.provider_id == b.provider_id
        && a.client_id == b.client_id
        && a.host_id == b.host_id
        && a.user_id == b.user_id
        && a.session_id == b.session_id
        && a.scope == b.scope
        && a.binding == b.binding
        && a.expires_at == b.expires_at
        && b.status == "ACTIVE"
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encrypted_credentials_bind_identity_generation_and_key() {
        let keys =
            CredentialKeys::new("one".into(), BTreeMap::from([("one".into(), [17; 32])])).unwrap();
        let id = Uuid::new_v4();
        let a = keys.seal(id, 1, "private-refresh-token").unwrap();
        let b = keys.seal(id, 1, "private-refresh-token").unwrap();
        assert_ne!(a, b);
        assert_eq!(
            keys.open("one", id, 1, &a).unwrap(),
            "private-refresh-token"
        );
        assert!(keys.open("one", id, 2, &a).is_err());
        assert!(keys.open("one", Uuid::new_v4(), 1, &a).is_err());
        assert!(keys.open("two", id, 1, &a).is_err());
        let mut corrupt = a;
        corrupt[20] ^= 1;
        assert!(keys.open("one", id, 1, &corrupt).is_err());
    }
}

/// Holds credential/run read locks across an operational action transaction.
/// No credential material is exposed. Lock ordering is credential grant/run,
/// then operational authority/permit/dispatch. Never acquire these in reverse.
pub struct RunAuthorityGuard<'a> {
    _transaction: sqlx::Transaction<'a, sqlx::Postgres>,
}
impl CredentialBroker {
    pub async fn lock_run_authority(
        &self,
        run: Uuid,
        grant: Uuid,
        host: Uuid,
        user: Uuid,
    ) -> Result<RunAuthorityGuard<'_>, BrokerError> {
        let mut tx = self.pool.begin().await.map_err(|_| BrokerError::Store)?;
        let row=sqlx::query_as::<_,(i64,Value)>("SELECT g.generation,g.issuer_grant FROM workflow_secret.grant_t g JOIN workflow_secret.run_t r USING(grant_id) WHERE r.run_id=$1 AND g.grant_id=$2 AND r.host_id=$3 AND g.host_id=$3 AND r.user_id=$4 AND g.user_id=$4 AND r.active AND r.expires_at>clock_timestamp() AND g.state='ACTIVE' AND NOT g.revocation_pending AND g.expires_at>clock_timestamp() AND r.binding=g.binding FOR SHARE OF g,r")
            .bind(run).bind(grant).bind(host).bind(user).fetch_optional(&mut *tx).await.map_err(|_|BrokerError::Store)?.ok_or(BrokerError::Reauthorize)?;
        let local: IssuerGrant =
            serde_json::from_value(row.1).map_err(|_| BrokerError::Evidence)?;
        let live = self
            .provider
            .status(&grant.to_string())
            .await
            .map_err(|_| BrokerError::Reauthorize)?;
        if !live.active
            || live.grant.grant_id != grant
            || live.grant.host_id != host
            || live.grant.user_id != user
            || live.grant.generation != row.0
            || live.grant.client_id != local.client_id
            || live.grant.binding != local.binding
            || live.grant.scope != local.scope
            || live.grant.expires_at != local.expires_at
            || live.grant.status != "ACTIVE"
        {
            return Err(BrokerError::Reauthorize);
        }
        Ok(RunAuthorityGuard { _transaction: tx })
    }
}
