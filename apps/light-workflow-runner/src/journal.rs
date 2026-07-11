use std::path::Path;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use execution_runner_protocol::{ExecuteLease, ExecutionId, LeaseContext, LeaseId};
use rusqlite::{Connection, OptionalExtension, params};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JournalState {
    IntentRecorded,
    Preparing,
    Prepared,
    Executing,
    TerminalPending,
    TerminalReported,
    CleanupRequired,
    CleanupConfirmed,
    Unknown,
}

impl JournalState {
    fn as_str(self) -> &'static str {
        match self {
            Self::IntentRecorded => "INTENT_RECORDED",
            Self::Preparing => "PREPARING",
            Self::Prepared => "PREPARED",
            Self::Executing => "EXECUTING",
            Self::TerminalPending => "TERMINAL_PENDING",
            Self::TerminalReported => "TERMINAL_REPORTED",
            Self::CleanupRequired => "CLEANUP_REQUIRED",
            Self::CleanupConfirmed => "CLEANUP_CONFIRMED",
            Self::Unknown => "UNKNOWN",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "INTENT_RECORDED" => Ok(Self::IntentRecorded),
            "PREPARING" => Ok(Self::Preparing),
            "PREPARED" => Ok(Self::Prepared),
            "EXECUTING" => Ok(Self::Executing),
            "TERMINAL_PENDING" => Ok(Self::TerminalPending),
            "TERMINAL_REPORTED" => Ok(Self::TerminalReported),
            "CLEANUP_REQUIRED" => Ok(Self::CleanupRequired),
            "CLEANUP_CONFIRMED" => Ok(Self::CleanupConfirmed),
            "UNKNOWN" => Ok(Self::Unknown),
            other => Err(format!("unknown journal state {other}")),
        }
    }
}

#[derive(Debug, Clone)]
pub struct JournalRecord {
    pub execution_id: ExecutionId,
    pub lease_id: LeaseId,
    pub fencing_token: u64,
    pub deadline: DateTime<Utc>,
    pub state: JournalState,
    pub backend_operation_id: Option<String>,
    pub policy_digest: String,
    pub compatibility_digest: String,
    pub definition_digest: String,
    pub command_template_digest: String,
    pub lease_context: LeaseContext,
}

#[derive(Clone)]
pub struct Journal {
    connection: Arc<Mutex<Connection>>,
}

