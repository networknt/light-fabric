//! A one-shot final-write guard. Installing this around a high-level queued
//! request is NOT sufficient: the transport must finish connection/TLS/capacity
//! preparation first and call `poll_first_write` at its actual write hook.
use crate::{Decision, MAX_DISPATCH_LEASE_MS};
use std::{
    io,
    sync::Mutex,
    task::Poll,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendState {
    NotStarted,
    Started,
    AbortedNotInitiated,
}
struct Inner {
    state: SendState,
    acknowledged: bool,
}
pub struct SendGuard {
    deadline: Instant,
    decision: Decision,
    inner: Mutex<Inner>,
}
impl SendGuard {
    /// `authorize_started` is captured before the very first authorize request,
    /// not before begin-dispatch and not when its response arrives. It cannot be
    /// reconstructed after a process restart.
    pub fn new(authorize_started: Instant, decision: Decision) -> io::Result<Self> {
        if decision.lease_ms == 0
            || decision.lease_ms > MAX_DISPATCH_LEASE_MS
            || decision.generation <= 0
            || decision.owner.fencing_generation <= 0
            || decision.owner.boot.is_nil()
            || decision.owner.replica.is_nil()
            || decision.owner.gateway_service.is_empty()
            || decision.decision_id.is_nil()
            || decision.binding.validate().is_err()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid dispatch decision",
            ));
        }
        let deadline = authorize_started
            .checked_add(Duration::from_millis(decision.lease_ms))
            .ok_or_else(|| io::Error::other("invalid dispatch deadline"))?;
        Ok(Self {
            deadline,
            decision,
            inner: Mutex::new(Inner {
                state: SendState::NotStarted,
                acknowledged: false,
            }),
        })
    }
    /// Only the newly committed begin transition yields `new_permission=true`.
    /// A duplicate begin/status response must never arm a transport.
    pub fn acknowledge(&self, decision: &Decision, new_permission: bool) -> io::Result<()> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("send guard poisoned"))?;
        if !new_permission
            || decision != &self.decision
            || inner.state != SendState::NotStarted
            || inner.acknowledged
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "send acknowledgement rejected",
            ));
        }
        inner.acknowledged = true;
        Ok(())
    }
    /// Recheck before resuming a pending first-write poll. This never resets
    /// the state or permits a new initiation.
    pub fn deadline_valid(&self) -> bool {
        Instant::now() < self.deadline
    }

    pub fn state(&self) -> SendState {
        self.inner
            .lock()
            .map(|s| s.state)
            .unwrap_or(SendState::Started)
    }
    /// Returns true only if no first-write operation can ever run afterwards.
    /// Poisoned/unknown state is conservatively treated as possibly initiated.
    pub fn abort_not_initiated(&self) -> bool {
        let Ok(mut inner) = self.inner.lock() else {
            return false;
        };
        match inner.state {
            SendState::NotStarted => {
                inner.state = SendState::AbortedNotInitiated;
                true
            }
            SendState::AbortedNotInitiated => true,
            SendState::Started => false,
        }
    }
    /// The closure MUST synchronously initiate the prepared transport's first
    /// write (e.g. its poll_write), without creating/enqueueing a request or
    /// returning an unpolled async future. Pending/error after this call is
    /// uncertain; it never permits a second first-write attempt.
    pub fn poll_first_write<T>(
        &self,
        write: impl FnOnce() -> Poll<io::Result<T>>,
    ) -> Poll<io::Result<T>> {
        let Ok(mut inner) = self.inner.lock() else {
            return Poll::Ready(Err(io::Error::other("send guard poisoned")));
        };
        if inner.state != SendState::NotStarted || !inner.acknowledged {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "dispatch already started, aborted or unacknowledged",
            )));
        }
        if Instant::now() >= self.deadline {
            inner.state = SendState::AbortedNotInitiated;
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "dispatch lease expired before initiation",
            )));
        }
        inner.state = SendState::Started;
        // Keep the mutex across the synchronous initiation so abort cannot race
        // between the final deadline check and the first transport operation.
        write()
    }
}
