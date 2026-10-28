//! Request-triggered refresh lifetime, independent of any requesting future.
use super::*;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

type AttemptResult = Result<(), HandlerRejection>;

#[derive(Default)]
pub(super) struct JwksRefreshGate {
    entries: Mutex<HashMap<String, GateEntry>>,
    #[cfg(any(test, feature = "conformance-worker"))]
    before_fetch: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    #[cfg(any(test, feature = "conformance-worker"))]
    abort_handle: Mutex<Option<tokio::task::AbortHandle>>,
}

#[derive(Default)]
struct GateEntry {
    last_attempt: Option<Instant>,
    active: Option<watch::Receiver<Option<AttemptResult>>>,
    #[cfg(any(test, feature = "conformance-worker"))]
    joined: usize,
    #[cfg(any(test, feature = "conformance-worker"))]
    starts: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum RefreshOutcome {
    Fetched,
    Joined,
    Throttled,
}

// Sorting is for gate identity only. Key/provider lookup retains caller order.
fn gate_key(service_ids: &[String]) -> String {
    let mut ids = normalized_service_ids(service_ids);
    ids.sort();
    ids.dedup();
    // Length-delimited JSON avoids collisions between IDs containing separators.
    serde_json::to_string(&ids).expect("serializing strings cannot fail")
}

pub(super) fn record_bootstrap(runtime: &SecurityRuntime) {
    runtime
        .refresh_gate
        .entries
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(gate_key(&[]))
        .or_default()
        .last_attempt = Some(Instant::now());
}

pub(super) fn deadline_error() -> HandlerRejection {
    HandlerRejection::new(504, "ERR14017", "JWT verification deadline exceeded")
}

fn fetch_interrupted() -> HandlerRejection {
    // Retain the existing JWKS dependency error family, not an authentication error.
    HandlerRejection::new(502, "ERR10056", "JWKS refresh task did not complete")
}

fn fetch_timeout() -> HandlerRejection {
    HandlerRejection::new(502, "ERR10056", "JWKS refresh timed out")
}

/// Owned before spawn so even dropping an unpolled task cleans up its publication.
struct Completion {
    gate: Arc<JwksRefreshGate>,
    key: String,
    sender: watch::Sender<Option<AttemptResult>>,
    result: Option<AttemptResult>,
}

impl Drop for Completion {
    fn drop(&mut self) {
        let mut entries = self.gate.entries.lock().unwrap_or_else(|e| e.into_inner());
        // Publish and clear atomically with respect to a new caller. Existing receivers
        // retain the terminal value, including when changed() observes channel closure.
        self.sender.send_replace(Some(
            self.result
                .take()
                .unwrap_or_else(|| Err(fetch_interrupted())),
        ));
        if let Some(entry) = entries.get_mut(&self.key) {
            entry.active = None;
        }
        // last_attempt is deliberately retained on success, error, abort, and panic.
    }
}

pub(super) async fn refresh_jwks_gated(
    runtime: &SecurityRuntime,
    service_ids: &[String],
    deadline: Option<Instant>,
) -> Result<RefreshOutcome, HandlerRejection> {
    let now = Instant::now();
    if deadline.is_some_and(|deadline| deadline <= now) {
        return Err(deadline_error());
    }
    let Some(source) = runtime.jwk_source.as_ref() else {
        return Ok(RefreshOutcome::Throttled);
    };
    let timeout = Duration::from_millis(source.request.timeout);
    let key = gate_key(service_ids);
    let (mut receiver, completion, outcome) = {
        let mut entries = runtime
            .refresh_gate
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let entry = entries.entry(key.clone()).or_default();
        if let Some(active) = &entry.active {
            #[cfg(any(test, feature = "conformance-worker"))]
            {
                entry.joined += 1;
            }
            (active.clone(), None, RefreshOutcome::Joined)
        } else if entry.last_attempt.is_some_and(|last| {
            now.duration_since(last)
                < Duration::from_secs(runtime.config.jwks_refresh_min_interval_secs)
        }) {
            return Ok(RefreshOutcome::Throttled);
        } else {
            let (sender, receiver) = watch::channel(None);
            entry.last_attempt = Some(now);
            entry.active = Some(receiver.clone());
            #[cfg(any(test, feature = "conformance-worker"))]
            {
                entry.starts += 1;
            }
            let completion = Completion {
                gate: runtime.refresh_gate.clone(),
                key,
                sender,
                result: None,
            };
            (receiver, Some(completion), RefreshOutcome::Fetched)
        }
    };
    if let Some(completion) = completion {
        let runtime = runtime.clone();
        let service_ids = service_ids.to_vec();
        #[cfg(any(test, feature = "conformance-worker"))]
        let gate = runtime.refresh_gate.clone();
        let task = tokio::spawn(async move {
            // Move the WHOLE guard into the future, not just its result field.
            let mut completion = completion;
            let fetch = async {
                #[cfg(any(test, feature = "conformance-worker"))]
                {
                    let barrier = runtime
                        .refresh_gate
                        .before_fetch
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .take();
                    if let Some(barrier) = barrier {
                        let _ = barrier.await;
                        panic!("test-only JWKS task-boundary panic");
                    }
                }
                refresh_jwks_for_services(&runtime, &service_ids).await
            };
            completion.result = Some(
                tokio::time::timeout(timeout, fetch)
                    .await
                    .unwrap_or_else(|_| Err(fetch_timeout())),
            );
            // Completion drops before the task finishes; no caller must drive cleanup.
        });
        #[cfg(any(test, feature = "conformance-worker"))]
        {
            *gate.abort_handle.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(task.abort_handle());
        }
        drop(task); // Detach; caller cancellation must not abort shared work.
    }
    let wait = async {
        loop {
            if let Some(result) = receiver.borrow_and_update().clone() {
                return result;
            }
            if receiver.changed().await.is_err() {
                // A completed value may exist even if the sender has already dropped.
                return receiver
                    .borrow()
                    .clone()
                    .unwrap_or_else(|| Err(fetch_interrupted()));
            }
        }
    };
    let client_end = now + timeout;
    let end = deadline.map_or(client_end, |deadline| deadline.min(client_end));
    tokio::time::timeout_at(end, wait).await.map_err(|_| {
        if deadline.is_some_and(|deadline| deadline <= client_end) {
            deadline_error()
        } else {
            fetch_timeout()
        }
    })??;
    Ok(outcome)
}

#[cfg(test)]
mod tests;

#[cfg(feature = "conformance-worker")]
pub(crate) mod conformance_worker;
