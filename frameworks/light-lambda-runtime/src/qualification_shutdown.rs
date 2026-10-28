//! Explicit qualification composition only. Ordinary startup never calls this module.
//! Own the notification future; it neither exits the process nor drains a worker.
use reqwest::{Client, Response, redirect::Policy};
use serde_json::{Value, json};
use std::{
    future::Future,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::time::Instant;

// Composition type already used by this runtime. Avoid a new application dependency.
pub use gateway_policy::openapi_operation::SourceFormat;
pub const CLEANUP_CAP: Duration = Duration::from_millis(350);
pub const DEADLINE_RESERVE: Duration = Duration::from_millis(50);
pub const INIT_TIMEOUT: Duration = Duration::from_secs(30);
const BODY_LIMIT: usize = 4096;
const EXTENSION_ID: &str = "lambda-extension-identifier";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Configuration,
    Registration,
    Protocol,
    Transport,
    Signal,
}

/// Only bounded typed state is retained. No response bodies or identifiers enter it.
type Notice = Result<Option<Instant>, Refusal>;
#[derive(Clone, Default)]
pub struct ShutdownState(Arc<Mutex<Option<Notice>>>);
impl ShutdownState {
    pub fn record(&self, notice: Result<Option<Instant>, Refusal>) {
        let mut first = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if first.is_none() {
            *first = Some(notice);
        }
    }
    pub fn failure(&self) -> Option<Refusal> {
        match *self.0.lock().unwrap_or_else(|e| e.into_inner()) {
            Some(Err(error)) => Some(error),
            _ => None,
        }
    }
    pub fn cleanup_deadline(&self, stopped_at: Instant) -> Instant {
        let cap = stopped_at.checked_add(CLEANUP_CAP).unwrap_or(stopped_at);
        match *self.0.lock().unwrap_or_else(|e| e.into_inner()) {
            Some(Ok(Some(deadline))) => cap.min(deadline),
            _ => cap,
        }
    }
}

