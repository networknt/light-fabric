use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use controller_rs::auth::RunnerJwtClaims;
use controller_rs::config::Settings;
use controller_rs::runner::{LiveRunnerSession, RunnerRuntime};
use execution_backend::BackendError;
use execution_backend_cube::{
    CubeApi, CubeBackendConfig, CubeCommandResult, CubeCreateRequest, CubeExecutionBackend,
    CubeInputMount, CubeResource, CubeState,
};
use execution_runner_protocol::{
    ArtifactEvidence, AttemptState, CommandExecutionSpec, ControllerToRunner, ExecutionId,
    ExecutionRequirements, HostExposure, IsolationBoundary, MessageEnvelope, RegisterAccepted,
    RunnerToController,
};
use futures_util::{SinkExt, StreamExt};
use light_workflow_runner::configuration::{
    CubeImplementation, CubeRunnerBackendConfig, RunnerBackendConfig, RunnerConfig,
};
use light_workflow_runner::health::HealthState;
use light_workflow_runner::journal::{Journal, JournalState};
use light_workflow_runner::staging::InputStager;
use light_workflow_runner::supervisor::Supervisor;
use serde_json::{Value, json};
use sqlx::{PgPool, Row, postgres::PgPoolOptions};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::{WebSocketStream, accept_async, tungstenite::Message};
use uuid::Uuid;

const ORIGIN_SERVICE: &str = "runner-cube-full-stack-origin";
const RUNNER_ID: &str = "runner-cube-full-stack";
const ENROLLMENT_ID: &str = "runner-cube-full-stack";
const RUNNER_SUBJECT: &str = "runner-cube-full-stack-subject";
const COMPATIBILITY: &str = "sha256:runner-cube-full-stack-compatibility";
const CONFIG_DIGEST: &str = "sha256:runner-cube-full-stack-config";
const ALLOWLIST_DIGEST: &str = "sha256:runner-cube-full-stack-allowlist";
const BINARY_DIGEST: &str = "sha256:runner-cube-full-stack-binary";
const TEMPLATE_DIGEST: &str = "sha256:runner-cube-full-stack-template";

#[derive(Default)]
struct DeterministicCube {
    resource: Mutex<Option<CubeResource>>,
    creates: Mutex<u32>,
    executes: Mutex<u32>,
    deletes: Mutex<u32>,
}

impl DeterministicCube {
    fn counts(&self) -> (u32, u32, u32) {
        (
            *self.creates.lock().unwrap(),
            *self.executes.lock().unwrap(),
            *self.deletes.lock().unwrap(),
        )
    }
}

#[async_trait]
impl CubeApi for DeterministicCube {
    async fn create(&self, request: CubeCreateRequest) -> Result<CubeResource, BackendError> {
        *self.creates.lock().unwrap() += 1;
        let resource = CubeResource {
            environment_id: format!("cube-full-stack-{}", Uuid::now_v7()),
            idempotency_key: request.idempotency_key,
            state: CubeState::Ready,
            expires_at: request.expires_at,
            tags: request.tags,
        };
        *self.resource.lock().unwrap() = Some(resource.clone());
        Ok(resource)
    }

    async fn find_by_idempotency_key(
        &self,
        key: &str,
    ) -> Result<Option<CubeResource>, BackendError> {
        Ok(self
            .resource
            .lock()
            .unwrap()
            .clone()
            .filter(|resource| resource.idempotency_key == key))
    }

    async fn stage_inputs(&self, _: &str, inputs: &[CubeInputMount]) -> Result<(), BackendError> {
        if !inputs.is_empty() {
            return Err(BackendError::InvalidRequest(
                "full-stack fixture expects no inputs".into(),
            ));
        }
        Ok(())
    }

    async fn inspect(&self, id: &str) -> Result<Option<CubeResource>, BackendError> {
        Ok(self
            .resource
            .lock()
            .unwrap()
            .clone()
            .filter(|resource| resource.environment_id == id))
    }

