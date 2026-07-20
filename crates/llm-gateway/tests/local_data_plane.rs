use async_trait::async_trait;
use futures_util::stream;
use llm_gateway::audit::{
    AuditAdmission, AuditFinish, AuditReservation, AuditStart, WalAudit, WalConfig,
};
use llm_gateway::config::{
    AliasConfig, AuditMode, DeploymentConfig, LlmRouterConfig, ProviderConfig,
};
use llm_gateway::credentials::MapSecretResolver;
use llm_gateway::http::{BodyAccessControl, BufferedHttpRequest, LlmBufferedHttp, LlmHttpResponse};
use llm_gateway::pii::{PiiKind, PiiProfile, UnresolvedPiiBehavior};
use llm_gateway::routing::PassiveCircuit;
use llm_gateway::runtime::{
    AliasPlan, CompileProbe, DeploymentRuntime, LlmCompiler, LlmPublishedSnapshot,
    LlmSnapshotStore, PrincipalPermitStripes, ProviderAccountRuntime, PublishOutcome,
    StreamStartBarrier,
};
use llm_gateway::usage::{Price, UsageLedger, UsageReservation};
use llm_gateway::{LlmGatewayError, LlmRequestContext, LlmRuntime};
use model_provider::conformance::{CapabilityRequirements, ConformanceResult, FixtureProvenance};
use model_provider::inference::{
    AcceptanceEvidence, ContentBlock, ContentCapabilities, FinishReason, InferenceError,
    InferenceEvent, InferenceProvider, InferenceRequest, InferenceResponse, InferenceStream,
    NormalizedUsage, Operation, ProviderCapabilities, ProviderEvidence, ProviderFormat,
    ProviderRequestContext, TerminalState,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

struct ScriptedProvider {
    format: ProviderFormat,
    results: Mutex<VecDeque<Result<InferenceResponse, InferenceError>>>,
    calls: AtomicUsize,
    capabilities: ProviderCapabilities,
}

struct SseProvider {
    format: ProviderFormat,
    events: Vec<Result<InferenceEvent, InferenceError>>,
    calls: AtomicUsize,
    wait_for_cancellation: bool,
    cancellation_observed: Arc<AtomicBool>,
}

struct DurableStartProvider {
    audit: Arc<WalAudit>,
    durable_before_dispatch: AtomicBool,
}

struct PiiEchoProvider {
    received: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl InferenceProvider for PiiEchoProvider {
    fn format(&self) -> ProviderFormat {
        ProviderFormat::OpenAi
    }

    fn capabilities(&self) -> ProviderCapabilities {
        capabilities(true, true, true)
    }

    async fn infer(
        &self,
        _context: ProviderRequestContext,
        request: InferenceRequest,
    ) -> Result<InferenceResponse, InferenceError> {
        let encoded = serde_json::to_string(&request).unwrap();
        self.received.lock().unwrap().push(encoded);
        let ContentBlock::Text { text } = &request.messages[0].content[0] else {
            panic!("PII test expects text")
        };
        let mut response = success_response();
        response.content = vec![ContentBlock::text(text.clone())];
        Ok(response)
    }

    async fn stream(
        &self,
        _context: ProviderRequestContext,
        request: InferenceRequest,
    ) -> Result<InferenceStream, InferenceError> {
        let encoded = serde_json::to_string(&request).unwrap();
        self.received.lock().unwrap().push(encoded);
        let ContentBlock::Text { text } = &request.messages[0].content[0] else {
            panic!("PII test expects text")
        };
        let marker = text.find("[[PII:").unwrap();
        let token = &text[marker..];
        let split = token.len() / 2;
        Ok(Box::pin(stream::iter(vec![
            Ok(InferenceEvent::TextDelta {
                text: format!("echo {}", &token[..split]),
            }),
            Ok(InferenceEvent::TextDelta {
                text: token[split..].to_string(),
            }),
            Ok(InferenceEvent::MessageEnd {
                finish_reason: FinishReason::Stop,
                terminal_state: TerminalState::Complete,
            }),
        ])))
    }
}

#[async_trait]
impl InferenceProvider for DurableStartProvider {
    fn format(&self) -> ProviderFormat {
        ProviderFormat::OpenAi
    }

    fn capabilities(&self) -> ProviderCapabilities {
        capabilities(true, true, true)
    }

    async fn infer(
        &self,
        _context: ProviderRequestContext,
        _request: InferenceRequest,
    ) -> Result<InferenceResponse, InferenceError> {
        self.durable_before_dispatch
            .store(self.audit.status().durable_sequence >= 2, Ordering::SeqCst);
        Ok(success_response())
    }

    async fn stream(
        &self,
        _context: ProviderRequestContext,
        _request: InferenceRequest,
    ) -> Result<InferenceStream, InferenceError> {
        self.durable_before_dispatch
            .store(self.audit.status().durable_sequence >= 2, Ordering::SeqCst);
        Ok(Box::pin(stream::iter(SseProvider::success().events)))
    }
}

impl SseProvider {
    fn success() -> Self {
        Self {
            format: ProviderFormat::OpenAi,
            events: vec![
                Ok(InferenceEvent::TextDelta {
                    text: "hello".to_string(),
                }),
                Ok(InferenceEvent::MessageEnd {
                    finish_reason: FinishReason::Stop,
                    terminal_state: TerminalState::Complete,
                }),
                Ok(InferenceEvent::Usage {
                    usage: NormalizedUsage {
                        input_tokens: Some(3),
                        output_tokens: Some(1),
                        cached_input_tokens: None,
                        reasoning_tokens: None,
                    },
                }),
            ],
            calls: AtomicUsize::new(0),
            wait_for_cancellation: false,
            cancellation_observed: Arc::new(AtomicBool::new(false)),
        }
    }
}

#[async_trait]
impl InferenceProvider for SseProvider {
    fn format(&self) -> ProviderFormat {
        self.format
    }

    fn capabilities(&self) -> ProviderCapabilities {
        capabilities(true, true, true)
    }

    async fn infer(
        &self,
        _context: ProviderRequestContext,
        _request: InferenceRequest,
    ) -> Result<InferenceResponse, InferenceError> {
        Err(InferenceError::unsupported("stream-only test provider"))
    }

    async fn stream(
        &self,
        context: ProviderRequestContext,
        _request: InferenceRequest,
    ) -> Result<InferenceStream, InferenceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let events = self.events.clone();
        if !self.wait_for_cancellation {
            return Ok(Box::pin(stream::iter(events)));
        }
        let observed = Arc::clone(&self.cancellation_observed);
        let cancellation = context.cancellation;
        tokio::spawn({
            let cancellation = cancellation.clone();
            let observed = Arc::clone(&observed);
            async move {
                cancellation.cancelled().await;
                observed.store(true, Ordering::SeqCst);
            }
        });
        Ok(Box::pin(stream::unfold(
            (events.into_iter(), false),
            move |(mut events, cancelled)| {
                let cancellation = cancellation.clone();
                let observed = Arc::clone(&observed);
                async move {
                    if let Some(event) = events.next() {
                        return Some((event, (events, cancelled)));
                    }
                    if !cancelled {
                        cancellation.cancelled().await;
                        observed.store(true, Ordering::SeqCst);
                        return Some((Err(InferenceError::cancelled()), (events, true)));
                    }
                    None
                }
            },
        )))
    }
}

impl ScriptedProvider {
    fn new(
        format: ProviderFormat,
        results: Vec<Result<InferenceResponse, InferenceError>>,
    ) -> Self {
        Self::with_capabilities(format, results, capabilities(true, true, true))
    }

    fn with_capabilities(
        format: ProviderFormat,
        results: Vec<Result<InferenceResponse, InferenceError>>,
        capabilities: ProviderCapabilities,
    ) -> Self {
        Self {
            format,
            results: Mutex::new(results.into()),
            calls: AtomicUsize::new(0),
            capabilities,
        }
    }
}

#[async_trait]
impl InferenceProvider for ScriptedProvider {
    fn format(&self) -> ProviderFormat {
        self.format
    }
    fn capabilities(&self) -> ProviderCapabilities {
        self.capabilities.clone()
    }
    async fn infer(
        &self,
        _context: ProviderRequestContext,
        _request: InferenceRequest,
    ) -> Result<InferenceResponse, InferenceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.results
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(success_response()))
    }
    async fn stream(
        &self,
        _context: ProviderRequestContext,
        _request: InferenceRequest,
    ) -> Result<InferenceStream, InferenceError> {
        Err(InferenceError::unsupported("not used"))
    }
}

