//! Integration with the existing Workflow invocation/budget authority. Action
//! tables are never a replacement for cancellation and budget state here.
use crate::{
    Binding,
    ledger::{Error, Ledger},
};
use chrono::{DateTime, Utc};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

pub(crate) async fn lock_live_invocation(
    tx: &mut Transaction<'_, Postgres>,
    b: &Binding,
) -> Result<(), Error> {
    let row=sqlx::query("SELECT i.end_user_subject,i.policy_digest,i.response_policy_digest,i.execution_class,i.deadline_ts,i.response_policy_snapshot,i.state,i.cancel_requested_ts,p.deadline_ts AS process_deadline_ts FROM workflow_ops.workflow_invocation_t i JOIN workflow_ops.process_info_t p ON p.host_id=i.host_id AND p.process_id=i.process_id WHERE i.host_id=$1 AND i.workflow_instance_id=$2 FOR SHARE OF i")
        .bind(b.host_id).bind(b.run_id).fetch_optional(&mut **tx).await?.ok_or(Error::Denied)?;
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut **tx)
        .await?;
    if !matches!(
        row.get::<&str, _>("state"),
        "ACCEPTED" | "RUNNING" | "WAITING"
    ) || row
        .get::<Option<DateTime<Utc>>, _>("cancel_requested_ts")
        .is_some()
        || row.get::<String, _>("end_user_subject") != b.user_id.to_string()
        || row.get::<String, _>("policy_digest") != b.policy_digest
        || row.get::<String, _>("response_policy_digest") != b.disclosure_digest
        || row.get::<String, _>("execution_class")
            != serde_json::to_value(b.execution_class)?
                .as_str()
                .ok_or(Error::Denied)?
        || (row.get::<serde_json::Value, _>("response_policy_snapshot")["privateExecutionProfile"]
            ["version"]
            != 1
            && row.get::<DateTime<Utc>, _>("deadline_ts") < b.deadline)
        || row
            .get::<Option<DateTime<Utc>>, _>("process_deadline_ts")
            .is_some_and(|deadline| deadline < b.deadline)
        || b.deadline <= now
    {
        return Err(Error::Denied);
    }
    let row=sqlx::query("SELECT generation,deadline_ts,lifetime_version,task_attempt_used,task_attempt_reserved,task_attempt_limit,nested_call_used,nested_call_reserved,nested_call_limit,byte_used,byte_reserved,byte_limit,cost_unit_used,cost_unit_reserved,cost_unit_limit FROM workflow_ops.workflow_invocation_budget_t WHERE host_id=$1 AND workflow_instance_id=$2 FOR UPDATE")
        .bind(b.host_id).bind(b.run_id).fetch_optional(&mut **tx).await?.ok_or(Error::Denied)?;
    if row.get::<i64, _>("generation") != b.budget_generation
        || (row.get::<Option<i16>, _>("lifetime_version") != Some(1)
            && row.get::<DateTime<Utc>, _>("deadline_ts") < b.deadline)
    {
        return Err(Error::Denied);
    }
    for (used, reserved, limit) in [
        (
            "task_attempt_used",
            "task_attempt_reserved",
            "task_attempt_limit",
        ),
        (
            "nested_call_used",
            "nested_call_reserved",
            "nested_call_limit",
        ),
        ("byte_used", "byte_reserved", "byte_limit"),
        ("cost_unit_used", "cost_unit_reserved", "cost_unit_limit"),
    ] {
        if row
            .get::<i64, _>(used)
            .checked_add(row.get::<i64, _>(reserved))
            .is_none_or(|n| n > row.get::<i64, _>(limit))
        {
            return Err(Error::Denied);
        }
    }
    Ok(())
}
impl Ledger {
    /// Trusted admission only, after broker.bind_run has verified the consent
    /// binding. Replays require the same grant/user; no public API exposes this.
    pub async fn admit_run(
        &self,
        host: Uuid,
        run: Uuid,
        grant: Uuid,
        user: Uuid,
    ) -> Result<(), Error> {
        if [host, run, grant, user].iter().any(Uuid::is_nil) {
            return Err(Error::Denied);
        }
        let mut tx = self.pool.begin().await?;
        Self::admit_run_in(&mut tx, host, run, grant, user).await?;
        tx.commit().await?;
        Ok(())
    }
    pub async fn admit_run_in(
        tx: &mut Transaction<'_, Postgres>,
        host: Uuid,
        run: Uuid,
        grant: Uuid,
        user: Uuid,
    ) -> Result<(), Error> {
        let row=sqlx::query("SELECT i.end_user_subject,i.deadline_ts,b.generation,b.nested_call_limit FROM workflow_ops.workflow_invocation_t i JOIN workflow_ops.workflow_invocation_budget_t b ON b.host_id=i.host_id AND b.workflow_instance_id=i.workflow_instance_id WHERE i.host_id=$1 AND i.workflow_instance_id=$2 AND i.state IN ('ACCEPTED','RUNNING','WAITING') AND i.cancel_requested_ts IS NULL AND i.deadline_ts>clock_timestamp() AND (b.deadline_ts>=i.deadline_ts OR (b.lifetime_version=1 AND i.response_policy_snapshot->'privateExecutionProfile'->>'version'='1')) FOR SHARE OF i,b")
            .bind(host).bind(run).fetch_optional(&mut **tx).await?.ok_or(Error::Denied)?;
        if row.get::<String, _>("end_user_subject") != user.to_string() {
            return Err(Error::Denied);
        }
        sqlx::query("INSERT INTO workflow_ops.workflow_action_authority_t(host_id,run_id,grant_id,user_id,grant_generation,run_generation,budget_generation,active,deadline,action_limit) VALUES($1,$2,$3,$4,1,1,$5,true,$6,$7) ON CONFLICT DO NOTHING")
            .bind(host).bind(run).bind(grant).bind(user).bind(row.get::<i64,_>("generation"))
            .bind(row.get::<DateTime<Utc>,_>("deadline_ts")).bind(row.get::<i64,_>("nested_call_limit")).execute(&mut **tx).await?;
        let stored:(Uuid,Uuid)=sqlx::query_as("SELECT grant_id,user_id FROM workflow_ops.workflow_action_authority_t WHERE host_id=$1 AND run_id=$2")
            .bind(host).bind(run).fetch_one(&mut **tx).await?;
        if stored != (grant, user) {
            return Err(Error::Conflict);
        }
        Ok(())
    }

