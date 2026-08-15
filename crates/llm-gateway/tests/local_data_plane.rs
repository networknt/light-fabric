use async_trait::async_trait;
use base64::Engine as _;
use futures_util::stream;
use llm_gateway::audit::{
    AuditAdmission, AuditFinish, AuditReservation, AuditStart, AuditTransportContext, WalAudit,
    WalConfig,
};
use llm_gateway::config::{
    AliasCapabilityRequirements, AliasConfig, AuditMode, AwsCredentialSource,
    BedrockDeploymentPolicy, DeploymentConfig, EmbeddingWorkloadLane, EndpointAuth,
    LlmRouterConfig, NetworkProfileMode, NetworkTermination, NetworkZone, ProviderConfig,
    ProviderProfileType, ReadinessPolicy, RuntimeCapacity,
};
use llm_gateway::credentials::MapSecretResolver;
use llm_gateway::http::{BodyAccessControl, BufferedHttpRequest, LlmBufferedHttp, LlmHttpResponse};
use llm_gateway::pii::{PiiKind, PiiProfile, UnresolvedPiiBehavior};
use llm_gateway::routing::{OwnedCircuitPermit, PassiveCircuit};
use llm_gateway::runtime::{
    AliasPlan, CompileProbe, DeploymentReadiness, DeploymentReadinessState, DeploymentRuntime,
    LlmCompiler, LlmPublishedSnapshot, LlmSnapshotStore, PrincipalPermitStripes,
    ProviderAccountRuntime, PublishOutcome, StreamStartBarrier,
};
use llm_gateway::usage::{
    EmbeddingPrice, GenerationPrice, OperationPrice, UsageLedger, UsageReservation,
};
use llm_gateway::{LlmGatewayError, LlmRequestContext, LlmRuntime};
use model_provider::conformance::{CapabilityRequirements, ConformanceResult, FixtureProvenance};
use model_provider::inference::{
    AcceptanceEvidence, CompiledProvider, ContentBlock, ContentCapabilities, EmbeddingCapabilities,
    EmbeddingDistanceMetric, EmbeddingEncoding, EmbeddingNormalization, EmbeddingProvider,
    EmbeddingRequest, EmbeddingResponse, EmbeddingSpaceContract, EmbeddingVector, FinishReason,
    GenerateOutputItem, GenerationCapabilities, GenerationProvider, GenerationStream,
    InferenceError, InferenceEvent, InferenceRequest, InferenceResponse, ItemStatus,
    NormalizedUsage, Operation, ProviderCapabilities, ProviderEvidence, ProviderProtocol,
    ProviderRequestContext, Role, TerminalState,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

struct ScriptedProvider {
    protocol: ProviderProtocol,
    results: Mutex<VecDeque<Result<InferenceResponse, InferenceError>>>,
    calls: AtomicUsize,
    capabilities: GenerationCapabilities,
}

struct SseProvider {
    protocol: ProviderProtocol,
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

struct ScriptedEmbeddingProvider {
    results: Mutex<VecDeque<Result<EmbeddingResponse, InferenceError>>>,
    calls: AtomicUsize,
    received_dimensions: Mutex<Vec<Option<u32>>>,
    capabilities: EmbeddingCapabilities,
}

#[async_trait]
impl EmbeddingProvider for ScriptedEmbeddingProvider {
    fn protocol(&self) -> ProviderProtocol {
        ProviderProtocol::OpenAiEmbeddings
    }

    fn capabilities(&self) -> EmbeddingCapabilities {
        self.capabilities.clone()
    }

    async fn embed(
        &self,
        _context: ProviderRequestContext,
        request: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, InferenceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.received_dimensions
            .lock()
            .unwrap()
            .push(request.dimensions);
        self.results
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Ok(embedding_response(2, 2, 3, 0)))
    }
}

#[async_trait]
impl GenerationProvider for PiiEchoProvider {
    fn protocol(&self) -> ProviderProtocol {
        ProviderProtocol::OpenAiChat
    }

    fn capabilities(&self) -> GenerationCapabilities {
        generation_capabilities(true, true, true)
    }

    async fn generate(
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
        response.output = vec![GenerateOutputItem::Message {
            id: "message-0".to_string(),
            role: Role::Assistant,
            content: vec![ContentBlock::text(text.clone())],
            status: ItemStatus::Completed,
        }];
        Ok(response)
    }

    async fn generate_stream(
        &self,
        _context: ProviderRequestContext,
        request: InferenceRequest,
    ) -> Result<GenerationStream, InferenceError> {
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
impl GenerationProvider for DurableStartProvider {
    fn protocol(&self) -> ProviderProtocol {
        ProviderProtocol::OpenAiChat
    }

    fn capabilities(&self) -> GenerationCapabilities {
        generation_capabilities(true, true, true)
    }

    async fn generate(
        &self,
        _context: ProviderRequestContext,
        _request: InferenceRequest,
    ) -> Result<InferenceResponse, InferenceError> {
        self.durable_before_dispatch
            .store(self.audit.status().durable_sequence >= 2, Ordering::SeqCst);
        Ok(success_response())
    }

    async fn generate_stream(
        &self,
        _context: ProviderRequestContext,
        _request: InferenceRequest,
    ) -> Result<GenerationStream, InferenceError> {
        self.durable_before_dispatch
            .store(self.audit.status().durable_sequence >= 2, Ordering::SeqCst);
        Ok(Box::pin(stream::iter(SseProvider::success().events)))
    }
}

impl SseProvider {
    fn success() -> Self {
        Self {
            protocol: ProviderProtocol::OpenAiChat,
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
impl GenerationProvider for SseProvider {
    fn protocol(&self) -> ProviderProtocol {
        self.protocol
    }

    fn capabilities(&self) -> GenerationCapabilities {
        generation_capabilities(true, true, true)
    }

    async fn generate(
        &self,
        _context: ProviderRequestContext,
        _request: InferenceRequest,
    ) -> Result<InferenceResponse, InferenceError> {
        Err(InferenceError::unsupported("stream-only test provider"))
    }

    async fn generate_stream(
        &self,
        context: ProviderRequestContext,
        _request: InferenceRequest,
    ) -> Result<GenerationStream, InferenceError> {
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
        protocol: ProviderProtocol,
        results: Vec<Result<InferenceResponse, InferenceError>>,
    ) -> Self {
        Self::with_capabilities(protocol, results, generation_capabilities(true, true, true))
    }

    fn with_capabilities(
        protocol: ProviderProtocol,
        results: Vec<Result<InferenceResponse, InferenceError>>,
        capabilities: GenerationCapabilities,
    ) -> Self {
        Self {
            protocol,
            results: Mutex::new(results.into()),
            calls: AtomicUsize::new(0),
            capabilities,
        }
    }
}

#[async_trait]
impl GenerationProvider for ScriptedProvider {
    fn protocol(&self) -> ProviderProtocol {
        self.protocol
    }
    fn capabilities(&self) -> GenerationCapabilities {
        self.capabilities.clone()
    }
    async fn generate(
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
    async fn generate_stream(
        &self,
        _context: ProviderRequestContext,
        _request: InferenceRequest,
    ) -> Result<GenerationStream, InferenceError> {
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

fn generation_capabilities(images: bool, tools: bool, structured: bool) -> GenerationCapabilities {
    GenerationCapabilities {
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
        output: vec![GenerateOutputItem::Message {
            id: "message-0".to_string(),
            role: Role::Assistant,
            content: vec![ContentBlock::text("ok")],
            status: ItemStatus::Completed,
        }],
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

fn embedding_capabilities() -> EmbeddingCapabilities {
    EmbeddingCapabilities {
        max_batch_items: 8,
        max_input_tokens_per_item: 10,
        max_aggregate_input_tokens: 80,
        supported_dimensions: BTreeSet::from([2]),
        supported_encodings: BTreeSet::from([EmbeddingEncoding::Float, EmbeddingEncoding::Base64]),
        max_response_bytes: 4096,
        space: Some(embedding_space(2)),
    }
}

fn embedding_space(dimension: u32) -> EmbeddingSpaceContract {
    EmbeddingSpaceContract {
        space_id: format!("test-space-{dimension}"),
        revision: 1,
        dimension,
        normalization: EmbeddingNormalization::L2,
        distance_metric: EmbeddingDistanceMetric::Cosine,
        document_input_transform_version: "identity-v1".to_string(),
    }
}

fn scripted_embedding_provider(
    results: Vec<Result<EmbeddingResponse, InferenceError>>,
) -> Arc<ScriptedEmbeddingProvider> {
    Arc::new(ScriptedEmbeddingProvider {
        results: Mutex::new(results.into()),
        calls: AtomicUsize::new(0),
        received_dimensions: Mutex::new(Vec::new()),
        capabilities: embedding_capabilities(),
    })
}

fn embedding_response(
    count: usize,
    dimensions: usize,
    input_tokens: u64,
    output_tokens: u64,
) -> EmbeddingResponse {
    EmbeddingResponse {
        vectors: (0..count)
            .map(|index| EmbeddingVector {
                index: index as u32,
                values: (0..dimensions)
                    .map(|dimension| (index + dimension + 1) as f32 / 10.0)
                    .collect(),
            })
            .collect(),
        usage: NormalizedUsage {
            input_tokens: Some(input_tokens),
            output_tokens: (output_tokens != 0).then_some(output_tokens),
            cached_input_tokens: None,
            reasoning_tokens: None,
        },
        evidence: ProviderEvidence::default(),
    }
}

fn embedding_runtime(
    providers: Vec<(Arc<ScriptedEmbeddingProvider>, EmbeddingPrice)>,
    attempts: usize,
    max_cost_micros: Option<u64>,
) -> (Arc<LlmRuntime>, Arc<UsageLedger>) {
    embedding_runtime_with_policy(
        providers,
        attempts,
        max_cost_micros,
        false,
        EmbeddingWorkloadLane::Standard,
        EmbeddingWorkloadLane::Standard,
    )
}

fn embedding_runtime_with_policy(
    providers: Vec<(Arc<ScriptedEmbeddingProvider>, EmbeddingPrice)>,
    attempts: usize,
    max_cost_micros: Option<u64>,
    require_expected_embedding_space: bool,
    alias_lane: EmbeddingWorkloadLane,
    root_lane: EmbeddingWorkloadLane,
) -> (Arc<LlmRuntime>, Arc<UsageLedger>) {
    let deployments = providers
        .into_iter()
        .enumerate()
        .map(|(index, (provider, price))| {
            let provider: Arc<dyn EmbeddingProvider> = provider;
            Arc::new(DeploymentRuntime {
                id: format!("embed-{index}"),
                provider_endpoint_id: format!("embed-{index}"),
                model: format!("embed-{index}-physical"),
                configured_concurrency: 4,
                bedrock_policy: None,
                capabilities: ProviderCapabilities {
                    operations: BTreeSet::from([Operation::Embed]),
                    generation: None,
                    embedding: Some(provider.capabilities()),
                },
                provider: CompiledProvider::Embedding(provider),
                provider_digest: format!("embed-{index}"),
                provider_client_generation: 1,
                provider_client_built_at: Instant::now(),
                audit_transport: test_audit_transport(&format!("embed-{index}")),
                conformance_result: None,
                required_conformance_provenance: None,
                readiness_policy: ReadinessPolicy::Immediate,
                readiness: Arc::new(DeploymentReadiness::new(DeploymentReadinessState::Ready)),
                cold_start_timeout_ms: 30_000,
                request_timeout_ms: 30_000,
                stream_setup_timeout_ms: 10_000,
                permits: Arc::new(Semaphore::new(4)),
                circuit: Arc::new(PassiveCircuit::new(3, Duration::ZERO)),
                account: Arc::new(ProviderAccountRuntime {
                    provider_account_id: format!("embed-{index}"),
                    quota_group_id: format!("embed-{index}"),
                    configured_concurrency: 4,
                    permits: Arc::new(Semaphore::new(4)),
                }),
                prices: BTreeMap::from([(Operation::Embed, OperationPrice::Embed(price))]),
            })
        })
        .collect::<Vec<_>>();
    let ledger = Arc::new(UsageLedger::default());
    let alias = Arc::new(AliasPlan {
        public_name: "embedding-default".to_string(),
        deployments: deployments.clone(),
        operations: BTreeSet::from([Operation::Embed]),
        max_attempts: attempts,
        configured_concurrency: 2,
        permits: Arc::new(Semaphore::new(2)),
        max_input_tokens: None,
        max_output_tokens: None,
        max_cost_micros,
        internal: false,
        bound_principal: None,
        audit: AuditMode::Required,
        pii: PiiProfile::default(),
        required_capabilities: AliasCapabilityRequirements {
            embedding_space: Some(embedding_space(2)),
            ..Default::default()
        },
        require_expected_embedding_space,
        embedding_workload_lane: alias_lane,
        ledger: Arc::clone(&ledger),
    });
    let snapshot = LlmPublishedSnapshot {
        generation: 7,
        digest: "embedding-root".to_string(),
        global_concurrency: 2,
        global_stream_concurrency: 1,
        stream_channel_capacity: 1,
        max_stream_response_bytes: 16 * 1024,
        stream_write_timeout_ms: 100,
        stream_setup_timeout_ms: 100,
        stream_idle_timeout_ms: 100,
        stream_minimum_drain_bytes_per_second: 1,
        stream_drain_grace_ms: 100,
        max_replay_bytes: 4096,
        embedding_memory: llm_gateway::runtime::EmbeddingMemoryBounds {
            admission_slots: 1,
            per_slot_peak_bytes: 64 * 1024,
            aggregate_peak_bytes: 64 * 1024,
            max_memory_bytes: 64 * 1024,
            max_request_body_bytes: 4096,
            max_replay_bytes: 4096,
            max_replay_resident_bytes: 12 * 1024,
            max_canonical_vector_bytes: 4096,
            max_rendered_response_bytes: 4096,
            overlapping_provider_response_bytes: 4096,
            ingress_concurrency: 2,
            max_ingress_resident_bytes: 8192,
            aggregate_ingress_bytes: 16 * 1024,
            max_ingress_memory_bytes: 16 * 1024,
            items_per_permit: 2,
            write_timeout_ms: 1000,
            minimum_drain_bytes_per_second: 1,
            max_input_bytes_per_item: 1024,
            max_total_input_bytes: 2048,
            body_read_timeout_ms: 1000,
            minimum_receive_bytes_per_second: 1,
            authorization_timeout_ms: 1000,
        },
        embedding_memory_permits: Arc::new(Semaphore::new(1)),
        embedding_workload_lane: root_lane,
        aliases: BTreeMap::from([("embedding-default".to_string(), alias)]),
        deployments: deployments
            .into_iter()
            .map(|deployment| (deployment.id.clone(), deployment))
            .collect(),
        principal_permits: Arc::new(PrincipalPermitStripes::new(8, 2)),
        reasoning_sealer: Arc::new(Default::default()),
        reasoning_key_set_generation: 0,
        anthropic_messages_enabled: false,
        reasoning_key_set_digest: String::new(),
    };
    (
        Arc::new(LlmRuntime::new(
            Arc::new(LlmSnapshotStore::new(snapshot, 2)),
            Arc::new(RecordingAudit::default()),
        )),
        ledger,
    )
}

fn test_embedding_memory_bounds(
    admission_slots: usize,
) -> llm_gateway::runtime::EmbeddingMemoryBounds {
    llm_gateway::runtime::EmbeddingMemoryBounds {
        admission_slots,
        per_slot_peak_bytes: 64 * 1024,
        aggregate_peak_bytes: 64 * 1024 * admission_slots,
        max_memory_bytes: 64 * 1024 * admission_slots,
        max_request_body_bytes: 4096,
        max_replay_bytes: 4096,
        max_replay_resident_bytes: 12 * 1024,
        max_canonical_vector_bytes: 4096,
        max_rendered_response_bytes: 4096,
        overlapping_provider_response_bytes: 4096,
        ingress_concurrency: 2,
        max_ingress_resident_bytes: 8192,
        aggregate_ingress_bytes: 16 * 1024,
        max_ingress_memory_bytes: 16 * 1024,
        items_per_permit: 2,
        write_timeout_ms: 1000,
        minimum_drain_bytes_per_second: 1,
        max_input_bytes_per_item: 1024,
        max_total_input_bytes: 2048,
        body_read_timeout_ms: 1000,
        minimum_receive_bytes_per_second: 1,
        authorization_timeout_ms: 1000,
    }
}

fn deployment(id: &str, provider: Arc<dyn GenerationProvider>) -> Arc<DeploymentRuntime> {
    Arc::new(DeploymentRuntime {
        id: id.to_string(),
        provider_endpoint_id: id.to_string(),
        model: format!("{id}-physical"),
        configured_concurrency: 2,
        bedrock_policy: None,
        capabilities: ProviderCapabilities {
            operations: BTreeSet::from([Operation::Generate]),
            generation: Some(provider.capabilities()),
            embedding: None,
        },
        provider: CompiledProvider::Generation(provider),
        provider_digest: id.to_string(),
        provider_client_generation: 1,
        provider_client_built_at: Instant::now(),
        audit_transport: test_audit_transport(id),
        conformance_result: None,
        required_conformance_provenance: None,
        readiness_policy: ReadinessPolicy::Immediate,
        readiness: Arc::new(DeploymentReadiness::new(DeploymentReadinessState::Ready)),
        cold_start_timeout_ms: 30_000,
        request_timeout_ms: 30_000,
        stream_setup_timeout_ms: 10_000,
        permits: Arc::new(Semaphore::new(2)),
        circuit: Arc::new(PassiveCircuit::new(1, Duration::ZERO)),
        account: Arc::new(ProviderAccountRuntime {
            provider_account_id: id.to_string(),
            quota_group_id: id.to_string(),
            configured_concurrency: 2,
            permits: Arc::new(Semaphore::new(2)),
        }),
        prices: BTreeMap::from([(
            Operation::Generate,
            OperationPrice::Generate(GenerationPrice {
                version: 7,
                input_micros_per_million: 1_000_000,
                output_micros_per_million: 2_000_000,
            }),
        )]),
    })
}

fn occupy_half_open_probe(circuit: &Arc<PassiveCircuit>) -> OwnedCircuitPermit {
    for _ in 0..3 {
        circuit.failure(
            &InferenceError::from_status(503, None, "down"),
            Instant::now(),
        );
    }
    circuit
        .acquire_owned(Instant::now() + Duration::from_millis(2))
        .expect("test owns the single half-open probe")
}

fn conformance_result(provenance: FixtureProvenance, valid_until: &str) -> ConformanceResult {
    let mut result: ConformanceResult = serde_json::from_str(include_str!(
        "../../model-provider/conformance/results/openai-chat.json"
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
    provider: Arc<dyn GenerationProvider>,
    conformance_result: ConformanceResult,
) -> Arc<DeploymentRuntime> {
    Arc::new(DeploymentRuntime {
        id: "governed".to_string(),
        provider_endpoint_id: "governed".to_string(),
        model: conformance_result.physical_model.clone(),
        configured_concurrency: 2,
        bedrock_policy: None,
        capabilities: conformance_result.capabilities.clone(),
        provider: CompiledProvider::Generation(provider),
        provider_digest: "governed".to_string(),
        provider_client_generation: 1,
        provider_client_built_at: Instant::now(),
        audit_transport: test_audit_transport("governed"),
        conformance_result: Some(conformance_result),
        required_conformance_provenance: Some(FixtureProvenance::CapturedSanitized),
        readiness_policy: ReadinessPolicy::Immediate,
        readiness: Arc::new(DeploymentReadiness::new(DeploymentReadinessState::Ready)),
        cold_start_timeout_ms: 30_000,
        request_timeout_ms: 30_000,
        stream_setup_timeout_ms: 10_000,
        permits: Arc::new(Semaphore::new(2)),
        circuit: Arc::new(PassiveCircuit::new(1, Duration::ZERO)),
        account: Arc::new(ProviderAccountRuntime {
            provider_account_id: "governed".to_string(),
            quota_group_id: "governed".to_string(),
            configured_concurrency: 2,
            permits: Arc::new(Semaphore::new(2)),
        }),
        prices: BTreeMap::from([(
            Operation::Generate,
            OperationPrice::Generate(GenerationPrice {
                version: 7,
                input_micros_per_million: 1_000_000,
                output_micros_per_million: 2_000_000,
            }),
        )]),
    })
}

fn runtime_with_deployment(
    deployment: Arc<DeploymentRuntime>,
    audit: Arc<dyn AuditAdmission>,
) -> Arc<LlmRuntime> {
    let alias = Arc::new(AliasPlan {
        public_name: "public-model".to_string(),
        deployments: vec![Arc::clone(&deployment)],
        operations: BTreeSet::from([Operation::Generate]),
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
        require_expected_embedding_space: false,
        embedding_workload_lane: EmbeddingWorkloadLane::Standard,
        ledger: Arc::new(UsageLedger::default()),
    });
    let snapshot = LlmPublishedSnapshot {
        generation: 4,
        digest: "governed-root".to_string(),
        global_concurrency: 2,
        global_stream_concurrency: 1,
        stream_channel_capacity: 1,
        max_stream_response_bytes: 16 * 1024,
        stream_write_timeout_ms: 100,
        stream_setup_timeout_ms: 100,
        stream_idle_timeout_ms: 100,
        stream_minimum_drain_bytes_per_second: 1,
        stream_drain_grace_ms: 100,
        max_replay_bytes: 4096,
        embedding_memory: test_embedding_memory_bounds(1),
        embedding_memory_permits: Arc::new(Semaphore::new(1)),
        embedding_workload_lane: EmbeddingWorkloadLane::Standard,
        aliases: BTreeMap::from([("public-model".to_string(), alias)]),
        deployments: BTreeMap::from([(deployment.id.clone(), deployment)]),
        principal_permits: Arc::new(PrincipalPermitStripes::new(8, 2)),
        reasoning_sealer: Arc::new(Default::default()),
        reasoning_key_set_generation: 0,
        anthropic_messages_enabled: false,
        reasoning_key_set_digest: String::new(),
    };
    Arc::new(LlmRuntime::new(
        Arc::new(LlmSnapshotStore::new(snapshot, 2)),
        audit,
    ))
}

fn runtime_with(
    providers: Vec<Arc<dyn GenerationProvider>>,
    attempts: usize,
    max_replay: usize,
    audit: Arc<dyn AuditAdmission>,
) -> Arc<LlmRuntime> {
    runtime_with_mode(providers, attempts, max_replay, audit, AuditMode::Required)
}

fn runtime_with_mode(
    providers: Vec<Arc<dyn GenerationProvider>>,
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
    providers: Vec<Arc<dyn GenerationProvider>>,
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
        operations: BTreeSet::from([Operation::Generate]),
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
        require_expected_embedding_space: false,
        embedding_workload_lane: EmbeddingWorkloadLane::Standard,
        ledger: Arc::new(UsageLedger::default()),
    });
    let internal_alias = Arc::new(AliasPlan {
        public_name: "legacy-agent-internal".to_string(),
        deployments: vec![Arc::clone(&deployments[0])],
        operations: BTreeSet::from([Operation::Generate]),
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
        require_expected_embedding_space: false,
        embedding_workload_lane: EmbeddingWorkloadLane::Standard,
        ledger: Arc::new(UsageLedger::default()),
    });
    let snapshot = LlmPublishedSnapshot {
        generation: 4,
        digest: "root".to_string(),
        global_concurrency: 2,
        global_stream_concurrency: 1,
        stream_channel_capacity: 1,
        max_stream_response_bytes: 16 * 1024,
        stream_write_timeout_ms: 100,
        stream_setup_timeout_ms: 100,
        stream_idle_timeout_ms: 100,
        stream_minimum_drain_bytes_per_second: 1,
        stream_drain_grace_ms: 100,
        max_replay_bytes: max_replay,
        embedding_memory: test_embedding_memory_bounds(1),
        embedding_memory_permits: Arc::new(Semaphore::new(1)),
        embedding_workload_lane: EmbeddingWorkloadLane::Standard,
        aliases: BTreeMap::from([
            ("public-model".to_string(), alias),
            ("legacy-agent-internal".to_string(), internal_alias),
        ]),
        deployments: deployments
            .into_iter()
            .map(|deployment| (deployment.id.clone(), deployment))
            .collect(),
        principal_permits: Arc::new(PrincipalPermitStripes::new(8, 2)),
        reasoning_sealer: Arc::new(Default::default()),
        reasoning_key_set_generation: 0,
        anthropic_messages_enabled: false,
        reasoning_key_set_digest: String::new(),
    };
    Arc::new(LlmRuntime::new(
        Arc::new(LlmSnapshotStore::new(snapshot, 2)),
        audit,
    ))
}

#[test]
fn deployment_supports_requires_current_passing_captured_conformance() {
    let provider: Arc<dyn GenerationProvider> = Arc::new(ScriptedProvider::new(
        ProviderProtocol::OpenAiChat,
        Vec::new(),
    ));
    let requirements = CapabilityRequirements::generation();

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
        ProviderProtocol::OpenAiChat,
        vec![Ok(success_response())],
    ));
    let provider_runtime: Arc<dyn GenerationProvider> = provider.clone();
    let deployment = governed_deployment(
        provider_runtime,
        conformance_result(FixtureProvenance::CapturedSanitized, "2000-01-01T00:00:00Z"),
    );
    let runtime = runtime_with_deployment(deployment, Arc::new(RecordingAudit::default()));
    let request = InferenceRequest::text("public-model", "hello");

    assert!(runtime.visible_models().is_empty());
    assert!(matches!(
        runtime.eligible_formats(&runtime.snapshot(), "user", &request, false),
        Err(LlmGatewayError::NoReadyDeployment)
    ));
    assert!(matches!(
        runtime
            .execute(
                LlmRequestContext::with_timeout("user", Duration::from_secs(1)),
                request.clone(),
            )
            .await,
        Err(LlmGatewayError::NoReadyDeployment)
    ));
    assert!(matches!(
        runtime
            .execute_stream_with_snapshot(
                LlmRequestContext::with_timeout("user", Duration::from_secs(1)),
                runtime.snapshot(),
                request,
            )
            .await,
        Err(LlmGatewayError::NoReadyDeployment)
    ));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn lf5_single_attempt_never_uses_fallback_and_finalizes_audit() {
    let first = Arc::new(ScriptedProvider::new(
        ProviderProtocol::OpenAiChat,
        vec![Err(InferenceError::from_status(429, None, "limited"))],
    ));
    let second = Arc::new(ScriptedProvider::new(
        ProviderProtocol::AnthropicMessages,
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
async fn circuit_blocked_candidate_does_not_consume_the_generation_attempt_budget() {
    let first = Arc::new(ScriptedProvider::new(
        ProviderProtocol::OpenAiChat,
        vec![Ok(success_response())],
    ));
    let second = Arc::new(ScriptedProvider::new(
        ProviderProtocol::AnthropicMessages,
        vec![Ok(success_response())],
    ));
    let runtime = runtime_with(
        vec![first.clone(), second.clone()],
        1,
        4096,
        Arc::new(RecordingAudit::default()),
    );
    let root = runtime.snapshot();
    let _half_open = occupy_half_open_probe(&root.deployments["d0"].circuit);

    let execution = runtime
        .execute_with_snapshot(
            LlmRequestContext::with_timeout("user", Duration::from_secs(1)),
            root,
            InferenceRequest::text("public-model", "hello"),
        )
        .await
        .expect("the healthy fallback receives the one real attempt");

    assert_eq!(execution.attempts, 1);
    assert_eq!(first.calls.load(Ordering::SeqCst), 0);
    assert_eq!(second.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn lf5b_safe_failure_falls_back_once_and_reconciles_exact_usage() {
    let first = Arc::new(ScriptedProvider::new(
        ProviderProtocol::OpenAiChat,
        vec![Err(InferenceError::from_status(429, Some("1"), "limited"))],
    ));
    let second = Arc::new(ScriptedProvider::new(
        ProviderProtocol::AnthropicMessages,
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
        ProviderProtocol::OpenAiChat,
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
        ProviderProtocol::OpenAiChat,
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
        GenerationPrice {
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
fn dispatch_health_rechecks_readiness_before_acquiring_the_circuit() {
    let provider: Arc<dyn GenerationProvider> = Arc::new(ScriptedProvider::new(
        ProviderProtocol::OpenAiChat,
        vec![Ok(success_response())],
    ));
    let deployment = deployment("readiness-race", provider);
    assert!(deployment.supports(&CapabilityRequirements::generation()));

    deployment.readiness.mark_not_ready();

    assert!(matches!(
        deployment.acquire_dispatch_health(Instant::now()),
        Err(LlmGatewayError::NoReadyDeployment)
    ));
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

fn test_audit_transport(endpoint: &str) -> AuditTransportContext {
    AuditTransportContext {
        network_profile_mode: "public_tls".to_string(),
        termination: "native".to_string(),
        provider_endpoint_id: endpoint.to_string(),
        profile_digest: "a".repeat(64),
        physical_runtime_id: None,
        capacity_domain_id: None,
        pricing_basis: "external_provider".to_string(),
        trust_digest_prefix: None,
    }
}

fn compiler_config() -> LlmRouterConfig {
    LlmRouterConfig {
        enabled: true,
        development_fixtures: true,
        providers: BTreeMap::from([(
            "p".to_string(),
            ProviderConfig {
                provider_account_id: "p".to_string(),
                provider_type: Default::default(),
                provider_protocol: ProviderProtocol::OpenAiChat,
                aws_region: None,
                material_generation: 1,
                base_url: "http://127.0.0.1:9/v1".to_string(),
                endpoint_auth: EndpointAuth::Bearer {
                    credential_ref: "secret".to_string(),
                },
                network_profile: Default::default(),
                headers: BTreeMap::new(),
                quota_group_id: Some("quota".to_string()),
            },
        )]),
        deployments: BTreeMap::from([(
            "d".to_string(),
            DeploymentConfig {
                provider: "p".to_string(),
                deployment_revision_id: String::new(),
                model: "physical".to_string(),
                concurrency: 2,
                runtime_capacity: None,
                sidecar: None,
                pricing_basis: Default::default(),
                prices: BTreeMap::from([(
                    Operation::Generate,
                    OperationPrice::Generate(GenerationPrice {
                        version: 1,
                        input_micros_per_million: 1,
                        output_micros_per_million: 2,
                    }),
                )]),
                bedrock_policy: None,
                declared_capabilities: None,
                embedding_capabilities: None,
                conformance_digest: "a".repeat(64),
                conformance_result: None,
                text: true,
                images: false,
                tools: false,
                structured_json: false,
                streaming: true,
                pii_placeholder_preservation_percent: 0,
            },
        )]),
        aliases: BTreeMap::from([(
            "public-model".to_string(),
            AliasConfig {
                operations: BTreeSet::from([Operation::Generate]),
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
                require_expected_embedding_space: false,
                embedding_workload_lane: EmbeddingWorkloadLane::Standard,
            },
        )]),
        ..Default::default()
    }
}

fn private_plaintext_compiler_config() -> LlmRouterConfig {
    let mut config = compiler_config();
    config.development_fixtures = true;
    config.local_transport_enabled = false;
    config.network_zones.insert(
        "local-zone".to_string(),
        NetworkZone {
            id: "local-zone".to_string(),
            dns_names: BTreeSet::new(),
            cidrs: BTreeSet::from(["10.0.0.0/8".to_string()]),
            ports: BTreeSet::from([9]),
            allow_private_tls: false,
            allow_private_plaintext: true,
        },
    );
    let provider = config.providers.get_mut("p").unwrap();
    provider.base_url = "http://10.0.0.9:9/v1".to_string();
    provider.endpoint_auth = EndpointAuth::None;
    provider.network_profile.mode = NetworkProfileMode::PrivatePlaintext;
    provider.network_profile.termination = NetworkTermination::Native;
    provider.network_profile.network_zone_id = Some("local-zone".to_string());
    config.deployments.get_mut("d").unwrap().runtime_capacity = Some(RuntimeCapacity {
        physical_runtime_id: "runtime-local".to_string(),
        capacity_domain_id: "capacity-local".to_string(),
        max_parallel_requests: 2,
        max_queued_requests: 2,
        readiness_policy: ReadinessPolicy::Immediate,
        cold_start_timeout_ms: 30_000,
        stream_setup_timeout_ms: 10_000,
        request_timeout_ms: 30_000,
    });
    config
}

fn bedrock_compiler_config(auth: EndpointAuth) -> LlmRouterConfig {
    let mut config = compiler_config();
    let provider = config.providers.get_mut("p").unwrap();
    provider.provider_type = ProviderProfileType::AwsBedrock;
    provider.provider_protocol = ProviderProtocol::BedrockConverse;
    provider.aws_region = Some("us-east-1".to_string());
    provider.base_url = "https://bedrock-runtime.us-east-1.amazonaws.com".to_string();
    provider.endpoint_auth = auth;
    let deployment = config.deployments.get_mut("d").unwrap();
    deployment.model = "us.anthropic.claude-sonnet-4-6".to_string();
    deployment.bedrock_policy = Some(BedrockDeploymentPolicy {
        sampling: Default::default(),
        reasoning: Default::default(),
    });
    deployment.declared_capabilities = Some(ProviderCapabilities {
        operations: BTreeSet::from([Operation::Generate]),
        generation: Some(generation_capabilities(true, true, false)),
        embedding: None,
    });
    config
}

#[test]
fn compiler_builds_bedrock_api_key_endpoint_from_values_backed_shape() {
    let compiler = LlmCompiler::new(Arc::new(MapSecretResolver(BTreeMap::from([(
        "secret".to_string(),
        "development-token".to_string(),
    )]))));
    let snapshot = compiler
        .compile(
            &bedrock_compiler_config(EndpointAuth::BedrockApiKey {
                credential_ref: "secret".to_string(),
            }),
            1,
            None,
        )
        .unwrap();
    assert_eq!(
        snapshot.deployments["d"].provider.protocol(),
        ProviderProtocol::BedrockConverse
    );
    assert_eq!(snapshot.deployments["d"].provider_client_generation, 1);
}

#[test]
fn compiler_builds_bedrock_sigv4_endpoint_without_resolving_a_secret() {
    let probe = Arc::new(CompileProbe::default());
    let compiler = LlmCompiler::with_probe(
        Arc::new(MapSecretResolver(BTreeMap::new())),
        Arc::clone(&probe),
    );
    let snapshot = compiler
        .compile(
            &bedrock_compiler_config(EndpointAuth::AwsSigV4 {
                credential_source: AwsCredentialSource::DefaultChain,
            }),
            1,
            None,
        )
        .unwrap();
    assert_eq!(probe.secret_resolutions.load(Ordering::SeqCst), 0);
    assert_eq!(
        snapshot.deployments["d"].provider.protocol(),
        ProviderProtocol::BedrockConverse
    );
}

#[test]
fn local_profile_rejection_precedes_legacy_development_fixture_validation() {
    let probe = Arc::new(CompileProbe::default());
    let compiler = LlmCompiler::with_probe(
        Arc::new(MapSecretResolver(BTreeMap::new())),
        Arc::clone(&probe),
    );
    let mut config = private_plaintext_compiler_config();
    config.development_fixtures = false;
    let error = compiler
        .compile(&config, 1, None)
        .err()
        .expect("disabled local transport must fail");
    assert_eq!(
        error.to_string(),
        "configuration error: local transport not enabled"
    );
    assert_eq!(probe.secret_resolutions.load(Ordering::SeqCst), 0);
    assert_eq!(probe.client_builds.load(Ordering::SeqCst), 0);
}

#[test]
fn credential_free_plaintext_never_calls_the_secret_resolver() {
    let probe = Arc::new(CompileProbe::default());
    let compiler = LlmCompiler::with_probe(
        Arc::new(MapSecretResolver(BTreeMap::new())),
        Arc::clone(&probe),
    );
    let mut config = private_plaintext_compiler_config();
    config.local_transport_enabled = true;
    let snapshot = compiler.compile(&config, 1, None).unwrap();
    assert_eq!(probe.secret_resolutions.load(Ordering::SeqCst), 0);
    assert_eq!(probe.client_builds.load(Ordering::SeqCst), 1);
    assert_eq!(snapshot.deployments["d"].provider_endpoint_id, "p");
}

#[test]
fn embedding_memory_uses_admission_slots_not_deployment_concurrency() {
    assert_eq!(
        2048_usize * 3072 * std::mem::size_of::<f32>(),
        24 * 1024 * 1024
    );
    let mut config = compiler_config();
    config.global_concurrency = 512;
    config.providers.get_mut("p").unwrap().provider_protocol = ProviderProtocol::OpenAiEmbeddings;
    let alias = config.aliases.get_mut("public-model").unwrap();
    alias.operations = BTreeSet::from([Operation::Embed]);
    alias.required_capabilities.embedding_space = Some(embedding_space(3072));
    alias.concurrency = 512;
    alias.max_attempts = 1;
    let deployment = config.deployments.get_mut("d").unwrap();
    deployment.concurrency = 8;
    deployment.prices = BTreeMap::from([(
        Operation::Embed,
        OperationPrice::Embed(EmbeddingPrice {
            version: 1,
            input_micros_per_million: 1,
        }),
    )]);
    deployment.embedding_capabilities = Some(EmbeddingCapabilities {
        max_batch_items: 2048,
        max_input_tokens_per_item: 8192,
        max_aggregate_input_tokens: 16_777_216,
        supported_dimensions: BTreeSet::from([3072]),
        supported_encodings: BTreeSet::from([EmbeddingEncoding::Float]),
        max_response_bytes: 32 * 1024 * 1024,
        space: Some(embedding_space(3072)),
    });
    config.embedding_memory.items_per_permit = 256;
    config.embedding_memory.max_memory_bytes = 16 * 1024 * 1024 * 1024;
    let compiler = LlmCompiler::new(Arc::new(MapSecretResolver(BTreeMap::from([(
        "secret".to_string(),
        "value".to_string(),
    )]))));
    let error = compiler
        .compile(&config, 1, None)
        .err()
        .expect("512 admission slots must fail");
    assert!(
        error.to_string().contains("aggregate memory bound"),
        "{error}"
    );
}

#[test]
fn compiled_embedding_response_bound_covers_worst_case_f32_json_widening() {
    let dimension = 2048;
    let mut config = compiler_config();
    configure_compiler_embedding_alias(&mut config);
    let alias = config.aliases.get_mut("public-model").unwrap();
    alias.required_capabilities.embedding_space = Some(embedding_space(dimension));
    let embedding = config
        .deployments
        .get_mut("d")
        .unwrap()
        .embedding_capabilities
        .as_mut()
        .unwrap();
    embedding.max_batch_items = 1;
    embedding.max_aggregate_input_tokens = embedding.max_input_tokens_per_item;
    embedding.supported_dimensions = BTreeSet::from([dimension]);
    embedding.max_response_bytes = 16 * 1024 * 1024;
    embedding.space = Some(embedding_space(dimension));

    let compiler = LlmCompiler::new(Arc::new(MapSecretResolver(BTreeMap::from([(
        "secret".to_string(),
        "value".to_string(),
    )]))));
    let snapshot = compiler.compile(&config, 1, None).unwrap();

    // Match the HTTP response path: f32 values are first stored in Value and
    // then the complete response Value is serialized. This value widens to the
    // proven maximum 24 rendered bytes, plus a comma between array elements.
    let worst_case = -1.0000006e-5_f32;
    assert_eq!(
        serde_json::to_string(&serde_json::Value::from(worst_case as f64))
            .unwrap()
            .len(),
        24
    );
    let embedding = serde_json::json!(vec![worst_case; dimension as usize]);
    let data = vec![serde_json::json!({
        "object": "embedding",
        "embedding": embedding,
        "index": 0
    })];
    let body = serde_json::to_vec(&serde_json::json!({
        "object": "list",
        "data": data,
        "model": "public-model",
        "usage": {"prompt_tokens": 1, "total_tokens": 1}
    }))
    .unwrap();
    let former_bound = dimension as usize * 17 + 128 + 4096;

    assert!(
        body.len() > former_bound,
        "fixture must reproduce the former undercount: {} <= {former_bound}",
        body.len()
    );
    assert!(
        body.len() <= snapshot.embedding_memory.max_rendered_response_bytes,
        "rendered response {} exceeds compiled bound {}",
        body.len(),
        snapshot.embedding_memory.max_rendered_response_bytes
    );
}

#[test]
fn compiled_embedding_response_bound_covers_rendered_alias_name() {
    let alias_name = "alias".repeat(1024);
    let mut config = compiler_config();
    configure_compiler_embedding_alias(&mut config);
    let alias = config.aliases.remove("public-model").unwrap();
    config.aliases.insert(alias_name.clone(), alias);

    let compiler = LlmCompiler::new(Arc::new(MapSecretResolver(BTreeMap::from([(
        "secret".to_string(),
        "value".to_string(),
    )]))));
    let snapshot = compiler.compile(&config, 1, None).unwrap();

    let data = vec![serde_json::json!({
        "object": "embedding",
        "embedding": [0.1_f32, 0.2_f32],
        "index": 0
    })];
    let body = serde_json::to_vec(&serde_json::json!({
        "object": "list",
        "data": data,
        "model": alias_name,
        "usage": {"prompt_tokens": 1, "total_tokens": 1}
    }))
    .unwrap();

    let fixed_envelope_bound = 2 * 25 + 128 + 4096;
    assert!(
        body.len() > fixed_envelope_bound,
        "fixture must exceed the fixed envelope"
    );
    assert!(
        body.len() <= snapshot.embedding_memory.max_rendered_response_bytes,
        "rendered response {} exceeds compiled bound {}",
        body.len(),
        snapshot.embedding_memory.max_rendered_response_bytes
    );
}

#[test]
fn compiler_rejects_embedding_batch_weight_above_deployment_capacity() {
    let mut config = compiler_config();
    config.providers.get_mut("p").unwrap().provider_protocol = ProviderProtocol::OpenAiEmbeddings;
    config.aliases.get_mut("public-model").unwrap().operations = BTreeSet::from([Operation::Embed]);
    config
        .aliases
        .get_mut("public-model")
        .unwrap()
        .required_capabilities
        .embedding_space = Some(embedding_space(2));
    config.embedding_memory.items_per_permit = 2;
    let deployment = config.deployments.get_mut("d").unwrap();
    deployment.concurrency = 4;
    deployment.prices = BTreeMap::from([(
        Operation::Embed,
        OperationPrice::Embed(EmbeddingPrice {
            version: 1,
            input_micros_per_million: 1,
        }),
    )]);
    deployment.embedding_capabilities = Some(EmbeddingCapabilities {
        max_batch_items: 9,
        max_input_tokens_per_item: 128,
        max_aggregate_input_tokens: 1152,
        supported_dimensions: BTreeSet::from([2]),
        supported_encodings: BTreeSet::from([EmbeddingEncoding::Float]),
        max_response_bytes: 4096,
        space: Some(embedding_space(2)),
    });
    let compiler = LlmCompiler::new(Arc::new(MapSecretResolver(BTreeMap::from([(
        "secret".to_string(),
        "value".to_string(),
    )]))));
    let error = compiler
        .compile(&config, 1, None)
        .err()
        .expect("oversized weighted batch must fail");
    assert!(
        error.to_string().contains("maximum weighted batch"),
        "{error}"
    );
}

fn configure_compiler_embedding_alias(config: &mut LlmRouterConfig) {
    config.providers.get_mut("p").unwrap().provider_protocol = ProviderProtocol::OpenAiEmbeddings;
    let alias = config.aliases.get_mut("public-model").unwrap();
    alias.operations = BTreeSet::from([Operation::Embed]);
    alias.required_capabilities.embedding_space = Some(embedding_space(2));
    alias.require_expected_embedding_space = true;
    let deployment = config.deployments.get_mut("d").unwrap();
    deployment.prices = BTreeMap::from([(
        Operation::Embed,
        OperationPrice::Embed(EmbeddingPrice {
            version: 1,
            input_micros_per_million: 1,
        }),
    )]);
    deployment.embedding_capabilities = Some(EmbeddingCapabilities {
        max_batch_items: 2,
        max_input_tokens_per_item: 128,
        max_aggregate_input_tokens: 256,
        supported_dimensions: BTreeSet::from([2]),
        supported_encodings: BTreeSet::from([EmbeddingEncoding::Float]),
        max_response_bytes: 4096,
        space: Some(embedding_space(2)),
    });
}

#[test]
fn compiler_rejects_mixed_or_repointed_embedding_spaces() {
    let compiler = LlmCompiler::new(Arc::new(MapSecretResolver(BTreeMap::from([(
        "secret".to_string(),
        "value".to_string(),
    )]))));
    let mut mixed = compiler_config();
    configure_compiler_embedding_alias(&mut mixed);
    mixed
        .deployments
        .get_mut("d")
        .unwrap()
        .embedding_capabilities
        .as_mut()
        .unwrap()
        .space
        .as_mut()
        .unwrap()
        .space_id = "another-space".to_string();
    let error = compiler
        .compile(&mixed, 1, None)
        .err()
        .expect("mixed space");
    assert!(error.to_string().contains("embedding space"), "{error}");

    let mut original = compiler_config();
    configure_compiler_embedding_alias(&mut original);
    let first = compiler.compile(&original, 1, None).unwrap();
    let changed = original
        .aliases
        .get_mut("public-model")
        .unwrap()
        .required_capabilities
        .embedding_space
        .as_mut()
        .unwrap();
    changed.revision = 2;
    original
        .deployments
        .get_mut("d")
        .unwrap()
        .embedding_capabilities
        .as_mut()
        .unwrap()
        .space
        .as_mut()
        .unwrap()
        .revision = 2;
    let error = compiler
        .compile(&original, 2, Some(&first))
        .err()
        .expect("repointed space");
    assert!(error.to_string().contains("immutable"), "{error}");
}

#[test]
fn compiler_rejects_shared_capacity_between_query_and_index_lanes() {
    let mut config = compiler_config();
    configure_compiler_embedding_alias(&mut config);
    let query = config.aliases.get_mut("public-model").unwrap();
    query.internal = true;
    query.bound_principal = Some("knowledge-service".to_string());
    query.embedding_workload_lane = EmbeddingWorkloadLane::KbQuery;
    let mut index = query.clone();
    index.embedding_workload_lane = EmbeddingWorkloadLane::KbIndex;
    config.aliases.insert("kb-index".to_string(), index);

    let compiler = LlmCompiler::new(Arc::new(MapSecretResolver(BTreeMap::from([(
        "secret".to_string(),
        "value".to_string(),
    )]))));
    let error = compiler
        .compile(&config, 1, None)
        .err()
        .expect("shared lane capacity");
    assert!(error.to_string().contains("must not share"), "{error}");

    let mut config = compiler_config();
    configure_compiler_embedding_alias(&mut config);
    let query = config.aliases.get_mut("public-model").unwrap();
    query.internal = true;
    query.bound_principal = Some("knowledge-service".to_string());
    query.embedding_workload_lane = EmbeddingWorkloadLane::KbQuery;
    let mut standard = query.clone();
    standard.internal = false;
    standard.bound_principal = None;
    standard.embedding_workload_lane = EmbeddingWorkloadLane::Standard;
    config
        .aliases
        .insert("tenant-embedding".to_string(), standard);
    let error = compiler
        .compile(&config, 1, None)
        .err()
        .expect("standard traffic must not share KB query capacity");
    assert!(error.to_string().contains("must not share"), "{error}");
}

#[test]
fn compiler_rejects_price_key_and_variant_mismatch() {
    let mut config = compiler_config();
    config.deployments.get_mut("d").unwrap().prices = BTreeMap::from([(
        Operation::Generate,
        OperationPrice::Embed(EmbeddingPrice {
            version: 1,
            input_micros_per_million: 1,
        }),
    )]);
    let compiler = LlmCompiler::new(Arc::new(MapSecretResolver(BTreeMap::from([(
        "secret".to_string(),
        "value".to_string(),
    )]))));
    let error = compiler
        .compile(&config, 1, None)
        .err()
        .expect("mismatched price variant must fail");
    assert!(error.to_string().contains("price key"), "{error}");
}

#[test]
fn compiler_accepts_alias_with_separate_generate_and_embed_deployments() {
    let mut config = compiler_config();
    config.providers.insert(
        "embedding-provider".to_string(),
        ProviderConfig {
            provider_account_id: "embedding-provider".to_string(),
            provider_type: Default::default(),
            provider_protocol: ProviderProtocol::OpenAiEmbeddings,
            aws_region: None,
            material_generation: 1,
            base_url: "http://127.0.0.1:9/v1".to_string(),
            endpoint_auth: EndpointAuth::Bearer {
                credential_ref: "secret".to_string(),
            },
            network_profile: Default::default(),
            headers: BTreeMap::new(),
            quota_group_id: Some("quota".to_string()),
        },
    );
    config.deployments.insert(
        "embedding-deployment".to_string(),
        DeploymentConfig {
            provider: "embedding-provider".to_string(),
            deployment_revision_id: String::new(),
            model: "physical-embedding".to_string(),
            concurrency: 2,
            runtime_capacity: None,
            sidecar: None,
            pricing_basis: Default::default(),
            prices: BTreeMap::from([(
                Operation::Embed,
                OperationPrice::Embed(EmbeddingPrice {
                    version: 1,
                    input_micros_per_million: 1,
                }),
            )]),
            bedrock_policy: None,
            declared_capabilities: None,
            embedding_capabilities: Some(EmbeddingCapabilities {
                max_batch_items: 2,
                max_input_tokens_per_item: 128,
                max_aggregate_input_tokens: 256,
                supported_dimensions: BTreeSet::from([3]),
                supported_encodings: BTreeSet::from([EmbeddingEncoding::Float]),
                max_response_bytes: 1024,
                space: Some(embedding_space(3)),
            }),
            conformance_digest: "b".repeat(64),
            conformance_result: None,
            text: false,
            images: false,
            tools: false,
            structured_json: false,
            streaming: false,
            pii_placeholder_preservation_percent: 0,
        },
    );
    let alias = config.aliases.get_mut("public-model").unwrap();
    alias.operations = BTreeSet::from([Operation::Generate, Operation::Embed]);
    alias.required_capabilities.embedding_space = Some(embedding_space(3));
    alias.deployments.push("embedding-deployment".to_string());

    let compiler = LlmCompiler::new(Arc::new(MapSecretResolver(BTreeMap::from([(
        "secret".to_string(),
        "value".to_string(),
    )]))));
    let snapshot = compiler.compile(&config, 1, None).unwrap();
    assert_eq!(
        snapshot.aliases["public-model"].operations,
        BTreeSet::from([Operation::Generate, Operation::Embed])
    );
}

#[tokio::test]
async fn unconfigured_embeddings_route_is_not_reported_as_capacity() {
    let config = compiler_config();
    let compiler = LlmCompiler::new(Arc::new(MapSecretResolver(BTreeMap::from([(
        "secret".to_string(),
        "value".to_string(),
    )]))));
    let snapshot = compiler.compile(&config, 1, None).unwrap();
    assert_eq!(snapshot.embedding_memory.admission_slots, 0);
    let runtime = Arc::new(LlmRuntime::new(
        Arc::new(LlmSnapshotStore::new(snapshot, 2)),
        Arc::new(RecordingAudit::default()),
    ));
    let response = LlmBufferedHttp::new(runtime, Arc::new(Allow), 4096, 16, Duration::from_secs(1))
        .handle(embedding_http_request(
            br#"{"model":"missing-embedding","input":"hello"}"#,
        ))
        .await;
    assert_eq!(response.status, 404);
    assert!(String::from_utf8_lossy(&response.body).contains("model_not_found"));
}

#[tokio::test]
async fn embedding_alias_is_visible_while_chat_operation_mismatch_fails_before_dispatch() {
    let mut config = compiler_config();
    config.providers.get_mut("p").unwrap().provider_protocol = ProviderProtocol::OpenAiEmbeddings;
    let alias = config.aliases.get_mut("public-model").unwrap();
    alias.operations = BTreeSet::from([Operation::Embed]);
    alias.required_capabilities.embedding_space = Some(embedding_space(3));
    let deployment = config.deployments.get_mut("d").unwrap();
    deployment.prices = BTreeMap::from([(
        Operation::Embed,
        OperationPrice::Embed(EmbeddingPrice {
            version: 1,
            input_micros_per_million: 1,
        }),
    )]);
    deployment.embedding_capabilities = Some(EmbeddingCapabilities {
        max_batch_items: 2,
        max_input_tokens_per_item: 128,
        max_aggregate_input_tokens: 256,
        supported_dimensions: BTreeSet::from([3]),
        supported_encodings: BTreeSet::from([EmbeddingEncoding::Float]),
        max_response_bytes: 1024,
        space: Some(embedding_space(3)),
    });
    let compiler = LlmCompiler::new(Arc::new(MapSecretResolver(BTreeMap::from([(
        "secret".to_string(),
        "value".to_string(),
    )]))));
    let snapshot = compiler.compile(&config, 1, None).unwrap();
    let runtime = LlmRuntime::new(
        Arc::new(LlmSnapshotStore::new(snapshot, 2)),
        Arc::new(RecordingAudit::default()),
    );

    assert_eq!(runtime.visible_models(), vec!["public-model".to_string()]);
    let error = runtime
        .execute(
            LlmRequestContext::with_timeout("user", Duration::from_secs(1)),
            InferenceRequest::text("public-model", "hello"),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, LlmGatewayError::UnsupportedCapability(_)));
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
    assert!(Arc::ptr_eq(
        &first.deployments["d"].permits,
        &second.deployments["d"].permits
    ));
    assert!(Arc::ptr_eq(
        &first.deployments["d"].circuit,
        &second.deployments["d"].circuit
    ));
    assert!(Arc::ptr_eq(
        &first.deployments["d"].readiness,
        &second.deployments["d"].readiness
    ));
}

#[test]
fn same_material_client_refresh_replaces_pool_without_advancing_material_generation() {
    let mut config = compiler_config();
    let connection = &mut config
        .providers
        .get_mut("p")
        .unwrap()
        .network_profile
        .connection;
    connection.pool_idle_timeout_ms = 1;
    connection.client_refresh_interval_ms = 1;
    let compiler = LlmCompiler::new(Arc::new(MapSecretResolver(BTreeMap::from([(
        "secret".to_string(),
        "value".to_string(),
    )]))));
    let first = compiler.compile(&config, 1, None).unwrap();
    std::thread::sleep(Duration::from_millis(2));
    let second = compiler.compile(&config, 2, Some(&first)).unwrap();
    assert_eq!(first.deployments["d"].provider_client_generation, 1);
    assert_eq!(second.deployments["d"].provider_client_generation, 1);
    assert!(!Arc::ptr_eq(
        &first.deployments["d"],
        &second.deployments["d"]
    ));
    let store = LlmSnapshotStore::new(first, 1);
    assert!(matches!(store.publish(second), PublishOutcome::Published));
}

#[test]
fn warm_before_eligible_receives_no_user_traffic_until_atomic_warmup_success() {
    let mut config = compiler_config();
    config.deployments.get_mut("d").unwrap().runtime_capacity = Some(RuntimeCapacity {
        physical_runtime_id: "runtime-a".to_string(),
        capacity_domain_id: "capacity-a".to_string(),
        max_parallel_requests: 2,
        max_queued_requests: 1,
        readiness_policy: ReadinessPolicy::WarmBeforeEligible,
        cold_start_timeout_ms: 1_000,
        stream_setup_timeout_ms: 1_000,
        request_timeout_ms: 1_000,
    });
    let compiler = LlmCompiler::new(Arc::new(MapSecretResolver(BTreeMap::from([(
        "secret".to_string(),
        "value".to_string(),
    )]))));
    let snapshot = compiler.compile(&config, 1, None).unwrap();
    let readiness = Arc::clone(&snapshot.deployments["d"].readiness);
    let runtime = LlmRuntime::new(
        Arc::new(LlmSnapshotStore::new(snapshot, 1)),
        Arc::new(RecordingAudit::default()),
    );
    assert!(runtime.visible_models().is_empty());
    assert!(readiness.begin_warmup());
    assert!(runtime.visible_models().is_empty());
    assert!(readiness.warmup_succeeded());
    assert_eq!(runtime.visible_models(), ["public-model"]);
}

#[test]
fn multi_attempt_alias_rejects_raw_body_bound_above_replay_policy_bound() {
    let mut config = compiler_config();
    let second = config.deployments["d"].clone();
    config.deployments.insert("d2".to_string(), second);
    let alias = config.aliases.get_mut("public-model").unwrap();
    alias.deployments = vec!["d".to_string(), "d2".to_string()];
    alias.max_attempts = 2;
    config.max_replay_bytes = 1024;
    config.max_request_body_bytes = 2048;
    let compiler = LlmCompiler::new(Arc::new(MapSecretResolver(BTreeMap::from([(
        "secret".to_string(),
        "value".to_string(),
    )]))));
    let error = compiler
        .compile(&config, 1, None)
        .err()
        .expect("body bound above replay bound must fail compile");
    let message = error.to_string();
    assert!(message.contains("raw-body maxRequestBodyBytes"), "{error}");
    assert!(message.contains("canonical maxReplayBytes"), "{error}");

    config.max_request_body_bytes = 1024;
    assert!(
        compiler.compile(&config, 1, None).is_ok(),
        "equal configured bounds satisfy the conservative reload policy"
    );
}

#[test]
fn development_fixture_deployment_supports_streaming_by_default() {
    // Regression: the dev-fixture capability fallback hard-coded
    // `streaming: false`, so every SSE request against a fixture deployment
    // returned `model_not_found`. A YAML deployment without a `streaming` key
    // must keep SSE parity with production conformance-backed deployments.
    let deserialized: llm_gateway::config::DeploymentConfig =
        serde_json::from_value(serde_json::json!({
            "provider": "p", "model": "physical",
            "prices": {"generate": {"operation": "generate", "version": 1,
                "inputMicrosPerMillion": 1, "outputMicrosPerMillion": 2}}
        }))
        .unwrap();
    assert!(deserialized.streaming);

    let compiler = LlmCompiler::new(Arc::new(MapSecretResolver(BTreeMap::from([(
        "secret".to_string(),
        "value".to_string(),
    )]))));
    let snapshot = compiler.compile(&compiler_config(), 1, None).unwrap();
    let streaming = CapabilityRequirements {
        streaming: true,
        ..CapabilityRequirements::generation()
    };
    assert!(snapshot.deployments["d"].supports(&streaming));

    let mut opted_out = compiler_config();
    opted_out.deployments.get_mut("d").unwrap().streaming = false;
    let snapshot = compiler.compile(&opted_out, 2, None).unwrap();
    assert!(!snapshot.deployments["d"].supports(&streaming));
    assert!(snapshot.deployments["d"].supports(&CapabilityRequirements::generation()));
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
fn compiler_rejects_zero_stream_response_memory_bound() {
    let compiler = LlmCompiler::new(Arc::new(MapSecretResolver(BTreeMap::from([(
        "secret".to_string(),
        "value".to_string(),
    )]))));
    let mut config = compiler_config();
    config.max_stream_response_bytes = 0;
    assert!(compiler.compile(&config, 1, None).is_err());
}

#[test]
fn compiler_requires_ingress_to_cover_the_single_parsed_embedding_request() {
    let mut config = compiler_config();
    config.embedding_memory.ingress_overhead_bytes = config
        .embedding_memory
        .max_request_body_bytes
        .checked_mul(20)
        .unwrap()
        + (64 * 1024)
        - 1;
    let compiler = LlmCompiler::new(Arc::new(MapSecretResolver(BTreeMap::from([(
        "secret".to_string(),
        "value".to_string(),
    )]))));
    let error = compiler
        .compile(&config, 1, None)
        .err()
        .expect("undersized parsed admission bound");
    assert!(
        error.to_string().contains("parsed admission amplification"),
        "{error}"
    );
}

#[test]
fn production_instance_properties_allow_declared_capabilities_without_conformance() {
    let mut config = compiler_config();
    config.development_fixtures = false;
    let provider = config.providers.get_mut("p").unwrap();
    provider.base_url = "https://api.example.com/v1".to_string();
    provider.endpoint_auth = EndpointAuth::Bearer {
        credential_ref: "env:PROVIDER_API_KEY".to_string(),
    };
    let deployment = config.deployments.get_mut("d").unwrap();
    deployment.conformance_digest.clear();
    deployment.conformance_result = None;
    deployment.tools = true;
    config
        .aliases
        .get_mut("public-model")
        .unwrap()
        .required_capabilities
        .tools = true;

    let compiler = LlmCompiler::new(Arc::new(MapSecretResolver(BTreeMap::from([(
        "env:PROVIDER_API_KEY".to_string(),
        "locally-injected-value".to_string(),
    )]))));
    let snapshot = compiler.compile(&config, 1, None).expect(
        "Portal-published declared capabilities must not require Portal-managed provider conformance",
    );
    assert!(snapshot.deployments["d"].supports(&CapabilityRequirements {
        tools: true,
        ..CapabilityRequirements::generation()
    }));
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
        tenant_id: Some("tenant-test".to_string()),
        trusted_request_id: "trusted".to_string(),
    }
}

fn embedding_http_request(body: &[u8]) -> BufferedHttpRequest {
    let mut request = http_request(body);
    request.path = "/v1/embeddings".to_string();
    request
}

fn responses_http_request(body: &[u8]) -> BufferedHttpRequest {
    let mut request = http_request(body);
    request.path = "/v1/responses".to_string();
    request
}

#[tokio::test]
async fn buffered_responses_reuses_generation_and_hides_provider_identity() {
    let mut provider_response = success_response();
    provider_response
        .output
        .push(GenerateOutputItem::FunctionCall {
            id: "provider-private-item".to_string(),
            call_id: "call_1".to_string(),
            name: "weather".to_string(),
            arguments: serde_json::json!({"city":"Toronto"}),
            status: ItemStatus::Completed,
        });
    let provider = Arc::new(ScriptedProvider::new(
        ProviderProtocol::OpenAiChat,
        vec![Ok(provider_response)],
    ));
    let runtime = runtime_with(
        vec![provider.clone()],
        1,
        16 * 1024,
        Arc::new(RecordingAudit::default()),
    );
    let http = LlmBufferedHttp::new(
        runtime,
        Arc::new(Allow),
        16 * 1024,
        32,
        Duration::from_secs(1),
    );
    let response = http
        .handle(responses_http_request(
            br#"{
        "model":"public-model","instructions":"be concise","input":[
          {"role":"user","content":[{"type":"input_text","text":"weather?"}]}
        ],"tools":[{"type":"function","name":"weather","parameters":{"type":"object"},"strict":false}],
        "tool_choice":"required","temperature":0.2,"top_p":0.8,
        "store":null
    }"#,
        ))
        .await;
    assert_eq!(
        response.status,
        200,
        "{}",
        String::from_utf8_lossy(&response.body)
    );
    let value: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(value["object"], "response");
    assert_eq!(value["model"], "public-model");
    assert_eq!(value["store"], false);
    assert_eq!(value["instructions"], "be concise");
    assert_eq!(value["tool_choice"], "required");
    assert_eq!(value["temperature"], 0.2);
    assert_eq!(value["top_p"], 0.8);
    assert_eq!(value["tools"][0]["strict"], false);
    assert_eq!(value["output"][1]["call_id"], "call_1");
    let rendered = String::from_utf8(response.body).unwrap();
    assert!(!rendered.contains("physical-secret"));
    assert!(!rendered.contains("provider-private-item"));
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn responses_deferred_features_and_reasoning_mismatch_fail_before_dispatch() {
    let provider = Arc::new(ScriptedProvider::new(
        ProviderProtocol::OpenAiChat,
        vec![Ok(success_response())],
    ));
    let runtime = runtime_with(
        vec![provider.clone()],
        1,
        4096,
        Arc::new(RecordingAudit::default()),
    );
    let http = LlmBufferedHttp::new(runtime, Arc::new(Allow), 4096, 32, Duration::from_secs(1));
    for body in [
        br#"{"model":"public-model","input":"x","store":true}"#.as_slice(),
        br#"{"model":"public-model","input":"x","previous_response_id":"resp_1"}"#.as_slice(),
        br#"{"model":"public-model","input":"x","tools":[{"type":"web_search_preview"}]}"#
            .as_slice(),
        br#"{"model":"public-model","input":"x","reasoning":{"effort":"high"}}"#.as_slice(),
    ] {
        let response = http.handle(responses_http_request(body)).await;
        assert_eq!(
            response.status,
            400,
            "{}",
            String::from_utf8_lossy(&response.body)
        );
    }
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn responses_stream_uses_named_events_without_chat_done_marker() {
    let mut provider = SseProvider::success();
    provider.protocol = ProviderProtocol::OpenAiResponses;
    let provider = Arc::new(provider);
    let runtime = runtime_with(vec![provider], 1, 4096, Arc::new(RecordingAudit::default()));
    let http = LlmBufferedHttp::new(runtime, Arc::new(Allow), 4096, 32, Duration::from_secs(1));
    let LlmHttpResponse::Streaming(mut response) = http
        .handle_route(responses_http_request(
            br#"{"model":"public-model","input":"hello","stream":true}"#,
        ))
        .await
    else {
        panic!("expected Responses SSE")
    };
    let mut body = Vec::new();
    while let Some(frame) = response.stream.next_frame().await {
        body.extend_from_slice(&frame);
    }
    let body = String::from_utf8(body).unwrap();
    assert!(body.contains("event: response.created"));
    assert!(body.contains("event: response.output_text.delta"));
    assert!(body.contains("event: response.completed"));
    assert!(!body.contains("[DONE]"));
    assert!(
        body.find("response.created").unwrap() < body.find("response.output_text.delta").unwrap()
    );
}

#[tokio::test]
async fn responses_stream_preserves_function_argument_event_order() {
    let provider = Arc::new(SseProvider {
        protocol: ProviderProtocol::OpenAiResponses,
        events: vec![
            Ok(InferenceEvent::ToolCallDelta {
                delta: model_provider::inference::ToolCallDelta {
                    index: 0,
                    id: Some("call_1".to_string()),
                    name: Some("weather".to_string()),
                    arguments_fragment: "{\"city\":".to_string(),
                },
            }),
            Ok(InferenceEvent::ToolCallDelta {
                delta: model_provider::inference::ToolCallDelta {
                    index: 0,
                    id: None,
                    name: None,
                    arguments_fragment: "\"Toronto\"}".to_string(),
                },
            }),
            Ok(InferenceEvent::Usage {
                usage: NormalizedUsage {
                    input_tokens: Some(3),
                    output_tokens: Some(2),
                    ..Default::default()
                },
            }),
            Ok(InferenceEvent::MessageEnd {
                finish_reason: FinishReason::ToolCalls,
                terminal_state: TerminalState::Complete,
            }),
        ],
        calls: AtomicUsize::new(0),
        wait_for_cancellation: false,
        cancellation_observed: Arc::new(AtomicBool::new(false)),
    });
    let runtime = runtime_with(vec![provider], 1, 4096, Arc::new(RecordingAudit::default()));
    let http = LlmBufferedHttp::new(runtime, Arc::new(Allow), 4096, 32, Duration::from_secs(1));
    let LlmHttpResponse::Streaming(mut response) = http
        .handle_route(responses_http_request(
            br#"{"model":"public-model","input":"weather?","tools":[{"type":"function","name":"weather","parameters":{"type":"object"},"strict":false}],"tool_choice":"required","temperature":0.2,"stream":true}"#,
        ))
        .await
    else {
        panic!("expected Responses SSE")
    };
    let mut body = Vec::new();
    while let Some(frame) = response.stream.next_frame().await {
        body.extend_from_slice(&frame);
    }
    let body = String::from_utf8(body).unwrap();
    let added = body.find("event: response.output_item.added").unwrap();
    let delta = body
        .find("event: response.function_call_arguments.delta")
        .unwrap();
    let done = body
        .find("event: response.function_call_arguments.done")
        .unwrap();
    let completed = body.find("event: response.completed").unwrap();
    assert!(added < delta && delta < done && done < completed);
    assert!(body.contains(r#"\"city\":\"Toronto\""#));
    assert!(!body.contains("[DONE]"));
}

#[tokio::test]
async fn responses_stream_assigns_unique_output_indices_for_mixed_text_and_tools() {
    let provider = Arc::new(SseProvider {
        protocol: ProviderProtocol::OpenAiChat,
        events: vec![
            Ok(InferenceEvent::TextDelta {
                text: "checking".to_string(),
            }),
            Ok(InferenceEvent::ToolCallDelta {
                delta: model_provider::inference::ToolCallDelta {
                    index: 0,
                    id: Some("call_1".to_string()),
                    name: Some("weather".to_string()),
                    arguments_fragment: "{}".to_string(),
                },
            }),
            Ok(InferenceEvent::MessageEnd {
                finish_reason: FinishReason::ToolCalls,
                terminal_state: TerminalState::Complete,
            }),
        ],
        calls: AtomicUsize::new(0),
        wait_for_cancellation: false,
        cancellation_observed: Arc::new(AtomicBool::new(false)),
    });
    let runtime = runtime_with(vec![provider], 1, 4096, Arc::new(RecordingAudit::default()));
    let http = LlmBufferedHttp::new(runtime, Arc::new(Allow), 4096, 32, Duration::from_secs(1));
    let LlmHttpResponse::Streaming(mut response) = http
        .handle_route(responses_http_request(
            br#"{"model":"public-model","input":"weather?","tools":[{"type":"function","name":"weather","parameters":{"type":"object"},"strict":false}],"tool_choice":"required","temperature":0.2,"stream":true}"#,
        ))
        .await
    else { panic!("expected Responses SSE") };
    let mut body = Vec::new();
    while let Some(frame) = response.stream.next_frame().await {
        body.extend_from_slice(&frame);
    }
    let body = String::from_utf8(body).unwrap();
    let added_indices = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|event| event["type"] == "response.output_item.added")
        .filter_map(|event| event["output_index"].as_u64())
        .collect::<Vec<_>>();
    assert_eq!(added_indices, vec![0, 1]);
    assert!(body.contains(r#""tool_choice":"required""#), "{body}");
    assert!(body.contains(r#""temperature":0.2"#), "{body}");
}

#[tokio::test]
async fn responses_stream_cumulative_output_limit_cancels_and_fails_safely() {
    let provider = Arc::new(SseProvider {
        protocol: ProviderProtocol::OpenAiResponses,
        events: vec![
            Ok(InferenceEvent::TextDelta {
                text: "x".repeat(20 * 1024),
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
    let http = LlmBufferedHttp::new(runtime, Arc::new(Allow), 4096, 32, Duration::from_secs(1));
    let LlmHttpResponse::Streaming(mut response) = http
        .handle_route(responses_http_request(
            br#"{"model":"public-model","input":"large","stream":true}"#,
        ))
        .await
    else {
        panic!("expected Responses SSE")
    };
    let mut body = Vec::new();
    while let Some(frame) = response.stream.next_frame().await {
        body.extend_from_slice(&frame);
    }
    let body = String::from_utf8(body).unwrap();
    assert!(body.contains("event: response.failed"));
    assert!(body.contains("response_too_large"));
    assert!(!body.contains("event: response.completed"));
}

#[tokio::test]
async fn multi_attempt_http_rechecks_canonical_replay_size_before_dispatch() {
    let provider = Arc::new(ScriptedProvider::new(
        ProviderProtocol::OpenAiChat,
        vec![Ok(success_response())],
    ));
    let runtime = runtime_with(
        vec![provider.clone(), provider.clone()],
        2,
        100,
        Arc::new(RecordingAudit::default()),
    );
    let http = LlmBufferedHttp::new(runtime, Arc::new(Allow), 100, 32, Duration::from_secs(1));
    let body = br#"{"model":"public-model","messages":[{"role":"user","content":"x"}]}"#;

    assert!(body.len() <= 100, "raw HTTP body must pass admission");
    assert!(
        serde_json::to_vec(&InferenceRequest::text("public-model", "x"))
            .expect("serialize canonical request")
            .len()
            > 100,
        "canonical replay representation must exceed the configured bound"
    );

    let response = http.handle(http_request(body)).await;
    assert_eq!(response.status, 400);
    assert!(
        String::from_utf8(response.body)
            .expect("UTF-8 error response")
            .contains("request exceeds replay bound required by retry policy")
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn buffered_security_denies_before_json_and_alias_parse() {
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = Arc::new(ScriptedProvider::new(
        ProviderProtocol::OpenAiChat,
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
async fn embeddings_http_supports_string_batch_float_and_base64() {
    let provider = scripted_embedding_provider(vec![
        Ok(embedding_response(1, 2, 3, 0)),
        Ok(embedding_response(2, 2, 5, 0)),
    ]);
    let (runtime, ledger) = embedding_runtime(
        vec![(
            Arc::clone(&provider),
            EmbeddingPrice {
                version: 1,
                input_micros_per_million: 1_000_000,
            },
        )],
        1,
        Some(100),
    );
    let http = LlmBufferedHttp::new(runtime, Arc::new(Allow), 1024, 16, Duration::from_secs(1));

    let first = http
        .handle(embedding_http_request(
            br#"{"model":"embedding-default","input":"hello","dimensions":2}"#,
        ))
        .await;
    assert_eq!(first.status, 200);
    assert!(first.lifecycle.is_some());
    assert!(!first.headers.contains_key("x-light-config-generation"));
    let first_json: serde_json::Value = serde_json::from_slice(&first.body).unwrap();
    assert_eq!(first_json["model"], "embedding-default");
    assert_eq!(first_json["data"][0]["index"], 0);
    let rendered = first_json["data"][0]["embedding"].as_array().unwrap();
    assert!((rendered[0].as_f64().unwrap() - 0.1).abs() < 1e-6);
    assert!((rendered[1].as_f64().unwrap() - 0.2).abs() < 1e-6);
    drop(first);

    let second = http
        .handle(embedding_http_request(
            br#"{"model":"embedding-default","input":["a","b"],"encoding_format":"base64","dimensions":2}"#,
        ))
        .await;
    assert_eq!(second.status, 200);
    let second_json: serde_json::Value = serde_json::from_slice(&second.body).unwrap();
    let encoded = second_json["data"][0]["embedding"].as_str().unwrap();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .unwrap();
    assert_eq!(f32::from_le_bytes(bytes[..4].try_into().unwrap()), 0.1);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert_eq!(ledger.charged(), 8);
}

#[tokio::test]
async fn embeddings_expected_space_is_checked_before_admission_and_echoed_on_success() {
    let provider = scripted_embedding_provider(vec![Ok(embedding_response(1, 2, 1, 0))]);
    let (runtime, _) = embedding_runtime_with_policy(
        vec![(
            Arc::clone(&provider),
            EmbeddingPrice {
                version: 1,
                input_micros_per_million: 1,
            },
        )],
        1,
        None,
        true,
        EmbeddingWorkloadLane::KbQuery,
        EmbeddingWorkloadLane::KbQuery,
    );
    let http = LlmBufferedHttp::new(
        Arc::clone(&runtime),
        Arc::new(Allow),
        1024,
        16,
        Duration::from_secs(1),
    );

    let missing = http
        .handle(embedding_http_request(
            br#"{"model":"embedding-default","input":"x"}"#,
        ))
        .await;
    assert_eq!(missing.status, 400);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.embedding_memory_metrics().current_slots, 0);

    let mut over_budget = embedding_http_request(br#"{"model":"embedding-default","input":"x"}"#);
    over_budget.headers.insert(
        "x-light-expected-embedding-space-id".to_string(),
        "test-space-2".to_string(),
    );
    over_budget.headers.insert(
        "x-light-expected-embedding-space-revision".to_string(),
        "1".to_string(),
    );
    over_budget.headers.insert(
        "x-light-maximum-billed-cost-micros".to_string(),
        "0".to_string(),
    );
    let over_budget = http.handle(over_budget).await;
    assert_ne!(over_budget.status, 200);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);

    let mut partial = embedding_http_request(br#"{"model":"embedding-default","input":"x"}"#);
    partial.headers.insert(
        "x-light-expected-embedding-space-id".to_string(),
        "test-space-2".to_string(),
    );
    let partial = http.handle(partial).await;
    assert_eq!(partial.status, 400);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);

    let mut mismatch = embedding_http_request(br#"{"model":"embedding-default","input":"x"}"#);
    mismatch.headers.insert(
        "x-light-expected-embedding-space-id".to_string(),
        "another-space".to_string(),
    );
    mismatch.headers.insert(
        "x-light-expected-embedding-space-revision".to_string(),
        "1".to_string(),
    );
    let mismatch = http.handle(mismatch).await;
    assert_eq!(mismatch.status, 400);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.embedding_memory_metrics().current_slots, 0);

    let mut matching = embedding_http_request(br#"{"model":"embedding-default","input":"x"}"#);
    matching.headers.insert(
        "x-light-expected-embedding-space-id".to_string(),
        "test-space-2".to_string(),
    );
    matching.headers.insert(
        "x-light-expected-embedding-space-revision".to_string(),
        "1".to_string(),
    );
    let matching = http.handle(matching).await;
    assert_eq!(matching.status, 200);
    assert_eq!(
        matching.headers["x-light-embedding-space-id"],
        "test-space-2"
    );
    assert_eq!(matching.headers["x-light-embedding-space-revision"], "1");
    assert_eq!(matching.headers["x-light-config-generation"], "7");
    assert_eq!(matching.headers["x-light-billed-cost-micros"], "1");
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        provider.received_dimensions.lock().unwrap().as_slice(),
        &[Some(2)]
    );
}

#[tokio::test]
async fn embeddings_hide_aliases_assigned_to_another_workload_lane() {
    let provider = scripted_embedding_provider(vec![]);
    let (runtime, _) = embedding_runtime_with_policy(
        vec![(
            Arc::clone(&provider),
            EmbeddingPrice {
                version: 1,
                input_micros_per_million: 1,
            },
        )],
        1,
        None,
        true,
        EmbeddingWorkloadLane::KbIndex,
        EmbeddingWorkloadLane::KbQuery,
    );
    let http = LlmBufferedHttp::new(runtime, Arc::new(Allow), 1024, 16, Duration::from_secs(1));
    let mut request = embedding_http_request(br#"{"model":"embedding-default","input":"x"}"#);
    request.headers.insert(
        "x-light-expected-embedding-space-id".to_string(),
        "test-space-2".to_string(),
    );
    request.headers.insert(
        "x-light-expected-embedding-space-revision".to_string(),
        "1".to_string(),
    );
    let response = http.handle(request).await;
    assert_eq!(response.status, 404);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn embeddings_validation_and_operation_mismatch_fail_before_dispatch() {
    let provider = scripted_embedding_provider(vec![]);
    let (runtime, _) = embedding_runtime(
        vec![(
            Arc::clone(&provider),
            EmbeddingPrice {
                version: 1,
                input_micros_per_million: 1,
            },
        )],
        1,
        None,
    );
    let http = LlmBufferedHttp::new(runtime, Arc::new(Allow), 1024, 16, Duration::from_secs(1));
    for (body, status, code) in [
        (
            br#"{"model":"embedding-default","input":[]}"#.as_slice(),
            400,
            "invalid_request",
        ),
        (
            br#"{"model":"embedding-default","input":[""]}"#.as_slice(),
            400,
            "invalid_request",
        ),
        (
            br#"{"model":"embedding-default","input":[[1,2]]}"#.as_slice(),
            400,
            "unsupported_feature",
        ),
        (
            br#"{"model":"embedding-default","input":"x","dimensions":3}"#.as_slice(),
            400,
            "unsupported_feature",
        ),
        (
            br#"{"model":"embedding-default","input":"x","unknown":true}"#.as_slice(),
            400,
            "invalid_request",
        ),
    ] {
        let response = http.handle(embedding_http_request(body)).await;
        assert_eq!(response.status, status);
        assert!(String::from_utf8_lossy(&response.body).contains(code));
    }
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);

    let chat_provider = Arc::new(ScriptedProvider::new(
        ProviderProtocol::OpenAiChat,
        vec![Ok(success_response())],
    ));
    let chat_runtime = runtime_with(
        vec![chat_provider.clone()],
        1,
        4096,
        Arc::new(RecordingAudit::default()),
    );
    let chat_http = LlmBufferedHttp::new(
        chat_runtime,
        Arc::new(Allow),
        4096,
        16,
        Duration::from_secs(1),
    );
    let response = chat_http
        .handle(embedding_http_request(
            br#"{"model":"public-model","input":"x"}"#,
        ))
        .await;
    assert_eq!(response.status, 400);
    assert!(String::from_utf8_lossy(&response.body).contains("unsupported_feature"));
    assert_eq!(chat_provider.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn embeddings_denial_does_not_claim_the_post_authorization_memory_slot() {
    let provider = scripted_embedding_provider(vec![Ok(embedding_response(1, 2, 1, 0))]);
    let (runtime, _) = embedding_runtime(
        vec![(
            Arc::clone(&provider),
            EmbeddingPrice {
                version: 1,
                input_micros_per_million: 1,
            },
        )],
        1,
        None,
    );
    let denied = LlmBufferedHttp::new(
        Arc::clone(&runtime),
        Arc::new(DenyBeforeParse {
            calls: Arc::new(AtomicUsize::new(0)),
        }),
        1024,
        16,
        Duration::from_secs(1),
    )
    .handle(embedding_http_request(b"not-json"))
    .await;
    assert_eq!(denied.status, 403);
    let allowed = LlmBufferedHttp::new(runtime, Arc::new(Allow), 1024, 16, Duration::from_secs(1))
        .handle(embedding_http_request(
            br#"{"model":"embedding-default","input":"x","dimensions":2}"#,
        ))
        .await;
    assert_eq!(allowed.status, 200);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn embedding_memory_metrics_track_current_high_water_and_rejections() {
    let provider = scripted_embedding_provider(vec![]);
    let (runtime, _) = embedding_runtime(
        vec![(
            provider,
            EmbeddingPrice {
                version: 1,
                input_micros_per_million: 1,
            },
        )],
        1,
        None,
    );
    let root = runtime.snapshot();
    let permit = runtime
        .try_acquire_embedding_memory_slot(&root)
        .expect("first memory slot");
    let active = runtime.embedding_memory_metrics();
    assert_eq!(active.current_slots, 1);
    assert_eq!(
        active.current_retained_bytes,
        root.embedding_memory.per_slot_peak_bytes
    );
    assert!(matches!(
        runtime.try_acquire_embedding_memory_slot(&root),
        Err(LlmGatewayError::Capacity)
    ));
    assert_eq!(runtime.embedding_memory_metrics().rejection_count, 1);
    drop(permit);
    let released = runtime.embedding_memory_metrics();
    assert_eq!(released.current_slots, 0);
    assert_eq!(released.current_retained_bytes, 0);
    assert_eq!(released.high_water_slots, 1);
    assert_eq!(
        released.high_water_retained_bytes,
        root.embedding_memory.per_slot_peak_bytes
    );
}

#[tokio::test]
async fn embeddings_reserve_each_fallback_price_and_reconcile_input_only_usage() {
    let first = scripted_embedding_provider(vec![Err(InferenceError::timeout_before_acceptance())]);
    let second = scripted_embedding_provider(vec![Ok(embedding_response(2, 2, 4, 0))]);
    let prices = vec![
        (
            Arc::clone(&first),
            EmbeddingPrice {
                version: 1,
                input_micros_per_million: 1_000_000,
            },
        ),
        (
            Arc::clone(&second),
            EmbeddingPrice {
                version: 2,
                input_micros_per_million: 3_000_000,
            },
        ),
    ];
    let (budget_runtime, _) = embedding_runtime(prices.clone(), 2, Some(79));
    let request = EmbeddingRequest {
        model: "embedding-default".to_string(),
        inputs: vec!["a".to_string(), "b".to_string()],
        dimensions: Some(2),
    };
    let error = budget_runtime
        .execute_embedding_with_snapshot(
            LlmRequestContext::with_timeout("user", Duration::from_secs(1)),
            budget_runtime.snapshot(),
            request.clone(),
        )
        .await
        .unwrap_err();
    assert_eq!(error, LlmGatewayError::Budget);
    assert_eq!(first.calls.load(Ordering::SeqCst), 0);
    assert_eq!(second.calls.load(Ordering::SeqCst), 0);

    let (runtime, ledger) = embedding_runtime(prices, 2, Some(80));
    let execution = runtime
        .execute_embedding_with_snapshot(
            LlmRequestContext::with_timeout("user", Duration::from_secs(1)),
            runtime.snapshot(),
            request,
        )
        .await
        .unwrap();
    assert!(execution.usage.complete);
    assert_eq!(execution.usage.charged_micros, 12);
    assert_eq!(ledger.reserved(), 0);
    assert_eq!(ledger.charged(), 12);
}

#[tokio::test]
async fn embedding_fallback_reserves_an_expensive_later_candidate_before_dispatch() {
    let first = scripted_embedding_provider(vec![Ok(embedding_response(2, 2, 4, 0))]);
    let second = scripted_embedding_provider(vec![Ok(embedding_response(2, 2, 4, 0))]);
    let prices = vec![
        (
            Arc::clone(&first),
            EmbeddingPrice {
                version: 1,
                input_micros_per_million: 1_000_000,
            },
        ),
        (
            Arc::clone(&second),
            EmbeddingPrice {
                version: 2,
                input_micros_per_million: 3_000_000,
            },
        ),
    ];
    let request = EmbeddingRequest {
        model: "embedding-default".to_string(),
        inputs: vec!["a".to_string(), "b".to_string()],
        dimensions: Some(2),
    };

    let (under_budget, _) = embedding_runtime(prices.clone(), 1, Some(59));
    let error = under_budget
        .execute_embedding_with_snapshot(
            LlmRequestContext::with_timeout("user", Duration::from_secs(1)),
            under_budget.snapshot(),
            request.clone(),
        )
        .await
        .expect_err("the later candidate must be covered before any dispatch");
    assert_eq!(error, LlmGatewayError::Budget);
    assert_eq!(first.calls.load(Ordering::SeqCst), 0);
    assert_eq!(second.calls.load(Ordering::SeqCst), 0);

    let (runtime, ledger) = embedding_runtime(prices, 1, Some(60));
    let root = runtime.snapshot();
    let _half_open = occupy_half_open_probe(&root.deployments["embed-0"].circuit);
    let execution = runtime
        .execute_embedding_with_snapshot(
            LlmRequestContext::with_timeout("user", Duration::from_secs(1)),
            root,
            request,
        )
        .await
        .expect("the expensive healthy fallback is fully reserved");

    assert_eq!(execution.attempts, 1);
    assert_eq!(first.calls.load(Ordering::SeqCst), 0);
    assert_eq!(second.calls.load(Ordering::SeqCst), 1);
    assert_eq!(ledger.reserved(), 0);
    assert_eq!(ledger.charged(), 12);
}

#[tokio::test]
async fn embeddings_reject_output_usage_and_preserve_ambiguous_charge() {
    let invalid = scripted_embedding_provider(vec![Ok(embedding_response(1, 2, 2, 1))]);
    let (runtime, ledger) = embedding_runtime(
        vec![(
            invalid,
            EmbeddingPrice {
                version: 1,
                input_micros_per_million: 1_000_000,
            },
        )],
        1,
        None,
    );
    let request = EmbeddingRequest {
        model: "embedding-default".to_string(),
        inputs: vec!["x".to_string()],
        dimensions: Some(2),
    };
    let error = runtime
        .execute_embedding_with_snapshot(
            LlmRequestContext::with_timeout("user", Duration::from_secs(1)),
            runtime.snapshot(),
            request.clone(),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, LlmGatewayError::Invariant(_)));
    assert_eq!(ledger.reserved(), 0);
    assert_eq!(ledger.charged(), 10);

    let ambiguous = scripted_embedding_provider(vec![Err(
        InferenceError::timeout_after_possible_acceptance(),
    )]);
    let (runtime, ledger) = embedding_runtime(
        vec![(
            ambiguous,
            EmbeddingPrice {
                version: 1,
                input_micros_per_million: 1_000_000,
            },
        )],
        1,
        None,
    );
    assert!(
        runtime
            .execute_embedding_with_snapshot(
                LlmRequestContext::with_timeout("user", Duration::from_secs(1)),
                runtime.snapshot(),
                request,
            )
            .await
            .is_err()
    );
    assert_eq!(ledger.reserved(), 0);
    assert_eq!(ledger.charged(), 10);
}

#[tokio::test]
async fn buffered_response_uses_trusted_id_and_hides_physical_provider_evidence() {
    let mut refusal_response = success_response();
    refusal_response.output = vec![GenerateOutputItem::Message {
        id: "message-refusal".to_string(),
        role: Role::Assistant,
        content: vec![ContentBlock::Refusal {
            refusal: "cannot comply".to_string(),
        }],
        status: ItemStatus::Completed,
    }];
    let provider = Arc::new(ScriptedProvider::new(
        ProviderProtocol::OpenAiChat,
        vec![Ok(success_response()), Ok(refusal_response)],
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
    let body: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
    assert!(!body.to_string().contains("physical-secret"));
    assert_eq!(body["model"], "public-model");
    assert!(body["choices"][0]["message"].get("refusal").is_none());

    let refusal = http
        .handle(http_request(
            br#"{"model":"public-model","messages":[{"role":"user","content":"unsafe"}]}"#,
        ))
        .await;
    assert_eq!(refusal.status, 200);
    let refusal: serde_json::Value = serde_json::from_slice(&refusal.body).unwrap();
    assert!(refusal["choices"][0]["message"]["content"].is_null());
    assert_eq!(refusal["choices"][0]["message"]["refusal"], "cannot comply");
}

#[tokio::test]
async fn buffered_http_rejects_method_media_size_and_operated_field_conflicts() {
    let provider = Arc::new(ScriptedProvider::new(
        ProviderProtocol::OpenAiChat,
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
        ProviderProtocol::AnthropicMessages,
        vec![Ok(success_response())],
        generation_capabilities(false, true, false),
    ));
    let openai = Arc::new(ScriptedProvider::with_capabilities(
        ProviderProtocol::OpenAiChat,
        vec![Ok(success_response())],
        generation_capabilities(true, true, true),
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
        ProviderProtocol::OpenAiChat,
        vec![Ok(success_response())],
    ));
    let anthropic = Arc::new(ScriptedProvider::new(
        ProviderProtocol::AnthropicMessages,
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
        protocol: ProviderProtocol::OpenAiChat,
        events: vec![Err(InferenceError::from_status(429, None, "limited"))],
        calls: AtomicUsize::new(0),
        wait_for_cancellation: false,
        cancellation_observed: Arc::new(AtomicBool::new(false)),
    });
    let mut second_provider = SseProvider::success();
    second_provider.protocol = ProviderProtocol::AnthropicMessages;
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
        protocol: ProviderProtocol::OpenAiChat,
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
async fn circuit_blocked_candidate_does_not_consume_the_streaming_attempt_budget() {
    let first = Arc::new(SseProvider::success());
    let second = Arc::new(SseProvider::success());
    let runtime = runtime_with(
        vec![first.clone(), second.clone()],
        1,
        4096,
        Arc::new(RecordingAudit::default()),
    );
    let root = runtime.snapshot();
    let _half_open = occupy_half_open_probe(&root.deployments["d0"].circuit);

    let execution = runtime
        .execute_stream_with_snapshot(
            LlmRequestContext::with_timeout("user", Duration::from_secs(1)),
            root,
            InferenceRequest::text("public-model", "hello"),
        )
        .await
        .expect("the healthy streaming fallback receives the one real attempt");

    assert_eq!(first.calls.load(Ordering::SeqCst), 0);
    assert_eq!(second.calls.load(Ordering::SeqCst), 1);
    drop(execution);
}

#[tokio::test]
async fn full_sse_rejects_duplicate_terminal_events_without_done() {
    let provider = Arc::new(SseProvider {
        protocol: ProviderProtocol::OpenAiChat,
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
        protocol: ProviderProtocol::OpenAiChat,
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
        protocol: ProviderProtocol::OpenAiChat,
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
        protocol: ProviderProtocol::OpenAiChat,
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
        ProviderProtocol::OpenAiChat,
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
            tenant_id: Some("tenant-test".to_string()),
            trusted_request_id: "trusted".to_string(),
        })
        .await;
    let body = String::from_utf8(response.body).unwrap();
    assert_eq!(response.status, 200);
    assert!(body.contains("public-model"));
    assert!(!body.contains("legacy-agent-internal"));

    for (alias, expected_status) in [
        ("public-model", 200),
        ("legacy-agent-internal", 404),
        ("missing", 404),
    ] {
        let response = http
            .handle(BufferedHttpRequest {
                method: "GET".to_string(),
                path: format!("/v1/models/{alias}"),
                headers: BTreeMap::new(),
                body: Vec::new(),
                principal_id: "test-agent".to_string(),
                tenant_id: Some("tenant-test".to_string()),
                trusted_request_id: "trusted-model".to_string(),
            })
            .await;
        assert_eq!(response.status, expected_status, "{alias}");
        if expected_status == 200 {
            let value: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
            assert_eq!(value["id"], "public-model");
        }
    }
}

#[tokio::test]
async fn internal_alias_invocation_is_bound_to_its_approved_principal() {
    let provider = Arc::new(ScriptedProvider::new(
        ProviderProtocol::OpenAiChat,
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
        ProviderProtocol::OpenAiChat,
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
        ProviderProtocol::OpenAiChat,
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
        ProviderProtocol::OpenAiChat,
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
fn request_scoped_pii_does_not_require_provider_conformance_evidence() {
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
        .pii_placeholder_preservation_percent = 0;
    assert!(compiler.compile(&config, 1, None).is_ok());

    config
        .deployments
        .get_mut("d")
        .unwrap()
        .pii_placeholder_preservation_percent = 101;
    assert!(compiler.compile(&config, 1, None).is_err());
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
    let GenerateOutputItem::Message { content, .. } = &execution.response.output[0] else {
        panic!("expected message response")
    };
    let ContentBlock::Text { text } = &content[0] else {
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
