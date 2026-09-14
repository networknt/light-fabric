//! Durable dispatch transitions. The admission component installs authority and
//! permits; public callers cannot create them. Every transition serializes on
//! authority then permit, followed by the dispatch row. Database clock only.
use crate::*;
use sqlx::{PgPool, Postgres, Row, Transaction};
pub const MIGRATION: &str = include_str!(
    "../../workflow-store/migrations/workflow-postgres/0007_workflow_action_dispatch.sql"
);
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("action authority denied")]
    Denied,
    #[error("action binding conflicts")]
    Conflict,
    #[error("action outcome requires reconciliation")]
    Uncertain,
    #[error("action store unavailable")]
    Store(#[from] sqlx::Error),
    #[error("invalid stored action contract")]
    Contract(#[from] serde_json::Error),
}
#[derive(Clone)]
pub struct Ledger {
    pub(crate) pool: PgPool,
}
impl Ledger {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
    /// Never call from a model-facing route. The producer must obtain this exact
    /// binding from admitted user/grant state and the pinned dependency registry.
    pub async fn install_permit(&self, binding: &Binding, retry_limit: i64) -> Result<(), Error> {
        binding.validate().map_err(|_| Error::Denied)?;
        if !(1..=100).contains(&retry_limit) {
            return Err(Error::Denied);
        }
        let mut tx = self.pool.begin().await?;
        self.authority(&mut tx, binding, true).await?;
        if let Some(parent) = binding.parent_action_id {
            let parent:serde_json::Value=sqlx::query_scalar("SELECT binding FROM workflow_ops.workflow_action_permit_t WHERE host_id=$1 AND action_id=$2 AND active FOR SHARE")
                .bind(binding.host_id).bind(parent).fetch_optional(&mut *tx).await?.ok_or(Error::Denied)?;
            binding
                .validate_child_of(&serde_json::from_value(parent)?)
                .map_err(|_| Error::Denied)?;
        }
        sqlx::query("INSERT INTO workflow_ops.workflow_action_permit_t(host_id,action_id,run_id,attempt_id,binding,retry_limit) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT DO NOTHING")
            .bind(binding.host_id).bind(binding.action_id).bind(binding.run_id).bind(binding.attempt_id).bind(serde_json::to_value(binding)?).bind(retry_limit).execute(&mut *tx).await?;
        let stored:serde_json::Value=sqlx::query_scalar("SELECT binding FROM workflow_ops.workflow_action_permit_t WHERE host_id=$1 AND action_id=$2 AND active")
            .bind(binding.host_id).bind(binding.action_id).fetch_optional(&mut *tx).await?.ok_or(Error::Conflict)?;
        if serde_json::from_value::<Binding>(stored)? != *binding {
            return Err(Error::Conflict);
        }
        tx.commit().await?;
        Ok(())
    }
    async fn authority(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        b: &Binding,
        live: bool,
    ) -> Result<(), Error> {
        if live {
            crate::runtime::lock_live_invocation(tx, b).await?;
        } else {
            crate::runtime::lock_budget(tx, b).await?;
        }
        let row=sqlx::query("SELECT *,clock_timestamp() AS now FROM workflow_ops.workflow_action_authority_t WHERE host_id=$1 AND run_id=$2 FOR UPDATE")
            .bind(b.host_id).bind(b.run_id).fetch_optional(&mut **tx).await?.ok_or(Error::Denied)?;
        if row.get::<Uuid, _>("grant_id") != b.grant_id
            || row.get::<Uuid, _>("user_id") != b.user_id
        {
            return Err(Error::Denied);
        }
        if live
            && (!row.get::<bool, _>("active")
                || row.get::<i64, _>("grant_generation") != b.grant_generation
                || row.get::<i64, _>("run_generation") != b.run_generation
                || row.get::<i64, _>("budget_generation") != b.budget_generation
                || row.get::<DateTime<Utc>, _>("deadline") <= row.get::<DateTime<Utc>, _>("now")
                || b.deadline <= row.get::<DateTime<Utc>, _>("now")
                || b.deadline > row.get::<DateTime<Utc>, _>("deadline"))
        {
            return Err(Error::Denied);
        }
        Ok(())
    }
    async fn permit(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        b: &Binding,
        live: bool,
    ) -> Result<(i64, i64), Error> {
        let row=sqlx::query("SELECT binding,active,retry_limit,authorization_count FROM workflow_ops.workflow_action_permit_t WHERE host_id=$1 AND action_id=$2 FOR UPDATE")
            .bind(b.host_id).bind(b.action_id).fetch_optional(&mut **tx).await?.ok_or(Error::Denied)?;
        if serde_json::from_value::<Binding>(row.get("binding"))? != *b {
            return Err(Error::Conflict);
        }
        if live && !row.get::<bool, _>("active") {
            return Err(Error::Denied);
        }
        Ok((row.get("authorization_count"), row.get("retry_limit")))
    }
    async fn audit(
        tx: &mut Transaction<'_, Postgres>,
        b: &Binding,
        g: Option<i64>,
        event: &str,
    ) -> Result<(), Error> {
        sqlx::query("INSERT INTO workflow_ops.workflow_action_audit_t(event_id,host_id,action_id,generation,event_type) VALUES($1,$2,$3,$4,$5)")
            .bind(Uuid::now_v7()).bind(b.host_id).bind(b.action_id).bind(g).bind(event).execute(&mut **tx).await?;
        Ok(())
    }
    pub async fn authorize(&self, b: &Binding, owner: &Owner) -> Result<Decision, Error> {
        b.validate().map_err(|_| Error::Denied)?;
        if owner.gateway_service.is_empty()
            || owner.replica.is_nil()
            || owner.boot.is_nil()
            || owner.fencing_generation <= 0
        {
            return Err(Error::Denied);
        }
        let mut tx = self.pool.begin().await?;
        crate::owners::lock_owner(&mut tx, owner).await?;
        self.authority(&mut tx, b, true).await?;
        let (count, limit) = self.permit(&mut tx, b, true).await?;
        let row=sqlx::query("SELECT *,clock_timestamp() AS now FROM workflow_ops.workflow_action_dispatch_t WHERE host_id=$1 AND action_id=$2 ORDER BY generation DESC LIMIT 1 FOR UPDATE")
            .bind(b.host_id).bind(b.action_id).fetch_optional(&mut *tx).await?;
        let mut generation = 1;
        let mut already_reserved = false;
        if let Some(row) = row {
            let old: Decision = serde_json::from_value(row.get("decision"))?;
            let state: String = row.get("state");
            if state == "AUTHORIZED" {
                if row.get::<DateTime<Utc>, _>("lease_deadline")
                    > row.get::<DateTime<Utc>, _>("now")
                {
                    if old.owner != *owner {
                        return Err(Error::Conflict);
                    }
                    tx.commit().await?;
                    return Ok(old);
                }
                // Online reauthorization fences the previous decision generation.
                already_reserved = row.get("reservation_held");
            } else if state != "NOT_INITIATED" {
                return Err(Error::Uncertain);
            }
            generation = old.generation.checked_add(1).ok_or(Error::Denied)?;
        }
        if count >= limit {
            return Err(Error::Denied);
        }
        let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
            .fetch_one(&mut *tx)
            .await?;
        let remaining = (b.deadline - now).num_milliseconds();
        let lease_ms = remaining.min(MAX_DISPATCH_LEASE_MS as i64);
        if lease_ms <= 0 {
            return Err(Error::Denied);
        }
        if !already_reserved {
            crate::runtime::reserve_budget(&mut tx, b).await?;
            let changed=sqlx::query("UPDATE workflow_ops.workflow_action_authority_t SET reserved=reserved+1 WHERE host_id=$1 AND run_id=$2 AND reserved+used<action_limit")
                .bind(b.host_id).bind(b.run_id).execute(&mut *tx).await?.rows_affected();
            if changed != 1 {
                return Err(Error::Denied);
            }
        } else {
            sqlx::query("UPDATE workflow_ops.workflow_action_dispatch_t SET reservation_held=false WHERE host_id=$1 AND action_id=$2 AND generation=$3")
                .bind(b.host_id).bind(b.action_id).bind(generation-1).execute(&mut *tx).await?;
        }
        let decision = Decision {
            binding: b.clone(),
            decision_id: Uuid::now_v7(),
            owner: owner.clone(),
            generation,
            lease_ms: lease_ms as u64,
        };
        sqlx::query("INSERT INTO workflow_ops.workflow_action_dispatch_t(host_id,action_id,generation,decision_id,owner,decision,state,authorized_at,lease_deadline) VALUES($1,$2,$3,$4,$5,$6,'AUTHORIZED',$7,$8)")
            .bind(b.host_id).bind(b.action_id).bind(generation).bind(decision.decision_id).bind(serde_json::to_value(owner)?).bind(serde_json::to_value(&decision)?).bind(now).bind(now+chrono::Duration::milliseconds(lease_ms)).execute(&mut *tx).await?;
        sqlx::query("UPDATE workflow_ops.workflow_action_permit_t SET authorization_count=authorization_count+1 WHERE host_id=$1 AND action_id=$2")
            .bind(b.host_id).bind(b.action_id).execute(&mut *tx).await?;
        Self::audit(&mut tx, b, Some(generation), "AUTHORIZED").await?;
        tx.commit().await?;
        Ok(decision)
    }
    /// True only for the *new* SEND_INTENT transition. Lost acknowledgements
    /// recover via status, never by granting permission again.
    pub async fn begin(&self, d: &Decision) -> Result<bool, Error> {
        let mut tx = self.pool.begin().await?;
        crate::owners::lock_owner(&mut tx, &d.owner).await?;
        self.authority(&mut tx, &d.binding, true).await?;
        self.permit(&mut tx, &d.binding, true).await?;
        let row = self.current(&mut tx, d).await?;
        if row.get::<String, _>("state") != "AUTHORIZED" {
            tx.commit().await?;
            return Ok(false);
        }
        if row.get::<DateTime<Utc>, _>("lease_deadline") <= row.get::<DateTime<Utc>, _>("now") {
            return Err(Error::Denied);
        }
        sqlx::query("UPDATE workflow_ops.workflow_action_dispatch_t SET state='SEND_INTENT' WHERE decision_id=$1").bind(d.decision_id).execute(&mut *tx).await?;
        Self::audit(&mut tx, &d.binding, Some(d.generation), "SEND_INTENT").await?;
        tx.commit().await?;
        Ok(true)
    }
    async fn current(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        d: &Decision,
    ) -> Result<sqlx::postgres::PgRow, Error> {
        let row=sqlx::query("SELECT *,clock_timestamp() AS now FROM workflow_ops.workflow_action_dispatch_t WHERE host_id=$1 AND action_id=$2 ORDER BY generation DESC LIMIT 1 FOR UPDATE")
            .bind(d.binding.host_id).bind(d.binding.action_id).fetch_optional(&mut **tx).await?.ok_or(Error::Denied)?;
        if serde_json::from_value::<Decision>(row.get("decision"))? != *d {
            return Err(Error::Conflict);
        }
        Ok(row)
    }
    /// Caller authentication must require the original Gateway's current peer,
    /// service, replica and boot. User expiry/revocation does not prevent cleanup.
    pub async fn complete(&self, c: &Completion) -> Result<(), Error> {
        if !matches!(
            c.outcome,
            DispatchState::NotInitiated
                | DispatchState::Uncertain
                | DispatchState::Succeeded
                | DispatchState::Failed
        ) || c.evidence_digest.as_deref().is_some_and(|s| !is_digest(s))
            || matches!(c.outcome, DispatchState::Succeeded | DispatchState::Failed)
                && c.evidence_digest.is_none()
        {
            return Err(Error::Denied);
        }
        let d = &c.decision;
        let b = &d.binding;
        let mut tx = self.pool.begin().await?;
        crate::owners::lock_owner(&mut tx, &d.owner).await?;
        self.authority(&mut tx, b, false).await?;
        self.permit(&mut tx, b, false).await?;
        // First replay an exact historical completion, without touching the
        // reservation of a newer authorization generation.
        let old=sqlx::query("SELECT decision,state,evidence_digest FROM workflow_ops.workflow_action_dispatch_t WHERE decision_id=$1")
            .bind(d.decision_id).fetch_optional(&mut *tx).await?.ok_or(Error::Denied)?;
        if serde_json::from_value::<Decision>(old.get("decision"))? != *d {
            return Err(Error::Conflict);
        }
        let outcome = serde_json::to_value(c.outcome)?
            .as_str()
            .ok_or(Error::Denied)?
            .to_owned();
        if old.get::<String, _>("state") == outcome {
            if old.get::<Option<String>, _>("evidence_digest") != c.evidence_digest {
                return Err(Error::Conflict);
            }
            tx.commit().await?;
            return Ok(());
        }
        let row = self.current(&mut tx, d).await?;
        if row.get::<String, _>("state") != "SEND_INTENT" {
            return Err(Error::Conflict);
        }
        let held: bool = row.get("reservation_held");
        // UNCERTAIN keeps its reservation. A reconciler must supply verified
        // target evidence; lease expiry never turns it into a retryable request.
        let release = c.outcome != DispatchState::Uncertain;
        if release && held {
            crate::runtime::finish_budget(&mut tx, b, c.outcome != DispatchState::NotInitiated)
                .await?;
            let spent = if c.outcome == DispatchState::NotInitiated {
                0i64
            } else {
                1
            };
            sqlx::query("UPDATE workflow_ops.workflow_action_authority_t SET reserved=reserved-1,used=used+$3 WHERE host_id=$1 AND run_id=$2")
                .bind(b.host_id).bind(b.run_id).bind(spent).execute(&mut *tx).await?;
        }
        sqlx::query("UPDATE workflow_ops.workflow_action_dispatch_t SET state=$2,evidence_digest=$3,reservation_held=$4 WHERE decision_id=$1")
            .bind(d.decision_id).bind(&outcome).bind(&c.evidence_digest).bind(held && !release).execute(&mut *tx).await?;
        Self::audit(&mut tx, b, Some(d.generation), &outcome).await?;
        tx.commit().await?;
        Ok(())
    }
    /// Status is metadata only; business-result disclosure belongs to the
    /// receiver's current user/policy checks, not a cached action receipt.
    pub async fn status(&self, d: &Decision) -> Result<DispatchState, Error> {
        let mut tx = self.pool.begin().await?;
        self.authority(&mut tx, &d.binding, true).await?;
        self.permit(&mut tx, &d.binding, true).await?;
        let row = self.current(&mut tx, d).await?;
        let state = serde_json::from_value(serde_json::Value::String(row.get("state")))?;
        tx.commit().await?;
        Ok(state)
    }
}

impl Ledger {
    /// Resolution does not grant dispatch permission. Authorize subsequently
    /// re-locks and checks the exact stored binding with current authority.
    pub async fn resolve(&self, reference: &ActionReference) -> Result<Binding, Error> {
        let value:serde_json::Value=sqlx::query_scalar("SELECT binding FROM workflow_ops.workflow_action_permit_t WHERE host_id=$1 AND action_id=$2 AND active")
            .bind(reference.host_id).bind(reference.action_id).fetch_optional(&self.pool).await?.ok_or(Error::Denied)?;
        let binding: Binding = serde_json::from_value(value)?;
        binding.validate().map_err(|_| Error::Denied)?;
        if !reference.matches(&binding) {
            return Err(Error::Denied);
        }
        Ok(binding)
    }
}

impl Ledger {
    /// Recovery on another registered Gateway replica. This returns metadata,
    /// never a new send permission or stored business result.
    pub async fn latest_status(&self, b: &Binding, owner: &Owner) -> Result<DispatchState, Error> {
        let mut tx = self.pool.begin().await?;
        crate::owners::lock_owner(&mut tx, owner).await?;
        self.authority(&mut tx, b, true).await?;
        self.permit(&mut tx, b, true).await?;
        let state:String=sqlx::query_scalar("SELECT state FROM workflow_ops.workflow_action_dispatch_t WHERE host_id=$1 AND action_id=$2 ORDER BY generation DESC LIMIT 1")
            .bind(b.host_id).bind(b.action_id).fetch_optional(&mut *tx).await?.ok_or(Error::Denied)?;
        let state = serde_json::from_value(serde_json::Value::String(state))?;
        tx.commit().await?;
        Ok(state)
    }
    pub async fn record_denial(&self, r: &ActionReference, stage: &str) -> Result<(), Error> {
        let event = match stage {
            "authorize" => "AUTHORIZE_DENIED",
            "begin" => "BEGIN_DENIED",
            _ => "STATUS_DENIED",
        };
        sqlx::query("INSERT INTO workflow_ops.workflow_action_audit_t(event_id,host_id,action_id,event_type) VALUES($1,$2,$3,$4)")
            .bind(Uuid::now_v7()).bind(r.host_id).bind(r.action_id).bind(event).execute(&self.pool).await?;
        Ok(())
    }
}

impl Ledger {
    pub async fn inspect(&self, b: &Binding, owner: &Owner) -> Result<(), Error> {
        let mut tx = self.pool.begin().await?;
        crate::owners::lock_owner(&mut tx, owner).await?;
        self.authority(&mut tx, b, true).await?;
        self.permit(&mut tx, b, true).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Resolve immutable metadata for an authenticated receipt producer. This
    /// grants no dispatch permission and deliberately works after run expiry or
    /// cancellation so a previously uncertain reservation can be settled.
    pub async fn reconciliation_binding(
        &self,
        host: Uuid,
        action: Uuid,
        generation: i64,
    ) -> Result<Binding, Error> {
        if host.is_nil() || action.is_nil() || generation <= 0 {
            return Err(Error::Denied);
        }
        let row = sqlx::query(
            "SELECT p.binding,d.state
               FROM workflow_ops.workflow_action_permit_t p
               JOIN workflow_ops.workflow_action_dispatch_t d
                 ON d.host_id=p.host_id AND d.action_id=p.action_id
              WHERE p.host_id=$1 AND p.action_id=$2 AND d.generation=$3",
        )
        .bind(host)
        .bind(action)
        .bind(generation)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(Error::Denied)?;
        if !matches!(
            row.get::<String, _>("state").as_str(),
            "UNCERTAIN" | "SUCCEEDED" | "FAILED"
        ) {
            return Err(Error::Denied);
        }
        let binding: Binding = serde_json::from_value(row.get("binding"))?;
        binding.validate().map_err(|_| Error::Denied)?;
        Ok(binding)
    }

    /// Settle an uncertain action from qualified target evidence. Lease expiry,
    /// a Gateway restart, or a caller assertion can never enter this path.
    pub async fn reconcile(&self, receipt: &Reconciliation) -> Result<(), Error> {
        if receipt.host_id.is_nil()
            || receipt.action_id.is_nil()
            || receipt.generation <= 0
            || !matches!(
                receipt.outcome,
                DispatchState::Succeeded | DispatchState::Failed
            )
            || !is_digest(&receipt.evidence_digest)
        {
            return Err(Error::Denied);
        }
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "SELECT p.binding,d.state,d.evidence_digest,d.reservation_held
               FROM workflow_ops.workflow_action_permit_t p
               JOIN workflow_ops.workflow_action_dispatch_t d
                 ON d.host_id=p.host_id AND d.action_id=p.action_id
              WHERE p.host_id=$1 AND p.action_id=$2 AND d.generation=$3
              FOR UPDATE OF p,d",
        )
        .bind(receipt.host_id)
        .bind(receipt.action_id)
        .bind(receipt.generation)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::Denied)?;
        let binding: Binding = serde_json::from_value(row.get("binding"))?;
        binding.validate().map_err(|_| Error::Denied)?;
        self.authority(&mut tx, &binding, false).await?;
        let outcome = serde_json::to_value(receipt.outcome)?
            .as_str()
            .ok_or(Error::Denied)?
            .to_owned();
        let state: String = row.get("state");
        if state == outcome {
            if row.get::<Option<String>, _>("evidence_digest")
                != Some(receipt.evidence_digest.clone())
            {
                return Err(Error::Conflict);
            }
            tx.commit().await?;
            return Ok(());
        }
        if state != "UNCERTAIN" || !row.get::<bool, _>("reservation_held") {
            return Err(Error::Conflict);
        }
        crate::runtime::finish_budget(&mut tx, &binding, true).await?;
        let changed = sqlx::query(
            "UPDATE workflow_ops.workflow_action_authority_t
                SET reserved=reserved-1,used=used+1
              WHERE host_id=$1 AND run_id=$2 AND reserved>0",
        )
        .bind(binding.host_id)
        .bind(binding.run_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if changed != 1 {
            return Err(Error::Conflict);
        }
        sqlx::query(
            "UPDATE workflow_ops.workflow_action_dispatch_t
                SET state=$4,evidence_digest=$5,reservation_held=false
              WHERE host_id=$1 AND action_id=$2 AND generation=$3",
        )
        .bind(receipt.host_id)
        .bind(receipt.action_id)
        .bind(receipt.generation)
        .bind(&outcome)
        .bind(&receipt.evidence_digest)
        .execute(&mut *tx)
        .await?;
        Self::audit(
            &mut tx,
            &binding,
            Some(receipt.generation),
            if receipt.outcome == DispatchState::Succeeded {
                "RECONCILED_SUCCEEDED"
            } else {
                "RECONCILED_FAILED"
            },
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Resolve the live parent at the receiving service after Gateway has
    /// committed SEND_INTENT. The verified peer selects the current Gateway
    /// owner; no request header supplies depth, grant, class or generations.
    pub async fn receiver_parent(
        &self,
        host: Uuid,
        action: Uuid,
        peer_sha256: &str,
        gateway_service: &str,
    ) -> Result<Binding, Error> {
        if host.is_nil() || action.is_nil() || peer_sha256.is_empty() || gateway_service.is_empty()
        {
            return Err(Error::Denied);
        }
        let mut tx = self.pool.begin().await?;
        let current: serde_json::Value = sqlx::query_scalar(
            "SELECT jsonb_build_object('gatewayService',gateway_service,'replica',replica,'boot',boot,'fencingGeneration',fencing_generation) FROM workflow_ops.workflow_gateway_owner_t WHERE peer_sha256=$1 AND gateway_service=$2 AND active FOR SHARE",
        )
        .bind(peer_sha256)
        .bind(gateway_service)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::Denied)?;
        let owner: Owner = serde_json::from_value(current)?;
        let value: serde_json::Value = sqlx::query_scalar(
            "SELECT binding FROM workflow_ops.workflow_action_permit_t WHERE host_id=$1 AND action_id=$2 AND active FOR SHARE",
        )
        .bind(host)
        .bind(action)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::Denied)?;
        let binding: Binding = serde_json::from_value(value)?;
        binding.validate().map_err(|_| Error::Denied)?;
        self.authority(&mut tx, &binding, true).await?;
        self.permit(&mut tx, &binding, true).await?;
        let dispatch_owner: serde_json::Value = sqlx::query_scalar(
            "SELECT owner FROM workflow_ops.workflow_action_dispatch_t WHERE host_id=$1 AND action_id=$2 AND state='SEND_INTENT' ORDER BY generation DESC LIMIT 1 FOR SHARE",
        )
        .bind(host)
        .bind(action)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::Denied)?;
        if serde_json::from_value::<Owner>(dispatch_owner)? != owner {
            return Err(Error::Denied);
        }
        tx.commit().await?;
        Ok(binding)
    }

    /// Read-only authorization for a receiving API/resource. The receiver has
    /// its own verified app/peer identity at the API boundary; this method also
    /// proves that the dispatch belongs to a currently fenced Gateway owner.
    pub async fn receiver_binding(&self, host: Uuid, action: Uuid) -> Result<Binding, Error> {
        if host.is_nil() || action.is_nil() {
            return Err(Error::Denied);
        }
        let mut tx = self.pool.begin().await?;
        let value: serde_json::Value = sqlx::query_scalar(
            "SELECT binding FROM workflow_ops.workflow_action_permit_t WHERE host_id=$1 AND action_id=$2 AND active FOR SHARE",
        )
        .bind(host)
        .bind(action)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::Denied)?;
        let binding: Binding = serde_json::from_value(value)?;
        binding.validate().map_err(|_| Error::Denied)?;
        self.authority(&mut tx, &binding, true).await?;
        self.permit(&mut tx, &binding, true).await?;
        let owner_value: serde_json::Value = sqlx::query_scalar(
            "SELECT owner FROM workflow_ops.workflow_action_dispatch_t WHERE host_id=$1 AND action_id=$2 AND state='SEND_INTENT' ORDER BY generation DESC LIMIT 1 FOR SHARE",
        )
        .bind(host)
        .bind(action)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(Error::Denied)?;
        let owner: Owner = serde_json::from_value(owner_value)?;
        crate::owners::lock_owner(&mut tx, &owner).await?;
        tx.commit().await?;
        Ok(binding)
    }
}
