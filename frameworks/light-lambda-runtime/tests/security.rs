#[path = "../../../crates/serverless-core/tests/security_support/mod.rs"]
mod security_support;
use async_trait::async_trait;
use config_loader::EmbeddedConfigFile;
use light_lambda_runtime::{init::*, service::handle};
use security_support::{claims, config, jwks, server, signed};
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
use tempfile::TempDir;

static EMBEDDED: &[EmbeddedConfigFile] = &[
    EmbeddedConfigFile {
        name: "server.yml",
        content: "ip: 127.0.0.1\nhttpPort: 0\nhttpsPort: 0\nenableHttp: false\nenableHttps: false\nserviceId: test-only\nenableRegistry: false\n",
    },
    EmbeddedConfigFile {
        name: "light-lambda.yml",
        content: "stage: test-only\ningressTimeoutMs: 1000\nresponseReserveMs: 200\npostReserveMs: 20\n",
    },
    EmbeddedConfigFile {
        name: "lambda-proxy.yml",
        content: "functions:\n  /*@get: test-only-business\nregion: us-east-1\n",
    },
];
#[derive(Default)]
struct Events {
    calls: AtomicUsize,
    events: Mutex<Vec<Value>>,
}
#[async_trait]
impl Backend for Events {
    async fn dispatch(
        &self,
        inv: &Invocation,
        st: &ExchangeState,
        _: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.events.lock().unwrap().push(
            build_v1_event(
                inv,
                st,
                &EventConfig::new("test-only".into()).unwrap(),
                SystemClock.now(),
            )
            .unwrap(),
        );
        Ok(GatewayResponse {
            status: http::StatusCode::OK,
            headers: http::HeaderMap::new(),
            body: None,
            body_is_binary: false,
        })
    }
}
fn class(id: HandlerId) -> &'static str {
    COMPATIBILITY.iter().find(|c| c.id == id).unwrap().class
}

