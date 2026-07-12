use agent_runtime_protocol::{
    AttemptBrokerGrant, BrokerOperation, BrokerRequest, BrokerResponse, RuntimeIdentity,
    canonical_digest,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use chrono::Utc;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{Mutex, watch},
};

const MAXIMUM_BROKER_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrokerRouteConfig {
    pub target: String,
    pub operation: BrokerOperation,
    pub base_url: String,
    #[serde(default)]
    pub credential_class: Option<String>,
    #[serde(default)]
    pub credential_file: Option<PathBuf>,
    #[serde(default = "default_auth_header")]
    pub auth_header: String,
    #[serde(default = "default_auth_scheme")]
    pub auth_scheme: String,
    /// Trusted rate used to reserve model cost; it is never supplied by the worker.
    #[serde(default)]
    pub cost_per_1k_tokens_micros: u64,
}

fn default_auth_header() -> String {
    "authorization".into()
}
fn default_auth_scheme() -> String {
    "Bearer".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AttemptBrokerConfig {
    pub socket_directory: PathBuf,
    pub maximum_request_bytes: usize,
    pub request_timeout_ms: u64,
    pub routes: Vec<BrokerRouteConfig>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RouteEvidence<'a> {
    target: &'a str,
    operation: BrokerOperation,
    base_url: &'a str,
    credential_class: &'a Option<String>,
    cost_per_1k_tokens_micros: u64,
}

struct ResolvedRoute {
    config: BrokerRouteConfig,
    base_url: url::Url,
    credential: Option<String>,
}
struct Usage {
    requests: u32,
    tokens: u64,
    cost: u64,
    replay: HashMap<uuid::Uuid, BrokerResponse>,
    used: std::collections::HashSet<uuid::Uuid>,
}

pub struct AttemptBroker {
    listener: UnixListener,
    socket_path: PathBuf,
    identity: RuntimeIdentity,
    grant: AttemptBrokerGrant,
    routes: Arc<BTreeMap<String, ResolvedRoute>>,
    usage: Arc<Mutex<Usage>>,
    client: reqwest::Client,
    maximum_request_bytes: usize,
}

impl AttemptBrokerConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !self.socket_directory.is_absolute()
            || self.maximum_request_bytes == 0
            || self.maximum_request_bytes > 16 * 1024 * 1024
            || self.request_timeout_ms == 0
            || self.routes.is_empty()
        {
            return Err(
                "attempt broker directory, limits, timeout, and routes are required".into(),
            );
        }
        let mut targets = std::collections::BTreeSet::new();
        for route in &self.routes {
            if route.target.is_empty() || !targets.insert(route.target.clone()) {
                return Err("attempt broker route target is empty or duplicated".into());
            }
            let url = url::Url::parse(&route.base_url)
                .map_err(|e| format!("invalid broker route URL: {e}"))?;
            if url.scheme() != "https"
                && !(url.scheme() == "http"
                    && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1")))
            {
                return Err("broker routes require HTTPS except loopback test routes".into());
            }
            if route.credential_file.is_some() != route.credential_class.is_some() {
                return Err("credential class and file must be configured together".into());
            }
            if let Some(path) = &route.credential_file {
                validate_secret_file(path)?;
            }
            if !route
                .auth_header
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            {
                return Err("broker auth header is invalid".into());
            }
        }
        Ok(())
    }
    pub fn route_digest(&self) -> Result<String, String> {
        let evidence = self
            .routes
            .iter()
            .map(|r| RouteEvidence {
                target: &r.target,
                operation: r.operation,
                base_url: &r.base_url,
                credential_class: &r.credential_class,
                cost_per_1k_tokens_micros: r.cost_per_1k_tokens_micros,
            })
            .collect::<Vec<_>>();
        canonical_digest(&evidence).map_err(|e| e.to_string())
    }
}

