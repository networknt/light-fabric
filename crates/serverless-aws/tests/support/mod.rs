#![allow(dead_code)] // Shared integration-test helpers; each executable uses a subset.
use aws_sdk_lambda::config::{BehaviorVersion, Credentials, Region};
use axum::{
    Router,
    body::Bytes,
    extract::{Request, State},
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use http::{HeaderMap, StatusCode};
use serde_json::{Value, json};
use serverless_aws::{
    alb::{self, DecodeContext},
    invoke::{Clock, InvokeOptions, LambdaInvokeBackend},
    limits::Limits,
    target::{GatewayIdentity, Targets},
    v1::EventConfig,
};
use serverless_core::Invocation;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::Notify,
    time::Instant,
};
use tokio_util::sync::CancellationToken;

pub fn event() -> Value {
    json!({"requestContext":{"elb":{"targetGroupArn":"test-only-tg"}},"httpMethod":"GET","path":"/x","multiValueHeaders":{},"multiValueQueryStringParameters":{},"body":null})
}
pub fn invocation(value: Value) -> Invocation {
    alb::decode(value, context(), &Limits::default()).unwrap()
}
pub fn context() -> DecodeContext {
    DecodeContext {
        request_id: "test-only-request".into(),
        deadline: Instant::now() + Duration::from_secs(5),
        response_deadline: Instant::now() + Duration::from_secs(6),
        cancel: CancellationToken::new(),
    }
}
pub fn cfg() -> EventConfig {
    EventConfig::new("test-only".into()).unwrap()
}
pub fn time() -> DateTime<Utc> {
    DateTime::from_timestamp_millis(1_700_000_000_123).unwrap()
}
pub struct FixedClock;
impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        time()
    }
}
pub fn identity() -> GatewayIdentity {
    GatewayIdentity {
        function_name: "test-only-gateway".into(),
        region: "us-east-1".into(),
        account_id: "000000000000".into(),
    }
}
pub fn targets() -> Targets {
    Targets::new(
        BTreeMap::from([("FnX".into(), "test-only-business:live".into())]),
        &identity(),
    )
    .unwrap()
}
#[derive(Clone)]
pub struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
    pub error: Option<&'static str>,
    pub log_result: Option<String>,
    pub delay: Duration,
}
impl Reply {
    pub fn ok() -> Self {
        Self::raw(200, br#"{"statusCode":200,"body":"ok"}"#.to_vec())
    }
    pub fn raw(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            body,
            error: None,
            log_result: None,
            delay: Duration::ZERO,
        }
    }
    pub fn error(status: u16, code: &str) -> Self {
        Self::raw(
            status,
            serde_json::to_vec(
                &json!({"Type":"User","message":"test-only-service-error","__type":code}),
            )
            .unwrap(),
        )
    }
}
#[derive(Clone, Debug)]
pub struct Received {
    pub method: String,
    pub path: String,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}
pub struct WireState {
    pub received: Mutex<Vec<Received>>,
    pub replies: Vec<Reply>,
    pub requests: AtomicUsize,
    pub connections: AtomicUsize,
    pub starts: AtomicUsize,
    pub started: Notify,
}
struct CountingListener {
    listener: TcpListener,
    state: Arc<WireState>,
}
impl axum::serve::Listener for CountingListener {
    type Io = TcpStream;
    type Addr = std::net::SocketAddr;
    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let pair = self.listener.accept().await.unwrap();
        self.state.connections.fetch_add(1, Ordering::SeqCst);
        pair
    }
    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}
pub struct Fake {
    pub endpoint: String,
    pub state: Arc<WireState>,
    task: tokio::task::JoinHandle<()>,
}
impl Fake {
    pub async fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(WireState {
            received: Mutex::new(Vec::new()),
            replies,
            requests: AtomicUsize::new(0),
            connections: AtomicUsize::new(0),
            starts: AtomicUsize::new(0),
            started: Notify::new(),
        });
        async fn handle(State(state): State<Arc<WireState>>, request: Request) -> Response {
            let (parts, body) = request.into_parts();
            let body = axum::body::to_bytes(body, 7_000_000).await.unwrap();
            state.received.lock().unwrap().push(Received {
                method: parts.method.to_string(),
                path: parts.uri.to_string(),
                headers: parts.headers,
                body: body.to_vec(),
            });
            let i = state.requests.fetch_add(1, Ordering::SeqCst);
            state.started.notify_one();
            let reply = &state.replies[i.min(state.replies.len() - 1)];
            if !reply.delay.is_zero() {
                tokio::time::sleep(reply.delay).await;
            }
            let mut response = (
                StatusCode::from_u16(reply.status).unwrap(),
                Bytes::copy_from_slice(&reply.body),
            )
                .into_response();
            response
                .headers_mut()
                .insert("x-amz-executed-version", "7".parse().unwrap());
            if let Some(log) = &reply.log_result {
                response
                    .headers_mut()
                    .insert("x-amz-log-result", log.parse().unwrap());
            }
            if let Some(error) = reply.error {
                response
                    .headers_mut()
                    .insert("x-amz-function-error", error.parse().unwrap());
            }
            response
        }
        let router = Router::new().fallback(handle).with_state(state.clone());
        let retained = state.clone();
        let task = tokio::spawn(async move {
            axum::serve(CountingListener { listener, state }, router)
                .await
                .unwrap()
        });
        Self {
            endpoint,
            state: retained,
            task,
        }
    }
    pub fn backend(&self, options: InvokeOptions, limits: Limits) -> LambdaInvokeBackend {
        backend(&self.endpoint, options, limits)
    }
    pub fn requests(&self) -> usize {
        self.state.requests.load(Ordering::SeqCst)
    }
    pub fn connections(&self) -> usize {
        self.state.connections.load(Ordering::SeqCst)
    }
}
impl Drop for Fake {
    fn drop(&mut self) {
        self.task.abort();
    }
}
pub fn backend(endpoint: &str, options: InvokeOptions, limits: Limits) -> LambdaInvokeBackend {
    backend_with_http_client(endpoint, options, limits, None)
}
pub fn backend_with_http_client(
    endpoint: &str,
    options: InvokeOptions,
    limits: Limits,
    http: Option<aws_smithy_runtime_api::client::http::SharedHttpClient>,
) -> LambdaInvokeBackend {
    // No aws-config load/default chain: credentials and region are entirely synthetic.
    let mut config = aws_sdk_lambda::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .credentials_provider(Credentials::new(
            "test-only-access",
            "test-only-secret",
            Some("test-only-session".into()),
            None,
            "test-only-static",
        ));
    config.set_http_client(http);
    let config = config.build();
    LambdaInvokeBackend::new(
        config,
        targets(),
        cfg(),
        limits,
        options,
        Arc::new(FixedClock),
        Some(serverless_aws::target::EndpointOverride::new(endpoint.into(), true).unwrap()),
    )
    .unwrap()
}
