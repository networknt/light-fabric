//! Fenced, database-clock timers. No worker or HTTP request waits for a deadline.
use chrono::{DateTime, Utc};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;
use workflow_core::models::duration::OneOfDurationOrIso8601Expression;

pub(crate) fn duration_seconds(value: &OneOfDurationOrIso8601Expression) -> Result<i32, String> {
    let OneOfDurationOrIso8601Expression::Iso8601Expression(literal) = value else {
        return Err("wait requires a canonical PT<n>S literal (1..=600 whole seconds)".into());
    };
    let digits = literal.strip_prefix("PT").and_then(|s| s.strip_suffix('S'));
    let valid = digits.filter(|s| {
        !s.is_empty()
            && s.len() <= 3
            && !s.starts_with('0')
            && s.bytes().all(|b| b.is_ascii_digit())
    });
    let seconds = valid.and_then(|s| s.parse::<i32>().ok());
    seconds
        .filter(|n| (1..=600).contains(n))
        .ok_or_else(|| "wait requires a canonical PT<n>S literal (1..=600 whole seconds)".into())
}

pub(crate) struct Parent {
    pub run: Option<Uuid>,
    pub deadline: Option<DateTime<Utc>>,
    pub blocked: Option<&'static str>,
    pub binding: Option<Uuid>,
    pub private: bool,
    pub private_lifetime: bool,
    pub authority_deadline: Option<DateTime<Utc>>,
}

/// Lock order shared with cancellation: invocation -> process -> task -> timer
/// -> action authority. Candidate scans never retain timer locks first.
pub(crate) async fn lock_parent(
    tx: &mut Transaction<'_, Postgres>,
    host: Uuid,
    process: Uuid,
) -> Result<Parent, sqlx::Error> {
    lock_parent_mode(tx, host, process, false).await
}

pub(crate) async fn try_lock_parent(
    tx: &mut Transaction<'_, Postgres>,
    host: Uuid,
    process: Uuid,
) -> Result<Parent, sqlx::Error> {
    lock_parent_mode(tx, host, process, true).await
}

async fn lock_parent_mode(
    tx: &mut Transaction<'_, Postgres>,
    host: Uuid,
    process: Uuid,
    nonblocking: bool,
) -> Result<Parent, sqlx::Error> {
    let lock = if nonblocking {
        "FOR UPDATE NOWAIT"
    } else {
        "FOR UPDATE"
    };
    let invocation = sqlx::query(&format!(
        "SELECT workflow_instance_id,binding_id,state,cancel_requested_ts,deadline_ts,
                response_policy_snapshot->>'acceptedAdmissionProfile' AS profile,
                response_policy_snapshot->'privateExecutionProfile'->>'version'='1' AS private_lifetime
           FROM workflow_invocation_t WHERE host_id=$1 AND process_id=$2 {lock}",
    ))
    .bind(host)
    .bind(process)
    .fetch_optional(&mut **tx)
    .await?;
    let p = sqlx::query(&format!(
        "SELECT status_code::text,active,deadline_ts,started_ts,definition_snapshot
           FROM process_info_t WHERE host_id=$1 AND process_id=$2 {lock}",
    ))
    .bind(host)
    .bind(process)
    .fetch_one(&mut **tx)
    .await?;
    let mut deadline: Option<DateTime<Utc>> = p.get("deadline_ts");
    let mut blocked = if !p.get::<bool, _>("active")
        || !matches!(p.get::<String, _>("status_code").as_str(), "A" | "W")
    {
        Some("WORKFLOW_TIMER_CANCELLED")
    } else {
        None
    };
    let mut run = None;
    let mut binding = None;
    let mut private = false;
    let mut private_lifetime = false;
    if let Some(i) = invocation {
        run = Some(i.get("workflow_instance_id"));
        binding = Some(i.get("binding_id"));
        private = i.get::<Option<String>, _>("profile").as_deref() == Some("portal_execution");
        private_lifetime = i
            .get::<Option<bool>, _>("private_lifetime")
            .unwrap_or(false);
        private |= private_lifetime;
        // Match admission, task timeout and bound dispatch: v1's invocation
        // deadline bounds the start envelope, not private execution lifetime.
        if !private_lifetime {
            let d: DateTime<Utc> = i.get("deadline_ts");
            deadline = Some(deadline.map_or(d, |old| old.min(d)));
        }
        if i.get::<Option<DateTime<Utc>>, _>("cancel_requested_ts")
            .is_some()
            || !matches!(
                i.get::<String, _>("state").as_str(),
                "ACCEPTED" | "RUNNING" | "WAITING"
            )
        {
            blocked = Some("WORKFLOW_TIMER_CANCELLED");
        }
    }
    // Preparation's absolute budget is anchored to the admitted process, not
    // each poll or restart. Names alone cannot select an authority profile.
    let definition: Option<serde_json::Value> = p.get("definition_snapshot");
    if definition
        .as_ref()
        .and_then(|d| d.pointer("/document/metadata/developmentInputProfile"))
        .and_then(serde_json::Value::as_str)
        == Some("capture-v1")
    {
        let d = p.get::<DateTime<Utc>, _>("started_ts") + chrono::Duration::seconds(600);
        deadline = Some(deadline.map_or(d, |old| old.min(d)));
    }
    Ok(Parent {
        run,
        deadline,
        blocked,
        binding,
        private,
        private_lifetime,
        authority_deadline: None,
    })
}

