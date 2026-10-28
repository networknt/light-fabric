//! Real application Init/service wiring only. Scripted typed IPC replies do not
//! qualify precision semantics, process resources, recovery or engine admission.
use async_trait::async_trait;
use light_lambda_runtime::{
    init::{self, BackendConfiguration, InitInput},
    validator_ipc::{Engine, Failure, Reply, Response, Session, Settings},
    validator_policy::{FailureDisposition, ValidatorPolicyProvider},
};
use serde_json::{Value, json};
use serverless_core::*;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst},
    },
    time::Duration,
};
use tokio::{sync::Notify, time::Instant};

const SOURCE: &str = r#"{"openapi":"3.1.0","paths":{"/x":{"post":{"requestBody":{"content":{"application/json":{"schema":{"type":"object"}}}}}}}}"#;
const BAD: &str = r#"{"statusCode":400,"code":"ERR11004","message":"VALIDATOR_REQUEST_BODY","description":"scripted IPC violation bytes","severity":"ERROR"}"#;
struct ExplicitTestDisposition;
impl FailureDisposition for ExplicitTestDisposition {
    fn reject(&self, _: Failure) -> StatusError {
        panic!("no infrastructure HTTP mapping is admitted by this wiring test")
    }
}
#[derive(Default)]
struct BackendCounts {
    dispatches: AtomicUsize,
    completions: AtomicUsize,
}
#[async_trait]
impl Backend for BackendCounts {
    async fn dispatch(
        &self,
        _: &Invocation,
        _: &ExchangeState,
        _: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        self.dispatches.fetch_add(1, SeqCst);
        Ok(StatusError::new(200, "TEST", "TEST", "backend").response())
    }
}
impl Finalizer for BackendCounts {
    fn finalize(&self, _: &Invocation, _: &ExchangeState, _: &GatewayResponse) {
        self.completions.fetch_add(1, SeqCst);
    }
}
struct MetadataGate {
    entered: Notify,
    released: Mutex<bool>,
    changed: std::sync::Condvar,
}
impl MetadataGate {
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.changed.notify_all();
    }
}
struct ReleaseMetadata(Arc<MetadataGate>);
impl Drop for ReleaseMetadata {
    fn drop(&mut self) {
        self.0.release();
    }
}

struct Script {
    init_scope: Mutex<Option<Arc<light_lambda_runtime::validator_ipc::ownership::Drain>>>,
    metadata_gate: Mutex<Option<Arc<MetadataGate>>>,
    admission_blocked: AtomicBool,
    admission_attempts: AtomicUsize,
    reservations: AtomicUsize,
    metadata: std::sync::OnceLock<String>,
    replies: Mutex<VecDeque<bool>>,
    events: Mutex<Vec<Value>>,
    pending: AtomicBool,
    dropped: AtomicUsize,
}
#[async_trait]
impl Session for Script {
    fn initialization_scope(
        &self,
    ) -> Option<light_lambda_runtime::validator_ipc::ownership::Initialization> {
        self.init_scope
            .lock()
            .unwrap()
            .as_ref()
            .map(|d| light_lambda_runtime::validator_ipc::ownership::Initialization::new(d.clone()))
    }
    fn admit_request(
        &self,
    ) -> Result<light_lambda_runtime::validator_ipc::controller::Admission, Failure> {
        self.admission_attempts.fetch_add(1, SeqCst);
        if self.admission_blocked.load(SeqCst) {
            return Err(Failure::NoReady);
        }
        Ok(light_lambda_runtime::validator_ipc::controller::Admission::legacy())
    }
    fn reserve_request(
        &self,
        _: usize,
    ) -> Result<Option<light_lambda_runtime::validator_ipc::controller::Reservation>, Failure> {
        self.reservations.fetch_add(1, SeqCst);
        Ok(None)
    }
    fn metadata_json(&self) -> Option<&str> {
        if let Some(gate) = self.metadata_gate.lock().unwrap().clone() {
            gate.entered.notify_one();
            let mut released = gate.released.lock().unwrap();
            while !*released {
                released = gate.changed.wait(released).unwrap();
            }
        }
        self.metadata.get().map(String::as_str)
    }
    async fn request(&self, event: Value, _: Instant) -> Result<Response, Failure> {
        self.events.lock().unwrap().push(event);
        if self.pending.swap(false, SeqCst) {
            struct DropWitness<'a>(&'a AtomicUsize);
            impl Drop for DropWitness<'_> {
                fn drop(&mut self) {
                    self.0.fetch_add(1, SeqCst);
                }
            }
            let _witness = DropWitness(&self.dropped);
            std::future::pending::<()>().await;
        }
        let success = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected extra validation");
        let wire = if success {
            json!({"status":200,"body":"ok"})
        } else {
            json!({"status":400,"body":BAD,"violation":{"code":"ERR11004","message":"VALIDATOR_REQUEST_BODY","description":"scripted IPC violation bytes"}})
        };
        Ok(Response::Decoded(Reply::decode(
            &serde_json::to_vec(&wire).unwrap(),
        )?))
    }
}
struct Compiler {
    preparation_gate: Mutex<Option<Arc<light_lambda_runtime::init::PreparationGate>>>,
    initialization_thread: Mutex<Option<std::thread::ThreadId>>,
    expected_source: Mutex<Option<Vec<u8>>>,
    settings: Mutex<Vec<Settings>>,
    session: Arc<Script>,
    starts: AtomicUsize,
    captured: Mutex<Vec<Arc<[u8]>>>,
    block: AtomicBool,
    entered: Notify,
    release: Notify,
    fail: bool,
    mutate: Mutex<Option<std::path::PathBuf>>,
}
#[async_trait]
impl Engine for Compiler {
    fn configuration_preparation(
        &self,
    ) -> Option<Arc<light_lambda_runtime::init::PreparationGate>> {
        self.preparation_gate.lock().unwrap().clone()
    }
    async fn initialize(
        &self,
        source: Arc<[u8]>,
        format: gateway_policy::openapi_operation::SourceFormat,
        settings: Settings,
        _: Instant,
    ) -> Result<Arc<dyn Session>, Failure> {
        *self.initialization_thread.lock().unwrap() = Some(std::thread::current().id());
        assert!(matches!(
            format,
            gateway_policy::openapi_operation::SourceFormat::Json
        ));
        {
            let expected = self.expected_source.lock().unwrap();
            assert_eq!(&*source, expected.as_deref().unwrap_or(SOURCE.as_bytes()));
        }
        self.starts.fetch_add(1, SeqCst);
        self.settings.lock().unwrap().push(settings);
        self.captured.lock().unwrap().push(source);
        if let Some(path) = self.mutate.lock().unwrap().take() {
            std::fs::write(path, "changed-after-capture: [").unwrap();
        }
        self.entered.notify_one();
        if self.block.load(SeqCst) {
            self.release.notified().await;
        }
        if self.fail {
            return Err(Failure::Protocol);
        }
        Ok(self.session.clone())
    }
}
fn compiler(replies: &[bool], fail: bool) -> Arc<Compiler> {
    Arc::new(Compiler {
        preparation_gate: Mutex::new(None),
        initialization_thread: Mutex::new(None),
        expected_source: Mutex::new(None),
        settings: Mutex::new(vec![]),
        session: Arc::new(Script {
            init_scope: Mutex::new(None),
            metadata_gate: Mutex::new(None),
            admission_blocked: AtomicBool::new(false),
            admission_attempts: AtomicUsize::new(0),
            reservations: AtomicUsize::new(0),
            metadata: std::sync::OnceLock::new(),
            replies: Mutex::new(replies.iter().copied().collect()),
            events: Mutex::new(vec![]),
            pending: AtomicBool::new(false),
            dropped: AtomicUsize::new(0),
        }),
        starts: AtomicUsize::new(0),
        captured: Mutex::new(vec![]),
        block: AtomicBool::new(false),
        entered: Notify::new(),
        release: Notify::new(),
        fail,
        mutate: Mutex::new(None),
    })
}
fn input(compiler: Option<Arc<Compiler>>) -> (tempfile::TempDir, InitInput, Arc<BackendCounts>) {
    let dir = tempfile::tempdir().unwrap();
    for (name,value) in [
        ("values.yml", "light-lambda.stage: test-only\nlight-lambda.ingressTimeoutMs: 1000\nlight-lambda.responseReserveMs: 50\nlight-lambda.postReserveMs: 20\nlambda-proxy.region: us-east-1\nlambda-proxy.metricsInjection: false\n".to_owned()),
        ("openapi-handler.yml", "enabled: true\n".into()),
        ("openapi-validator.yml", "enabled: true\n".into()),
        ("openapi.json", SOURCE.into()),
        ("lambda-proxy.yml", "functions: {'/x@post': TestFn}\nregion: us-east-1\nmetricsInjection: false\n".into()),
    ] { std::fs::write(dir.path().join(name),value).unwrap(); }
    // Class authority comes from production compatibility, not test spellings.
    let class = |id| {
        compat::COMPATIBILITY
            .iter()
            .find(|c| c.id == id)
            .unwrap()
            .class
    };
    std::fs::write(dir.path().join("handler.yml"),json!({"handlers":[format!("{}@validator",class(HandlerId::Validator)),format!("{}@spec",class(HandlerId::Specification)),format!("{}@proxy",class(HandlerId::Proxy))],"defaultHandlers":["spec","validator","proxy"]}).to_string()).unwrap();
    let mut input = light_lambda::input(Some(dir.path().into()), false).unwrap();
    if let Some(compiler) = compiler {
        input
            .handlers
            .iter_mut()
            .find(|h| h.id == HandlerId::Validator)
            .unwrap()
            .factory = Some(Arc::new(ValidatorPolicyProvider::with_engine(
            compiler,
            Arc::new(ExplicitTestDisposition),
            Duration::from_secs(2),
        )));
    }
    input.handlers.reverse(); // Producer capture must not depend on registration order.
    let counts = Arc::new(BackendCounts::default());
    input.backend = BackendConfiguration::Injected(counts.clone());
    input.finalizers = vec![counts.clone()];
    (dir, input, counts)
}
async fn event(snapshot: Arc<init::Snapshot>, name: &str, headers: Value, ms: u64) -> Value {
    let mut context = lambda_runtime_context(name, ms);
    context.request_id = name.into();
    light_lambda_runtime::service::handle(snapshot,lambda_runtime::LambdaEvent::new(json!({"path":"/x","httpMethod":"POST","multiValueHeaders":headers,"body":"{\"n\":1e400}","isBase64Encoded":false,"multiValueQueryStringParameters":{"q":["first","second"]},"requestContext":{"elb":{"targetGroupArn":"test-only"}}}),context)).await.unwrap()
}
fn lambda_runtime_context(name: &str, ms: u64) -> lambda_runtime::Context {
    let mut c = lambda_runtime::Context::default();
    c.request_id = name.into();
    c.deadline = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + ms;
    c
}

