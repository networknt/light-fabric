//! Explicit supervised Init preparation and precision rollback.
//! One owned blocking orchestration lane, not a new runtime or precision worker.
use super::*;
use crate::validator_ipc::{
    Failure,
    ownership::{Drain, Initialization},
};
use std::sync::{Mutex, OnceLock, Weak, atomic::Ordering::SeqCst};
use tokio::{sync::Notify, time::Instant};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cleanup {
    NotRequired,
    Pending,
    Drained,
    Incomplete,
    Uncertain,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreparationSnapshot {
    pub active: bool,
    pub published: bool,
    pub cancelled: bool,
    pub cleanup: Cleanup,
}
struct GateState {
    snapshot: PreparationSnapshot,
    current: Weak<PreparationContext>,
    sealed: bool,
}
pub struct PreparationGate {
    state: Mutex<GateState>,
    changed: Notify,
}
static GLOBAL: OnceLock<Arc<PreparationGate>> = OnceLock::new();
impl PreparationGate {
    /// Explicit owned test/composition gate; no profile or executable admission.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(GateState {
                snapshot: PreparationSnapshot {
                    active: false,
                    published: false,
                    cancelled: false,
                    cleanup: Cleanup::NotRequired,
                },
                current: Weak::new(),
                sealed: false,
            }),
            changed: Notify::new(),
        })
    }
    pub fn process() -> Arc<Self> {
        GLOBAL.get_or_init(Self::new).clone()
    }
    pub fn snapshot(&self) -> PreparationSnapshot {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if self.state.is_poisoned() {
            state.snapshot.cleanup = Cleanup::Uncertain;
        }
        state.snapshot
    }
    fn begin(
        self: &Arc<Self>,
        timeout: Duration,
    ) -> Result<(Arc<PreparationContext>, Permit), InitError> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(InitError::Preparation)?;
        if timeout.is_zero() {
            return Err(InitError::Preparation);
        }
        let mut state = self.state.lock().map_err(|_| InitError::Preparation)?;
        if state.snapshot.active
            || state.sealed
            || state.current.upgrade().is_some()
            || matches!(
                state.snapshot.cleanup,
                Cleanup::Incomplete | Cleanup::Uncertain
            )
        {
            return Err(InitError::Preparation);
        }
        let context = Arc::new(PreparationContext {
            gate: self.clone(),
            deadline,
            cancel: CancellationToken::new(),
            entry: Mutex::new(None),
            background: AtomicBool::new(true),
            cleanup_started: AtomicBool::new(false),
        });
        state.snapshot = PreparationSnapshot {
            active: true,
            published: false,
            cancelled: false,
            cleanup: Cleanup::NotRequired,
        };
        state.current = Arc::downgrade(&context);
        Ok((context, Permit { gate: self.clone() }))
    }
    /// Wait for this flow's actual blocking lane and precision cleanup. Pending
    /// means a live published scope or unfinished rollback, never successful reap.
    pub async fn settled(&self, deadline: Instant) -> Result<PreparationSnapshot, InitError> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let snapshot = self.snapshot();
            if !snapshot.active {
                match snapshot.cleanup {
                    Cleanup::Incomplete | Cleanup::Uncertain => return Err(InitError::Preparation),
                    Cleanup::Pending => {}
                    Cleanup::NotRequired | Cleanup::Drained => return Ok(snapshot),
                }
            }
            tokio::time::timeout_at(deadline, changed)
                .await
                .map_err(|_| InitError::Preparation)?;
        }
    }
}
pub struct PreparationProfile {
    pub gate: Arc<PreparationGate>,
    pub timeout: Duration,
}
struct Entry {
    drain: Arc<Drain>,
}
/// A trusted Init context. Its token is not a worker/profile admission capability.
pub struct PreparationContext {
    gate: Arc<PreparationGate>,
    deadline: Instant,
    cancel: CancellationToken,
    entry: Mutex<Option<Entry>>,
    background: AtomicBool,
    cleanup_started: AtomicBool,
}
impl PreparationContext {
    pub fn deadline(&self) -> Instant {
        self.deadline
    }
    pub fn check(&self) -> Result<(), InitError> {
        if self.cancel.is_cancelled() || Instant::now() >= self.deadline {
            Err(InitError::Preparation)
        } else {
            Ok(())
        }
    }
    pub(crate) fn register_scope(&self, scope: Initialization) -> Result<(), InitError> {
        self.register_drain(scope.handoff())
    }
    pub(crate) fn register_drain(&self, drain: Arc<Drain>) -> Result<(), InitError> {
        let mut entry = match self.entry.lock() {
            Ok(entry) => entry,
            Err(error) => {
                let mut entry = error.into_inner();
                if entry.is_none() {
                    *entry = Some(Entry {
                        drain: drain.clone(),
                    });
                }
                let previous = entry.as_ref().map(|e| e.drain.clone());
                drop(entry);
                drain.close();
                if let Some(previous) = previous {
                    previous.close();
                }
                self.set_cleanup(Cleanup::Uncertain);
                return Err(InitError::Preparation);
            }
        };
        if let Some(current) = &*entry {
            if !Arc::ptr_eq(&current.drain, &drain) {
                drop(entry);
                drain.close();
                self.set_cleanup(Cleanup::Uncertain);
                return Err(InitError::Preparation);
            }
        } else {
            *entry = Some(Entry {
                drain: drain.clone(),
            });
        }
        {
            let mut state = self.gate.state.lock().map_err(|_| InitError::Preparation)?;
            state.sealed = true;
            if state.snapshot.cleanup != Cleanup::Uncertain {
                state.snapshot.cleanup = Cleanup::Pending;
            }
        }
        drop(entry);
        if self.check().is_err() {
            drain.close();
            return Err(InitError::Preparation);
        }
        self.gate.changed.notify_waiters();
        Ok(())
    }
    fn set_cleanup(&self, cleanup: Cleanup) {
        let mut state = self.gate.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.snapshot.cleanup != Cleanup::Uncertain {
            state.snapshot.cleanup = cleanup;
        }
        drop(state);
        self.gate.changed.notify_waiters();
    }
    fn close(&self) {
        self.cancel.cancel();
        {
            let mut state = self.gate.state.lock().unwrap_or_else(|e| e.into_inner());
            state.snapshot.cancelled = true;
        }
        let drain = self
            .entry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|e| e.drain.clone());
        if let Some(drain) = drain {
            drain.close();
        }
        self.gate.changed.notify_waiters();
    }
    async fn rollback(&self) {
        self.cleanup_started.store(true, SeqCst);
        self.close();
        let drain = self
            .entry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|e| e.drain.clone());
        let cleanup = if let Some(drain) = drain {
            match drain.shutdown(self.deadline).await {
                Ok(_) => Cleanup::Drained,
                Err(Failure::Deadline) => Cleanup::Incomplete,
                Err(_) => Cleanup::Uncertain,
            }
        } else {
            Cleanup::NotRequired
        };
        self.set_cleanup(cleanup);
    }
    pub(crate) fn publish<T>(&self, f: impl FnOnce() -> T) -> Result<T, InitError> {
        let mut state = self.gate.state.lock().map_err(|_| InitError::Preparation)?;
        self.check()?;
        let value = f();
        state.snapshot.published = true;
        state.sealed = true;
        Ok(value)
    }
    pub(crate) fn snapshot_owner(self: &Arc<Self>) -> Arc<dyn RuntimeResource> {
        Arc::new(SnapshotOwner(self.clone()))
    }
}
struct Permit {
    gate: Arc<PreparationGate>,
}
impl Drop for Permit {
    fn drop(&mut self) {
        let mut state = self.gate.state.lock().unwrap_or_else(|e| e.into_inner());
        state.snapshot.active = false;
        if let Some(context) = state.current.upgrade() {
            context.background.store(false, SeqCst);
        }
        drop(state);
        self.gate.changed.notify_waiters();
    }
}
struct Caller {
    context: Arc<PreparationContext>,
    armed: bool,
}
impl Drop for Caller {
    fn drop(&mut self) {
        if self.armed {
            self.context.close();
        }
    }
}
struct SnapshotOwner(Arc<PreparationContext>);
impl Drop for SnapshotOwner {
    fn drop(&mut self) {
        self.0.close();
        if !self.0.background.load(SeqCst)
            && !self.0.cleanup_started.swap(true, SeqCst)
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let context = self.0.clone();
            runtime.spawn(async move {
                context.rollback().await;
            });
        }
    }
}
pub(super) fn profile(input: &InitInput) -> Option<PreparationProfile> {
    // The precision producer is authoritative; Validator-only composition is the
    // fallback. Duplicate/class/enable validation remains in the existing core.
    [HandlerId::Specification, HandlerId::Validator]
        .into_iter()
        .find_map(|id| {
            input.handlers.iter().filter(|h| h.id == id).find_map(|h| {
                h.factory
                    .as_ref()
                    .and_then(|f| f.runtime_preparation_profile())
            })
        })
}
pub(super) async fn initialize(
    input: InitInput,
    profile: PreparationProfile,
) -> Result<Arc<Snapshot>, InitError> {
    run(profile, move |context| {
        super::initialize_owned(input, Some(context))
    })
    .await
}
async fn run<T, F, Fut>(profile: PreparationProfile, body: F) -> Result<T, InitError>
where
    T: Send + 'static,
    F: FnOnce(Arc<PreparationContext>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<T, InitError>> + Send + 'static,
{
    let (context, permit) = profile.gate.begin(profile.timeout)?;
    let mut caller = Caller {
        context: context.clone(),
        armed: true,
    };
    let runtime = tokio::runtime::Handle::current();
    let work_context = context.clone();
    let work = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        runtime.block_on(async {
            let owned = work_context.clone();
            let result = AssertUnwindSafe(async {
                tokio::select! {biased;
                    _=work_context.cancel.cancelled()=>Err(InitError::Preparation),
                    result=body(owned)=>result,
                }
            })
            .catch_unwind()
            .await
            .unwrap_or(Err(InitError::Panic));
            if result.is_err() {
                work_context.rollback().await;
            }
            result
        })
    });
    let result = tokio::time::timeout_at(context.deadline, work)
        .await
        .map_err(|_| InitError::Preparation)?
        .map_err(|_| InitError::Panic)?;
    if result.is_ok() {
        caller.armed = false;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Condvar, atomic::AtomicUsize};
    fn later() -> Instant {
        Instant::now() + Duration::from_secs(5)
    }
    async fn closed(drain: &Drain) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !drain.is_closed() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    #[test]
    fn busy_gate_refuses_without_cancelling_incumbent() {
        let gate = PreparationGate::new();
        let (context, permit) = gate.begin(Duration::from_secs(5)).unwrap();
        assert!(gate.begin(Duration::from_secs(5)).is_err());
        assert!(!context.cancel.is_cancelled());
        assert!(gate.snapshot().active);
        drop(permit);
        assert!(!gate.snapshot().active);
        drop(context);
    }
    #[tokio::test]
    async fn rollback_awaits_actual_owned_ticket_completion() {
        let gate = PreparationGate::new();
        let (context, permit) = gate.begin(Duration::from_secs(5)).unwrap();
        let drain = Drain::new();
        let ticket = drain.metadata().unwrap();
        context
            .register_scope(Initialization::new(drain.clone()))
            .unwrap();
        let cleanup = context.clone();
        let task = tokio::spawn(async move {
            cleanup.rollback().await;
        });
        closed(&drain).await;
        assert!(!task.is_finished());
        assert_eq!(gate.snapshot().cleanup, Cleanup::Pending);
        drop(ticket);
        task.await.unwrap();
        drop(permit);
        assert_eq!(
            gate.settled(later()).await.unwrap().cleanup,
            Cleanup::Drained
        );
    }
    #[tokio::test]
    async fn cancellation_before_registration_closes_only_new_owned_epoch() {
        let gate = PreparationGate::new();
        let (context, permit) = gate.begin(Duration::from_secs(5)).unwrap();
        context.close();
        let drain = Drain::new();
        assert!(
            context
                .register_scope(Initialization::new(drain.clone()))
                .is_err()
        );
        assert!(drain.is_closed());
        context.rollback().await;
        drop(permit);
        assert_eq!(gate.snapshot().cleanup, Cleanup::Drained);
    }
    #[tokio::test]
    async fn conflicting_registration_is_sticky_uncertain() {
        let gate = PreparationGate::new();
        let (context, permit) = gate.begin(Duration::from_secs(5)).unwrap();
        let first = Drain::new();
        let second = Drain::new();
        context.register_drain(first.clone()).unwrap();
        assert!(context.register_drain(second.clone()).is_err());
        assert!(second.is_closed());
        context.register_drain(first).unwrap();
        context.rollback().await;
        drop(permit);
        assert_eq!(gate.snapshot().cleanup, Cleanup::Uncertain);
        assert!(gate.settled(later()).await.is_err());
    }
    #[test]
    fn cancelled_publication_does_not_run_callback() {
        let gate = PreparationGate::new();
        let (context, _permit) = gate.begin(Duration::from_secs(5)).unwrap();
        let calls = AtomicUsize::new(0);
        context.close();
        assert!(context.publish(|| calls.fetch_add(1, SeqCst)).is_err());
        assert_eq!(calls.load(SeqCst), 0);
        assert!(!gate.snapshot().published);
    }
    #[tokio::test]
    async fn cancelled_blocking_source_keeps_lane_until_completion_and_cannot_publish() {
        let gate = PreparationGate::new();
        let block = Arc::new((Mutex::new(false), Condvar::new()));
        struct Release(Arc<(Mutex<bool>, Condvar)>);
        impl Drop for Release {
            fn drop(&mut self) {
                let (l, c) = &*self.0;
                *l.lock().unwrap() = true;
                c.notify_all();
            }
        }
        let _release = Release(block.clone());
        let entered = Arc::new(Notify::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let b = block.clone();
        let e = entered.clone();
        let c = calls.clone();
        let profile = PreparationProfile {
            gate: gate.clone(),
            timeout: Duration::from_secs(5),
        };
        let task = tokio::spawn(run(profile, move |context| async move {
            e.notify_one();
            let (l, w) = &*b;
            let mut ready = l.lock().unwrap();
            while !*ready {
                ready = w.wait(ready).unwrap();
            }
            context.check()?;
            context.publish(|| c.fetch_add(1, SeqCst))
        }));
        entered.notified().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(gate.snapshot().active);
        assert!(gate.snapshot().cancelled);
        assert!(gate.begin(Duration::from_secs(5)).is_err());
        let (l, w) = &*block;
        *l.lock().unwrap() = true;
        w.notify_all();
        assert!(gate.settled(later()).await.is_ok());
        assert_eq!(calls.load(SeqCst), 0);
        assert!(!gate.snapshot().published);
    }
    #[tokio::test]
    async fn panic_preserves_primary_error_and_waits_for_precision_cleanup() {
        let gate = PreparationGate::new();
        let drain = Drain::new();
        let ticket = drain.metadata().unwrap();
        let d = drain.clone();
        let profile = PreparationProfile {
            gate: gate.clone(),
            timeout: Duration::from_secs(5),
        };
        let task = tokio::spawn(run::<(), _, _>(profile, move |context| async move {
            context.register_drain(d)?;
            panic!("synthetic");
        }));
        closed(&drain).await;
        assert!(!task.is_finished());
        drop(ticket);
        assert!(matches!(task.await.unwrap(), Err(InitError::Panic)));
        assert_eq!(
            gate.settled(later()).await.unwrap().cleanup,
            Cleanup::Drained
        );
    }
    #[tokio::test]
    async fn expired_cleanup_remains_incomplete_after_work_finishes() {
        let gate = PreparationGate::new();
        let (context, permit) = gate.begin(Duration::from_nanos(1)).unwrap();
        let drain = Drain::new();
        let ticket = drain.metadata().unwrap();
        let _ = context.register_drain(drain);
        context.rollback().await;
        drop(ticket);
        drop(permit);
        assert_eq!(gate.snapshot().cleanup, Cleanup::Incomplete);
        assert!(gate.settled(later()).await.is_err());
    }
    #[tokio::test]
    async fn published_owner_drop_closes_and_awaits_precision_scope() {
        let gate = PreparationGate::new();
        let (context, permit) = gate.begin(Duration::from_secs(5)).unwrap();
        let drain = Drain::new();
        let ticket = drain.metadata().unwrap();
        context.register_drain(drain.clone()).unwrap();
        let owner = context.snapshot_owner();
        context.publish(|| ()).unwrap();
        drop(permit);
        drop(owner);
        closed(&drain).await;
        assert_eq!(gate.snapshot().cleanup, Cleanup::Pending);
        drop(ticket);
        assert_eq!(
            gate.settled(later()).await.unwrap().cleanup,
            Cleanup::Drained
        );
    }

    #[tokio::test]
    async fn poisoned_registration_closes_and_retains_owned_drain_as_uncertain() {
        let gate = PreparationGate::new();
        let (context, permit) = gate.begin(Duration::from_secs(5)).unwrap();
        let other = context.clone();
        assert!(
            std::thread::spawn(move || {
                let _entry = other.entry.lock().unwrap();
                panic!("synthetic");
            })
            .join()
            .is_err()
        );
        let drain = Drain::new();
        assert!(context.register_drain(drain.clone()).is_err());
        assert!(drain.is_closed());
        context.rollback().await;
        drop(permit);
        assert_eq!(gate.snapshot().cleanup, Cleanup::Uncertain);
        assert!(gate.settled(later()).await.is_err());
    }
}
