mod compiler;
mod readiness;
mod snapshot;
mod store;
mod streaming;

pub use compiler::{CompileProbe, LlmCompiler};
pub use readiness::{
    DeploymentReadiness, DeploymentReadinessState, ReadinessControllerTask,
    start_readiness_controller,
};
pub use snapshot::{
    AliasPlan, DeploymentRuntime, EmbeddingMemoryBounds, LlmPublishedSnapshot,
    PrincipalPermitStripes, ProviderAccountRuntime,
};
pub use store::{LlmSnapshotStore, PublishOutcome};
pub use streaming::{
    ImmediateStreamStartBarrier, LlmStreamExecution, ResponsesResponseMetadata, StreamStartBarrier,
};

use crate::admission::fail_fast_permits;
use crate::audit::{
    AuditAdmission, AuditAttemptFinish, AuditAttemptStart, AuditFinish, AuditReservation,
    AuditStart,
};
use crate::error::LlmGatewayError;
use crate::pii::RequestPiiSession;
use crate::reasoning_seal::{ReasoningSealError, RouteCandidate};
use crate::routing::{request_capabilities, retryable};
use crate::usage::{
    ReconciledUsage, UsageReservation, cost, cost_embedding, maximum_attempt_envelope,
};
use model_provider::inference::{
    AcceptanceEvidence, ClientProtocol, ContentBlock, EmbeddingEncoding, EmbeddingRequest,
    EmbeddingResponse, InferenceRequest, InferenceResponse, Operation, ProviderContinuationState,
    ProviderProtocol, ProviderRequestContext,
};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct LlmRequestContext {
    pub request_id: String,
    pub principal_id: String,
    pub billing_subject: String,
    pub tenant_id: Option<String>,
    pub deadline: Instant,
}

impl LlmRequestContext {
    pub fn with_timeout(principal_id: impl Into<String>, timeout: Duration) -> Self {
        let principal_id = principal_id.into();
        Self {
            request_id: Uuid::now_v7().to_string(),
            billing_subject: principal_id.clone(),
            principal_id,
            tenant_id: None,
            deadline: Instant::now() + timeout,
        }
    }
}

#[derive(Debug)]
pub struct LlmExecution {
    pub response: InferenceResponse,
    pub request_id: String,
    pub alias: String,
    pub attempts: usize,
    pub usage: ReconciledUsage,
    pub generation: u64,
    pub deployment_id: String,
    pub provider_material_generation: u64,
}

#[derive(Debug)]
pub struct LlmEmbeddingExecution {
    pub response: EmbeddingResponse,
    pub request_id: String,
    pub alias: String,
    pub attempts: usize,
    pub usage: ReconciledUsage,
    pub generation: u64,
    pub selected_space: EmbeddingSpaceSelection,
}

