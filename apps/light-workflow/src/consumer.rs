use crate::events::{CloudEventEnvelope, ProcessInfoDeletedPayload, WorkflowStartedPayload};
use crate::repositories::{NewProcess, NewTask, WorkflowRepository};
use execution_runner_protocol::canonical_sha256;
use serde_json::{Value, from_str, json};
use serde_yaml;
use sqlx::{PgPool, Postgres, Transaction, postgres::PgListener};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, error, info};
use uuid::Uuid;
use workflow_core::models::task::{CallTaskDefinition, TaskDefinition};
use workflow_core::models::workflow::WorkflowDefinition;
use workflow_policy::{
    ExecutionPlacement, ExecutionProfile, ResolvedExecutionPolicy, TaskKind, parse_security_policy,
    resolve_policy,
};

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
    execution_profiles: BTreeMap<String, ExecutionProfile>,
}

impl EventConsumer {
    fn supported_task_type(
        task_def: &workflow_core::models::task::TaskDefinition,
    ) -> Option<&'static str> {
        match task_def {
            workflow_core::models::task::TaskDefinition::Ask(_) => Some("ask"),
            workflow_core::models::task::TaskDefinition::Assert(_) => Some("assert"),
            workflow_core::models::task::TaskDefinition::Call(_) => Some("call"),
            workflow_core::models::task::TaskDefinition::Set(_) => Some("set"),
            workflow_core::models::task::TaskDefinition::Switch(_) => Some("switch"),
            workflow_core::models::task::TaskDefinition::Run(_) => Some("run"),
            _ => None,
        }
    }

    fn policy_task_kind(task_def: &TaskDefinition) -> Result<TaskKind, sqlx::Error> {
        match task_def {
            TaskDefinition::Ask(_) => Ok(TaskKind::Ask),
            TaskDefinition::Assert(_) => Ok(TaskKind::Assert),
            TaskDefinition::Set(_) => Ok(TaskKind::Set),
            TaskDefinition::Switch(_) => Ok(TaskKind::Switch),
            TaskDefinition::Call(call) => match call {
                CallTaskDefinition::Agent(_) => Ok(TaskKind::CallAgent),
                CallTaskDefinition::Mcp(_) => Ok(TaskKind::CallMcp),
                _ => Ok(TaskKind::CallHttp),
            },
            TaskDefinition::Run(run) if run.run.shell.is_some() => Ok(TaskKind::RunShell),
            TaskDefinition::Run(run) if run.run.container.is_some() => Ok(TaskKind::RunContainer),
            TaskDefinition::Run(run) if run.run.script.is_some() => Ok(TaskKind::RunScript),
            TaskDefinition::Run(_) => Err(sqlx::Error::Protocol(
                "run.workflow is not supported by the execution runner".to_string(),
            )),
            _ => Err(sqlx::Error::Protocol(
                "task type is not supported by light-workflow".to_string(),
            )),
        }
    }

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
            execution_profiles: BTreeMap::new(),
        }
    }

    pub fn with_execution_profiles(
        mut self,
        execution_profiles: BTreeMap<String, ExecutionProfile>,
    ) -> Self {
        self.execution_profiles = execution_profiles;
        self
    }

    pub async fn run(&self) -> Result<(), sqlx::Error> {
        self.ensure_consumer_group().await?;

        info!("Starting DbEventConsumer loop for group {}", self.group_id);
        loop {
            match self.run_listen_loop().await {
                Ok(_) => {
                    return Err(sqlx::Error::Protocol(
                        "listener loop exited unexpectedly".to_string(),
                    ));
                }
                Err(e) => {
                    error!("Error in listener loop: {}, reconnecting in 5s", e);
                    sleep(Duration::from_secs(5)).await;
                }
            }
        }
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

    async fn run_listen_loop(&self) -> Result<(), sqlx::Error> {
        let mut listener = PgListener::connect_with(&self.pool).await?;
        listener.listen("event_channel").await?;
        info!("Listening to 'event_channel' on PG connection");

        loop {
            let processed = self.process_batch().await?;
            if !processed {
                // If there were no events processed, we wait for a notification or fallback timeout
                if let Ok(Ok(_notification)) =
                    tokio::time::timeout(Duration::from_secs(1), listener.recv()).await
                {
                    debug!("Received PG notification on event_channel, waking up batch processor.");
                } else {
                    // Timeout hit (1 second wait period), just loop to poll
                }
            }
        }
    }

    async fn process_batch(&self) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;

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
                if let Err(e) = self.handle_event(&mut tx, &event).await {
                    error!("Error handling event at offset {}: {}", event.c_offset, e);
                    tx.rollback().await?;
                    return Err(sqlx::Error::Protocol(format!(
                        "failed to handle event at offset {}: {}",
                        event.c_offset, e
                    )));
                }
            }
        }

        tx.commit().await?;
        Ok(true)
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

        let ce: CloudEventEnvelope = match from_str(&event.payload) {
            Ok(ce) => ce,
            Err(e) => {
                error!(
                    "Failed to parse CloudEvent payload: {}. Payload: {}",
                    e, event.payload
                );
                return Ok(()); // Skip invalid events
            }
        };

        if ce.r#type == "WorkflowStartedEvent" {
            if let Some(data) = ce.data.clone() {
                let payload: WorkflowStartedPayload = match serde_json::from_value(data) {
                    Ok(p) => p,
                    Err(e) => {
                        error!("Failed to parse WorkflowStartedPayload: {}", e);
                        return Ok(());
                    }
                };

                // 1. Generate ids
                let wf_instance_id = payload.wf_instance_id.unwrap_or_else(Uuid::new_v4);
                let process_id = Uuid::new_v4();
                let host_id: Uuid = event.host_id.parse()?;
                let input_data = payload.input.clone().unwrap_or_else(|| json!({}));

                if payload.host_id != host_id {
                    error!(
                        "WorkflowStartedEvent host_id mismatch: payload={}, envelope={}",
                        payload.host_id, host_id
                    );
                    return Ok(());
                }

                if let Some(existing_process_id) = WorkflowRepository::find_process_by_source_event(
                    tx,
                    host_id,
                    payload.wf_def_id,
                    &ce.id,
                )
                .await?
                {
                    info!(
                        source_event_id = %ce.id,
                        process_id = %existing_process_id,
                        "WorkflowStartedEvent was already projected"
                    );
                    return Ok(());
                }

                info!(
                    ">>> Workflow Triggered: host_id={}, wf_def_id={}",
                    host_id, payload.wf_def_id
                );

                // 2. Fetch Workflow Definition (DSL)
                let dsl_yaml = self
                    .get_workflow_definition(tx, &host_id, &payload.wf_def_id)
                    .await?;
                let definition: WorkflowDefinition = serde_yaml::from_str(&dsl_yaml)?;
                let raw_definition: serde_yaml::Value = serde_yaml::from_str(&dsl_yaml)?;
                let definition_snapshot: Value = serde_yaml::from_str(&dsl_yaml)?;
                let definition_digest = canonical_sha256(&definition_snapshot)
                    .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;

                let (task_name, task_def) = definition
                    .do_
                    .entries
                    .first()
                    .and_then(|entry| entry.iter().next())
                    .ok_or_else(|| {
                        sqlx::Error::Protocol("workflow has no initial task".to_string())
                    })?;
                let task_type = Self::supported_task_type(task_def).ok_or_else(|| {
                    let message = format!(
                        "unsupported initial task type for workflow {}: first task '{}' must be ask/assert/call/set/switch/run",
                        payload.wf_def_id, task_name
                    );
                    error!("{}", message);
                    sqlx::Error::Protocol(message)
                })?;
                let task_kind = Self::policy_task_kind(task_def)?;
                let security = parse_security_policy(&raw_definition)
                    .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
                let resolved_policy: ResolvedExecutionPolicy =
                    resolve_policy(task_kind, security.as_ref(), &self.execution_profiles)
                        .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
                let policy_snapshot_id = WorkflowRepository::store_policy_snapshot(
                    tx,
                    host_id,
                    &definition_digest,
                    &resolved_policy,
                    ce.user.as_deref().unwrap_or("light-workflow"),
                )
                .await?;
                let execution_profile_id = resolved_policy
                    .profile
                    .as_ref()
                    .map(|profile| profile.id.as_str())
                    .unwrap_or("host");

                // 3. Persist to process_info_t (Generic Projection)
                let inserted = self
                    .persist_process_info(
                        tx,
                        &host_id,
                        &process_id,
                        &payload.wf_def_id,
                        &wf_instance_id,
                        ce.source.as_str(),
                        &input_data,
                        &definition_snapshot,
                        &definition_digest,
                        policy_snapshot_id,
                        &resolved_policy.policy_digest,
                        &ce.id,
                        execution_profile_id,
                    )
                    .await?;
                if !inserted {
                    info!(
                        source_event_id = %ce.id,
                        "WorkflowStartedEvent lost an idempotent insert race"
                    );
                    return Ok(());
                }

                // 4. Identify and Initialize First Task
                let task_id = Uuid::new_v4();
                self.persist_task_info(
                    tx,
                    &host_id,
                    &task_id,
                    task_type,
                    &process_id,
                    &wf_instance_id,
                    task_name,
                    &input_data,
                    resolved_policy.placement,
                    &resolved_policy.policy_digest,
                )
                .await?;

                info!(
                    ">>> First Task initialized: {} ({}, {:?})",
                    task_name, task_type, resolved_policy.placement
                );

                info!(">>> Workflow instance started: {}", wf_instance_id);
            }
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

    async fn persist_process_info(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        host_id: &Uuid,
        process_id: &Uuid,
        wf_def_id: &Uuid,
        wf_instance_id: &Uuid,
        app_id: &str,
        input_data: &Value,
        definition_snapshot: &Value,
        definition_digest: &str,
        policy_snapshot_id: Uuid,
        policy_digest: &str,
        source_event_id: &str,
        execution_profile_id: &str,
    ) -> Result<bool, sqlx::Error> {
        WorkflowRepository::insert_process_if_absent(
            tx,
            &NewProcess {
                host_id: *host_id,
                process_id: *process_id,
                wf_def_id: *wf_def_id,
                wf_instance_id: wf_instance_id.to_string(),
                app_id,
                input_data,
                definition_snapshot,
                definition_digest,
                policy_snapshot_id,
                policy_digest,
                source_event_id,
                execution_profile_id,
            },
        )
        .await
    }

    async fn get_workflow_definition(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        host_id: &Uuid,
        wf_def_id: &Uuid,
    ) -> Result<String, sqlx::Error> {
        let row: (String,) = sqlx::query_as(
            "SELECT definition FROM wf_definition_t WHERE host_id = $1 AND wf_def_id = $2",
        )
        .bind(host_id)
        .bind(wf_def_id)
        .fetch_one(&mut **tx)
        .await?;
        Ok(row.0)
    }

    async fn persist_task_info(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        host_id: &Uuid,
        task_id: &Uuid,
        task_type: &str,
        process_id: &Uuid,
        wf_instance_id: &Uuid,
        wf_task_id: &str,
        task_input: &Value,
        placement: ExecutionPlacement,
        policy_digest: &str,
    ) -> Result<(), sqlx::Error> {
        WorkflowRepository::insert_task(
            tx,
            &NewTask {
                host_id: *host_id,
                task_id: *task_id,
                task_type,
                process_id: *process_id,
                wf_instance_id: wf_instance_id.to_string(),
                wf_task_id,
                task_input,
                placement,
                policy_digest,
            },
        )
        .await
    }
}