    async fn execute(
        &self,
        environment_id: &str,
        command: &CommandExecutionSpec,
    ) -> Result<CubeCommandResult, BackendError> {
        let mut resource = self.resource.lock().unwrap();
        let current = resource
            .as_mut()
            .filter(|resource| resource.environment_id == environment_id)
            .ok_or_else(|| BackendError::NotFound(environment_id.into()))?;
        if command.executable != "/usr/bin/printf" || command.arguments != ["full-stack-ok"] {
            return Err(BackendError::InvalidRequest(
                "unexpected full-stack command".into(),
            ));
        }
        *self.executes.lock().unwrap() += 1;
        current.state = CubeState::Succeeded;
        let now = Utc::now();
        Ok(CubeCommandResult {
            exit_code: 0,
            stdout: b"full-stack-ok".to_vec(),
            stderr: Vec::new(),
            started_at: now,
            finished_at: now,
            evidence: BTreeMap::from([("fixture".into(), "controller-runner-cube".into())]),
        })
    }

    async fn set_timeout(&self, _: &str, seconds: u64) -> Result<(), BackendError> {
        if seconds == 0 {
            return Err(BackendError::InvalidRequest("zero Cube timeout".into()));
        }
        Ok(())
    }

    async fn cancel(&self, environment_id: &str) -> Result<(), BackendError> {
        if let Some(resource) = self.resource.lock().unwrap().as_mut()
            && resource.environment_id == environment_id
        {
            resource.state = CubeState::Cancelled;
        }
        Ok(())
    }

    async fn artifacts(&self, _: &str) -> Result<Vec<ArtifactEvidence>, BackendError> {
        Ok(Vec::new())
    }

    async fn delete(&self, environment_id: &str) -> Result<(), BackendError> {
        let mut resource = self.resource.lock().unwrap();
        if resource
            .as_ref()
            .is_some_and(|resource| resource.environment_id == environment_id)
        {
            *self.deletes.lock().unwrap() += 1;
            *resource = None;
        }
        Ok(())
    }

    async fn discover_owned(
        &self,
        owner_runner: &str,
        _: Option<&str>,
        _: usize,
    ) -> Result<(Vec<CubeResource>, Option<String>), BackendError> {
        let resources = self
            .resource
            .lock()
            .unwrap()
            .clone()
            .filter(|resource| {
                resource.tags.get("light.runner").map(String::as_str) == Some(owner_runner)
            })
            .into_iter()
            .collect();
        Ok((resources, None))
    }
}