pub(crate) async fn check_authority(
    tx: &mut Transaction<'_, Postgres>,
    host: Uuid,
    parent: &mut Parent,
) -> Result<(), sqlx::Error> {
    check_authority_mode(tx, host, parent, false).await
}

pub(crate) async fn try_check_authority(
    tx: &mut Transaction<'_, Postgres>,
    host: Uuid,
    parent: &mut Parent,
) -> Result<(), sqlx::Error> {
    check_authority_mode(tx, host, parent, true).await
}

async fn check_authority_mode(
    tx: &mut Transaction<'_, Postgres>,
    host: Uuid,
    parent: &mut Parent,
    nonblocking: bool,
) -> Result<(), sqlx::Error> {
    let lock = if nonblocking {
        "FOR UPDATE NOWAIT"
    } else {
        "FOR UPDATE"
    };
    if let Some(run) = parent.run {
        let authority = sqlx::query(&format!("SELECT active,deadline FROM workflow_action_authority_t WHERE host_id=$1 AND run_id=$2 {lock}"))
            .bind(host).bind(run).fetch_optional(&mut **tx).await?;
        if let Some(a) = authority {
            let d: DateTime<Utc> = a.get("deadline");
            parent.authority_deadline = Some(d);
            if !a.get::<bool, _>("active") {
                parent.blocked = Some("AUTHORITY_BLOCKED");
            }
        } else if parent.private {
            parent.blocked = Some("AUTHORITY_BLOCKED");
        }
    }
    Ok(())
}

