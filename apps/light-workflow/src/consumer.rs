use crate::events::{CloudEventEnvelope, ProcessInfoDeletedPayload, WorkflowStartedPayload};
use crate::repositories::{NewProcess, NewTask, WorkflowRepository};
use execution_runner_protocol::canonical_sha256;
use serde_json::{Value, from_str, json};
use serde_yaml;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Transaction, postgres::PgListener};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, error, info};
use uuid::Uuid;
use workflow_core::models::task::{CallTaskDefinition, TaskDefinition};
use workflow_core::models::workflow::{RuntimeExpressionLanguage, WorkflowDefinition};
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

fn validate_runtime_definition(definition: &WorkflowDefinition) -> Result<(), String> {
    match definition.evaluate.as_ref() {
        Some(evaluate) if evaluate.language == RuntimeExpressionLanguage::CEL => {}
        Some(evaluate) => {
            return Err(format!(
                "light-workflow supports evaluate.language 'cel', not '{}'",
                evaluate.language
            ));
        }
        None => {
            return Err(
                "light-workflow requires evaluate.language: cel; Open Workflow defaults an omitted evaluate block to jq"
                    .to_string(),
            );
        }
    }

    for entry in &definition.do_.entries {
        let Some((task_name, task)) = entry.iter().next() else {
            return Err("workflow task entry is empty".to_string());
        };
        match task {
            TaskDefinition::LegacyAgent(_) => {
                return Err(format!(
                    "task '{task_name}' uses deprecated standalone agentTask; use call: agent"
                ));
            }
            TaskDefinition::Ask(_)
            | TaskDefinition::Assert(_)
            | TaskDefinition::Set(_)
            | TaskDefinition::Switch(_) => {}
            TaskDefinition::Run(run)
                if run.run.shell.is_some()
                    || run.run.container.is_some()
                    || run.run.script.is_some() => {}
            TaskDefinition::Run(_) => {
                return Err(format!(
                    "task '{task_name}' uses run.workflow, which is not supported by light-workflow"
                ));
            }
            TaskDefinition::Call(call) => match call {
                CallTaskDefinition::Http(_)
                | CallTaskDefinition::JsonRpc(_)
                | CallTaskDefinition::OpenRpc(_)
                | CallTaskDefinition::Agent(_)
                | CallTaskDefinition::Rule(_) => {}
                CallTaskDefinition::Mcp(call) => {
                    if call
                        .with
                        .transport
                        .as_ref()
                        .is_some_and(|transport| transport.stdio.is_some())
                    {
                        return Err(format!(
                            "task '{task_name}' uses MCP stdio, which is not supported by the durable executor"
                        ));
                    }
                    if let Some(method) = call.with.method.as_deref()
                        && !matches!(
                            method,
                            "tools/list"
                                | "tools/call"
                                | "prompts/list"
                                | "prompts/get"
                                | "resources/list"
                                | "resources/read"
                                | "resources/templates/list"
                        )
                    {
                        return Err(format!(
                            "task '{task_name}' uses unsupported MCP method '{method}'"
                        ));
                    }
                }
                CallTaskDefinition::AsyncApi(_) => {
                    return Err(format!(
                        "task '{task_name}' uses call asyncapi, which is not implemented by light-workflow"
                    ));
                }
                CallTaskDefinition::Grpc(_) => {
                    return Err(format!(
                        "task '{task_name}' uses call grpc, which is not implemented by light-workflow"
                    ));
                }
                CallTaskDefinition::OpenApi(_) => {
                    return Err(format!(
                        "task '{task_name}' uses call openapi, which is not implemented by light-workflow"
                    ));
                }
                CallTaskDefinition::A2a(_) => {
                    return Err(format!(
                        "task '{task_name}' uses call a2a, which is not implemented by light-workflow"
                    ));
                }
                CallTaskDefinition::Function(_) => {
                    return Err(format!(
                        "task '{task_name}' uses a custom function call, which is not implemented by light-workflow"
                    ));
                }
            },
            TaskDefinition::Fork(_) => {
                return Err(format!(
                    "task '{task_name}' uses fork, which is not yet supported by the execution-policy placement layer"
                ));
            }
            TaskDefinition::Do(_) => {
                return Err(format!("task '{task_name}' uses unimplemented task do"));
            }
            TaskDefinition::Emit(_) => {
                return Err(format!("task '{task_name}' uses unimplemented task emit"));
            }
            TaskDefinition::For(_) => {
                return Err(format!("task '{task_name}' uses unimplemented task for"));
            }
            TaskDefinition::Listen(_) => {
                return Err(format!("task '{task_name}' uses unimplemented task listen"));
            }
            TaskDefinition::Raise(_) => {
                return Err(format!("task '{task_name}' uses unimplemented task raise"));
            }
            TaskDefinition::Try(_) => {
                return Err(format!("task '{task_name}' uses unimplemented task try"));
            }
            TaskDefinition::Wait(_) => {
                return Err(format!("task '{task_name}' uses unimplemented task wait"));
            }
        }
    }
    Ok(())
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
        // Keep the permanent LISTEN connection out of the transaction pool.
        let database_url = std::env::var("DATABASE_URL")
            .map_err(|error| sqlx::Error::Configuration(Box::new(error)))?;
        let mut listener = PgListener::connect(&database_url).await?;
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
            if let Some(data) = ce.data.clone() {
                let payload: WorkflowStartedPayload =
                    serde_json::from_value(data).map_err(|error| {
                        sqlx::Error::Protocol(format!("invalid WorkflowStartedPayload: {error}"))
                    })?;

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
                    return Err(sqlx::Error::Protocol(
                        "WorkflowStartedEvent host_id mismatch".to_string(),
                    )
                    .into());
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
                validate_runtime_definition(&definition).map_err(sqlx::Error::Protocol)?;
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
            validate_runtime_definition(&jq)
                .unwrap_err()
                .contains("evaluate.language 'cel'")
        );

        let implicit_jq: WorkflowDefinition = serde_yaml::from_str(
            "document: { dsl: 1.0.3, namespace: test, name: implicit-jq, version: 1.0.0 }\ndo:\n  - start:\n      set: { ok: true }",
        )
        .unwrap();
        assert_eq!(
            validate_runtime_definition(&implicit_jq).unwrap_err(),
            "light-workflow requires evaluate.language: cel; Open Workflow defaults an omitted evaluate block to jq"
        );

        let wait: WorkflowDefinition = serde_yaml::from_str(
            "document: { dsl: 1.0.3, namespace: test, name: wait, version: 1.0.0 }\nevaluate: { language: cel }\ndo:\n  - pause:\n      wait: PT1S",
        )
        .unwrap();
        assert_eq!(
            validate_runtime_definition(&wait).unwrap_err(),
            "task 'pause' uses unimplemented task wait"
        );
    }

    #[test]
    fn runtime_definition_accepts_canonical_http_mcp_and_rejects_stdio() {
        let http: WorkflowDefinition = serde_yaml::from_str(
            "document: { dsl: 1.0.3, namespace: test, name: mcp-http, version: 1.0.0 }\nevaluate: { language: cel }\ndo:\n  - listTools:\n      call: mcp\n      with:\n        method: tools/list\n        transport:\n          http:\n            endpoint: https://gateway.example/mcp",
        )
        .unwrap();
        validate_runtime_definition(&http).expect("canonical MCP HTTP is executable");

        let stdio: WorkflowDefinition = serde_yaml::from_str(
            "document: { dsl: 1.0.3, namespace: test, name: mcp-stdio, version: 1.0.0 }\nevaluate: { language: cel }\ndo:\n  - listTools:\n      call: mcp\n      with:\n        method: tools/list\n        transport:\n          stdio:\n            command: mcp-server",
        )
        .unwrap();
        assert!(
            validate_runtime_definition(&stdio)
                .unwrap_err()
                .contains("MCP stdio")
        );
    }
}
