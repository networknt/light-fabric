use super::{AuditEventKind, AuditWal, WalRecord};
use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, Transaction};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;
use tracing::{error, warn};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct AuditSinkConfig {
    pub batch_records: usize,
    pub batch_bytes: usize,
    pub poll_interval: Duration,
    pub retry_initial: Duration,
    pub retry_max: Duration,
}

impl Default for AuditSinkConfig {
    fn default() -> Self {
        Self {
            batch_records: 256,
            batch_bytes: 1024 * 1024,
            poll_interval: Duration::from_millis(100),
            retry_initial: Duration::from_millis(100),
            retry_max: Duration::from_secs(10),
        }
    }
}

pub struct AuditSinkTask {
    cancellation: CancellationToken,
    handle: tokio::task::JoinHandle<()>,
}

impl AuditSinkTask {
    pub fn stop(&self) {
        self.cancellation.cancel();
        self.handle.abort();
    }
}

impl Drop for AuditSinkTask {
    fn drop(&mut self) {
        self.stop();
    }
}

pub struct PostgresAuditSink;

impl PostgresAuditSink {
    pub fn start(wal: Arc<AuditWal>, pool: PgPool, config: AuditSinkConfig) -> AuditSinkTask {
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let handle = tokio::spawn(async move {
            let mut retry = config.retry_initial;
            loop {
                if task_cancellation.is_cancelled() {
                    break;
                }
                let replay_wal = Arc::clone(&wal);
                let batch_records = config.batch_records;
                let batch_bytes = config.batch_bytes;
                let batch = tokio::task::spawn_blocking(move || {
                    replay_wal.replay_batch(batch_records, batch_bytes)
                })
                .await;
                let batch = match batch {
                    Ok(Ok(batch)) => batch,
                    Ok(Err(error)) => {
                        error!(error = %error, "LLM audit WAL replay failed");
                        break;
                    }
                    Err(error) => {
                        error!(error = %error, "LLM audit WAL replay task failed");
                        break;
                    }
                };
                if batch.is_empty() {
                    retry = config.retry_initial;
                    tokio::select! {
                        _ = task_cancellation.cancelled() => break,
                        _ = tokio::time::sleep(config.poll_interval) => {}
                    }
                    continue;
                }
                match ingest_batch(&pool, &batch).await {
                    Ok(()) => {
                        let sequence = batch.last().map(|record| record.sequence).unwrap_or(0);
                        let acknowledge_wal = Arc::clone(&wal);
                        match tokio::task::spawn_blocking(move || {
                            acknowledge_wal.acknowledge(sequence)
                        })
                        .await
                        {
                            Ok(Ok(())) => retry = config.retry_initial,
                            Ok(Err(error)) => {
                                error!(error = %error, sequence, "LLM audit checkpoint failed after authoritative ingest");
                                break;
                            }
                            Err(error) => {
                                error!(error = %error, sequence, "LLM audit checkpoint task failed");
                                break;
                            }
                        }
                    }
                    Err(error) => {
                        warn!(error = %error, retry_ms = retry.as_millis(), "LLM audit sink unavailable; retaining WAL batch");
                        tokio::select! {
                            _ = task_cancellation.cancelled() => break,
                            _ = tokio::time::sleep(retry) => {}
                        }
                        retry = retry.saturating_mul(2).min(config.retry_max);
                    }
                }
            }
        });
        AuditSinkTask {
            cancellation,
            handle,
        }
    }
}

pub async fn ingest_batch(pool: &PgPool, records: &[WalRecord]) -> Result<(), sqlx::Error> {
    let mut transaction = pool.begin().await?;
    for record in records {
        ingest_record(&mut transaction, record).await?;
    }
    transaction.commit().await
}