#[tokio::test]
async fn router_before_real_jwt_rejection_has_zero_http_and_lambda_dispatch_and_no_warm_target_leak()
 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (jwks, key) = server(false).await;
    let (dir, mut input, backend) = setup(Some(&jwks), false);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let http_calls = Arc::new(AtomicUsize::new(0));
    let calls = http_calls.clone();
    let task = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut bytes = [0; 1024];
            loop {
                let n = stream.read(&mut bytes).await.unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&bytes[..n]);
                if request.windows(4).any(|s| s == b"\r\n\r\n") {
                    break;
                }
            }
            calls.fetch_add(1, Ordering::SeqCst);
            assert!(
                !String::from_utf8_lossy(&request)
                    .to_lowercase()
                    .contains("service_id")
            );
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\nhttp-ok",
                )
                .await
                .unwrap();
        }
    });
    std::fs::write(
        dir.path().join("router.yml"),
        "enabled: true\nhttpsEnabled: false\nmaxRequestTime: 1000\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("direct-registry.yml"),
        json!({"directUrls":{"service":format!("http://{address}")}}).to_string(),
    )
    .unwrap();
    std::fs::write(dir.path().join("handler.yml"),json!({"handlers":[format!("{}@router",class(HandlerId::Router)),format!("{}@jwt",class(HandlerId::Jwt)),format!("{}@proxy",class(HandlerId::Proxy))],"defaultHandlers":["router","jwt","proxy"]}).to_string()).unwrap();
    input.handlers.push(HandlerRegistration {
        id: HandlerId::Router,
        config_file: "router.yml",
        factory: Some(Arc::new(
            light_lambda_runtime::router_policy::RouterPolicyProvider,
        )),
    });
    let snapshot = initialize(input).await.unwrap();
    for auth in [None, Some("Bearer invalid")] {
        let mut event = event("/x", auth);
        event.payload["multiValueHeaders"]["service_id"] = json!(["service"]);
        assert_eq!(
            handle(snapshot.clone(), event).await.unwrap()["statusCode"],
            401
        );
        assert_eq!(http_calls.load(Ordering::SeqCst), 0);
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
    }
    let auth = format!("Bearer {}", signed(&key, claims("router-user")));
    let mut request = event("/x", Some(&auth));
    request.payload["multiValueHeaders"]["service_id"] = json!(["service"]);
    let response = handle(snapshot.clone(), request).await.unwrap();
    assert_eq!(response["statusCode"], 200);
    assert_eq!(response["body"], "http-ok");
    assert_eq!(http_calls.load(Ordering::SeqCst), 1);
    assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        handle(snapshot, event("/x", Some(&auth))).await.unwrap()["statusCode"],
        200
    );
    assert_eq!(http_calls.load(Ordering::SeqCst), 1);
    assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
    assert_eq!(jwks.requests(), 1);
    task.abort();
}
fn setup(server: Option<&jwks::Server>, unified: bool) -> (TempDir, InitInput, Arc<Events>) {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(Events::default());
    let id = if unified {
        HandlerId::Security
    } else {
        HandlerId::Jwt
    };
    std::fs::write(dir.path().join("handler.yml"),json!({"enabled":true,"handlers":[format!("{}@auth",class(id)),format!("{}@proxy",class(HandlerId::Proxy))],"defaultHandlers":["auth","proxy"]}).to_string()).unwrap();
    let mut cfg = config();
    cfg.skip_path_prefixes = vec!["/public".into()];
    std::fs::write(
        dir.path().join("security.yml"),
        serde_yaml::to_string(&cfg).unwrap(),
    )
    .unwrap();
    if unified {
        std::fs::write(
            dir.path().join("unified-security.yml"),
            json!({"anonymousPrefixes":["/public"],"pathPrefixAuths":[{"prefix":"/x","jwt":true}]})
                .to_string(),
        )
        .unwrap();
    }
    if let Some(server) = server {
        std::fs::write(dir.path().join("client.yml"),json!({"request":{"timeout":2000,"connectTimeout":2000},"oauth":{"token":{"key":{"server_url":server.url,"uri":"/jwks","enableHttp2":false}}}}).to_string()).unwrap();
    }
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
            account_id: "123456789012".into(),
        },
        handlers: vec![],
        finalizers: vec![],
        backend: BackendConfiguration::Injected(backend.clone()),
    };
    (dir, input, backend)
}
fn event(path: &str, auth: Option<&str>) -> lambda_runtime::LambdaEvent<Value> {
    let mut context = lambda_runtime::Context::default();
    context.request_id = "test-request".into();
    context.deadline = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 5000;
    let mut headers = json!({});
    if let Some(auth) = auth {
        headers["Authorization"] = json!([auth]);
    }
    lambda_runtime::LambdaEvent::new(
        json!({"httpMethod":"GET","path":path,"requestContext":{"elb":{"targetGroupArn":"test-only"},"authorizer":{"principalId":"untrusted-envelope-marker"}},"multiValueHeaders":headers}),
        context,
    )
}

#[tokio::test]
async fn wp12_parsed_factors_stay_unavailable_until_real_bindings_qualify() {
    for enabled in [false, true] {
        for factor in ["basic", "apikey"] {
            let (server, _) = server(false).await;
            let (dir, input, backend) = setup(Some(&server), true);
            let mut prefix = json!({"prefix":"/unused","jwt":true});
            prefix[factor] = json!(true);
            let settings = json!({"enabled":enabled,"pathPrefixAuths":[prefix]});
            assert!(
                serverless_core::handlers::security::UnifiedSettings::from_value(&settings).is_ok()
            );
            std::fs::write(
                dir.path().join("unified-security.yml"),
                settings.to_string(),
            )
            .unwrap();
            let offline = validate_configuration(&input).await;
            assert!(!offline.valid);
            let error = initialize(input).await.err().unwrap();
            assert!(matches!(
                error,
                light_lambda_runtime::init_error::InitError::InvalidConfiguration(_)
            ));
            assert_eq!(offline.errors, vec![error.message().to_string()]);
            assert_eq!(
                server.requests(),
                0,
                "unavailable factors must fail before JWKS bootstrap"
            );
            assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
        }
    }
}

