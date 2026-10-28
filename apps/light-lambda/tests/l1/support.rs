use axum::{
    Router,
    body::{Bytes, to_bytes},
    extract::{Request, State},
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use http::{HeaderMap, StatusCode};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    net::SocketAddr,
    path::Path,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    process::{Child, Command},
    task::{JoinHandle, JoinSet},
    time::{Instant, timeout},
};
use tokio_util::sync::CancellationToken;

pub const ARN: &str = "arn:aws:lambda:us-east-1:123456789012:function:light-lambda-test";
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
pub fn event(path: &str, method: &str, token: Option<&str>) -> Value {
    let mut headers = json!({"x-forwarded-for":["198.51.100.7"],"user-agent":["l1-test"]});
    if let Some(token) = token {
        headers["authorization"] = json!([format!("Bearer {token}")]);
    }
    json!({"requestContext":{"elb":{"targetGroupArn":"test-only-target-group"},"authorizer":{"principalId":"untrusted"}},"httpMethod":method,"path":path,"multiValueHeaders":headers,"multiValueQueryStringParameters":{},"body":null})
}
pub fn claims(user: &str) -> Value {
    json!({"sub":user,"user_id":user,"client_id":"test-client","iss":"test-issuer","aud":"test-audience","exp":now_ms()/1000+3600,"scope":"read write"})
}
pub struct Keys {
    pub secret: Vec<u8>,
    pub kid: String,
}
impl Keys {
    pub fn new(kid: &str) -> Self {
        Self {
            secret: [
                uuid::Uuid::new_v4().as_bytes().as_slice(),
                uuid::Uuid::new_v4().as_bytes().as_slice(),
            ]
            .concat(),
            kid: kid.into(),
        }
    }
    pub fn jwks(&self) -> Value {
        json!({"keys":[{"kty":"oct","alg":"HS256","kid":self.kid,"k":URL_SAFE_NO_PAD.encode(&self.secret)}]})
    }
    pub fn token(&self, claims: Value) -> String {
        let mut header = Header::new(Algorithm::HS256);
        header.kid = Some(self.kid.clone());
        encode(&header, &claims, &EncodingKey::from_secret(&self.secret)).unwrap()
    }
}
#[derive(Clone)]
pub struct Reply {
    pub status: u16,
    pub body: Value,
    pub delay: Duration,
    pub function_error: bool,
    pub headers: HeaderMap,
    pub disconnect: bool,
}
impl Reply {
    pub fn json(body: Value) -> Self {
        Self {
            status: 200,
            body,
            delay: Duration::ZERO,
            function_error: false,
            headers: HeaderMap::new(),
            disconnect: false,
        }
    }
    pub fn ok() -> Self {
        Self::json(
            json!({"statusCode":200,"body":"ok","multiValueHeaders":{"set-cookie":["a=1","b=2"]}}),
        )
    }
    pub fn slow(ms: u64, body: Value) -> Self {
        Self {
            delay: Duration::from_millis(ms),
            ..Self::json(body)
        }
    }
}
#[derive(Clone)]
pub struct Wire {
    pub path: String,
    pub method: String,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
    pub received: Instant,
    pub closed: Option<Instant>,
}
impl Wire {
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }
    pub fn sanitized(&self) -> Value {
        // Do not persist Authorization, SDK signing headers, tokens or effective claims.
        let body = if self.path.ends_with("/response") || self.path.ends_with("/error") {
            self.json()
        } else if self.path.contains("/invocations") {
            let mut value = self.json();
            for field in ["headers", "multiValueHeaders"] {
                if let Some(map) = value[field].as_object_mut() {
                    map.remove("authorization");
                    map.remove("x-scope-token");
                    map.remove("x-test-key");
                    map.remove("x-api-key");
                }
            }
            if let Some(map) = value
                .pointer_mut("/requestContext/authorizer")
                .and_then(Value::as_object_mut)
            {
                map.remove("claims");
            }
            value
        } else {
            Value::Null
        };
        let headers = [
            "content-type",
            "x-amz-invocation-type",
            "x-amz-log-type",
            "lambda-runtime-function-error-type",
        ]
        .into_iter()
        .filter_map(|name| {
            self.headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(|value| (name, value))
        })
        .collect::<std::collections::BTreeMap<_, _>>();
        json!({"method":self.method,"path":self.path,"headers":headers,"body":body,"connectionClosedAfterMs":self.closed.map(|t|t.duration_since(self.received).as_millis())})
    }
}
/// Actual HTTP wire endpoint. Invoke uses the SDK-selected REST JSON protocol,
/// not a Backend double. Delayed responses race a read so client EOF is observable.
pub struct RawServer {
    pub url: String,
    pub wires: Arc<Mutex<Vec<Wire>>>,
    pub accepts: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}
