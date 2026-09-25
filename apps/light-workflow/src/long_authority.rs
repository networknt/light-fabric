//! Customer-hosted, Gateway-only LONG owner authority. Original user bearers
//! are encrypted in the restricted credential database, never process context.
use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, AeadCore, OsRng, Payload},
};
use base64::Engine as _;
use light_client::config::OAuthWorkflowLongConfig;
use light_client::long_binding::{
    LongBindingClient, LongBindingStore, LongClientError, LongCredentialBroker, StoredLongBinding,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::{collections::BTreeMap, path::Path};
use uuid::Uuid;
use zeroize::Zeroizing;

fn issuer_failure(status: reqwest::StatusCode) -> LongError {
    if status.is_server_error()
        || status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
    {
        LongError::Retryable
    } else {
        LongError::Denied
    }
}

impl From<LongClientError> for LongError {
    fn from(error: LongClientError) -> Self {
        match error {
            LongClientError::Evidence => Self::Evidence,
            LongClientError::Retryable => Self::Retryable,
            LongClientError::Denied => Self::Denied,
            LongClientError::Store => Self::Store,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LongError {
    #[error("LONG authority configuration or evidence rejected")]
    Evidence,
    #[error("LONG credential store unavailable")]
    Store,
    #[error("LONG issuer temporarily unavailable")]
    Retryable,
    #[error("LONG owner authority denied")]
    Denied,
}

struct Keys {
    active: String,
    keys: BTreeMap<String, [u8; 32]>,
}

impl Keys {
    async fn load(path: &Path) -> Result<Self, LongError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Ring {
            active_key_id: String,
            keys: BTreeMap<String, String>,
        }
        let bytes = tokio::fs::read(path).await.map_err(|_| LongError::Store)?;
        let ring: Ring = serde_json::from_slice(&bytes).map_err(|_| LongError::Evidence)?;
        let keys = ring
            .keys
            .into_iter()
            .map(|(id, value)| {
                let decoded = base64::engine::general_purpose::STANDARD
                    .decode(value)
                    .map_err(|_| LongError::Evidence)?;
                Ok((id, decoded.try_into().map_err(|_| LongError::Evidence)?))
            })
            .collect::<Result<BTreeMap<String, [u8; 32]>, LongError>>()?;
        if ring.active_key_id == "plaintext"
            || keys.contains_key("plaintext")
            || !keys.contains_key(&ring.active_key_id)
        {
            return Err(LongError::Evidence);
        }
        Ok(Self {
            active: ring.active_key_id,
            keys,
        })
    }
    fn seal(&self, id: Uuid, token: &str) -> Result<Vec<u8>, LongError> {
        let cipher =
            Aes256Gcm::new_from_slice(&self.keys[&self.active]).map_err(|_| LongError::Evidence)?;
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let aad = format!("workflow-long-owner:{id}:{}", self.active);
        let body = cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: token.as_bytes(),
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| LongError::Evidence)?;
        Ok([nonce.as_slice(), body.as_slice()].concat())
    }
    fn open(&self, key: &str, id: Uuid, bytes: &[u8]) -> Result<String, LongError> {
        if bytes.len() < 28 {
            return Err(LongError::Evidence);
        }
        let cipher = Aes256Gcm::new_from_slice(self.keys.get(key).ok_or(LongError::Evidence)?)
            .map_err(|_| LongError::Evidence)?;
        let aad = format!("workflow-long-owner:{id}:{key}");
        let plain = cipher
            .decrypt(
                Nonce::from_slice(&bytes[..12]),
                Payload {
                    msg: &bytes[12..],
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| LongError::Evidence)?;
        String::from_utf8(plain).map_err(|_| LongError::Evidence)
    }
}

fn encode_source(
    keys: Option<&Keys>,
    binding: Uuid,
    token: &str,
) -> Result<(String, Vec<u8>), LongError> {
    match keys {
        Some(keys) => Ok((keys.active.clone(), keys.seal(binding, token)?)),
        None => Ok(("plaintext".to_string(), token.as_bytes().to_vec())),
    }
}

fn decode_source(
    keys: Option<&Keys>,
    key_id: &str,
    binding: Uuid,
    bytes: &[u8],
) -> Result<String, LongError> {
    if key_id == "plaintext" {
        String::from_utf8(bytes.to_vec()).map_err(|_| LongError::Evidence)
    } else {
        keys.ok_or(LongError::Store)?.open(key_id, binding, bytes)
    }
}

pub struct LongAuthority {
    pool: PgPool,
    client: LongBindingClient,
    client_id: String,
    keys: Option<Keys>,
}

impl LongAuthority {
    pub fn client_id(&self) -> &str {
        &self.client_id
    }
    pub fn broker(self: &std::sync::Arc<Self>) -> LongCredentialBroker<Self> {
        LongCredentialBroker::new(std::sync::Arc::new(self.client.clone()), self.clone())
    }
    pub fn gateway_origin(&self) -> url::Origin {
        self.client.gateway_origin()
    }

    /// Uses the Workflow operational pool. A missing keyring selects the
    /// explicitly configured plaintext storage mode.
    pub async fn open(
        config: &OAuthWorkflowLongConfig,
        dir: &Path,
        pool: PgPool,
        keyring_file: Option<&Path>,
        scope: &[String],
    ) -> Result<Option<Self>, LongError> {
        if config.client_id.is_empty()
            && config.client_secret.is_empty()
            && config.gateway_url.is_empty()
            && config.provider_id.is_empty()
            && config.ca_file.is_empty()
        {
            return Ok(None);
        }
        if config.client_id.is_empty()
            || config.client_secret.is_empty()
            || config.provider_id.is_empty()
        {
            return Err(LongError::Evidence);
        }
        let client = LongBindingClient::from_workflow_config(config, dir)
            .await?
            .with_scope(scope)?;
        let keys = match keyring_file {
            Some(path) => Some(Keys::load(&dir.join(path)).await?),
            None => None,
        };
        sqlx::query("SELECT binding_id FROM workflow_ops.workflow_long_credential_t LIMIT 0")
            .execute(&pool)
            .await
            .map_err(|_| LongError::Store)?;
        sqlx::query("INSERT INTO workflow_ops.workflow_long_identity_t(singleton,gateway_url,provider_id,client_id)
            SELECT true,$1,$2,$3 WHERE NOT EXISTS(SELECT 1 FROM workflow_ops.workflow_long_credential_t) ON CONFLICT DO NOTHING")
        .bind(&config.gateway_url).bind(&config.provider_id).bind(&config.client_id).execute(&pool).await.map_err(|_|LongError::Store)?;
        let matches: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM workflow_ops.workflow_long_identity_t
            WHERE singleton AND gateway_url=$1 AND provider_id=$2 AND client_id=$3)",
        )
        .bind(&config.gateway_url)
        .bind(&config.provider_id)
        .bind(&config.client_id)
        .fetch_one(&pool)
        .await
        .map_err(|_| LongError::Store)?;
        if !matches {
            return Err(LongError::Evidence);
        }
        Ok(Some(Self {
            pool,
            client,
            client_id: config.client_id.clone(),
            keys,
        }))
    }

    /// The same confidential Workflow client obtains a fresh finite app token
    /// through Gateway for dual-token downstream authorization. It is never
    /// persisted in a process context or reused after an unknown expiry.
    pub async fn workload_token(&self) -> Result<String, LongError> {
        self.client.workload_token().await.map_err(Into::into)
    }

    pub async fn register(
        &self,
        run: Uuid,
        host: Uuid,
        owner: Uuid,
        original_token: &str,
        registration_key: &str,
    ) -> Result<Uuid, LongError> {
        if original_token.is_empty() || original_token.len() > 16_384 || registration_key.len() < 32
        {
            return Err(LongError::Evidence);
        }
        let token_hash = hex::encode(Sha256::digest(original_token.as_bytes()));
        let key_hash = hex::encode(Sha256::digest(registration_key.as_bytes()));
        let prior: Option<(Uuid, Uuid, Uuid, String, String, String)> = sqlx::query_as(
            "SELECT binding_id,host_id,owner_user_id,registration_key_sha256,subject_token_sha256,state
             FROM workflow_ops.workflow_long_credential_t WHERE run_id=$1",
        )
        .bind(run)
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| LongError::Store)?;
        if let Some((binding, stored_host, stored_owner, stored_key, stored_token, state)) = prior {
            if stored_host == host
                && stored_owner == owner
                && stored_key == key_hash
                && stored_token == token_hash
                && matches!(state.as_str(), "PENDING" | "ACTIVE")
            {
                return Ok(binding);
            }
            return Err(LongError::Evidence);
        }
        let binding = self
            .client
            .register_work(run, host, original_token, registration_key)
            .await?;
        if binding.workflow_instance_id != run
            || binding.work_id != Some(run)
            || binding.host_id != host
            || binding.owner_user_id != owner
            || binding.state != "PENDING"
            || binding.version < 1
        {
            return Err(LongError::Evidence);
        }
        let (key_id, token_bytes) =
            encode_source(self.keys.as_ref(), binding.binding_id, original_token)?;
        let changed=sqlx::query("INSERT INTO workflow_ops.workflow_long_credential_t
          (binding_id,run_id,host_id,owner_user_id,issuer_client_id,registration_key_sha256,
           subject_token_sha256,state,issuer_version,key_id,token_bytes)
           VALUES($1,$2,$3,$4,$5,$6,$7,'PENDING',$8,$9,$10)
           ON CONFLICT(run_id) DO UPDATE SET run_id=EXCLUDED.run_id
           WHERE workflow_ops.workflow_long_credential_t.binding_id=EXCLUDED.binding_id
             AND workflow_ops.workflow_long_credential_t.host_id=EXCLUDED.host_id
             AND workflow_ops.workflow_long_credential_t.owner_user_id=EXCLUDED.owner_user_id
             AND workflow_ops.workflow_long_credential_t.registration_key_sha256=EXCLUDED.registration_key_sha256
             AND workflow_ops.workflow_long_credential_t.subject_token_sha256=EXCLUDED.subject_token_sha256")
            .bind(binding.binding_id).bind(run).bind(host).bind(owner).bind(&self.client_id)
            .bind(key_hash).bind(token_hash).bind(binding.version).bind(key_id).bind(token_bytes)
            .execute(&self.pool).await.map_err(|_|LongError::Store)?.rows_affected();
        if changed != 1 {
            return Err(LongError::Evidence);
        }
        Ok(binding.binding_id)
    }

    pub async fn activate(&self, run: Uuid, acceptance_digest: &str) -> Result<(), LongError> {
        if acceptance_digest.len() != 64
            || !acceptance_digest.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(LongError::Evidence);
        }
        let (binding,version,state,stored_digest):(Uuid,i64,String,Option<String>)=sqlx::query_as(
            "SELECT binding_id,issuer_version,state,acceptance_digest FROM workflow_ops.workflow_long_credential_t WHERE run_id=$1")
            .bind(run).fetch_optional(&self.pool).await.map_err(|_|LongError::Store)?.ok_or(LongError::Denied)?;
        if state == "ACTIVE" {
            return if stored_digest.as_deref() == Some(acceptance_digest) {
                Ok(())
            } else {
                Err(LongError::Evidence)
            };
        }
        if state != "PENDING" {
            return Err(LongError::Denied);
        }
        let reply = self
            .client
            .activate_workflow(binding, run, version, acceptance_digest)
            .await?;
        if reply.binding_id != binding || reply.state != "ACTIVE" {
            return Err(LongError::Evidence);
        }
        let changed = sqlx::query(
            "UPDATE workflow_ops.workflow_long_credential_t SET state='ACTIVE',issuer_version=$2,
            acceptance_digest=$3,updated_ts=CURRENT_TIMESTAMP WHERE run_id=$1 AND state='PENDING'",
        )
        .bind(run)
        .bind(reply.version)
        .bind(acceptance_digest)
        .execute(&self.pool)
        .await
        .map_err(|_| LongError::Store)?
        .rows_affected();
        if changed != 1 {
            // A concurrent terminal transition has fenced dispatch locally.
            return Err(LongError::Denied);
        }
        Ok(())
    }

    pub async fn token_for(&self, run: Uuid, host: Uuid, owner: Uuid) -> Result<String, LongError> {
        let row = LongBindingStore::active(self, run, host, owner)
            .await?
            .ok_or(LongError::Denied)?;
        self.client
            .exchange(row.binding_id, &row.source_token)
            .await
            .map_err(Into::into)
    }

    pub async fn binding_for(&self, run: Uuid, host: Uuid, owner: Uuid) -> Result<Uuid, LongError> {
        sqlx::query_scalar("SELECT binding_id FROM workflow_ops.workflow_long_credential_t
            WHERE run_id=$1 AND host_id=$2 AND owner_user_id=$3 AND issuer_client_id=$4 AND state='ACTIVE'")
            .bind(run).bind(host).bind(owner).bind(&self.client_id)
            .fetch_optional(&self.pool).await.map_err(|_|LongError::Store)?.ok_or(LongError::Denied)
    }

    /// The caller first commits the terminal operational transition and a
    /// durable close intent; this method can be retried after any outage.
    pub async fn close(
        &self,
        run: Uuid,
        terminal_state: &str,
        terminal_version: i64,
        close_id: Uuid,
    ) -> Result<String, LongError> {
        if !matches!(terminal_state, "COMPLETED" | "CANCELED") || terminal_version < 1 {
            return Err(LongError::Evidence);
        }
        let row = sqlx::query_as::<_, (Uuid, String, Option<Uuid>)>(
            "SELECT binding_id,state,close_id FROM workflow_ops.workflow_long_credential_t WHERE run_id=$1",
        )
        .bind(run)
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| LongError::Store)?
        .ok_or(LongError::Denied)?;
        if row.1 == "REVOKED" || row.1 == "CLOSED" && row.2 == Some(close_id) {
            return Ok(row.1);
        }
        if !matches!(row.1.as_str(), "PENDING" | "ACTIVE" | "CLOSING") {
            return Err(LongError::Denied);
        }
        let changed = sqlx::query(
            "UPDATE workflow_ops.workflow_long_credential_t SET state='CLOSING',close_id=$2,
            terminal_state=$3,terminal_version=$4,updated_ts=CURRENT_TIMESTAMP
            WHERE run_id=$1 AND state IN ('PENDING','ACTIVE','CLOSING')
              AND (close_id IS NULL OR close_id=$2)",
        )
        .bind(run)
        .bind(close_id)
        .bind(terminal_state)
        .bind(terminal_version)
        .execute(&self.pool)
        .await
        .map_err(|_| LongError::Store)?
        .rows_affected();
        if changed != 1 {
            return Err(LongError::Evidence);
        }
        let reply = self
            .client
            .close_workflow(row.0, run, close_id, terminal_state, terminal_version)
            .await?;
        if reply.binding_id != row.0 || !matches!(reply.state.as_str(), "CLOSED" | "REVOKED") {
            return Err(LongError::Evidence);
        }
        let changed = sqlx::query(
            "UPDATE workflow_ops.workflow_long_credential_t SET state=$4,issuer_version=$2,
            updated_ts=CURRENT_TIMESTAMP WHERE run_id=$1 AND state='CLOSING' AND close_id=$3",
        )
        .bind(run)
        .bind(reply.version)
        .bind(close_id)
        .bind(&reply.state)
        .execute(&self.pool)
        .await
        .map_err(|_| LongError::Store)?
        .rows_affected();
        if changed != 1 {
            return Err(LongError::Denied);
        }
        Ok(reply.state)
    }
}

#[async_trait::async_trait]
impl LongBindingStore for LongAuthority {
    async fn active(
        &self,
        work: Uuid,
        host: Uuid,
        owner: Uuid,
    ) -> Result<Option<StoredLongBinding>, LongClientError> {
        let row = sqlx::query_as::<_, (Uuid, String, Vec<u8>)>(
            "SELECT binding_id,key_id,token_bytes FROM workflow_ops.workflow_long_credential_t
             WHERE run_id=$1 AND host_id=$2 AND owner_user_id=$3 AND issuer_client_id=$4
               AND state='ACTIVE'",
        )
        .bind(work)
        .bind(host)
        .bind(owner)
        .bind(&self.client_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| LongClientError::Store)?;
        row.map(|(binding_id, key_id, token_bytes)| {
            let source = decode_source(self.keys.as_ref(), &key_id, binding_id, &token_bytes)
                .map_err(|error| match error {
                    LongError::Store => LongClientError::Store,
                    _ => LongClientError::Evidence,
                })?;
            Ok(StoredLongBinding {
                binding_id,
                work_id: work,
                host_id: host,
                owner_user_id: owner,
                source_token: Zeroizing::new(source),
            })
        })
        .transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_outage_and_rate_limit_are_retryable_but_revocation_is_denied() {
        assert!(matches!(
            issuer_failure(reqwest::StatusCode::BAD_GATEWAY),
            LongError::Retryable
        ));
        assert!(matches!(
            issuer_failure(reqwest::StatusCode::TOO_MANY_REQUESTS),
            LongError::Retryable
        ));
        assert!(matches!(
            issuer_failure(reqwest::StatusCode::REQUEST_TIMEOUT),
            LongError::Retryable
        ));
        assert!(matches!(
            issuer_failure(reqwest::StatusCode::UNAUTHORIZED),
            LongError::Denied
        ));
        assert!(matches!(
            issuer_failure(reqwest::StatusCode::CONFLICT),
            LongError::Denied
        ));
    }

    #[test]
    fn encrypted_owner_bearer_is_bound_to_exact_binding_and_detects_tampering() {
        let keys = Keys {
            active: "one".into(),
            keys: BTreeMap::from([("one".into(), [7u8; 32])]),
        };
        let binding = Uuid::new_v4();
        let encrypted = keys.seal(binding, "owner-test-bearer").unwrap();
        assert!(
            !encrypted
                .windows(17)
                .any(|part| part == b"owner-test-bearer")
        );
        assert_eq!(
            keys.open("one", binding, &encrypted).unwrap(),
            "owner-test-bearer"
        );
        assert!(keys.open("one", Uuid::new_v4(), &encrypted).is_err());
        let mut tampered = encrypted;
        *tampered.last_mut().unwrap() ^= 1;
        assert!(keys.open("one", binding, &tampered).is_err());
    }

    #[test]
    fn optional_keyring_preserves_plaintext_and_encrypted_modes() {
        let binding = Uuid::new_v4();
        let (plain_id, plain_bytes) = encode_source(None, binding, "owner-token").unwrap();
        assert_eq!(plain_id, "plaintext");
        assert_eq!(plain_bytes, b"owner-token");
        assert_eq!(
            decode_source(None, &plain_id, binding, &plain_bytes).unwrap(),
            "owner-token"
        );
        let keys = Keys {
            active: "one".into(),
            keys: BTreeMap::from([("one".into(), [7u8; 32])]),
        };
        let (key_id, encrypted) = encode_source(Some(&keys), binding, "owner-token").unwrap();
        assert_eq!(key_id, "one");
        assert_ne!(encrypted, b"owner-token");
        assert!(decode_source(None, &key_id, binding, &encrypted).is_err());
        assert_eq!(
            decode_source(Some(&keys), &key_id, binding, &encrypted).unwrap(),
            "owner-token"
        );
    }

    #[tokio::test]
    async fn workflow_long_rejects_direct_http_issuer_configuration() {
        let config = OAuthWorkflowLongConfig {
            gateway_url: "http://issuer.example.invalid".into(),
            provider_id: "a1".into(),
            client_id: "workflow".into(),
            client_secret: "test-only".into(),
            database_url_file: "db-url".into(),
            keyring_file: "keyring".into(),
            ca_file: String::new(),
        };
        assert!(matches!(
            LongAuthority::open(
                &config,
                Path::new("/tmp"),
                PgPool::connect_lazy("postgres://localhost/test").unwrap(),
                None,
                &[],
            )
            .await,
            Err(LongError::Evidence)
        ));
    }
}
