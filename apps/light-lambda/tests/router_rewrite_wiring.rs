//! Actual application Init/service, with a lifecycle-only non-child endpoint.
//! This endpoint never creates a Candidate or implements regex/recovery logic.
use async_trait::async_trait;
use light_lambda_runtime::{
    init::{self, BackendConfiguration, InitInput},
    init_error::InitError,
    router_java_worker::{Candidate, Failure, recovery::RecoveryFailure},
    router_program::Program,
    router_rewrite_policy::{Controller, ControllerFactory, Rewriter},
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
use tokio::{
    sync::{Notify, OwnedSemaphorePermit, Semaphore},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

struct Endpoint {
    starts: AtomicUsize,
    shutdowns: AtomicUsize,
    block: AtomicBool,
    fail: AtomicBool,
    entered: Notify,
    release: Notify,
    stopped: Notify,
    capacity: Arc<Semaphore>,
    owner: Mutex<Option<OwnedSemaphorePermit>>,
    paths: Mutex<Vec<String>>,
    replies: Mutex<VecDeque<RecoveryFailure>>,
    hold_requests: AtomicBool,
    dropped_requests: AtomicUsize,
    last_cancel: Mutex<Option<CancellationToken>>,
}
impl Endpoint {
    fn new(block: bool, fail: bool) -> Arc<Self> {
        Arc::new(Self {
            starts: AtomicUsize::new(0),
            shutdowns: AtomicUsize::new(0),
            block: AtomicBool::new(block),
            fail: AtomicBool::new(fail),
            entered: Notify::new(),
            release: Notify::new(),
            stopped: Notify::new(),
            capacity: Arc::new(Semaphore::new(1)),
            owner: Mutex::new(None),
            paths: Mutex::new(vec![]),
            replies: Mutex::new(VecDeque::new()),
            hold_requests: AtomicBool::new(false),
            dropped_requests: AtomicUsize::new(0),
            last_cancel: Mutex::new(None),
        })
    }
}
#[async_trait]
impl Controller for Endpoint {
    async fn initialize(&self, _: Instant, _: CancellationToken) -> Result<(), RecoveryFailure> {
        *self.owner.lock().unwrap() = Some(self.capacity.clone().try_acquire_owned().unwrap());
        self.starts.fetch_add(1, SeqCst);
        self.entered.notify_one();
        if self.block.load(SeqCst) {
            self.release.notified().await;
        }
        if self.fail.load(SeqCst) {
            Err(RecoveryFailure::Worker(Failure::Artifact))
        } else {
            Ok(())
        }
    }
    async fn shutdown(&self) {
        self.shutdowns.fetch_add(1, SeqCst);
        self.owner.lock().unwrap().take();
        self.stopped.notify_one();
    }
}
#[async_trait]
impl Rewriter for Endpoint {
    async fn rewrite(
        &self,
        path: &str,
        _: Instant,
        cancel: CancellationToken,
    ) -> Result<Candidate, RecoveryFailure> {
        self.paths.lock().unwrap().push(path.into());
        *self.last_cancel.lock().unwrap() = Some(cancel);
        if self.hold_requests.load(SeqCst) {
            struct Dropped<'a>(&'a AtomicUsize);
            impl Drop for Dropped<'_> {
                fn drop(&mut self) {
                    self.0.fetch_add(1, SeqCst);
                }
            }
            let _dropped = Dropped(&self.dropped_requests);
            std::future::pending::<()>().await;
        }
        Err(self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(RecoveryFailure::Worker(Failure::Resource)))
    }
}
struct Factory {
    endpoint: Arc<Endpoint>,
    creates: AtomicUsize,
    programs: Mutex<Vec<Arc<Program>>>,
}
impl Factory {
    fn new(endpoint: Arc<Endpoint>) -> Arc<Self> {
        Arc::new(Self {
            endpoint,
            creates: AtomicUsize::new(0),
            programs: Mutex::new(vec![]),
        })
    }
}
impl ControllerFactory for Factory {
    fn create(&self, program: Arc<Program>) -> Result<Arc<dyn Controller>, InitError> {
        self.creates.fetch_add(1, SeqCst);
        self.programs.lock().unwrap().push(program);
        Ok(self.endpoint.clone())
    }
}
#[derive(Default)]
struct Lambda(AtomicUsize);
#[async_trait]
impl Backend for Lambda {
    async fn dispatch(
        &self,
        _: &Invocation,
        _: &ExchangeState,
        _: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        self.0.fetch_add(1, SeqCst);
        Ok(StatusError::new(200, "TEST", "TEST", "owned lambda").response())
    }
}
struct Setup {
    dir: tempfile::TempDir,
    listener: std::net::TcpListener,
    input: InitInput,
    lambda: Arc<Lambda>,
}
fn setup(factory: Arc<dyn ControllerFactory>, timeout: Duration) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    for (name, text) in [
        ("values.yml", "light-lambda.stage: rewrite-wiring\nlambda-proxy.region: us-east-1\nlambda-proxy.metricsInjection: false\n".to_owned()),
        ("router.yml", "enabled: true\nhttpsEnabled: false\nurlRewriteRules: ['^/x$ /changed']\n".into()),
        ("direct-registry.yml", format!("directUrls:\n  owned: http://{}\n",listener.local_addr().unwrap())),
        ("lambda-proxy.yml", "functions: {'/x@get': TestFn}\nregion: us-east-1\nmetricsInjection: false\n".into()),
    ] { std::fs::write(dir.path().join(name),text).unwrap(); }
    let class = |id| {
        compat::COMPATIBILITY
            .iter()
            .find(|c| c.id == id)
            .unwrap()
            .class
    };
    std::fs::write(dir.path().join("handler.yml"), json!({"handlers":[format!("{}@router",class(HandlerId::Router)),format!("{}@proxy",class(HandlerId::Proxy))],"defaultHandlers":["router","proxy"]}).to_string()).unwrap();
    let mut input = light_lambda::input_with_router_qualification(
        Some(dir.path().into()),
        false,
        factory,
        timeout,
    )
    .unwrap();
    let lambda = Arc::new(Lambda::default());
    input.backend = BackendConfiguration::Injected(lambda.clone());
    Setup {
        dir,
        listener,
        input,
        lambda,
    }
}
async fn request(snapshot: Arc<init::Snapshot>, routed: bool) -> Value {
    let mut context = lambda_runtime::Context::default();
    context.request_id = "non-child-wiring".into();
    context.deadline = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 2000;
    light_lambda_runtime::service::handle(snapshot,lambda_runtime::LambdaEvent::new(json!({"path":"/x","httpMethod":"GET","requestContext":{"elb":{"targetGroupArn":"owned"}},"multiValueHeaders":if routed {json!({"service_id":["owned"]})} else {json!({})}}),context)).await.unwrap()
}
fn zero_http(listener: &std::net::TcpListener) {
    assert!(matches!(listener.accept(),Err(e) if e.kind()==std::io::ErrorKind::WouldBlock));
}