impl AttemptBroker {
    pub async fn bind(
        config: &AttemptBrokerConfig,
        grant: AttemptBrokerGrant,
        identity: RuntimeIdentity,
    ) -> Result<Self, String> {
        config.validate()?;
        if grant.route_digest != config.route_digest()?
            || grant.policy_digest.is_empty()
            || grant.data_boundary_digest.is_empty()
            || grant.maximum_requests == 0
            || grant.maximum_response_bytes == 0
            || grant.maximum_response_bytes > MAXIMUM_BROKER_RESPONSE_BYTES
            || grant.expires_at <= Utc::now()
        {
            return Err("attempt broker grant is invalid or incompatible".into());
        }
        fs::create_dir_all(&config.socket_directory).map_err(|e| e.to_string())?;
        fs::set_permissions(&config.socket_directory, fs::Permissions::from_mode(0o700))
            .map_err(|e| e.to_string())?;
        let socket_path = config
            .socket_directory
            .join(format!("broker-{}.sock", identity.execution_id));
        if socket_path.exists() {
            fs::remove_file(&socket_path).map_err(|e| e.to_string())?;
        }
        let listener = UnixListener::bind(&socket_path).map_err(|e| e.to_string())?;
        fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600))
            .map_err(|e| e.to_string())?;
        let mut routes = BTreeMap::new();
        for route in &config.routes {
            let credential = route
                .credential_file
                .as_ref()
                .map(|p| fs::read_to_string(p).map(|v| v.trim().to_string()))
                .transpose()
                .map_err(|e| e.to_string())?;
            if credential.as_ref().is_some_and(String::is_empty) {
                return Err("broker credential file is empty".into());
            }
            routes.insert(
                route.target.clone(),
                ResolvedRoute {
                    config: route.clone(),
                    base_url: url::Url::parse(&route.base_url).map_err(|e| e.to_string())?,
                    credential,
                },
            );
        }
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(config.request_timeout_ms))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            listener,
            socket_path,
            identity,
            grant,
            routes: Arc::new(routes),
            usage: Arc::new(Mutex::new(Usage {
                requests: 0,
                tokens: 0,
                cost: 0,
                replay: HashMap::new(),
                used: std::collections::HashSet::new(),
            })),
            client,
            maximum_request_bytes: config.maximum_request_bytes,
        })
    }
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
    pub async fn serve(
        self,
        expected_pid: u32,
        mut shutdown: watch::Receiver<bool>,
    ) -> Result<(), String> {
        loop {
            tokio::select! {accepted=self.listener.accept()=>{let(stream,_)=accepted.map_err(|e|e.to_string())?;if stream.peer_cred().map_err(|e|e.to_string())?.pid()!=i32::try_from(expected_pid).ok(){continue;}let identity=self.identity.clone();let grant=self.grant.clone();let routes=Arc::clone(&self.routes);let usage=Arc::clone(&self.usage);let client=self.client.clone();let max=self.maximum_request_bytes;tokio::spawn(async move{let _=handle(stream,identity,grant,routes,usage,client,max).await;});},changed=shutdown.changed()=>{if changed.is_err()||*shutdown.borrow(){break;}}}
        }
        let _ = fs::remove_file(&self.socket_path);
        Ok(())
    }
}

async fn handle(
    stream: UnixStream,
    identity: RuntimeIdentity,
    grant: AttemptBrokerGrant,
    routes: Arc<BTreeMap<String, ResolvedRoute>>,
    usage: Arc<Mutex<Usage>>,
    client: reqwest::Client,
    maximum_request_bytes: usize,
) -> Result<(), String> {
    let (read, mut write) = stream.into_split();
    let read = BufReader::new(read);
    let mut frame = Vec::new();
    let mut bounded = read.take((maximum_request_bytes + 1) as u64);
    let bytes = bounded
        .read_until(b'\n', &mut frame)
        .await
        .map_err(|e| e.to_string())?;
    if bytes == 0 || frame.last() != Some(&b'\n') || frame.len() > maximum_request_bytes {
        return Err("broker request frame exceeds limit".into());
    }
    frame.pop();
    let request: BrokerRequest = serde_json::from_slice(&frame).map_err(|e| e.to_string())?;
    let response = dispatch(request, &identity, &grant, &routes, &usage, &client).await?;
    let mut bytes = serde_json::to_vec(&response).map_err(|e| e.to_string())?;
    bytes.push(b'\n');
    write.write_all(&bytes).await.map_err(|e| e.to_string())?;
    write.shutdown().await.map_err(|e| e.to_string())
}