async fn bounded(mut response: Response) -> Result<Vec<u8>, Refusal> {
    if response
        .content_length()
        .is_some_and(|n| n > BODY_LIMIT as u64)
    {
        return Err(Refusal::Protocol);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| Refusal::Transport)? {
        if chunk.len() > BODY_LIMIT.saturating_sub(body.len()) {
            return Err(Refusal::Protocol);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Captured wall/monotonic clocks prevent recomputing or extending an event budget.
pub fn event_deadline(body: &[u8], wall: SystemTime, now: Instant) -> Result<Instant, Refusal> {
    if body.len() > BODY_LIMIT {
        return Err(Refusal::Protocol);
    }
    let event: Value = serde_json::from_slice(body).map_err(|_| Refusal::Protocol)?;
    if event.get("eventType").and_then(Value::as_str) != Some("SHUTDOWN")
        || !event
            .get("shutdownReason")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty() && s.len() <= 128)
    {
        return Err(Refusal::Protocol);
    }
    let epoch_ms = event
        .get("deadlineMs")
        .and_then(Value::as_u64)
        .ok_or(Refusal::Protocol)?;
    let supplied = UNIX_EPOCH
        .checked_add(Duration::from_millis(epoch_ms))
        .ok_or(Refusal::Protocol)?;
    let remaining = supplied
        .duration_since(wall)
        .unwrap_or_default()
        .saturating_sub(DEADLINE_RESERVE);
    now.checked_add(remaining).ok_or(Refusal::Protocol)
}

pub struct QualificationShutdown {
    client: Client,
    base: String,
    identifier: String,
    pub state: ShutdownState,
}
impl QualificationShutdown {
    /// Before preparation. No redirects, proxies, retries, or detached extension task.
    pub async fn register(
        authority: &str,
        deadline: Instant,
        state: ShutdownState,
    ) -> Result<Self, Refusal> {
        let endpoint = crate::init_error::endpoint(authority, "/2020-01-01/extension/register")
            .map_err(|_| Refusal::Configuration)?;
        let base = endpoint
            .to_string()
            .trim_end_matches("/2020-01-01/extension/register")
            .to_owned();
        let client = Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .build()
            .map_err(|_| Refusal::Configuration)?;
        let request = async {
            let response = client
                .post(endpoint.to_string())
                .header("Lambda-Extension-Name", "validator-lambda-qualification")
                .json(&json!({"events":["SHUTDOWN"]}))
                .send()
                .await
                .map_err(|_| Refusal::Transport)?;
            if response.status() != 200 {
                return Err(Refusal::Registration);
            }
            if response.headers().get_all(EXTENSION_ID).iter().count() != 1 {
                return Err(Refusal::Protocol);
            }
            let identifier = response
                .headers()
                .get(EXTENSION_ID)
                .and_then(|h| h.to_str().ok())
                .filter(|s| {
                    !s.is_empty()
                        && s.len() <= 128
                        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                })
                .ok_or(Refusal::Protocol)?
                .to_owned();
            let body = bounded(response).await?;
            let value: Value = serde_json::from_slice(&body).map_err(|_| Refusal::Protocol)?;
            if !value.is_object() {
                return Err(Refusal::Protocol);
            }
            Ok(Self {
                client,
                base,
                identifier,
                state,
            })
        };
        tokio::time::timeout_at(deadline, request)
            .await
            .map_err(|_| Refusal::Registration)?
    }
    /// Genuine external helper only; the executable name must match this fixed API name.
    pub async fn register_external(
        authority: &str,
        deadline: Instant,
        state: ShutdownState,
    ) -> Result<Self, Refusal> {
        let endpoint = crate::init_error::endpoint(authority, "/2020-01-01/extension/register")
            .map_err(|_| Refusal::Configuration)?;
        let base = endpoint
            .to_string()
            .trim_end_matches("/2020-01-01/extension/register")
            .to_owned();
        let client = Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .build()
            .map_err(|_| Refusal::Configuration)?;
        let request = async {
            let response = client
                .post(endpoint.to_string())
                .header("Lambda-Extension-Name", "validator-shutdown-bridge")
                .json(&json!({"events":["SHUTDOWN"]}))
                .send()
                .await
                .map_err(|_| Refusal::Transport)?;
            if response.status() != 200 {
                return Err(Refusal::Registration);
            }
            if response.headers().get_all(EXTENSION_ID).iter().count() != 1 {
                return Err(Refusal::Protocol);
            }
            let identifier = response
                .headers()
                .get(EXTENSION_ID)
                .and_then(|h| h.to_str().ok())
                .filter(|s| {
                    !s.is_empty()
                        && s.len() <= 128
                        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                })
                .ok_or(Refusal::Protocol)?
                .to_owned();
            let body = bounded(response).await?;
            let value: Value = serde_json::from_slice(&body).map_err(|_| Refusal::Protocol)?;
            if !value.is_object() {
                return Err(Refusal::Protocol);
            }
            Ok(Self {
                client,
                base,
                identifier,
                state,
            })
        };
        tokio::time::timeout_at(deadline, request)
            .await
            .map_err(|_| Refusal::Registration)?
    }
    async fn next(&self) -> Result<Option<Instant>, Refusal> {
        let response = self
            .client
            .get(format!("{}/2020-01-01/extension/event/next", self.base))
            .header(EXTENSION_ID, &self.identifier)
            .send()
            .await
            .map_err(|_| Refusal::Transport)?;
        if response.status() != 200 {
            return Err(Refusal::Protocol);
        }
        let body = bounded(response).await?;
        event_deadline(&body, SystemTime::now(), Instant::now()).map(Some)
    }
    /// Validated platform event for the authenticated external bridge. No API identifier escapes.
    pub async fn external_event(&self) -> Result<Vec<u8>, Refusal> {
        let response = self
            .client
            .get(format!("{}/2020-01-01/extension/event/next", self.base))
            .header(EXTENSION_ID, &self.identifier)
            .send()
            .await
            .map_err(|_| Refusal::Transport)?;
        if response.status() != 200 {
            return Err(Refusal::Protocol);
        }
        let body = bounded(response).await?;
        event_deadline(&body, SystemTime::now(), Instant::now())?;
        Ok(body)
    }

    /// A returned refusal must also make the owner's final status fail.
    pub async fn wait<S>(self, signal: S)
    where
        S: Future<Output = Result<(), Refusal>>,
    {
        let notice = tokio::select! { biased;
            result=self.next()=>result,
            result=signal=>result.map(|()| None),
        };
        self.state.record(notice);
    }
}

/// Install before preparation. Tests use synthetic futures, never process signals.
pub fn termination_signals() -> Result<impl Future<Output = Result<(), Refusal>>, Refusal> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut term = signal(SignalKind::terminate()).map_err(|_| Refusal::Signal)?;
    let mut interrupt = signal(SignalKind::interrupt()).map_err(|_| Refusal::Signal)?;
    Ok(async move {
        let received = tokio::select! { value=term.recv()=>value, value=interrupt.recv()=>value };
        received.ok_or(Refusal::Signal)
    })
}

/// Actual entry coordination; tests supply failing preparation and mock cleanup.
/// No engine/profile is selected here, and no detached task owns termination.
pub async fn run_qualification<P, S, C, F>(
    authority: Option<&str>,
    signals: Result<S, Refusal>,
    prepare: P,
    guard: Option<crate::service::RequestGuard>,
    cleanup: C,
) -> crate::owned_runtime::Outcome
where
    P: FnOnce() -> Result<crate::init::InitInput, crate::init_error::InitError>,
    S: Future<Output = Result<(), Refusal>>,
    C: FnOnce(crate::owned_runtime::ExitKind, Instant) -> F,
    F: Future<Output = Result<(), crate::validator_ipc::Failure>>,
{
    let state = ShutdownState::default();
    let registration = match (&signals, authority) {
        (Ok(_), Some(authority)) => {
            QualificationShutdown::register(authority, Instant::now() + INIT_TIMEOUT, state.clone())
                .await
        }
        _ => Err(Refusal::Configuration),
    };
    if let Err(error) = &registration {
        state.record(Err(*error));
    }
    let ready = registration.is_ok();
    crate::owned_runtime::run_initialized_owned_with_guard(
        move || {
            if !ready {
                return Err(crate::init_error::InitError::InvalidConfiguration(
                    "validator: qualification startup unavailable",
                ));
            }
            prepare()
        },
        guard,
        async move {
            if let (Ok(bridge), Ok(signals)) = (registration, signals) {
                bridge.wait(signals).await;
            } else {
                std::future::pending::<()>().await;
            }
        },
        move |kind, stopped_at| async move {
            let deadline = state.cleanup_deadline(stopped_at);
            tokio::time::timeout_at(deadline, cleanup(kind, deadline))
                .await
                .map_err(|_| crate::validator_ipc::Failure::Deadline)??;
            // A synchronously blocking cleanup can become Ready after the
            // timer elapsed without yielding. Never label that on-time success.
            if Instant::now() >= deadline {
                return Err(crate::validator_ipc::Failure::Deadline);
            }
            if state.failure().is_some() {
                return Err(crate::validator_ipc::Failure::NoReady);
            }
            Ok(())
        },
    )
    .await
}
