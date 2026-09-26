use crate::events::{CloudEventEnvelope, ProcessInfoDeletedPayload};
use serde_json::from_str;
use sha2::{Digest, Sha256};
use sqlx::{postgres::PgListener, PgPool, Postgres, Transaction};
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

#[derive(sqlx::FromRow)]
pub struct RawEvent {
    pub payload: String,
    pub host_id: String,
    pub c_offset: i64,
}

pub struct EventConsumer {
    pool: PgPool,
    group_id: String,
    partition_id: i32,
    total_partitions: i32,
    batch_size: i64,
    database_url: Option<String>,
}

fn retryable_event_infrastructure_error(
    error: &(dyn std::error::Error + Send + Sync + 'static),
) -> bool {
    let Some(error) = error.downcast_ref::<sqlx::Error>() else {
        return false;
    };
    match error {
        sqlx::Error::Io(_)
        | sqlx::Error::Tls(_)
        | sqlx::Error::PoolTimedOut
        | sqlx::Error::PoolClosed
        | sqlx::Error::WorkerCrashed => true,
        sqlx::Error::Database(database) => database
            .code()
            .is_none_or(|code| matches!(code.as_ref(), "40001" | "40P01" | "55P03" | "57014")),
        _ => false,
    }
}

impl EventConsumer {
    pub fn new(
        pool: PgPool,
        group_id: String,
        partition_id: i32,
        total_partitions: i32,
        batch_size: i64,
    ) -> Self {
        Self {
            pool,
            group_id,
            partition_id,
            total_partitions,
            batch_size,
            database_url: None,
        }
    }

    pub fn with_database_url(mut self, database_url: String) -> Self {
        self.database_url = Some(database_url);
        self
    }

    pub async fn run(
        &self,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<(), sqlx::Error> {
        self.initialize().await?;

        info!("Starting DbEventConsumer loop for group {}", self.group_id);
        loop {
            if shutdown.is_cancelled() {
                return Ok(());
            }
            match self.run_listen_loop(&shutdown).await {
                Ok(_) => {
                    return Err(sqlx::Error::Protocol(
                        "listener loop exited unexpectedly".to_string(),
                    ));
                }
                Err(e) => {
                    error!("Error in listener loop: {}, reconnecting in 5s", e);
                    tokio::select! { _ = shutdown.cancelled() => return Ok(()), _ = sleep(Duration::from_secs(5)) => {} }
                }
            }
        }
    }

    pub async fn initialize(&self) -> Result<(), sqlx::Error> {
        self.ensure_consumer_group().await
    }

    async fn ensure_consumer_group(&self) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO consumer_offsets (group_id, topic_id, partition_id, next_offset)
            VALUES ($1, 1, $2, 1)
            ON CONFLICT (group_id, topic_id, partition_id) DO NOTHING
            "#,
        )
        .bind(&self.group_id)
        .bind(self.partition_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn run_listen_loop(
        &self,
        shutdown: &tokio_util::sync::CancellationToken,
    ) -> Result<(), sqlx::Error> {
        // Keep the permanent LISTEN connection out of the transaction pool.
        let database_url = self.database_url.as_deref().ok_or_else(|| {
            sqlx::Error::Protocol("consumer database URL is not configured".to_string())
        })?;
        let mut listener = PgListener::connect(database_url).await?;
        listener.listen("event_channel").await?;
        info!("Listening to 'event_channel' on PG connection");

        loop {
            if shutdown.is_cancelled() {
                return Ok(());
            }
            let processed = self.process_batch().await?;
            if !processed {
                // If there were no events processed, we wait for a notification or fallback timeout
                if let Ok(Ok(_notification)) = tokio::select! {
                    _ = shutdown.cancelled() => return Ok(()),
                    result = tokio::time::timeout(Duration::from_secs(1), listener.recv()) => result,
                } {
                    debug!("Received PG notification on event_channel, waking up batch processor.");
                } else {
                    // Timeout hit (1 second wait period), just loop to poll
                }
            }
        }
    }

    async fn process_batch(&self) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        if self.process_repaired_event(&mut tx).await? {
            tx.commit().await?;
            return Ok(true);
        }

        // Simplified gapless claim process
        let claim_sql = r#"
            WITH counter_tip AS (
              SELECT (next_offset - 1) AS highest_committed_offset
              FROM log_counter
              WHERE id = 1
            ),
            to_claim AS (
              SELECT
                c.group_id,
                c.partition_id,
                c.next_offset AS n0,
                LEAST(
                  $1::bigint,
                  GREATEST(0, (SELECT highest_committed_offset FROM counter_tip) - c.next_offset + 1)
                ) AS delta
              FROM consumer_offsets c
              WHERE c.group_id = $2 AND c.topic_id = 1 AND c.partition_id = $3
              FOR UPDATE
            ),
            upd AS (
              UPDATE consumer_offsets c
              SET next_offset = c.next_offset + t.delta
              FROM to_claim t
              WHERE c.group_id = t.group_id AND c.topic_id = 1 AND c.partition_id = t.partition_id
              RETURNING
                t.n0 AS claimed_start_offset,
                (c.next_offset - 1) AS claimed_end_offset
            )
            SELECT claimed_start_offset, claimed_end_offset FROM upd
        "#;

        let claim_res = sqlx::query_as::<_, (i64, i64)>(claim_sql)
            .bind(self.batch_size)
            .bind(&self.group_id)
            .bind(self.partition_id)
            .fetch_optional(&mut *tx)
            .await?;

        let (start_offset, end_offset) = match claim_res {
            Some((start, end)) if start <= end => (start, end),
            _ => {
                tx.commit().await?;
                return Ok(false);
            }
        };

        debug!("Claimed offsets {} to {}", start_offset, end_offset);

        let read_sql = r#"
            SELECT payload::text AS payload, host_id::text AS host_id, c_offset FROM outbox_message_t
            WHERE c_offset BETWEEN $1 AND $2
              AND ((hashtext(host_id::text) % $3) + $3) % $3 = $4
            ORDER BY c_offset
        "#;

        let events = sqlx::query_as::<_, RawEvent>(read_sql)
            .bind(start_offset)
            .bind(end_offset)
            .bind(self.total_partitions)
            .bind(self.partition_id)
            .fetch_all(&mut *tx)
            .await?;

        if !events.is_empty() {
            debug!("Fetched {} events", events.len());
            for event in events {
                if self.aggregate_is_quarantined(&mut tx, &event).await? {
                    self.quarantine_event(
                        &mut tx,
                        &event,
                        "WORKFLOW_EVENT_DEFERRED_BY_AGGREGATE",
                        "an earlier event for this aggregate remains quarantined",
                    )
                    .await?;
                    continue;
                }
                sqlx::query("SAVEPOINT workflow_event_v1")
                    .execute(&mut *tx)
                    .await?;
                match self.handle_event(&mut tx, &event).await {
                    Ok(()) => {
                        sqlx::query("RELEASE SAVEPOINT workflow_event_v1")
                            .execute(&mut *tx)
                            .await?;
                    }
                    Err(error) => {
                        error!(
                            offset = event.c_offset,
                            "Workflow event handler failed: {error}"
                        );
                        sqlx::query("ROLLBACK TO SAVEPOINT workflow_event_v1")
                            .execute(&mut *tx)
                            .await?;
                        sqlx::query("RELEASE SAVEPOINT workflow_event_v1")
                            .execute(&mut *tx)
                            .await?;
                        if retryable_event_infrastructure_error(error.as_ref()) {
                            // Roll back the offset claim and let the outer listener loop
                            // retry after its reconnect backoff. Infrastructure failures
                            // must never become aggregate poison records.
                            return Err(sqlx::Error::Protocol(format!(
                                "retryable workflow event infrastructure failure: {error}"
                            )));
                        }
                        self.quarantine_event(
                            &mut tx,
                            &event,
                            "WORKFLOW_EVENT_HANDLER_FAILED",
                            &error.to_string(),
                        )
                        .await?;
                    }
                }
            }
        }

        tx.commit().await?;
        Ok(true)
    }

    async fn process_repaired_event(
        &self,
        tx: &mut Transaction<'_, Postgres>,
    ) -> Result<bool, sqlx::Error> {
        let repaired: Option<(Uuid, String, String, String, i64)> = sqlx::query_as(
            "SELECT quarantine.quarantine_id,quarantine.aggregate_id,
                    outbox.payload::text AS payload,outbox.host_id::text AS host_id,
                    outbox.c_offset
               FROM workflow_invocation_event_quarantine_t quarantine
               JOIN outbox_message_t outbox ON outbox.c_offset=quarantine.source_offset
              WHERE quarantine.consumer_group=$1 AND quarantine.partition_id=$2
                AND quarantine.replay_state='REPAIRED'
              ORDER BY quarantine.source_offset
              LIMIT 1 FOR UPDATE OF quarantine SKIP LOCKED",
        )
        .bind(&self.group_id)
        .bind(self.partition_id)
        .fetch_optional(&mut **tx)
        .await?;
        let Some((quarantine_id, aggregate_id, payload, host_id, c_offset)) = repaired else {
            return Ok(false);
        };
        let event = RawEvent {
            payload,
            host_id,
            c_offset,
        };
        sqlx::query("SAVEPOINT workflow_quarantine_replay_v1")
            .execute(&mut **tx)
            .await?;
        match self.handle_event(tx, &event).await {
            Ok(()) => {
                sqlx::query("RELEASE SAVEPOINT workflow_quarantine_replay_v1")
                    .execute(&mut **tx)
                    .await?;
                sqlx::query(
                    "UPDATE workflow_invocation_event_quarantine_t
                        SET replay_state='REPLAYED',resolved_ts=CURRENT_TIMESTAMP
                      WHERE quarantine_id=$1",
                )
                .bind(quarantine_id)
                .execute(&mut **tx)
                .await?;
                // Release exactly the next event for this aggregate so replay
                // preserves source ordering rather than creating a second log.
                sqlx::query(
                    "UPDATE workflow_invocation_event_quarantine_t candidate
                        SET replay_state='REPAIRED',repaired_by='workflow-consumer',
                            repair_reason='ordered replay after predecessor'
                      WHERE candidate.quarantine_id=(
                        SELECT next.quarantine_id
                          FROM workflow_invocation_event_quarantine_t next
                         WHERE next.host_id=$1
                           AND next.aggregate_id=$2
                           AND next.replay_state='BLOCKED'
                           AND next.failure_code='WORKFLOW_EVENT_DEFERRED_BY_AGGREGATE'
                         ORDER BY next.source_offset LIMIT 1)",
                )
                .bind(Uuid::parse_str(&event.host_id).map_err(|error| {
                    sqlx::Error::Protocol(format!("invalid replay event host UUID: {error}"))
                })?)
                .bind(aggregate_id)
                .execute(&mut **tx)
                .await?;
            }
            Err(error) => {
                sqlx::query("ROLLBACK TO SAVEPOINT workflow_quarantine_replay_v1")
                    .execute(&mut **tx)
                    .await?;
                sqlx::query("RELEASE SAVEPOINT workflow_quarantine_replay_v1")
                    .execute(&mut **tx)
                    .await?;
                sqlx::query(
                    "UPDATE workflow_invocation_event_quarantine_t
                        SET replay_state='BLOCKED',attempt_count=attempt_count+1,
                            failure_detail=$2
                      WHERE quarantine_id=$1",
                )
                .bind(quarantine_id)
                .bind(error.to_string().chars().take(4096).collect::<String>())
                .execute(&mut **tx)
                .await?;
            }
        }
        Ok(true)
    }

    async fn quarantine_event(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        event: &RawEvent,
        failure_code: &str,
        failure: &str,
    ) -> Result<(), sqlx::Error> {
        let host_id: Uuid = event
            .host_id
            .parse()
            .map_err(|error| sqlx::Error::Protocol(format!("invalid event host UUID: {error}")))?;
        let payload_digest = format!("sha256:{:x}", Sha256::digest(event.payload.as_bytes()));
        let (aggregate_id, aggregate_version) = event_aggregate_identity(event);
        sqlx::query(
            "INSERT INTO workflow_invocation_event_quarantine_t(
                host_id,quarantine_id,consumer_group,partition_id,source_offset,
                aggregate_id,aggregate_version,payload_digest,immutable_payload_reference,
                failure_code,failure_detail,attempt_count)
             VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)
             ON CONFLICT(consumer_group,partition_id,source_offset) DO NOTHING",
        )
        .bind(host_id)
        .bind(Uuid::now_v7())
        .bind(&self.group_id)
        .bind(self.partition_id)
        .bind(event.c_offset)
        .bind(aggregate_id)
        .bind(aggregate_version)
        .bind(payload_digest)
        .bind(format!("outbox_message_t:c_offset={}", event.c_offset))
        .bind(failure_code)
        .bind(failure.chars().take(4096).collect::<String>())
        .bind(i32::from(failure_code == "WORKFLOW_EVENT_HANDLER_FAILED"))
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    async fn aggregate_is_quarantined(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        event: &RawEvent,
    ) -> Result<bool, sqlx::Error> {
        let host_id: Uuid = event
            .host_id
            .parse()
            .map_err(|error| sqlx::Error::Protocol(format!("invalid event host UUID: {error}")))?;
        let (aggregate_id, _) = event_aggregate_identity(event);
        sqlx::query_scalar(
            "SELECT EXISTS(
               SELECT 1 FROM workflow_invocation_event_quarantine_t
                WHERE host_id=$1 AND aggregate_id=$2 AND replay_state='BLOCKED'
            )",
        )
        .bind(host_id)
        .bind(aggregate_id)
        .fetch_one(&mut **tx)
        .await
    }

    async fn handle_event(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        event: &RawEvent,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        debug!(
            "Processing event at offset {} for host: {}",
            event.c_offset, event.host_id
        );

        let ce: CloudEventEnvelope = from_str(&event.payload).map_err(|error| {
            sqlx::Error::Protocol(format!("invalid CloudEvent payload: {error}"))
        })?;

        if ce.r#type == "WorkflowStartedEvent" {
            warn!(
                event_id = %ce.id,
                "Ignoring retired WorkflowStartedEvent; root starts must use Gateway MCP workflow_start"
            );
            return Ok(());
        }

        if ce.r#type == "ProcessInfoDeletedEvent" {
            if let Some(data) = ce.data.clone() {
                let payload: ProcessInfoDeletedPayload = serde_json::from_value(data)?;
                let envelope_host: Uuid = event.host_id.parse()?;
                if payload.host_id != envelope_host {
                    return Err("ProcessInfoDeletedEvent host mismatch".into());
                }
                sqlx::query("UPDATE workflow_artifact_t SET deletion_state='DELETE_PENDING',deletion_next_retry_ts=now(),deletion_evidence=COALESCE(deletion_evidence,'{}'::jsonb)||jsonb_build_object('processDeletedEvent',$3),updated_ts=now() WHERE host_id=$1 AND process_id=$2 AND legal_hold=FALSE AND deletion_state='RETAINED'")
                    .bind(payload.host_id).bind(payload.process_id).bind(&ce.id).execute(&mut **tx).await?;
            }
        }

        Ok(())
    }

}