#[tokio::test]
async fn publication_waits_for_activation_and_unavailable_never_falls_back() {
    let endpoint = Endpoint::new(true, false);
    let factory = Factory::new(endpoint.clone());
    let setup = setup(factory.clone(), Duration::from_secs(2));
    let task = tokio::spawn(init::initialize(setup.input));
    endpoint.entered.notified().await;
    assert!(
        !task.is_finished(),
        "no Snapshot while initialization is pending"
    );
    assert_eq!(setup.lambda.0.load(SeqCst), 0);
    endpoint.release.notify_one();
    let snapshot = task.await.unwrap().unwrap();
    assert_eq!(factory.creates.load(SeqCst), 1);
    assert_eq!(endpoint.starts.load(SeqCst), 1);
    assert_eq!(
        factory.programs.lock().unwrap()[0].rules()[0].pattern,
        "^/x$"
    );
    std::fs::write(setup.dir.path().join("router.yml"), "changed after Init").unwrap();
    for _ in 0..2 {
        let result = request(snapshot.clone(), true).await;
        assert_eq!(result["statusCode"], 503);
        let body: Value = serde_json::from_str(result["body"].as_str().unwrap()).unwrap();
        assert_eq!(body["code"], "ERR14035");
        assert_eq!(
            body["description"],
            "Router rewrite execution is unavailable."
        );
        assert_eq!(setup.lambda.0.load(SeqCst), 0);
        zero_http(&setup.listener);
    }
    assert_eq!(endpoint.paths.lock().unwrap().as_slice(), ["/x", "/x"]);
    assert_eq!(request(snapshot.clone(), false).await["statusCode"], 200);
    assert_eq!(
        setup.lambda.0.load(SeqCst),
        1,
        "independent warm Lambda route is not fallback"
    );
    let retained = snapshot.clone();
    drop(snapshot);
    assert_eq!(
        endpoint.shutdowns.load(SeqCst),
        0,
        "a retained Snapshot still owns the lifecycle"
    );
    drop(retained);
    tokio::time::timeout(Duration::from_secs(2), endpoint.stopped.notified())
        .await
        .unwrap();
    assert_eq!(endpoint.capacity.available_permits(), 1);
    assert_eq!(endpoint.shutdowns.load(SeqCst), 1);
}

