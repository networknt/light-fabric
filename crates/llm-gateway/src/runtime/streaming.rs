use async_trait::async_trait;
use bytes::Bytes;
use futures_util::StreamExt;
use model_provider::inference::{
    AcceptanceEvidence, FinishReason, InferenceError, InferenceErrorCategory, InferenceEvent,
    InferenceRequest, NormalizedUsage, ProviderRequestContext,
};
use serde_json::json;
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
use crate::routing::{request_capabilities, retryable};
use crate::usage::{UsageReservation, cost};

#[async_trait]
pub trait StreamStartBarrier: Send + Sync {
    async fn wait_until_durable(&self, request_id: &str) -> Result<(), LlmGatewayError>;
}

pub struct ImmediateStreamStartBarrier;

#[async_trait]
impl StreamStartBarrier for ImmediateStreamStartBarrier {
    async fn wait_until_durable(&self, _request_id: &str) -> Result<(), LlmGatewayError> {
        Ok(())
    }
}

pub struct LlmStreamExecution {
    receiver: mpsc::Receiver<Bytes>,
    cancellation: CancellationToken,
    pub request_id: String,
    pub alias: String,
    pub generation: u64,
    pub write_timeout: Duration,
    pub minimum_drain_bytes_per_second: u64,
    pub drain_grace: Duration,
}

impl LlmStreamExecution {
    pub async fn next_frame(&mut self) -> Option<Bytes> {
        self.receiver.recv().await
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
        mut request: InferenceRequest,
        client_include_usage: bool,
    ) -> Result<LlmStreamExecution, LlmGatewayError> {
        if context.deadline <= Instant::now() {
            return Err(LlmGatewayError::Provider(
                InferenceError::timeout_before_acceptance(),
            ));
        }
        let alias = root
            .aliases
            .get(&request.model)
            .ok_or(LlmGatewayError::ModelUnavailable)?
            .clone();
        if alias.internal && alias.bound_principal.as_deref() != Some(context.principal_id.as_str())
        {
            return Err(LlmGatewayError::ModelUnavailable);
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
                    generation: root.generation,
                    snapshot_digest: root.digest.clone(),
                    max_attempts: alias.max_attempts,
                    pii_profile: pii.profile_id(),
                },
            )
            .await?;

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

        let candidates = alias
            .deployments
            .iter()
            .filter(|deployment| deployment.supports(request_capabilities(&request)))
            .cloned()
            .collect::<Vec<_>>();
        let Some(first_price) = candidates.first().map(|candidate| candidate.price) else {
            finish_audit(audit, rejected_finish(), 404, "model_not_found").await?;
            return Err(LlmGatewayError::ModelUnavailable);
        };
        let envelope = candidates
            .iter()
            .take(alias.max_attempts)
            .map(|deployment| cost(deployment.price, estimated_input, max_output))
            .fold(0_u64, u64::saturating_add);
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

        let deadline = tokio::time::Instant::from_std(context.deadline);
        let setup_deadline = deadline
            .min(tokio::time::Instant::now() + Duration::from_millis(root.stream_setup_timeout_ms));
        let cancellation = CancellationToken::new();
        let idle_timeout = Duration::from_millis(root.stream_idle_timeout_ms);
        let progress_timeout = Duration::from_millis(root.stream_write_timeout_ms);
        let mut attempts = 0_usize;
        let mut attempted_envelope = 0_u64;
        let mut last_error = None;
        let mut selected = None;

