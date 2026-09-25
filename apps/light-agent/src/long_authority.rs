//! Agent-owned LONG source storage. A polled owner bearer is used only to
//! register, then sealed in this restricted operational table before the job
//! can use Gateway credentials after restart.
use light_client::{
    config::OAuthTokenConfig,
    long_binding::{
        LongBindingClient, LongBindingStore, LongClientError, LongCredentialBroker,
        StoredLongBinding,
    },
    long_source::{SealedLongSource, SourceKeyring},
};
use sqlx::{PgPool, Postgres, Transaction};
use std::{path::Path, sync::Arc};
use uuid::Uuid;

pub struct AgentLongAuthority {
    pool: PgPool,
    client: Arc<LongBindingClient>,
    keys: SourceKeyring,
    client_id: String,
}

pub struct PreparedBinding {
    job_id: Uuid,
    host_id: Uuid,
    owner_user_id: Uuid,
    binding_id: Uuid,
    version: i64,
    acceptance_digest: String,
    sealed: SealedLongSource,
}

impl AgentLongAuthority {
    pub fn client(&self) -> Arc<LongBindingClient> {
        self.client.clone()
    }
    pub async fn status(
        &self,
        job: Uuid,
        host: Uuid,
        owner: Uuid,
    ) -> Result<Option<String>, LongClientError> {
        sqlx::query_scalar(
            "SELECT state FROM agent_ops.agent_long_binding_t
            WHERE job_id=$1 AND host_id=$2 AND owner_user_id=$3 AND issuer_client_id=$4",
        )
        .bind(job)
        .bind(host)
        .bind(owner)
        .bind(&self.client_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| LongClientError::Store)
    }