#[derive(Default)]
struct RecordingAudit {
    events: Arc<Mutex<Vec<&'static str>>>,
}
struct RecordingReservation {
    events: Arc<Mutex<Vec<&'static str>>>,
}

struct FailingFinishAudit;
struct FailingFinishReservation;

struct BlockingStartBarrier {
    entered: AtomicBool,
    release: Semaphore,
}

#[async_trait]
impl AuditAdmission for RecordingAudit {
    async fn reserve(
        &self,
        _mode: AuditMode,
        _start: AuditStart,
    ) -> Result<Box<dyn AuditReservation>, LlmGatewayError> {
        self.events.lock().unwrap().push("reserve");
        Ok(Box::new(RecordingReservation {
            events: Arc::clone(&self.events),
        }))
    }
}

#[async_trait]
impl AuditReservation for RecordingReservation {
    async fn finish(self: Box<Self>, _finish: AuditFinish) -> Result<(), LlmGatewayError> {
        self.events.lock().unwrap().push("finish");
        Ok(())
    }
}

#[async_trait]
impl AuditAdmission for FailingFinishAudit {
    async fn reserve(
        &self,
        _mode: AuditMode,
        _start: AuditStart,
    ) -> Result<Box<dyn AuditReservation>, LlmGatewayError> {
        Ok(Box::new(FailingFinishReservation))
    }
}

#[async_trait]
impl AuditReservation for FailingFinishReservation {
    async fn finish(self: Box<Self>, _finish: AuditFinish) -> Result<(), LlmGatewayError> {
        Err(LlmGatewayError::AuditUnavailable)
    }
}

#[async_trait]
impl StreamStartBarrier for BlockingStartBarrier {
    async fn wait_until_durable(&self, _request_id: &str) -> Result<(), LlmGatewayError> {
        self.entered.store(true, Ordering::SeqCst);
        self.release
            .acquire()
            .await
            .map_err(|_| LlmGatewayError::AuditUnavailable)?
            .forget();
        Ok(())
    }
}

fn capabilities(images: bool, tools: bool, structured: bool) -> ProviderCapabilities {
    ProviderCapabilities {
        operations: BTreeSet::from([Operation::ChatCompletions]),
        content: ContentCapabilities {
            text: true,
            images,
            tools,
            parallel_tools: tools,
            structured_json: structured,
            reasoning_usage: false,
        },
        streaming: true,
    }
}

fn success_response() -> InferenceResponse {
    InferenceResponse {
        content: vec![ContentBlock::text("ok")],
        finish_reason: FinishReason::Stop,
        usage: Some(NormalizedUsage {
            input_tokens: Some(10),
            output_tokens: Some(2),
            cached_input_tokens: None,
            reasoning_tokens: None,
        }),
        evidence: ProviderEvidence {
            request_id: Some("physical-secret-id".to_string()),
            physical_model: Some("physical-secret-model".to_string()),
            ..Default::default()
        },
        terminal_state: TerminalState::Complete,
    }
}

fn deployment(id: &str, provider: Arc<dyn InferenceProvider>) -> Arc<DeploymentRuntime> {
    Arc::new(DeploymentRuntime {
        id: id.to_string(),
        model: format!("{id}-physical"),
        configured_concurrency: 2,
        capabilities: provider.capabilities(),
        provider,
        provider_digest: id.to_string(),
        conformance_result: None,
        required_conformance_provenance: None,
        permits: Arc::new(Semaphore::new(2)),
        circuit: Arc::new(PassiveCircuit::new(1, Duration::ZERO)),
        account: Arc::new(ProviderAccountRuntime {
            provider_account_id: id.to_string(),
            quota_group_id: id.to_string(),
        }),
        price: Price {
            version: 7,
            input_micros_per_million: 1_000_000,
            output_micros_per_million: 2_000_000,
        },
    })
}

fn conformance_result(provenance: FixtureProvenance, valid_until: &str) -> ConformanceResult {
    let mut result: ConformanceResult = serde_json::from_str(include_str!(
        "../../model-provider/conformance/results/openai.json"
    ))
    .expect("checked-in OpenAI conformance result");
    result.physical_model = "governed-physical".to_string();
    result.tested_at = "2026-07-19T00:00:00Z".parse().unwrap();
    result.valid_until = valid_until.parse().unwrap();
    for evidence in result.capability_evidence.values_mut() {
        evidence.provenances = BTreeSet::from([provenance]);
    }
    result.refresh_digest();
    result
}

fn governed_deployment(
    provider: Arc<dyn InferenceProvider>,
    conformance_result: ConformanceResult,
) -> Arc<DeploymentRuntime> {
    Arc::new(DeploymentRuntime {
        id: "governed".to_string(),
        model: conformance_result.physical_model.clone(),
        configured_concurrency: 2,
        capabilities: conformance_result.capabilities.clone(),
        provider,
        provider_digest: "governed".to_string(),
        conformance_result: Some(conformance_result),
        required_conformance_provenance: Some(FixtureProvenance::CapturedSanitized),
        permits: Arc::new(Semaphore::new(2)),
        circuit: Arc::new(PassiveCircuit::new(1, Duration::ZERO)),
        account: Arc::new(ProviderAccountRuntime {
            provider_account_id: "governed".to_string(),
            quota_group_id: "governed".to_string(),
        }),
        price: Price {
            version: 7,
            input_micros_per_million: 1_000_000,
            output_micros_per_million: 2_000_000,
        },
    })
}

fn runtime_with_deployment(
    deployment: Arc<DeploymentRuntime>,
    audit: Arc<dyn AuditAdmission>,
) -> Arc<LlmRuntime> {
    let alias = Arc::new(AliasPlan {
        public_name: "public-model".to_string(),
        deployments: vec![Arc::clone(&deployment)],
        max_attempts: 1,
        configured_concurrency: 2,
        permits: Arc::new(Semaphore::new(2)),
        max_input_tokens: Some(10_000),
        max_output_tokens: Some(100),
        max_cost_micros: Some(10_000),
        internal: false,
        bound_principal: None,
        audit: AuditMode::Required,
        pii: Default::default(),
        required_capabilities: Default::default(),
        ledger: Arc::new(UsageLedger::default()),
    });
    let snapshot = LlmPublishedSnapshot {
        generation: 4,
        digest: "governed-root".to_string(),
        global_concurrency: 2,
        global_stream_concurrency: 1,
        stream_channel_capacity: 1,
        stream_write_timeout_ms: 100,
        stream_setup_timeout_ms: 100,
        stream_idle_timeout_ms: 100,
        stream_minimum_drain_bytes_per_second: 1,
        stream_drain_grace_ms: 100,
        max_replay_bytes: 4096,
        aliases: BTreeMap::from([("public-model".to_string(), alias)]),
        deployments: BTreeMap::from([(deployment.id.clone(), deployment)]),
        principal_permits: Arc::new(PrincipalPermitStripes::new(8, 2)),
    };
    Arc::new(LlmRuntime::new(
        Arc::new(LlmSnapshotStore::new(snapshot, 2)),
        audit,
    ))
}

fn runtime_with(
    providers: Vec<Arc<dyn InferenceProvider>>,
    attempts: usize,
    max_replay: usize,
    audit: Arc<dyn AuditAdmission>,
) -> Arc<LlmRuntime> {
    runtime_with_mode(providers, attempts, max_replay, audit, AuditMode::Required)
}

fn runtime_with_mode(
    providers: Vec<Arc<dyn InferenceProvider>>,
    attempts: usize,
    max_replay: usize,
    audit: Arc<dyn AuditAdmission>,
    audit_mode: AuditMode,
) -> Arc<LlmRuntime> {
    runtime_with_mode_and_pii(
        providers,
        attempts,
        max_replay,
        audit,
        audit_mode,
        PiiProfile::default(),
    )
}