fn event_aggregate_identity(event: &RawEvent) -> (String, i64) {
    let envelope = from_str::<CloudEventEnvelope>(&event.payload).ok();
    let aggregate_id = envelope
        .as_ref()
        .and_then(|value| value.subject.clone())
        .filter(|value| !value.trim().is_empty())
        .or_else(|| envelope.as_ref().map(|value| value.id.clone()))
        .unwrap_or_else(|| format!("offset:{}", event.c_offset));
    let aggregate_version = envelope
        .and_then(|value| value.eventaggregateversion)
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or_default();
    (aggregate_id, aggregate_version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::configuration::DEFAULT_MAXIMUM_PARALLELISM;
    use crate::runtime_definition::{
        policy_task_kind, supported_task_type, validate_runtime_definition,
    };
    use serde_yaml;
    use workflow_core::models::workflow::WorkflowDefinition;

    #[test]
    fn event_retry_classification_separates_infrastructure_from_poison() {
        let infrastructure = sqlx::Error::PoolTimedOut;
        assert!(retryable_event_infrastructure_error(&infrastructure));

        let poison = sqlx::Error::Protocol("invalid CloudEvent payload".to_string());
        assert!(!retryable_event_infrastructure_error(&poison));
    }

    #[test]
    fn runtime_definition_rejects_explicit_unsupported_language_and_tasks() {
        let jq: WorkflowDefinition = serde_yaml::from_str(
            "document: { dsl: 1.0.3, namespace: test, name: jq, version: 1.0.0 }\nevaluate: { language: jq }\ndo:\n  - start:\n      set: { ok: true }",
        )
        .unwrap();
        assert!(
            validate_runtime_definition(&jq, DEFAULT_MAXIMUM_PARALLELISM)
                .unwrap_err()
                .contains("evaluate.language 'cel'")
        );

        let implicit_jq: WorkflowDefinition = serde_yaml::from_str(
            "document: { dsl: 1.0.3, namespace: test, name: implicit-jq, version: 1.0.0 }\ndo:\n  - start:\n      set: { ok: true }",
        )
        .unwrap();
        assert_eq!(
            validate_runtime_definition(&implicit_jq, DEFAULT_MAXIMUM_PARALLELISM).unwrap_err(),
            "light-workflow requires evaluate.language: cel; Open Workflow defaults an omitted evaluate block to jq"
        );

        let wait: WorkflowDefinition = serde_yaml::from_str(
            "document: { dsl: 1.0.3, namespace: test, name: wait, version: 1.0.0 }\nevaluate: { language: cel }\ndo:\n  - pause:\n      wait: PT1S",
        )
        .unwrap();
        assert_eq!(
            validate_runtime_definition(&wait, DEFAULT_MAXIMUM_PARALLELISM).unwrap_err(),
            "task 'pause' uses unimplemented task wait"
        );
    }

    #[test]
    fn runtime_definition_accepts_canonical_http_mcp_and_rejects_stdio() {
        let http: WorkflowDefinition = serde_yaml::from_str(
            "document: { dsl: 1.0.3, namespace: test, name: mcp-http, version: 1.0.0 }\nevaluate: { language: cel }\ndo:\n  - listTools:\n      call: mcp\n      with:\n        method: tools/list\n        transport:\n          http:\n            endpoint: https://gateway.example/mcp",
        )
        .unwrap();
        validate_runtime_definition(&http, DEFAULT_MAXIMUM_PARALLELISM)
            .expect("canonical MCP HTTP is executable");

        let stdio: WorkflowDefinition = serde_yaml::from_str(
            "document: { dsl: 1.0.3, namespace: test, name: mcp-stdio, version: 1.0.0 }\nevaluate: { language: cel }\ndo:\n  - listTools:\n      call: mcp\n      with:\n        method: tools/list\n        transport:\n          stdio:\n            command: mcp-server",
        )
        .unwrap();
        assert!(
            validate_runtime_definition(&stdio, DEFAULT_MAXIMUM_PARALLELISM)
                .unwrap_err()
                .contains("MCP stdio")
        );
    }

    #[test]
    fn runtime_definition_accepts_governed_a2a_alias_and_rejects_raw_destinations() {
        let governed: WorkflowDefinition = serde_yaml::from_str(
            r#"
document: { dsl: 1.0.3, namespace: test, name: governed-a2a, version: 1.0.0 }
evaluate: { language: cel }
do:
  - invoke:
      call: a2a
      with:
        agentRef: external-account
        method: message/send
        parameters: { message: { text: hello } }
"#,
        )
        .unwrap();
        validate_runtime_definition(&governed, DEFAULT_MAXIMUM_PARALLELISM)
            .expect("stable A2A publication alias should be executable");

        let legacy: WorkflowDefinition = serde_yaml::from_str(
            r#"
document: { dsl: 1.0.3, namespace: test, name: legacy-a2a, version: 1.0.0 }
evaluate: { language: cel }
do:
  - invoke:
      call: a2a
      with:
        agentRef: external-account
        server: https://untrusted.example/a2a
        method: message/send
"#,
        )
        .unwrap();
        assert!(
            validate_runtime_definition(&legacy, DEFAULT_MAXIMUM_PARALLELISM)
                .unwrap_err()
                .contains("legacy agentCard/server destinations are non-executable")
        );
    }

    #[test]
    fn runtime_definition_accepts_a_host_side_fork_and_rejects_nested_forks() {
        let fork: WorkflowDefinition = serde_yaml::from_str(
            r#"
document: { dsl: 1.0.3, namespace: test, name: fork, version: 1.0.0 }
evaluate: { language: cel }
do:
  - load:
      fork:
        branches:
          - profile:
              set: { source: profile }
          - preferences:
              set: { source: preferences }
        compete: false
"#,
        )
        .unwrap();
        validate_runtime_definition(&fork, DEFAULT_MAXIMUM_PARALLELISM)
            .expect("one-level fork should be executable");
        let initial_task = fork.do_.entries[0].get("load").unwrap();
        assert_eq!(supported_task_type(initial_task), Some("fork"));
        assert_eq!(
            policy_task_kind(initial_task).unwrap(),
            workflow_policy::TaskKind::Fork
        );

        let nested: WorkflowDefinition = serde_yaml::from_str(
            r#"
document: { dsl: 1.0.3, namespace: test, name: nested-fork, version: 1.0.0 }
evaluate: { language: cel }
do:
  - outer:
      fork:
        branches:
          - inner:
              fork:
                branches:
                  - leaf:
                      set: { ok: true }
                compete: false
        compete: false
"#,
        )
        .unwrap();
        assert!(
            validate_runtime_definition(&nested, DEFAULT_MAXIMUM_PARALLELISM)
                .unwrap_err()
                .contains("nested fork")
        );
    }

    #[test]
    fn runtime_definition_rejects_oversized_and_transitioning_fork_branches() {
        let mut oversized = String::from(
            r#"document: { dsl: 1.0.3, namespace: test, name: oversized, version: 1.0.0 }
evaluate: { language: cel }
do:
  - load:
      fork:
        branches:
"#,
        );
        for index in 0..65 {
            oversized.push_str(&format!(
                "          - branch{index}:\n              set: {{ value: {index} }}\n"
            ));
        }
        oversized.push_str("        compete: false\n");
        let oversized: WorkflowDefinition = serde_yaml::from_str(&oversized).unwrap();
        assert!(
            validate_runtime_definition(&oversized, DEFAULT_MAXIMUM_PARALLELISM)
                .unwrap_err()
                .contains("configured light-workflow maximum of 64")
        );

        let transitioning: WorkflowDefinition = serde_yaml::from_str(
            r#"
document: { dsl: 1.0.3, namespace: test, name: transitioning, version: 1.0.0 }
evaluate: { language: cel }
do:
  - load:
      fork:
        branches:
          - profile:
              set: { source: profile }
              then: done
        compete: false
  - done:
      set: { complete: true }
"#,
        )
        .unwrap();
        assert!(
            validate_runtime_definition(&transitioning, DEFAULT_MAXIMUM_PARALLELISM)
                .unwrap_err()
                .contains("then/export")
        );
    }

    #[test]
    fn runtime_definition_enforces_configured_parallelism_ceiling() {
        let fork: WorkflowDefinition = serde_yaml::from_str(
            r#"
document: { dsl: 1.0.3, namespace: test, name: configured-ceiling, version: 1.0.0 }
evaluate: { language: cel }
do:
  - load:
      fork:
        branches:
          - profile:
              set: { source: profile }
          - preferences:
              set: { source: preferences }
          - policies:
              set: { source: policies }
        compete: false
"#,
        )
        .unwrap();

        validate_runtime_definition(&fork, 3).expect("three branches fit the service ceiling");
        assert!(
            validate_runtime_definition(&fork, 2)
                .unwrap_err()
                .contains("configured light-workflow maximum of 2")
        );
    }
}