#[tokio::test]
async fn offline_validation_never_constructs_or_initializes_a_controller() {
    let endpoint = Endpoint::new(false, false);
    let factory = Factory::new(endpoint.clone());
    let setup = setup(factory.clone(), Duration::from_secs(1));
    let report = init::validate_configuration(&setup.input).await;
    assert!(!report.valid);
    assert!(
        report
            .errors
            .iter()
            .any(|s| s.contains("offline validation never starts a worker"))
    );
    assert_eq!(factory.creates.load(SeqCst), 0);
    assert_eq!(endpoint.starts.load(SeqCst), 0);
    assert_eq!(endpoint.capacity.available_permits(), 1);
    zero_http(&setup.listener);
}

#[tokio::test]
async fn failing_activation_releases_partial_owner_before_error_returns() {
    let endpoint = Endpoint::new(false, true);
    let setup = setup(Factory::new(endpoint.clone()), Duration::from_secs(1));
    assert!(init::initialize(setup.input).await.is_err());
    assert_eq!(endpoint.capacity.available_permits(), 1);
    assert_eq!(endpoint.shutdowns.load(SeqCst), 1);
    assert_eq!(setup.lambda.0.load(SeqCst), 0);
    zero_http(&setup.listener);
}

#[tokio::test]
async fn later_init_failure_rolls_back_activated_router_before_returning() {
    let endpoint = Endpoint::new(false, false);
    let mut setup = setup(Factory::new(endpoint.clone()), Duration::from_secs(1));
    setup.input.backend = BackendConfiguration::Offline;
    assert!(init::initialize(setup.input).await.is_err());
    assert_eq!(endpoint.starts.load(SeqCst), 1);
    assert_eq!(endpoint.shutdowns.load(SeqCst), 1);
    assert_eq!(endpoint.capacity.available_permits(), 1);
}

#[tokio::test]
async fn activation_timeout_and_cancelled_init_release_the_non_child_owner() {
    for cancel in [false, true] {
        let endpoint = Endpoint::new(true, false);
        let setup = setup(Factory::new(endpoint.clone()), Duration::from_millis(50));
        let task = tokio::spawn(init::initialize(setup.input));
        endpoint.entered.notified().await;
        if cancel {
            task.abort();
            assert!(matches!(task.await, Err(error) if error.is_cancelled()));
        } else {
            assert!(task.await.unwrap().is_err());
        }
        tokio::time::timeout(Duration::from_secs(2), endpoint.stopped.notified())
            .await
            .unwrap();
        assert_eq!(endpoint.capacity.available_permits(), 1);
        assert_eq!(endpoint.shutdowns.load(SeqCst), 1);
        zero_http(&setup.listener);
    }
}

