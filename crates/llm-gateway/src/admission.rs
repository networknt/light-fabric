use std::sync::Arc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::error::LlmGatewayError;

#[derive(Debug)]
pub struct PermitSet {
    _global: OwnedSemaphorePermit,
    _principal: OwnedSemaphorePermit,
    _alias: OwnedSemaphorePermit,
}

pub fn fail_fast_permits(
    global: &Arc<Semaphore>,
    principal: &Arc<Semaphore>,
    alias: &Arc<Semaphore>,
) -> Result<PermitSet, LlmGatewayError> {
    let global = Arc::clone(global)
        .try_acquire_owned()
        .map_err(|_| LlmGatewayError::Capacity)?;
    let principal = Arc::clone(principal)
        .try_acquire_owned()
        .map_err(|_| LlmGatewayError::Capacity)?;
    let alias = Arc::clone(alias)
        .try_acquire_owned()
        .map_err(|_| LlmGatewayError::Capacity)?;
    Ok(PermitSet {
        _global: global,
        _principal: principal,
        _alias: alias,
    })
}