#[tokio::test]
async fn wp12_parser_expansion_preserves_real_jwt_denials_with_zero_dispatch() {
    let (server, key) = server(false).await;
    let (dir, input, backend) = setup(Some(&server), true);
    std::fs::write(
        dir.path().join("unified-security.yml"),
        json!({"pathPrefixAuths":[{"prefix":"/x","jwt":true,"basic":false,"apikey":false}]})
            .to_string(),
    )
    .unwrap();
    let snapshot = initialize(input).await.unwrap();
    let mut expired_claims = claims("test-only-expired");
    expired_claims["exp"] = json!(jwks::now() - 120);
    let expired = format!("Bearer {}", signed(&key, expired_claims));
    for authorization in [
        None,
        Some("Basic dGVzdC11c2VyOnRlc3Qtb25seS1wYXNzd29yZA=="),
        Some("Unknown test-only"),
        Some(expired.as_str()),
    ] {
        let response = handle(snapshot.clone(), event("/x", authorization))
            .await
            .unwrap();
        assert_eq!(response["statusCode"], 401);
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
    }
    let response = handle(snapshot, event("/unconfigured", None))
        .await
        .unwrap();
    assert_eq!(response["statusCode"], 400);
    assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        server.requests(),
        1,
        "bootstrap only; denials do not refresh keys"
    );
}

#[tokio::test]
async fn real_runtime_factories_alternate_users_denials_and_anonymous_without_authorizer_leakage() {
    for unified in [false, true] {
        let (server, key) = server(false).await;
        let (_dir, input, backend) = setup(Some(&server), unified);
        let snapshot = initialize(input).await.unwrap();
        let a = signed(&key, claims("user-A"));
        let b = signed(&key, claims("user-B"));
        let mut c = claims("expired-user");
        c["exp"] = json!(jwks::now() - 120);
        let expired = signed(&key, c);
        for i in 0..20 {
            let (path, auth, status, principal) = match i % 5 {
                0 => ("/x", Some(format!("Bearer {a}")), 200, Some("user-A")),
                1 => ("/x", Some(format!("Bearer {b}")), 200, Some("user-B")),
                2 => ("/x", None, 401, None),
                3 => ("/public", None, 200, None),
                _ => ("/x", Some(format!("Bearer {expired}")), 401, None),
            };
            let before = backend.calls.load(Ordering::SeqCst);
            let response = handle(snapshot.clone(), event(path, auth.as_deref()))
                .await
                .unwrap();
            assert_eq!(response["statusCode"], status);
            if status == 200 {
                assert_eq!(backend.calls.load(Ordering::SeqCst), before + 1);
                let events = backend.events.lock().unwrap();
                let authorizer = events.last().unwrap().pointer("/requestContext/authorizer");
                match principal {
                    Some(user) => {
                        let authorizer = authorizer.unwrap();
                        assert_eq!(authorizer["principalId"], user);
                        assert_eq!(authorizer["claims"]["user_id"], user);
                        assert_eq!(authorizer["claims"]["sub"], user);
                        assert_eq!(authorizer["scope"], "read write");
                        assert_eq!(authorizer.as_object().unwrap().len(), 3);
                    }
                    None => assert!(authorizer.is_none()),
                }
            } else {
                assert_eq!(
                    backend.calls.load(Ordering::SeqCst),
                    before,
                    "denied real-runtime request must not dispatch"
                );
            }
        }
        assert_eq!(backend.calls.load(Ordering::SeqCst), 12);
        assert_eq!(server.requests(), 1);
        assert!(
            !backend
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|e| e.to_string().contains("untrusted-envelope-marker"))
        );
    }
}