#[tokio::test]
async fn controller_runner_and_cube_complete_one_fenced_lease_and_cleanup() {
    let Some(pool) = test_pool().await else {
        return;
    };
    let Some((host_id, workflow_definition_id)): Option<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT h.host_id,w.wf_def_id FROM host_t h
         JOIN wf_definition_t w ON w.host_id=h.host_id LIMIT 1",
    )
    .fetch_optional(&pool)
    .await
    .expect("query test host and workflow definition") else {
        return;
    };
    cleanup_rows(&pool, host_id).await;

    let root = std::env::temp_dir().join(format!("runner-cube-full-stack-{}", Uuid::now_v7()));
    fs::create_dir_all(&root).unwrap();
    let admission_path = root.join("admission.json");
    fs::write(
        &admission_path,
        serde_json::to_vec(&admission(host_id)).unwrap(),
    )
    .unwrap();
    let token_path = root.join("runner.jwt");
    fs::write(&token_path, "integration-token").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&token_path, fs::Permissions::from_mode(0o600)).unwrap();
    }

    let mut settings = Settings::for_tests();
    settings.database_url = std::env::var("DATABASE_URL").unwrap();
    settings.host_id = host_id.to_string();
    settings.runner_enabled = true;
    settings.runner_admission_path = Some(admission_path);
    settings.runner_scheduler_interval = Duration::from_millis(10);
    settings.runner_heartbeat_timeout = Duration::from_secs(5);
    settings.runner_reservation_ttl = Duration::from_secs(30);
    let runtime = RunnerRuntime::from_settings(pool.clone(), &settings).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let controller_address = listener.local_addr().unwrap();
    let claims = claims(host_id);
    let bridge_runtime = runtime.clone();
    let bridge = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        controller_bridge(stream, bridge_runtime, claims)
            .await
            .unwrap();
    });

    let cube = Arc::new(DeterministicCube::default());
    let backend = Arc::new(CubeExecutionBackend::new(
        cube.clone(),
        CubeBackendConfig {
            template_id: "full-stack-template".into(),
            compatibility_digest: COMPATIBILITY.into(),
            owner_runner: RUNNER_ID.into(),
            available_slots: 1,
            maximum_native_ttl_seconds: 300,
            discovery_page_limit: 100,
        },
    ));
    let journal = Journal::open(&root.join("journal.sqlite")).unwrap();
    let journal_evidence = journal.clone();
    let stager = InputStager::new(root.join("staging"), 1024).unwrap();
    let supervisor = Supervisor::new(
        backend,
        journal,
        stager,
        BTreeSet::from([TEMPLATE_DIGEST.into()]),
        1,
        None,
    );
    let health = HealthState::new(supervisor.clone());
    let config = Arc::new(runner_config(
        host_id,
        &root,
        token_path,
        controller_address,
    ));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let runner = tokio::spawn(light_workflow_runner::transport::run(
        config,
        supervisor,
        health,
        shutdown_rx,
    ));

    let request_id = insert_request(&pool, host_id, workflow_definition_id).await;
    let terminal = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(row) = sqlx::query(
                "SELECT a.execution_id,a.state,a.cleanup_state,a.normalized_result
                 FROM execution_attempt_t a WHERE a.host_id=$1 AND a.request_id=$2",
            )
            .bind(host_id)
            .bind(request_id)
            .fetch_optional(&pool)
            .await
            .unwrap()
                && row.try_get::<String, _>("state").unwrap() == "SUCCEEDED"
            {
                break row;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("full-stack lease did not complete");
    assert_eq!(
        terminal.try_get::<String, _>("cleanup_state").unwrap(),
        "CONFIRMED"
    );
    let result: Value = terminal.try_get("normalized_result").unwrap();
    assert_eq!(
        result.pointer("/stdout/inline"),
        Some(&json!("full-stack-ok"))
    );
    assert_eq!(result["evidence"]["fixture"], "controller-runner-cube");
    assert_eq!(cube.counts(), (1, 1, 1));
    let execution_id = ExecutionId(terminal.try_get::<Uuid, _>("execution_id").unwrap());
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if journal_evidence
                .find(execution_id)
                .unwrap()
                .is_some_and(|record| record.state == JournalState::CleanupConfirmed)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("runner did not durably acknowledge the accepted terminal result");

    shutdown_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(2), runner)
        .await
        .expect("runner shutdown timeout")
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), bridge)
        .await
        .expect("controller bridge shutdown timeout")
        .unwrap();
    cleanup_rows(&pool, host_id).await;
    fs::remove_dir_all(root).unwrap();
}

