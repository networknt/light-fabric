mod sink;
mod wal;

pub use sink::{AuditSinkConfig, AuditSinkTask, PostgresAuditSink};
pub use wal::{AuditWal, WalConfig, WalRecord, WalStatus};

use async_trait::async_trait;
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use uuid::Uuid;

use crate::config::AuditMode;
use crate::error::LlmGatewayError;

#[derive(Debug, Clone)]
pub struct AuditStart {
    pub request_id: String,
    pub principal_id: String,
    pub alias: String,
    pub generation: u64,
    pub snapshot_digest: String,
    pub max_attempts: usize,
    pub pii_profile: String,
}

#[derive(Debug, Clone)]
pub struct AuditAttemptStart {
    pub attempt: usize,
    pub deployment_id: String,
}

#[derive(Debug, Clone)]
pub struct AuditAttemptFinish {
    pub attempt: usize,
    pub terminal: &'static str,
    pub category: &'static str,
}

#[derive(Debug, Clone)]
pub struct AuditFinish {
    pub terminal: &'static str,
    pub attempts: usize,
    pub charged_micros: u64,
    pub usage_complete: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuditEventKind {
    RequestAdmitted,
    AttemptStarted,
    AttemptFinished,
    RequestFinished,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AuditEvent {
    pub schema_version: u16,
    pub event_id: String,
    pub kind: AuditEventKind,
    pub request_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attempt_count: Option<usize>,
    pub timestamp: String,
    /// Immutable logical-call partition key captured at admission.
    pub request_day: String,
    pub generation: u64,
    pub snapshot_digest: String,
    pub host_id: String,
    pub public_alias: String,
    pub operation: String,
    pub status: String,
    pub category: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deployment_id: Option<String>,
    pub duration_ms: u64,
    pub content_mode: String,
    pub pii_profile: String,
    pub principal_digest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub charged_micros: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_complete: Option<bool>,
}

#[async_trait]
pub trait AuditReservation: Send + Sync {
    async fn attempt_started(&self, _start: AuditAttemptStart) -> Result<(), LlmGatewayError> {
        Ok(())
    }

    async fn attempt_finished(&self, _finish: AuditAttemptFinish) -> Result<(), LlmGatewayError> {
        Ok(())
    }

    async fn finish(self: Box<Self>, finish: AuditFinish) -> Result<(), LlmGatewayError>;
}

#[async_trait]
pub trait AuditAdmission: Send + Sync {
    async fn reserve(
        &self,
        mode: AuditMode,
        start: AuditStart,
    ) -> Result<Box<dyn AuditReservation>, LlmGatewayError>;
}

#[derive(Default)]
pub struct DisabledAudit;

struct DisabledReservation;

#[async_trait]
impl AuditReservation for DisabledReservation {
    async fn finish(self: Box<Self>, _finish: AuditFinish) -> Result<(), LlmGatewayError> {
        Ok(())
    }
}

#[async_trait]
impl AuditAdmission for DisabledAudit {
    async fn reserve(
        &self,
        mode: AuditMode,
        _start: AuditStart,
    ) -> Result<Box<dyn AuditReservation>, LlmGatewayError> {
        if mode != AuditMode::Disabled {
            return Err(LlmGatewayError::AuditUnavailable);
        }
        Ok(Box::new(DisabledReservation))
    }
}

pub fn disabled_audit() -> Arc<dyn AuditAdmission> {
    Arc::new(DisabledAudit)
}

#[derive(Default)]
struct AuditCounters {
    reserved: AtomicU64,
    finished: AtomicU64,
}

#[derive(Clone, Default)]
pub struct ProcessAudit {
    counters: Arc<AuditCounters>,
}

impl ProcessAudit {
    pub fn reserved(&self) -> u64 {
        self.counters.reserved.load(Ordering::Acquire)
    }
    pub fn finished(&self) -> u64 {
        self.counters.finished.load(Ordering::Acquire)
    }
}

struct ProcessReservation {
    counters: Arc<AuditCounters>,
}

#[async_trait]
impl AuditReservation for ProcessReservation {
    async fn finish(self: Box<Self>, _finish: AuditFinish) -> Result<(), LlmGatewayError> {
        self.counters.finished.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

#[async_trait]
impl AuditAdmission for ProcessAudit {
    async fn reserve(
        &self,
        mode: AuditMode,
        _start: AuditStart,
    ) -> Result<Box<dyn AuditReservation>, LlmGatewayError> {
        if mode.is_local_durable() || mode == AuditMode::RemoteDurable {
            return Err(LlmGatewayError::AuditUnavailable);
        }
        self.counters.reserved.fetch_add(1, Ordering::AcqRel);
        Ok(Box::new(ProcessReservation {
            counters: Arc::clone(&self.counters),
        }))
    }
}

pub struct WalAudit {
    wal: Arc<AuditWal>,
    host_id: String,
}

impl WalAudit {
    pub fn open(config: WalConfig, host_id: impl Into<String>) -> Result<Self, LlmGatewayError> {
        Ok(Self {
            wal: AuditWal::open(config)?,
            host_id: host_id.into(),
        })
    }

    pub fn status(&self) -> WalStatus {
        self.wal.status()
    }

    pub fn start_postgres_sink(
        &self,
        pool: sqlx::PgPool,
        config: AuditSinkConfig,
    ) -> AuditSinkTask {
        PostgresAuditSink::start(Arc::clone(&self.wal), pool, config)
    }
}

struct WalReservation {
    wal: Arc<AuditWal>,
    _envelope: Option<wal::WalEnvelope>,
    start: AuditStart,
    host_id: String,
    mode: AuditMode,
    request_day: String,
    admitted_at: Instant,
}

#[async_trait]
impl AuditReservation for WalReservation {
    async fn attempt_started(&self, start: AuditAttemptStart) -> Result<(), LlmGatewayError> {
        let event = event(
            &self.start,
            &self.host_id,
            AuditEventKind::AttemptStarted,
            Some(start.attempt),
            "started",
            "dispatch",
            Some(&start.deployment_id),
            None,
            None,
            &self.request_day,
            self.admitted_at,
        );
        match self.wal.append(&event, self.mode.is_local_durable()).await {
            Err(_) if self.mode == AuditMode::BestEffort => Ok(()),
            result => result.map(|_| ()),
        }
    }

    async fn attempt_finished(&self, finish: AuditAttemptFinish) -> Result<(), LlmGatewayError> {
        let event = event(
            &self.start,
            &self.host_id,
            AuditEventKind::AttemptFinished,
            Some(finish.attempt),
            finish.terminal,
            finish.category,
            None,
            None,
            None,
            &self.request_day,
            self.admitted_at,
        );
        match self.wal.append(&event, false).await {
            Err(_) if self.mode == AuditMode::BestEffort => Ok(()),
            result => result.map(|_| ()),
        }
    }

    async fn finish(self: Box<Self>, finish: AuditFinish) -> Result<(), LlmGatewayError> {
        let mut event = event(
            &self.start,
            &self.host_id,
            AuditEventKind::RequestFinished,
            None,
            finish.terminal,
            if finish.usage_complete {
                "usage_complete"
            } else {
                "usage_incomplete"
            },
            None,
            Some(finish.charged_micros),
            Some(finish.usage_complete),
            &self.request_day,
            self.admitted_at,
        );
        event.attempt_count = Some(finish.attempts);
        let durable = self.mode.is_local_durable() || self.wal.terminal_commit_before_response();
        match self.wal.append(&event, durable).await {
            Err(_) if self.mode == AuditMode::BestEffort => Ok(()),
            result => result.map(|_| ()),
        }
    }
}

#[async_trait]
impl AuditAdmission for WalAudit {
    async fn reserve(
        &self,
        mode: AuditMode,
        start: AuditStart,
    ) -> Result<Box<dyn AuditReservation>, LlmGatewayError> {
        if mode == AuditMode::Disabled {
            return Ok(Box::new(DisabledReservation));
        }
        if mode == AuditMode::RemoteDurable {
            return Err(LlmGatewayError::AuditUnavailable);
        }
        if mode.is_local_durable() && !self.wal.config.persistent_volume {
            return Err(LlmGatewayError::AuditUnavailable);
        }
        let envelope = match self.wal.reserve_envelope(start.max_attempts) {
            Ok(envelope) => Some(envelope),
            Err(_) if mode == AuditMode::BestEffort => None,
            Err(error) => return Err(error),
        };
        let reservation = WalReservation {
            wal: Arc::clone(&self.wal),
            _envelope: envelope,
            start,
            host_id: self.host_id.clone(),
            mode,
            request_day: Utc::now().date_naive().to_string(),
            admitted_at: Instant::now(),
        };
        let admitted = event(
            &reservation.start,
            &reservation.host_id,
            AuditEventKind::RequestAdmitted,
            None,
            "admitted",
            "accepted",
            None,
            None,
            None,
            &reservation.request_day,
            reservation.admitted_at,
        );
        if let Err(error) = reservation.wal.append(&admitted, false).await
            && mode != AuditMode::BestEffort
        {
            return Err(error);
        }
        Ok(Box::new(reservation))
    }
}

#[allow(clippy::too_many_arguments)]
fn event(
    start: &AuditStart,
    host_id: &str,
    kind: AuditEventKind,
    attempt: Option<usize>,
    status: &str,
    category: &str,
    deployment_id: Option<&str>,
    charged_micros: Option<u64>,
    usage_complete: Option<bool>,
    request_day: &str,
    admitted_at: Instant,
) -> AuditEvent {
    AuditEvent {
        schema_version: 1,
        event_id: Uuid::now_v7().to_string(),
        kind,
        request_id: start.request_id.clone(),
        attempt,
        attempt_count: None,
        timestamp: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        request_day: request_day.to_string(),
        generation: start.generation,
        snapshot_digest: start.snapshot_digest.clone(),
        host_id: host_id.to_string(),
        public_alias: start.alias.clone(),
        operation: "chat_completions".to_string(),
        status: status.to_string(),
        category: category.to_string(),
        deployment_id: deployment_id.map(str::to_string),
        duration_ms: admitted_at
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX),
        content_mode: "metadata_only".to_string(),
        pii_profile: start.pii_profile.clone(),
        principal_digest: format!("{:x}", Sha256::digest(start.principal_id.as_bytes())),
        charged_micros,
        usage_complete,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::time::Duration;

    fn config(directory: &std::path::Path) -> WalConfig {
        WalConfig {
            directory: directory.to_path_buf(),
            gateway_instance: "gateway-test".to_string(),
            max_record_bytes: 4 * 1024,
            max_segment_bytes: 32 * 1024,
            max_spool_bytes: 128 * 1024,
            queue_records: 16,
            batch_records: 8,
            batch_bytes: 32 * 1024,
            commit_delay: Duration::from_millis(1),
            terminal_commit_before_response: false,
            persistent_volume: true,
        }
    }

    fn start() -> AuditStart {
        AuditStart {
            request_id: Uuid::now_v7().to_string(),
            principal_id: "principal-secret".to_string(),
            alias: "public-model".to_string(),
            generation: 7,
            snapshot_digest: "a".repeat(64),
            max_attempts: 1,
            pii_profile: "none".to_string(),
        }
    }

    fn segment(directory: &std::path::Path) -> std::path::PathBuf {
        let mut paths = std::fs::read_dir(directory)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|value| value == "wal"))
            .collect::<Vec<_>>();
        paths.sort();
        paths.pop().unwrap()
    }

    #[tokio::test]
    async fn local_durable_start_reaches_watermark_and_recovery_has_no_incomplete_attempt() {
        let directory = tempfile::tempdir().unwrap();
        let audit = WalAudit::open(config(directory.path()), "host-a").unwrap();
        let reservation = audit
            .reserve(AuditMode::LocalDurable, start())
            .await
            .unwrap();
        reservation
            .attempt_started(AuditAttemptStart {
                attempt: 1,
                deployment_id: "deployment-a".to_string(),
            })
            .await
            .unwrap();
        assert!(audit.status().durable_sequence >= 2);
        reservation
            .attempt_finished(AuditAttemptFinish {
                attempt: 1,
                terminal: "complete",
                category: "success",
            })
            .await
            .unwrap();
        reservation
            .finish(AuditFinish {
                terminal: "complete",
                attempts: 1,
                charged_micros: 42,
                usage_complete: true,
            })
            .await
            .unwrap();
        drop(audit);
        std::thread::sleep(Duration::from_millis(10));

        let recovered = WalAudit::open(config(directory.path()), "host-a").unwrap();
        assert_eq!(recovered.status().recovered_incomplete_attempts, 0);
        assert!(recovered.status().durable_sequence >= 4);
    }

    #[tokio::test]
    async fn recovery_truncates_only_a_partial_tail_and_rejects_checksum_corruption() {
        let directory = tempfile::tempdir().unwrap();
        let audit = WalAudit::open(config(directory.path()), "host-a").unwrap();
        let reservation = audit
            .reserve(AuditMode::LocalDurable, start())
            .await
            .unwrap();
        reservation
            .attempt_started(AuditAttemptStart {
                attempt: 1,
                deployment_id: "deployment-a".to_string(),
            })
            .await
            .unwrap();
        drop(reservation);
        drop(audit);
        std::thread::sleep(Duration::from_millis(10));

        let path = segment(directory.path());
        let committed_len = std::fs::metadata(&path).unwrap().len();
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&[0xaa, 0xbb, 0xcc])
            .unwrap();
        let recovered = WalAudit::open(config(directory.path()), "host-a").unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), committed_len);
        assert_eq!(recovered.status().recovered_incomplete_attempts, 1);
        drop(recovered);
        std::thread::sleep(Duration::from_millis(10));

        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        file.seek(SeekFrom::Start(wal::HEADER_BYTES + 4)).unwrap();
        let mut byte = [0_u8; 1];
        file.read_exact(&mut byte).unwrap();
        file.seek(SeekFrom::Current(-1)).unwrap();
        file.write_all(&[byte[0] ^ 0xff]).unwrap();
        file.sync_data().unwrap();
        assert!(WalAudit::open(config(directory.path()), "host-a").is_err());
    }

    #[tokio::test]
    async fn required_capacity_and_persistent_volume_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        let mut bounded = config(directory.path());
        bounded.max_segment_bytes = 20 * 1024;
        bounded.max_spool_bytes = 20 * 1024;
        let audit = WalAudit::open(bounded, "host-a").unwrap();
        let _first = audit.reserve(AuditMode::Required, start()).await.unwrap();
        assert!(matches!(
            audit.reserve(AuditMode::Required, start()).await,
            Err(LlmGatewayError::AuditUnavailable)
        ));

        let other = tempfile::tempdir().unwrap();
        let mut ephemeral = config(other.path());
        ephemeral.persistent_volume = false;
        let audit = WalAudit::open(ephemeral, "host-a").unwrap();
        assert!(matches!(
            audit.reserve(AuditMode::LocalDurable, start()).await,
            Err(LlmGatewayError::AuditUnavailable)
        ));
    }

    #[tokio::test]
    async fn segment_rotation_preserves_monotonic_recovery() {
        let directory = tempfile::tempdir().unwrap();
        let mut rotating = config(directory.path());
        rotating.max_record_bytes = 1024;
        rotating.max_segment_bytes = 1600;
        rotating.max_spool_bytes = 64 * 1024;
        rotating.batch_bytes = 8 * 1024;
        let audit = WalAudit::open(rotating.clone(), "host-a").unwrap();
        for _ in 0..4 {
            let reservation = audit
                .reserve(AuditMode::LocalDurable, start())
                .await
                .unwrap();
            reservation
                .attempt_started(AuditAttemptStart {
                    attempt: 1,
                    deployment_id: "deployment-a".to_string(),
                })
                .await
                .unwrap();
            reservation
                .attempt_finished(AuditAttemptFinish {
                    attempt: 1,
                    terminal: "complete",
                    category: "success",
                })
                .await
                .unwrap();
            reservation
                .finish(AuditFinish {
                    terminal: "complete",
                    attempts: 1,
                    charged_micros: 1,
                    usage_complete: true,
                })
                .await
                .unwrap();
        }
        assert!(audit.status().segments > 1);
        let durable = audit.status().durable_sequence;
        drop(audit);
        std::thread::sleep(Duration::from_millis(10));
        let recovered = WalAudit::open(rotating, "host-a").unwrap();
        assert_eq!(recovered.status().durable_sequence, durable);
        assert_eq!(recovered.status().recovered_incomplete_attempts, 0);
    }

    #[tokio::test]
    async fn authoritative_checkpoint_replays_idempotently_and_reclaims_only_old_segments() {
        let directory = tempfile::tempdir().unwrap();
        let mut rotating = config(directory.path());
        rotating.max_record_bytes = 1024;
        rotating.max_segment_bytes = 900;
        rotating.max_spool_bytes = 64 * 1024;
        rotating.batch_bytes = 8 * 1024;
        let audit = WalAudit::open(rotating.clone(), "host-a").unwrap();
        for _ in 0..3 {
            let reservation = audit
                .reserve(AuditMode::LocalDurable, start())
                .await
                .unwrap();
            reservation
                .attempt_started(AuditAttemptStart {
                    attempt: 1,
                    deployment_id: "deployment-a".to_string(),
                })
                .await
                .unwrap();
            reservation
                .attempt_finished(AuditAttemptFinish {
                    attempt: 1,
                    terminal: "complete",
                    category: "success",
                })
                .await
                .unwrap();
            reservation
                .finish(AuditFinish {
                    terminal: "complete",
                    attempts: 1,
                    charged_micros: 1,
                    usage_complete: true,
                })
                .await
                .unwrap();
        }
        let batch = audit.wal.replay_batch(64, 64 * 1024).unwrap();
        assert_eq!(batch.len(), 12);
        let acknowledged = batch[7].sequence;
        let before_segments = audit.status().segments;
        audit.wal.acknowledge(acknowledged).unwrap();
        let status = audit.status();
        assert_eq!(status.acknowledged_sequence, acknowledged);
        assert_eq!(status.sink_lag_records, 4);
        assert!(status.segments <= before_segments);
        assert_eq!(audit.wal.replay_batch(64, 64 * 1024).unwrap().len(), 4);
        drop(audit);
        std::thread::sleep(Duration::from_millis(10));

        let recovered = WalAudit::open(rotating, "host-a").unwrap();
        assert_eq!(recovered.status().acknowledged_sequence, acknowledged);
        assert_eq!(recovered.wal.replay_batch(64, 64 * 1024).unwrap().len(), 4);
    }

    #[tokio::test]
    async fn postgres_sink_duplicate_delivery_is_idempotent_when_database_is_available() {
        let Ok(database_url) = std::env::var("LLM_AUDIT_TEST_DATABASE_URL") else {
            return;
        };
        let directory = tempfile::tempdir().unwrap();
        let audit = WalAudit::open(config(directory.path()), "host-a").unwrap();
        let audit_start = start();
        let request_id = audit_start.request_id.clone();
        let reservation = audit
            .reserve(AuditMode::LocalDurable, audit_start)
            .await
            .unwrap();
        reservation
            .attempt_started(AuditAttemptStart {
                attempt: 1,
                deployment_id: "deployment-a".to_string(),
            })
            .await
            .unwrap();
        reservation
            .attempt_finished(AuditAttemptFinish {
                attempt: 1,
                terminal: "complete",
                category: "success",
            })
            .await
            .unwrap();
        reservation
            .finish(AuditFinish {
                terminal: "complete",
                attempts: 1,
                charged_micros: 42,
                usage_complete: true,
            })
            .await
            .unwrap();
        let records = audit.wal.replay_batch(16, 64 * 1024).unwrap();
        let pool = sqlx::PgPool::connect(&database_url).await.unwrap();
        sink::ingest_batch(&pool, &records).await.unwrap();
        sink::ingest_batch(&pool, &records).await.unwrap();
        let request_uuid = Uuid::parse_str(&request_id).unwrap();
        let event_count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM llm_audit_event_t WHERE request_id = $1")
                .bind(request_uuid)
                .fetch_one(&pool)
                .await
                .unwrap();
        let request_count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM llm_request_t WHERE request_id = $1")
                .bind(request_uuid)
                .fetch_one(&pool)
                .await
                .unwrap();
        let attempt_count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM llm_attempt_t WHERE request_id = $1")
                .bind(request_uuid)
                .fetch_one(&pool)
                .await
                .unwrap();
        let request_incomplete: bool =
            sqlx::query_scalar("SELECT incomplete FROM llm_request_t WHERE request_id = $1")
                .bind(request_uuid)
                .fetch_one(&pool)
                .await
                .unwrap();
        let attempt_incomplete: bool =
            sqlx::query_scalar("SELECT incomplete FROM llm_attempt_t WHERE request_id = $1")
                .bind(request_uuid)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(event_count, 4);
        assert_eq!(request_count, 1);
        assert_eq!(attempt_count, 1);
        assert!(!request_incomplete);
        assert!(!attempt_incomplete);
    }

    #[tokio::test]
    async fn wal_process_kill_helper() {
        let Ok(directory) = std::env::var("LLM_AUDIT_KILL_HELPER_DIR") else {
            return;
        };
        let audit = WalAudit::open(config(std::path::Path::new(&directory)), "host-kill").unwrap();
        let reservation = audit
            .reserve(AuditMode::LocalDurable, start())
            .await
            .unwrap();
        reservation
            .attempt_started(AuditAttemptStart {
                attempt: 1,
                deployment_id: "deployment-kill".to_string(),
            })
            .await
            .unwrap();
        if std::env::var("LLM_AUDIT_KILL_HELPER_TERMINAL").is_ok() {
            reservation
                .attempt_finished(AuditAttemptFinish {
                    attempt: 1,
                    terminal: "complete",
                    category: "success",
                })
                .await
                .unwrap();
            reservation
                .finish(AuditFinish {
                    terminal: "complete",
                    attempts: 1,
                    charged_micros: 1,
                    usage_complete: true,
                })
                .await
                .unwrap();
        }
        std::process::exit(0);
    }

    #[test]
    fn process_kill_recovers_committed_start_and_terminal_boundaries() {
        fn run_child(directory: &std::path::Path, terminal: bool) {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .arg("--exact")
                .arg("audit::tests::wal_process_kill_helper")
                .env("LLM_AUDIT_KILL_HELPER_DIR", directory);
            if terminal {
                command.env("LLM_AUDIT_KILL_HELPER_TERMINAL", "1");
            }
            assert!(command.status().unwrap().success());
        }

        let incomplete = tempfile::tempdir().unwrap();
        run_child(incomplete.path(), false);
        let recovered = WalAudit::open(config(incomplete.path()), "host-kill").unwrap();
        assert_eq!(recovered.status().recovered_incomplete_attempts, 1);
        drop(recovered);

        let complete = tempfile::tempdir().unwrap();
        run_child(complete.path(), true);
        let recovered = WalAudit::open(config(complete.path()), "host-kill").unwrap();
        assert_eq!(recovered.status().recovered_incomplete_attempts, 0);
        assert!(recovered.status().durable_sequence >= 4);
    }

    #[tokio::test]
    async fn wal_rejects_a_directory_owned_by_another_gateway_instance() {
        let directory = tempfile::tempdir().unwrap();
        let original = config(directory.path());
        let audit = WalAudit::open(original.clone(), "host-a").unwrap();
        let reservation = audit
            .reserve(AuditMode::LocalDurable, start())
            .await
            .unwrap();
        reservation
            .attempt_started(AuditAttemptStart {
                attempt: 1,
                deployment_id: "deployment-a".to_string(),
            })
            .await
            .unwrap();
        drop(audit);
        std::thread::sleep(Duration::from_millis(10));

        let mut conflicting = original;
        conflicting.gateway_instance = "gateway-other".to_string();
        assert!(AuditWal::open(conflicting).is_err());
    }

    #[tokio::test]
    async fn wal_directory_allows_exactly_one_live_writer() {
        let directory = tempfile::tempdir().unwrap();
        let wal_config = config(directory.path());
        let first = AuditWal::open(wal_config.clone()).unwrap();
        assert!(AuditWal::open(wal_config.clone()).is_err());

        drop(first);
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Ok(reopened) = AuditWal::open(wal_config.clone()) {
                    drop(reopened);
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("writer lock must release after the WAL writer exits");
    }

    #[tokio::test]
    async fn in_flight_reservation_retains_writer_lock_until_terminal_audit_drains() {
        let directory = tempfile::tempdir().unwrap();
        let wal_config = config(directory.path());
        let audit = WalAudit::open(wal_config.clone(), "host-a").unwrap();
        let reservation = audit.reserve(AuditMode::Required, start()).await.unwrap();

        // Disabling the module drops its owner, but an admitted request keeps
        // the WAL alive until its terminal event has been submitted.
        drop(audit);
        let error = AuditWal::open(wal_config.clone())
            .err()
            .expect("re-enable must not steal the in-flight writer lock");
        assert!(error.to_string().contains("already has an active writer"));

        reservation
            .finish(AuditFinish {
                terminal: "complete",
                attempts: 0,
                charged_micros: 0,
                usage_complete: false,
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if let Ok(reopened) = AuditWal::open(wal_config.clone()) {
                    drop(reopened);
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("writer lock must transfer after terminal audit drains");
    }

    #[tokio::test]
    async fn wal_lock_process_helper() {
        let Ok(directory) = std::env::var("LLM_AUDIT_LOCK_HELPER_DIR") else {
            return;
        };
        let directory = std::path::Path::new(&directory);
        let wal = AuditWal::open(config(directory)).unwrap();
        std::fs::write(directory.join("lock-ready"), b"ready").unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !directory.join("lock-release").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("parent must release lock helper");
        drop(wal);
    }

    #[test]
    fn separate_processes_cannot_own_the_same_wal_directory() {
        let directory = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("audit::tests::wal_lock_process_helper")
            .env("LLM_AUDIT_LOCK_HELPER_DIR", directory.path())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !directory.path().join("lock-ready").exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "child did not acquire WAL lock"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(AuditWal::open(config(directory.path())).is_err());
        std::fs::write(directory.path().join("lock-release"), b"release").unwrap();
        assert!(child.wait().unwrap().success());
        assert!(AuditWal::open(config(directory.path())).is_ok());
    }

    #[test]
    fn audit_event_is_metadata_only_and_hashes_the_principal() {
        let mut start = start();
        start.pii_profile = "local-regex-v1:v1:request".to_string();
        let value = serde_json::to_value(event(
            &start,
            "host-a",
            AuditEventKind::RequestAdmitted,
            None,
            "admitted",
            "accepted",
            None,
            None,
            None,
            "2026-07-19",
            Instant::now(),
        ))
        .unwrap();
        let encoded = value.to_string();
        assert!(!encoded.contains("principal-secret"));
        assert!(!encoded.contains("person@example.com"));
        assert_eq!(value["piiProfile"], "local-regex-v1:v1:request");
        for forbidden in ["prompt", "completion", "toolArguments", "credential"] {
            assert!(!value.as_object().unwrap().contains_key(forbidden));
        }
    }
}
