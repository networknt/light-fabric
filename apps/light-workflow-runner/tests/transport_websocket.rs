use std::collections::BTreeSet;
use std::fs;
use std::sync::Arc;
use std::time::Duration;

use execution_backend_mock::{MockBehavior, MockExecutionBackend};
use execution_runner_protocol::{
    ControllerToRunner, MessageEnvelope, RegisterAccepted, RunnerSessionId, RunnerToController,
};
use futures_util::{SinkExt, StreamExt};
use light_workflow_runner::configuration::{MockBackendConfig, RunnerBackendConfig, RunnerConfig};
use light_workflow_runner::health::HealthState;
use light_workflow_runner::journal::Journal;
use light_workflow_runner::staging::InputStager;
use light_workflow_runner::supervisor::Supervisor;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;
use uuid::Uuid;

#[tokio::test]
async fn registers_unbound_then_sends_session_bound_heartbeat() {
    let root = std::env::temp_dir().join(format!("runner-transport-{}", Uuid::new_v4()));
    fs::create_dir_all(&root).expect("create test directory");
    let token_path = root.join("runner.jwt");
    fs::write(&token_path, "opaque-test-token").expect("write token");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&token_path, fs::Permissions::from_mode(0o600))
            .expect("restrict token permissions");
    }

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake controller");
    let address = listener.local_addr().expect("fake controller address");
    let session_id = RunnerSessionId::new();
    let generation = 7;

    let controller = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept runner connection");
        let mut socket = tokio_tungstenite::accept_async(stream)
            .await
            .expect("upgrade runner websocket");

        let registration = receive::<RunnerToController>(&mut socket).await;
        assert!(registration.runner_session_id.is_none());
        assert!(registration.connection_generation.is_none());
        assert!(matches!(
            registration.payload,
            RunnerToController::RunnerRegister(_)
        ));

        let mut accepted = MessageEnvelope::new(ControllerToRunner::RunnerRegisterAccepted(
            RegisterAccepted {
                runner_session_id: session_id,
                connection_generation: generation,
                heartbeat_interval_ms: 10,
                admission_digest: "admission-digest".to_string(),
            },
        ));
        accepted.runner_session_id = Some(session_id);
        accepted.connection_generation = Some(generation);
        socket
            .send(Message::Text(
                serde_json::to_string(&accepted).unwrap().into(),
            ))
            .await
            .expect("send registration acceptance");

        let heartbeat = tokio::time::timeout(
            Duration::from_secs(1),
            receive::<RunnerToController>(&mut socket),
        )
        .await
        .expect("runner heartbeat timeout");
        assert_eq!(heartbeat.runner_session_id, Some(session_id));
        assert_eq!(heartbeat.connection_generation, Some(generation));
        assert!(matches!(
            heartbeat.payload,
            RunnerToController::RunnerHeartbeat(_)
        ));
    });

    let compatibility_digest = "0123456789abcdef".to_string();
    let command_digest = "fedcba9876543210".to_string();
    let config = Arc::new(RunnerConfig {
        runner_id: "runner-test".to_string(),
        enrollment_id: "enrollment-test".to_string(),
        host_id: Uuid::new_v4(),
        controller_url: format!("ws://{address}/ws/runner"),
        jwt_file: token_path,
        data_directory: root.clone(),
        health_address: "127.0.0.1:0".parse().unwrap(),
        maximum_concurrency: 1,
        heartbeat_interval: Duration::from_millis(10),
        reconnect_maximum: Duration::from_millis(100),
        shutdown_grace: Duration::from_secs(1),
        staging_maximum_bytes: 1024,
        backend: RunnerBackendConfig::Mock(MockBackendConfig {
            compatibility_digest: compatibility_digest.clone(),
            available_slots: 1,
            behavior: MockBehavior::default(),
        }),
        allowed_command_template_digests: BTreeSet::from([command_digest.clone()]),
        effective_config_digest: "effective-config-digest".to_string(),
        command_allowlist_digest: "command-allowlist-digest".to_string(),
        binary_digest: "sha256:test-binary".to_string(),
        agent_worker: None,
    });
    let backend = Arc::new(MockExecutionBackend::new(
        compatibility_digest,
        MockBehavior::default(),
    ));
    let journal = Journal::open(&root.join("journal.sqlite")).expect("open journal");
    let stager = InputStager::new(root.join("staging"), 1024).expect("create stager");
    let supervisor = Supervisor::new(
        backend,
        journal,
        stager,
        BTreeSet::from([command_digest]),
        1,
        None,
    );
    let health = HealthState::new(supervisor.clone());
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let runner = tokio::spawn(light_workflow_runner::transport::run(
        config,
        supervisor,
        health,
        shutdown_rx,
    ));

    controller.await.expect("fake controller task");
    shutdown_tx.send(true).expect("request runner shutdown");
    tokio::time::timeout(Duration::from_secs(1), runner)
        .await
        .expect("runner shutdown timeout")
        .expect("runner transport task");
    fs::remove_dir_all(root).expect("remove test directory");
}

async fn receive<T>(
    socket: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
) -> MessageEnvelope<T>
where
    T: serde::de::DeserializeOwned,
{
    let message = socket
        .next()
        .await
        .expect("websocket remains open")
        .expect("valid websocket frame");
    let Message::Text(text) = message else {
        panic!("expected websocket text frame")
    };
    serde_json::from_str(&text).expect("valid protocol envelope")
}
