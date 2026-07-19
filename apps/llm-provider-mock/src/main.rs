use async_stream::stream;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::time::sleep;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MockProfile {
    name: String,
    base_latency_ms: u64,
    #[serde(default)]
    jitter_ms: u64,
    #[serde(default = "default_response_bytes")]
    response_bytes: usize,
    #[serde(default)]
    error_status: Option<u16>,
    #[serde(default)]
    reset_after_chunks: Option<usize>,
    #[serde(default)]
    first_token_delay_ms: u64,
    #[serde(default = "default_chunk_interval_ms")]
    chunk_interval_ms: u64,
    #[serde(default = "default_chunk_bytes")]
    chunk_bytes: usize,
    #[serde(default = "default_prompt_tokens")]
    usage_prompt_tokens: u64,
    #[serde(default = "default_completion_tokens")]
    usage_completion_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct ChatRequest {
    #[serde(default = "default_model")]
    model: String,
    #[serde(default)]
    messages: Vec<Value>,
    #[serde(default)]
    stream: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MetricsSnapshot {
    requests: u64,
    successes: u64,
    errors: u64,
    streams: u64,
    bytes_sent: u64,
    inflight: u64,
    max_inflight: u64,
    provider_time_micros: u64,
}

#[derive(Default)]
struct Metrics {
    requests: AtomicU64,
    successes: AtomicU64,
    errors: AtomicU64,
    streams: AtomicU64,
    bytes_sent: AtomicU64,
    inflight: AtomicU64,
    max_inflight: AtomicU64,
    provider_time_micros: AtomicU64,
}

impl Metrics {
    fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            requests: self.requests.load(Ordering::Relaxed),
            successes: self.successes.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            streams: self.streams.load(Ordering::Relaxed),
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
            inflight: self.inflight.load(Ordering::Relaxed),
            max_inflight: self.max_inflight.load(Ordering::Relaxed),
            provider_time_micros: self.provider_time_micros.load(Ordering::Relaxed),
        }
    }
}

struct InflightGuard {
    metrics: Arc<Metrics>,
    started: Instant,
}

impl InflightGuard {
    fn new(metrics: Arc<Metrics>) -> Self {
        let current = metrics.inflight.fetch_add(1, Ordering::Relaxed) + 1;
        metrics.requests.fetch_add(1, Ordering::Relaxed);
        metrics.max_inflight.fetch_max(current, Ordering::Relaxed);
        Self {
            metrics,
            started: Instant::now(),
        }
    }
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        self.metrics.inflight.fetch_sub(1, Ordering::Relaxed);
        self.metrics.provider_time_micros.fetch_add(
            self.started
                .elapsed()
                .as_micros()
                .try_into()
                .unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }
}

struct AppState {
    profiles: BTreeMap<String, MockProfile>,
    default_profile: String,
    metrics: Arc<Metrics>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if env::args().any(|argument| argument == "--healthcheck") {
        let response = reqwest::get("http://127.0.0.1:8080/health").await?;
        if !response.status().is_success() {
            return Err(format!("health endpoint returned {}", response.status()).into());
        }
        return Ok(());
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "llm_provider_mock=info".into()),
        )
        .init();
    let profile_dir = env::var("MOCK_PROFILE_DIR")
        .unwrap_or_else(|_| "benchmarks/llm-gateway/profiles".to_string());
    let profiles = load_profiles(Path::new(&profile_dir))?;
    let default_profile = env::var("MOCK_PROFILE").unwrap_or_else(|_| "stable-60ms".to_string());
    if !profiles.contains_key(&default_profile) {
        return Err(format!("unknown MOCK_PROFILE `{default_profile}`").into());
    }
    let state = Arc::new(AppState {
        profiles,
        default_profile,
        metrics: Arc::new(Metrics::default()),
    });
    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/metrics", get(prometheus_metrics))
        .route("/metrics.json", get(json_metrics))
        .route("/v1/chat/completions", post(chat_completions))
        .with_state(state);
    let address: SocketAddr = env::var("MOCK_LISTEN")
        .unwrap_or_else(|_| "0.0.0.0:8080".to_string())
        .parse()?;
    tracing::info!(%address, "deterministic LLM provider mock listening");
    let listener = tokio::net::TcpListener::bind(address).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

fn load_profiles(dir: &Path) -> Result<BTreeMap<String, MockProfile>, Box<dyn std::error::Error>> {
    let mut profiles = BTreeMap::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let profile: MockProfile = serde_json::from_slice(&fs::read(&path)?)?;
        if profiles.insert(profile.name.clone(), profile).is_some() {
            return Err(format!("duplicate mock profile in {}", path.display()).into());
        }
    }
    if profiles.is_empty() {
        return Err(format!("no JSON mock profiles in {}", dir.display()).into());
    }
    Ok(profiles)
}

