use async_trait::async_trait;
use bytes::Bytes;
use futures_util::StreamExt;
use model_provider::inference::{
    AcceptanceEvidence, ClientProtocol, FinishReason, InferenceError, InferenceErrorCategory,
    InferenceEvent, InferenceRequest, NormalizedUsage, Operation, ProviderContinuationState,
    ProviderProtocol, ProviderRequestContext, ToolCallDelta,
};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{
    LlmRequestContext, LlmRuntime, estimate_tokens, finish_audit, inference_error_category,
};
use crate::admission::fail_fast_permits;
use crate::audit::{AuditAttemptFinish, AuditAttemptStart, AuditFinish, AuditStart};
use crate::error::LlmGatewayError;
use crate::pii::{RequestPiiSession, UnresolvedPiiBehavior};
use crate::reasoning_seal::{ReasoningBinding, ReasoningSealError, RouteCandidate};
use crate::routing::{request_capabilities, retryable};
use crate::usage::{UsageReservation, cost, maximum_attempt_envelope};

#[async_trait]
pub trait StreamStartBarrier: Send + Sync {
    async fn wait_until_durable(&self, request_id: &str) -> Result<(), LlmGatewayError>;
}

pub struct ImmediateStreamStartBarrier;

/// Sanitized client-visible Responses request settings. The client codec has
/// already validated these values before this profile is constructed.
#[derive(Clone)]
pub struct ResponsesResponseMetadata {
    fields: Map<String, Value>,
}

impl ResponsesResponseMetadata {
    pub fn from_validated_request(request: &Value) -> Self {
        let mut fields = Map::new();
        for (key, default) in [
            ("instructions", Value::Null),
            ("max_output_tokens", Value::Null),
            ("parallel_tool_calls", Value::Bool(false)),
            ("reasoning", json!({"effort":null,"summary":null})),
            ("temperature", Value::Null),
            ("text", json!({"format":{"type":"text"}})),
            ("tool_choice", json!("auto")),
            ("tools", json!([])),
            ("top_p", Value::Null),
            ("truncation", json!("disabled")),
            ("metadata", json!({})),
        ] {
            fields.insert(
                key.to_string(),
                request
                    .get(key)
                    .filter(|value| !value.is_null())
                    .cloned()
                    .unwrap_or(default),
            );
        }
        Self { fields }
    }

    pub(crate) fn apply(&self, response: &mut Value) {
        let Some(object) = response.as_object_mut() else {
            return;
        };
        object.extend(self.fields.clone());
    }
}

impl Default for ResponsesResponseMetadata {
    fn default() -> Self {
        Self::from_validated_request(&Value::Null)
    }
}

#[async_trait]
impl StreamStartBarrier for ImmediateStreamStartBarrier {
    async fn wait_until_durable(&self, _request_id: &str) -> Result<(), LlmGatewayError> {
        Ok(())
    }
}

pub struct LlmStreamExecution {
    receiver: mpsc::Receiver<GatewayStreamEvent>,
    cancellation: CancellationToken,
    encoder: ClientStreamEncoder,
    pub request_id: String,
    pub alias: String,
    pub generation: u64,
    pub write_timeout: Duration,
    pub minimum_drain_bytes_per_second: u64,
    pub drain_grace: Duration,
}

impl LlmStreamExecution {
    pub async fn next_frame(&mut self) -> Option<Bytes> {
        loop {
            if let Some(frame) = self.encoder.pending.pop_front() {
                return Some(frame);
            }
            let event = self.receiver.recv().await?;
            self.encoder.encode(event);
            if self.encoder.limit_exceeded {
                self.cancellation.cancel();
                self.receiver.close();
            }
        }
    }

    pub fn cancel(&self) {
        self.cancellation.cancel();
    }
}

impl Drop for LlmStreamExecution {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

impl LlmRuntime {
    pub async fn execute_stream_with_snapshot(
        &self,
        context: LlmRequestContext,
        root: Arc<super::LlmPublishedSnapshot>,
        request: InferenceRequest,
    ) -> Result<LlmStreamExecution, LlmGatewayError> {
        self.execute_stream_with_snapshot_options(context, root, request, true)
            .await
    }

    pub async fn execute_stream_with_snapshot_options(
        &self,
        context: LlmRequestContext,
        root: Arc<super::LlmPublishedSnapshot>,
        request: InferenceRequest,
        client_include_usage: bool,
    ) -> Result<LlmStreamExecution, LlmGatewayError> {
        self.execute_stream_with_snapshot_protocol(
            context,
            root,
            request,
            ClientProtocol::OpenAiChat,
            client_include_usage,
            None,
        )
        .await
    }