fn runtime_with_mode_and_pii(
    providers: Vec<Arc<dyn InferenceProvider>>,
    attempts: usize,
    max_replay: usize,
    audit: Arc<dyn AuditAdmission>,
    audit_mode: AuditMode,
    pii: PiiProfile,
) -> Arc<LlmRuntime> {
    let deployments = providers
        .into_iter()
        .enumerate()
        .map(|(index, provider)| deployment(&format!("d{index}"), provider))
        .collect::<Vec<_>>();
    let alias = Arc::new(AliasPlan {
        public_name: "public-model".to_string(),
        deployments: deployments.clone(),
        max_attempts: attempts,
        configured_concurrency: 2,
        permits: Arc::new(Semaphore::new(2)),
        max_input_tokens: Some(10_000),
        max_output_tokens: Some(100),
        max_cost_micros: Some(10_000),
        internal: false,
        bound_principal: None,
        audit: audit_mode,
        pii: pii.clone(),
        required_capabilities: Default::default(),
        ledger: Arc::new(UsageLedger::default()),
    });
    let internal_alias = Arc::new(AliasPlan {
        public_name: "legacy-agent-internal".to_string(),
        deployments: vec![Arc::clone(&deployments[0])],
        max_attempts: 1,
        configured_concurrency: 1,
        permits: Arc::new(Semaphore::new(1)),
        max_input_tokens: Some(10_000),
        max_output_tokens: Some(100),
        max_cost_micros: Some(10_000),
        internal: true,
        bound_principal: Some("test-agent".to_string()),
        audit: audit_mode,
        pii: Default::default(),
        required_capabilities: Default::default(),
        ledger: Arc::new(UsageLedger::default()),
    });
    let snapshot = LlmPublishedSnapshot {
        generation: 4,
        digest: "root".to_string(),
        global_concurrency: 2,
        global_stream_concurrency: 1,
        stream_channel_capacity: 1,
        stream_write_timeout_ms: 100,
        stream_setup_timeout_ms: 100,
        stream_idle_timeout_ms: 100,
        stream_minimum_drain_bytes_per_second: 1,
        stream_drain_grace_ms: 100,
        max_replay_bytes: max_replay,
        aliases: BTreeMap::from([
            ("public-model".to_string(), alias),
            ("legacy-agent-internal".to_string(), internal_alias),
        ]),
        deployments: deployments
            .into_iter()
            .map(|deployment| (deployment.id.clone(), deployment))
            .collect(),
        principal_permits: Arc::new(PrincipalPermitStripes::new(8, 2)),
    };
    Arc::new(LlmRuntime::new(
        Arc::new(LlmSnapshotStore::new(snapshot, 2)),
        audit,
    ))
}

#[test]
fn deployment_supports_requires_current_passing_captured_conformance() {
    let provider: Arc<dyn InferenceProvider> =
        Arc::new(ScriptedProvider::new(ProviderFormat::OpenAi, Vec::new()));
    let requirements = CapabilityRequirements::default();

    let captured = conformance_result(FixtureProvenance::CapturedSanitized, "2999-01-01T00:00:00Z");
    assert!(governed_deployment(Arc::clone(&provider), captured.clone()).supports(&requirements));

    let expired = conformance_result(FixtureProvenance::CapturedSanitized, "2000-01-01T00:00:00Z");
    assert!(!governed_deployment(Arc::clone(&provider), expired).supports(&requirements));

    let quarantined = captured.quarantine("provider drift detected");
    assert!(!governed_deployment(Arc::clone(&provider), quarantined).supports(&requirements));

    let synthetic = conformance_result(
        FixtureProvenance::SyntheticSpecDerived,
        "2999-01-01T00:00:00Z",
    );
    assert!(!governed_deployment(provider, synthetic).supports(&requirements));
}

#[tokio::test]
async fn expired_conformance_is_excluded_from_every_gateway_route() {
    let provider = Arc::new(ScriptedProvider::new(
        ProviderFormat::OpenAi,
        vec![Ok(success_response())],
    ));
    let provider_runtime: Arc<dyn InferenceProvider> = provider.clone();
    let deployment = governed_deployment(
        provider_runtime,
        conformance_result(FixtureProvenance::CapturedSanitized, "2000-01-01T00:00:00Z"),
    );
    let runtime = runtime_with_deployment(deployment, Arc::new(RecordingAudit::default()));
    let request = InferenceRequest::text("public-model", "hello");

    assert!(runtime.visible_models().is_empty());
    assert!(matches!(
        runtime.eligible_formats(&runtime.snapshot(), "user", &request, false),
        Err(LlmGatewayError::ModelUnavailable)
    ));
    assert!(matches!(
        runtime
            .execute(
                LlmRequestContext::with_timeout("user", Duration::from_secs(1)),
                request.clone(),
            )
            .await,
        Err(LlmGatewayError::ModelUnavailable)
    ));
    assert!(matches!(
        runtime
            .execute_stream_with_snapshot(
                LlmRequestContext::with_timeout("user", Duration::from_secs(1)),
                runtime.snapshot(),
                request,
            )
            .await,
        Err(LlmGatewayError::ModelUnavailable)
    ));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn lf5_single_attempt_never_uses_fallback_and_finalizes_audit() {
    let first = Arc::new(ScriptedProvider::new(
        ProviderFormat::OpenAi,
        vec![Err(InferenceError::from_status(429, None, "limited"))],
    ));
    let second = Arc::new(ScriptedProvider::new(
        ProviderFormat::Anthropic,
        vec![Ok(success_response())],
    ));
    let audit = Arc::new(RecordingAudit::default());
    let runtime = runtime_with(vec![first.clone(), second.clone()], 1, 4096, audit.clone());
    let error = runtime
        .execute(
            LlmRequestContext::with_timeout("user", Duration::from_secs(1)),
            InferenceRequest::text("public-model", "hello"),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, LlmGatewayError::Provider(_)));
    assert_eq!(first.calls.load(Ordering::SeqCst), 1);
    assert_eq!(second.calls.load(Ordering::SeqCst), 0);
    assert_eq!(*audit.events.lock().unwrap(), ["reserve", "finish"]);
}

#[tokio::test]
async fn lf5b_safe_failure_falls_back_once_and_reconciles_exact_usage() {
    let first = Arc::new(ScriptedProvider::new(
        ProviderFormat::OpenAi,
        vec![Err(InferenceError::from_status(429, Some("1"), "limited"))],
    ));
    let second = Arc::new(ScriptedProvider::new(
        ProviderFormat::Anthropic,
        vec![Ok(success_response())],
    ));
    let runtime = runtime_with(
        vec![first.clone(), second.clone()],
        2,
        4096,
        Arc::new(RecordingAudit::default()),
    );
    let execution = runtime
        .execute(
            LlmRequestContext::with_timeout("user", Duration::from_secs(1)),
            InferenceRequest::text("public-model", "hello"),
        )
        .await
        .unwrap();
    assert_eq!(execution.attempts, 2);
    assert_eq!(execution.usage.charged_micros, 14);
    assert!(execution.usage.complete);
}

