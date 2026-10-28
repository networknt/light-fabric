use crate::{
    init::{Budget, Snapshot},
    telemetry::{self, Completion},
};
use lambda_runtime::{Error, LambdaEvent};
use serde_json::Value;
use serverless_aws::alb::{self, DecodeContext, IngressMode};
use serverless_core::{ExchangeState, Finalizer, GatewayResponse, Invocation, Pipeline, Target};
use std::{
    sync::{Arc, Mutex, atomic::Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

#[derive(Clone, Copy, Debug)]
pub struct Deadlines {
    pub hard: Instant,
    pub request: Instant,
    pub response: Instant,
}
/// A controlled wall/monotonic pair makes arithmetic independently testable.
pub fn deadlines(entry: Instant, wall_ms: u64, lambda_ms: u64, budget: Budget) -> Deadlines {
    let remaining = Duration::from_millis(lambda_ms.saturating_sub(wall_ms)).min(budget.ingress);
    let hard = entry.checked_add(remaining).unwrap_or(entry);
    Deadlines {
        hard,
        request: entry
            .checked_add(remaining.saturating_sub(budget.response_reserve))
            .unwrap_or(entry),
        response: entry
            .checked_add(remaining.saturating_sub(budget.post_reserve))
            .unwrap_or(entry),
    }
}
struct Capture(Arc<Mutex<Completion>>);
impl Finalizer for Capture {
    fn finalize(&self, inv: &Invocation, st: &ExchangeState, _: &GatewayResponse) {
        let mut c = self.0.lock().unwrap_or_else(|p| p.into_inner());
        c.route = inv.route.as_ref().map(|r| r.template.clone());
        c.target = inv
            .route
            .as_ref()
            .and_then(|r| r.target.as_ref())
            .map(|t| match t {
                Target::Function(s) | Target::HttpService(s) => s.clone(),
            });
        c.dispatch_ms = c.dispatch_ms.max(
            st.metrics
                .dispatch_latency
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
        );
        c.finalizations += 1;
    }
}
struct DispatchTimer {
    start: Instant,
    capture: Arc<Mutex<Completion>>,
}
impl Drop for DispatchTimer {
    fn drop(&mut self) {
        self.capture
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .dispatch_ms = self
            .start
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
    }
}
struct TimedBackend<'a> {
    inner: &'a dyn serverless_core::Backend,
    capture: Arc<Mutex<Completion>>,
}
#[async_trait::async_trait]
impl serverless_core::Backend for TimedBackend<'_> {
    async fn dispatch(
        &self,
        inv: &Invocation,
        st: &ExchangeState,
        target: &Target,
    ) -> Result<GatewayResponse, serverless_core::DispatchError> {
        let _timer = DispatchTimer {
            start: Instant::now(),
            capture: self.capture.clone(),
        };
        // Instrument the future, never hold an entered span across await.
        // Context belongs to this dispatch and cannot leak into another warm
        // invocation. The formatter's existing privacy settings are unchanged.
        self.inner
            .dispatch(inv, st, target)
            .instrument(tracing::info_span!(
                target: "light_lambda::context",
                "backend_dispatch",
                traceability_id = st.traceability_id.as_deref().map(telemetry::bounded)
            ))
            .await
    }
}
pub(crate) struct Output {
    pub bytes: Vec<u8>,
}
/// Trusted app wiring sees the canonical, once-decoded routing path.
pub type RequestGuard = Arc<dyn Fn(&mut Invocation) -> Option<GatewayResponse> + Send + Sync>;
pub async fn handle(snapshot: Arc<Snapshot>, event: LambdaEvent<Value>) -> Result<Value, Error> {
    let result = execute(snapshot, event).await?;
    serde_json::from_slice(&result.bytes)
        .map_err(|_| "internal: ALB encoder returned invalid JSON".into())
}
pub async fn handle_with_guard(
    snapshot: Arc<Snapshot>,
    event: LambdaEvent<Value>,
    guard: Option<&RequestGuard>,
) -> Result<Value, Error> {
    let result = execute_with_guard(snapshot, event, guard).await?;
    serde_json::from_slice(&result.bytes)
        .map_err(|_| "internal: ALB encoder returned invalid JSON".into())
}
pub(crate) async fn execute(
    snapshot: Arc<Snapshot>,
    event: LambdaEvent<Value>,
) -> Result<Output, Error> {
    execute_with_guard(snapshot, event, None).await
}
async fn execute_with_guard(
    snapshot: Arc<Snapshot>,
    event: LambdaEvent<Value>,
    guard: Option<&RequestGuard>,
) -> Result<Output, Error> {
    let entry = Instant::now();
    let wall = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX);
    let deadlines = deadlines(entry, wall, event.context.deadline, snapshot.budget);
    let cold = snapshot.cold.swap(false, Ordering::AcqRel);
    let id = event.context.request_id;
    let capture = Arc::new(Mutex::new(Completion::default()));
    let context = DecodeContext {
        request_id: id.clone(),
        deadline: deadlines.request,
        response_deadline: deadlines.response,
        cancel: CancellationToken::new(),
    };
    let decoded = if snapshot.plan.requires_validator_media() {
        alb::decode_with_validator_media(event.payload, context, &snapshot.limits)
    } else {
        alb::decode(event.payload, context, &snapshot.limits)
            .map(|inv| (inv, serverless_core::ValidatorMediaProof::default()))
    };
    let prepared = Mutex::new(None::<alb::PreparedResponse>);
    let ingress_failed = decoded.is_err();
    let (response, mode) = match decoded {
        Ok((mut inv, media_proof)) => {
            let mut finalizers = snapshot.finalizers.clone();
            finalizers.push(Arc::new(Capture(capture.clone())));
            let backend = TimedBackend {
                inner: snapshot.backend.as_ref(),
                capture: capture.clone(),
            };
            let pipeline = Pipeline::new(snapshot.plan.clone(), finalizers);
            let prepare = |response| {
                let result = alb::prepare_response(
                    response,
                    IngressMode::Multi,
                    snapshot.force_base64,
                    &snapshot.limits,
                )
                .map_err(|_| serverless_core::StatusError::handler_failure())?;
                let response = result.response.clone();
                *prepared.lock().unwrap_or_else(|p| p.into_inner()) = Some(result);
                Ok(response)
            };
            let reservation = reserved_plan_response(&mut inv, &snapshot.plan)
                .or_else(|| guard.and_then(|guard| guard(&mut inv)));
            let response = match reservation {
                Some(response) => pipeline.respond_prepared(inv, response, prepare),
                None => {
                    pipeline
                        .execute_prepared_with_validator_media(inv, &backend, media_proof, prepare)
                        .await
                }
            };
            (response, IngressMode::Multi)
        }
        Err(error) => (error.status.response(), error.mode),
    };
    let frozen = prepared.into_inner().unwrap_or_else(|p| p.into_inner());
    let bytes = match frozen {
        Some(prepared) if prepared.response == response => prepared.bytes,
        _ => alb::encode_final(
            &response,
            mode,
            snapshot.force_base64 && !ingress_failed,
            &snapshot.limits,
        )
        .map_err(|_| "internal: prepared ALB encoding invariant failed")?,
    };
    let envelope: Value =
        serde_json::from_slice(&bytes).map_err(|_| "internal: ALB encoding invariant failed")?;
    let status = envelope["statusCode"]
        .as_u64()
        .ok_or("internal: missing encoded status")? as u16;
    let completion = capture.lock().unwrap_or_else(|p| p.into_inner()).clone();
    let outcome = match status {
        504 => "timeout",
        502..=503 => "backend_error",
        400..=499 => "denial",
        500..=599 => "gateway_error",
        _ => "success",
    };
    telemetry::invocation(telemetry::Record {
        request_id: &id,
        completion: &completion,
        status,
        outcome,
        gateway_ms: entry.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
        cold_start: cold,
        config_digest: &snapshot.digest,
        layer_identity: snapshot.layer.as_deref(),
    });
    Ok(Output { bytes })
}