async fn controller_bridge(
    stream: TcpStream,
    runtime: Arc<RunnerRuntime>,
    claims: RunnerJwtClaims,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut socket = accept_async(stream).await?;
    let registration = receive(&mut socket).await?;
    let RunnerToController::RunnerRegister(registration) = registration.payload else {
        return Err("runner did not register first".into());
    };
    let (outbound_tx, mut outbound_rx) = mpsc::channel(64);
    let session = runtime
        .register(&claims, &registration, outbound_tx)
        .await?;
    let accepted = bound(
        &session,
        ControllerToRunner::RunnerRegisterAccepted(RegisterAccepted {
            runner_session_id: session.session_id,
            connection_generation: session.connection_generation,
            heartbeat_interval_ms: 25,
            admission_digest: session.accepted.admission_digest.clone(),
        }),
    );
    socket
        .send(Message::Text(serde_json::to_string(&accepted)?.into()))
        .await?;
    let mut scheduler = tokio::time::interval(Duration::from_millis(10));
    loop {
        tokio::select! {
            outbound = outbound_rx.recv() => {
                let Some(outbound) = outbound else { break };
                socket.send(Message::Text(serde_json::to_string(&outbound)?.into())).await?;
            }
            incoming = socket.next() => {
                let Some(incoming) = incoming else { break };
                match incoming? {
                    Message::Text(text) => {
                        let envelope: MessageEnvelope<RunnerToController> = serde_json::from_str(&text)?;
                        if envelope.runner_session_id != Some(session.session_id)
                            || envelope.connection_generation != Some(session.connection_generation) {
                            return Err("stale runner envelope".into());
                        }
                        process_runner_message(&runtime, &session, envelope.payload).await?;
                    }
                    Message::Close(_) => break,
                    Message::Ping(payload) => socket.send(Message::Pong(payload)).await?,
                    Message::Pong(_) => {},
                    _ => return Err("unsupported runner frame".into()),
                }
            }
            _ = scheduler.tick() => { runtime.schedule(&session).await?; }
        }
    }
    runtime.disconnect(&session).await?;
    Ok(())
}

async fn process_runner_message(
    runtime: &RunnerRuntime,
    session: &LiveRunnerSession,
    message: RunnerToController,
) -> Result<(), controller_rs::error::AppError> {
    match message {
        RunnerToController::RunnerHeartbeat(value) => runtime.heartbeat(session, &value).await,
        RunnerToController::RunnerLeaseAccepted(value) => {
            runtime.lease_accepted(session, &value).await
        }
        RunnerToController::RunnerLeaseStarted(value) => {
            runtime.lease_started(session, &value).await
        }
        RunnerToController::RunnerLeaseRenew(value) => runtime.lease_renewed(session, &value).await,
        RunnerToController::RunnerLeaseSucceeded(value) => {
            runtime
                .terminal(session, &value, AttemptState::Succeeded)
                .await
        }
        RunnerToController::RunnerLeaseFailed(value) => {
            runtime
                .terminal(session, &value, AttemptState::Failed)
                .await
        }
        RunnerToController::RunnerLeaseUnknown(value) => {
            runtime
                .terminal(session, &value, AttemptState::Unknown)
                .await
        }
        RunnerToController::RunnerLeaseCancelled(value) => {
            runtime
                .terminal(session, &value, AttemptState::Cancelled)
                .await
        }
        RunnerToController::RunnerCleanupCompleted(value) => {
            runtime.cleanup_completed(session, &value).await
        }
        RunnerToController::RunnerSessionUpdated(value) => {
            runtime.session_updated(session, &value).await
        }
        RunnerToController::RunnerDrain(_) => runtime.drain(session).await,
        RunnerToController::RunnerRegister(_) => Err(controller_rs::error::AppError::BadRequest(
            "duplicate registration".into(),
        )),
    }
}

fn bound(
    session: &LiveRunnerSession,
    payload: ControllerToRunner,
) -> MessageEnvelope<ControllerToRunner> {
    let mut envelope = MessageEnvelope::new(payload);
    envelope.runner_session_id = Some(session.session_id);
    envelope.connection_generation = Some(session.connection_generation);
    envelope
}

async fn receive(
    socket: &mut WebSocketStream<TcpStream>,
) -> Result<MessageEnvelope<RunnerToController>, Box<dyn std::error::Error + Send + Sync>> {
    let message = socket
        .next()
        .await
        .ok_or("runner closed before registration")??;
    let Message::Text(text) = message else {
        return Err("registration is not text".into());
    };
    Ok(serde_json::from_str(&text)?)
}

async fn test_pool() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    Some(
        PgPoolOptions::new()
            .max_connections(8)
            .connect(&url)
            .await
            .unwrap(),
    )
}