#[tokio::test]
async fn mandatory_retry_rejects_oversize_replay_before_dispatch() {
    let provider = Arc::new(ScriptedProvider::new(
        ProviderFormat::OpenAi,
        vec![Ok(success_response())],
    ));
    let runtime = runtime_with(
        vec![provider.clone(), provider.clone()],
        2,
        8,
        Arc::new(RecordingAudit::default()),
    );
    let error = runtime
        .execute(
            LlmRequestContext::with_timeout("user", Duration::from_secs(1)),
            InferenceRequest::text("public-model", "larger than replay"),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, LlmGatewayError::InvalidRequest(_)));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn audit_finish_failure_remains_fail_closed_for_a_rejected_request() {
    let provider = Arc::new(ScriptedProvider::new(
        ProviderFormat::OpenAi,
        vec![Ok(success_response())],
    ));
    let runtime = runtime_with(
        vec![provider.clone(), provider.clone()],
        2,
        8,
        Arc::new(FailingFinishAudit),
    );
    let error = runtime
        .execute(
            LlmRequestContext::with_timeout("user", Duration::from_secs(1)),
            InferenceRequest::text("public-model", "larger than replay"),
        )
        .await
        .unwrap_err();
    assert_eq!(error, LlmGatewayError::AuditUnavailable);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn ambiguous_usage_is_conservatively_nonzero_and_incomplete() {
    let ledger = Arc::new(UsageLedger::default());
    let reservation = UsageReservation::reserve(Arc::clone(&ledger), 77, Some(100)).unwrap();
    let result = reservation.reconcile(
        Price {
            version: 9,
            input_micros_per_million: 1,
            output_micros_per_million: 1,
        },
        None,
        AcceptanceEvidence::PossiblyAccepted,
    );
    assert_eq!(result.charged_micros, 77);
    assert!(!result.complete);
    assert_eq!(ledger.reserved(), 0);
}

#[test]
fn passive_circuit_allows_only_one_half_open_probe() {
    let circuit = PassiveCircuit::new(1, Duration::ZERO);
    circuit.failure(
        &InferenceError::from_status(503, None, "down"),
        Instant::now(),
    );
    let after_cooldown = Instant::now() + Duration::from_millis(2);
    let probe = circuit.acquire(after_cooldown).expect("half-open probe");
    assert!(circuit.acquire(after_cooldown).is_err());
    probe.success();
    assert!(circuit.acquire(Instant::now()).is_ok());
}

#[test]
fn half_open_probe_non_circuit_failure_releases_probe_slot() {
    let circuit = PassiveCircuit::new(1, Duration::ZERO);
    circuit.failure(
        &InferenceError::from_status(503, None, "down"),
        Instant::now(),
    );
    let after_cooldown = Instant::now() + Duration::from_millis(2);
    let probe = circuit.acquire(after_cooldown).expect("half-open probe");
    probe.failure(
        &InferenceError::invalid_request("bad client request"),
        after_cooldown,
    );
    assert!(
        circuit
            .acquire(after_cooldown + Duration::from_secs(3600))
            .is_ok(),
        "a non-circuit probe failure must not wedge the probe slot"
    );
}

fn compiler_config() -> LlmRouterConfig {
    LlmRouterConfig {
        enabled: true,
        development_fixtures: true,
        providers: BTreeMap::from([(
            "p".to_string(),
            ProviderConfig {
                format: ProviderFormat::OpenAi,
                base_url: "http://127.0.0.1:9/v1".to_string(),
                secret_ref: "secret".to_string(),
                headers: BTreeMap::new(),
                quota_group_id: Some("quota".to_string()),
            },
        )]),
        deployments: BTreeMap::from([(
            "d".to_string(),
            DeploymentConfig {
                provider: "p".to_string(),
                model: "physical".to_string(),
                concurrency: 2,
                input_micros_per_million: Some(1),
                output_micros_per_million: Some(2),
                conformance_digest: "a".repeat(64),
                conformance_result: None,
                text: true,
                images: false,
                tools: false,
                structured_json: false,
                pii_placeholder_preservation_percent: 0,
            },
        )]),
        aliases: BTreeMap::from([(
            "public-model".to_string(),
            AliasConfig {
                deployments: vec!["d".to_string()],
                max_attempts: 1,
                concurrency: 2,
                max_input_tokens: None,
                max_output_tokens: None,
                max_cost_micros: None,
                internal: false,
                bound_principal: None,
                audit: AuditMode::Disabled,
                pii: Default::default(),
                required_capabilities: Default::default(),
            },
        )]),
        ..Default::default()
    }
}

#[test]
fn portal_agent_eligibility_contract_is_safe_for_gateway_model_resolution() {
    let fixture_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(
        "../../benchmarks/llm-gateway/contracts/v1/get-eligible-llm-models-for-agent.fixture.json",
    );
    let fixture: serde_json::Value = serde_json::from_slice(
        &std::fs::read(fixture_path).expect("read Portal eligibility contract fixture"),
    )
    .expect("parse Portal eligibility contract fixture");
    assert_eq!(fixture["schemaVersion"], "1");
    assert_eq!(fixture["productionAuthority"], false);
    let cases = fixture["cases"].as_array().expect("contract cases");
    assert!(
        cases
            .iter()
            .any(|case| case["response"]["resolutionStatus"] == "AMBIGUOUS_DEFAULT")
    );
    assert!(
        cases
            .iter()
            .any(|case| case["response"]["resolutionStatus"] == "NO_DEFAULT")
    );
    assert!(cases.iter().any(|case| {
        case["response"]["models"].as_array().is_some_and(|models| {
            models.iter().any(|model| {
                model["selection_mode"] == "INTERNAL_LEGACY"
                    && model["alias_name"] == "legacy-agent-internal"
            })
        })
    }));
    for case in cases {
        let response = &case["response"];
        let encoded = serde_json::to_string(response).expect("encode contract response");
        assert!(!encoded.contains("modelPolicyId"));
        assert!(!encoded.contains("model_policy_id"));
        if response["resolutionStatus"] == "RESOLVED" {
            assert!(
                response["resolvedModel"]
                    .as_str()
                    .is_some_and(|model| !model.is_empty())
            );
        } else {
            assert!(response["resolvedModel"].is_null());
        }
    }
}

#[test]
fn compiler_resolves_secrets_and_clients_off_path_and_reuses_deployments() {
    let probe = Arc::new(CompileProbe::default());
    let compiler = LlmCompiler::with_probe(
        Arc::new(MapSecretResolver(BTreeMap::from([(
            "secret".to_string(),
            "value".to_string(),
        )]))),
        Arc::clone(&probe),
    );
    let first = compiler.compile(&compiler_config(), 1, None).unwrap();
    let second = compiler
        .compile(&compiler_config(), 2, Some(&first))
        .unwrap();
    assert!(Arc::ptr_eq(
        &first.deployments["d"],
        &second.deployments["d"]
    ));
    assert!(Arc::ptr_eq(
        &first.aliases["public-model"],
        &second.aliases["public-model"]
    ));
    assert!(Arc::ptr_eq(
        &first.principal_permits,
        &second.principal_permits
    ));
    let before = (
        probe.secret_resolutions.load(Ordering::SeqCst),
        probe.client_builds.load(Ordering::SeqCst),
    );
    let store = Arc::new(LlmSnapshotStore::new(second, 2));
    let runtime = LlmRuntime::new(store, Arc::new(RecordingAudit::default()));
    assert_eq!(runtime.visible_models(), ["public-model"]);
    assert_eq!(
        before,
        (
            probe.secret_resolutions.load(Ordering::SeqCst),
            probe.client_builds.load(Ordering::SeqCst)
        )
    );
    assert_eq!(probe.client_builds.load(Ordering::SeqCst), 1);
}

#[test]
fn credential_rotation_rebuilds_client_but_preserves_provider_account_runtime() {
    let first_compiler = LlmCompiler::new(Arc::new(MapSecretResolver(BTreeMap::from([(
        "secret".to_string(),
        "old".to_string(),
    )]))));
    let first = first_compiler.compile(&compiler_config(), 1, None).unwrap();
    let second_compiler = LlmCompiler::new(Arc::new(MapSecretResolver(BTreeMap::from([(
        "secret".to_string(),
        "new".to_string(),
    )]))));
    let second = second_compiler
        .compile(&compiler_config(), 2, Some(&first))
        .unwrap();
    assert!(!Arc::ptr_eq(
        &first.deployments["d"],
        &second.deployments["d"]
    ));
    assert!(Arc::ptr_eq(
        &first.deployments["d"].account,
        &second.deployments["d"].account
    ));
}

#[test]
fn production_config_rejects_loopback_plaintext_fixture_provider() {
    let compiler = LlmCompiler::new(Arc::new(MapSecretResolver(BTreeMap::from([(
        "secret".to_string(),
        "value".to_string(),
    )]))));
    let mut config = compiler_config();
    config.development_fixtures = false;
    assert!(compiler.compile(&config, 1, None).is_err());
}

#[test]
fn invalid_candidate_is_not_published_and_retirement_is_bounded() {
    let compiler = LlmCompiler::new(Arc::new(MapSecretResolver(BTreeMap::from([(
        "secret".to_string(),
        "value".to_string(),
    )]))));
    let first = compiler.compile(&compiler_config(), 1, None).unwrap();
    let store = LlmSnapshotStore::new(first, 1);
    let original = store.load();
    let mut invalid = compiler_config();
    invalid.aliases.get_mut("public-model").unwrap().deployments = vec!["missing".to_string()];
    assert!(compiler.compile(&invalid, 2, Some(&original)).is_err());
    assert_eq!(store.load().generation, 1);
    let mut changed = compiler_config();
    changed.deployments.get_mut("d").unwrap().model = "other".to_string();
    assert!(matches!(
        store.publish(compiler.compile(&changed, 2, Some(&original)).unwrap()),
        PublishOutcome::Published
    ));
    changed.deployments.get_mut("d").unwrap().model = "third".to_string();
    let current = store.load();
    store.publish(compiler.compile(&changed, 3, Some(&current)).unwrap());
    assert_eq!(store.retained_generations(), 1);
}

struct DenyBeforeParse {
    calls: Arc<AtomicUsize>,
}
#[async_trait]
impl BodyAccessControl for DenyBeforeParse {
    async fn authorize(
        &self,
        _request: &BufferedHttpRequest,
        _body: &[u8],
    ) -> Result<(), LlmGatewayError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(LlmGatewayError::Forbidden)
    }
}

