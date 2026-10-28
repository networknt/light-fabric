use async_trait::async_trait;
use serde_json::json;
use serverless_core::{handlers::local_admin::LocalAdminHandler, plan::*, *};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::time::Instant;
#[derive(Default)]
struct Counts {
    backend: AtomicUsize,
    finalized: AtomicUsize,
}
fn n(n: &AtomicUsize) -> usize {
    n.load(Ordering::SeqCst)
}
impl Finalizer for Counts {
    fn finalize(&self, _: &Invocation, _: &ExchangeState, _: &GatewayResponse) {
        self.finalized.fetch_add(1, Ordering::SeqCst);
    }
}
struct Auth;
#[async_trait]
impl RequestHandler for Auth {
    fn id(&self) -> HandlerId {
        HandlerId::Jwt
    }
    async fn on_request(
        &self,
        _: &mut Invocation,
        s: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        s.principal = Some(Principal::from_verified(
            "test-only".into(),
            None,
            None,
            json!({}),
        ));
        Ok(HandlerOutcome::Continue)
    }
}
struct Factory(Binding);
impl HandlerFactory for Factory {
    fn build(
        &self,
        _: &serverless_core::compat::Compatibility,
        _: &str,
    ) -> Result<Binding, InitError> {
        Ok(self.0.clone())
    }
}
fn install(r: &mut Registry, id: HandlerId, b: Binding) {
    r.register(id, Arc::new(Factory(b)));
}
fn config(ids: &[(HandlerId, &str)], exec: &[&str]) -> serde_json::Value {
    json!({"handlers":ids.iter().map(|(id,alias)|format!("{}@{alias}",serverless_core::compat::COMPATIBILITY.iter().find(|c|c.id==*id).unwrap().class)).collect::<Vec<_>>(),"defaultHandlers":exec})
}
fn compile(v: serde_json::Value, r: &Registry) -> Arc<ExecutionPlan> {
    Arc::new(ExecutionPlan::compile(v, json!({}), r).unwrap())
}
fn registry() -> Registry {
    let mut r = Registry::default();
    install(
        &mut r,
        HandlerId::Proxy,
        Binding::Dispatch(
            TargetRoutes::compile([("/*@get".into(), Target::Function("test-only".into()))])
                .unwrap(),
        ),
    );
    r
}
fn invocation() -> Invocation {
    Invocation {
        method: http::Method::GET,
        raw_path: "/x".into(),
        path: "/x".into(),
        raw_query: None,
        query: MultiMap::default(),
        headers: http::HeaderMap::new(),
        body: None,
        body_was_base64: false,
        source: SourceMetadata::default(),
        request_id: "test-only".into(),
        deadline: Instant::now() + Duration::from_secs(1),
        response_deadline: Instant::now() + Duration::from_secs(2),
        cancel: tokio_util::sync::CancellationToken::new(),
        path_params: Default::default(),
        route: None,
        original: serde_json::Value::Null,
    }
}
struct FakeBackend(Arc<Counts>);
impl FakeBackend {
    fn new(c: &Arc<Counts>) -> Self {
        Self(c.clone())
    }
}
#[async_trait]
impl Backend for FakeBackend {
    async fn dispatch(
        &self,
        _: &Invocation,
        _: &ExchangeState,
        _: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        self.0.backend.fetch_add(1, Ordering::SeqCst);
        panic!("local terminals must not dispatch")
    }
}
fn installed() -> Option<&'static str> {
    Some("info")
}
#[tokio::test]
async fn sanitizer_decoded_body_headers_selectors_and_rejection_are_request_local() {
    use serverless_core::handlers::sanitizer::SanitizerHandler;
    let handler = SanitizerHandler::from_value(&json!({"enabled":true,"bodyEnabled":true,"headerEnabled":true,"bodyEncoder":"html","headerEncoder":"html","bodyAttributesToIgnore":["skip"],"headerAttributesToEncode":["x-test"]})).unwrap();
    for base64 in [false, true] {
        let mut inv = invocation();
        inv.method = http::Method::POST;
        inv.body_was_base64 = base64;
        inv.body = Some(bytes::Bytes::from_static(
            br#" {"value":"<b>","skip":"<x>","nested":{"v":"<i>"}} "#,
        ));
        inv.headers.insert("x-test", "<h>".parse().unwrap());
        inv.headers.insert("x-other", "<other>".parse().unwrap());
        let mut state = ExchangeState::default();
        assert!(matches!(
            handler.on_request(&mut inv, &mut state).await.unwrap(),
            HandlerOutcome::Continue
        ));
        assert_eq!(
            inv.body.unwrap().as_ref(),
            br#"{"value":"&lt;b&gt;","skip":"<x>","nested":{"v":"&lt;i&gt;"}}"#
        );
        assert_eq!(inv.body_was_base64, base64);
        assert_eq!(inv.headers["x-test"], "&lt;h&gt;");
        assert_eq!(inv.headers["x-other"], "<other>");
    }
    let mut bad = invocation();
    bad.method = http::Method::PATCH;
    bad.body = Some(bytes::Bytes::from_static(b"{bad"));
    bad.headers.insert(
        "content-type",
        "application/json; x=\"quoted\"".parse().unwrap(),
    );
    let HandlerOutcome::Reject(error) = handler
        .on_request(&mut bad, &mut ExchangeState::default())
        .await
        .unwrap()
    else {
        panic!("malformed body must reject")
    };
    assert_eq!(error.to_bytes().as_ref(), br#"{"statusCode":400,"code":"ERR10015","message":"CONTENT_TYPE_MISMATCH","description":"Either the Content-Type header application/json; x="quoted" does not match the body, or the body was serialized incorrectly.","severity":"ERROR"}"#);
    let mut plain = invocation();
    plain.method = http::Method::PUT;
    plain.body = Some(bytes::Bytes::from_static(b"plain <b>"));
    assert!(matches!(
        handler
            .on_request(&mut plain, &mut ExchangeState::default())
            .await
            .unwrap(),
        HandlerOutcome::Continue
    ));
    assert_eq!(plain.body.unwrap().as_ref(), b"plain <b>");
}
#[tokio::test]
async fn real_local_health_and_protected_payloads_use_compiled_terminal_and_zero_dispatch() {
    for (id, handler, expected, content_type) in [
        (
            HandlerId::Health,
            LocalAdminHandler::health(),
            "OK",
            "text/plain; charset=utf-8",
        ),
        (
            HandlerId::Info,
            LocalAdminHandler::info(),
            r#"{"runtime":"rust","modules":["serverless-core","serverless-aws"]}"#,
            "application/json",
        ),
        (
            HandlerId::GetLogger,
            LocalAdminHandler::logger(installed),
            r#"{"filter":"info"}"#,
            "application/json",
        ),
    ] {
        let counts = Arc::new(Counts::default());
        let mut registry = Registry::default();
        registry.register(id, Arc::new(handler));
        install(
            &mut registry,
            HandlerId::Jwt,
            Binding::Request(
                Arc::new(Auth),
                RequestCapabilities {
                    establishes_verified_identity: true,
                    requires_verified_identity: false,
                },
            ),
        );
        let plan = compile(
            config(
                &[(HandlerId::Jwt, "auth"), (id, "terminal")],
                &["auth", "terminal"],
            ),
            &registry,
        );
        let pipeline = Pipeline::new(plan, vec![counts.clone()]);
        let response = pipeline
            .execute(invocation(), &FakeBackend::new(&counts))
            .await;
        assert_eq!(response.status, 200);
        assert_eq!(response.body.as_deref(), Some(expected.as_bytes()));
        assert_eq!(response.headers["content-type"], content_type);
        assert!(!response.body_is_binary);
        assert_eq!(n(&counts.backend), 0);
        assert_eq!(n(&counts.finalized), 1);
        if id != HandlerId::Health {
            let plan = compile(config(&[(id, "terminal")], &["terminal"]), &registry);
            let response = Pipeline::new(plan, vec![])
                .execute(invocation(), &FakeBackend::new(&counts))
                .await;
            assert_eq!(response.status, 401);
            assert_eq!(n(&counts.backend), 0);
        }
    }
}
#[test]
fn validator_consumer_requires_preceding_specification_after_alias_expansion() {
    use async_trait::async_trait;
    struct Consumer;
    #[async_trait]
    impl RequestHandler for Consumer {
        fn id(&self) -> HandlerId {
            HandlerId::Validator
        }
        fn operation_authorization_role(&self) -> OperationAuthorizationRole {
            OperationAuthorizationRole::MetadataConsumer
        }
        async fn on_request(
            &self,
            _: &mut Invocation,
            _: &mut ExchangeState,
        ) -> Result<HandlerOutcome, HandlerFailure> {
            panic!("compiler test must not execute")
        }
    }
    let mut r = registry();
    r.register(
        HandlerId::Specification,
        Arc::new(
            serverless_core::handlers::specification::SpecificationHandler::from_source(
                br#"{"openapi":"3.0.3","paths":{"/x":{"get":{}}}}"#,
                gateway_policy::openapi_operation::SourceFormat::Json,
            )
            .unwrap(),
        ),
    );
    install(
        &mut r,
        HandlerId::Validator,
        Binding::Request(Arc::new(Consumer), RequestCapabilities::default()),
    );
    for exec in [vec!["validate", "proxy"], vec!["validate", "spec", "proxy"]] {
        assert!(
            ExecutionPlan::compile(
                config(
                    &[
                        (HandlerId::Specification, "spec"),
                        (HandlerId::Validator, "validate"),
                        (HandlerId::Proxy, "proxy")
                    ],
                    &exec
                ),
                json!({}),
                &r
            )
            .is_err()
        );
    }
    for exec in [
        vec!["spec", "validate", "proxy"],
        vec!["spec", "validate", "validate", "proxy"],
    ] {
        assert!(
            ExecutionPlan::compile(
                config(
                    &[
                        (HandlerId::Specification, "spec"),
                        (HandlerId::Validator, "validate"),
                        (HandlerId::Proxy, "proxy")
                    ],
                    &exec
                ),
                json!({}),
                &r
            )
            .is_ok()
        );
    }
}