    pub async fn open(
        config: &OAuthTokenConfig,
        dir: &Path,
        pool: PgPool,
    ) -> Result<Self, LongClientError> {
        if config.workflow_long.keyring_file.is_empty() {
            return Err(LongClientError::Evidence);
        }
        let client = Arc::new(LongBindingClient::from_token_config(config, dir).await?);
        let keys = SourceKeyring::load(&dir.join(&config.workflow_long.keyring_file)).await?;
        let installed: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM operational_meta.operational_schema_migration_t
             WHERE migration_owner='agent-store' AND schema_name='agent_ops'
               AND migration_id='0006_long_work_binding')",
        )
        .fetch_one(&pool)
        .await
        .map_err(|_| LongClientError::Store)?;
        if !installed {
            return Err(LongClientError::Store);
        }
        Ok(Self {
            pool,
            client_id: client.client_id().into(),
            client,
            keys,
        })
    }

    pub fn broker(self: &Arc<Self>) -> LongCredentialBroker<Self> {
        LongCredentialBroker::new(self.client.clone(), self.clone())
    }

    /// Call only for a validated, non-cancelled delivery carrying a fresh owner
    /// token. The issuer's authenticated client ID binds this Agent type.
    pub async fn prepare(
        &self,
        job: Uuid,
        host: Uuid,
        owner: Uuid,
        source: &str,
        registration_key: &str,
        acceptance_digest: &str,
    ) -> Result<PreparedBinding, LongClientError> {
        if job.is_nil()
            || host.is_nil()
            || owner.is_nil()
            || acceptance_digest.len() != 64
            || !acceptance_digest.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(LongClientError::Evidence);
        }
        let response = self
            .client
            .register_work(job, host, source, registration_key)
            .await?;
        if response.owner_user_id != owner || response.state != "PENDING" {
            return Err(LongClientError::Evidence);
        }
        let sealed = self
            .keys
            .seal(&self.client_id, response.binding_id, source)?;
        Ok(PreparedBinding {
            job_id: job,
            host_id: host,
            owner_user_id: owner,
            binding_id: response.binding_id,
            version: response.version,
            acceptance_digest: acceptance_digest.into(),
            sealed,
        })
    }

    /// Insert after the Agent job row in the same local acceptance transaction.
    pub async fn record(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        prepared: PreparedBinding,
    ) -> Result<(), LongClientError> {
        let inserted = sqlx::query(
            "INSERT INTO agent_ops.agent_long_binding_t
             (host_id,job_id,owner_user_id,binding_id,issuer_client_id,key_id,ciphertext,
              acceptance_digest,registration_version,state)
             VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,'ACTIVATION_PENDING')
             ON CONFLICT(host_id,job_id) DO NOTHING",
        )
        .bind(prepared.host_id)
        .bind(prepared.job_id)
        .bind(prepared.owner_user_id)
        .bind(prepared.binding_id)
        .bind(&self.client_id)
        .bind(&prepared.sealed.key_id)
        .bind(&prepared.sealed.ciphertext)
        .bind(&prepared.acceptance_digest)
        .bind(prepared.version)
        .execute(&mut **tx)
        .await
        .map_err(|_| LongClientError::Store)?;
        if inserted.rows_affected() == 0 {
            let same: Option<bool> = sqlx::query_scalar(
                "SELECT owner_user_id=$3 AND binding_id=$4 AND issuer_client_id=$5
                 FROM agent_ops.agent_long_binding_t WHERE host_id=$1 AND job_id=$2",
            )
            .bind(prepared.host_id)
            .bind(prepared.job_id)
            .bind(prepared.owner_user_id)
            .bind(prepared.binding_id)
            .bind(&self.client_id)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|_| LongClientError::Store)?;
            if same != Some(true) {
                return Err(LongClientError::Evidence);
            }
        }
        Ok(())
    }

    pub async fn reconcile(&self) -> Result<(), LongClientError> {
        let pending = sqlx::query_as::<_, (Uuid, Uuid, i64, String)>(
            "SELECT binding_id,job_id,registration_version,acceptance_digest
             FROM agent_ops.agent_long_binding_t WHERE issuer_client_id=$1
               AND state='ACTIVATION_PENDING' ORDER BY created_ts LIMIT 32",
        )
        .bind(&self.client_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|_| LongClientError::Store)?;
        for (binding, job, version, digest) in pending {
            let reply = self
                .client
                .activate_work(binding, job, version, &digest)
                .await?;
            if reply.binding_id != binding || reply.state != "ACTIVE" {
                return Err(LongClientError::Evidence);
            }
            sqlx::query(
                "UPDATE agent_ops.agent_long_binding_t SET state='ACTIVE'
                WHERE binding_id=$1 AND issuer_client_id=$2 AND state='ACTIVATION_PENDING'",
            )
            .bind(binding)
            .bind(&self.client_id)
            .execute(&self.pool)
            .await
            .map_err(|_| LongClientError::Store)?;
        }
        let terminal = sqlx::query_as::<_, (Uuid, String, bool)>(
            "SELECT b.binding_id,j.state,(j.cancellation_requested_ts IS NOT NULL)
             FROM agent_ops.agent_long_binding_t b
             JOIN agent_ops.agent_job_t j ON j.host_id=b.host_id AND j.job_id=b.job_id
             WHERE b.issuer_client_id=$1 AND b.state='ACTIVE'
               AND (j.state IN ('SUCCEEDED','FAILED','CANCELLED','UNKNOWN')
                    OR j.cancellation_requested_ts IS NOT NULL)
             ORDER BY b.created_ts LIMIT 32",
        )
        .bind(&self.client_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|_| LongClientError::Store)?;
        for (binding, state, cancelled) in terminal {
            let reason = if cancelled || state == "CANCELLED" {
                "CANCELED"
            } else {
                "COMPLETED"
            };
            sqlx::query(
                "UPDATE agent_ops.agent_long_binding_t
                SET state='CLOSE_PENDING',close_id=$2,close_reason=$3
                WHERE binding_id=$1 AND issuer_client_id=$4 AND state='ACTIVE'",
            )
            .bind(binding)
            .bind(Uuid::new_v4())
            .bind(reason)
            .bind(&self.client_id)
            .execute(&self.pool)
            .await
            .map_err(|_| LongClientError::Store)?;
        }
        let closing = sqlx::query_as::<_, (Uuid, Uuid, Uuid, String)>(
            "SELECT binding_id,job_id,close_id,close_reason
             FROM agent_ops.agent_long_binding_t WHERE issuer_client_id=$1
               AND state='CLOSE_PENDING' ORDER BY created_ts LIMIT 32",
        )
        .bind(&self.client_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|_| LongClientError::Store)?;
        for (binding, job, close_id, reason) in closing {
            let reply = self
                .client
                .close_work(binding, job, close_id, &reason, 1)
                .await?;
            if reply.binding_id != binding || !matches!(reply.state.as_str(), "CLOSED" | "REVOKED")
            {
                return Err(LongClientError::Evidence);
            }
            sqlx::query(
                "UPDATE agent_ops.agent_long_binding_t SET state='CLOSED',closed_ts=now()
                WHERE binding_id=$1 AND issuer_client_id=$2 AND state='CLOSE_PENDING'",
            )
            .bind(binding)
            .bind(&self.client_id)
            .execute(&self.pool)
            .await
            .map_err(|_| LongClientError::Store)?;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl LongBindingStore for AgentLongAuthority {
    async fn active(
        &self,
        work: Uuid,
        host: Uuid,
        owner: Uuid,
    ) -> Result<Option<StoredLongBinding>, LongClientError> {
        let row = sqlx::query_as::<_, (Uuid, String, Vec<u8>)>(
            "SELECT b.binding_id,b.key_id,b.ciphertext
             FROM agent_ops.agent_long_binding_t b
             JOIN agent_ops.agent_job_t j ON j.host_id=b.host_id AND j.job_id=b.job_id
             WHERE b.job_id=$1 AND b.host_id=$2 AND b.owner_user_id=$3
               AND b.issuer_client_id=$4 AND b.state='ACTIVE'
               AND j.state IN ('PENDING','TURN_CREATED','RUNNING')
               AND j.cancellation_requested_ts IS NULL",
        )
        .bind(work)
        .bind(host)
        .bind(owner)
        .bind(&self.client_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| LongClientError::Store)?;
        row.map(|(binding_id, key_id, ciphertext)| {
            Ok(StoredLongBinding {
                binding_id,
                work_id: work,
                host_id: host,
                owner_user_id: owner,
                source_token: self
                    .keys
                    .open(&self.client_id, binding_id, &key_id, &ciphertext)?,
            })
        })
        .transpose()
    }
}
