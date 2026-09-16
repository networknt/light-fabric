//! Storage primitives for fixed publication actions, not an authorization API.
//! Callers must validate owner, active stage, pinned policy and retained bytes in
//! the same transaction before claiming. Never dispatch before that transaction
//! commits. A committed intent permits exactly one send; all later attempts are
//! read-only provider reconciliation, even if no remote result is found yet.
use development_workflow_contract::publication::{EffectState, PublicationEffect};
use serde_json::Value;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::development_store::{StoreResult, check};

#[derive(Debug, PartialEq)]
pub enum PublicationClaim {
    Dispatch,
    Reconcile,
    Confirmed(Value),
}

/// The invocation and task are Workflow-owned, never selected by a worker.
pub struct PublicationJournalKey<'a> {
    pub host_id: Uuid,
    pub workflow_instance_id: Uuid,
    pub task_name: &'a str,
    pub effect: &'a PublicationEffect,
}

impl PublicationJournalKey<'_> {
    pub async fn claim(&self, tx: &mut Transaction<'_, Postgres>) -> StoreResult<PublicationClaim> {
        check(
            !self.host_id.is_nil()
                && !self.workflow_instance_id.is_nil()
                && !self.task_name.is_empty()
                && self.task_name.len() <= 255
                && !self.effect.id.is_empty()
                && self.effect.id.len() <= 255
                && self.effect.state == EffectState::Prepared,
            "invalid publication journal identity",
        )?;
        let inserted = sqlx::query(
            "INSERT INTO workflow_task_effect_t(host_id,workflow_instance_id,task_name,idempotency_key,request_digest)
             VALUES($1,$2,$3,$4,$5) ON CONFLICT DO NOTHING",
        )
        .bind(self.host_id).bind(self.workflow_instance_id).bind(self.task_name)
        .bind(&self.effect.id).bind(&self.effect.request_digest)
        .execute(&mut **tx).await?.rows_affected() == 1;
        let (digest, state, result): (String, String, Option<Value>) = sqlx::query_as(
            "SELECT request_digest,effect_state,result FROM workflow_task_effect_t
             WHERE host_id=$1 AND workflow_instance_id=$2 AND task_name=$3 AND idempotency_key=$4 FOR UPDATE",
        )
        .bind(self.host_id).bind(self.workflow_instance_id).bind(self.task_name)
        .bind(&self.effect.id).fetch_one(&mut **tx).await?;
        check(
            digest == self.effect.request_digest,
            "publication replay changed immutable request",
        )?;
        match (state.as_str(), result) {
            ("confirmed", Some(result)) => Ok(PublicationClaim::Confirmed(result)),
            ("possible", None) if inserted => Ok(PublicationClaim::Dispatch),
            ("possible", None) => Ok(PublicationClaim::Reconcile),
            _ => Err(crate::development_store::StoreError::Conflict(
                "invalid publication journal state",
            )),
        }
    }

    /// Only a provider result whose target/content proof has been verified may
    /// reach this operation. Persist its retained verification artifact in this
    /// transaction as well. A conflicting confirmation never overwrites proof.
    pub async fn confirm(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        verified_result: &Value,
    ) -> StoreResult<()> {
        check(
            verified_result.is_object(),
            "publication result must be an evidence object",
        )?;
        let changed = sqlx::query(
            "UPDATE workflow_task_effect_t SET effect_state='confirmed',result=$6,
             confirmed_ts=COALESCE(confirmed_ts,CURRENT_TIMESTAMP)
             WHERE host_id=$1 AND workflow_instance_id=$2 AND task_name=$3 AND idempotency_key=$4
               AND request_digest=$5 AND
               ((effect_state='possible' AND result IS NULL) OR (effect_state='confirmed' AND result=$6))",
        )
        .bind(self.host_id).bind(self.workflow_instance_id).bind(self.task_name)
        .bind(&self.effect.id).bind(&self.effect.request_digest).bind(verified_result)
        .execute(&mut **tx).await?.rows_affected();
        check(
            changed == 1,
            "publication confirmation missing or conflicting",
        )
    }
}