fn claims(host_id: Uuid) -> RunnerJwtClaims {
    RunnerJwtClaims {
        sub: RUNNER_SUBJECT.into(),
        exp: usize::MAX,
        scp: Some(json!("runner.connect")),
        roles: None,
        host: host_id.to_string(),
        runner_id: RUNNER_ID.into(),
        enrollment_id: ENROLLMENT_ID.into(),
    }
}

fn admission(host_id: Uuid) -> Value {
    json!({
        "version":1,
        "origins":[{"kind":"workflow","serviceId":ORIGIN_SERVICE,"allowedSubjectKinds":["workflow-task"]}],
        "enrollments":[{
            "enrollmentId":ENROLLMENT_ID,"runnerId":RUNNER_ID,"authenticatedSubject":RUNNER_SUBJECT,
            "hostId":host_id,"binaryDigest":BINARY_DIGEST,"effectiveConfigDigest":CONFIG_DIGEST,
            "commandAllowlistDigest":ALLOWLIST_DIGEST,"maximumConcurrency":1,"heartbeatIntervalMs":25,
            "backends":[{"backendId":"cube","boundary":"micro-vm","hostExposure":"none",
                "actions":["run.shell","coding.fixture","coding.pi-rpc-v1"],
                "features":["deny-all-egress","native-ttl","bounded-metadata-recovery","immutable-repository-upload","canonical-patch-output","pi-rpc-v1","bounded-tag-discovery"],
                "compatibilityDigest":COMPATIBILITY,"maximumSlots":1}]
        }]
    })
}

fn runner_config(
    host_id: Uuid,
    root: &std::path::Path,
    token_path: std::path::PathBuf,
    address: std::net::SocketAddr,
) -> RunnerConfig {
    RunnerConfig {
        runner_id: RUNNER_ID.into(),
        enrollment_id: ENROLLMENT_ID.into(),
        host_id,
        controller_url: format!("ws://{address}/ws/runner"),
        jwt_file: token_path,
        data_directory: root.into(),
        health_address: "127.0.0.1:0".parse().unwrap(),
        maximum_concurrency: 1,
        heartbeat_interval: Duration::from_millis(25),
        reconnect_maximum: Duration::from_millis(100),
        shutdown_grace: Duration::from_secs(1),
        staging_maximum_bytes: 1024,
        orphan_reconcile_interval: Duration::from_secs(60),
        orphan_reconcile_startup_timeout: Duration::from_secs(5),
        backend: RunnerBackendConfig::Cube(CubeRunnerBackendConfig {
            implementation: CubeImplementation::Cube,
            api_url: "https://unused.invalid/".into(),
            sandbox_url: None,
            api_key_file: root.join("unused-cube-key"),
            tls_ca_file: None,
            allow_insecure_http: false,
            template_id: "full-stack-template".into(),
            compatibility_digest: COMPATIBILITY.into(),
            available_slots: 1,
            maximum_native_ttl_seconds: 300,
            discovery_page_limit: 100,
            request_timeout_ms: 1000,
            maximum_response_bytes: 1024,
        }),
        allowed_command_template_digests: BTreeSet::from([TEMPLATE_DIGEST.into()]),
        effective_config_digest: CONFIG_DIGEST.into(),
        command_allowlist_digest: ALLOWLIST_DIGEST.into(),
        binary_digest: BINARY_DIGEST.into(),
        agent_worker: None,
    }
}

