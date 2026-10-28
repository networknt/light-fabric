//! Bounded explicit qualification work ownership; not a production admission token.
//! Counts are work/owner bounds, never wall-clock, RSS or CPU guarantees.
use super::controller::Failure;
use std::sync::{Arc, Mutex, OnceLock};
use tokio::{sync::Notify, time::Instant};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    pub captures: usize,
    pub metadata: usize,
    pub owners: usize,
    pub closed: bool,
    pub uncertain: bool,
}
#[derive(Clone, Copy)]
enum Kind {
    Capture,
    Metadata,
    Owner,
}
pub struct Drain {
    state: Mutex<Snapshot>,
    changed: Notify,
}
static GLOBAL: OnceLock<Arc<Drain>> = OnceLock::new();
impl Drain {
    /// An explicitly owned scope, useful for independent non-child composition.
    /// Creating a drain does not admit any executable/profile.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(Snapshot {
                captures: 0,
                metadata: 0,
                owners: 0,
                closed: false,
                uncertain: false,
            }),
            changed: Notify::new(),
        })
    }
    pub fn process() -> Arc<Self> {
        GLOBAL.get_or_init(Self::new).clone()
    }
    pub fn snapshot(&self) -> Snapshot {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if self.state.is_poisoned() {
            state.closed = true;
            state.uncertain = true;
        }
        *state
    }
    pub fn close(&self) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.closed = true;
        if self.state.is_poisoned() {
            s.uncertain = true;
        }
        drop(s);
        self.changed.notify_waiters();
    }
    pub(super) fn uncertain(&self) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.closed = true;
        s.uncertain = true;
        drop(s);
        self.changed.notify_waiters();
    }
    pub fn is_closed(&self) -> bool {
        self.snapshot().closed
    }
    fn enter(self: &Arc<Self>, kind: Kind) -> Result<Work, Failure> {
        let mut s = self.state.lock().map_err(|_| Failure::NoReady)?;
        if s.closed || s.uncertain {
            return Err(Failure::NoReady);
        }
        let count = match kind {
            Kind::Capture => &mut s.captures,
            Kind::Metadata => &mut s.metadata,
            Kind::Owner => &mut s.owners,
        };
        if *count != 0 {
            return Err(Failure::NoReady);
        }
        *count = 1;
        Ok(Work {
            drain: self.clone(),
            kind,
        })
    }
    pub(super) fn capture(self: &Arc<Self>) -> Result<Work, Failure> {
        self.enter(Kind::Capture)
    }
    pub(super) fn owner(self: &Arc<Self>) -> Result<Work, Failure> {
        self.enter(Kind::Owner)
    }
    pub fn metadata(self: &Arc<Self>) -> Result<Work, Failure> {
        self.enter(Kind::Metadata)
    }
    /// Publication linearizes with close; no lock is held across an async await.
    pub(super) fn publish<T>(
        &self,
        deadline: Instant,
        f: impl FnOnce() -> T,
    ) -> Result<T, Failure> {
        let s = self.state.lock().map_err(|_| Failure::NoReady)?;
        if Instant::now() >= deadline {
            return Err(Failure::Deadline);
        }
        if s.closed || s.uncertain {
            return Err(Failure::NoReady);
        }
        Ok(f())
    }
    pub(super) async fn closed(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.is_closed() {
                return;
            }
            changed.await;
        }
    }
    pub async fn shutdown(&self, deadline: Instant) -> Result<Snapshot, Failure> {
        self.close();
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let s = self.snapshot();
            if s.uncertain {
                return Err(Failure::NoReady);
            }
            if s.captures == 0 && s.metadata == 0 && s.owners == 0 {
                return Ok(s);
            }
            tokio::time::timeout_at(deadline, changed)
                .await
                .map_err(|_| Failure::Deadline)?;
        }
    }
}
/// Opaque non-Clone ticket stays in queued/running work or retained resources.
pub struct Work {
    drain: Arc<Drain>,
    kind: Kind,
}
impl Drop for Work {
    fn drop(&mut self) {
        let mut s = self.drain.state.lock().unwrap_or_else(|e| e.into_inner());
        let count = match self.kind {
            Kind::Capture => &mut s.captures,
            Kind::Metadata => &mut s.metadata,
            Kind::Owner => &mut s.owners,
        };
        *count -= 1;
        drop(s);
        self.drain.changed.notify_waiters();
    }
}
/// Failed/cancelled initialization closes its owned epoch; only explicit publication
/// disarms. A busy competing initialization must never construct this scope.
pub struct Initialization {
    drain: Arc<Drain>,
    armed: bool,
}
impl Initialization {
    pub fn new(drain: Arc<Drain>) -> Self {
        Self { drain, armed: true }
    }
    pub(crate) fn handoff(mut self) -> Arc<Drain> {
        self.armed = false;
        self.drain.clone()
    }
    pub fn metadata(&self) -> Result<Work, Failure> {
        self.drain.metadata()
    }
    pub fn publish<T>(mut self, deadline: Instant, f: impl FnOnce() -> T) -> Result<T, Failure> {
        let result = self.drain.publish(deadline, f)?;
        self.armed = false;
        Ok(result)
    }
    pub async fn shutdown(self, deadline: Instant) -> Result<Snapshot, Failure> {
        self.drain.shutdown(deadline).await
    }
}
impl Drop for Initialization {
    fn drop(&mut self) {
        if self.armed {
            self.drain.close();
        }
    }
}
/// One admitted capture job. Timeout/drop closes the epoch but never pretends to
/// preempt blocking IO; ticket, owner and credits remain in the closure until done.
pub(super) async fn capture<T: Send + 'static>(
    ticket: Work,
    deadline: Instant,
    job: impl FnOnce() -> Result<T, Failure> + Send + 'static,
) -> Result<T, Failure> {
    let drain = ticket.drain.clone();
    let scope = Initialization::new(drain.clone());
    if Instant::now() >= deadline {
        return Err(Failure::Deadline);
    }
    let work = tokio::task::spawn_blocking(move || {
        let _ticket = ticket;
        let value = job()?;
        drain.publish(deadline, || value)
    });
    let value = tokio::time::timeout_at(deadline, work)
        .await
        .map_err(|_| Failure::Deadline)?
        .map_err(|_| Failure::AdapterInvariant)??;
    scope.publish(deadline, || value)
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Condvar;
    fn later() -> Instant {
        Instant::now() + std::time::Duration::from_secs(5)
    }
    #[test]
    fn count_admission_is_fail_fast_and_refunds_on_completion() {
        let d = Drain::new();
        let capture = d.capture().unwrap();
        assert!(d.capture().is_err());
        let metadata = d.metadata().unwrap();
        assert!(d.metadata().is_err());
        let owner = d.owner().unwrap();
        assert!(d.owner().is_err());
        drop((capture, metadata, owner));
        assert_eq!(
            (
                d.snapshot().captures,
                d.snapshot().metadata,
                d.snapshot().owners
            ),
            (0, 0, 0)
        );
    }
    #[tokio::test]
    async fn close_refuses_new_work_and_waits_for_actual_ticket_drop() {
        let d = Drain::new();
        let ticket = d.capture().unwrap();
        d.close();
        assert!(d.metadata().is_err());
        assert_eq!(d.shutdown(Instant::now()).await, Err(Failure::Deadline));
        drop(ticket);
        assert!(d.shutdown(later()).await.is_ok());
    }
    #[tokio::test]
    async fn uncertainty_is_not_green_even_with_zero_activity() {
        let d = Drain::new();
        d.uncertain();
        assert_eq!(d.shutdown(later()).await, Err(Failure::NoReady));
    }
    #[test]
    fn late_or_closed_publication_never_runs_callback() {
        let d = Drain::new();
        let ran = std::sync::atomic::AtomicBool::new(false);
        d.close();
        assert!(
            d.publish(later(), || ran
                .store(true, std::sync::atomic::Ordering::SeqCst))
                .is_err()
        );
        assert!(!ran.load(std::sync::atomic::Ordering::SeqCst));
    }
    #[test]
    fn cancelled_initialization_closes_but_successful_publication_disarms() {
        let d = Drain::new();
        drop(Initialization::new(d.clone()));
        assert!(d.is_closed());
        let d = Drain::new();
        assert_eq!(Initialization::new(d.clone()).publish(later(), || 3), Ok(3));
        assert!(!d.is_closed());
    }
    #[tokio::test]
    async fn cancelled_capture_retains_queued_work_and_discards_late_result() {
        let d = Drain::new();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        struct Release(Arc<(Mutex<bool>, Condvar)>);
        impl Drop for Release {
            fn drop(&mut self) {
                let (lock, changed) = &*self.0;
                *lock.lock().unwrap() = true;
                changed.notify_all();
            }
        }
        let _release = Release(gate.clone());
        let started = Arc::new(Notify::new());
        let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        #[derive(Debug)]
        struct Value(Arc<std::sync::atomic::AtomicUsize>);
        impl Drop for Value {
            fn drop(&mut self) {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let ticket = d.capture().unwrap();
        let g = gate.clone();
        let signal = started.clone();
        let count = drops.clone();
        let task = tokio::spawn(capture(ticket, later(), move || {
            signal.notify_one();
            let (lock, changed) = &*g;
            let mut ready = lock.lock().unwrap();
            while !*ready {
                ready = changed.wait(ready).unwrap();
            }
            Ok(Value(count))
        }));
        started.notified().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_eq!(d.snapshot().captures, 1);
        assert!(d.is_closed());
        assert!(d.capture().is_err());
        assert_eq!(d.shutdown(Instant::now()).await, Err(Failure::Deadline));
        let (lock, changed) = &*gate;
        *lock.lock().unwrap() = true;
        changed.notify_one();
        assert!(d.shutdown(later()).await.is_ok());
        assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn ordinary_capture_succeeds_without_closing_epoch() {
        let d = Drain::new();
        assert_eq!(
            capture(d.capture().unwrap(), later(), || Ok(7)).await,
            Ok(7)
        );
        assert!(!d.is_closed());
        assert_eq!(d.snapshot().captures, 0);
    }
    #[tokio::test]
    async fn blocking_panic_fails_closed_without_escaping_details() {
        let d = Drain::new();
        let r = capture::<()>(d.capture().unwrap(), later(), || {
            panic!("synthetic-private")
        })
        .await;
        assert_eq!(r, Err(Failure::AdapterInvariant));
        assert!(d.is_closed());
        assert!(d.shutdown(later()).await.is_ok());
    }
}
