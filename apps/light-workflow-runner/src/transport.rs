use std::sync::Arc;
use std::time::Duration;

use execution_runner_protocol::{
    ControllerToRunner, MessageEnvelope, PROTOCOL_VERSION, RunnerCapabilityDocument,
    RunnerHeartbeat, RunnerRegistration, RunnerToController,
};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, watch};
use tokio::time::{MissedTickBehavior, interval};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::Message;
use tracing::{info, warn};

use crate::configuration::RunnerConfig;
use crate::health::HealthState;
use crate::supervisor::Supervisor;

#[derive(Clone, Copy)]
struct ConnectionBinding {
    session_id: execution_runner_protocol::RunnerSessionId,
    generation: u64,
    heartbeat_interval: Duration,
}

pub async fn run(
    config: Arc<RunnerConfig>,
    supervisor: Arc<Supervisor>,
    health: Arc<HealthState>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut retry = Duration::from_millis(250);
    loop {
        if *shutdown.borrow() {
            return;
        }
        health.set_controller_connected(false);
        match connect_once(&config, &supervisor, &health, &mut shutdown).await {
            Ok(()) if *shutdown.borrow() => return,
            Ok(()) => warn!("runner controller connection closed"),
            Err(error) => warn!(%error, "runner controller connection failed"),
        }
        health.set_controller_connected(false);
        tokio::select! {
            _ = tokio::time::sleep(retry) => {}
            _ = shutdown.changed() => continue,
        }
        retry = (retry * 2).min(config.reconnect_maximum);
    }
}

async fn connect_once(
    config: &RunnerConfig,
    supervisor: &Arc<Supervisor>,
    health: &Arc<HealthState>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<(), String> {
    let token = config.read_jwt()?;
    let mut request = config
        .controller_url
        .as_str()
        .into_client_request()
        .map_err(|error| format!("build controller request: {error}"))?;
    request.headers_mut().insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|error| format!("invalid runner token: {error}"))?,
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .map_err(|error| format!("connect controller websocket: {error}"))?;

    send_envelope(&mut socket, registration(config, supervisor), None).await?;
    let binding = tokio::time::timeout(config.reconnect_maximum, receive_registration(&mut socket))
        .await
        .map_err(|_| "controller registration response timed out".to_string())??;
    health.set_controller_connected(true);
    info!(session_id = %binding.session_id, generation = binding.generation, "runner controller registration accepted");

    let (outbound_tx, mut outbound_rx) = mpsc::channel::<RunnerToController>(128);
    supervisor.recover(&outbound_tx).await?;
    supervisor.resend_pending(&outbound_tx).await;
    let mut heartbeat = interval(binding.heartbeat_interval);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    let _ = socket.close(None).await;
                    return Ok(());
                }
            }
            _ = heartbeat.tick() => {
                send_envelope(&mut socket, RunnerToController::RunnerHeartbeat(heartbeat_payload(config, supervisor)), Some(binding)).await?;
                supervisor.resend_pending(&outbound_tx).await;
                for lease in supervisor.active_lease_contexts() {
                    send_envelope(
                        &mut socket,
                        RunnerToController::RunnerLeaseRenew(execution_runner_protocol::LeaseRenewal {
                            lease,
                            observed_at: chrono::Utc::now(),
                        }),
                        Some(binding),
                    ).await?;
                }
            }
            outbound = outbound_rx.recv() => {
                let outbound = outbound.ok_or_else(|| "runner outbound channel closed".to_string())?;
                send_envelope(&mut socket, outbound, Some(binding)).await?;
            }
            incoming = socket.next() => {
                let incoming = incoming.ok_or_else(|| "controller closed websocket".to_string())?
                    .map_err(|error| format!("read controller websocket: {error}"))?;
                match incoming {
                    Message::Text(text) => process_controller_message(&text, binding, supervisor, &outbound_tx).await?,
                    Message::Ping(payload) => socket.send(Message::Pong(payload)).await.map_err(|error| format!("send pong: {error}"))?,
                    Message::Pong(_) => {}
                    Message::Close(frame) => return Err(format!("controller closed websocket: {frame:?}")),
                    Message::Binary(_) | Message::Frame(_) => return Err("controller sent unsupported websocket frame".to_string()),
                }
            }
        }
    }
}