fn http_request(body: &[u8]) -> BufferedHttpRequest {
    BufferedHttpRequest {
        method: "POST".to_string(),
        path: "/v1/chat/completions".to_string(),
        headers: BTreeMap::from([
            ("content-type".to_string(), "application/json".to_string()),
            ("x-request-id".to_string(), "spoofed".to_string()),
        ]),
        body: body.to_vec(),
        principal_id: "user".to_string(),
        trusted_request_id: "trusted".to_string(),
    }
}

#[tokio::test]
async fn buffered_security_denies_before_json_and_alias_parse() {
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(ScriptedProvider::new(
        ProviderFormat::OpenAi,
        vec![Ok(success_response())],
    ));
    let runtime = runtime_with(vec![provider], 1, 4096, Arc::new(RecordingAudit::default()));
    let http = LlmBufferedHttp::new(
        runtime,
        Arc::new(DenyBeforeParse {
            calls: Arc::clone(&calls),
        }),
        1024,
        16,
        Duration::from_secs(1),
    );
    let response = http
        .handle(http_request(b"not-json-and-secret-model"))
        .await;
    assert_eq!(response.status, 403);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(!String::from_utf8_lossy(&response.body).contains("secret-model"));
}

struct Allow;
#[async_trait]
impl BodyAccessControl for Allow {
    async fn authorize(
        &self,
        _request: &BufferedHttpRequest,
        _body: &[u8],
    ) -> Result<(), LlmGatewayError> {
        Ok(())
    }
}

#[tokio::test]
async fn buffered_response_uses_trusted_id_and_hides_physical_provider_evidence() {
    let provider = Arc::new(ScriptedProvider::new(
        ProviderFormat::OpenAi,
        vec![Ok(success_response())],
    ));
    let runtime = runtime_with(vec![provider], 1, 4096, Arc::new(RecordingAudit::default()));
    let http = LlmBufferedHttp::new(
        Arc::clone(&runtime),
        Arc::new(Allow),
        4096,
        32,
        Duration::from_secs(1),
    );
    let response = http
        .handle(http_request(
            br#"{"model":"public-model","messages":[{"role":"user","content":"hello"}]}"#,
        ))
        .await;
    assert_eq!(response.status, 200);
    assert_eq!(response.headers["x-request-id"], "trusted");
    let body = String::from_utf8(response.body).unwrap();
    assert!(!body.contains("physical-secret"));
    assert!(body.contains("public-model"));
}

