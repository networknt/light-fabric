//! Retryable Portal delivery for authoring-time Tool access decisions.
use chrono::{DateTime, Utc};
use std::sync::Arc;

use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::approval_portal::{ActorEvidence, Client, DecisionRequest, Error};

pub async fn run(pool: PgPool, client: Arc<Client>) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
    loop {
        interval.tick().await;
        if let Err(error) = deliver_cycle(&pool, &client).await {
            tracing::warn!(error = %error, "Workflow Tool access delivery cycle failed");
        }
    }
}

async fn deliver_cycle(pool: &PgPool, client: &Client) -> Result<(), sqlx::Error> {
    let rows = sqlx::query(
        "SELECT host_id,request_id,request_digest,request_version,workflow_instance_id,decision_id,
                decision_kind,decision_payload_digest,approver_user_id,approver_claims_digest,
                task_id,task_asst_id,decision_comment
           FROM workflow_tool_access_approval_run_t
          WHERE delivery_state IN ('PENDING','BLOCKED')
            AND (last_attempt_ts IS NULL OR last_attempt_ts < CURRENT_TIMESTAMP - INTERVAL '15 seconds')
          ORDER BY decision_ts LIMIT 16",
    ).fetch_all(pool).await?;
    for row in rows {
        let host: Uuid = row.get("host_id");
        let request: Uuid = row.get("request_id");
        let decision: Uuid = row.get("decision_id");
        let claim = sqlx::query(
            "UPDATE workflow_tool_access_approval_run_t
                SET attempt_count=attempt_count+1,last_attempt_ts=CURRENT_TIMESTAMP
              WHERE host_id=$1 AND request_id=$2 AND decision_id=$3
                AND delivery_state IN ('PENDING','BLOCKED')
                AND (last_attempt_ts IS NULL OR last_attempt_ts < CURRENT_TIMESTAMP - INTERVAL '15 seconds')",
        ).bind(host).bind(request).bind(decision).execute(pool).await?;
        if claim.rows_affected() != 1 {
            continue;
        }
        let run: Uuid = row.get("workflow_instance_id");
        let approver: Uuid = row.get("approver_user_id");
        let mut actor = match ActorEvidence::new(
            host,
            request,
            "deliverWorkflowToolAccessDecision",
            &approver.to_string(),
            row.get::<String, _>("approver_claims_digest").as_str(),
        ) {
            Ok(actor) => actor,
            Err(_) => {
                block(pool, host, request, decision, "ACTOR_EVIDENCE").await?;
                continue;
            }
        };
        actor.accepted_instance_id = Some(run);
        actor.decision_id = Some(decision);
        let input = DecisionRequest {
            host_id: host,
            decision_id: decision,
            request_id: request,
            request_digest: row.get("request_digest"),
            accepted_instance_id: run,
            task_id: row.get("task_id"),
            task_asst_id: row.get("task_asst_id"),
            decision: row.get("decision_kind"),
            comment: row.get("decision_comment"),
            approver_subject: approver.to_string(),
            payload_digest: row.get("decision_payload_digest"),
        };
        match client.decide(&input, &actor).await {
            Ok(receipt) if receipt.request_version >= row.get::<i64, _>("request_version") => {
                acknowledge(pool, &input, &receipt.outcome, &receipt.committed_at).await?;
            }
            Ok(_) => block(pool, host, request, decision, "STALE_RECEIPT").await?,
            Err(Error::Denied | Error::Conflict) => {
                block(pool, host, request, decision, "PORTAL_DENIED").await?;
            }
            Err(_) => {
                block(pool, host, request, decision, "PORTAL_UNAVAILABLE").await?;
            }
        }
    }
    Ok(())
}

async fn block(
    pool: &PgPool,
    host: Uuid,
    request: Uuid,
    decision: Uuid,
    code: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE workflow_tool_access_approval_run_t
        SET delivery_state='BLOCKED',last_delivery_error=$1
        WHERE host_id=$2 AND request_id=$3 AND decision_id=$4 AND delivery_state<>'ACKED'",
    )
    .bind(code)
    .bind(host)
    .bind(request)
    .bind(decision)
    .execute(pool)
    .await?;
    Ok(())
}

