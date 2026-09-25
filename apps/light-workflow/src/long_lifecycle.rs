//! Durable operational half of an issuer-owned LONG binding. Step 05 calls
//! `record_acceptance` inside its start transaction, after registering with
//! the issuer and inserting the action-authority row. This module never calls
//! the issuer while an operational transaction is open.
use crate::long_authority::{LongAuthority, LongError};
use sqlx::{PgPool, Postgres, Transaction};
use std::sync::Arc;
use uuid::Uuid;

#[derive(sqlx::FromRow)]
struct Delivery {
    host_id: Uuid,
    workflow_instance_id: Uuid,
    binding_id: Uuid,
    state: String,
    acceptance_digest: String,
    close_id: Option<Uuid>,
    terminal_state: Option<String>,
    terminal_version: Option<i64>,
    attempts: i32,
}

/// Step 05's accepted invocation and action authority must already be in this
/// transaction. The issuer binding remains PENDING until the reconciler sees
/// the committed row. A duplicate call accepts only the exact same evidence.
pub async fn record_acceptance(
    tx: &mut Transaction<'_, Postgres>,
    host: Uuid,
    run: Uuid,
    binding: Uuid,
    owner: Uuid,
    digest: &str,
) -> Result<(), sqlx::Error> {
    if host.is_nil()
        || run.is_nil()
        || binding.is_nil()
        || owner.is_nil()
        || digest.len() != 64
        || !digest
            .bytes()
            .all(|value| value.is_ascii_hexdigit() && !value.is_ascii_uppercase())
    {
        return Err(sqlx::Error::Protocol(
            "invalid LONG acceptance evidence".into(),
        ));
    }
    let row: Option<(String, String, String)> = sqlx::query_as(
        "SELECT state,end_user_subject,
                response_policy_snapshot->>'acceptedAdmissionProfile'
           FROM workflow_ops.workflow_invocation_t
          WHERE host_id=$1 AND workflow_instance_id=$2 FOR UPDATE",
    )
    .bind(host)
    .bind(run)
    .fetch_optional(&mut **tx)
    .await?;
    if !row.is_some_and(|(state, subject, profile)| {
        state == "ACCEPTED" && subject == owner.to_string() && profile == "portal_execution"
    }) {
        return Err(sqlx::Error::Protocol(
            "LONG acceptance requires the matching private owner and accepted run".into(),
        ));
    }
    let prior: Option<(Uuid, Uuid, String)> = sqlx::query_as(
        "SELECT binding_id,owner_user_id,acceptance_digest
           FROM workflow_ops.workflow_long_owner_binding_t
          WHERE host_id=$1 AND workflow_instance_id=$2",
    )
    .bind(host)
    .bind(run)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some((stored_binding, stored_owner, stored_digest)) = prior {
        return if (stored_binding, stored_owner, stored_digest.as_str()) == (binding, owner, digest)
        {
            Ok(())
        } else {
            Err(sqlx::Error::Protocol(
                "LONG acceptance replay conflicts".into(),
            ))
        };
    }
    let fenced = sqlx::query(
        "UPDATE workflow_ops.workflow_action_authority_t SET active=false
          WHERE host_id=$1 AND run_id=$2 AND grant_id=$3 AND user_id=$4",
    )
    .bind(host)
    .bind(run)
    .bind(binding)
    .bind(owner)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if fenced != 1 {
        return Err(sqlx::Error::Protocol(
            "LONG acceptance has no matching action authority".into(),
        ));
    }
    sqlx::query(
        "INSERT INTO workflow_ops.workflow_long_owner_binding_t
            (host_id,workflow_instance_id,binding_id,owner_user_id,acceptance_digest,state)
         VALUES($1,$2,$3,$4,$5,'ACTIVATE_PENDING')",
    )
    .bind(host)
    .bind(run)
    .bind(binding)
    .bind(owner)
    .bind(digest)
    .execute(&mut **tx)
    .await?;
    let waiting = sqlx::query(
        "UPDATE workflow_ops.process_info_t p SET status_code='W',
         custom_status_code='WORKFLOW_AUTHORITY_PENDING'
         FROM workflow_ops.workflow_invocation_t i
         WHERE i.host_id=$1 AND i.workflow_instance_id=$2
           AND p.host_id=i.host_id AND p.process_id=i.process_id AND p.status_code='A'",
    )
    .bind(host)
    .bind(run)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    let fenced_task = sqlx::query(
        "UPDATE workflow_ops.task_info_t t SET status_code='W',active=false
         FROM workflow_ops.workflow_invocation_t i
         WHERE i.host_id=$1 AND i.workflow_instance_id=$2
           AND t.host_id=i.host_id AND t.process_id=i.process_id AND t.status_code='A'",
    )
    .bind(host)
    .bind(run)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if waiting != 1 || fenced_task != 1 {
        return Err(sqlx::Error::Protocol(
            "LONG acceptance could not fence the initial process and task".into(),
        ));
    }
    Ok(())
}

