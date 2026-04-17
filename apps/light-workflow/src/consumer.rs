use crate::events::{CloudEventEnvelope, WorkflowStartedPayload};
use serde_json::from_str;
use serde_yaml;
use sqlx::{PgPool, Postgres, Transaction, postgres::PgListener};
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, error, info};
use uuid::Uuid;
use workflow_core::models::workflow::WorkflowDefinition;

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
}

impl EventConsumer {
    fn supported_task_type(
        task_def: &workflow_core::models::task::TaskDefinition,
    ) -> Option<&'static str> {
        match task_def {
            workflow_core::models::task::TaskDefinition::Call(_) => Some("call"),
            workflow_core::models::task::TaskDefinition::Set(_) => Some("set"),
            workflow_core::models::task::TaskDefinition::Switch(_) => Some("switch"),
            _ => None,
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
        }
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
            if let Some(data) = ce.data {
                let payload: WorkflowStartedPayload = match serde_json::from_value(data) {
                    Ok(p) => p,
                    Err(e) => {
                        error!("Failed to parse WorkflowStartedPayload: {}", e);
                        return Ok(());
                    }
                };

                // 1. Generate ids
                let wf_instance_id = Uuid::new_v4();
                let process_id = Uuid::new_v4();
                let host_id: Uuid = event.host_id.parse()?;

                if payload.host_id != host_id {
                    error!(
                        "WorkflowStartedEvent host_id mismatch: payload={}, envelope={}",
                        payload.host_id, host_id
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

                // 3. Persist to process_info_t (Generic Projection)
                self.persist_process_info(
                    tx,
                    &host_id,
                    &process_id,
                    &payload.wf_def_id,
                    &wf_instance_id,
                    ce.source.as_str(),
                    payload.data.as_ref(),
                )
                .await?;

                // 4. Identify and Initialize First Task
                if let Some(entry) = definition.do_.entries.first() {
                    // Map key is task name, value is TaskDefinition
                    if let Some((task_name, task_def)) = entry.iter().next() {
                        let task_id = Uuid::new_v4();
                        let task_type = Self::supported_task_type(task_def).ok_or_else(|| {
                            let message = format!(
                                "unsupported initial task type for workflow {}: first task '{}' must be one of call/set/switch",
                                payload.wf_def_id, task_name
                            );
                            error!("{}", message);
                            sqlx::Error::Protocol(message)
                        })?;

                        self.persist_task_info(
                            tx,
                            &host_id,
                            &task_id,
                            task_type,
                            &process_id,
                            &wf_instance_id,
                            task_name,
                            payload.data.as_ref(),
                        )
                        .await?;

                        info!(">>> First Task initialized: {} ({})", task_name, task_type);
                    }
                }

                info!(">>> Workflow instance started: {}", wf_instance_id);
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
        input_data: Option<&serde_json::Value>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO process_info_t (
                host_id, process_id, wf_def_id, wf_instance_id, app_id, 
                process_type, status_code, started_ts, ex_trigger_ts, 
                input_data, context_data
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, $8, $9)
            "#,
        )
        .bind(host_id)
        .bind(process_id)
        .bind(wf_def_id)
        .bind(wf_instance_id.to_string())
        .bind(app_id)
        .bind("Workflow") // process_type
        .bind("A") // status_code (Active)
        .bind(input_data)
        .bind(input_data) // Initial context is the input
        .execute(&mut **tx)
        .await?;
        Ok(())
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
        task_input: Option<&serde_json::Value>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO task_info_t (
                host_id, task_id, task_type, process_id, wf_instance_id, 
                wf_task_id, status_code, started_ts, locked, priority, 
                task_input
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, CURRENT_TIMESTAMP, $8, $9, $10)
            "#,
        )
        .bind(host_id)
        .bind(task_id)
        .bind(task_type)
        .bind(process_id)
        .bind(wf_instance_id.to_string())
        .bind(wf_task_id)
        .bind("A") // status_code (Active)
        .bind("N") // locked ('N')
        .bind(1) // priority
        .bind(task_input)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }
}
