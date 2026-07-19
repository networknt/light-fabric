use async_trait::async_trait;
use bytes::Bytes;
use futures_util::StreamExt;
use model_provider::inference::{
    AcceptanceEvidence, FinishReason, InferenceError, InferenceEvent, InferenceRequest,
    NormalizedUsage, ProviderRequestContext,
};
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{LlmRequestContext, LlmRuntime, estimate_tokens, finish_audit};
use crate::admission::fail_fast_permits;
use crate::audit::{AuditFinish, AuditStart};
use crate::error::LlmGatewayError;
use crate::routing::request_capabilities;
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
        mut request: InferenceRequest,
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
        let audit = self
            .audit
            .reserve(
                alias.audit,
                AuditStart {
                    request_id: context.request_id.clone(),
                    principal_id: context.principal_id.clone(),
                    alias: alias.public_name.clone(),
                    generation: root.generation,
                },
            )
            .await?;

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
        if alias.max_attempts != 1 || candidates.len() != 1 {
            let error = LlmGatewayError::InvalidRequest(
                "early streaming requires exactly one eligible deployment and one attempt"
                    .to_string(),
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
        let deployment = candidates[0].clone();
        let reservation = match UsageReservation::reserve(
            Arc::clone(&alias.ledger),
            cost(deployment.price, estimated_input, max_output),
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
        let circuit_permit = match deployment.circuit.acquire_owned(Instant::now()) {
            Ok(permit) => permit,
            Err(error) => {
                let usage =
                    reservation.reconcile(deployment.price, None, AcceptanceEvidence::NotAccepted);
                finish_audit(
                    audit,
                    AuditFinish {
                        terminal: "rejected",
                        attempts: 0,
                        charged_micros: usage.charged_micros,
                        usage_complete: usage.complete,
                    },
                    error.public_status(),
                    error.public_code(),
                )
                .await?;
                return Err(error);
            }
        };
        let provider_permit = match Arc::clone(&deployment.permits).try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                let error = LlmGatewayError::Capacity;
                let usage =
                    reservation.reconcile(deployment.price, None, AcceptanceEvidence::NotAccepted);
                finish_audit(
                    audit,
                    AuditFinish {
                        terminal: "rejected",
                        attempts: 0,
                        charged_micros: usage.charged_micros,
                        usage_complete: usage.complete,
                    },
                    error.public_status(),
                    error.public_code(),
                )
                .await?;
                return Err(error);
            }
        };

        let deadline = tokio::time::Instant::from_std(context.deadline);
        let cancellation = CancellationToken::new();
        let barrier_result = tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                Err(LlmGatewayError::Provider(InferenceError::timeout_before_acceptance()))
            }
            result = self.stream_start_barrier.wait_until_durable(&context.request_id) => result,
        };
        if let Err(error) = barrier_result {
            let usage =
                reservation.reconcile(deployment.price, None, AcceptanceEvidence::NotAccepted);
            finish_audit(
                audit,
                AuditFinish {
                    terminal: "rejected",
                    attempts: 0,
                    charged_micros: usage.charged_micros,
                    usage_complete: usage.complete,
                },
                error.public_status(),
                error.public_code(),
            )
            .await?;
            return Err(error);
        }

        request.model = deployment.model.clone();
        let provider_context = ProviderRequestContext {
            deadline: context.deadline,
            cancellation: cancellation.clone(),
            attempt_id: format!("{}-1", context.request_id),
            trace: Default::default(),
        };
        let provider_result = tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                cancellation.cancel();
                Err(InferenceError::timeout_after_possible_acceptance())
            }
            result = deployment.provider.stream(provider_context, request) => result,
        };
        let provider_stream = match provider_result {
            Ok(stream) => stream,
            Err(error) => {
                circuit_permit.failure(&error, Instant::now());
                let usage = reservation.reconcile(deployment.price, None, error.acceptance);
                let public_error = LlmGatewayError::Provider(error);
                finish_audit(
                    audit,
                    AuditFinish {
                        terminal: "failed",
                        attempts: 1,
                        charged_micros: usage.charged_micros,
                        usage_complete: usage.complete,
                    },
                    public_error.public_status(),
                    public_error.public_code(),
                )
                .await?;
                return Err(public_error);
            }
        };

        let (sender, receiver) = mpsc::channel(root.stream_channel_capacity);
        let producer_cancellation = cancellation.clone();
        let request_id = context.request_id.clone();
        tokio::spawn(async move {
            let _request_permits = request_permits;
            let _provider_permit = provider_permit;
            let mut provider_stream = provider_stream;
            let mut usage: Option<NormalizedUsage> = None;
            let mut finish_reason: Option<FinishReason> = None;
            let mut visible = false;
            let mut completed = false;
            let mut stream_error = None;
            loop {
                let next = tokio::select! {
                    _ = producer_cancellation.cancelled() => {
                        // Poll once after cancellation so a cancellation-aware provider
                        // observes the token and can release its transport immediately.
                        let _ = tokio::time::timeout(
                            Duration::from_millis(10),
                            provider_stream.next(),
                        )
                        .await;
                        stream_error = Some(InferenceError::cancelled());
                        break;
                    }
                    _ = tokio::time::sleep_until(deadline) => {
                        producer_cancellation.cancel();
                        stream_error = Some(InferenceError::timeout_after_possible_acceptance());
                        break;
                    }
                    next = provider_stream.next() => next,
                };
                let Some(next) = next else { break };
                match next {
                    Ok(InferenceEvent::Usage { usage: value }) => {
                        usage = Some(value);
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
                        if let Some(frame) = semantic_frame(&request_id, event) {
                            if let Err(error) =
                                send_frame(&sender, frame, &producer_cancellation, deadline).await
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
                    let terminal_frames =
                        std::iter::once(finish_frame(&request_id, &finish_reason))
                            .chain(usage.as_ref().map(|value| usage_frame(&request_id, value)))
                            .chain(std::iter::once(Bytes::from_static(b"data: [DONE]\n\n")));
                    for frame in terminal_frames {
                        if let Err(error) =
                            send_frame(&sender, frame, &producer_cancellation, deadline).await
                        {
                            producer_cancellation.cancel();
                            stream_error = Some(error);
                            break;
                        }
                        visible = true;
                    }
                    completed = stream_error.is_none();
                } else {
                    stream_error = Some(InferenceError::protocol(
                        "provider stream ended without a terminal event",
                    ));
                }
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
            let _ = finish_audit(
                audit,
                AuditFinish {
                    terminal,
                    attempts: 1,
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
            write_timeout: Duration::from_millis(root.stream_write_timeout_ms),
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
) -> Result<(), InferenceError> {
    tokio::select! {
        _ = cancellation.cancelled() => Err(InferenceError::cancelled()),
        _ = tokio::time::sleep_until(deadline) => {
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

fn finish_frame(
    request_id: &str,
    finish_reason: &model_provider::inference::FinishReason,
) -> Bytes {
    sse_json(json!({
        "id": format!("chatcmpl-{request_id}"),
        "object": "chat.completion.chunk",
        "choices": [{"index":0,"delta":{},"finish_reason":finish_reason}]
    }))
}

fn sse_json(value: serde_json::Value) -> Bytes {
    Bytes::from(format!("data: {value}\n\n"))
}