/// App-owned reservations may only enter an implemented local terminal. An
/// unrelated/default dispatch mapping cannot claim these operations.
fn reserved_plan_response(
    inv: &mut Invocation,
    plan: &serverless_core::ExecutionPlan,
) -> Option<GatewayResponse> {
    use serverless_core::HandlerId;
    let (template, expected) = match inv.path.as_str() {
        "/health" => ("/health", Some(HandlerId::Health)),
        "/adm/health" => ("/adm/health", Some(HandlerId::Health)),
        "/adm/server/info" => ("/adm/server/info", Some(HandlerId::Info)),
        "/adm/logger" if inv.method == http::Method::POST => {
            ("/adm/logger", Some(HandlerId::SetLogger))
        }
        "/adm/logger" => ("/adm/logger", Some(HandlerId::GetLogger)),
        path if path
            .strip_prefix("/adm/cache/")
            .is_some_and(|name| !name.is_empty() && !name.contains('/')) =>
        {
            ("/adm/cache/{name}", None)
        }
        _ => return None,
    };
    let ids = plan.handler_ids(inv);
    let protected = matches!(
        expected,
        Some(HandlerId::Info | HandlerId::GetLogger | HandlerId::SetLogger)
    );
    let authenticated_chain = !protected
        || ids.is_some_and(|ids| {
            if expected == Some(HandlerId::SetLogger) {
                return ids
                    .iter()
                    .take_while(|id| **id != HandlerId::SetLogger)
                    .any(|id| matches!(id, HandlerId::Jwt | HandlerId::Security));
            }
            ids.iter().any(|id| {
                matches!(
                    id,
                    HandlerId::Jwt | HandlerId::Security | HandlerId::Basic | HandlerId::ApiKey
                )
            })
        });
    let admitted_method = if expected == Some(HandlerId::SetLogger) {
        inv.method == http::Method::POST
    } else {
        inv.method == http::Method::GET
    };
    if admitted_method
        && expected.is_some_and(|terminal| ids.is_some_and(|ids| ids.contains(&terminal)))
        && authenticated_chain
    {
        return None;
    }
    inv.route = Some(serverless_core::RouteMatch {
        template: template.into(),
        target: None,
    });
    Some(
        serverless_core::StatusError::new(
            404,
            "ERR10007",
            "INVALID_REQUEST_PATH",
            &format!("Request path {template} cannot be found in the spec."),
        )
        .response(),
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use async_trait::async_trait;
    use bytes::Bytes;
    use http::{HeaderMap, StatusCode};
    use serverless_core::{
        Backend, DispatchError, ExecutionPlan, HandlerId,
        compat::COMPATIBILITY,
        plan::{Binding, HandlerFactory, Registry, TargetRoutes},
    };
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use tracing::instrument::WithSubscriber;
    struct ReservedRouteObserver(Arc<Mutex<Option<String>>>);
    impl Finalizer for ReservedRouteObserver {
        fn finalize(&self, inv: &Invocation, _: &ExchangeState, response: &GatewayResponse) {
            assert_eq!(response.status.as_u16(), 404);
            *self.0.lock().unwrap() = inv.route.as_ref().map(|r| r.template.clone());
        }
    }
    #[tokio::test]
    async fn reserved_rejection_preserves_route_metadata_and_zero_dispatch() {
        let (mut snapshot, calls, finalized) = snapshot(false, false);
        let observed = Arc::new(Mutex::new(None));
        Arc::get_mut(&mut snapshot)
            .unwrap()
            .finalizers
            .push(Arc::new(ReservedRouteObserver(observed.clone())));
        let mut request = event(false);
        request.payload["path"] = Value::String("/health".into());
        assert_eq!(handle(snapshot, request).await.unwrap()["statusCode"], 404);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(finalized.load(Ordering::SeqCst), 1);
        assert_eq!(observed.lock().unwrap().as_deref(), Some("/health"));
    }
    struct Factory;
    impl HandlerFactory for Factory {
        fn build(
            &self,
            _: &serverless_core::compat::Compatibility,
            _: &str,
        ) -> Result<Binding, serverless_core::plan::InitError> {
            Ok(Binding::Dispatch(TargetRoutes::compile([(
                "/x@get".into(),
                Target::Function("configured".into()),
            )])?))
        }
    }
    struct Fake {
        calls: Arc<AtomicUsize>,
        large: bool,
        slow: bool,
    }
    #[async_trait]
    impl Backend for Fake {
        async fn dispatch(
            &self,
            inv: &Invocation,
            _: &ExchangeState,
            _: &Target,
        ) -> Result<GatewayResponse, DispatchError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(inv.request_id, "test-request");
            if self.slow {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Ok(GatewayResponse {
                status: StatusCode::OK,
                headers: HeaderMap::new(),
                body: Some(Bytes::from(if self.large {
                    "x".repeat(5000)
                } else {
                    String::new()
                })),
                body_is_binary: false,
            })
        }
    }
    struct Count(Arc<AtomicUsize>);
    impl Finalizer for Count {
        fn finalize(&self, _: &Invocation, _: &ExchangeState, _: &GatewayResponse) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    pub(crate) fn snapshot(
        large: bool,
        slow: bool,
    ) -> (Arc<Snapshot>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let mut registry = Registry::default();
        registry.register(HandlerId::Proxy, Arc::new(Factory));
        let class = COMPATIBILITY
            .iter()
            .find(|c| c.id == HandlerId::Proxy)
            .unwrap()
            .class;
        let plan=ExecutionPlan::compile(serde_json::json!({"enabled":true,"handlers":[format!("{class}@proxy")],"defaultHandlers":["proxy"]}),serde_json::json!({}),&registry).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let finalized = Arc::new(AtomicUsize::new(0));
        (
            Arc::new(Snapshot {
                resource_owners: vec![],
                plan: Arc::new(plan),
                backend: Arc::new(Fake {
                    calls: calls.clone(),
                    large,
                    slow,
                }),
                finalizers: vec![Arc::new(Count(finalized.clone()))],
                budget: Budget {
                    ingress: Duration::from_millis(200),
                    response_reserve: Duration::from_millis(80),
                    post_reserve: Duration::from_millis(20),
                },
                limits: serverless_aws::limits::Limits {
                    max_response_bytes: 4096,
                    ..Default::default()
                },
                force_base64: false,
                cold: AtomicBool::new(true),
                digest: "test-only-digest".into(),
                layer: None,
                security: None,
            }),
            calls,
            finalized,
        )
    }
    fn event(expired: bool) -> LambdaEvent<Value> {
        let mut context = lambda_runtime::Context::default();
        context.request_id = "test-request".into();
        context.deadline = if expired {
            0
        } else {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64
                + 5000
        };
        LambdaEvent::new(
            serde_json::json!({"httpMethod":"GET","path":"/x","requestContext":{"elb":{"targetGroupArn":"test-only-alb"}},"multiValueHeaders":{}}),
            context,
        )
    }
    #[test]
    fn reserve_arithmetic_saturates_and_is_once_only() {
        let entry = Instant::now();
        let b = Budget {
            ingress: Duration::from_millis(1000),
            response_reserve: Duration::from_millis(250),
            post_reserve: Duration::from_millis(100),
        };
        let d = deadlines(entry, 1000, 1800, b);
        assert_eq!(d.hard - entry, Duration::from_millis(800));
        assert_eq!(d.request - entry, Duration::from_millis(550));
        assert_eq!(d.response - entry, Duration::from_millis(700));
        let d = deadlines(entry, u64::MAX, 1, b);
        assert_eq!(d.hard, entry);
        assert_eq!(d.request, entry);
        assert_eq!(d.response, entry);
        assert!(
            Budget {
                post_reserve: Duration::ZERO,
                ..b
            }
            .validate()
            .is_err()
        );
        assert!(
            Budget {
                post_reserve: b.response_reserve,
                ..b
            }
            .validate()
            .is_err()
        );
    }
    #[tokio::test]
    async fn ordinary_expired_timeout_invalid_and_oversize_are_http_results() {
        for (large, slow, expired, status, calls_expected) in [
            (false, false, false, 200, 1),
            (false, false, true, 504, 0),
            (false, true, false, 504, 1),
            (true, false, false, 502, 1),
        ] {
            let (snapshot, calls, finalized) = snapshot(large, slow);
            let response = handle(snapshot.clone(), event(expired)).await.unwrap();
            assert_eq!(response["statusCode"], status);
            assert_eq!(calls.load(Ordering::SeqCst), calls_expected);
            assert_eq!(finalized.load(Ordering::SeqCst), 1);
            assert!(!snapshot.cold.load(Ordering::SeqCst));
            let encoded = serde_json::to_vec(&response).unwrap();
            assert!(encoded.len() <= 4096);
            if large {
                assert!(response["body"].as_str().unwrap().contains("ERR14016"));
            }
        }
        let (snapshot, calls, finalized) = snapshot(false, false);
        let mut invalid = event(false);
        invalid.payload = serde_json::json!({"headers":{}});
        let response = handle(snapshot, invalid).await.unwrap();
        assert_eq!(response["statusCode"], 400);
        assert!(response.get("headers").is_some());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(finalized.load(Ordering::SeqCst), 0); // No decoded invocation exists.
    }
    struct ResponsePolicy {
        entries: Arc<AtomicUsize>,
        completions: Arc<AtomicUsize>,
        slow: bool,
    }
    #[async_trait]
    impl serverless_core::ResponseHandler for ResponsePolicy {
        fn id(&self) -> HandlerId {
            HandlerId::ResponseHeader
        }
        async fn on_response(
            &self,
            _: &Invocation,
            _: &mut ExchangeState,
            response: &mut GatewayResponse,
        ) -> Result<(), serverless_core::HandlerFailure> {
            self.entries.fetch_add(1, Ordering::SeqCst);
            if self.slow {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            response.headers.insert(
                "x-large",
                http::HeaderValue::from_str(&"x".repeat(5000)).unwrap(),
            );
            self.completions.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
    struct ResponseFactory(Arc<ResponsePolicy>);
    impl HandlerFactory for ResponseFactory {
        fn build(
            &self,
            _: &serverless_core::compat::Compatibility,
            _: &str,
        ) -> Result<Binding, serverless_core::plan::InitError> {
            Ok(Binding::Response(self.0.clone()))
        }
    }
    #[derive(Clone)]
    struct Writer(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Writer {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    #[tokio::test]
    async fn response_replacement_has_one_finalization_and_telemetry_uses_encoded_outcome() {
        if std::env::var_os("WP9_TELEMETRY_CHILD").is_none() {
            let output=std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact","service::tests::response_replacement_has_one_finalization_and_telemetry_uses_encoded_outcome","--nocapture"])
                .env_clear().env("WP9_TELEMETRY_CHILD","1").output().unwrap();
            assert!(
                output.status.success(),
                "isolated telemetry assertions failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        for slow in [false, true] {
            let (mut snapshot, calls, finalized) = snapshot(false, false);
            let entries = Arc::new(AtomicUsize::new(0));
            let completions = Arc::new(AtomicUsize::new(0));
            let mut registry = Registry::default();
            registry.register(HandlerId::Proxy, Arc::new(Factory));
            registry.register(
                HandlerId::ResponseHeader,
                Arc::new(ResponseFactory(Arc::new(ResponsePolicy {
                    entries: entries.clone(),
                    completions: completions.clone(),
                    slow,
                }))),
            );
            let proxy = COMPATIBILITY
                .iter()
                .find(|c| c.id == HandlerId::Proxy)
                .unwrap()
                .class;
            let response = COMPATIBILITY
                .iter()
                .find(|c| c.id == HandlerId::ResponseHeader)
                .unwrap()
                .class;
            Arc::get_mut(&mut snapshot).unwrap().plan=Arc::new(ExecutionPlan::compile(serde_json::json!({"enabled":true,"handlers":[format!("{proxy}@proxy"),format!("{response}@response")],"defaultHandlers":["proxy","response"]}),serde_json::json!({}),&registry).unwrap());
            let logs = Arc::new(Mutex::new(Vec::new()));
            let writer = Writer(logs.clone());
            let subscriber = tracing_subscriber::fmt()
                .json()
                .with_max_level(tracing::Level::TRACE)
                .with_writer(move || writer.clone())
                .finish();
            async {
                for _ in 0..2 {
                    let mut event = event(false);
                    event.payload["body"] = Value::String("test-only-private-body".into());
                    let output = handle(snapshot.clone(), event).await.unwrap();
                    assert_eq!(output["statusCode"], if slow { 504 } else { 502 });
                    assert!(output.get("x-large").is_none());
                }
            }
            .with_subscriber(subscriber)
            .await;
            assert_eq!(calls.load(Ordering::SeqCst), 2);
            assert_eq!(finalized.load(Ordering::SeqCst), 2);
            assert_eq!(entries.load(Ordering::SeqCst), 2);
            assert_eq!(completions.load(Ordering::SeqCst), if slow { 0 } else { 2 });
            let text = String::from_utf8(logs.lock().unwrap().clone()).unwrap();
            assert!(!text.contains("test-only-private-body"));
            let lines: Vec<Value> = text
                .lines()
                .map(|s| serde_json::from_str(s).unwrap())
                .filter(|v: &Value| v["target"] == "audit")
                .collect();
            assert_eq!(lines.len(), 2, "approved telemetry capture: {text}");
            assert_eq!(lines[0]["fields"]["cold_start"], true);
            assert_eq!(lines[1]["fields"]["cold_start"], false);
            for line in lines {
                assert_eq!(line["fields"]["status"], if slow { 504 } else { 502 });
                assert_eq!(line["fields"]["route"], "/x");
                assert_eq!(line["fields"]["target"], "configured");
                assert_eq!(line["fields"]["request_id"], "test-request");
                assert!(line["fields"].get("gateway_ms").is_some());
                assert!(line["fields"].get("dispatch_ms").is_some());
            }
        }
    }
}
