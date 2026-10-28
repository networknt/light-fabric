//! Real Init/ingress/Header/pipeline composition with an owned schema consumer.
//! This test-only consumer is not Validator production activation.
#[path = "profile_support/openapi_request.rs"]
mod adapter;
use async_trait::async_trait;
use light_lambda_runtime::init::*;
use serde_json::{Value, json};
use serverless_core::{compat::Compatibility, plan::*, *};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

static EMBEDDED: &[config_loader::EmbeddedConfigFile] = &[
    config_loader::EmbeddedConfigFile {
        name: "server.yml",
        content: "ip: 127.0.0.1\nhttpPort: 0\nhttpsPort: 0\nenableHttp: false\nenableHttps: false\nserviceId: test-only\nenableRegistry: false\n",
    },
    config_loader::EmbeddedConfigFile {
        name: "light-lambda.yml",
        content: "stage: test-only\ningressTimeoutMs: 2000\nresponseReserveMs: 200\npostReserveMs: 50\n",
    },
    config_loader::EmbeddedConfigFile {
        name: "lambda-proxy.yml",
        content: "functions:\n  /x@post: TestFn\nregion: us-east-1\n",
    },
];
struct Consumer(adapter::Prepared);
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
        inv: &mut Invocation,
        state: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        assert!(state.operation.is_some());
        let json_body = state.validator_media_is_json()?;
        let mut event = inv.original.clone();
        event["headers"] = json!({});
        for key in inv.headers.keys() {
            event["headers"][key.as_str()] =
                json!(inv.headers[key].to_str().map_err(|_| HandlerFailure)?);
        }
        // The owned validator receives only the classified media decision.
        event["headers"]["Content-Type"] = json!(if json_body {
            "application/json"
        } else {
            "text/plain"
        });
        event["body"] = inv
            .body
            .as_ref()
            .map(|b| std::str::from_utf8(b).map(str::to_owned))
            .transpose()
            .map_err(|_| HandlerFailure)?
            .map_or(Value::Null, Value::String);
        Ok(match self.0.validate(&event).map_err(|_| HandlerFailure)? {
            Some(error) => HandlerOutcome::Reject(error),
            None => HandlerOutcome::Continue,
        })
    }
}
struct Factory {
    binding: Binding,
    role: ValidatorMediaRole,
}
impl HandlerFactory for Factory {
    fn validator_media_role(&self) -> ValidatorMediaRole {
        self.role
    }
    fn build(&self, _: &Compatibility, _: &str) -> Result<Binding, serverless_core::InitError> {
        Ok(self.binding.clone())
    }
}
struct Provider(Arc<dyn HandlerFactory>);
impl HandlerFactoryProvider for Provider {
    fn create(
        &self,
        _: &light_runtime::RuntimeConfig,
        _: Option<Arc<light_security::SecurityRuntime>>,
    ) -> Result<Arc<dyn HandlerFactory>, light_lambda_runtime::init_error::InitError> {
        Ok(self.0.clone())
    }
}
#[derive(Default)]
struct Counts {
    calls: AtomicUsize,
    finalized: Mutex<Vec<(u16, Option<bool>)>>,
}
#[async_trait]
impl Backend for Counts {
    async fn dispatch(
        &self,
        _: &Invocation,
        _: &ExchangeState,
        _: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(GatewayResponse {
            status: http::StatusCode::OK,
            headers: http::HeaderMap::new(),
            body: Some(bytes::Bytes::from_static(b"ok")),
            body_is_binary: false,
        })
    }
}
impl Finalizer for Counts {
    fn finalize(&self, _: &Invocation, state: &ExchangeState, response: &GatewayResponse) {
        assert!(state.metrics.finalized);
        self.finalized.lock().unwrap().push((
            response.status.as_u16(),
            state.validator_media_is_json().ok(),
        ));
    }
}
fn class(id: HandlerId) -> &'static str {
    compat::COMPATIBILITY
        .iter()
        .find(|c| c.id == id)
        .unwrap()
        .class
}
fn event(input: &Value) -> lambda_runtime::LambdaEvent<Value> {
    let mut payload = input.clone();
    payload["multiValueHeaders"] = Value::Object(
        input["headers"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), json!([v])))
            .collect(),
    );
    // Hostile proof hints have no authority, including the legacy headers field.
    payload["validatorMediaProof"] = json!({"known":true,"json":false});
    payload["headers"]["Content-Type"] = json!("text/plain");
    let mut context = lambda_runtime::Context::default();
    context.request_id = "test-only-media".into();
    context.deadline = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        + 5000;
    lambda_runtime::LambdaEvent::new(payload, context)
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sixteen_native_cases_through_common_init_real_header_ingress_and_warm_pipeline() {
    let rows: Vec<Value> =
        serde_json::from_str(include_str!("data/wp12-header-validator-java.json")).unwrap();
    assert_eq!(rows.len(), 16);
    for row in rows {
        let input = &row["input"];
        let dir = tempfile::tempdir().unwrap();
        for (file, value) in [
            ("header.yml", &input["files"]["header"]),
            ("openapi.yml", &input["files"]["openapi"]),
        ] {
            std::fs::write(dir.path().join(file), value.to_string()).unwrap();
        }
        std::fs::write(dir.path().join("openapi-handler.yml"), "enabled: true\n").unwrap();
        std::fs::write(dir.path().join("openapi-validator.yml"), "enabled: true\n").unwrap();
        std::fs::write(dir.path().join("handler.yml"), json!({"handlers":[format!("{}@spec",class(HandlerId::Specification)),format!("{}@head",class(HandlerId::RequestHeader)),format!("{}@val",class(HandlerId::Validator)),format!("{}@proxy",class(HandlerId::Proxy))],"defaultHandlers":["spec","head","val","proxy"]}).to_string()).unwrap();
        let prepared = adapter::Prepared::compile(
            &input["files"]["openapi"],
            &input["files"]["openapi-validator"],
        )
        .unwrap();
        assert!(prepared.schema_count() > 0);
        let counts = Arc::new(Counts::default());
        let snapshot = initialize(InitInput {
            embedded: EMBEDDED,
            local_dir: dir.path().join("missing"),
            default_dir: None,
            external_dir: dir.path().into(),
            environment: Environment {
                region: Some("us-east-1".into()),
                function_name: "test-only-gateway".into(),
                function_version: "1".into(),
                memory_mb: 128,
                allow_insecure_test_endpoints: true,
            },
            gateway_identity: serverless_aws::target::GatewayIdentity {
                function_name: "test-only-gateway".into(),
                region: "us-east-1".into(),
                account_id: "000000000000".into(),
            },
            handlers: vec![
                HandlerRegistration {
                    id: HandlerId::Specification,
                    config_file: "openapi-handler.yml",
                    factory: Some(Arc::new(
                        light_lambda_runtime::specification_policy::SpecificationPolicyProvider,
                    )),
                },
                HandlerRegistration {
                    id: HandlerId::RequestHeader,
                    config_file: "header.yml",
                    factory: Some(Arc::new(
                        light_lambda_runtime::head_policy::HeadPolicyProvider::RequestHeader,
                    )),
                },
                HandlerRegistration {
                    id: HandlerId::Validator,
                    config_file: "openapi-validator.yml",
                    factory: Some(Arc::new(Provider(Arc::new(Factory {
                        binding: Binding::Request(
                            Arc::new(Consumer(prepared)),
                            RequestCapabilities::default(),
                        ),
                        role: ValidatorMediaRole::Requires,
                    })))),
                },
            ],
            finalizers: vec![counts.clone()],
            backend: BackendConfiguration::Injected(counts.clone()),
        })
        .await
        .unwrap();
        for _ in 0..2 {
            let response =
                light_lambda_runtime::service::handle(snapshot.clone(), event(&input["event"]))
                    .await
                    .unwrap();
            assert_eq!(
                response["statusCode"], row["java"]["status"],
                "{}",
                input["id"]
            );
            assert_eq!(response["body"], row["java"]["body"], "{}", input["id"]);
            assert!(!response.to_string().contains("validatorMediaProof"));
        }
        assert_eq!(
            counts.calls.load(Ordering::SeqCst),
            row["java"]["lambdaCaptures"].as_u64().unwrap() as usize * 2
        );
        assert_eq!(counts.finalized.lock().unwrap().len(), 2);
    }
}

