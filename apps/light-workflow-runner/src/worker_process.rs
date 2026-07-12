use crate::broker::{AttemptBroker, AttemptBrokerConfig};
use crate::journal::Journal;
use agent_core::ResultClass;
use agent_runtime_protocol::{
    AgentWorkerExecutionSpec, RuntimeCommand, RuntimeEvent, RuntimeEventPayload, RuntimeIdentity,
};
use execution_runner_protocol::ExecuteLease;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{path::Path, process::Stdio};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::watch,
};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkerProcessConfig {
    pub origin_service_id: String,
    pub executable: std::path::PathBuf,
    pub binary_digest: String,
    pub capability_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub broker: Option<AttemptBrokerConfig>,
}

impl WorkerProcessConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.origin_service_id.is_empty()
            || !self
                .origin_service_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return Err("agent worker originServiceId is invalid".into());
        }
        if !self.executable.is_absolute() {
            return Err("agent worker executable must be an absolute path".into());
        }
        for (name, digest) in [
            ("agent worker binary", self.binary_digest.as_str()),
            ("agent worker capability", self.capability_digest.as_str()),
        ] {
            let value = digest.strip_prefix("sha256:").unwrap_or_default();
            if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(format!("{name} digest must be sha256 plus 64 hex digits"));
            }
        }
        if let Some(broker) = &self.broker {
            broker.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct WorkerOutcome {
    pub class: ResultClass,
    pub output: Option<serde_json::Value>,
    pub error: Option<String>,
    pub events: u64,
}

pub async fn run_worker_process(
    lease: &ExecuteLease,
    spec: &AgentWorkerExecutionSpec,
    config: &WorkerProcessConfig,
    journal: &Journal,
    mut cancel: watch::Receiver<bool>,
) -> Result<WorkerOutcome, String> {
    config.validate()?;
    validate_spec(lease, spec)?;
    if spec.expected_capability_digest != config.capability_digest {
        return Err("worker capability digest is not admitted by this runner".into());
    }
    verify_binary(&config.executable, &config.binary_digest).await?;
    let identity = RuntimeIdentity {
        execution_id: lease.lease.execution_id,
        lease_id: lease.lease.lease_id,
        fencing_token: lease.lease.fencing_token,
        transport_nonce: Uuid::new_v4().simple().to_string(),
    };
    let broker = match (&spec.broker, &config.broker) {
        (Some(grant), Some(config)) => {
            Some(AttemptBroker::bind(config, grant.clone(), identity.clone()).await?)
        }
        (None, None) => None,
        _ => {
            return Err(
                "agent broker grant and runner broker configuration must both be present".into(),
            );
        }
    };
    let mut command = Command::new(&config.executable);
    command
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(broker) = &broker {
        command.env("LIGHT_AGENT_BROKER_SOCKET", broker.socket_path());
    }
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .map_err(|e| format!("spawn agent worker: {e}"))?;
    let (broker_shutdown, broker_task) = if let Some(broker) = broker {
        let expected_pid = child.id().ok_or("spawned worker has no process id")?;
        let (shutdown, receiver) = watch::channel(false);
        let task = tokio::spawn(broker.serve(expected_pid, receiver));
        (Some(shutdown), Some(task))
    } else {
        (None, None)
    };
    let admitted_duration = std::time::Duration::from_millis(spec.wall_clock_timeout_ms).min(
        lease
            .lease
            .deadline
            .signed_duration_since(chrono::Utc::now())
            .to_std()
            .unwrap_or_default(),
    );
    let execution_deadline = tokio::time::Instant::now() + admitted_duration;
    if *cancel.borrow() {
        kill_tree(&mut child).await;
        return Err("agent worker cancelled".into());
    }
    let mut stdin = child.stdin.take().ok_or("worker stdin unavailable")?;
    let stdout = child.stdout.take().ok_or("worker stdout unavailable")?;
    let mut stdout = BufReader::new(stdout);
    let mut stderr = child.stderr.take().ok_or("worker stderr unavailable")?;
    let stderr_limit = spec.maximum_stderr_bytes;
    let mut stderr_task = tokio::spawn(async move {
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = stderr.read(&mut buf).await.map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            if out.len() + n > stderr_limit {
                return Err("worker stderr limit exceeded".into());
            }
            out.extend_from_slice(&buf[..n]);
        }
        Ok::<_, String>(out)
    });
    write_command(
        &mut stdin,
        &RuntimeCommand::Hello {
            identity: identity.clone(),
            expected_capability_digest: spec.expected_capability_digest.clone(),
        },
    )
    .await?;
    let ready = tokio::select! {
        event = read_event(&mut stdout, spec.maximum_event_bytes) => event?,
        changed = cancel.changed() => {
            kill_tree(&mut child).await;
            if changed.is_ok() && *cancel.borrow() {
                return Err("agent worker cancelled".into());
            }
            return Err("agent worker cancellation channel closed".into());
        }
        stderr = &mut stderr_task => {
            kill_tree(&mut child).await;
            return Err(stderr.map_err(|error| error.to_string())?.err()
                .unwrap_or_else(|| "agent worker exited before ready".into()));
        }
        _ = tokio::time::sleep_until(execution_deadline) => {
            kill_tree(&mut child).await;
            return Err("agent worker deadline expired before ready".into());
        }
    };
    ready.validate(&identity, 0).map_err(|e| e.to_string())?;
    match &ready.payload {
        RuntimeEventPayload::Ready { capabilities }
            if agent_runtime_protocol::canonical_digest(capabilities)
                .map_err(|e| e.to_string())?
                == spec.expected_capability_digest => {}
        _ => return Err("worker did not return admitted capabilities".into()),
    }
    journal.record_runtime_event(&ready)?;
    write_command(
        &mut stdin,
        &RuntimeCommand::Start {
            session_id: spec.session_id,
            turn_id: spec.turn_id,
            action_attempt_id: spec.action_attempt_id,
            policy_digest: spec.policy_digest.clone(),
            input: spec.input.clone(),
        },
    )
    .await?;
    let deadline = tokio::time::sleep_until(execution_deadline);
    tokio::pin!(deadline);
    let mut sequence = ready.sequence;
    let outcome = loop {
        tokio::select! {event=read_event(&mut stdout,spec.maximum_event_bytes)=>{let event=event?;event.validate(&identity,sequence).map_err(|e|e.to_string())?;sequence=event.sequence;journal.record_runtime_event(&event)?;if let RuntimeEventPayload::Terminal{class,output,error}=event.payload{break WorkerOutcome{class,output,error,events:sequence}}},changed=cancel.changed()=>{if changed.is_ok()&&*cancel.borrow(){kill_tree(&mut child).await;return Err("agent worker cancelled".into())}},stderr=&mut stderr_task=>{kill_tree(&mut child).await;return Err(stderr.map_err(|error|error.to_string())?.err().unwrap_or_else(||"agent worker exited before terminal event".into()))},_=&mut deadline=>{kill_tree(&mut child).await;return Err("agent worker deadline expired".into())}}
    };
    drop(stdin);
    let status = child.wait().await.map_err(|e| e.to_string())?;
    let stderr = stderr_task.await.map_err(|e| e.to_string())??;
    if !status.success() {
        return Err(format!(
            "agent worker exited {status}: {}",
            String::from_utf8_lossy(&stderr)
        ));
    }
    if let Some(shutdown) = broker_shutdown {
        let _ = shutdown.send(true);
    }
    if let Some(task) = broker_task {
        task.await.map_err(|e| e.to_string())??;
    }
    Ok(outcome)
}
fn validate_spec(lease: &ExecuteLease, s: &AgentWorkerExecutionSpec) -> Result<(), String> {
    if s.schema_version != 1
        || s.policy_digest != lease.lease.policy_digest
        || s.template_digest != lease.command_template_digest
        || s.wall_clock_timeout_ms == 0
        || s.maximum_event_bytes == 0
        || s.maximum_event_bytes > agent_runtime_protocol::MAX_FRAME_BYTES
        || s.maximum_stderr_bytes == 0
        || s.maximum_stderr_bytes > agent_runtime_protocol::MAX_FRAME_BYTES
        || s.broker.as_ref().is_some_and(|grant| {
            grant.policy_digest != s.policy_digest || grant.expires_at > lease.lease.deadline
        })
    {
        return Err("invalid agent worker execution specification".into());
    }
    Ok(())
}
async fn verify_binary(path: &Path, expected: &str) -> Result<(), String> {
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| format!("read worker binary: {e}"))?;
    let actual = format!("sha256:{}", hex::encode(Sha256::digest(bytes)));
    if actual != expected {
        return Err("agent worker binary digest mismatch".into());
    }
    Ok(())
}
async fn write_command<W: AsyncWriteExt + Unpin>(
    w: &mut W,
    c: &RuntimeCommand,
) -> Result<(), String> {
    let mut b = serde_json::to_vec(c).map_err(|e| e.to_string())?;
    b.push(b'\n');
    w.write_all(&b).await.map_err(|e| e.to_string())?;
    w.flush().await.map_err(|e| e.to_string())
}
async fn read_event<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    maximum_bytes: usize,
) -> Result<RuntimeEvent, String> {
    // `read_until` avoids one async read per byte. Wrapping the borrowed reader
    // in `take` is essential: plain `read_until` can allocate without bound if
    // a hostile worker never emits a newline.
    let limit = u64::try_from(maximum_bytes)
        .map_err(|_| "worker event limit does not fit u64".to_string())?
        .saturating_add(1);
    let mut bounded = reader.take(limit);
    let mut frame = Vec::with_capacity(maximum_bytes.min(8 * 1024).saturating_add(1));
    let bytes = bounded
        .read_until(b'\n', &mut frame)
        .await
        .map_err(|error| format!("worker event stream ended: {error}"))?;
    if bytes == 0 {
        return Err("worker event stream ended before a complete frame".into());
    }
    if frame.last() != Some(&b'\n') {
        return Err("worker event exceeds admitted limit".into());
    }
    frame.pop();
    if frame.len() > maximum_bytes {
        return Err("worker event exceeds admitted limit".into());
    }
    serde_json::from_slice(&frame).map_err(|error| format!("invalid worker event: {error}"))
}
async fn kill_tree(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(id) = child.id() {
        unsafe {
            libc::kill(-(id as i32), libc::SIGKILL);
        }
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use agent_core::{AgentActionAttemptId, AgentSessionId, AgentTurnId};
    use agent_runtime_protocol::{RuntimeCapabilities, canonical_digest};
    use chrono::Utc;
    use execution_runner_protocol::{
        AuthenticatedOrigin, ExecutionId, ExecutionSubject, LeaseContext, LeaseId, OriginKind,
        SchedulingRequestId,
    };
    use std::collections::BTreeSet;
    use std::os::unix::fs::PermissionsExt;

    fn lease() -> ExecuteLease {
        ExecuteLease {
            lease: LeaseContext {
                scheduling_request_id: SchedulingRequestId::new(),
                execution_id: ExecutionId::new(),
                origin: AuthenticatedOrigin {
                    kind: OriginKind::Agent,
                    service_id: "light-agent".into(),
                    instance_id: "test".into(),
                    host_id: Uuid::nil(),
                },
                subject: ExecutionSubject::AgentTurn {
                    subject_id: Uuid::new_v4(),
                    session_id: Uuid::new_v4(),
                    turn_id: Uuid::new_v4(),
                },
                attempt: 1,
                lease_id: LeaseId::new(),
                fencing_token: 7,
                policy_digest: "sha256:policy".into(),
                compatibility_digest: "sha256:compatibility".into(),
                deadline: Utc::now() + chrono::Duration::minutes(1),
            },
            backend_id: "mock".into(),
            execution_profile: serde_json::json!({}),
            command: serde_json::json!({}),
            inputs: Vec::new(),
            definition_digest: "sha256:definition".into(),
            command_template_digest: "sha256:template".into(),
        }
    }

    fn capabilities() -> RuntimeCapabilities {
        RuntimeCapabilities {
            adapter_id: "fixture".into(),
            adapter_version: "1".into(),
            protocol_version: agent_runtime_protocol::PROTOCOL_VERSION.into(),
            actions: BTreeSet::from(["mock".into()]),
            supports_checkpoint: false,
            maximum_event_bytes: 4096,
        }
    }

    fn fixture(
        directory: &std::path::Path,
        capabilities: &RuntimeCapabilities,
        wait: bool,
    ) -> WorkerProcessConfig {
        let executable = directory.join("worker.py");
        let capability_digest = canonical_digest(capabilities).unwrap();
        let capabilities = serde_json::to_string(capabilities).unwrap();
        let wait = if wait { "time.sleep(30)" } else { "terminal()" };
        let source = format!(
            r##"#!/usr/bin/python3
import datetime,json,sys,time,uuid
hello=json.loads(sys.stdin.readline())
i=hello["identity"]
caps=json.loads(r'''{capabilities}''')
def event(sequence,payload):
 print(json.dumps({{"protocolVersion":"1.0","eventId":str(uuid.uuid4()),"executionId":i["executionId"],"leaseId":i["leaseId"],"fencingToken":i["fencingToken"],"sequence":sequence,"occurredAt":datetime.datetime.now(datetime.timezone.utc).isoformat(),"payload":payload}},separators=(",",":")),flush=True)
def terminal():
 event(2,{{"type":"terminal","class":"success","output":{{"ok":True}},"error":None}})
event(1,{{"type":"ready","capabilities":caps}})
sys.stdin.readline()
{wait}
"##
        );
        std::fs::write(&executable, source).unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions).unwrap();
        let digest = format!(
            "sha256:{}",
            hex::encode(Sha256::digest(std::fs::read(&executable).unwrap()))
        );
        WorkerProcessConfig {
            origin_service_id: "light-agent".into(),
            executable,
            binary_digest: digest,
            capability_digest,
            broker: None,
        }
    }

    fn spec(capability_digest: String) -> AgentWorkerExecutionSpec {
        AgentWorkerExecutionSpec {
            schema_version: 1,
            template_digest: "sha256:template".into(),
            expected_capability_digest: capability_digest,
            session_id: AgentSessionId::new(),
            turn_id: AgentTurnId::new(),
            action_attempt_id: AgentActionAttemptId::new(),
            policy_digest: "sha256:policy".into(),
            input: serde_json::json!({"scenario":"success"}),
            wall_clock_timeout_ms: 5_000,
            maximum_event_bytes: 16 * 1024,
            maximum_stderr_bytes: 16 * 1024,
            broker: None,
        }
    }

    #[tokio::test]
    async fn authenticates_journals_and_returns_terminal_event() {
        let directory = tempfile::tempdir().unwrap();
        let lease = lease();
        let journal = Journal::open(&directory.path().join("journal.sqlite")).unwrap();
        journal.record_intent(&lease).unwrap();
        let config = fixture(directory.path(), &capabilities(), false);
        let spec = spec(config.capability_digest.clone());
        let (_cancel, cancellation) = watch::channel(false);

        let outcome = run_worker_process(&lease, &spec, &config, &journal, cancellation)
            .await
            .unwrap();

        assert_eq!(outcome.class, ResultClass::Success);
        assert_eq!(outcome.events, 2);
        assert_eq!(
            journal
                .runtime_events_after(lease.lease.execution_id, 0)
                .unwrap()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn cancellation_kills_a_worker_that_has_not_completed() {
        let directory = tempfile::tempdir().unwrap();
        let lease = lease();
        let journal = Journal::open(&directory.path().join("journal.sqlite")).unwrap();
        journal.record_intent(&lease).unwrap();
        let config = fixture(directory.path(), &capabilities(), true);
        let spec = spec(config.capability_digest.clone());
        let (cancel, cancellation) = watch::channel(false);
        let task = tokio::spawn(async move {
            run_worker_process(&lease, &spec, &config, &journal, cancellation).await
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        cancel.send(true).unwrap();

        let error = tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(error, "agent worker cancelled");
    }

    #[tokio::test]
    async fn rejects_worker_binary_drift_before_spawn() {
        let directory = tempfile::tempdir().unwrap();
        let lease = lease();
        let journal = Journal::open(&directory.path().join("journal.sqlite")).unwrap();
        journal.record_intent(&lease).unwrap();
        let mut config = fixture(directory.path(), &capabilities(), false);
        config.binary_digest = format!("sha256:{}", "0".repeat(64));
        let spec = spec(config.capability_digest.clone());
        let (_cancel, cancellation) = watch::channel(false);

        let error = run_worker_process(&lease, &spec, &config, &journal, cancellation)
            .await
            .unwrap_err();
        assert_eq!(error, "agent worker binary digest mismatch");
    }

    #[tokio::test]
    async fn buffered_reader_bounds_a_frame_without_a_newline() {
        let bytes = vec![b'x'; 1024];
        let mut reader = BufReader::new(bytes.as_slice());

        let error = read_event(&mut reader, 128).await.unwrap_err();

        assert_eq!(error, "worker event exceeds admitted limit");
    }
}
