//! Canonical private contract between `light-a2a` and a local business agent.
//!
//! This boundary deliberately excludes caller credentials and Portal policy.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures_util::{Stream, StreamExt};
use hmac::{Hmac, Mac};
use reqwest::redirect::Policy;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use url::{Host, Url};
use uuid::Uuid;

pub const CONTRACT_VERSION: &str = "light-a2a-backend/v1";
pub const CONTEXT_HEADER: &str = "x-light-a2a-backend-context";
pub const SIGNATURE_HEADER: &str = "x-light-a2a-backend-signature";
pub const CONTRACT_DIGEST_HEADER: &str = "x-light-a2a-backend-contract-digest";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BackendOperation {
    Invoke,
    InvokeStream,
    Status,
    Cancel,
}

impl BackendOperation {
    fn path(self) -> &'static str {
        match self {
            Self::Invoke => "/v1/invoke",
            Self::InvokeStream => "/v1/invoke-stream",
            Self::Status => "/v1/status",
            Self::Cancel => "/v1/cancel",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InvocationBudget {
    pub maximum_input_bytes: u64,
    pub maximum_output_bytes: u64,
    pub maximum_artifact_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackendAuthorizedInvocation {
    pub contract_version: String,
    pub invocation_id: Uuid,
    pub issuer: String,
    pub audience: String,
    pub host_id: Uuid,
    pub environment: String,
    pub principal_subject: String,
    pub caller_agent_ref: String,
    pub target_agent_ref: String,
    pub binding_id: Uuid,
    pub publication_id: Uuid,
    pub selected_skill_id: Option<String>,
    pub operation: BackendOperation,
    pub task_id: Uuid,
    pub context_id: Uuid,
    pub idempotency_key: String,
    pub backend_operation_id: Option<String>,
    pub policy_digest: String,
    pub data_boundary_digest: String,
    pub request_digest: String,
    pub budget: InvocationBudget,
    pub traceparent: Option<String>,
    pub issued_at: DateTime<Utc>,
    pub deadline: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl BackendAuthorizedInvocation {
    pub fn validate_shape(&self, now: DateTime<Utc>) -> Result<(), BackendError> {
        if self.contract_version != CONTRACT_VERSION
            || self.issuer != "light-a2a"
            || self.audience.trim().is_empty()
            || self.environment.trim().is_empty()
            || self.principal_subject.trim().is_empty()
            || self.target_agent_ref.trim().is_empty()
            || self.idempotency_key.trim().is_empty()
            || !valid_digest(&self.policy_digest)
            || !valid_digest(&self.data_boundary_digest)
            || !valid_digest(&self.request_digest)
            || self.issued_at > now + chrono::Duration::seconds(30)
            || self.expires_at <= now
            || self.deadline <= now
            || self.expires_at > self.deadline
            || self.expires_at > self.issued_at + chrono::Duration::minutes(5)
            || self.budget.maximum_input_bytes == 0
            || self.budget.maximum_output_bytes == 0
            || self.budget.maximum_artifact_bytes == 0
        {
            return Err(BackendError::Unauthorized(
                "invalid invocation envelope".into(),
            ));
        }
        if matches!(
            self.operation,
            BackendOperation::Status | BackendOperation::Cancel
        ) && self
            .backend_operation_id
            .as_deref()
            .is_none_or(str::is_empty)
        {
            return Err(BackendError::Unauthorized(
                "status and cancel require a backend operation identity".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BusinessRequest {
    pub task_id: Uuid,
    pub context_id: Uuid,
    pub idempotency_key: String,
    pub skill_id: Option<String>,
    #[serde(default)]
    pub message: Value,
    #[serde(default)]
    pub metadata: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InlineArtifact {
    pub artifact_id: Uuid,
    pub logical_name: String,
    pub media_type: String,
    pub content_base64: String,
    pub content_digest: String,
    pub visibility: ArtifactVisibility,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ArtifactVisibility {
    Owner,
    AuthorizedCaller,
    TenantPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BusinessState {
    Submitted,
    Working,
    InputRequired,
    AuthRequired,
    Completed,
    Failed,
    Canceled,
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BusinessResponse {
    pub state: BusinessState,
    pub backend_operation_id: Option<String>,
    pub result: Option<Value>,
    pub error: Option<BusinessError>,
    #[serde(default)]
    pub artifacts: Vec<InlineArtifact>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BusinessEvent {
    pub sequence_number: u64,
    pub state: BusinessState,
    pub backend_operation_id: Option<String>,
    pub result: Option<Value>,
    pub error: Option<BusinessError>,
    pub artifact: Option<InlineArtifact>,
    #[serde(default)]
    pub terminal: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BusinessError {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackendCapabilities {
    pub contract_version: String,
    pub streaming: bool,
    pub cancellation: bool,
    pub status_reconciliation: bool,
    pub accepted_content_modes: BTreeSet<String>,
    pub maximum_artifact_bytes: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("backend invocation rejected: {0}")]
    Unauthorized(String),
    #[error("backend invocation replayed")]
    Replay,
    #[error("backend contract violation: {0}")]
    Contract(String),
    #[error("backend transport failed: {0}")]
    Transport(String),
}

pub fn request_digest(body: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(body))
}

pub fn sign_invocation(
    invocation: &BackendAuthorizedInvocation,
    body: &[u8],
    key: &[u8],
) -> Result<(String, String), BackendError> {
    let encoded = URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(invocation).map_err(|e| BackendError::Contract(e.to_string()))?);
    let mut mac = Hmac::<Sha256>::new_from_slice(key)
        .map_err(|_| BackendError::Contract("invalid HMAC key".into()))?;
    mac.update(encoded.as_bytes());
    mac.update(&[0]);
    mac.update(body);
    Ok((encoded, hex::encode(mac.finalize().into_bytes())))
}

pub fn verify_invocation(
    encoded: &str,
    signature: &str,
    body: &[u8],
    key: &[u8],
    expected: &BackendExpectation,
    now: DateTime<Utc>,
) -> Result<BackendAuthorizedInvocation, BackendError> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key)
        .map_err(|_| BackendError::Unauthorized("invalid HMAC key".into()))?;
    mac.update(encoded.as_bytes());
    mac.update(&[0]);
    mac.update(body);
    let signature = hex::decode(signature)
        .map_err(|_| BackendError::Unauthorized("malformed signature".into()))?;
    mac.verify_slice(&signature)
        .map_err(|_| BackendError::Unauthorized("signature mismatch".into()))?;
    let context: BackendAuthorizedInvocation = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| BackendError::Unauthorized("malformed context".into()))?,
    )
    .map_err(|_| BackendError::Unauthorized("malformed context".into()))?;
    context.validate_shape(now)?;
    if context.audience != expected.audience
        || context.host_id != expected.host_id
        || context.environment != expected.environment
        || context.target_agent_ref != expected.target_agent_ref
        || context.binding_id != expected.binding_id
        || context.publication_id != expected.publication_id
        || context.policy_digest != expected.policy_digest
        || context.data_boundary_digest != expected.data_boundary_digest
        || context.operation != expected.operation
        || expected
            .skill_id
            .as_ref()
            .is_some_and(|skill| context.selected_skill_id.as_ref() != Some(skill))
        || context.request_digest != request_digest(body)
    {
        return Err(BackendError::Unauthorized(
            "invocation binding mismatch".into(),
        ));
    }
    Ok(context)
}

#[derive(Debug, Clone)]
pub struct BackendExpectation {
    pub audience: String,
    pub host_id: Uuid,
    pub environment: String,
    pub target_agent_ref: String,
    pub binding_id: Uuid,
    pub publication_id: Uuid,
    pub policy_digest: String,
    pub data_boundary_digest: String,
    pub operation: BackendOperation,
    pub skill_id: Option<String>,
}

fn validate_request_binding(
    context: &BackendAuthorizedInvocation,
    request: &BusinessRequest,
) -> Result<(), BackendError> {
    if request.task_id != context.task_id
        || request.context_id != context.context_id
        || request.idempotency_key != context.idempotency_key
        || request.skill_id != context.selected_skill_id
    {
        return Err(BackendError::Unauthorized(
            "business request identifiers do not match signed context".into(),
        ));
    }
    Ok(())
}

fn valid_digest(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

#[async_trait]
pub trait AgentBackend: Send + Sync + 'static {
    fn capabilities(&self) -> BackendCapabilities;
    async fn invoke(
        &self,
        context: BackendAuthorizedInvocation,
        request: BusinessRequest,
    ) -> Result<BusinessResponse, BusinessError>;
    async fn invoke_stream(
        &self,
        context: BackendAuthorizedInvocation,
        request: BusinessRequest,
    ) -> Result<BusinessEventStream, BusinessError>;
    async fn status(
        &self,
        context: BackendAuthorizedInvocation,
        request: BusinessRequest,
    ) -> Result<BusinessResponse, BusinessError>;
    async fn cancel(
        &self,
        context: BackendAuthorizedInvocation,
        request: BusinessRequest,
    ) -> Result<BusinessResponse, BusinessError>;
}

pub type BusinessEventStream =
    std::pin::Pin<Box<dyn Stream<Item = Result<BusinessEvent, BusinessError>> + Send>>;

#[derive(Clone)]
pub struct AdapterConfig {
    pub expectation: BackendExpectation,
    pub key: Arc<Vec<u8>>,
    pub maximum_request_bytes: usize,
    pub replay_store: Arc<dyn ReplayStore>,
}

struct AdapterState<B> {
    backend: Arc<B>,
    config: AdapterConfig,
}

impl<B> Clone for AdapterState<B> {
    fn clone(&self) -> Self {
        Self {
            backend: self.backend.clone(),
            config: self.config.clone(),
        }
    }
}

pub fn adapter_router<B: AgentBackend>(backend: B, config: AdapterConfig) -> Router {
    let maximum = config.maximum_request_bytes;
    let state = AdapterState {
        backend: Arc::new(backend),
        config,
    };
    Router::new()
        .route("/v1/capabilities", get(capabilities::<B>))
        .route("/v1/invoke", post(invoke::<B>))
        .route("/v1/invoke-stream", post(invoke_stream::<B>))
        .route("/v1/status", post(status::<B>))
        .route("/v1/cancel", post(cancel::<B>))
        .route("/health/live", get(|| async { StatusCode::NO_CONTENT }))
        .route("/health/ready", get(|| async { StatusCode::NO_CONTENT }))
        .layer(DefaultBodyLimit::max(maximum))
        .with_state(state)
}

async fn capabilities<B: AgentBackend>(
    State(state): State<AdapterState<B>>,
    headers: HeaderMap,
) -> Response {
    if header(&headers, CONTRACT_DIGEST_HEADER).ok() != Some(contract_digest_value()) {
        return adapter_error(BackendError::Unauthorized(
            "contract digest mismatch".into(),
        ));
    }
    Json(state.backend.capabilities()).into_response()
}

async fn invoke<B: AgentBackend>(
    State(state): State<AdapterState<B>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    dispatch(
        &state,
        headers,
        body,
        BackendOperation::Invoke,
        |backend, context, request| async move { backend.invoke(context, request).await },
    )
    .await
}

async fn status<B: AgentBackend>(
    State(state): State<AdapterState<B>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    dispatch(
        &state,
        headers,
        body,
        BackendOperation::Status,
        |backend, context, request| async move { backend.status(context, request).await },
    )
    .await
}

async fn cancel<B: AgentBackend>(
    State(state): State<AdapterState<B>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    dispatch(
        &state,
        headers,
        body,
        BackendOperation::Cancel,
        |backend, context, request| async move { backend.cancel(context, request).await },
    )
    .await
}

async fn dispatch<B, F, Fut>(
    state: &AdapterState<B>,
    headers: HeaderMap,
    body: Bytes,
    operation: BackendOperation,
    call: F,
) -> Response
where
    B: AgentBackend,
    F: FnOnce(Arc<B>, BackendAuthorizedInvocation, BusinessRequest) -> Fut,
    Fut: std::future::Future<Output = Result<BusinessResponse, BusinessError>>,
{
    let (context, request) = match admit(state, &headers, &body, operation) {
        Ok(value) => value,
        Err(error) => return adapter_error(error),
    };
    match call(state.backend.clone(), context, request).await {
        Ok(value) => Json(value).into_response(),
        Err(error) => (StatusCode::UNPROCESSABLE_ENTITY, Json(error)).into_response(),
    }
}

async fn invoke_stream<B: AgentBackend>(
    State(state): State<AdapterState<B>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let (context, request) = match admit(&state, &headers, &body, BackendOperation::InvokeStream) {
        Ok(value) => value,
        Err(error) => return adapter_error(error),
    };
    if !state.backend.capabilities().streaming {
        return adapter_error(BackendError::Contract("streaming is not supported".into()));
    }
    let stream = match state.backend.invoke_stream(context, request).await {
        Ok(stream) => stream,
        Err(error) => return (StatusCode::UNPROCESSABLE_ENTITY, Json(error)).into_response(),
    };
    let stream = stream.map(|event| {
        let payload = match event {
            Ok(value) => serde_json::to_string(&value).expect("serializable backend event"),
            Err(error) => serde_json::to_string(&error).expect("serializable backend error"),
        };
        Ok::<_, std::convert::Infallible>(format!("data: {payload}\n\n"))
    });
    Response::builder()
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-store")
        .body(Body::from_stream(stream))
        .expect("valid SSE response")
}

fn admit<B: AgentBackend>(
    state: &AdapterState<B>,
    headers: &HeaderMap,
    body: &[u8],
    operation: BackendOperation,
) -> Result<(BackendAuthorizedInvocation, BusinessRequest), BackendError> {
    let encoded = header(headers, CONTEXT_HEADER)?;
    let signature = header(headers, SIGNATURE_HEADER)?;
    let contract_digest = header(headers, CONTRACT_DIGEST_HEADER)?;
    if contract_digest != contract_digest_value() {
        return Err(BackendError::Unauthorized(
            "contract digest mismatch".into(),
        ));
    }
    let mut expected = state.config.expectation.clone();
    expected.operation = operation;
    let context = verify_invocation(
        encoded,
        signature,
        body,
        &state.config.key,
        &expected,
        Utc::now(),
    )?;
    let request: BusinessRequest = serde_json::from_slice(body)
        .map_err(|_| BackendError::Contract("invalid business request".into()))?;
    validate_request_binding(&context, &request)?;
    state
        .config
        .replay_store
        .consume(context.invocation_id, context.expires_at)?;
    Ok((context, request))
}

pub trait ReplayStore: Send + Sync + 'static {
    fn consume(&self, invocation_id: Uuid, expires_at: DateTime<Utc>) -> Result<(), BackendError>;
}

pub struct FileReplayStore {
    path: PathBuf,
    maximum_entries: usize,
    lock: Mutex<()>,
}

impl FileReplayStore {
    pub fn new(path: impl AsRef<Path>, maximum_entries: usize) -> Result<Self, BackendError> {
        let path = path.as_ref().to_path_buf();
        if !path.is_absolute() || maximum_entries == 0 {
            return Err(BackendError::Contract(
                "replay file must be absolute and bounded".into(),
            ));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| BackendError::Contract(error.to_string()))?;
        }
        Ok(Self {
            path,
            maximum_entries,
            lock: Mutex::new(()),
        })
    }
}

impl ReplayStore for FileReplayStore {
    fn consume(&self, invocation_id: Uuid, expires_at: DateTime<Utc>) -> Result<(), BackendError> {
        let _guard = self.lock.lock().map_err(|_| BackendError::Replay)?;
        let mut entries: BTreeMap<Uuid, DateTime<Utc>> = match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|_| BackendError::Contract("invalid replay file".into()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(BackendError::Contract(error.to_string())),
        };
        let now = Utc::now();
        entries.retain(|_, expiry| *expiry > now);
        if entries.contains_key(&invocation_id) {
            return Err(BackendError::Replay);
        }
        if entries.len() >= self.maximum_entries {
            return Err(BackendError::Unauthorized("replay store is full".into()));
        }
        entries.insert(invocation_id, expires_at);
        let temporary = self.path.with_extension(format!("{}.tmp", Uuid::now_v7()));
        std::fs::write(
            &temporary,
            serde_json::to_vec(&entries)
                .map_err(|error| BackendError::Contract(error.to_string()))?,
        )
        .map_err(|error| BackendError::Contract(error.to_string()))?;
        std::fs::rename(&temporary, &self.path)
            .map_err(|error| BackendError::Contract(error.to_string()))?;
        Ok(())
    }
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, BackendError> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| BackendError::Unauthorized(format!("missing {name}")))
}

fn adapter_error(error: BackendError) -> Response {
    let status = match error {
        BackendError::Replay | BackendError::Unauthorized(_) => StatusCode::UNAUTHORIZED,
        BackendError::Contract(_) => StatusCode::BAD_REQUEST,
        BackendError::Transport(_) => StatusCode::BAD_GATEWAY,
    };
    (
        status,
        Json(BusinessError {
            code: "BACKEND_INVOCATION_REJECTED".into(),
            message: error.to_string(),
            retryable: false,
        }),
    )
        .into_response()
}

pub fn contract_digest_value() -> &'static str {
    env!("A2A_BACKEND_CONTRACT_DIGEST")
}

#[derive(Debug, Clone)]
pub struct BackendEndpoint {
    origin: Url,
}

impl BackendEndpoint {
    pub fn parse(value: &str) -> Result<Self, BackendError> {
        let origin = Url::parse(value).map_err(|e| BackendError::Contract(e.to_string()))?;
        let loopback = match origin.host() {
            Some(Host::Ipv4(value)) => value.is_loopback(),
            Some(Host::Ipv6(value)) => value.is_loopback(),
            Some(Host::Domain("localhost")) => true,
            _ => false,
        };
        if origin.scheme() != "http"
            || !loopback
            || origin.port().is_none()
            || origin.username() != ""
            || origin.password().is_some()
            || origin.path() != "/"
            || origin.query().is_some()
            || origin.fragment().is_some()
        {
            return Err(BackendError::Contract(
                "backend origin must be http://loopback:port/".into(),
            ));
        }
        Ok(Self { origin })
    }

    fn operation_url(&self, operation: BackendOperation) -> Result<Url, BackendError> {
        self.origin
            .join(operation.path())
            .map_err(|e| BackendError::Contract(e.to_string()))
    }
}

#[derive(Clone)]
pub struct BackendClient {
    endpoint: BackendEndpoint,
    client: reqwest::Client,
    key: Arc<Vec<u8>>,
    maximum_response_bytes: usize,
}

impl BackendClient {
    pub fn new(
        endpoint: BackendEndpoint,
        key: Arc<Vec<u8>>,
        timeout: Duration,
        maximum_response_bytes: usize,
    ) -> Result<Self, BackendError> {
        if key.len() < 32 || maximum_response_bytes == 0 {
            return Err(BackendError::Contract(
                "invalid backend client limits".into(),
            ));
        }
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .redirect(Policy::none())
            .no_proxy()
            .build()
            .map_err(|e| BackendError::Transport(e.to_string()))?;
        Ok(Self {
            endpoint,
            client,
            key,
            maximum_response_bytes,
        })
    }

    pub async fn call(
        &self,
        context: &BackendAuthorizedInvocation,
        request: &BusinessRequest,
    ) -> Result<BusinessResponse, BackendError> {
        let body =
            serde_json::to_vec(request).map_err(|e| BackendError::Contract(e.to_string()))?;
        let (encoded, signature) = sign_invocation(context, &body, &self.key)?;
        let response = self
            .client
            .post(self.endpoint.operation_url(context.operation)?)
            .header(CONTEXT_HEADER, encoded)
            .header(SIGNATURE_HEADER, signature)
            .header(CONTRACT_DIGEST_HEADER, contract_digest_value())
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| BackendError::Transport(e.to_string()))?;
        if !response.status().is_success() {
            return Err(BackendError::Transport(format!(
                "backend returned {}",
                response.status()
            )));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|e| BackendError::Transport(e.to_string()))?;
        if bytes.len() > self.maximum_response_bytes {
            return Err(BackendError::Contract(
                "backend response exceeded limit".into(),
            ));
        }
        serde_json::from_slice(&bytes).map_err(|e| BackendError::Contract(e.to_string()))
    }

    pub async fn capabilities(&self) -> Result<BackendCapabilities, BackendError> {
        let response = self
            .client
            .get(
                self.endpoint
                    .origin
                    .join("/v1/capabilities")
                    .map_err(|e| BackendError::Contract(e.to_string()))?,
            )
            .header(CONTRACT_DIGEST_HEADER, contract_digest_value())
            .send()
            .await
            .map_err(|e| BackendError::Transport(e.to_string()))?;
        if !response.status().is_success() {
            return Err(BackendError::Transport(format!(
                "backend capabilities returned {}",
                response.status()
            )));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|e| BackendError::Transport(e.to_string()))?;
        if bytes.len() > self.maximum_response_bytes {
            return Err(BackendError::Contract(
                "backend capabilities exceeded limit".into(),
            ));
        }
        serde_json::from_slice(&bytes).map_err(|e| BackendError::Contract(e.to_string()))
    }

    pub async fn call_stream(
        &self,
        context: &BackendAuthorizedInvocation,
        request: &BusinessRequest,
    ) -> Result<Vec<BusinessEvent>, BackendError> {
        if context.operation != BackendOperation::InvokeStream {
            return Err(BackendError::Contract(
                "stream client requires INVOKE_STREAM".into(),
            ));
        }
        let body =
            serde_json::to_vec(request).map_err(|e| BackendError::Contract(e.to_string()))?;
        let (encoded, signature) = sign_invocation(context, &body, &self.key)?;
        let response = self
            .client
            .post(self.endpoint.operation_url(context.operation)?)
            .header(CONTEXT_HEADER, encoded)
            .header(SIGNATURE_HEADER, signature)
            .header(CONTRACT_DIGEST_HEADER, contract_digest_value())
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .body(body)
            .send()
            .await
            .map_err(|e| BackendError::Transport(e.to_string()))?;
        if !response.status().is_success()
            || !response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with("text/event-stream"))
        {
            return Err(BackendError::Transport(
                "backend did not return a successful SSE response".into(),
            ));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|e| BackendError::Transport(e.to_string()))?;
        if bytes.len() > self.maximum_response_bytes {
            return Err(BackendError::Contract(
                "backend stream exceeded limit".into(),
            ));
        }
        let text = std::str::from_utf8(&bytes)
            .map_err(|_| BackendError::Contract("backend stream is not UTF-8".into()))?;
        let mut events: Vec<BusinessEvent> = Vec::new();
        for frame in text.split("\n\n") {
            let data = frame
                .lines()
                .filter_map(|line| line.strip_prefix("data:"))
                .map(str::trim_start)
                .collect::<Vec<_>>()
                .join("\n");
            if data.is_empty() {
                continue;
            }
            events.push(
                serde_json::from_str(&data)
                    .map_err(|e| BackendError::Contract(format!("invalid SSE event: {e}")))?,
            );
        }
        if events.is_empty()
            || events
                .windows(2)
                .any(|pair| pair[1].sequence_number <= pair[0].sequence_number)
            || !events.last().is_some_and(|event| event.terminal)
        {
            return Err(BackendError::Contract(
                "backend stream must be ordered and terminal".into(),
            ));
        }
        Ok(events)
    }

    pub async fn start_stream(
        &self,
        context: &BackendAuthorizedInvocation,
        request: &BusinessRequest,
    ) -> Result<tokio::sync::mpsc::Receiver<Result<BusinessEvent, BackendError>>, BackendError>
    {
        if context.operation != BackendOperation::InvokeStream {
            return Err(BackendError::Contract(
                "stream client requires INVOKE_STREAM".into(),
            ));
        }
        let body =
            serde_json::to_vec(request).map_err(|e| BackendError::Contract(e.to_string()))?;
        let (encoded, signature) = sign_invocation(context, &body, &self.key)?;
        let response = self
            .client
            .post(self.endpoint.operation_url(context.operation)?)
            .header(CONTEXT_HEADER, encoded)
            .header(SIGNATURE_HEADER, signature)
            .header(CONTRACT_DIGEST_HEADER, contract_digest_value())
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .body(body)
            .send()
            .await
            .map_err(|e| BackendError::Transport(e.to_string()))?;
        if !response.status().is_success()
            || !response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with("text/event-stream"))
        {
            return Err(BackendError::Transport(
                "backend did not return a successful SSE response".into(),
            ));
        }
        let maximum = self.maximum_response_bytes;
        let mut source = response.bytes_stream();
        let (sender, receiver) = tokio::sync::mpsc::channel(16);
        tokio::spawn(async move {
            let mut buffer = Vec::new();
            let mut total = 0usize;
            let mut previous = 0u64;
            let mut terminal = false;
            while let Some(chunk) = source.next().await {
                let chunk = match chunk {
                    Ok(value) => value,
                    Err(error) => {
                        let _ = sender
                            .send(Err(BackendError::Transport(error.to_string())))
                            .await;
                        return;
                    }
                };
                total = total.saturating_add(chunk.len());
                if total > maximum {
                    let _ = sender
                        .send(Err(BackendError::Contract(
                            "backend stream exceeded limit".into(),
                        )))
                        .await;
                    return;
                }
                buffer.extend_from_slice(&chunk);
                while let Some(boundary) = buffer.windows(2).position(|value| value == b"\n\n") {
                    let frame = match std::str::from_utf8(&buffer[..boundary]) {
                        Ok(value) => value.to_owned(),
                        Err(_) => {
                            let _ = sender
                                .send(Err(BackendError::Contract(
                                    "backend stream is not UTF-8".into(),
                                )))
                                .await;
                            return;
                        }
                    };
                    buffer.drain(..boundary + 2);
                    let data = frame
                        .lines()
                        .filter_map(|line| line.strip_prefix("data:"))
                        .map(str::trim_start)
                        .collect::<Vec<_>>()
                        .join("\n");
                    if data.is_empty() {
                        continue;
                    }
                    let event: BusinessEvent = match serde_json::from_str(&data) {
                        Ok(value) => value,
                        Err(error) => {
                            let _ = sender
                                .send(Err(BackendError::Contract(format!(
                                    "invalid SSE event: {error}"
                                ))))
                                .await;
                            return;
                        }
                    };
                    if event.sequence_number <= previous || terminal {
                        let _ = sender
                            .send(Err(BackendError::Contract(
                                "backend stream is not ordered or emitted after terminal".into(),
                            )))
                            .await;
                        return;
                    }
                    previous = event.sequence_number;
                    terminal = event.terminal;
                    if sender.send(Ok(event)).await.is_err() {
                        return;
                    }
                }
            }
            if !terminal || buffer.iter().any(|byte| !byte.is_ascii_whitespace()) {
                let _ = sender
                    .send(Err(BackendError::Contract(
                        "backend stream ended without one terminal event".into(),
                    )))
                    .await;
            }
        });
        Ok(receiver)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invocation(operation: BackendOperation) -> BackendAuthorizedInvocation {
        let now = Utc::now();
        BackendAuthorizedInvocation {
            contract_version: CONTRACT_VERSION.into(),
            invocation_id: Uuid::now_v7(),
            issuer: "light-a2a".into(),
            audience: "account-backend".into(),
            host_id: Uuid::nil(),
            environment: "dev".into(),
            principal_subject: "user:1".into(),
            caller_agent_ref: "caller".into(),
            target_agent_ref: "account.agent".into(),
            binding_id: Uuid::nil(),
            publication_id: Uuid::now_v7(),
            selected_skill_id: Some("account.lookup".into()),
            operation,
            task_id: Uuid::now_v7(),
            context_id: Uuid::now_v7(),
            idempotency_key: "message-1".into(),
            backend_operation_id: matches!(
                operation,
                BackendOperation::Status | BackendOperation::Cancel
            )
            .then(|| "op-1".into()),
            policy_digest: format!("sha256:{}", "a".repeat(64)),
            data_boundary_digest: format!("sha256:{}", "b".repeat(64)),
            request_digest: String::new(),
            budget: InvocationBudget {
                maximum_input_bytes: 1024,
                maximum_output_bytes: 2048,
                maximum_artifact_bytes: 4096,
            },
            traceparent: None,
            issued_at: now,
            deadline: now + chrono::Duration::minutes(2),
            expires_at: now + chrono::Duration::minutes(1),
        }
    }

    #[test]
    fn signed_context_binds_every_business_identifier() {
        let key = vec![b'k'; 32];
        let mut context = invocation(BackendOperation::Invoke);
        let request = BusinessRequest {
            task_id: context.task_id,
            context_id: context.context_id,
            idempotency_key: context.idempotency_key.clone(),
            skill_id: context.selected_skill_id.clone(),
            message: Value::Null,
            metadata: Value::Null,
        };
        let body = serde_json::to_vec(&request).unwrap();
        context.request_digest = request_digest(&body);
        let (encoded, signature) = sign_invocation(&context, &body, &key).unwrap();
        let expected = BackendExpectation {
            audience: context.audience.clone(),
            host_id: context.host_id,
            environment: context.environment.clone(),
            target_agent_ref: context.target_agent_ref.clone(),
            binding_id: context.binding_id,
            publication_id: context.publication_id,
            policy_digest: context.policy_digest.clone(),
            data_boundary_digest: context.data_boundary_digest.clone(),
            operation: context.operation,
            skill_id: context.selected_skill_id.clone(),
        };
        assert!(
            verify_invocation(&encoded, &signature, &body, &key, &expected, Utc::now()).is_ok()
        );
        let mut wrong = request;
        wrong.task_id = Uuid::now_v7();
        assert!(validate_request_binding(&context, &wrong).is_err());
    }

    #[test]
    fn only_fixed_loopback_origins_are_accepted() {
        assert!(BackendEndpoint::parse("http://127.0.0.1:9010/").is_ok());
        assert!(BackendEndpoint::parse("http://[::1]:9010/").is_ok());
        for value in [
            "https://127.0.0.1:9010/",
            "http://10.0.0.1:9010/",
            "http://localhost/",
            "http://user@localhost:9010/",
        ] {
            assert!(BackendEndpoint::parse(value).is_err(), "{value}");
        }
    }

    #[test]
    fn replay_state_survives_adapter_restart() {
        let path =
            std::env::temp_dir().join(format!("light-a2a-backend-replay-{}.json", Uuid::now_v7()));
        let invocation_id = Uuid::now_v7();
        FileReplayStore::new(&path, 16)
            .unwrap()
            .consume(invocation_id, Utc::now() + chrono::Duration::minutes(1))
            .unwrap();
        assert!(matches!(
            FileReplayStore::new(&path, 16)
                .unwrap()
                .consume(invocation_id, Utc::now() + chrono::Duration::minutes(1)),
            Err(BackendError::Replay)
        ));
        std::fs::remove_file(path).unwrap();
    }
}
