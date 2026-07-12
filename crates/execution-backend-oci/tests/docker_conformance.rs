use chrono::{Duration, Utc};
use execution_backend_conformance::exercise_lifecycle;
use execution_backend_oci::{OciBackendConfig, OciExecutionBackend, OciRuntime};
use execution_runner_protocol::{
    AuthenticatedOrigin, CommandExecutionSpec, ExecuteLease, ExecutionId, ExecutionSubject,
    LeaseContext, LeaseId, OriginKind, SchedulingRequestId,
};
use std::{collections::BTreeMap, path::PathBuf};
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires a local Docker daemon and LIGHT_OCI_CONFORMANCE_IMAGE pinned by sha256"]
async fn docker_passes_shared_backend_conformance() {
    let image = std::env::var("LIGHT_OCI_CONFORMANCE_IMAGE").expect("pinned image is required");
    let backend = OciExecutionBackend::new(OciBackendConfig {
        runtime: OciRuntime::Docker,
        binary: PathBuf::from("/usr/bin/docker"),
        image,
        compatibility_digest: "sha256:oci-conformance-v1".into(),
        available_slots: 1,
        rootless: false,
        maximum_memory_bytes: 128 * 1024 * 1024,
        maximum_pids: 64,
    })
    .unwrap();
    let lease = ExecuteLease {
        lease: LeaseContext {
            scheduling_request_id: SchedulingRequestId::new(),
            execution_id: ExecutionId::new(),
            origin: AuthenticatedOrigin {
                kind: OriginKind::Workflow,
                service_id: "conformance".into(),
                instance_id: "local".into(),
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
            compatibility_digest: "sha256:oci-conformance-v1".into(),
            deadline: Utc::now() + Duration::minutes(1),
        },
        backend_id: "docker".into(),
        execution_profile: serde_json::json!({}),
        command: serde_json::to_value(CommandExecutionSpec {
            schema_version: 1,
            template_id: "conformance".into(),
            template_version: 1,
            template_digest: "template".into(),
            executable: "/hello".into(),
            arguments: vec![],
            working_directory: "/workspace".into(),
            environment: BTreeMap::new(),
            wall_clock_timeout_ms: 30_000,
            stdout_limit_bytes: 64 * 1024,
            stderr_limit_bytes: 64 * 1024,
            network_enabled: false,
            credentials_enabled: false,
            persistent_workspace: false,
        })
        .unwrap(),
        inputs: vec![],
        definition_digest: "definition".into(),
        command_template_digest: "template".into(),
    };
    exercise_lifecycle(&backend, &lease, &[]).await.unwrap();
}
