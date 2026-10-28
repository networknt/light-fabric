//! Component Runtime API tests through runtime::run. Handlers/backends are
//! declared synthetic test doubles; this is not WP9a authentication or WP10 L1.
use super::*;
use serverless_core::{
    Backend, DispatchError, ExchangeState, ExecutionPlan, GatewayResponse, HandlerFailure,
    HandlerId, HandlerOutcome, Invocation, RequestHandler, StatusError, Target,
};
use serverless_core::{
    compat::{COMPATIBILITY, Compatibility},
    plan::{Binding, HandlerFactory, Registry, RequestCapabilities, TargetRoutes},
};
use std::{
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

struct FailureBackend(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl Backend for FailureBackend {
    async fn dispatch(
        &self,
        inv: &Invocation,
        _: &ExchangeState,
        _: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        assert_eq!(inv.request_id, "test-request");
        Err(DispatchError(StatusError::new(
            502,
            "ERR14015",
            "BACKEND_FAILURE",
            "Synthetic dispatch failure.",
        )))
    }
}
struct Reject;
#[async_trait::async_trait]
impl RequestHandler for Reject {
    fn id(&self) -> HandlerId {
        HandlerId::Correlation
    }
    async fn on_request(
        &self,
        _: &mut Invocation,
        _: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        Ok(HandlerOutcome::Reject(StatusError::missing_identity()))
    }
}
struct RejectFactory;
impl HandlerFactory for RejectFactory {
    fn build(
        &self,
        _: &Compatibility,
        _: &str,
    ) -> Result<Binding, serverless_core::plan::InitError> {
        Ok(Binding::Request(
            Arc::new(Reject),
            RequestCapabilities::default(),
        ))
    }
}
struct ProxyFactory;
impl HandlerFactory for ProxyFactory {
    fn build(
        &self,
        _: &Compatibility,
        _: &str,
    ) -> Result<Binding, serverless_core::plan::InitError> {
        Ok(Binding::Dispatch(TargetRoutes::compile([(
            "/x@get".into(),
            Target::Function("configured".into()),
        )])?))
    }
}
pub(super) fn snapshot(mode: &str) -> (Arc<Snapshot>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let (mut snapshot, calls, finalized) =
        crate::service::tests::snapshot(false, mode == "timeout");
    if mode == "failure" {
        Arc::get_mut(&mut snapshot).unwrap().backend = Arc::new(FailureBackend(calls.clone()));
    }
    if mode == "http500" {
        Arc::get_mut(&mut snapshot).unwrap().backend =
            Arc::new(NextInvocationBackend(calls.clone()));
    }
    if mode == "rejection" {
        let mut registry = Registry::default();
        registry.register(HandlerId::Correlation, Arc::new(RejectFactory));
        registry.register(HandlerId::Proxy, Arc::new(ProxyFactory));
        let class = |id| COMPATIBILITY.iter().find(|c| c.id == id).unwrap().class;
        let plan=ExecutionPlan::compile(serde_json::json!({"enabled":true,"handlers":[format!("{}@reject",class(HandlerId::Correlation)),format!("{}@proxy",class(HandlerId::Proxy))],"defaultHandlers":["reject","proxy"]}),serde_json::json!({}),&registry).unwrap();
        Arc::get_mut(&mut snapshot).unwrap().plan = Arc::new(plan);
    }
    (snapshot, calls, finalized)
}
struct Received {
    line: String,
    headers: String,
    body: Vec<u8>,
}
async fn receive(s: &mut TcpStream) -> Received {
    let mut bytes = Vec::new();
    let mut buf = [0; 4096];
    let end = loop {
        let n = s.read(&mut buf).await.unwrap();
        assert!(n > 0);
        bytes.extend_from_slice(&buf[..n]);
        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            let length = String::from_utf8_lossy(&bytes[..end])
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length: ")
                        .and_then(|v| v.parse::<usize>().ok())
                })
                .unwrap_or(0);
            if bytes.len() >= end + 4 + length {
                break end;
            }
        }
    };
    let headers = String::from_utf8(bytes[..end].to_vec()).unwrap();
    Received {
        line: headers.lines().next().unwrap().to_owned(),
        headers,
        body: bytes[end + 4..].to_vec(),
    }
}
async fn reply(s: &mut TcpStream, status: u16, body: &str) {
    s.write_all(format!("HTTP/1.1 {status} Test\r\nlambda-runtime-aws-request-id: test-request\r\nlambda-runtime-deadline-ms: 9999999999999\r\nlambda-runtime-invocation-id: test-only-invocation\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
}
pub(super) async fn matrix() {
    for mode in [
        "success",
        "rejection",
        "failure",
        "timeout",
        "internal",
        "disconnect",
        "refused",
    ] {
        case(mode).await;
    }
}
pub(super) async fn case(mode: &str) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let owned_mode = mode.to_owned();
    let server = tokio::spawn(async move {
        let mut received = Vec::new();
        let mut connections = 0;
        let (mut stream, _) = listener.accept().await.unwrap();
        connections += 1;
        received.push(receive(&mut stream).await);
        // Closing the listener before delivering the invocation proves failure of
        // the subsequent SDK post connection; no POST has been received.
        if owned_mode == "refused" {
            drop(listener);
            reply(&mut stream,200,r#"{"httpMethod":"GET","path":"/x","requestContext":{"elb":{"targetGroupArn":"test-only-alb"}},"multiValueHeaders":{}}"#).await;
            return (connections, received);
        }
        let event = r#"{"httpMethod":"GET","path":"/x","requestContext":{"elb":{"targetGroupArn":"test-only-alb"}},"multiValueHeaders":{"authorization":["test-only-runtime-secret-marker"]},"body":"test-only-runtime-secret-marker"}"#;
        reply(&mut stream, 200, event).await;
        drop(stream);
        let (mut stream, _) = listener.accept().await.unwrap();
        connections += 1;
        received.push(receive(&mut stream).await);
        if owned_mode == "disconnect" {
            stream.shutdown().await.unwrap();
            drop(stream);
        } else {
            reply(&mut stream, 202, "").await;
            drop(stream);
        }
        // Successful posts resume polling; transport failures terminate. A
        // separate polling error ends successful probes deterministically.
        if let Ok(Ok((mut stream, _))) =
            tokio::time::timeout(Duration::from_millis(300), listener.accept()).await
        {
            connections += 1;
            received.push(receive(&mut stream).await);
            reply(&mut stream, 500, "").await;
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
        (connections, received)
    });
    let owned_mode = mode.to_owned();
    let child = tokio::task::spawn_blocking(move || {
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "runtime::tests::runtime_child", "--nocapture"])
            .env_clear()
            .env("WP9_RUNTIME_PROBE", owned_mode)
            .env("RUST_LOG", "trace")
            .env("AWS_LAMBDA_RUNTIME_API", address.to_string())
            .env("AWS_LAMBDA_FUNCTION_NAME", "test-only-gateway")
            .env("AWS_LAMBDA_FUNCTION_VERSION", "1")
            .env("AWS_LAMBDA_FUNCTION_MEMORY_SIZE", "128")
            .output()
            .unwrap()
    });
    let output = tokio::time::timeout(Duration::from_secs(10), child)
        .await
        .unwrap()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stdout.contains("test-only-runtime-secret-marker"));
    assert!(!stderr.contains("test-only-runtime-secret-marker"));
    assert!(output.status.success(), "isolated runtime assertion failed");
    let (connections, received) = server.await.unwrap();
    let expected = if mode == "refused" {
        1
    } else if mode == "disconnect" {
        2
    } else {
        3
    };
    assert_eq!(connections, received.len());
    assert_eq!(
        received[0].line,
        "GET /2018-06-01/runtime/invocation/next HTTP/1.1"
    );
    if mode != "refused" {
        let endpoint = if mode == "internal" {
            "error"
        } else {
            "response"
        };
        assert_eq!(
            received[1].line,
            format!("POST /2018-06-01/runtime/invocation/test-request/{endpoint} HTTP/1.1")
        );
        assert!(
            received[1]
                .headers
                .to_ascii_lowercase()
                .contains("lambda-runtime-invocation-id: test-only-invocation")
        );
        let payload: Value = serde_json::from_slice(&received[1].body).unwrap();
        assert!(
            !String::from_utf8_lossy(&received[1].body).contains("test-only-runtime-secret-marker")
        );
        if mode == "internal" {
            assert_eq!(
                payload["errorMessage"],
                "internal: test-only invariant failure"
            );
        } else {
            let status = match mode {
                "rejection" => 401,
                "failure" => 502,
                "timeout" => 504,
                _ => 200,
            };
            assert_eq!(payload["statusCode"], status);
            assert!(received[1].body.len() <= 4096);
            assert_eq!(serde_json::to_vec(&payload).unwrap(), received[1].body);
            assert!(received.iter().all(|r| !r.line.contains("/error")));
        }
    }
    let records: Vec<Value> = stdout
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let counts = records
        .iter()
        .find(|v| v["target"] == "light_lambda::runtime_test")
        .unwrap();
    assert_eq!(
        counts["fields"]["dispatches"],
        if ["internal", "rejection"].contains(&mode) {
            0
        } else {
            1
        }
    );
    assert_eq!(
        counts["fields"]["finalizations"],
        if mode == "internal" { 0 } else { 1 }
    );
    if mode != "internal" {
        assert_eq!(records.iter().filter(|v| v["target"] == "audit").count(), 1);
    }
    assert_eq!(
        received.len(),
        expected,
        "{mode}: unexpected polling after a response-post transport failure"
    );
}

