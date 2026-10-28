use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "owned Java resource/recovery qualification; requires WP12_ROUTER_ARTIFACTS"]
async fn aggregate_program_recovery_and_rate() {
    let capacity = Arc::new(Semaphore::new(2));
    let mut rules = (0..127)
        .map(|i| Rule {
            pattern: format!("/unmatched-{i}-{}", "x".repeat(440)),
            replacement: "/unused".into(),
        })
        .collect::<Vec<_>>();
    rules.push(Rule {
        pattern: "/x+".into(),
        replacement: "/ok".into(),
    });
    let raw_bytes: usize = rules
        .iter()
        .map(|r| r.pattern.len() + r.replacement.len())
        .sum();
    assert!(raw_bytes < 65536);
    let program: Arc<[Rule]> = rules.into();
    async fn prepare(program: Arc<[Rule]>, capacity: Arc<Semaphore>, epoch: u64) -> Session {
        let artifacts = artifacts();
        Session::prepare(
            Preparation {
                artifacts: &artifacts,
                limits: limits(),
                capacity,
                epoch,
            },
            program,
            Instant::now() + Duration::from_secs(15),
            CancellationToken::new(),
        )
        .await
        .unwrap()
    }
    let started = std::time::Instant::now();
    let old = prepare(program.clone(), capacity.clone(), 7).await;
    let old_init_micros = started.elapsed().as_micros();
    let task = tokio::spawn(prepare(program.clone(), capacity.clone(), 8));
    let path = format!("/{}", "x".repeat(65535));
    let result = old
        .rewrite(
            &path,
            Instant::now() + Duration::from_secs(2),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let mut inv = invocation(&path);
    result.commit(&mut inv).unwrap();
    assert_eq!(inv.path, "/ok");
    let replacement = task.await.unwrap();
    assert_eq!(capacity.available_permits(), 0);
    let memory = |session: &Session| {
        let pid = session.pid.load(Ordering::SeqCst);
        std::fs::read_to_string(format!("/proc/{pid}/status"))
            .unwrap()
            .lines()
            .filter(|s| {
                s.starts_with("VmRSS:") || s.starts_with("VmSize:") || s.starts_with("VmHWM:")
            })
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    println!(
        "ROUTER_AGGREGATE {}",
        json!({"rules":program.len(),"programBytes":raw_bytes,"inputUtf16":path.encode_utf16().count(),"firstInitMicros":old_init_micros,"oldMemory":memory(&old),"replacementMemory":memory(&replacement),"liveChildPermits":2,"productionAdmission":false})
    );
    let old_pid = old.pid.load(Ordering::SeqCst);
    old.shutdown().await;
    assert!(!std::path::Path::new(&format!("/proc/{old_pid}")).exists());
    assert_eq!(capacity.available_permits(), 1);
    let mut inv = invocation("/xxxx");
    replacement
        .rewrite(&inv.path, inv.deadline, inv.cancel.clone())
        .await
        .unwrap()
        .commit(&mut inv)
        .unwrap();
    assert_eq!(inv.path, "/ok");
    replacement.shutdown().await;
    assert_eq!(capacity.available_permits(), 2);

    // Sliding-window admission consumes a token before work; cancellation never refunds it.
    let rate = Arc::new(std::sync::Mutex::new((
        std::collections::VecDeque::<u64>::new(),
        0u64,
    )));
    let mut tasks = Vec::new();
    for _ in 0..64 {
        let rate = rate.clone();
        tasks.push(tokio::spawn(async move {
            let mut guard = rate.lock().unwrap();
            let now = 0;
            while guard.0.front().is_some_and(|t| *t + 30000 <= now) {
                guard.0.pop_front();
            }
            if guard.0.len() == 2 {
                return false;
            }
            guard.0.push_back(now);
            guard.1 += 1;
            true
        }));
    }
    let mut admitted = 0;
    for task in tasks {
        admitted += usize::from(task.await.unwrap());
    }
    assert_eq!(admitted, 2);
    assert_eq!(rate.lock().unwrap().1, 2);
    let mut starts = std::collections::VecDeque::new();
    let mut accepted = Vec::new();
    let mut last = 0u64;
    for now in [0, 0, 29999, 30000, 30000, 59999, 60000]
        .into_iter()
        .chain(0..100000)
    {
        let now = last.max(now);
        last = now;
        while starts.front().is_some_and(|t| *t + 30000 <= now) {
            starts.pop_front();
        }
        if starts.len() < 2 {
            starts.push_back(now);
            accepted.push(now);
        }
        assert!(
            accepted
                .iter()
                .filter(|t| **t > now.saturating_sub(30000) || now < 30000)
                .count()
                <= 2
        );
    }
}

#[path = "tests_common_init.rs"]
mod common_init;
use serde_json::json;
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

#[test]
fn strict_reply_and_error_categories_never_map_resource_loss_to_500() {
    let reply = |value: serde_json::Value| serde_json::to_vec(&value).unwrap();
    let valid = reply(json!({"id":1,"epoch":2,"units":[47,55357,56832],"validUtf16":true}));
    assert_eq!(decode(&valid, 3, 1, 2), Ok("/😀".into()));
    assert_eq!(decode(&valid, 2, 1, 2), Err(Failure::OutputProfile));
    for value in [
        json!({"id":1,"epoch":2,"units":[55357],"validUtf16":false}),
        json!({"id":1,"epoch":2,"units":[56832],"validUtf16":true}),
    ] {
        assert_eq!(decode(&reply(value), 3, 1, 2), Err(Failure::InvalidUtf16));
    }
    for value in [
        json!({"id":2,"epoch":2,"units":[],"validUtf16":true}),
        json!({"id":1,"epoch":3,"units":[],"validUtf16":true}),
        json!({"id":1,"epoch":2,"units":[65536],"validUtf16":true}),
        json!({"id":1,"epoch":2,"units":[],"validUtf16":true,"extra":0}),
        json!({"id":1,"epoch":2,"units":[],"validUtf16":false}),
        json!({"id":1,"epoch":2,"units":[],"validUtf16":true,"error":"AssertionError"}),
    ] {
        assert_eq!(decode(&reply(value), 3, 1, 2), Err(Failure::Protocol));
    }
    assert_eq!(
        decode(
            br#"{"id":1,"id":1,"epoch":2,"units":[],"validUtf16":true}"#,
            3,
            1,
            2
        ),
        Err(Failure::Protocol)
    );
    for (tag, expected) in [
        ("IllegalArgumentException", Failure::InvalidReplacement),
        ("IndexOutOfBoundsException", Failure::InvalidReplacement),
        ("AssertionError", Failure::Panic),
        ("StackOverflowError", Failure::Resource),
        ("OutOfMemoryError", Failure::Resource),
        ("IllegalStateException", Failure::OutputProfile),
    ] {
        assert_eq!(
            decode(&reply(json!({"id":1,"epoch":2,"error":tag})), 3, 1, 2),
            Err(expected)
        );
    }
    for failure in [
        Failure::InvalidUtf16,
        Failure::InvalidReplacement,
        Failure::Protocol,
        Failure::Panic,
    ] {
        assert_eq!(failure.approved_status().unwrap().to_bytes().as_ref(),br#"{"statusCode":500,"code":"ERR14004","message":"MIDDLEWARE_UNHANDLED_EXCEPTION","description":"Middleware thread unhandled runtime exception.","severity":"ERROR"}"#);
    }
    for failure in [
        Failure::Resource,
        Failure::ChildLost,
        Failure::NoReady,
        Failure::InputProfile,
        Failure::OutputProfile,
    ] {
        assert!(failure.approved_status().is_none());
    }
    assert!(encode(&"x".repeat(65), 64).is_err());
    assert_eq!(encode(&"x".repeat(62), 64).unwrap().len(), 64);
}
pub(super) fn limits() -> QualificationLimits {
    QualificationLimits {
        frame_bytes: 2 * 1024 * 1024,
        input_utf16: 65536,
        output_utf16: 16384,
        heap_mib: 64,
        stack_kib: 256,
        address_space_bytes: 2 * 1024 * 1024 * 1024,
    }
}
pub(super) fn artifacts() -> Artifacts {
    let value: serde_json::Value = serde_json::from_slice(
        &std::fs::read(std::env::var_os("WP12_ROUTER_ARTIFACTS").expect("owned artifact manifest"))
            .unwrap(),
    )
    .unwrap();
    let pins = value["pins"]
        .as_array()
        .unwrap()
        .iter()
        .map(|pin| {
            let path = PathBuf::from(pin["path"].as_str().unwrap());
            let sha = pin["sha256"].as_str().unwrap();
            let mut digest = [0; 32];
            for (i, byte) in digest.iter_mut().enumerate() {
                *byte = u8::from_str_radix(&sha[i * 2..i * 2 + 2], 16).unwrap();
            }
            ArtifactPin {
                path,
                sha256: digest,
                bytes: pin["bytes"].as_u64().unwrap(),
            }
        })
        .collect();
    Artifacts {
        java: value["java"].as_str().unwrap().into(),
        classpath: value["classpath"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| PathBuf::from(s.as_str().unwrap()))
            .collect(),
        pins,
    }
}
async fn start(rules: &[(&str, &str)], capacity: Arc<Semaphore>) -> Session {
    Session::prepare(
        Preparation {
            artifacts: &artifacts(),
            limits: limits(),
            capacity,
            epoch: 7,
        },
        rules
            .iter()
            .map(|(p, r)| Rule {
                pattern: (*p).into(),
                replacement: (*r).into(),
            })
            .collect::<Vec<_>>()
            .into(),
        Instant::now() + Duration::from_secs(4),
        CancellationToken::new(),
    )
    .await
    .unwrap()
}
async fn fault(
    session: &Session,
    name: &str,
    deadline: Instant,
    cancel: CancellationToken,
) -> Result<String, Failure> {
    let (reply, rx) = oneshot::channel();
    session
        .commands
        .send(Request {
            bytes: encode(
                &json!({"id":42,"epoch":7,"path":"/x","fault":name}),
                limits().frame_bytes,
            )
            .unwrap(),
            id: 42,
            deadline,
            cancel,
            reply,
        })
        .await
        .unwrap();
    rx.await.unwrap()
}
fn invocation(path: &str) -> Invocation {
    Invocation {
        method: http::Method::GET,
        raw_path: path.into(),
        path: path.into(),
        raw_query: None,
        query: serverless_core::MultiMap::default(),
        headers: http::HeaderMap::new(),
        body: None,
        body_was_base64: false,
        source: serverless_core::SourceMetadata::default(),
        request_id: "owned-router".into(),
        deadline: Instant::now() + Duration::from_secs(2),
        response_deadline: Instant::now() + Duration::from_secs(3),
        cancel: CancellationToken::new(),
        path_params: BTreeMap::new(),
        route: None,
        original: serde_json::Value::Null,
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "owned local JRE child; requires WP12_ROUTER_ARTIFACTS"]
async fn async_java_parity_faults_capacity_cancellation_and_reap() {
    let capacity = Arc::new(Semaphore::new(2));
    for (pattern, replacement, path, expected) in [
        ("/(a)(b)", "$2$1", "/ab", "ba"),
        ("/(?<name>a)", "/${name}", "/a", "/a"),
        ("(?iu)/ä", "/yes", "/Ä", "/yes"),
        ("(?U)/\\w+", "/yes", "/é", "/yes"),
        ("/(?<=/)(a)", "/$1", "/a", "/a"),
        ("/(a)\\1", "/$1", "/aa", "/a"),
        ("/(a)", "$12", "/a", "a2"),
        ("/a{2,3}", "/yes", "/aaa", "/yes"),
        ("/😀", "/😀", "/😀", "/😀"),
    ] {
        let worker = start(&[(pattern, replacement)], capacity.clone()).await;
        let mut inv = invocation(path);
        worker
            .rewrite(path, inv.deadline, inv.cancel.clone())
            .await
            .unwrap()
            .commit(&mut inv)
            .unwrap();
        assert_eq!(inv.path, expected);
        if inv.path == "/😀" {
            let base = url::Url::parse(&format!("http://127.0.0.1:9{}", inv.path)).unwrap();
            assert_eq!(base.path(), "/%F0%9F%98%80");
        }
        let pid = worker.pid.load(Ordering::SeqCst);
        worker.shutdown().await;
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
        assert_eq!(capacity.available_permits(), 2);
    }
    let worker = start(&[("/(a)", "$9")], capacity.clone()).await;
    assert!(
        worker
            .rewrite(
                "/other",
                Instant::now() + Duration::from_secs(1),
                CancellationToken::new()
            )
            .await
            .is_ok()
    );
    assert!(matches!(
        worker
            .rewrite(
                "/a",
                Instant::now() + Duration::from_secs(1),
                CancellationToken::new()
            )
            .await,
        Err(Failure::InvalidReplacement)
    ));
    assert!(
        worker
            .rewrite(
                "/other",
                Instant::now() + Duration::from_secs(1),
                CancellationToken::new()
            )
            .await
            .is_ok()
    );
    worker.shutdown().await;
    let worker = start(&[(".*?", "/a")], capacity.clone()).await;
    assert!(matches!(
        worker
            .rewrite(
                "/😀",
                Instant::now() + Duration::from_secs(1),
                CancellationToken::new()
            )
            .await,
        Err(Failure::InvalidUtf16)
    ));
    worker.shutdown().await;
    for (name, expected) in [
        ("malformed", Failure::Protocol),
        ("stale", Failure::Protocol),
        ("panic", Failure::Panic),
        ("death", Failure::ChildLost),
        ("oversized", Failure::OutputProfile),
        ("hang", Failure::Deadline),
    ] {
        let worker = start(&[], capacity.clone()).await;
        let pid = worker.pid.load(Ordering::SeqCst);
        assert_eq!(
            fault(
                &worker,
                name,
                Instant::now() + Duration::from_millis(40),
                CancellationToken::new()
            )
            .await,
            Err(expected),
            "{name}"
        );
        worker.shutdown().await;
        assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
        assert_eq!(capacity.available_permits(), 2);
    }
    let worker = start(&[("(a|aa)*", "ok")], capacity.clone()).await;
    assert!(matches!(
        worker
            .rewrite(
                &"a".repeat(20000),
                Instant::now() + Duration::from_secs(2),
                CancellationToken::new()
            )
            .await,
        Err(Failure::Resource)
    ));
    worker.shutdown().await;
    let worker = start(&[], capacity.clone()).await;
    let cancel = CancellationToken::new();
    let signal = cancel.clone();
    {
        let request = fault(
            &worker,
            "hang",
            Instant::now() + Duration::from_secs(2),
            cancel,
        );
        tokio::pin!(request);
        tokio::select! {biased;r=&mut request=>panic!("unexpected {r:?}"),_=tokio::time::sleep(Duration::from_millis(20))=>signal.cancel()};
        assert_eq!(request.await, Err(Failure::Cancelled));
    }
    worker.shutdown().await;
    assert_eq!(capacity.available_permits(), 2);
    let worker = Arc::new(start(&[], capacity.clone()).await);
    let pid = worker.pid.load(Ordering::SeqCst);
    let owned = worker.clone();
    let task = tokio::spawn(async move {
        fault(
            &owned,
            "hang",
            Instant::now() + Duration::from_secs(2),
            CancellationToken::new(),
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(2), worker.stopped.cancelled())
        .await
        .unwrap();
    assert_eq!(capacity.available_permits(), 2);
    assert!(!std::path::Path::new(&format!("/proc/{pid}")).exists());
    let mut bad = artifacts();
    bad.pins[0].sha256[0] ^= 1;
    assert_eq!(bad.verify(), Err(Failure::Artifact));
}

struct Stage {
    session: Session,
    fault: Option<&'static str>,
}
#[async_trait::async_trait]
impl serverless_core::RequestHandler for Stage {
    fn id(&self) -> serverless_core::HandlerId {
        serverless_core::HandlerId::Router
    }
    async fn on_request(
        &self,
        inv: &mut Invocation,
        _: &mut serverless_core::ExchangeState,
    ) -> Result<serverless_core::HandlerOutcome, serverless_core::HandlerFailure> {
        let outcome = if let Some(name) = self.fault {
            fault(&self.session, name, inv.deadline, inv.cancel.clone())
                .await
                .map(|rewritten| Candidate {
                    original: inv.path.clone(),
                    rewritten,
                })
        } else {
            self.session
                .rewrite(&inv.path, inv.deadline, inv.cancel.clone())
                .await
        };
        match outcome {
            Ok(candidate) => candidate
                .commit(inv)
                .map(|()| serverless_core::HandlerOutcome::Continue)
                .map_err(|_| serverless_core::HandlerFailure),
            Err(failure) => Ok(serverless_core::HandlerOutcome::Reject(
                failure
                    .approved_status()
                    .expect("test only maps explicitly approved classes"),
            )),
        }
    }
}
struct Counts {
    dispatch: AtomicUsize,
    complete: AtomicUsize,
    paths: std::sync::Mutex<Vec<String>>,
}
#[async_trait::async_trait]
impl serverless_core::Backend for Counts {
    async fn dispatch(
        &self,
        _: &Invocation,
        _: &serverless_core::ExchangeState,
        _: &serverless_core::Target,
    ) -> Result<serverless_core::GatewayResponse, serverless_core::DispatchError> {
        self.dispatch.fetch_add(1, Ordering::SeqCst);
        Ok(serverless_core::GatewayResponse {
            status: http::StatusCode::OK,
            headers: http::HeaderMap::new(),
            body: None,
            body_is_binary: false,
        })
    }
}
impl serverless_core::Finalizer for Counts {
    fn finalize(
        &self,
        inv: &Invocation,
        state: &serverless_core::ExchangeState,
        _: &serverless_core::GatewayResponse,
    ) {
        assert!(state.metrics.finalized);
        self.paths.lock().unwrap().push(inv.path.clone());
        self.complete.fetch_add(1, Ordering::SeqCst);
    }
}

struct BindingFactory(serverless_core::plan::Binding);
impl serverless_core::plan::HandlerFactory for BindingFactory {
    fn build(
        &self,
        _: &serverless_core::compat::Compatibility,
        _: &str,
    ) -> Result<serverless_core::plan::Binding, serverless_core::InitError> {
        Ok(self.0.clone())
    }
}
struct Deny;
#[async_trait::async_trait]
impl serverless_core::RequestHandler for Deny {
    fn id(&self) -> serverless_core::HandlerId {
        serverless_core::HandlerId::Correlation
    }
    async fn on_request(
        &self,
        _: &mut Invocation,
        _: &mut serverless_core::ExchangeState,
    ) -> Result<serverless_core::HandlerOutcome, serverless_core::HandlerFailure> {
        Ok(serverless_core::HandlerOutcome::Reject(
            StatusError::missing_identity(),
        ))
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "owned JRE plus real core pipeline; requires WP12_ROUTER_ARTIFACTS"]
async fn approved_errors_rollback_zero_dispatch_deadline_and_exactly_once_completion() {
    use serverless_core::{plan::*, *};
    for (pattern, replacement, path, injected, deny, status) in [
        ("/(a)", "$9", "/a", None, false, 500),
        (".*?", "/a", "/😀", None, false, 500),
        ("/x", "/ok", "/x", Some("malformed"), false, 500),
        ("/x", "/ok", "/x", Some("panic"), false, 500),
        ("/x", "/ok", "/x", Some("hang"), false, 504),
        ("/😀", "/😀", "/😀", None, false, 200),
        ("/x", "/ok", "/x", None, true, 401),
    ] {
        let capacity = Arc::new(Semaphore::new(1));
        let session = start(&[(pattern, replacement)], capacity.clone()).await;
        let stopped = session.stopped.clone();
        let counts = Arc::new(Counts {
            dispatch: AtomicUsize::new(0),
            complete: AtomicUsize::new(0),
            paths: std::sync::Mutex::new(Vec::new()),
        });
        let mut registry = Registry::default();
        registry.register(
            HandlerId::Router,
            Arc::new(BindingFactory(Binding::Request(
                Arc::new(Stage {
                    session,
                    fault: injected,
                }),
                RequestCapabilities::default(),
            ))),
        );
        registry.register(
            HandlerId::Correlation,
            Arc::new(BindingFactory(Binding::Request(
                Arc::new(Deny),
                RequestCapabilities::default(),
            ))),
        );
        registry.register(
            HandlerId::Proxy,
            Arc::new(BindingFactory(Binding::Dispatch(
                TargetRoutes::compile([("/*@get".into(), Target::HttpService("trusted".into()))])
                    .unwrap(),
            ))),
        );
        let class = |id| {
            compat::COMPATIBILITY
                .iter()
                .find(|c| c.id == id)
                .unwrap()
                .class
        };
        let chain = if deny {
            vec!["rewrite", "deny", "proxy"]
        } else {
            vec!["rewrite", "proxy"]
        };
        let plan=ExecutionPlan::compile(json!({"handlers":[format!("{}@rewrite",class(HandlerId::Router)),format!("{}@deny",class(HandlerId::Correlation)),format!("{}@proxy",class(HandlerId::Proxy))],"defaultHandlers":chain}),json!({}),&registry).unwrap();
        let pipeline = Pipeline::new(Arc::new(plan), vec![counts.clone()]);
        let mut inv = invocation(path);
        if injected == Some("hang") {
            inv.deadline = Instant::now() + Duration::from_millis(40);
        }
        let result = pipeline.execute(inv, &*counts).await;
        assert_eq!(result.status.as_u16(), status, "{path}/{injected:?}");
        if status == 500 {
            assert_eq!(
                result.body.unwrap(),
                Failure::Protocol.approved_status().unwrap().to_bytes()
            );
            assert_eq!(&*counts.paths.lock().unwrap(), &[path]);
        }
        assert_eq!(
            counts.dispatch.load(Ordering::SeqCst),
            usize::from(status == 200)
        );
        assert_eq!(counts.complete.load(Ordering::SeqCst), 1);
        drop(pipeline);
        drop(registry);
        tokio::time::timeout(Duration::from_secs(2), stopped.cancelled())
            .await
            .unwrap();
        assert_eq!(capacity.available_permits(), 1);
    }
    let session = start(&[("/x", "/fresh")], Arc::new(Semaphore::new(1))).await;
    for cancel in [false, true] {
        let mut inv = invocation("/x");
        let candidate = session
            .rewrite(&inv.path, inv.deadline, inv.cancel.clone())
            .await
            .unwrap();
        if cancel {
            inv.cancel.cancel()
        } else {
            inv.deadline = Instant::now();
        }
        assert!(candidate.commit(&mut inv).is_err());
        assert_eq!(inv.path, "/x");
    }
    session.shutdown().await;
}