impl RawServer {
    pub async fn start(replies: Vec<Reply>) -> Self {
        assert!(!replies.is_empty());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let wires = Arc::new(Mutex::new(vec![]));
        let accepts = Arc::new(AtomicUsize::new(0));
        let (records, counter) = (wires.clone(), accepts.clone());
        let task = tokio::spawn(async move {
            let mut tasks = JoinSet::new();
            loop {
                tokio::select! {
                    result=listener.accept()=> {
                        let Ok((mut stream,_))=result else { break; };
                        counter.fetch_add(1,Ordering::SeqCst);
                        let records=records.clone(); let replies=replies.clone();
                        tasks.spawn(async move {
                            let mut data=vec![]; let mut buf=[0u8;4096];
                            let header_end=loop {
                                let n=stream.read(&mut buf).await.unwrap_or(0); if n==0 { return; }
                                data.extend_from_slice(&buf[..n]); assert!(data.len()<8_000_000);
                                if let Some(index)=data.windows(4).position(|w|w==b"\r\n\r\n") { break index+4; }
                            };
                            let head=String::from_utf8(data[..header_end].to_vec()).unwrap();
                            let mut lines=head.lines(); let mut first=lines.next().unwrap().split_whitespace();
                            let method=first.next().unwrap().to_owned(); let path=first.next().unwrap().to_owned();
                            let mut headers=HeaderMap::new();
                            for line in lines { if let Some((key,value))=line.split_once(':') { headers.append(http::HeaderName::from_bytes(key.as_bytes()).unwrap(),value.trim().parse().unwrap()); } }
                            let len=headers.get("content-length").and_then(|h|h.to_str().ok()).and_then(|h|h.parse::<usize>().ok()).unwrap_or(0);
                            assert!(len<8_000_000);
                            while data.len()<header_end+len { let n=stream.read(&mut buf).await.unwrap_or(0); if n==0 { return; } data.extend_from_slice(&buf[..n]); }
                            let index={let mut records=records.lock().unwrap(); let index=records.len(); records.push(Wire {method,path,headers,body:data[header_end..header_end+len].to_vec(),received:Instant::now(),closed:None}); index};
                            let reply=&replies[index.min(replies.len()-1)];
                            if reply.disconnect { return; }
                            let mut probe=[0u8;1];
                            tokio::select! {
                                _=tokio::time::sleep(reply.delay)=>{},
                                read=stream.read(&mut probe)=>{ if read.unwrap_or(0)==0 { records.lock().unwrap()[index].closed=Some(Instant::now()); } return; }
                            }
                            let body=reply.body.to_string();
                            let mut extra=if reply.function_error { "X-Amz-Function-Error: Unhandled\r\n".to_owned() } else { String::new() };
                            for (key,value) in &reply.headers { extra.push_str(&format!("{}: {}\r\n",key,value.to_str().unwrap())); }
                            let response=format!("HTTP/1.1 {} Test\r\nContent-Type: application/json\r\nConnection: close\r\nX-Amz-Executed-Version: 7\r\n{extra}Content-Length: {}\r\n\r\n{body}",reply.status,body.len());
                            let _=stream.write_all(response.as_bytes()).await;
                        });
                    }
                    Some(result)=tasks.join_next(), if !tasks.is_empty()=>{ result.unwrap(); }
                }
            }
        });
        Self {
            url,
            wires,
            accepts,
            task,
        }
    }
    pub fn records(&self) -> Vec<Wire> {
        self.wires.lock().unwrap().clone()
    }
    pub fn count(&self) -> usize {
        self.wires.lock().unwrap().len()
    }
}
impl Drop for RawServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub struct Invocation {
    pub id: String,
    pub event: Value,
    pub budget_ms: u64,
    pub pause_ms: u64,
}
impl Invocation {
    pub fn new(id: &str, event: Value) -> Self {
        Self {
            id: id.into(),
            event,
            budget_ms: 5000,
            pause_ms: 0,
        }
    }
}
pub struct RuntimeState {
    pub events: Mutex<VecDeque<Invocation>>,
    pub wires: Mutex<Vec<Wire>>,
    pub delivered: Mutex<Vec<(String, Instant, u64)>>,
    pub polls: AtomicUsize,
    pub first_post_500: bool,
    pub hang_init: bool,
    pub init_status: u16,
    pub stop: CancellationToken,
}
struct CountingListener {
    listener: TcpListener,
    accepts: Arc<AtomicUsize>,
}
impl axum::serve::Listener for CountingListener {
    type Io = TcpStream;
    type Addr = SocketAddr;
    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let pair = self.listener.accept().await.unwrap();
        self.accepts.fetch_add(1, Ordering::SeqCst);
        pair
    }
    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}