    /// Admit a child from a stored parent action in the same transaction that
    /// accepted the child invocation. The child publication may have its own
    /// policy, but identity, grant, class, depth and deadlines can only narrow.
    pub async fn admit_child_run_in(
        tx: &mut Transaction<'_, Postgres>,
        host: Uuid,
        child_run: Uuid,
        user: Uuid,
        parent_action: Uuid,
    ) -> Result<Binding, Error> {
        let value: serde_json::Value = sqlx::query_scalar(
            "SELECT binding FROM workflow_ops.workflow_action_permit_t WHERE host_id=$1 AND action_id=$2 AND active FOR SHARE",
        )
        .bind(host)
        .bind(parent_action)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(Error::Denied)?;
        let parent: Binding = serde_json::from_value(value)?;
        if parent.user_id != user {
            return Err(Error::Denied);
        }
        let latest: String = sqlx::query_scalar(
            "SELECT state FROM workflow_ops.workflow_action_dispatch_t WHERE host_id=$1 AND action_id=$2 ORDER BY generation DESC LIMIT 1 FOR SHARE",
        )
        .bind(host)
        .bind(parent_action)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(Error::Denied)?;
        if latest != "SEND_INTENT" {
            return Err(Error::Denied);
        }
        lock_live_invocation(tx, &parent).await?;
        let child=sqlx::query("SELECT i.end_user_subject,i.deadline_ts,i.execution_class,i.permit_depth,b.generation,b.nested_call_limit FROM workflow_ops.workflow_invocation_t i JOIN workflow_ops.workflow_invocation_budget_t b ON b.host_id=i.host_id AND b.workflow_instance_id=i.workflow_instance_id WHERE i.host_id=$1 AND i.workflow_instance_id=$2 AND i.state IN('ACCEPTED','RUNNING','WAITING') AND i.cancel_requested_ts IS NULL AND i.deadline_ts>clock_timestamp() FOR SHARE OF i,b")
            .bind(host).bind(child_run).fetch_optional(&mut **tx).await?.ok_or(Error::Denied)?;
        let expected_depth = parent.depth.checked_add(1).ok_or(Error::Denied)?;
        let class = serde_json::to_value(parent.execution_class)?
            .as_str()
            .ok_or(Error::Denied)?
            .to_owned();
        if child.get::<String, _>("end_user_subject") != user.to_string()
            || child.get::<DateTime<Utc>, _>("deadline_ts") > parent.deadline
            || child.get::<String, _>("execution_class") != class
            || child.get::<i32, _>("permit_depth") != i32::from(expected_depth)
            || expected_depth > parent.maximum_depth
        {
            return Err(Error::Denied);
        }
        sqlx::query("INSERT INTO workflow_ops.workflow_action_authority_t(host_id,run_id,grant_id,user_id,grant_generation,run_generation,budget_generation,active,deadline,action_limit,parent_action_id,parent_run_id,depth,maximum_depth) VALUES($1,$2,$3,$4,$5,$6,$7,true,$8,$9,$10,$11,$12,$13) ON CONFLICT DO NOTHING")
            .bind(host).bind(child_run).bind(parent.grant_id).bind(user)
            .bind(parent.grant_generation).bind(parent.run_generation)
            .bind(child.get::<i64,_>("generation")).bind(child.get::<DateTime<Utc>,_>("deadline_ts"))
            .bind(child.get::<i64,_>("nested_call_limit")).bind(parent.action_id).bind(parent.run_id)
            .bind(i32::from(expected_depth)).bind(i32::from(parent.maximum_depth)).execute(&mut **tx).await?;
        let stored:(Uuid,Uuid,Option<Uuid>,Option<Uuid>,i32,i32)=sqlx::query_as("SELECT grant_id,user_id,parent_action_id,parent_run_id,depth,maximum_depth FROM workflow_ops.workflow_action_authority_t WHERE host_id=$1 AND run_id=$2")
            .bind(host).bind(child_run).fetch_one(&mut **tx).await?;
        if stored
            != (
                parent.grant_id,
                user,
                Some(parent.action_id),
                Some(parent.run_id),
                i32::from(expected_depth),
                i32::from(parent.maximum_depth),
            )
        {
            return Err(Error::Conflict);
        }
        Ok(parent)
    }
}

