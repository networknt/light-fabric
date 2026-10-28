use crate::{
    plan::{CompiledChain, Terminal},
    *,
};
use futures_util::FutureExt;
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Arc,
};
use tokio::time::{Instant, timeout_at};
use tokio_util::sync::CancellationToken;

/// Immutable shared plan; every execution allocates a fresh exchange and completion guard.
pub struct Pipeline {
    plan: Arc<ExecutionPlan>,
    finalizers: Finalizers,
}
impl Pipeline {
    pub fn new(plan: Arc<ExecutionPlan>, finalizers: Finalizers) -> Self {
        Self { plan, finalizers }
    }
    /// Complete an app-owned local reservation before selecting configured policies.
    /// Uses the same completion guard; never dispatches or runs a second response pass.
    pub fn respond(&self, invocation: Invocation, response: GatewayResponse) -> GatewayResponse {
        let mut guard = CompletionGuard::new(invocation, self.finalizers.clone());
        guard.response = response;
        guard.finalize();
        guard.response.clone()
    }
    pub async fn execute(&self, invocation: Invocation, backend: &dyn Backend) -> GatewayResponse {
        self.execute_prepared(invocation, backend, Ok).await
    }
    /// The trusted transport may bound/replace the completed response before callbacks
    /// observe it. Preparation is synchronous, intrinsically bounded and does not
    /// perform I/O, run policies, dispatch, or receive mutable exchange state.
    /// Provider types and serialization remain in the transport adapter.
    pub async fn execute_prepared<F>(
        &self,
        invocation: Invocation,
        backend: &dyn Backend,
        prepare: F,
    ) -> GatewayResponse
    where
        F: FnOnce(GatewayResponse) -> Result<GatewayResponse, StatusError>,
    {
        self.execute_prepared_with_validator_media(
            invocation,
            backend,
            ValidatorMediaProof::default(),
            prepare,
        )
        .await
    }
    /// Additive trusted ingress path. Legacy execution retains unknown proof.
    pub async fn execute_prepared_with_validator_media<F>(
        &self,
        invocation: Invocation,
        backend: &dyn Backend,
        proof: ValidatorMediaProof,
        prepare: F,
    ) -> GatewayResponse
    where
        F: FnOnce(GatewayResponse) -> Result<GatewayResponse, StatusError>,
    {
        let mut guard =
            CompletionGuard::new_with_validator_media(invocation, self.finalizers.clone(), proof);
        let request_deadline = guard.inv.deadline;
        let response_deadline = guard.inv.response_deadline;
        let cancel = guard.inv.cancel.clone();
        let chain = self.plan.select(&mut guard.inv);
        if let Some(chain) = chain {
            for (slot, (handler, _)) in chain.request.iter().enumerate() {
                if let Some(callback) = handler.prepare_completion(&guard.inv) {
                    guard.state.completion.prepare(slot, callback);
                }
            }
        }
        let request = async {
            budget(&guard.inv, request_deadline)?;
            let Some(chain) = chain else {
                return Err(StatusError::missing_target());
            };
            run_request(
                chain,
                &mut guard.inv,
                &mut guard.state,
                backend,
                request_deadline,
            )
            .await
        };
        let outcome = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(StatusError::timeout()),
            result = timeout_at(request_deadline, AssertUnwindSafe(request).catch_unwind()) => match result {
                Ok(Ok(result)) => result,
                Ok(Err(_)) => Err(StatusError::handler_failure()),
                Err(_) => Err(StatusError::timeout()),
            }
        };
        // Dropping a cancelled/timed-out handler future cannot publish its
        // staged media candidate, including when it never returns to the loop.
        guard.state.end_validator_media_transaction(false);
        guard.inv.deadline = request_deadline;
        guard.inv.response_deadline = response_deadline;
        guard.response = match outcome {
            Ok(response) => response,
            Err(error) => {
                if error.code == "ERR14017" {
                    cancel.cancel();
                }
                guard.state.audit.status = Some(error.clone());
                error.response()
            }
        };
        for name in guard.state.response_headers.keys() {
            for value in guard.state.response_headers.get_all(name) {
                guard.response.headers.append(name, value.clone());
            }
        }
        if let Some(chain) = chain {
            // Request cancellation is deliberately not the response-phase signal.
            // Keep a separate copy so bounded error treatment remains possible.
            let mut response_inv = guard.inv.clone();
            response_inv.cancel = CancellationToken::new();
            let mut candidate = guard.response.clone();
            let result = if Instant::now() >= response_deadline {
                Err(StatusError::timeout())
            } else {
                match timeout_at(response_deadline, async {
                    for h in &chain.response {
                        if Instant::now() >= response_deadline {
                            return Err(StatusError::timeout());
                        }
                        match AssertUnwindSafe(async {
                            h.on_response(&response_inv, &mut guard.state, &mut candidate)
                                .await
                        })
                        .catch_unwind()
                        .await
                        {
                            Ok(Ok(())) => {}
                            _ => return Err(StatusError::handler_failure()),
                        }
                    }
                    if Instant::now() >= response_deadline {
                        Err(StatusError::timeout())
                    } else {
                        Ok(())
                    }
                })
                .await
                {
                    Ok(result) => result,
                    Err(_) => Err(StatusError::timeout()),
                }
            };
            // An inner deadline-aware operation can return an error in the same
            // poll that the outer timer expires. Deadline wins over that failure.
            let result = if Instant::now() >= response_deadline {
                Err(StatusError::timeout())
            } else {
                result
            };
            match result {
                Ok(()) => guard.response = candidate,
                Err(error) => {
                    response_inv.cancel.cancel();
                    // Discard partial response and queued headers. Never rerun policies or dispatch.
                    guard.response = error.response();
                    guard.state.audit.status = Some(error);
                }
            }
        }
        prepare_completion(&mut guard, prepare);
        guard.finalize();
        guard.response.clone()
    }

    /// Local reservations use the same preparation/finalization boundary.
    pub fn respond_prepared<F>(
        &self,
        invocation: Invocation,
        response: GatewayResponse,
        prepare: F,
    ) -> GatewayResponse
    where
        F: FnOnce(GatewayResponse) -> Result<GatewayResponse, StatusError>,
    {
        let mut guard = CompletionGuard::new(invocation, self.finalizers.clone());
        guard.response = response;
        prepare_completion(&mut guard, prepare);
        guard.finalize();
        guard.response.clone()
    }
}