pub struct Runtime {
    pub authority: String,
    pub state: Arc<RuntimeState>,
    pub accepts: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}
impl Runtime {
    pub async fn start(
        events: Vec<Invocation>,
        first_post_500: bool,
        hang_init: bool,
        init_status: u16,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let authority = listener.local_addr().unwrap().to_string();
        let state = Arc::new(RuntimeState {
            events: Mutex::new(events.into()),
            wires: Mutex::new(vec![]),
            delivered: Mutex::new(vec![]),
            polls: AtomicUsize::new(0),
            first_post_500,
            hang_init,
            init_status,
            stop: CancellationToken::new(),
        });
        async fn handle(State(s): State<Arc<RuntimeState>>, request: Request) -> Response {
            let (parts, body) = request.into_parts();
            let path = parts.uri.path().to_owned();
            if parts.method == http::Method::GET && path == "/2018-06-01/runtime/invocation/next" {
                s.polls.fetch_add(1, Ordering::SeqCst);
                let event = s.events.lock().unwrap().pop_front();
                if let Some(event) = event {
                    tokio::time::sleep(Duration::from_millis(event.pause_ms)).await;
                    s.delivered.lock().unwrap().push((
                        event.id.clone(),
                        Instant::now(),
                        event.budget_ms,
                    ));
                    return ([("Lambda-Runtime-Aws-Request-Id",event.id),("Lambda-Runtime-Deadline-Ms",(now_ms()+event.budget_ms).to_string()),("Lambda-Runtime-Invoked-Function-Arn",ARN.into()),("Lambda-Runtime-Trace-Id","Root=1-00000000-000000000000000000000001;Parent=0000000000000001;Sampled=0".into())],event.event.to_string()).into_response();
                }
                s.stop.cancelled().await;
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            assert_eq!(parts.method, http::Method::POST);
            let body = to_bytes(body, 2_000_000).await.unwrap();
            let is_init = path == "/2018-06-01/runtime/init/error";
            let index = {
                let mut wires = s.wires.lock().unwrap();
                let index = wires.len();
                wires.push(Wire {
                    path,
                    method: parts.method.to_string(),
                    headers: parts.headers,
                    body: body.to_vec(),
                    received: Instant::now(),
                    closed: None,
                });
                index
            };
            if is_init && s.hang_init {
                s.stop.cancelled().await;
            }
            let code = if is_init {
                s.init_status
            } else if s.first_post_500 && index == 0 {
                500
            } else {
                202
            };
            (
                StatusCode::from_u16(code).unwrap(),
                Bytes::from_static(b"{}"),
            )
                .into_response()
        }
        let accepts = Arc::new(AtomicUsize::new(0));
        let retained = state.clone();
        let counter = accepts.clone();
        let task = tokio::spawn(async move {
            axum::serve(
                CountingListener {
                    listener,
                    accepts: counter,
                },
                Router::new().fallback(handle).with_state(state),
            )
            .await
            .unwrap();
        });
        Self {
            authority,
            state: retained,
            accepts,
            task,
        }
    }
    pub fn records(&self) -> Vec<Wire> {
        self.state.wires.lock().unwrap().clone()
    }
    pub async fn wait_posts(&self, n: usize) {
        timeout(Duration::from_secs(8), async {
            loop {
                if self.records().len() >= n {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("bounded Runtime API post wait");
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        self.state.stop.cancel();
        self.task.abort();
    }
}

pub fn config(invoke: &RawServer, jwks: &RawServer) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    let values = json!({"light-lambda.stage":"l1-test","lambda-proxy.region":"us-east-1","lambda-proxy.endpointOverride":invoke.url,"lambda-proxy.functions":{"/*@get":"test-only-business","/*@post":"test-only-business","/*@delete":"test-only-business","/*@patch":"test-only-business","/*@options":"test-only-business"},"client.tokenKeyServerUrl":jwks.url,"client.tokenKeyUri":"/jwks","client.tokenKeyEnableHttp2":false,"client.timeout":3000,"security.issuer":"test-issuer","security.audience":["test-audience"],"security.skipVerifyScopeWithoutSpec":true,"unified-security.pathPrefixAuths":[{"prefix":"/","jwt":true}]});
    std::fs::write(dir.path().join("values.yml"), values.to_string()).unwrap();
    dir
}
pub fn update_values(dir: &Path, key: &str, value: Value) {
    let file = dir.join("values.yml");
    let mut values: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    values[key] = value;
    std::fs::write(file, values.to_string()).unwrap();
}
pub fn use_jwt(dir: &Path) {
    let jwt = serverless_core::compat::lookup(
        "com.networknt.aws.lambda.handler.middleware.security.JwtVerifyMiddleware",
    )
    .unwrap()
    .class;
    let proxy = serverless_core::compat::lookup(
        "com.networknt.aws.lambda.handler.middleware.proxy.LambdaProxyMiddleware",
    )
    .unwrap()
    .class;
    std::fs::write(dir.join("handler.yml"),json!({"handlers":[format!("{jwt}@jwt"),format!("{proxy}@proxy")],"defaultHandlers":["jwt","proxy"]}).to_string()).unwrap();
}
pub fn command(dir: &Path, runtime: Option<&Runtime>) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bootstrap"));
    command
        .env_clear()
        .current_dir(dir)
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
        .env("AWS_REGION", "us-east-1")
        .env("AWS_ACCESS_KEY_ID", "test")
        .env("AWS_SECRET_ACCESS_KEY", "test")
        .env("AWS_EC2_METADATA_DISABLED", "true")
        .env("AWS_CONFIG_FILE", dir.join("no-aws-config"))
        .env(
            "AWS_SHARED_CREDENTIALS_FILE",
            dir.join("no-aws-credentials"),
        )
        .env("AWS_LAMBDA_FUNCTION_NAME", "light-lambda-test")
        .env("AWS_LAMBDA_FUNCTION_VERSION", "1")
        .env("AWS_LAMBDA_FUNCTION_MEMORY_SIZE", "128")
        .env("LIGHT_LAMBDA_GATEWAY_ARN", ARN)
        .env("LIGHT_LAMBDA_CONFIG_DIR", dir)
        .env("LIGHT_LAMBDA_ALLOW_INSECURE_TEST_ENDPOINTS", "1")
        .env("RUST_LOG", "info");
    if let Some(runtime) = runtime {
        command.env("AWS_LAMBDA_RUNTIME_API", &runtime.authority);
    }
    command
}
pub struct Process {
    child: Child,
    out: JoinHandle<Vec<u8>>,
    err: JoinHandle<Vec<u8>>,
    start: Instant,
}
pub struct Result {
    pub status: std::process::ExitStatus,
    pub stdout: String,
    pub stderr: String,
    pub elapsed: Duration,
    pub stopped_by_harness: bool,
}
impl Process {
    pub fn spawn(mut command: Command) -> Self {
        let mut child = command.spawn().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        Self {
            child,
            start: Instant::now(),
            out: tokio::spawn(async move {
                let mut data = vec![];
                stdout.read_to_end(&mut data).await.unwrap();
                data
            }),
            err: tokio::spawn(async move {
                let mut data = vec![];
                stderr.read_to_end(&mut data).await.unwrap();
                data
            }),
        }
    }
    pub async fn collect(mut self, stop: bool) -> Result {
        if stop {
            self.child.kill().await.unwrap();
        }
        let status = timeout(Duration::from_secs(6), self.child.wait())
            .await
            .expect("bounded child exit")
            .unwrap();
        Result {
            status,
            stdout: String::from_utf8(self.out.await.unwrap()).unwrap(),
            stderr: String::from_utf8(self.err.await.unwrap()).unwrap(),
            elapsed: self.start.elapsed(),
            stopped_by_harness: stop,
        }
    }
}
impl Result {
    pub fn logs(&self) -> Vec<Value> {
        self.stdout
            .lines()
            .filter_map(|s| serde_json::from_str::<Value>(s).ok())
            .collect()
    }
    pub fn audits(&self) -> Vec<Value> {
        self.logs()
            .into_iter()
            .filter(|v| v["target"] == "audit")
            .collect()
    }
    pub fn save(
        &self,
        name: &str,
        runtime: Option<&Runtime>,
        invoke: &RawServer,
        jwks: &RawServer,
    ) {
        let Some(root) = std::env::var_os("WP10_L1_EVIDENCE_DIR") else {
            return;
        };
        let root = std::path::PathBuf::from(root).join(name);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("stdout.log"), &self.stdout).unwrap();
        std::fs::write(root.join("stderr.log"), &self.stderr).unwrap();
        let runtime_records = runtime
            .map(|r| r.records().iter().map(Wire::sanitized).collect::<Vec<_>>())
            .unwrap_or_default();
        let deliveries=runtime.map(|r|r.state.delivered.lock().unwrap().iter().map(|(id,time,budget)|json!({"id":id,"budgetMs":budget,"postAfterMs":r.records().iter().find(|w|w.path.ends_with(&format!("/{id}/response"))).map(|w|w.received.duration_since(*time).as_millis())})).collect::<Vec<_>>()).unwrap_or_default();
        let report = json!({"exitCode":self.status.code(),"exitStatus":self.status.to_string(),"stoppedByHarness":self.stopped_by_harness,"elapsedMs":self.elapsed.as_millis(),"runtimeDeliveries":deliveries,"responsePostFirstHTTP500":runtime.map(|r|r.state.first_post_500),"initReplyWithheld":runtime.map(|r|r.state.hang_init),"initReplyStatus":runtime.map(|r|r.state.init_status),"runtimePolls":runtime.map(|r|r.state.polls.load(Ordering::SeqCst)),"runtimeConnections":runtime.map(|r|r.accepts.load(Ordering::SeqCst)),"runtimeRecords":runtime_records,"invokeConnections":invoke.accepts.load(Ordering::SeqCst),"invokeRecords":invoke.records().iter().map(Wire::sanitized).collect::<Vec<_>>(),"jwksConnections":jwks.accepts.load(Ordering::SeqCst),"jwksRequests":jwks.count()});
        std::fs::write(
            root.join("report.json"),
            serde_json::to_vec_pretty(&report).unwrap(),
        )
        .unwrap();
    }
}
pub fn assert_posts(runtime: &Runtime, expected: usize) {
    let records = runtime.records();
    assert_eq!(records.len(), expected);
    assert!(records.iter().all(|r| r.path.ends_with("/response")));
    let ids = records
        .iter()
        .map(|r| r.path.clone())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(ids.len(), expected);
}
pub fn assert_audits(result: &Result, expected: usize) {
    let audits = result.audits();
    assert_eq!(audits.len(), expected);
    assert!(audits.iter().all(|v| v["fields"]["finalizations"] == 1));
}
