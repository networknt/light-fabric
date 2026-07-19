use model_provider::inference::{AcceptanceEvidence, NormalizedUsage};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::LlmGatewayError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Price {
    pub version: u64,
    pub input_micros_per_million: u64,
    pub output_micros_per_million: u64,
}

#[derive(Debug, Default)]
pub struct UsageLedger {
    reserved_micros: AtomicU64,
    charged_micros: AtomicU64,
}

impl UsageLedger {
    pub fn reserved(&self) -> u64 {
        self.reserved_micros.load(Ordering::Acquire)
    }
    pub fn charged(&self) -> u64 {
        self.charged_micros.load(Ordering::Acquire)
    }
}

pub struct UsageReservation {
    ledger: Arc<UsageLedger>,
    reserved: u64,
    completed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconciledUsage {
    pub charged_micros: u64,
    pub complete: bool,
    pub price_version: u64,
}

impl UsageReservation {
    pub fn reserve(
        ledger: Arc<UsageLedger>,
        maximum_micros: u64,
        budget_micros: Option<u64>,
    ) -> Result<Self, LlmGatewayError> {
        let previous = ledger
            .reserved_micros
            .fetch_add(maximum_micros, Ordering::AcqRel);
        if budget_micros.is_some_and(|budget| previous.saturating_add(maximum_micros) > budget) {
            ledger
                .reserved_micros
                .fetch_sub(maximum_micros, Ordering::AcqRel);
            return Err(LlmGatewayError::Budget);
        }
        Ok(Self {
            ledger,
            reserved: maximum_micros,
            completed: false,
        })
    }

    pub fn reconcile(
        self,
        price: Price,
        usage: Option<&NormalizedUsage>,
        acceptance: AcceptanceEvidence,
    ) -> ReconciledUsage {
        let reserved = self.reserved;
        self.reconcile_with_ambiguous_bound(price, usage, acceptance, reserved)
    }

    pub fn reconcile_with_ambiguous_bound(
        mut self,
        price: Price,
        usage: Option<&NormalizedUsage>,
        acceptance: AcceptanceEvidence,
        ambiguous_charge_micros: u64,
    ) -> ReconciledUsage {
        let (charged, complete) = match usage {
            Some(usage) if usage.input_tokens.is_some() && usage.output_tokens.is_some() => (
                cost(
                    price,
                    usage.input_tokens.unwrap_or_default(),
                    usage.output_tokens.unwrap_or_default(),
                ),
                true,
            ),
            _ if acceptance == AcceptanceEvidence::NotAccepted => (0, true),
            _ => (ambiguous_charge_micros.min(self.reserved).max(1), false),
        };
        self.ledger
            .reserved_micros
            .fetch_sub(self.reserved, Ordering::AcqRel);
        self.ledger
            .charged_micros
            .fetch_add(charged, Ordering::AcqRel);
        self.completed = true;
        ReconciledUsage {
            charged_micros: charged,
            complete,
            price_version: price.version,
        }
    }
}

impl Drop for UsageReservation {
    fn drop(&mut self) {
        if !self.completed {
            self.ledger
                .reserved_micros
                .fetch_sub(self.reserved, Ordering::AcqRel);
        }
    }
}

pub fn cost(price: Price, input_tokens: u64, output_tokens: u64) -> u64 {
    input_tokens
        .saturating_mul(price.input_micros_per_million)
        .saturating_add(output_tokens.saturating_mul(price.output_micros_per_million))
        .saturating_add(999_999)
        / 1_000_000
}

pub fn worst_case_cost(
    price: Price,
    input_tokens: u64,
    output_tokens: u64,
    attempts: usize,
) -> u64 {
    cost(price, input_tokens, output_tokens).saturating_mul(attempts as u64)
}