async fn chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let guard = InflightGuard::new(Arc::clone(&state.metrics));
    let request: ChatRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => {
            state.metrics.errors.fetch_add(1, Ordering::Relaxed);
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":{"message":error.to_string(),"type":"invalid_request_error"}})),
            )
                .into_response();
        }
    };
    let profile_name = headers
        .get("x-mock-profile")
        .and_then(|value| value.to_str().ok())
        .unwrap_or(&state.default_profile);
    let Some(profile) = state.profiles.get(profile_name).cloned() else {
        state.metrics.errors.fetch_add(1, Ordering::Relaxed);
        return (
            StatusCode::BAD_REQUEST,
            Json(
                json!({"error":{"message":"unknown mock profile","type":"invalid_request_error"}}),
            ),
        )
            .into_response();
    };
    let request_id = request_id(&body);
    sleep(deterministic_latency(&profile, &request_id)).await;
    if let Some(status) = profile.error_status {
        state.metrics.errors.fetch_add(1, Ordering::Relaxed);
        return (
            StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(json!({
                "error": {"message":"deterministic provider error","type":"mock_provider_error"},
                "request_id": request_id,
                "profile": profile.name
            })),
        )
            .into_response();
    }
    if request.stream {
        state.metrics.streams.fetch_add(1, Ordering::Relaxed);
        streaming_response(state.metrics.clone(), guard, request, profile, request_id)
    } else {
        buffered_response(state.metrics.clone(), guard, request, profile, request_id)
    }
}

fn buffered_response(
    metrics: Arc<Metrics>,
    guard: InflightGuard,
    request: ChatRequest,
    profile: MockProfile,
    request_id: String,
) -> Response {
    let content = "x".repeat(profile.response_bytes);
    let encoded = serde_json::to_vec(&json!({
        "id": request_id,
        "object": "chat.completion",
        "created": 1_721_260_800_u64,
        "model": request.model,
        "choices": [{"index":0,"message":{"role":"assistant","content":content},"finish_reason":"stop"}],
        "usage": {
            "prompt_tokens": profile.usage_prompt_tokens,
            "completion_tokens": profile.usage_completion_tokens,
            "total_tokens": profile.usage_prompt_tokens + profile.usage_completion_tokens
        },
        "mock": {"profile":profile.name,"message_count":request.messages.len()}
    }))
    .expect("mock response serializes");
    if profile.reset_after_chunks.is_some() {
        let cutoff = encoded.len().min(profile.chunk_bytes.max(1));
        let first = Bytes::copy_from_slice(&encoded[..cutoff]);
        let body = stream! {
            yield Ok::<Bytes, std::io::Error>(first);
            yield Err(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "deterministic mock reset"));
            drop(guard);
        };
        metrics.errors.fetch_add(1, Ordering::Relaxed);
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-mock-reset", "true")
            .body(Body::from_stream(body))
            .expect("valid reset response");
    }
    metrics.successes.fetch_add(1, Ordering::Relaxed);
    metrics
        .bytes_sent
        .fetch_add(encoded.len() as u64, Ordering::Relaxed);
    drop(guard);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(
            "x-request-id",
            HeaderValue::from_str(&request_id).expect("safe id"),
        )
        .header("x-mock-profile", profile.name)
        .body(Body::from(encoded))
        .expect("valid buffered response")
}