pub(crate) async fn arm(
    tx: &mut Transaction<'_, Postgres>,
    host: Uuid,
    process: Uuid,
    task: Uuid,
    owner: Uuid,
    fence: i64,
    seconds: i32,
) -> Result<(), sqlx::Error> {
    let mut parent = lock_parent(tx, host, process).await?;
    let t = sqlx::query("SELECT status_code::text,lease_owner,lease_fencing_token,lease_expires_ts,deadline_ts,active FROM task_info_t WHERE host_id=$1 AND task_id=$2 AND process_id=$3 FOR UPDATE")
        .bind(host).bind(task).bind(process).fetch_one(&mut **tx).await?;
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut **tx)
        .await?;
    if t.get::<Option<Uuid>, _>("lease_owner") != Some(owner)
        || t.get::<i64, _>("lease_fencing_token") != fence
        || t.get::<Option<DateTime<Utc>>, _>("lease_expires_ts")
            .is_none_or(|d| d <= now)
        || t.get::<String, _>("status_code") != "A"
        || !t.get::<bool, _>("active")
    {
        return Err(sqlx::Error::Protocol(
            "WORKFLOW_STALE_HOST_TASK_FENCE".into(),
        ));
    }
    if let Some(d) = t.get::<Option<DateTime<Utc>>, _>("deadline_ts") {
        parent.deadline = Some(parent.deadline.map_or(d, |old| old.min(d)));
    }
    // Lock the timer before authority, even when recovering an already-pinned
    // intent. The original clock/deadline are never changed on conflict.
    sqlx::query("INSERT INTO workflow_task_timer_t(host_id,task_id,process_id,duration_seconds,armed_at,wake_at,effective_deadline,state,task_fence) VALUES($1,$2,$3,$4,$5,$5+make_interval(secs=>$4::double precision),$6,'ARMED',$7) ON CONFLICT(host_id,task_id) DO NOTHING")
        .bind(host).bind(task).bind(process).bind(seconds).bind(now).bind(parent.deadline).bind(fence)
        .execute(&mut **tx).await?;
    let timer = sqlx::query("SELECT duration_seconds,state FROM workflow_task_timer_t WHERE host_id=$1 AND task_id=$2 FOR UPDATE")
        .bind(host).bind(task).fetch_one(&mut **tx).await?;
    if timer.get::<i32, _>("duration_seconds") != seconds
        || timer.get::<String, _>("state") != "ARMED"
    {
        return Err(sqlx::Error::Protocol(
            "WORKFLOW_TIMER_INTENT_CONFLICT".into(),
        ));
    }
    check_authority(tx, host, &mut parent).await?;
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut **tx)
        .await?;
    if t.get::<Option<DateTime<Utc>>, _>("lease_expires_ts")
        .is_none_or(|d| d <= now)
    {
        return Err(sqlx::Error::Protocol(
            "WORKFLOW_STALE_HOST_TASK_FENCE".into(),
        ));
    }
    if parent.deadline.is_none() && !parent.private_lifetime {
        parent.blocked = Some("WORKFLOW_TIMER_DEADLINE_REQUIRED");
    }
    if parent.deadline.is_some_and(|d| d <= now)
        || parent.authority_deadline.is_some_and(|d| d <= now)
    {
        parent.blocked = Some("WORKFLOW_TIMEOUT");
    }
    if let Some(code) = parent.blocked {
        return stop(tx, host, process, task, code).await;
    }
    sqlx::query("UPDATE workflow_task_timer_t SET effective_deadline=LEAST(effective_deadline,$3),wake_at=LEAST(wake_at,$3) WHERE host_id=$1 AND task_id=$2")
        .bind(host).bind(task).bind(parent.deadline).execute(&mut **tx).await?;
    sqlx::query("UPDATE task_info_t SET status_code='W',completed_ts=NULL,locked='N',lease_owner=NULL,lease_expires_ts=NULL,task_output=jsonb_build_object('status','waiting_for_timer'),update_ts=clock_timestamp() WHERE host_id=$1 AND task_id=$2")
        .bind(host).bind(task).execute(&mut **tx).await?;
    sqlx::query("UPDATE process_info_t SET status_code='W' WHERE host_id=$1 AND process_id=$2 AND status_code='A'")
        .bind(host).bind(process).execute(&mut **tx).await?;
    sqlx::query("UPDATE workflow_invocation_t SET state='WAITING',updated_ts=clock_timestamp(),state_version=state_version+1 WHERE host_id=$1 AND process_id=$2 AND state IN('ACCEPTED','RUNNING','WAITING')")
        .bind(host).bind(process).execute(&mut **tx).await?;
    Ok(())
}