#[tokio::test]
async fn default_application_registration_stays_admission_closed() {
    let (_dir, input, counts) = input(None);
    assert!(
        input
            .handlers
            .iter()
            .find(|r| r.id == HandlerId::Validator)
            .unwrap()
            .factory
            .is_some()
    );
    let report = init::validate_configuration(&input).await;
    assert!(!report.valid);
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.contains("enabled handler not supported"))
    );
    assert!(init::initialize(input).await.is_err());
    assert_eq!(counts.dispatches.load(SeqCst), 0);
}
#[tokio::test]
async fn real_application_factory_activates_once_then_service_preserves_ipc_bytes_and_capture() {
    let engine = compiler(&[true, false, true], false);
    let (dir, input, counts) = input(Some(engine.clone()));
    let report = init::validate_configuration(&input).await;
    assert!(report.valid, "{:?}", report.errors);
    assert_eq!(
        engine.starts.load(SeqCst),
        0,
        "offline preparation must not launch the engine"
    );
    *engine.mutate.lock().unwrap() = Some(dir.path().join("openapi.json"));
    engine.block.store(true, SeqCst);
    let task = tokio::spawn(init::initialize(input));
    engine.entered.notified().await;
    assert!(
        !task.is_finished(),
        "Snapshot published before complete activation"
    );
    assert_eq!(counts.dispatches.load(SeqCst), 0);
    engine.release.notify_one();
    let snapshot = task.await.unwrap().unwrap();
    let digest = snapshot.config_digest().to_owned();
    assert_eq!(
        event(
            snapshot.clone(),
            "one",
            json!({"Content-Type":["application/json; charset=utf-8"]}),
            1000
        )
        .await["statusCode"],
        200
    );
    let invalid = event(
        snapshot.clone(),
        "two",
        json!({"content-type":["text/plain"]}),
        1000,
    )
    .await;
    assert_eq!(invalid["statusCode"], 400);
    assert_eq!(invalid["body"], BAD);
    assert_eq!(
        event(snapshot.clone(), "three", json!({}), 1000).await["statusCode"],
        200
    );
    assert_eq!(snapshot.config_digest(), digest);
    assert_eq!(engine.starts.load(SeqCst), 1);
    assert_eq!(counts.dispatches.load(SeqCst), 2);
    assert_eq!(counts.completions.load(SeqCst), 3);
    let events = engine.session.events.lock().unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0]["body"], "{\"n\":1e400}");
    assert_eq!(
        events[0]["headers"]["Content-Type"],
        "application/json; charset=utf-8"
    );
    assert_eq!(events[1]["headers"]["Content-Type"], "text/plain");
    assert!(events[1]["headers"].get("content-type").is_none());
    assert_eq!(events[0]["queryStringParameters"]["q"], "first");
}
#[tokio::test]
async fn activation_failure_never_publishes_or_dispatches() {
    let engine = compiler(&[], true);
    let (_dir, input, counts) = input(Some(engine.clone()));
    let error = init::initialize(input).await.err().unwrap();
    assert!(format!("{error:?}").contains("precision engine activation failed"));
    assert_eq!(engine.starts.load(SeqCst), 1);
    assert_eq!(engine.session.events.lock().unwrap().len(), 0);
    assert_eq!(counts.dispatches.load(SeqCst), 0);
}
#[tokio::test]
async fn request_deadline_drops_async_ipc_future_and_next_warm_request_works() {
    let engine = compiler(&[true], false);
    engine.session.pending.store(true, SeqCst);
    let (_dir, input, counts) = input(Some(engine.clone()));
    let snapshot = init::initialize(input).await.unwrap();
    assert_eq!(
        event(snapshot.clone(), "timeout", json!({}), 200).await["statusCode"],
        504
    );
    assert_eq!(engine.session.dropped.load(SeqCst), 1);
    assert_eq!(counts.dispatches.load(SeqCst), 0);
    assert_eq!(
        event(snapshot, "warm", json!({}), 1000).await["statusCode"],
        200
    );
    assert_eq!(counts.dispatches.load(SeqCst), 1);
    assert_eq!(counts.completions.load(SeqCst), 2);
}
#[tokio::test]
async fn malformed_controller_settings_reject_before_activation() {
    for extra in [
        "skipBodyValidation: wrong",
        "validateResponse: true",
        "skipPathPrefixes: ['relative']",
    ] {
        let engine = compiler(&[], false);
        let (dir, input, _counts) = input(Some(engine.clone()));
        std::fs::write(
            dir.path().join("openapi-validator.yml"),
            format!("enabled: true\n{extra}\n"),
        )
        .unwrap();
        assert!(init::initialize(input).await.is_err());
        assert_eq!(engine.starts.load(SeqCst), 0);
    }
}
#[test]
fn existing_precision_reply_contract_keeps_infrastructure_failures_typed() {
    for (tag, expected) in [
        ("engineResource", Failure::EngineResource),
        ("engineInternal", Failure::EngineInternal),
        ("adapterInvariant", Failure::AdapterInvariant),
    ] {
        assert_eq!(
            Reply::decode(&serde_json::to_vec(&json!({"failure":tag})).unwrap()).err(),
            Some(expected)
        );
    }
    assert_eq!(
        Reply::decode(br#"{"status":200,"body":"changed"}"#).err(),
        Some(Failure::Protocol)
    );
    assert_eq!(
        Reply::decode(br#"{"status":400,"body":"not json"}"#).err(),
        Some(Failure::Protocol)
    );
}

#[tokio::test]
async fn configured_settings_are_captured_for_worker_init_not_reimplemented_by_parent() {
    let engine = compiler(&[true], false);
    let (dir, input, _) = input(Some(engine.clone()));
    std::fs::write(dir.path().join("openapi-validator.yml"),
        "enabled: true\nskipBodyValidation: true\nhandleNullableField: false\nskipPathPrefixes: ['/x']\n").unwrap();
    let snapshot = init::initialize(input).await.unwrap();
    std::fs::write(dir.path().join("openapi-validator.yml"), "changed: [").unwrap();
    assert_eq!(
        event(snapshot, "settings", json!({}), 1000).await["statusCode"],
        200
    );
    let settings = engine.settings.lock().unwrap();
    assert_eq!(settings.len(), 1);
    assert_eq!(
        serde_json::to_value(&settings[0]).unwrap(),
        json!({"skipBodyValidation":true,"handleNullableField":false,"skipPathPrefixes":["/x"]})
    );
    assert_eq!(
        engine.session.events.lock().unwrap().len(),
        1,
        "parent must delegate configured skips to the worker"
    );
    assert_eq!(
        serde_json::to_value(Settings::default()).unwrap(),
        json!({}),
        "legacy constructors retain absent defaults"
    );
}

#[tokio::test]
async fn runtime_precision_projection_precedes_numeric_parsing_and_reuses_one_session() {
    // This proves plumbing with a supplied projection, never real-child parity.
    let raw = SOURCE.replace(
        "\"type\":\"object\"",
        "\"type\":\"object\",\"maximum\":1e400",
    );
    let projected = SOURCE.replace("\"type\":\"object\"", "\"type\":\"object\",\"maximum\":0");
    let engine = compiler(&[true, false], false);
    *engine.expected_source.lock().unwrap() = Some(raw.as_bytes().to_vec());
    engine.session.metadata.set(projected).unwrap();
    let (dir, _prior, counts) = input(None);
    std::fs::write(dir.path().join("openapi.json"), &raw).unwrap();
    *engine.mutate.lock().unwrap() = Some(dir.path().join("openapi.json"));
    let mut prepared = light_lambda::input_with_precision_engine(
        Some(dir.path().into()),
        false,
        engine.clone(),
        Duration::from_secs(2),
    )
    .unwrap();
    prepared.backend = BackendConfiguration::Injected(counts.clone());
    prepared.finalizers = vec![counts.clone()];
    prepared.handlers.reverse();
    let report = init::validate_configuration(&prepared).await;
    assert!(
        !report.valid,
        "offline precision preparation is explicitly pending runtime"
    );
    assert_eq!(engine.starts.load(SeqCst), 0);
    let snapshot = init::initialize(prepared).await.unwrap();
    assert_eq!(
        event(snapshot.clone(), "projected-valid", json!({}), 1000).await["statusCode"],
        200
    );
    assert_eq!(
        event(snapshot, "projected-invalid", json!({}), 1000).await["body"],
        BAD
    );
    assert_eq!(
        engine.starts.load(SeqCst),
        1,
        "consumer must not recompile or reopen"
    );
    assert_eq!(counts.dispatches.load(SeqCst), 1);
    assert_eq!(engine.captured.lock().unwrap()[0].as_ref(), raw.as_bytes());
}

#[tokio::test]
async fn runtime_precision_projection_missing_or_malformed_never_publishes() {
    for metadata in [None, Some("invalid projection".to_owned())] {
        let engine = compiler(&[], false);
        if let Some(metadata) = metadata {
            engine.session.metadata.set(metadata).unwrap();
        }
        let (dir, _prior, counts) = input(None);
        let mut prepared = light_lambda::input_with_precision_engine(
            Some(dir.path().into()),
            false,
            engine.clone(),
            Duration::from_secs(2),
        )
        .unwrap();
        prepared.backend = BackendConfiguration::Injected(counts.clone());
        assert!(init::initialize(prepared).await.is_err());
        assert_eq!(counts.dispatches.load(SeqCst), 0);
    }
}

// Non-child path/credit regressions. The scripted replies below prove the real
// Init/service projection and forwarding boundary, not schema semantics.
const PATH_SOURCE: &str = r#"{"openapi":"3.1.0","servers":[{"url":"https://test.invalid/v1"}],"paths":{"/pets":{"post":{}},"/pets/{petId}":{"get":{}},"/0/pets":{"post":{}},"/x/pets":{"post":{}},"/v2/pets":{"post":{}}}}"#;

struct ProjectionSession {
    script: Arc<Script>,
    budget: Arc<light_lambda_runtime::validator_ipc::controller::Budget>,
    limit: AtomicUsize,
    requested: Mutex<Vec<usize>>,
    held_during_request: Mutex<Vec<usize>>,
}
#[async_trait]
impl Session for ProjectionSession {
    fn reserve_request(
        &self,
        bytes: usize,
    ) -> Result<Option<light_lambda_runtime::validator_ipc::controller::Reservation>, Failure> {
        self.requested.lock().unwrap().push(bytes);
        if bytes > self.limit.load(SeqCst) {
            return Err(Failure::ParentBudget);
        }
        self.budget.reservation(bytes).map(Some)
    }
    async fn request(&self, event: Value, deadline: Instant) -> Result<Response, Failure> {
        self.held_during_request
            .lock()
            .unwrap()
            .push(self.budget.used());
        self.script.request(event, deadline).await
    }
}
struct ProjectionEngine {
    compiler: Arc<Compiler>,
    session: Arc<ProjectionSession>,
}
#[async_trait]
impl Engine for ProjectionEngine {
    async fn initialize(
        &self,
        source: Arc<[u8]>,
        format: gateway_policy::openapi_operation::SourceFormat,
        settings: Settings,
        deadline: Instant,
    ) -> Result<Arc<dyn Session>, Failure> {
        self.compiler
            .initialize(source, format, settings, deadline)
            .await?;
        Ok(self.session.clone())
    }
}
#[derive(Default)]
struct ProjectionBackend(Mutex<Vec<Value>>);
#[async_trait]
impl Backend for ProjectionBackend {
    async fn dispatch(
        &self,
        inv: &Invocation,
        _: &ExchangeState,
        _: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        self.0.lock().unwrap().push(json!({
            "path":inv.path,"rawPath":inv.raw_path,"query":inv.query.0,
            "pathParameters":inv.path_params,
            "body":inv.body.as_ref().map(|b|std::str::from_utf8(b).unwrap()),
            "projectionHeader":inv.headers.get("x-projection").map(|v|v.to_str().unwrap()),
            "originalPath":inv.original["path"],
        }));
        Ok(StatusError::new(200, "TEST", "TEST", "backend").response())
    }
}
fn projection_input(
    replies: &[bool],
) -> (
    tempfile::TempDir,
    InitInput,
    Arc<ProjectionSession>,
    Arc<ProjectionBackend>,
) {
    let compiler = compiler(replies, false);
    *compiler.expected_source.lock().unwrap() = Some(PATH_SOURCE.as_bytes().to_vec());
    let (dir, mut input, _) = input(Some(compiler.clone()));
    std::fs::write(dir.path().join("openapi.json"), PATH_SOURCE).unwrap();
    std::fs::write(
        dir.path().join("lambda-proxy.yml"),
        "functions: {'/*@post': TestFn, '/*@get': TestFn}\nregion: us-east-1\nmetricsInjection: false\n",
    )
    .unwrap();
    let session = Arc::new(ProjectionSession {
        script: compiler.session.clone(),
        budget: light_lambda_runtime::validator_ipc::controller::Budget::new(32 << 20),
        limit: AtomicUsize::new(usize::MAX),
        requested: Mutex::new(vec![]),
        held_during_request: Mutex::new(vec![]),
    });
    input
        .handlers
        .iter_mut()
        .find(|h| h.id == HandlerId::Validator)
        .unwrap()
        .factory = Some(Arc::new(ValidatorPolicyProvider::with_engine(
        Arc::new(ProjectionEngine {
            compiler,
            session: session.clone(),
        }),
        Arc::new(light_lambda_runtime::validator_failure_policy::ValidatorFailureDisposition),
        Duration::from_secs(2),
    )));
    let backend = Arc::new(ProjectionBackend::default());
    input.backend = BackendConfiguration::Injected(backend.clone());
    (dir, input, session, backend)
}
async fn projection_event(
    snapshot: Arc<init::Snapshot>,
    path: &str,
    method: &str,
    body: &str,
    marker: &str,
) -> Value {
    light_lambda_runtime::service::handle(
        snapshot,
        lambda_runtime::LambdaEvent::new(
            json!({"path":path,"httpMethod":method,"body":body,"isBase64Encoded":false,
                "multiValueHeaders":{"Content-Type":["application/json"],"x-projection":[marker]},
                "multiValueQueryStringParameters":{"tag":["first","second"]},
                "requestContext":{"elb":{"targetGroupArn":"test-only"}}}),
            lambda_runtime_context("projection", 2000),
        ),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn captured_base_path_projects_ipc_only_and_preserves_warm_forwarding() {
    let (dir, input, session, backend) = projection_input(&[true, false, true]);
    let snapshot = init::initialize(input).await.unwrap();
    // Both policies use captured resources, never reopen the changed source.
    std::fs::write(dir.path().join("openapi.json"), SOURCE).unwrap();
    for (body, marker, status) in [
        (r#"{"name":"Tucker"}"#, "first", 200),
        ("{}", "rejected", 400),
        (r#"{"name":"Milo"}"#, "warm", 200),
    ] {
        assert_eq!(
            projection_event(snapshot.clone(), "/v1/pets", "POST", body, marker).await["statusCode"],
            status
        );
    }
    let events = session.script.events.lock().unwrap();
    assert_eq!(events.len(), 3);
    for e in events.iter() {
        assert_eq!(e["path"], "/pets");
        assert_eq!(e["httpMethod"], "POST");
        assert_eq!(e["queryStringParameters"], json!({"tag":"first"}));
        assert_eq!(e["headers"]["Content-Type"], "application/json");
    }
    assert_eq!(events[1]["body"], "{}");
    assert_eq!(events[2]["headers"]["x-projection"], "warm");
    let forwarded = backend.0.lock().unwrap();
    assert_eq!(forwarded.len(), 2);
    for e in forwarded.iter() {
        assert_eq!(e["path"], "/v1/pets");
        assert_eq!(e["rawPath"], "/v1/pets");
        assert_eq!(e["originalPath"], "/v1/pets");
        assert_eq!(e["query"], json!([["tag", "first"], ["tag", "second"]]));
    }
    assert_eq!(forwarded[0]["body"], r#"{"name":"Tucker"}"#);
    assert_eq!(forwarded[1]["body"], r#"{"name":"Milo"}"#);
    assert_eq!(forwarded[1]["projectionHeader"], "warm");
    // Proxy adds its wildcard capture after Validator has executed.
    assert_eq!(events[0]["pathParameters"], json!({}));
    assert_eq!(forwarded[0]["pathParameters"], json!({"*":"v1/pets"}));
    assert_eq!(session.budget.used(), 0);
    assert!(
        session
            .held_during_request
            .lock()
            .unwrap()
            .iter()
            .all(|b| *b > 0)
    );
}

#[tokio::test]
async fn parameterized_and_unprefixed_ipc_paths_keep_actual_values() {
    let (_dir, input, session, backend) = projection_input(&[true, true]);
    let snapshot = init::initialize(input).await.unwrap();
    for path in ["/v1/pets/17", "/pets/23"] {
        assert_eq!(
            projection_event(snapshot.clone(), path, "GET", "", "parameter").await["statusCode"],
            200
        );
    }
    let events = session.script.events.lock().unwrap();
    assert_eq!(events[0]["path"], "/pets/17");
    assert_eq!(events[1]["path"], "/pets/23");
    let forwarded = backend.0.lock().unwrap();
    assert_eq!(forwarded[0]["path"], "/v1/pets/17");
    assert_eq!(forwarded[1]["path"], "/pets/23");
    for i in 0..2 {
        assert_eq!(events[i]["pathParameters"], json!({}));
        assert_eq!(
            forwarded[i]["pathParameters"],
            json!({"*": forwarded[i]["path"].as_str().unwrap().trim_start_matches('/')})
        );
    }
}

#[tokio::test]
async fn ipc_reuses_approved_first_match_profile_without_rewriting_forwarding() {
    let (_dir, input, session, backend) = projection_input(&[true, true, true]);
    let snapshot = init::initialize(input).await.unwrap();
    for path in ["/v10/pets", "/x/v1/pets", "/v2/pets"] {
        assert_eq!(
            projection_event(snapshot.clone(), path, "POST", "{}", "profile").await["statusCode"],
            200
        );
    }
    let events = session.script.events.lock().unwrap();
    let paths = events
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(paths, ["/0/pets", "/x/pets", "/v2/pets"]);
    let forwarded = backend.0.lock().unwrap();
    let paths = forwarded
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(paths, ["/v10/pets", "/x/v1/pets", "/v2/pets"]);
}

#[tokio::test]
async fn projection_credit_boundary_rejects_before_ipc_and_recovers_warm() {
    let (_dir, input, session, backend) = projection_input(&[true, true]);
    let snapshot = init::initialize(input).await.unwrap();
    assert_eq!(
        projection_event(snapshot.clone(), "/v1/pets", "POST", "{}", "budget").await["statusCode"],
        200
    );
    let bound = session.requested.lock().unwrap()[0];
    let event = session.script.events.lock().unwrap()[0].clone();
    let headers = event["headers"].as_object().unwrap();
    let prior_bytes = 2
        + "/v1/pets".len()
        + headers
            .iter()
            .map(|(k, v)| k.len() + v.as_str().unwrap().len())
            .sum::<usize>()
        + "tag".len() * 2
        + "first".len()
        + "second".len();
    let prior = light_lambda_runtime::validator_ipc::accounting::request_bound(
        prior_bytes,
        headers.len() + 2,
    )
    .unwrap();
    assert_eq!(bound - prior, 16 * ("/v1/pets".len() + 1));
    session.limit.store(bound - 1, SeqCst);
    assert_eq!(
        projection_event(snapshot.clone(), "/v1/pets", "POST", "{}", "budget").await["statusCode"],
        503
    );
    assert_eq!(session.script.events.lock().unwrap().len(), 1);
    assert_eq!(backend.0.lock().unwrap().len(), 1);
    assert_eq!(session.budget.used(), 0);
    session.limit.store(bound, SeqCst);
    assert_eq!(
        projection_event(snapshot, "/v1/pets", "POST", "{}", "budget").await["statusCode"],
        200
    );
    assert_eq!(backend.0.lock().unwrap().len(), 2);
    assert_eq!(session.script.events.lock().unwrap().len(), 2);
    assert_eq!(session.budget.used(), 0);
}

#[tokio::test]
async fn admission_rejection_precedes_body_projection_and_dispatch_then_warm_recovers() {
    let engine = compiler(&[true], false);
    engine.session.admission_blocked.store(true, SeqCst);
    let (_dir, mut input, counts) = input(Some(engine.clone()));
    input
        .handlers
        .iter_mut()
        .find(|h| h.id == HandlerId::Validator)
        .unwrap()
        .factory = Some(Arc::new(ValidatorPolicyProvider::with_engine(
        engine.clone(),
        Arc::new(light_lambda_runtime::validator_failure_policy::ValidatorFailureDisposition),
        Duration::from_secs(2),
    )));
    let snapshot = init::initialize(input).await.unwrap();
    // If owned body projection were attempted, these invalid UTF-8 bytes would
    // trigger its conversion failure. Admission must win before reserve_request.
    let rejected=light_lambda_runtime::service::handle(snapshot.clone(),lambda_runtime::LambdaEvent::new(
        json!({"path":"/x","httpMethod":"POST","multiValueHeaders":{"Content-Type":["application/json"]},
          "body":"/w==","isBase64Encoded":true,"requestContext":{"elb":{"targetGroupArn":"synthetic"}}}),
        lambda_runtime_context("admission-rejection",1000))).await.unwrap();
    assert_eq!(rejected["statusCode"], 503);
    let body: Value = serde_json::from_str(rejected["body"].as_str().unwrap()).unwrap();
    assert_eq!(body["code"], "ERR14034");
    assert_eq!(engine.session.admission_attempts.load(SeqCst), 1);
    assert_eq!(engine.session.reservations.load(SeqCst), 0);
    assert!(engine.session.events.lock().unwrap().is_empty());
    assert_eq!(counts.dispatches.load(SeqCst), 0);
    engine.session.admission_blocked.store(false, SeqCst);
    assert_eq!(
        event(snapshot, "warm-after-admission", json!({}), 1000).await["statusCode"],
        200
    );
    assert_eq!(engine.session.admission_attempts.load(SeqCst), 2);
    assert_eq!(engine.session.reservations.load(SeqCst), 1);
    assert_eq!(engine.session.events.lock().unwrap().len(), 1);
    assert_eq!(counts.dispatches.load(SeqCst), 1);
    assert_eq!(counts.completions.load(SeqCst), 2);
}
#[tokio::test]
async fn offline_supervised_validation_does_not_open_pin_or_spawn() {
    use light_lambda_runtime::validator_ipc::{SupervisedStaticEngine, supervision::Policy};
    let (_dir, mut input, counts) = input(None);
    input
        .handlers
        .iter_mut()
        .find(|h| h.id == HandlerId::Validator)
        .unwrap()
        .factory = Some(Arc::new(ValidatorPolicyProvider::with_engine(
        Arc::new(SupervisedStaticEngine {
            executable: light_lambda_runtime::bundle_snapshot::Pin {
                path: "/absent-never-opened-offline".into(),
                bytes: 64,
                sha256: [0; 32],
            },
            policy: Policy {
                platform_bytes: 3 << 20,
                parent_requested_bytes: 1 << 20,
                worker_requested_bytes: 1 << 20,
                headroom_bytes: 1 << 20,
                worker_address_space_bytes: 1 << 20,
                lifetime_cpu_seconds: 7,
            },
        }),
        Arc::new(light_lambda_runtime::validator_failure_policy::ValidatorFailureDisposition),
        Duration::from_secs(2),
    )));
    let report = init::validate_configuration(&input).await;
    assert!(report.valid, "{:?}", report.errors);
    assert_eq!(counts.dispatches.load(SeqCst), 0);
}

#[tokio::test]
async fn cancelled_precision_metadata_remains_tracked_until_real_blocking_work_ends() {
    use light_lambda_runtime::validator_ipc::ownership::Drain;
    let engine = compiler(&[], false);
    engine.session.metadata.set(SOURCE.into()).unwrap();
    let drain = Drain::new();
    *engine.session.init_scope.lock().unwrap() = Some(drain.clone());
    let gate = Arc::new(MetadataGate {
        entered: Notify::new(),
        released: Mutex::new(false),
        changed: std::sync::Condvar::new(),
    });
    let _release = ReleaseMetadata(gate.clone());
    *engine.session.metadata_gate.lock().unwrap() = Some(gate.clone());
    let (dir, _prior, counts) = input(None);
    let mut prepared = light_lambda::input_with_precision_engine(
        Some(dir.path().into()),
        false,
        engine.clone(),
        Duration::from_secs(5),
    )
    .unwrap();
    prepared.backend = BackendConfiguration::Injected(counts.clone());
    let task = tokio::spawn(init::initialize(prepared));
    tokio::time::timeout(Duration::from_secs(5), gate.entered.notified())
        .await
        .unwrap();
    assert_eq!(drain.snapshot().metadata, 1);
    task.abort();
    let joined = task.await;
    assert!(matches!(joined,Err(error) if error.is_cancelled()));
    assert!(drain.is_closed());
    assert_eq!(drain.snapshot().metadata, 1);
    assert_eq!(drain.shutdown(Instant::now()).await, Err(Failure::Deadline));
    gate.release();
    assert!(
        drain
            .shutdown(Instant::now() + Duration::from_secs(5))
            .await
            .is_ok()
    );
    assert_eq!(counts.dispatches.load(SeqCst), 0);
    assert!(engine.session.events.lock().unwrap().is_empty());
}
#[tokio::test]
async fn successful_scoped_precision_metadata_publishes_once_and_keeps_epoch_open() {
    use light_lambda_runtime::validator_ipc::ownership::Drain;
    let engine = compiler(&[true], false);
    engine.session.metadata.set(SOURCE.into()).unwrap();
    let drain = Drain::new();
    *engine.session.init_scope.lock().unwrap() = Some(drain.clone());
    let (dir, _prior, counts) = input(None);
    let mut prepared = light_lambda::input_with_precision_engine(
        Some(dir.path().into()),
        false,
        engine.clone(),
        Duration::from_secs(5),
    )
    .unwrap();
    prepared.backend = BackendConfiguration::Injected(counts.clone());
    let snapshot = init::initialize(prepared).await.unwrap();
    assert!(!drain.is_closed());
    assert_eq!(drain.snapshot().metadata, 0);
    assert_eq!(
        event(snapshot, "scoped-metadata-warm", json!({}), 1000).await["statusCode"],
        200
    );
    assert_eq!(counts.dispatches.load(SeqCst), 1);
    assert_eq!(engine.starts.load(SeqCst), 1);
    assert!(
        drain
            .shutdown(Instant::now() + Duration::from_secs(5))
            .await
            .is_ok()
    );
}
#[tokio::test]
async fn failed_scoped_precision_metadata_closes_and_awaits_its_work() {
    use light_lambda_runtime::validator_ipc::ownership::Drain;
    let engine = compiler(&[], false);
    engine
        .session
        .metadata
        .set("invalid projection".into())
        .unwrap();
    let drain = Drain::new();
    *engine.session.init_scope.lock().unwrap() = Some(drain.clone());
    let (dir, _prior, counts) = input(None);
    let mut prepared = light_lambda::input_with_precision_engine(
        Some(dir.path().into()),
        false,
        engine.clone(),
        Duration::from_secs(5),
    )
    .unwrap();
    prepared.backend = BackendConfiguration::Injected(counts.clone());
    assert!(init::initialize(prepared).await.is_err());
    assert!(drain.is_closed());
    assert_eq!(drain.snapshot().metadata, 0);
    assert_eq!(counts.dispatches.load(SeqCst), 0);
}

struct LaterFactory {
    delegate: Arc<dyn init::HandlerFactoryProvider>,
    drain: Arc<light_lambda_runtime::validator_ipc::ownership::Drain>,
    signal: Arc<MetadataGate>,
    pending: Mutex<Option<light_lambda_runtime::validator_ipc::ownership::Work>>,
    hold: bool,
    fail: bool,
    panic: bool,
    block: bool,
}
impl init::HandlerFactoryProvider for LaterFactory {
    fn create(
        &self,
        config: &init::RuntimeConfig,
        security: Option<Arc<light_security::SecurityRuntime>>,
    ) -> Result<
        Arc<dyn serverless_core::plan::HandlerFactory>,
        light_lambda_runtime::init_error::InitError,
    > {
        if self.hold {
            *self.pending.lock().unwrap() = Some(self.drain.metadata().unwrap());
        }
        self.signal.entered.notify_one();
        if self.block {
            let mut released = self.signal.released.lock().unwrap();
            while !*released {
                released = self.signal.changed.wait(released).unwrap();
            }
        }
        assert!(!self.panic, "synthetic later factory panic");
        if self.fail {
            return Err(light_lambda_runtime::init_error::InitError::Preparation);
        }
        self.delegate.create(config, security)
    }
}
fn scoped_engine(
    replies: &[bool],
) -> (
    Arc<Compiler>,
    Arc<init::PreparationGate>,
    Arc<light_lambda_runtime::validator_ipc::ownership::Drain>,
) {
    let engine = compiler(replies, false);
    let gate = init::PreparationGate::new();
    let drain = light_lambda_runtime::validator_ipc::ownership::Drain::new();
    *engine.preparation_gate.lock().unwrap() = Some(gate.clone());
    *engine.session.init_scope.lock().unwrap() = Some(drain.clone());
    engine.session.metadata.set(SOURCE.into()).unwrap();
    (engine, gate, drain)
}
fn later_factory(
    input: &mut InitInput,
    dir: &std::path::Path,
    drain: Arc<light_lambda_runtime::validator_ipc::ownership::Drain>,
    fail: bool,
    panic: bool,
    block: bool,
    hold: bool,
) -> Arc<LaterFactory> {
    std::fs::write(dir.join("correlation.yml"), "enabled: true\n").unwrap();
    let registration = input
        .handlers
        .iter_mut()
        .find(|h| h.id == HandlerId::Correlation)
        .unwrap();
    let later = Arc::new(LaterFactory {
        delegate: registration.factory.take().unwrap(),
        drain,
        signal: Arc::new(MetadataGate {
            entered: Notify::new(),
            released: Mutex::new(false),
            changed: std::sync::Condvar::new(),
        }),
        pending: Mutex::new(None),
        hold,
        fail,
        panic,
        block,
    });
    registration.factory = Some(later.clone());
    later
}
async fn wait_closed(drain: &light_lambda_runtime::validator_ipc::ownership::Drain) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !drain.is_closed() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn scoped_source_preparation_runs_off_runtime_thread_and_warm_service_is_unchanged() {
    let runtime_thread = std::thread::current().id();
    let (engine, gate, _drain) = scoped_engine(&[true]);
    let (dir, _prior, counts) = input(None);
    let mut prepared = light_lambda::input_with_precision_engine(
        Some(dir.path().into()),
        false,
        engine.clone(),
        Duration::from_secs(5),
    )
    .unwrap();
    prepared.backend = BackendConfiguration::Injected(counts.clone());
    let snapshot = init::initialize(prepared).await.unwrap();
    assert_ne!(
        engine.initialization_thread.lock().unwrap().unwrap(),
        runtime_thread
    );
    assert!(!gate.snapshot().active);
    assert!(gate.snapshot().published);
    assert_eq!(gate.snapshot().cleanup, init::PreparationCleanup::Pending);
    assert_eq!(
        event(snapshot.clone(), "owned-source-warm", json!({}), 1000).await["statusCode"],
        200
    );
    assert_eq!(counts.dispatches.load(SeqCst), 1);
    // Reconstructing Init with the same explicit profile cannot renew its epoch.
    let (_other_dir, other, _other_counts) = input(Some(engine.clone()));
    assert!(init::initialize(other).await.is_err());
    assert_eq!(engine.starts.load(SeqCst), 1);
    drop(snapshot);
    assert_eq!(
        gate.settled(Instant::now() + Duration::from_secs(5))
            .await
            .unwrap()
            .cleanup,
        init::PreparationCleanup::Drained
    );
}
#[tokio::test]
async fn later_factory_error_and_panic_await_precision_rollback_before_return() {
    for panic in [false, true] {
        let (engine, gate, drain) = scoped_engine(&[]);
        let (dir, _prior, counts) = input(None);
        let mut prepared = light_lambda::input_with_precision_engine(
            Some(dir.path().into()),
            false,
            engine,
            Duration::from_secs(5),
        )
        .unwrap();
        prepared.backend = BackendConfiguration::Injected(counts.clone());
        let later = later_factory(
            &mut prepared,
            dir.path(),
            drain.clone(),
            !panic,
            panic,
            false,
            true,
        );
        let task = tokio::spawn(init::initialize(prepared));
        tokio::time::timeout(Duration::from_secs(5), later.signal.entered.notified())
            .await
            .unwrap();
        wait_closed(&drain).await;
        assert!(!task.is_finished());
        assert!(gate.snapshot().active);
        assert!(!gate.snapshot().published);
        drop(later.pending.lock().unwrap().take());
        let result = task.await.unwrap();
        assert!(result.is_err());
        if panic {
            assert!(matches!(
                result,
                Err(light_lambda_runtime::init_error::InitError::Panic)
            ));
        } else {
            assert!(matches!(
                result,
                Err(light_lambda_runtime::init_error::InitError::Preparation)
            ));
        }
        assert_eq!(
            gate.settled(Instant::now() + Duration::from_secs(5))
                .await
                .unwrap()
                .cleanup,
            init::PreparationCleanup::Drained
        );
        assert_eq!(counts.dispatches.load(SeqCst), 0);
    }
}
#[tokio::test]
async fn cancelling_caller_during_later_factory_rollback_does_not_cancel_cleanup() {
    let (engine, gate, drain) = scoped_engine(&[]);
    let (dir, _prior, counts) = input(None);
    let mut prepared = light_lambda::input_with_precision_engine(
        Some(dir.path().into()),
        false,
        engine,
        Duration::from_secs(5),
    )
    .unwrap();
    prepared.backend = BackendConfiguration::Injected(counts.clone());
    let later = later_factory(
        &mut prepared,
        dir.path(),
        drain.clone(),
        true,
        false,
        false,
        true,
    );
    let task = tokio::spawn(init::initialize(prepared));
    tokio::time::timeout(Duration::from_secs(5), later.signal.entered.notified())
        .await
        .unwrap();
    wait_closed(&drain).await;
    task.abort();
    let result = task.await;
    assert!(matches!(result,Err(error) if error.is_cancelled()));
    assert!(gate.snapshot().active);
    assert_eq!(gate.snapshot().cleanup, init::PreparationCleanup::Pending);
    drop(later.pending.lock().unwrap().take());
    assert_eq!(
        gate.settled(Instant::now() + Duration::from_secs(5))
            .await
            .unwrap()
            .cleanup,
        init::PreparationCleanup::Drained
    );
    assert_eq!(counts.dispatches.load(SeqCst), 0);
    assert!(!gate.snapshot().published);
}
#[tokio::test]
async fn cancelled_blocking_factory_retains_lane_and_never_publishes_late() {
    let (engine, gate, drain) = scoped_engine(&[]);
    let (dir, _prior, counts) = input(None);
    let mut prepared = light_lambda::input_with_precision_engine(
        Some(dir.path().into()),
        false,
        engine,
        Duration::from_secs(5),
    )
    .unwrap();
    prepared.backend = BackendConfiguration::Injected(counts.clone());
    let later = later_factory(
        &mut prepared,
        dir.path(),
        drain.clone(),
        false,
        false,
        true,
        false,
    );
    let _release = ReleaseMetadata(later.signal.clone());
    let task = tokio::spawn(init::initialize(prepared));
    tokio::time::timeout(Duration::from_secs(5), later.signal.entered.notified())
        .await
        .unwrap();
    task.abort();
    let result = task.await;
    assert!(matches!(result,Err(error) if error.is_cancelled()));
    assert!(gate.snapshot().active);
    assert!(gate.snapshot().cancelled);
    assert!(!gate.snapshot().published);
    later.signal.release();
    assert_eq!(
        gate.settled(Instant::now() + Duration::from_secs(5))
            .await
            .unwrap()
            .cleanup,
        init::PreparationCleanup::Drained
    );
    assert!(!gate.snapshot().published);
    assert_eq!(counts.dispatches.load(SeqCst), 0);
}
#[tokio::test]
async fn validator_only_activation_joins_aggregate_cleanup_context() {
    let (engine, gate, _drain) = scoped_engine(&[true]);
    let (_dir, prepared, counts) = input(Some(engine.clone()));
    let snapshot = init::initialize(prepared).await.unwrap();
    assert_eq!(gate.snapshot().cleanup, init::PreparationCleanup::Pending);
    assert_eq!(
        event(snapshot.clone(), "fallback-owned-warm", json!({}), 1000).await["statusCode"],
        200
    );
    assert_eq!(engine.starts.load(SeqCst), 1);
    assert_eq!(counts.dispatches.load(SeqCst), 1);
    drop(snapshot);
    assert_eq!(
        gate.settled(Instant::now() + Duration::from_secs(5))
            .await
            .unwrap()
            .cleanup,
        init::PreparationCleanup::Drained
    );
}

// Proposed required-profile checks. All precision results are injected; no child.
mod required_profile_proposal {
    use super::*;
    use light_lambda_runtime::init::HandlerFactoryProvider;
    use light_lambda_runtime::init_error::InitError;
    use light_lambda_runtime::required_validator_profile::{
        PROFILE_ERROR, RequiredValidatorProfile, handler_identity_for_review,
    };

    const ACTIVE: &[HandlerId] = &[
        HandlerId::Metrics,
        HandlerId::RequestCors,
        HandlerId::ResponseCors,
        HandlerId::Specification,
        HandlerId::Validator,
    ];

    fn setup(
        replies: &[bool],
    ) -> (
        tempfile::TempDir,
        InitInput,
        Arc<BackendCounts>,
        Arc<Compiler>,
    ) {
        let engine = compiler(replies, false);
        engine.session.metadata.set(SOURCE.to_owned()).unwrap();
        let (dir, mut input, counts) = input(None);
        // Independent controlled baseline before any mutation. This is a test
        // fixture identity, never a runtime trust-on-first-use constructor.
        let handler: Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("handler.yml")).unwrap())
                .unwrap();
        let profile = Arc::new(RequiredValidatorProfile {
            handler_sha256: handler_identity_for_review(&handler).unwrap(),
            active_handlers: ACTIVE,
        });
        let provider = Arc::new(
            ValidatorPolicyProvider::with_engine(
                engine.clone(),
                Arc::new(ExplicitTestDisposition),
                Duration::from_secs(2),
            )
            .with_required_profile(profile),
        );
        for id in [HandlerId::Specification, HandlerId::Validator] {
            input
                .handlers
                .iter_mut()
                .find(|h| h.id == id)
                .unwrap()
                .factory = Some(provider.clone());
        }
        (dir, input, counts, engine)
    }
    fn mutate_handler(dir: &std::path::Path, mutate: impl FnOnce(&mut Value)) {
        let path = dir.join("handler.yml");
        let mut handler: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        mutate(&mut handler);
        std::fs::write(path, serde_json::to_vec(&handler).unwrap()).unwrap();
    }
    async fn rejected(input: InitInput, counts: &BackendCounts, engine: &Compiler) {
        let report = init::validate_configuration(&input).await;
        assert!(!report.valid);
        assert!(
            report
                .errors
                .iter()
                .any(|error| error.contains(PROFILE_ERROR)),
            "{:?}",
            report.errors
        );
        assert!(
            !report
                .errors
                .join(" ")
                .contains("sensitive-synthetic-marker")
        );
        match init::initialize(input).await {
            Err(InitError::InvalidConfiguration(message)) => assert_eq!(message, PROFILE_ERROR),
            _ => panic!("expected exact required-profile Init refusal"),
        }
        assert_eq!(engine.starts.load(SeqCst), 0);
        assert!(engine.captured.lock().unwrap().is_empty());
        assert_eq!(counts.dispatches.load(SeqCst), 0);
        assert_eq!(counts.completions.load(SeqCst), 0);
    }

    #[tokio::test]
    async fn matching_profile_offline_remains_fail_closed_without_precision_start() {
        let (_dir, input, counts, engine) = setup(&[]);
        let report = init::validate_configuration(&input).await;
        assert!(!report.valid);
        assert!(report.errors.iter().all(|e| !e.contains(PROFILE_ERROR)));
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.contains("offline validation does not start an engine")),
            "{:?}",
            report.errors
        );
        assert_eq!(engine.starts.load(SeqCst), 0);
        assert_eq!(counts.dispatches.load(SeqCst), 0);
    }
    #[tokio::test]
    async fn matching_profile_reaches_real_factory_and_service_with_injected_precision() {
        let (_dir, input, counts, engine) = setup(&[true, false, true]);
        let snapshot = init::initialize(input).await.unwrap();
        assert_eq!(
            event(snapshot.clone(), "profile-valid", json!({}), 1000).await["statusCode"],
            200
        );
        assert_eq!(
            event(snapshot.clone(), "profile-invalid", json!({}), 1000).await["body"],
            BAD
        );
        assert_eq!(
            event(snapshot, "profile-warm", json!({}), 1000).await["statusCode"],
            200
        );
        assert_eq!(engine.starts.load(SeqCst), 1);
        assert_eq!(engine.session.events.lock().unwrap().len(), 3);
        assert_eq!(counts.dispatches.load(SeqCst), 2);
        assert_eq!(counts.completions.load(SeqCst), 3);
    }
    #[tokio::test]
    async fn disabled_specification_or_validator_rejects_before_pruning() {
        for file in ["openapi-handler.yml", "openapi-validator.yml"] {
            let (dir, input, counts, engine) = setup(&[]);
            std::fs::write(dir.path().join(file), "enabled: false\n").unwrap();
            rejected(input, &counts, &engine).await;
        }
    }
    #[tokio::test]
    async fn missing_registration_or_factory_rejects_even_with_other_profile_provider() {
        for id in [HandlerId::Specification, HandlerId::Validator] {
            for remove_registration in [false, true] {
                let (_dir, mut input, counts, engine) = setup(&[]);
                if remove_registration {
                    input.handlers.retain(|h| h.id != id);
                } else {
                    input
                        .handlers
                        .iter_mut()
                        .find(|h| h.id == id)
                        .unwrap()
                        .factory = None;
                }
                rejected(input, &counts, &engine).await;
            }
        }
    }
    #[tokio::test]
    async fn changed_chains_aliases_routes_terminal_and_handler_settings_reject() {
        for case in [
            "omit", "order", "nested", "mapping", "terminal", "class", "alias", "disabled", "extra",
        ] {
            let (dir, input, counts, engine) = setup(&[]);
            mutate_handler(dir.path(), |handler| match case {
                "omit" => handler["defaultHandlers"] = json!(["spec", "proxy"]),
                "order" => handler["defaultHandlers"] = json!(["validator", "spec", "proxy"]),
                "nested" => {
                    handler["chains"] = json!({"bypass":["proxy"]});
                    handler["defaultHandlers"] = json!(["bypass"]);
                }
                "mapping" => {
                    handler["paths"] = json!([{"path":"/x","method":"post","executes":["proxy"]}])
                }
                "terminal" => handler["defaultHandlers"] = json!(["spec", "validator"]),
                "class" => handler["handlers"][0] = json!("sensitive-synthetic-marker@validator"),
                "alias" => handler["defaultHandlers"] = json!(["spec", "other", "proxy"]),
                "disabled" => handler["enabled"] = json!(false),
                "extra" => handler["reportHandlerDuration"] = json!(false),
                _ => unreachable!(),
            });
            rejected(input, &counts, &engine).await;
        }
    }
    #[tokio::test]
    async fn changed_complete_active_set_rejects_unreviewed_enable_or_disable() {
        for (file, enabled) in [("metrics.yml", false), ("header.yml", true)] {
            let (dir, input, counts, engine) = setup(&[]);
            std::fs::write(dir.path().join(file), format!("enabled: {enabled}\n")).unwrap();
            rejected(input, &counts, &engine).await;
        }
    }
    #[tokio::test]
    async fn proxy_disabled_rejects_before_engine_start() {
        let (dir, input, counts, engine) = setup(&[]);
        let path = dir.path().join("lambda-proxy.yml");
        let value = std::fs::read_to_string(&path).unwrap();
        std::fs::write(path, format!("enabled: false\n{value}")).unwrap();
        rejected(input, &counts, &engine).await;
    }
    #[tokio::test]
    async fn invalid_trusted_profile_cannot_omit_validator_or_duplicate_requirements() {
        for active in [
            &[HandlerId::Specification][..],
            &[HandlerId::Validator][..],
            &[
                HandlerId::Specification,
                HandlerId::Validator,
                HandlerId::Validator,
            ][..],
            &[
                HandlerId::Specification,
                HandlerId::Validator,
                HandlerId::Proxy,
            ][..],
        ] {
            let (dir, mut input, counts, engine) = setup(&[]);
            let handler: Value =
                serde_json::from_slice(&std::fs::read(dir.path().join("handler.yml")).unwrap())
                    .unwrap();
            let profile = Arc::new(RequiredValidatorProfile {
                handler_sha256: handler_identity_for_review(&handler).unwrap(),
                active_handlers: active,
            });
            let provider = Arc::new(
                ValidatorPolicyProvider::with_engine(
                    engine.clone(),
                    Arc::new(ExplicitTestDisposition),
                    Duration::from_secs(2),
                )
                .with_required_profile(profile),
            );
            for id in [HandlerId::Specification, HandlerId::Validator] {
                input
                    .handlers
                    .iter_mut()
                    .find(|h| h.id == id)
                    .unwrap()
                    .factory = Some(provider.clone());
            }
            rejected(input, &counts, &engine).await;
        }
    }
    struct NeverBuild(Arc<AtomicUsize>);
    impl HandlerFactoryProvider for NeverBuild {
        fn create(
            &self,
            _: &light_lambda_runtime::init::RuntimeConfig,
            _: Option<Arc<light_security::SecurityRuntime>>,
        ) -> Result<Arc<dyn serverless_core::plan::HandlerFactory>, InitError> {
            self.0.fetch_add(1, SeqCst);
            panic!("required-profile refusal must precede factory construction")
        }
    }
    #[tokio::test]
    async fn profile_rejection_precedes_unrelated_factory_and_retains_static_privacy() {
        let (dir, mut input, counts, engine) = setup(&[]);
        let calls = Arc::new(AtomicUsize::new(0));
        input
            .handlers
            .iter_mut()
            .find(|h| h.id == HandlerId::Metrics)
            .unwrap()
            .factory = Some(Arc::new(NeverBuild(calls.clone())));
        mutate_handler(dir.path(), |h| {
            h["defaultHandlers"] = json!(["sensitive-synthetic-marker"])
        });
        rejected(input, &counts, &engine).await;
        assert_eq!(calls.load(SeqCst), 0);
    }
    #[tokio::test]
    async fn registration_order_and_post_init_file_mutation_do_not_change_warm_plan() {
        let (dir, mut input, counts, engine) = setup(&[true, true]);
        input.handlers.reverse();
        let snapshot = init::initialize(input).await.unwrap();
        assert_eq!(
            event(snapshot.clone(), "profile-first", json!({}), 1000).await["statusCode"],
            200
        );
        mutate_handler(dir.path(), |h| h["defaultHandlers"] = json!(["proxy"]));
        std::fs::write(dir.path().join("openapi-validator.yml"), "enabled: false\n").unwrap();
        assert_eq!(
            event(snapshot, "profile-still-validated", json!({}), 1000).await["statusCode"],
            200
        );
        assert_eq!(engine.starts.load(SeqCst), 1);
        assert_eq!(engine.session.events.lock().unwrap().len(), 2);
        assert_eq!(counts.dispatches.load(SeqCst), 2);
    }
}
