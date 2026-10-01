//! Narrow current-run authority used by internal action and Agent job guards.
//! The stored credential kind, rather than process-wide LONG configuration,
//! selects the authority for each run.
use chrono::{DateTime, Utc};
use sqlx::{PgPool, Row};
use std::sync::Arc;
use uuid::Uuid;

#[async_trait::async_trait]
pub trait RunAuthority: Send + Sync {
    async fn lock_run_authority(
        &self,
        run: Uuid,
        grant: Uuid,
        host: Uuid,
        user: Uuid,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
}

pub struct InvokeRunAuthority {
    pool: PgPool,
}

impl InvokeRunAuthority {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn lock_at(
        &self,
        run: Uuid,
        grant: Uuid,
        host: Uuid,
        user: Uuid,
        now: DateTime<Utc>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM workflow_ops.workflow_action_authority_t a
             JOIN workflow_ops.workflow_invocation_t i ON i.host_id=a.host_id AND i.workflow_instance_id=a.run_id
             JOIN workflow_ops.workflow_run_credential_t c ON c.host_id=a.host_id AND c.workflow_instance_id=a.run_id
             WHERE a.run_id=$1 AND a.grant_id=$2 AND a.host_id=$3 AND a.user_id=$4
               AND a.credential_kind='invoke' AND a.active AND a.deadline>$5
               AND i.state IN ('ACCEPTED','RUNNING','WAITING') AND i.cancel_requested_ts IS NULL
               AND i.deadline_ts>$5 AND c.expires_ts>$5 AND a.user_id::text=i.end_user_subject)"
        ).bind(run).bind(grant).bind(host).bind(user).bind(now)
            .fetch_one(&self.pool).await?;
        if !exists {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "invoke run authority denied",
            )
            .into());
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl RunAuthority for InvokeRunAuthority {
    async fn lock_run_authority(
        &self,
        run: Uuid,
        grant: Uuid,
        host: Uuid,
        user: Uuid,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.lock_at(run, grant, host, user, Utc::now()).await
    }
}

pub struct PerRunAuthority {
    pool: PgPool,
    invoke: InvokeRunAuthority,
    long: Option<Arc<crate::long_authority::LongAuthority>>,
    tokens: Arc<crate::run_token::RunTokenSelector>,
}

impl PerRunAuthority {
    pub fn new(
        pool: PgPool,
        long: Option<Arc<crate::long_authority::LongAuthority>>,
        tokens: Arc<crate::run_token::RunTokenSelector>,
    ) -> Self {
        Self {
            invoke: InvokeRunAuthority::new(pool.clone()),
            pool,
            long,
            tokens,
        }
    }
}

#[async_trait::async_trait]
impl RunAuthority for PerRunAuthority {
    async fn lock_run_authority(
        &self,
        run: Uuid,
        grant: Uuid,
        host: Uuid,
        user: Uuid,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let row = sqlx::query("SELECT credential_kind FROM workflow_ops.workflow_action_authority_t
            WHERE host_id=$1 AND run_id=$2 AND grant_id=$3 AND user_id=$4 AND active AND deadline>clock_timestamp()")
            .bind(host).bind(run).bind(grant).bind(user).fetch_optional(&self.pool).await?
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::PermissionDenied, "run authority denied"))?;
        match row.get::<&str, _>("credential_kind") {
            "invoke" => self.invoke.lock_run_authority(run, grant, host, user).await,
            "long" => match &self.long {
                Some(long) => {
                    long.lock_run_authority(run, grant, host, user).await?;
                    let _ = self
                        .tokens
                        .select_run_token(run, host, user, Utc::now())
                        .await?;
                    Ok(())
                }
                None => Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "LONG authority unavailable",
                )
                .into()),
            },
            // The retired broker has no credential source. Existing rows fail closed.
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "retired broker authority denied",
            )
            .into()),
        }
    }
}

#[async_trait::async_trait]
impl RunAuthority for crate::long_authority::LongAuthority {
    async fn lock_run_authority(
        &self,
        run: Uuid,
        grant: Uuid,
        host: Uuid,
        user: Uuid,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if self.binding_for(run, host, user).await? != grant {
            return Err(crate::long_authority::LongError::Denied.into());
        }
        Ok(())
    }
}