pub(crate) async fn stop(
    tx: &mut Transaction<'_, Postgres>,
    host: Uuid,
    process: Uuid,
    task: Uuid,
    code: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE task_info_t SET status_code='F',result_code=$3,locked='N',lease_owner=NULL,lease_expires_ts=NULL,completed_ts=clock_timestamp(),task_output=jsonb_build_object('code',$3::text) WHERE host_id=$1 AND task_id=$2")
        .bind(host).bind(task).bind(code).execute(&mut **tx).await?;
    sqlx::query("UPDATE workflow_task_timer_t SET state=$3,updated_ts=clock_timestamp() WHERE host_id=$1 AND task_id=$2 AND state<>'FIRED'")
        .bind(host).bind(task).bind(match code { "WORKFLOW_TIMEOUT" => "EXPIRED", "WORKFLOW_TIMER_FAILED" => "FAILED", _ => "CANCELLED" })
        .execute(&mut **tx).await?;
    sqlx::query("UPDATE process_info_t SET status_code='F',custom_status_code=$3,error_info=$3,completed_ts=clock_timestamp() WHERE host_id=$1 AND process_id=$2 AND status_code IN('A','W')")
        .bind(host).bind(process).bind(code).execute(&mut **tx).await?;
    sqlx::query("UPDATE workflow_invocation_t SET state='FAILED',terminal_ts=clock_timestamp(),updated_ts=clock_timestamp(),state_version=state_version+1,normalized_error=jsonb_build_object('code',$3::text,'message','durable timer stopped','retryable',false),user_authorization=NULL,user_authorization_exp=NULL WHERE host_id=$1 AND process_id=$2 AND state IN('ACCEPTED','RUNNING','WAITING')")
        .bind(host).bind(process).bind(code).execute(&mut **tx).await?;
    Ok(())
}

pub(crate) fn is_lock_contention(error: &(dyn std::error::Error + 'static)) -> bool {
    error.downcast_ref::<sqlx::Error>().is_some_and(
        |error| matches!(error, sqlx::Error::Database(db) if db.code().as_deref() == Some("55P03")),
    )
}

pub(crate) async fn record_wake_failure(
    pool: &sqlx::PgPool,
    host: Uuid,
    process: Uuid,
    task: Uuid,
    generation: i64,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout='1ms'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SET LOCAL statement_timeout='100ms'")
        .execute(&mut *tx)
        .await?;
    let _parent = try_lock_parent(&mut tx, host, process).await?;
    sqlx::query(
        "SELECT task_id FROM task_info_t WHERE host_id=$1 AND task_id=$2 FOR UPDATE NOWAIT",
    )
    .bind(host)
    .bind(task)
    .fetch_one(&mut *tx)
    .await?;
    let timer = sqlx::query("SELECT state,generation,wake_failure_count,retry_after_ts<=clock_timestamp() AS eligible FROM workflow_task_timer_t WHERE host_id=$1 AND task_id=$2 FOR UPDATE NOWAIT")
        .bind(host).bind(task).fetch_one(&mut *tx).await?;
    if timer.get::<String, _>("state") != "ARMED"
        || timer.get::<i64, _>("generation") != generation
        || !timer.get::<bool, _>("eligible")
    {
        return Ok(());
    }
    let failures = timer.get::<i16, _>("wake_failure_count") + 1;
    // Fixed diagnostic classification, never provider/error content. Concurrent
    // reports for the same retry epoch cannot consume multiple retries.
    sqlx::query("UPDATE workflow_task_timer_t SET wake_failure_count=$3,retry_after_ts=clock_timestamp()+make_interval(secs=>$3::double precision),last_failure_code='WORKFLOW_TIMER_WAKE_FAILED',updated_ts=clock_timestamp() WHERE host_id=$1 AND task_id=$2")
        .bind(host).bind(task).bind(failures).execute(&mut *tx).await?;
    if failures == 3 {
        stop(&mut tx, host, process, task, "WORKFLOW_TIMER_FAILED").await?;
    }
    tx.commit().await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn canonical_duration_bounds_and_precision() {
        for n in [1, 5, 600] {
            assert_eq!(
                duration_seconds(&OneOfDurationOrIso8601Expression::Iso8601Expression(
                    format!("PT{n}S")
                ))
                .unwrap(),
                n
            );
        }
        for s in [
            "PT0S",
            "PT601S",
            "PT01S",
            "PT1.0S",
            "PT0.5S",
            "PT-1S",
            "PT+1S",
            " PT1S",
            "PT1S ",
            "PT1M",
            "PT1H",
            "P1D",
            "${ .delay }",
            "PT999999999999999999999S",
        ] {
            assert!(
                duration_seconds(&OneOfDurationOrIso8601Expression::Iso8601Expression(
                    s.into()
                ))
                .is_err(),
                "{s}"
            );
        }
        assert!(
            duration_seconds(&OneOfDurationOrIso8601Expression::Duration(
                Default::default()
            ))
            .is_err()
        );
    }
}