#[tokio::test]
async fn runtime_init_rejects_unsupported_factors_keys_skew_and_selected_disabled_jwt() {
    for file_value in [
        ("security.yml", json!({"jwt":{"clockSkewInSeconds":61}})),
        ("security.yml", json!({"unknown":false})),
        ("security.yml", json!({"enableVerifySwt":true})),
        ("security.yml", json!({"enableRelaxedKeyValidation":true})),
        (
            "unified-security.yml",
            json!({"pathPrefixAuths":[{"prefix":"/unused","basic":true}]}),
        ),
        (
            "unified-security.yml",
            json!({"pathPrefixAuths":[{"prefix":"/unused","apikey":true}]}),
        ),
        (
            "unified-security.yml",
            json!({"pathPrefixAuths":[{"prefix":"/unused","swt":true}]}),
        ),
        (
            "unified-security.yml",
            json!({"pathPrefixAuths":[{"prefix":"/unused","ldap":true}]}),
        ),
        (
            "unified-security.yml",
            json!({"pathPrefixAuths":[{"prefix":"/x","jwt":"true"}]}),
        ),
        ("unified-security.yml", json!({"unknown":false})),
        ("security.yml", json!({"enableVerifyJwt":false})),
    ] {
        let (server, _) = server(false).await;
        let (dir, input, backend) = setup(Some(&server), true);
        std::fs::write(dir.path().join(file_value.0), file_value.1.to_string()).unwrap();
        assert!(initialize(input).await.is_err(), "{file_value:?}");
        assert_eq!(server.requests(), 0);
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn disabled_unimplemented_handler_compiles_enabled_one_fails_init() {
    for enabled in [false, true] {
        let (_server, _) = server(false).await;
        let (dir, mut input, backend) = setup(None, false);
        std::fs::write(
            dir.path().join("security.yml"),
            json!({"enableVerifyJwt":false,"enableVerifyScope":false}).to_string(),
        )
        .unwrap();
        std::fs::write(
            dir.path().join("metrics.yml"),
            json!({"enabled":enabled}).to_string(),
        )
        .unwrap();
        std::fs::write(dir.path().join("handler.yml"),json!({"enabled":true,"handlers":[format!("{}@metrics",class(HandlerId::Metrics)),format!("{}@proxy",class(HandlerId::Proxy))],"defaultHandlers":["metrics","proxy"]}).to_string()).unwrap();
        input.handlers.push(HandlerRegistration {
            id: HandlerId::Metrics,
            config_file: "metrics.yml",
            factory: None,
        });
        let result = initialize(input).await;
        if enabled {
            assert!(result.is_err());
            assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
        } else {
            assert_eq!(
                handle(result.unwrap(), event("/x", None)).await.unwrap()["statusCode"],
                200
            );
        }
    }
}

#[test]
fn production_and_insecure_exception_init_are_isolated_from_ambient_environment() {
    for allow in [false, true] {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "security_policy_child",
                "--ignored",
                "--nocapture",
            ])
            .env_remove("LIGHT_LAMBDA_ALLOW_INSECURE_TEST_CONFIG");
        if allow {
            child.env("LIGHT_LAMBDA_ALLOW_INSECURE_TEST_CONFIG", "1");
        }
        let output = child.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}
#[tokio::test]
#[ignore = "Executed only by the environment-isolated parent test"]
async fn security_policy_child() {
    let allow = std::env::var("LIGHT_LAMBDA_ALLOW_INSECURE_TEST_CONFIG").as_deref() == Ok("1");
    for flag in ["ignoreJwtExpiry", "enableMockJwt", "logJwtToken"] {
        let (server, key) = server(false).await;
        let (dir, input, backend) = setup(Some(&server), false);
        let mut cfg = serde_json::to_value(config()).unwrap();
        cfg[flag] = json!(true);
        std::fs::write(dir.path().join("security.yml"), cfg.to_string()).unwrap();
        let result = initialize(input).await;
        if !allow {
            assert!(result.is_err());
            assert_eq!(server.requests(), 0);
            assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
        } else {
            let snapshot = result.unwrap();
            let mut c = claims("test-exception-user");
            c["exp"] = json!(jwks::now() - 120);
            let expired = signed(&key, c);
            let auth = if flag == "enableMockJwt" {
                None
            } else if flag == "ignoreJwtExpiry" {
                Some(format!("Bearer {expired}"))
            } else {
                Some(format!(
                    "Bearer {}",
                    signed(&key, claims("test-exception-user"))
                ))
            };
            assert_eq!(
                handle(snapshot, event("/x", auth.as_deref()))
                    .await
                    .unwrap()["statusCode"],
                200
            );
            if flag == "enableMockJwt" {
                assert_eq!(server.requests(), 0);
                assert!(
                    backend.events.lock().unwrap()[0]
                        .pointer("/requestContext/authorizer")
                        .is_none()
                );
            }
        }
    }
    let (_dir, mut input, _) = setup(None, false);
    input.external_dir = std::path::PathBuf::from("/nonexistent-test-only-wp9a");
    assert!(initialize(input).await.is_err());
}