#[tokio::test]
async fn qualification_rule_limit_rejects_before_controller_construction() {
    let endpoint = Endpoint::new(false, false);
    let factory = Factory::new(endpoint.clone());
    let setup = setup(factory.clone(), Duration::from_secs(1));
    std::fs::write(
        setup.dir.path().join("router.yml"),
        json!({"enabled":true,"urlRewriteRules":vec!["^/x$ /x";65]}).to_string(),
    )
    .unwrap();
    assert!(init::initialize(setup.input).await.is_err());
    assert_eq!(factory.creates.load(SeqCst), 0);
    assert_eq!(endpoint.starts.load(SeqCst), 0);
}

#[tokio::test]
async fn typed_rewrite_errors_keep_exact_mappings_and_make_zero_dependency_calls() {
    let endpoint = Endpoint::new(false, false);
    let mut cases = vec![];
    for failure in [
        Failure::Artifact,
        Failure::Init,
        Failure::Resource,
        Failure::InputProfile,
        Failure::OutputProfile,
        Failure::ChildLost,
        Failure::NoReady,
        Failure::Unclassified,
    ] {
        cases.push((RecoveryFailure::Worker(failure), 503, "ERR14035"));
    }
    for failure in [
        RecoveryFailure::NotInitialized,
        RecoveryFailure::Cooldown,
        RecoveryFailure::Stale,
    ] {
        cases.push((failure, 503, "ERR14035"));
    }
    for failure in [
        Failure::Protocol,
        Failure::Panic,
        Failure::InvalidUtf16,
        Failure::InvalidReplacement,
    ] {
        cases.push((RecoveryFailure::Worker(failure), 500, "ERR14004"));
    }
    for failure in [Failure::Deadline, Failure::Cancelled] {
        cases.push((RecoveryFailure::Worker(failure), 504, "ERR14017"));
    }
    let expected: Vec<_> = cases
        .iter()
        .map(|(_, status, code)| (*status, *code))
        .collect();
    endpoint
        .replies
        .lock()
        .unwrap()
        .extend(cases.into_iter().map(|(failure, _, _)| failure));
    let setup = setup(Factory::new(endpoint.clone()), Duration::from_secs(1));
    let snapshot = init::initialize(setup.input).await.unwrap();
    for (status, code) in expected {
        let response = request(snapshot.clone(), true).await;
        assert_eq!(response["statusCode"], status);
        let body: Value = serde_json::from_str(response["body"].as_str().unwrap()).unwrap();
        assert_eq!(body["code"], code);
        assert_eq!(setup.lambda.0.load(SeqCst), 0);
        zero_http(&setup.listener);
    }
    assert!(endpoint.replies.lock().unwrap().is_empty());
}

struct RecordFailure;
#[async_trait]
impl RequestHandler for RecordFailure {
    fn id(&self) -> HandlerId {
        HandlerId::RequestHeader
    }
    async fn on_request(
        &self,
        _: &mut Invocation,
        state: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        state.record_request_status(StatusError::new(
            401,
            "ERR10000",
            "INVALID_AUTH_TOKEN",
            "test-only-first-cause",
        ));
        Ok(HandlerOutcome::Continue)
    }
}
impl serverless_core::plan::HandlerFactory for RecordFailure {
    fn build(
        &self,
        _: &compat::Compatibility,
        _: &str,
    ) -> Result<serverless_core::plan::Binding, serverless_core::InitError> {
        Ok(serverless_core::plan::Binding::Request(
            Arc::new(Self),
            Default::default(),
        ))
    }
}
impl init::HandlerFactoryProvider for RecordFailure {
    fn create(
        &self,
        _: &light_lambda_runtime::router_rewrite_policy::RuntimeConfig,
        _: Option<Arc<light_security::SecurityRuntime>>,
    ) -> Result<Arc<dyn serverless_core::plan::HandlerFactory>, InitError> {
        Ok(Arc::new(Self))
    }
}
#[tokio::test]
async fn factory_failed_request_rejection_precedes_controller_and_both_backends() {
    let endpoint = Endpoint::new(false, false);
    let mut setup = setup(Factory::new(endpoint.clone()), Duration::from_secs(1));
    let class = compat::COMPATIBILITY
        .iter()
        .find(|c| c.id == HandlerId::RequestHeader)
        .unwrap()
        .class;
    let path = setup.dir.path().join("handler.yml");
    let mut handler: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    handler["handlers"]
        .as_array_mut()
        .unwrap()
        .push(json!(format!("{class}@recordFailure")));
    handler["defaultHandlers"] = json!(["recordFailure", "router", "proxy"]);
    std::fs::write(path, handler.to_string()).unwrap();
    std::fs::write(setup.dir.path().join("header.yml"), "enabled: true\n").unwrap();
    setup
        .input
        .handlers
        .iter_mut()
        .find(|d| d.id == HandlerId::RequestHeader)
        .unwrap()
        .factory = Some(Arc::new(RecordFailure));
    let snapshot = init::initialize(setup.input).await.unwrap();
    let response = request(snapshot, true).await;
    assert_eq!(response["statusCode"], 400);
    let body: Value = serde_json::from_str(response["body"].as_str().unwrap()).unwrap();
    assert_eq!(body["code"], "ERR10087");
    assert_eq!(body["description"], "Exchange has failed state 10.");
    assert!(!response.to_string().contains("test-only-first-cause"));
    assert!(endpoint.paths.lock().unwrap().is_empty());
    assert_eq!(setup.lambda.0.load(SeqCst), 0);
    zero_http(&setup.listener);
}

