//! Blocking work lifetime mechanism only. WP12 must validate each real action's
//! independent input/step bounds. Tokio cannot terminate a running blocking closure.
use std::{
    num::NonZeroUsize,
    sync::{Arc, OnceLock},
};
use tokio::{
    sync::Semaphore,
    time::{Instant, timeout_at},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockingError {
    Deadline,
    InvalidBounds,
    StepLimit,
    WorkFailed,
}

/// Trusted implementations must terminate within their declared step/input limits,
/// use owned isolated input/results, and never capture exchange state or shared
/// mutable response state. Arbitrary/unbounded rule actions cannot implement the
/// production adapter without WP12 validation. This API is not a sandbox.
pub trait BoundedWork: Send + 'static {
    type Output: Send + 'static;
    fn input_bytes(&self) -> usize;
    /// One independently bounded unit. The executor, not the action, owns the
    /// step loop. A unit must not block indefinitely; WP12 validates real units.
    fn step(&mut self) -> Result<WorkStep<Self::Output>, BlockingError>;
}
pub enum WorkStep<T> {
    Pending,
    Ready(T),
}
struct WorkBudget {
    remaining: usize,
    deadline: Instant,
}
impl WorkBudget {
    /// A work unit must itself have a finite bound. No claim of preempting a unit.
    fn step(&mut self) -> Result<(), BlockingError> {
        if Instant::now() >= self.deadline {
            return Err(BlockingError::Deadline);
        }
        if self.remaining == 0 {
            return Err(BlockingError::StepLimit);
        }
        self.remaining -= 1;
        Ok(())
    }
}