async fn dispatch(
    request: BrokerRequest,
    identity: &RuntimeIdentity,
    grant: &AttemptBrokerGrant,
    routes: &BTreeMap<String, ResolvedRoute>,
    usage: &Mutex<Usage>,
    client: &reqwest::Client,
) -> Result<BrokerResponse, String> {
    if request.execution_id != identity.execution_id
        || request.lease_id != identity.lease_id
        || request.fencing_token != identity.fencing_token
        || request.policy_digest != grant.policy_digest
        || request.data_boundary_digest != grant.data_boundary_digest
        || grant.expires_at <= Utc::now()
        || !grant.allowed_operations.contains(&request.operation)
        || !grant.allowed_targets.contains(&request.target)
    {
        return Err("broker request identity or authority mismatch".into());
    }
    if !matches!(request.method.as_str(), "GET" | "POST")
        || request.path.starts_with('/')
        || request.path.split('/').any(|p| p == "..")
    {
        return Err("broker method or relative path is invalid".into());
    }
    let route = routes
        .get(&request.target)
        .ok_or("broker target is not configured")?;
    if route.config.operation != request.operation {
        return Err("broker operation does not match route".into());
    }
    let body = STANDARD
        .decode(&request.body_base64)
        .map_err(|e| e.to_string())?;
    let (charged_tokens, charged_cost_micros) =
        if request.operation == BrokerOperation::ModelInference {
            if request.method != "POST" || request.declared_tokens == 0 {
                return Err("model requests require POST and a positive token ceiling".into());
            }
            let payload: serde_json::Value =
                serde_json::from_slice(&body).map_err(|_| "model request body must be JSON")?;
            let requested_output = payload
                .get("max_tokens")
                .or_else(|| payload.get("max_output_tokens"))
                .and_then(serde_json::Value::as_u64)
                .ok_or("model request must contain max_tokens or max_output_tokens")?;
            if requested_output == 0 || requested_output > request.declared_tokens {
                return Err("model output ceiling exceeds the admitted request ceiling".into());
            }
            // Conservatively reserve estimated input plus the provider output ceiling.
            let input_ceiling = u64::try_from(body.len())
                .map_err(|_| "model request size overflow")?
                .saturating_add(3)
                / 4;
            let tokens = requested_output
                .checked_add(input_ceiling)
                .ok_or("token budget overflow")?;
            let calculated_cost = tokens
                .checked_mul(route.config.cost_per_1k_tokens_micros)
                .and_then(|v| v.checked_add(999))
                .map(|v| v / 1000)
                .ok_or("cost budget overflow")?;
            (tokens, calculated_cost.max(request.declared_cost_micros))
        } else {
            if request.declared_tokens != 0 || request.declared_cost_micros != 0 {
                return Err("non-model requests cannot declare model usage".into());
            }
            (0, 0)
        };
    let mut state = usage.lock().await;
    if let Some(cached) = state.replay.get(&request.request_id) {
        return Ok(cached.clone());
    }
    if state.used.contains(&request.request_id) {
        return Err("broker request id is already in progress or failed".into());
    }
    let next_requests = state
        .requests
        .checked_add(1)
        .ok_or("request budget overflow")?;
    let next_tokens = state
        .tokens
        .checked_add(charged_tokens)
        .ok_or("token budget overflow")?;
    let next_cost = state
        .cost
        .checked_add(charged_cost_micros)
        .ok_or("cost budget overflow")?;
    if next_requests > grant.maximum_requests
        || next_tokens > grant.maximum_tokens
        || next_cost > grant.maximum_cost_micros
    {
        return Err("broker budget exceeded".into());
    }
    state.requests = next_requests;
    state.tokens = next_tokens;
    state.cost = next_cost;
    state.used.insert(request.request_id);
    drop(state);
    tracing::info!(
        execution_id = %identity.execution_id,
        lease_id = %identity.lease_id,
        fencing_token = identity.fencing_token,
        request_id = %request.request_id,
        operation = ?request.operation,
        target = %request.target,
        charged_tokens,
        charged_cost_micros,
        "attempt broker request admitted"
    );
    let url = route
        .base_url
        .join(&request.path)
        .map_err(|e| e.to_string())?;
    if url.origin() != route.base_url.origin() {
        return Err("broker route changed origin".into());
    }
    let method =
        reqwest::Method::from_bytes(request.method.as_bytes()).map_err(|e| e.to_string())?;
    let mut outbound = client.request(method, url).body(body);
    if let Some(secret) = &route.credential {
        let value = if route.config.auth_scheme.is_empty() {
            secret.clone()
        } else {
            format!("{} {}", route.config.auth_scheme, secret)
        };
        outbound = outbound.header(&route.config.auth_header, value);
    }
    let response = outbound.send().await.map_err(|e| e.to_string())?;
    let status = response.status().as_u16();
    let bytes = read_bounded_response(response, grant.maximum_response_bytes).await?;
    let mut state = usage.lock().await;
    if let Some(cached) = state.replay.get(&request.request_id) {
        return Ok(cached.clone());
    }
    let response = BrokerResponse {
        request_id: request.request_id,
        status,
        body_base64: STANDARD.encode(bytes),
        consumed_requests: state.requests,
        consumed_tokens: state.tokens,
        consumed_cost_micros: state.cost,
    };
    state.replay.insert(request.request_id, response.clone());
    Ok(response)
}

