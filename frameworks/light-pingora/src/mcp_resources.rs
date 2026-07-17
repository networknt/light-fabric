#![allow(dead_code)] // Phase 4 activates stateless request/backend admission.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// Stateless admission is independent from the legacy session store. Permits
/// are owned so cancellation, early return, and panic unwinding release every
/// acquired budget without an explicit cleanup path.
pub(crate) struct StatelessRequestPermit {
    _global: OwnedSemaphorePermit,
    _principal: OwnedSemaphorePermit,
}

pub(crate) struct BackendCallPermit {
    _target: OwnedSemaphorePermit,
}

#[derive(Debug)]
struct KeyedAdmission {
    entries: Mutex<BTreeMap<String, Weak<Semaphore>>>,
}

impl KeyedAdmission {
    fn new() -> Self {
        Self {
            entries: Mutex::new(BTreeMap::new()),
        }
    }

    fn try_acquire(&self, key: &str, limit: usize) -> Option<OwnedSemaphorePermit> {
        let semaphore = {
            let mut entries = self.entries.lock().ok()?;
            entries.retain(|_, value| value.strong_count() > 0);
            if let Some(semaphore) = entries.get(key).and_then(Weak::upgrade) {
                semaphore
            } else {
                let semaphore = Arc::new(Semaphore::new(limit));
                entries.insert(key.to_string(), Arc::downgrade(&semaphore));
                semaphore
            }
        };
        semaphore.try_acquire_owned().ok()
    }
}

#[derive(Debug)]
pub(crate) struct StatelessResourceBudgets {
    global_requests: Arc<Semaphore>,
    requests_by_principal: KeyedAdmission,
    backend_calls_by_target: KeyedAdmission,
    max_requests_per_principal: usize,
}

impl StatelessResourceBudgets {
    pub fn new(max_requests: usize, max_requests_per_principal: usize) -> Result<Self, String> {
        if max_requests == 0 || max_requests_per_principal == 0 {
            return Err("stateless request limits must be greater than zero".to_string());
        }
        Ok(Self {
            global_requests: Arc::new(Semaphore::new(max_requests)),
            requests_by_principal: KeyedAdmission::new(),
            backend_calls_by_target: KeyedAdmission::new(),
            max_requests_per_principal,
        })
    }

    pub fn try_request(&self, principal_key: &str) -> Option<StatelessRequestPermit> {
        let global = Arc::clone(&self.global_requests).try_acquire_owned().ok()?;
        let principal = self
            .requests_by_principal
            .try_acquire(principal_key, self.max_requests_per_principal)?;
        Some(StatelessRequestPermit {
            _global: global,
            _principal: principal,
        })
    }

    pub fn try_backend_call(
        &self,
        target_key: &str,
        max_calls_per_target: usize,
    ) -> Option<BackendCallPermit> {
        if max_calls_per_target == 0 {
            return None;
        }
        self.backend_calls_by_target
            .try_acquire(target_key, max_calls_per_target)
            .map(|target| BackendCallPermit { _target: target })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_limits_are_fail_fast_and_release_on_drop() {
        let budgets = StatelessResourceBudgets::new(2, 1).expect("budgets");
        let first = budgets.try_request("principal-a").expect("first");
        assert!(budgets.try_request("principal-a").is_none());
        let second = budgets.try_request("principal-b").expect("second");
        assert!(budgets.try_request("principal-c").is_none());
        drop(first);
        assert!(budgets.try_request("principal-a").is_some());
        drop(second);
    }

    #[test]
    fn backend_limit_is_per_target_and_raii_backed() {
        let budgets = StatelessResourceBudgets::new(1, 1).expect("budgets");
        let first = budgets.try_backend_call("a", 1).expect("first");
        assert!(budgets.try_backend_call("a", 1).is_none());
        assert!(budgets.try_backend_call("b", 1).is_some());
        drop(first);
        assert!(budgets.try_backend_call("a", 1).is_some());
    }

    #[test]
    fn panic_unwinding_releases_request_permits() {
        let budgets = StatelessResourceBudgets::new(1, 1).expect("budgets");
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _permit = budgets.try_request("principal").expect("permit");
            panic!("exercise unwind");
        }));
        assert!(budgets.try_request("principal").is_some());
    }
}
