//! Opt-in owned local controller qualification. No application registration,
//! production defaults, HTTP mappings or shared worker framework.
use super::*;
mod owned;
use std::{
    collections::VecDeque,
    sync::{Mutex as StdMutex, atomic::AtomicBool},
    time::Duration,
};

pub struct RecoveryPolicy {
    pub attempts: usize,
    pub cooldown: Duration,
}
#[derive(Debug, PartialEq, Eq)]
pub enum RecoveryFailure {
    Worker(Failure),
    NotInitialized,
    Cooldown,
    Stale,
}
struct Window {
    completed: VecDeque<Instant>,
    pending: usize,
    last: Instant,
    policy: RecoveryPolicy,
}
impl Window {
    fn begin(&mut self, now: Instant) -> bool {
        let now = now.max(self.last);
        self.last = now;
        while self
            .completed
            .front()
            .is_some_and(|t| *t + self.policy.cooldown <= now)
        {
            self.completed.pop_front();
        }
        if self.pending + self.completed.len() >= self.policy.attempts {
            return false;
        }
        self.pending += 1;
        true
    }
    fn finish(&mut self, now: Instant) {
        let now = now.max(self.last);
        self.last = now;
        self.pending -= 1;
        self.completed.push_back(now);
    }
}
struct Attempt(Arc<StdMutex<Window>>);
impl Drop for Attempt {
    fn drop(&mut self) {
        self.0.lock().expect("owned window").finish(Instant::now());
    }
}
struct CancelPreparation(CancellationToken);
impl Drop for CancelPreparation {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

pub struct RecoveryController {
    artifacts: Arc<Artifacts>,
    program: Arc<[Rule]>,
    limits: QualificationLimits,
    capacity: Arc<Semaphore>,
    compiler: Arc<Semaphore>,
    window: Arc<StdMutex<Window>>,
    epoch: AtomicU64,
    admitted: AtomicBool,
    ready: tokio::sync::RwLock<Option<Arc<Session>>>,
}
impl RecoveryController {
    pub fn new(
        artifacts: Arc<Artifacts>,
        program: Arc<[Rule]>,
        limits: QualificationLimits,
        capacity: Arc<Semaphore>,
        policy: RecoveryPolicy,
    ) -> Result<Self, RecoveryFailure> {
        if policy.attempts == 0 || policy.cooldown.is_zero() {
            return Err(RecoveryFailure::Worker(Failure::Init));
        }
        Ok(Self {
            artifacts,
            program,
            limits,
            capacity,
            compiler: Arc::new(Semaphore::new(1)),
            window: Arc::new(StdMutex::new(Window {
                completed: VecDeque::new(),
                pending: 0,
                last: Instant::now(),
                policy,
            })),
            epoch: AtomicU64::new(0),
            admitted: AtomicBool::new(false),
            ready: tokio::sync::RwLock::new(None),
        })
    }
    async fn current(&self) -> Option<Arc<Session>> {
        self.ready
            .read()
            .await
            .as_ref()
            .filter(|session| !session.stopped.is_cancelled())
            .cloned()
    }
    pub async fn initialize(
        &self,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<(), RecoveryFailure> {
        self.prepare(true, deadline, cancel).await?;
        self.admitted.store(true, Ordering::SeqCst);
        Ok(())
    }
    async fn prepare(
        &self,
        replace: bool,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<Arc<Session>, RecoveryFailure> {
        check(deadline, &cancel).map_err(RecoveryFailure::Worker)?;
        let permit = tokio::select! {biased;
        _=cancel.cancelled()=>return Err(RecoveryFailure::Worker(Failure::Cancelled)),
        _=tokio::time::sleep_until(deadline)=>return Err(RecoveryFailure::Worker(Failure::Deadline)),
        permit=self.compiler.clone().acquire_owned()=>permit.map_err(|_|RecoveryFailure::NotInitialized)?};
        if !replace && let Some(current) = self.current().await {
            return Ok(current);
        }
        if !self
            .window
            .lock()
            .expect("owned window")
            .begin(Instant::now())
        {
            return Err(RecoveryFailure::Cooldown);
        }
        let attempt = Attempt(self.window.clone());
        let epoch = self
            .epoch
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_add(1))
            .map_err(|_| RecoveryFailure::Stale)?
            + 1;
        let artifacts = self.artifacts.clone();
        let program = self.program.clone();
        let limits = self.limits.clone();
        let capacity = self.capacity.clone();
        let child_cancel = cancel.child_token();
        let guard = CancelPreparation(child_cancel.clone());
        let runtime = tokio::runtime::Handle::current();
        // Verification reads trusted local artifacts synchronously. It runs off the
        // async deadline executor. One compiler owner and the admission charge remain
        // held even if the caller cancels while that local read has not returned.
        let job = tokio::task::spawn_blocking(move || {
            let _attempt = attempt;
            let result = runtime.block_on(Session::prepare(
                Preparation {
                    artifacts: &artifacts,
                    limits,
                    capacity,
                    epoch,
                },
                program,
                deadline,
                child_cancel,
            ));
            // Transfer the compiler owner back with readiness. Releasing it before
            // publication lets waiters mistake a completed-but-unpublished compiler for
            // no ready generation and consume another admission.
            (result, permit)
        });
        let (session, _compiler) = tokio::select! {biased;
        _=cancel.cancelled()=>return Err(RecoveryFailure::Worker(Failure::Cancelled)),
        _=tokio::time::sleep_until(deadline)=>return Err(RecoveryFailure::Worker(Failure::Deadline)),
        result=job=>result.map_err(|_|RecoveryFailure::Worker(Failure::Panic))?};
        let session = session.map_err(RecoveryFailure::Worker)?;
        check(deadline, &cancel).map_err(RecoveryFailure::Worker)?;
        let mut ready = self.ready.write().await;
        if self.epoch.load(Ordering::SeqCst) != epoch {
            return Err(RecoveryFailure::Stale);
        }
        let session = Arc::new(session);
        *ready = Some(session.clone());
        drop(guard);
        Ok(session)
    }
    pub async fn invalidate(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
        self.ready.write().await.take();
    }
    pub async fn rewrite(
        &self,
        path: &str,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<Candidate, RecoveryFailure> {
        check(deadline, &cancel).map_err(RecoveryFailure::Worker)?;
        if !self.admitted.load(Ordering::SeqCst) {
            return Err(RecoveryFailure::NotInitialized);
        }
        if path.encode_utf16().count() > self.limits.input_utf16 {
            return Err(RecoveryFailure::Worker(Failure::InputProfile));
        }
        let session = match self.current().await {
            Some(session) => session,
            None => self.prepare(false, deadline, cancel.clone()).await?,
        };
        let result = session.rewrite(path, deadline, cancel.clone()).await;
        if result.as_ref().err().is_some_and(|failure| {
            !matches!(
                failure,
                Failure::InvalidReplacement | Failure::InvalidUtf16 | Failure::InputProfile
            )
        }) {
            let mut ready = self.ready.write().await;
            if ready
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &session))
            {
                ready.take();
            }
        }
        // Never retry a failed rewrite. A later invocation may recover the complete
        // immutable program; no destination, HTTP client or credentials enter it.
        result.map_err(RecoveryFailure::Worker)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pending_compilers_do_not_expire_during_scheduler_or_io_delay() {
        let origin = Instant::now();
        let mut window = Window {
            completed: VecDeque::new(),
            pending: 0,
            last: origin,
            policy: RecoveryPolicy {
                attempts: 2,
                cooldown: Duration::from_secs(30),
            },
        };
        assert!(window.begin(origin));
        assert!(window.begin(origin + Duration::from_secs(100)));
        assert!(!window.begin(origin + Duration::from_secs(1000)));
        window.finish(origin + Duration::from_secs(1001));
        window.finish(origin + Duration::from_secs(1002));
        assert!(!window.begin(origin + Duration::from_secs(1030)));
        assert!(window.begin(origin + Duration::from_secs(1031)));
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "owned local pinned JRE controller qualification"]
    async fn real_automatic_recovery_complete_program_and_concurrent_admission() {
        let capacity = Arc::new(Semaphore::new(2));
        let controller = Arc::new(
            RecoveryController::new(
                Arc::new(super::super::tests::artifacts()),
                vec![Rule {
                    pattern: "/(x)".into(),
                    replacement: "/r-$1".into(),
                }]
                .into(),
                super::super::tests::limits(),
                capacity.clone(),
                RecoveryPolicy {
                    attempts: 2,
                    cooldown: Duration::from_secs(30),
                },
            )
            .unwrap(),
        );
        let deadline = || Instant::now() + Duration::from_secs(15);
        assert!(matches!(
            controller
                .rewrite("/x", deadline(), CancellationToken::new())
                .await,
            Err(RecoveryFailure::NotInitialized)
        ));
        controller
            .initialize(deadline(), CancellationToken::new())
            .await
            .unwrap();
        let old = controller.current().await.unwrap();
        let old_program = old.program().clone();
        controller.invalidate().await;
        let mut tasks = Vec::new();
        for _ in 0..32 {
            let c = controller.clone();
            tasks.push(tokio::spawn(async move {
                c.rewrite(
                    "/x",
                    Instant::now() + Duration::from_secs(15),
                    CancellationToken::new(),
                )
                .await
                .unwrap()
                .rewritten
            }));
        }
        for task in tasks {
            assert_eq!(task.await.unwrap(), "/r-x");
        }
        let current = controller.current().await.unwrap();
        assert!(Arc::ptr_eq(current.program(), &old_program));
        assert_eq!(controller.window.lock().unwrap().completed.len(), 2);
        assert_eq!(capacity.available_permits(), 0);
        let old_stopped = old.stopped.clone();
        drop(old);
        old_stopped.cancelled().await;
        let current_stopped = current.stopped.clone();
        drop(current);
        controller.invalidate().await;
        current_stopped.cancelled().await;
        assert_eq!(capacity.available_permits(), 2);
        assert!(matches!(
            controller
                .rewrite("/x", deadline(), CancellationToken::new())
                .await,
            Err(RecoveryFailure::Cooldown)
        ));
    }
}
