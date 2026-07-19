use model_provider::inference::{
    ContentBlock, InferenceError, InferenceErrorCategory, InferenceRequest, RetryDisposition,
};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::error::LlmGatewayError;

#[derive(Debug)]
pub struct CircuitPermit<'a> {
    circuit: &'a PassiveCircuit,
    half_open: bool,
}

impl CircuitPermit<'_> {
    pub fn success(self) {
        self.circuit.success();
    }

    pub fn failure(self, error: &InferenceError, now: Instant) {
        self.circuit.failure(error, now);
    }
}

impl Drop for CircuitPermit<'_> {
    fn drop(&mut self) {
        if self.half_open {
            self.circuit.probe_active.store(false, Ordering::Release);
        }
    }
}

#[derive(Debug)]
pub struct PassiveCircuit {
    threshold: u32,
    cooldown: Duration,
    epoch: Instant,
    failures: AtomicU32,
    open_until_ms: AtomicU64,
    probe_active: AtomicBool,
}

impl PassiveCircuit {
    pub fn new(threshold: u32, cooldown: Duration) -> Self {
        Self {
            threshold: threshold.max(1),
            cooldown,
            epoch: Instant::now(),
            failures: AtomicU32::new(0),
            open_until_ms: AtomicU64::new(0),
            probe_active: AtomicBool::new(false),
        }
    }

    pub fn acquire(&self, now: Instant) -> Result<CircuitPermit<'_>, LlmGatewayError> {
        let until = self.open_until_ms.load(Ordering::Acquire);
        if until == 0 {
            return Ok(CircuitPermit {
                circuit: self,
                half_open: false,
            });
        }
        if elapsed_ms(self.epoch, now) < until {
            return Err(LlmGatewayError::ProviderUnavailable);
        }
        match self
            .probe_active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => Ok(CircuitPermit {
                circuit: self,
                half_open: true,
            }),
            Err(_) => Err(LlmGatewayError::ProviderUnavailable),
        }
    }

    pub fn success(&self) {
        self.failures.store(0, Ordering::Release);
        self.open_until_ms.store(0, Ordering::Release);
        self.probe_active.store(false, Ordering::Release);
    }

    pub fn failure(&self, error: &InferenceError, now: Instant) {
        if !circuit_failure(error) {
            return;
        }
        let failures = self.failures.fetch_add(1, Ordering::AcqRel) + 1;
        if failures >= self.threshold || self.open_until_ms.load(Ordering::Acquire) != 0 {
            let retry_after = error
                .retry_after_ms
                .map(Duration::from_millis)
                .unwrap_or(self.cooldown)
                .max(self.cooldown);
            let until = elapsed_ms(self.epoch, now)
                .saturating_add(retry_after.as_millis().min(u64::MAX as u128) as u64)
                .max(1);
            self.open_until_ms.store(until, Ordering::Release);
            self.probe_active.store(false, Ordering::Release);
        }
    }
}

fn elapsed_ms(epoch: Instant, now: Instant) -> u64 {
    now.saturating_duration_since(epoch)
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn circuit_failure(error: &InferenceError) -> bool {
    matches!(
        error.category,
        InferenceErrorCategory::RateLimited
            | InferenceErrorCategory::ProviderOverload
            | InferenceErrorCategory::Network
            | InferenceErrorCategory::TimeoutBeforeAcceptance
            | InferenceErrorCategory::TimeoutAfterPossibleAcceptance
    )
}

pub fn retryable(error: &InferenceError) -> bool {
    error.retry == RetryDisposition::Safe
}

pub fn request_capabilities(request: &InferenceRequest) -> (bool, bool, bool) {
    let images = request
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .any(|content| matches!(content, ContentBlock::Image { .. }));
    (
        images,
        !request.tools.is_empty(),
        request.response_format.is_some(),
    )
}