struct Noop(HandlerId);
#[async_trait]
impl RequestHandler for Noop {
    fn id(&self) -> HandlerId {
        self.0
    }
    async fn on_request(
        &self,
        _: &mut Invocation,
        _: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        Ok(HandlerOutcome::Continue)
    }
}
fn plan_with_role(
    role: ValidatorMediaRole,
    unknown_before: bool,
) -> Result<ExecutionPlan, serverless_core::InitError> {
    let mut registry = Registry::default();
    for (id, media) in [
        (HandlerId::RequestHeader, ValidatorMediaRole::Unspecified),
        (HandlerId::Validator, role),
    ] {
        registry.register(
            id,
            Arc::new(Factory {
                binding: Binding::Request(Arc::new(Noop(id)), RequestCapabilities::default()),
                role: media,
            }),
        );
    }
    registry.register(
        HandlerId::Proxy,
        Arc::new(Factory {
            binding: Binding::Dispatch(
                TargetRoutes::compile([("/x@post".into(), Target::Function("test-only".into()))])
                    .unwrap(),
            ),
            role: ValidatorMediaRole::Unspecified,
        }),
    );
    ExecutionPlan::compile(
        json!({"handlers":[format!("{}@head",class(HandlerId::RequestHeader)),format!("{}@val",class(HandlerId::Validator)),format!("{}@proxy",class(HandlerId::Proxy))],"chains":{"unused":["head","val","proxy"]},"defaultHandlers":if unknown_before {vec!["head","val","proxy"]}else{vec!["val","proxy"]}}),
        json!({}),
        &registry,
    )
}
#[tokio::test]
async fn selected_consumers_only_unknown_mutators_legacy_hooks_and_unknown_runtime_failure() {
    assert!(plan_with_role(ValidatorMediaRole::Requires, true).is_err());
    assert!(plan_with_role(ValidatorMediaRole::Unspecified, true).is_ok());
    let plan = plan_with_role(ValidatorMediaRole::Requires, false).unwrap();
    assert!(plan.requires_validator_media());
    let counts = Arc::new(Counts::default());
    let pipeline = Pipeline::new(Arc::new(plan), vec![counts.clone()]);
    let payload = json!({"requestContext":{"elb":{"targetGroupArn":"test-only"}},"httpMethod":"POST","path":"/x","multiValueHeaders":{}});
    for expired in [false, true] {
        let now = tokio::time::Instant::now();
        let inv = serverless_aws::alb::decode(
            payload.clone(),
            serverless_aws::alb::DecodeContext {
                request_id: "test-only".into(),
                deadline: if expired {
                    now
                } else {
                    now + Duration::from_secs(2)
                },
                response_deadline: now + Duration::from_secs(3),
                cancel: tokio_util::sync::CancellationToken::new(),
            },
            &serverless_aws::limits::Limits::default(),
        )
        .unwrap();
        let response = pipeline.execute(inv, counts.as_ref()).await;
        assert_eq!(response.status.as_u16(), if expired { 504 } else { 500 });
        assert_eq!(
            response.body.unwrap(),
            if expired {
                StatusError::timeout()
            } else {
                StatusError::handler_failure()
            }
            .to_bytes()
        );
    }
    assert_eq!(counts.calls.load(Ordering::SeqCst), 0);
    assert_eq!(counts.finalized.lock().unwrap().len(), 2);
}