impl Journal {
    pub fn open(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("create journal directory: {error}"))?;
        }
        let connection = Connection::open(path)
            .map_err(|error| format!("open journal {}: {error}", path.display()))?;
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA synchronous=FULL;
                 PRAGMA foreign_keys=ON;
                 CREATE TABLE IF NOT EXISTS execution_journal (
                    execution_id TEXT PRIMARY KEY,
                    lease_id TEXT NOT NULL,
                    fencing_token INTEGER NOT NULL,
                    deadline TEXT NOT NULL,
                    state TEXT NOT NULL,
                    backend_operation_id TEXT,
                    policy_digest TEXT NOT NULL,
                    compatibility_digest TEXT NOT NULL,
                    definition_digest TEXT NOT NULL,
                    command_template_digest TEXT NOT NULL,
                    lease_context TEXT NOT NULL DEFAULT '{}',
                    updated_at TEXT NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS execution_journal_recovery_idx
                    ON execution_journal(state, deadline);",
            )
            .map_err(|error| format!("initialize journal: {error}"))?;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    pub fn record_intent(&self, lease: &ExecuteLease) -> Result<bool, String> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "journal mutex poisoned")?;
        let existing: Option<(String, i64)> = connection
            .query_row(
                "SELECT lease_id, fencing_token FROM execution_journal WHERE execution_id = ?1",
                params![lease.lease.execution_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|error| format!("read journal intent: {error}"))?;
        if let Some((lease_id, fencing_token)) = existing {
            if lease_id == lease.lease.lease_id.to_string()
                && fencing_token == lease.lease.fencing_token as i64
            {
                return Ok(false);
            }
            return Err("execution ID was redelivered with a different lease or fence".to_string());
        }
        connection
            .execute(
                "INSERT INTO execution_journal (
                    execution_id, lease_id, fencing_token, deadline, state,
                    policy_digest, compatibility_digest, definition_digest,
                    command_template_digest, lease_context, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    lease.lease.execution_id.to_string(),
                    lease.lease.lease_id.to_string(),
                    lease.lease.fencing_token as i64,
                    lease.lease.deadline.to_rfc3339(),
                    JournalState::IntentRecorded.as_str(),
                    &lease.lease.policy_digest,
                    &lease.lease.compatibility_digest,
                    &lease.definition_digest,
                    &lease.command_template_digest,
                    serde_json::to_string(&lease.lease)
                        .map_err(|error| format!("serialize journal lease context: {error}"))?,
                    Utc::now().to_rfc3339(),
                ],
            )
            .map_err(|error| format!("record journal intent: {error}"))?;
        connection
            .execute_batch("PRAGMA wal_checkpoint(PASSIVE);")
            .map_err(|error| format!("checkpoint journal intent: {error}"))?;
        Ok(true)
    }

    pub fn set_state(
        &self,
        execution_id: ExecutionId,
        state: JournalState,
        backend_operation_id: Option<&str>,
    ) -> Result<(), String> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "journal mutex poisoned")?;
        let updated = connection
            .execute(
                "UPDATE execution_journal
                 SET state = ?1,
                     backend_operation_id = COALESCE(?2, backend_operation_id),
                     updated_at = ?3
                 WHERE execution_id = ?4",
                params![
                    state.as_str(),
                    backend_operation_id,
                    Utc::now().to_rfc3339(),
                    execution_id.to_string(),
                ],
            )
            .map_err(|error| format!("update journal state: {error}"))?;
        if updated != 1 {
            return Err(format!("journal execution {execution_id} was not found"));
        }
        connection
            .execute_batch("PRAGMA wal_checkpoint(PASSIVE);")
            .map_err(|error| format!("checkpoint journal state: {error}"))?;
        Ok(())
    }

    pub fn find(&self, execution_id: ExecutionId) -> Result<Option<JournalRecord>, String> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "journal mutex poisoned")?;
        connection
            .query_row(
                "SELECT execution_id, lease_id, fencing_token, deadline, state,
                        backend_operation_id, policy_digest, compatibility_digest,
                        definition_digest, command_template_digest, lease_context
                 FROM execution_journal WHERE execution_id = ?1",
                params![execution_id.to_string()],
                row_to_record,
            )
            .optional()
            .map_err(|error| format!("read journal record: {error}"))
    }

    pub fn unfinished(&self) -> Result<Vec<JournalRecord>, String> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "journal mutex poisoned")?;
        let mut statement = connection
            .prepare(
                "SELECT execution_id, lease_id, fencing_token, deadline, state,
                        backend_operation_id, policy_digest, compatibility_digest,
                        definition_digest, command_template_digest, lease_context
                 FROM execution_journal
                 WHERE state <> 'CLEANUP_CONFIRMED'
                 ORDER BY deadline, execution_id",
            )
            .map_err(|error| format!("prepare journal recovery query: {error}"))?;
        let records = statement
            .query_map([], row_to_record)
            .map_err(|error| format!("query journal recovery records: {error}"))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("decode journal recovery record: {error}"))?;
        Ok(records)
    }

    pub fn cleanup_backlog(&self) -> Result<u32, String> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "journal mutex poisoned")?;
        let count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM execution_journal
                 WHERE state IN ('CLEANUP_REQUIRED', 'TERMINAL_PENDING', 'TERMINAL_REPORTED', 'UNKNOWN')",
                [],
                |row| row.get(0),
            )
            .map_err(|error| format!("count cleanup backlog: {error}"))?;
        u32::try_from(count).map_err(|_| "cleanup backlog exceeds u32".to_string())
    }

    pub fn is_healthy(&self) -> bool {
        self.connection
            .lock()
            .ok()
            .and_then(|connection| {
                connection
                    .query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0))
                    .ok()
            })
            .is_some_and(|result| result == "ok")
    }
}

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<JournalRecord> {
    let execution_id = row.get::<_, String>(0)?.parse().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let lease_id = row.get::<_, String>(1)?.parse().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(error))
    })?;
    let fencing_token = u64::try_from(row.get::<_, i64>(2)?).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            2,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })?;
    let deadline = DateTime::parse_from_rfc3339(&row.get::<_, String>(3)?)
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?
        .with_timezone(&Utc);
    let state = JournalState::parse(&row.get::<_, String>(4)?).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, error.into())
    })?;
    Ok(JournalRecord {
        execution_id,
        lease_id,
        fencing_token,
        deadline,
        state,
        backend_operation_id: row.get(5)?,
        policy_digest: row.get(6)?,
        compatibility_digest: row.get(7)?,
        definition_digest: row.get(8)?,
        command_template_digest: row.get(9)?,
        lease_context: serde_json::from_str(&row.get::<_, String>(10)?).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                10,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use execution_runner_protocol::{
        AuthenticatedOrigin, ExecutionSubject, LeaseContext, OriginKind, SchedulingRequestId,
    };
    use uuid::Uuid;

    fn lease() -> ExecuteLease {
        ExecuteLease {
            lease: LeaseContext {
                scheduling_request_id: SchedulingRequestId::new(),
                execution_id: ExecutionId::new(),
                origin: AuthenticatedOrigin {
                    kind: OriginKind::Workflow,
                    service_id: "workflow".into(),
                    instance_id: "one".into(),
                    host_id: Uuid::nil(),
                },
                subject: ExecutionSubject::WorkflowTask {
                    subject_id: Uuid::new_v4(),
                    process_id: Uuid::new_v4(),
                    task_id: Uuid::new_v4(),
                },
                attempt: 1,
                lease_id: LeaseId::new(),
                fencing_token: 1,
                policy_digest: "policy".into(),
                compatibility_digest: "compat".into(),
                deadline: Utc::now() + chrono::Duration::minutes(1),
            },
            backend_id: "mock".into(),
            execution_profile: serde_json::json!({}),
            command: serde_json::json!({}),
            inputs: Vec::new(),
            definition_digest: "definition".into(),
            command_template_digest: "template".into(),
        }
    }

    #[test]
    fn duplicate_intent_is_idempotent_but_refencing_is_rejected() {
        let directory = std::env::temp_dir().join(format!("runner-journal-{}", Uuid::new_v4()));
        let journal = Journal::open(&directory.join("journal.sqlite")).unwrap();
        let lease = lease();
        assert!(journal.record_intent(&lease).unwrap());
        assert!(!journal.record_intent(&lease).unwrap());
        let mut refenced = lease.clone();
        refenced.lease.fencing_token += 1;
        assert!(journal.record_intent(&refenced).is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
