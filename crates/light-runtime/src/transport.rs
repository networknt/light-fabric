use async_trait::async_trait;
use std::collections::HashMap;

use crate::config::RuntimeConfig;
use crate::runtime::RuntimeError;

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
    ) -> Result<BoundTransport<Self::Handle>, RuntimeError>;

    async fn stop(&self, handle: &mut Self::Handle) -> Result<(), RuntimeError>;
}
