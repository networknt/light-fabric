//! Local qualification through the real runner admission, transport, event
//! validation and journal. The caller supplies an already admitted execution.
use agent_runtime_protocol::AgentWorkerExecutionSpec;
use execution_runner_protocol::*;
use light_workflow_runner::{
    journal::Journal,
    worker_process::{WorkerProcessConfig, run_worker_process},
};
use serde::Deserialize;
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Request {
    config: WorkerProcessConfig,
    spec: AgentWorkerExecutionSpec,
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.len() != 2 {
        return Err("usage: claude-dispatch REQUEST_JSON JOURNAL_SQLITE".into());
    }
    let Request { config, spec } = serde_json::from_slice(&std::fs::read(&args[0])?)?;
    let lease = ExecuteLease {
        lease: LeaseContext {
            scheduling_request_id: SchedulingRequestId::new(),
            execution_id: ExecutionId::new(),
            origin: AuthenticatedOrigin {
                kind: OriginKind::Agent,
                service_id: config.origin_service_id.clone(),
                instance_id: "phase2-agent".into(),
                host_id: uuid::Uuid::new_v4(),
            },
            subject: ExecutionSubject::AgentTurn {
                subject_id: spec.turn_id.0,
                session_id: spec.session_id.0,
                turn_id: spec.turn_id.0,
            },
            attempt: 1,
            lease_id: LeaseId::new(),
            fencing_token: 7,
            policy_digest: spec.policy_digest.clone(),
            compatibility_digest: spec.input["adapterContract"]["compatibilityDigest"]
                .as_str()
                .ok_or("compatibility missing")?
                .into(),
            deadline: chrono::Utc::now() + chrono::Duration::seconds(190),
        },
        backend_id: "local-claude".into(),
        execution_profile: serde_json::json!({}),
        command: serde_json::to_value(&spec)?,
        inputs: vec![],
        definition_digest: spec.template_digest.clone(),
        command_template_digest: spec.template_digest.clone(),
    };
    let journal = Journal::open(std::path::Path::new(&args[1]))?;
    journal.record_intent(&lease)?;
    let (_cancel, rx) = tokio::sync::watch::channel(false);
    let outcome = run_worker_process(&lease, &spec, &config, &journal, rx).await?;
    if outcome.class != agent_core::ResultClass::Success {
        return Err(format!("runner outcome {:?}: {:?}", outcome.class, outcome.error).into());
    }
    let output = outcome.output.ok_or("missing output")?;
    println!(
        "{}",
        serde_json::json!({"output":output,"events":outcome.events,"fencingToken":lease.lease.fencing_token})
    );
    Ok(())
}