pub struct Reconciler {
    pool: PgPool,
    issuer: Arc<LongAuthority>,
}

impl Reconciler {
    pub fn new(pool: PgPool, issuer: Arc<LongAuthority>) -> Self {
        Self { pool, issuer }
    }

    /// Claims delivery for thirty seconds. Lost issuer responses, local commit
    /// failures and worker crashes are retried using the same binding/digest or
    /// close ID. No access token or original bearer enters the operational DB.
    pub async fn reconcile_once(&self) -> Result<usize, sqlx::Error> {
        let rows: Vec<Delivery> = sqlx::query_as(
            "WITH candidate AS (
                SELECT host_id,workflow_instance_id
                  FROM workflow_ops.workflow_long_owner_binding_t
                 WHERE state IN ('ACTIVATE_PENDING','CLOSE_PENDING')
                   AND next_attempt_ts<=CURRENT_TIMESTAMP
                 ORDER BY next_attempt_ts,created_ts LIMIT 32 FOR UPDATE SKIP LOCKED
             ) UPDATE workflow_ops.workflow_long_owner_binding_t b SET
                attempts=b.attempts+1,
                next_attempt_ts=CURRENT_TIMESTAMP+INTERVAL '30 seconds',
                updated_ts=CURRENT_TIMESTAMP
               FROM candidate c WHERE b.host_id=c.host_id
                AND b.workflow_instance_id=c.workflow_instance_id
             RETURNING b.host_id,b.workflow_instance_id,b.binding_id,b.state,
                b.acceptance_digest,b.close_id,b.terminal_state,b.terminal_version,b.attempts",
        )
        .fetch_all(&self.pool)
        .await?;
        for row in &rows {
            let result = if row.state == "ACTIVATE_PENDING" {
                self.issuer
                    .activate(row.workflow_instance_id, &row.acceptance_digest)
                    .await
                    .map(|_| "ACTIVE".to_string())
            } else {
                let (Some(close_id), Some(state), Some(version)) = (
                    row.close_id,
                    row.terminal_state.as_deref(),
                    row.terminal_version,
                ) else {
                    self.record_error(row, "INVALID_CLOSE_INTENT", false)
                        .await?;
                    continue;
                };
                self.issuer
                    .close(row.workflow_instance_id, state, version, close_id)
                    .await
            };
            match result {
                Ok(_) if row.state == "ACTIVATE_PENDING" => {
                    Self::ack_activation(&self.pool, row).await?;
                }
                Ok(state) if matches!(state.as_str(), "CLOSED" | "REVOKED") => {
                    sqlx::query(
                        "UPDATE workflow_ops.workflow_long_owner_binding_t SET
                        state=$4,last_error_code=NULL,updated_ts=CURRENT_TIMESTAMP
                        WHERE host_id=$1 AND workflow_instance_id=$2
                          AND state='CLOSE_PENDING' AND attempts=$3",
                    )
                    .bind(row.host_id)
                    .bind(row.workflow_instance_id)
                    .bind(row.attempts)
                    .bind(state)
                    .execute(&self.pool)
                    .await?;
                }
                Ok(_) => self.record_error(row, "INVALID_ISSUER_ACK", false).await?,
                Err(error) => {
                    let (code, retry) = match error {
                        LongError::Retryable | LongError::Store => ("ISSUER_UNAVAILABLE", true),
                        LongError::Denied => ("AUTHORITY_BLOCKED", row.state == "CLOSE_PENDING"),
                        LongError::Evidence => ("ISSUER_EVIDENCE_INVALID", false),
                    };
                    self.record_error(row, code, retry).await?;
                }
            }
        }
        Ok(rows.len())
    }

    async fn ack_activation(pool: &PgPool, row: &Delivery) -> Result<(), sqlx::Error> {
        let mut tx = pool.begin().await?;
        let state: Option<String> = sqlx::query_scalar(
            "SELECT state FROM workflow_ops.workflow_invocation_t
              WHERE host_id=$1 AND workflow_instance_id=$2 FOR UPDATE",
        )
        .bind(row.host_id)
        .bind(row.workflow_instance_id)
        .fetch_optional(&mut *tx)
        .await?;
        if !state.is_some_and(|state| matches!(state.as_str(), "ACCEPTED" | "RUNNING" | "WAITING"))
        {
            // The terminal trigger already installed a close intent; it wins.
            return Ok(());
        }
        let changed = sqlx::query(
            "UPDATE workflow_ops.workflow_long_owner_binding_t SET
                state='ACTIVE',last_error_code=NULL,updated_ts=CURRENT_TIMESTAMP
              WHERE host_id=$1 AND workflow_instance_id=$2 AND binding_id=$3
                AND state='ACTIVATE_PENDING' AND attempts=$4",
        )
        .bind(row.host_id)
        .bind(row.workflow_instance_id)
        .bind(row.binding_id)
        .bind(row.attempts)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if changed == 1 {
            let active = sqlx::query(
                "UPDATE workflow_ops.workflow_action_authority_t SET active=true
                  WHERE host_id=$1 AND run_id=$2 AND grant_id=$3 AND active=false",
            )
            .bind(row.host_id)
            .bind(row.workflow_instance_id)
            .bind(row.binding_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if active != 1 {
                return Err(sqlx::Error::Protocol(
                    "LONG activation has no fenced action authority".into(),
                ));
            }
            let resumed_process = sqlx::query(
                "UPDATE workflow_ops.process_info_t p SET status_code='A',custom_status_code=NULL
                 FROM workflow_ops.workflow_invocation_t i
                 WHERE i.host_id=$1 AND i.workflow_instance_id=$2
                   AND p.host_id=i.host_id AND p.process_id=i.process_id
                   AND p.status_code='W' AND p.custom_status_code='WORKFLOW_AUTHORITY_PENDING'",
            )
            .bind(row.host_id)
            .bind(row.workflow_instance_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            let resumed_task = sqlx::query(
                "UPDATE workflow_ops.task_info_t t SET status_code='A',active=true
                 FROM workflow_ops.workflow_invocation_t i
                 WHERE i.host_id=$1 AND i.workflow_instance_id=$2
                   AND t.host_id=i.host_id AND t.process_id=i.process_id
                   AND t.status_code='W' AND NOT t.active",
            )
            .bind(row.host_id)
            .bind(row.workflow_instance_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if resumed_process != 1 || resumed_task != 1 {
                return Err(sqlx::Error::Protocol(
                    "LONG activation could not release the initial process and task".into(),
                ));
            }
        }
        tx.commit().await
    }

    async fn record_error(
        &self,
        row: &Delivery,
        code: &str,
        retry: bool,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE workflow_ops.workflow_long_owner_binding_t SET
                last_error_code=$4,
                next_attempt_ts=CASE WHEN $5 THEN next_attempt_ts ELSE 'infinity'::timestamptz END,
                updated_ts=CURRENT_TIMESTAMP
              WHERE host_id=$1 AND workflow_instance_id=$2 AND attempts=$3 AND state=$6",
        )
        .bind(row.host_id)
        .bind(row.workflow_instance_id)
        .bind(row.attempts)
        .bind(code)
        .bind(retry)
        .bind(&row.state)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn run(
        &self,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<(), sqlx::Error> {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return Ok(()),
                _ = interval.tick() => { self.reconcile_once().await?; }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires an isolated PostgreSQL database with Workflow migrations 0001, 0007 and 0013"]
    async fn accepted_owner_is_fenced_until_ack_and_terminal_commit_records_close() {
        let url = std::env::var("WORKFLOW_ROLE_TEST_DATABASE_URL").unwrap();
        let pool = PgPool::connect(&url).await.unwrap();
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT current_database()")
                .fetch_one(&pool)
                .await
                .unwrap(),
            "workflow_step04_test",
            "this gate only runs against a disposable Workflow database"
        );
        let (host, run, process, definition, tool_binding, tool, owner, owner_binding) = (
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
            Uuid::now_v7(),
        );
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("INSERT INTO workflow_ops.wf_definition_t(host_id,wf_def_id,namespace,name,version,definition)
            VALUES($1,$2,'test','long-lifecycle','1.0.0','{}')")
            .bind(host).bind(definition).execute(&mut *tx).await.unwrap();
        let digest = format!("sha256:{}", "a".repeat(64));
        sqlx::query("INSERT INTO workflow_ops.workflow_tool_binding_t(host_id,binding_id,tool_id,wf_def_id,workflow_version,
            definition_digest,schema_digest,policy_digest,response_policy_digest,invocation_mode,
            sync_wait_ms,total_deadline_ms,execution_class,result_text_mode,idempotency_policy,
            delegation_policy,runtime_bounds)
            VALUES($1,$2,$3,$4,'1.0.0',$5,$5,$5,$5,'async',1000,3600000,'standard','compact-json','{}','{}','{}')")
            .bind(host).bind(tool_binding).bind(tool).bind(definition).bind(&digest)
            .execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO workflow_ops.process_info_t(host_id,process_id,wf_def_id,wf_instance_id,app_id,
            process_type,status_code,ex_trigger_ts) VALUES($1,$2,$3,$4,'test','Workflow','A',now())")
            .bind(host).bind(process).bind(definition).bind(run.to_string())
            .execute(&mut *tx).await.unwrap();
        sqlx::query(
            "INSERT INTO workflow_ops.task_info_t(host_id,task_id,task_type,process_id,
            wf_instance_id,wf_task_id,status_code,locked,priority)
            VALUES($1,$2,'set',$3,$4,'initial','A','N',0)",
        )
        .bind(host)
        .bind(Uuid::now_v7())
        .bind(process)
        .bind(run.to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query(r#"INSERT INTO workflow_ops.workflow_invocation_t(host_id,workflow_instance_id,binding_id,process_id,
            stable_tool_ref,wf_def_id,workflow_version,definition_digest,schema_digest,
            policy_digest,response_policy_digest,principal_subject,end_user_subject,input,
            input_digest,canonical_input_profile,invocation_mode,execution_class,state,
            correlation_id,deadline_ts,response_policy_snapshot)
            VALUES($1,$2,$3,$4,$5,$6,'1.0.0',$7,$7,$7,$7,'workflow',$8,'{}',$7,
                'rfc8785-safe-json-v1','async','standard','ACCEPTED','step04-gate',
                now()+interval '1 hour','{"acceptedAdmissionProfile":"portal_execution"}'::jsonb)"#)
            .bind(host).bind(run).bind(tool_binding).bind(process).bind(tool).bind(definition)
            .bind(&digest).bind(owner.to_string()).execute(&mut *tx).await.unwrap();
        sqlx::query(
            "INSERT INTO workflow_ops.workflow_action_authority_t(host_id,run_id,grant_id,user_id,
            grant_generation,run_generation,budget_generation,active,deadline,action_limit)
            VALUES($1,$2,$3,$4,1,1,1,true,now()+interval '1 hour',10)",
        )
        .bind(host)
        .bind(run)
        .bind(owner_binding)
        .bind(owner)
        .execute(&mut *tx)
        .await
        .unwrap();
        record_acceptance(&mut tx, host, run, owner_binding, owner, &"b".repeat(64))
            .await
            .unwrap();
        record_acceptance(&mut tx, host, run, owner_binding, owner, &"b".repeat(64))
            .await
            .unwrap();
        assert!(
            record_acceptance(&mut tx, host, run, Uuid::now_v7(), owner, &"b".repeat(64))
                .await
                .is_err()
        );
        let (phase, active): (String, bool) = sqlx::query_as("SELECT b.state,a.active
            FROM workflow_ops.workflow_long_owner_binding_t b
            JOIN workflow_ops.workflow_action_authority_t a ON a.host_id=b.host_id AND a.run_id=b.workflow_instance_id
            WHERE b.host_id=$1 AND b.workflow_instance_id=$2")
            .bind(host).bind(run).fetch_one(&mut *tx).await.unwrap();
        assert_eq!(phase, "ACTIVATE_PENDING");
        assert!(!active);
        let (process_status, task_status, task_active): (String, String, bool) = sqlx::query_as(
            "SELECT p.status_code,t.status_code,t.active FROM workflow_ops.process_info_t p
             JOIN workflow_ops.task_info_t t ON t.host_id=p.host_id AND t.process_id=p.process_id
             WHERE p.host_id=$1 AND p.process_id=$2",
        )
        .bind(host)
        .bind(process)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        assert_eq!(
            (process_status.as_str(), task_status.as_str(), task_active),
            ("W", "W", false)
        );
        tx.commit().await.unwrap();
        Reconciler::ack_activation(
            &pool,
            &Delivery {
                host_id: host,
                workflow_instance_id: run,
                binding_id: owner_binding,
                state: "ACTIVATE_PENDING".into(),
                acceptance_digest: "b".repeat(64),
                close_id: None,
                terminal_state: None,
                terminal_version: None,
                attempts: 0,
            },
        )
        .await
        .unwrap();
        let (phase, active): (String, bool) = sqlx::query_as(
            "SELECT b.state,a.active FROM workflow_ops.workflow_long_owner_binding_t b
             JOIN workflow_ops.workflow_action_authority_t a ON a.host_id=b.host_id AND a.run_id=b.workflow_instance_id
             WHERE b.host_id=$1 AND b.workflow_instance_id=$2",
        )
        .bind(host)
        .bind(run)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(phase, "ACTIVE");
        assert!(active);
        let (process_status, task_status, task_active): (String, String, bool) = sqlx::query_as(
            "SELECT p.status_code,t.status_code,t.active FROM workflow_ops.process_info_t p
             JOIN workflow_ops.task_info_t t ON t.host_id=p.host_id AND t.process_id=p.process_id
             WHERE p.host_id=$1 AND p.process_id=$2",
        )
        .bind(host)
        .bind(process)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            (process_status.as_str(), task_status.as_str(), task_active),
            ("A", "A", true)
        );
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("UPDATE workflow_ops.workflow_invocation_t SET state='CANCELLED',
            state_version=state_version+1,terminal_ts=now() WHERE host_id=$1 AND workflow_instance_id=$2")
            .bind(host).bind(run).execute(&mut *tx).await.unwrap();
        let (phase, reason, close_id): (String, String, Option<Uuid>) = sqlx::query_as(
            "SELECT state,terminal_state,close_id FROM workflow_ops.workflow_long_owner_binding_t
              WHERE host_id=$1 AND workflow_instance_id=$2",
        )
        .bind(host)
        .bind(run)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        assert_eq!(
            (phase.as_str(), reason.as_str()),
            ("CLOSE_PENDING", "CANCELED")
        );
        assert!(close_id.is_some());
        let active: bool = sqlx::query_scalar(
            "SELECT active FROM workflow_ops.workflow_action_authority_t
             WHERE host_id=$1 AND run_id=$2 AND grant_id=$3",
        )
        .bind(host)
        .bind(run)
        .bind(owner_binding)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        assert!(!active);
        tx.commit().await.unwrap();
    }
}