fn registration(config: &RunnerConfig, supervisor: &Supervisor) -> RunnerToController {
    RunnerToController::RunnerRegister(RunnerRegistration {
        runner_id: config.runner_id.clone(),
        host_id: config.host_id,
        capability: RunnerCapabilityDocument {
            runner_version: env!("CARGO_PKG_VERSION").to_string(),
            protocol_versions: vec![PROTOCOL_VERSION.to_string()],
            maximum_concurrency: config.maximum_concurrency,
            effective_config_digest: config.effective_config_digest.clone(),
            command_allowlist_digest: config.command_allowlist_digest.clone(),
            watchdog_healthy: supervisor.watchdog_healthy(),
            journal_healthy: supervisor.journal_healthy(),
            backends: vec![supervisor.backend_capability()],
        },
        binary_digest: config.binary_digest.clone(),
        enrollment_id: config.enrollment_id.clone(),
    })
}

fn heartbeat_payload(config: &RunnerConfig, supervisor: &Supervisor) -> RunnerHeartbeat {
    RunnerHeartbeat {
        effective_config_digest: config.effective_config_digest.clone(),
        command_allowlist_digest: config.command_allowlist_digest.clone(),
        watchdog_healthy: supervisor.watchdog_healthy(),
        journal_healthy: supervisor.journal_healthy(),
        cleanup_backlog: supervisor.cleanup_backlog(),
        available_capacity: supervisor.available_capacity(),
        active_leases: supervisor.active_leases(),
    }
}

async fn receive_registration<S>(
    socket: &mut tokio_tungstenite::WebSocketStream<S>,
) -> Result<ConnectionBinding, String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let message = socket
        .next()
        .await
        .ok_or_else(|| "controller closed before registration".to_string())?
        .map_err(|error| format!("read registration response: {error}"))?;
    let Message::Text(text) = message else {
        return Err("registration response must be text".to_string());
    };
    let envelope = parse_envelope::<ControllerToRunner>(&text)?;
    let ControllerToRunner::RunnerRegisterAccepted(accepted) = envelope.payload else {
        return Err("controller did not accept runner registration".to_string());
    };
    if envelope.runner_session_id != Some(accepted.runner_session_id)
        || envelope.connection_generation != Some(accepted.connection_generation)
    {
        return Err("registration response binding is inconsistent".to_string());
    }
    if accepted.heartbeat_interval_ms == 0 {
        return Err("controller returned a zero heartbeat interval".to_string());
    }
    Ok(ConnectionBinding {
        session_id: accepted.runner_session_id,
        generation: accepted.connection_generation,
        heartbeat_interval: Duration::from_millis(accepted.heartbeat_interval_ms),
    })
}

async fn process_controller_message(
    text: &str,
    binding: ConnectionBinding,
    supervisor: &Arc<Supervisor>,
    outbound: &mpsc::Sender<RunnerToController>,
) -> Result<(), String> {
    let envelope = parse_envelope::<ControllerToRunner>(text)?;
    if envelope.runner_session_id != Some(binding.session_id)
        || envelope.connection_generation != Some(binding.generation)
    {
        return Err("controller message carries a stale session binding".to_string());
    }
    match envelope.payload {
        ControllerToRunner::RunnerExecuteLease(lease) => {
            supervisor.accept_execute(lease, outbound.clone()).await
        }
        ControllerToRunner::RunnerLeaseResultAccepted(accepted) => {
            supervisor.result_accepted(&accepted).await
        }
        ControllerToRunner::RunnerCancelLease(cancel) => supervisor.cancel_lease(&cancel).await,
        ControllerToRunner::RunnerDrainRequested(_) => {
            supervisor.drain();
            Ok(())
        }
        ControllerToRunner::RunnerReconcileLease(lease) => {
            supervisor.reconcile_lease(&lease, outbound).await
        }
        ControllerToRunner::RunnerSessionRejected(rejected) => Err(format!(
            "runner session rejected: {}: {}",
            rejected.reason_code, rejected.message
        )),
        ControllerToRunner::RunnerRegisterAccepted(_) => {
            Err("duplicate registration acceptance".to_string())
        }
        ControllerToRunner::RunnerHoldSession(_)
        | ControllerToRunner::RunnerResumeSession(_)
        | ControllerToRunner::RunnerCleanupSession(_) => {
            Err("execution sessions are not enabled for the mock runner".to_string())
        }
    }
}

fn parse_envelope<T: serde::de::DeserializeOwned>(
    text: &str,
) -> Result<MessageEnvelope<T>, String> {
    let envelope = serde_json::from_str::<MessageEnvelope<T>>(text)
        .map_err(|error| format!("invalid controller envelope: {error}"))?;
    envelope
        .protocol_major()
        .map_err(|error| error.to_string())?;
    Ok(envelope)
}

