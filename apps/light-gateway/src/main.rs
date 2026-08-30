use agent_delegation::{DelegationClaims, DelegationVerifier, TOKEN_PREFIX};
use anyhow::{Context, Result};
use arc_swap::ArcSwapOption;
use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use chrono::{DateTime, Utc};
use gateway_operational_store::{EvidenceClass, EvidenceRecord, sha256_digest};
use light_gateway::model_provider_sidecar;
use light_pingora::{
    AccessControlRuntime, AccessDecision, ActiveHandlerSet, ApiKeyConfig, AuthPrincipal,
    BasicAuthConfig, CorrelationConfig, CorrelationState, CorsConfig, CorsRequestOutcome,
    CorsResponseHeaders, HandlerBuildContext, HandlerMetricsLogLevel, HandlerRejection,
    HeaderConfig, HmacReplayAttempt, HmacRuntime, HmacVerificationError, McpHttpRequest,
    McpHttpResponse, McpRequestContext, McpResponseBody, McpResponseStream, McpRouterRuntime,
    MetricsConfig, MetricsRecorder, MsalAuthRuntime, MsalExchangeOutcome, MsalExchangeRuntime,
    PathPrefixServiceConfig, PiiTokenizationRuntime, PingoraApp, PingoraHandler,
    PingoraHandlerDescriptor, PingoraHandlerKind, PingoraHandlerRegistry, PingoraTransport,
    ProxyRoute, ProxyTarget, RateLimitHeaders, RateLimitRuntime, ReplayReservation, ReserveOutcome,
    RouterDecision, RouterRoute, SecurityRuntime, SpaAuthLegacyEndpoint, SpaAuthResponse,
    StatelessAuthOutcome, StatelessAuthRuntime, StaticResolution, StaticResourceSet, TokenRuntime,
    UnifiedSecurityConfig, WebSocketConnectionPermit, WebSocketHandshake, WebSocketRouteDecision,
    WebSocketRouteError, WebSocketRouterRuntime, apply_browser_websocket_upstream_credentials,
    apply_correlation_request, apply_correlation_response, apply_cors_response,
    apply_header_request, apply_header_response, apply_path_prefix_service,
    apply_rate_limit_headers, apply_router_upstream_request, apply_token_request,
    apply_websocket_upstream_request, build_metrics_event, check_rate_limit,
    correlation_id_for_upstream, evaluate_cors_request, load_access_control_runtime,
    load_active_handlers, load_api_key_config, load_basic_auth_config, load_correlation_config,
    load_cors_config, load_header_config, load_hmac_runtime, load_hmac_runtime_preserving,
    load_mcp_router_runtime, load_metrics_config, load_msal_auth_runtime,
    load_msal_exchange_runtime, load_path_prefix_service_config, load_pii_tokenization_runtime,
    load_proxy_route, load_rate_limit_runtime, load_router_route, load_security_runtime,
    load_stateless_auth_runtime, load_static_resources, load_token_runtime,
    load_unified_security_config, load_websocket_router_runtime_with_policy,
    merge_extra_response_headers, record_mcp_router_reload_rejection, record_spa_auth_legacy_get,
    select_router_target, validate_mcp_router_runtime_config, validate_unified_security_config,
    verify_api_key, verify_basic_auth, verify_jwt_request, verify_unified_security,
    websocket_policy_endpoint,
};
use light_runtime::{
    AdmissionGate, AdmissionKind, AdmissionPermit, CacheRegistry, ConfigManager,
    LifecycleRegistrar, LightRuntimeBuilder, ModuleKind, RegistryHandler, ReloadContext,
    ReloadOutcome, ReloadableModule, RuntimeConfig, RuntimeError, ShutdownWatcher, TracingOptions,
    init_tracing,
};
use llm_gateway::LlmRuntime;
use llm_gateway::audit::{AuditSinkConfig, AuditSinkTask, ProcessAudit, WalAudit, WalConfig};
use llm_gateway::config::{LLM_ROUTER_FILE, LLM_ROUTER_MODULE_ID, LlmRouterConfig};
use llm_gateway::credentials::{EnvironmentReferenceSecretResolver, SecretResolver};
use llm_gateway::http::{
    BufferedHttpRequest, LlmBufferedHttp, LlmHttpResponse, PreauthorizedBodyAccessControl,
    StreamingHttpResponse,
};
use llm_gateway::runtime::{
    LlmCompiler, LlmSnapshotStore, ReadinessControllerTask, start_readiness_controller,
};
use pingora::http::{HMap, ResponseHeader};
use pingora::prelude::{HttpPeer, ProxyHttp, Session};
use pingora::utils::tls::CertKey;
use pingora::{Error, ErrorType};
use serde_json::{Value as JsonValue, json};
use sqlx::postgres::PgPoolOptions;
use std::collections::{BTreeMap, BTreeSet};
use std::io::BufReader;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncReadExt;
use tokio::sync::Semaphore;
use tokio::time::timeout;
use tracing::{debug, info, warn};
use url::form_urlencoded;

mod live_validation;
use live_validation::{
    LiveValidationOptions, live_validation_usage, parse_live_validation_options,
};
mod operational_evidence;
use operational_evidence::{GatewayEvidenceRuntime, load_gateway_evidence_runtime};

mod embedded_config {
    include!(concat!(env!("OUT_DIR"), "/embedded_config.rs"));
}

const CONFIG_DIR: &str = "config";
const DEFAULT_CONFIG_DIR: &str = "config-defaults";
const EXTERNAL_CONFIG_DIR: &str = "config-cache";
const HEALTH_PATH: &str = "/health";
const ACCESS_CONTROL_MAX_BODY_SIZE: usize = 10 * 1024 * 1024;
const RUNTIME_INSTANCE_QUERY_ENDPOINT: &str = "lightapi.net/instance/getRuntimeInstance/0.1.0";

#[derive(Clone, Copy)]
enum SpaSessionEndpointRoute {
    Exchange,
    ExchangeLogout,
    AuthLogin,
    AuthLogout,
    StatelessAuthorization,
    StatelessLogout,
    GoogleCallback,
    FacebookCallback,
    GithubCallback,
}

impl SpaSessionEndpointRoute {
    fn handler_id(self) -> &'static str {
        match self {
            Self::Exchange | Self::ExchangeLogout => "msal-exchange",
            Self::AuthLogin | Self::AuthLogout => "msal-auth",
            Self::StatelessAuthorization | Self::StatelessLogout => "stateless",
            Self::GoogleCallback => "google",
            Self::FacebookCallback => "facebook",
            Self::GithubCallback => "github",
        }
    }

    fn allowed_method(self) -> &'static str {
        match self {
            Self::Exchange
            | Self::ExchangeLogout
            | Self::AuthLogin
            | Self::AuthLogout
            | Self::StatelessLogout => "POST",
            Self::StatelessAuthorization
            | Self::GoogleCallback
            | Self::FacebookCallback
            | Self::GithubCallback => "GET",
        }
    }

    fn allow_header(self) -> &'static str {
        match self {
            Self::Exchange
            | Self::ExchangeLogout
            | Self::AuthLogin
            | Self::AuthLogout
            | Self::StatelessLogout => "POST",
            Self::StatelessAuthorization
            | Self::GoogleCallback
            | Self::FacebookCallback
            | Self::GithubCallback => "GET",
        }
    }

    fn allows_method(self, method: &str) -> bool {
        method.eq_ignore_ascii_case("OPTIONS")
            || (matches!(
                self,
                Self::Exchange
                    | Self::ExchangeLogout
                    | Self::AuthLogin
                    | Self::AuthLogout
                    | Self::StatelessLogout
            ) && method.eq_ignore_ascii_case("POST"))
            || (matches!(
                self,
                Self::StatelessAuthorization
                    | Self::GoogleCallback
                    | Self::FacebookCallback
                    | Self::GithubCallback
            ) && method.eq_ignore_ascii_case("GET"))
    }

    fn legacy_get_endpoint(self) -> Option<SpaAuthLegacyEndpoint> {
        match self {
            Self::Exchange => Some(SpaAuthLegacyEndpoint::MsalExchange),
            Self::ExchangeLogout => Some(SpaAuthLegacyEndpoint::MsalExchangeLogout),
            Self::AuthLogout => Some(SpaAuthLegacyEndpoint::MsalAuthLogout),
            Self::StatelessLogout => Some(SpaAuthLegacyEndpoint::StatelessLogout),
            Self::AuthLogin
            | Self::StatelessAuthorization
            | Self::GoogleCallback
            | Self::FacebookCallback
            | Self::GithubCallback => None,
        }
    }
}

fn spa_session_rejection_uses_cors(
    active_handlers: &ActiveHandlerSet,
    request_path: &str,
    endpoint: SpaSessionEndpointRoute,
) -> Result<bool, RuntimeError> {
    let resolved =
        active_handlers.resolve_handler_chain(request_path, endpoint.allowed_method())?;
    let cors_index = resolved.handler_ids.iter().position(|id| id == "cors");
    let auth_index = resolved
        .handler_ids
        .iter()
        .position(|id| id == endpoint.handler_id());
    Ok(matches!((cors_index, auth_index), (Some(cors), Some(auth)) if cors < auth))
}

fn capture_cors_outcome(
    ctx: &mut GatewayRequestContext,
    outcome: CorsRequestOutcome,
) -> Option<u16> {
    match outcome {
        CorsRequestOutcome::Continue(headers) => {
            ctx.cors = headers;
            None
        }
        CorsRequestOutcome::Respond { status, headers } => {
            ctx.cors = Some(headers);
            Some(status)
        }
    }
}

fn spa_session_method_rejection(
    endpoint: SpaSessionEndpointRoute,
    method: &str,
) -> Option<HandlerRejection> {
    if endpoint.allows_method(method) {
        return None;
    }
    if method.eq_ignore_ascii_case("GET") {
        if let Some(legacy_endpoint) = endpoint.legacy_get_endpoint() {
            record_spa_auth_legacy_get(legacy_endpoint);
        }
    }
    Some(
        HandlerRejection::new(405, "ERR10008", "method not allowed")
            .with_header("allow", endpoint.allow_header())
            .with_header("cache-control", "no-store"),
    )
}

fn status_allows_content_length(status: u16) -> bool {
    status != 204
}

fn should_write_response_body(status: u16, is_head: bool) -> bool {
    !is_head && status != 204
}

fn buffered_embedding_drain_deadline(
    body_bytes: usize,
    write_timeout: Duration,
    minimum_drain_bytes_per_second: u64,
) -> Duration {
    let rate_ms = u64::try_from(body_bytes)
        .unwrap_or(u64::MAX)
        .saturating_mul(1_000)
        .div_ceil(minimum_drain_bytes_per_second)
        .max(1_000);
    write_timeout.saturating_add(Duration::from_millis(rate_ms))
}

#[derive(Clone, Default)]
struct GatewayApp {
    hmac_replay_admin: Arc<HmacReplayAdmin>,
}

impl PingoraApp for GatewayApp {
    type Proxy = GatewayProxy;

    fn proxy(
        &self,
        config: &RuntimeConfig,
        _lifecycle: &LifecycleRegistrar,
        admission: &AdmissionGate,
    ) -> Result<Self::Proxy, RuntimeError> {
        GatewayProxy::from_runtime_config_with_admission_and_admin(
            config,
            admission.clone(),
            Arc::clone(&self.hmac_replay_admin),
        )
    }
}

#[derive(Default)]
struct HmacReplayAdmin {
    runtime: RwLock<Option<HmacRuntime>>,
}

impl HmacReplayAdmin {
    fn replace(&self, runtime: Option<HmacRuntime>) {
        *self
            .runtime
            .write()
            .unwrap_or_else(|error| error.into_inner()) = runtime;
    }

    fn runtime(&self) -> Option<HmacRuntime> {
        self.runtime
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
}

struct HmacReplayRegistryHandler {
    admin: Arc<HmacReplayAdmin>,
}

#[async_trait]
impl RegistryHandler for HmacReplayRegistryHandler {
    async fn handle_request(&self, method: &str, params: JsonValue) -> JsonValue {
        if method == "tools/list" {
            let tools = self.admin.runtime().map_or_else(Vec::new, |_| {
                vec![json!({
                    "name": "remove_webhook_replay",
                    "description": "Administratively remove one HMAC webhook replay reservation before redelivery.",
                    "inputSchema": {
                        "type": "object",
                        "required": ["profile", "selector", "deliveryId"],
                        "properties": {
                            "profile": { "type": "string" },
                            "selector": { "type": "string" },
                            "deliveryId": { "type": "string" }
                        }
                    }
                })]
            });
            return json!({ "tools": tools });
        }
        if method != "tools/call"
            || params.get("name").and_then(JsonValue::as_str) != Some("remove_webhook_replay")
        {
            return json!({
                "supported": false,
                "status": "unsupported",
                "error": {
                    "code": "unsupported_method",
                    "message": format!("registry method `{method}` is not supported")
                }
            });
        }
        let arguments = params.get("arguments").and_then(JsonValue::as_object);
        let field = |name: &str| {
            arguments
                .and_then(|arguments| arguments.get(name))
                .and_then(JsonValue::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
        };
        let (Some(profile), Some(selector), Some(delivery_id)) =
            (field("profile"), field("selector"), field("deliveryId"))
        else {
            return json!({
                "status": "error",
                "message": "profile, selector, and deliveryId must be non-empty strings"
            });
        };
        let Some(runtime) = self.admin.runtime() else {
            return json!({ "status": "error", "message": "HMAC replay administration is unavailable" });
        };
        match runtime
            .force_remove_replay(profile, selector, delivery_id)
            .await
        {
            Ok(outcome) => {
                info!(
                    target: "light_gateway::hmac_audit",
                    event = "webhook_replay_removed",
                    profile,
                    removed = outcome.removed,
                    scope = outcome.scope.as_str(),
                    "administrative webhook replay removal completed"
                );
                json!({
                    "status": "success",
                    "removed": outcome.removed,
                    "scope": outcome.scope.as_str()
                })
            }
            Err(error) => {
                warn!(
                    target: "light_gateway::hmac_audit",
                    event = "webhook_replay_removal_failed",
                    profile,
                    error = %error,
                    "administrative webhook replay removal failed"
                );
                json!({ "status": "error", "message": error.to_string() })
            }
        }
    }
}

struct GatewayProxy {
    admission: AdmissionGate,
    agent_delegation: Option<Arc<DelegationVerifier>>,
    workflow_delegation: Option<Arc<DelegationVerifier>>,
    agent_delegation_replay: Option<Arc<dyn DelegationReplayStore>>,
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    correlation_config: Arc<ConfigManager<Option<CorrelationConfig>>>,
    cors_config: Arc<ConfigManager<Option<CorsConfig>>>,
    metrics_config: Arc<ConfigManager<Option<MetricsConfig>>>,
    header_config: Arc<ConfigManager<Option<HeaderConfig>>>,
    #[cfg_attr(not(test), allow(dead_code))]
    hmac_runtime: Arc<ConfigManager<Option<HmacRuntime>>>,
    security_execution: Arc<ConfigManager<GatewaySecurityExecutionSnapshot>>,
    hmac_body_bytes: Arc<AtomicUsize>,
    hmac_metrics: Arc<HmacMetricsRecorder>,
    rate_limit_runtime: Arc<ConfigManager<Option<RateLimitRuntime>>>,
    path_prefix_service_config: Arc<ConfigManager<Option<PathPrefixServiceConfig>>>,
    token_runtime: Arc<ConfigManager<Option<TokenRuntime>>>,
    stateless_auth: Arc<ConfigManager<Option<StatelessAuthRuntime>>>,
    msal_exchange: Arc<ConfigManager<Option<MsalExchangeRuntime>>>,
    msal_auth: Arc<ConfigManager<Option<MsalAuthRuntime>>>,
    pii_tokenization: Arc<ConfigManager<Option<PiiTokenizationRuntime>>>,
    access_control: Arc<ConfigManager<Option<AccessControlRuntime>>>,
    mcp_router: Arc<ConfigManager<Option<McpRouterRuntime>>>,
    websocket_router: Arc<ConfigManager<Option<WebSocketRouterRuntime>>>,
    gateway_evidence: Option<Arc<GatewayEvidenceRuntime>>,
    llm_gateway: Arc<ArcSwapOption<LlmGatewayModule>>,
    metrics_recorder: Arc<MetricsRecorder>,
    proxy_route: Arc<ConfigManager<Option<ProxyRoute>>>,
    router_route: Arc<ConfigManager<Option<RouterRoute>>>,
    static_resources: Arc<ConfigManager<StaticResourceSet>>,
    next_upstream: AtomicUsize,
    upstream_verify_hostname: bool,
    upstream_client_cert_key: Option<Arc<CertKey>>,
    upstream_connect_timeout: Option<Duration>,
    upstream_circuit_error_threshold: u32,
    upstream_circuit_reset_timeout: Duration,
    upstream_circuits: Mutex<BTreeMap<String, UpstreamCircuitState>>,
    server_scheme: String,
    server_port: u16,
}

#[derive(Clone)]
struct GatewaySecurityExecutionSnapshot {
    generation: u64,
    active_handlers: Arc<ActiveHandlerSet>,
    api_key: Arc<Option<ApiKeyConfig>>,
    basic_auth: Arc<Option<BasicAuthConfig>>,
    security: Arc<Option<SecurityRuntime>>,
    unified_security: Arc<Option<UnifiedSecurityConfig>>,
    hmac: Arc<Option<HmacRuntime>>,
}

impl GatewaySecurityExecutionSnapshot {
    fn new(
        generation: u64,
        active_handlers: ActiveHandlerSet,
        api_key: Option<ApiKeyConfig>,
        basic_auth: Option<BasicAuthConfig>,
        security: Option<SecurityRuntime>,
        unified_security: Option<UnifiedSecurityConfig>,
        hmac: Option<HmacRuntime>,
    ) -> Self {
        Self {
            generation,
            active_handlers: Arc::new(active_handlers),
            api_key: Arc::new(api_key),
            basic_auth: Arc::new(basic_auth),
            security: Arc::new(security),
            unified_security: Arc::new(unified_security),
            hmac: Arc::new(hmac),
        }
    }
}

#[derive(Default)]
struct HmacMetricsRecorder {
    requests: Mutex<BTreeMap<(String, &'static str), u64>>,
    body_bytes: Mutex<BTreeMap<String, u64>>,
    verification_micros: Mutex<BTreeMap<String, (u64, u64)>>,
    replay_operations: Mutex<BTreeMap<(&'static str, &'static str, &'static str), u64>>,
}

impl HmacMetricsRecorder {
    fn request(&self, profile: &str, outcome: &'static str) {
        let mut values = self
            .requests
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let value = values.entry((profile.to_string(), outcome)).or_default();
        *value = value.saturating_add(1);
        let value = *value;
        drop(values);
        info!(
            target: "light_pingora::metrics",
            metric = "hmac_webhook_requests_total",
            profile,
            outcome,
            value,
            "HMAC metric"
        );
    }

    fn body(&self, profile: &str, bytes: usize) {
        let mut values = self
            .body_bytes
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let value = values.entry(profile.to_string()).or_default();
        *value = value.saturating_add(u64::try_from(bytes).unwrap_or(u64::MAX));
        let total_bytes = *value;
        drop(values);
        info!(
            target: "light_pingora::metrics",
            metric = "hmac_webhook_body_bytes",
            profile,
            observed_bytes = bytes,
            total_bytes,
            "HMAC metric"
        );
    }

    fn verification(&self, profile: &str, duration: Duration) {
        let mut values = self
            .verification_micros
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let value = values.entry(profile.to_string()).or_default();
        value.0 = value
            .0
            .saturating_add(u64::try_from(duration.as_micros()).unwrap_or(u64::MAX));
        value.1 = value.1.saturating_add(1);
        let (total_micros, count) = *value;
        drop(values);
        info!(
            target: "light_pingora::metrics",
            metric = "hmac_webhook_verification_duration_seconds",
            profile,
            observed_micros = u64::try_from(duration.as_micros()).unwrap_or(u64::MAX),
            total_micros,
            count,
            "HMAC metric"
        );
    }

    fn replay(&self, store_type: &'static str, operation: &'static str, outcome: &'static str) {
        let mut values = self
            .replay_operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let value = values.entry((store_type, operation, outcome)).or_default();
        *value = value.saturating_add(1);
        let value = *value;
        drop(values);
        info!(
            target: "light_pingora::metrics",
            metric = "hmac_replay_operations_total",
            store_type,
            operation,
            outcome,
            value,
            "HMAC metric"
        );
    }

    fn local_entries(&self, entries: usize) {
        info!(
            target: "light_pingora::metrics",
            metric = "hmac_replay_local_entries",
            value = entries,
            "HMAC metric"
        );
    }
}

struct HmacBodyPermit {
    used: Arc<AtomicUsize>,
    bytes: usize,
}

impl HmacBodyPermit {
    fn acquire(used: Arc<AtomicUsize>, bytes: usize, limit: usize) -> Result<Self, ()> {
        let mut current = used.load(Ordering::Acquire);
        loop {
            let Some(next) = current.checked_add(bytes) else {
                return Err(());
            };
            if next > limit {
                return Err(());
            }
            match used.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Ok(Self { used, bytes }),
                Err(observed) => current = observed,
            }
        }
    }

    fn grow(&mut self, bytes: usize, limit: usize) -> Result<(), ()> {
        let added = Self::acquire(Arc::clone(&self.used), bytes, limit)?;
        self.bytes = self.bytes.saturating_add(added.bytes);
        std::mem::forget(added);
        Ok(())
    }
}

impl Drop for HmacBodyPermit {
    fn drop(&mut self) {
        self.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

#[derive(Default)]
enum WebhookReplayState {
    #[default]
    NotRequired,
    Reserved {
        store: Arc<dyn light_pingora::WebhookReplayStore>,
        reservation: ReplayReservation,
    },
    Committed2xx,
    Releasing,
    Released,
}

struct LlmGatewayModule {
    runtime: Arc<LlmRuntime>,
    http: LlmBufferedHttp,
    max_request_body_bytes: usize,
    max_embedding_request_body_bytes: usize,
    embedding_ingress_permits: Arc<Semaphore>,
    embedding_body_read_timeout: Duration,
    embedding_minimum_receive_bytes_per_second: u64,
    embedding_authorization_timeout: Duration,
    audit_sink_task: Option<Arc<AuditSinkTask>>,
    readiness_task: Arc<ReadinessControllerTask>,
    audit_fingerprint: String,
}

fn load_llm_gateway_module(
    runtime_config: &RuntimeConfig,
    active: bool,
    generation: u64,
    previous: Option<&Arc<LlmGatewayModule>>,
) -> Result<Option<Arc<LlmGatewayModule>>, RuntimeError> {
    if !active {
        stop_llm_background_tasks(previous);
        return Ok(None);
    }
    let config: LlmRouterConfig = runtime_config
        .module_registry
        .load_config(runtime_config, LLM_ROUTER_FILE)?;
    runtime_config.module_registry.register_loaded_config(
        LLM_ROUTER_MODULE_ID,
        "llm-router",
        ModuleKind::Framework,
        &config,
        [],
        config.enabled,
        Some(config.enabled),
        true,
    )?;
    if !config.enabled {
        stop_llm_background_tasks(previous);
        return Ok(None);
    }
    let resolver: Arc<dyn SecretResolver> = Arc::new(EnvironmentReferenceSecretResolver::new(
        config.runtime_material.credential_environment.clone(),
    ));
    let compiler = Arc::new(LlmCompiler::new(resolver));
    let previous_snapshot = previous.map(|module| module.runtime.snapshot());
    let snapshot = compiler
        .compile(&config, generation, previous_snapshot.as_deref())
        .map_err(|error| RuntimeError::Config(error.to_string()))?;
    let audit_fingerprint = serde_json::to_string(&config.audit_runtime)
        .map_err(|error| RuntimeError::Config(error.to_string()))?;
    if previous.is_some_and(|module| module.audit_fingerprint != audit_fingerprint) {
        return Err(RuntimeError::Config(
            "LLM auditRuntime changes require a gateway restart to preserve single-writer WAL ownership"
                .to_string(),
        ));
    }
    let reusable_runtime = previous.filter(|module| {
        let snapshot = module.runtime.snapshot();
        snapshot.global_concurrency == config.global_concurrency
            && snapshot.global_stream_concurrency == config.global_stream_concurrency
            && module.audit_fingerprint == audit_fingerprint
    });
    let (runtime, audit_sink_task) = match reusable_runtime {
        Some(previous) => {
            previous.runtime.publish(snapshot);
            (
                Arc::clone(&previous.runtime),
                previous.audit_sink_task.clone(),
            )
        }
        None => {
            let store = Arc::new(LlmSnapshotStore::new(snapshot, 2));
            let (audit, audit_sink_task): (
                Arc<dyn llm_gateway::audit::AuditAdmission>,
                Option<Arc<AuditSinkTask>>,
            ) = if config
                .aliases
                .values()
                .any(|alias| alias.audit != llm_gateway::config::AuditMode::Disabled)
            {
                let wal_audit = WalAudit::open(
                    WalConfig {
                        directory: config.audit_runtime.directory.clone().into(),
                        gateway_instance: config.audit_runtime.gateway_instance.clone(),
                        max_record_bytes: config.audit_runtime.max_record_bytes,
                        max_segment_bytes: config.audit_runtime.max_segment_bytes,
                        max_spool_bytes: config.audit_runtime.max_spool_bytes,
                        queue_records: config.audit_runtime.queue_records,
                        batch_records: config.audit_runtime.batch_records,
                        batch_bytes: config.audit_runtime.batch_bytes,
                        commit_delay: Duration::from_millis(config.audit_runtime.commit_delay_ms),
                        terminal_commit_before_response: config
                            .audit_runtime
                            .terminal_commit_before_response,
                        persistent_volume: config.audit_runtime.persistent_volume,
                    },
                    config.audit_runtime.host_id.clone(),
                )
                .map_err(|error| RuntimeError::Config(error.to_string()))?;
                let sink_task = config
                    .audit_runtime
                    .sink_database_url_env
                    .as_deref()
                    .map(|name| {
                        let url = std::env::var(name).map_err(|_| {
                            RuntimeError::Config(format!(
                                "LLM audit sink database environment variable {name} is unavailable"
                            ))
                        })?;
                        let pool = sqlx::postgres::PgPoolOptions::new()
                            .max_connections(4)
                            .connect_lazy(&url)
                            .map_err(|error| RuntimeError::Config(error.to_string()))?;
                        Ok::<_, RuntimeError>(Arc::new(wal_audit.start_postgres_sink(
                            pool,
                            AuditSinkConfig {
                                batch_records: config.audit_runtime.sink_batch_records,
                                batch_bytes: config.audit_runtime.sink_batch_bytes,
                                poll_interval: Duration::from_millis(
                                    config.audit_runtime.sink_poll_ms,
                                ),
                                retry_initial: Duration::from_millis(
                                    config.audit_runtime.sink_poll_ms,
                                ),
                                retry_max: Duration::from_millis(
                                    config.audit_runtime.sink_retry_max_ms,
                                ),
                            },
                        )))
                    })
                    .transpose()?;
                (Arc::new(wal_audit), sink_task)
            } else {
                (Arc::new(ProcessAudit::default()), None)
            };
            (Arc::new(LlmRuntime::new(store, audit)), audit_sink_task)
        }
    };
    let readiness_task = previous
        .filter(|module| Arc::ptr_eq(&module.runtime, &runtime))
        .map(|module| Arc::clone(&module.readiness_task))
        .unwrap_or_else(|| {
            start_readiness_controller(runtime.snapshot_store(), Duration::from_secs(1))
        });
    let http = LlmBufferedHttp::new(
        Arc::clone(&runtime),
        // The Pingora handler requires ctx.access_control_exchange before it
        // can call this adapter. That exchange proves the configured rule saw
        // these exact captured bytes before LLM JSON/alias parsing.
        Arc::new(PreauthorizedBodyAccessControl),
        config.max_request_body_bytes,
        config.max_json_depth,
        Duration::from_millis(config.request_timeout_ms),
    )
    .with_openai_extension_allowlist(config.openai_extension_allowlist.clone());
    if let Some(previous_task) = previous.map(|module| &module.readiness_task)
        && !Arc::ptr_eq(previous_task, &readiness_task)
    {
        previous_task.stop();
    }
    Ok(Some(Arc::new(LlmGatewayModule {
        runtime,
        http,
        max_request_body_bytes: config.max_request_body_bytes,
        max_embedding_request_body_bytes: config.embedding_memory.max_request_body_bytes,
        embedding_ingress_permits: Arc::new(Semaphore::new(
            config.embedding_memory.ingress_concurrency,
        )),
        embedding_body_read_timeout: Duration::from_millis(
            config.embedding_memory.body_read_timeout_ms,
        ),
        embedding_minimum_receive_bytes_per_second: config
            .embedding_memory
            .minimum_receive_bytes_per_second,
        embedding_authorization_timeout: Duration::from_millis(
            config.embedding_memory.authorization_timeout_ms,
        ),
        audit_sink_task,
        readiness_task,
        audit_fingerprint,
    })))
}

fn load_llm_gateway_module_at_startup(
    runtime_config: &RuntimeConfig,
    active: bool,
) -> Option<Arc<LlmGatewayModule>> {
    match load_llm_gateway_module(runtime_config, active, 1, None) {
        Ok(module) => module,
        Err(error) => {
            runtime_config.module_registry.register_config(
                LLM_ROUTER_MODULE_ID,
                "llm-router",
                ModuleKind::Framework,
                json!({
                    "status": "unavailable",
                    "reasonCode": "LLM_CONFIG_INVALID"
                }),
                [],
                false,
                None,
                true,
            );
            tracing::error!(
                target: "light_gateway::llm",
                component = "llm-router",
                state = "unavailable",
                reasonCode = "LLM_CONFIG_INVALID",
                error = %error,
                "LLM configuration is invalid; starting gateway with LLM routing unavailable"
            );
            None
        }
    }
}

fn stop_llm_background_tasks(previous: Option<&Arc<LlmGatewayModule>>) {
    if let Some(module) = previous {
        if let Some(task) = module.audit_sink_task.as_ref() {
            task.stop();
        }
        module.readiness_task.stop();
    }
}

#[async_trait]
trait DelegationReplayStore: Send + Sync {
    /// Atomically consumes a replay identifier. `false` means it was already consumed.
    async fn consume(&self, claims: &DelegationClaims) -> Result<bool, String>;
}

struct PostgresDelegationReplayStore {
    pool: sqlx::PgPool,
    gateway_instance: String,
}

#[async_trait]
impl DelegationReplayStore for PostgresDelegationReplayStore {
    async fn consume(&self, claims: &DelegationClaims) -> Result<bool, String> {
        let expires_ts =
            DateTime::<Utc>::from_timestamp(claims.expires_at, 0).ok_or_else(|| {
                "delegation expiry is outside the supported timestamp range".to_string()
            })?;
        // Keep cleanup bounded and opportunistic. The expiry index makes this
        // cheap, while the primary key remains the authoritative replay fence.
        sqlx::query(
            "DELETE FROM agent_delegation_replay_t WHERE ctid IN
             (SELECT ctid FROM agent_delegation_replay_t
              WHERE expires_ts <= CURRENT_TIMESTAMP
              ORDER BY expires_ts LIMIT 256)",
        )
        .execute(&self.pool)
        .await
        .map_err(|error| format!("shared delegation replay cleanup failed: {error}"))?;
        let result = sqlx::query(
            "INSERT INTO agent_delegation_replay_t
             (host_id,audience,replay_id,token_id,action_attempt_id,issuer,gateway_instance,expires_ts)
             VALUES($1,$2,$3,$4,$5,$6,$7,$8)
             ON CONFLICT(audience,replay_id) DO NOTHING",
        )
        .bind(claims.host_id)
        .bind(&claims.audience)
        .bind(claims.replay_id)
        .bind(claims.token_id)
        .bind(claims.action_attempt_id)
        .bind(&claims.issuer)
        .bind(&self.gateway_instance)
        .bind(expires_ts)
        .execute(&self.pool)
        .await
        .map_err(|error| format!("shared delegation replay store is unavailable: {error}"))?;
        Ok(result.rows_affected() == 1)
    }
}

impl GatewayProxy {
    fn active_spa_session_endpoint(
        &self,
        active_handlers: &ActiveHandlerSet,
        request_path: &str,
    ) -> Result<Option<SpaSessionEndpointRoute>, RuntimeError> {
        let mut candidates = Vec::new();
        let exchange = self.msal_exchange.load();
        if let Some(runtime) = exchange.as_ref().as_ref() {
            if request_path == runtime.config().exchange_path {
                candidates.push(SpaSessionEndpointRoute::Exchange);
            }
            if request_path == runtime.config().logout_path {
                candidates.push(SpaSessionEndpointRoute::ExchangeLogout);
            }
        }
        let auth = self.msal_auth.load();
        if let Some(runtime) = auth
            .as_ref()
            .as_ref()
            .filter(|runtime| runtime.config.enabled)
        {
            if request_path == runtime.config.login_path {
                candidates.push(SpaSessionEndpointRoute::AuthLogin);
            }
            if request_path == runtime.config.logout_path {
                candidates.push(SpaSessionEndpointRoute::AuthLogout);
            }
        }
        let stateless = self.stateless_auth.load();
        if let Some(runtime) = stateless.as_ref().as_ref() {
            let config = runtime.config();
            if active_handlers.is_handler_active("stateless") && request_path == config.auth_path {
                candidates.push(SpaSessionEndpointRoute::StatelessAuthorization);
            }
            if active_handlers.is_handler_active("stateless") && request_path == config.logout_path
            {
                candidates.push(SpaSessionEndpointRoute::StatelessLogout);
            }
            if active_handlers.is_handler_active("google") && request_path == config.google_path {
                candidates.push(SpaSessionEndpointRoute::GoogleCallback);
            }
            if active_handlers.is_handler_active("facebook") && request_path == config.facebook_path
            {
                candidates.push(SpaSessionEndpointRoute::FacebookCallback);
            }
            if active_handlers.is_handler_active("github") && request_path == config.github_path {
                candidates.push(SpaSessionEndpointRoute::GithubCallback);
            }
        }

        let fallback = candidates.first().copied();
        let mut selected = None;
        for endpoint in candidates {
            let resolved =
                active_handlers.resolve_handler_chain(request_path, endpoint.allowed_method())?;
            let Some(handler_index) = resolved
                .handler_ids
                .iter()
                .position(|id| id == endpoint.handler_id())
            else {
                continue;
            };
            if selected
                .as_ref()
                .is_none_or(|(selected_index, _)| handler_index < *selected_index)
            {
                selected = Some((handler_index, endpoint));
            }
        }
        Ok(selected.map(|(_, endpoint)| endpoint).or(fallback))
    }

    async fn authenticate_agent_delegation(
        &self,
        session: &Session,
    ) -> Option<Result<(AuthPrincipal, DelegationClaims), HandlerRejection>> {
        let authorization = request_header(session, "authorization")?;
        let (scheme, token) = authorization.split_once(' ')?;
        if !scheme.eq_ignore_ascii_case("bearer") || !token.starts_with(&format!("{TOKEN_PREFIX}."))
        {
            return None;
        }
        if self.agent_delegation.is_none() && self.workflow_delegation.is_none() {
            return Some(Err(HandlerRejection::unauthorized(
                "agent delegation is not configured",
            )));
        }
        let claims = self
            .agent_delegation
            .as_ref()
            .and_then(|verifier| verifier.verify_token(token).ok())
            .or_else(|| {
                self.workflow_delegation
                    .as_ref()
                    .and_then(|verifier| verifier.verify_token(token).ok())
            });
        let claims = match claims {
            Some(claims) => claims,
            None => {
                return Some(Err(HandlerRejection::unauthorized(
                    "invalid agent delegation",
                )));
            }
        };
        let Some(replay_store) = self.agent_delegation_replay.as_ref() else {
            return Some(Err(HandlerRejection::unauthorized(
                "agent delegation replay protection is not configured",
            )));
        };
        match replay_store.consume(&claims).await {
            Ok(true) => {}
            Ok(false) => {
                return Some(Err(HandlerRejection::unauthorized(
                    "invalid or replayed agent delegation",
                )));
            }
            Err(error) => {
                warn!(error = %error, "rejecting delegation because shared replay storage failed");
                return Some(Err(HandlerRejection::unauthorized(
                    "agent delegation replay protection is unavailable",
                )));
            }
        }
        let principal = AuthPrincipal {
            client_id: Some(claims.agent_actor.clone()),
            user_id: Some(claims.caller_subject.clone()),
            issuer: Some(claims.issuer.clone()),
            host: Some(claims.host_id.to_string()),
            role: claims
                .caller_claims
                .get("role")
                .and_then(JsonValue::as_str)
                .map(str::to_string),
            claims: claims.caller_claims.clone(),
            ..AuthPrincipal::default()
        };
        Some(Ok((principal, claims)))
    }

    #[cfg(test)]
    fn from_runtime_config(config: &RuntimeConfig) -> Result<Self, RuntimeError> {
        let admission = AdmissionGate::default();
        admission.open();
        Self::from_runtime_config_with_admission(config, admission)
    }

    #[cfg(test)]
    fn from_runtime_config_with_admission(
        config: &RuntimeConfig,
        admission: AdmissionGate,
    ) -> Result<Self, RuntimeError> {
        Self::from_runtime_config_with_admission_and_admin(
            config,
            admission,
            Arc::new(HmacReplayAdmin::default()),
        )
    }

    fn from_runtime_config_with_admission_and_admin(
        config: &RuntimeConfig,
        admission: AdmissionGate,
        hmac_replay_admin: Arc<HmacReplayAdmin>,
    ) -> Result<Self, RuntimeError> {
        let active_handlers = load_active_handlers(config, &gateway_handler_registry())?;
        let correlation_config =
            load_correlation_config(config, active_handlers.is_handler_active("correlation"))?;
        let cors_config = load_cors_config(config, active_handlers.is_handler_active("cors"))?;
        let metrics_config =
            load_metrics_config(config, active_handlers.is_handler_active("metrics"))?;
        let header_config = load_header_config(
            config,
            handler_active(&active_handlers, &["header", "headers"]),
        )?;
        let api_key_config = load_api_key_config(
            config,
            handler_active(
                &active_handlers,
                &["api-key", "apikey", "unified-security", "unified"],
            ),
        )?;
        let basic_auth_config = load_basic_auth_config(
            config,
            handler_active(
                &active_handlers,
                &["basic-auth", "basic", "unified-security", "unified"],
            ),
        )?;
        let security_runtime = load_security_runtime(
            config,
            handler_active(
                &active_handlers,
                &["security", "jwt", "unified-security", "unified"],
            ),
        )?;
        let unified_security_config = load_unified_security_config(
            config,
            handler_active(&active_handlers, &["unified-security", "unified"]),
        )?;
        let hmac_runtime = load_hmac_runtime(
            config,
            active_handlers.is_handler_active("hmac")
                || unified_security_config
                    .as_ref()
                    .is_some_and(UnifiedSecurityConfig::requires_hmac),
        )?;
        if let Some(unified) = unified_security_config.as_ref() {
            validate_unified_security_config(unified, hmac_runtime.as_ref())?;
        }
        if let (Some(runtime), Some(cache_registry)) =
            (hmac_runtime.as_ref(), config.cache_registry.as_ref())
        {
            runtime.register_local_replay_caches(cache_registry);
        }
        hmac_replay_admin.replace(hmac_runtime.clone());
        let rate_limit_runtime = load_rate_limit_runtime(
            config,
            handler_active(&active_handlers, &["limit", "rate-limit"]),
        )?;
        let path_prefix_service_config = load_path_prefix_service_config(
            config,
            handler_active(
                &active_handlers,
                &["prefix", "path-prefix-service", "pathPrefixService"],
            ),
        )?;
        let token_runtime = load_token_runtime(config, active_handlers.is_handler_active("token"))?;
        let stateless_auth = load_stateless_auth_runtime(
            config,
            handler_active(
                &active_handlers,
                &["stateless", "google", "facebook", "github"],
            ),
        )?;
        let msal_exchange =
            load_msal_exchange_runtime(config, active_handlers.is_handler_active("msal-exchange"))?;
        let msal_auth =
            load_msal_auth_runtime(config, active_handlers.is_handler_active("msal-auth"))?;
        let pii_tokenization = load_pii_tokenization_runtime(
            config,
            handler_active(&active_handlers, &["tokenize", "detokenize"]),
        )?;
        let access_control = load_access_control_runtime(
            config,
            active_handlers.is_handler_active("access-control"),
        )?;
        log_access_control_revision(access_control.as_ref());
        let mcp_router = load_mcp_router_runtime(config, active_handlers.is_handler_active("mcp"))?;
        let websocket_router = load_websocket_router_runtime_with_policy(
            config,
            active_handlers.is_handler_active("websocket"),
            access_control.clone().map(Arc::new),
        )?;
        let gateway_evidence = load_gateway_evidence_runtime(config, admission.clone())?;
        let llm_gateway =
            load_llm_gateway_module_at_startup(config, active_handlers.is_handler_active("llm"));
        let router_route = load_router_route(config, active_handlers.is_handler_active("router"))?;
        let proxy_route = load_proxy_route(config)?;
        let static_resources = load_static_resources(config)?;
        validate_hmac_effective_chains(
            &active_handlers,
            hmac_runtime.as_ref(),
            unified_security_config.as_ref(),
        )?;
        let security_execution =
            Arc::new(ConfigManager::new(GatewaySecurityExecutionSnapshot::new(
                1,
                active_handlers.clone(),
                api_key_config.clone(),
                basic_auth_config.clone(),
                security_runtime.clone(),
                unified_security_config.clone(),
                hmac_runtime.clone(),
            )));
        let active_handlers = Arc::new(ConfigManager::new(active_handlers));
        let correlation_config = Arc::new(ConfigManager::new(correlation_config));
        let cors_config = Arc::new(ConfigManager::new(cors_config));
        let metrics_config = Arc::new(ConfigManager::new(metrics_config));
        let header_config = Arc::new(ConfigManager::new(header_config));
        let api_key_config = Arc::new(ConfigManager::new(api_key_config));
        let basic_auth_config = Arc::new(ConfigManager::new(basic_auth_config));
        let security_runtime = Arc::new(ConfigManager::new(security_runtime));
        let unified_security_config = Arc::new(ConfigManager::new(unified_security_config));
        let hmac_runtime = Arc::new(ConfigManager::new(hmac_runtime));
        let rate_limit_runtime = Arc::new(ConfigManager::new(rate_limit_runtime));
        let path_prefix_service_config = Arc::new(ConfigManager::new(path_prefix_service_config));
        let token_runtime = Arc::new(ConfigManager::new(token_runtime));
        let stateless_auth = Arc::new(ConfigManager::new(stateless_auth));
        let msal_exchange = Arc::new(ConfigManager::new(msal_exchange));
        let msal_auth = Arc::new(ConfigManager::new(msal_auth));
        let pii_tokenization = Arc::new(ConfigManager::new(pii_tokenization));
        let access_control = Arc::new(ConfigManager::new(access_control));
        let mcp_router = Arc::new(ConfigManager::new(mcp_router));
        let websocket_router = Arc::new(ConfigManager::new(websocket_router));
        let llm_gateway = Arc::new(ArcSwapOption::from(llm_gateway));
        let router_route = Arc::new(ConfigManager::new(router_route));
        let proxy_route = Arc::new(ConfigManager::new(proxy_route));
        let static_resources = Arc::new(ConfigManager::new(static_resources));
        let metrics_recorder = Arc::new(MetricsRecorder::default());

        config.module_registry.register_reloader(
            light_pingora::HANDLER_MODULE_ID,
            Arc::new(HandlerReloader {
                active_handlers: Arc::clone(&active_handlers),
                correlation_config: Arc::clone(&correlation_config),
                cors_config: Arc::clone(&cors_config),
                metrics_config: Arc::clone(&metrics_config),
                header_config: Arc::clone(&header_config),
                api_key_config: Arc::clone(&api_key_config),
                basic_auth_config: Arc::clone(&basic_auth_config),
                security_runtime: Arc::clone(&security_runtime),
                unified_security_config: Arc::clone(&unified_security_config),
                hmac_runtime: Arc::clone(&hmac_runtime),
                security_execution: Arc::clone(&security_execution),
                hmac_replay_admin: Arc::clone(&hmac_replay_admin),
                rate_limit_runtime: Arc::clone(&rate_limit_runtime),
                path_prefix_service_config: Arc::clone(&path_prefix_service_config),
                token_runtime: Arc::clone(&token_runtime),
                stateless_auth: Arc::clone(&stateless_auth),
                msal_exchange: Arc::clone(&msal_exchange),
                msal_auth: Arc::clone(&msal_auth),
                pii_tokenization: Arc::clone(&pii_tokenization),
                access_control: Arc::clone(&access_control),
                mcp_router: Arc::clone(&mcp_router),
                websocket_router: Arc::clone(&websocket_router),
                llm_gateway: Arc::clone(&llm_gateway),
                router_route: Arc::clone(&router_route),
            }),
        );
        config.module_registry.register_reloader(
            light_pingora::CORRELATION_MODULE_ID,
            Arc::new(CorrelationReloader {
                active_handlers: Arc::clone(&active_handlers),
                correlation_config: Arc::clone(&correlation_config),
            }),
        );
        config.module_registry.register_reloader(
            light_pingora::CORS_MODULE_ID,
            Arc::new(CorsReloader {
                active_handlers: Arc::clone(&active_handlers),
                cors_config: Arc::clone(&cors_config),
            }),
        );
        config.module_registry.register_reloader(
            light_pingora::METRICS_MODULE_ID,
            Arc::new(MetricsReloader {
                active_handlers: Arc::clone(&active_handlers),
                metrics_config: Arc::clone(&metrics_config),
            }),
        );
        config.module_registry.register_reloader(
            light_pingora::HEADER_MODULE_ID,
            Arc::new(HeaderReloader {
                active_handlers: Arc::clone(&active_handlers),
                header_config: Arc::clone(&header_config),
            }),
        );
        config.module_registry.register_reloader(
            light_pingora::APIKEY_MODULE_ID,
            Arc::new(ApiKeyReloader {
                active_handlers: Arc::clone(&active_handlers),
                api_key_config: Arc::clone(&api_key_config),
                security_execution: Arc::clone(&security_execution),
            }),
        );
        config.module_registry.register_reloader(
            light_pingora::BASIC_AUTH_MODULE_ID,
            Arc::new(BasicAuthReloader {
                active_handlers: Arc::clone(&active_handlers),
                basic_auth_config: Arc::clone(&basic_auth_config),
                security_execution: Arc::clone(&security_execution),
            }),
        );
        config.module_registry.register_reloader(
            light_pingora::SECURITY_MODULE_ID,
            Arc::new(SecurityReloader {
                active_handlers: Arc::clone(&active_handlers),
                security_runtime: Arc::clone(&security_runtime),
                stateless_auth: Arc::clone(&stateless_auth),
                msal_exchange: Arc::clone(&msal_exchange),
                msal_auth: Arc::clone(&msal_auth),
                security_execution: Arc::clone(&security_execution),
            }),
        );
        config.module_registry.register_reloader(
            light_pingora::UNIFIED_SECURITY_MODULE_ID,
            Arc::new(UnifiedSecurityReloader {
                active_handlers: Arc::clone(&active_handlers),
                unified_security_config: Arc::clone(&unified_security_config),
                hmac_runtime: Arc::clone(&hmac_runtime),
                security_execution: Arc::clone(&security_execution),
            }),
        );
        config.module_registry.register_reloader(
            light_pingora::HMAC_MODULE_ID,
            Arc::new(HmacReloader {
                active_handlers: Arc::clone(&active_handlers),
                unified_security_config: Arc::clone(&unified_security_config),
                hmac_runtime: Arc::clone(&hmac_runtime),
                hmac_replay_admin: Arc::clone(&hmac_replay_admin),
                security_execution: Arc::clone(&security_execution),
            }),
        );
        config.module_registry.register_reloader(
            light_pingora::LIMIT_MODULE_ID,
            Arc::new(RateLimitReloader {
                active_handlers: Arc::clone(&active_handlers),
                rate_limit_runtime: Arc::clone(&rate_limit_runtime),
            }),
        );
        config.module_registry.register_reloader(
            light_pingora::PATH_PREFIX_SERVICE_MODULE_ID,
            Arc::new(PathPrefixServiceReloader {
                active_handlers: Arc::clone(&active_handlers),
                path_prefix_service_config: Arc::clone(&path_prefix_service_config),
            }),
        );
        config.module_registry.register_reloader(
            light_pingora::TOKEN_MODULE_ID,
            Arc::new(TokenReloader {
                active_handlers: Arc::clone(&active_handlers),
                token_runtime: Arc::clone(&token_runtime),
                stateless_auth: Arc::clone(&stateless_auth),
                msal_exchange: Arc::clone(&msal_exchange),
            }),
        );
        config.module_registry.register_reloader(
            light_pingora::CLIENT_TOKEN_MODULE_ID,
            Arc::new(TokenReloader {
                active_handlers: Arc::clone(&active_handlers),
                token_runtime: Arc::clone(&token_runtime),
                stateless_auth: Arc::clone(&stateless_auth),
                msal_exchange: Arc::clone(&msal_exchange),
            }),
        );
        config.module_registry.register_reloader(
            light_pingora::SIDECAR_MODULE_ID,
            Arc::new(TokenReloader {
                active_handlers: Arc::clone(&active_handlers),
                token_runtime: Arc::clone(&token_runtime),
                stateless_auth: Arc::clone(&stateless_auth),
                msal_exchange: Arc::clone(&msal_exchange),
            }),
        );
        config.module_registry.register_reloader(
            light_pingora::STATELESS_AUTH_MODULE_ID,
            Arc::new(StatelessAuthReloader {
                active_handlers: Arc::clone(&active_handlers),
                stateless_auth: Arc::clone(&stateless_auth),
            }),
        );
        config.module_registry.register_reloader(
            light_pingora::MSAL_EXCHANGE_MODULE_ID,
            Arc::new(MsalExchangeReloader {
                active_handlers: Arc::clone(&active_handlers),
                msal_exchange: Arc::clone(&msal_exchange),
            }),
        );
        config.module_registry.register_reloader(
            light_pingora::SECURITY_MSAL_MODULE_ID,
            Arc::new(MsalSecurityReloader {
                active_handlers: Arc::clone(&active_handlers),
                msal_exchange: Arc::clone(&msal_exchange),
                msal_auth: Arc::clone(&msal_auth),
            }),
        );
        config.module_registry.register_reloader(
            light_pingora::MSAL_AUTH_MODULE_ID,
            Arc::new(MsalAuthReloader {
                active_handlers: Arc::clone(&active_handlers),
                msal_auth: Arc::clone(&msal_auth),
            }),
        );
        config.module_registry.register_reloader(
            light_pingora::PII_TOKENIZATION_MODULE_ID,
            Arc::new(PiiTokenizationReloader {
                active_handlers: Arc::clone(&active_handlers),
                pii_tokenization: Arc::clone(&pii_tokenization),
            }),
        );
        let mcp_reloader: Arc<dyn ReloadableModule> = Arc::new(McpRouterReloader {
            active_handlers: Arc::clone(&active_handlers),
            mcp_router: Arc::clone(&mcp_router),
        });
        config.module_registry.register_reloader(
            light_pingora::MCP_ROUTER_MODULE_ID,
            Arc::clone(&mcp_reloader),
        );
        config.module_registry.register_reloader(
            light_pingora::WEBSOCKET_ROUTER_MODULE_ID,
            Arc::new(WebSocketRouterReloader {
                active_handlers: Arc::clone(&active_handlers),
                access_control: Arc::clone(&access_control),
                websocket_router: Arc::clone(&websocket_router),
            }),
        );
        config.module_registry.register_reloader(
            LLM_ROUTER_MODULE_ID,
            Arc::new(LlmRouterReloader {
                active_handlers: Arc::clone(&active_handlers),
                llm_gateway: Arc::clone(&llm_gateway),
            }),
        );
        let access_control_reloader: Arc<dyn ReloadableModule> = Arc::new(AccessControlReloader {
            active_handlers: Arc::clone(&active_handlers),
            access_control: Arc::clone(&access_control),
            mcp_router: Arc::clone(&mcp_router),
            websocket_router: Arc::clone(&websocket_router),
        });
        config.module_registry.register_reloader(
            light_pingora::ACCESS_CONTROL_MODULE_ID,
            Arc::clone(&access_control_reloader),
        );
        config
            .module_registry
            .register_reloader(light_pingora::RULE_MODULE_ID, access_control_reloader);
        config.module_registry.register_reloader(
            light_pingora::PROXY_MODULE_ID,
            Arc::new(ProxyReloader {
                proxy_route: Arc::clone(&proxy_route),
            }),
        );
        config.module_registry.register_reloader(
            light_pingora::ROUTER_MODULE_ID,
            Arc::new(RouterReloader {
                active_handlers: Arc::clone(&active_handlers),
                router_route: Arc::clone(&router_route),
            }),
        );
        let static_reloader: Arc<dyn ReloadableModule> = Arc::new(StaticResourceReloader {
            static_resources: Arc::clone(&static_resources),
        });
        config.module_registry.register_reloader(
            light_pingora::PATH_RESOURCE_MODULE_ID,
            Arc::clone(&static_reloader),
        );
        config
            .module_registry
            .register_reloader(light_pingora::VIRTUAL_HOST_MODULE_ID, static_reloader);

        let (upstream_circuit_error_threshold, upstream_circuit_reset_timeout) =
            upstream_circuit_config(config);
        let agent_delegation = std::env::var("LIGHT_GATEWAY_AGENT_DELEGATION_SECRET")
            .ok()
            .filter(|secret| !secret.trim().is_empty())
            .map(|secret| {
                DelegationVerifier::new(secret.as_bytes(), "light-agent", "light-gateway")
                    .map(Arc::new)
                    .map_err(|error| {
                        RuntimeError::Config(format!(
                            "invalid agent delegation configuration: {error}"
                        ))
                    })
            })
            .transpose()?;
        let workflow_delegation = std::env::var("LIGHT_GATEWAY_WORKFLOW_DELEGATION_SECRET")
            .ok()
            .filter(|secret| !secret.trim().is_empty())
            .map(|secret| {
                DelegationVerifier::new(secret.as_bytes(), "light-workflow", "light-gateway")
                    .map(Arc::new)
                    .map_err(|error| {
                        RuntimeError::Config(format!(
                            "invalid workflow delegation configuration: {error}"
                        ))
                    })
            })
            .transpose()?;
        let agent_delegation_replay = if agent_delegation.is_some() || workflow_delegation.is_some()
        {
            let database_url = std::env::var("LIGHT_GATEWAY_DELEGATION_DATABASE_URL")
                .or_else(|_| std::env::var("DATABASE_URL"))
                .map_err(|_| RuntimeError::Config(
                    "LIGHT_GATEWAY_DELEGATION_DATABASE_URL (or DATABASE_URL) is required when agent delegation is enabled".to_string(),
                ))?;
            let pool = PgPoolOptions::new()
                .max_connections(8)
                .connect_lazy(&database_url)
                .map_err(|error| {
                    RuntimeError::Config(format!("invalid delegation database URL: {error}"))
                })?;
            let gateway_instance = std::env::var("LIGHT_GATEWAY_INSTANCE_ID")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| config.service_identity.service_id.clone());
            Some(Arc::new(PostgresDelegationReplayStore {
                pool,
                gateway_instance,
            }) as Arc<dyn DelegationReplayStore>)
        } else {
            None
        };

        Ok(Self {
            admission,
            agent_delegation,
            workflow_delegation,
            agent_delegation_replay,
            active_handlers,
            correlation_config,
            cors_config,
            metrics_config,
            header_config,
            hmac_runtime,
            security_execution,
            hmac_body_bytes: Arc::new(AtomicUsize::new(0)),
            hmac_metrics: Arc::new(HmacMetricsRecorder::default()),
            rate_limit_runtime,
            path_prefix_service_config,
            token_runtime,
            stateless_auth,
            msal_exchange,
            msal_auth,
            pii_tokenization,
            access_control,
            mcp_router,
            websocket_router,
            gateway_evidence,
            llm_gateway,
            metrics_recorder,
            proxy_route,
            router_route,
            static_resources,
            next_upstream: AtomicUsize::new(0),
            upstream_verify_hostname: upstream_verify_hostname(config),
            upstream_client_cert_key: upstream_client_cert_key(config)?,
            upstream_connect_timeout: upstream_connect_timeout(config),
            upstream_circuit_error_threshold,
            upstream_circuit_reset_timeout,
            upstream_circuits: Mutex::new(BTreeMap::new()),
            server_scheme: if config.server.enable_https {
                "https".to_string()
            } else {
                "http".to_string()
            },
            server_port: if config.server.enable_https {
                config.server.https_port
            } else {
                config.server.http_port
            },
        })
    }

    fn select_upstream(&self) -> Option<(ProxyTarget, bool, bool)> {
        let route = self.proxy_route.load();
        let route = route.as_ref().as_ref()?;
        let mut first_open_target = None;
        for _ in 0..route.targets.len() {
            let index = self.next_upstream.fetch_add(1, Ordering::Relaxed);
            let Some(target) = route.select(index) else {
                continue;
            };
            if self.is_upstream_circuit_open(&target) {
                first_open_target.get_or_insert(target);
                continue;
            }
            return Some((
                target,
                route.rewrite_host_header(),
                route.config.reuse_x_forwarded,
            ));
        }
        first_open_target.map(|target| {
            (
                target,
                route.rewrite_host_header(),
                route.config.reuse_x_forwarded,
            )
        })
    }

    fn is_upstream_circuit_open(&self, target: &ProxyTarget) -> bool {
        if self.upstream_circuit_error_threshold == 0 {
            return false;
        }
        let key = upstream_circuit_key(target);
        let mut circuits = self
            .upstream_circuits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(state) = circuits.get_mut(&key) else {
            return false;
        };
        let Some(opened_at) = state.opened_at else {
            return false;
        };
        if opened_at.elapsed() < self.upstream_circuit_reset_timeout {
            return true;
        }
        state.failures = 0;
        state.opened_at = None;
        false
    }

    fn record_upstream_success(&self, ctx: &GatewayRequestContext) {
        if self.upstream_circuit_error_threshold == 0 {
            return;
        }
        let Some(target) = ctx.proxy_target.as_ref() else {
            return;
        };
        let key = upstream_circuit_key(target);
        let mut circuits = self
            .upstream_circuits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        circuits.remove(&key);
    }

    fn record_upstream_failure(&self, ctx: &GatewayRequestContext) {
        if self.upstream_circuit_error_threshold == 0 {
            return;
        }
        let Some(target) = ctx.proxy_target.as_ref() else {
            return;
        };
        let key = upstream_circuit_key(target);
        let mut circuits = self
            .upstream_circuits
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let state = circuits.entry(key).or_default();
        state.failures = state.failures.saturating_add(1);
        if state.failures >= self.upstream_circuit_error_threshold {
            state.opened_at = Some(Instant::now());
        }
    }

    async fn write_static_resolution(
        &self,
        session: &mut Session,
        ctx: &mut GatewayRequestContext,
        resolution: StaticResolution,
    ) -> pingora::Result<bool> {
        if !static_method_allowed(session) {
            return self
                .write_bytes_response_with_headers(
                    session,
                    ctx,
                    405,
                    Some("text/plain; charset=utf-8"),
                    None,
                    Bytes::from_static(b"method not allowed"),
                    &[("allow".to_string(), "GET, HEAD".to_string())],
                )
                .await;
        }

        match resolution {
            StaticResolution::File(file) => {
                let metadata = tokio::fs::metadata(&file.path).await.map_err(|error| {
                    Error::because(
                        ErrorType::FileReadError,
                        format!("failed to stat static file `{}`", file.path.display()),
                        error,
                    )
                })?;
                let validators = static_file_validators(&metadata);
                if static_request_not_modified(session, &validators) {
                    return self
                        .write_static_not_modified(session, ctx, &file, &validators)
                        .await;
                }
                if should_stream_static_file(metadata.len(), file.transfer_min_size) {
                    self.write_streaming_static_file(session, ctx, &file, &metadata, &validators)
                        .await
                } else {
                    let body = tokio::fs::read(&file.path).await.map_err(|error| {
                        Error::because(
                            ErrorType::FileReadError,
                            format!("failed to read static file `{}`", file.path.display()),
                            error,
                        )
                    })?;
                    self.write_static_bytes_response(
                        session,
                        ctx,
                        &file,
                        &validators,
                        Bytes::from(body),
                    )
                    .await
                }
            }
            StaticResolution::Forbidden => {
                self.write_text_response(session, ctx, 403, "forbidden")
                    .await
            }
            StaticResolution::NotFound => {
                self.write_text_response(session, ctx, 404, "not found")
                    .await
            }
        }
    }

    async fn write_static_not_modified(
        &self,
        session: &mut Session,
        ctx: &mut GatewayRequestContext,
        file: &light_pingora::StaticFile,
        validators: &StaticFileValidators,
    ) -> pingora::Result<bool> {
        let mut response = ResponseHeader::build(304, Some(8))?;
        response.insert_header("cache-control", file.cache_control.as_str())?;
        insert_static_validators(&mut response, validators)?;
        self.apply_response_headers(&mut response, ctx)?;
        session
            .write_response_header(Box::new(response), true)
            .await?;
        self.record_metrics(ctx, 304);
        self.log_handler_durations(ctx);
        Ok(true)
    }

    async fn write_static_bytes_response(
        &self,
        session: &mut Session,
        ctx: &mut GatewayRequestContext,
        file: &light_pingora::StaticFile,
        validators: &StaticFileValidators,
        body: Bytes,
    ) -> pingora::Result<bool> {
        let is_head = is_head_request(session);
        let mut response = self.static_response_header(file, validators, body.len() as u64)?;
        self.apply_response_headers(&mut response, ctx)?;
        session
            .write_response_header(Box::new(response), is_head)
            .await?;
        if !is_head {
            session.write_response_body(Some(body), true).await?;
        }
        self.record_metrics(ctx, 200);
        self.log_handler_durations(ctx);
        Ok(true)
    }

    async fn write_streaming_static_file(
        &self,
        session: &mut Session,
        ctx: &mut GatewayRequestContext,
        file: &light_pingora::StaticFile,
        metadata: &std::fs::Metadata,
        validators: &StaticFileValidators,
    ) -> pingora::Result<bool> {
        let is_head = is_head_request(session);
        let content_length = metadata.len();
        let mut response = self.static_response_header(file, validators, content_length)?;
        self.apply_response_headers(&mut response, ctx)?;
        let end_with_header = is_head || content_length == 0;
        session
            .write_response_header(Box::new(response), end_with_header)
            .await?;
        if end_with_header {
            self.record_metrics(ctx, 200);
            self.log_handler_durations(ctx);
            return Ok(true);
        }

        let mut file_handle = tokio::fs::File::open(&file.path).await.map_err(|error| {
            Error::because(
                ErrorType::FileReadError,
                format!("failed to open static file `{}`", file.path.display()),
                error,
            )
        })?;
        let mut buffer = vec![0_u8; 64 * 1024];
        let mut sent = 0_u64;
        loop {
            let remaining = content_length.saturating_sub(sent);
            if remaining == 0 {
                break;
            }
            let max_read = buffer.len().min(remaining as usize);
            let read = file_handle
                .read(&mut buffer[..max_read])
                .await
                .map_err(|error| {
                    Error::because(
                        ErrorType::FileReadError,
                        format!("failed to stream static file `{}`", file.path.display()),
                        error,
                    )
                })?;
            if read == 0 {
                session
                    .write_response_body(Some(Bytes::new()), true)
                    .await?;
                break;
            }
            sent = sent.saturating_add(read as u64);
            let end = sent >= content_length;
            session
                .write_response_body(Some(Bytes::copy_from_slice(&buffer[..read])), end)
                .await?;
            if end {
                break;
            }
        }

        self.record_metrics(ctx, 200);
        self.log_handler_durations(ctx);
        Ok(true)
    }

    fn static_response_header(
        &self,
        file: &light_pingora::StaticFile,
        validators: &StaticFileValidators,
        content_length: u64,
    ) -> pingora::Result<ResponseHeader> {
        let content_length = usize::try_from(content_length).map_err(|_| {
            Error::explain(
                ErrorType::InternalError,
                "static file is too large to set content-length",
            )
        })?;
        let mut response = ResponseHeader::build(200, Some(12))?;
        response.insert_header("content-type", file.content_type.as_str())?;
        response.insert_header("cache-control", file.cache_control.as_str())?;
        insert_static_validators(&mut response, validators)?;
        response.set_content_length(content_length)?;
        Ok(response)
    }

    async fn write_empty_response(
        &self,
        session: &mut Session,
        ctx: &mut GatewayRequestContext,
        status: u16,
    ) -> pingora::Result<bool> {
        self.write_bytes_response(
            session,
            ctx,
            status,
            "text/plain; charset=utf-8",
            None,
            Bytes::new(),
        )
        .await
    }

    async fn write_text_response(
        &self,
        session: &mut Session,
        ctx: &mut GatewayRequestContext,
        status: u16,
        body: &'static str,
    ) -> pingora::Result<bool> {
        self.write_bytes_response(
            session,
            ctx,
            status,
            "text/plain; charset=utf-8",
            None,
            Bytes::from_static(body.as_bytes()),
        )
        .await
    }

    async fn write_llm_error_response(
        &self,
        session: &mut Session,
        ctx: &mut GatewayRequestContext,
        status: u16,
        code: &'static str,
        message: &'static str,
    ) -> pingora::Result<bool> {
        let body =
            serde_json::to_vec(&json!({"error":{"message":message,"type":code,"code":code}}))
                .map(Bytes::from)
                .map_err(|_| {
                    Error::explain(
                        ErrorType::InternalError,
                        "LLM error response serialization failed",
                    )
                })?;
        self.write_bytes_response(session, ctx, status, "application/json", None, body)
            .await
    }

    async fn write_string_response(
        &self,
        session: &mut Session,
        ctx: &mut GatewayRequestContext,
        status: u16,
        body: String,
    ) -> pingora::Result<bool> {
        self.write_bytes_response(
            session,
            ctx,
            status,
            "text/plain; charset=utf-8",
            None,
            Bytes::from(body),
        )
        .await
    }

    async fn write_bytes_response(
        &self,
        session: &mut Session,
        ctx: &mut GatewayRequestContext,
        status: u16,
        content_type: &str,
        cache_control: Option<&str>,
        body: Bytes,
    ) -> pingora::Result<bool> {
        self.write_bytes_response_with_headers(
            session,
            ctx,
            status,
            Some(content_type),
            cache_control,
            body,
            &[],
        )
        .await
    }

    async fn write_bytes_response_with_headers(
        &self,
        session: &mut Session,
        ctx: &mut GatewayRequestContext,
        status: u16,
        content_type: Option<&str>,
        cache_control: Option<&str>,
        body: Bytes,
        extra_headers: &[(String, String)],
    ) -> pingora::Result<bool> {
        let is_head = session
            .req_header()
            .method
            .as_str()
            .eq_ignore_ascii_case("HEAD");
        let no_content = status == 204;
        if no_content && !body.is_empty() {
            return Err(Error::explain(
                ErrorType::InternalError,
                "204 response must not contain a body",
            ));
        }
        let mut response = ResponseHeader::build(status, Some(8 + extra_headers.len()))?;
        if let Some(content_type) = content_type {
            response.insert_header("content-type", content_type)?;
        }
        if let Some(cache_control) = cache_control {
            response.insert_header("cache-control", cache_control)?;
        }
        self.apply_response_headers(&mut response, ctx)?;
        for (name, value) in extra_headers {
            response.append_header(name.to_string(), value.to_string())?;
        }
        if status_allows_content_length(status) {
            response.set_content_length(body.len())?;
        }
        session
            .write_response_header(Box::new(response), is_head || no_content)
            .await?;
        if should_write_response_body(status, is_head) {
            session.write_response_body(Some(body), true).await?;
        }
        self.record_metrics(ctx, status);
        self.log_handler_durations(ctx);
        Ok(true)
    }

    async fn write_rejection_response(
        &self,
        session: &mut Session,
        ctx: &mut GatewayRequestContext,
        rejection: HandlerRejection,
    ) -> pingora::Result<bool> {
        let body = Bytes::from(format!("{}: {}", rejection.code, rejection.message));
        self.write_bytes_response_with_headers(
            session,
            ctx,
            rejection.status,
            Some("text/plain; charset=utf-8"),
            None,
            body,
            rejection.headers.as_slice(),
        )
        .await
    }

    async fn write_spa_session_rejection_response(
        &self,
        session: &mut Session,
        ctx: &mut GatewayRequestContext,
        mut rejection: HandlerRejection,
    ) -> pingora::Result<bool> {
        if !rejection
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("cache-control"))
        {
            rejection
                .headers
                .push(("cache-control".to_string(), "no-store".to_string()));
        }
        let body = Bytes::from(
            serde_json::to_vec(&json!({
                "code": rejection.code,
                "message": rejection.message,
            }))
            .unwrap_or_default(),
        );
        self.write_bytes_response_with_headers(
            session,
            ctx,
            rejection.status,
            Some("application/json"),
            None,
            body,
            rejection.headers.as_slice(),
        )
        .await
    }

    async fn write_spa_auth_response(
        &self,
        session: &mut Session,
        ctx: &mut GatewayRequestContext,
        response: SpaAuthResponse,
    ) -> pingora::Result<bool> {
        self.write_bytes_response_with_headers(
            session,
            ctx,
            response.status,
            response.content_type.as_deref(),
            None,
            Bytes::from(response.body),
            response.headers.as_slice(),
        )
        .await
    }

    async fn write_mcp_response(
        &self,
        session: &mut Session,
        ctx: &mut GatewayRequestContext,
        response: McpHttpResponse,
    ) -> pingora::Result<bool> {
        let McpHttpResponse {
            status,
            content_type,
            headers,
            body,
        } = response;
        match body {
            McpResponseBody::Stream(stream) => {
                self.write_streaming_mcp_response(
                    session,
                    ctx,
                    status,
                    content_type,
                    headers,
                    stream,
                )
                .await
            }
            McpResponseBody::Empty => {
                self.write_bytes_response_with_headers(
                    session,
                    ctx,
                    status,
                    Some(content_type.as_str()),
                    None,
                    Bytes::new(),
                    headers.as_slice(),
                )
                .await
            }
            McpResponseBody::Buffered(body) => {
                self.write_bytes_response_with_headers(
                    session,
                    ctx,
                    status,
                    Some(content_type.as_str()),
                    None,
                    body,
                    headers.as_slice(),
                )
                .await
            }
        }
    }

    async fn write_streaming_mcp_response(
        &self,
        session: &mut Session,
        ctx: &mut GatewayRequestContext,
        status: u16,
        content_type: String,
        headers: Vec<(String, String)>,
        mut stream: McpResponseStream,
    ) -> pingora::Result<bool> {
        let mut header = ResponseHeader::build(status, Some(8 + headers.len()))?;
        header.insert_header("content-type", content_type.as_str())?;
        self.apply_response_headers(&mut header, ctx)?;
        for (name, value) in &headers {
            header.append_header(name.to_string(), value.to_string())?;
        }
        session
            .write_response_header(Box::new(header), false)
            .await?;
        while let Some(frame) = stream.next_frame().await {
            session.write_response_body(Some(frame), false).await?;
        }
        session.write_response_body(None, true).await?;
        self.record_metrics(ctx, status);
        self.log_handler_durations(ctx);
        Ok(true)
    }

    async fn write_llm_streaming_response(
        &self,
        session: &mut Session,
        ctx: &mut GatewayRequestContext,
        response: StreamingHttpResponse,
    ) -> pingora::Result<bool> {
        let StreamingHttpResponse {
            status,
            headers,
            stream,
        } = response;
        let mut header = ResponseHeader::build(status, Some(8 + headers.len()))?;
        for (name, value) in &headers {
            header.append_header(name.to_string(), value.to_string())?;
        }
        self.apply_response_headers(&mut header, ctx)?;
        light_pingora::write_llm_sse_response(session, header, stream).await?;
        self.record_metrics(ctx, status);
        self.log_handler_durations(ctx);
        Ok(true)
    }

    fn apply_response_headers(
        &self,
        response: &mut ResponseHeader,
        ctx: &GatewayRequestContext,
    ) -> pingora::Result<()> {
        apply_correlation_response(response, &ctx.correlation)?;
        if let Some(cors) = ctx.cors.as_ref() {
            apply_cors_response(response, cors)?;
        }
        if let Some(header_config) = self.header_config.load().as_ref().as_ref() {
            apply_header_response(response, header_config, ctx.request_path.as_str())?;
        }
        if let Some(rate_limit_headers) = ctx.rate_limit_headers.as_ref() {
            apply_rate_limit_headers(response, rate_limit_headers)?;
        }
        for (name, value) in &ctx.extra_response_headers {
            response.append_header(name.to_string(), value.to_string())?;
        }
        Ok(())
    }

    fn record_metrics(&self, ctx: &mut GatewayRequestContext, status: u16) {
        ctx.response_status = Some(status);
        if ctx.metrics_recorded || !ctx.metrics_enabled {
            return;
        }
        let Some(config) = self.metrics_config.load().as_ref().as_ref().cloned() else {
            return;
        };

        let event = build_metrics_event(
            ctx.endpoint.as_str(),
            ctx.method.as_str(),
            status,
            ctx.request_start.elapsed(),
            ctx.correlation.correlation_id.clone(),
        );
        let counts = self.metrics_recorder.record(status);
        let mcp_schema = self
            .mcp_router
            .load()
            .as_ref()
            .as_ref()
            .map(McpRouterRuntime::schema_metrics)
            .unwrap_or_default();
        ctx.metrics_recorded = true;

        info!(
            target: "light_pingora::metrics",
            product = %config.product_name,
            endpoint = %event.endpoint,
            method = %event.method,
            status = event.status,
            statusClass = event.status_class,
            durationMs = event.duration_ms,
            correlationId = ?event.correlation_id,
            requestCount = counts.request,
            successCount = counts.success,
            authErrorCount = counts.auth_error,
            requestErrorCount = counts.request_error,
            serverErrorCount = counts.server_error,
            mcpSchemaPreparationAccepted = mcp_schema.preparation_accepted,
            mcpSchemaPreparationRejected = mcp_schema.preparation_rejected,
            mcpSchemaValidationsValid = mcp_schema.validations_valid,
            mcpSchemaValidationsInvalid = mcp_schema.validations_invalid,
            mcpSchemaValidationsOverloaded = mcp_schema.validations_overloaded,
            mcpSchemaValidationsWorkerFailed = mcp_schema.validations_worker_failed,
            mcpSchemaValidationDurationCount = mcp_schema.validation_duration_count,
            mcpSchemaValidationDurationTotalMicros = mcp_schema.validation_duration_total_micros,
            mcpSchemaValidationDurationMaxMicros = mcp_schema.validation_duration_max_micros,
            mcpSchemaOutputFallbackAttempted = mcp_schema.output_fallback_attempted,
            mcpSchemaOutputFallbackSucceeded = mcp_schema.output_fallback_succeeded,
            mcpSchemaOutputFallbackFailed = mcp_schema.output_fallback_failed,
            mcpSchemaWatchdogExceeded = mcp_schema.validation_watchdog_exceeded,
            mcpRouterReloadRejected = mcp_schema.router_reload_rejected,
            mcpRouterLastKnownGoodRetained = mcp_schema.router_last_known_good_retained,
            "request metrics"
        );
    }

    fn log_handler_durations(&self, ctx: &mut GatewayRequestContext) {
        let (report_handler_duration, handler_metrics_log_level) = ctx
            .security_execution
            .as_ref()
            .map(|snapshot| {
                (
                    snapshot.active_handlers.config().report_handler_duration,
                    snapshot.active_handlers.config().handler_metrics_log_level,
                )
            })
            .unwrap_or_else(|| {
                let active = self.active_handlers.load();
                (
                    active.config().report_handler_duration,
                    active.config().handler_metrics_log_level,
                )
            });
        if ctx.handler_timings_logged || ctx.handler_timings.is_empty() || !report_handler_duration
        {
            return;
        }

        let durations = ctx
            .handler_timings
            .iter()
            .map(|timing| format!("{}={}us", timing.handler_id, timing.duration.as_micros()))
            .collect::<Vec<_>>()
            .join(", ");

        match handler_metrics_log_level {
            HandlerMetricsLogLevel::Trace => {
                tracing::trace!(target: "light_pingora::handler", %durations, "handler durations")
            }
            HandlerMetricsLogLevel::Debug => {
                tracing::debug!(target: "light_pingora::handler", %durations, "handler durations")
            }
            HandlerMetricsLogLevel::Info => {
                tracing::info!(target: "light_pingora::handler", %durations, "handler durations")
            }
            HandlerMetricsLogLevel::Warn => {
                tracing::warn!(target: "light_pingora::handler", %durations, "handler durations")
            }
            HandlerMetricsLogLevel::Error => {
                tracing::error!(target: "light_pingora::handler", %durations, "handler durations")
            }
        }
        ctx.handler_timings_logged = true;
    }

    fn request_handler_active(&self, ctx: &GatewayRequestContext, handler_id: &str) -> bool {
        ctx.security_execution.as_ref().map_or_else(
            || self.active_handlers.load().is_handler_active(handler_id),
            |snapshot| snapshot.active_handlers.is_handler_active(handler_id),
        )
    }

    fn prepare_response_handlers(
        &self,
        ctx: &mut GatewayRequestContext,
        handler_ids: &[String],
        request_path: &str,
        method: &str,
    ) -> Result<(), HandlerRejection> {
        for handler_id in handler_ids {
            let started = Instant::now();
            if handler_id.as_str() == "detokenize" {
                let runtime = self.pii_tokenization.load();
                let Some(runtime) = runtime.as_ref().as_ref() else {
                    return Err(HandlerRejection::new(
                        502,
                        "ERR13021",
                        "pii tokenization is not configured",
                    ));
                };
                if runtime.has_response_rules(request_path, method) {
                    runtime.validate_auth(ctx.auth.as_ref())?;
                    ctx.detokenize_active = true;
                }
                ctx.record_handler_duration(handler_id, started.elapsed());
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn current_proxy_route(&self) -> Arc<Option<ProxyRoute>> {
        self.proxy_route.load()
    }

    #[cfg(test)]
    fn current_router_route(&self) -> Arc<Option<RouterRoute>> {
        self.router_route.load()
    }

    #[cfg(test)]
    fn current_static_resources(&self) -> Arc<StaticResourceSet> {
        self.static_resources.load()
    }

    #[cfg(test)]
    fn current_path_prefix_service_config(&self) -> Arc<Option<PathPrefixServiceConfig>> {
        self.path_prefix_service_config.load()
    }

    #[cfg(test)]
    fn current_token_runtime(&self) -> Arc<Option<TokenRuntime>> {
        self.token_runtime.load()
    }

    #[cfg(test)]
    fn current_stateless_auth(&self) -> Arc<Option<StatelessAuthRuntime>> {
        self.stateless_auth.load()
    }

    #[cfg(test)]
    fn current_msal_exchange(&self) -> Arc<Option<MsalExchangeRuntime>> {
        self.msal_exchange.load()
    }

    #[cfg(test)]
    fn current_msal_auth(&self) -> Arc<Option<MsalAuthRuntime>> {
        self.msal_auth.load()
    }

    #[cfg(test)]
    fn current_mcp_router(&self) -> Arc<Option<McpRouterRuntime>> {
        self.mcp_router.load()
    }

    #[cfg(test)]
    fn current_websocket_router(&self) -> Arc<Option<WebSocketRouterRuntime>> {
        self.websocket_router.load()
    }

    #[cfg(test)]
    fn current_hmac_runtime(&self) -> Arc<Option<HmacRuntime>> {
        self.hmac_runtime.load()
    }

    #[cfg(test)]
    fn current_security_execution(&self) -> Arc<GatewaySecurityExecutionSnapshot> {
        self.security_execution.load()
    }

    #[cfg(test)]
    fn active_handler_ids(&self) -> Vec<String> {
        self.active_handlers.load().active_handler_ids().to_vec()
    }
}

struct HandlerReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    correlation_config: Arc<ConfigManager<Option<CorrelationConfig>>>,
    cors_config: Arc<ConfigManager<Option<CorsConfig>>>,
    metrics_config: Arc<ConfigManager<Option<MetricsConfig>>>,
    header_config: Arc<ConfigManager<Option<HeaderConfig>>>,
    api_key_config: Arc<ConfigManager<Option<ApiKeyConfig>>>,
    basic_auth_config: Arc<ConfigManager<Option<BasicAuthConfig>>>,
    security_runtime: Arc<ConfigManager<Option<SecurityRuntime>>>,
    unified_security_config: Arc<ConfigManager<Option<UnifiedSecurityConfig>>>,
    hmac_runtime: Arc<ConfigManager<Option<HmacRuntime>>>,
    security_execution: Arc<ConfigManager<GatewaySecurityExecutionSnapshot>>,
    hmac_replay_admin: Arc<HmacReplayAdmin>,
    rate_limit_runtime: Arc<ConfigManager<Option<RateLimitRuntime>>>,
    path_prefix_service_config: Arc<ConfigManager<Option<PathPrefixServiceConfig>>>,
    token_runtime: Arc<ConfigManager<Option<TokenRuntime>>>,
    stateless_auth: Arc<ConfigManager<Option<StatelessAuthRuntime>>>,
    msal_exchange: Arc<ConfigManager<Option<MsalExchangeRuntime>>>,
    msal_auth: Arc<ConfigManager<Option<MsalAuthRuntime>>>,
    pii_tokenization: Arc<ConfigManager<Option<PiiTokenizationRuntime>>>,
    access_control: Arc<ConfigManager<Option<AccessControlRuntime>>>,
    mcp_router: Arc<ConfigManager<Option<McpRouterRuntime>>>,
    websocket_router: Arc<ConfigManager<Option<WebSocketRouterRuntime>>>,
    llm_gateway: Arc<ArcSwapOption<LlmGatewayModule>>,
    router_route: Arc<ConfigManager<Option<RouterRoute>>>,
}

#[async_trait]
impl ReloadableModule for HandlerReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active_handlers =
            load_active_handlers(&ctx.runtime_config, &gateway_handler_registry())?;
        let correlation_config = load_correlation_config(
            &ctx.runtime_config,
            active_handlers.is_handler_active("correlation"),
        )?;
        let cors_config = load_cors_config(
            &ctx.runtime_config,
            active_handlers.is_handler_active("cors"),
        )?;
        let metrics_config = load_metrics_config(
            &ctx.runtime_config,
            active_handlers.is_handler_active("metrics"),
        )?;
        let header_config = load_header_config(
            &ctx.runtime_config,
            handler_active(&active_handlers, &["header", "headers"]),
        )?;
        let api_key_config = load_api_key_config(
            &ctx.runtime_config,
            handler_active(
                &active_handlers,
                &["api-key", "apikey", "unified-security", "unified"],
            ),
        )?;
        let basic_auth_config = load_basic_auth_config(
            &ctx.runtime_config,
            handler_active(
                &active_handlers,
                &["basic-auth", "basic", "unified-security", "unified"],
            ),
        )?;
        let security_runtime = load_security_runtime(
            &ctx.runtime_config,
            handler_active(
                &active_handlers,
                &["security", "jwt", "unified-security", "unified"],
            ),
        )?;
        let unified_security_config = load_unified_security_config(
            &ctx.runtime_config,
            handler_active(&active_handlers, &["unified-security", "unified"]),
        )?;
        let previous_hmac = self.hmac_runtime.load();
        let hmac_runtime = load_hmac_runtime_preserving(
            &ctx.runtime_config,
            active_handlers.is_handler_active("hmac")
                || unified_security_config
                    .as_ref()
                    .is_some_and(UnifiedSecurityConfig::requires_hmac),
            previous_hmac.as_ref().as_ref(),
        )?;
        if let Some(unified) = unified_security_config.as_ref() {
            validate_unified_security_config(unified, hmac_runtime.as_ref())?;
        }
        let rate_limit_runtime = load_rate_limit_runtime(
            &ctx.runtime_config,
            handler_active(&active_handlers, &["limit", "rate-limit"]),
        )?;
        let path_prefix_service_config = load_path_prefix_service_config(
            &ctx.runtime_config,
            handler_active(
                &active_handlers,
                &["prefix", "path-prefix-service", "pathPrefixService"],
            ),
        )?;
        let token_runtime = load_token_runtime(
            &ctx.runtime_config,
            active_handlers.is_handler_active("token"),
        )?;
        let stateless_auth = load_stateless_auth_runtime(
            &ctx.runtime_config,
            handler_active(
                &active_handlers,
                &["stateless", "google", "facebook", "github"],
            ),
        )?;
        let msal_exchange = load_msal_exchange_runtime(
            &ctx.runtime_config,
            active_handlers.is_handler_active("msal-exchange"),
        )?;
        let msal_auth = load_msal_auth_runtime(
            &ctx.runtime_config,
            active_handlers.is_handler_active("msal-auth"),
        )?;
        let pii_tokenization = load_pii_tokenization_runtime(
            &ctx.runtime_config,
            handler_active(&active_handlers, &["tokenize", "detokenize"]),
        )?;
        let access_control = load_access_control_runtime(
            &ctx.runtime_config,
            active_handlers.is_handler_active("access-control"),
        )?;
        log_access_control_revision(access_control.as_ref());
        let mcp_router = load_mcp_router_runtime_preserving_state(
            &ctx.runtime_config,
            active_handlers.is_handler_active("mcp"),
            &self.mcp_router,
        )?;
        let websocket_router = load_websocket_router_runtime_preserving_state(
            &ctx.runtime_config,
            active_handlers.is_handler_active("websocket"),
            access_control.as_ref(),
            &self.websocket_router,
        )?;
        let previous_llm = self.llm_gateway.load_full();
        let llm_generation = previous_llm
            .as_ref()
            .map_or(1, |module| module.runtime.snapshot().generation + 1);
        let llm_gateway = load_llm_gateway_module(
            &ctx.runtime_config,
            active_handlers.is_handler_active("llm"),
            llm_generation,
            previous_llm.as_ref(),
        )?;
        let router_route = load_router_route(
            &ctx.runtime_config,
            active_handlers.is_handler_active("router"),
        )?;
        validate_hmac_effective_chains(
            &active_handlers,
            hmac_runtime.as_ref(),
            unified_security_config.as_ref(),
        )?;
        let generation = self.security_execution.load().generation.saturating_add(1);
        let security_execution = GatewaySecurityExecutionSnapshot::new(
            generation,
            active_handlers.clone(),
            api_key_config.clone(),
            basic_auth_config.clone(),
            security_runtime.clone(),
            unified_security_config.clone(),
            hmac_runtime.clone(),
        );
        self.active_handlers.store(active_handlers);
        self.correlation_config.store(correlation_config);
        self.cors_config.store(cors_config);
        self.metrics_config.store(metrics_config);
        self.header_config.store(header_config);
        self.api_key_config.store(api_key_config);
        self.basic_auth_config.store(basic_auth_config);
        self.security_runtime.store(security_runtime);
        self.unified_security_config.store(unified_security_config);
        if let Some(cache_registry) = ctx.runtime_config.cache_registry.as_ref() {
            if let Some(previous) = previous_hmac.as_ref().as_ref() {
                previous.unregister_local_replay_caches(cache_registry);
            }
            if let Some(runtime) = hmac_runtime.as_ref() {
                runtime.register_local_replay_caches(cache_registry);
            }
        }
        self.hmac_replay_admin.replace(hmac_runtime.clone());
        self.hmac_runtime.store(hmac_runtime);
        self.rate_limit_runtime.store(rate_limit_runtime);
        self.path_prefix_service_config
            .store(path_prefix_service_config);
        self.token_runtime.store(token_runtime);
        self.stateless_auth.store(stateless_auth);
        self.msal_exchange.store(msal_exchange);
        self.msal_auth.store(msal_auth);
        self.pii_tokenization.store(pii_tokenization);
        self.access_control.store(access_control);
        store_mcp_reload(&self.mcp_router, mcp_router);
        self.websocket_router.store(websocket_router);
        self.llm_gateway.store(llm_gateway);
        self.router_route.store(router_route);
        self.security_execution.store(security_execution);
        Ok(ReloadOutcome::success("handler.yml reloaded"))
    }
}

struct LlmRouterReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    llm_gateway: Arc<ArcSwapOption<LlmGatewayModule>>,
}

#[async_trait]
impl ReloadableModule for LlmRouterReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let previous = self.llm_gateway.load_full();
        let generation = previous
            .as_ref()
            .map_or(1, |module| module.runtime.snapshot().generation + 1);
        let candidate = load_llm_gateway_module(
            &ctx.runtime_config,
            self.active_handlers.load().is_handler_active("llm"),
            generation,
            previous.as_ref(),
        )?;
        self.llm_gateway.store(candidate);
        Ok(ReloadOutcome::success("llm-router.yml reloaded"))
    }
}

struct CorrelationReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    correlation_config: Arc<ConfigManager<Option<CorrelationConfig>>>,
}

#[async_trait]
impl ReloadableModule for CorrelationReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active = self.active_handlers.load().is_handler_active("correlation");
        let config = load_correlation_config(&ctx.runtime_config, active)?;
        self.correlation_config.store(config);
        Ok(ReloadOutcome::success("correlation.yml reloaded"))
    }
}

struct CorsReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    cors_config: Arc<ConfigManager<Option<CorsConfig>>>,
}

#[async_trait]
impl ReloadableModule for CorsReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active = self.active_handlers.load().is_handler_active("cors");
        let config = load_cors_config(&ctx.runtime_config, active)?;
        self.cors_config.store(config);
        Ok(ReloadOutcome::success("cors.yml reloaded"))
    }
}

struct MetricsReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    metrics_config: Arc<ConfigManager<Option<MetricsConfig>>>,
}

#[async_trait]
impl ReloadableModule for MetricsReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active = self.active_handlers.load().is_handler_active("metrics");
        let config = load_metrics_config(&ctx.runtime_config, active)?;
        self.metrics_config.store(config);
        Ok(ReloadOutcome::success("metrics.yml reloaded"))
    }
}

struct HeaderReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    header_config: Arc<ConfigManager<Option<HeaderConfig>>>,
}

#[async_trait]
impl ReloadableModule for HeaderReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active_handlers = self.active_handlers.load();
        let active = handler_active(&active_handlers, &["header", "headers"]);
        let config = load_header_config(&ctx.runtime_config, active)?;
        self.header_config.store(config);
        Ok(ReloadOutcome::success("header.yml reloaded"))
    }
}

struct ApiKeyReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    api_key_config: Arc<ConfigManager<Option<ApiKeyConfig>>>,
    security_execution: Arc<ConfigManager<GatewaySecurityExecutionSnapshot>>,
}

#[async_trait]
impl ReloadableModule for ApiKeyReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active_handlers = self.active_handlers.load();
        let active = handler_active(
            &active_handlers,
            &["api-key", "apikey", "unified-security", "unified"],
        );
        let config = load_api_key_config(&ctx.runtime_config, active)?;
        self.api_key_config.store(config.clone());
        let previous = self.security_execution.load();
        self.security_execution
            .store(GatewaySecurityExecutionSnapshot {
                generation: previous.generation.saturating_add(1),
                active_handlers: Arc::clone(&previous.active_handlers),
                api_key: Arc::new(config),
                basic_auth: Arc::clone(&previous.basic_auth),
                security: Arc::clone(&previous.security),
                unified_security: Arc::clone(&previous.unified_security),
                hmac: Arc::clone(&previous.hmac),
            });
        Ok(ReloadOutcome::success("apikey.yml reloaded"))
    }
}

struct BasicAuthReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    basic_auth_config: Arc<ConfigManager<Option<BasicAuthConfig>>>,
    security_execution: Arc<ConfigManager<GatewaySecurityExecutionSnapshot>>,
}

#[async_trait]
impl ReloadableModule for BasicAuthReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active_handlers = self.active_handlers.load();
        let active = handler_active(
            &active_handlers,
            &["basic-auth", "basic", "unified-security", "unified"],
        );
        let config = load_basic_auth_config(&ctx.runtime_config, active)?;
        self.basic_auth_config.store(config.clone());
        let previous = self.security_execution.load();
        self.security_execution
            .store(GatewaySecurityExecutionSnapshot {
                generation: previous.generation.saturating_add(1),
                active_handlers: Arc::clone(&previous.active_handlers),
                api_key: Arc::clone(&previous.api_key),
                basic_auth: Arc::new(config),
                security: Arc::clone(&previous.security),
                unified_security: Arc::clone(&previous.unified_security),
                hmac: Arc::clone(&previous.hmac),
            });
        Ok(ReloadOutcome::success("basic-auth.yml reloaded"))
    }
}

struct SecurityReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    security_runtime: Arc<ConfigManager<Option<SecurityRuntime>>>,
    stateless_auth: Arc<ConfigManager<Option<StatelessAuthRuntime>>>,
    msal_exchange: Arc<ConfigManager<Option<MsalExchangeRuntime>>>,
    msal_auth: Arc<ConfigManager<Option<MsalAuthRuntime>>>,
    security_execution: Arc<ConfigManager<GatewaySecurityExecutionSnapshot>>,
}

#[async_trait]
impl ReloadableModule for SecurityReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active_handlers = self.active_handlers.load();
        let active = handler_active(
            &active_handlers,
            &["security", "jwt", "unified-security", "unified"],
        );
        let config = load_security_runtime(&ctx.runtime_config, active)?;
        if let Some(ref runtime) = config {
            if let Err(error) = runtime.bootstrap().await {
                tracing::warn!(
                    "Failed to bootstrap JWT keys on security config reload: {} (status: {}, code: {})",
                    error.message,
                    error.status,
                    error.code
                );
            }
        }
        let stateless_auth = load_stateless_auth_runtime(
            &ctx.runtime_config,
            handler_active(
                &active_handlers,
                &["stateless", "google", "facebook", "github"],
            ),
        )?;
        let msal_exchange = load_msal_exchange_runtime(
            &ctx.runtime_config,
            active_handlers.is_handler_active("msal-exchange"),
        )?;
        let msal_auth = load_msal_auth_runtime(
            &ctx.runtime_config,
            active_handlers.is_handler_active("msal-auth"),
        )?;
        if let Some(ref msal) = msal_exchange {
            if let Err(error) = msal.bootstrap().await {
                tracing::warn!(
                    "Failed to bootstrap MSAL keys on security config reload: {} (status: {}, code: {})",
                    error.message,
                    error.status,
                    error.code
                );
            }
        }
        if let Some(ref msal) = msal_auth {
            if let Err(error) = msal.bootstrap().await {
                tracing::warn!(
                    "Failed to bootstrap MSAL keys on security config reload for msal-auth: {} (status: {}, code: {})",
                    error.message,
                    error.status,
                    error.code
                );
            }
        }
        self.security_runtime.store(config.clone());
        self.stateless_auth.store(stateless_auth);
        self.msal_exchange.store(msal_exchange);
        self.msal_auth.store(msal_auth);
        let previous = self.security_execution.load();
        self.security_execution
            .store(GatewaySecurityExecutionSnapshot {
                generation: previous.generation.saturating_add(1),
                active_handlers: Arc::clone(&previous.active_handlers),
                api_key: Arc::clone(&previous.api_key),
                basic_auth: Arc::clone(&previous.basic_auth),
                security: Arc::new(config),
                unified_security: Arc::clone(&previous.unified_security),
                hmac: Arc::clone(&previous.hmac),
            });
        Ok(ReloadOutcome::success("security.yml reloaded"))
    }
}

struct UnifiedSecurityReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    unified_security_config: Arc<ConfigManager<Option<UnifiedSecurityConfig>>>,
    hmac_runtime: Arc<ConfigManager<Option<HmacRuntime>>>,
    security_execution: Arc<ConfigManager<GatewaySecurityExecutionSnapshot>>,
}

#[async_trait]
impl ReloadableModule for UnifiedSecurityReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active_handlers = self.active_handlers.load();
        let active = handler_active(&active_handlers, &["unified-security", "unified"]);
        let config = load_unified_security_config(&ctx.runtime_config, active)?;
        if let Some(config) = config.as_ref() {
            let hmac = self.hmac_runtime.load();
            validate_unified_security_config(config, hmac.as_ref().as_ref())?;
        }
        let previous = self.security_execution.load();
        validate_hmac_effective_chains(
            previous.active_handlers.as_ref(),
            previous.hmac.as_ref().as_ref(),
            config.as_ref(),
        )?;
        self.unified_security_config.store(config.clone());
        self.security_execution
            .store(GatewaySecurityExecutionSnapshot {
                generation: previous.generation.saturating_add(1),
                active_handlers: Arc::clone(&previous.active_handlers),
                api_key: Arc::clone(&previous.api_key),
                basic_auth: Arc::clone(&previous.basic_auth),
                security: Arc::clone(&previous.security),
                unified_security: Arc::new(config),
                hmac: Arc::clone(&previous.hmac),
            });
        Ok(ReloadOutcome::success("unified-security.yml reloaded"))
    }
}

struct HmacReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    unified_security_config: Arc<ConfigManager<Option<UnifiedSecurityConfig>>>,
    hmac_runtime: Arc<ConfigManager<Option<HmacRuntime>>>,
    hmac_replay_admin: Arc<HmacReplayAdmin>,
    security_execution: Arc<ConfigManager<GatewaySecurityExecutionSnapshot>>,
}

#[async_trait]
impl ReloadableModule for HmacReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active_handlers = self.active_handlers.load();
        let unified = self.unified_security_config.load();
        let required = active_handlers.is_handler_active("hmac")
            || unified
                .as_ref()
                .as_ref()
                .is_some_and(UnifiedSecurityConfig::requires_hmac);
        let previous = self.hmac_runtime.load();
        let runtime = load_hmac_runtime_preserving(
            &ctx.runtime_config,
            required,
            previous.as_ref().as_ref(),
        )?;
        if let Some(unified) = unified.as_ref().as_ref() {
            validate_unified_security_config(unified, runtime.as_ref())?;
        }
        validate_hmac_effective_chains(
            active_handlers.as_ref(),
            runtime.as_ref(),
            unified.as_ref().as_ref(),
        )?;
        if let Some(cache_registry) = ctx.runtime_config.cache_registry.as_ref() {
            if let Some(previous) = previous.as_ref().as_ref() {
                previous.unregister_local_replay_caches(cache_registry);
            }
            if let Some(runtime) = runtime.as_ref() {
                runtime.register_local_replay_caches(cache_registry);
            }
        }
        self.hmac_replay_admin.replace(runtime.clone());
        self.hmac_runtime.store(runtime.clone());
        let previous_execution = self.security_execution.load();
        self.security_execution
            .store(GatewaySecurityExecutionSnapshot {
                generation: previous_execution.generation.saturating_add(1),
                active_handlers: Arc::clone(&previous_execution.active_handlers),
                api_key: Arc::clone(&previous_execution.api_key),
                basic_auth: Arc::clone(&previous_execution.basic_auth),
                security: Arc::clone(&previous_execution.security),
                unified_security: Arc::clone(&previous_execution.unified_security),
                hmac: Arc::new(runtime),
            });
        Ok(ReloadOutcome::success("hmac.yml reloaded"))
    }
}

struct RateLimitReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    rate_limit_runtime: Arc<ConfigManager<Option<RateLimitRuntime>>>,
}

#[async_trait]
impl ReloadableModule for RateLimitReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active_handlers = self.active_handlers.load();
        let active = handler_active(&active_handlers, &["limit", "rate-limit"]);
        let config = load_rate_limit_runtime(&ctx.runtime_config, active)?;
        self.rate_limit_runtime.store(config);
        Ok(ReloadOutcome::success("limit.yml reloaded"))
    }
}

struct PathPrefixServiceReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    path_prefix_service_config: Arc<ConfigManager<Option<PathPrefixServiceConfig>>>,
}

#[async_trait]
impl ReloadableModule for PathPrefixServiceReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active_handlers = self.active_handlers.load();
        let active = handler_active(
            &active_handlers,
            &["prefix", "path-prefix-service", "pathPrefixService"],
        );
        let config = load_path_prefix_service_config(&ctx.runtime_config, active)?;
        self.path_prefix_service_config.store(config);
        Ok(ReloadOutcome::success("pathPrefixService.yml reloaded"))
    }
}

struct TokenReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    token_runtime: Arc<ConfigManager<Option<TokenRuntime>>>,
    stateless_auth: Arc<ConfigManager<Option<StatelessAuthRuntime>>>,
    msal_exchange: Arc<ConfigManager<Option<MsalExchangeRuntime>>>,
}

struct StatelessAuthReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    stateless_auth: Arc<ConfigManager<Option<StatelessAuthRuntime>>>,
}

#[async_trait]
impl ReloadableModule for StatelessAuthReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active_handlers = self.active_handlers.load();
        let active = handler_active(
            &active_handlers,
            &["stateless", "google", "facebook", "github"],
        );
        let runtime = load_stateless_auth_runtime(&ctx.runtime_config, active)?;
        self.stateless_auth.store(runtime);
        Ok(ReloadOutcome::success("statelessAuth.yml reloaded"))
    }
}

struct MsalExchangeReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    msal_exchange: Arc<ConfigManager<Option<MsalExchangeRuntime>>>,
}

#[async_trait]
impl ReloadableModule for MsalExchangeReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active = self
            .active_handlers
            .load()
            .is_handler_active("msal-exchange");
        let runtime = load_msal_exchange_runtime(&ctx.runtime_config, active)?;
        if let Some(ref msal) = runtime {
            if let Err(error) = msal.bootstrap().await {
                tracing::warn!(
                    "Failed to bootstrap MSAL keys on msal-exchange config reload: {} (status: {}, code: {})",
                    error.message,
                    error.status,
                    error.code
                );
            }
        }
        self.msal_exchange.store(runtime);
        Ok(ReloadOutcome::success("msal-exchange.yml reloaded"))
    }
}

struct MsalAuthReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    msal_auth: Arc<ConfigManager<Option<MsalAuthRuntime>>>,
}

#[async_trait]
impl ReloadableModule for MsalAuthReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active = self.active_handlers.load().is_handler_active("msal-auth");
        let runtime = load_msal_auth_runtime(&ctx.runtime_config, active)?;
        if let Some(ref msal) = runtime {
            if let Err(error) = msal.bootstrap().await {
                tracing::warn!(
                    "Failed to bootstrap MSAL keys on msal-auth config reload: {} (status: {}, code: {})",
                    error.message,
                    error.status,
                    error.code
                );
            }
        }
        self.msal_auth.store(runtime);
        Ok(ReloadOutcome::success("msal-auth.yml reloaded"))
    }
}

struct MsalSecurityReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    msal_exchange: Arc<ConfigManager<Option<MsalExchangeRuntime>>>,
    msal_auth: Arc<ConfigManager<Option<MsalAuthRuntime>>>,
}

#[async_trait]
impl ReloadableModule for MsalSecurityReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active_handlers = self.active_handlers.load();
        let msal_exchange = load_msal_exchange_runtime(
            &ctx.runtime_config,
            active_handlers.is_handler_active("msal-exchange"),
        )?;
        let msal_auth = load_msal_auth_runtime(
            &ctx.runtime_config,
            active_handlers.is_handler_active("msal-auth"),
        )?;
        if let Some(ref msal) = msal_exchange {
            if let Err(error) = msal.bootstrap().await {
                tracing::warn!(
                    "Failed to bootstrap MSAL keys on security-msal config reload for msal-exchange: {} (status: {}, code: {})",
                    error.message,
                    error.status,
                    error.code
                );
            }
        }
        if let Some(ref msal) = msal_auth {
            if let Err(error) = msal.bootstrap().await {
                tracing::warn!(
                    "Failed to bootstrap MSAL keys on security-msal config reload for msal-auth: {} (status: {}, code: {})",
                    error.message,
                    error.status,
                    error.code
                );
            }
        }
        self.msal_exchange.store(msal_exchange);
        self.msal_auth.store(msal_auth);
        Ok(ReloadOutcome::success("security-msal.yml reloaded"))
    }
}

struct PiiTokenizationReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    pii_tokenization: Arc<ConfigManager<Option<PiiTokenizationRuntime>>>,
}

#[async_trait]
impl ReloadableModule for PiiTokenizationReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active_handlers = self.active_handlers.load();
        let active = handler_active(&active_handlers, &["tokenize", "detokenize"]);
        let runtime = load_pii_tokenization_runtime(&ctx.runtime_config, active)?;
        self.pii_tokenization.store(runtime);
        Ok(ReloadOutcome::success("pii-tokenization.yml reloaded"))
    }
}

struct McpRouterReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    mcp_router: Arc<ConfigManager<Option<McpRouterRuntime>>>,
}

#[async_trait]
impl ReloadableModule for McpRouterReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active = self.active_handlers.load().is_handler_active("mcp");
        let runtime = load_mcp_router_runtime_preserving_state(
            &ctx.runtime_config,
            active,
            &self.mcp_router,
        )?;
        store_mcp_reload(&self.mcp_router, runtime);
        Ok(ReloadOutcome::success("mcp-router.yml reloaded"))
    }
}

struct WebSocketRouterReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    access_control: Arc<ConfigManager<Option<AccessControlRuntime>>>,
    websocket_router: Arc<ConfigManager<Option<WebSocketRouterRuntime>>>,
}

#[async_trait]
impl ReloadableModule for WebSocketRouterReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active = self.active_handlers.load().is_handler_active("websocket");
        let runtime = load_websocket_router_runtime_preserving_state(
            &ctx.runtime_config,
            active,
            self.access_control.load().as_ref().as_ref(),
            &self.websocket_router,
        )?;
        self.websocket_router.store(runtime);
        Ok(ReloadOutcome::success("websocket-router.yml reloaded"))
    }
}

struct AccessControlReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    access_control: Arc<ConfigManager<Option<AccessControlRuntime>>>,
    mcp_router: Arc<ConfigManager<Option<McpRouterRuntime>>>,
    websocket_router: Arc<ConfigManager<Option<WebSocketRouterRuntime>>>,
}

#[async_trait]
impl ReloadableModule for AccessControlReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active_handlers = self.active_handlers.load();
        let access_control = load_access_control_runtime(
            &ctx.runtime_config,
            active_handlers.is_handler_active("access-control"),
        )?;
        log_access_control_revision(access_control.as_ref());
        let mcp_router = load_mcp_router_runtime_preserving_state(
            &ctx.runtime_config,
            active_handlers.is_handler_active("mcp"),
            &self.mcp_router,
        )?;
        let websocket_router = load_websocket_router_runtime_preserving_state(
            &ctx.runtime_config,
            active_handlers.is_handler_active("websocket"),
            access_control.as_ref(),
            &self.websocket_router,
        )?;
        self.access_control.store(access_control);
        store_mcp_reload(&self.mcp_router, mcp_router);
        self.websocket_router.store(websocket_router);
        Ok(ReloadOutcome::success("access-control/rule.yml reloaded"))
    }
}

#[async_trait]
impl ReloadableModule for TokenReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active_handlers = self.active_handlers.load();
        let active = active_handlers.is_handler_active("token");
        let runtime = load_token_runtime(&ctx.runtime_config, active)?;
        let stateless_auth = load_stateless_auth_runtime(
            &ctx.runtime_config,
            handler_active(
                &active_handlers,
                &["stateless", "google", "facebook", "github"],
            ),
        )?;
        let msal_exchange = load_msal_exchange_runtime(
            &ctx.runtime_config,
            active_handlers.is_handler_active("msal-exchange"),
        )?;
        if let Some(ref msal) = msal_exchange {
            if let Err(error) = msal.bootstrap().await {
                tracing::warn!(
                    "Failed to bootstrap MSAL keys on token config reload: {} (status: {}, code: {})",
                    error.message,
                    error.status,
                    error.code
                );
            }
        }
        self.token_runtime.store(runtime);
        self.stateless_auth.store(stateless_auth);
        self.msal_exchange.store(msal_exchange);
        Ok(ReloadOutcome::success("token/client/sidecar.yml reloaded"))
    }
}

struct ProxyReloader {
    proxy_route: Arc<ConfigManager<Option<ProxyRoute>>>,
}

#[async_trait]
impl ReloadableModule for ProxyReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let proxy_route = load_proxy_route(&ctx.runtime_config)?;
        self.proxy_route.store(proxy_route);
        Ok(ReloadOutcome::success("proxy.yml reloaded"))
    }
}

struct RouterReloader {
    active_handlers: Arc<ConfigManager<ActiveHandlerSet>>,
    router_route: Arc<ConfigManager<Option<RouterRoute>>>,
}

#[async_trait]
impl ReloadableModule for RouterReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let active = self.active_handlers.load().is_handler_active("router");
        let router_route = load_router_route(&ctx.runtime_config, active)?;
        self.router_route.store(router_route);
        Ok(ReloadOutcome::success("router.yml reloaded"))
    }
}

struct StaticResourceReloader {
    static_resources: Arc<ConfigManager<StaticResourceSet>>,
}

#[async_trait]
impl ReloadableModule for StaticResourceReloader {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError> {
        let static_resources = load_static_resources(&ctx.runtime_config)?;
        self.static_resources.store(static_resources);
        Ok(ReloadOutcome::success(
            "static resource configuration reloaded",
        ))
    }
}

enum HmacCaptureFailure {
    TooLarge,
    BufferUnavailable,
}

fn content_length(headers: &HMap) -> Option<usize> {
    let mut values = headers.get_all("content-length").iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    value.to_str().ok()?.trim().parse().ok()
}

fn identity_content_encoding(headers: &HMap) -> bool {
    headers.get_all("content-encoding").iter().all(|value| {
        value
            .to_str()
            .ok()
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("identity"))
    })
}

impl GatewayProxy {
    async fn capture_hmac_body(
        &self,
        session: &mut Session,
        max_body_bytes: usize,
        max_buffered_body_bytes: usize,
    ) -> pingora::Result<Result<(Bytes, HmacBodyPermit), HmacCaptureFailure>> {
        let advertised = content_length(&session.req_header().headers);
        if advertised.is_some_and(|length| length > max_body_bytes) {
            return Ok(Err(HmacCaptureFailure::TooLarge));
        }
        let initial = advertised.unwrap_or(0);
        let mut permit = match HmacBodyPermit::acquire(
            Arc::clone(&self.hmac_body_bytes),
            initial,
            max_buffered_body_bytes,
        ) {
            Ok(permit) => permit,
            Err(()) => return Ok(Err(HmacCaptureFailure::BufferUnavailable)),
        };
        let mut output = BytesMut::with_capacity(initial);
        loop {
            let Some(chunk) = session.read_request_body().await? else {
                return Ok(Ok((output.freeze(), permit)));
            };
            let Some(next_len) = output.len().checked_add(chunk.len()) else {
                return Ok(Err(HmacCaptureFailure::TooLarge));
            };
            if next_len > max_body_bytes {
                return Ok(Err(HmacCaptureFailure::TooLarge));
            }
            if next_len > permit.bytes
                && permit
                    .grow(next_len - permit.bytes, max_buffered_body_bytes)
                    .is_err()
            {
                return Ok(Err(HmacCaptureFailure::BufferUnavailable));
            }
            output.extend_from_slice(&chunk);
        }
    }

    async fn enter_hmac_gate(
        &self,
        session: &mut Session,
        ctx: &mut GatewayRequestContext,
        entry: &'static str,
        profile: &str,
    ) -> pingora::Result<Option<bool>> {
        if ctx.hmac_entry.is_some() {
            self.hmac_metrics.request(profile, "chain_error");
            return self
                .write_rejection_response(
                    session,
                    ctx,
                    HandlerRejection::new(503, "ERR10001", "HMAC authentication entered twice"),
                )
                .await
                .map(Some);
        }
        ctx.hmac_entry = Some(entry);
        let snapshot = ctx.security_execution.as_ref().ok_or_else(|| {
            pingora_internal_error(RuntimeError::Config(
                "HMAC request has no security execution snapshot".to_string(),
            ))
        })?;
        let Some(runtime) = snapshot.hmac.as_ref() else {
            self.hmac_metrics.request(profile, "chain_error");
            return self
                .write_rejection_response(
                    session,
                    ctx,
                    HandlerRejection::new(503, "ERR10001", "HMAC runtime is unavailable"),
                )
                .await
                .map(Some);
        };
        let Some((max_body_bytes, timeout_millis)) = runtime.profile_limits(profile) else {
            self.hmac_metrics.request(profile, "chain_error");
            return self
                .write_rejection_response(
                    session,
                    ctx,
                    HandlerRejection::new(503, "ERR10001", "HMAC profile is unavailable"),
                )
                .await
                .map(Some);
        };
        if !identity_content_encoding(&session.req_header().headers) {
            self.hmac_metrics.request(profile, "unsupported_encoding");
            return self
                .write_rejection_response(
                    session,
                    ctx,
                    HandlerRejection::new(
                        415,
                        "ERR10001",
                        "encoded webhook bodies are not supported",
                    )
                    .with_header("connection", "close"),
                )
                .await
                .map(Some);
        }
        let headers = session.req_header().headers.clone();
        let capture = timeout(
            Duration::from_millis(timeout_millis),
            self.capture_hmac_body(session, max_body_bytes, runtime.max_buffered_body_bytes()),
        )
        .await;
        let (body, permit) = match capture {
            Err(_) => {
                self.hmac_metrics.request(profile, "timeout");
                return self
                    .write_rejection_response(
                        session,
                        ctx,
                        HandlerRejection::new(408, "ERR10001", "webhook body read timed out")
                            .with_header("connection", "close"),
                    )
                    .await
                    .map(Some);
            }
            Ok(Err(error)) => {
                self.hmac_metrics.request(profile, "runtime_error");
                warn!(profile, error = %error, "HMAC body capture failed");
                return Err(error);
            }
            Ok(Ok(Err(HmacCaptureFailure::TooLarge))) => {
                self.hmac_metrics.request(profile, "too_large");
                return self
                    .write_rejection_response(
                        session,
                        ctx,
                        HandlerRejection::new(413, "ERR10001", "webhook body is too large")
                            .with_header("connection", "close"),
                    )
                    .await
                    .map(Some);
            }
            Ok(Ok(Err(HmacCaptureFailure::BufferUnavailable))) => {
                self.hmac_metrics.request(profile, "buffer_unavailable");
                return self
                    .write_rejection_response(
                        session,
                        ctx,
                        HandlerRejection::new(
                            503,
                            "ERR10001",
                            "webhook body buffer is unavailable",
                        )
                        .with_header("connection", "close"),
                    )
                    .await
                    .map(Some);
            }
            Ok(Ok(Ok(value))) => value,
        };
        let started = Instant::now();
        let evidence = match runtime.verify(profile, &headers, body.as_ref()) {
            Ok(evidence) => evidence,
            Err(HmacVerificationError::Invalid) => {
                self.hmac_metrics.verification(profile, started.elapsed());
                self.hmac_metrics.request(profile, "invalid");
                return self
                    .write_rejection_response(
                        session,
                        ctx,
                        HandlerRejection::new(401, "ERR10001", "invalid webhook authentication"),
                    )
                    .await
                    .map(Some);
            }
            Err(HmacVerificationError::BodyTooLarge) => {
                self.hmac_metrics.verification(profile, started.elapsed());
                self.hmac_metrics.request(profile, "too_large");
                return self
                    .write_rejection_response(
                        session,
                        ctx,
                        HandlerRejection::new(413, "ERR10001", "webhook body is too large"),
                    )
                    .await
                    .map(Some);
            }
        };
        self.hmac_metrics.verification(profile, started.elapsed());
        let replay = match runtime.replay_attempt(&evidence, &headers) {
            Ok(replay) => replay,
            Err(_) => {
                self.hmac_metrics.request(profile, "invalid");
                return self
                    .write_rejection_response(
                        session,
                        ctx,
                        HandlerRejection::new(401, "ERR10001", "invalid webhook authentication"),
                    )
                    .await
                    .map(Some);
            }
        };
        if let Some(HmacReplayAttempt {
            key,
            retention,
            store,
        }) = replay
        {
            let store_type = store.scope().as_str();
            let metrics_store = Arc::clone(&store);
            match store.reserve(&key, retention).await {
                Ok(ReserveOutcome::Reserved(reservation)) => {
                    self.hmac_metrics.replay(store_type, "reserve", "reserved");
                    ctx.hmac_replay = WebhookReplayState::Reserved { store, reservation };
                }
                Ok(ReserveOutcome::Duplicate) => {
                    self.hmac_metrics.replay(store_type, "reserve", "duplicate");
                    self.hmac_metrics.request(profile, "duplicate");
                    if let Some(entries) = metrics_store.local_entries().await {
                        self.hmac_metrics.local_entries(entries);
                    }
                    return self.write_empty_response(session, ctx, 200).await.map(Some);
                }
                Err(error) => {
                    self.hmac_metrics
                        .replay(store_type, "reserve", "unavailable");
                    self.hmac_metrics.request(profile, "store_unavailable");
                    warn!(profile, error = %error, "HMAC replay reservation failed");
                    return self
                        .write_rejection_response(
                            session,
                            ctx,
                            HandlerRejection::new(
                                503,
                                "ERR10001",
                                "webhook replay store is unavailable",
                            ),
                        )
                        .await
                        .map(Some);
                }
            }
            if let Some(entries) = metrics_store.local_entries().await {
                self.hmac_metrics.local_entries(entries);
            }
        }
        self.hmac_metrics.body(profile, body.len());
        self.hmac_metrics.request(profile, "accepted");
        ctx.hmac_profile = Some(profile.to_string());
        ctx.hmac_verified_body = Some(body);
        ctx.hmac_body_permit = Some(permit);
        Ok(None)
    }

    async fn release_hmac_reservation(&self, ctx: &mut GatewayRequestContext) {
        let state = std::mem::replace(&mut ctx.hmac_replay, WebhookReplayState::Releasing);
        let WebhookReplayState::Reserved { store, reservation } = state else {
            ctx.hmac_replay = state;
            return;
        };
        let store_type = store.scope().as_str();
        let metrics_store = Arc::clone(&store);
        match store.release(&reservation).await {
            Ok(()) => {
                self.hmac_metrics.replay(store_type, "release", "released");
                ctx.hmac_replay = WebhookReplayState::Released;
            }
            Err(error) => {
                self.hmac_metrics
                    .replay(store_type, "release", "unavailable");
                warn!(profile = ctx.hmac_profile.as_deref().unwrap_or("unknown"), error = %error, "HMAC replay release failed");
                ctx.hmac_replay = WebhookReplayState::Reserved { store, reservation };
            }
        }
        if let Some(entries) = metrics_store.local_entries().await {
            self.hmac_metrics.local_entries(entries);
        }
    }
}

#[async_trait]
impl ProxyHttp for GatewayProxy {
    type CTX = GatewayRequestContext;

    fn new_ctx(&self) -> Self::CTX {
        GatewayRequestContext::default()
    }

    fn prebuffered_request_body(&self, _session: &Session, ctx: &Self::CTX) -> Option<Bytes> {
        ctx.hmac_verified_body.clone()
    }

    async fn request_filter(
        &self,
        session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<bool>
    where
        Self::CTX: Send + Sync,
    {
        ctx.begin_request();
        let request_path = session.req_header().uri.path().to_string();
        ctx.request_path = request_path.clone();
        if request_path == HEALTH_PATH {
            return self.write_text_response(session, ctx, 200, "ok").await;
        }
        match self.admission.try_enter(AdmissionKind::Application) {
            Ok(permit) => ctx.admission_permit = Some(permit),
            Err(_) => {
                return self
                    .write_bytes_response_with_headers(
                        session,
                        ctx,
                        503,
                        Some("text/plain; charset=utf-8"),
                        None,
                        Bytes::from_static(b"service unavailable"),
                        &[
                            ("connection".to_string(), "close".to_string()),
                            ("retry-after".to_string(), "0".to_string()),
                        ],
                    )
                    .await;
            }
        }

        let method = session.req_header().method.as_str().to_string();
        ctx.method = method.clone();
        let security_execution = self.security_execution.load();
        ctx.security_execution = Some(Arc::clone(&security_execution));
        let active_handlers = Arc::clone(&security_execution.active_handlers);
        let spa_session_endpoint = self
            .active_spa_session_endpoint(&active_handlers, &request_path)
            .map_err(pingora_internal_error)?;
        if let Some(endpoint) = spa_session_endpoint {
            if let Some(rejection) = spa_session_method_rejection(endpoint, &method) {
                if spa_session_rejection_uses_cors(&active_handlers, &request_path, endpoint)
                    .map_err(pingora_internal_error)?
                {
                    if let Some(config) = self.cors_config.load().as_ref().as_ref() {
                        let outcome = evaluate_cors_request(
                            session,
                            config,
                            &request_path,
                            &self.server_scheme,
                            self.server_port,
                        );
                        if let Some(status) = capture_cors_outcome(ctx, outcome) {
                            return self.write_empty_response(session, ctx, status).await;
                        }
                    }
                }
                return self
                    .write_spa_session_rejection_response(session, ctx, rejection)
                    .await;
            }
        }

        let resolved = active_handlers
            .resolve_handler_chain(&request_path, &method)
            .map_err(pingora_internal_error)?;
        ctx.handler_ids = resolved.handler_ids.clone();
        ctx.endpoint = resolved.endpoint(&request_path, &method);
        ctx.path_params = resolved
            .path
            .as_ref()
            .map(|path| path.params.clone())
            .unwrap_or_default();

        if ctx.handler_ids.is_empty() {
            if let Some((target, rewrite_host_header, reuse_x_forwarded)) = self.select_upstream() {
                ctx.proxy_target = Some(target);
                ctx.rewrite_host_header = rewrite_host_header;
                ctx.reuse_x_forwarded = reuse_x_forwarded;
                return Ok(false);
            }
            return self
                .write_text_response(session, ctx, 404, "not found")
                .await;
        }

        let handler_ids = ctx.handler_ids.clone();
        for (handler_index, handler_id) in handler_ids.clone().into_iter().enumerate() {
            let started = Instant::now();
            match handler_id.as_str() {
                "correlation" => {
                    if let Some(config) = self.correlation_config.load().as_ref().as_ref() {
                        ctx.correlation = apply_correlation_request(session, config)?;
                    }
                }
                "cors" => {
                    if let Some(config) = self.cors_config.load().as_ref().as_ref() {
                        let outcome = evaluate_cors_request(
                            session,
                            config,
                            &request_path,
                            &self.server_scheme,
                            self.server_port,
                        );
                        if let Some(status) = capture_cors_outcome(ctx, outcome) {
                            ctx.record_handler_duration(&handler_id, started.elapsed());
                            return self.write_empty_response(session, ctx, status).await;
                        }
                    }
                }
                "metrics" => {
                    ctx.metrics_enabled = self.metrics_config.load().as_ref().is_some();
                }
                "header" | "headers" => {
                    if let Some(config) = self.header_config.load().as_ref().as_ref() {
                        apply_header_request(session, config, &request_path)?;
                    }
                }
                "api-key" | "apikey" => {
                    if let Some(config) = security_execution.api_key.as_ref() {
                        if let Err(rejection) = verify_api_key(session, config, &request_path) {
                            ctx.record_handler_duration(&handler_id, started.elapsed());
                            return self.write_rejection_response(session, ctx, rejection).await;
                        }
                    }
                }
                "basic-auth" | "basic" => {
                    if let Some(config) = security_execution.basic_auth.as_ref() {
                        if let Err(rejection) = verify_basic_auth(session, config, &request_path) {
                            ctx.record_handler_duration(&handler_id, started.elapsed());
                            return self.write_rejection_response(session, ctx, rejection).await;
                        }
                    }
                }
                "security" | "jwt" => {
                    if let Some(result) = self.authenticate_agent_delegation(session).await {
                        match result {
                            Ok((principal, delegation)) => {
                                ctx.auth = Some(principal);
                                ctx.agent_delegation = Some(delegation);
                                continue;
                            }
                            Err(rejection) => {
                                return self
                                    .write_rejection_response(session, ctx, rejection)
                                    .await;
                            }
                        }
                    }
                    if let Some(runtime) = security_execution.security.as_ref() {
                        match verify_jwt_request(session, runtime, &request_path).await {
                            Ok(auth) => {
                                if auth.is_some() {
                                    ctx.auth = auth;
                                }
                            }
                            Err(rejection) => {
                                ctx.record_handler_duration(&handler_id, started.elapsed());
                                return self
                                    .write_rejection_response(session, ctx, rejection)
                                    .await;
                            }
                        }
                    }
                }
                "unified-security" | "unified" => {
                    let unified_config = Arc::clone(&security_execution.unified_security);
                    let hmac_required = unified_config.as_ref().as_ref().is_some_and(|config| {
                        config.hmac_profile_for(&request_path, &method).is_some()
                    });
                    if !hmac_required {
                        if let Some(result) = self.authenticate_agent_delegation(session).await {
                            match result {
                                Ok((principal, delegation)) => {
                                    ctx.auth = Some(principal);
                                    ctx.agent_delegation = Some(delegation);
                                    continue;
                                }
                                Err(rejection) => {
                                    return self
                                        .write_rejection_response(session, ctx, rejection)
                                        .await;
                                }
                            }
                        }
                    }
                    if let Some(config) = unified_config.as_ref().as_ref() {
                        match verify_unified_security(
                            session,
                            config,
                            security_execution.basic_auth.as_ref().as_ref(),
                            security_execution.api_key.as_ref().as_ref(),
                            security_execution.security.as_ref().as_ref(),
                            &request_path,
                            &method,
                        )
                        .await
                        {
                            Ok(outcome) => {
                                if outcome.principal.is_some() {
                                    ctx.auth = outcome.principal;
                                }
                                if let Some(profile) = outcome.hmac_profile {
                                    ctx.record_handler_duration(&handler_id, started.elapsed());
                                    if let Some(response) = self
                                        .enter_hmac_gate(session, ctx, "unified-security", &profile)
                                        .await?
                                    {
                                        return Ok(response);
                                    }
                                }
                            }
                            Err(rejection) => {
                                ctx.record_handler_duration(&handler_id, started.elapsed());
                                return self
                                    .write_rejection_response(session, ctx, rejection)
                                    .await;
                            }
                        }
                    }
                }
                "hmac" => {
                    let profile = security_execution
                        .hmac
                        .as_ref()
                        .as_ref()
                        .and_then(|runtime| runtime.standalone_profile(&request_path, &method))
                        .map(str::to_string);
                    ctx.record_handler_duration(&handler_id, started.elapsed());
                    let Some(profile) = profile else {
                        self.hmac_metrics.request("unmatched", "chain_error");
                        return self
                            .write_rejection_response(
                                session,
                                ctx,
                                HandlerRejection::new(503, "ERR10001", "request path is not configured for standalone HMAC authentication"),
                            )
                            .await;
                    };
                    if let Some(response) =
                        self.enter_hmac_gate(session, ctx, "hmac", &profile).await?
                    {
                        return Ok(response);
                    }
                }
                "limit" | "rate-limit" => {
                    if let Some(runtime) = self.rate_limit_runtime.load().as_ref().as_ref() {
                        match check_rate_limit(session, runtime, ctx.auth.as_ref(), &request_path) {
                            Ok(headers) => {
                                ctx.rate_limit_headers = headers;
                            }
                            Err(rejection) => {
                                ctx.record_handler_duration(&handler_id, started.elapsed());
                                return self
                                    .write_rejection_response(session, ctx, rejection)
                                    .await;
                            }
                        }
                    }
                }
                "prefix" | "path-prefix-service" | "pathPrefixService" => {
                    if let Some(config) = self.path_prefix_service_config.load().as_ref().as_ref() {
                        apply_path_prefix_service(session, config, &request_path)?;
                    }
                }
                "token" => {
                    if let Some(runtime) = self.token_runtime.load().as_ref().as_ref()
                        && let Err(rejection) =
                            apply_token_request(session, runtime, &request_path).await
                    {
                        ctx.record_handler_duration(&handler_id, started.elapsed());
                        return self.write_rejection_response(session, ctx, rejection).await;
                    }
                }
                "tokenize" => {
                    let runtime = self.pii_tokenization.load();
                    let Some(runtime) = runtime.as_ref().as_ref() else {
                        ctx.record_handler_duration(&handler_id, started.elapsed());
                        return self
                            .write_text_response(
                                session,
                                ctx,
                                502,
                                "pii tokenization is not configured",
                            )
                            .await;
                    };
                    if runtime.has_request_rules(&request_path, &method) {
                        if let Err(rejection) = runtime.validate_auth(ctx.auth.as_ref()) {
                            ctx.record_handler_duration(&handler_id, started.elapsed());
                            return self.write_rejection_response(session, ctx, rejection).await;
                        }
                        if request_header(session, "content-encoding").is_some() {
                            ctx.record_handler_duration(&handler_id, started.elapsed());
                            return self
                                .write_rejection_response(
                                    session,
                                    ctx,
                                    HandlerRejection::new(
                                        415,
                                        "ERR13017",
                                        "tokenize handler does not support encoded request bodies",
                                    ),
                                )
                                .await;
                        }
                        session.req_header_mut().remove_header("content-length");
                        ctx.tokenize_active = true;
                    }
                }
                "detokenize" => {
                    let runtime = self.pii_tokenization.load();
                    let Some(runtime) = runtime.as_ref().as_ref() else {
                        ctx.record_handler_duration(&handler_id, started.elapsed());
                        return self
                            .write_text_response(
                                session,
                                ctx,
                                502,
                                "pii tokenization is not configured",
                            )
                            .await;
                    };
                    if runtime.has_response_rules(&request_path, &method) {
                        if let Err(rejection) = runtime.validate_auth(ctx.auth.as_ref()) {
                            ctx.record_handler_duration(&handler_id, started.elapsed());
                            return self.write_rejection_response(session, ctx, rejection).await;
                        }
                        ctx.detokenize_active = true;
                    }
                }
                "access-control" => {
                    let runtime = self.access_control.load();
                    let Some(runtime) = runtime
                        .as_ref()
                        .as_ref()
                        .filter(|runtime| runtime.authorization_enabled())
                    else {
                        ctx.record_handler_duration(&handler_id, started.elapsed());
                        continue;
                    };
                    if request_header(session, "content-encoding").is_some()
                        && method_has_request_body(&method)
                    {
                        ctx.record_handler_duration(&handler_id, started.elapsed());
                        return self
                            .write_rejection_response(
                                session,
                                ctx,
                                HandlerRejection::new(
                                    415,
                                    "ERR13021",
                                    "access-control handler does not support encoded request bodies",
                                ),
                            )
                            .await;
                    }
                    if method_has_request_body(&method) {
                        ctx.access_control_active = true;
                    } else {
                        let exchange = access_control_exchange(
                            ctx.endpoint.as_str(),
                            ctx.request_path.as_str(),
                            session.req_header().uri.query(),
                            None,
                            ctx.auth.as_ref(),
                        )
                        .map_err(handler_rejection_error)?;
                        match runtime
                            .authorize_http_endpoint(
                                exchange.endpoint.as_str(),
                                &agent_headers(session),
                                ctx.auth.as_ref(),
                                &exchange.request_data,
                                ctx.correlation.correlation_id.as_deref(),
                            )
                            .await
                        {
                            AccessDecision::Allowed => {
                                let has_response_filter =
                                    runtime.has_response_filter(exchange.endpoint.as_str());
                                ctx.access_control_exchange = Some(exchange);
                                ctx.access_control_response_active = has_response_filter;
                            }
                            AccessDecision::Denied(message) => {
                                warn!(
                                    endpoint = exchange.endpoint.as_str(),
                                    request_path = ctx.request_path.as_str(),
                                    method = ctx.method.as_str(),
                                    client_id = ctx
                                        .auth
                                        .as_ref()
                                        .and_then(|auth| auth.client_id.as_deref())
                                        .unwrap_or(""),
                                    user_id = ctx
                                        .auth
                                        .as_ref()
                                        .and_then(|auth| auth.user_id.as_deref())
                                        .unwrap_or(""),
                                    correlation_id =
                                        ctx.correlation.correlation_id.as_deref().unwrap_or(""),
                                    reason = message.as_str(),
                                    "access-control denied request"
                                );
                                ctx.record_handler_duration(&handler_id, started.elapsed());
                                return self
                                    .write_string_response(session, ctx, 403, message)
                                    .await;
                            }
                        }
                    }
                }
                "stateless" | "google" | "facebook" | "github" => {
                    let runtime = self.stateless_auth.load();
                    let Some(runtime) = runtime.as_ref().as_ref() else {
                        ctx.record_handler_duration(&handler_id, started.elapsed());
                        continue;
                    };
                    match runtime.handle_request(session, handler_id.as_str()).await {
                        Err(rejection) => {
                            ctx.record_handler_duration(&handler_id, started.elapsed());
                            return self.write_rejection_response(session, ctx, rejection).await;
                        }
                        Ok(outcome) => match outcome {
                            StatelessAuthOutcome::Continue {
                                auth,
                                response_headers,
                            } => {
                                if auth.is_some() {
                                    ctx.auth = auth;
                                }
                                merge_extra_response_headers(
                                    &mut ctx.extra_response_headers,
                                    response_headers,
                                );
                            }
                            StatelessAuthOutcome::Respond(response) => {
                                ctx.record_handler_duration(&handler_id, started.elapsed());
                                return self.write_spa_auth_response(session, ctx, response).await;
                            }
                        },
                    }
                }
                "msal-exchange" => {
                    let runtime = self.msal_exchange.load();
                    let Some(runtime) = runtime.as_ref().as_ref() else {
                        ctx.record_handler_duration(&handler_id, started.elapsed());
                        continue;
                    };
                    match runtime.handle_request(session).await {
                        Err(rejection) => {
                            ctx.record_handler_duration(&handler_id, started.elapsed());
                            return self
                                .write_spa_session_rejection_response(session, ctx, rejection)
                                .await;
                        }
                        Ok(outcome) => match outcome {
                            MsalExchangeOutcome::Continue {
                                auth,
                                response_headers,
                            } => {
                                if auth.is_some() {
                                    ctx.auth = auth;
                                }
                                merge_extra_response_headers(
                                    &mut ctx.extra_response_headers,
                                    response_headers,
                                );
                            }
                            MsalExchangeOutcome::Respond(response) => {
                                ctx.record_handler_duration(&handler_id, started.elapsed());
                                return self.write_spa_auth_response(session, ctx, response).await;
                            }
                        },
                    }
                }
                "msal-auth" => {
                    let runtime = self.msal_auth.load();
                    let Some(runtime) = runtime.as_ref().as_ref() else {
                        ctx.record_handler_duration(&handler_id, started.elapsed());
                        continue;
                    };
                    match runtime.handle_request(session).await {
                        Err(rejection) => {
                            ctx.record_handler_duration(&handler_id, started.elapsed());
                            return self
                                .write_spa_session_rejection_response(session, ctx, rejection)
                                .await;
                        }
                        Ok(outcome) => match outcome {
                            light_pingora::SpaSessionOutcome::Continue {
                                auth,
                                response_headers,
                            } => {
                                if auth.is_some() {
                                    ctx.auth = auth;
                                }
                                merge_extra_response_headers(
                                    &mut ctx.extra_response_headers,
                                    response_headers,
                                );
                            }
                            light_pingora::SpaSessionOutcome::Respond(response) => {
                                ctx.record_handler_duration(&handler_id, started.elapsed());
                                return self.write_spa_auth_response(session, ctx, response).await;
                            }
                        },
                    }
                }
                "websocket" => {
                    let runtime = self.websocket_router.load();
                    let Some(runtime) = runtime.as_ref().as_ref() else {
                        ctx.record_handler_duration(&handler_id, started.elapsed());
                        return self
                            .write_text_response(
                                session,
                                ctx,
                                502,
                                "websocket router is not configured",
                            )
                            .await;
                    };
                    if !is_websocket_upgrade(session) {
                        ctx.record_handler_duration(&handler_id, started.elapsed());
                        return self
                            .write_text_response(session, ctx, 426, "upgrade required")
                            .await;
                    }
                    let handshake = match runtime.prepare_handshake(
                        request_path.as_str(),
                        request_header(session, "origin").as_deref(),
                        request_cookie_value(session, "csrf").as_deref(),
                        request_header(session, "sec-websocket-protocol").as_deref(),
                    ) {
                        Ok(handshake) => handshake,
                        Err(error) => {
                            ctx.record_handler_duration(&handler_id, started.elapsed());
                            return self
                                .write_string_response(
                                    session,
                                    ctx,
                                    websocket_route_status(&error),
                                    error.to_string(),
                                )
                                .await;
                        }
                    };
                    let trusted_authorization = handshake
                        .as_ref()
                        .and_then(|_| request_header(session, "authorization"));
                    let headers = agent_headers(session);
                    let decision = match runtime.resolve(
                        &request_path,
                        session.req_header().uri.query(),
                        headers
                            .iter()
                            .map(|(name, value)| (name.as_str(), value.as_str())),
                    ) {
                        Ok(decision) => decision,
                        Err(error) => {
                            ctx.record_handler_duration(&handler_id, started.elapsed());
                            return self
                                .write_string_response(
                                    session,
                                    ctx,
                                    websocket_route_status(&error),
                                    error.to_string(),
                                )
                                .await;
                        }
                    };
                    ctx.endpoint =
                        websocket_policy_endpoint(request_path.as_str(), ctx.endpoint.as_str());
                    match runtime
                        .authorize(
                            &decision,
                            ctx.endpoint.as_str(),
                            &websocket_policy_headers(session),
                            ctx.auth.as_ref(),
                            ctx.correlation.correlation_id.as_deref(),
                        )
                        .await
                    {
                        AccessDecision::Allowed => {}
                        AccessDecision::Denied(message) => {
                            warn!(
                                policy_endpoint = ctx.endpoint.as_str(),
                                denial_category = "connection_policy_denied",
                                "websocket connection denied by access-control policy"
                            );
                            ctx.record_handler_duration(&handler_id, started.elapsed());
                            return self.write_string_response(session, ctx, 403, message).await;
                        }
                    }
                    if let Err(error) = runtime.check_upgrade_rate() {
                        ctx.record_handler_duration(&handler_id, started.elapsed());
                        return self
                            .write_string_response(
                                session,
                                ctx,
                                websocket_route_status(&error),
                                error.to_string(),
                            )
                            .await;
                    }
                    let index = self.next_upstream.fetch_add(1, Ordering::Relaxed);
                    match runtime.select_target(&decision, index).await {
                        Ok(target) => {
                            let permit = match runtime.acquire_connection() {
                                Ok(permit) => permit,
                                Err(error) => {
                                    ctx.record_handler_duration(&handler_id, started.elapsed());
                                    return self
                                        .write_string_response(
                                            session,
                                            ctx,
                                            websocket_route_status(&error),
                                            error.to_string(),
                                        )
                                        .await;
                                }
                            };
                            ctx.proxy_target = Some(target);
                            ctx.rewrite_host_header = true;
                            ctx.websocket_preserve_routing_headers =
                                runtime.config().preserve_routing_headers;
                            ctx.websocket_idle_timeout = runtime.idle_timeout();
                            ctx.websocket_max_connection_duration =
                                runtime.max_connection_duration();
                            ctx.websocket_permit = Some(permit);
                            ctx.websocket_handshake = handshake;
                            ctx.websocket_trusted_authorization = trusted_authorization;
                            let timeout = websocket_io_timeout(ctx);
                            session.as_downstream_mut().set_read_timeout(timeout);
                            session.as_downstream_mut().set_write_timeout(timeout);
                            ctx.websocket_decision = Some(decision);
                            ctx.record_handler_duration(&handler_id, started.elapsed());
                            return Ok(false);
                        }
                        Err(error) => {
                            ctx.record_handler_duration(&handler_id, started.elapsed());
                            return self
                                .write_string_response(
                                    session,
                                    ctx,
                                    websocket_route_status(&error),
                                    error.to_string(),
                                )
                                .await;
                        }
                    }
                }
                "llm" => {
                    let Some(module) = self.llm_gateway.load_full() else {
                        ctx.record_handler_duration(&handler_id, started.elapsed());
                        return self
                            .write_llm_error_response(
                                session,
                                ctx,
                                503,
                                "service_unavailable",
                                "LLM routing is unavailable",
                            )
                            .await;
                    };
                    let preceding = &handler_ids[..handler_index];
                    let ordered_security = preceding.iter().any(|id| id == "correlation")
                        && preceding
                            .iter()
                            .any(|id| id == "unified-security" || id == "unified")
                        && preceding
                            .iter()
                            .any(|id| id == "limit" || id == "rate-limit")
                        && preceding.iter().any(|id| id == "access-control");
                    if !ordered_security {
                        ctx.record_handler_duration(&handler_id, started.elapsed());
                        return self
                            .write_llm_error_response(
                                session,
                                ctx,
                                500,
                                "internal_error",
                                "invalid llm handler security order",
                            )
                            .await;
                    }
                    if !llm_access_control_ready(
                        &method,
                        ctx.access_control_active,
                        ctx.access_control_exchange.is_some(),
                    ) {
                        ctx.record_handler_duration(&handler_id, started.elapsed());
                        return self
                            .write_llm_error_response(
                                session,
                                ctx,
                                503,
                                "service_unavailable",
                                "LLM body-aware access control is unavailable",
                            )
                            .await;
                    }
                    let embedding_route = request_path == "/v1/embeddings";
                    let embedding_ingress_permit = if embedding_route {
                        match Arc::clone(&module.embedding_ingress_permits).try_acquire_owned() {
                            Ok(permit) => Some(permit),
                            Err(_) => {
                                ctx.record_handler_duration(&handler_id, started.elapsed());
                                return self
                                    .write_llm_error_response(
                                        session,
                                        ctx,
                                        429,
                                        "capacity_exhausted",
                                        "embedding ingress capacity is exhausted",
                                    )
                                    .await;
                            }
                        }
                    } else {
                        None
                    };
                    let body = if method_has_request_body(&method) {
                        let body = if embedding_route {
                            match read_bounded_request_body_with_rate(
                                session,
                                module.max_embedding_request_body_bytes,
                                module.embedding_body_read_timeout,
                                module.embedding_minimum_receive_bytes_per_second,
                            )
                            .await?
                            {
                                BoundedBodyRead::Complete(body) => body,
                                BoundedBodyRead::TooLarge => {
                                    ctx.record_handler_duration(&handler_id, started.elapsed());
                                    return self
                                        .write_llm_error_response(
                                            session,
                                            ctx,
                                            413,
                                            "payload_too_large",
                                            "request body is too large",
                                        )
                                        .await;
                                }
                                BoundedBodyRead::TooSlow => {
                                    ctx.record_handler_duration(&handler_id, started.elapsed());
                                    return self
                                        .write_llm_error_response(
                                            session,
                                            ctx,
                                            408,
                                            "request_timeout",
                                            "embedding request body timed out",
                                        )
                                        .await;
                                }
                            }
                        } else {
                            let Some(body) =
                                read_bounded_request_body(session, module.max_request_body_bytes)
                                    .await?
                            else {
                                ctx.record_handler_duration(&handler_id, started.elapsed());
                                return self
                                    .write_llm_error_response(
                                        session,
                                        ctx,
                                        413,
                                        "payload_too_large",
                                        "request body is too large",
                                    )
                                    .await;
                            };
                            body
                        };
                        if ctx.access_control_active {
                            let exchange = access_control_exchange(
                                ctx.endpoint.as_str(),
                                ctx.request_path.as_str(),
                                session.req_header().uri.query(),
                                Some(body.as_slice()),
                                ctx.auth.as_ref(),
                            )
                            .map_err(handler_rejection_error)?;
                            let runtime = self.access_control.load();
                            let Some(runtime) = runtime
                                .as_ref()
                                .as_ref()
                                .filter(|runtime| runtime.authorization_enabled())
                            else {
                                return self
                                    .write_llm_error_response(
                                        session,
                                        ctx,
                                        503,
                                        "service_unavailable",
                                        "access control is unavailable",
                                    )
                                    .await;
                            };
                            let authorization_headers = agent_headers(session);
                            let authorization = runtime.authorize_http_endpoint(
                                exchange.endpoint.as_str(),
                                &authorization_headers,
                                ctx.auth.as_ref(),
                                &exchange.request_data,
                                ctx.correlation.correlation_id.as_deref(),
                            );
                            let decision = if embedding_route {
                                match tokio::time::timeout(
                                    module.embedding_authorization_timeout,
                                    authorization,
                                )
                                .await
                                {
                                    Ok(decision) => decision,
                                    Err(_) => {
                                        ctx.record_handler_duration(&handler_id, started.elapsed());
                                        return self
                                            .write_llm_error_response(
                                                session,
                                                ctx,
                                                503,
                                                "service_unavailable",
                                                "embedding authorization timed out",
                                            )
                                            .await;
                                    }
                                }
                            } else {
                                authorization.await
                            };
                            match decision {
                                AccessDecision::Allowed => {
                                    ctx.access_control_active = false;
                                    ctx.access_control_exchange = Some(exchange);
                                }
                                AccessDecision::Denied(message) => {
                                    ctx.record_handler_duration(&handler_id, started.elapsed());
                                    tracing::debug!(reason = %message, "LLM request denied");
                                    return self
                                        .write_llm_error_response(
                                            session,
                                            ctx,
                                            403,
                                            "permission_denied",
                                            "The request was denied",
                                        )
                                        .await;
                                }
                            }
                        }
                        body
                    } else {
                        Vec::new()
                    };
                    let headers = agent_headers(session)
                        .into_iter()
                        .map(|(name, value)| (name.to_ascii_lowercase(), value))
                        .collect();
                    let principal_id = ctx
                        .auth
                        .as_ref()
                        .and_then(|auth| auth.client_id.clone().or_else(|| auth.user_id.clone()))
                        .unwrap_or_else(|| "anonymous".to_string());
                    let tenant_id = ctx.auth.as_ref().and_then(|auth| {
                        auth.host
                            .clone()
                            .filter(|value| !value.is_empty())
                            .or_else(|| {
                                [
                                    "tenant",
                                    "tenant_id",
                                    "tenantId",
                                    "host",
                                    "host_id",
                                    "hostId",
                                ]
                                .into_iter()
                                .find_map(|claim| {
                                    auth.claims
                                        .get(claim)
                                        .and_then(serde_json::Value::as_str)
                                        .filter(|value| !value.is_empty())
                                        .map(str::to_string)
                                })
                            })
                    });
                    let trusted_request_id = ctx
                        .correlation
                        .correlation_id
                        .clone()
                        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
                    let response = module
                        .http
                        .handle_route_with_embedding_ingress(
                            BufferedHttpRequest {
                                method: method.clone(),
                                path: request_path.clone(),
                                headers,
                                body,
                                principal_id,
                                tenant_id,
                                trusted_request_id,
                            },
                            embedding_ingress_permit,
                        )
                        .await;
                    ctx.record_handler_duration(&handler_id, started.elapsed());
                    return match response {
                        LlmHttpResponse::Buffered(response) => {
                            let llm_gateway::http::BufferedHttpResponse {
                                status,
                                headers,
                                body,
                                lifecycle,
                            } = response;
                            let content_type = headers
                                .get("content-type")
                                .map(String::as_str)
                                .unwrap_or("application/json");
                            let extra_headers = headers
                                .iter()
                                .filter(|(name, _)| name.as_str() != "content-type")
                                .map(|(name, value)| (name.clone(), value.clone()))
                                .collect::<Vec<_>>();
                            let response_body = Bytes::from(body);
                            let write = self.write_bytes_response_with_headers(
                                session,
                                ctx,
                                status,
                                Some(content_type),
                                None,
                                response_body.clone(),
                                &extra_headers,
                            );
                            if let Some(lifecycle) = lifecycle {
                                let deadline = buffered_embedding_drain_deadline(
                                    response_body.len(),
                                    lifecycle.write_timeout,
                                    lifecycle.minimum_drain_bytes_per_second,
                                );
                                let result =
                                    tokio::time::timeout(deadline, write).await.map_err(|_| {
                                        Error::explain(
                                            ErrorType::InternalError,
                                            "embedding response write grace or minimum drain rate was exceeded",
                                        )
                                    })?;
                                drop(lifecycle.memory_permit);
                                result
                            } else {
                                write.await
                            }
                        }
                        LlmHttpResponse::Streaming(response) => {
                            self.write_llm_streaming_response(session, ctx, *response)
                                .await
                        }
                    };
                }
                "mcp" => {
                    let runtime = self.mcp_router.load();
                    let Some(runtime) = runtime.as_ref().as_ref() else {
                        ctx.record_handler_duration(&handler_id, started.elapsed());
                        continue;
                    };
                    if !runtime.matches_path(&request_path) {
                        ctx.record_handler_duration(&handler_id, started.elapsed());
                        continue;
                    }
                    let path_with_query = match session.req_header().uri.query() {
                        Some(query) => format!("{request_path}?{query}"),
                        None => request_path.clone(),
                    };
                    let headers = agent_headers(session);
                    if let Some(response) = runtime
                        .preflight_request(path_with_query.as_str(), &headers)
                        .map_err(pingora_internal_error)?
                    {
                        ctx.record_handler_duration(&handler_id, started.elapsed());
                        return self.write_mcp_response(session, ctx, response).await;
                    }
                    let Some(body) =
                        read_bounded_request_body(session, runtime.max_request_body_bytes())
                            .await?
                    else {
                        let response = runtime
                            .request_body_too_large_response()
                            .map_err(pingora_internal_error)?;
                        ctx.record_handler_duration(&handler_id, started.elapsed());
                        return self.write_mcp_response(session, ctx, response).await;
                    };
                    let request = McpHttpRequest {
                        method: method.clone(),
                        path: path_with_query,
                        headers,
                        body,
                    };
                    match runtime
                        .handle_request_with_context(
                            request,
                            McpRequestContext {
                                auth: ctx.auth.clone(),
                                authorization: request_header(session, "authorization"),
                                correlation_id: ctx.correlation.correlation_id.clone(),
                                delegation: ctx.agent_delegation.clone(),
                                anonymous_binding: client_ip(session)
                                    .map(|address| format!("peer:{address}")),
                            },
                        )
                        .await
                        .map_err(pingora_internal_error)?
                    {
                        Some(response) => {
                            ctx.record_handler_duration(&handler_id, started.elapsed());
                            return self.write_mcp_response(session, ctx, response).await;
                        }
                        None => {
                            ctx.record_handler_duration(&handler_id, started.elapsed());
                            continue;
                        }
                    }
                }
                "health" => {
                    ctx.record_handler_duration(&handler_id, started.elapsed());
                    return self.write_text_response(session, ctx, 200, "ok").await;
                }
                "sidecar-deny" => {
                    ctx.record_handler_duration(&handler_id, started.elapsed());
                    return self
                        .write_text_response(session, ctx, 404, "not found")
                        .await;
                }
                "sidecar-identity" => {
                    let body = model_provider_sidecar::sidecar_identity_json()
                        .map_err(|error| pingora_internal_error(RuntimeError::Config(error)))?;
                    ctx.record_handler_duration(&handler_id, started.elapsed());
                    return self
                        .write_bytes_response(
                            session,
                            ctx,
                            200,
                            "application/json",
                            Some("no-store"),
                            Bytes::from(body),
                        )
                        .await;
                }
                "virtual" => {
                    let host_header = request_header(session, "host");
                    let resolution = self
                        .static_resources
                        .load()
                        .resolve_virtual_host(host_header.as_deref(), &request_path);
                    ctx.record_handler_duration(&handler_id, started.elapsed());
                    return self.write_static_resolution(session, ctx, resolution).await;
                }
                "path-resource" | "resource" => {
                    let resolution = self
                        .static_resources
                        .load()
                        .resolve_path_resource(&request_path);
                    ctx.record_handler_duration(&handler_id, started.elapsed());
                    return self.write_static_resolution(session, ctx, resolution).await;
                }
                "proxy" => {
                    if let Some((target, rewrite_host_header, reuse_x_forwarded)) =
                        self.select_upstream()
                    {
                        ctx.proxy_target = Some(target);
                        ctx.rewrite_host_header = rewrite_host_header;
                        ctx.reuse_x_forwarded = reuse_x_forwarded;
                        if let Err(rejection) = self.prepare_response_handlers(
                            ctx,
                            &handler_ids[handler_index + 1..],
                            &request_path,
                            &method,
                        ) {
                            ctx.record_handler_duration(&handler_id, started.elapsed());
                            return self.write_rejection_response(session, ctx, rejection).await;
                        }
                        ctx.record_handler_duration(&handler_id, started.elapsed());
                        return Ok(false);
                    }
                    ctx.record_handler_duration(&handler_id, started.elapsed());
                    return self
                        .write_text_response(session, ctx, 502, "proxy is not configured")
                        .await;
                }
                "router" => {
                    ctx.record_handler_duration(&handler_id, started.elapsed());
                    if request_path == "/portal/query" && !ctx.access_control_active {
                        if method_has_request_body(&method) {
                            ctx.access_control_active = true;
                            ctx.runtime_query_access_control_only = true;
                        } else if let Ok(Some(exchange)) = required_runtime_query_exchange(
                            ctx.endpoint.as_str(),
                            request_path.as_str(),
                            session.req_header().uri.query(),
                            None,
                        ) {
                            let runtime = self.access_control.load();
                            let decision = authorize_required_runtime_query(
                                runtime.as_ref().as_ref(),
                                &exchange,
                                &agent_headers(session),
                                ctx.auth.as_ref(),
                                ctx.correlation.correlation_id.as_deref(),
                            )
                            .await;
                            if let Err(rejection) = decision {
                                return self
                                    .write_rejection_response(session, ctx, rejection)
                                    .await;
                            }
                        }
                    }
                    let route = self.router_route.load();
                    let Some(route) = route.as_ref().as_ref() else {
                        return self
                            .write_text_response(session, ctx, 502, "router is not configured")
                            .await;
                    };
                    let index = self.next_upstream.fetch_add(1, Ordering::Relaxed);
                    match select_router_target(session, route, index).await {
                        Ok(decision) => {
                            ctx.proxy_target = Some(decision.target.clone());
                            ctx.rewrite_host_header = route.config.rewrite_host_header;
                            ctx.reuse_x_forwarded = route.config.reuse_x_forwarded;
                            ctx.router_decision = Some(decision);
                            if let Err(rejection) = self.prepare_response_handlers(
                                ctx,
                                &handler_ids[handler_index + 1..],
                                &request_path,
                                &method,
                            ) {
                                return self
                                    .write_rejection_response(session, ctx, rejection)
                                    .await;
                            }
                            return Ok(false);
                        }
                        Err(rejection) => {
                            return self.write_rejection_response(session, ctx, rejection).await;
                        }
                    }
                }
                _ => {}
            }
            ctx.record_handler_duration(&handler_id, started.elapsed());
        }

        self.write_text_response(session, ctx, 404, "not found")
            .await
    }

    async fn upstream_peer(
        &self,
        _session: &mut Session,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<Box<HttpPeer>> {
        let upstream = ctx.proxy_target.as_ref().ok_or_else(|| {
            Error::explain(
                ErrorType::InternalError,
                "no proxy target selected by handler chain",
            )
        })?;
        if self.is_upstream_circuit_open(upstream) {
            return Err(Error::explain(
                ErrorType::HTTPStatus(503),
                format!("upstream circuit is open for {}", upstream.address),
            ));
        }
        debug!("proxying request to {}", upstream.address);
        let mut peer = if upstream.tls {
            if let Some(cert_key) = self.upstream_client_cert_key.as_ref() {
                HttpPeer::new_mtls(
                    upstream.address.as_str(),
                    upstream.sni.clone(),
                    Arc::clone(cert_key),
                )
            } else {
                HttpPeer::new(
                    upstream.address.as_str(),
                    upstream.tls,
                    upstream.sni.clone(),
                )
            }
        } else {
            HttpPeer::new(
                upstream.address.as_str(),
                upstream.tls,
                upstream.sni.clone(),
            )
        };
        if !self.upstream_verify_hostname {
            peer.options.verify_hostname = false;
        }
        if let Some(timeout) = self.upstream_connect_timeout {
            peer.options.connection_timeout = Some(timeout);
        }
        if self.request_handler_active(ctx, model_provider_sidecar::SIDECAR_IDENTITY_HANDLER) {
            let (connect_ms, _, idle_ms, _, _) = model_provider_sidecar::sidecar_limits()
                .map_err(|error| pingora_internal_error(RuntimeError::Config(error)))?;
            peer.options.connection_timeout = Some(Duration::from_millis(connect_ms));
            peer.options.read_timeout = Some(Duration::from_millis(idle_ms));
            peer.options.write_timeout = Some(Duration::from_millis(idle_ms));
        }
        if ctx.websocket_decision.is_some()
            && let Some(timeout) = websocket_io_timeout(ctx)
        {
            peer.options.read_timeout = Some(timeout);
            peer.options.write_timeout = Some(timeout);
        }
        Ok(Box::new(peer))
    }

    async fn upstream_request_filter(
        &self,
        session: &mut Session,
        upstream_request: &mut pingora::http::RequestHeader,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<()> {
        if let Some(target) = ctx.proxy_target.as_ref() {
            if ctx.rewrite_host_header {
                if let Some(original_host) = request_header(session, "host") {
                    upstream_request.insert_header("x-forwarded-host", original_host)?;
                }
                upstream_request.insert_header("host", target.host_header.clone())?;
            }
            apply_forwarded_headers(
                session,
                upstream_request,
                ctx.reuse_x_forwarded,
                self.server_scheme.as_str(),
                self.server_port,
            )?;
            if let Some(decision) = ctx.websocket_decision.as_ref() {
                apply_websocket_upstream_request(
                    upstream_request,
                    decision,
                    ctx.websocket_preserve_routing_headers,
                )?;
                if let Some(handshake) = ctx.websocket_handshake.as_ref() {
                    apply_browser_websocket_upstream_credentials(
                        upstream_request,
                        handshake,
                        ctx.websocket_trusted_authorization.as_deref(),
                    )?;
                }
            } else if let Some(decision) = ctx.router_decision.as_ref() {
                let route = self.router_route.load();
                let route = route.as_ref().as_ref().ok_or_else(|| {
                    Error::explain(
                        ErrorType::InternalError,
                        "router target selected but router.yml is not loaded",
                    )
                })?;
                apply_router_upstream_request(upstream_request, route, decision, &ctx.endpoint)?;
            } else if !target.path_prefix.is_empty() {
                rewrite_upstream_path(upstream_request, &target.path_prefix)?;
            }
        }
        if ctx.access_control_active || ctx.access_control_response_active {
            upstream_request.remove_header("accept-encoding");
        }
        if self.request_handler_active(ctx, model_provider_sidecar::SIDECAR_IDENTITY_HANDLER) {
            model_provider_sidecar::apply_sidecar_upstream_headers(upstream_request)
                .await
                .map_err(|error| pingora_internal_error(RuntimeError::Config(error)))?;
        }
        strip_retired_gateway_marker(upstream_request);
        if let Some(correlation_id) = correlation_id_for_upstream(&ctx.correlation) {
            upstream_request.insert_header(light_pingora::CORRELATION_ID_HEADER, correlation_id)?;
        }
        if let Some(traceability_id) = ctx.correlation.traceability_id.as_deref() {
            upstream_request
                .insert_header(light_pingora::TRACEABILITY_ID_HEADER, traceability_id)?;
        }
        Ok(())
    }

    async fn connected_to_upstream(
        &self,
        _session: &mut Session,
        _reused: bool,
        _peer: &HttpPeer,
        #[cfg(unix)] _fd: std::os::unix::io::RawFd,
        #[cfg(windows)] _sock: std::os::windows::io::RawSocket,
        _digest: Option<&pingora::protocols::Digest>,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<()>
    where
        Self::CTX: Send + Sync,
    {
        ctx.upstream_connected_at = Some(Instant::now());
        if ctx.websocket_decision.is_some() {
            let now = Instant::now();
            ctx.websocket_connected_at = Some(now);
            ctx.websocket_last_activity = Some(now);
        }
        Ok(())
    }

    async fn request_body_filter(
        &self,
        session: &mut Session,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<()>
    where
        Self::CTX: Send + Sync,
    {
        if let Some(verified) = ctx.hmac_verified_body.as_ref() {
            if !end_of_stream || body.as_ref() != Some(verified) {
                return Err(Error::explain(
                    ErrorType::InternalError,
                    "verified HMAC body was not re-injected as one exact final chunk",
                ));
            }
        }
        if ctx.websocket_decision.is_some() && session.was_upgraded() {
            enforce_websocket_tunnel_limits(ctx, body)?;
        }
        if self.request_handler_active(ctx, model_provider_sidecar::SIDECAR_IDENTITY_HANDLER) {
            let (_, _, _, request_limit, _) = model_provider_sidecar::sidecar_limits()
                .map_err(|error| pingora_internal_error(RuntimeError::Config(error)))?;
            ctx.sidecar_request_bytes = ctx
                .sidecar_request_bytes
                .saturating_add(body.as_ref().map_or(0, Bytes::len));
            if ctx.sidecar_request_bytes > request_limit {
                return Err(Error::explain(
                    ErrorType::HTTPStatus(413),
                    "sidecar request body exceeds the generated profile limit",
                ));
            }
        }
        if ctx.tokenize_active {
            let runtime = self.pii_tokenization.load();
            let Some(runtime) = runtime.as_ref().as_ref() else {
                return Err(Error::explain(
                    ErrorType::InternalError,
                    "pii tokenization is not configured",
                ));
            };
            buffer_body_chunk(
                &mut ctx.tokenize_request_body,
                body,
                runtime.max_body_size(),
                "request",
            )?;
            if end_of_stream {
                let input = std::mem::take(&mut ctx.tokenize_request_body);
                let transformed = runtime
                    .tokenize_request_body(
                        ctx.auth.as_ref(),
                        ctx.request_path.as_str(),
                        ctx.method.as_str(),
                        input.as_slice(),
                    )
                    .await
                    .map_err(handler_rejection_error)?;
                *body = Some(Bytes::from(transformed));
            } else {
                *body = Some(Bytes::new());
            }
        }
        if ctx.access_control_active {
            buffer_body_chunk(
                &mut ctx.access_control_request_body,
                body,
                ACCESS_CONTROL_MAX_BODY_SIZE,
                "access-control request",
            )?;
            if end_of_stream {
                let input = std::mem::take(&mut ctx.access_control_request_body);
                let exchange = access_control_exchange(
                    ctx.endpoint.as_str(),
                    ctx.request_path.as_str(),
                    session.req_header().uri.query(),
                    Some(input.as_slice()),
                    ctx.auth.as_ref(),
                )
                .map_err(handler_rejection_error)?;
                if ctx.runtime_query_access_control_only {
                    if exchange.endpoint != RUNTIME_INSTANCE_QUERY_ENDPOINT {
                        *body = Some(Bytes::from(input));
                        return Ok(());
                    }
                    let runtime = self.access_control.load();
                    authorize_required_runtime_query(
                        runtime.as_ref().as_ref(),
                        &exchange,
                        &[],
                        ctx.auth.as_ref(),
                        ctx.correlation.correlation_id.as_deref(),
                    )
                    .await
                    .map_err(handler_rejection_error)?;
                    ctx.access_control_exchange = Some(exchange);
                    *body = Some(Bytes::from(input));
                    return Ok(());
                }
                let runtime = self.access_control.load();
                let Some(runtime) = runtime
                    .as_ref()
                    .as_ref()
                    .filter(|runtime| runtime.authorization_enabled())
                else {
                    *body = Some(Bytes::from(input));
                    return Ok(());
                };
                match runtime
                    .authorize_http_endpoint(
                        exchange.endpoint.as_str(),
                        &agent_headers(session),
                        ctx.auth.as_ref(),
                        &exchange.request_data,
                        ctx.correlation.correlation_id.as_deref(),
                    )
                    .await
                {
                    AccessDecision::Allowed => {
                        let has_response_filter =
                            runtime.has_response_filter(exchange.endpoint.as_str());
                        ctx.access_control_exchange = Some(exchange);
                        ctx.access_control_response_active = has_response_filter;
                        *body = Some(Bytes::from(input));
                    }
                    AccessDecision::Denied(message) => {
                        warn!(
                            endpoint = exchange.endpoint.as_str(),
                            request_path = ctx.request_path.as_str(),
                            method = ctx.method.as_str(),
                            client_id = ctx
                                .auth
                                .as_ref()
                                .and_then(|auth| auth.client_id.as_deref())
                                .unwrap_or(""),
                            user_id = ctx
                                .auth
                                .as_ref()
                                .and_then(|auth| auth.user_id.as_deref())
                                .unwrap_or(""),
                            correlation_id =
                                ctx.correlation.correlation_id.as_deref().unwrap_or(""),
                            reason = message.as_str(),
                            "access-control denied request"
                        );
                        return Err(access_control_status_error(403, message));
                    }
                }
            } else {
                *body = Some(Bytes::new());
            }
        }
        Ok(())
    }

    fn response_body_filter(
        &self,
        session: &mut Session,
        body: &mut Option<Bytes>,
        end_of_stream: bool,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<Option<Duration>>
    where
        Self::CTX: Send + Sync,
    {
        if ctx.websocket_decision.is_some() && session.was_upgraded() {
            enforce_websocket_tunnel_limits(ctx, body)?;
        }
        if self.request_handler_active(ctx, model_provider_sidecar::SIDECAR_IDENTITY_HANDLER) {
            let (_, _, _, _, response_limit) = model_provider_sidecar::sidecar_limits()
                .map_err(|error| pingora_internal_error(RuntimeError::Config(error)))?;
            ctx.sidecar_response_bytes = ctx
                .sidecar_response_bytes
                .saturating_add(body.as_ref().map_or(0, Bytes::len));
            if ctx.sidecar_response_bytes > response_limit {
                return Err(Error::explain(
                    ErrorType::HTTPStatus(502),
                    "sidecar response body exceeds the generated profile limit",
                ));
            }
        }
        if ctx.detokenize_active {
            let runtime = self.pii_tokenization.load();
            let Some(runtime) = runtime.as_ref().as_ref() else {
                return Err(Error::explain(
                    ErrorType::InternalError,
                    "pii tokenization is not configured",
                ));
            };
            buffer_body_chunk(
                &mut ctx.detokenize_response_body,
                body,
                runtime.max_body_size(),
                "response",
            )?;
            if end_of_stream {
                let input = std::mem::take(&mut ctx.detokenize_response_body);
                let transformed = block_on_detokenize_response(
                    runtime,
                    ctx.auth.as_ref(),
                    ctx.request_path.as_str(),
                    ctx.method.as_str(),
                    input.as_slice(),
                )
                .map_err(handler_rejection_error)?;
                *body = Some(Bytes::from(transformed));
            } else {
                *body = None;
            }
        }
        if ctx.access_control_response_active {
            buffer_body_chunk(
                &mut ctx.access_control_response_body,
                body,
                ACCESS_CONTROL_MAX_BODY_SIZE,
                "access-control response",
            )?;
            if end_of_stream {
                let input = std::mem::take(&mut ctx.access_control_response_body);
                let Some(exchange) = ctx.access_control_exchange.as_ref() else {
                    tracing::error!(
                        "access-control response filter is active without request context"
                    );
                    return Err(access_control_response_filter_error());
                };
                let runtime = self.access_control.load();
                let Some(runtime) = runtime.as_ref().as_ref() else {
                    tracing::error!(
                        "access-control response filter runtime became unavailable while processing the request"
                    );
                    return Err(access_control_response_filter_error());
                };
                let transformed = block_on_access_control_response(
                    runtime,
                    exchange,
                    &agent_headers(session),
                    ctx.auth.as_ref(),
                    ctx.correlation.correlation_id.as_deref(),
                    ctx.upstream_status.unwrap_or(200),
                    input.as_slice(),
                )?;
                *body = Some(Bytes::from(transformed));
            } else {
                *body = None;
            }
        }
        Ok(None)
    }

    async fn response_filter(
        &self,
        _session: &mut Session,
        upstream_response: &mut ResponseHeader,
        ctx: &mut Self::CTX,
    ) -> pingora::Result<()>
    where
        Self::CTX: Send + Sync,
    {
        let upstream_status = upstream_response.status.as_u16();
        ctx.upstream_status = Some(upstream_status);
        if matches!(ctx.hmac_replay, WebhookReplayState::Reserved { .. }) {
            if (200..300).contains(&upstream_status) {
                ctx.hmac_replay = WebhookReplayState::Committed2xx;
            } else {
                self.release_hmac_reservation(ctx).await;
            }
        }
        if self.request_handler_active(ctx, model_provider_sidecar::SIDECAR_IDENTITY_HANDLER) {
            let (_, setup_ms, _, _, _) = model_provider_sidecar::sidecar_limits()
                .map_err(|error| pingora_internal_error(RuntimeError::Config(error)))?;
            let upstream_connected_at = ctx.upstream_connected_at.ok_or_else(|| {
                pingora_internal_error(RuntimeError::Config(
                    "sidecar response arrived without an upstream connection timestamp".to_string(),
                ))
            })?;
            if upstream_connected_at.elapsed() > Duration::from_millis(setup_ms) {
                return Err(Error::explain(
                    ErrorType::HTTPStatus(504),
                    "sidecar stream setup exceeded the generated profile limit",
                ));
            }
        }
        if ctx.detokenize_active {
            if upstream_response.headers.get("content-encoding").is_some() {
                return Err(handler_rejection_error(HandlerRejection::new(
                    415,
                    "ERR13018",
                    "detokenize handler does not support encoded response bodies",
                )));
            }
            upstream_response.remove_header("content-length");
            upstream_response.remove_header("etag");
            upstream_response.remove_header("last-modified");
        }
        if ctx.access_control_response_active {
            if upstream_response.headers.get("content-encoding").is_some() {
                return Err(handler_rejection_error(HandlerRejection::new(
                    415,
                    "ERR13022",
                    "access-control handler does not support encoded response bodies",
                )));
            }
            upstream_response.remove_header("content-length");
            upstream_response.remove_header("etag");
            upstream_response.remove_header("last-modified");
        }
        if upstream_response.status.as_u16() == 101
            && let Some(handshake) = ctx.websocket_handshake.as_ref()
        {
            let upstream_selection = upstream_response
                .headers
                .get("sec-websocket-protocol")
                .and_then(|value| value.to_str().ok());
            let protocol = handshake
                .downstream_protocol(upstream_selection)
                .map_err(|error| Error::explain(ErrorType::InvalidHTTPHeader, error.to_string()))?;
            upstream_response.insert_header("sec-websocket-protocol", protocol)?;
        }
        self.apply_response_headers(upstream_response, ctx)?;
        if upstream_response.status.as_u16() >= 500 {
            self.record_upstream_failure(ctx);
        } else {
            self.record_upstream_success(ctx);
        }
        self.record_metrics(ctx, upstream_response.status.as_u16());
        self.log_handler_durations(ctx);
        Ok(())
    }

    fn fail_to_connect(
        &self,
        _session: &mut Session,
        _peer: &HttpPeer,
        ctx: &mut Self::CTX,
        e: Box<Error>,
    ) -> Box<Error> {
        self.record_upstream_failure(ctx);
        e
    }

    async fn logging(&self, _session: &mut Session, error: Option<&Error>, ctx: &mut Self::CTX)
    where
        Self::CTX: Send + Sync,
    {
        if matches!(ctx.hmac_replay, WebhookReplayState::Reserved { .. }) {
            self.release_hmac_reservation(ctx).await;
        }
        ctx.hmac_body_permit = None;
        if error.is_some() {
            self.record_metrics(ctx, 500);
        }
        if let Some(runtime) = self.gateway_evidence.as_ref()
            && !ctx.method.is_empty()
        {
            let status = ctx
                .response_status
                .or(ctx.upstream_status)
                .unwrap_or(if error.is_some() { 500 } else { 200 });
            let event_class = if matches!(status, 401 | 403 | 429) {
                EvidenceClass::RequiredAudit
            } else {
                EvidenceClass::Traffic
            };
            let event_type = match status {
                401 | 403 => "gateway.authorization.denied",
                429 => "gateway.rate_limited",
                _ => "gateway.request.completed",
            };
            let principal_digest = ctx.auth.as_ref().map(|principal| {
                sha256_digest(&format!(
                    "{}|{}|{}|{}",
                    principal.client_id.as_deref().unwrap_or(""),
                    principal.user_id.as_deref().unwrap_or(""),
                    principal.issuer.as_deref().unwrap_or(""),
                    principal.host.as_deref().unwrap_or("")
                ))
            });
            let policy_digest = ctx
                .security_execution
                .as_ref()
                .map(|snapshot| sha256_digest(&snapshot.generation.to_string()));
            let handler_digest =
                (!ctx.handler_ids.is_empty()).then(|| sha256_digest(&ctx.handler_ids.join("|")));
            let request_bytes = ctx
                .sidecar_request_bytes
                .saturating_add(ctx.hmac_verified_body.as_ref().map_or(0, Bytes::len));
            let record = EvidenceRecord {
                event_id: uuid::Uuid::now_v7(),
                event_class,
                event_type: event_type.to_string(),
                method: ctx.method.clone(),
                endpoint: if ctx.endpoint.is_empty() {
                    "<unmatched>".to_string()
                } else {
                    ctx.endpoint.clone()
                },
                status_code: status,
                duration_micros: u64::try_from(ctx.request_start.elapsed().as_micros())
                    .unwrap_or(u64::MAX),
                request_bytes: u64::try_from(request_bytes).unwrap_or(u64::MAX),
                response_bytes: u64::try_from(ctx.sidecar_response_bytes).unwrap_or(u64::MAX),
                correlation_digest: ctx.correlation.correlation_id.as_deref().map(sha256_digest),
                principal_digest,
                policy_digest,
                handler_digest,
                occurred_at: Utc::now(),
            };
            match runtime.record(&record).await {
                Ok(gateway_operational_store::AdmissionOutcome::Persisted) => {}
                Ok(gateway_operational_store::AdmissionOutcome::DroppedOptional) => {
                    warn!("optional Gateway traffic evidence was dropped at the configured bound");
                }
                Err(error) => {
                    tracing::error!(required = event_class == EvidenceClass::RequiredAudit, error = %error, "Gateway operational evidence admission failed");
                }
            }
        }
        self.log_handler_durations(ctx);
    }
}

#[derive(Debug, Default)]
struct UpstreamCircuitState {
    failures: u32,
    opened_at: Option<Instant>,
}

struct GatewayRequestContext {
    admission_permit: Option<AdmissionPermit>,
    proxy_target: Option<ProxyTarget>,
    rewrite_host_header: bool,
    reuse_x_forwarded: bool,
    router_decision: Option<RouterDecision>,
    websocket_decision: Option<WebSocketRouteDecision>,
    websocket_permit: Option<WebSocketConnectionPermit>,
    websocket_handshake: Option<WebSocketHandshake>,
    websocket_trusted_authorization: Option<String>,
    websocket_preserve_routing_headers: bool,
    websocket_idle_timeout: Option<Duration>,
    websocket_max_connection_duration: Option<Duration>,
    websocket_connected_at: Option<Instant>,
    websocket_last_activity: Option<Instant>,
    upstream_connected_at: Option<Instant>,
    request_start: Instant,
    handler_ids: Vec<String>,
    request_path: String,
    endpoint: String,
    method: String,
    path_params: BTreeMap<String, String>,
    correlation: CorrelationState,
    cors: Option<CorsResponseHeaders>,
    auth: Option<AuthPrincipal>,
    agent_delegation: Option<DelegationClaims>,
    tokenize_active: bool,
    detokenize_active: bool,
    access_control_active: bool,
    runtime_query_access_control_only: bool,
    access_control_response_active: bool,
    tokenize_request_body: Vec<u8>,
    detokenize_response_body: Vec<u8>,
    access_control_request_body: Vec<u8>,
    access_control_response_body: Vec<u8>,
    sidecar_request_bytes: usize,
    sidecar_response_bytes: usize,
    access_control_exchange: Option<AccessControlExchange>,
    upstream_status: Option<u16>,
    response_status: Option<u16>,
    rate_limit_headers: Option<RateLimitHeaders>,
    extra_response_headers: Vec<(String, String)>,
    metrics_enabled: bool,
    metrics_recorded: bool,
    handler_timings: Vec<HandlerTiming>,
    handler_timings_logged: bool,
    security_execution: Option<Arc<GatewaySecurityExecutionSnapshot>>,
    hmac_entry: Option<&'static str>,
    hmac_profile: Option<String>,
    hmac_verified_body: Option<Bytes>,
    hmac_body_permit: Option<HmacBodyPermit>,
    hmac_replay: WebhookReplayState,
}

impl Default for GatewayRequestContext {
    fn default() -> Self {
        Self {
            admission_permit: None,
            proxy_target: None,
            rewrite_host_header: false,
            reuse_x_forwarded: false,
            router_decision: None,
            websocket_decision: None,
            websocket_permit: None,
            websocket_handshake: None,
            websocket_trusted_authorization: None,
            websocket_preserve_routing_headers: false,
            websocket_idle_timeout: None,
            websocket_max_connection_duration: None,
            websocket_connected_at: None,
            websocket_last_activity: None,
            upstream_connected_at: None,
            request_start: Instant::now(),
            handler_ids: Vec::new(),
            request_path: String::new(),
            endpoint: String::new(),
            method: String::new(),
            path_params: BTreeMap::new(),
            correlation: CorrelationState::default(),
            cors: None,
            auth: None,
            agent_delegation: None,
            tokenize_active: false,
            detokenize_active: false,
            access_control_active: false,
            runtime_query_access_control_only: false,
            access_control_response_active: false,
            tokenize_request_body: Vec::new(),
            detokenize_response_body: Vec::new(),
            access_control_request_body: Vec::new(),
            access_control_response_body: Vec::new(),
            sidecar_request_bytes: 0,
            sidecar_response_bytes: 0,
            access_control_exchange: None,
            upstream_status: None,
            response_status: None,
            rate_limit_headers: None,
            extra_response_headers: Vec::new(),
            metrics_enabled: false,
            metrics_recorded: false,
            handler_timings: Vec::new(),
            handler_timings_logged: false,
            security_execution: None,
            hmac_entry: None,
            hmac_profile: None,
            hmac_verified_body: None,
            hmac_body_permit: None,
            hmac_replay: WebhookReplayState::NotRequired,
        }
    }
}

impl GatewayRequestContext {
    fn begin_request(&mut self) {
        self.admission_permit = None;
        self.proxy_target = None;
        self.rewrite_host_header = false;
        self.reuse_x_forwarded = false;
        self.router_decision = None;
        self.websocket_decision = None;
        self.websocket_permit = None;
        self.websocket_handshake = None;
        self.websocket_trusted_authorization = None;
        self.websocket_preserve_routing_headers = false;
        self.websocket_idle_timeout = None;
        self.websocket_max_connection_duration = None;
        self.websocket_connected_at = None;
        self.websocket_last_activity = None;
        self.upstream_connected_at = None;
        self.request_start = Instant::now();
        self.handler_ids.clear();
        self.request_path.clear();
        self.endpoint.clear();
        self.method.clear();
        self.path_params.clear();
        self.correlation = CorrelationState::default();
        self.cors = None;
        self.auth = None;
        self.tokenize_active = false;
        self.detokenize_active = false;
        self.access_control_active = false;
        self.access_control_response_active = false;
        self.tokenize_request_body.clear();
        self.detokenize_response_body.clear();
        self.access_control_request_body.clear();
        self.access_control_response_body.clear();
        self.sidecar_request_bytes = 0;
        self.sidecar_response_bytes = 0;
        self.access_control_exchange = None;
        self.upstream_status = None;
        self.response_status = None;
        self.rate_limit_headers = None;
        self.extra_response_headers.clear();
        self.metrics_enabled = false;
        self.metrics_recorded = false;
        self.handler_timings.clear();
        self.handler_timings_logged = false;
        self.security_execution = None;
        self.hmac_entry = None;
        self.hmac_profile = None;
        self.hmac_verified_body = None;
        self.hmac_body_permit = None;
        self.hmac_replay = WebhookReplayState::NotRequired;
    }

    fn record_handler_duration(&mut self, handler_id: &str, duration: Duration) {
        self.handler_timings.push(HandlerTiming {
            handler_id: handler_id.to_string(),
            duration,
        });
    }
}

struct HandlerTiming {
    handler_id: String,
    duration: Duration,
}

#[derive(Debug, Clone)]
struct AccessControlExchange {
    endpoint: String,
    request_data: JsonValue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum GatewayCommand {
    Start,
    ValidateConfig { local_only: bool },
    ShowLlmLiveHelp,
    ValidateLlmLive(LiveValidationOptions),
}

fn parse_gateway_command<I, S>(args: I) -> Result<GatewayCommand>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let args = args
        .into_iter()
        .map(|value| value.as_ref().to_string())
        .collect::<Vec<_>>();
    match args.as_slice() {
        [] => Ok(GatewayCommand::Start),
        [command] if command == "validate-config" => {
            Ok(GatewayCommand::ValidateConfig { local_only: true })
        }
        [command, option] if command == "validate-config" && option == "--local-only" => {
            Ok(GatewayCommand::ValidateConfig { local_only: true })
        }
        [command, option] if command == "validate-config" && option == "--with-remote" => {
            Ok(GatewayCommand::ValidateConfig { local_only: false })
        }
        [command, option]
            if command == "validate-llm-live" && matches!(option.as_str(), "--help" | "-h") =>
        {
            Ok(GatewayCommand::ShowLlmLiveHelp)
        }
        [command, options @ ..] if command == "validate-llm-live" => {
            parse_live_validation_options(options).map(GatewayCommand::ValidateLlmLive)
        }
        _ => anyhow::bail!(
            "unknown light-gateway arguments; expected no arguments, `validate-config [--local-only|--with-remote]`, or `validate-llm-live --help`"
        ),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let watcher = ShutdownWatcher::install().context("failed to install shutdown handlers")?;
    let tracing_guard = init_tracing(
        TracingOptions::new("light-gateway").with_legacy_ansi_env("GATEWAY_LOG_ANSI"),
    )?;
    if config_loader::handle_embedded_config_cli(embedded_config::FILES)? {
        return Ok(());
    }

    let command = parse_gateway_command(std::env::args().skip(1))?;
    if command == GatewayCommand::ShowLlmLiveHelp {
        println!("{}", live_validation_usage());
        return Ok(());
    }
    if let GatewayCommand::ValidateLlmLive(options) = command {
        let outcome = live_validation::validate_live(options).await;
        println!("{}", outcome.render()?);
        if outcome.exit_code != 0 {
            std::process::exit(outcome.exit_code);
        }
        return Ok(());
    }
    let cache_registry = Arc::new(CacheRegistry::new());
    let hmac_replay_admin = Arc::new(HmacReplayAdmin::default());
    let gateway_app = GatewayApp {
        hmac_replay_admin: Arc::clone(&hmac_replay_admin),
    };
    let runtime = LightRuntimeBuilder::new(PingoraTransport::new(gateway_app))
        .with_embedded_config(embedded_config::FILES)
        .with_default_config_dir(DEFAULT_CONFIG_DIR)
        .with_config_dir(CONFIG_DIR)
        .with_external_config_dir(EXTERNAL_CONFIG_DIR)
        .with_cache_registry(cache_registry)
        .with_registry_handler(Arc::new(HmacReplayRegistryHandler {
            admin: hmac_replay_admin,
        }))
        .with_logging_control(tracing_guard.logging_control())
        .with_log_stream(tracing_guard.log_stream())
        .with_optional_log_file_access(tracing_guard.log_file_access())
        .build();

    if let GatewayCommand::ValidateConfig { local_only } = command {
        let runtime_config = if local_only {
            runtime.prepare_local_config().await
        } else {
            runtime.prepare_config().await
        }
        .context("failed to load effective light-gateway configuration")?;
        let report = validate_mcp_router_runtime_config(&runtime_config)
            .context("failed to validate effective MCP configuration")?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        if !report.valid {
            anyhow::bail!("MCP configuration validation failed");
        }
        return Ok(());
    }

    runtime
        .run_until_shutdown(watcher)
        .await
        .context("light-gateway lifecycle failed")?;

    Ok(())
}

fn rewrite_upstream_path(
    upstream_request: &mut pingora::http::RequestHeader,
    path_prefix: &str,
) -> pingora::Result<()> {
    let original = upstream_request
        .uri
        .path_and_query()
        .map(|path_and_query| path_and_query.as_str())
        .unwrap_or("/");
    let (path, query) = original
        .split_once('?')
        .map_or((original, None), |(path, query)| (path, Some(query)));
    let path = if path == "/" {
        path_prefix.to_string()
    } else {
        format!("{}{}", path_prefix.trim_end_matches('/'), path)
    };
    let path_and_query = query.map_or(path.clone(), |query| format!("{path}?{query}"));
    let uri = path_and_query.parse().map_err(|error| {
        Error::because(
            ErrorType::InvalidHTTPHeader,
            format!("invalid upstream URI `{path_and_query}`"),
            error,
        )
    })?;
    upstream_request.set_uri(uri);
    Ok(())
}

fn apply_forwarded_headers(
    session: &Session,
    upstream_request: &mut pingora::http::RequestHeader,
    reuse_x_forwarded: bool,
    server_scheme: &str,
    server_port: u16,
) -> pingora::Result<()> {
    let remote = client_ip(session).unwrap_or_else(|| "unknown".to_string());
    let forwarded_for = if reuse_x_forwarded {
        upstream_request
            .headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
            .filter(|value| !value.is_empty())
            .map(|value| format!("{value},{remote}"))
            .unwrap_or(remote)
    } else {
        remote
    };
    upstream_request.insert_header("x-forwarded-for", forwarded_for)?;

    if !reuse_x_forwarded || !upstream_request.headers.contains_key("x-forwarded-proto") {
        upstream_request.insert_header("x-forwarded-proto", server_scheme.to_string())?;
    }
    if !reuse_x_forwarded || !upstream_request.headers.contains_key("x-forwarded-port") {
        upstream_request.insert_header(
            "x-forwarded-port",
            host_port(session).unwrap_or(server_port).to_string(),
        )?;
    }
    if !reuse_x_forwarded || !upstream_request.headers.contains_key("x-forwarded-server") {
        if let Some(host) = request_header(session, "host").and_then(|host| host_name(&host)) {
            upstream_request.insert_header("x-forwarded-server", host)?;
        }
    }
    Ok(())
}

fn request_header(session: &Session, name: &str) -> Option<String> {
    let header = session
        .req_header()
        .headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    if header.is_some() {
        return header;
    }
    if name.eq_ignore_ascii_case("host") {
        return session
            .req_header()
            .uri
            .authority()
            .map(|authority| authority.as_str().to_string());
    }
    None
}

fn request_cookie_value(session: &Session, name: &str) -> Option<String> {
    request_header(session, "cookie")?
        .split(';')
        .filter_map(|cookie| cookie.trim().split_once('='))
        .find(|(cookie_name, _)| cookie_name.trim() == name)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn websocket_policy_headers(session: &Session) -> Vec<(String, String)> {
    const SAFE_HEADERS: [&str; 6] = [
        "host",
        "origin",
        "user-agent",
        "x-forwarded-host",
        "x-forwarded-port",
        "x-forwarded-proto",
    ];
    SAFE_HEADERS
        .iter()
        .filter_map(|name| request_header(session, name).map(|value| ((*name).to_string(), value)))
        .collect()
}

fn log_access_control_revision(runtime: Option<&AccessControlRuntime>) {
    if let Some(runtime) = runtime {
        info!(
            policy_revision = %runtime.policy_revision(),
            enabled = runtime.authorization_enabled(),
            default_deny = runtime.default_deny(),
            "access-control policy loaded"
        );
    } else {
        info!("access-control policy is not active");
    }
}

fn strip_retired_gateway_marker(upstream_request: &mut pingora::http::RequestHeader) {
    upstream_request.remove_header("x-light-gateway");
}

fn agent_headers(session: &Session) -> Vec<(String, String)> {
    session
        .req_header()
        .headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_string(), value.to_string()))
        })
        .collect()
}

fn method_has_request_body(method: &str) -> bool {
    matches!(
        method.to_ascii_uppercase().as_str(),
        "POST" | "PUT" | "PATCH" | "DELETE"
    )
}

fn llm_access_control_ready(
    method: &str,
    body_authorization_required: bool,
    bodyless_authorization_completed: bool,
) -> bool {
    if method_has_request_body(method) {
        body_authorization_required
    } else {
        bodyless_authorization_completed
    }
}

fn access_control_exchange(
    endpoint: &str,
    request_path: &str,
    query: Option<&str>,
    body: Option<&[u8]>,
    _auth: Option<&AuthPrincipal>,
) -> Result<AccessControlExchange, HandlerRejection> {
    if is_portal_hybrid_path(request_path) {
        return portal_access_control_exchange(query, body);
    }
    let request_data = body
        .filter(|body| !body.is_empty())
        .and_then(|body| serde_json::from_slice::<JsonValue>(body).ok())
        .unwrap_or_else(|| JsonValue::Object(Default::default()));
    Ok(AccessControlExchange {
        endpoint: endpoint.to_string(),
        request_data: request_data.clone(),
    })
}

fn required_runtime_query_exchange(
    endpoint: &str,
    request_path: &str,
    query: Option<&str>,
    body: Option<&[u8]>,
) -> Result<Option<AccessControlExchange>, HandlerRejection> {
    let exchange = access_control_exchange(endpoint, request_path, query, body, None)?;
    Ok((exchange.endpoint == RUNTIME_INSTANCE_QUERY_ENDPOINT).then_some(exchange))
}

async fn authorize_required_runtime_query(
    runtime: Option<&AccessControlRuntime>,
    exchange: &AccessControlExchange,
    headers: &[(String, String)],
    auth: Option<&AuthPrincipal>,
    correlation_id: Option<&str>,
) -> Result<(), HandlerRejection> {
    let runtime = runtime
        .filter(|runtime| runtime.authorization_enabled())
        .ok_or_else(|| {
            HandlerRejection::new(
                503,
                "ERR13025",
                "runtime instance query access policy is unavailable",
            )
        })?;
    runtime
        .validate_request_policy(RUNTIME_INSTANCE_QUERY_ENDPOINT)
        .map_err(|_| {
            HandlerRejection::new(
                503,
                "ERR13025",
                "runtime instance query access policy is invalid",
            )
        })?;
    match runtime
        .authorize_http_endpoint(
            exchange.endpoint.as_str(),
            headers,
            auth,
            &exchange.request_data,
            correlation_id,
        )
        .await
    {
        AccessDecision::Allowed => Ok(()),
        AccessDecision::Denied(_) => Err(HandlerRejection::new(
            403,
            "ERR13026",
            "runtime instance query access denied",
        )),
    }
}

fn portal_access_control_exchange(
    query: Option<&str>,
    body: Option<&[u8]>,
) -> Result<AccessControlExchange, HandlerRejection> {
    let envelope = if let Some(body) = body.filter(|body| !body.is_empty()) {
        let parsed = serde_json::from_slice::<JsonValue>(body).map_err(|error| {
            HandlerRejection::new(
                400,
                "ERR13023",
                format!("invalid hybrid portal request body: {error}"),
            )
        })?;
        normalize_hybrid_body_envelope(parsed)?
    } else {
        hybrid_envelope_from_query(query)?
    };
    let host = required_text(&envelope, "host")?;
    let service = required_text(&envelope, "service")?;
    let action_name = required_text(&envelope, "action")?;
    let version = required_text(&envelope, "version")?;
    let endpoint = format!("{host}/{service}/{action_name}/{version}");
    let request_data = envelope
        .get("data")
        .cloned()
        .unwrap_or_else(|| JsonValue::Object(Default::default()));
    Ok(AccessControlExchange {
        endpoint: endpoint.clone(),
        request_data: request_data.clone(),
    })
}

fn normalize_hybrid_body_envelope(envelope: JsonValue) -> Result<JsonValue, HandlerRejection> {
    if envelope.get("host").and_then(JsonValue::as_str).is_some()
        && envelope
            .get("service")
            .and_then(JsonValue::as_str)
            .is_some()
        && envelope.get("action").and_then(JsonValue::as_str).is_some()
        && envelope
            .get("version")
            .and_then(JsonValue::as_str)
            .is_some()
    {
        return Ok(envelope);
    }

    let Some(method) = envelope.get("method").and_then(JsonValue::as_str) else {
        return Ok(envelope);
    };
    let parts: Vec<&str> = method.split('/').collect();
    if parts.len() != 4 || parts.iter().any(|part| part.trim().is_empty()) {
        return Ok(envelope);
    }

    Ok(json!({
        "host": parts[0],
        "service": parts[1],
        "action": parts[2],
        "version": parts[3],
        "data": envelope
            .get("params")
            .cloned()
            .unwrap_or_else(|| JsonValue::Object(Default::default()))
    }))
}

fn is_portal_hybrid_path(request_path: &str) -> bool {
    matches!(request_path, "/portal/query" | "/portal/command")
}

fn hybrid_envelope_from_query(query: Option<&str>) -> Result<JsonValue, HandlerRejection> {
    let mut envelope = serde_json::Map::new();
    let mut data = serde_json::Map::new();
    if let Some(query) = query {
        for (key, value) in form_urlencoded::parse(query.as_bytes()) {
            match key.as_ref() {
                "cmd" => {
                    let parsed = serde_json::from_str::<JsonValue>(&value).map_err(|error| {
                        HandlerRejection::new(
                            400,
                            "ERR13023",
                            format!("invalid hybrid portal request cmd: {error}"),
                        )
                    })?;
                    if !parsed.is_object() {
                        return Err(HandlerRejection::new(
                            400,
                            "ERR13023",
                            "invalid hybrid portal request cmd: expected JSON object",
                        ));
                    }
                    return Ok(parsed);
                }
                "host" | "service" | "action" | "version" => {
                    envelope.insert(key.into_owned(), JsonValue::String(value.into_owned()));
                }
                "data" => {
                    if let Ok(value) = serde_json::from_str::<JsonValue>(&value) {
                        envelope.insert("data".to_string(), value);
                    }
                }
                _ => {
                    data.insert(key.into_owned(), JsonValue::String(value.into_owned()));
                }
            }
        }
    }
    if !envelope.contains_key("data") {
        envelope.insert("data".to_string(), JsonValue::Object(data));
    }
    Ok(JsonValue::Object(envelope))
}

fn required_text(envelope: &JsonValue, field: &str) -> Result<String, HandlerRejection> {
    envelope
        .get(field)
        .and_then(JsonValue::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            HandlerRejection::new(
                400,
                "ERR13024",
                format!("hybrid portal request is missing `{field}`"),
            )
        })
}

fn block_on_access_control_response(
    runtime: &AccessControlRuntime,
    exchange: &AccessControlExchange,
    headers: &[(String, String)],
    auth: Option<&AuthPrincipal>,
    correlation_id: Option<&str>,
    status_code: u16,
    body: &[u8],
) -> pingora::Result<Vec<u8>> {
    let future = runtime.filter_http_response(
        exchange.endpoint.as_str(),
        headers,
        auth,
        &exchange.request_data,
        correlation_id,
        status_code,
        body,
    );
    let handle = tokio::runtime::Handle::try_current().map_err(|error| {
        tracing::error!(error = %error, "access-control response filter requires a Tokio runtime");
        access_control_response_filter_error()
    })?;
    let result = tokio::task::block_in_place(|| handle.block_on(future));
    match result {
        Ok(Some(filtered)) => Ok(filtered),
        Ok(None) => {
            tracing::error!(
                endpoint = exchange.endpoint,
                "access-control response filter became unavailable while processing the request"
            );
            Err(access_control_response_filter_error())
        }
        Err(error) => {
            tracing::error!(
                endpoint = exchange.endpoint,
                error = %error,
                "access-control response filter failed"
            );
            Err(access_control_response_filter_error())
        }
    }
}

fn access_control_response_filter_error() -> Box<Error> {
    Error::explain(
        ErrorType::HTTPStatus(500),
        "access-control response filter failed",
    )
}

fn access_control_status_error(status: u16, message: String) -> Box<Error> {
    Error::explain(ErrorType::HTTPStatus(status), message)
}

async fn read_bounded_request_body(
    session: &mut Session,
    limit: usize,
) -> pingora::Result<Option<Vec<u8>>> {
    let mut body = Vec::new();
    while let Some(chunk) = session.read_request_body().await? {
        if chunk.len() > limit.saturating_sub(body.len()) {
            return Ok(None);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Some(body))
}

enum BoundedBodyRead {
    Complete(Vec<u8>),
    TooLarge,
    TooSlow,
}

async fn read_bounded_request_body_with_rate(
    session: &mut Session,
    limit: usize,
    timeout: Duration,
    minimum_bytes_per_second: u64,
) -> pingora::Result<BoundedBodyRead> {
    let started = tokio::time::Instant::now();
    let deadline = started + timeout;
    let mut body = Vec::new();
    loop {
        let chunk = match tokio::time::timeout_at(deadline, session.read_request_body()).await {
            Ok(chunk) => chunk?,
            Err(_) => return Ok(BoundedBodyRead::TooSlow),
        };
        let Some(chunk) = chunk else { break };
        if chunk.len() > limit.saturating_sub(body.len()) {
            return Ok(BoundedBodyRead::TooLarge);
        }
        body.extend_from_slice(&chunk);
        let elapsed = started.elapsed();
        if elapsed >= Duration::from_secs(1)
            && (body.len() as u128).saturating_mul(1_000_000_000)
                < (minimum_bytes_per_second as u128).saturating_mul(elapsed.as_nanos())
        {
            return Ok(BoundedBodyRead::TooSlow);
        }
    }
    Ok(BoundedBodyRead::Complete(body))
}

fn static_method_allowed(session: &Session) -> bool {
    matches!(
        session.req_header().method.as_str(),
        method if method.eq_ignore_ascii_case("GET") || method.eq_ignore_ascii_case("HEAD")
    )
}

fn is_head_request(session: &Session) -> bool {
    session
        .req_header()
        .method
        .as_str()
        .eq_ignore_ascii_case("HEAD")
}

fn is_websocket_upgrade(session: &Session) -> bool {
    session
        .req_header()
        .method
        .as_str()
        .eq_ignore_ascii_case("GET")
        && header_contains_token(session, "connection", "upgrade")
        && header_contains_token(session, "upgrade", "websocket")
        && request_header(session, "sec-websocket-key")
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
}

fn header_contains_token(session: &Session, name: &str, token: &str) -> bool {
    session
        .req_header()
        .headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(|value| {
            value
                .split(',')
                .any(|part| part.trim().eq_ignore_ascii_case(token))
        })
}

fn websocket_route_status(error: &WebSocketRouteError) -> u16 {
    match error {
        WebSocketRouteError::MissingTarget => 403,
        WebSocketRouteError::InvalidProtocol(_)
        | WebSocketRouteError::InvalidProtocolOffer
        | WebSocketRouteError::InvalidCsrfProtocol => 400,
        WebSocketRouteError::MissingOrigin | WebSocketRouteError::OriginDenied => 403,
        WebSocketRouteError::UnofferedUpstreamProtocol => 502,
        WebSocketRouteError::UpgradeRateExceeded(_) => 429,
        WebSocketRouteError::TooManyActiveConnections(_) => 503,
        WebSocketRouteError::DiscoveryUnavailable(_)
        | WebSocketRouteError::DiscoveryFailed(_)
        | WebSocketRouteError::NoUsableEndpoint(_) => 502,
    }
}

fn websocket_io_timeout(ctx: &GatewayRequestContext) -> Option<Duration> {
    match (
        ctx.websocket_idle_timeout,
        ctx.websocket_max_connection_duration,
    ) {
        (Some(idle), Some(max_duration)) => Some(idle.min(max_duration)),
        (Some(idle), None) => Some(idle),
        (None, Some(max_duration)) => Some(max_duration),
        (None, None) => None,
    }
}

fn enforce_websocket_tunnel_limits(
    ctx: &mut GatewayRequestContext,
    body: &Option<Bytes>,
) -> pingora::Result<()> {
    let now = Instant::now();
    if let Some(max_duration) = ctx.websocket_max_connection_duration {
        let started = ctx.websocket_connected_at.unwrap_or(ctx.request_start);
        if now.duration_since(started) > max_duration {
            return Err(Error::explain(
                ErrorType::ReadTimedout,
                "websocket connection exceeded max duration",
            ));
        }
    }
    if let Some(idle_timeout) = ctx.websocket_idle_timeout
        && let Some(last_activity) = ctx.websocket_last_activity
        && now.duration_since(last_activity) > idle_timeout
    {
        return Err(Error::explain(
            ErrorType::ReadTimedout,
            "websocket connection exceeded idle timeout",
        ));
    }
    if body.as_ref().is_some_and(|body| !body.is_empty()) {
        ctx.websocket_last_activity = Some(now);
    }
    Ok(())
}

fn buffer_body_chunk(
    buffer: &mut Vec<u8>,
    body: &mut Option<Bytes>,
    max_body_size: usize,
    label: &str,
) -> pingora::Result<()> {
    if let Some(chunk) = body.take() {
        if buffer.len().saturating_add(chunk.len()) > max_body_size {
            return Err(handler_rejection_error(HandlerRejection::new(
                413,
                "ERR13019",
                format!("PII tokenization {label} body exceeds maxBodySize"),
            )));
        }
        buffer.extend_from_slice(&chunk);
    }
    Ok(())
}

fn block_on_detokenize_response(
    runtime: &PiiTokenizationRuntime,
    auth: Option<&AuthPrincipal>,
    path: &str,
    method: &str,
    body: &[u8],
) -> Result<Vec<u8>, HandlerRejection> {
    let future = runtime.detokenize_response_body(auth, path, method, body);
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        tokio::task::block_in_place(|| handle.block_on(future))
    } else {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| HandlerRejection::new(500, "ERR13020", "failed to create runtime"))?
            .block_on(future)
    }
}

#[derive(Debug, Clone)]
struct StaticFileValidators {
    etag: String,
    last_modified: Option<String>,
    last_modified_time: Option<SystemTime>,
}

fn static_file_validators(metadata: &std::fs::Metadata) -> StaticFileValidators {
    let modified = metadata.modified().ok();
    StaticFileValidators {
        etag: static_etag(metadata.len(), modified),
        last_modified: modified.map(format_http_date),
        last_modified_time: modified,
    }
}

fn static_etag(length: u64, modified: Option<SystemTime>) -> String {
    let (seconds, nanos) = modified
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| (duration.as_secs(), duration.subsec_nanos()))
        .unwrap_or((0, 0));
    format!("W/\"{length:x}-{seconds:x}-{nanos:x}\"")
}

fn format_http_date(time: SystemTime) -> String {
    let datetime: DateTime<Utc> = time.into();
    datetime.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

fn parse_http_date(value: &str) -> Option<SystemTime> {
    let parsed = DateTime::parse_from_rfc2822(value).ok()?;
    let utc = parsed.with_timezone(&Utc);
    let seconds = u64::try_from(utc.timestamp()).ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(seconds))
}

fn static_request_not_modified(session: &Session, validators: &StaticFileValidators) -> bool {
    if let Some(if_none_match) = request_header(session, "if-none-match") {
        return etag_header_matches(if_none_match.as_str(), validators.etag.as_str());
    }

    let Some(modified) = validators.last_modified_time else {
        return false;
    };
    request_header(session, "if-modified-since")
        .as_deref()
        .and_then(parse_http_date)
        .is_some_and(|since| same_or_after_http_second(since, modified))
}

fn etag_header_matches(header: &str, etag: &str) -> bool {
    header.split(',').any(|candidate| {
        let candidate = candidate.trim();
        candidate == "*" || candidate == etag || weak_etag_value(candidate) == weak_etag_value(etag)
    })
}

fn weak_etag_value(value: &str) -> &str {
    value.strip_prefix("W/").unwrap_or(value)
}

fn same_or_after_http_second(candidate: SystemTime, modified: SystemTime) -> bool {
    let Some(candidate_seconds) = unix_seconds(candidate) else {
        return false;
    };
    let Some(modified_seconds) = unix_seconds(modified) else {
        return false;
    };
    candidate_seconds >= modified_seconds
}

fn unix_seconds(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs())
}

fn should_stream_static_file(file_size: u64, transfer_min_size: u64) -> bool {
    file_size >= transfer_min_size
}

fn insert_static_validators(
    response: &mut ResponseHeader,
    validators: &StaticFileValidators,
) -> pingora::Result<()> {
    response.insert_header("etag", validators.etag.as_str())?;
    if let Some(last_modified) = validators.last_modified.as_deref() {
        response.insert_header("last-modified", last_modified)?;
    }
    Ok(())
}

fn client_ip(session: &Session) -> Option<String> {
    session.as_downstream().client_addr().map(|address| {
        address
            .as_inet()
            .map(|address| address.ip().to_string())
            .unwrap_or_else(|| address.to_string())
    })
}

fn host_port(session: &Session) -> Option<u16> {
    request_header(session, "host").and_then(|host| {
        let host = host.split(',').next().unwrap_or(host.as_str()).trim();
        if host.starts_with('[') {
            return host
                .rsplit_once("]:")
                .and_then(|(_, port)| port.parse::<u16>().ok());
        }
        host.rsplit_once(':')
            .and_then(|(_, port)| port.parse::<u16>().ok())
    })
}

fn host_name(host: &str) -> Option<String> {
    let host = host.split(',').next().unwrap_or(host).trim();
    if host.is_empty() {
        return None;
    }
    if host.starts_with('[') {
        return host
            .strip_prefix('[')
            .and_then(|value| value.split_once(']'))
            .map(|(host, _)| host.to_string());
    }
    Some(
        host.rsplit_once(':')
            .map(|(host, _)| host)
            .unwrap_or(host)
            .to_string(),
    )
}

fn pingora_internal_error(error: RuntimeError) -> Box<Error> {
    Error::because(ErrorType::InternalError, error.to_string(), error)
}

fn handler_rejection_error(rejection: HandlerRejection) -> Box<Error> {
    Error::explain(
        ErrorType::InternalError,
        format!("{}: {}", rejection.code, rejection.message),
    )
}

fn handler_active(active_handlers: &ActiveHandlerSet, ids: &[&str]) -> bool {
    ids.iter().any(|id| active_handlers.is_handler_active(id))
}

fn validate_hmac_chain(
    location: &str,
    chain: &[String],
    standalone_profile: Option<&str>,
    composed_profile: Option<&str>,
) -> Result<(), RuntimeError> {
    let hmac_positions = chain
        .iter()
        .enumerate()
        .filter(|(_, id)| id.as_str() == "hmac")
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let unified_positions = chain
        .iter()
        .enumerate()
        .filter(|(_, id)| matches!(id.as_str(), "unified-security" | "unified"))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if hmac_positions.len() > 1 || unified_positions.len() > 1 {
        return Err(RuntimeError::Config(format!(
            "HMAC route `{location}` has a duplicate authentication entry point"
        )));
    }
    if !hmac_positions.is_empty() && !unified_positions.is_empty() {
        return Err(RuntimeError::Config(format!(
            "HMAC route `{location}` cannot execute both hmac and unified-security"
        )));
    }
    if standalone_profile.is_some() != (hmac_positions.len() == 1) {
        return Err(RuntimeError::Config(format!(
            "standalone HMAC policy and effective handler chain disagree for `{location}`"
        )));
    }
    if composed_profile.is_some() && unified_positions.len() != 1 {
        return Err(RuntimeError::Config(format!(
            "composed HMAC policy for `{location}` requires exactly one unified-security handler"
        )));
    }
    if standalone_profile.is_some() || composed_profile.is_some() {
        let entry = hmac_positions
            .first()
            .copied()
            .or_else(|| unified_positions.first().copied())
            .expect("validated HMAC entry point");
        let router = chain.iter().position(|id| id == "router").ok_or_else(|| {
            RuntimeError::Config(format!(
                "HMAC route `{location}` must use a proxy/router chain"
            ))
        })?;
        if entry >= router {
            return Err(RuntimeError::Config(format!(
                "HMAC authentication must precede router for `{location}`"
            )));
        }
    }
    Ok(())
}

fn validate_hmac_effective_chains(
    active_handlers: &ActiveHandlerSet,
    hmac: Option<&HmacRuntime>,
    unified: Option<&UnifiedSecurityConfig>,
) -> Result<(), RuntimeError> {
    let representative_methods = [
        "GET", "POST", "PUT", "DELETE", "PATCH", "OPTIONS", "HEAD", "TRACE", "CONNECT",
    ]
    .into_iter()
    .map(str::to_string)
    .chain(
        active_handlers
            .config()
            .paths
            .iter()
            .map(|path| path.method.to_ascii_uppercase()),
    )
    .collect::<BTreeSet<_>>();
    if let Some(runtime) = hmac {
        for rule in runtime.standalone_routes() {
            let methods = if rule.methods.is_empty() {
                representative_methods.iter().cloned().collect::<Vec<_>>()
            } else {
                rule.methods
            };
            for method in methods {
                let chain = active_handlers.resolve_handler_ids(rule.prefix.as_str(), &method)?;
                validate_hmac_chain(
                    format!("{}@{}", rule.prefix, method.to_ascii_lowercase()).as_str(),
                    &chain,
                    Some(rule.profile.as_str()),
                    unified
                        .and_then(|config| config.hmac_profile_for(rule.prefix.as_str(), &method)),
                )?;
            }
        }
    }
    if let Some(config) = unified {
        for rule in &config.path_prefix_auths {
            let methods = if rule.methods.is_empty() {
                representative_methods.iter().cloned().collect()
            } else {
                rule.methods.clone()
            };
            for method in methods {
                let Some(profile) = config.hmac_profile_for(rule.prefix.as_str(), &method) else {
                    continue;
                };
                let chain = active_handlers.resolve_handler_ids(rule.prefix.as_str(), &method)?;
                validate_hmac_chain(
                    format!("{}@{}", rule.prefix, method.to_ascii_lowercase()).as_str(),
                    &chain,
                    hmac.and_then(|runtime| {
                        runtime.standalone_profile(rule.prefix.as_str(), &method)
                    }),
                    Some(profile),
                )?;
            }
        }
    }
    for path in &active_handlers.config().paths {
        let request_path = hmac_handler_path_probe(path.path.as_str());
        let chain = active_handlers.materialized_path_handler_ids(path)?;
        let standalone_profile =
            hmac.and_then(|runtime| runtime.standalone_profile(&request_path, &path.method));
        let composed_profile =
            unified.and_then(|config| config.hmac_profile_for(&request_path, &path.method));
        if standalone_profile.is_some()
            || composed_profile.is_some()
            || chain.iter().any(|id| id == "hmac")
        {
            validate_hmac_chain(
                format!("{}@{}", path.path, path.method.to_ascii_lowercase()).as_str(),
                &chain,
                standalone_profile,
                composed_profile,
            )?;
        }
    }
    let default_chain = active_handlers.materialized_default_handler_ids()?;
    for method in &representative_methods {
        let chain = &default_chain;
        if chain.iter().any(|id| id == "hmac") {
            validate_hmac_chain(
                format!("default@{}", method.to_ascii_lowercase()).as_str(),
                chain,
                hmac.and_then(|runtime| runtime.standalone_profile("/", method)),
                unified.and_then(|config| config.hmac_profile_for("/", method)),
            )?;
        }
    }
    Ok(())
}

fn hmac_handler_path_probe(path: &str) -> String {
    path.split('/')
        .map(|segment| {
            if segment == "*" {
                "__hmac_wildcard_probe__"
            } else if segment.starts_with('{') && segment.ends_with('}') {
                "__hmac_parameter_probe__"
            } else {
                segment
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn upstream_verify_hostname(config: &RuntimeConfig) -> bool {
    config
        .client
        .as_ref()
        .map(|client| client.tls.verify_hostname)
        .unwrap_or(true)
}

fn upstream_connect_timeout(config: &RuntimeConfig) -> Option<Duration> {
    config
        .client
        .as_ref()
        .and_then(|client| duration_from_millis(client.request.connect_timeout))
}

fn duration_from_millis(value: u64) -> Option<Duration> {
    (value > 0).then(|| Duration::from_millis(value))
}

fn upstream_circuit_config(config: &RuntimeConfig) -> (u32, Duration) {
    config
        .client
        .as_ref()
        .map(|client| {
            (
                client.request.error_threshold,
                Duration::from_millis(client.request.reset_timeout),
            )
        })
        .unwrap_or((0, Duration::ZERO))
}

fn upstream_circuit_key(target: &ProxyTarget) -> String {
    target.address.clone()
}

fn upstream_client_cert_key(config: &RuntimeConfig) -> Result<Option<Arc<CertKey>>, RuntimeError> {
    let Some(client) = config.client.as_ref() else {
        return Ok(None);
    };
    let client_cert_path = client
        .tls
        .client_cert_path
        .as_ref()
        .filter(|path| !path.as_os_str().is_empty());
    let client_key_path = client
        .tls
        .client_key_path
        .as_ref()
        .filter(|path| !path.as_os_str().is_empty());

    let (Some(client_cert_path), Some(client_key_path)) = (client_cert_path, client_key_path)
    else {
        if client_cert_path.is_some() || client_key_path.is_some() {
            return Err(RuntimeError::Unsupported(
                "client TLS identity requires both tls.clientCertPath and tls.clientKeyPath"
                    .to_string(),
            ));
        }
        return Ok(None);
    };

    let cert_file = std::fs::File::open(client_cert_path)?;
    let mut cert_reader = BufReader::new(cert_file);
    let certificates = rustls_pemfile::certs(&mut cert_reader)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|certificate| certificate.as_ref().to_vec())
        .collect::<Vec<_>>();
    if certificates.is_empty() {
        return Err(RuntimeError::Unsupported(format!(
            "client TLS certificate `{}` contains no certificates",
            client_cert_path.display()
        )));
    }

    let key_file = std::fs::File::open(client_key_path)?;
    let mut key_reader = BufReader::new(key_file);
    let Some(key) = rustls_pemfile::private_key(&mut key_reader)? else {
        return Err(RuntimeError::Unsupported(format!(
            "client TLS key `{}` contains no private key",
            client_key_path.display()
        )));
    };
    let key = key.secret_der().to_vec();
    let cert_key = std::panic::catch_unwind(|| CertKey::new(certificates, key)).map_err(|_| {
        RuntimeError::Unsupported(format!(
            "invalid client TLS identity cert=`{}` key=`{}`",
            client_cert_path.display(),
            client_key_path.display()
        ))
    })?;
    Ok(Some(Arc::new(cert_key)))
}

fn load_mcp_router_runtime_preserving_state(
    runtime_config: &RuntimeConfig,
    active: bool,
    current: &ConfigManager<Option<McpRouterRuntime>>,
) -> Result<Option<McpRouterRuntime>, RuntimeError> {
    let previous = current.load();
    let mut runtime = match load_mcp_router_runtime(runtime_config, active) {
        Ok(runtime) => runtime,
        Err(error) => {
            record_mcp_router_reload_rejection(previous.as_ref().is_some());
            return Err(error);
        }
    };
    if let Some(runtime) = runtime.as_mut()
        && let Some(previous) = previous.as_ref().as_ref()
    {
        runtime.preserve_state_from(previous);
    }
    Ok(runtime)
}

fn store_mcp_reload(
    current: &ConfigManager<Option<McpRouterRuntime>>,
    candidate: Option<McpRouterRuntime>,
) {
    let previous = current.load();
    let active = current.store(candidate);
    if let Some(active) = active.as_ref() {
        active.activate_reload();
    } else if let Some(previous) = previous.as_ref() {
        previous.shutdown_subscriptions();
    }
}

fn load_websocket_router_runtime_preserving_state(
    runtime_config: &RuntimeConfig,
    active: bool,
    access_control: Option<&AccessControlRuntime>,
    current: &ConfigManager<Option<WebSocketRouterRuntime>>,
) -> Result<Option<WebSocketRouterRuntime>, RuntimeError> {
    let previous = current.load();
    let mut runtime = load_websocket_router_runtime_with_policy(
        runtime_config,
        active,
        access_control.cloned().map(Arc::new),
    )?;
    if let Some(runtime) = runtime.as_mut()
        && let Some(previous) = previous.as_ref().as_ref()
    {
        runtime.preserve_state_from(previous);
    }
    Ok(runtime)
}

struct RegisteredGatewayHandler {
    id: &'static str,
}

impl PingoraHandler for RegisteredGatewayHandler {
    fn id(&self) -> &'static str {
        self.id
    }
}

fn gateway_handler_registry() -> PingoraHandlerRegistry {
    let mut registry = PingoraHandlerRegistry::new();
    for (id, kind) in GATEWAY_HANDLER_DESCRIPTORS {
        registry = registry.register(gateway_handler_descriptor(id, *kind));
    }
    registry
}

const GATEWAY_HANDLER_DESCRIPTORS: &[(&str, PingoraHandlerKind)] = &[
    ("exception", PingoraHandlerKind::Core),
    ("metrics", PingoraHandlerKind::Observability),
    ("correlation", PingoraHandlerKind::Observability),
    ("cors", PingoraHandlerKind::Traffic),
    ("specification", PingoraHandlerKind::Security),
    ("security", PingoraHandlerKind::Security),
    ("jwt", PingoraHandlerKind::Security),
    ("api-key", PingoraHandlerKind::Security),
    ("apikey", PingoraHandlerKind::Security),
    ("basic-auth", PingoraHandlerKind::Security),
    ("basic", PingoraHandlerKind::Security),
    ("unified-security", PingoraHandlerKind::Security),
    ("unified", PingoraHandlerKind::Security),
    ("hmac", PingoraHandlerKind::Security),
    ("access-control", PingoraHandlerKind::Security),
    ("body", PingoraHandlerKind::Traffic),
    ("audit", PingoraHandlerKind::Observability),
    ("sanitizer", PingoraHandlerKind::Security),
    ("validator", PingoraHandlerKind::Security),
    ("header", PingoraHandlerKind::Traffic),
    ("headers", PingoraHandlerKind::Traffic),
    ("limit", PingoraHandlerKind::Traffic),
    ("rate-limit", PingoraHandlerKind::Traffic),
    ("request-size-limit", PingoraHandlerKind::Traffic),
    ("prefix", PingoraHandlerKind::Traffic),
    ("path-prefix-service", PingoraHandlerKind::Traffic),
    ("pathPrefixService", PingoraHandlerKind::Traffic),
    ("token", PingoraHandlerKind::Security),
    ("tokenize", PingoraHandlerKind::Traffic),
    ("detokenize", PingoraHandlerKind::Traffic),
    ("router", PingoraHandlerKind::Traffic),
    ("proxy", PingoraHandlerKind::Traffic),
    ("proxyServerInfo", PingoraHandlerKind::Application),
    ("virtual", PingoraHandlerKind::Application),
    ("path-resource", PingoraHandlerKind::Application),
    ("resource", PingoraHandlerKind::Application),
    ("killapp", PingoraHandlerKind::Application),
    ("latency", PingoraHandlerKind::Application),
    ("memory", PingoraHandlerKind::Application),
    ("exchaos", PingoraHandlerKind::Application),
    ("chaosget", PingoraHandlerKind::Application),
    ("chaospost", PingoraHandlerKind::Application),
    ("health", PingoraHandlerKind::Application),
    ("sidecar-deny", PingoraHandlerKind::Security),
    ("sidecar-identity", PingoraHandlerKind::Application),
    ("info", PingoraHandlerKind::Application),
    ("getLogger", PingoraHandlerKind::Application),
    ("postLogger", PingoraHandlerKind::Application),
    ("getLogContents", PingoraHandlerKind::Application),
    ("modules", PingoraHandlerKind::Application),
    ("configReload", PingoraHandlerKind::Application),
    ("spec", PingoraHandlerKind::Application),
    ("swaggerui", PingoraHandlerKind::Application),
    ("favicon", PingoraHandlerKind::Application),
    ("oauth", PingoraHandlerKind::Application),
    ("getOauth", PingoraHandlerKind::Application),
    ("shutdown", PingoraHandlerKind::Application),
    ("stateless", PingoraHandlerKind::Security),
    ("google", PingoraHandlerKind::Security),
    ("facebook", PingoraHandlerKind::Security),
    ("github", PingoraHandlerKind::Security),
    ("msal-exchange", PingoraHandlerKind::Security),
    ("msal-auth", PingoraHandlerKind::Security),
    ("websocket", PingoraHandlerKind::Traffic),
    ("mcp", PingoraHandlerKind::Application),
    ("llm", PingoraHandlerKind::Application),
];

fn gateway_handler_descriptor(
    id: &'static str,
    kind: PingoraHandlerKind,
) -> PingoraHandlerDescriptor {
    PingoraHandlerDescriptor {
        id,
        kind,
        factory: build_registered_gateway_handler,
    }
}

fn build_registered_gateway_handler(
    ctx: &HandlerBuildContext<'_>,
) -> Result<Arc<dyn PingoraHandler>, RuntimeError> {
    let id: &'static str = Box::leak(ctx.handler_id.to_string().into_boxed_str());
    Ok(Arc::new(RegisteredGatewayHandler { id }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use light_runtime::config::ClientConfig;
    use light_runtime::{
        BootstrapConfig, DirectRegistryConfig, ModuleRegistry, PortalRegistryConfig, ServerConfig,
        ServiceIdentity,
    };
    use portal_registry::{
        PortalRegistryClient, RegistrationState, RegistryHandler, ServiceRegistrationParams,
    };
    use serde_json::{Value as JsonValue, json};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::oneshot;
    use tokio::time::{Duration as TokioDuration, sleep, timeout};
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    use tokio_tungstenite::tungstenite::handshake::server::{
        Request as WsServerRequest, Response as WsServerResponse,
    };
    use tokio_tungstenite::tungstenite::http::HeaderValue;
    use tokio_tungstenite::tungstenite::protocol::Message;
    use tokio_tungstenite::{accept_async, accept_hdr_async, connect_async};

    #[test]
    fn gateway_command_rejects_unknown_positional_arguments() {
        assert_eq!(
            parse_gateway_command(std::iter::empty::<&str>()).expect("server command"),
            GatewayCommand::Start
        );
        assert_eq!(
            parse_gateway_command(["validate-config"]).expect("local validation command"),
            GatewayCommand::ValidateConfig { local_only: true }
        );
        assert_eq!(
            parse_gateway_command(["validate-config", "--with-remote"])
                .expect("remote validation command"),
            GatewayCommand::ValidateConfig { local_only: false }
        );
        let live = parse_gateway_command([
            "validate-llm-live",
            "--gateway-url",
            "https://gateway.example.com",
            "--operation",
            "responses",
            "--alias",
            "public-model",
            "--header-file",
            "/protected/gateway-header",
            "--timeout-seconds",
            "30",
        ])
        .expect("live validation command");
        assert!(matches!(live, GatewayCommand::ValidateLlmLive(_)));
        assert_eq!(
            parse_gateway_command(["validate-llm-live", "--help"]).expect("live help command"),
            GatewayCommand::ShowLlmLiveHelp
        );
        assert!(parse_gateway_command(["validate-cfg"]).is_err());
        assert!(parse_gateway_command(["validate-config", "--unknown"]).is_err());
        assert!(parse_gateway_command(["validate-llm-live", "--input", "secret"]).is_err());
    }

    #[test]
    fn rejected_mcp_candidate_records_whether_last_known_good_was_retained() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        std::fs::write(
            config_dir.path().join("mcp-router.yml"),
            r#"
enabled: true
tools:
  - name: broken
    targetHost: https://example.com
    inputSchema:
      type: object
      $ref: '#/$defs/missing'
"#,
        )
        .expect("invalid MCP config fixture");
        let config = runtime_config(&config_dir, &external_dir, HashMap::new());

        let current = ConfigManager::new(Some(
            McpRouterRuntime::new(Default::default()).expect("last-known-good MCP runtime"),
        ));
        let before = current
            .load()
            .as_ref()
            .as_ref()
            .expect("active runtime")
            .schema_metrics();
        assert!(load_mcp_router_runtime_preserving_state(&config, true, &current).is_err());
        let retained = current
            .load()
            .as_ref()
            .as_ref()
            .expect("failed candidate must not replace the active runtime")
            .schema_metrics();
        assert!(retained.router_reload_rejected > before.router_reload_rejected);
        assert!(retained.router_last_known_good_retained > before.router_last_known_good_retained);

        let empty = ConfigManager::new(None);
        let before_without_previous = current
            .load()
            .as_ref()
            .as_ref()
            .expect("active runtime")
            .schema_metrics();
        assert!(load_mcp_router_runtime_preserving_state(&config, true, &empty).is_err());
        assert!(empty.load().as_ref().is_none());
        let without_previous = current
            .load()
            .as_ref()
            .as_ref()
            .expect("active runtime")
            .schema_metrics();
        assert!(without_previous.router_reload_rejected > retained.router_reload_rejected);
        assert_eq!(
            without_previous
                .router_last_known_good_retained
                .saturating_sub(before_without_previous.router_last_known_good_retained),
            0,
            "a rejected candidate cannot retain a runtime that did not exist"
        );
    }

    #[test]
    fn llm_handler_requires_body_aware_access_control_proof() {
        assert!(!llm_access_control_ready("POST", false, false));
        assert!(llm_access_control_ready("POST", true, false));
        assert!(!llm_access_control_ready("GET", false, false));
        assert!(llm_access_control_ready("GET", false, true));
    }

    fn runtime_config(
        config_dir: &TempDir,
        external_config_dir: &TempDir,
        resolved_values: HashMap<String, serde_yaml::Value>,
    ) -> RuntimeConfig {
        let client = [
            external_config_dir.path().join(light_pingora::CLIENT_FILE),
            config_dir.path().join(light_pingora::CLIENT_FILE),
        ]
        .into_iter()
        .find_map(|path| {
            std::fs::read_to_string(path)
                .ok()
                .and_then(|content| serde_yaml::from_str::<ClientConfig>(&content).ok())
        });

        RuntimeConfig {
            bootstrap: BootstrapConfig::default(),
            server: ServerConfig::default(),
            client,
            portal_registry: None::<PortalRegistryConfig>,
            direct_registry: DirectRegistryConfig::default(),
            service_identity: ServiceIdentity::default(),
            config_dir: config_dir.path().to_path_buf(),
            external_config_dir: external_config_dir.path().to_path_buf(),
            resolved_values,
            default_config_dir: None,
            embedded_config: &[],
            module_registry: Arc::new(ModuleRegistry::new()),
            cache_registry: None,
            registry_client: None,
        }
    }

    #[test]
    fn llm_handler_is_registered_but_disabled_path_does_no_config_or_secret_work() {
        assert!(gateway_handler_registry().contains("llm"));
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        let config = runtime_config(&config_dir, &external_dir, HashMap::new());
        let module = load_llm_gateway_module(&config, false, 1, None)
            .expect("inactive LLM handler must not require llm-router.yml");
        assert!(module.is_none());
    }

    #[tokio::test]
    async fn llm_module_reload_publishes_a_new_immutable_values_snapshot() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        let config = runtime_config(&config_dir, &external_dir, HashMap::new());
        let path = external_dir.path().join(LLM_ROUTER_FILE);
        std::fs::write(&path, "enabled: true\nopenaiExtensionAllowlist: [first]\n")
            .expect("write initial llm-router config");

        let first = load_llm_gateway_module(&config, true, 1, None)
            .expect("compile initial values-backed configuration")
            .expect("active LLM module");
        let first_digest = first.runtime.snapshot().digest.clone();

        std::fs::write(&path, "enabled: true\nopenaiExtensionAllowlist: [second]\n")
            .expect("write replacement llm-router config");
        let second = load_llm_gateway_module(&config, true, 2, Some(&first))
            .expect("compile replacement values-backed configuration")
            .expect("reloaded LLM module");
        let published = second.runtime.snapshot();

        assert!(Arc::ptr_eq(&first.runtime, &second.runtime));
        assert_eq!(published.generation, 2);
        assert_ne!(published.digest, first_digest);
        stop_llm_background_tasks(Some(&second));
    }

    #[tokio::test]
    async fn rejected_llm_module_reload_retains_last_known_good_snapshot() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        let config = runtime_config(&config_dir, &external_dir, HashMap::new());
        let path = external_dir.path().join(LLM_ROUTER_FILE);
        std::fs::write(&path, "enabled: true\n").expect("write initial llm-router config");

        let active = load_llm_gateway_module(&config, true, 1, None)
            .expect("compile initial values-backed configuration")
            .expect("active LLM module");
        let before = active.runtime.snapshot();

        std::fs::write(&path, "enabled: true\nglobalConcurrency: 0\n")
            .expect("write invalid replacement llm-router config");
        assert!(load_llm_gateway_module(&config, true, 2, Some(&active)).is_err());

        let retained = active.runtime.snapshot();
        assert_eq!(retained.generation, before.generation);
        assert_eq!(retained.digest, before.digest);
        stop_llm_background_tasks(Some(&active));
    }

    #[test]
    fn active_response_filter_without_tokio_runtime_fails_closed() {
        let runtime = AccessControlRuntime::new(
            Some(light_pingora::AccessControlConfig::default()),
            light_pingora::RuleFileConfig::default(),
        );
        let exchange = AccessControlExchange {
            endpoint: "/v1/accounts@get".to_string(),
            request_data: json!({}),
        };

        let error = block_on_access_control_response(
            &runtime,
            &exchange,
            &[],
            None,
            None,
            200,
            br#"[{"accountNo":"1","ssn":"secret"}]"#,
        )
        .expect_err("response filter must not create a fallback runtime");

        assert!(
            error
                .to_string()
                .contains("access-control response filter failed")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn active_response_filter_fails_when_filter_becomes_unavailable() {
        let runtime = AccessControlRuntime::new(
            Some(light_pingora::AccessControlConfig::default()),
            light_pingora::RuleFileConfig::default(),
        );
        let exchange = AccessControlExchange {
            endpoint: "/v1/accounts@get".to_string(),
            request_data: json!({}),
        };

        let result = block_on_access_control_response(
            &runtime,
            &exchange,
            &[],
            None,
            None,
            200,
            br#"[{"accountNo":"1","ssn":"secret"}]"#,
        );

        let error = result.expect_err("active filter must fail closed");
        assert!(
            error
                .to_string()
                .contains("access-control response filter failed")
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn active_response_filter_propagates_filter_failure() {
        let rules = serde_yaml::from_str::<light_pingora::RuleFileConfig>(
            r#"
ruleBodies:
  filter:
    common: Y
    ruleId: filter
    ruleName: Filter
    ruleType: res-fil
    expression: "true"
    conditionLanguage: cel
    conditionSecurityProfile: strict
endpointRules:
  /v1/accounts@get:
    res-fil:
      - filter
"#,
        )
        .expect("rule config");
        let runtime =
            AccessControlRuntime::new(Some(light_pingora::AccessControlConfig::default()), rules);
        let exchange = AccessControlExchange {
            endpoint: "/v1/accounts@get".to_string(),
            request_data: json!({}),
        };

        let result = block_on_access_control_response(
            &runtime,
            &exchange,
            &[],
            None,
            None,
            200,
            b"unfiltered secret",
        );

        let error = result.expect_err("filter failure must fail closed");
        let message = error.to_string();
        assert!(message.contains("access-control response filter failed"));
        assert!(!message.contains("valid JSON"));
    }

    #[test]
    fn portal_access_control_exchange_derives_logical_hybrid_endpoint() {
        let body = br#"{
            "host":"lightapi.net",
            "service":"service",
            "action":"getApi",
            "version":"0.1.0",
            "data":{"hostId":"host-1","apiId":"api-1"}
        }"#;
        let exchange = access_control_exchange(
            "/portal/query@post",
            "/portal/query",
            None,
            Some(body),
            None,
        )
        .expect("hybrid exchange");
        assert_eq!(exchange.endpoint, "lightapi.net/service/getApi/0.1.0");
        assert_eq!(exchange.request_data["hostId"], "host-1");
    }

    #[test]
    fn retired_gateway_marker_is_stripped_before_proxying() {
        let mut request =
            pingora::http::RequestHeader::build("POST", b"/anything", Some(2)).expect("request");
        request
            .append_header("x-light-gateway", "light-pingora")
            .expect("client marker");

        strip_retired_gateway_marker(&mut request);

        assert!(request.headers.get("x-light-gateway").is_none());
    }

    #[test]
    fn portal_access_control_exchange_derives_endpoint_from_get_cmd() {
        let cmd = r#"{"host":"lightapi.net","service":"user","action":"getUnreadPrivateMessageCount","version":"0.1.0","data":{"hostId":"01964b05-552a-7c4b-9184-6857e7f3dc5f","userId":"01964b05-5532-7c79-8cde-191dcbd421b8"}}"#;
        let query = format!(
            "cmd={}",
            form_urlencoded::byte_serialize(cmd.as_bytes()).collect::<String>()
        );

        let exchange = access_control_exchange(
            "/portal/query@get",
            "/portal/query",
            Some(&query),
            None,
            None,
        )
        .expect("hybrid exchange from cmd");

        assert_eq!(
            exchange.endpoint,
            "lightapi.net/user/getUnreadPrivateMessageCount/0.1.0"
        );
        assert_eq!(
            exchange.request_data["hostId"],
            "01964b05-552a-7c4b-9184-6857e7f3dc5f"
        );
    }

    #[test]
    fn required_runtime_query_exchange_matches_only_the_frozen_endpoint() {
        let runtime_cmd = r#"{"host":"lightapi.net","service":"instance","action":"getRuntimeInstance","version":"0.1.0","data":{"hostId":"host-a"}}"#;
        let runtime_query = format!(
            "cmd={}",
            form_urlencoded::byte_serialize(runtime_cmd.as_bytes()).collect::<String>()
        );
        let exchange = required_runtime_query_exchange(
            "/portal/query@get",
            "/portal/query",
            Some(&runtime_query),
            None,
        )
        .expect("valid hybrid query")
        .expect("protected endpoint");
        assert_eq!(exchange.endpoint, RUNTIME_INSTANCE_QUERY_ENDPOINT);
        assert_eq!(exchange.request_data["hostId"], "host-a");

        let other_cmd = r#"{"host":"lightapi.net","service":"instance","action":"getInstance","version":"0.1.0","data":{"hostId":"host-a"}}"#;
        let other_query = format!(
            "cmd={}",
            form_urlencoded::byte_serialize(other_cmd.as_bytes()).collect::<String>()
        );
        assert!(
            required_runtime_query_exchange(
                "/portal/query@get",
                "/portal/query",
                Some(&other_query),
                None,
            )
            .expect("valid unrelated query")
            .is_none()
        );
    }

    #[tokio::test]
    async fn runtime_query_policy_allows_only_the_three_exact_roles_and_fails_closed() {
        let rules = serde_yaml::from_str::<light_pingora::RuleFileConfig>(
            r#"
ruleBodies:
  allow-role:
    common: Y
    ruleId: allow-role
    ruleName: Allow role
    ruleType: req-acc
    actions:
      - actionClassName: com.networknt.rule.RoleBasedAccessControlAction
endpointRules:
  lightapi.net/instance/getRuntimeInstance/0.1.0:
    permission:
      roles: admin host-admin instance-admin
    req-acc: [allow-role]
"#,
        )
        .expect("runtime query policy");
        let runtime =
            AccessControlRuntime::new(Some(light_pingora::AccessControlConfig::default()), rules);
        let exchange = AccessControlExchange {
            endpoint: RUNTIME_INSTANCE_QUERY_ENDPOINT.to_string(),
            request_data: json!({"hostId": "host-a"}),
        };
        for role in ["admin", "host-admin", "instance-admin"] {
            let auth = AuthPrincipal {
                role: Some(role.to_string()),
                claims: json!({"role": role}),
                ..AuthPrincipal::default()
            };
            assert!(
                authorize_required_runtime_query(Some(&runtime), &exchange, &[], Some(&auth), None)
                    .await
                    .is_ok(),
                "{role} should be admitted"
            );
        }
        for role in ["user", "host-admin-extra", ""] {
            let auth = AuthPrincipal {
                role: Some(role.to_string()),
                claims: json!({"role": role}),
                ..AuthPrincipal::default()
            };
            let rejection =
                authorize_required_runtime_query(Some(&runtime), &exchange, &[], Some(&auth), None)
                    .await
                    .expect_err("role must be denied");
            assert_eq!(rejection.status, 403);
        }
        assert_eq!(
            authorize_required_runtime_query(None, &exchange, &[], None, None)
                .await
                .expect_err("missing policy must fail closed")
                .status,
            503
        );
    }

    #[test]
    fn portal_access_control_exchange_derives_endpoint_from_json_rpc_body() {
        let body = br#"{
            "jsonrpc":"2.0",
            "method":"lightapi.net/rule/createRule/0.1.0",
            "params":{"hostId":"host-1","ruleId":"rule-1"},
            "id":"request-1"
        }"#;
        let exchange = access_control_exchange(
            "/portal/command@post",
            "/portal/command",
            None,
            Some(body),
            None,
        )
        .expect("hybrid exchange from json-rpc body");

        assert_eq!(exchange.endpoint, "lightapi.net/rule/createRule/0.1.0");
        assert_eq!(exchange.request_data["hostId"], "host-1");
    }

    #[test]
    fn portal_access_control_exchange_rejects_non_object_get_cmd() {
        let cmd = r#"["lightapi.net","user"]"#;
        let query = format!(
            "cmd={}",
            form_urlencoded::byte_serialize(cmd.as_bytes()).collect::<String>()
        );

        let rejection = access_control_exchange(
            "/portal/query@get",
            "/portal/query",
            Some(&query),
            None,
            None,
        )
        .expect_err("non-object cmd should fail");

        assert_eq!(rejection.status, 400);
        assert_eq!(rejection.code, "ERR13023");
        assert_eq!(
            rejection.message,
            "invalid hybrid portal request cmd: expected JSON object"
        );
    }

    #[derive(Debug, Clone)]
    struct ObservedBackendHandshake {
        path_and_query: String,
        authorization: Option<String>,
        agent_header: Option<String>,
        service_id_header: Option<String>,
        subprotocol: Option<String>,
    }

    struct NoopRegistryHandler;

    #[async_trait]
    impl RegistryHandler for NoopRegistryHandler {}

    async fn spawn_websocket_echo_backend() -> (
        std::net::SocketAddr,
        Arc<Mutex<Option<ObservedBackendHandshake>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind echo backend");
        let address = listener.local_addr().expect("echo backend address");
        let observed = Arc::new(Mutex::new(None));
        let observed_for_task = Arc::clone(&observed);
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept echo connection");
            let observed_for_callback = Arc::clone(&observed_for_task);
            let callback = move |request: &WsServerRequest, mut response: WsServerResponse| {
                let subprotocol = header_value(request, "sec-websocket-protocol");
                *observed_for_callback.lock().expect("observed lock") =
                    Some(ObservedBackendHandshake {
                        path_and_query: request
                            .uri()
                            .path_and_query()
                            .map(|value| value.as_str().to_string())
                            .unwrap_or_else(|| request.uri().path().to_string()),
                        authorization: header_value(request, "authorization"),
                        agent_header: header_value(request, "x-agent-test"),
                        service_id_header: header_value(request, "service_id")
                            .or_else(|| header_value(request, "serviceId"))
                            .or_else(|| header_value(request, "Service-Id")),
                        subprotocol: subprotocol.clone(),
                    });
                if subprotocol
                    .as_deref()
                    .is_some_and(|value| websocket_protocol_contains(value, "chat.v1"))
                {
                    response.headers_mut().insert(
                        "sec-websocket-protocol",
                        HeaderValue::from_static("chat.v1"),
                    );
                }
                Ok(response)
            };
            let mut websocket = accept_hdr_async(stream, callback)
                .await
                .expect("accept echo websocket");
            while let Some(message) = websocket.next().await {
                match message.expect("echo websocket message") {
                    Message::Text(text) => {
                        websocket
                            .send(Message::Text(format!("echo:{text}").into()))
                            .await
                            .expect("send text echo");
                    }
                    Message::Binary(bytes) => {
                        websocket
                            .send(Message::Binary(bytes))
                            .await
                            .expect("send binary echo");
                    }
                    Message::Close(_) => {
                        break;
                    }
                    Message::Ping(bytes) => {
                        websocket
                            .send(Message::Pong(bytes))
                            .await
                            .expect("send pong");
                    }
                    Message::Pong(_) | Message::Frame(_) => {}
                }
            }
        });
        (address, observed, task)
    }

    async fn spawn_fake_registry(
        backend_address: std::net::SocketAddr,
    ) -> (
        String,
        oneshot::Receiver<JsonValue>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake registry");
        let address = listener.local_addr().expect("registry address");
        let (lookup_tx, lookup_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept registry connection");
            let mut websocket = accept_async(stream)
                .await
                .expect("accept registry websocket");

            let register = websocket
                .next()
                .await
                .expect("registry register message")
                .expect("valid registry register frame")
                .into_text()
                .expect("register text");
            let register_json =
                serde_json::from_str::<JsonValue>(&register).expect("register json");
            assert_eq!(register_json["method"], "service/register");
            websocket
                .send(Message::Text(
                    json!({
                        "jsonrpc": "2.0",
                        "id": register_json["id"],
                        "result": {
                            "runtimeInstanceId": "0195ef10-2f24-7af2-85e9-a8ef54642f39",
                            "status": "registered"
                        }
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .expect("send register ack");

            let mut lookup_tx = Some(lookup_tx);
            while let Some(message) = websocket.next().await {
                let message = message.expect("valid registry frame");
                let Message::Text(text) = message else {
                    continue;
                };
                let lookup_json =
                    serde_json::from_str::<JsonValue>(&text).expect("registry request json");
                if lookup_json["method"] != "discovery/lookup" {
                    continue;
                }
                if let Some(sender) = lookup_tx.take() {
                    let _ = sender.send(lookup_json.clone());
                }
                websocket
                    .send(Message::Text(
                        json!({
                            "jsonrpc": "2.0",
                            "id": lookup_json["id"],
                            "result": {
                                "serviceId": lookup_json["params"]["serviceId"],
                                "envTag": lookup_json["params"]["envTag"],
                                "protocol": lookup_json["params"]["protocol"],
                                "nodes": [{
                                    "runtimeInstanceId": "0195ef10-2f24-7af2-85e9-a8ef54642f40",
                                    "serviceId": lookup_json["params"]["serviceId"],
                                    "envTag": lookup_json["params"]["envTag"],
                                    "environment": "dev",
                                    "version": "1.0.0",
                                    "protocol": "http",
                                    "address": backend_address.ip().to_string(),
                                    "port": backend_address.port(),
                                    "tags": {},
                                    "connectedAt": "2026-01-01T00:00:00Z",
                                    "lastSeenAt": "2026-01-01T00:00:01Z",
                                    "connected": true
                                }]
                            }
                        })
                        .to_string()
                        .into(),
                    ))
                    .await
                    .expect("send discovery response");
            }
        });
        (format!("ws://{address}"), lookup_rx, task)
    }

    async fn wait_for_registry_registration(
        receiver: &mut tokio::sync::watch::Receiver<RegistrationState>,
    ) {
        timeout(TokioDuration::from_secs(5), async {
            loop {
                if matches!(
                    receiver.borrow().clone(),
                    RegistrationState::Registered { .. }
                ) {
                    break;
                }
                receiver.changed().await.expect("registration state change");
            }
        })
        .await
        .expect("registry registration");
    }

    async fn wait_for_tcp(address: std::net::SocketAddr) {
        timeout(TokioDuration::from_secs(5), async {
            loop {
                if TcpStream::connect(address).await.is_ok() {
                    break;
                }
                sleep(TokioDuration::from_millis(20)).await;
            }
        })
        .await
        .expect("tcp listener ready");
    }

    fn free_tcp_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind free port")
            .local_addr()
            .expect("free port address")
            .port()
    }

    async fn raw_http_exchange(address: std::net::SocketAddr, request: &str) -> Vec<u8> {
        let mut client = TcpStream::connect(address)
            .await
            .expect("connect test HTTP listener");
        client
            .write_all(request.as_bytes())
            .await
            .expect("write test HTTP request");
        let mut response = Vec::new();
        timeout(
            TokioDuration::from_secs(5),
            client.read_to_end(&mut response),
        )
        .await
        .expect("test HTTP response timeout")
        .expect("read test HTTP response");
        response
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn invalid_llm_config_starts_degraded_and_returns_503_for_llm_routes() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        let gateway_port = free_tcp_port();
        let gateway_address = format!("127.0.0.1:{gateway_port}")
            .parse::<std::net::SocketAddr>()
            .expect("gateway address");
        std::fs::write(
            config_dir.path().join("server.yml"),
            format!(
                "ip: 127.0.0.1\nadvertisedAddress: 127.0.0.1\nhttpPort: {gateway_port}\nenableHttp: true\nhttpsPort: 8443\nenableHttps: false\nserviceId: com.networknt.light-gateway-1.0.0\nenableRegistry: false\nstartOnRegistryFailure: true\ndynamicPort: false\nenvironment: dev\nshutdownGracefulPeriod: 100\n"
            ),
        )
        .expect("write server config");
        std::fs::write(
            config_dir.path().join("handler.yml"),
            r#"
handlers: [llm]
paths:
  - path: /v1/chat/completions
    method: POST
    exec: [llm]
defaultHandlers: []
"#,
        )
        .expect("write handler config");
        std::fs::write(
            config_dir.path().join(LLM_ROUTER_FILE),
            r#"
enabled: true
providers:
  broken:
    format: openai
    baseUrl: https://example.invalid/v1
deployments: {}
aliases: {}
"#,
        )
        .expect("write invalid LLM config");

        let runtime = LightRuntimeBuilder::new(PingoraTransport::new(GatewayApp::default()))
            .with_config_dir(config_dir.path())
            .with_external_config_dir(external_dir.path())
            .build();
        let running = runtime
            .start()
            .await
            .expect("invalid LLM config must not abort gateway startup");
        wait_for_tcp(gateway_address).await;

        let health = raw_http_exchange(
            gateway_address,
            &format!(
                "GET /health HTTP/1.1\r\nHost: 127.0.0.1:{gateway_port}\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        let health = String::from_utf8_lossy(&health);
        assert!(health.starts_with("HTTP/1.1 200"), "response: {health}");

        let response = raw_http_exchange(
            gateway_address,
            &format!(
                "POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{gateway_port}\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
            ),
        )
        .await;
        let response = String::from_utf8_lossy(&response);
        assert!(response.starts_with("HTTP/1.1 503"), "response: {response}");
        assert!(response.contains("\"code\":\"service_unavailable\""));
        assert!(response.contains("LLM routing is unavailable"));

        running.shutdown().await.expect("shutdown gateway");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn phase3_hmac_route_rejects_invalid_signature_before_upstream_selection() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        let gateway_port = free_tcp_port();
        let gateway_address = format!("127.0.0.1:{gateway_port}")
            .parse::<std::net::SocketAddr>()
            .expect("gateway address");
        std::fs::write(
            config_dir.path().join("server.yml"),
            format!(
                "ip: 127.0.0.1\nadvertisedAddress: 127.0.0.1\nhttpPort: {gateway_port}\nenableHttp: true\nhttpsPort: 8443\nenableHttps: false\nserviceId: com.networknt.light-gateway-1.0.0\nenableRegistry: false\nstartOnRegistryFailure: true\ndynamicPort: false\nenvironment: dev\nshutdownGracefulPeriod: 100\n"
            ),
        )
        .expect("write server config");
        write_hmac_phase1_fixture(config_dir.path(), 1024);

        let runtime = LightRuntimeBuilder::new(PingoraTransport::new(GatewayApp::default()))
            .with_config_dir(config_dir.path())
            .with_external_config_dir(external_dir.path())
            .build();
        let running = runtime.start().await.expect("start Phase 1 HMAC gateway");
        wait_for_tcp(gateway_address).await;

        let response = raw_http_exchange(
            gateway_address,
            &format!(
                "POST /webhook HTTP/1.1\r\nHost: 127.0.0.1:{gateway_port}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"
            ),
        )
        .await;
        let response = String::from_utf8_lossy(&response);
        assert!(response.starts_with("HTTP/1.1 401"), "response: {response}");
        assert!(response.contains("invalid webhook authentication"));

        running.shutdown().await.expect("shutdown gateway");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn phase4_github_to_counting_jenkins_preserves_body_and_replay_lifecycle() {
        use hmac::{Hmac, Mac};

        let upstream = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind HMAC upstream");
        let upstream_address = upstream.local_addr().expect("HMAC upstream address");
        let upstream_requests = Arc::new(AtomicUsize::new(0));
        let upstream_bodies = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let upstream_saw_github_event = Arc::new(AtomicBool::new(false));
        let upstream_failed_once = Arc::new(AtomicBool::new(false));
        let upstream_task = tokio::spawn({
            let upstream_requests = Arc::clone(&upstream_requests);
            let upstream_bodies = Arc::clone(&upstream_bodies);
            let upstream_saw_github_event = Arc::clone(&upstream_saw_github_event);
            let upstream_failed_once = Arc::clone(&upstream_failed_once);
            async move {
                while let Ok((mut socket, _)) = upstream.accept().await {
                    let request = read_complete_http_request(&mut socket).await;
                    upstream_requests.fetch_add(1, Ordering::SeqCst);
                    let header_end = request
                        .windows(4)
                        .position(|value| value == b"\r\n\r\n")
                        .expect("complete upstream headers");
                    let body = request[header_end + 4..].to_vec();
                    if String::from_utf8_lossy(&request[..header_end])
                        .to_ascii_lowercase()
                        .contains("x-github-event: push")
                    {
                        upstream_saw_github_event.store(true, Ordering::SeqCst);
                    }
                    upstream_bodies
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .push(body.clone());
                    let status =
                        if body == b"fail" && !upstream_failed_once.swap(true, Ordering::SeqCst) {
                            500
                        } else {
                            204
                        };
                    let response = format!(
                        "HTTP/1.1 {status} Test\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                }
            }
        });

        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        let gateway_port = free_tcp_port();
        let gateway_address = format!("127.0.0.1:{gateway_port}")
            .parse::<std::net::SocketAddr>()
            .expect("gateway address");
        std::fs::write(
            config_dir.path().join("server.yml"),
            format!(
                "ip: 127.0.0.1\nadvertisedAddress: 127.0.0.1\nhttpPort: {gateway_port}\nenableHttp: true\nhttpsPort: 8443\nenableHttps: false\nserviceId: com.networknt.light-gateway-1.0.0\nenableRegistry: false\nstartOnRegistryFailure: true\ndynamicPort: false\nenvironment: dev\nshutdownGracefulPeriod: 100\n"
            ),
        )
        .expect("write server config");
        write_hmac_phase1_fixture(config_dir.path(), 1024);
        std::fs::write(
            config_dir.path().join(light_pingora::ROUTER_FILE),
            "hostWhitelist: ['127\\.0\\.0\\.1']\n",
        )
        .expect("write router config");

        let runtime = LightRuntimeBuilder::new(PingoraTransport::new(GatewayApp::default()))
            .with_config_dir(config_dir.path())
            .with_external_config_dir(external_dir.path())
            .build();
        let running = runtime.start().await.expect("start Phase 3 HMAC gateway");
        wait_for_tcp(gateway_address).await;
        let secret = std::env::var("PATH").expect("PATH test secret");
        let sign = |body: &[u8]| {
            let mut mac =
                Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC test key");
            mac.update(body);
            format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
        };
        let send = |delivery: &str, body: &[u8]| {
            let signature = sign(body);
            let request = format!(
                "POST /webhook HTTP/1.1\r\nHost: 127.0.0.1:{gateway_port}\r\nservice_url: http://{upstream_address}\r\nX-Hub-Signature-256: {signature}\r\nX-GitHub-Delivery: {delivery}\r\nX-GitHub-Event: push\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                String::from_utf8_lossy(body)
            );
            async move { raw_http_exchange(gateway_address, &request).await }
        };

        let body = b"{ \"event\": \"push\" }";
        let accepted = send("delivery-accepted", body).await;
        assert!(String::from_utf8_lossy(&accepted).starts_with("HTTP/1.1 204"));
        let duplicate = send("delivery-accepted", body).await;
        assert!(String::from_utf8_lossy(&duplicate).starts_with("HTTP/1.1 200"));
        assert_eq!(upstream_requests.load(Ordering::SeqCst), 1);
        assert!(upstream_saw_github_event.load(Ordering::SeqCst));

        let failed = send("delivery-retry", b"fail").await;
        assert!(String::from_utf8_lossy(&failed).starts_with("HTTP/1.1 500"));
        let retried = send("delivery-retry", b"fail").await;
        assert!(String::from_utf8_lossy(&retried).starts_with("HTTP/1.1 204"));
        assert_eq!(upstream_requests.load(Ordering::SeqCst), 3);

        let locally_rejected_body = b"local-rejection";
        let locally_rejected_request = format!(
            "POST /webhook HTTP/1.1\r\nHost: 127.0.0.1:{gateway_port}\r\nX-Hub-Signature-256: {}\r\nX-GitHub-Delivery: delivery-local-rejection\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            sign(locally_rejected_body),
            locally_rejected_body.len(),
            String::from_utf8_lossy(locally_rejected_body)
        );
        let locally_rejected = raw_http_exchange(gateway_address, &locally_rejected_request).await;
        assert!(String::from_utf8_lossy(&locally_rejected).starts_with("HTTP/1.1 502"));
        assert_eq!(upstream_requests.load(Ordering::SeqCst), 3);
        let locally_retried = send("delivery-local-rejection", locally_rejected_body).await;
        assert!(String::from_utf8_lossy(&locally_retried).starts_with("HTTP/1.1 204"));

        let unavailable_port = free_tcp_port();
        let transport_body = b"transport-retry";
        let transport_request = format!(
            "POST /webhook HTTP/1.1\r\nHost: 127.0.0.1:{gateway_port}\r\nservice_url: http://127.0.0.1:{unavailable_port}\r\nX-Hub-Signature-256: {}\r\nX-GitHub-Delivery: delivery-transport-retry\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            sign(transport_body),
            transport_body.len(),
            String::from_utf8_lossy(transport_body)
        );
        let transport_failure = raw_http_exchange(gateway_address, &transport_request).await;
        assert!(String::from_utf8_lossy(&transport_failure).starts_with("HTTP/1.1 502"));
        let transport_retried = send("delivery-transport-retry", transport_body).await;
        assert!(String::from_utf8_lossy(&transport_retried).starts_with("HTTP/1.1 204"));
        assert_eq!(upstream_requests.load(Ordering::SeqCst), 5);
        assert_eq!(
            upstream_bodies
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_slice(),
            &[
                body.to_vec(),
                b"fail".to_vec(),
                b"fail".to_vec(),
                locally_rejected_body.to_vec(),
                transport_body.to_vec(),
            ]
        );

        running.shutdown().await.expect("shutdown gateway");
        upstream_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn phase4_composed_api_key_and_hmac_reach_upstream_only_when_both_verify() {
        use hmac::{Hmac, Mac};

        let upstream = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind composed-auth upstream");
        let upstream_address = upstream.local_addr().expect("composed upstream address");
        let upstream_requests = Arc::new(AtomicUsize::new(0));
        let upstream_body = Arc::new(Mutex::new(Vec::new()));
        let upstream_task = tokio::spawn({
            let upstream_requests = Arc::clone(&upstream_requests);
            let upstream_body = Arc::clone(&upstream_body);
            async move {
                while let Ok((mut socket, _)) = upstream.accept().await {
                    let request = read_complete_http_request(&mut socket).await;
                    upstream_requests.fetch_add(1, Ordering::SeqCst);
                    let header_end = request
                        .windows(4)
                        .position(|value| value == b"\r\n\r\n")
                        .expect("complete composed upstream headers");
                    *upstream_body
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()) =
                        request[header_end + 4..].to_vec();
                    let _ = socket
                        .write_all(
                            b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                }
            }
        });

        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        let gateway_port = free_tcp_port();
        let gateway_address = format!("127.0.0.1:{gateway_port}")
            .parse::<std::net::SocketAddr>()
            .expect("gateway address");
        std::fs::write(
            config_dir.path().join("server.yml"),
            format!(
                "ip: 127.0.0.1\nadvertisedAddress: 127.0.0.1\nhttpPort: {gateway_port}\nenableHttp: true\nhttpsPort: 8443\nenableHttps: false\nserviceId: com.networknt.light-gateway-1.0.0\nenableRegistry: false\nstartOnRegistryFailure: true\ndynamicPort: false\nenvironment: dev\nshutdownGracefulPeriod: 100\n"
            ),
        )
        .expect("write composed server config");
        std::fs::write(
            config_dir.path().join("handler.yml"),
            "handlers: [unified-security, router]\npaths:\n  - path: /partner\n    method: POST\n    exec: [unified-security, router]\ndefaultHandlers: []\n",
        )
        .expect("write composed handler config");
        std::fs::write(
            config_dir.path().join(light_pingora::HMAC_FILE),
            "enabled: true\npathPrefixAuths: []\nprofiles:\n  partner:\n    maxBodyBytes: 1024\n    secrets:\n      defaultEnvNames: [PATH]\n",
        )
        .expect("write composed HMAC config");
        std::fs::write(
            config_dir
                .path()
                .join(light_pingora::UNIFIED_SECURITY_FILE),
            "enabled: true\nanonymousPrefixes: []\npathPrefixAuths:\n  - prefix: /partner\n    methods: [POST]\n    authentication:\n      allOf:\n        - type: apiKey\n        - type: hmac\n          profile: partner\n",
        )
        .expect("write composed security config");
        std::fs::write(
            config_dir.path().join(light_pingora::APIKEY_FILE),
            "enabled: true\nhashEnabled: false\npathPrefixAuths:\n  - pathPrefix: /partner\n    headerName: X-Partner-Key\n    apiKey: partner-key\n",
        )
        .expect("write API-key config");
        std::fs::write(
            config_dir.path().join(light_pingora::ROUTER_FILE),
            "hostWhitelist: ['127\\.0\\.0\\.1']\n",
        )
        .expect("write composed router config");

        let runtime = LightRuntimeBuilder::new(PingoraTransport::new(GatewayApp::default()))
            .with_config_dir(config_dir.path())
            .with_external_config_dir(external_dir.path())
            .build();
        let running = runtime.start().await.expect("start composed HMAC gateway");
        wait_for_tcp(gateway_address).await;
        let body = b"{\"event\":\"partner\"}";
        let secret = std::env::var("PATH").expect("PATH test secret");
        let mut mac =
            Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC test key");
        mac.update(body);
        let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        let send = |api_key: Option<&str>, signature: &str| {
            let api_key = api_key
                .map(|value| format!("X-Partner-Key: {value}\r\n"))
                .unwrap_or_default();
            let request = format!(
                "POST /partner HTTP/1.1\r\nHost: 127.0.0.1:{gateway_port}\r\nservice_url: http://{upstream_address}\r\n{api_key}X-Hub-Signature-256: {signature}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                String::from_utf8_lossy(body)
            );
            async move { raw_http_exchange(gateway_address, &request).await }
        };

        let missing_api_key = send(None, &signature).await;
        assert!(String::from_utf8_lossy(&missing_api_key).starts_with("HTTP/1.1 401"));
        let invalid_hmac = send(Some("partner-key"), "sha256=00").await;
        assert!(String::from_utf8_lossy(&invalid_hmac).starts_with("HTTP/1.1 401"));
        assert_eq!(upstream_requests.load(Ordering::SeqCst), 0);

        let accepted = send(Some("partner-key"), &signature).await;
        assert!(String::from_utf8_lossy(&accepted).starts_with("HTTP/1.1 204"));
        assert_eq!(upstream_requests.load(Ordering::SeqCst), 1);
        assert_eq!(
            upstream_body
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_slice(),
            body
        );

        running.shutdown().await.expect("shutdown composed gateway");
        upstream_task.abort();
    }

    async fn read_complete_http_request(socket: &mut TcpStream) -> Vec<u8> {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        loop {
            let read = timeout(TokioDuration::from_secs(2), socket.read(&mut buffer))
                .await
                .expect("runtime request timeout")
                .expect("runtime request read");
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
            let Some(header_end) = request.windows(4).position(|value| value == b"\r\n\r\n") else {
                continue;
            };
            let headers = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
            let content_length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if headers.lines().any(|line| {
                line.strip_prefix("transfer-encoding:")
                    .is_some_and(|value| value.trim().eq_ignore_ascii_case("chunked"))
            }) {
                if request[header_end + 4..]
                    .windows(5)
                    .any(|value| value == b"0\r\n\r\n")
                {
                    break;
                }
                continue;
            }
            if request.len() >= header_end + 4 + content_length {
                break;
            }
        }
        request
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn model_provider_sidecar_denies_unmatched_method_paths_without_runtime_contact() {
        let runtime_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind counting runtime");
        let runtime_address = runtime_listener.local_addr().expect("runtime address");
        let runtime_connections = Arc::new(AtomicUsize::new(0));
        let runtime_disconnects = Arc::new(AtomicUsize::new(0));
        let runtime_saw_chunked_body = Arc::new(AtomicBool::new(false));
        let runtime_task = tokio::spawn({
            let runtime_connections = Arc::clone(&runtime_connections);
            let runtime_disconnects = Arc::clone(&runtime_disconnects);
            let runtime_saw_chunked_body = Arc::clone(&runtime_saw_chunked_body);
            async move {
                while let Ok((mut socket, _peer)) = runtime_listener.accept().await {
                    runtime_connections.fetch_add(1, Ordering::SeqCst);
                    let request = read_complete_http_request(&mut socket).await;
                    let body = String::from_utf8_lossy(&request);
                    if body.contains("chunked-body") {
                        runtime_saw_chunked_body.store(true, Ordering::SeqCst);
                    }
                    let response_body = if body.contains("large-response") {
                        "x".repeat(8_192)
                    } else {
                        r#"{"data":[{"embedding":[0.1,0.2],"index":0}],"model":"fake"}"#.to_string()
                    };
                    if body.contains("disconnect-mode") {
                        if socket
                            .write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                            )
                            .await
                            .is_ok()
                        {
                            let mut chunk = b"1000\r\n".to_vec();
                            chunk.extend(std::iter::repeat_n(b'x', 4_096));
                            chunk.extend_from_slice(b"\r\n");
                            for _ in 0..200 {
                                if socket.write_all(&chunk).await.is_err() {
                                    runtime_disconnects.fetch_add(1, Ordering::SeqCst);
                                    break;
                                }
                                sleep(TokioDuration::from_millis(10)).await;
                            }
                        } else {
                            runtime_disconnects.fetch_add(1, Ordering::SeqCst);
                        }
                    } else if body.contains("stream-mode") {
                        let midpoint = response_body.len() / 2;
                        let first = &response_body[..midpoint];
                        let second = &response_body[midpoint..];
                        socket
                            .write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                            )
                            .await
                            .expect("write streaming runtime headers");
                        for chunk in [first, second] {
                            socket
                                .write_all(format!("{:x}\r\n{chunk}\r\n", chunk.len()).as_bytes())
                                .await
                                .expect("write streaming runtime chunk");
                            sleep(TokioDuration::from_millis(20)).await;
                        }
                        socket
                            .write_all(b"0\r\n\r\n")
                            .await
                            .expect("finish streaming runtime response");
                    } else {
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                            response_body.len()
                        );
                        let _ = socket.write_all(response.as_bytes()).await;
                    }
                }
            }
        });

        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        let gateway_port = free_tcp_port();
        let gateway_address = format!("127.0.0.1:{gateway_port}")
            .parse::<std::net::SocketAddr>()
            .expect("gateway address");
        std::fs::write(
            config_dir.path().join("server.yml"),
            format!(
                "ip: 127.0.0.1\nadvertisedAddress: 127.0.0.1\nhttpPort: {gateway_port}\nenableHttp: true\nhttpsPort: 8443\nenableHttps: false\nserviceId: com.networknt.model-provider-sidecar-1.0.0\nenableRegistry: false\nstartOnRegistryFailure: true\ndynamicPort: false\nenvironment: test\nshutdownGracefulPeriod: 100\n"
            ),
        )
        .expect("write server config");
        let request = model_provider_sidecar::SidecarProfileRequest {
            profile_version: "embedding-only-v1".to_string(),
            physical_runtime_id: "test-node/runtime-a".to_string(),
            runtime_base_url: format!("http://{runtime_address}"),
            certificate_identity_sha256: "a".repeat(64),
            isolation_evidence_sha256: "b".repeat(64),
            operations: std::collections::BTreeSet::from([
                model_provider_sidecar::SidecarOperation::Embeddings,
            ]),
            jwt_trust: model_provider_sidecar::SidecarJwtTrust {
                issuer: "https://issuer.example".to_string(),
                audience: "model-provider-sidecar".to_string(),
                key_server_url: "https://oauth.example".to_string(),
                key_uri: "/oauth2/key".to_string(),
                key_service_id: None,
                ca_cert_path: None,
            },
            runtime_auth: llm_gateway::config::RuntimeAuth::None,
            max_request_time_ms: 5_000,
            connect_timeout_ms: 500,
            stream_setup_timeout_ms: 1_000,
            idle_timeout_ms: 1_000,
            max_request_body_bytes: 512,
            max_response_body_bytes: 4_096,
        };
        let bundle = model_provider_sidecar::generate_sidecar_bundle(&request)
            .expect("generate sidecar profile");
        model_provider_sidecar::write_sidecar_bundle(config_dir.path(), &bundle)
            .expect("write generated sidecar profile");
        // The integration fixture intentionally uses local plaintext and
        // anonymous routes. Production consumes the generated TLS/JWT files.
        std::fs::write(
            config_dir.path().join("server.yml"),
            format!(
                "ip: 127.0.0.1\nadvertisedAddress: 127.0.0.1\nhttpPort: {gateway_port}\nenableHttp: true\nhttpsPort: 8443\nenableHttps: false\nserviceId: com.networknt.model-provider-sidecar-1.0.0\nenableRegistry: false\nstartOnRegistryFailure: true\ndynamicPort: false\nenvironment: test\nshutdownGracefulPeriod: 100\n"
            ),
        )
        .expect("override production TLS for integration fixture");
        std::fs::write(
            config_dir.path().join("security.yml"),
            "enableVerifyJwt: true\nenableVerifyScope: false\nbootstrapFromKeyService: false\n",
        )
        .expect("disable key bootstrap for anonymous integration fixture");
        std::fs::write(
            config_dir.path().join(light_pingora::UNIFIED_SECURITY_FILE),
            "enabled: true\nanonymousPrefixes: [/v1/embeddings, /sidecar/health, /sidecar/identity]\npathPrefixAuths: []\n",
        )
        .expect("make generated operation paths anonymous for proxy integration test");
        let manifest_path = config_dir.path().join("sidecar-manifest.json");
        // SAFETY: this is the only test that boots the process-wide sidecar manifest,
        // and the value is removed after the runtime is shut down.
        unsafe { std::env::set_var("MODEL_PROVIDER_SIDECAR_MANIFEST", &manifest_path) };

        let runtime = LightRuntimeBuilder::new(PingoraTransport::new(GatewayApp::default()))
            .with_config_dir(config_dir.path())
            .with_external_config_dir(external_dir.path())
            .build();
        let running = runtime.start().await.expect("start sidecar gateway");
        wait_for_tcp(gateway_address).await;

        for (method, path) in [
            ("POST", "/api/tags"),
            ("POST", "/api/pull"),
            ("GET", "/v1/embeddings"),
            ("POST", "/v1/chat/completions"),
        ] {
            let request = format!(
                "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{gateway_port}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let response = raw_http_exchange(gateway_address, &request).await;
            let response = String::from_utf8_lossy(&response);
            assert!(
                response.starts_with("HTTP/1.1 404"),
                "{method} {path} was not denied locally: {response}"
            );
        }

        sleep(TokioDuration::from_millis(100)).await;
        assert_eq!(
            runtime_connections.load(Ordering::SeqCst),
            0,
            "a denied sidecar request reached the raw model runtime"
        );

        let health = raw_http_exchange(
            gateway_address,
            &format!(
                "GET /sidecar/health HTTP/1.1\r\nHost: 127.0.0.1:{gateway_port}\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        let health = String::from_utf8_lossy(&health);
        assert!(health.starts_with("HTTP/1.1 200"));
        assert!(health.ends_with("ok"));

        let identity = raw_http_exchange(
            gateway_address,
            &format!(
                "GET /sidecar/identity HTTP/1.1\r\nHost: 127.0.0.1:{gateway_port}\r\nConnection: close\r\n\r\n"
            ),
        )
        .await;
        let identity = String::from_utf8_lossy(&identity);
        assert!(identity.starts_with("HTTP/1.1 200"));
        assert!(identity.contains("\"profileVersion\":\"embedding-only-v1\""));
        assert!(identity.contains("\"path\":\"/v1/embeddings\""));
        assert!(!identity.contains(&request.runtime_base_url));

        for body in [r#"{"input":["buffered"]}"#, r#"{"input":["stream-mode"]}"#] {
            let request = format!(
                "POST /v1/embeddings HTTP/1.1\r\nHost: 127.0.0.1:{gateway_port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let response = raw_http_exchange(gateway_address, &request).await;
            let response = String::from_utf8_lossy(&response);
            assert!(response.starts_with("HTTP/1.1 200"), "response: {response}");
            assert!(response.contains("embedding"), "response: {response}");
        }
        assert_eq!(runtime_connections.load(Ordering::SeqCst), 2);

        let chunked_body = r#"{"input":["chunked-body"]}"#;
        let chunked_response = raw_http_exchange(
            gateway_address,
            &format!(
                "POST /v1/embeddings HTTP/1.1\r\nHost: 127.0.0.1:{gateway_port}\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{chunked_body}\r\n0\r\n\r\n",
                chunked_body.len()
            ),
        )
        .await;
        assert!(
            String::from_utf8_lossy(&chunked_response).starts_with("HTTP/1.1 200"),
            "chunked sidecar request failed: {}",
            String::from_utf8_lossy(&chunked_response)
        );
        assert!(
            runtime_saw_chunked_body.load(Ordering::SeqCst),
            "sidecar lost the chunked request body before forwarding"
        );

        let large_body = r#"{"input":["large-response"]}"#;
        let large_response = raw_http_exchange(
            gateway_address,
            &format!(
                "POST /v1/embeddings HTTP/1.1\r\nHost: 127.0.0.1:{gateway_port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{large_body}",
                large_body.len()
            ),
        )
        .await;
        assert!(
            large_response.iter().filter(|byte| **byte == b'x').count() < 8_192,
            "generated sidecar response limit allowed the full oversized response"
        );

        let disconnect_body = r#"{"input":["disconnect-mode"]}"#;
        let disconnect_request = format!(
            "POST /v1/embeddings HTTP/1.1\r\nHost: 127.0.0.1:{gateway_port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{disconnect_body}",
            disconnect_body.len()
        );
        let mut disconnecting_client = TcpStream::connect(gateway_address)
            .await
            .expect("connect disconnecting sidecar client");
        disconnecting_client
            .write_all(disconnect_request.as_bytes())
            .await
            .expect("write disconnect probe");
        let mut first_response = [0_u8; 512];
        let first_read = timeout(
            TokioDuration::from_secs(5),
            disconnecting_client.read(&mut first_response),
        )
        .await
        .expect("disconnect stream did not start")
        .expect("read disconnect stream");
        assert!(
            first_read > 0,
            "disconnect stream closed before its first bytes"
        );
        let disconnecting_client = disconnecting_client
            .into_std()
            .expect("convert disconnecting client");
        socket2::SockRef::from(&disconnecting_client)
            .set_linger(Some(std::time::Duration::ZERO))
            .expect("configure downstream reset");
        drop(disconnecting_client);
        timeout(TokioDuration::from_secs(5), async {
            while runtime_disconnects.load(Ordering::SeqCst) == 0 {
                sleep(TokioDuration::from_millis(20)).await;
            }
        })
        .await
        .expect("downstream disconnect did not propagate to the runtime stream");

        let oversized_body = format!(r#"{{"input":"{}"}}"#, "x".repeat(1_024));
        let oversized = raw_http_exchange(
            gateway_address,
            &format!(
                "POST /v1/embeddings HTTP/1.1\r\nHost: 127.0.0.1:{gateway_port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{oversized_body}",
                oversized_body.len()
            ),
        )
        .await;
        assert!(
            String::from_utf8_lossy(&oversized).starts_with("HTTP/1.1 413"),
            "generated sidecar request limit did not fail closed: {}",
            String::from_utf8_lossy(&oversized)
        );
        running.shutdown().await.expect("shutdown sidecar gateway");
        unsafe { std::env::remove_var("MODEL_PROVIDER_SIDECAR_MANIFEST") };
        runtime_task.abort();
    }

    fn header_value(request: &WsServerRequest, name: &str) -> Option<String> {
        request
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    }

    fn websocket_protocol_contains(header: &str, expected: &str) -> bool {
        header
            .split(',')
            .any(|value| value.trim().eq_ignore_ascii_case(expected))
    }

    #[test]
    fn proxy_config_uses_runtime_resolved_values() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        std::fs::write(
            config_dir.path().join(light_pingora::PROXY_FILE),
            "enabled: ${proxy.enabled:true}\nhosts: ${proxy.hosts}\nrewriteHostHeader: ${proxy.rewriteHostHeader:true}\n",
        )
        .expect("write proxy config");
        let values = serde_yaml::from_str(
            r#"
proxy.hosts: https://api.example.com/base
proxy.rewriteHostHeader: false
"#,
        )
        .expect("parse values");

        let config = runtime_config(&config_dir, &external_dir, values);
        let route = load_proxy_route(&config)
            .expect("load proxy config")
            .expect("proxy route");

        assert!(!route.config.rewrite_host_header);
        assert_eq!(route.targets[0].address, "api.example.com:443");
        assert_eq!(route.targets[0].path_prefix, "/base");
        assert!(
            config
                .module_registry
                .module_summaries()
                .iter()
                .any(|entry| entry.module_id == light_pingora::PROXY_MODULE_ID && entry.reloadable)
        );
    }

    #[test]
    fn external_proxy_config_overlays_base_file() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        std::fs::write(
            config_dir.path().join(light_pingora::PROXY_FILE),
            "hosts: http://127.0.0.1:8081\n",
        )
        .expect("write base proxy config");
        std::fs::write(
            external_dir.path().join(light_pingora::PROXY_FILE),
            "hosts: http://127.0.0.1:8082\n",
        )
        .expect("write external proxy config");

        let config = runtime_config(&config_dir, &external_dir, HashMap::new());
        let route = load_proxy_route(&config)
            .expect("load proxy config")
            .expect("proxy route");

        assert_eq!(route.targets[0].address, "127.0.0.1:8082");
    }

    #[test]
    fn gateway_external_config_dir_is_separate_from_base_config() {
        assert_ne!(CONFIG_DIR, EXTERNAL_CONFIG_DIR);
    }

    #[test]
    fn gateway_loads_active_handlers_from_handler_yml() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        std::fs::write(
            config_dir.path().join("handler.yml"),
            r#"
enabled: ${handler.enabled:true}
reportHandlerDuration: ${handler.reportHandlerDuration:false}
handlerMetricsLogLevel: ${handler.handlerMetricsLogLevel:DEBUG}
basePath: ${handler.basePath:/}
handlers: ${handler.handlers:[]}
chains: ${handler.chains:{}}
paths: ${handler.paths:[]}
defaultHandlers: ${handler.defaultHandlers:[]}
"#,
        )
        .expect("write handler config");
        let values = serde_yaml::from_str(
            r#"
handler.handlers:
  - correlation
  - headers
  - jwt
handler.chains:
  api:
    exec:
      - correlation
      - headers
handler.paths:
  - path: /v1/test
    method: GET
    exec:
      - api
handler.defaultHandlers: []
"#,
        )
        .expect("parse handler values");
        let config = runtime_config(&config_dir, &external_dir, values);

        let proxy = GatewayProxy::from_runtime_config(&config).expect("build proxy");

        assert_eq!(
            proxy.active_handler_ids(),
            vec!["correlation".to_string(), "headers".to_string()]
        );
        assert!(
            config
                .module_registry
                .module_summaries()
                .iter()
                .any(|entry| entry.module_id == light_pingora::HANDLER_MODULE_ID && entry.active)
        );
    }

    fn write_hmac_phase1_fixture(config_dir: &std::path::Path, max_body_bytes: usize) {
        std::fs::write(
            config_dir.join("handler.yml"),
            r#"
handlers: [hmac, router]
paths:
  - path: /webhook
    method: POST
    exec: [hmac, router]
defaultHandlers: []
"#,
        )
        .expect("write HMAC handler config");
        std::fs::write(
            config_dir.join(light_pingora::HMAC_FILE),
            format!(
                r#"
enabled: true
pathPrefixAuths:
  - prefix: /webhook
    methods: [POST]
    profile: github
profiles:
  github:
    maxBodyBytes: {max_body_bytes}
    secrets:
      defaultEnvNames: [PATH]
    replay:
      enabled: true
      idHeader: X-GitHub-Delivery
      store: local
      retentionSeconds: 60
replayStores:
  local:
    type: local
    maxEntries: 4
"#
            ),
        )
        .expect("write HMAC config");
    }

    #[test]
    fn gateway_compiles_standalone_hmac_runtime_when_handler_is_active() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        write_hmac_phase1_fixture(config_dir.path(), 1024);
        let config = runtime_config(&config_dir, &external_dir, HashMap::new());

        let proxy = GatewayProxy::from_runtime_config(&config).expect("build HMAC gateway");
        let runtime = proxy.current_hmac_runtime();
        let runtime = runtime.as_ref().as_ref().expect("compiled HMAC runtime");
        assert_eq!(
            runtime.standalone_profile("/webhook/github", "POST"),
            Some("github")
        );
        assert_eq!(runtime.profile_limits("github"), Some((1024, 10_000)));
        let public = config.module_registry.component_configs();
        assert!(!public["hmac"].to_string().contains("PATH"));
    }

    #[test]
    fn hmac_body_budget_is_weighted_and_released_by_request_ownership() {
        let used = Arc::new(AtomicUsize::new(0));
        let mut first = HmacBodyPermit::acquire(Arc::clone(&used), 5, 8).expect("first permit");
        assert!(HmacBodyPermit::acquire(Arc::clone(&used), 4, 8).is_err());
        first.grow(3, 8).expect("grow to budget boundary");
        assert_eq!(used.load(Ordering::Acquire), 8);
        drop(first);
        assert_eq!(used.load(Ordering::Acquire), 0);
        assert!(HmacBodyPermit::acquire(used, 8, 8).is_ok());
    }

    #[test]
    fn hmac_effective_chain_rejects_authentication_after_router() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        write_hmac_phase1_fixture(config_dir.path(), 1024);
        std::fs::write(
            config_dir.path().join("handler.yml"),
            r#"
handlers: [hmac, router]
paths:
  - path: /webhook
    method: POST
    exec: [router, hmac]
defaultHandlers: []
"#,
        )
        .expect("write invalid HMAC handler order");
        let config = runtime_config(&config_dir, &external_dir, HashMap::new());
        let error = GatewayProxy::from_runtime_config(&config)
            .err()
            .expect("invalid materialized HMAC chain must fail");
        assert!(error.to_string().contains("must precede router"));
    }

    #[test]
    fn hmac_effective_chain_rejects_more_specific_route_without_standalone_entry() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        write_hmac_phase1_fixture(config_dir.path(), 1024);
        std::fs::write(
            config_dir.path().join("handler.yml"),
            r#"
handlers: [hmac, router]
paths:
  - path: /webhook
    method: POST
    exec: [hmac, router]
  - path: /webhook/special
    method: POST
    exec: [router]
defaultHandlers: []
"#,
        )
        .expect("write route override fixture");
        let config = runtime_config(&config_dir, &external_dir, HashMap::new());
        let error = GatewayProxy::from_runtime_config(&config)
            .err()
            .expect("more-specific route must not bypass standalone HMAC");
        assert!(error.to_string().contains("/webhook/special@post"));
        assert!(
            error
                .to_string()
                .contains("standalone HMAC policy and effective handler chain disagree")
        );
    }

    #[test]
    fn hmac_effective_chain_checks_all_method_composed_policy_on_every_configured_method() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        std::fs::write(
            config_dir.path().join("handler.yml"),
            r#"
handlers: [unified-security, router]
paths:
  - path: /partner/special
    method: PUT
    exec: [router]
defaultHandlers: [unified-security, router]
"#,
        )
        .expect("write composed route override fixture");
        std::fs::write(
            config_dir.path().join(light_pingora::HMAC_FILE),
            r#"
enabled: true
pathPrefixAuths: []
profiles:
  partner:
    secrets:
      defaultEnvNames: [PATH]
"#,
        )
        .expect("write composed HMAC config");
        std::fs::write(
            config_dir.path().join(light_pingora::UNIFIED_SECURITY_FILE),
            r#"
enabled: true
anonymousPrefixes: []
pathPrefixAuths:
  - prefix: /partner
    authentication:
      allOf:
        - type: hmac
          profile: partner
"#,
        )
        .expect("write all-method composed policy");
        let config = runtime_config(&config_dir, &external_dir, HashMap::new());
        let error = GatewayProxy::from_runtime_config(&config)
            .err()
            .expect("method-specific route must not bypass composed HMAC");
        assert!(
            error.to_string().contains("/partner/special@put"),
            "unexpected validation error: {error}"
        );
        assert!(
            error
                .to_string()
                .contains("requires exactly one unified-security handler")
        );
    }

    #[test]
    fn hmac_chain_probe_materializes_wildcard_and_parameter_paths() {
        assert_eq!(
            hmac_handler_path_probe("/webhook/{delivery}/*"),
            "/webhook/__hmac_parameter_probe__/__hmac_wildcard_probe__"
        );
    }

    #[tokio::test]
    async fn replay_admin_tool_removes_only_the_requested_logical_key() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        write_hmac_phase1_fixture(config_dir.path(), 1024);
        std::fs::write(
            config_dir.path().join(light_pingora::HMAC_FILE),
            r#"
enabled: true
pathPrefixAuths:
  - prefix: /webhook
    methods: [POST]
    profile: github
profiles:
  github:
    secrets:
      defaultEnvNames: [PATH]
    replay:
      enabled: true
      idHeader: X-GitHub-Delivery
      store: local
      retentionSeconds: 60
replayStores:
  local:
    type: local
    maxEntries: 4
"#,
        )
        .expect("write replay config");
        let config = runtime_config(&config_dir, &external_dir, HashMap::new());
        let admin = Arc::new(HmacReplayAdmin::default());
        let _proxy = GatewayProxy::from_runtime_config_with_admission_and_admin(
            &config,
            AdmissionGate::default(),
            Arc::clone(&admin),
        )
        .expect("build HMAC replay gateway");
        let runtime = admin.runtime().expect("admin runtime snapshot");
        let key = light_pingora::WebhookReplayKey::new("github", "shared", "delivery-1")
            .expect("replay key");
        runtime
            .replay_store("github")
            .unwrap()
            .reserve(&key, Duration::from_secs(60))
            .await
            .expect("reserve replay key");

        let handler = HmacReplayRegistryHandler { admin };
        let tools = handler.handle_request("tools/list", json!({})).await;
        assert_eq!(tools["tools"][0]["name"], "remove_webhook_replay");
        let removed = handler
            .handle_request(
                "tools/call",
                json!({
                    "name": "remove_webhook_replay",
                    "arguments": {
                        "profile": "github",
                        "selector": "shared",
                        "deliveryId": "delivery-1"
                    }
                }),
            )
            .await;
        assert_eq!(removed["status"], "success");
        assert_eq!(removed["removed"], true);
        assert_eq!(removed["scope"], "local");
        assert!(!removed.to_string().contains("delivery-1"));
        let absent = handler
            .handle_request(
                "tools/call",
                json!({
                    "name": "remove_webhook_replay",
                    "arguments": {
                        "profile": "github",
                        "selector": "shared",
                        "deliveryId": "delivery-1"
                    }
                }),
            )
            .await;
        assert_eq!(absent["removed"], false);
    }

    #[tokio::test]
    async fn hmac_reload_swaps_only_a_fully_compiled_candidate() {
        use hmac::{Hmac, Mac};

        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        write_hmac_phase1_fixture(config_dir.path(), 1024);
        let config = runtime_config(&config_dir, &external_dir, HashMap::new());
        let proxy = GatewayProxy::from_runtime_config(&config).expect("build HMAC gateway");
        let pinned_execution = proxy.current_security_execution();
        let body = b"secret-rotation-exercise";
        let signature = |secret: &str| {
            let mut mac =
                Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC test key");
            mac.update(body);
            format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
        };
        let headers = |value: String| {
            let mut headers = http::HeaderMap::new();
            headers.insert(
                "x-hub-signature-256",
                HeaderValue::from_str(&value).expect("test signature header"),
            );
            headers
        };
        let path_secret = std::env::var("PATH").expect("PATH test secret");
        let home_secret = std::env::var("HOME").expect("HOME rotation test secret");
        let path_headers = headers(signature(&path_secret));
        let home_headers = headers(signature(&home_secret));
        let initial_runtime = pinned_execution
            .hmac
            .as_ref()
            .as_ref()
            .expect("initial HMAC runtime");
        assert!(
            initial_runtime
                .verify("github", &path_headers, body)
                .is_ok()
        );
        assert!(
            initial_runtime
                .verify("github", &home_headers, body)
                .is_err()
        );
        let replay_key =
            light_pingora::WebhookReplayKey::new("github", "shared", "reload-preserved-delivery")
                .unwrap();
        let initial_store = proxy
            .current_hmac_runtime()
            .as_ref()
            .as_ref()
            .and_then(|runtime| runtime.replay_store("github"))
            .expect("initial replay store");
        assert!(matches!(
            initial_store
                .reserve(&replay_key, Duration::from_secs(60))
                .await,
            Ok(light_pingora::ReserveOutcome::Reserved(_))
        ));

        std::fs::write(
            external_dir.path().join(light_pingora::HMAC_FILE),
            r#"
enabled: true
pathPrefixAuths:
  - prefix: /webhook
    methods: [POST]
    profile: github
profiles:
  github:
    maxBodyBytes: 2048
    secrets:
      defaultEnvNames: [HOME, PATH]
    replay:
      enabled: true
      idHeader: X-GitHub-Delivery
      store: local
      retentionSeconds: 60
replayStores:
  local:
    type: local
    maxEntries: 4
"#,
        )
        .expect("write reloaded HMAC config");
        let result = config
            .module_registry
            .reload_modules(
                ReloadContext::new(config.clone()),
                &[light_pingora::HMAC_MODULE_ID.to_string()],
            )
            .await;
        assert!(result.failed.is_empty(), "valid HMAC reload failed");
        assert_eq!(
            proxy
                .current_hmac_runtime()
                .as_ref()
                .as_ref()
                .and_then(|runtime| runtime.profile_limits("github")),
            Some((2048, 10_000))
        );
        let reloaded_execution = proxy.current_security_execution();
        assert!(reloaded_execution.generation > pinned_execution.generation);
        let reloaded_runtime = reloaded_execution
            .hmac
            .as_ref()
            .as_ref()
            .expect("reloaded HMAC runtime");
        assert!(
            reloaded_runtime
                .verify("github", &home_headers, body)
                .is_ok()
        );
        assert!(
            reloaded_runtime
                .verify("github", &path_headers, body)
                .is_ok()
        );
        assert!(
            initial_runtime
                .verify("github", &path_headers, body)
                .is_ok()
        );
        assert!(
            initial_runtime
                .verify("github", &home_headers, body)
                .is_err()
        );
        assert_eq!(
            pinned_execution
                .hmac
                .as_ref()
                .as_ref()
                .and_then(|runtime| runtime.profile_limits("github")),
            Some((1024, 10_000))
        );
        assert_eq!(
            reloaded_execution
                .hmac
                .as_ref()
                .as_ref()
                .and_then(|runtime| runtime.profile_limits("github")),
            Some((2048, 10_000))
        );
        let reloaded_store = proxy
            .current_hmac_runtime()
            .as_ref()
            .as_ref()
            .and_then(|runtime| runtime.replay_store("github"))
            .expect("reloaded replay store");
        assert_eq!(
            reloaded_store
                .reserve(&replay_key, Duration::from_secs(60))
                .await
                .unwrap(),
            light_pingora::ReserveOutcome::Duplicate,
            "valid HMAC reload reset local replay history"
        );

        std::fs::write(
            external_dir.path().join(light_pingora::HMAC_FILE),
            r#"
enabled: true
pathPrefixAuths:
  - prefix: /webhook
    methods: [POST]
    profile: github
profiles:
  github:
    maxBodyBytes: 4096
    secrets:
      defaultEnvNames: [LIGHT_HMAC_TEST_SECRET_THAT_DOES_NOT_EXIST]
"#,
        )
        .expect("write invalid HMAC reload");
        let result = config
            .module_registry
            .reload_modules(
                ReloadContext::new(config.clone()),
                &[light_pingora::HMAC_MODULE_ID.to_string()],
            )
            .await;
        assert_eq!(result.failed.len(), 1);
        assert_eq!(
            proxy
                .current_hmac_runtime()
                .as_ref()
                .as_ref()
                .and_then(|runtime| runtime.profile_limits("github")),
            Some((2048, 10_000)),
            "failed HMAC reload replaced the active runtime"
        );
    }

    #[test]
    fn gateway_loads_static_resources_for_virtual_hosts() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        let dist = config_dir.path().join("dist");
        std::fs::create_dir_all(&dist).expect("create dist");
        std::fs::write(dist.join("index.html"), "<html></html>").expect("write index");
        std::fs::write(
            config_dir.path().join(light_pingora::VIRTUAL_HOST_FILE),
            r#"
hosts:
  - domain: local.localhost
    path: /
    base: dist
"#,
        )
        .expect("write virtual host config");
        let config = runtime_config(&config_dir, &external_dir, HashMap::new());

        let proxy = GatewayProxy::from_runtime_config(&config).expect("build proxy");

        assert!(
            proxy
                .current_static_resources()
                .virtual_hosts
                .contains_key("local.localhost")
        );
    }

    #[test]
    fn static_file_validators_emit_http_cache_headers() {
        let config_dir = TempDir::new().expect("config temp dir");
        let file = config_dir.path().join("app.js");
        std::fs::write(&file, "console.log(1);").expect("write static file");
        let metadata = std::fs::metadata(&file).expect("metadata");

        let validators = static_file_validators(&metadata);

        assert!(validators.etag.starts_with("W/\""));
        let last_modified = validators
            .last_modified
            .as_deref()
            .expect("last modified header");
        assert!(parse_http_date(last_modified).is_some());
        assert!(etag_header_matches(
            &format!("\"other\", {}", validators.etag),
            &validators.etag
        ));
    }

    #[test]
    fn static_file_streaming_uses_transfer_threshold() {
        assert!(!should_stream_static_file(1024, 2048));
        assert!(should_stream_static_file(2048, 2048));
        assert!(should_stream_static_file(1, 0));
    }

    #[test]
    fn gateway_loads_router_only_when_router_handler_is_active() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        std::fs::write(
            config_dir.path().join("handler.yml"),
            r#"
handlers:
  - router
defaultHandlers:
  - router
"#,
        )
        .expect("write handler config");
        let config = runtime_config(&config_dir, &external_dir, HashMap::new());

        let proxy = GatewayProxy::from_runtime_config(&config).expect("build proxy");

        let router = proxy.current_router_route();
        assert!(router.as_ref().as_ref().is_some());
        assert!(
            config
                .module_registry
                .module_summaries()
                .iter()
                .any(|entry| entry.module_id == light_pingora::ROUTER_MODULE_ID && entry.active)
        );
    }

    #[test]
    fn gateway_loads_path_prefix_and_token_when_handlers_are_active() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        std::fs::write(
            config_dir.path().join("handler.yml"),
            r#"
handlers:
  - prefix
  - token
defaultHandlers:
  - prefix
  - token
"#,
        )
        .expect("write handler config");
        std::fs::write(
            config_dir
                .path()
                .join(light_pingora::PATH_PREFIX_SERVICE_FILE),
            r#"
enabled: true
mapping:
  /v1/pets: com.networknt.petstore-1.0.0
"#,
        )
        .expect("write path prefix service config");
        std::fs::write(
            config_dir.path().join(light_pingora::TOKEN_FILE),
            r#"
enabled: true
appliedPathPrefixes:
  - /v1
"#,
        )
        .expect("write token config");
        std::fs::write(
            config_dir.path().join(light_pingora::CLIENT_FILE),
            r#"
tls:
  verifyHostname: false
oauth:
  multipleAuthServers: false
  token:
    cache:
      capacity: 4
    server_url: http://localhost:6882
    client_credentials:
      uri: /oauth2/token
      client_id: client
      client_secret: secret
      scope:
        - petstore.r
pathPrefixServices:
  /v1/pets: com.networknt.petstore-1.0.0
request:
  connectTimeout: 100
  timeout: 200
"#,
        )
        .expect("write client config");
        let mut config = runtime_config(&config_dir, &external_dir, HashMap::new());
        config.cache_registry = Some(Arc::new(CacheRegistry::new()));

        let proxy = GatewayProxy::from_runtime_config(&config).expect("build proxy");

        assert_eq!(
            proxy
                .current_path_prefix_service_config()
                .as_ref()
                .as_ref()
                .expect("path prefix config")
                .mapping["/v1/pets"],
            "com.networknt.petstore-1.0.0"
        );
        let token_runtime = proxy.current_token_runtime();
        let token_runtime = token_runtime.as_ref().as_ref().expect("token runtime");
        assert_eq!(token_runtime.client_config().oauth.token.cache.capacity, 4);
        assert_eq!(
            token_runtime
                .handler_config()
                .applied_path_prefixes
                .as_slice(),
            ["/v1".to_string()]
        );
        assert!(
            config
                .module_registry
                .module_summaries()
                .iter()
                .any(|entry| entry.module_id == light_pingora::TOKEN_MODULE_ID && entry.active)
        );
        assert!(
            config
                .cache_registry
                .as_ref()
                .expect("cache registry")
                .names()
                .contains(&light_pingora::TOKEN_CACHE_NAME.to_string())
        );
    }

    #[test]
    fn gateway_loads_stateless_auth_when_stateless_handler_is_active() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        std::fs::write(
            config_dir.path().join("handler.yml"),
            r#"
handlers:
  - cors
  - stateless
paths:
  - path: /authorization
    method: GET
    exec:
      - cors
      - stateless
  - path: /logout
    method: POST
    exec:
      - cors
      - stateless
  - path: /logout
    method: OPTIONS
    exec:
      - cors
      - stateless
defaultHandlers:
  - stateless
"#,
        )
        .expect("write handler config");
        std::fs::write(
            config_dir.path().join(light_pingora::STATELESS_AUTH_FILE),
            r#"
enabled: true
authPath: /authorization
logoutPath: /logout
cookieDomain: localhost
cookieSecure: true
"#,
        )
        .expect("write stateless config");
        std::fs::write(
            config_dir.path().join(light_pingora::CLIENT_FILE),
            r#"
tls:
  verifyHostname: false
oauth:
  token:
    server_url: http://localhost:6882
    authorization_code:
      uri: /oauth2/token
      client_id: ac-client
      client_secret: ac-secret
    refresh_token:
      uri: /oauth2/token
      client_id: rt-client
      client_secret: rt-secret
"#,
        )
        .expect("write client config");
        let config = runtime_config(&config_dir, &external_dir, HashMap::new());

        let proxy = GatewayProxy::from_runtime_config(&config).expect("build proxy");

        let stateless = proxy.current_stateless_auth();
        let stateless = stateless.as_ref().as_ref().expect("stateless runtime");
        assert_eq!(stateless.config().auth_path, "/authorization");
        let active = proxy.active_handlers.load();
        assert!(matches!(
            proxy
                .active_spa_session_endpoint(&active, "/authorization")
                .expect("resolve stateless authorization endpoint"),
            Some(SpaSessionEndpointRoute::StatelessAuthorization)
        ));
        assert!(
            proxy
                .active_spa_session_endpoint(&active, "/google")
                .expect("resolve inactive Google callback")
                .is_none()
        );
        let resolved = active
            .resolve_handler_chain("/logout", "POST")
            .expect("resolve stateless POST logout route");
        assert!(resolved.handler_ids.iter().any(|id| id == "stateless"));
        assert!(
            spa_session_rejection_uses_cors(
                &active,
                "/logout",
                SpaSessionEndpointRoute::StatelessLogout,
            )
            .expect("inspect stateless rejection CORS chain")
        );
        let mut rejection_ctx = GatewayRequestContext::default();
        let cors_headers = CorsResponseHeaders {
            allow_origin: Some("https://portal.example.com".to_string()),
            allow_methods: vec!["POST".to_string(), "OPTIONS".to_string()],
            allow_headers: "Content-Type, Authorization".to_string(),
        };
        assert_eq!(
            capture_cors_outcome(
                &mut rejection_ctx,
                CorsRequestOutcome::Continue(Some(cors_headers))
            ),
            None
        );
        let mut rejection_response =
            ResponseHeader::build(405, Some(8)).expect("build strict-method rejection response");
        proxy
            .apply_response_headers(&mut rejection_response, &rejection_ctx)
            .expect("apply CORS to strict-method rejection response");
        assert_eq!(
            rejection_response
                .headers
                .get("access-control-allow-origin")
                .and_then(|value| value.to_str().ok()),
            Some("https://portal.example.com")
        );
        assert_eq!(
            rejection_response
                .headers
                .get("access-control-allow-credentials")
                .and_then(|value| value.to_str().ok()),
            Some("true")
        );
        let options = active
            .resolve_handler_chain("/logout", "OPTIONS")
            .expect("resolve stateless OPTIONS route");
        assert_eq!(options.handler_ids, vec!["cors", "stateless"]);
        assert!(
            spa_session_method_rejection(SpaSessionEndpointRoute::StatelessLogout, "OPTIONS")
                .is_none()
        );
        assert!(
            spa_session_method_rejection(SpaSessionEndpointRoute::StatelessAuthorization, "GET")
                .is_none()
        );
        let callback_rejection =
            spa_session_method_rejection(SpaSessionEndpointRoute::StatelessAuthorization, "POST")
                .expect("callback POST rejected before default-handler fallback");
        assert_eq!(callback_rejection.status, 405);
        assert!(
            callback_rejection
                .headers
                .contains(&("allow".into(), "GET".into()))
        );
        assert!(
            callback_rejection
                .headers
                .contains(&("cache-control".into(), "no-store".into()))
        );
        for callback in [
            SpaSessionEndpointRoute::GoogleCallback,
            SpaSessionEndpointRoute::FacebookCallback,
            SpaSessionEndpointRoute::GithubCallback,
        ] {
            assert!(spa_session_method_rejection(callback, "GET").is_none());
            let rejection = spa_session_method_rejection(callback, "POST")
                .expect("social callback POST rejected");
            assert!(rejection.headers.contains(&("allow".into(), "GET".into())));
        }
        let legacy_before =
            light_pingora::spa_auth_legacy_get_count(SpaAuthLegacyEndpoint::StatelessLogout);
        let get_rejection =
            spa_session_method_rejection(SpaSessionEndpointRoute::StatelessLogout, "GET")
                .expect("logout GET rejected before default-handler fallback");
        assert_eq!(get_rejection.status, 405);
        assert_eq!(get_rejection.code, "ERR10008");
        assert!(
            get_rejection
                .headers
                .contains(&("allow".into(), "POST".into()))
        );
        assert_eq!(
            light_pingora::spa_auth_legacy_get_count(SpaAuthLegacyEndpoint::StatelessLogout),
            legacy_before + 1
        );
        let logout_rejection =
            spa_session_method_rejection(SpaSessionEndpointRoute::StatelessLogout, "DELETE")
                .expect("logout DELETE rejected before default-handler fallback");
        assert!(
            logout_rejection
                .headers
                .contains(&("allow".into(), "POST".into()))
        );
        assert!(
            config
                .module_registry
                .module_summaries()
                .iter()
                .any(|entry| {
                    entry.module_id == light_pingora::STATELESS_AUTH_MODULE_ID && entry.active
                })
        );
    }

    #[test]
    fn gateway_loads_msal_exchange_when_handler_is_active() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        std::fs::write(
            config_dir.path().join("handler.yml"),
            r#"
handlers:
  - cors
  - msal-exchange
paths:
  - path: /auth/ms/exchange
    method: POST
    exec:
      - cors
      - msal-exchange
  - path: /auth/ms/logout
    method: POST
    exec:
      - cors
      - msal-exchange
  - path: /auth/ms/exchange
    method: OPTIONS
    exec:
      - cors
      - msal-exchange
  - path: /auth/ms/logout
    method: OPTIONS
    exec:
      - cors
      - msal-exchange
defaultHandlers:
  - msal-exchange
"#,
        )
        .expect("write handler config");
        std::fs::write(
            config_dir.path().join(light_pingora::MSAL_EXCHANGE_FILE),
            r#"
enabled: true
exchangePath: /auth/ms/exchange
logoutPath: /auth/ms/logout
subjectTokenType: urn:ietf:params:oauth:token-type:jwt
"#,
        )
        .expect("write msal config");
        std::fs::write(
            config_dir.path().join(light_pingora::SECURITY_MSAL_FILE),
            r#"
enableVerifyJwt: true
issuer: https://login.microsoftonline.com/tenant/v2.0
audience: spa-client
"#,
        )
        .expect("write security-msal config");
        std::fs::write(
            config_dir.path().join(light_pingora::CLIENT_FILE),
            r#"
tls:
  verifyHostname: false
oauth:
  token:
    server_url: http://localhost:6882
    refresh_token:
      uri: /oauth2/token
      client_id: rt-client
      client_secret: rt-secret
    token_exchange:
      uri: /oauth2/token
      client_id: ex-client
      client_secret: ex-secret
"#,
        )
        .expect("write client config");
        let config = runtime_config(&config_dir, &external_dir, HashMap::new());

        let proxy = GatewayProxy::from_runtime_config(&config).expect("build proxy");

        let msal = proxy.current_msal_exchange();
        let msal = msal.as_ref().as_ref().expect("msal runtime");
        assert_eq!(msal.config().exchange_path, "/auth/ms/exchange");
        assert_eq!(
            msal.config().subject_token_type.as_deref(),
            Some("urn:ietf:params:oauth:token-type:jwt")
        );
        let active = proxy.active_handlers.load();
        for path in ["/auth/ms/exchange", "/auth/ms/logout"] {
            let route = if path.ends_with("exchange") {
                SpaSessionEndpointRoute::Exchange
            } else {
                SpaSessionEndpointRoute::ExchangeLogout
            };
            let resolved = active
                .resolve_handler_chain(path, "POST")
                .expect("resolve strict POST route");
            assert!(resolved.handler_ids.iter().any(|id| id == "msal-exchange"));
            assert!(
                spa_session_rejection_uses_cors(&active, path, route)
                    .expect("inspect MSAL exchange rejection CORS chain")
            );
            let get_rejection = spa_session_method_rejection(route, "GET")
                .expect("GET rejected before default-handler fallback");
            assert_eq!(get_rejection.status, 405);
            assert_eq!(get_rejection.code, "ERR10008");
            assert!(
                get_rejection
                    .headers
                    .contains(&("allow".into(), "POST".into()))
            );
            let options = active
                .resolve_handler_chain(path, "OPTIONS")
                .expect("resolve OPTIONS route");
            assert_eq!(options.handler_ids, vec!["cors", "msal-exchange"]);
        }
        assert!(
            spa_session_method_rejection(SpaSessionEndpointRoute::Exchange, "OPTIONS").is_none()
        );
        let rejection = spa_session_method_rejection(SpaSessionEndpointRoute::Exchange, "DELETE")
            .expect("DELETE rejected before route fallback");
        assert_eq!(rejection.status, 405);
        assert_eq!(rejection.code, "ERR10008");
        assert!(rejection.headers.contains(&("allow".into(), "POST".into())));
        assert!(
            rejection
                .headers
                .contains(&("cache-control".into(), "no-store".into()))
        );
        assert!(
            config
                .module_registry
                .module_summaries()
                .iter()
                .any(|entry| {
                    entry.module_id == light_pingora::MSAL_EXCHANGE_MODULE_ID && entry.active
                })
        );
    }

    #[test]
    fn response_framing_suppresses_body_and_length_only_for_no_content() {
        assert!(!status_allows_content_length(204));
        assert!(!should_write_response_body(204, false));
        assert!(status_allows_content_length(200));
        assert!(should_write_response_body(200, false));
        assert!(!should_write_response_body(200, true));
    }

    #[test]
    fn buffered_embedding_drain_deadline_preserves_configured_rate_floor() {
        let body_bytes = 25 * 1024 * 1024;
        let deadline = buffered_embedding_drain_deadline(body_bytes, Duration::from_secs(30), 1024);
        assert_eq!(deadline, Duration::from_secs(25_630));
        assert!(deadline > Duration::from_secs(30));
    }

    #[test]
    fn spa_session_rejection_captures_cors_headers_before_response() {
        let headers = CorsResponseHeaders {
            allow_origin: Some("https://portal.example.com".to_string()),
            allow_methods: vec!["POST".to_string(), "OPTIONS".to_string()],
            allow_headers: "Content-Type, Authorization".to_string(),
        };
        let mut ctx = GatewayRequestContext::default();

        assert_eq!(
            capture_cors_outcome(
                &mut ctx,
                CorsRequestOutcome::Continue(Some(headers.clone()))
            ),
            None
        );
        assert_eq!(ctx.cors, Some(headers.clone()));

        assert_eq!(
            capture_cors_outcome(
                &mut ctx,
                CorsRequestOutcome::Respond {
                    status: 403,
                    headers: headers.clone(),
                },
            ),
            Some(403)
        );
        assert_eq!(ctx.cors, Some(headers));
    }

    #[test]
    fn gateway_loads_msal_auth_when_handler_is_active() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        std::fs::write(
            config_dir.path().join("handler.yml"),
            r#"
handlers:
  - cors
  - msal-auth
paths:
  - path: /auth/ms/login
    method: POST
    exec:
      - cors
      - msal-auth
  - path: /auth/ms/logout
    method: POST
    exec:
      - cors
      - msal-auth
  - path: /auth/ms/login
    method: OPTIONS
    exec:
      - cors
      - msal-auth
  - path: /auth/ms/logout
    method: OPTIONS
    exec:
      - cors
      - msal-auth
  - path: /**
    method: GET
    exec:
      - msal-auth
defaultHandlers:
  - msal-auth
"#,
        )
        .expect("write handler config");
        std::fs::write(
            config_dir.path().join(light_pingora::MSAL_AUTH_FILE),
            r#"
enabled: true
loginPath: /auth/ms/login
logoutPath: /auth/ms/logout
sessionTimeout: 1200
"#,
        )
        .expect("write msal-auth config");
        std::fs::write(
            config_dir.path().join(light_pingora::SECURITY_MSAL_FILE),
            r#"
enableVerifyJwt: true
issuer: https://login.microsoftonline.com/tenant/v2.0
audience: spa-client
"#,
        )
        .expect("write security-msal config");
        let config = runtime_config(&config_dir, &external_dir, HashMap::new());

        let proxy = GatewayProxy::from_runtime_config(&config).expect("build proxy");

        let msal = proxy.current_msal_auth();
        let msal = msal.as_ref().as_ref().expect("msal auth runtime");
        assert_eq!(msal.config.login_path, "/auth/ms/login");
        assert_eq!(msal.config.session_timeout, 1200);
        let active = proxy.active_handlers.load();
        let resolved = active
            .resolve_handler_chain("/auth/ms/logout", "POST")
            .expect("resolve msal-auth POST logout route");
        assert!(resolved.handler_ids.iter().any(|id| id == "msal-auth"));
        assert!(
            spa_session_rejection_uses_cors(
                &active,
                "/auth/ms/logout",
                SpaSessionEndpointRoute::AuthLogout,
            )
            .expect("inspect MSAL auth rejection CORS chain")
        );
        let get_rejection =
            spa_session_method_rejection(SpaSessionEndpointRoute::AuthLogout, "GET")
                .expect("MSAL logout GET rejected before default-handler fallback");
        assert_eq!(get_rejection.status, 405);
        assert_eq!(get_rejection.code, "ERR10008");
        assert!(
            get_rejection
                .headers
                .contains(&("allow".into(), "POST".into()))
        );
        for path in ["/auth/ms/login", "/auth/ms/logout"] {
            let options = active
                .resolve_handler_chain(path, "OPTIONS")
                .expect("resolve MSAL OPTIONS route");
            assert_eq!(options.handler_ids, vec!["cors", "msal-auth"]);
        }
        assert!(spa_session_method_rejection(SpaSessionEndpointRoute::AuthLogin, "GET").is_some());
        assert!(
            spa_session_method_rejection(SpaSessionEndpointRoute::AuthLogin, "OPTIONS").is_none()
        );
        assert!(
            config
                .module_registry
                .module_summaries()
                .iter()
                .any(|entry| entry.module_id == light_pingora::MSAL_AUTH_MODULE_ID && entry.active)
        );
    }

    #[tokio::test]
    async fn gateway_uses_canonical_chain_when_msal_runtimes_share_logout_path() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        std::fs::write(
            config_dir.path().join("handler.yml"),
            r#"
handlers:
  - cors
  - msal-exchange
  - msal-auth
paths:
  - path: /auth/ms/exchange
    method: POST
    exec:
      - cors
      - msal-exchange
  - path: /auth/ms/logout
    method: POST
    exec:
      - cors
      - msal-auth
  - path: /auth/ms/exchange
    method: OPTIONS
    exec:
      - cors
      - msal-exchange
  - path: /auth/ms/logout
    method: OPTIONS
    exec:
      - cors
      - msal-auth
defaultHandlers: []
"#,
        )
        .expect("write handler config");
        std::fs::write(
            config_dir.path().join(light_pingora::MSAL_EXCHANGE_FILE),
            r#"
enabled: true
exchangePath: /auth/ms/exchange
logoutPath: /auth/ms/logout
subjectTokenType: urn:ietf:params:oauth:token-type:jwt
"#,
        )
        .expect("write msal-exchange config");
        std::fs::write(
            config_dir.path().join(light_pingora::MSAL_AUTH_FILE),
            r#"
enabled: true
loginPath: /auth/ms/login
logoutPath: /auth/ms/logout
sessionTimeout: 1200
"#,
        )
        .expect("write msal-auth config");
        std::fs::write(
            config_dir.path().join(light_pingora::SECURITY_MSAL_FILE),
            r#"
enableVerifyJwt: true
issuer: https://login.microsoftonline.com/tenant/v2.0
audience: spa-client
"#,
        )
        .expect("write security-msal config");
        std::fs::write(
            config_dir.path().join(light_pingora::CLIENT_FILE),
            r#"
tls:
  verifyHostname: false
oauth:
  token:
    server_url: http://localhost:6882
    refresh_token:
      uri: /oauth2/token
      client_id: rt-client
      client_secret: rt-secret
    token_exchange:
      uri: /oauth2/token
      client_id: ex-client
      client_secret: ex-secret
"#,
        )
        .expect("write client config");
        std::fs::write(
            config_dir.path().join(light_pingora::CORS_FILE),
            r#"
enabled: true
allowedOrigins:
  - https://portal.example.com
allowedMethods:
  - POST
  - OPTIONS
"#,
        )
        .expect("write CORS config");
        let config = runtime_config(&config_dir, &external_dir, HashMap::new());
        let proxy = GatewayProxy::from_runtime_config(&config).expect("build proxy");
        let active = proxy.active_handlers.load();

        assert!(matches!(
            proxy
                .active_spa_session_endpoint(&active, "/auth/ms/logout")
                .expect("resolve shared MSAL logout path"),
            Some(SpaSessionEndpointRoute::AuthLogout)
        ));
        assert!(matches!(
            proxy
                .active_spa_session_endpoint(&active, "/auth/ms/login")
                .expect("classify configured MSAL login without a canonical route"),
            Some(SpaSessionEndpointRoute::AuthLogin)
        ));

        let (mut client, server) = tokio::io::duplex(16 * 1024);
        client
            .write_all(
                b"GET /auth/ms/logout HTTP/1.1\r\nHost: localhost\r\nOrigin: https://portal.example.com\r\nConnection: close\r\n\r\n",
            )
            .await
            .expect("write legacy GET request");
        let mut session = Session::new_h1(Box::new(server));
        assert!(
            session
                .as_downstream_mut()
                .read_request()
                .await
                .expect("parse legacy GET request")
        );
        let mut ctx = proxy.new_ctx();
        assert!(
            proxy
                .request_filter(&mut session, &mut ctx)
                .await
                .expect("reject legacy GET")
        );
        assert!(ctx.handler_ids.is_empty(), "auth chain must remain blocked");

        drop(session);
        let mut wire = Vec::new();
        client
            .read_to_end(&mut wire)
            .await
            .expect("read strict-method response");
        let response = String::from_utf8(wire).expect("HTTP response is UTF-8");
        let response_lower = response.to_ascii_lowercase();
        assert!(response_lower.starts_with("http/1.1 405"), "{response}");
        assert!(response_lower.contains("allow: post\r\n"), "{response}");
        assert!(
            response_lower.contains("access-control-allow-origin: https://portal.example.com\r\n"),
            "{response}"
        );
        assert!(
            response_lower.contains("access-control-allow-credentials: true\r\n"),
            "{response}"
        );
        assert!(
            response_lower.contains("cache-control: no-store\r\n"),
            "{response}"
        );
        assert!(response.contains("\"code\":\"ERR10008\""), "{response}");
        assert!(!response_lower.contains("set-cookie:"), "{response}");
    }

    #[test]
    fn gateway_disables_msal_auth_without_security_msal_config() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        std::fs::write(
            config_dir.path().join("handler.yml"),
            r#"
handlers:
  - msal-auth
paths:
  - path: /auth/ms/login
    method: POST
    exec:
      - msal-auth
defaultHandlers:
  - msal-auth
"#,
        )
        .expect("write handler config");
        std::fs::write(
            config_dir.path().join(light_pingora::MSAL_AUTH_FILE),
            r#"
enabled: false
"#,
        )
        .expect("write disabled msal-auth config");
        let config = runtime_config(&config_dir, &external_dir, HashMap::new());

        let proxy = GatewayProxy::from_runtime_config(&config).expect("build proxy");

        assert!(proxy.current_msal_auth().as_ref().is_none());
        assert!(
            config
                .module_registry
                .module_summaries()
                .iter()
                .any(|entry| {
                    entry.module_id == light_pingora::MSAL_AUTH_MODULE_ID && !entry.active
                })
        );
    }

    #[test]
    fn gateway_loads_mcp_router_when_mcp_handler_is_active() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        std::fs::write(
            config_dir.path().join("handler.yml"),
            r#"
handlers:
  - mcp
paths:
  - path: /mcp
    method: POST
    exec:
      - mcp
defaultHandlers: []
"#,
        )
        .expect("write handler config");
        std::fs::write(
            config_dir.path().join(light_pingora::MCP_ROUTER_FILE),
            r#"
enabled: true
path: /mcp
tools:
  - name: weather
    description: Get weather.
    targetHost: http://127.0.0.1:8080
    path: /weather
    method: GET
"#,
        )
        .expect("write mcp config");
        let config = runtime_config(&config_dir, &external_dir, HashMap::new());

        let proxy = GatewayProxy::from_runtime_config(&config).expect("build proxy");

        let mcp = proxy.current_mcp_router();
        let mcp = mcp.as_ref().as_ref().expect("mcp runtime");
        assert!(mcp.matches_path("/mcp"));
        assert_eq!(mcp.config().tools[0].name, "weather");
        assert!(
            config
                .module_registry
                .module_summaries()
                .iter()
                .any(
                    |entry| entry.module_id == light_pingora::MCP_ROUTER_MODULE_ID && entry.active
                )
        );
    }

    #[test]
    fn gateway_loads_websocket_router_when_websocket_handler_is_active() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        std::fs::write(
            config_dir.path().join("handler.yml"),
            r#"
handlers:
  - websocket
paths:
  - path: /chat
    method: GET
    exec:
      - websocket
defaultHandlers: []
"#,
        )
        .expect("write handler config");
        std::fs::write(
            config_dir.path().join(light_pingora::WEBSOCKET_ROUTER_FILE),
            r#"
defaultProtocol: https
defaultEnvTag: dev
pathPrefixService:
  /chat:
    serviceId: com.networknt.llmchat-1.0.0
    protocol: http
"#,
        )
        .expect("write websocket config");
        let config = runtime_config(&config_dir, &external_dir, HashMap::new());

        let proxy = GatewayProxy::from_runtime_config(&config).expect("build proxy");

        let websocket = proxy.current_websocket_router();
        let websocket = websocket.as_ref().as_ref().expect("websocket runtime");
        assert_eq!(
            websocket.config().path_prefix_service["/chat"].service_id,
            "com.networknt.llmchat-1.0.0"
        );
        assert!(
            config
                .module_registry
                .module_summaries()
                .iter()
                .any(
                    |entry| entry.module_id == light_pingora::WEBSOCKET_ROUTER_MODULE_ID
                        && entry.active
                )
        );
    }

    #[tokio::test]
    async fn gateway_reload_swaps_live_mcp_router_config() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        std::fs::write(
            config_dir.path().join("handler.yml"),
            r#"
handlers:
  - mcp
paths:
  - path: /mcp
    method: POST
    exec:
      - mcp
defaultHandlers: []
"#,
        )
        .expect("write handler config");
        std::fs::write(
            config_dir.path().join(light_pingora::MCP_ROUTER_FILE),
            r#"
enabled: true
path: /mcp
tools:
  - name: weather
    targetHost: http://127.0.0.1:8080
    path: /weather
"#,
        )
        .expect("write mcp config");
        let config = runtime_config(&config_dir, &external_dir, HashMap::new());
        let proxy = GatewayProxy::from_runtime_config(&config).expect("build proxy");

        assert_eq!(
            proxy
                .current_mcp_router()
                .as_ref()
                .as_ref()
                .expect("mcp runtime")
                .config()
                .tools[0]
                .name,
            "weather"
        );

        std::fs::write(
            external_dir.path().join(light_pingora::MCP_ROUTER_FILE),
            r#"
enabled: true
path: /mcp
tools:
  - name: forecast
    targetHost: http://127.0.0.1:8081
    path: /forecast
"#,
        )
        .expect("write external mcp config");

        let result = config
            .module_registry
            .reload_modules(
                ReloadContext::new(config.clone()),
                &[light_pingora::MCP_ROUTER_MODULE_ID.to_string()],
            )
            .await;

        assert_eq!(result.reloaded, vec![light_pingora::MCP_ROUTER_MODULE_ID]);
        assert!(result.skipped.is_empty());
        assert!(result.failed.is_empty());
        assert_eq!(
            proxy
                .current_mcp_router()
                .as_ref()
                .as_ref()
                .expect("mcp runtime")
                .config()
                .tools[0]
                .name,
            "forecast"
        );
    }

    #[tokio::test]
    async fn gateway_mcp_session_survives_config_reload() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        std::fs::write(
            config_dir.path().join("handler.yml"),
            r#"
handlers:
  - mcp
paths:
  - path: /mcp
    method: POST
    exec:
      - mcp
defaultHandlers: []
"#,
        )
        .expect("write handler config");
        std::fs::write(
            config_dir.path().join(light_pingora::MCP_ROUTER_FILE),
            r#"
enabled: true
path: /mcp
tools:
  - name: weather
    targetHost: http://127.0.0.1:8080
    path: /weather
"#,
        )
        .expect("write mcp config");
        let config = runtime_config(&config_dir, &external_dir, HashMap::new());
        let proxy = GatewayProxy::from_runtime_config(&config).expect("build proxy");

        // --- Initialize a session before the reload ---
        let mcp = proxy.current_mcp_router();
        let mcp = mcp.as_ref().as_ref().expect("mcp runtime");
        let request_context = || light_pingora::McpRequestContext {
            anonymous_binding: Some("test-peer:192.0.2.1".to_string()),
            ..light_pingora::McpRequestContext::default()
        };
        let init_response = mcp
            .handle_request_with_context(
                light_pingora::McpHttpRequest {
                    method: "POST".to_string(),
                    path: "/mcp".to_string(),
                    headers: vec![("accept".to_string(), "application/json".to_string())],
                    body: br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}"#
                        .to_vec(),
                },
                request_context(),
            )
            .await
            .expect("handle")
            .expect("response");
        assert_eq!(init_response.status, 200, "initialize must succeed");
        let session_id = init_response
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(light_pingora::MCP_SESSION_ID_HEADER))
            .map(|(_, v)| v.clone())
            .expect("session id header after initialize");

        // --- Reload the MCP router config (simulates /reload endpoint) ---
        std::fs::write(
            external_dir.path().join(light_pingora::MCP_ROUTER_FILE),
            r#"
enabled: true
path: /mcp
tools:
  - name: forecast
    targetHost: http://127.0.0.1:8081
    path: /forecast
"#,
        )
        .expect("write updated mcp config");
        let result = config
            .module_registry
            .reload_modules(
                ReloadContext::new(config.clone()),
                &[light_pingora::MCP_ROUTER_MODULE_ID.to_string()],
            )
            .await;
        assert!(result.failed.is_empty(), "reload must not fail");

        // --- Verify that the existing session survives the reload ---
        let mcp_after = proxy.current_mcp_router();
        let mcp_after = mcp_after
            .as_ref()
            .as_ref()
            .expect("mcp runtime after reload");
        // Config swap must have happened.
        assert_eq!(mcp_after.config().tools[0].name, "forecast");

        // tools/list with the original session ID must still succeed.
        let tools_response = mcp_after
            .handle_request_with_context(
                light_pingora::McpHttpRequest {
                    method: "POST".to_string(),
                    path: "/mcp".to_string(),
                    headers: vec![
                        ("accept".to_string(), "application/json".to_string()),
                        (
                            light_pingora::MCP_SESSION_ID_HEADER.to_string(),
                            session_id.clone(),
                        ),
                    ],
                    body: br#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#.to_vec(),
                },
                request_context(),
            )
            .await
            .expect("handle")
            .expect("response");
        assert_eq!(
            tools_response.status, 200,
            "tools/list must succeed with pre-reload session after config reload"
        );
    }

    #[tokio::test]
    async fn gateway_client_config_reload() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");

        // Write initial client config with verifyHostname: false
        std::fs::write(
            config_dir.path().join("client.yml"),
            r#"
tls:
  verifyHostname: false
"#,
        )
        .expect("write client config");

        let config = runtime_config(&config_dir, &external_dir, HashMap::new());
        config
            .module_registry
            .register_runtime_configs(&config)
            .expect("register configs");
        let _proxy = GatewayProxy::from_runtime_config(&config).expect("build proxy");

        // Verify initial value in component configs
        let component_configs = config.module_registry.component_configs();
        assert_eq!(component_configs["client"]["tls"]["verifyHostname"], false);

        // Update client config on disk in external dir with verifyHostname: true
        std::fs::write(
            external_dir.path().join("client.yml"),
            r#"
tls:
  verifyHostname: true
"#,
        )
        .expect("write updated client config");

        // Reload the client module
        let reload_ctx = config.reload_context().await.expect("reload context");
        let result = config
            .module_registry
            .reload_modules(reload_ctx, &[light_runtime::CLIENT_MODULE_ID.to_string()])
            .await;

        assert_eq!(result.reloaded, vec![light_runtime::CLIENT_MODULE_ID]);
        assert!(result.skipped.is_empty());
        assert!(result.failed.is_empty());

        // Verify updated value in component configs
        let updated_configs = config.module_registry.component_configs();
        assert_eq!(updated_configs["client"]["tls"]["verifyHostname"], true);
    }

    #[tokio::test]
    async fn gateway_reload_swaps_live_websocket_router_config() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        std::fs::write(
            config_dir.path().join("handler.yml"),
            r#"
handlers:
  - websocket
paths:
  - path: /chat
    method: GET
    exec:
      - websocket
defaultHandlers: []
"#,
        )
        .expect("write handler config");
        std::fs::write(
            config_dir.path().join(light_pingora::WEBSOCKET_ROUTER_FILE),
            r#"
pathPrefixService:
  /chat: com.networknt.llmchat-1.0.0
"#,
        )
        .expect("write websocket config");
        let config = runtime_config(&config_dir, &external_dir, HashMap::new());
        let proxy = GatewayProxy::from_runtime_config(&config).expect("build proxy");

        assert_eq!(
            proxy
                .current_websocket_router()
                .as_ref()
                .as_ref()
                .expect("websocket runtime")
                .config()
                .path_prefix_service["/chat"]
                .service_id,
            "com.networknt.llmchat-1.0.0"
        );

        std::fs::write(
            external_dir
                .path()
                .join(light_pingora::WEBSOCKET_ROUTER_FILE),
            r#"
pathPrefixService:
  /chat: com.networknt.chat-v2-1.0.0
"#,
        )
        .expect("write external websocket config");

        let result = config
            .module_registry
            .reload_modules(
                ReloadContext::new(config.clone()),
                &[light_pingora::WEBSOCKET_ROUTER_MODULE_ID.to_string()],
            )
            .await;

        assert_eq!(
            result.reloaded,
            vec![light_pingora::WEBSOCKET_ROUTER_MODULE_ID]
        );
        assert!(result.skipped.is_empty());
        assert!(result.failed.is_empty());
        assert_eq!(
            proxy
                .current_websocket_router()
                .as_ref()
                .as_ref()
                .expect("websocket runtime")
                .config()
                .path_prefix_service["/chat"]
                .service_id,
            "com.networknt.chat-v2-1.0.0"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn websocket_gateway_proxies_text_binary_close_subprotocol_and_headers() {
        let (backend_address, observed_backend, backend_task) =
            spawn_websocket_echo_backend().await;
        let (registry_url, lookup_rx, registry_task) = spawn_fake_registry(backend_address).await;
        let registry_client = Arc::new(
            PortalRegistryClient::new(
                registry_url.as_str(),
                ServiceRegistrationParams {
                    service_id: "light-gateway-test".to_string(),
                    version: "1.0.0".to_string(),
                    protocol: "http".to_string(),
                    address: "127.0.0.1".to_string(),
                    port: 0,
                    tags: HashMap::new(),
                    env_tag: Some("dev".to_string()),
                    jwt: "test-token".to_string(),
                },
                Arc::new(NoopRegistryHandler),
            )
            .expect("build registry client"),
        );
        let mut registration_rx = registry_client.subscribe_registration();
        let registry_client_task = tokio::spawn({
            let registry_client = Arc::clone(&registry_client);
            async move { registry_client.run().await }
        });
        wait_for_registry_registration(&mut registration_rx).await;

        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        let gateway_port = free_tcp_port();
        let gateway_address = format!("127.0.0.1:{gateway_port}")
            .parse::<std::net::SocketAddr>()
            .expect("gateway address");
        std::fs::write(
            config_dir.path().join("server.yml"),
            format!(
                r#"
ip: 127.0.0.1
advertisedAddress: 127.0.0.1
httpPort: {gateway_port}
enableHttp: true
httpsPort: 8443
enableHttps: false
serviceId: com.networknt.light-gateway-1.0.0
enableRegistry: false
startOnRegistryFailure: true
dynamicPort: false
environment: dev
shutdownGracefulPeriod: 100
"#
            ),
        )
        .expect("write server config");
        std::fs::write(
            config_dir.path().join("handler.yml"),
            r#"
handlers:
  - websocket
paths:
  - path: /chat
    method: GET
    exec:
      - websocket
defaultHandlers: []
"#,
        )
        .expect("write handler config");
        std::fs::write(
            config_dir.path().join(light_pingora::WEBSOCKET_ROUTER_FILE),
            r#"
defaultProtocol: http
defaultEnvTag: dev
pathPrefixService:
  /chat:
    serviceId: com.networknt.llmchat-1.0.0
    protocol: http
    envTag: dev
"#,
        )
        .expect("write websocket config");

        let runtime = LightRuntimeBuilder::new(PingoraTransport::new(GatewayApp::default()))
            .with_config_dir(config_dir.path())
            .with_external_config_dir(external_dir.path())
            .with_registry_client(Arc::clone(&registry_client))
            .build();
        let running = runtime.start().await.expect("start gateway");
        wait_for_tcp(gateway_address).await;

        let mut request = format!(
            "ws://127.0.0.1:{gateway_port}/chat?service_id=com.networknt.llmchat-1.0.0&protocol=http&env_tag=dev&room=one"
        )
        .into_client_request()
        .expect("websocket client request");
        request.headers_mut().insert(
            "sec-websocket-protocol",
            HeaderValue::from_static("chat.v1"),
        );
        request.headers_mut().insert(
            "authorization",
            HeaderValue::from_static("Bearer agent-token"),
        );
        request
            .headers_mut()
            .insert("x-agent-test", HeaderValue::from_static("present"));
        request.headers_mut().insert(
            "service_id",
            HeaderValue::from_static("com.networknt.llmchat-1.0.0"),
        );

        let (mut websocket, response) =
            timeout(TokioDuration::from_secs(5), connect_async(request))
                .await
                .expect("connect timeout")
                .expect("connect through gateway");
        assert_eq!(
            response
                .headers()
                .get("sec-websocket-protocol")
                .and_then(|value| value.to_str().ok()),
            Some("chat.v1")
        );

        let lookup = timeout(TokioDuration::from_secs(5), lookup_rx)
            .await
            .expect("lookup timeout")
            .expect("lookup payload");
        assert_eq!(lookup["method"], "discovery/lookup");
        assert_eq!(lookup["params"]["serviceId"], "com.networknt.llmchat-1.0.0");
        assert_eq!(lookup["params"]["envTag"], "dev");
        assert_eq!(lookup["params"]["protocol"], "http");

        let observed = observed_backend
            .lock()
            .expect("observed backend lock")
            .clone()
            .expect("backend handshake observed");
        assert_eq!(observed.path_and_query, "/chat?room=one");
        assert_eq!(
            observed.authorization.as_deref(),
            Some("Bearer agent-token")
        );
        assert_eq!(observed.agent_header.as_deref(), Some("present"));
        assert_eq!(observed.service_id_header, None);
        assert!(
            observed
                .subprotocol
                .as_deref()
                .is_some_and(|value| websocket_protocol_contains(value, "chat.v1"))
        );

        websocket
            .send(Message::Text("hello".into()))
            .await
            .expect("send text");
        let text = timeout(TokioDuration::from_secs(5), websocket.next())
            .await
            .expect("text timeout")
            .expect("text frame")
            .expect("valid text frame")
            .into_text()
            .expect("text payload");
        assert_eq!(text, "echo:hello");

        websocket
            .send(Message::Binary(vec![1_u8, 2, 3, 4].into()))
            .await
            .expect("send binary");
        let binary = timeout(TokioDuration::from_secs(5), websocket.next())
            .await
            .expect("binary timeout")
            .expect("binary frame")
            .expect("valid binary frame")
            .into_data();
        assert_eq!(binary.as_slice(), &[1_u8, 2, 3, 4]);

        websocket.close(None).await.expect("close websocket");
        timeout(TokioDuration::from_secs(5), async {
            while let Some(message) = websocket.next().await {
                match message {
                    Ok(Message::Close(_)) => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        })
        .await
        .expect("close timeout");
        timeout(TokioDuration::from_secs(5), backend_task)
            .await
            .expect("backend close timeout")
            .expect("backend task");

        running.shutdown().await.expect("shutdown gateway");
        registry_client_task.abort();
        registry_task.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn mcp_subscription_streams_ack_before_terminal_over_live_pingora() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        let gateway_port = free_tcp_port();
        let gateway_address = format!("127.0.0.1:{gateway_port}")
            .parse::<std::net::SocketAddr>()
            .expect("gateway address");
        std::fs::write(
            config_dir.path().join("server.yml"),
            format!(
                r#"
ip: 127.0.0.1
advertisedAddress: 127.0.0.1
httpPort: {gateway_port}
enableHttp: true
httpsPort: 8443
enableHttps: false
serviceId: com.networknt.light-gateway-1.0.0
enableRegistry: false
startOnRegistryFailure: true
dynamicPort: false
environment: dev
shutdownGracefulPeriod: 100
"#
            ),
        )
        .expect("write server config");
        std::fs::write(
            config_dir.path().join("handler.yml"),
            r#"
handlers:
  - mcp
paths:
  - path: /mcp
    method: POST
    exec:
      - mcp
defaultHandlers: []
"#,
        )
        .expect("write handler config");
        std::fs::write(
            config_dir.path().join(light_pingora::MCP_ROUTER_FILE),
            r#"
enabled: true
path: /mcp
protocols:
  legacy:
    enabled: true
    versions: ["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"]
  stateless:
    enabled: true
    versions: ["2026-07-28"]
    maxSubscriptionDurationMs: 2000
tools: []
"#,
        )
        .expect("write mcp config");

        let runtime = LightRuntimeBuilder::new(PingoraTransport::new(GatewayApp::default()))
            .with_config_dir(config_dir.path())
            .with_external_config_dir(external_dir.path())
            .build();
        let running = runtime.start().await.expect("start gateway");
        wait_for_tcp(gateway_address).await;

        let body = json!({
            "jsonrpc": "2.0",
            "id": "live-subscription",
            "method": "subscriptions/listen",
            "params": {
                "notifications": {"toolsListChanged": true},
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientInfo": {"name":"phase8-live","version":"1"},
                    "io.modelcontextprotocol/clientCapabilities": {}
                }
            }
        })
        .to_string();
        let request = format!(
            "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{gateway_port}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nMCP-Protocol-Version: 2026-07-28\r\nMcp-Method: subscriptions/listen\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let mut client = TcpStream::connect(gateway_address)
            .await
            .expect("connect gateway");
        client
            .write_all(request.as_bytes())
            .await
            .expect("write request");
        let mut received = Vec::new();
        timeout(TokioDuration::from_secs(1), async {
            let mut chunk = [0_u8; 4096];
            loop {
                let read = client.read(&mut chunk).await.expect("read acknowledgment");
                assert!(read > 0, "stream closed before acknowledgment");
                received.extend_from_slice(&chunk[..read]);
                if String::from_utf8_lossy(&received)
                    .contains("notifications/subscriptions/acknowledged")
                {
                    break;
                }
            }
        })
        .await
        .expect("acknowledgment was buffered");
        let first_delivery = String::from_utf8_lossy(&received);
        assert!(first_delivery.contains("x-accel-buffering: no"));
        assert!(!first_delivery.contains("\"resultType\":\"complete\""));

        timeout(TokioDuration::from_secs(3), async {
            let mut chunk = [0_u8; 4096];
            loop {
                let read = client.read(&mut chunk).await.expect("read terminal");
                assert!(read > 0, "stream closed before terminal result");
                received.extend_from_slice(&chunk[..read]);
                if String::from_utf8_lossy(&received).contains("\"resultType\":\"complete\"") {
                    break;
                }
            }
        })
        .await
        .expect("terminal result timeout");

        running.shutdown().await.expect("shutdown gateway");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn llm_sse_smoke_streams_openai_frames_over_live_pingora() {
        let provider_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock provider");
        let provider_address = provider_listener.local_addr().expect("provider address");
        let provider_task = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut socket, _) = provider_listener.accept().await.expect("provider accept");
                let mut request = vec![0_u8; 8192];
                let read = socket.read(&mut request).await.expect("provider read");
                assert!(String::from_utf8_lossy(&request[..read]).contains("\"stream\":true"));
                let body = concat!(
                    "data: {\"id\":\"mock\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"},\"finish_reason\":null}]}\n\n",
                    "data: {\"id\":\"mock\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                    "data: {\"id\":\"mock\",\"object\":\"chat.completion.chunk\",\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":1,\"total_tokens\":4}}\n\n",
                    "data: [DONE]\n\n"
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("provider write");
            }
        });

        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        let gateway_port = free_tcp_port();
        let gateway_address = format!("127.0.0.1:{gateway_port}")
            .parse::<std::net::SocketAddr>()
            .expect("gateway address");
        std::fs::write(
            config_dir.path().join("server.yml"),
            format!(
                "ip: 127.0.0.1\nadvertisedAddress: 127.0.0.1\nhttpPort: {gateway_port}\nenableHttp: true\nhttpsPort: 8443\nenableHttps: false\nserviceId: com.networknt.light-gateway-1.0.0\nenableRegistry: false\nstartOnRegistryFailure: true\ndynamicPort: false\nenvironment: dev\nshutdownGracefulPeriod: 100\n"
            ),
        )
        .expect("write server config");
        std::fs::write(
            config_dir.path().join("handler.yml"),
            r#"
handlers: [correlation, unified-security, limit, access-control, llm]
paths:
  - path: /v1/chat/completions
    method: POST
    exec: [correlation, unified-security, limit, access-control, llm]
  - path: /v1/responses
    method: POST
    exec: [correlation, unified-security, limit, access-control, llm]
defaultHandlers: []
"#,
        )
        .expect("write handler config");
        std::fs::write(
            config_dir.path().join(light_pingora::UNIFIED_SECURITY_FILE),
            "enabled: true\nanonymousPrefixes: [/v1/chat/completions, /v1/responses]\npathPrefixAuths: []\n",
        )
        .expect("write unified security config");
        std::fs::write(
            config_dir.path().join(light_pingora::ACCESS_CONTROL_FILE),
            "enabled: true\ndefaultDeny: false\n",
        )
        .expect("write access-control config");
        std::fs::write(
            config_dir.path().join(LLM_ROUTER_FILE),
            format!(
                r#"
enabled: true
developmentFixtures: true
globalConcurrency: 4
globalStreamConcurrency: 1
streamChannelCapacity: 1
streamWriteTimeoutMs: 1000
providers:
  mock:
    providerProtocol: openai_chat
    materialGeneration: 1
    baseUrl: http://{provider_address}/v1
    endpointAuth:
      mode: bearer
      credential_ref: env:LIGHT_GATEWAY_LF6B_TEST_KEY
deployments:
  mock:
    provider: mock
    model: mock-model
    concurrency: 1
    prices:
      generate:
        operation: generate
        version: 1
        inputMicrosPerMillion: 1
        outputMicrosPerMillion: 1
    conformanceDigest: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
aliases:
  public-model:
    operations: [generate]
    deployments: [mock]
    maxAttempts: 1
    concurrency: 1
    maxInputTokens: 1000
    maxOutputTokens: 100
    maxCostMicros: 1000
    audit: disabled
"#
            ),
        )
        .expect("write LLM config");
        // SAFETY: the test uses a unique process-local variable and removes it
        // immediately after off-path client construction.
        unsafe { std::env::set_var("LIGHT_GATEWAY_LF6B_TEST_KEY", "test-key") };
        let runtime = LightRuntimeBuilder::new(PingoraTransport::new(GatewayApp::default()))
            .with_config_dir(config_dir.path())
            .with_external_config_dir(external_dir.path())
            .build();
        let running = runtime.start().await.expect("start gateway");
        unsafe { std::env::remove_var("LIGHT_GATEWAY_LF6B_TEST_KEY") };
        wait_for_tcp(gateway_address).await;

        let body = r#"{"model":"public-model","messages":[{"role":"user","content":"hello"}],"stream":true,"stream_options":{"include_usage":true}}"#;
        let request = format!(
            "POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{gateway_port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let mut client = TcpStream::connect(gateway_address)
            .await
            .expect("connect gateway");
        client
            .write_all(request.as_bytes())
            .await
            .expect("write request");
        let mut response = Vec::new();
        timeout(
            TokioDuration::from_secs(5),
            client.read_to_end(&mut response),
        )
        .await
        .expect("SSE response timeout")
        .expect("read SSE response");
        let response = String::from_utf8_lossy(&response).to_ascii_lowercase();
        assert!(response.contains("http/1.1 200"), "response: {response}");
        assert!(response.contains("content-type: text/event-stream"));
        assert!(response.contains("\"content\":\"hello\""));
        let finish = response.find("\"finish_reason\":\"stop\"").unwrap();
        let usage = response.find("\"usage\"").unwrap();
        let done = response.find("data: [done]").unwrap();
        assert!(finish < usage && usage < done);

        let body = r#"{"model":"public-model","input":"hello","stream":true}"#;
        let request = format!(
            "POST /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{gateway_port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let mut client = TcpStream::connect(gateway_address)
            .await
            .expect("connect gateway");
        client
            .write_all(request.as_bytes())
            .await
            .expect("write request");
        let mut response = Vec::new();
        timeout(
            TokioDuration::from_secs(5),
            client.read_to_end(&mut response),
        )
        .await
        .expect("Responses SSE timeout")
        .expect("read Responses SSE response");
        let response = String::from_utf8_lossy(&response).to_ascii_lowercase();
        assert!(response.contains("http/1.1 200"), "response: {response}");
        assert!(response.contains("event: response.created"));
        assert!(response.contains("event: response.output_text.delta"));
        assert!(response.contains("event: response.completed"));
        assert!(response.contains("\"model\":\"public-model\""));
        assert!(!response.contains("data: [done]"));

        running.shutdown().await.expect("shutdown gateway");
        provider_task.await.expect("provider task");
    }

    #[tokio::test]
    async fn gateway_reload_swaps_live_proxy_config() {
        let config_dir = TempDir::new().expect("config temp dir");
        let external_dir = TempDir::new().expect("external temp dir");
        std::fs::write(
            config_dir.path().join(light_pingora::PROXY_FILE),
            "hosts: http://127.0.0.1:8081\n",
        )
        .expect("write proxy config");
        let config = runtime_config(&config_dir, &external_dir, HashMap::new());
        let proxy = GatewayProxy::from_runtime_config(&config).expect("build proxy");

        assert_eq!(
            proxy
                .current_proxy_route()
                .as_ref()
                .as_ref()
                .expect("proxy route")
                .targets[0]
                .address,
            "127.0.0.1:8081"
        );

        std::fs::write(
            external_dir.path().join(light_pingora::PROXY_FILE),
            "hosts: http://127.0.0.1:8082\n",
        )
        .expect("write external proxy config");

        let result = config
            .module_registry
            .reload_modules(
                ReloadContext::new(config.clone()),
                &[light_pingora::PROXY_MODULE_ID.to_string()],
            )
            .await;

        assert_eq!(result.reloaded, vec![light_pingora::PROXY_MODULE_ID]);
        assert!(result.skipped.is_empty());
        assert!(result.failed.is_empty());
        assert_eq!(
            proxy
                .current_proxy_route()
                .as_ref()
                .as_ref()
                .expect("proxy route")
                .targets[0]
                .address,
            "127.0.0.1:8082"
        );
    }
}
