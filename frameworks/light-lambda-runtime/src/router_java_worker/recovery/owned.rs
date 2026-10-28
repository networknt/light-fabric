//! New lifecycle ownership barrier; existing recovery experiments are unchanged.
use super::*;
impl RecoveryController {
    /// Caller must first cancel its lifetime token and prevent new requests.
    /// Wait for any detached preparation owner, remove a published generation,
    /// then wait for all private worker permits to return after actor cleanup.
    /// This is not an assertion that an observed Child::wait succeeded.
    pub async fn shutdown_owned(&self, slots: u32) {
        self.admitted.store(false, Ordering::SeqCst);
        self.invalidate().await;
        let Ok(_compiler) = self.compiler.acquire().await else {
            return;
        };
        self.invalidate().await;
        let _capacity = self.capacity.clone().acquire_many_owned(slots).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn ownership_barrier_waits_for_compiler_and_actor_permits_without_a_child() {
        let capacity = Arc::new(Semaphore::new(1));
        let controller = RecoveryController::new(
            Arc::new(Artifacts {
                java: "/not-executed".into(),
                classpath: vec![],
                pins: vec![],
            }),
            vec![].into(),
            QualificationLimits {
                frame_bytes: 1024,
                input_utf16: 64,
                output_utf16: 64,
                heap_mib: 1,
                stack_kib: 256,
                address_space_bytes: 1 << 20,
            },
            capacity.clone(),
            RecoveryPolicy {
                attempts: 1,
                cooldown: Duration::from_secs(1),
            },
        )
        .unwrap();
        let compiler = controller.compiler.acquire().await.unwrap();
        let actor = capacity.clone().acquire_owned().await.unwrap();
        let shutdown = controller.shutdown_owned(1);
        tokio::pin!(shutdown);
        assert!(futures_util::poll!(&mut shutdown).is_pending());
        drop(compiler);
        assert!(futures_util::poll!(&mut shutdown).is_pending());
        drop(actor);
        shutdown.await;
        assert_eq!(capacity.available_permits(), 1);
        assert!(matches!(
            controller
                .initialize(
                    Instant::now() + Duration::from_secs(1),
                    CancellationToken::new()
                )
                .await,
            Err(RecoveryFailure::Worker(Failure::Artifact))
        ));
        controller.shutdown_owned(1).await;
        assert_eq!(capacity.available_permits(), 1);
    }
}