async fn insert_request(pool: &PgPool, host_id: Uuid, workflow_definition_id: Uuid) -> Uuid {
    let policy_id = Uuid::now_v7();
    let policy_digest = format!("{:064x}", Uuid::now_v7().as_u128());
    sqlx::query("INSERT INTO workflow_execution_policy_t(policy_snapshot_id,host_id,definition_digest,profile_id,profile_version,resolved_policy,policy_digest,source,created_by) VALUES($1,$2,$3,'full-stack',1,'{}'::jsonb,$3,'test','test')")
        .bind(policy_id).bind(host_id).bind(&policy_digest).execute(pool).await.unwrap();
    let request_id = Uuid::now_v7();
    let process_id = Uuid::now_v7();
    let task_id = Uuid::now_v7();
    let workflow_instance = format!("runner-cube-full-stack-{request_id}");
    sqlx::query("INSERT INTO process_info_t(host_id,process_id,wf_def_id,wf_instance_id,app_id,process_type,status_code,ex_trigger_ts) VALUES($1,$2,$3,$4,'runner-cube-full-stack','integration','A',CURRENT_TIMESTAMP)")
        .bind(host_id).bind(process_id).bind(workflow_definition_id).bind(&workflow_instance)
        .execute(pool).await.unwrap();
    sqlx::query("INSERT INTO task_info_t(host_id,task_id,task_type,process_id,wf_instance_id,wf_task_id,status_code,locked,priority) VALUES($1,$2,'integration',$3,$4,'runner-cube-full-stack','A','N',100)")
        .bind(host_id).bind(task_id).bind(process_id).bind(&workflow_instance)
        .execute(pool).await.unwrap();
    let requirements = ExecutionRequirements {
        action_kind: "run.shell".into(),
        minimum_boundary: IsolationBoundary::MicroVm,
        maximum_host_exposure: HostExposure::None,
        network_enabled: false,
        credential_classes: vec![],
        persistent_workspace: false,
        required_features: vec!["deny-all-egress".into()],
        policy_digest: policy_digest.clone(),
        compatibility_digest: COMPATIBILITY.into(),
    };
    let command = CommandExecutionSpec {
        schema_version: 1,
        template_id: "full-stack".into(),
        template_version: 1,
        template_digest: TEMPLATE_DIGEST.into(),
        executable: "/usr/bin/printf".into(),
        arguments: vec!["full-stack-ok".into()],
        working_directory: "/workspace".into(),
        environment: BTreeMap::new(),
        wall_clock_timeout_ms: 5_000,
        stdout_limit_bytes: 1024,
        stderr_limit_bytes: 1024,
        network_enabled: false,
        credentials_enabled: false,
        persistent_workspace: false,
    };
    sqlx::query("INSERT INTO runner_scheduling_request_t(host_id,request_id,idempotency_key,origin_kind,origin_service_id,origin_instance_id,subject_kind,subject_id,process_id,task_id,policy_snapshot_id,policy_digest,normalized_requirements,execution_spec,fairness_key,priority,state) VALUES($1,$2,$3,'workflow',$4,$5,'workflow-task',$6,$7,$6,$8,$9,$10,$11,$12,100,'PENDING_CAPACITY')")
        .bind(host_id).bind(request_id).bind(format!("full-stack-{request_id}"))
        .bind(ORIGIN_SERVICE).bind(format!("full-stack-{request_id}"))
        .bind(task_id).bind(process_id).bind(policy_id).bind(&policy_digest)
        .bind(serde_json::to_value(requirements).unwrap()).bind(serde_json::to_value(command).unwrap())
        .bind(format!("full-stack-{request_id}")).execute(pool).await.unwrap();
    request_id
}

async fn cleanup_rows(pool: &PgPool, host_id: Uuid) {
    sqlx::query("DELETE FROM execution_attempt_t WHERE host_id=$1 AND origin_service_id=$2")
        .bind(host_id)
        .bind(ORIGIN_SERVICE)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "DELETE FROM runner_scheduling_request_t WHERE host_id=$1 AND origin_service_id=$2",
    )
    .bind(host_id)
    .bind(ORIGIN_SERVICE)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "DELETE FROM workflow_execution_policy_t WHERE host_id=$1 AND profile_id='full-stack'",
    )
    .bind(host_id)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("DELETE FROM runner_session_t WHERE host_id=$1 AND runner_id=$2")
        .bind(host_id)
        .bind(RUNNER_ID)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM process_info_t WHERE host_id=$1 AND wf_instance_id LIKE 'runner-cube-full-stack-%'")
        .bind(host_id)
        .execute(pool)
        .await
        .unwrap();
}
