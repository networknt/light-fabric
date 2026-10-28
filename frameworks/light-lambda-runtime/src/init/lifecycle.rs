//! Transactional activation: publish only after activation, unwind in reverse.
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct State {
    lifecycles: Vec<Arc<dyn RuntimeLifecycle>>,
    active: AtomicUsize,
    stopped: AtomicBool,
}

impl State {
    async fn stop(&self) {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        for lifecycle in self.lifecycles[..self.active.load(Ordering::SeqCst)]
            .iter()
            .rev()
        {
            // One bad cleanup callback cannot suppress the remaining owners.
            let _ = AssertUnwindSafe(lifecycle.deactivate())
                .catch_unwind()
                .await;
        }
    }
}
struct Owner(Arc<State>);
impl Drop for Owner {
    fn drop(&mut self) {
        if self.0.active.load(Ordering::SeqCst) == 0 {
            return;
        }
        if !self.0.stopped.load(Ordering::SeqCst)
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let state = self.0.clone();
            runtime.spawn(async move {
                state.stop().await;
            });
        }
    }
}
pub(super) async fn publish<T, F, Fut>(
    lifecycles: Vec<Arc<dyn RuntimeLifecycle>>,
    publish: F,
) -> Result<T, InitError>
where
    F: FnOnce(Arc<dyn RuntimeResource>) -> Fut,
    Fut: std::future::Future<Output = Result<T, InitError>>,
{
    let state = Arc::new(State {
        lifecycles,
        active: AtomicUsize::new(0),
        stopped: AtomicBool::new(false),
    });
    let owner = Arc::new(Owner(state.clone()));
    let result = AssertUnwindSafe(async {
        for (index, lifecycle) in state.lifecycles.iter().enumerate() {
            // Include a partially activated/failing lifecycle in rollback.
            state.active.store(index + 1, Ordering::SeqCst);
            lifecycle.activate().await?;
        }
        publish(owner.clone()).await
    })
    .catch_unwind()
    .await
    .unwrap_or(Err(InitError::Panic));
    if result.is_err() {
        let state = state.clone();
        // Dropping the caller's future must not cancel a cleanup already begun.
        let _ = tokio::spawn(async move {
            state.stop().await;
        })
        .await;
    }
    // Success transfers the owner to the Snapshot; cancellation drops it and
    // schedules the same cleanup, while ordinary errors await cleanup above.
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    struct Stage {
        name: &'static str,
        fail: bool,
        panic: bool,
        events: Arc<Mutex<Vec<String>>>,
    }
    #[async_trait]
    impl RuntimeLifecycle for Stage {
        async fn activate(&self) -> Result<(), InitError> {
            self.events
                .lock()
                .unwrap()
                .push(format!("start:{}", self.name));
            assert!(!self.panic, "test-only lifecycle panic");
            if self.fail {
                Err(InitError::Preparation)
            } else {
                Ok(())
            }
        }
        async fn deactivate(&self) {
            self.events
                .lock()
                .unwrap()
                .push(format!("stop:{}", self.name));
        }
    }
    #[tokio::test]
    async fn failed_and_panicking_partial_activation_roll_back_in_reverse_without_publication() {
        for panic in [false, true] {
            let events = Arc::new(Mutex::new(vec![]));
            let stages: Vec<Arc<dyn RuntimeLifecycle>> = [
                Stage {
                    name: "a",
                    fail: false,
                    panic: false,
                    events: events.clone(),
                },
                Stage {
                    name: "b",
                    fail: !panic,
                    panic,
                    events: events.clone(),
                },
            ]
            .into_iter()
            .map(|s| Arc::new(s) as Arc<dyn RuntimeLifecycle>)
            .collect();
            let published = AtomicBool::new(false);
            let result = publish(stages, |_| async {
                published.store(true, Ordering::SeqCst);
                Ok::<_, InitError>(())
            })
            .await;
            assert!(result.is_err());
            assert!(!published.load(Ordering::SeqCst));
            assert_eq!(
                *events.lock().unwrap(),
                ["start:a", "start:b", "stop:b", "stop:a"]
            );
        }
    }
    #[tokio::test]
    async fn cancellation_during_rollback_does_not_cancel_cleanup() {
        struct Slow {
            entered: tokio::sync::Notify,
            release: tokio::sync::Notify,
            finished: tokio::sync::Notify,
        }
        #[async_trait]
        impl RuntimeLifecycle for Slow {
            async fn activate(&self) -> Result<(), InitError> {
                Ok(())
            }
            async fn deactivate(&self) {
                self.entered.notify_one();
                self.release.notified().await;
                self.finished.notify_one();
            }
        }
        let stage = Arc::new(Slow {
            entered: Default::default(),
            release: Default::default(),
            finished: Default::default(),
        });
        let owned = stage.clone();
        let task = tokio::spawn(async move {
            publish(vec![owned], |_| async {
                Err::<(), _>(InitError::Preparation)
            })
            .await
        });
        stage.entered.notified().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        stage.release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), stage.finished.notified())
            .await
            .unwrap();
    }
}