pub struct BlockingExecutor {
    permits: Arc<Semaphore>,
    max_input: NonZeroUsize,
    max_steps: NonZeroUsize,
}
static PROCESS_EXECUTOR: OnceLock<BlockingExecutor> = OnceLock::new();
impl BlockingExecutor {
    /// One process-wide limiter; callers cannot create per-invocation production pools.
    pub fn process() -> &'static Self {
        PROCESS_EXECUTOR.get_or_init(|| Self::new(4, 1024 * 1024, 100_000))
    }
    fn new(capacity: usize, max_input: usize, max_steps: usize) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(capacity)),
            max_input: NonZeroUsize::new(max_input).expect("positive input bound"),
            max_steps: NonZeroUsize::new(max_steps).expect("positive step bound"),
        }
    }
    pub async fn execute<W: BoundedWork>(
        &self,
        mut work: W,
        deadline: Instant,
    ) -> Result<W::Output, BlockingError> {
        if work.input_bytes() > self.max_input.get() {
            return Err(BlockingError::InvalidBounds);
        }
        if Instant::now() >= deadline {
            return Err(BlockingError::Deadline);
        }
        let permit = timeout_at(deadline, self.permits.clone().acquire_owned())
            .await
            .map_err(|_| BlockingError::Deadline)?
            .map_err(|_| BlockingError::WorkFailed)?;
        if Instant::now() >= deadline {
            return Err(BlockingError::Deadline);
        }
        let mut budget = WorkBudget {
            remaining: self.max_steps.get(),
            deadline,
        };
        let job = tokio::task::spawn_blocking(move || {
            // This owner MUST stay inside the closure until it actually returns.
            let _permit = permit;
            loop {
                budget.step()?;
                if let WorkStep::Ready(output) = work.step()? {
                    return Ok(output);
                }
            }
        });
        timeout_at(deadline, job)
            .await
            .map_err(|_| BlockingError::Deadline)?
            .map_err(|_| BlockingError::WorkFailed)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
        time::Duration,
    };
    struct Steps {
        size: usize,
        calls: Arc<AtomicUsize>,
        panic: bool,
    }
    impl BoundedWork for Steps {
        type Output = ();
        fn input_bytes(&self) -> usize {
            self.size
        }
        fn step(&mut self) -> Result<WorkStep<()>, BlockingError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert!(!self.panic, "test-only blocking panic");
            Ok(WorkStep::Pending)
        }
    }
    #[tokio::test]
    async fn input_and_step_caps_are_enforced_and_panics_release_permits() {
        let executor = BlockingExecutor::new(1, 8, 3);
        let calls = Arc::new(AtomicUsize::new(0));
        let deadline = Instant::now() + Duration::from_secs(1);
        assert_eq!(
            executor
                .execute(
                    Steps {
                        size: 9,
                        calls: calls.clone(),
                        panic: false
                    },
                    deadline
                )
                .await,
            Err(BlockingError::InvalidBounds)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            executor
                .execute(
                    Steps {
                        size: 1,
                        calls: calls.clone(),
                        panic: false
                    },
                    deadline
                )
                .await,
            Err(BlockingError::StepLimit)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(
            executor
                .execute(
                    Steps {
                        size: 1,
                        calls: calls.clone(),
                        panic: true
                    },
                    deadline
                )
                .await,
            Err(BlockingError::WorkFailed)
        );
        assert_eq!(executor.permits.available_permits(), 1);
        assert!(std::ptr::eq(
            BlockingExecutor::process(),
            BlockingExecutor::process()
        ));
    }
    struct FiniteWork {
        starts: Arc<AtomicUsize>,
        completed: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
        started: Option<tokio::sync::oneshot::Sender<()>>,
    }
    impl BoundedWork for FiniteWork {
        type Output = Vec<u8>;
        fn input_bytes(&self) -> usize {
            1
        }
        fn step(&mut self) -> Result<WorkStep<Self::Output>, BlockingError> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            let _ = self.started.take().unwrap().send(());
            // Test-only finite wall-clock wait: at most 350ms, even if test fails.
            let _ = self.release.recv_timeout(Duration::from_millis(350));
            self.completed.send(()).unwrap();
            Ok(WorkStep::Ready(b"late isolated output".to_vec()))
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn abandoned_work_keeps_single_permit_until_closure_returns() {
        let executor = Arc::new(BlockingExecutor::new(1, 64, 10));
        let starts = Arc::new(AtomicUsize::new(0));
        let (_release, rx) = mpsc::channel();
        let (done, completed) = mpsc::channel();
        let (started, entered) = tokio::sync::oneshot::channel();
        let first_executor = executor.clone();
        let first_starts = starts.clone();
        let first = tokio::spawn(async move {
            first_executor
                .execute(
                    FiniteWork {
                        starts: first_starts,
                        completed: done,
                        release: rx,
                        started: Some(started),
                    },
                    Instant::now() + Duration::from_millis(50),
                )
                .await
        });
        entered.await.unwrap();
        assert_eq!(first.await.unwrap(), Err(BlockingError::Deadline));
        assert!(completed.try_recv().is_err());
        assert_eq!(executor.permits.available_permits(), 0);
        let (_release2, rx2) = mpsc::channel();
        let (done2, _completed2) = mpsc::channel();
        let (started2, _entered2) = tokio::sync::oneshot::channel();
        let second = executor
            .execute(
                FiniteWork {
                    starts: starts.clone(),
                    completed: done2,
                    release: rx2,
                    started: Some(started2),
                },
                Instant::now() + Duration::from_millis(50),
            )
            .await;
        assert_eq!(second, Err(BlockingError::Deadline));
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert_eq!(executor.permits.available_permits(), 0);
        tokio::task::spawn_blocking(move || {
            completed.recv_timeout(Duration::from_secs(1)).unwrap()
        })
        .await
        .unwrap();
        let permit = timeout_at(
            Instant::now() + Duration::from_secs(1),
            executor.permits.clone().acquire_owned(),
        )
        .await
        .unwrap()
        .unwrap();
        drop(permit);
    }
}

/// Isolated closed fixture pool; never changes the process production executor.
#[cfg(feature = "conformance")]
pub mod conformance {
    use super::BlockingExecutor;
    pub fn capacity_one() -> BlockingExecutor {
        BlockingExecutor::new(1, 64, 1)
    }
    pub fn available(executor: &BlockingExecutor) -> usize {
        executor.permits.available_permits()
    }
}