async fn ingest_record(
    transaction: &mut Transaction<'_, Postgres>,
    record: &WalRecord,
) -> Result<(), sqlx::Error> {
    let event = &record.event;
    let event_id = Uuid::parse_str(&event.event_id).map_err(protocol_error)?;
    let request_id = Uuid::parse_str(&event.request_id).map_err(protocol_error)?;
    let event_ts = DateTime::parse_from_rfc3339(&event.timestamp)
        .map_err(protocol_error)?
        .with_timezone(&Utc);
    let event_day = event_ts.date_naive();
    let request_day = chrono::NaiveDate::parse_from_str(&event.request_day, "%Y-%m-%d")
        .map_err(protocol_error)?;
    let event_kind = match event.kind {
        AuditEventKind::RequestAdmitted => "request_admitted",
        AuditEventKind::AttemptStarted => "attempt_started",
        AuditEventKind::AttemptFinished => "attempt_finished",
        AuditEventKind::RequestFinished => "request_finished",
    };
    let inserted = sqlx::query(
        "INSERT INTO llm_audit_event_t (event_day,event_id,schema_version,event_kind,request_id,attempt_no,attempt_count,event_ts,generation,snapshot_digest,host_id,public_alias,operation,status,category,deployment_id,duration_ms,content_mode,pii_profile,principal_digest,charged_micros,usage_complete) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22) ON CONFLICT (event_day,event_id) DO NOTHING",
    )
    .bind(event_day)
    .bind(event_id)
    .bind(i16::try_from(event.schema_version).unwrap_or(i16::MAX))
    .bind(event_kind)
    .bind(request_id)
    .bind(event.attempt.and_then(|value| i32::try_from(value).ok()))
    .bind(event.attempt_count.and_then(|value| i32::try_from(value).ok()))
    .bind(event_ts)
    .bind(i64::try_from(event.generation).unwrap_or(i64::MAX))
    .bind(&event.snapshot_digest)
    .bind(&event.host_id)
    .bind(&event.public_alias)
    .bind(&event.operation)
    .bind(&event.status)
    .bind(&event.category)
    .bind(&event.deployment_id)
    .bind(i64::try_from(event.duration_ms).unwrap_or(i64::MAX))
    .bind(&event.content_mode)
    .bind(&event.pii_profile)
    .bind(&event.principal_digest)
    .bind(event.charged_micros.and_then(|value| i64::try_from(value).ok()))
    .bind(event.usage_complete)
    .execute(&mut **transaction)
    .await?
    .rows_affected();
    if inserted == 0 {
        return Ok(());
    }

    match event.kind {
        AuditEventKind::RequestAdmitted => {
            sqlx::query("INSERT INTO llm_request_t (event_day,request_id,admitted_event_id,host_id,public_alias,generation,snapshot_digest) VALUES ($1,$2,$3,$4,$5,$6,$7) ON CONFLICT (event_day,request_id) DO NOTHING")
                .bind(request_day).bind(request_id).bind(event_id).bind(&event.host_id)
                .bind(&event.public_alias)
                .bind(i64::try_from(event.generation).unwrap_or(i64::MAX))
                .bind(&event.snapshot_digest)
                .execute(&mut **transaction).await?;
        }
        AuditEventKind::AttemptStarted => {
            sqlx::query("INSERT INTO llm_attempt_t (event_day,request_id,attempt_no,started_event_id,deployment_id) VALUES ($1,$2,$3,$4,$5) ON CONFLICT (event_day,request_id,attempt_no) DO NOTHING")
                .bind(request_day).bind(request_id)
                .bind(event.attempt.and_then(|value| i32::try_from(value).ok()).unwrap_or_default())
                .bind(event_id).bind(event.deployment_id.as_deref().unwrap_or("unknown"))
                .execute(&mut **transaction).await?;
        }
        AuditEventKind::AttemptFinished => {
            sqlx::query("UPDATE llm_attempt_t SET finished_event_id=$4,terminal_status=$5,incomplete=false WHERE event_day=$1 AND request_id=$2 AND attempt_no=$3")
                .bind(request_day).bind(request_id)
                .bind(event.attempt.and_then(|value| i32::try_from(value).ok()).unwrap_or_default())
                .bind(event_id).bind(&event.status).execute(&mut **transaction).await?;
        }
        AuditEventKind::RequestFinished => {
            sqlx::query("UPDATE llm_request_t SET finished_event_id=$3,terminal_status=$4,charged_micros=$5,usage_complete=$6,attempt_count=$7,incomplete=false WHERE event_day=$1 AND request_id=$2")
                .bind(request_day).bind(request_id).bind(event_id).bind(&event.status)
                .bind(event.charged_micros.and_then(|value| i64::try_from(value).ok()))
                .bind(event.usage_complete)
                .bind(event.attempt_count.and_then(|value| i32::try_from(value).ok()))
                .execute(&mut **transaction).await?;
        }
    }
    Ok(())
}

fn protocol_error(error: impl std::fmt::Display) -> sqlx::Error {
    sqlx::Error::Protocol(format!("invalid LLM audit WAL event: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn explicit_stop_aborts_the_audit_sink_worker() {
        let cancellation = CancellationToken::new();
        let task = AuditSinkTask {
            cancellation: cancellation.clone(),
            handle: tokio::spawn(std::future::pending()),
        };

        task.stop();
        tokio::task::yield_now().await;

        assert!(cancellation.is_cancelled());
        assert!(task.handle.is_finished());
    }
}
