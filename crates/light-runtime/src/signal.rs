use crate::ShutdownReason;

/// Eagerly installed process-shutdown signal streams.
///
/// Installation requires an active Tokio reactor. Call [`Self::install`] as
/// the first lifecycle statement inside an async `#[tokio::main]` body.
pub struct ShutdownWatcher {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
    #[cfg(not(unix))]
    interrupt: tokio::signal::windows::CtrlC,
}

impl ShutdownWatcher {
    /// Installs all supported signal streams immediately.
    ///
    /// # Panics
    ///
    /// Tokio panics if this is called outside an active runtime reactor.
    pub fn install() -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            Ok(Self {
                interrupt: tokio::signal::unix::signal(
                    tokio::signal::unix::SignalKind::interrupt(),
                )?,
                terminate: tokio::signal::unix::signal(
                    tokio::signal::unix::SignalKind::terminate(),
                )?,
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {
                interrupt: tokio::signal::windows::ctrl_c()?,
            })
        }
    }

    pub async fn recv(&mut self) -> ShutdownReason {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = self.interrupt.recv() => ShutdownReason::Interrupt,
                _ = self.terminate.recv() => ShutdownReason::Terminate,
            }
        }
        #[cfg(not(unix))]
        {
            let _ = self.interrupt.recv().await;
            ShutdownReason::Interrupt
        }
    }
}
