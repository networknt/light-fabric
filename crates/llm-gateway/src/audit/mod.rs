use async_trait::async_trait;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::config::AuditMode;
use crate::error::LlmGatewayError;

#[derive(Debug, Clone)]
pub struct AuditStart {
    pub request_id: String,
    pub principal_id: String,
    pub alias: String,
    pub generation: u64,
}

#[derive(Debug, Clone)]
pub struct AuditFinish {
    pub terminal: &'static str,
    pub attempts: usize,
    pub charged_micros: u64,
    pub usage_complete: bool,
}

#[async_trait]
pub trait AuditReservation: Send + Sync {
    async fn finish(self: Box<Self>, finish: AuditFinish) -> Result<(), LlmGatewayError>;
}

#[async_trait]
pub trait AuditAdmission: Send + Sync {
    async fn reserve(
        &self,
        mode: AuditMode,
        start: AuditStart,
    ) -> Result<Box<dyn AuditReservation>, LlmGatewayError>;
}

#[derive(Default)]
pub struct DisabledAudit;

struct DisabledReservation;

#[async_trait]
impl AuditReservation for DisabledReservation {
    async fn finish(self: Box<Self>, _finish: AuditFinish) -> Result<(), LlmGatewayError> {
        Ok(())
    }
}

#[async_trait]
impl AuditAdmission for DisabledAudit {
    async fn reserve(
        &self,
        mode: AuditMode,
        _start: AuditStart,
    ) -> Result<Box<dyn AuditReservation>, LlmGatewayError> {
        if mode != AuditMode::Disabled {
            return Err(LlmGatewayError::AuditUnavailable);
        }
        Ok(Box::new(DisabledReservation))
    }
}

pub fn disabled_audit() -> Arc<dyn AuditAdmission> {
    Arc::new(DisabledAudit)
}

#[derive(Default)]
struct AuditCounters {
    reserved: AtomicU64,
    finished: AtomicU64,
}

#[derive(Clone, Default)]
pub struct ProcessAudit {
    counters: Arc<AuditCounters>,
}

impl ProcessAudit {
    pub fn reserved(&self) -> u64 {
        self.counters.reserved.load(Ordering::Acquire)
    }
    pub fn finished(&self) -> u64 {
        self.counters.finished.load(Ordering::Acquire)
    }
}

struct ProcessReservation {
    counters: Arc<AuditCounters>,
}

#[async_trait]
impl AuditReservation for ProcessReservation {
    async fn finish(self: Box<Self>, _finish: AuditFinish) -> Result<(), LlmGatewayError> {
        self.counters.finished.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

#[async_trait]
impl AuditAdmission for ProcessAudit {
    async fn reserve(
        &self,
        mode: AuditMode,
        _start: AuditStart,
    ) -> Result<Box<dyn AuditReservation>, LlmGatewayError> {
        if mode == AuditMode::Durable {
            return Err(LlmGatewayError::AuditUnavailable);
        }
        self.counters.reserved.fetch_add(1, Ordering::AcqRel);
        Ok(Box::new(ProcessReservation {
            counters: Arc::clone(&self.counters),
        }))
    }
}