fn streaming_response(
    metrics: Arc<Metrics>,
    guard: InflightGuard,
    request: ChatRequest,
    profile: MockProfile,
    request_id: String,
) -> Response {
    let request_id_header = HeaderValue::from_str(&request_id).expect("safe id");
    let profile_name_header = profile.name.clone();
    let total_chunks = profile
        .response_bytes
        .div_ceil(profile.chunk_bytes.max(1))
        .max(1);
    let stream = stream! {
        sleep(Duration::from_millis(profile.first_token_delay_ms)).await;
        for index in 0..total_chunks {
            if profile.reset_after_chunks.is_some_and(|reset| index >= reset) {
                metrics.errors.fetch_add(1, Ordering::Relaxed);
                yield Err::<Bytes, std::io::Error>(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "deterministic mock reset",
                ));
                drop(guard);
                return;
            }
            if index > 0 {
                sleep(Duration::from_millis(profile.chunk_interval_ms)).await;
            }
            let remaining = profile.response_bytes.saturating_sub(index * profile.chunk_bytes.max(1));
            let content = "x".repeat(remaining.min(profile.chunk_bytes.max(1)));
            let event = format!("data: {}\n\n", json!({
                "id": request_id,
                "object": "chat.completion.chunk",
                "created": 1_721_260_800_u64,
                "model": request.model,
                "choices": [{"index":0,"delta":{"content":content},"finish_reason":Value::Null}]
            }));
            metrics.bytes_sent.fetch_add(event.len() as u64, Ordering::Relaxed);
            yield Ok(Bytes::from(event));
        }
        let usage = format!("data: {}\n\n", json!({
            "id": request_id,
            "object": "chat.completion.chunk",
            "choices": [],
            "usage": {
                "prompt_tokens": profile.usage_prompt_tokens,
                "completion_tokens": profile.usage_completion_tokens,
                "total_tokens": profile.usage_prompt_tokens + profile.usage_completion_tokens
            }
        }));
        metrics.bytes_sent.fetch_add((usage.len() + 14) as u64, Ordering::Relaxed);
        yield Ok(Bytes::from(usage));
        yield Ok(Bytes::from_static(b"data: [DONE]\n\n"));
        metrics.successes.fetch_add(1, Ordering::Relaxed);
        drop(guard);
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header("x-request-id", request_id_header)
        .header("x-mock-profile", profile_name_header)
        .body(Body::from_stream(stream))
        .expect("valid SSE response")
}

async fn json_metrics(State(state): State<Arc<AppState>>) -> Json<MetricsSnapshot> {
    Json(state.metrics.snapshot())
}

async fn prometheus_metrics(State(state): State<Arc<AppState>>) -> Response {
    let value = state.metrics.snapshot();
    let body = format!(
        concat!(
            "# TYPE llm_mock_requests_total counter\nllm_mock_requests_total {}\n",
            "# TYPE llm_mock_successes_total counter\nllm_mock_successes_total {}\n",
            "# TYPE llm_mock_errors_total counter\nllm_mock_errors_total {}\n",
            "# TYPE llm_mock_streams_total counter\nllm_mock_streams_total {}\n",
            "# TYPE llm_mock_bytes_sent_total counter\nllm_mock_bytes_sent_total {}\n",
            "# TYPE llm_mock_inflight gauge\nllm_mock_inflight {}\n",
            "# TYPE llm_mock_max_inflight gauge\nllm_mock_max_inflight {}\n",
            "# TYPE llm_mock_provider_time_micros_total counter\nllm_mock_provider_time_micros_total {}\n"
        ),
        value.requests,
        value.successes,
        value.errors,
        value.streams,
        value.bytes_sent,
        value.inflight,
        value.max_inflight,
        value.provider_time_micros,
    );
    ([(header::CONTENT_TYPE, "text/plain; version=0.0.4")], body).into_response()
}

fn request_id(body: &[u8]) -> String {
    let digest = Sha256::digest(body);
    format!("mock-{}", hex_prefix(&digest, 12))
}

fn hex_prefix(bytes: &[u8], length: usize) -> String {
    bytes
        .iter()
        .flat_map(|byte| format!("{byte:02x}").chars().collect::<Vec<_>>())
        .take(length)
        .collect()
}

fn deterministic_latency(profile: &MockProfile, request_id: &str) -> Duration {
    if profile.jitter_ms == 0 {
        return Duration::from_millis(profile.base_latency_ms);
    }
    let digest = Sha256::digest(request_id.as_bytes());
    let sample = u64::from_be_bytes(digest[..8].try_into().expect("eight bytes"));
    let width = profile.jitter_ms.saturating_mul(2).saturating_add(1);
    let offset = sample % width;
    Duration::from_millis(
        profile
            .base_latency_ms
            .saturating_sub(profile.jitter_ms)
            .saturating_add(offset),
    )
}

fn default_model() -> String {
    "mock-model".to_string()
}

fn default_response_bytes() -> usize {
    256
}

fn default_chunk_interval_ms() -> u64 {
    10
}

fn default_chunk_bytes() -> usize {
    32
}

fn default_prompt_tokens() -> u64 {
    32
}

fn default_completion_tokens() -> u64 {
    16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_id_and_latency_are_stable() {
        let profile = MockProfile {
            name: "variable".to_string(),
            base_latency_ms: 60,
            jitter_ms: 10,
            response_bytes: 10,
            error_status: None,
            reset_after_chunks: None,
            first_token_delay_ms: 0,
            chunk_interval_ms: 1,
            chunk_bytes: 2,
            usage_prompt_tokens: 1,
            usage_completion_tokens: 1,
        };
        assert_eq!(request_id(b"same"), request_id(b"same"));
        assert_eq!(
            deterministic_latency(&profile, "request"),
            deterministic_latency(&profile, "request")
        );
    }
}