fn decoded(headers: Value, body: &str) -> (Invocation, ValidatorMediaProof) {
    let now = tokio::time::Instant::now();
    serverless_aws::alb::decode_with_validator_media(json!({"requestContext":{"elb":{"targetGroupArn":"test-only"}},"httpMethod":"POST","path":"/x","multiValueHeaders":headers,"body":body}),serverless_aws::alb::DecodeContext {request_id:"test-only".into(),deadline:now+Duration::from_secs(2),response_deadline:now+Duration::from_secs(3),cancel:tokio_util::sync::CancellationToken::new()},&serverless_aws::limits::Limits::default()).unwrap()
}
#[tokio::test]
async fn real_header_sanitizer_repetition_and_failed_mutations_do_not_publish() {
    use serverless_core::handlers::{header::HeaderHandler, sanitizer::SanitizerHandler};
    let handler = HeaderHandler::from_yaml(serde_yaml::to_value(json!({"enabled":true,"request":{"remove":["Content-Type"],"update":{"Content-Type":"text/plain"}}})).unwrap(),false).unwrap();
    let (mut inv, proof) = decoded(json!({"content-type":["application/json"]}), "{}");
    let mut state = ExchangeState::default();
    state.commit_validator_media(proof);
    for _ in 0..3 {
        handler.on_request(&mut inv, &mut state).await.unwrap();
        assert!(!state.validator_media_is_json().unwrap());
    }
    let bad = HeaderHandler::from_yaml(serde_yaml::to_value(json!({"enabled":true,"request":{"update":{"Content-Type":"application/json","z-bad":"\n"}}})).unwrap(),false).unwrap();
    assert!(bad.on_request(&mut inv, &mut state).await.is_err());
    assert!(!state.validator_media_is_json().unwrap());
    for (headers, malformed) in [
        (json!({"Content-Type":["application/json"]}), false),
        (json!({"content-type":["application/json"]}), false),
        (json!({"Content-Type":["application/json"]}), true),
    ] {
        let sanitizer=SanitizerHandler::from_value(&json!({"headerEnabled":true,"bodyEnabled":malformed,"bodyEncoder":"html","headerEncoder":"uri-component"})).unwrap();
        let (mut inv, proof) = decoded(headers, if malformed { "{broken" } else { "{}" });
        let mut state = ExchangeState::default();
        state.commit_validator_media(proof);
        let had_exact = inv.original["multiValueHeaders"]
            .get("Content-Type")
            .is_some();
        let result = sanitizer.on_request(&mut inv, &mut state).await.unwrap();
        if malformed {
            assert!(matches!(result, HandlerOutcome::Reject(_)));
        }
        assert_eq!(
            state.validator_media_is_json().unwrap(),
            malformed || !had_exact
        );
    }
}
fn transformer(patch: Value) -> light_lambda_runtime::transformer_adapter::TransformerAdapter {
    let mut fixture:Value=serde_json::from_str(include_str!("../../../tests/serverless-conformance/fixtures/v1/t/T-01-metadata-native-cel-multi-contract-v1.json")).unwrap();
    fixture["ruleConfigurationPair"]["rust"]["rule"]["ruleBodies"]["test-edge"]["actions"][0]["actionValues"] =
        json!({"requestHeaders":patch});
    light_lambda_runtime::transformer_adapter::TransformerAdapter::new(
        Arc::new(
            light_lambda_runtime::transformer_program::Candidate::compile(
                &fixture["ruleConfigurationPair"]["rust"]["rule"],
            )
            .unwrap(),
        ),
        vec!["/x".into()],
    )
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_transformer_header_patch_commit_repetition_rollback_and_deadline() {
    for (ingress, patch, json_media) in [
        (
            json!({"content-type":["application/json"]}),
            json!({"update":{"Content-Type":"text/plain"}}),
            true,
        ),
        (
            json!({"content-type":["application/json"]}),
            json!({"remove":["content-type"],"update":{"Content-Type":"text/plain"}}),
            false,
        ),
        (
            json!({"Content-Type":["application/json"]}),
            json!({"remove":["Content-Type"],"update":{"content-type":"text/plain"}}),
            true,
        ),
    ] {
        let handler = transformer(patch);
        let (mut inv, proof) = decoded(ingress, "mutate");
        let mut state = ExchangeState::default();
        state.commit_validator_media(proof);
        for _ in 0..3 {
            assert!(matches!(
                RequestHandler::on_request(&handler, &mut inv, &mut state)
                    .await
                    .unwrap(),
                HandlerOutcome::Continue
            ));
            assert_eq!(state.validator_media_is_json().unwrap(), json_media);
        }
    }
    // An admitted patch adds a 65th transport header after an admitted input.
    // Stage proof with the patch, then discard both on the output-profile gate.
    let handler = transformer(json!({"update":{"Content-Type":"text/plain","x-new":"value"}}));
    let (mut inv, proof) = decoded(json!({"Content-Type":["application/json"]}), "mutate");
    let mut state = ExchangeState::default();
    state.commit_validator_media(proof);
    for index in 0..63 {
        inv.headers.insert(
            http::HeaderName::try_from(format!("x-{index}")).unwrap(),
            http::HeaderValue::from_static("value"),
        );
    }
    let before = inv.headers.clone();
    assert!(matches!(
        RequestHandler::on_request(&handler, &mut inv, &mut state)
            .await
            .unwrap(),
        HandlerOutcome::Reject(_)
    ));
    assert_eq!(inv.headers, before);
    assert!(state.validator_media_is_json().unwrap());
    let handler = transformer(json!({"update":{"Content-Type":"text/plain"}}));
    inv.cancel.cancel();
    let HandlerOutcome::Reject(error) = RequestHandler::on_request(&handler, &mut inv, &mut state)
        .await
        .unwrap()
    else {
        panic!("deadline rejection")
    };
    assert_eq!(error.code, "ERR14017");
    assert_eq!(inv.headers, before);
    assert!(state.validator_media_is_json().unwrap());
}

struct StagesThenFails {
    mode: u8,
    started: Arc<tokio::sync::Notify>,
}
#[async_trait]
impl RequestHandler for StagesThenFails {
    fn id(&self) -> HandlerId {
        HandlerId::RequestHeader
    }
    async fn on_request(
        &self,
        inv: &mut Invocation,
        state: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        let mut proof = state.validator_media_snapshot();
        proof.record_insert(
            "Content-Type",
            &http::HeaderValue::from_static("text/plain"),
            true,
        );
        state.commit_validator_media(proof);
        self.started.notify_one();
        match self.mode {
            0 => Err(HandlerFailure),
            1 => Ok(HandlerOutcome::Reject(StatusError::handler_failure())),
            2 => panic!("owned test panic"),
            3 => std::future::pending().await,
            4 => {
                inv.cancel.cancel();
                Ok(HandlerOutcome::Continue)
            }
            _ => Ok(HandlerOutcome::Continue),
        }
    }
}
#[tokio::test]
async fn pending_proof_is_discarded_on_error_panic_cancellation_deadline_and_outer_abort() {
    for mode in 0..7 {
        let started = Arc::new(tokio::sync::Notify::new());
        let counts = Arc::new(Counts::default());
        let mut registry = Registry::default();
        registry.register(
            HandlerId::RequestHeader,
            Arc::new(Factory {
                binding: Binding::Request(
                    Arc::new(StagesThenFails {
                        mode: if mode >= 5 { 3 } else { mode },
                        started: started.clone(),
                    }),
                    RequestCapabilities::default(),
                ),
                role: ValidatorMediaRole::Maintains,
            }),
        );
        registry.register(
            HandlerId::Proxy,
            Arc::new(Factory {
                binding: Binding::Dispatch(
                    TargetRoutes::compile([(
                        "/x@post".into(),
                        Target::Function("test-only".into()),
                    )])
                    .unwrap(),
                ),
                role: ValidatorMediaRole::Unspecified,
            }),
        );
        let plan=ExecutionPlan::compile(json!({"handlers":[format!("{}@head",class(HandlerId::RequestHeader)),format!("{}@proxy",class(HandlerId::Proxy))],"defaultHandlers":["head","proxy"]}),json!({}),&registry).unwrap();
        let (mut inv, proof) = decoded(json!({"Content-Type":["application/json"]}), "{}");
        if mode == 3 {
            inv.deadline = tokio::time::Instant::now() + Duration::from_millis(50);
        }
        let cancel = inv.cancel.clone();
        let pipeline = Pipeline::new(Arc::new(plan), vec![counts.clone()]);
        let backend = counts.clone();
        let task = tokio::spawn(async move {
            pipeline
                .execute_prepared_with_validator_media(inv, backend.as_ref(), proof, Ok)
                .await
        });
        started.notified().await;
        if mode == 5 {
            cancel.cancel();
        }
        if mode == 6 {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        } else {
            let response = task.await.unwrap();
            assert_eq!(response.status.as_u16(), if mode >= 3 { 504 } else { 500 });
        }
        assert_eq!(counts.calls.load(Ordering::SeqCst), 0);
        let finalized = counts.finalized.lock().unwrap();
        assert_eq!(finalized.len(), 1);
        assert_eq!(finalized[0].1, Some(true));
    }
}

#[tokio::test]
async fn known_missing_empty_and_case_variants_are_isolated_across_warm_invocations() {
    let counts = Arc::new(Counts::default());
    let pipeline = Pipeline::new(
        Arc::new(plan_with_role(ValidatorMediaRole::Requires, false).unwrap()),
        vec![counts.clone()],
    );
    for (headers, expected) in [
        (json!({"Content-Type":["text/plain"]}), false),
        (json!({}), true),
        (json!({"content-type":["text/plain"]}), true),
        (json!({"Content-Type":[]}), true),
        (
            json!({"Content-Type":["application/json","text/plain"]}),
            true,
        ),
    ] {
        let (inv, proof) = decoded(headers, "{}");
        assert_eq!(proof.validates_json().unwrap(), expected);
        let response = pipeline
            .execute_prepared_with_validator_media(inv, counts.as_ref(), proof, Ok)
            .await;
        assert_eq!(response.status, 200);
        assert_eq!(
            counts.finalized.lock().unwrap().last().unwrap().1,
            Some(expected)
        );
    }
    assert_eq!(counts.calls.load(Ordering::SeqCst), 5);
    assert_eq!(counts.finalized.lock().unwrap().len(), 5);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_correlation_traceability_and_token_cache_preserve_media_without_new_configuration() {
    use gateway_policy::token::{
        ClientTokenConfig, SidecarTrafficConfig, TokenHandlerConfig, TokenRuntime,
    };
    use serverless_core::handlers::{
        correlation::CorrelationHandler, token::TokenHandler, traceability::TraceabilityHandler,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // Actual Lambda bindings have fixed names, unlike generic provider-neutral
    // cores. They do not support a configurable Content-Type mutation path.
    assert!(
        CorrelationHandler::from_value(&json!({"enabled":true,"headerName":"Content-Type"}))
            .is_err()
    );
    assert!(
        TraceabilityHandler::from_value(&json!({"enabled":true,"headerName":"Content-Type"}))
            .is_err()
    );
    let correlation = CorrelationHandler::from_value(&json!({"enabled":true})).unwrap();
    let trace = TraceabilityHandler::from_value(&json!({"enabled":true})).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut client = ClientTokenConfig::default();
    client.oauth.token.server_url = Some(format!("http://{}", listener.local_addr().unwrap()));
    client.oauth.token.enable_http2 = false;
    client.oauth.token.token_renew_before_expired = 0;
    client.oauth.token.expired_refresh_retry_delay = 0;
    client.oauth.token.client_credentials.client_id = "test-only-client".into();
    client.oauth.token.client_credentials.client_secret = "test-only-secret".into();
    client.oauth.token.client_credentials.uri = "/token".into();
    let runtime = Arc::new(TokenRuntime::new(
        TokenHandlerConfig::default(),
        SidecarTrafficConfig::default(),
        client,
        light_runtime::DirectRegistryConfig::default(),
        None,
    ));
    let token = TokenHandler::from_yaml(
        serde_yaml::from_str("enabled: true\nappliedPathPrefixes: ['/x']").unwrap(),
        runtime,
        "test-only".into(),
    )
    .unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        assert!(stream.read(&mut request).await.unwrap() > 0);
        let body = r#"{"access_token":"test-only-token","expires_in":3600}"#;
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .as_bytes(),
            )
            .await
            .unwrap();
    });
    for (headers, expected) in [
        (
            json!({"Content-Type":["text/plain"],"service_id":["test-only"]}),
            false,
        ),
        (
            json!({"content-type":["text/plain"],"service_id":["test-only"]}),
            true,
        ),
    ] {
        let (mut inv, proof) = decoded(headers, "{}");
        let mut state = ExchangeState::default();
        state.commit_validator_media(proof);
        for handler in [&correlation as &dyn RequestHandler, &trace, &token] {
            assert!(matches!(
                handler.on_request(&mut inv, &mut state).await.unwrap(),
                HandlerOutcome::Continue
            ));
            assert_eq!(state.validator_media_is_json().unwrap(), expected);
        }
        assert!(inv.headers.contains_key("authorization"));
    }
    server.await.unwrap();
}