async fn acknowledge(
    pool: &PgPool,
    input: &DecisionRequest,
    outcome: &str,
    committed_at: &str,
) -> Result<(), sqlx::Error> {
    if !matches!(outcome, "GRANTED" | "REJECTED" | "STALE") {
        return Err(sqlx::Error::Protocol(
            "invalid Portal approval outcome".into(),
        ));
    }
    let portal_committed = DateTime::parse_from_rfc3339(committed_at)
        .map_err(|_| sqlx::Error::Protocol("Portal approval commit time is invalid".into()))?
        .with_timezone(&Utc);
    let mut tx = pool.begin().await?;
    let current: Option<(Option<String>, Uuid, Uuid)> = sqlx::query_as(
        "SELECT delivery_state,task_id,task_asst_id
           FROM workflow_tool_access_approval_run_t
          WHERE host_id=$1 AND request_id=$2 AND decision_id=$3 FOR UPDATE",
    )
    .bind(input.host_id)
    .bind(input.request_id)
    .bind(input.decision_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some((state, task, assignment)) = current else {
        return Err(sqlx::Error::Protocol(
            "approval delivery record disappeared".into(),
        ));
    };
    if state.as_deref() == Some("ACKED") {
        return Ok(());
    }
    sqlx::query(
        "UPDATE workflow_tool_access_approval_run_t
        SET delivery_state='ACKED',portal_outcome=$1,portal_committed_ts=$2,
            acknowledged_ts=CURRENT_TIMESTAMP,last_delivery_error=NULL
            WHERE host_id=$3 AND request_id=$4 AND decision_id=$5",
    )
    .bind(outcome)
    .bind(portal_committed)
    .bind(input.host_id)
    .bind(input.request_id)
    .bind(input.decision_id)
    .execute(&mut *tx)
    .await?;
    let completed = sqlx::query(
        "UPDATE task_info_t
        SET status_code='C',locked='N',completed_ts=CURRENT_TIMESTAMP,
            completed_user=$1,result_code=$2,aggregate_version=aggregate_version+1,
            update_ts=CURRENT_TIMESTAMP,update_user=$1
        WHERE host_id=$3 AND task_id=$4 AND status_code='W'",
    )
    .bind(&input.approver_subject)
    .bind(outcome)
    .bind(input.host_id)
    .bind(task)
    .execute(&mut *tx)
    .await?;
    if completed.rows_affected() != 1 {
        return Err(sqlx::Error::Protocol(
            "approval task is no longer waiting".into(),
        ));
    }
    let assignment_done = sqlx::query(
        "UPDATE task_asst_t SET assignment_status_code='COMPLETED',
        decision=$1,decision_comment=$2,completion_id=$3,completed_ts=CURRENT_TIMESTAMP,
        active=FALSE,aggregate_version=aggregate_version+1,update_ts=CURRENT_TIMESTAMP,
        update_user=$4 WHERE host_id=$5 AND task_asst_id=$6
        AND assignment_status_code='DECISION_PENDING'",
    )
    .bind(serde_json::Value::String(input.decision.clone()))
    .bind(&input.comment)
    .bind(input.decision_id)
    .bind(&input.approver_subject)
    .bind(input.host_id)
    .bind(assignment)
    .execute(&mut *tx)
    .await?;
    if assignment_done.rows_affected() != 1 {
        return Err(sqlx::Error::Protocol(
            "approval assignment reservation disappeared".into(),
        ));
    }
    sqlx::query(
        "UPDATE task_asst_t SET assignment_status_code='CANCELLED',
        claimed_by=NULL,claimed_ts=NULL,claim_expires_ts=NULL,active=FALSE,
        aggregate_version=aggregate_version+1,update_ts=CURRENT_TIMESTAMP
        WHERE host_id=$1 AND task_id=$2 AND task_asst_id<>$3 AND active",
    )
    .bind(input.host_id)
    .bind(task)
    .bind(assignment)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}