#[tokio::test]
async fn service_deadline_drops_rewrite_and_preserves_next_warm_exchange() {
    let endpoint = Endpoint::new(false, false);
    endpoint.hold_requests.store(true, SeqCst);
    let setup = setup(Factory::new(endpoint.clone()), Duration::from_secs(1));
    let path = setup.dir.path().join("values.yml");
    let mut values = std::fs::read_to_string(&path).unwrap();
    values.push_str("light-lambda.ingressTimeoutMs: 100\nlight-lambda.responseReserveMs: 10\nlight-lambda.postReserveMs: 5\n");
    std::fs::write(path, values).unwrap();
    let snapshot = init::initialize(setup.input).await.unwrap();
    let response = request(snapshot.clone(), true).await;
    assert_eq!(response["statusCode"], 504);
    assert_eq!(endpoint.dropped_requests.load(SeqCst), 1);
    assert!(
        endpoint
            .last_cancel
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .is_cancelled()
    );
    endpoint.hold_requests.store(false, SeqCst);
    assert_eq!(request(snapshot, true).await["statusCode"], 503);
    assert_eq!(setup.lambda.0.load(SeqCst), 0);
    zero_http(&setup.listener);
}

#[tokio::test]
async fn real_controller_artifact_rejection_rolls_back_without_starting_java() {
    use light_lambda_runtime::{
        router_java_worker::{Artifacts, QualificationLimits, recovery::RecoveryPolicy},
        router_rewrite_policy::JavaControllerFactory,
    };
    // Invalid classpath shape is rejected before any artifact open or Command
    // construction. This is real-controller Init rejection, not child proof.
    let factory = Arc::new(JavaControllerFactory {
        artifacts: Arc::new(Artifacts {
            java: "/never-executed-by-this-test".into(),
            classpath: vec![],
            pins: vec![],
        }),
        worker_limits: QualificationLimits {
            frame_bytes: 1024,
            input_utf16: 64,
            output_utf16: 64,
            heap_mib: 1,
            stack_kib: 256,
            address_space_bytes: 1 << 20,
        },
        worker_slots: 1,
        recovery: RecoveryPolicy {
            attempts: 1,
            cooldown: Duration::from_secs(1),
        },
    });
    let setup = setup(factory, Duration::from_secs(1));
    let result = tokio::time::timeout(Duration::from_secs(2), init::initialize(setup.input))
        .await
        .unwrap();
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("unavailable artifact must reject Init"),
    };
    assert!(!format!("{error:?}").contains("never-executed"));
    assert_eq!(setup.lambda.0.load(SeqCst), 0);
    zero_http(&setup.listener);
}
