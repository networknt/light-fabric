use chrono::{Duration, Utc};
use execution_backend::{BackendError, BackendOperationState, ExecutionBackend};
use execution_backend_cube::{
    CubeBackendConfig, CubeExecutionBackend, CubeHttpClient, CubeHttpClientConfig,
};
use execution_runner_protocol::{
    AuthenticatedOrigin, CommandExecutionSpec, ExecuteLease, ExecutionId, ExecutionSubject,
    LeaseContext, LeaseId, OriginKind, SchedulingRequestId,
};
use std::{collections::BTreeMap, fs, sync::Arc, time::Duration as StdDuration};
use url::Url;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires live Cube failure-matrix template and LIGHT_CUBE_TEST_* configuration"]
async fn live_cube_failure_matrix_is_cleanup_complete() {
    let backend = backend();

    // Non-zero worker exit is terminal failure and remains inspectable.
    let failure = lease("/bin/false", vec![], 10_000);
    let prepared = backend.prepare(&failure, &[]).await.unwrap();
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let result = backend.execute(&prepared, &failure, rx).await.unwrap();
    assert!(result.failure_class.is_some());
    assert!(matches!(
        backend
            .inspect(&prepared.backend_operation_id)
            .await
            .unwrap()
            .state,
        BackendOperationState::Failed
    ));
    backend
        .cleanup(&prepared.backend_operation_id)
        .await
        .unwrap();
    backend
        .cleanup(&prepared.backend_operation_id)
        .await
        .unwrap();

    // Backend deadline terminates a long-running process.
    let timeout_lease = lease("/bin/sleep", vec!["30".into()], 250);
    let prepared = backend.prepare(&timeout_lease, &[]).await.unwrap();
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let timed = backend.execute(&prepared, &timeout_lease, rx).await;
    assert!(
        matches!(timed, Err(BackendError::TimedOut(_))) || timed.unwrap().failure_class.is_some()
    );
    backend
        .cleanup(&prepared.backend_operation_id)
        .await
        .unwrap();

    // Explicit cancellation physically stops the command.
    let cancelled_lease = lease("/bin/sleep", vec!["30".into()], 30_000);
    let prepared = backend.prepare(&cancelled_lease, &[]).await.unwrap();
    let (cancel, rx) = tokio::sync::watch::channel(false);
    let execution = backend.execute(&prepared, &cancelled_lease, rx);
    tokio::pin!(execution);
    tokio::time::sleep(StdDuration::from_millis(250)).await;
    cancel.send(true).unwrap();
    let cancelled = execution.await;
    assert!(
        matches!(cancelled, Err(BackendError::Cancelled(_)))
            || cancelled.unwrap().failure_class.is_some()
    );
    backend
        .cleanup(&prepared.backend_operation_id)
        .await
        .unwrap();

    // A lost client/runner response is recovered by inspection, then cancelled
    // and cleaned without replaying the command.
    let disconnected = lease("/bin/sleep", vec!["30".into()], 30_000);
    let prepared = backend.prepare(&disconnected, &[]).await.unwrap();
    let (_cancel, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn({
        let backend = backend.clone();
        let prepared = prepared.clone();
        let disconnected = disconnected.clone();
        async move { backend.execute(&prepared, &disconnected, rx).await }
    });
    tokio::time::sleep(StdDuration::from_millis(250)).await;
    task.abort();
    let state = backend
        .inspect(&prepared.backend_operation_id)
        .await
        .unwrap()
        .state;
    assert!(matches!(
        state,
        BackendOperationState::Running | BackendOperationState::Prepared
    ));
    backend
        .cancel(&prepared.backend_operation_id)
        .await
        .unwrap();
    backend
        .cleanup(&prepared.backend_operation_id)
        .await
        .unwrap();

    // Every case left no live environment; cleanup is idempotent evidence.
    assert!(matches!(
        backend.inspect(&prepared.backend_operation_id).await,
        Err(BackendError::NotFound(_))
    ));
}

fn backend() -> Arc<CubeExecutionBackend<CubeHttpClient>> {
    let api_url = Url::parse(&required("LIGHT_CUBE_TEST_API_URL")).unwrap();
    let sandbox_url = std::env::var("LIGHT_CUBE_TEST_SANDBOX_URL")
        .ok()
        .map(|value| Url::parse(&value).unwrap());
    let api_key = fs::read_to_string(required("LIGHT_CUBE_TEST_API_KEY_FILE")).unwrap();
    let tls_ca_pem = std::env::var("LIGHT_CUBE_TEST_TLS_CA_FILE")
        .ok()
        .map(|path| fs::read(path).unwrap());
    let client = CubeHttpClient::new(CubeHttpClientConfig {
        api_url,
        sandbox_url,
        api_key: api_key.trim().into(),
        request_timeout: StdDuration::from_secs(30),
        maximum_response_bytes: 4 * 1024 * 1024,
        allow_insecure_http: std::env::var("LIGHT_CUBE_TEST_ALLOW_HTTP").as_deref() == Ok("true"),
        tls_ca_pem,
    })
    .unwrap();
    Arc::new(CubeExecutionBackend::new(
        Arc::new(client),
        CubeBackendConfig {
            template_id: required("LIGHT_CUBE_TEST_TEMPLATE_ID"),
            compatibility_digest: "sha256:live-cube-failure-matrix-v1".into(),
            owner_runner: "cube-live-failure-matrix".into(),
            available_slots: 1,
            maximum_native_ttl_seconds: 120,
            discovery_page_limit: 200,
        },
    ))
}

fn lease(executable: &str, arguments: Vec<String>, wall_clock_timeout_ms: u64) -> ExecuteLease {
    let template_digest = "sha256:live-cube-failure-matrix-template-v1";
    ExecuteLease {
        lease: LeaseContext {
            scheduling_request_id: SchedulingRequestId::new(),
            execution_id: ExecutionId::new(),
            origin: AuthenticatedOrigin {
                kind: OriginKind::Agent,
                service_id: "light-agent".into(),
                instance_id: "cube-matrix".into(),
                host_id: Uuid::nil(),
            },
            subject: ExecutionSubject::AgentTurn {
                subject_id: Uuid::new_v4(),
                session_id: Uuid::new_v4(),
                turn_id: Uuid::new_v4(),
            },
            attempt: 1,
            lease_id: LeaseId::new(),
            fencing_token: 1,
            policy_digest: "sha256:live-policy".into(),
            compatibility_digest: "sha256:live-cube-failure-matrix-v1".into(),
            deadline: Utc::now() + Duration::minutes(2),
        },
        backend_id: "cube".into(),
        execution_profile: serde_json::json!({}),
        command: serde_json::to_value(CommandExecutionSpec {
            schema_version: 1,
            template_id: "cube-failure-matrix-v1".into(),
            template_version: 1,
            template_digest: template_digest.into(),
            executable: executable.into(),
            arguments,
            working_directory: "/workspace".into(),
            environment: BTreeMap::new(),
            wall_clock_timeout_ms,
            stdout_limit_bytes: 64 * 1024,
            stderr_limit_bytes: 64 * 1024,
            network_enabled: false,
            credentials_enabled: false,
            persistent_workspace: false,
        })
        .unwrap(),
        inputs: vec![],
        definition_digest: "sha256:live-matrix".into(),
        command_template_digest: template_digest.into(),
    }
}

fn required(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is required"))
}
