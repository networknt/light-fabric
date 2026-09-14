use crate::{
    Owner, RegisterOwner,
    ledger::{Error, Ledger},
};
use sqlx::Row;
impl Ledger {
    /// Invoked only after certificate-to-service/replica registration checks.
    /// A retried registration replays its boot, but a superseded boot never
    /// regains ownership. No user authority is created by registration.
    pub async fn register_owner(&self, peer: &str, r: &RegisterOwner) -> Result<Owner, Error> {
        if peer.len() != 64
            || !peer.bytes().all(|b| b.is_ascii_hexdigit())
            || r.gateway_service.is_empty()
            || r.replica.is_nil()
            || r.boot.is_nil()
        {
            return Err(Error::Denied);
        }
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,374))")
            .bind(peer)
            .execute(&mut *tx)
            .await?;
        let current = sqlx::query(
            "SELECT * FROM workflow_ops.workflow_gateway_owner_t WHERE peer_sha256=$1 FOR UPDATE",
        )
        .bind(peer)
        .fetch_optional(&mut *tx)
        .await?;
        let mut generation = 1;
        if let Some(row) = current {
            if row.get::<String, _>("gateway_service") != r.gateway_service
                || row.get::<uuid::Uuid, _>("replica") != r.replica
                || !row.get::<bool, _>("active")
            {
                return Err(Error::Denied);
            }
            generation = row.get::<i64, _>("fencing_generation");
            if row.get::<uuid::Uuid, _>("boot") == r.boot {
                tx.commit().await?;
                return Ok(Owner {
                    gateway_service: r.gateway_service.clone(),
                    replica: r.replica,
                    boot: r.boot,
                    fencing_generation: generation,
                });
            }
            generation = generation.checked_add(1).ok_or(Error::Denied)?;
        }
        let seen:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workflow_ops.workflow_gateway_boot_t WHERE peer_sha256=$1 AND boot=$2)")
            .bind(peer).bind(r.boot).fetch_one(&mut *tx).await?;
        if seen {
            return Err(Error::Conflict);
        }
        sqlx::query("INSERT INTO workflow_ops.workflow_gateway_boot_t(peer_sha256,boot,fencing_generation) VALUES($1,$2,$3)")
            .bind(peer).bind(r.boot).bind(generation).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO workflow_ops.workflow_gateway_owner_t(peer_sha256,gateway_service,replica,boot,fencing_generation) VALUES($1,$2,$3,$4,$5) ON CONFLICT(peer_sha256) DO UPDATE SET boot=EXCLUDED.boot,fencing_generation=EXCLUDED.fencing_generation")
            .bind(peer).bind(&r.gateway_service).bind(r.replica).bind(r.boot).bind(generation).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(Owner {
            gateway_service: r.gateway_service.clone(),
            replica: r.replica,
            boot: r.boot,
            fencing_generation: generation,
        })
    }
    pub async fn is_current_owner(&self, peer: &str, o: &Owner) -> Result<bool, Error> {
        Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workflow_ops.workflow_gateway_owner_t WHERE peer_sha256=$1 AND gateway_service=$2 AND replica=$3 AND boot=$4 AND fencing_generation=$5 AND active)")
            .bind(peer).bind(&o.gateway_service).bind(o.replica).bind(o.boot).bind(o.fencing_generation).fetch_one(&self.pool).await?)
    }
}

pub(crate) async fn lock_owner(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    o: &Owner,
) -> Result<(), Error> {
    let found:Option<bool>=sqlx::query_scalar("SELECT active FROM workflow_ops.workflow_gateway_owner_t WHERE gateway_service=$1 AND replica=$2 AND boot=$3 AND fencing_generation=$4 FOR SHARE")
        .bind(&o.gateway_service).bind(o.replica).bind(o.boot).bind(o.fencing_generation).fetch_optional(&mut **tx).await?;
    if found != Some(true) {
        return Err(Error::Denied);
    }
    Ok(())
}