async fn read_bounded_response(
    response: reqwest::Response,
    maximum_bytes: usize,
) -> Result<Vec<u8>, String> {
    if response
        .content_length()
        .is_some_and(|length| length > maximum_bytes as u64)
    {
        return Err("broker response exceeds admitted limit".into());
    }
    let mut bytes = Vec::with_capacity(maximum_bytes.min(8 * 1024));
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        let next_len = bytes
            .len()
            .checked_add(chunk.len())
            .ok_or("broker response size overflow")?;
        if next_len > maximum_bytes {
            return Err("broker response exceeds admitted limit".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn validate_secret_file(path: &Path) -> Result<(), String> {
    let metadata = fs::metadata(path)
        .map_err(|e| format!("broker credential {} unavailable: {e}", path.display()))?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o077 != 0 {
        return Err("broker credential must be an owner-only regular file".into());
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use agent_runtime_protocol::{BrokerRequest, RuntimeIdentity};
    use axum::{
        Router,
        body::{Body, Bytes},
        http::HeaderMap,
        routing::{get, post},
    };
    use execution_runner_protocol::{ExecutionId, LeaseId};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use uuid::Uuid;

    async fn upstream(headers: HeaderMap) -> String {
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("missing")
            .to_string()
    }

    async fn oversized_chunked_upstream() -> Body {
        let chunks = futures_util::stream::iter([
            Ok::<_, std::convert::Infallible>(Bytes::from(vec![b'a'; 700])),
            Ok(Bytes::from(vec![b'b'; 700])),
        ]);
        Body::from_stream(chunks)
    }

    #[tokio::test]
    async fn response_stream_stops_at_the_admitted_limit() {
        let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = tcp.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                tcp,
                Router::new().route("/chunked", get(oversized_chunked_upstream)),
            )
            .await
            .unwrap();
        });
        let response = reqwest::get(format!("http://{address}/chunked"))
            .await
            .unwrap();

        let error = read_bounded_response(response, 1024).await.unwrap_err();

        assert_eq!(error, "broker response exceeds admitted limit");
    }

    async fn send(path: &Path, request: &BrokerRequest) -> Result<BrokerResponse, String> {
        let mut stream = UnixStream::connect(path).await.map_err(|e| e.to_string())?;
        let mut bytes = serde_json::to_vec(request).map_err(|e| e.to_string())?;
        bytes.push(b'\n');
        stream.write_all(&bytes).await.map_err(|e| e.to_string())?;
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .map_err(|e| e.to_string())?;
        if line.is_empty() {
            return Err("broker closed without response".into());
        }
        serde_json::from_str(&line).map_err(|e| e.to_string())
    }

    #[tokio::test]
    async fn broker_binds_peer_identity_injects_secret_and_replays_once() {
        let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = tcp.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(tcp, Router::new().route("/invoke", post(upstream)))
                .await
                .unwrap();
        });
        let directory = tempfile::tempdir().unwrap();
        let secret = directory.path().join("model.token");
        fs::write(&secret, "secret-value\n").unwrap();
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
        let config = AttemptBrokerConfig {
            socket_directory: directory.path().join("sockets"),
            maximum_request_bytes: 64 * 1024,
            request_timeout_ms: 5_000,
            routes: vec![BrokerRouteConfig {
                target: "model-main".into(),
                operation: BrokerOperation::ModelInference,
                base_url: format!("http://{address}/"),
                credential_class: Some("model-provider".into()),
                credential_file: Some(secret),
                auth_header: "authorization".into(),
                auth_scheme: "Bearer".into(),
                cost_per_1k_tokens_micros: 100,
            }],
        };
        let identity = RuntimeIdentity {
            execution_id: ExecutionId::new(),
            lease_id: LeaseId::new(),
            fencing_token: 7,
            transport_nonce: "transport".into(),
        };
        let grant = AttemptBrokerGrant {
            policy_digest: "sha256:policy".into(),
            data_boundary_digest: "sha256:boundary".into(),
            route_digest: config.route_digest().unwrap(),
            allowed_operations: std::collections::BTreeSet::from([BrokerOperation::ModelInference]),
            allowed_targets: std::collections::BTreeSet::from(["model-main".into()]),
            maximum_requests: 1,
            maximum_tokens: 100,
            maximum_cost_micros: 1000,
            maximum_response_bytes: 1024,
            expires_at: Utc::now() + chrono::Duration::minutes(1),
        };
        let broker = AttemptBroker::bind(&config, grant, identity.clone())
            .await
            .unwrap();
        let socket = broker.socket_path().to_path_buf();
        let (shutdown, rx) = watch::channel(false);
        let task = tokio::spawn(broker.serve(std::process::id(), rx));
        let request = BrokerRequest {
            request_id: Uuid::new_v4(),
            execution_id: identity.execution_id,
            lease_id: identity.lease_id,
            fencing_token: 7,
            policy_digest: "sha256:policy".into(),
            data_boundary_digest: "sha256:boundary".into(),
            operation: BrokerOperation::ModelInference,
            target: "model-main".into(),
            method: "POST".into(),
            path: "invoke".into(),
            body_base64: STANDARD.encode(br#"{"max_tokens":10}"#),
            declared_tokens: 10,
            declared_cost_micros: 25,
        };
        let first = send(&socket, &request).await.unwrap();
        assert_eq!(first.status, 200);
        assert_eq!(
            String::from_utf8(STANDARD.decode(&first.body_base64).unwrap()).unwrap(),
            "Bearer secret-value"
        );
        let replay = send(&socket, &request).await.unwrap();
        assert_eq!(replay, first);
        assert_eq!(replay.consumed_requests, 1);
        shutdown.send(true).unwrap();
        task.await.unwrap().unwrap();
        assert!(!socket.exists());
    }

    #[tokio::test]
    async fn broker_rejects_payload_child_before_parsing_request() {
        let directory = tempfile::tempdir().unwrap();
        let config = AttemptBrokerConfig {
            socket_directory: directory.path().join("sockets"),
            maximum_request_bytes: 1024,
            request_timeout_ms: 100,
            routes: vec![BrokerRouteConfig {
                target: "network".into(),
                operation: BrokerOperation::NetworkRequest,
                base_url: "http://127.0.0.1:9/".into(),
                credential_class: None,
                credential_file: None,
                auth_header: "authorization".into(),
                auth_scheme: "Bearer".into(),
                cost_per_1k_tokens_micros: 0,
            }],
        };
        let identity = RuntimeIdentity {
            execution_id: ExecutionId::new(),
            lease_id: LeaseId::new(),
            fencing_token: 1,
            transport_nonce: "transport".into(),
        };
        let grant = AttemptBrokerGrant {
            policy_digest: "p".into(),
            data_boundary_digest: "d".into(),
            route_digest: config.route_digest().unwrap(),
            allowed_operations: std::collections::BTreeSet::from([BrokerOperation::NetworkRequest]),
            allowed_targets: std::collections::BTreeSet::from(["network".into()]),
            maximum_requests: 1,
            maximum_tokens: 0,
            maximum_cost_micros: 0,
            maximum_response_bytes: 100,
            expires_at: Utc::now() + chrono::Duration::minutes(1),
        };
        let broker = AttemptBroker::bind(&config, grant, identity).await.unwrap();
        let socket = broker.socket_path().to_path_buf();
        let (shutdown, rx) = watch::channel(false);
        let task = tokio::spawn(broker.serve(std::process::id(), rx));
        let output = tokio::process::Command::new("/usr/bin/python3")
            .arg("-c")
            .arg(
                "import socket,sys; s=socket.socket(socket.AF_UNIX); \
                 s.connect(sys.argv[1]); s.sendall(b'{}\\n'); data=s.recv(1); \
                 sys.exit(0 if data else 7)",
            )
            .arg(&socket)
            .output()
            .await
            .unwrap();
        assert!(!output.status.success());
        shutdown.send(true).unwrap();
        task.await.unwrap().unwrap();
    }
}
