use std::path::Path;
use std::sync::{Arc, Mutex};

use agent_runtime_protocol::BrokerResponse;
use agent_runtime_protocol::RuntimeEvent;
use chrono::{DateTime, Utc};
use execution_runner_protocol::{
    ExecuteLease, ExecutionId, LeaseContext, LeaseId, TerminalLeaseResult,
};
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
    pub terminal_result: Option<TerminalLeaseResult>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BrokerRequestDisposition {
    New,
    Replay(BrokerResponse),
    Unknown,
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
                    terminal_result TEXT,
                    updated_at TEXT NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS execution_journal_recovery_idx
                    ON execution_journal(state, deadline);
                 CREATE TABLE IF NOT EXISTS agent_runtime_event_journal (
                    execution_id TEXT NOT NULL,
                    event_id TEXT NOT NULL,
                    sequence INTEGER NOT NULL,
                    fencing_token INTEGER NOT NULL,
                    event_json TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    PRIMARY KEY(execution_id, event_id),
                    UNIQUE(execution_id, sequence)
                 );
                 CREATE TABLE IF NOT EXISTS broker_request_journal (
                    execution_id TEXT NOT NULL,
                    request_id TEXT NOT NULL,
                    fencing_token INTEGER NOT NULL,
                    request_digest TEXT NOT NULL,
                    operation TEXT NOT NULL,
                    target TEXT NOT NULL,
                    state TEXT NOT NULL CHECK(state IN('IN_FLIGHT','COMPLETED','UNKNOWN')),
                    charged_tokens INTEGER NOT NULL,
                    charged_cost_micros INTEGER NOT NULL,
                    response_json TEXT,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    PRIMARY KEY(execution_id,request_id),
                    FOREIGN KEY(execution_id) REFERENCES execution_journal(execution_id) ON DELETE CASCADE
                 );",
            )
            .map_err(|error| format!("initialize journal: {error}"))?;
        // Upgrade journals created by the initial runner bootstrap. SQLite has
        // no `ADD COLUMN IF NOT EXISTS`, so inspect the schema explicitly and
        // propagate real migration failures.
        let has_terminal_result: bool = connection
            .query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM pragma_table_info('execution_journal')
                    WHERE name = 'terminal_result'
                 )",
                [],
                |row| row.get(0),
            )
            .map_err(|error| format!("inspect journal schema: {error}"))?;
        if !has_terminal_result {
            connection
                .execute(
                    "ALTER TABLE execution_journal ADD COLUMN terminal_result TEXT",
                    [],
                )
                .map_err(|error| format!("upgrade journal terminal result storage: {error}"))?;
        }
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    #[cfg(test)]
    pub(crate) fn record_broker_test_execution(
        &self,
        execution_id: ExecutionId,
        lease_id: LeaseId,
        fencing_token: u64,
    ) -> Result<(), String> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "journal mutex poisoned")?;
        connection.execute("INSERT INTO execution_journal(execution_id,lease_id,fencing_token,deadline,state,policy_digest,compatibility_digest,definition_digest,command_template_digest,lease_context,updated_at) VALUES(?1,?2,?3,?4,'EXECUTING','p','c','d','t','{}',?5)",params![execution_id.to_string(),lease_id.to_string(),fencing_token as i64,(Utc::now()+chrono::Duration::minutes(5)).to_rfc3339(),Utc::now().to_rfc3339()]).map_err(|e|format!("record broker test execution: {e}"))?;
        Ok(())
    }

    pub fn begin_broker_request(
        &self,
        execution_id: ExecutionId,
        fencing_token: u64,
        request_id: uuid::Uuid,
        request_digest: &str,
        operation: &str,
        target: &str,
        charged_tokens: u64,
        charged_cost_micros: u64,
        maximum_requests: u32,
        maximum_tokens: u64,
        maximum_cost_micros: u64,
    ) -> Result<BrokerRequestDisposition, String> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| "journal mutex poisoned")?;
        let transaction = connection
            .transaction()
            .map_err(|e| format!("begin broker journal transaction: {e}"))?;
        let fence: Option<i64> = transaction
            .query_row(
                "SELECT fencing_token FROM execution_journal WHERE execution_id=?1",
                params![execution_id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| format!("read broker execution fence: {e}"))?;
        if fence != Some(fencing_token as i64) {
            return Err("broker request did not match the durable execution fence".into());
        }
        let existing:Option<(String,String,Option<String>)>=transaction.query_row("SELECT request_digest,state,response_json FROM broker_request_journal WHERE execution_id=?1 AND request_id=?2",params![execution_id.to_string(),request_id.to_string()],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional().map_err(|e|format!("read broker request journal: {e}"))?;
        let disposition = if let Some((digest, state, response)) = existing {
            if digest != request_digest {
                return Err("broker request ID was reused with different content".into());
            }
            match state.as_str() {
                "COMPLETED" => BrokerRequestDisposition::Replay(
                    serde_json::from_str(
                        response
                            .as_deref()
                            .ok_or("completed broker request lacks response")?,
                    )
                    .map_err(|e| format!("decode durable broker response: {e}"))?,
                ),
                "IN_FLIGHT" => {
                    transaction.execute("UPDATE broker_request_journal SET state='UNKNOWN',updated_at=?3 WHERE execution_id=?1 AND request_id=?2 AND state='IN_FLIGHT'",params![execution_id.to_string(),request_id.to_string(),Utc::now().to_rfc3339()]).map_err(|e|format!("mark interrupted broker request unknown: {e}"))?;
                    BrokerRequestDisposition::Unknown
                }
                "UNKNOWN" => BrokerRequestDisposition::Unknown,
                other => return Err(format!("unknown broker journal state {other}")),
            }
        } else {
            let (requests,tokens,cost):(i64,i64,i64)=transaction.query_row("SELECT COUNT(*),COALESCE(SUM(charged_tokens),0),COALESCE(SUM(charged_cost_micros),0) FROM broker_request_journal WHERE execution_id=?1",params![execution_id.to_string()],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).map_err(|e|format!("read broker budget usage: {e}"))?;
            if u64::try_from(requests).unwrap_or(u64::MAX) >= u64::from(maximum_requests)
                || u64::try_from(tokens)
                    .unwrap_or(u64::MAX)
                    .saturating_add(charged_tokens)
                    > maximum_tokens
                || u64::try_from(cost)
                    .unwrap_or(u64::MAX)
                    .saturating_add(charged_cost_micros)
                    > maximum_cost_micros
            {
                return Err("broker budget exceeded".into());
            }
            transaction.execute("INSERT INTO broker_request_journal(execution_id,request_id,fencing_token,request_digest,operation,target,state,charged_tokens,charged_cost_micros,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,'IN_FLIGHT',?7,?8,?9,?9)",params![execution_id.to_string(),request_id.to_string(),fencing_token as i64,request_digest,operation,target,i64::try_from(charged_tokens).map_err(|_|"broker token charge exceeds SQLite integer")?,i64::try_from(charged_cost_micros).map_err(|_|"broker cost charge exceeds SQLite integer")?,Utc::now().to_rfc3339()]).map_err(|e|format!("record broker request intent: {e}"))?;
            BrokerRequestDisposition::New
        };
        transaction
            .commit()
            .map_err(|e| format!("commit broker request intent: {e}"))?;
        connection
            .execute_batch("PRAGMA wal_checkpoint(PASSIVE);")
            .map_err(|e| format!("checkpoint broker request intent: {e}"))?;
        Ok(disposition)
    }

    pub fn complete_broker_request(
        &self,
        execution_id: ExecutionId,
        request_id: uuid::Uuid,
        response: &BrokerResponse,
    ) -> Result<(), String> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "journal mutex poisoned")?;
        let changed=connection.execute("UPDATE broker_request_journal SET state='COMPLETED',response_json=?3,updated_at=?4 WHERE execution_id=?1 AND request_id=?2 AND state='IN_FLIGHT'",params![execution_id.to_string(),request_id.to_string(),serde_json::to_string(response).map_err(|e|format!("serialize broker response: {e}"))?,Utc::now().to_rfc3339()]).map_err(|e|format!("complete broker request: {e}"))?;
        if changed != 1 {
            return Err("broker request was no longer durably in flight".into());
        }
        connection
            .execute_batch("PRAGMA wal_checkpoint(PASSIVE);")
            .map_err(|e| format!("checkpoint broker response: {e}"))?;
        Ok(())
    }

    pub fn mark_broker_request_unknown(
        &self,
        execution_id: ExecutionId,
        request_id: uuid::Uuid,
    ) -> Result<(), String> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "journal mutex poisoned")?;
        connection.execute("UPDATE broker_request_journal SET state='UNKNOWN',updated_at=?3 WHERE execution_id=?1 AND request_id=?2 AND state='IN_FLIGHT'",params![execution_id.to_string(),request_id.to_string(),Utc::now().to_rfc3339()]).map_err(|e|format!("mark broker request unknown: {e}"))?;
        connection
            .execute_batch("PRAGMA wal_checkpoint(PASSIVE);")
            .map_err(|e| format!("checkpoint unknown broker request: {e}"))?;
        Ok(())
    }

    pub fn broker_usage(&self, execution_id: ExecutionId) -> Result<(u32, u64, u64), String> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "journal mutex poisoned")?;
        let (requests,tokens,cost):(i64,i64,i64)=connection.query_row("SELECT COUNT(*),COALESCE(SUM(charged_tokens),0),COALESCE(SUM(charged_cost_micros),0) FROM broker_request_journal WHERE execution_id=?1",params![execution_id.to_string()],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).map_err(|e|format!("read durable broker usage: {e}"))?;
        Ok((
            u32::try_from(requests).map_err(|_| "broker request count overflow")?,
            u64::try_from(tokens).map_err(|_| "broker token usage is negative")?,
            u64::try_from(cost).map_err(|_| "broker cost usage is negative")?,
        ))
    }

    pub fn record_runtime_event(&self, event: &RuntimeEvent) -> Result<bool, String> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "journal mutex poisoned")?;
        let lease: Option<i64> = connection
            .query_row(
                "SELECT fencing_token FROM execution_journal WHERE execution_id=?1",
                params![event.execution_id.to_string()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| format!("read runtime event fence: {error}"))?;
        if lease != Some(event.fencing_token as i64) {
            return Err("runtime event did not match the journal fence".into());
        }
        let changed = connection.execute(
            "INSERT INTO agent_runtime_event_journal(execution_id,event_id,sequence,fencing_token,event_json,created_at) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(execution_id,event_id) DO NOTHING",
            params![event.execution_id.to_string(),event.event_id.to_string(),event.sequence as i64,event.fencing_token as i64,serde_json::to_string(event).map_err(|error| format!("serialize runtime event: {error}"))?,Utc::now().to_rfc3339()],
        ).map_err(|error| format!("record runtime event: {error}"))?;
        Ok(changed == 1)
    }

    pub fn runtime_events_after(
        &self,
        execution_id: ExecutionId,
        sequence: u64,
    ) -> Result<Vec<RuntimeEvent>, String> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "journal mutex poisoned")?;
        let mut statement = connection.prepare("SELECT event_json FROM agent_runtime_event_journal WHERE execution_id=?1 AND sequence>?2 ORDER BY sequence")
            .map_err(|error| format!("prepare runtime event replay: {error}"))?;
        statement
            .query_map(params![execution_id.to_string(), sequence as i64], |row| {
                row.get::<_, String>(0)
            })
            .map_err(|error| format!("query runtime event replay: {error}"))?
            .map(|row| {
                row.map_err(|error| format!("read runtime event replay: {error}"))
                    .and_then(|json| {
                        serde_json::from_str(&json)
                            .map_err(|error| format!("decode runtime event replay: {error}"))
                    })
            })
            .collect()
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
                     terminal_result = CASE WHEN ?1 = 'TERMINAL_REPORTED' THEN NULL ELSE terminal_result END,
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

    /// Persists the complete terminal payload before it may be sent. This is
    /// what makes an acknowledgement lost across a process restart replayable
    /// without changing the outcome to UNKNOWN.
    pub fn set_terminal(&self, terminal: &TerminalLeaseResult) -> Result<(), String> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| "journal mutex poisoned")?;
        let updated = connection
            .execute(
                "UPDATE execution_journal
                 SET state = ?1, terminal_result = ?2, updated_at = ?3
                 WHERE execution_id = ?4 AND lease_id = ?5 AND fencing_token = ?6",
                params![
                    JournalState::TerminalPending.as_str(),
                    serde_json::to_string(terminal)
                        .map_err(|error| format!("serialize terminal result: {error}"))?,
                    Utc::now().to_rfc3339(),
                    terminal.lease.execution_id.to_string(),
                    terminal.lease.lease_id.to_string(),
                    terminal.lease.fencing_token as i64,
                ],
            )
            .map_err(|error| format!("persist terminal result: {error}"))?;
        if updated != 1 {
            return Err("terminal result did not match the journal lease fence".to_string());
        }
        connection
            .execute_batch("PRAGMA wal_checkpoint(PASSIVE);")
            .map_err(|error| format!("checkpoint terminal result: {error}"))?;
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
                        definition_digest, command_template_digest, lease_context,
                        terminal_result
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
                        definition_digest, command_template_digest, lease_context,
                        terminal_result
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
        terminal_result: row
            .get::<_, Option<String>>(11)?
            .map(|value| serde_json::from_str(&value))
            .transpose()
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    11,
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

    #[test]
    fn opening_legacy_journal_adds_terminal_result_storage() {
        let directory =
            std::env::temp_dir().join(format!("runner-legacy-journal-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("journal.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE execution_journal (
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
                 );",
            )
            .unwrap();
        drop(connection);

        let journal = Journal::open(&path).unwrap();
        assert!(journal.record_intent(&lease()).unwrap());
        let columns = journal
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('execution_journal')
                 WHERE name = 'terminal_result'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        assert_eq!(columns, 1);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn broker_requests_replay_completed_and_fence_interrupted_effects_as_unknown() {
        let directory =
            std::env::temp_dir().join(format!("runner-broker-journal-{}", Uuid::new_v4()));
        let path = directory.join("journal.sqlite");
        let journal = Journal::open(&path).unwrap();
        let lease = lease();
        journal.record_intent(&lease).unwrap();
        let request_id = Uuid::new_v4();
        assert_eq!(
            journal
                .begin_broker_request(
                    lease.lease.execution_id,
                    lease.lease.fencing_token,
                    request_id,
                    "sha256:request",
                    "CredentialedRequest",
                    "calendar",
                    0,
                    0,
                    2,
                    0,
                    0
                )
                .unwrap(),
            BrokerRequestDisposition::New
        );
        let response = BrokerResponse {
            request_id,
            status: 200,
            body_base64: "e30=".into(),
            consumed_requests: 1,
            consumed_tokens: 0,
            consumed_cost_micros: 0,
        };
        journal
            .complete_broker_request(lease.lease.execution_id, request_id, &response)
            .unwrap();
        drop(journal);
        let reopened = Journal::open(&path).unwrap();
        assert_eq!(
            reopened
                .begin_broker_request(
                    lease.lease.execution_id,
                    lease.lease.fencing_token,
                    request_id,
                    "sha256:request",
                    "CredentialedRequest",
                    "calendar",
                    0,
                    0,
                    2,
                    0,
                    0
                )
                .unwrap(),
            BrokerRequestDisposition::Replay(response)
        );
        let interrupted = Uuid::new_v4();
        assert_eq!(
            reopened
                .begin_broker_request(
                    lease.lease.execution_id,
                    lease.lease.fencing_token,
                    interrupted,
                    "sha256:other",
                    "CredentialedRequest",
                    "mail",
                    0,
                    0,
                    2,
                    0,
                    0
                )
                .unwrap(),
            BrokerRequestDisposition::New
        );
        drop(reopened);
        let restarted = Journal::open(&path).unwrap();
        assert_eq!(
            restarted
                .begin_broker_request(
                    lease.lease.execution_id,
                    lease.lease.fencing_token,
                    interrupted,
                    "sha256:other",
                    "CredentialedRequest",
                    "mail",
                    0,
                    0,
                    2,
                    0,
                    0
                )
                .unwrap(),
            BrokerRequestDisposition::Unknown
        );
        assert_eq!(
            restarted.broker_usage(lease.lease.execution_id).unwrap().0,
            2
        );
        std::fs::remove_dir_all(directory).unwrap();
    }
}
