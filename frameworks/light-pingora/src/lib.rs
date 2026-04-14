use async_trait::async_trait;
use light_runtime::{BoundTransport, RuntimeConfig, RuntimeError, TransportRuntime};

pub trait PingoraApp: Send + Sync + 'static {}

pub struct PingoraTransport<A>
where
    A: PingoraApp,
{
    _app: A,
}

impl<A> PingoraTransport<A>
where
    A: PingoraApp,
{
    pub fn new(app: A) -> Self {
        Self { _app: app }
    }
}

#[async_trait]
impl<A> TransportRuntime for PingoraTransport<A>
where
    A: PingoraApp,
{
    type Handle = ();

    async fn bind(
        &self,
        _config: &RuntimeConfig,
    ) -> Result<BoundTransport<Self::Handle>, RuntimeError> {
        Err(RuntimeError::Unsupported(
            "light-pingora transport is not implemented yet".to_string(),
        ))
    }

    async fn stop(&self, _handle: &mut Self::Handle) -> Result<(), RuntimeError> {
        Ok(())
    }
}
