//! Actual offline/startup/compiler/pipeline/ALB factor wiring, synthetic inputs only.
#[path = "../../../crates/serverless-core/tests/security_support/mod.rs"]
mod security_support;
use async_trait::async_trait;
use config_loader::EmbeddedConfigFile;
use light_lambda_runtime::{init::*, service::handle};
use serde_json::{Value, json};
use serverless_aws::{
    invoke::{Clock, SystemClock},
    target::GatewayIdentity,
    v1::{EventConfig, build_v1_event},
};
use serverless_core::{compat::COMPATIBILITY, *};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};
static EMBEDDED: &[EmbeddedConfigFile] = &[
    EmbeddedConfigFile {
        name: "server.yml",
        content: "ip: 127.0.0.1\nhttpPort: 0\nhttpsPort: 0\nenableHttp: false\nenableHttps: false\nserviceId: test-only\nenableRegistry: false\n",
    },
    EmbeddedConfigFile {
        name: "light-lambda.yml",
        content: "stage: test-only\ningressTimeoutMs: 1000\nresponseReserveMs: 200\npostReserveMs: 50\n",
    },
    EmbeddedConfigFile {
        name: "lambda-proxy.yml",
        content: "region: us-east-1\nfunctions:\n  /*@get: TestFn\n",
    },
];
#[derive(Default)]
struct BackendProbe {
    calls: AtomicUsize,
    events: Mutex<Vec<Value>>,
}
#[async_trait]
impl Backend for BackendProbe {
    async fn dispatch(
        &self,
        inv: &Invocation,
        state: &ExchangeState,
        _: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.events.lock().unwrap().push(
            build_v1_event(
                inv,
                state,
                &EventConfig::new("test-only".into()).unwrap(),
                SystemClock.now(),
            )
            .unwrap(),
        );
        Ok(GatewayResponse {
            status: http::StatusCode::OK,
            headers: http::HeaderMap::new(),
            body: Some(bytes::Bytes::from_static(b"ok")),
            body_is_binary: false,
        })
    }
}
#[derive(Default)]
struct CompletionProbe(Mutex<Vec<(u16, u32)>>);
impl Finalizer for CompletionProbe {
    fn finalize(&self, _: &Invocation, state: &ExchangeState, response: &GatewayResponse) {
        assert!(state.metrics.finalized);
        self.0
            .lock()
            .unwrap()
            .push((response.status.as_u16(), state.metrics.dispatches));
    }
}
fn class(id: HandlerId) -> &'static str {
    COMPATIBILITY.iter().find(|c| c.id == id).unwrap().class
}
fn write(dir: &std::path::Path, file: &str, value: Value) {
    std::fs::write(dir.join(file), serde_yaml::to_string(&value).unwrap()).unwrap();
}
fn basic() -> Value {
    json!({"enabled":true,"enableAD":false,"users":[{"username":"test-user","password":"test-only-password","paths":["/b","/mixed"]}]})
}
fn api() -> Value {
    json!({"enabled":true,"hashEnabled":false,"pathPrefixAuths":[{"pathPrefix":"/a","headerName":"x-test-key","apiKey":"test-only-key"},{"pathPrefix":"/mixed","headerName":"x-test-key","apiKey":"test-only-key"}]})
}
fn setup(
    table: Value,
    basic_config: Value,
    api_config: Value,
    basic_alias: bool,
    api_alias: bool,
) -> (
    tempfile::TempDir,
    InitInput,
    Arc<BackendProbe>,
    Arc<CompletionProbe>,
) {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "security.yml",
        json!({"enabled":false,"enableVerifyJwt":false,"enableVerifyScope":false}),
    );
    write(
        dir.path(),
        "unified-security.yml",
        json!({"enabled":true,"pathPrefixAuths":table}),
    );
    write(dir.path(), "basic-auth.yml", basic_config);
    write(dir.path(), "apikey.yml", api_config);
    let mut handlers = vec![
        format!("{}@security", class(HandlerId::Security)),
        format!("{}@proxy", class(HandlerId::Proxy)),
    ];
    if basic_alias {
        handlers.push(format!("{}@basic", class(HandlerId::Basic)));
    }
    if api_alias {
        handlers.push(format!("{}@apikey", class(HandlerId::ApiKey)));
    }
    write(
        dir.path(),
        "handler.yml",
        json!({"enabled":true,"handlers":handlers,"defaultHandlers":["security","proxy"]}),
    );
    let backend = Arc::new(BackendProbe::default());
    let completion = Arc::new(CompletionProbe::default());
    let input = InitInput {
        embedded: EMBEDDED,
        local_dir: dir.path().join("absent-local"),
        default_dir: None,
        external_dir: dir.path().into(),
        environment: Environment {
            region: Some("us-east-1".into()),
            function_name: "test-only-gateway".into(),
            function_version: "1".into(),
            memory_mb: 128,
            allow_insecure_test_endpoints: false,
        },
        gateway_identity: GatewayIdentity {
            function_name: "test-only-gateway".into(),
            region: "us-east-1".into(),
            account_id: "000000000000".into(),
        },
        handlers: vec![],
        finalizers: vec![completion.clone()],
        backend: BackendConfiguration::Injected(backend.clone()),
    };
    (dir, input, backend, completion)
}
fn event(path: &str, auth: Option<&str>, key: Option<&str>) -> lambda_runtime::LambdaEvent<Value> {
    let mut headers = json!({});
    if let Some(auth) = auth {
        headers["authorization"] = json!([auth]);
    }
    if let Some(key) = key {
        headers["x-test-key"] = json!([key]);
    }
    let mut context = lambda_runtime::Context::default();
    context.request_id = "test-only-factor".into();
    context.deadline = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 5000;
    lambda_runtime::LambdaEvent::new(
        json!({"httpMethod":"GET","path":path,"multiValueHeaders":headers,"requestContext":{"elb":{"targetGroupArn":"test-only"},"authorizer":{"principalId":"untrusted-envelope-marker"}}}),
        context,
    )
}
#[tokio::test]
async fn real_factors_succeed_deny_and_remain_isolated_across_warm_invocations() {
    let (dir, input, backend, completion) = setup(
        json!([{"prefix":"/b","basic":true},{"prefix":"/a","apikey":true}]),
        basic(),
        api(),
        true,
        true,
    );
    let report = validate_configuration(&input).await;
    assert!(report.valid, "{:?}", report.errors);
    let snapshot = initialize(input).await.unwrap();
    assert!(
        snapshot.security.is_none(),
        "Basic/API-key do not activate JWT/JWKS"
    );
    assert_eq!(
        report.config_digest.as_deref(),
        Some(snapshot.config_digest())
    );
    // Snapshot does not reread credentials or activation after Init.
    write(dir.path(), "basic-auth.yml", json!({"enabled":false}));
    write(dir.path(), "apikey.yml", json!({"enabled":false}));
    for _ in 0..4 {
        for (path, auth, key, status) in [
            ("/b", Some("Basic test-user:test-only-password"), None, 200),
            ("/b", Some("Basic test-user:wrong"), None, 401),
            ("/a", None, Some("test-only-key"), 200),
            ("/a", None, Some("wrong"), 401),
            ("/b", None, None, 401),
            ("/unconfigured", None, None, 400),
        ] {
            let before = backend.calls.load(Ordering::SeqCst);
            let callbacks = completion.0.lock().unwrap().len();
            let response = handle(snapshot.clone(), event(path, auth, key))
                .await
                .unwrap();
            assert_eq!(response["statusCode"], status);
            if status == 200 {
                assert!(
                    response["multiValueHeaders"]
                        .get("www-authenticate")
                        .is_none(),
                    "previous challenge must not leak across warm invocations"
                );
            }
            assert_eq!(
                backend.calls.load(Ordering::SeqCst),
                before + usize::from(status == 200)
            );
            let observed = completion.0.lock().unwrap();
            assert_eq!(observed.len(), callbacks + 1);
            assert_eq!(observed.last(), Some(&(status, u32::from(status == 200))));
            if status == 200 {
                let events = backend.events.lock().unwrap();
                let last = events.last().unwrap();
                assert!(last.pointer("/requestContext/authorizer").is_none());
                assert!(!last.to_string().contains("untrusted-envelope-marker"));
                if path == "/a" {
                    assert!(last["multiValueHeaders"].get("authorization").is_none());
                }
            }
        }
    }
    assert_eq!(backend.calls.load(Ordering::SeqCst), 8);
    assert_eq!(completion.0.lock().unwrap().len(), 24);
}
#[tokio::test]
async fn incomplete_disabled_and_wrong_alias_bindings_reject_offline_and_startup_before_dispatch() {
    for (basic_alias, api_alias, basic_config, api_config) in [
        (false, true, basic(), api()),
        (true, false, basic(), api()),
        (true, true, json!({"enabled":false}), api()),
        (true, true, basic(), json!({"enabled":false})),
    ] {
        let (_dir, input, backend, _) = setup(
            json!([{"prefix":"/mixed","basic":true,"apikey":true}]),
            basic_config,
            api_config,
            basic_alias,
            api_alias,
        );
        let offline = validate_configuration(&input).await;
        assert!(!offline.valid);
        let error = initialize(input).await.err().unwrap();
        assert_eq!(offline.errors, vec![error.message().to_string()]);
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
    }
    let (dir, input, backend, _) = setup(
        json!([{"prefix":"/b","basic":true}]),
        basic(),
        api(),
        true,
        true,
    );
    write(
        dir.path(),
        "handler.yml",
        json!({"enabled":true,"handlers":[format!("{}@security",class(HandlerId::Security)),format!("{}@proxy",class(HandlerId::Proxy)),format!("{}@basic",class(HandlerId::Basic)),format!("{}@basic",class(HandlerId::Proxy))],"defaultHandlers":["security","proxy"]}),
    );
    assert!(initialize(input).await.is_err());
    assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
}
#[tokio::test]
async fn basic_before_api_key_and_jwt_only_scheme_restriction_preserve_precedence() {
    let (_dir, input, backend, _) = setup(
        json!([{"prefix":"/mixed","basic":true,"apikey":true}]),
        basic(),
        api(),
        true,
        true,
    );
    let snapshot = initialize(input).await.unwrap();
    assert_eq!(
        handle(
            snapshot.clone(),
            event(
                "/mixed",
                Some("Basic test-user:test-only-password"),
                Some("wrong")
            )
        )
        .await
        .unwrap()["statusCode"],
        200
    );
    assert_eq!(
        handle(
            snapshot.clone(),
            event(
                "/mixed",
                Some("Basic test-user:wrong"),
                Some("test-only-key")
            )
        )
        .await
        .unwrap()["statusCode"],
        401
    );
    assert_eq!(
        handle(snapshot, event("/mixed", None, Some("test-only-key")))
            .await
            .unwrap()["statusCode"],
        401
    );
    assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
    let (server, key) = security_support::server(false).await;
    let (dir, input, backend, _) = setup(
        json!([{"prefix":"/mixed","basic":true,"jwt":true,"apikey":true},{"prefix":"/jwt","jwt":true}]),
        basic(),
        api(),
        true,
        true,
    );
    let mut config = security_support::config();
    config.enable_verify_scope = false;
    write(
        dir.path(),
        "security.yml",
        serde_json::to_value(config).unwrap(),
    );
    write(
        dir.path(),
        "client.yml",
        json!({"request":{"timeout":2000,"connectTimeout":2000},"oauth":{"token":{"key":{"server_url":server.url,"uri":"/jwks","enableHttp2":false}}}}),
    );
    let snapshot = initialize(input).await.unwrap();
    let token = security_support::signed(&key, security_support::claims("test-only-jwt-user"));
    assert_eq!(
        handle(
            snapshot.clone(),
            event("/mixed", Some(&format!("Bearer {token}")), None)
        )
        .await
        .unwrap()["statusCode"],
        200
    );
    assert_eq!(
        handle(
            snapshot.clone(),
            event("/mixed", None, Some("test-only-key"))
        )
        .await
        .unwrap()["statusCode"],
        401
    );
    assert_eq!(
        handle(
            snapshot.clone(),
            event("/jwt", Some("Basic test-user:test-only-password"), None)
        )
        .await
        .unwrap()["statusCode"],
        401
    );
    assert_eq!(
        handle(
            snapshot,
            event(
                "/mixed",
                Some("Basic test-user:wrong"),
                Some("test-only-key")
            )
        )
        .await
        .unwrap()["statusCode"],
        401
    );
    assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
    assert_eq!(server.requests(), 1);
}