struct NextInvocationBackend(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl Backend for NextInvocationBackend {
    async fn dispatch(
        &self,
        inv: &Invocation,
        _: &ExchangeState,
        _: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        assert!(["test-request", "test-request-2"].contains(&inv.request_id.as_str()));
        tracing::info!(target:"light_lambda::runtime_test", business_request_id=%inv.request_id, message="test-only business dispatch");
        Ok(GatewayResponse {
            status: http::StatusCode::OK,
            headers: http::HeaderMap::new(),
            body: Some(bytes::Bytes::from(inv.request_id.clone())),
            body_is_binary: false,
        })
    }
}

/// Closed, owner-approved protocol contract; not a fixture-discovery runner.
pub(super) async fn http500_continues() {
    let contract: Value = serde_json::from_str(include_str!(
        "../../../tests/serverless-conformance/runtime-api/l1-08-response-post-http-500.json"
    ))
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let fixture = contract.clone();
    let server = tokio::spawn(async move {
        let mut received = Vec::new();
        let mut connections = 0;
        for index in 0..5 {
            let (mut stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
                .await
                .unwrap()
                .unwrap();
            connections += 1;
            received.push(
                tokio::time::timeout(Duration::from_secs(5), receive(&mut stream))
                    .await
                    .unwrap(),
            );
            let invocation = &fixture["invocations"][if index >= 2 { 1 } else { 0 }];
            let (status, body, request_id, invocation_id) = if index == 4 {
                (
                    fixture["shutdown"]["terminalPollStatus"].as_u64().unwrap(),
                    String::new(),
                    "test-stop",
                    "test-only-stop",
                )
            } else {
                (
                    if index % 2 == 0 {
                        200
                    } else {
                        invocation["responsePostStatus"].as_u64().unwrap()
                    },
                    if index % 2 == 0 {
                        fixture["event"].to_string()
                    } else {
                        String::new()
                    },
                    invocation["requestId"].as_str().unwrap(),
                    invocation["invocationId"].as_str().unwrap(),
                )
            };
            stream.write_all(format!("HTTP/1.1 {status} Test\r\nlambda-runtime-aws-request-id: {request_id}\r\nlambda-runtime-deadline-ms: 9999999999999\r\nlambda-runtime-invocation-id: {invocation_id}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
        (connections, received)
    });
    let child = tokio::task::spawn_blocking(move || {
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "runtime::tests::runtime_child", "--nocapture"])
            .env_clear()
            .env("WP9_RUNTIME_PROBE", "http500")
            .env("RUST_LOG", "trace")
            .env("AWS_LAMBDA_RUNTIME_API", address.to_string())
            .env("AWS_LAMBDA_FUNCTION_NAME", "test-only-gateway")
            .env("AWS_LAMBDA_FUNCTION_VERSION", "1")
            .env("AWS_LAMBDA_FUNCTION_MEMORY_SIZE", "128")
            .output()
            .unwrap()
    });
    let output = tokio::time::timeout(Duration::from_secs(10), child)
        .await
        .unwrap()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stdout.contains("test-only-runtime-secret-marker"));
    assert!(!stderr.contains("test-only-runtime-secret-marker"));
    assert!(
        output.status.success(),
        "isolated runtime assertions failed"
    );
    let (connections, received) = server.await.unwrap();
    assert_eq!(
        connections,
        contract["expected"]["acceptedConnections"]
            .as_u64()
            .unwrap()
    );
    assert_eq!(
        received.len() as u64,
        contract["expected"]["receivedRequests"].as_u64().unwrap()
    );
    assert_eq!(
        received.iter().map(|r| r.line.as_str()).collect::<Vec<_>>(),
        contract["expected"]["requests"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        received
            .iter()
            .filter(|r| r.line.contains("/error"))
            .count(),
        contract["expected"]["invocationErrorPosts"]
            .as_u64()
            .unwrap() as usize
    );
    for (invocation, post) in contract["invocations"]
        .as_array()
        .unwrap()
        .iter()
        .zip([&received[1], &received[3]])
    {
        let id = invocation["requestId"].as_str().unwrap();
        let path = format!("POST /2018-06-01/runtime/invocation/{id}/response HTTP/1.1");
        assert_eq!(
            received.iter().filter(|r| r.line == path).count(),
            invocation["expectedResponsePosts"].as_u64().unwrap() as usize
        );
        assert!(post.headers.to_ascii_lowercase().contains(&format!(
            "lambda-runtime-invocation-id: {}",
            invocation["invocationId"].as_str().unwrap()
        )));
        let envelope: Value = serde_json::from_slice(&post.body).unwrap();
        assert_eq!(envelope["statusCode"], invocation["expectedGatewayStatus"]);
        assert_eq!(envelope["body"], id);
        assert!(post.body.len() <= 4096);
        assert_eq!(serde_json::to_vec(&envelope).unwrap(), post.body);
        assert!(!String::from_utf8_lossy(&post.body).contains("test-only-runtime-secret-marker"));
    }
    let records: Vec<Value> = stdout
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let log_contract = &contract["expected"]["sdkNonSuccessLog"];
    let sdk_errors: Vec<_> = records
        .iter()
        .enumerate()
        .filter(|(_, v)| {
            v["target"] == log_contract["target"]
                && v["level"] == log_contract["level"]
                && v["fields"].get("status").is_some()
        })
        .collect();
    assert_eq!(sdk_errors.len(), 1);
    assert_eq!(
        sdk_errors[0].1["fields"]["status"],
        log_contract["fieldRedaction"]
    );
    assert_eq!(
        sdk_errors[0].1["fields"]["message"],
        log_contract["fieldRedaction"]
    );
    let audits: Vec<_> = records
        .iter()
        .enumerate()
        .filter(|(_, v)| v["target"] == "audit")
        .collect();
    assert_eq!(audits.len(), 2);
    assert!(audits[0].0 < sdk_errors[0].0 && sdk_errors[0].0 < audits[1].0);
    for (index, invocation) in contract["invocations"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        let id = &invocation["requestId"];
        assert_eq!(
            records
                .iter()
                .filter(|v| v["target"] == "light_lambda::runtime_test"
                    && v["fields"]["business_request_id"] == *id)
                .count(),
            invocation["expectedBusinessDispatches"].as_u64().unwrap() as usize
        );
        assert_eq!(audits[index].1["fields"]["request_id"], *id);
        assert_eq!(audits[index].1["fields"]["status"], 200);
        assert_eq!(audits[index].1["fields"]["cold_start"], index == 0);
    }
    let counts = records
        .iter()
        .find(|v| {
            v["target"] == "light_lambda::runtime_test" && v["fields"].get("dispatches").is_some()
        })
        .unwrap();
    assert_eq!(counts["fields"]["dispatches"], 2);
    assert_eq!(counts["fields"]["finalizations"], 2);
}