fn prepare_completion<F>(guard: &mut CompletionGuard, prepare: F)
where
    F: FnOnce(GatewayResponse) -> Result<GatewayResponse, StatusError>,
{
    match catch_unwind(AssertUnwindSafe(|| prepare(guard.response.clone()))) {
        Ok(Ok(response)) => guard.response = response,
        result => {
            let error = match result {
                Ok(Err(error)) => error,
                _ => StatusError::handler_failure(),
            };
            guard.response = error.response();
            guard.state.audit.status = Some(error);
        }
    }
}

fn budget(inv: &Invocation, deadline: Instant) -> Result<(), StatusError> {
    if inv.cancel.is_cancelled() || Instant::now() >= deadline {
        Err(StatusError::timeout())
    } else {
        Ok(())
    }
}
async fn run_request(
    chain: &CompiledChain,
    inv: &mut Invocation,
    state: &mut ExchangeState,
    backend: &dyn Backend,
    deadline: Instant,
) -> Result<GatewayResponse, StatusError> {
    let mut initial_response = None;
    for (slot, (h, caps)) in chain.request.iter().enumerate() {
        budget(inv, deadline)?;
        if caps.requires_verified_identity && state.principal.is_none() {
            return Err(StatusError::missing_identity());
        }
        if chain.media_roles[slot] == crate::plan::ValidatorMediaRole::Requires {
            state
                .validator_media_is_json()
                .map_err(|_| StatusError::handler_failure())?;
        }
        state.begin_validator_media_transaction();
        state.completion.enter_policy(slot);
        let result = AssertUnwindSafe(async { h.on_request(inv, state).await })
            .catch_unwind()
            .await;
        state.completion.leave_policy();
        inv.deadline = deadline;
        if let Err(error) = budget(inv, deadline) {
            state.end_validator_media_transaction(false);
            return Err(error);
        }
        let commit_media = matches!(
            &result,
            Ok(Ok(
                HandlerOutcome::Continue | HandlerOutcome::ContinueWithInitialResponse(_)
            ))
        );
        state.end_validator_media_transaction(commit_media);
        match result {
            Ok(Ok(HandlerOutcome::Continue)) => {}
            Ok(Ok(HandlerOutcome::ContinueWithInitialResponse(response))) => {
                // Exchange.setInitialResponse keeps the first successful response.
                // This candidate does not grant permission to skip later policies.
                initial_response.get_or_insert(response);
            }
            Ok(Ok(HandlerOutcome::Respond(response))) => return Ok(response),
            Ok(Ok(HandlerOutcome::Reject(error))) => return Err(error),
            _ => return Err(StatusError::handler_failure()),
        }
    }
    budget(inv, deadline)?;
    match &chain.terminal {
        Terminal::Local(h) => {
            let result = AssertUnwindSafe(async { h.on_request(inv, state).await })
                .catch_unwind()
                .await;
            budget(inv, deadline)?;
            match result {
                Ok(Ok(HandlerOutcome::Respond(response))) => Ok(response),
                Ok(Ok(HandlerOutcome::Reject(error))) => Err(error),
                _ => Err(StatusError::handler_failure()),
            }
        }
        Terminal::Dispatch(routes) => {
            let target = state
                .http_target()
                .cloned()
                .or_else(|| routes.resolve(inv))
                .ok_or_else(|| {
                    StatusError::new(
                        400,
                        "ERR10086",
                        "FAILED_TO_INVOKE_LAMBDA",
                        &format!(
                            "Failed to invoke lambda function {}@{}.",
                            inv.path,
                            inv.method.as_str().to_ascii_lowercase()
                        ),
                    )
                })?;
            budget(inv, deadline)?;
            state.metrics.dispatches += 1;
            let start = Instant::now();
            let result = AssertUnwindSafe(async { backend.dispatch(inv, state, &target).await })
                .catch_unwind()
                .await;
            state.metrics.dispatch_latency = start.elapsed();
            budget(inv, deadline)?;
            match result {
                Ok(Ok(response)) => Ok(initial_response.unwrap_or(response)),
                Ok(Err(error)) => Err(error.0),
                Err(_) => Err(StatusError::handler_failure()),
            }
        }
    }
}