fn rejected_finish() -> AuditFinish {
    AuditFinish {
        terminal: "rejected",
        attempts: 0,
        charged_micros: 0,
        usage_complete: true,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingSpaceExpectation {
    pub space_id: String,
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingSpaceSelection {
    pub contract: model_provider::inference::EmbeddingSpaceContract,
    pub required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbeddingMemoryMetricsSnapshot {
    pub current_slots: usize,
    pub current_retained_bytes: usize,
    pub high_water_slots: usize,
    pub high_water_retained_bytes: usize,
    pub rejection_count: u64,
}

#[derive(Debug, Default)]
struct EmbeddingMemoryMetrics {
    current_slots: AtomicUsize,
    current_retained_bytes: AtomicUsize,
    high_water_slots: AtomicUsize,
    high_water_retained_bytes: AtomicUsize,
    rejection_count: AtomicU64,
}

#[derive(Debug)]
pub struct EmbeddingMemoryPermit {
    _permit: OwnedSemaphorePermit,
    metrics: Arc<EmbeddingMemoryMetrics>,
    retained_bytes: usize,
}

impl Drop for EmbeddingMemoryPermit {
    fn drop(&mut self) {
        let current_slots = self
            .metrics
            .current_slots
            .fetch_sub(1, Ordering::AcqRel)
            .saturating_sub(1);
        let current_retained_bytes = self
            .metrics
            .current_retained_bytes
            .fetch_sub(self.retained_bytes, Ordering::AcqRel)
            .saturating_sub(self.retained_bytes);
        tracing::info!(
            target: "llm_gateway_metrics",
            metric = "embedding_memory_retained",
            current_slots,
            current_retained_bytes,
            "LLM embedding memory slot released"
        );
    }
}

pub struct LlmRuntime {
    store: Arc<LlmSnapshotStore>,
    audit: Arc<dyn AuditAdmission>,
    global_permits: Arc<Semaphore>,
    stream_permits: Arc<Semaphore>,
    stream_start_barrier: Arc<dyn StreamStartBarrier>,
    embedding_memory_metrics: Arc<EmbeddingMemoryMetrics>,
}

impl LlmRuntime {
    pub fn new(store: Arc<LlmSnapshotStore>, audit: Arc<dyn AuditAdmission>) -> Self {
        let permits = store.load().global_concurrency;
        let stream_permits = store.load().global_stream_concurrency;
        Self {
            store,
            audit,
            global_permits: Arc::new(Semaphore::new(permits)),
            stream_permits: Arc::new(Semaphore::new(stream_permits)),
            stream_start_barrier: Arc::new(ImmediateStreamStartBarrier),
            embedding_memory_metrics: Arc::new(EmbeddingMemoryMetrics::default()),
        }
    }

    pub fn with_stream_start_barrier(mut self, barrier: Arc<dyn StreamStartBarrier>) -> Self {
        self.stream_start_barrier = barrier;
        self
    }

    pub fn snapshot(&self) -> Arc<LlmPublishedSnapshot> {
        self.store.load()
    }

    /// Module reload publishes a fully compiled candidate through the same
    /// store used by request execution; each request captures one immutable root.
    pub fn snapshot_store(&self) -> Arc<LlmSnapshotStore> {
        Arc::clone(&self.store)
    }

    pub fn publish(&self, candidate: LlmPublishedSnapshot) -> PublishOutcome {
        self.store.publish(candidate)
    }

    pub fn try_acquire_embedding_memory_slot(
        &self,
        root: &LlmPublishedSnapshot,
    ) -> Result<EmbeddingMemoryPermit, LlmGatewayError> {
        let permit = match Arc::clone(&root.embedding_memory_permits).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                let rejection_count = self
                    .embedding_memory_metrics
                    .rejection_count
                    .fetch_add(1, Ordering::AcqRel)
                    .saturating_add(1);
                tracing::warn!(
                    target: "llm_gateway_metrics",
                    metric = "embedding_memory_rejections",
                    rejection_count,
                    "LLM embedding memory admission rejected"
                );
                return Err(LlmGatewayError::Capacity);
            }
        };
        let retained_bytes = root.embedding_memory.per_slot_peak_bytes;
        let current_slots = self
            .embedding_memory_metrics
            .current_slots
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        let current_retained_bytes = self
            .embedding_memory_metrics
            .current_retained_bytes
            .fetch_add(retained_bytes, Ordering::AcqRel)
            .saturating_add(retained_bytes);
        self.embedding_memory_metrics
            .high_water_slots
            .fetch_max(current_slots, Ordering::AcqRel);
        self.embedding_memory_metrics
            .high_water_retained_bytes
            .fetch_max(current_retained_bytes, Ordering::AcqRel);
        tracing::info!(
            target: "llm_gateway_metrics",
            metric = "embedding_memory_retained",
            current_slots,
            current_retained_bytes,
            high_water_slots = self
                .embedding_memory_metrics
                .high_water_slots
                .load(Ordering::Acquire),
            high_water_retained_bytes = self
                .embedding_memory_metrics
                .high_water_retained_bytes
                .load(Ordering::Acquire),
            "LLM embedding memory slot acquired"
        );
        Ok(EmbeddingMemoryPermit {
            _permit: permit,
            metrics: Arc::clone(&self.embedding_memory_metrics),
            retained_bytes,
        })
    }

    pub fn embedding_memory_metrics(&self) -> EmbeddingMemoryMetricsSnapshot {
        EmbeddingMemoryMetricsSnapshot {
            current_slots: self
                .embedding_memory_metrics
                .current_slots
                .load(Ordering::Acquire),
            current_retained_bytes: self
                .embedding_memory_metrics
                .current_retained_bytes
                .load(Ordering::Acquire),
            high_water_slots: self
                .embedding_memory_metrics
                .high_water_slots
                .load(Ordering::Acquire),
            high_water_retained_bytes: self
                .embedding_memory_metrics
                .high_water_retained_bytes
                .load(Ordering::Acquire),
            rejection_count: self
                .embedding_memory_metrics
                .rejection_count
                .load(Ordering::Acquire),
        }
    }

    pub async fn execute(
        &self,
        context: LlmRequestContext,
        request: InferenceRequest,
    ) -> Result<LlmExecution, LlmGatewayError> {
        let root = self.store.load();
        self.execute_with_snapshot(context, root, request).await
    }

    pub async fn execute_with_snapshot(
        &self,
        context: LlmRequestContext,
        root: Arc<LlmPublishedSnapshot>,
        request: InferenceRequest,
    ) -> Result<LlmExecution, LlmGatewayError> {
        self.execute_with_snapshot_protocol(
            context,
            root,
            request,
            ClientProtocol::InternalCanonical,
        )
        .await
    }

    pub async fn execute_with_snapshot_protocol(
        &self,
        context: LlmRequestContext,
        root: Arc<LlmPublishedSnapshot>,
        mut request: InferenceRequest,
        client_protocol: ClientProtocol,
    ) -> Result<LlmExecution, LlmGatewayError> {
        if context.deadline <= Instant::now() {
            return Err(LlmGatewayError::Provider(
                model_provider::inference::InferenceError::timeout_before_acceptance(),
            ));
        }
        // The caller captures exactly one root; all request work uses that generation.
        let alias = root
            .aliases
            .get(&request.model)
            .ok_or(LlmGatewayError::AliasNotFound)?
            .clone();
        if alias.internal && alias.bound_principal.as_deref() != Some(context.principal_id.as_str())
        {
            return Err(LlmGatewayError::AliasNotFound);
        }
        if !alias.operations.contains(&Operation::Generate) {
            return Err(LlmGatewayError::UnsupportedCapability(
                "model alias does not support generate".to_string(),
            ));
        }
        let principal_permit = root.principal_permits.permits_for(&context.principal_id);
        let _permits = fail_fast_permits(&self.global_permits, &principal_permit, &alias.permits)?;

        let mut pii = RequestPiiSession::new(alias.pii.clone())?;
        let audit = self
            .audit
            .reserve(
                alias.audit,
                AuditStart {
                    request_id: context.request_id.clone(),
                    principal_id: context.principal_id.clone(),
                    billing_subject: context.billing_subject.clone(),
                    alias: alias.public_name.clone(),
                    operation: Operation::Generate,
                    generation: root.generation,
                    snapshot_digest: root.digest.clone(),
                    max_attempts: alias.max_attempts,
                    pii_profile: pii.profile_id(),
                    expected_embedding_space_id: None,
                    expected_embedding_space_revision: None,
                    selected_embedding_space_id: None,
                    selected_embedding_space_revision: None,
                },
            )
            .await?;

        let has_tool_result = request.messages.iter().any(|message| {
            message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
        });
        let has_assistant_tool_call = request.messages.iter().any(|message| {
            message.role == model_provider::inference::Role::Assistant
                && message
                    .content
                    .iter()
                    .any(|block| matches!(block, ContentBlock::ToolCall { .. }))
        });
        let mut pinned_deployment_id = None;
        let mut carries_continuation = false;
        if let Some(sealed) = request.provider_continuation.take() {
            carries_continuation = true;
            if client_protocol == ClientProtocol::AnthropicMessages
                && sealed.protocol == ProviderProtocol::AnthropicMessages
            {
                request.provider_continuation = Some(sealed);
            } else {
                let resolved = (|| {
                    if client_protocol != ClientProtocol::OpenAiResponses
                        || sealed.protocol != ProviderProtocol::OpenAiResponses
                    {
                        return Err(LlmGatewayError::ReasoningState(ReasoningSealError::Invalid));
                    }
                    let tenant_id = context.tenant_id.as_deref().ok_or_else(|| {
                        LlmGatewayError::InvalidRequest(
                            "authenticated tenant identity is required for reasoning continuation"
                                .to_string(),
                        )
                    })?;
                    let encoded = std::str::from_utf8(sealed.payload.as_slice()).map_err(|_| {
                        LlmGatewayError::ReasoningState(ReasoningSealError::Invalid)
                    })?;
                    let route_candidates = alias
                        .deployments
                        .iter()
                        .map(|deployment| RouteCandidate {
                            deployment_id: deployment.id.as_str(),
                            provider_material_generation: deployment.provider_client_generation,
                        })
                        .collect::<Vec<_>>();
                    root.reasoning_sealer
                        .unseal(
                            encoded,
                            tenant_id,
                            &alias.public_name,
                            client_protocol,
                            &route_candidates,
                        )
                        .map_err(LlmGatewayError::ReasoningState)
                })();
                let resolved = match resolved {
                    Ok(resolved) => resolved,
                    Err(error) => {
                        finish_audit(
                            audit,
                            rejected_finish(),
                            error.public_status(),
                            error.public_code(),
                        )
                        .await?;
                        return Err(error);
                    }
                };
                pinned_deployment_id = Some(resolved.deployment_id);
                request.provider_continuation = Some(ProviderContinuationState {
                    protocol: ProviderProtocol::BedrockConverse,
                    payload: resolved.provider_state,
                });
            }
        } else if client_protocol == ClientProtocol::OpenAiResponses
            && request.reasoning.is_some()
            && has_assistant_tool_call
            && has_tool_result
        {
            let error = LlmGatewayError::ReasoningState(ReasoningSealError::Required);
            finish_audit(
                audit,
                rejected_finish(),
                error.public_status(),
                error.public_code(),
            )
            .await?;
            return Err(error);
        }

        if let Err(error) = pii.tokenize_request(&mut request) {
            finish_audit(
                audit,
                AuditFinish {
                    terminal: "rejected",
                    attempts: 0,
                    charged_micros: 0,
                    usage_complete: true,
                },
                error.public_status(),
                error.public_code(),
            )
            .await?;
            return Err(error);
        }

        let estimated_input = estimate_tokens(&request);
        let max_output = request
            .token_limits
            .max_output_tokens
            .map(u64::from)
            .or(alias.max_output_tokens)
            .unwrap_or(1024);
        if alias
            .max_input_tokens
            .is_some_and(|limit| estimated_input > limit)
            || alias
                .max_output_tokens
                .is_some_and(|limit| max_output > limit)
        {
            let error = LlmGatewayError::InvalidRequest("token limit exceeded".to_string());
            finish_audit(
                audit,
                AuditFinish {
                    terminal: "rejected",
                    attempts: 0,
                    charged_micros: 0,
                    usage_complete: true,
                },
                error.public_status(),
                error.public_code(),
            )
            .await?;
            return Err(error);
        }
        if alias.max_attempts > 1
            && serde_json::to_vec(&request)
                .map_or(true, |bytes| bytes.len() > root.max_replay_bytes)
        {
            let error = LlmGatewayError::InvalidRequest(
                "request exceeds replay bound required by retry policy".to_string(),
            );
            finish_audit(
                audit,
                AuditFinish {
                    terminal: "rejected",
                    attempts: 0,
                    charged_micros: 0,
                    usage_complete: true,
                },
                error.public_status(),
                error.public_code(),
            )
            .await?;
            return Err(error);
        }

        let required = alias.merge_requirements(request_capabilities(&request, false));
        if !alias.deployments.iter().any(|deployment| {
            let mut candidate_request = request.clone();
            deployment.supports_static(&required)
                && deployment
                    .prepare_generate_request(&mut candidate_request, client_protocol)
                    .is_ok()
        }) {
            let error = LlmGatewayError::UnsupportedCapability(
                "no configured route preserves the requested generate capabilities".to_string(),
            );
            finish_audit(
                audit,
                AuditFinish {
                    terminal: "rejected",
                    attempts: 0,
                    charged_micros: 0,
                    usage_complete: true,
                },
                error.public_status(),
                error.public_code(),
            )
            .await?;
            return Err(error);
        }
        let candidates = alias
            .deployments
            .iter()
            .filter(|deployment| {
                pinned_deployment_id
                    .as_ref()
                    .is_none_or(|pinned| pinned == &deployment.id)
                    && deployment.supports(&required)
                    && {
                        let mut candidate_request = request.clone();
                        deployment
                            .prepare_generate_request(&mut candidate_request, client_protocol)
                            .is_ok()
                    }
                    && (pinned_deployment_id.is_none()
                        || deployment.provider.protocol() == ProviderProtocol::BedrockConverse)
            })
            .cloned()
            .collect::<Vec<_>>();
        if carries_continuation && pinned_deployment_id.is_none() && candidates.len() != 1 {
            let error = LlmGatewayError::ReasoningState(ReasoningSealError::RouteUnavailable);
            finish_audit(
                audit,
                rejected_finish(),
                error.public_status(),
                error.public_code(),
            )
            .await?;
            return Err(error);
        }
        if pinned_deployment_id.is_some() && candidates.is_empty() {
            let error = LlmGatewayError::ReasoningState(ReasoningSealError::RouteUnavailable);
            finish_audit(
                audit,
                rejected_finish(),
                error.public_status(),
                error.public_code(),
            )
            .await?;
            return Err(error);
        }
        let Some(first_price) = candidates.first().map(|candidate| {
            candidate
                .generation_price()
                .expect("generation route has generation price")
        }) else {
            let error = LlmGatewayError::NoReadyDeployment;
            finish_audit(
                audit,
                AuditFinish {
                    terminal: "rejected",
                    attempts: 0,
                    charged_micros: 0,
                    usage_complete: true,
                },
                error.public_status(),
                error.public_code(),
            )
            .await?;
            return Err(error);
        };
        let envelope = maximum_attempt_envelope(
            candidates.iter().map(|candidate| {
                cost(
                    candidate
                        .generation_price()
                        .expect("generation route has generation price"),
                    estimated_input,
                    max_output,
                )
            }),
            alias.max_attempts,
        )
        .unwrap_or(u64::MAX);
        let reservation = match UsageReservation::reserve(
            Arc::clone(&alias.ledger),
            envelope,
            alias.max_cost_micros,
        ) {
            Ok(reservation) => reservation,
            Err(error) => {
                finish_audit(
                    audit,
                    AuditFinish {
                        terminal: "rejected",
                        attempts: 0,
                        charged_micros: 0,
                        usage_complete: true,
                    },
                    error.public_status(),
                    error.public_code(),
                )
                .await?;
                return Err(error);
            }
        };

        let mut attempts = 0;
        let mut last_error = None;
        let mut attempted_envelope = 0_u64;
        for deployment in candidates {
            if attempts >= alias.max_attempts {
                break;
            }
            if context.deadline <= Instant::now() {
                last_error =
                    Some(model_provider::inference::InferenceError::timeout_before_acceptance());
                break;
            }
            let circuit_permit = match deployment.acquire_dispatch_health(Instant::now()) {
                Ok(permit) => permit,
                Err(_) => continue,
            };
            let _provider_permit = match Arc::clone(&deployment.permits).try_acquire_owned() {
                Ok(permit) => permit,
                Err(_) => continue,
            };
            let next_attempt = attempts + 1;
            if let Err(audit_error) = audit
                .attempt_started(AuditAttemptStart {
                    attempt: next_attempt,
                    deployment_id: deployment.id.clone(),
                    transport_context: Some(deployment.audit_transport.clone()),
                })
                .await
            {
                let usage = reservation.reconcile(
                    deployment
                        .generation_price()
                        .expect("generation route has generation price"),
                    None,
                    AcceptanceEvidence::NotAccepted,
                );
                finish_audit(
                    audit,
                    AuditFinish {
                        terminal: "audit_failed",
                        attempts,
                        charged_micros: usage.charged_micros,
                        usage_complete: usage.complete,
                    },
                    audit_error.public_status(),
                    audit_error.public_code(),
                )
                .await?;
                return Err(audit_error);
            }
            attempts = next_attempt;
            attempted_envelope = attempted_envelope.saturating_add(cost(
                deployment
                    .generation_price()
                    .expect("generation route has generation price"),
                estimated_input,
                max_output,
            ));
            let mut provider_request = request.clone();
            provider_request.model = deployment.model.clone();
            deployment.prepare_generate_request(&mut provider_request, client_protocol)?;
            let deployment_deadline = context
                .deadline
                .min(Instant::now() + Duration::from_millis(deployment.request_timeout_ms));
            let provider_context = ProviderRequestContext {
                deadline: deployment_deadline,
                cancellation: tokio_util::sync::CancellationToken::new(),
                attempt_id: format!("{}-{attempts}", context.request_id),
                trace: Default::default(),
            };
            let provider = deployment.provider.generation().ok_or_else(|| {
                LlmGatewayError::Invariant(
                    "generate route selected an embedding executor".to_string(),
                )
            })?;
            let provider_result = tokio::time::timeout(
                deployment_deadline.saturating_duration_since(Instant::now()),
                provider.generate(provider_context, provider_request),
            )
            .await
            .unwrap_or_else(|_| {
                Err(model_provider::inference::InferenceError::timeout_after_possible_acceptance())
            });
            let provider_result = provider_result.and_then(|response| {
                let continuation_protocol = response
                    .evidence
                    .continuation
                    .as_ref()
                    .map(|continuation| continuation.protocol);
                let eligible = match (client_protocol, continuation_protocol) {
                    (_, None) => true,
                    (ClientProtocol::OpenAiResponses, Some(ProviderProtocol::BedrockConverse)) => true,
                    (
                        ClientProtocol::AnthropicMessages,
                        Some(ProviderProtocol::BedrockConverse | ProviderProtocol::AnthropicMessages),
                    ) => true,
                    _ => false,
                };
                if eligible {
                    Ok(response)
                } else {
                    Err(model_provider::inference::InferenceError::provider_protocol(
                        Some(502),
                        "provider reasoning continuation is not eligible for this client protocol",
                    ))
                }
            });
            match provider_result {
                Ok(mut response) => {
                    circuit_permit.success();
                    let usage = reservation.reconcile(
                        deployment
                            .generation_price()
                            .expect("generation route has generation price"),
                        response.usage.as_ref(),
                        AcceptanceEvidence::Accepted,
                    );
                    if let Err(error) = pii.recover_response(&mut response) {
                        audit
                            .attempt_finished(AuditAttemptFinish {
                                attempt: attempts,
                                terminal: "failed",
                                category: "pii_unresolved",
                            })
                            .await?;
                        finish_audit(
                            audit,
                            AuditFinish {
                                terminal: "pii_recovery_failed",
                                attempts,
                                charged_micros: usage.charged_micros,
                                usage_complete: usage.complete,
                            },
                            error.public_status(),
                            error.public_code(),
                        )
                        .await?;
                        return Err(error);
                    }
                    let attempt_audit = audit
                        .attempt_finished(AuditAttemptFinish {
                            attempt: attempts,
                            terminal: "complete",
                            category: "success",
                        })
                        .await;
                    if let Err(audit_error) = attempt_audit {
                        finish_audit(
                            audit,
                            AuditFinish {
                                terminal: "audit_failed",
                                attempts,
                                charged_micros: usage.charged_micros,
                                usage_complete: usage.complete,
                            },
                            audit_error.public_status(),
                            audit_error.public_code(),
                        )
                        .await?;
                        return Err(audit_error);
                    }
                    finish_audit(
                        audit,
                        AuditFinish {
                            terminal: "complete",
                            attempts,
                            charged_micros: usage.charged_micros,
                            usage_complete: usage.complete,
                        },
                        200,
                        "success",
                    )
                    .await?;
                    return Ok(LlmExecution {
                        response,
                        request_id: context.request_id,
                        alias: alias.public_name.clone(),
                        attempts,
                        usage,
                        generation: root.generation,
                        deployment_id: deployment.id.clone(),
                        provider_material_generation: deployment.provider_client_generation,
                    });
                }
                Err(error) => {
                    circuit_permit.failure(&error, Instant::now());
                    let attempt_audit = audit
                        .attempt_finished(AuditAttemptFinish {
                            attempt: attempts,
                            terminal: "failed",
                            category: inference_error_category(error.category),
                        })
                        .await;
                    if let Err(audit_error) = attempt_audit {
                        let usage = reservation.reconcile_with_ambiguous_bound(
                            deployment
                                .generation_price()
                                .expect("generation route has generation price"),
                            None,
                            error.acceptance,
                            attempted_envelope,
                        );
                        finish_audit(
                            audit,
                            AuditFinish {
                                terminal: "audit_failed",
                                attempts,
                                charged_micros: usage.charged_micros,
                                usage_complete: usage.complete,
                            },
                            audit_error.public_status(),
                            audit_error.public_code(),
                        )
                        .await?;
                        return Err(audit_error);
                    }
                    let can_retry = !carries_continuation
                        && pinned_deployment_id.is_none()
                        && retryable(&error)
                        && attempts < alias.max_attempts;
                    last_error = Some(error);
                    if !can_retry {
                        break;
                    }
                }
            }
        }
        let error = last_error.unwrap_or_else(|| model_provider::inference::InferenceError {
            category: model_provider::inference::InferenceErrorCategory::ProviderOverload,
            provider_status: None,
            retry: model_provider::inference::RetryDisposition::Safe,
            acceptance: AcceptanceEvidence::NotAccepted,
            retry_after_ms: None,
            detail: "no deployment is currently available".to_string(),
        });
        let usage = reservation.reconcile_with_ambiguous_bound(
            first_price,
            None,
            error.acceptance,
            attempted_envelope,
        );
        let public_error = if error.category
            == model_provider::inference::InferenceErrorCategory::UnsupportedFeature
        {
            LlmGatewayError::Invariant(
                "eligible generation provider returned UnsupportedFeature".to_string(),
            )
        } else {
            LlmGatewayError::Provider(error)
        };
        finish_audit(
            audit,
            AuditFinish {
                terminal: "failed",
                attempts,
                charged_micros: usage.charged_micros,
                usage_complete: usage.complete,
            },
            public_error.public_status(),
            public_error.public_code(),
        )
        .await?;
        Err(public_error)
    }

    pub async fn execute_embedding_with_snapshot(
        &self,
        context: LlmRequestContext,
        root: Arc<LlmPublishedSnapshot>,
        request: EmbeddingRequest,
    ) -> Result<LlmEmbeddingExecution, LlmGatewayError> {
        self.execute_embedding_with_snapshot_expectation(context, root, request, None)
            .await
    }

    pub async fn execute_embedding_with_snapshot_expectation(
        &self,
        context: LlmRequestContext,
        root: Arc<LlmPublishedSnapshot>,
        request: EmbeddingRequest,
        expectation: Option<EmbeddingSpaceExpectation>,
    ) -> Result<LlmEmbeddingExecution, LlmGatewayError> {
        self.execute_embedding_with_snapshot_expectation_and_budget(
            context,
            root,
            request,
            expectation,
            None,
        )
        .await
    }

    pub async fn execute_embedding_with_snapshot_expectation_and_budget(
        &self,
        context: LlmRequestContext,
        root: Arc<LlmPublishedSnapshot>,
        mut request: EmbeddingRequest,
        expectation: Option<EmbeddingSpaceExpectation>,
        maximum_billed_cost_micros: Option<u64>,
    ) -> Result<LlmEmbeddingExecution, LlmGatewayError> {
        if context.deadline <= Instant::now() {
            return Err(LlmGatewayError::Provider(
                model_provider::inference::InferenceError::timeout_before_acceptance(),
            ));
        }
        let alias = root
            .aliases
            .get(&request.model)
            .ok_or(LlmGatewayError::AliasNotFound)?
            .clone();
        if alias.internal && alias.bound_principal.as_deref() != Some(context.principal_id.as_str())
        {
            return Err(LlmGatewayError::AliasNotFound);
        }
        if !alias.operations.contains(&Operation::Embed) {
            return Err(LlmGatewayError::UnsupportedCapability(
                "model alias does not support embed".to_string(),
            ));
        }
        if alias.embedding_workload_lane != root.embedding_workload_lane {
            return Err(LlmGatewayError::AliasNotFound);
        }
        let selected_space = self.validate_embedding_space_expectation(
            &alias,
            expectation.as_ref(),
            request.dimensions,
        )?;
        if selected_space.required && request.dimensions.is_none() {
            request.dimensions = Some(selected_space.contract.dimension);
        }
        if request.inputs.is_empty() {
            return Err(LlmGatewayError::InvalidRequest(
                "embedding input must not be empty".to_string(),
            ));
        }
        let principal_permit = root.principal_permits.permits_for(&context.principal_id);
        let _permits = fail_fast_permits(&self.global_permits, &principal_permit, &alias.permits)?;
        let audit = self
            .audit
            .reserve(
                alias.audit,
                AuditStart {
                    request_id: context.request_id.clone(),
                    principal_id: context.principal_id.clone(),
                    billing_subject: context.billing_subject.clone(),
                    alias: alias.public_name.clone(),
                    operation: Operation::Embed,
                    generation: root.generation,
                    snapshot_digest: root.digest.clone(),
                    max_attempts: alias.max_attempts,
                    pii_profile: "none".to_string(),
                    expected_embedding_space_id: expectation
                        .as_ref()
                        .map(|value| value.space_id.clone()),
                    expected_embedding_space_revision: expectation
                        .as_ref()
                        .map(|value| value.revision),
                    selected_embedding_space_id: Some(selected_space.contract.space_id.clone()),
                    selected_embedding_space_revision: Some(selected_space.contract.revision),
                },
            )
            .await?;
        if alias.max_attempts > 1
            && serde_json::to_vec(&request).map_or(true, |bytes| {
                bytes.len() > root.embedding_memory.max_replay_bytes
            })
        {
            let error = LlmGatewayError::InvalidRequest(
                "embedding request exceeds replay bound required by retry policy".to_string(),
            );
            finish_audit(
                audit,
                AuditFinish {
                    terminal: "rejected",
                    attempts: 0,
                    charged_micros: 0,
                    usage_complete: true,
                },
                error.public_status(),
                error.public_code(),
            )
            .await?;
            return Err(error);
        }
        let item_count = request.inputs.len();
        let items_per_permit = root.embedding_memory.items_per_permit;
        let compatible = |deployment: &DeploymentRuntime, dynamic: bool| {
            let requirements = model_provider::conformance::CapabilityRequirements::embedding();
            let base = if dynamic {
                deployment.supports(&requirements)
            } else {
                deployment.supports_static(&requirements)
            };
            let Some(capabilities) = deployment.capabilities.embedding.as_ref() else {
                return false;
            };
            let weight = item_count.div_ceil(items_per_permit);
            base && item_count <= capabilities.max_batch_items as usize
                && weight <= deployment.configured_concurrency
                && capabilities
                    .supported_encodings
                    .contains(&EmbeddingEncoding::Float)
                && request.dimensions.is_none_or(|dimensions| {
                    capabilities.supported_dimensions.contains(&dimensions)
                })
        };
        if !alias
            .deployments
            .iter()
            .any(|deployment| compatible(deployment, false))
        {
            let error = LlmGatewayError::UnsupportedCapability(
                "no configured route preserves the requested embedding capabilities".to_string(),
            );
            finish_audit(
                audit,
                AuditFinish {
                    terminal: "rejected",
                    attempts: 0,
                    charged_micros: 0,
                    usage_complete: true,
                },
                error.public_status(),
                error.public_code(),
            )
            .await?;
            return Err(error);
        }
        let candidates = alias
            .deployments
            .iter()
            .filter(|deployment| compatible(deployment, true))
            .cloned()
            .collect::<Vec<_>>();
        let Some(first_price) = candidates
            .first()
            .and_then(|candidate| candidate.embedding_price())
        else {
            let error = LlmGatewayError::NoReadyDeployment;
            finish_audit(
                audit,
                AuditFinish {
                    terminal: "rejected",
                    attempts: 0,
                    charged_micros: 0,
                    usage_complete: true,
                },
                error.public_status(),
                error.public_code(),
            )
            .await?;
            return Err(error);
        };
        let candidate_envelope = |deployment: &DeploymentRuntime| {
            let capabilities = deployment
                .capabilities
                .embedding
                .as_ref()
                .expect("embedding route has embedding capabilities");
            let count = u64::try_from(item_count).unwrap_or(u64::MAX);
            let input_bound = count
                .saturating_mul(capabilities.max_input_tokens_per_item)
                .min(capabilities.max_aggregate_input_tokens);
            cost_embedding(
                deployment
                    .embedding_price()
                    .expect("embedding route has embedding price"),
                input_bound,
                0,
            )
        };
        let envelope_result = candidates
            .iter()
            .map(|candidate| candidate_envelope(candidate))
            .collect::<Result<Vec<_>, _>>()
            .and_then(|costs| {
                maximum_attempt_envelope(costs, alias.max_attempts).ok_or_else(|| {
                    LlmGatewayError::Invariant(
                        "embedding reservation envelope overflow".to_string(),
                    )
                })
            });
        let envelope = match envelope_result {
            Ok(envelope) => envelope,
            Err(error) => {
                finish_audit(
                    audit,
                    AuditFinish {
                        terminal: "rejected",
                        attempts: 0,
                        charged_micros: 0,
                        usage_complete: true,
                    },
                    error.public_status(),
                    error.public_code(),
                )
                .await?;
                return Err(error);
            }
        };
        let request_budget = match (alias.max_cost_micros, maximum_billed_cost_micros) {
            (Some(alias_budget), Some(request_budget)) => Some(alias_budget.min(request_budget)),
            (alias_budget, request_budget) => alias_budget.or(request_budget),
        };
        let reservation =
            match UsageReservation::reserve(Arc::clone(&alias.ledger), envelope, request_budget) {
                Ok(reservation) => reservation,
                Err(error) => {
                    finish_audit(
                        audit,
                        AuditFinish {
                            terminal: "rejected",
                            attempts: 0,
                            charged_micros: 0,
                            usage_complete: true,
                        },
                        error.public_status(),
                        error.public_code(),
                    )
                    .await?;
                    return Err(error);
                }
            };
        let mut attempts = 0;
        let mut last_error = None;
        let mut attempted_envelope = 0_u64;
        for deployment in candidates {
            if attempts >= alias.max_attempts {
                break;
            }
            if context.deadline <= Instant::now() {
                last_error =
                    Some(model_provider::inference::InferenceError::timeout_before_acceptance());
                break;
            }
            let circuit_permit = match deployment.acquire_dispatch_health(Instant::now()) {
                Ok(permit) => permit,
                Err(_) => continue,
            };
            let weight = item_count.div_ceil(items_per_permit);
            let Ok(weight) = u32::try_from(weight) else {
                return Err(LlmGatewayError::Invariant(
                    "embedding permit weight exceeds u32".to_string(),
                ));
            };
            let _account_permit =
                match Arc::clone(&deployment.account.permits).try_acquire_many_owned(weight) {
                    Ok(permit) => permit,
                    Err(_) => continue,
                };
            let _provider_permit =
                match Arc::clone(&deployment.permits).try_acquire_many_owned(weight) {
                    Ok(permit) => permit,
                    Err(_) => continue,
                };
            let next_attempt = attempts + 1;
            if let Err(audit_error) = audit
                .attempt_started(AuditAttemptStart {
                    attempt: next_attempt,
                    deployment_id: deployment.id.clone(),
                    transport_context: Some(deployment.audit_transport.clone()),
                })
                .await
            {
                let usage = match reservation.reconcile_embedding(
                    deployment
                        .embedding_price()
                        .expect("embedding route has embedding price"),
                    None,
                    AcceptanceEvidence::NotAccepted,
                    attempted_envelope,
                ) {
                    Ok(usage) => usage,
                    Err(reconcile_error) => {
                        finish_audit(
                            audit,
                            AuditFinish {
                                terminal: "audit_failed",
                                attempts,
                                charged_micros: 0,
                                usage_complete: false,
                            },
                            reconcile_error.public_status(),
                            reconcile_error.public_code(),
                        )
                        .await?;
                        return Err(reconcile_error);
                    }
                };
                finish_audit(
                    audit,
                    AuditFinish {
                        terminal: "audit_failed",
                        attempts,
                        charged_micros: usage.charged_micros,
                        usage_complete: usage.complete,
                    },
                    audit_error.public_status(),
                    audit_error.public_code(),
                )
                .await?;
                return Err(audit_error);
            }
            attempts = next_attempt;
            attempted_envelope = attempted_envelope
                .checked_add(candidate_envelope(&deployment)?)
                .ok_or_else(|| {
                    LlmGatewayError::Invariant("embedding attempted envelope overflow".to_string())
                })?;
            request.model = deployment.model.clone();
            let deployment_deadline = context
                .deadline
                .min(Instant::now() + Duration::from_millis(deployment.request_timeout_ms));
            let provider_context = ProviderRequestContext {
                deadline: deployment_deadline,
                cancellation: tokio_util::sync::CancellationToken::new(),
                attempt_id: format!("{}-{attempts}", context.request_id),
                trace: Default::default(),
            };
            let provider = deployment.provider.embedding().ok_or_else(|| {
                LlmGatewayError::Invariant("embed route selected a generation executor".to_string())
            })?;
            let provider_result = tokio::time::timeout(
                deployment_deadline.saturating_duration_since(Instant::now()),
                provider.embed(provider_context, request.clone()),
            )
            .await
            .unwrap_or_else(|_| {
                Err(model_provider::inference::InferenceError::timeout_after_possible_acceptance())
            });
            match provider_result {
                Ok(response) => {
                    if let Err(error) = validate_embedding_response(
                        &deployment,
                        &request,
                        &response,
                        root.embedding_memory.max_canonical_vector_bytes,
                    ) {
                        circuit_permit.failure(&error, Instant::now());
                        audit
                            .attempt_finished(AuditAttemptFinish {
                                attempt: attempts,
                                terminal: "failed",
                                category: "protocol",
                            })
                            .await?;
                        let usage = match reservation.reconcile_embedding(
                            deployment
                                .embedding_price()
                                .expect("embedding route has embedding price"),
                            None,
                            error.acceptance,
                            attempted_envelope,
                        ) {
                            Ok(usage) => usage,
                            Err(reconcile_error) => {
                                finish_audit(
                                    audit,
                                    AuditFinish {
                                        terminal: "failed",
                                        attempts,
                                        charged_micros: 0,
                                        usage_complete: false,
                                    },
                                    reconcile_error.public_status(),
                                    reconcile_error.public_code(),
                                )
                                .await?;
                                return Err(reconcile_error);
                            }
                        };
                        let public_error = LlmGatewayError::Provider(error);
                        finish_audit(
                            audit,
                            AuditFinish {
                                terminal: "failed",
                                attempts,
                                charged_micros: usage.charged_micros,
                                usage_complete: usage.complete,
                            },
                            public_error.public_status(),
                            public_error.public_code(),
                        )
                        .await?;
                        return Err(public_error);
                    }
                    circuit_permit.success();
                    if response
                        .usage
                        .output_tokens
                        .is_some_and(|tokens| tokens != 0)
                    {
                        let usage = match reservation.reconcile_embedding(
                            deployment
                                .embedding_price()
                                .expect("embedding route has embedding price"),
                            None,
                            AcceptanceEvidence::Accepted,
                            attempted_envelope,
                        ) {
                            Ok(usage) => usage,
                            Err(reconcile_error) => {
                                finish_audit(
                                    audit,
                                    AuditFinish {
                                        terminal: "failed",
                                        attempts,
                                        charged_micros: 0,
                                        usage_complete: false,
                                    },
                                    reconcile_error.public_status(),
                                    reconcile_error.public_code(),
                                )
                                .await?;
                                return Err(reconcile_error);
                            }
                        };
                        let error = LlmGatewayError::Invariant(
                            "embedding usage reported output tokens".to_string(),
                        );
                        audit
                            .attempt_finished(AuditAttemptFinish {
                                attempt: attempts,
                                terminal: "failed",
                                category: "invariant",
                            })
                            .await?;
                        finish_audit(
                            audit,
                            AuditFinish {
                                terminal: "failed",
                                attempts,
                                charged_micros: usage.charged_micros,
                                usage_complete: usage.complete,
                            },
                            error.public_status(),
                            error.public_code(),
                        )
                        .await?;
                        return Err(error);
                    }
                    let usage = match reservation.reconcile_embedding(
                        deployment
                            .embedding_price()
                            .expect("embedding route has embedding price"),
                        Some(&response.usage),
                        AcceptanceEvidence::Accepted,
                        attempted_envelope,
                    ) {
                        Ok(usage) => usage,
                        Err(error) => {
                            audit
                                .attempt_finished(AuditAttemptFinish {
                                    attempt: attempts,
                                    terminal: "failed",
                                    category: "invariant",
                                })
                                .await?;
                            finish_audit(
                                audit,
                                AuditFinish {
                                    terminal: "failed",
                                    attempts,
                                    charged_micros: 0,
                                    usage_complete: false,
                                },
                                error.public_status(),
                                error.public_code(),
                            )
                            .await?;
                            return Err(error);
                        }
                    };
                    audit
                        .attempt_finished(AuditAttemptFinish {
                            attempt: attempts,
                            terminal: "complete",
                            category: "success",
                        })
                        .await?;
                    finish_audit(
                        audit,
                        AuditFinish {
                            terminal: "complete",
                            attempts,
                            charged_micros: usage.charged_micros,
                            usage_complete: usage.complete,
                        },
                        200,
                        "success",
                    )
                    .await?;
                    tracing::info!(
                        target: "llm_gateway_metrics",
                        operation = "embed",
                        embedding_items = item_count,
                        "LLM embedding request completed"
                    );
                    return Ok(LlmEmbeddingExecution {
                        response,
                        request_id: context.request_id,
                        alias: alias.public_name.clone(),
                        attempts,
                        usage,
                        generation: root.generation,
                        selected_space,
                    });
                }
                Err(error) => {
                    circuit_permit.failure(&error, Instant::now());
                    audit
                        .attempt_finished(AuditAttemptFinish {
                            attempt: attempts,
                            terminal: "failed",
                            category: inference_error_category(error.category),
                        })
                        .await?;
                    let can_retry = retryable(&error) && attempts < alias.max_attempts;
                    last_error = Some(error);
                    if !can_retry {
                        break;
                    }
                }
            }
        }
        let error = last_error.unwrap_or_else(|| model_provider::inference::InferenceError {
            category: model_provider::inference::InferenceErrorCategory::ProviderOverload,
            provider_status: None,
            retry: model_provider::inference::RetryDisposition::Safe,
            acceptance: AcceptanceEvidence::NotAccepted,
            retry_after_ms: None,
            detail: "no embedding deployment is currently available".to_string(),
        });
        let usage = match reservation.reconcile_embedding(
            first_price,
            None,
            error.acceptance,
            attempted_envelope,
        ) {
            Ok(usage) => usage,
            Err(reconcile_error) => {
                finish_audit(
                    audit,
                    AuditFinish {
                        terminal: "failed",
                        attempts,
                        charged_micros: 0,
                        usage_complete: false,
                    },
                    reconcile_error.public_status(),
                    reconcile_error.public_code(),
                )
                .await?;
                return Err(reconcile_error);
            }
        };
        let public_error = if error.category
            == model_provider::inference::InferenceErrorCategory::UnsupportedFeature
        {
            LlmGatewayError::Invariant(
                "eligible embedding provider returned UnsupportedFeature".to_string(),
            )
        } else {
            LlmGatewayError::Provider(error)
        };
        finish_audit(
            audit,
            AuditFinish {
                terminal: "failed",
                attempts,
                charged_micros: usage.charged_micros,
                usage_complete: usage.complete,
            },
            public_error.public_status(),
            public_error.public_code(),
        )
        .await?;
        Err(public_error)
    }

    pub fn probe_embedding_space(
        &self,
        root: &LlmPublishedSnapshot,
        principal_id: &str,
        model: &str,
        expectation: Option<&EmbeddingSpaceExpectation>,
        dimensions: Option<u32>,
    ) -> Result<EmbeddingSpaceSelection, LlmGatewayError> {
        let alias = root
            .aliases
            .get(model)
            .ok_or(LlmGatewayError::AliasNotFound)?;
        if alias.internal && alias.bound_principal.as_deref() != Some(principal_id) {
            return Err(LlmGatewayError::AliasNotFound);
        }
        if !alias.operations.contains(&Operation::Embed) {
            return Err(LlmGatewayError::UnsupportedCapability(
                "model alias does not support embed".to_string(),
            ));
        }
        if alias.embedding_workload_lane != root.embedding_workload_lane {
            return Err(LlmGatewayError::AliasNotFound);
        }
        self.validate_embedding_space_expectation(alias, expectation, dimensions)
    }

    pub async fn audit_embedding_space_rejection(
        &self,
        root: &LlmPublishedSnapshot,
        principal_id: &str,
        billing_subject: &str,
        model: &str,
        expectation: Option<&EmbeddingSpaceExpectation>,
    ) -> Result<(), LlmGatewayError> {
        let Some(alias) = root.aliases.get(model) else {
            return Ok(());
        };
        let selected = alias.required_capabilities.embedding_space.as_ref();
        let audit = self
            .audit
            .reserve(
                alias.audit,
                AuditStart {
                    request_id: uuid::Uuid::now_v7().to_string(),
                    principal_id: principal_id.to_string(),
                    billing_subject: billing_subject.to_string(),
                    alias: alias.public_name.clone(),
                    operation: Operation::Embed,
                    generation: root.generation,
                    snapshot_digest: root.digest.clone(),
                    max_attempts: 0,
                    pii_profile: "none".to_string(),
                    expected_embedding_space_id: expectation.map(|value| value.space_id.clone()),
                    expected_embedding_space_revision: expectation.map(|value| value.revision),
                    selected_embedding_space_id: selected.map(|value| value.space_id.clone()),
                    selected_embedding_space_revision: selected.map(|value| value.revision),
                },
            )
            .await?;
        audit
            .finish(AuditFinish {
                terminal: "rejected",
                attempts: 0,
                charged_micros: 0,
                usage_complete: true,
            })
            .await
    }

    fn validate_embedding_space_expectation(
        &self,
        alias: &AliasPlan,
        expectation: Option<&EmbeddingSpaceExpectation>,
        dimensions: Option<u32>,
    ) -> Result<EmbeddingSpaceSelection, LlmGatewayError> {
        let contract = alias
            .required_capabilities
            .embedding_space
            .clone()
            .ok_or_else(|| {
                LlmGatewayError::Invariant(
                    "published embedding alias has no embedding-space contract".to_string(),
                )
            })?;
        if alias.require_expected_embedding_space && expectation.is_none() {
            return Err(LlmGatewayError::UnsupportedCapability(
                "model alias requires an expected embedding-space contract".to_string(),
            ));
        }
        if expectation.is_some_and(|expected| {
            expected.space_id != contract.space_id || expected.revision != contract.revision
        }) {
            tracing::warn!(
                target: "llm_gateway_audit",
                public_alias = %alias.public_name,
                expected_space_id = expectation.map(|value| value.space_id.as_str()).unwrap_or(""),
                expected_space_revision = expectation.map(|value| value.revision).unwrap_or_default(),
                selected_space_id = %contract.space_id,
                selected_space_revision = contract.revision,
                outcome = "embedding_space_mismatch",
                "LLM embedding space expectation rejected before dispatch"
            );
            return Err(LlmGatewayError::UnsupportedCapability(
                "expected embedding space does not match the model alias".to_string(),
            ));
        }
        if dimensions.is_some_and(|value| value != contract.dimension) {
            return Err(LlmGatewayError::UnsupportedCapability(
                "dimensions do not match the model alias embedding space".to_string(),
            ));
        }
        Ok(EmbeddingSpaceSelection {
            contract,
            required: alias.require_expected_embedding_space,
        })
    }

    pub fn eligible_formats(
        &self,
        root: &LlmPublishedSnapshot,
        principal: &str,
        request: &InferenceRequest,
        streaming: bool,
    ) -> Result<BTreeSet<ProviderProtocol>, LlmGatewayError> {
        let alias = root
            .aliases
            .get(&request.model)
            .ok_or(LlmGatewayError::AliasNotFound)?;
        if alias.internal && alias.bound_principal.as_deref() != Some(principal) {
            return Err(LlmGatewayError::AliasNotFound);
        }
        let required = alias.merge_requirements(request_capabilities(request, streaming));
        if !alias
            .deployments
            .iter()
            .any(|deployment| deployment.supports_static(&required))
        {
            return Err(LlmGatewayError::UnsupportedCapability(
                "no configured route preserves the requested generate capabilities".to_string(),
            ));
        }
        let formats = alias
            .deployments
            .iter()
            .filter(|deployment| deployment.supports(&required))
            .map(|deployment| deployment.provider.protocol())
            .collect::<BTreeSet<_>>();
        if formats.is_empty() {
            return Err(LlmGatewayError::NoReadyDeployment);
        }
        Ok(formats)
    }

    pub fn visible_models(&self) -> Vec<String> {
        self.store
            .load()
            .aliases
            .values()
            .filter(|alias| {
                !alias.internal
                    && alias.operations.iter().any(|operation| {
                        let required = match operation {
                            Operation::Generate => {
                                model_provider::conformance::CapabilityRequirements::generation()
                            }
                            Operation::Embed => {
                                model_provider::conformance::CapabilityRequirements::embedding()
                            }
                        };
                        alias.deployments.iter().any(|deployment| {
                            deployment.supports(&alias.merge_requirements(required.clone()))
                        })
                    })
            })
            .map(|alias| alias.public_name.clone())
            .collect()
    }
}

async fn finish_audit(
    audit: Box<dyn AuditReservation>,
    finish: AuditFinish,
    suppressed_status: u16,
    suppressed_code: &'static str,
) -> Result<(), LlmGatewayError> {
    if let Err(audit_error) = audit.finish(finish).await {
        tracing::warn!(
            audit_error = %audit_error,
            suppressed_status,
            suppressed_code,
            "audit finalization failure suppressed an LLM terminal result"
        );
        return Err(audit_error);
    }
    Ok(())
}

fn estimate_tokens(request: &InferenceRequest) -> u64 {
    // MVP admission heuristic: deliberately conservative until LF-6B installs
    // provider-aware tokenizers. JSON framing and tool schemas are included.
    let bytes = serde_json::to_vec(request).map_or(0, |bytes| bytes.len() as u64);
    bytes.saturating_add(3) / 4
}

fn validate_embedding_response(
    deployment: &DeploymentRuntime,
    request: &EmbeddingRequest,
    response: &EmbeddingResponse,
    max_canonical_vector_bytes: usize,
) -> Result<(), model_provider::inference::InferenceError> {
    let capabilities = deployment.capabilities.embedding.as_ref().ok_or_else(|| {
        model_provider::inference::InferenceError::provider_protocol(
            Some(502),
            "embedding deployment has no compiled capabilities",
        )
    })?;
    if response.vectors.len() != request.inputs.len() {
        return Err(
            model_provider::inference::InferenceError::provider_protocol(
                Some(502),
                "embedding provider returned the wrong vector count",
            ),
        );
    }
    let mut bytes = 0_usize;
    for (position, vector) in response.vectors.iter().enumerate() {
        let dimensions = u32::try_from(vector.values.len()).map_err(|_| {
            model_provider::inference::InferenceError::provider_protocol(
                Some(502),
                "embedding vector dimension exceeds u32",
            )
        })?;
        if usize::try_from(vector.index).ok() != Some(position)
            || dimensions == 0
            || !capabilities.supported_dimensions.contains(&dimensions)
            || request
                .dimensions
                .is_some_and(|expected| expected != dimensions)
            || vector.values.iter().any(|value| !value.is_finite())
        {
            return Err(
                model_provider::inference::InferenceError::provider_protocol(
                    Some(502),
                    "embedding provider returned invalid indices, dimensions, or values",
                ),
            );
        }
        bytes = bytes
            .checked_add(
                vector
                    .values
                    .len()
                    .checked_mul(std::mem::size_of::<f32>())
                    .ok_or_else(|| {
                        model_provider::inference::InferenceError::provider_protocol(
                            Some(502),
                            "embedding vector byte size overflow",
                        )
                    })?,
            )
            .ok_or_else(|| {
                model_provider::inference::InferenceError::provider_protocol(
                    Some(502),
                    "embedding response byte size overflow",
                )
            })?;
    }
    if bytes > max_canonical_vector_bytes {
        return Err(
            model_provider::inference::InferenceError::provider_protocol(
                Some(502),
                "embedding canonical vectors exceed the compiled byte bound",
            ),
        );
    }
    Ok(())
}

fn inference_error_category(
    category: model_provider::inference::InferenceErrorCategory,
) -> &'static str {
    use model_provider::inference::InferenceErrorCategory as Category;
    match category {
        Category::InvalidRequest => "invalid_request",
        Category::Authentication => "authentication",
        Category::PermissionDenied => "permission_denied",
        Category::RateLimited => "rate_limited",
        Category::TimeoutBeforeAcceptance => "timeout_before_acceptance",
        Category::TimeoutAfterPossibleAcceptance => "timeout_after_possible_acceptance",
        Category::ProviderOverload => "provider_overload",
        Category::Network => "network",
        Category::SecurityInvariant => "security_invariant",
        Category::Protocol => "protocol",
        Category::Cancelled => "cancelled",
        Category::UnsupportedFeature => "unsupported_feature",
    }
}