        for deployment in candidates.into_iter().take(alias.max_attempts) {
            if context.deadline <= Instant::now() {
                last_error = Some(InferenceError::timeout_before_acceptance());
                break;
            }
            let circuit_permit = match deployment.circuit.acquire_owned(Instant::now()) {
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
                })
                .await
            {
                let usage =
                    reservation.reconcile(deployment.price, None, AcceptanceEvidence::NotAccepted);
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
                deployment.price,
                estimated_input,
                max_output,
            ));

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
                        deployment.price,
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
                let usage =
                    reservation.reconcile(deployment.price, None, AcceptanceEvidence::NotAccepted);
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
            let attempt_cancellation = cancellation.child_token();
            let provider_context = ProviderRequestContext {
                deadline: context.deadline,
                cancellation: attempt_cancellation.clone(),
                attempt_id: format!("{}-{attempts}", context.request_id),
                trace: Default::default(),
            };
            let stream_result = tokio::select! {
                _ = tokio::time::sleep_until(setup_deadline) => {
                    attempt_cancellation.cancel();
                    Err(InferenceError::timeout_after_possible_acceptance())
                }
                result = deployment.provider.stream(provider_context, provider_request) => result,
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
                                    deployment.price,
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
                            let can_retry = retryable(&error) && attempts < alias.max_attempts;
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
                            deployment.price,
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
                    let can_retry = retryable(&error) && attempts < alias.max_attempts;
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
        };

        let (sender, receiver) = mpsc::channel(root.stream_channel_capacity);
        let producer_cancellation = cancellation.clone();
        let request_id = context.request_id.clone();
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
                    Ok(InferenceEvent::Usage { usage: value }) => usage = Some(value),
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
                        if let Some(frame) =
                            event.and_then(|event| semantic_frame(&request_id, event))
                        {
                            if let Err(error) = send_frame(
                                &sender,
                                frame,
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
                                if let Some(frame) = semantic_frame(&request_id, event) {
                                    if let Err(error) = send_frame(
                                        &sender,
                                        frame,
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
                        let terminal_frames =
                            std::iter::once(finish_frame(&request_id, &finish_reason))
                                .chain(
                                    client_include_usage
                                        .then_some(usage.as_ref())
                                        .flatten()
                                        .map(|value| usage_frame(&request_id, value)),
                                )
                                .chain(std::iter::once(Bytes::from_static(b"data: [DONE]\n\n")));
                        for frame in terminal_frames {
                            if let Err(error) = send_frame(
                                &sender,
                                frame,
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
                        completed = stream_error.is_none();
                    }
                } else {
                    stream_error = Some(InferenceError::protocol(
                        "provider stream ended without a terminal event",
                    ));
                }
            }

            if stream_error.is_some() && !producer_cancellation.is_cancelled() {
                let _ = send_frame(
                    &sender,
                    stream_error_frame(),
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
            let reconciled = reservation.reconcile(deployment.price, usage.as_ref(), acceptance);
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

async fn send_frame(
    sender: &mpsc::Sender<Bytes>,
    frame: Bytes,
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
        result = sender.send(frame) => result.map_err(|_| InferenceError::cancelled()),
    }
}

fn semantic_frame(request_id: &str, event: InferenceEvent) -> Option<Bytes> {
    let value = match event {
        InferenceEvent::MessageStart { .. } => json!({
            "id": format!("chatcmpl-{request_id}"),
            "object": "chat.completion.chunk",
            "choices": [{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]
        }),
        InferenceEvent::TextDelta { text } => json!({
            "id": format!("chatcmpl-{request_id}"),
            "object": "chat.completion.chunk",
            "choices": [{"index":0,"delta":{"content":text},"finish_reason":null}]
        }),
        InferenceEvent::ToolCallDelta { delta } => json!({
            "id": format!("chatcmpl-{request_id}"),
            "object": "chat.completion.chunk",
            "choices": [{"index":0,"delta":{"tool_calls":[{
                "index":delta.index,"id":delta.id,"type":"function",
                "function":{"name":delta.name,"arguments":delta.arguments_fragment}
            }]},"finish_reason":null}]
        }),
        InferenceEvent::Usage { .. } | InferenceEvent::MessageEnd { .. } => return None,
    };
    Some(sse_json(value))
}

fn usage_frame(request_id: &str, usage: &NormalizedUsage) -> Bytes {
    sse_json(json!({
        "id": format!("chatcmpl-{request_id}"),
        "object": "chat.completion.chunk",
        "choices": [],
        "usage": {
            "prompt_tokens": usage.input_tokens,
            "completion_tokens": usage.output_tokens,
            "total_tokens": usage.input_tokens.zip(usage.output_tokens)
                .map(|(input, output)| input.saturating_add(output))
        }
    }))
}

fn finish_frame(request_id: &str, finish_reason: &FinishReason) -> Bytes {
    sse_json(json!({
        "id": format!("chatcmpl-{request_id}"),
        "object": "chat.completion.chunk",
        "choices": [{"index":0,"delta":{},"finish_reason":finish_reason}]
    }))
}

fn stream_error_frame() -> Bytes {
    sse_json(json!({
        "error": {
            "message": "The model stream terminated before completion.",
            "type": "provider_error",
            "code": "provider_error"
        }
    }))
}

fn sse_json(value: serde_json::Value) -> Bytes {
    Bytes::from(format!("data: {value}\n\n"))
}