struct CompletionGuard {
    inv: Invocation,
    state: ExchangeState,
    response: GatewayResponse,
    callbacks: Finalizers,
    finalized: bool,
}
impl CompletionGuard {
    fn new(inv: Invocation, callbacks: Finalizers) -> Self {
        Self::new_with_validator_media(inv, callbacks, ValidatorMediaProof::default())
    }
    fn new_with_validator_media(
        inv: Invocation,
        callbacks: Finalizers,
        proof: ValidatorMediaProof,
    ) -> Self {
        let mut state = ExchangeState::default();
        state.commit_validator_media(proof);
        Self {
            inv,
            state,
            response: StatusError::timeout().response(),
            callbacks,
            finalized: false,
        }
    }
    fn finalize(&mut self) {
        if self.finalized {
            return;
        }
        self.finalized = true;
        self.state.end_validator_media_transaction(false);
        self.state.metrics.finalized = true;
        let local_callbacks = self.state.completion.finish().unwrap_or_default();
        // Local callbacks are already reversed. Global callbacks retain reverse
        // registration order and also run when a local callback panics.
        self.callbacks.extend(local_callbacks.into_iter().rev());
        while let Some(callback) = self.callbacks.pop() {
            // Completion observes the immutable prepared outcome. A callback
            // failure cannot retroactively change records already emitted by
            // another callback, and must not prevent the remaining drain.
            let _ = catch_unwind(AssertUnwindSafe(|| {
                callback.finalize(&self.inv, &self.state, &self.response)
            }));
        }
    }
}
impl Drop for CompletionGuard {
    fn drop(&mut self) {
        if !self.finalized {
            // Actual bounded finalization on outer task cancellation; an assertion
            // alone would lose audit/metrics. There can be no HTTP return after abort.
            self.inv.cancel.cancel();
            self.response = StatusError::timeout().response();
            self.state.audit.status = Some(StatusError::timeout());
            self.finalize();
        }
        debug_assert!(self.finalized);
    }
}