    pub async fn execute_stream_with_snapshot_protocol(
        &self,
        context: LlmRequestContext,
        root: Arc<super::LlmPublishedSnapshot>,
        mut request: InferenceRequest,
        client_protocol: ClientProtocol,
        client_include_usage: bool,
        responses_metadata: Option<ResponsesResponseMetadata>,
    ) -> Result<LlmStreamExecution, LlmGatewayError> {
        if context.deadline <= Instant::now() {
            return Err(LlmGatewayError::Provider(
                InferenceError::timeout_before_acceptance(),
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
        if !alias.operations.contains(&Operation::Generate) {
            return Err(LlmGatewayError::UnsupportedCapability(
                "model alias does not support generate".to_string(),
            ));
        }
        let principal_permit = root.principal_permits.permits_for(&context.principal_id);
        let request_permits =
            fail_fast_permits(&self.stream_permits, &principal_permit, &alias.permits)?;
        let mut pii = RequestPiiSession::new(alias.pii.clone())?;
        let audit = self
            .audit
            .reserve(
                alias.audit,
                AuditStart {
                    request_id: context.request_id.clone(),
                    principal_id: context.principal_id.clone(),
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
            message.content.iter().any(|block| {
                matches!(
                    block,
                    model_provider::inference::ContentBlock::ToolResult { .. }
                )
            })
        });
        let has_assistant_tool_call = request.messages.iter().any(|message| {
            message.role == model_provider::inference::Role::Assistant
                && message.content.iter().any(|block| {
                    matches!(
                        block,
                        model_provider::inference::ContentBlock::ToolCall { .. }
                    )
                })
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
                let result = (|| {
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
                let resolved = match result {
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
                rejected_finish(),
                error.public_status(),
                error.public_code(),
            )
            .await?;
            return Err(error);
        }

        if alias.pii.enabled && alias.pii.unresolved == UnresolvedPiiBehavior::RejectBuffered {
            let error = LlmGatewayError::InvalidRequest(
                "reject-buffered PII policy is not streaming-compatible".to_string(),
            );
            finish_audit(
                audit,
                rejected_finish(),
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
                rejected_finish(),
                error.public_status(),
                error.public_code(),
            )
            .await?;
            return Err(error);
        }

        let required = alias.merge_requirements(request_capabilities(&request, true));
        if !alias.deployments.iter().any(|deployment| {
            let mut candidate_request = request.clone();
            deployment.supports_static(&required)
                && deployment
                    .prepare_generate_request(&mut candidate_request, client_protocol)
                    .is_ok()
        }) {
            let error = LlmGatewayError::UnsupportedCapability(
                "no configured route preserves the requested streaming capabilities".to_string(),
            );
            finish_audit(
                audit,
                rejected_finish(),
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
            finish_audit(audit, rejected_finish(), 404, "model_not_found").await?;
            return Err(LlmGatewayError::NoReadyDeployment);
        };
        let envelope = maximum_attempt_envelope(
            candidates.iter().map(|deployment| {
                cost(
                    deployment
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
                    rejected_finish(),
                    error.public_status(),
                    error.public_code(),
                )
                .await?;
                return Err(error);
            }
        };

        let mut deadline = tokio::time::Instant::from_std(context.deadline);
        let cancellation = CancellationToken::new();
        let idle_timeout = Duration::from_millis(root.stream_idle_timeout_ms);
        let progress_timeout = Duration::from_millis(root.stream_write_timeout_ms);
        let mut attempts = 0_usize;
        let mut attempted_envelope = 0_u64;
        let mut last_error = None;
        let mut selected = None;

        for deployment in candidates {
            if attempts >= alias.max_attempts {
                break;
            }
            if context.deadline <= Instant::now() {
                last_error = Some(InferenceError::timeout_before_acceptance());
                break;
            }
            let circuit_permit = match deployment.acquire_dispatch_health(Instant::now()) {
                Ok(permit) => permit,
                Err(_) => continue,
            };
            let provider_permit = match Arc::clone(&deployment.permits).try_acquire_owned() {
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

            let attempt_started_at = tokio::time::Instant::now();
            let deployment_deadline = deadline
                .min(attempt_started_at + Duration::from_millis(deployment.request_timeout_ms));
            let setup_deadline = deployment_deadline.min(
                attempt_started_at + Duration::from_millis(deployment.stream_setup_timeout_ms),
            );

            let barrier = tokio::select! {
                _ = tokio::time::sleep_until(setup_deadline) => {
                    Err(LlmGatewayError::Provider(InferenceError::timeout_before_acceptance()))
                }
                result = self.stream_start_barrier.wait_until_durable(&context.request_id) => result,
            };
            if let Err(error) = barrier {
                if let Err(audit_error) = audit
                    .attempt_finished(AuditAttemptFinish {
                        attempt: attempts,
                        terminal: "rejected",
                        category: "setup_timeout",
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
                        terminal: "rejected",
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

            let mut provider_request = request.clone();
            provider_request.model = deployment.model.clone();
            deployment.prepare_generate_request(&mut provider_request, client_protocol)?;
            let attempt_cancellation = cancellation.child_token();
            let provider_context = ProviderRequestContext {
                deadline: deployment_deadline.into_std(),
                cancellation: attempt_cancellation.clone(),
                attempt_id: format!("{}-{attempts}", context.request_id),
                trace: Default::default(),
            };
            let stream_result = tokio::select! {
                _ = tokio::time::sleep_until(setup_deadline) => {
                    attempt_cancellation.cancel();
                    Err(InferenceError::timeout_after_possible_acceptance())
                }
                result = deployment.provider.generation()
                    .expect("generate route selected a generation executor")
                    .generate_stream(provider_context, provider_request) => result,
            };
            match stream_result {
                Ok(mut provider_stream) => {
                    let first = tokio::select! {
                        _ = tokio::time::sleep_until(setup_deadline) => {
                            attempt_cancellation.cancel();
                            Err(InferenceError::timeout_after_possible_acceptance())
                        }
                        _ = tokio::time::sleep(idle_timeout) => {
                            attempt_cancellation.cancel();
                            Err(InferenceError::timeout_after_possible_acceptance())
                        }
                        event = provider_stream.next() => event.unwrap_or_else(|| {
                            Err(InferenceError::protocol(
                                "provider stream ended before its first event",
                            ))
                        }),
                    };
                    match first {
                        Ok(event) => {
                            // Once an attempt is selected, all downstream
                            // stream reads/writes use its qualified total
                            // request deadline rather than the router default.
                            deadline = deployment_deadline;
                            selected = Some((
                                deployment,
                                circuit_permit,
                                provider_permit,
                                provider_stream,
                                event,
                            ));
                            break;
                        }
                        Err(error) => {
                            attempt_cancellation.cancel();
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

        let Some((deployment, circuit_permit, provider_permit, mut provider_stream, first_event)) =
            selected
        else {
            let error = last_error.unwrap_or_else(|| InferenceError {
                category: model_provider::inference::InferenceErrorCategory::ProviderOverload,
                provider_status: None,
                retry: model_provider::inference::RetryDisposition::Safe,
                acceptance: AcceptanceEvidence::NotAccepted,
                retry_after_ms: None,
                detail: "no streaming deployment is currently available".to_string(),
            });
            let usage = reservation.reconcile_with_ambiguous_bound(
                first_price,
                None,
                error.acceptance,
                attempted_envelope,
            );
            let public_error = if error.category == InferenceErrorCategory::UnsupportedFeature {
                LlmGatewayError::Invariant(
                    "eligible streaming provider returned UnsupportedFeature".to_string(),
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
            return Err(public_error);
        };

        let (sender, receiver) = mpsc::channel(root.stream_channel_capacity);
        let producer_cancellation = cancellation.clone();
        let request_id = context.request_id.clone();
        let response_tenant_id = context.tenant_id.clone();
        let response_alias = alias.public_name.clone();
        let reasoning_sealer = Arc::clone(&root.reasoning_sealer);
        let response_deployment_id = deployment.id.clone();
        let response_material_generation = deployment.provider_client_generation;
        tokio::spawn(async move {
            let _request_permits = request_permits;
            let _provider_permit = provider_permit;
            let mut pending = Some(Ok(first_event));
            let mut usage: Option<NormalizedUsage> = None;
            let mut finish_reason: Option<FinishReason> = None;
            let mut visible = false;
            let mut completed = false;
            let mut stream_error = None;
            let mut pii = pii.stream_recoverer();
            loop {
                let next = if let Some(event) = pending.take() {
                    Some(event)
                } else {
                    tokio::select! {
                        _ = producer_cancellation.cancelled() => {
                            let _ = tokio::time::timeout(
                                Duration::from_millis(10),
                                provider_stream.next(),
                            ).await;
                            stream_error = Some(InferenceError::cancelled());
                            None
                        }
                        _ = tokio::time::sleep_until(deadline) => {
                            producer_cancellation.cancel();
                            stream_error = Some(InferenceError::timeout_after_possible_acceptance());
                            None
                        }
                        _ = tokio::time::sleep(idle_timeout) => {
                            producer_cancellation.cancel();
                            stream_error = Some(InferenceError::timeout_after_possible_acceptance());
                            None
                        }
                        next = provider_stream.next() => next,
                    }
                };
                let Some(next) = next else { break };
                match next {
                    Ok(InferenceEvent::Usage { usage: value }) => {
                        usage = Some(value.clone());
                        if client_protocol == ClientProtocol::AnthropicMessages
                            && let Err(error) = send_event(
                                &sender,
                                GatewayStreamEvent::Usage(value),
                                &producer_cancellation,
                                deadline,
                                progress_timeout,
                            )
                            .await
                        {
                            producer_cancellation.cancel();
                            stream_error = Some(error);
                            break;
                        }
                    }
                    Ok(InferenceEvent::ProviderContinuation { state }) => {
                        if finish_reason.is_some()
                            || !continuation_is_eligible(client_protocol, state.protocol)
                        {
                            stream_error = Some(InferenceError::protocol(
                                "provider reasoning continuation is not eligible for this client protocol",
                            ));
                            break;
                        }
                        let gateway_event = match client_protocol {
                            ClientProtocol::OpenAiResponses => {
                                let Some(tenant_id) = response_tenant_id.as_deref() else {
                                    stream_error = Some(InferenceError::protocol(
                                        "authenticated tenant identity is required for reasoning continuation",
                                    ));
                                    break;
                                };
                                let encrypted = match reasoning_sealer.seal(
                                    &ReasoningBinding {
                                        tenant_id,
                                        alias: &response_alias,
                                        client_protocol,
                                        deployment_id: &response_deployment_id,
                                        provider_material_generation: response_material_generation,
                                    },
                                    state.payload.as_slice(),
                                ) {
                                    Ok(value) => value,
                                    Err(error) => {
                                        stream_error = Some(InferenceError::protocol(error.code()));
                                        break;
                                    }
                                };
                                GatewayStreamEvent::ReasoningContinuation(encrypted)
                            }
                            ClientProtocol::AnthropicMessages => {
                                GatewayStreamEvent::AnthropicReasoningContinuation(state)
                            }
                            _ => {
                                stream_error = Some(InferenceError::protocol(
                                    "provider reasoning continuation is not eligible for this client protocol",
                                ));
                                break;
                            }
                        };
                        if let Err(error) = send_event(
                            &sender,
                            gateway_event,
                            &producer_cancellation,
                            deadline,
                            progress_timeout,
                        )
                        .await
                        {
                            producer_cancellation.cancel();
                            stream_error = Some(error);
                            break;
                        }
                        visible = true;
                    }
                    Ok(InferenceEvent::MessageEnd {
                        finish_reason: terminal_reason,
                        ..
                    }) => {
                        if finish_reason.replace(terminal_reason).is_some() {
                            stream_error = Some(InferenceError::protocol(
                                "provider emitted more than one terminal stream event",
                            ));
                            break;
                        }
                    }
                    Ok(event) => {
                        if finish_reason.is_some() {
                            stream_error = Some(InferenceError::protocol(
                                "provider emitted semantic output after the terminal stream event",
                            ));
                            break;
                        }
                        let event = match pii.recover(event) {
                            Ok(event) => event,
                            Err(_) => {
                                stream_error =
                                    Some(InferenceError::protocol("PII stream recovery failed"));
                                break;
                            }
                        };
                        if let Some(event) = event.and_then(GatewayStreamEvent::from_provider) {
                            if let Err(error) = send_event(
                                &sender,
                                event,
                                &producer_cancellation,
                                deadline,
                                progress_timeout,
                            )
                            .await
                            {
                                producer_cancellation.cancel();
                                stream_error = Some(error);
                                break;
                            }
                            visible = true;
                        }
                    }
                    Err(error) => {
                        stream_error = Some(error);
                        break;
                    }
                }
            }

            if stream_error.is_none() {
                if let Some(finish_reason) = finish_reason {
                    match pii.finish() {
                        Ok(events) => {
                            for event in events {
                                if let Some(event) = GatewayStreamEvent::from_provider(event) {
                                    if let Err(error) = send_event(
                                        &sender,
                                        event,
                                        &producer_cancellation,
                                        deadline,
                                        progress_timeout,
                                    )
                                    .await
                                    {
                                        producer_cancellation.cancel();
                                        stream_error = Some(error);
                                        break;
                                    }
                                    visible = true;
                                }
                            }
                        }
                        Err(_) => {
                            stream_error =
                                Some(InferenceError::protocol("PII stream recovery failed"));
                        }
                    }
                    if stream_error.is_some() {
                        // Do not emit a successful terminal marker after PII
                        // recovery failed.
                    } else {
                        if let Err(error) = send_event(
                            &sender,
                            GatewayStreamEvent::Completed {
                                finish_reason,
                                usage: usage.clone(),
                                include_usage: client_include_usage,
                            },
                            &producer_cancellation,
                            deadline,
                            progress_timeout,
                        )
                        .await
                        {
                            producer_cancellation.cancel();
                            stream_error = Some(error);
                        }
                        visible = true;
                        completed = stream_error.is_none();
                    }
                } else {
                    stream_error = Some(InferenceError::protocol(
                        "provider stream ended without a terminal event",
                    ));
                }
            }

            if stream_error.is_some() && !producer_cancellation.is_cancelled() {
                let _ = send_event(
                    &sender,
                    GatewayStreamEvent::Failed,
                    &producer_cancellation,
                    deadline,
                    progress_timeout,
                )
                .await;
            }

            let acceptance = if completed {
                AcceptanceEvidence::Accepted
            } else if visible {
                AcceptanceEvidence::PossiblyAccepted
            } else {
                stream_error
                    .as_ref()
                    .map_or(AcceptanceEvidence::PossiblyAccepted, |error| {
                        error.acceptance
                    })
            };
            let reconciled = reservation.reconcile(
                deployment
                    .generation_price()
                    .expect("generation route has generation price"),
                usage.as_ref(),
                acceptance,
            );
            if completed {
                circuit_permit.success();
            } else if let Some(error) = stream_error.as_ref() {
                circuit_permit.failure(error, Instant::now());
            }
            let terminal = if completed {
                "complete"
            } else if producer_cancellation.is_cancelled() {
                "cancelled"
            } else {
                "failed"
            };
            let category = stream_error
                .as_ref()
                .map_or("success", |error| inference_error_category(error.category));
            if let Some(error) = stream_error
                .as_ref()
                .filter(|error| error.category == InferenceErrorCategory::Protocol)
            {
                // Protocol details originate from the gateway's fixed
                // decoder/runtime taxonomy, never from provider response
                // bodies, so operators can distinguish strictness drift.
                tracing::warn!(
                    request_id = %request_id,
                    protocol_reason = %error.detail,
                    "LLM provider stream violated the terminal-event contract"
                );
            }
            if audit
                .attempt_finished(AuditAttemptFinish {
                    attempt: attempts,
                    terminal,
                    category,
                })
                .await
                .is_err()
            {
                tracing::warn!(
                    request_id = %request_id,
                    "LLM stream attempt audit finalization failed"
                );
            }
            let _ = finish_audit(
                audit,
                AuditFinish {
                    terminal,
                    attempts,
                    charged_micros: reconciled.charged_micros,
                    usage_complete: reconciled.complete,
                },
                if completed { 200 } else { 502 },
                if completed {
                    "success"
                } else {
                    "provider_error"
                },
            )
            .await;
        });

        Ok(LlmStreamExecution {
            receiver,
            cancellation,
            encoder: ClientStreamEncoder::new(
                client_protocol,
                context.request_id.clone(),
                alias.public_name.clone(),
                root.max_stream_response_bytes,
                responses_metadata.unwrap_or_default(),
            ),
            request_id: context.request_id,
            alias: alias.public_name.clone(),
            generation: root.generation,
            write_timeout: progress_timeout,
            minimum_drain_bytes_per_second: root.stream_minimum_drain_bytes_per_second,
            drain_grace: Duration::from_millis(root.stream_drain_grace_ms),
        })
    }
}

fn rejected_finish() -> AuditFinish {
    AuditFinish {
        terminal: "rejected",
        attempts: 0,
        charged_micros: 0,
        usage_complete: true,
    }
}

fn continuation_is_eligible(
    client_protocol: ClientProtocol,
    provider_protocol: ProviderProtocol,
) -> bool {
    matches!(
        (client_protocol, provider_protocol),
        (
            ClientProtocol::OpenAiResponses,
            ProviderProtocol::BedrockConverse
        ) | (
            ClientProtocol::AnthropicMessages,
            ProviderProtocol::BedrockConverse | ProviderProtocol::AnthropicMessages
        )
    )
}

#[derive(Debug)]
enum GatewayStreamEvent {
    MessageStart,
    Usage(NormalizedUsage),
    TextDelta(String),
    RefusalDelta(String),
    ReasoningSummaryDelta {
        index: u32,
        text: String,
    },
    ToolCallDelta(ToolCallDelta),
    ReasoningContinuation(String),
    AnthropicReasoningContinuation(ProviderContinuationState),
    Completed {
        finish_reason: FinishReason,
        usage: Option<NormalizedUsage>,
        include_usage: bool,
    },
    Failed,
}

impl GatewayStreamEvent {
    fn from_provider(event: InferenceEvent) -> Option<Self> {
        match event {
            InferenceEvent::MessageStart { .. } => Some(Self::MessageStart),
            InferenceEvent::TextDelta { text } => Some(Self::TextDelta(text)),
            InferenceEvent::RefusalDelta { refusal } => Some(Self::RefusalDelta(refusal)),
            InferenceEvent::ReasoningSummaryDelta { index, text } => {
                Some(Self::ReasoningSummaryDelta { index, text })
            }
            InferenceEvent::ToolCallDelta { delta } => Some(Self::ToolCallDelta(delta)),
            InferenceEvent::ProviderContinuation { .. }
            | InferenceEvent::Usage { .. }
            | InferenceEvent::MessageEnd { .. } => None,
        }
    }
}

async fn send_event(
    sender: &mpsc::Sender<GatewayStreamEvent>,
    event: GatewayStreamEvent,
    cancellation: &CancellationToken,
    deadline: tokio::time::Instant,
    progress_timeout: Duration,
) -> Result<(), InferenceError> {
    tokio::select! {
        _ = cancellation.cancelled() => Err(InferenceError::cancelled()),
        _ = tokio::time::sleep_until(deadline) => {
            Err(InferenceError::timeout_after_possible_acceptance())
        }
        _ = tokio::time::sleep(progress_timeout) => {
            Err(InferenceError::timeout_after_possible_acceptance())
        }
        result = sender.send(event) => result.map_err(|_| InferenceError::cancelled()),
    }
}

struct ClientStreamEncoder {
    protocol: ClientProtocol,
    request_id: String,
    alias: String,
    created_at: u64,
    sequence_number: u64,
    pending: VecDeque<Bytes>,
    text: String,
    text_started: bool,
    refusal: String,
    refusal_started: bool,
    message_output_index: Option<u32>,
    responses_started: bool,
    tool_calls: BTreeMap<u32, EncodedToolCall>,
    reasoning_summaries: BTreeMap<u32, EncodedReasoningSummary>,
    reasoning_continuation: Option<EncodedReasoningContinuation>,
    anthropic_started: bool,
    anthropic_usage: Option<NormalizedUsage>,
    anthropic_open_blocks: BTreeSet<u32>,
    next_output_index: u32,
    max_response_bytes: usize,
    response_bytes: usize,
    limit_exceeded: bool,
    responses_metadata: ResponsesResponseMetadata,
}

#[derive(Clone, Default)]
struct EncodedToolCall {
    output_index: u32,
    call_id: String,
    name: String,
    arguments: String,
}

#[derive(Clone, Default)]
struct EncodedReasoningSummary {
    output_index: u32,
    text: String,
}

#[derive(Clone, Default)]
struct EncodedReasoningContinuation {
    output_index: u32,
    encrypted_content: String,
}

impl ClientStreamEncoder {
    fn new(
        protocol: ClientProtocol,
        request_id: String,
        alias: String,
        max_response_bytes: usize,
        responses_metadata: ResponsesResponseMetadata,
    ) -> Self {
        Self {
            protocol,
            request_id,
            alias,
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |value| value.as_secs()),
            sequence_number: 0,
            pending: VecDeque::new(),
            text: String::new(),
            text_started: false,
            refusal: String::new(),
            refusal_started: false,
            message_output_index: None,
            responses_started: false,
            tool_calls: BTreeMap::new(),
            reasoning_summaries: BTreeMap::new(),
            reasoning_continuation: None,
            anthropic_started: false,
            anthropic_usage: None,
            anthropic_open_blocks: BTreeSet::new(),
            next_output_index: 0,
            max_response_bytes,
            response_bytes: 0,
            limit_exceeded: false,
            responses_metadata,
        }
    }

    fn encode(&mut self, event: GatewayStreamEvent) {
        match self.protocol {
            ClientProtocol::OpenAiResponses => self.encode_responses(event),
            ClientProtocol::OpenAiChat | ClientProtocol::InternalCanonical => {
                self.encode_chat(event)
            }
            ClientProtocol::OpenAiEmbeddings => self.pending.push_back(sse_json(json!({
                "error":{"message":"Invalid streaming protocol.","type":"server_error","code":"internal_error"}
            }))),
            ClientProtocol::AnthropicMessages => self.encode_anthropic(event),
        }
    }

    fn encode_chat(&mut self, event: GatewayStreamEvent) {
        let id = format!("chatcmpl-{}", self.request_id);
        match event {
            GatewayStreamEvent::MessageStart => self.pending.push_back(sse_json(json!({
                "id":id,"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]
            }))),
            GatewayStreamEvent::Usage(_) => {}
            GatewayStreamEvent::TextDelta(text) => self.pending.push_back(sse_json(json!({
                "id":id,"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]
            }))),
            GatewayStreamEvent::RefusalDelta(refusal) => self.pending.push_back(sse_json(json!({
                "id":id,"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"refusal":refusal},"finish_reason":null}]
            }))),
            GatewayStreamEvent::ReasoningSummaryDelta { .. } => {},
            GatewayStreamEvent::ToolCallDelta(delta) => self.pending.push_back(sse_json(json!({
                "id":id,"object":"chat.completion.chunk","choices":[{"index":0,"delta":{"tool_calls":[{
                    "index":delta.index,"id":delta.id,"type":"function","function":{"name":delta.name,"arguments":delta.arguments_fragment}
                }]},"finish_reason":null}]
            }))),
            GatewayStreamEvent::ReasoningContinuation(_) => {
                self.pending.push_back(sse_json(json!({"error":{
                    "message":"Reasoning continuation is not available through Chat Completions.",
                    "type":"invalid_request_error","code":"unsupported_feature"
                }})));
            }
            GatewayStreamEvent::AnthropicReasoningContinuation(_) => {
                self.pending.push_back(sse_json(json!({"error":{
                    "message":"Anthropic reasoning state cannot cross into Chat Completions.",
                    "type":"invalid_request_error","code":"unsupported_feature"
                }})));
            }
            GatewayStreamEvent::Completed { finish_reason, usage, include_usage } => {
                self.pending.push_back(sse_json(json!({"id":id,"object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":finish_reason}]})));
                if include_usage && let Some(usage) = usage {
                    self.pending.push_back(sse_json(json!({"id":id,"object":"chat.completion.chunk","choices":[],"usage":{
                        "prompt_tokens":usage.input_tokens,"completion_tokens":usage.output_tokens,
                        "total_tokens":usage.input_tokens.zip(usage.output_tokens).map(|(input,output)| input.saturating_add(output))
                    }})));
                }
                self.pending.push_back(Bytes::from_static(b"data: [DONE]\n\n"));
            }
            GatewayStreamEvent::Failed => self.pending.push_back(sse_json(json!({"error":{
                "message":"The model stream terminated before completion.","type":"provider_error","code":"provider_error"
            }}))),
        }
    }

    fn encode_responses(&mut self, event: GatewayStreamEvent) {
        if self.limit_exceeded {
            return;
        }
        let response_id = format!("resp_{}", self.request_id);
        if !self.responses_started {
            self.responses_started = true;
            let mut response = json!({
                "id":response_id,"object":"response","created_at":self.created_at,"status":"in_progress",
                "background":false,"error":null,"incomplete_details":null,
                "model":self.alias,"output":[],"previous_response_id":null,
                "store":false,"usage":null
            });
            self.responses_metadata.apply(&mut response);
            self.push_named(
                "response.created",
                json!({"type":"response.created","response":response}),
            );
        }
        match event {
            GatewayStreamEvent::MessageStart => {}
            GatewayStreamEvent::Usage(_) => {}
            GatewayStreamEvent::TextDelta(delta) => {
                if !self.text_started {
                    self.text_started = true;
                    let output_index = self.ensure_message_output_index();
                    let message_id = format!("msg_{}_{}", self.request_id, output_index);
                    self.push_named("response.output_item.added", json!({"type":"response.output_item.added","output_index":output_index,"item":{"id":message_id,"type":"message","role":"assistant","status":"in_progress","content":[]}}));
                    self.push_named("response.content_part.added", json!({"type":"response.content_part.added","item_id":message_id,"output_index":output_index,"content_index":0,"part":{"type":"output_text","text":"","annotations":[]}}));
                }
                self.text.push_str(&delta);
                let output_index = self.message_output_index.unwrap_or(0);
                let message_id = format!("msg_{}_{}", self.request_id, output_index);
                self.push_named("response.output_text.delta", json!({"type":"response.output_text.delta","item_id":message_id,"output_index":output_index,"content_index":0,"delta":delta}));
            }
            GatewayStreamEvent::RefusalDelta(delta) => {
                let output_index = self.ensure_message_output_index();
                let message_id = format!("msg_{}_{}", self.request_id, output_index);
                let content_index = u32::from(self.text_started);
                if !self.refusal_started {
                    self.refusal_started = true;
                    if !self.text_started {
                        self.push_named("response.output_item.added", json!({"type":"response.output_item.added","output_index":output_index,"item":{"id":message_id,"type":"message","role":"assistant","status":"in_progress","content":[]}}));
                    }
                    self.push_named("response.content_part.added", json!({"type":"response.content_part.added","item_id":message_id,"output_index":output_index,"content_index":content_index,"part":{"type":"refusal","refusal":""}}));
                }
                self.refusal.push_str(&delta);
                self.push_named("response.refusal.delta", json!({"type":"response.refusal.delta","item_id":message_id,"output_index":output_index,"content_index":content_index,"delta":delta}));
            }
            GatewayStreamEvent::ReasoningSummaryDelta { index, text } => {
                let is_new = !self.reasoning_summaries.contains_key(&index);
                if is_new {
                    let output_index = self.allocate_output_index();
                    self.reasoning_summaries.insert(index, EncodedReasoningSummary { output_index, text: String::new() });
                    let item_id = format!("rs_{}_{}", self.request_id, output_index);
                    self.push_named("response.output_item.added", json!({"type":"response.output_item.added","output_index":output_index,"item":{"id":item_id,"type":"reasoning","status":"in_progress","summary":[]}}));
                    self.push_named("response.reasoning_summary_part.added", json!({"type":"response.reasoning_summary_part.added","item_id":item_id,"output_index":output_index,"summary_index":0,"part":{"type":"summary_text","text":""}}));
                }
                let output_index = self.reasoning_summaries.get(&index).map(|summary| summary.output_index).unwrap_or(0);
                if let Some(summary) = self.reasoning_summaries.get_mut(&index) {
                    summary.text.push_str(&text);
                }
                let item_id = format!("rs_{}_{}", self.request_id, output_index);
                self.push_named("response.reasoning_summary_text.delta", json!({"type":"response.reasoning_summary_text.delta","item_id":item_id,"output_index":output_index,"summary_index":0,"delta":text}));
            }
            GatewayStreamEvent::ToolCallDelta(delta) => {
                let is_new = !self.tool_calls.contains_key(&delta.index);
                if is_new {
                    let output_index = self.allocate_output_index();
                    let call_id = delta.id.clone().unwrap_or_else(|| format!("call_{}_{}", self.request_id, delta.index));
                    let name = delta.name.clone().unwrap_or_default();
                    self.tool_calls.insert(delta.index, EncodedToolCall { output_index, call_id: call_id.clone(), name: name.clone(), arguments: String::new() });
                    let item_id = format!("fc_{}_{}", self.request_id, output_index);
                    self.push_named("response.output_item.added", json!({"type":"response.output_item.added","output_index":output_index,"item":{"id":item_id,"type":"function_call","call_id":call_id,"name":name,"arguments":"","status":"in_progress"}}));
                }
                let output_index = self.tool_calls.get(&delta.index).map(|call| call.output_index).unwrap_or(0);
                let item_id = format!("fc_{}_{}", self.request_id, output_index);
                if let Some(call) = self.tool_calls.get_mut(&delta.index) {
                    if let Some(id) = delta.id { call.call_id = id; }
                    if let Some(name) = delta.name { call.name = name; }
                    call.arguments.push_str(&delta.arguments_fragment);
                }
                self.push_named("response.function_call_arguments.delta", json!({"type":"response.function_call_arguments.delta","item_id":item_id,"output_index":output_index,"delta":delta.arguments_fragment}));
            }
            GatewayStreamEvent::ReasoningContinuation(encrypted_content) => {
                if self.reasoning_continuation.is_some() {
                    self.fail_response_limit();
                    return;
                }
                let output_index = self.allocate_output_index();
                let item_id = format!("rs_{}_continuation", self.request_id);
                let item = json!({"id":item_id,"type":"reasoning","status":"completed","summary":[],"encrypted_content":encrypted_content});
                self.push_named("response.output_item.added", json!({"type":"response.output_item.added","output_index":output_index,"item":item}));
                self.push_named("response.output_item.done", json!({"type":"response.output_item.done","output_index":output_index,"item":item}));
                self.reasoning_continuation = Some(EncodedReasoningContinuation {
                    output_index,
                    encrypted_content,
                });
            }
            GatewayStreamEvent::AnthropicReasoningContinuation(_) => {
                self.fail_response_limit();
                return;
            }
            GatewayStreamEvent::Completed { finish_reason, usage, .. } => {
                let mut output = Vec::<(u32, Value)>::new();
                if let Some(output_index) = self.message_output_index {
                    let message_id = format!("msg_{}_{}", self.request_id, output_index);
                    let mut content = Vec::new();
                    if self.text_started {
                        self.push_named("response.output_text.done", json!({"type":"response.output_text.done","item_id":message_id,"output_index":output_index,"content_index":0,"text":self.text}));
                        self.push_named("response.content_part.done", json!({"type":"response.content_part.done","item_id":message_id,"output_index":output_index,"content_index":0,"part":{"type":"output_text","text":self.text,"annotations":[]}}));
                        content.push(json!({"type":"output_text","text":self.text,"annotations":[]}));
                    }
                    if self.refusal_started {
                        let content_index = u32::from(self.text_started);
                        self.push_named("response.refusal.done", json!({"type":"response.refusal.done","item_id":message_id,"output_index":output_index,"content_index":content_index,"refusal":self.refusal}));
                        self.push_named("response.content_part.done", json!({"type":"response.content_part.done","item_id":message_id,"output_index":output_index,"content_index":content_index,"part":{"type":"refusal","refusal":self.refusal}}));
                        content.push(json!({"type":"refusal","refusal":self.refusal}));
                    }
                    let item = json!({"id":message_id,"type":"message","role":"assistant","status":"completed","content":content});
                    self.push_named("response.output_item.done", json!({"type":"response.output_item.done","output_index":output_index,"item":item}));
                    output.push((output_index, item));
                }
                let completed_calls = self
                    .tool_calls
                    .iter()
                    .map(|(index, call)| (*index, call.clone()))
                    .collect::<Vec<_>>();
                for (_, call) in completed_calls {
                    let index = call.output_index;
                    let item_id = format!("fc_{}_{}", self.request_id, index);
                    self.push_named("response.function_call_arguments.done", json!({"type":"response.function_call_arguments.done","item_id":item_id,"output_index":index,"arguments":call.arguments}));
                    let item = json!({"id":item_id,"type":"function_call","call_id":call.call_id,"name":call.name,"arguments":call.arguments,"status":"completed"});
                    self.push_named("response.output_item.done", json!({"type":"response.output_item.done","output_index":index,"item":item}));
                    output.push((index, item));
                }
                let completed_reasoning = self.reasoning_summaries.values().cloned().collect::<Vec<_>>();
                for summary in completed_reasoning {
                    let index = summary.output_index;
                    let item_id = format!("rs_{}_{}", self.request_id, index);
                    self.push_named("response.reasoning_summary_text.done", json!({"type":"response.reasoning_summary_text.done","item_id":item_id,"output_index":index,"summary_index":0,"text":summary.text}));
                    self.push_named("response.reasoning_summary_part.done", json!({"type":"response.reasoning_summary_part.done","item_id":item_id,"output_index":index,"summary_index":0,"part":{"type":"summary_text","text":summary.text}}));
                    let item = json!({"id":item_id,"type":"reasoning","status":"completed","summary":[{"type":"summary_text","text":summary.text}]});
                    self.push_named("response.output_item.done", json!({"type":"response.output_item.done","output_index":index,"item":item}));
                    output.push((index, item));
                }
                if let Some(continuation) = self.reasoning_continuation.clone() {
                    output.push((continuation.output_index, json!({
                        "id":format!("rs_{}_continuation", self.request_id),
                        "type":"reasoning","status":"completed","summary":[],
                        "encrypted_content":continuation.encrypted_content
                    })));
                }
                output.sort_by_key(|(index, _)| *index);
                let output = output.into_iter().map(|(_, item)| item).collect::<Vec<_>>();
                let usage_value = usage.as_ref().map(responses_usage).unwrap_or(Value::Null);
                let terminal_event = if finish_reason == FinishReason::Length {
                    "response.incomplete"
                } else {
                    "response.completed"
                };
                let mut response = json!({
                    "id":response_id,"object":"response","created_at":self.created_at,
                    "status":if finish_reason == FinishReason::Length {"incomplete"} else {"completed"},
                    "background":false,"error":null,
                    "incomplete_details":if finish_reason == FinishReason::Length {json!({"reason":"max_output_tokens"})} else {Value::Null},
                    "model":self.alias,"output":output,"previous_response_id":null,"store":false,"usage":usage_value
                });
                self.responses_metadata.apply(&mut response);
                self.push_named(terminal_event, json!({"type":terminal_event,"response":response}));
            }
            GatewayStreamEvent::Failed => self.push_named("response.failed", json!({"type":"response.failed","response":{
                "id":response_id,"object":"response","status":"failed","store":false,"output":[],"error":{"code":"provider_error","message":"The model stream terminated before completion."}
            }})),
        }
    }

    fn encode_anthropic(&mut self, event: GatewayStreamEvent) {
        match event {
            GatewayStreamEvent::MessageStart => {}
            GatewayStreamEvent::Usage(usage) => self.anthropic_usage = Some(usage),
            GatewayStreamEvent::TextDelta(delta) | GatewayStreamEvent::RefusalDelta(delta) => {
                self.ensure_anthropic_started();
                let index = if let Some(index) = self.message_output_index {
                    index
                } else {
                    let index = self.allocate_output_index();
                    self.message_output_index = Some(index);
                    self.pending.push_back(named_sse("content_block_start", json!({
                        "type":"content_block_start","index":index,"content_block":{"type":"text","text":""}
                    })));
                    self.anthropic_open_blocks.insert(index);
                    index
                };
                self.pending.push_back(named_sse("content_block_delta", json!({
                    "type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":delta}
                })));
            }
            GatewayStreamEvent::ToolCallDelta(delta) => {
                self.ensure_anthropic_started();
                let is_new = !self.tool_calls.contains_key(&delta.index);
                if is_new {
                    let output_index = self.allocate_output_index();
                    let call_id = delta.id.clone().unwrap_or_else(|| format!("toolu_{}_{}",self.request_id,delta.index));
                    let name = delta.name.clone().unwrap_or_default();
                    self.tool_calls.insert(delta.index, EncodedToolCall { output_index, call_id: call_id.clone(), name: name.clone(), arguments: String::new() });
                    self.pending.push_back(named_sse("content_block_start", json!({
                        "type":"content_block_start","index":output_index,
                        "content_block":{"type":"tool_use","id":call_id,"name":name,"input":{}}
                    })));
                    self.anthropic_open_blocks.insert(output_index);
                }
                if let Some(call) = self.tool_calls.get_mut(&delta.index) {
                    if let Some(id) = delta.id { call.call_id = id; }
                    if let Some(name) = delta.name { call.name = name; }
                    call.arguments.push_str(&delta.arguments_fragment);
                }
                if !delta.arguments_fragment.is_empty() {
                    let output_index = self.tool_calls.get(&delta.index).map_or(0, |call| call.output_index);
                    self.pending.push_back(named_sse("content_block_delta", json!({
                        "type":"content_block_delta","index":output_index,
                        "delta":{"type":"input_json_delta","partial_json":delta.arguments_fragment}
                    })));
                }
            }
            GatewayStreamEvent::AnthropicReasoningContinuation(state) => {
                self.ensure_anthropic_started();
                self.close_anthropic_blocks();
                let value: Value = match serde_json::from_slice(&state.payload) {
                    Ok(value) => value,
                    Err(_) => {
                        self.pending.push_back(named_sse("error", json!({"type":"error","error":{"type":"api_error","message":"Invalid provider reasoning state."}})));
                        return;
                    }
                };
                let Some(turns) = value.get("turns").and_then(Value::as_array) else {
                    self.pending.push_back(named_sse("error", json!({"type":"error","error":{"type":"api_error","message":"Invalid provider reasoning state."}})));
                    return;
                };
                for block in turns
                    .iter()
                    .filter_map(|turn| turn.get("blocks").and_then(Value::as_array))
                    .flatten()
                {
                    let index = self.allocate_output_index();
                    match block.get("type").and_then(Value::as_str) {
                        Some("reasoning_text") => {
                            let text = block.get("text").and_then(Value::as_str).unwrap_or_default();
                            let signature = block.get("signature").and_then(Value::as_str).unwrap_or_default();
                            self.pending.push_back(named_sse("content_block_start", json!({"type":"content_block_start","index":index,"content_block":{"type":"thinking","thinking":"","signature":""}})));
                            if !text.is_empty() { self.pending.push_back(named_sse("content_block_delta", json!({"type":"content_block_delta","index":index,"delta":{"type":"thinking_delta","thinking":text}}))); }
                            if !signature.is_empty() { self.pending.push_back(named_sse("content_block_delta", json!({"type":"content_block_delta","index":index,"delta":{"type":"signature_delta","signature":signature}}))); }
                        }
                        Some("redacted_content") => {
                            let data = block.get("data").and_then(Value::as_str).unwrap_or_default();
                            self.pending.push_back(named_sse("content_block_start", json!({"type":"content_block_start","index":index,"content_block":{"type":"redacted_thinking","data":data}})));
                        }
                        _ => {
                            self.pending.push_back(named_sse("error", json!({"type":"error","error":{"type":"api_error","message":"Unsupported provider reasoning state."}})));
                            return;
                        }
                    }
                    self.pending.push_back(named_sse("content_block_stop", json!({"type":"content_block_stop","index":index})));
                }
            }
            GatewayStreamEvent::ReasoningSummaryDelta { .. } => {}
            GatewayStreamEvent::ReasoningContinuation(_) => {
                self.pending.push_back(named_sse("error", json!({"type":"error","error":{"type":"api_error","message":"Invalid Anthropic continuation state."}})));
            }
            GatewayStreamEvent::Completed { finish_reason, usage, .. } => {
                let usage = usage.unwrap_or_default();
                self.anthropic_usage = Some(usage.clone());
                self.ensure_anthropic_started();
                self.close_anthropic_blocks();
                self.pending.push_back(named_sse("message_delta", json!({
                    "type":"message_delta","delta":{"stop_reason":anthropic_stop_reason(finish_reason),"stop_sequence":null},
                    "usage":{
                        "input_tokens":usage.input_tokens.unwrap_or(0),
                        "output_tokens":usage.output_tokens.unwrap_or(0),
                        "cache_read_input_tokens":usage.cached_input_tokens.unwrap_or(0)
                    }
                })));
                self.pending.push_back(named_sse("message_stop", json!({"type":"message_stop"})));
            }
            GatewayStreamEvent::Failed => self.pending.push_back(named_sse("error", json!({
                "type":"error","error":{"type":"api_error","message":"The model stream terminated before completion."}
            }))),
        }
    }

    fn ensure_anthropic_started(&mut self) {
        if self.anthropic_started {
            return;
        }
        self.anthropic_started = true;
        let usage = self.anthropic_usage.clone().unwrap_or_default();
        self.pending.push_back(named_sse(
            "message_start",
            json!({
                "type":"message_start","message":{
                    "id":format!("msg_{}",self.request_id),"type":"message","role":"assistant",
                    "model":self.alias,"content":[],"stop_reason":null,"stop_sequence":null,
                    "usage":{
                        "input_tokens":usage.input_tokens.unwrap_or(0),
                        "output_tokens":usage.output_tokens.unwrap_or(0),
                        "cache_read_input_tokens":usage.cached_input_tokens.unwrap_or(0)
                    }
                }
            }),
        ));
    }

    fn close_anthropic_blocks(&mut self) {
        let indices = std::mem::take(&mut self.anthropic_open_blocks);
        for index in indices {
            self.pending.push_back(named_sse(
                "content_block_stop",
                json!({"type":"content_block_stop","index":index}),
            ));
        }
    }

    fn allocate_output_index(&mut self) -> u32 {
        let index = self.next_output_index;
        self.next_output_index = self.next_output_index.saturating_add(1);
        index
    }

    fn ensure_message_output_index(&mut self) -> u32 {
        if let Some(index) = self.message_output_index {
            return index;
        }
        let index = self.allocate_output_index();
        self.message_output_index = Some(index);
        index
    }

    fn push_named(&mut self, name: &str, mut value: serde_json::Value) {
        if self.limit_exceeded {
            return;
        }
        if let Some(object) = value.as_object_mut() {
            object.insert(
                "sequence_number".to_string(),
                Value::from(self.sequence_number),
            );
        }
        self.sequence_number = self.sequence_number.saturating_add(1);
        let frame = named_sse(name, value);
        let Some(total) = self.response_bytes.checked_add(frame.len()) else {
            self.fail_response_limit();
            return;
        };
        if total > self.max_response_bytes {
            self.fail_response_limit();
            return;
        }
        self.response_bytes = total;
        self.pending.push_back(frame);
    }

    fn fail_response_limit(&mut self) {
        self.limit_exceeded = true;
        self.pending.clear();
        self.text.clear();
        self.refusal.clear();
        self.tool_calls.clear();
        self.reasoning_summaries.clear();
        self.reasoning_continuation = None;
        self.anthropic_started = false;
        let response_id = format!("resp_{}", self.request_id);
        self.pending.push_back(named_sse("response.failed", json!({
            "type":"response.failed","sequence_number":self.sequence_number,"response":{
                "id":response_id,"object":"response","status":"failed","store":false,"output":[],
                "error":{"code":"response_too_large","message":"The model response exceeded the gateway output limit."}
            }
        })));
    }
}

fn anthropic_stop_reason(reason: FinishReason) -> &'static str {
    match reason {
        FinishReason::Length => "max_tokens",
        FinishReason::ToolCalls => "tool_use",
        FinishReason::Stop => "end_turn",
        FinishReason::ContentFilter => "refusal",
        FinishReason::Cancelled | FinishReason::Error | FinishReason::Unknown => "end_turn",
    }
}

fn responses_usage(usage: &NormalizedUsage) -> Value {
    json!({
        "input_tokens":usage.input_tokens,
        "output_tokens":usage.output_tokens,
        "total_tokens":usage.input_tokens.zip(usage.output_tokens).map(|(input,output)| input.saturating_add(output)),
        "input_tokens_details":{"cached_tokens":usage.cached_input_tokens},
        "output_tokens_details":{"reasoning_tokens":usage.reasoning_tokens}
    })
}

fn named_sse(name: &str, value: serde_json::Value) -> Bytes {
    Bytes::from(format!("event: {name}\ndata: {value}\n\n"))
}

fn sse_json(value: serde_json::Value) -> Bytes {
    Bytes::from(format!("data: {value}\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroize::Zeroizing;

    #[test]
    fn anthropic_facade_accepts_native_and_bedrock_continuations() {
        assert!(continuation_is_eligible(
            ClientProtocol::AnthropicMessages,
            ProviderProtocol::AnthropicMessages
        ));
        assert!(continuation_is_eligible(
            ClientProtocol::AnthropicMessages,
            ProviderProtocol::BedrockConverse
        ));
        assert!(!continuation_is_eligible(
            ClientProtocol::OpenAiResponses,
            ProviderProtocol::AnthropicMessages
        ));
    }

    #[test]
    fn anthropic_stream_closes_text_before_continuation_and_preserves_usage() {
        let mut encoder = ClientStreamEncoder::new(
            ClientProtocol::AnthropicMessages,
            "request-1".to_string(),
            "claude".to_string(),
            1024 * 1024,
            ResponsesResponseMetadata::default(),
        );
        encoder.encode(GatewayStreamEvent::Usage(NormalizedUsage {
            input_tokens: Some(11),
            output_tokens: Some(0),
            cached_input_tokens: Some(2),
            reasoning_tokens: None,
        }));
        encoder.encode(GatewayStreamEvent::MessageStart);
        encoder.encode(GatewayStreamEvent::TextDelta("answer".to_string()));
        encoder.encode(GatewayStreamEvent::AnthropicReasoningContinuation(
            ProviderContinuationState {
                protocol: ProviderProtocol::BedrockConverse,
                payload: Zeroizing::new(
                    serde_json::to_vec(&json!({
                        "version":1,
                        "turns":[{"blocks":[{
                            "type":"reasoning_text","text":"opaque","signature":"sig"
                        }]}]
                    }))
                    .unwrap(),
                ),
            },
        ));
        encoder.encode(GatewayStreamEvent::Completed {
            finish_reason: FinishReason::Stop,
            usage: Some(NormalizedUsage {
                input_tokens: Some(11),
                output_tokens: Some(7),
                cached_input_tokens: Some(2),
                reasoning_tokens: None,
            }),
            include_usage: true,
        });
        let stream = encoder
            .pending
            .iter()
            .map(|frame| String::from_utf8_lossy(frame).into_owned())
            .collect::<String>();
        let text_stop = stream
            .find("content_block_stop\ndata: {\"index\":0")
            .expect("text stop");
        let thinking_start = stream
            .find("\"content_block\":{\"signature\":\"\",\"thinking\":\"\",\"type\":\"thinking\"}")
            .expect("thinking start");
        assert!(text_stop < thinking_start);
        assert_eq!(
            stream
                .matches("content_block_stop\ndata: {\"index\":0")
                .count(),
            1
        );
        assert!(stream.contains("\"input_tokens\":11"));
        assert!(stream.contains("\"output_tokens\":7"));
        assert!(stream.contains("\"cache_read_input_tokens\":2"));
    }
}