async fn send_envelope<S>(
    socket: &mut tokio_tungstenite::WebSocketStream<S>,
    payload: RunnerToController,
    binding: Option<ConnectionBinding>,
) -> Result<(), String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut envelope = MessageEnvelope::new(payload);
    if let Some(binding) = binding {
        envelope.runner_session_id = Some(binding.session_id);
        envelope.connection_generation = Some(binding.generation);
    }
    let text = serde_json::to_string(&envelope)
        .map_err(|error| format!("serialize runner envelope: {error}"))?;
    socket
        .send(Message::Text(text.into()))
        .await
        .map_err(|error| format!("send runner envelope: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;

    use chrono::Duration as ChronoDuration;
    use execution_backend_mock::{MockBehavior, MockExecutionBackend};
    use execution_runner_protocol::{
        AttemptState, AuthenticatedOrigin, CommandExecutionSpec, ExecuteLease, ExecutionId,
        ExecutionSubject, LeaseId, OriginKind, RegisterAccepted, RunnerSessionId,
        SchedulingRequestId,
    };
    use tempfile::TempDir;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_hdr_async;
    use uuid::Uuid;

    use crate::{journal::Journal, staging::InputStager};

    #[tokio::test]
    async fn registration_execute_and_terminal_ack_round_trip() {
        let temp = TempDir::new().unwrap();
        let token_path = temp.path().join("runner.jwt");
        fs::write(&token_path, "test-token").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&token_path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let listener = match TcpListener::bind("127.0.0.1:0").await {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(error) => panic!("bind fake controller: {error}"),
        };
        let address = listener.local_addr().unwrap();
        let config = Arc::new(RunnerConfig {
            runner_id: "runner-test".into(),
            enrollment_id: "enrollment-test".into(),
            host_id: Uuid::nil(),
            controller_url: format!("ws://{address}/ws/runner"),
            jwt_file: token_path,
            data_directory: temp.path().join("data"),
            health_address: "127.0.0.1:0".parse().unwrap(),
            maximum_concurrency: 1,
            heartbeat_interval: Duration::from_millis(25),
            reconnect_maximum: Duration::from_secs(1),
            shutdown_grace: Duration::from_secs(1),
            staging_maximum_bytes: 1024,
            backend: crate::configuration::MockBackendConfig {
                compatibility_digest: "sha256:compatibility".into(),
                available_slots: 1,
                behavior: MockBehavior::default(),
            },
            allowed_command_template_digests: BTreeSet::from(["sha256:template".into()]),
            effective_config_digest: "sha256:effective-config".into(),
            command_allowlist_digest: "sha256:command-allowlist".into(),
            binary_digest: "sha256:runner-binary".into(),
            agent_worker: None,
        });
        let journal = Journal::open(&temp.path().join("journal.sqlite")).unwrap();
        let stager = InputStager::new(temp.path().join("staging"), 1024).unwrap();
        let backend = Arc::new(MockExecutionBackend::new(
            "sha256:compatibility",
            MockBehavior::default(),
        ));
        let supervisor = Supervisor::new(
            backend,
            journal,
            stager,
            config.allowed_command_template_digests.clone(),
            1,
            None,
        );
        let health = HealthState::new(Arc::clone(&supervisor));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let lease = test_lease();
        let expected_execution_id = lease.lease.execution_id;
        let server_supervisor = Arc::clone(&supervisor);

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_hdr_async(stream, |request: &tokio_tungstenite::tungstenite::handshake::server::Request, response: tokio_tungstenite::tungstenite::handshake::server::Response| {
                assert_eq!(request.uri().path(), "/ws/runner");
                assert_eq!(request.headers().get("authorization").unwrap(), "Bearer test-token");
                Ok(response)
            }).await.unwrap();
            let registration = receive_runner(&mut socket).await;
            assert!(matches!(
                registration.payload,
                RunnerToController::RunnerRegister(_)
            ));
            assert!(registration.runner_session_id.is_none());
            let session_id = RunnerSessionId::new();
            let generation = 3;
            send_controller(
                &mut socket,
                ControllerToRunner::RunnerRegisterAccepted(RegisterAccepted {
                    runner_session_id: session_id,
                    connection_generation: generation,
                    heartbeat_interval_ms: 25,
                    admission_digest: "sha256:admission".into(),
                }),
                session_id,
                generation,
            )
            .await;

            loop {
                let envelope = receive_runner(&mut socket).await;
                assert_eq!(envelope.runner_session_id, Some(session_id));
                assert_eq!(envelope.connection_generation, Some(generation));
                if matches!(envelope.payload, RunnerToController::RunnerHeartbeat(_)) {
                    break;
                }
            }
            send_controller(
                &mut socket,
                ControllerToRunner::RunnerExecuteLease(lease),
                session_id,
                generation,
            )
            .await;

            let mut accepted = false;
            let mut started = false;
            loop {
                let envelope = receive_runner(&mut socket).await;
                match envelope.payload {
                    RunnerToController::RunnerLeaseAccepted(context) => {
                        assert_eq!(context.execution_id, expected_execution_id);
                        accepted = true;
                    }
                    RunnerToController::RunnerLeaseStarted(context) => {
                        assert_eq!(context.execution_id, expected_execution_id);
                        started = true;
                    }
                    RunnerToController::RunnerLeaseSucceeded(terminal) => {
                        assert!(accepted && started);
                        assert_eq!(terminal.result.state, AttemptState::Succeeded);
                        send_controller(
                            &mut socket,
                            ControllerToRunner::RunnerLeaseResultAccepted(
                                execution_runner_protocol::LeaseResultAccepted {
                                    execution_id: terminal.lease.execution_id,
                                    lease_id: terminal.lease.lease_id,
                                    fencing_token: terminal.lease.fencing_token,
                                    state: terminal.result.state,
                                },
                            ),
                            session_id,
                            generation,
                        )
                        .await;
                        tokio::time::timeout(Duration::from_secs(1), async {
                            while server_supervisor.cleanup_backlog() != 0 {
                                tokio::time::sleep(Duration::from_millis(5)).await;
                            }
                        })
                        .await
                        .unwrap();
                        let _ = shutdown_tx.send(true);
                        tokio::time::sleep(Duration::from_millis(25)).await;
                        break;
                    }
                    _ => {}
                }
            }
        });

        tokio::time::timeout(
            Duration::from_secs(5),
            connect_once(&config, &supervisor, &health, &mut shutdown_rx.clone()),
        )
        .await
        .unwrap()
        .unwrap();
        server.await.unwrap();
        assert_eq!(supervisor.cleanup_backlog(), 0);
    }

    fn test_lease() -> ExecuteLease {
        let command = CommandExecutionSpec {
            schema_version: 1,
            template_id: "test".into(),
            template_version: 1,
            template_digest: "sha256:template".into(),
            executable: "/usr/bin/true".into(),
            arguments: Vec::new(),
            working_directory: "/workspace".into(),
            environment: BTreeMap::new(),
            wall_clock_timeout_ms: 5_000,
            stdout_limit_bytes: 1024,
            stderr_limit_bytes: 1024,
            network_enabled: false,
            credentials_enabled: false,
            persistent_workspace: false,
        };
        ExecuteLease {
            lease: execution_runner_protocol::LeaseContext {
                scheduling_request_id: SchedulingRequestId::new(),
                execution_id: ExecutionId::new(),
                origin: AuthenticatedOrigin {
                    kind: OriginKind::Workflow,
                    service_id: "light-workflow".into(),
                    instance_id: "workflow-test".into(),
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
                policy_digest: "sha256:policy".into(),
                compatibility_digest: "sha256:compatibility".into(),
                deadline: chrono::Utc::now() + ChronoDuration::minutes(1),
            },
            backend_id: "mock".into(),
            execution_profile: serde_json::json!({}),
            command: serde_json::to_value(command).unwrap(),
            inputs: Vec::new(),
            definition_digest: "sha256:definition".into(),
            command_template_digest: "sha256:template".into(),
        }
    }

    async fn receive_runner<S>(
        socket: &mut tokio_tungstenite::WebSocketStream<S>,
    ) -> MessageEnvelope<RunnerToController>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        let Message::Text(text) = socket.next().await.unwrap().unwrap() else {
            panic!("expected text frame");
        };
        parse_envelope(&text).unwrap()
    }

    async fn send_controller<S>(
        socket: &mut tokio_tungstenite::WebSocketStream<S>,
        payload: ControllerToRunner,
        session_id: RunnerSessionId,
        generation: u64,
    ) where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        let mut envelope = MessageEnvelope::new(payload);
        envelope.runner_session_id = Some(session_id);
        envelope.connection_generation = Some(generation);
        socket
            .send(Message::Text(
                serde_json::to_string(&envelope).unwrap().into(),
            ))
            .await
            .unwrap();
    }
}
