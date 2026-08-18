use async_trait::async_trait;
use std::collections::HashMap;

use crate::config::RuntimeConfig;
use crate::runtime::RuntimeError;
use crate::{AdmissionGate, LifecycleRegistrar, ShutdownContext};
use tokio_util::sync::CancellationToken;

#[derive(Debug)]
pub struct BoundTransport<H> {
    pub handle: H,
    pub metadata: ResolvedServerMetadata,
}

#[derive(Debug, Clone, Default)]
pub struct ResolvedServerMetadata {
    pub protocol: String,
    pub address: String,
    pub port: u16,
    pub tags: HashMap<String, String>,
}

#[async_trait]
pub trait TransportRuntime: Send + Sync {
    type Handle: Send + Sync;

    async fn bind(
        &self,
        config: &RuntimeConfig,
        lifecycle: &LifecycleRegistrar,
        admission: &AdmissionGate,
        startup_cancel: CancellationToken,
    ) -> Result<BoundTransport<Self::Handle>, RuntimeError>;

    async fn stop(
        &self,
        handle: &mut Self::Handle,
        context: &ShutdownContext,
    ) -> Result<(), RuntimeError>;
}