#[tokio::test]
async fn buffered_http_rejects_method_media_size_and_operated_field_conflicts() {
    let provider = Arc::new(ScriptedProvider::new(
        ProviderFormat::OpenAi,
        vec![Ok(success_response())],
    ));
    let runtime = runtime_with(
        vec![provider.clone()],
        1,
        4096,
        Arc::new(RecordingAudit::default()),
    );
    let http = LlmBufferedHttp::new(runtime, Arc::new(Allow), 256, 32, Duration::from_secs(1));

    let mut request = http_request(br#"{"model":"public-model","messages":[]}"#);
    request.method = "GET".to_string();
    assert_eq!(http.handle(request).await.status, 405);

    let mut request = http_request(br#"{"model":"public-model","messages":[]}"#);
    request
        .headers
        .insert("content-encoding".to_string(), "gzip".to_string());
    assert_eq!(http.handle(request).await.status, 415);

    let mut request = http_request(br#"{"model":"public-model","messages":[]}"#);
    request
        .headers
        .insert("content-length".to_string(), "257".to_string());
    assert_eq!(http.handle(request).await.status, 413);

    let request = http_request(
        br#"{"model":"public-model","messages":[],"max_tokens":1,"max_completion_tokens":2}"#,
    );
    assert_eq!(http.handle(request).await.status, 400);

    let request = http_request(
        br#"{"model":"public-model","messages":[],"stream":true,"stream_options":{"include_usage":"yes"}}"#,
    );
    assert_eq!(http.handle(request).await.status, 400);
    let request = http_request(
        br#"{"model":"public-model","messages":[],"stream":true,"stream_options":{"future_option":true}}"#,
    );
    assert_eq!(http.handle(request).await.status, 400);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn mixed_format_alias_parses_for_the_eligible_provider_set() {
    let anthropic = Arc::new(ScriptedProvider::with_capabilities(
        ProviderFormat::Anthropic,
        vec![Ok(success_response())],
        capabilities(false, true, false),
    ));
    let openai = Arc::new(ScriptedProvider::with_capabilities(
        ProviderFormat::OpenAi,
        vec![Ok(success_response())],
        capabilities(true, true, true),
    ));
    let runtime = runtime_with(
        vec![anthropic.clone(), openai.clone()],
        2,
        4096,
        Arc::new(RecordingAudit::default()),
    );
    let http = LlmBufferedHttp::new(runtime, Arc::new(Allow), 4096, 32, Duration::from_secs(1));
    let response = http
        .handle(http_request(
            br#"{"model":"public-model","messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"https://example.com/image.png"}}]}],"response_format":{"type":"json_object"}}"#,
        ))
        .await;
    assert_eq!(response.status, 200);
    assert_eq!(anthropic.calls.load(Ordering::SeqCst), 0);
    assert_eq!(openai.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn mixed_format_alias_rejects_allowlisted_openai_only_extensions() {
    let openai = Arc::new(ScriptedProvider::new(
        ProviderFormat::OpenAi,
        vec![Ok(success_response())],
    ));
    let anthropic = Arc::new(ScriptedProvider::new(
        ProviderFormat::Anthropic,
        vec![Ok(success_response())],
    ));
    let runtime = runtime_with(
        vec![openai.clone(), anthropic.clone()],
        2,
        4096,
        Arc::new(RecordingAudit::default()),
    );
    let http = LlmBufferedHttp::new(runtime, Arc::new(Allow), 4096, 32, Duration::from_secs(1))
        .with_openai_extension_allowlist(BTreeSet::from(["service_tier".to_string()]));
    let response = http
        .handle(http_request(
            br#"{"model":"public-model","messages":[{"role":"user","content":"hello"}],"service_tier":"priority"}"#,
        ))
        .await;
    assert_eq!(response.status, 400);
    assert_eq!(openai.calls.load(Ordering::SeqCst), 0);
    assert_eq!(anthropic.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn early_sse_smoke_frames_success_and_done_through_bounded_channel() {
    let provider = Arc::new(SseProvider::success());
    let audit = Arc::new(RecordingAudit::default());
    let runtime = runtime_with(vec![provider.clone()], 1, 4096, audit.clone());
    let http = LlmBufferedHttp::new(
        Arc::clone(&runtime),
        Arc::new(Allow),
        4096,
        32,
        Duration::from_secs(1),
    );
    let response = http
        .handle_route(http_request(
            br#"{"model":"public-model","messages":[{"role":"user","content":"hello"}],"stream":true,"stream_options":{"include_usage":true}}"#,
        ))
        .await;
    let LlmHttpResponse::Streaming(mut response) = response else {
        panic!("expected SSE response");
    };
    assert_eq!(response.status, 200);
    assert_eq!(response.headers["content-type"], "text/event-stream");
    let mut body = Vec::new();
    while let Some(frame) = response.stream.next_frame().await {
        body.extend_from_slice(&frame);
    }
    let body = String::from_utf8(body).unwrap();
    assert!(body.contains("hello"));
    let finish = body.find("\"finish_reason\":\"stop\"").unwrap();
    let usage = body.find("\"usage\"").unwrap();
    let done = body.find("data: [DONE]").unwrap();
    assert!(finish < usage && usage < done);
    assert!(body.ends_with("data: [DONE]\n\n"));
    assert_eq!(
        runtime.snapshot().aliases["public-model"].ledger.charged(),
        5
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(*audit.events.lock().unwrap(), ["reserve", "finish"]);
}

#[tokio::test]
async fn full_sse_falls_back_before_visible_output_across_provider_formats() {
    let first = Arc::new(SseProvider {
        format: ProviderFormat::OpenAi,
        events: vec![Err(InferenceError::from_status(429, None, "limited"))],
        calls: AtomicUsize::new(0),
        wait_for_cancellation: false,
        cancellation_observed: Arc::new(AtomicBool::new(false)),
    });
    let mut second_provider = SseProvider::success();
    second_provider.format = ProviderFormat::Anthropic;
    let second = Arc::new(second_provider);
    let runtime = runtime_with(
        vec![first.clone(), second.clone()],
        2,
        4096,
        Arc::new(RecordingAudit::default()),
    );
    let http = LlmBufferedHttp::new(runtime, Arc::new(Allow), 4096, 32, Duration::from_secs(1));
    let LlmHttpResponse::Streaming(mut response) = http
        .handle_route(http_request(
            br#"{"model":"public-model","messages":[{"role":"user","content":"hello"}],"stream":true,"stream_options":{"include_usage":true}}"#,
        ))
        .await
    else {
        panic!("expected SSE response");
    };
    let mut body = Vec::new();
    while let Some(frame) = response.stream.next_frame().await {
        body.extend_from_slice(&frame);
    }
    let body = String::from_utf8(body).unwrap();
    assert!(body.contains("hello"));
    assert!(body.ends_with("data: [DONE]\n\n"));
    assert_eq!(first.calls.load(Ordering::SeqCst), 1);
    assert_eq!(second.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn full_sse_preserves_client_usage_preference_while_accounting_upstream_usage() {
    let provider = Arc::new(SseProvider::success());
    let runtime = runtime_with(vec![provider], 1, 4096, Arc::new(RecordingAudit::default()));
    let http = LlmBufferedHttp::new(
        Arc::clone(&runtime),
        Arc::new(Allow),
        4096,
        32,
        Duration::from_secs(1),
    );
    let LlmHttpResponse::Streaming(mut response) = http
        .handle_route(http_request(
            br#"{"model":"public-model","messages":[{"role":"user","content":"hello"}],"stream":true}"#,
        ))
        .await
    else {
        panic!("expected SSE response");
    };
    let mut body = Vec::new();
    while let Some(frame) = response.stream.next_frame().await {
        body.extend_from_slice(&frame);
    }
    let body = String::from_utf8(body).unwrap();
    assert!(!body.contains("\"usage\""));
    assert!(body.ends_with("data: [DONE]\n\n"));
    assert_eq!(
        runtime.snapshot().aliases["public-model"].ledger.charged(),
        5
    );
}

#[tokio::test]
async fn early_sse_disconnect_cancels_upstream_and_releases_stream_permits() {
    let observed = Arc::new(AtomicBool::new(false));
    let provider = Arc::new(SseProvider {
        format: ProviderFormat::OpenAi,
        events: vec![Ok(InferenceEvent::TextDelta {
            text: "first".to_string(),
        })],
        calls: AtomicUsize::new(0),
        wait_for_cancellation: true,
        cancellation_observed: Arc::clone(&observed),
    });
    let runtime = runtime_with(
        vec![provider.clone()],
        1,
        4096,
        Arc::new(RecordingAudit::default()),
    );
    let http = LlmBufferedHttp::new(
        Arc::clone(&runtime),
        Arc::new(Allow),
        4096,
        32,
        Duration::from_secs(1),
    );
    let response = http
        .handle_route(http_request(
            br#"{"model":"public-model","messages":[{"role":"user","content":"hello"}],"stream":true}"#,
        ))
        .await;
    let LlmHttpResponse::Streaming(mut response) = response else {
        panic!("expected SSE response");
    };
    assert!(response.stream.next_frame().await.is_some());
    drop(response);
    tokio::time::timeout(Duration::from_secs(1), async {
        while !observed.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("upstream cancellation must be observed");

    let second = runtime
        .execute_stream_with_snapshot(
            LlmRequestContext::with_timeout("user", Duration::from_secs(1)),
            runtime.snapshot(),
            InferenceRequest::text("public-model", "second"),
        )
        .await
        .expect("disconnect must release the single stream permit");
    drop(second);
}

#[tokio::test]
async fn full_sse_slow_consumer_is_bounded_and_releases_stream_permits() {
    let provider = Arc::new(SseProvider::success());
    let runtime = runtime_with(vec![provider], 1, 4096, Arc::new(RecordingAudit::default()));
    let first = runtime
        .execute_stream_with_snapshot(
            LlmRequestContext::with_timeout("user", Duration::from_secs(1)),
            runtime.snapshot(),
            InferenceRequest::text("public-model", "first"),
        )
        .await
        .unwrap();
    // The one-frame channel remains full. The producer must stop at the
    // independent write-progress deadline rather than buffering the stream.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let second = runtime
        .execute_stream_with_snapshot(
            LlmRequestContext::with_timeout("user", Duration::from_secs(1)),
            runtime.snapshot(),
            InferenceRequest::text("public-model", "second"),
        )
        .await
        .expect("slow consumer must release the single stream permit");
    drop(first);
    drop(second);
}

#[tokio::test]
async fn full_sse_rejects_duplicate_terminal_events_without_done() {
    let provider = Arc::new(SseProvider {
        format: ProviderFormat::OpenAi,
        events: vec![
            Ok(InferenceEvent::TextDelta {
                text: "visible".to_string(),
            }),
            Ok(InferenceEvent::MessageEnd {
                finish_reason: FinishReason::Stop,
                terminal_state: TerminalState::Complete,
            }),
            Ok(InferenceEvent::MessageEnd {
                finish_reason: FinishReason::Stop,
                terminal_state: TerminalState::Complete,
            }),
        ],
        calls: AtomicUsize::new(0),
        wait_for_cancellation: false,
        cancellation_observed: Arc::new(AtomicBool::new(false)),
    });
    let runtime = runtime_with(vec![provider], 1, 4096, Arc::new(RecordingAudit::default()));
    let mut execution = runtime
        .execute_stream_with_snapshot(
            LlmRequestContext::with_timeout("user", Duration::from_secs(1)),
            runtime.snapshot(),
            InferenceRequest::text("public-model", "hello"),
        )
        .await
        .unwrap();
    let mut body = Vec::new();
    while let Some(frame) = execution.next_frame().await {
        body.extend_from_slice(&frame);
    }
    let body = String::from_utf8(body).unwrap();
    assert!(body.contains("visible"));
    assert!(body.contains("provider_error"));
    assert!(!body.contains("[DONE]"));
}

#[tokio::test]
async fn early_sse_deadline_cancels_a_trickling_provider_and_releases_permits() {
    let observed = Arc::new(AtomicBool::new(false));
    let provider = Arc::new(SseProvider {
        format: ProviderFormat::OpenAi,
        events: vec![Ok(InferenceEvent::TextDelta {
            text: "first".to_string(),
        })],
        calls: AtomicUsize::new(0),
        wait_for_cancellation: true,
        cancellation_observed: Arc::clone(&observed),
    });
    let runtime = runtime_with(vec![provider], 1, 4096, Arc::new(RecordingAudit::default()));
    let http = LlmBufferedHttp::new(
        Arc::clone(&runtime),
        Arc::new(Allow),
        4096,
        32,
        Duration::from_millis(25),
    );
    let LlmHttpResponse::Streaming(mut response) = http
        .handle_route(http_request(
            br#"{"model":"public-model","messages":[{"role":"user","content":"hello"}],"stream":true}"#,
        ))
        .await
    else {
        panic!("expected SSE response");
    };
    let mut body = Vec::new();
    tokio::time::timeout(Duration::from_secs(1), async {
        while let Some(frame) = response.stream.next_frame().await {
            body.extend_from_slice(&frame);
        }
    })
    .await
    .expect("the request deadline must terminate a trickling stream");
    let body = String::from_utf8(body).unwrap();
    assert!(body.contains("first"));
    assert!(!body.contains("[DONE]"));
    assert!(observed.load(Ordering::SeqCst));

    let second = runtime
        .execute_stream_with_snapshot(
            LlmRequestContext::with_timeout("user", Duration::from_secs(1)),
            runtime.snapshot(),
            InferenceRequest::text("public-model", "second"),
        )
        .await
        .expect("deadline termination must release the stream permit");
    drop(second);
}

#[tokio::test]
async fn full_sse_setup_deadline_cancels_a_provider_that_stalls_before_first_event() {
    let observed = Arc::new(AtomicBool::new(false));
    let provider = Arc::new(SseProvider {
        format: ProviderFormat::OpenAi,
        events: Vec::new(),
        calls: AtomicUsize::new(0),
        wait_for_cancellation: true,
        cancellation_observed: Arc::clone(&observed),
    });
    let runtime = runtime_with(vec![provider], 1, 4096, Arc::new(RecordingAudit::default()));
    let started = Instant::now();
    assert!(
        runtime
            .execute_stream_with_snapshot(
                LlmRequestContext::with_timeout("user", Duration::from_secs(1)),
                runtime.snapshot(),
                InferenceRequest::text("public-model", "hello"),
            )
            .await
            .is_err()
    );
    assert!(started.elapsed() < Duration::from_millis(500));
    tokio::time::timeout(Duration::from_secs(1), async {
        while !observed.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("setup timeout must cancel the stalled provider");
}

#[tokio::test]
async fn early_sse_headers_wait_for_the_durable_start_barrier() {
    let provider = Arc::new(SseProvider::success());
    let barrier = Arc::new(BlockingStartBarrier {
        entered: AtomicBool::new(false),
        release: Semaphore::new(0),
    });
    let runtime = Arc::try_unwrap(runtime_with(
        vec![provider.clone()],
        1,
        4096,
        Arc::new(RecordingAudit::default()),
    ))
    .ok()
    .expect("runtime has one owner")
    .with_stream_start_barrier(barrier.clone());
    let http = Arc::new(LlmBufferedHttp::new(
        Arc::new(runtime),
        Arc::new(Allow),
        4096,
        32,
        Duration::from_secs(1),
    ));
    let task = tokio::spawn({
        let http = Arc::clone(&http);
        async move {
            http.handle_route(http_request(
                br#"{"model":"public-model","messages":[{"role":"user","content":"hello"}],"stream":true}"#,
            ))
            .await
        }
    });
    tokio::time::timeout(Duration::from_secs(1), async {
        while !barrier.entered.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("barrier must be reached");
    assert!(!task.is_finished());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    barrier.release.add_permits(1);
    assert!(matches!(task.await.unwrap(), LlmHttpResponse::Streaming(_)));
}

#[tokio::test]
async fn local_durable_audit_commits_attempt_start_before_provider_dispatch() {
    let directory = tempfile::tempdir().unwrap();
    let audit = Arc::new(
        WalAudit::open(
            WalConfig {
                directory: directory.path().to_path_buf(),
                gateway_instance: "gateway-test".to_string(),
                max_record_bytes: 4096,
                max_segment_bytes: 32 * 1024,
                max_spool_bytes: 128 * 1024,
                queue_records: 16,
                batch_records: 8,
                batch_bytes: 32 * 1024,
                commit_delay: Duration::from_millis(5),
                terminal_commit_before_response: false,
                persistent_volume: true,
            },
            "host-a",
        )
        .unwrap(),
    );
    let provider = Arc::new(DurableStartProvider {
        audit: Arc::clone(&audit),
        durable_before_dispatch: AtomicBool::new(false),
    });
    let runtime = runtime_with_mode(
        vec![provider.clone()],
        1,
        4096,
        audit,
        AuditMode::LocalDurable,
    );
    let http = LlmBufferedHttp::new(runtime, Arc::new(Allow), 4096, 32, Duration::from_secs(1));
    let LlmHttpResponse::Streaming(mut response) = http
        .handle_route(http_request(
            br#"{"model":"public-model","messages":[{"role":"user","content":"hello"}],"stream":true}"#,
        ))
        .await
    else {
        panic!("expected SSE response");
    };
    while response.stream.next_frame().await.is_some() {}
    assert!(provider.durable_before_dispatch.load(Ordering::SeqCst));
}

#[tokio::test]
async fn local_durable_buffered_audit_commits_attempt_start_before_provider_dispatch() {
    let directory = tempfile::tempdir().unwrap();
    let audit = Arc::new(
        WalAudit::open(
            WalConfig {
                directory: directory.path().to_path_buf(),
                gateway_instance: "gateway-buffered-test".to_string(),
                max_record_bytes: 4096,
                max_segment_bytes: 32 * 1024,
                max_spool_bytes: 128 * 1024,
                queue_records: 16,
                batch_records: 8,
                batch_bytes: 32 * 1024,
                commit_delay: Duration::from_millis(5),
                terminal_commit_before_response: false,
                persistent_volume: true,
            },
            "host-a",
        )
        .unwrap(),
    );
    let provider = Arc::new(DurableStartProvider {
        audit: Arc::clone(&audit),
        durable_before_dispatch: AtomicBool::new(false),
    });
    let runtime = runtime_with_mode(
        vec![provider.clone()],
        1,
        4096,
        audit,
        AuditMode::LocalDurable,
    );
    let http = LlmBufferedHttp::new(runtime, Arc::new(Allow), 4096, 32, Duration::from_secs(1));
    assert!(matches!(
        http.handle_route(http_request(
            br#"{"model":"public-model","messages":[{"role":"user","content":"hello"}]}"#,
        ))
        .await,
        LlmHttpResponse::Buffered(_)
    ));
    assert!(provider.durable_before_dispatch.load(Ordering::SeqCst));
}

#[tokio::test]
async fn early_sse_never_emits_done_or_retries_after_visible_output_error() {
    let provider = Arc::new(SseProvider {
        format: ProviderFormat::OpenAi,
        events: vec![
            Ok(InferenceEvent::TextDelta {
                text: "visible".to_string(),
            }),
            Err(InferenceError::from_status(503, None, "down")),
        ],
        calls: AtomicUsize::new(0),
        wait_for_cancellation: false,
        cancellation_observed: Arc::new(AtomicBool::new(false)),
    });
    let runtime = runtime_with(
        vec![provider.clone()],
        1,
        4096,
        Arc::new(RecordingAudit::default()),
    );
    let http = LlmBufferedHttp::new(runtime, Arc::new(Allow), 4096, 32, Duration::from_secs(1));
    let LlmHttpResponse::Streaming(mut response) = http
        .handle_route(http_request(
            br#"{"model":"public-model","messages":[{"role":"user","content":"hello"}],"stream":true}"#,
        ))
        .await
    else {
        panic!("expected SSE response");
    };
    let mut body = Vec::new();
    while let Some(frame) = response.stream.next_frame().await {
        body.extend_from_slice(&frame);
    }
    let body = String::from_utf8(body).unwrap();
    assert!(body.contains("visible"));
    assert!(body.contains("The model stream terminated before completion."));
    assert!(!body.contains("down"));
    assert!(!body.contains("[DONE]"));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn models_never_enumerate_internal_aliases() {
    let provider = Arc::new(ScriptedProvider::new(
        ProviderFormat::OpenAi,
        vec![Ok(success_response())],
    ));
    let runtime = runtime_with(vec![provider], 1, 4096, Arc::new(RecordingAudit::default()));
    let http = LlmBufferedHttp::new(runtime, Arc::new(Allow), 4096, 32, Duration::from_secs(1));
    let response = http
        .handle(BufferedHttpRequest {
            method: "GET".to_string(),
            path: "/v1/models".to_string(),
            headers: BTreeMap::new(),
            body: Vec::new(),
            principal_id: "test-agent".to_string(),
            trusted_request_id: "trusted".to_string(),
        })
        .await;
    let body = String::from_utf8(response.body).unwrap();
    assert_eq!(response.status, 200);
    assert!(body.contains("public-model"));
    assert!(!body.contains("legacy-agent-internal"));
}

#[tokio::test]
async fn internal_alias_invocation_is_bound_to_its_approved_principal() {
    let provider = Arc::new(ScriptedProvider::new(
        ProviderFormat::OpenAi,
        vec![Ok(success_response())],
    ));
    let runtime = runtime_with(
        vec![provider.clone()],
        1,
        4096,
        Arc::new(RecordingAudit::default()),
    );
    let http = LlmBufferedHttp::new(runtime, Arc::new(Allow), 4096, 32, Duration::from_secs(1));
    let mut request = http_request(
        br#"{"model":"legacy-agent-internal","messages":[{"role":"user","content":"hello"}]}"#,
    );
    request.principal_id = "different-agent".to_string();
    let response = http.handle(request).await;
    assert_eq!(response.status, 404);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert!(!String::from_utf8_lossy(&response.body).contains("legacy-agent-internal"));
}

#[tokio::test]
async fn buffered_errors_preserve_retry_after_and_use_client_fault_message() {
    let rate_limited = Arc::new(ScriptedProvider::new(
        ProviderFormat::OpenAi,
        vec![Err(InferenceError::from_status(
            429,
            Some("3"),
            "secret upstream detail",
        ))],
    ));
    let runtime = runtime_with(
        vec![rate_limited],
        1,
        4096,
        Arc::new(RecordingAudit::default()),
    );
    let http = LlmBufferedHttp::new(runtime, Arc::new(Allow), 4096, 32, Duration::from_secs(1));
    let response = http
        .handle(http_request(
            br#"{"model":"public-model","messages":[{"role":"user","content":"hello"}]}"#,
        ))
        .await;
    assert_eq!(response.status, 429);
    assert_eq!(
        response.headers.get("retry-after").map(String::as_str),
        Some("3")
    );
    assert!(!String::from_utf8_lossy(&response.body).contains("secret upstream detail"));

    let invalid = Arc::new(ScriptedProvider::new(
        ProviderFormat::OpenAi,
        vec![Err(InferenceError::invalid_request(
            "secret invalid detail",
        ))],
    ));
    let runtime = runtime_with(vec![invalid], 1, 4096, Arc::new(RecordingAudit::default()));
    let http = LlmBufferedHttp::new(runtime, Arc::new(Allow), 4096, 32, Duration::from_secs(1));
    let response = http
        .handle(http_request(
            br#"{"model":"public-model","messages":[{"role":"user","content":"hello"}]}"#,
        ))
        .await;
    let body = String::from_utf8(response.body).unwrap();
    assert_eq!(response.status, 400);
    assert!(body.contains("rejected by the model provider"));
    assert!(!body.contains("secret invalid detail"));
}

#[tokio::test]
async fn partial_usage_keeps_total_tokens_unknown() {
    let mut partial = success_response();
    partial.usage = Some(NormalizedUsage {
        input_tokens: Some(10),
        output_tokens: None,
        cached_input_tokens: None,
        reasoning_tokens: None,
    });
    let provider = Arc::new(ScriptedProvider::new(
        ProviderFormat::OpenAi,
        vec![Ok(partial)],
    ));
    let runtime = runtime_with(vec![provider], 1, 4096, Arc::new(RecordingAudit::default()));
    let http = LlmBufferedHttp::new(runtime, Arc::new(Allow), 4096, 32, Duration::from_secs(1));
    let response = http
        .handle(http_request(
            br#"{"model":"public-model","messages":[{"role":"user","content":"hello"}]}"#,
        ))
        .await;
    let body: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(
        body.pointer("/usage/prompt_tokens")
            .and_then(|value| value.as_u64()),
        Some(10)
    );
    assert!(
        body.pointer("/usage/completion_tokens")
            .is_some_and(serde_json::Value::is_null)
    );
    assert!(
        body.pointer("/usage/total_tokens")
            .is_some_and(serde_json::Value::is_null)
    );
}

fn request_pii_profile(unresolved: UnresolvedPiiBehavior) -> PiiProfile {
    PiiProfile {
        enabled: true,
        unresolved,
        kinds: BTreeSet::from([PiiKind::Email]),
        ..PiiProfile::default()
    }
}

#[test]
fn pii_alias_requires_placeholder_preservation_evidence_from_every_deployment() {
    let compiler = LlmCompiler::new(Arc::new(MapSecretResolver(BTreeMap::from([(
        "secret".to_string(),
        "value".to_string(),
    )]))));
    let mut config = compiler_config();
    config.aliases.get_mut("public-model").unwrap().pii =
        request_pii_profile(UnresolvedPiiBehavior::LeaveMasked);
    config
        .deployments
        .get_mut("d")
        .unwrap()
        .pii_placeholder_preservation_percent = 99;
    assert!(compiler.compile(&config, 1, None).is_err());

    config
        .deployments
        .get_mut("d")
        .unwrap()
        .pii_placeholder_preservation_percent = 100;
    assert!(compiler.compile(&config, 1, None).is_ok());
}

#[tokio::test]
async fn request_scoped_pii_tokenizes_before_provider_and_recovers_buffered_response() {
    let received = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(PiiEchoProvider {
        received: Arc::clone(&received),
    });
    let runtime = runtime_with_mode_and_pii(
        vec![provider],
        1,
        4096,
        Arc::new(RecordingAudit::default()),
        AuditMode::Required,
        request_pii_profile(UnresolvedPiiBehavior::LeaveMasked),
    );

    let execution = runtime
        .execute(
            LlmRequestContext::with_timeout("principal", Duration::from_secs(1)),
            InferenceRequest::text("public-model", "contact a@example.com"),
        )
        .await
        .unwrap();

    let provider_request = &received.lock().unwrap()[0];
    assert!(!provider_request.contains("a@example.com"));
    assert!(provider_request.contains("[[PII:v1:email:"));
    let ContentBlock::Text { text } = &execution.response.content[0] else {
        panic!("expected text response")
    };
    assert_eq!(text, "contact a@example.com");
}

#[tokio::test]
async fn request_scoped_pii_recovers_fragmented_stream_without_exposing_partial_token() {
    let received = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(PiiEchoProvider {
        received: Arc::clone(&received),
    });
    let runtime = runtime_with_mode_and_pii(
        vec![provider],
        1,
        4096,
        Arc::new(RecordingAudit::default()),
        AuditMode::Required,
        request_pii_profile(UnresolvedPiiBehavior::LeaveMasked),
    );
    let root = runtime.snapshot();
    let mut execution = runtime
        .execute_stream_with_snapshot(
            LlmRequestContext::with_timeout("principal", Duration::from_secs(1)),
            root,
            InferenceRequest::text("public-model", "a@example.com"),
        )
        .await
        .unwrap();
    let mut body = Vec::new();
    while let Some(frame) = execution.next_frame().await {
        body.extend_from_slice(&frame);
    }
    let body = String::from_utf8(body).unwrap();

    assert!(!received.lock().unwrap()[0].contains("a@example.com"));
    assert!(body.contains("echo "));
    assert!(body.contains("a@example.com"));
    assert!(!body.contains("[[PII:"));
    assert!(body.contains("data: [DONE]"));
}

#[tokio::test]
async fn reject_buffered_pii_profile_rejects_streaming_before_provider_dispatch() {
    let received = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(PiiEchoProvider {
        received: Arc::clone(&received),
    });
    let runtime = runtime_with_mode_and_pii(
        vec![provider],
        1,
        4096,
        Arc::new(RecordingAudit::default()),
        AuditMode::Required,
        request_pii_profile(UnresolvedPiiBehavior::RejectBuffered),
    );
    let result = runtime
        .execute_stream_with_snapshot(
            LlmRequestContext::with_timeout("principal", Duration::from_secs(1)),
            runtime.snapshot(),
            InferenceRequest::text("public-model", "a@example.com"),
        )
        .await;

    assert!(matches!(result, Err(LlmGatewayError::InvalidRequest(_))));
    assert!(received.lock().unwrap().is_empty());
}
