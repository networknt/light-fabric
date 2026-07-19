mod compiler;
mod snapshot;
mod store;
mod streaming;

pub use compiler::{CompileProbe, LlmCompiler};
pub use snapshot::{
    AliasPlan, DeploymentRuntime, LlmPublishedSnapshot, PrincipalPermitStripes,
    ProviderAccountRuntime,
};
pub use store::{LlmSnapshotStore, PublishOutcome};
pub use streaming::{ImmediateStreamStartBarrier, LlmStreamExecution, StreamStartBarrier};

use crate::admission::fail_fast_permits;
use crate::audit::{AuditAdmission, AuditFinish, AuditReservation, AuditStart};
use crate::error::LlmGatewayError;
use crate::routing::{request_capabilities, retryable};
use crate::usage::{ReconciledUsage, UsageReservation, cost};
use model_provider::inference::{
    AcceptanceEvidence, InferenceRequest, InferenceResponse, ProviderFormat, ProviderRequestContext,
};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct LlmRequestContext {
    pub request_id: String,
    pub principal_id: String,
    pub deadline: Instant,
}

impl LlmRequestContext {
    pub fn with_timeout(principal_id: impl Into<String>, timeout: Duration) -> Self {
        Self {
            request_id: Uuid::now_v7().to_string(),
            principal_id: principal_id.into(),
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
}

pub struct LlmRuntime {
    store: Arc<LlmSnapshotStore>,
    audit: Arc<dyn AuditAdmission>,
    global_permits: Arc<Semaphore>,
    stream_permits: Arc<Semaphore>,
    stream_start_barrier: Arc<dyn StreamStartBarrier>,
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
        }
    }

    pub fn with_stream_start_barrier(mut self, barrier: Arc<dyn StreamStartBarrier>) -> Self {
        self.stream_start_barrier = barrier;
        self
    }

    pub fn snapshot(&self) -> Arc<LlmPublishedSnapshot> {
        self.store.load()
    }

    pub fn publish(&self, candidate: LlmPublishedSnapshot) -> PublishOutcome {
        self.store.publish(candidate)
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
        mut request: InferenceRequest,
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
            .ok_or(LlmGatewayError::ModelUnavailable)?
            .clone();
        if alias.internal && alias.bound_principal.as_deref() != Some(context.principal_id.as_str())
        {
            return Err(LlmGatewayError::ModelUnavailable);
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

        let required = request_capabilities(&request);
        let candidates = alias
            .deployments
            .iter()
            .filter(|deployment| deployment.supports(required))
            .cloned()
            .collect::<Vec<_>>();
        let Some(first_price) = candidates.first().map(|candidate| candidate.price) else {
            let error = LlmGatewayError::ModelUnavailable;
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
        let envelope = candidates
            .iter()
            .take(alias.max_attempts)
            .map(|candidate| cost(candidate.price, estimated_input, max_output))
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
        for deployment in candidates.into_iter().take(alias.max_attempts) {
            if context.deadline <= Instant::now() {
                last_error =
                    Some(model_provider::inference::InferenceError::timeout_before_acceptance());
                break;
            }
            let circuit_permit = match deployment.circuit.acquire(Instant::now()) {
                Ok(permit) => permit,
                Err(_) => continue,
            };
            let _provider_permit = match Arc::clone(&deployment.permits).try_acquire_owned() {
                Ok(permit) => permit,
                Err(_) => continue,
            };
            attempts += 1;
            attempted_envelope = attempted_envelope.saturating_add(cost(
                deployment.price,
                estimated_input,
                max_output,
            ));
            request.model = deployment.model.clone();
            let provider_context = ProviderRequestContext {
                deadline: context.deadline,
                cancellation: tokio_util::sync::CancellationToken::new(),
                attempt_id: format!("{}-{attempts}", context.request_id),
                trace: Default::default(),
            };
            match deployment
                .provider
                .infer(provider_context, request.clone())
                .await
            {
                Ok(response) => {
                    circuit_permit.success();
                    let usage = reservation.reconcile(
                        deployment.price,
                        response.usage.as_ref(),
                        AcceptanceEvidence::Accepted,
                    );
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
                    });
                }
                Err(error) => {
                    circuit_permit.failure(&error, Instant::now());
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
            detail: "no deployment is currently available".to_string(),
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
        Err(public_error)
    }

    pub fn eligible_formats(
        &self,
        root: &LlmPublishedSnapshot,
        principal: &str,
        request: &InferenceRequest,
    ) -> Result<BTreeSet<ProviderFormat>, LlmGatewayError> {
        let alias = root
            .aliases
            .get(&request.model)
            .ok_or(LlmGatewayError::ModelUnavailable)?;
        if alias.internal && alias.bound_principal.as_deref() != Some(principal) {
            return Err(LlmGatewayError::ModelUnavailable);
        }
        let required = request_capabilities(request);
        let formats = alias
            .deployments
            .iter()
            .filter(|deployment| deployment.supports(required))
            .map(|deployment| deployment.provider.format())
            .collect::<BTreeSet<_>>();
        if formats.is_empty() {
            return Err(LlmGatewayError::ModelUnavailable);
        }
        Ok(formats)
    }

    pub fn visible_models(&self) -> Vec<String> {
        self.store
            .load()
            .aliases
            .values()
            .filter(|alias| !alias.internal)
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
