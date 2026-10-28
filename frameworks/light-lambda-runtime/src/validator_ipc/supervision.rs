//! Explicit supervised qualification primitives. Not a default activation capability.
use std::sync::{Arc, Mutex, OnceLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Busy,
    Closed,
    InvalidPolicy,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Reserved,
    Started,
    Ready,
    Reaped,
    Quarantined,
}
struct State {
    phase: Phase,
    requests: usize,
}
/// The initial proposed policy is fail-stop: one started generation per process.
/// A reap proves cleanup, but does not silently grant a restart or reset CPU.
pub struct Authority {
    state: Mutex<State>,
}
static GLOBAL: OnceLock<Arc<Authority>> = OnceLock::new();
impl Authority {
    #[cfg(test)]
    pub(super) fn isolated() -> Arc<Self> {
        Arc::new(Self::new())
    }
    fn new() -> Self {
        Self {
            state: Mutex::new(State {
                phase: Phase::Idle,
                requests: 0,
            }),
        }
    }
    pub fn process() -> Arc<Self> {
        GLOBAL.get_or_init(|| Arc::new(Self::new())).clone()
    }
    pub fn phase(&self) -> Phase {
        self.state.lock().map_or(Phase::Quarantined, |s| s.phase)
    }
    pub fn reserve_owner(self: &Arc<Self>) -> Result<Owner, Refusal> {
        let mut s = self.state.lock().map_err(|_| Refusal::Closed)?;
        match s.phase {
            Phase::Idle => {
                s.phase = Phase::Reserved;
                Ok(Owner {
                    authority: self.clone(),
                    started: false,
                    finished: false,
                })
            }
            Phase::Reserved | Phase::Started | Phase::Ready => Err(Refusal::Busy),
            Phase::Reaped | Phase::Quarantined => Err(Refusal::Closed),
        }
    }
    /// Call before owned request projection. No waiter queue or allocation on refusal.
    /// Existing IPC actor must still enforce the one-active execution constraint.
    pub fn reserve_request(self: &Arc<Self>) -> Result<Request, Refusal> {
        let mut s = self.state.lock().map_err(|_| Refusal::Closed)?;
        if s.phase != Phase::Ready {
            return Err(Refusal::Closed);
        }
        if s.requests >= 2 {
            return Err(Refusal::Busy);
        }
        s.requests += 1;
        Ok(Request {
            authority: self.clone(),
        })
    }
}
/// Single owner reservation. Drop after start without successful wait is uncertain.
/// This ledger retains capacity; the controller separately retains real Process
/// resources and its owner observation in bounded quarantine.
pub struct Owner {
    authority: Arc<Authority>,
    started: bool,
    finished: bool,
}
impl Owner {
    pub fn started(&mut self) -> Result<(), Refusal> {
        let mut s = self.authority.state.lock().map_err(|_| Refusal::Closed)?;
        if self.started || s.phase != Phase::Reserved {
            return Err(Refusal::Closed);
        }
        self.started = true;
        s.phase = Phase::Started;
        Ok(())
    }
    pub fn ready(&mut self) -> Result<(), Refusal> {
        let mut s = self.authority.state.lock().map_err(|_| Refusal::Closed)?;
        if !self.started || s.phase != Phase::Started {
            return Err(Refusal::Closed);
        }
        s.phase = Phase::Ready;
        Ok(())
    }
    /// Wire only to the actual owning Child::wait outcome. Nonzero exit or a
    /// terminating signal still constitutes successful reap when wait succeeds.
    /// No raw error or exit detail is stored or exposed by this ledger.
    pub fn waited(
        mut self,
        result: &std::io::Result<std::process::ExitStatus>,
    ) -> Result<(), Refusal> {
        let mut s = self.authority.state.lock().map_err(|_| Refusal::Closed)?;
        if !self.started || !matches!(s.phase, Phase::Started | Phase::Ready) {
            return Err(Refusal::Closed);
        }
        self.finished = true;
        if result.is_ok() {
            s.phase = Phase::Reaped;
            Ok(())
        } else {
            s.phase = Phase::Quarantined;
            Err(Refusal::Closed)
        }
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        if !self.finished {
            // Poison remains fail-closed for every admission call.
            if let Ok(mut s) = self.authority.state.lock() {
                s.phase = if self.started {
                    Phase::Quarantined
                } else {
                    Phase::Idle
                };
            }
        }
    }
}
/// Share one Arc<Request> between caller and queued/in-flight work. Cancelling
/// the caller cannot refund capacity while work retains its reference.
pub struct Request {
    authority: Arc<Authority>,
}
impl Request {
    pub(super) fn belongs_to(&self, authority: &Arc<Authority>) -> bool {
        Arc::ptr_eq(&self.authority, authority)
    }
}
impl Drop for Request {
    fn drop(&mut self) {
        if let Ok(mut s) = self.authority.state.lock() {
            // Exactly one decrement for this non-Clone lease.
            s.requests -= 1;
        }
    }
}
/// Explicit review values only, with no Default/Deserialize or adoption token.
/// Requested allocation envelopes are not RSS bounds; address space is separate.
#[derive(Clone, Copy)]
pub struct Policy {
    pub platform_bytes: u64,
    pub parent_requested_bytes: u64,
    pub worker_requested_bytes: u64,
    pub headroom_bytes: u64,
    pub worker_address_space_bytes: u64,
    pub lifetime_cpu_seconds: u64,
}
impl Policy {
    pub fn validate(&self) -> Result<(), Refusal> {
        if [
            self.platform_bytes,
            self.parent_requested_bytes,
            self.worker_requested_bytes,
            self.headroom_bytes,
            self.worker_address_space_bytes,
            self.lifetime_cpu_seconds,
        ]
        .contains(&0)
            || self.worker_address_space_bytes < self.worker_requested_bytes
        {
            return Err(Refusal::InvalidPolicy);
        }
        let planned = self
            .parent_requested_bytes
            .checked_add(self.worker_requested_bytes)
            .and_then(|n| n.checked_add(self.headroom_bytes))
            .ok_or(Refusal::InvalidPolicy)?;
        if planned > self.platform_bytes {
            return Err(Refusal::InvalidPolicy);
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    fn authority() -> Arc<Authority> {
        Arc::new(Authority::new())
    }
    fn ready(a: &Arc<Authority>) -> Owner {
        let mut o = a.reserve_owner().unwrap();
        o.started().unwrap();
        o.ready().unwrap();
        o
    }
    fn policy() -> Policy {
        Policy {
            platform_bytes: 100,
            parent_requested_bytes: 20,
            worker_requested_bytes: 30,
            headroom_bytes: 50,
            worker_address_space_bytes: 200,
            lifetime_cpu_seconds: 1,
        }
    }
    #[test]
    fn process_authority_is_shared() {
        assert!(Arc::ptr_eq(&Authority::process(), &Authority::process()));
    }
    #[test]
    fn only_one_owner_reservation() {
        let a = authority();
        let _o = a.reserve_owner().unwrap();
        assert!(matches!(a.reserve_owner(), Err(Refusal::Busy)));
    }
    #[test]
    fn unstarted_owner_refunds() {
        let a = authority();
        drop(a.reserve_owner().unwrap());
        assert_eq!(a.phase(), Phase::Idle);
        assert!(a.reserve_owner().is_ok());
    }
    #[test]
    fn started_drop_quarantines() {
        let a = authority();
        let mut o = a.reserve_owner().unwrap();
        o.started().unwrap();
        drop(o);
        assert_eq!(a.phase(), Phase::Quarantined);
        assert!(matches!(a.reserve_owner(), Err(Refusal::Closed)));
    }
    #[test]
    fn readiness_requires_started_owner() {
        let a = authority();
        let mut o = a.reserve_owner().unwrap();
        assert_eq!(o.ready(), Err(Refusal::Closed));
        o.started().unwrap();
        assert_eq!(o.started(), Err(Refusal::Closed));
    }
    #[test]
    fn requests_require_ready() {
        let a = authority();
        assert!(matches!(a.reserve_request(), Err(Refusal::Closed)));
        let mut o = a.reserve_owner().unwrap();
        o.started().unwrap();
        assert!(matches!(a.reserve_request(), Err(Refusal::Closed)));
    }
    #[test]
    fn exactly_two_requests_no_waiters() {
        let a = authority();
        let _o = ready(&a);
        let r = a.reserve_request().unwrap();
        let _s = a.reserve_request().unwrap();
        assert!(matches!(a.reserve_request(), Err(Refusal::Busy)));
        drop(r);
        assert!(a.reserve_request().is_ok());
    }
    #[test]
    fn work_reference_retains_request_capacity() {
        let a = authority();
        let _o = ready(&a);
        let r = Arc::new(a.reserve_request().unwrap());
        let work = r.clone();
        let _s = a.reserve_request().unwrap();
        drop(r);
        assert!(matches!(a.reserve_request(), Err(Refusal::Busy)));
        drop(work);
        assert!(a.reserve_request().is_ok());
    }
    #[test]
    fn successful_wait_is_terminal_even_for_nonzero_exit() {
        let a = authority();
        ready(&a)
            .waited(&Ok(std::process::ExitStatus::from_raw(256)))
            .unwrap();
        assert_eq!(a.phase(), Phase::Reaped);
        assert!(matches!(a.reserve_owner(), Err(Refusal::Closed)));
    }
    #[test]
    fn wait_error_quarantines_without_raw_error() {
        let a = authority();
        assert_eq!(
            ready(&a).waited(&Err(std::io::Error::other("synthetic-private-message"))),
            Err(Refusal::Closed)
        );
        assert_eq!(a.phase(), Phase::Quarantined);
        assert!(matches!(a.reserve_request(), Err(Refusal::Closed)));
    }
    #[test]
    fn concurrent_callers_admit_exactly_two() {
        let a = authority();
        let _o = ready(&a);
        let leases = std::thread::scope(|s| {
            let handles: Vec<_> = (0..16)
                .map(|_| {
                    let a = a.clone();
                    s.spawn(move || a.reserve_request().ok())
                })
                .collect();
            handles
                .into_iter()
                .filter_map(|h| h.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(leases.len(), 2);
    }
    #[test]
    fn valid_explicit_policy_has_no_runtime_side_effect() {
        assert_eq!(policy().validate(), Ok(()));
    }
    #[test]
    fn zero_fields_rejected() {
        let p = policy();
        for field in 0..6 {
            let mut p = p;
            match field {
                0 => p.platform_bytes = 0,
                1 => p.parent_requested_bytes = 0,
                2 => p.worker_requested_bytes = 0,
                3 => p.headroom_bytes = 0,
                4 => p.worker_address_space_bytes = 0,
                _ => p.lifetime_cpu_seconds = 0,
            };
            assert_eq!(p.validate(), Err(Refusal::InvalidPolicy));
        }
    }
    #[test]
    fn overflowing_or_insufficient_envelope_rejected() {
        let mut p = policy();
        p.parent_requested_bytes = u64::MAX;
        assert_eq!(p.validate(), Err(Refusal::InvalidPolicy));
        p = policy();
        p.platform_bytes = 99;
        assert_eq!(p.validate(), Err(Refusal::InvalidPolicy));
    }
    #[test]
    fn virtual_memory_is_not_added_as_rss() {
        let mut p = policy();
        p.worker_address_space_bytes = u64::MAX;
        assert_eq!(p.validate(), Ok(()));
        p.worker_address_space_bytes = 29;
        assert_eq!(p.validate(), Err(Refusal::InvalidPolicy));
    }
    #[test]
    fn poisoned_authority_fails_closed() {
        let a = authority();
        let b = a.clone();
        assert!(
            std::thread::spawn(move || {
                let _s = b.state.lock().unwrap();
                panic!("synthetic");
            })
            .join()
            .is_err()
        );
        assert_eq!(a.phase(), Phase::Quarantined);
        assert!(matches!(a.reserve_owner(), Err(Refusal::Closed)));
    }
}
