use model_provider::inference::{AcceptanceEvidence, NormalizedUsage};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::LlmGatewayError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GenerationPrice {
    pub version: u64,
    pub input_micros_per_million: u64,
    pub output_micros_per_million: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EmbeddingPrice {
    pub version: u64,
    pub input_micros_per_million: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum OperationPrice {
    Generate(GenerationPrice),
    Embed(EmbeddingPrice),
}

impl OperationPrice {
    pub fn version(self) -> u64 {
        match self {
            Self::Generate(value) => value.version,
            Self::Embed(value) => value.version,
        }
    }
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
        price: GenerationPrice,
        usage: Option<&NormalizedUsage>,
        acceptance: AcceptanceEvidence,
    ) -> ReconciledUsage {
        let reserved = self.reserved;
        self.reconcile_with_ambiguous_bound(price, usage, acceptance, reserved)
    }

    pub fn reconcile_with_ambiguous_bound(
        mut self,
        price: GenerationPrice,
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

    pub fn reconcile_embedding(
        mut self,
        price: EmbeddingPrice,
        usage: Option<&NormalizedUsage>,
        acceptance: AcceptanceEvidence,
        ambiguous_charge_micros: u64,
    ) -> Result<ReconciledUsage, LlmGatewayError> {
        if usage
            .and_then(|value| value.output_tokens)
            .is_some_and(|tokens| tokens != 0)
        {
            return Err(LlmGatewayError::Invariant(
                "embedding usage reported output tokens".to_string(),
            ));
        }
        let (charged, complete) = match usage {
            Some(usage) if usage.input_tokens.is_some() => (
                cost_embedding(price, usage.input_tokens.unwrap_or_default(), 0)?,
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
        Ok(ReconciledUsage {
            charged_micros: charged,
            complete,
            price_version: price.version,
        })
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

pub fn cost(price: GenerationPrice, input_tokens: u64, output_tokens: u64) -> u64 {
    input_tokens
        .saturating_mul(price.input_micros_per_million)
        .saturating_add(output_tokens.saturating_mul(price.output_micros_per_million))
        .saturating_add(999_999)
        / 1_000_000
}

pub fn cost_embedding(
    price: EmbeddingPrice,
    input_tokens: u64,
    output_tokens: u64,
) -> Result<u64, LlmGatewayError> {
    if output_tokens != 0 {
        return Err(LlmGatewayError::Invariant(
            "embedding usage reported output tokens".to_string(),
        ));
    }
    let scaled = input_tokens
        .checked_mul(price.input_micros_per_million)
        .and_then(|value| value.checked_add(999_999))
        .ok_or_else(|| {
            LlmGatewayError::Invariant("embedding cost arithmetic overflow".to_string())
        })?;
    Ok(scaled / 1_000_000)
}

pub fn worst_case_cost(
    price: GenerationPrice,
    input_tokens: u64,
    output_tokens: u64,
    attempts: usize,
) -> u64 {
    cost(price, input_tokens, output_tokens).saturating_mul(attempts as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedding_price_has_no_output_rate_and_rejects_output_usage() {
        let price = EmbeddingPrice {
            version: 3,
            input_micros_per_million: 2_000_000,
        };
        assert_eq!(cost_embedding(price, 4, 0).unwrap(), 8);
        assert!(matches!(
            cost_embedding(price, 4, 1),
            Err(LlmGatewayError::Invariant(_))
        ));
        assert!(matches!(
            cost_embedding(
                EmbeddingPrice {
                    version: 3,
                    input_micros_per_million: u64::MAX,
                },
                2,
                0,
            ),
            Err(LlmGatewayError::Invariant(_))
        ));
        let wire = serde_json::to_value(OperationPrice::Embed(price)).unwrap();
        assert_eq!(wire["operation"], "embed");
        assert!(wire.get("outputMicrosPerMillion").is_none());
    }
}