pub(crate) async fn lock_budget(
    tx: &mut Transaction<'_, Postgres>,
    b: &Binding,
) -> Result<(), Error> {
    let found:Option<i64>=sqlx::query_scalar("SELECT generation FROM workflow_ops.workflow_invocation_budget_t WHERE host_id=$1 AND workflow_instance_id=$2 FOR UPDATE")
        .bind(b.host_id).bind(b.run_id).fetch_optional(&mut **tx).await?;
    if found != Some(b.budget_generation) {
        return Err(Error::Denied);
    }
    Ok(())
}
pub(crate) async fn reserve_budget(
    tx: &mut Transaction<'_, Postgres>,
    b: &Binding,
) -> Result<(), Error> {
    let bytes = b
        .request_bytes
        .checked_add(b.response_byte_limit)
        .and_then(|v| i64::try_from(v).ok())
        .ok_or(Error::Denied)?;
    let cost = i64::try_from(b.cost_unit_limit).map_err(|_| Error::Denied)?;
    let n=sqlx::query("UPDATE workflow_ops.workflow_invocation_budget_t SET nested_call_reserved=nested_call_reserved+1,byte_reserved=byte_reserved+$4,cost_unit_reserved=cost_unit_reserved+$5,updated_ts=clock_timestamp() WHERE host_id=$1 AND workflow_instance_id=$2 AND generation=$3 AND (deadline_ts>clock_timestamp() OR (deadline_ts IS NULL AND lifetime_version=1)) AND nested_call_used+nested_call_reserved<nested_call_limit AND byte_used+byte_reserved<=byte_limit-$4 AND cost_unit_used+cost_unit_reserved<=cost_unit_limit-$5")
        .bind(b.host_id).bind(b.run_id).bind(b.budget_generation).bind(bytes).bind(cost)
        .execute(&mut **tx).await?.rows_affected();
    if n != 1 {
        return Err(Error::Denied);
    }
    Ok(())
}
pub(crate) async fn finish_budget(
    tx: &mut Transaction<'_, Postgres>,
    b: &Binding,
    spent: bool,
) -> Result<(), Error> {
    let bytes = i64::try_from(
        b.request_bytes
            .checked_add(b.response_byte_limit)
            .ok_or(Error::Denied)?,
    )
    .map_err(|_| Error::Denied)?;
    let cost = i64::try_from(b.cost_unit_limit).map_err(|_| Error::Denied)?;
    // Until the target has qualified cost receipts, charge the full declared
    // bound for initiated effects. Non-initiation alone refunds the reservation.
    let n=sqlx::query("UPDATE workflow_ops.workflow_invocation_budget_t SET nested_call_reserved=nested_call_reserved-1,byte_reserved=byte_reserved-$4,cost_unit_reserved=cost_unit_reserved-$5,nested_call_used=nested_call_used+$6,byte_used=byte_used+CASE WHEN $6=1 THEN $4 ELSE 0 END,cost_unit_used=cost_unit_used+CASE WHEN $6=1 THEN $5 ELSE 0 END,updated_ts=clock_timestamp() WHERE host_id=$1 AND workflow_instance_id=$2 AND generation=$3 AND nested_call_reserved>=1 AND byte_reserved>=$4 AND cost_unit_reserved>=$5")
        .bind(b.host_id).bind(b.run_id).bind(b.budget_generation).bind(bytes).bind(cost).bind(if spent{1i64}else{0})
        .execute(&mut **tx).await?.rows_affected();
    if n != 1 {
        return Err(Error::Conflict);
    }
    Ok(())
}
