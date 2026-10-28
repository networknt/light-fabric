//! Proposed opt-in owned termination seam, not signal/extension registration.
//! The caller retains/awaits this future, supplies shutdown notification and an
//! actual owner-drain callback. Callback success alone is not child-reap proof.
use crate::{
    init::{InitInput, Snapshot},
    init_error::{self, InitError},
};
use futures_util::FutureExt;
use lambda_runtime::{LambdaEvent, Runtime, service_fn};
use serde_json::Value;
use std::{future::Future, panic::AssertUnwindSafe, sync::Arc};
use tokio::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitKind {
    Completed,
    ShutdownRequested,
    InitFailure,
    RuntimeFailure,
}
pub enum Primary {
    Completed,
    ShutdownRequested,
    Init(InitError),
    RuntimeTerminated,
}
impl Primary {
    pub fn kind(&self) -> ExitKind {
        match self {
            Self::Completed => ExitKind::Completed,
            Self::ShutdownRequested => ExitKind::ShutdownRequested,
            Self::Init(_) => ExitKind::InitFailure,
            Self::RuntimeTerminated => ExitKind::RuntimeFailure,
        }
    }
}
#[derive(Debug, PartialEq, Eq)]
pub enum CleanupFailure {
    Engine(crate::validator_ipc::Failure),
    Panic,
}
pub struct Outcome {
    pub primary: Primary,
    pub cleanup: Result<(), CleanupFailure>,
    pub init_report: Option<init_error::ReportOutcome>,
    pub owned_future_drop_panicked: bool,
}
impl Outcome {
    /// Return/control status only; not proof of actual child cleanup or AWS exit.
    pub fn completed_without_failure(&self) -> bool {
        matches!(
            self.primary,
            Primary::Completed | Primary::ShutdownRequested
        ) && self.cleanup.is_ok()
            && !self.owned_future_drop_panicked
    }
}

async fn finish_owned<R, S, C, F>(run: R, shutdown: S, cleanup: C) -> Outcome
where
    R: Future<Output = Primary>,
    S: Future<Output = ()>,
    C: FnOnce(ExitKind, Instant) -> F,
    F: Future<Output = Result<(), crate::validator_ipc::Failure>>,
{
    let mut running = Box::pin(AssertUnwindSafe(run).catch_unwind());
    let mut stopping = Box::pin(AssertUnwindSafe(shutdown).catch_unwind());
    // A completed primary failure wins over a concurrently ready shutdown.
    let mut primary = tokio::select! {biased;
        result=running.as_mut()=>result.unwrap_or(Primary::RuntimeTerminated),
        result=stopping.as_mut()=>if result.is_ok() { Primary::ShutdownRequested } else { Primary::RuntimeTerminated },
    };
    let stopped_at = Instant::now();
    // Charge cancellation/drop time to the same stop instant. Drop both owned
    // futures even if one destructor panics; never skip the drain callback.
    let runtime_drop = std::panic::catch_unwind(AssertUnwindSafe(|| drop(running)));
    let shutdown_drop = std::panic::catch_unwind(AssertUnwindSafe(|| drop(stopping)));
    let owned_future_drop_panicked = runtime_drop.is_err() || shutdown_drop.is_err();
    if owned_future_drop_panicked
        && matches!(primary, Primary::Completed | Primary::ShutdownRequested)
    {
        primary = Primary::RuntimeTerminated;
    }
    let kind = primary.kind();
    let cleanup = match std::panic::catch_unwind(AssertUnwindSafe(|| cleanup(kind, stopped_at))) {
        Err(_) => Err(CleanupFailure::Panic),
        Ok(future) => {
            let mut cleaning = Box::pin(AssertUnwindSafe(future).catch_unwind());
            let result = cleaning.as_mut().await;
            let dropped = std::panic::catch_unwind(AssertUnwindSafe(|| drop(cleaning)));
            if dropped.is_err() {
                Err(CleanupFailure::Panic)
            } else {
                match result {
                    Err(_) => Err(CleanupFailure::Panic),
                    Ok(result) => result.map_err(CleanupFailure::Engine),
                }
            }
        }
    };
    Outcome {
        primary,
        cleanup,
        init_report: None,
        owned_future_drop_panicked,
    }
}

async fn sdk_returning(
    snapshot: Arc<Snapshot>,
    guard: Option<crate::service::RequestGuard>,
) -> Primary {
    let runtime = std::panic::catch_unwind(AssertUnwindSafe(|| {
        let authority = std::env::var("AWS_LAMBDA_RUNTIME_API").map_err(|_| {
            InitError::InvalidConfiguration("runtime: missing AWS_LAMBDA_RUNTIME_API")
        })?;
        init_error::endpoint(&authority, "/")?;
        crate::init::Environment::from_env()?;
        let handler = service_fn(move |event: LambdaEvent<Value>| {
            let snapshot = snapshot.clone();
            let guard = guard.clone();
            async move { crate::service::handle_with_guard(snapshot, event, guard.as_ref()).await }
        });
        Ok::<_, InitError>(Runtime::new(handler))
    }))
    .unwrap_or(Err(InitError::Panic));
    match runtime {
        Err(error) => Primary::Init(error),
        Ok(runtime) => match runtime.run().await {
            Ok(()) => Primary::Completed,
            Err(_) => Primary::RuntimeTerminated,
        },
    }
}

/// Additive entry only: defaults/current bootstrap remain unchanged. No stdin,
/// process exit, new deadline, signal handling or extension protocol is selected.
/// Drain gets the first stop instant; caller chooses policy/deadline explicitly.
/// Do not abort this owning future to request shutdown: resolve `shutdown` and
/// await its returned Outcome. Actual wait/reap evidence is the drain's duty.
pub async fn run_initialized_owned_with_guard<P, S, C, F>(
    prepare: P,
    guard: Option<crate::service::RequestGuard>,
    shutdown: S,
    cleanup: C,
) -> Outcome
where
    P: FnOnce() -> Result<InitInput, InitError>,
    S: Future<Output = ()>,
    C: FnOnce(ExitKind, Instant) -> F,
    F: Future<Output = Result<(), crate::validator_ipc::Failure>>,
{
    crate::telemetry::install_panic_redaction();
    let run = async {
        let initialized = AssertUnwindSafe(async {
            crate::telemetry::initialize()?;
            let input = std::panic::catch_unwind(AssertUnwindSafe(prepare))
                .unwrap_or(Err(InitError::Panic))?;
            crate::init::initialize(input).await
        })
        .catch_unwind()
        .await
        .unwrap_or(Err(InitError::Panic));
        match initialized {
            Ok(snapshot) => sdk_returning(snapshot, guard).await,
            Err(error) => Primary::Init(error),
        }
    };
    let mut outcome = finish_owned(run, shutdown, cleanup).await;
    if let Primary::Init(error) = &outcome.primary {
        let authority = std::env::var("AWS_LAMBDA_RUNTIME_API").ok();
        let reported = init_error::report(error, authority.as_deref()).await;
        tracing::error!(target:"light_lambda::init",error_type=error.error_type(),message=error.message(),report_outcome=?reported);
        outcome.init_report = Some(reported);
    } else if matches!(outcome.primary, Primary::RuntimeTerminated) {
        tracing::error!(target:"light_lambda::runtime",message="runtime transport terminated");
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validator_ipc::Failure;
    use std::{
        pin::Pin,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst},
        task::{Context, Poll},
    };
    struct Witness {
        ready: bool,
        dropped: Arc<AtomicBool>,
    }
    impl Future for Witness {
        type Output = Primary;
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Primary> {
            if self.ready {
                Poll::Ready(Primary::Completed)
            } else {
                Poll::Pending
            }
        }
    }
    impl Drop for Witness {
        fn drop(&mut self) {
            self.dropped.store(true, SeqCst);
        }
    }
    #[tokio::test]
    async fn runtime_is_dropped_before_single_cleanup_call() {
        let dropped = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let d = dropped.clone();
        let c = calls.clone();
        let result = finish_owned(
            Witness {
                ready: true,
                dropped,
            },
            std::future::pending(),
            move |kind, stop| async move {
                assert!(d.load(SeqCst));
                assert_eq!(kind, ExitKind::Completed);
                assert!(stop <= Instant::now());
                c.fetch_add(1, SeqCst);
                Ok(())
            },
        )
        .await;
        assert!(result.completed_without_failure());
        assert_eq!(calls.load(SeqCst), 1);
    }
    #[tokio::test]
    async fn explicit_shutdown_drops_pending_runtime_before_cleanup() {
        let dropped = Arc::new(AtomicBool::new(false));
        let d = dropped.clone();
        let result = finish_owned(
            Witness {
                ready: false,
                dropped,
            },
            async {},
            move |kind, _| async move {
                assert!(d.load(SeqCst));
                assert_eq!(kind, ExitKind::ShutdownRequested);
                Ok(())
            },
        )
        .await;
        assert!(matches!(result.primary, Primary::ShutdownRequested));
    }
    #[tokio::test]
    async fn completed_init_failure_wins_over_ready_shutdown() {
        let result = finish_owned(
            async { Primary::Init(InitError::Preparation) },
            async {},
            |_, _| async { Ok(()) },
        )
        .await;
        assert!(matches!(
            result.primary,
            Primary::Init(InitError::Preparation)
        ));
        assert!(!result.completed_without_failure());
    }
    #[tokio::test]
    async fn cleanup_failure_cannot_replace_primary_init_error() {
        let result = finish_owned(
            async { Primary::Init(InitError::Backend) },
            std::future::pending(),
            |kind, _| async move {
                assert_eq!(kind, ExitKind::InitFailure);
                Err(Failure::NoReady)
            },
        )
        .await;
        assert!(matches!(result.primary, Primary::Init(InitError::Backend)));
        assert_eq!(
            result.cleanup,
            Err(CleanupFailure::Engine(Failure::NoReady))
        );
    }
    #[tokio::test]
    async fn runtime_failure_retained_with_cleanup_failure() {
        let result = finish_owned(
            async { Primary::RuntimeTerminated },
            std::future::pending(),
            |_, _| async { Err(Failure::Deadline) },
        )
        .await;
        assert!(matches!(result.primary, Primary::RuntimeTerminated));
        assert!(!result.completed_without_failure());
    }
    #[tokio::test]
    async fn successful_primary_with_failed_cleanup_is_not_success() {
        let result = finish_owned(
            async { Primary::Completed },
            std::future::pending(),
            |_, _| async { Err(Failure::NoReady) },
        )
        .await;
        assert!(!result.completed_without_failure());
    }
    fn panic_constructor(_: ExitKind, _: Instant) -> std::future::Ready<Result<(), Failure>> {
        panic!("synthetic constructor panic")
    }
    #[tokio::test]
    async fn cleanup_constructor_panic_is_typed_without_primary_loss() {
        let result = finish_owned(
            async { Primary::Init(InitError::Panic) },
            std::future::pending(),
            panic_constructor,
        )
        .await;
        assert!(matches!(result.primary, Primary::Init(InitError::Panic)));
        assert_eq!(result.cleanup, Err(CleanupFailure::Panic));
    }
    #[tokio::test]
    async fn cleanup_future_panic_is_typed() {
        let result = finish_owned(
            async { Primary::Completed },
            std::future::pending(),
            |_, _| async {
                assert!(std::hint::black_box(false), "synthetic future panic");
                Ok(())
            },
        )
        .await;
        assert_eq!(result.cleanup, Err(CleanupFailure::Panic));
        assert!(!result.completed_without_failure());
    }
    #[tokio::test]
    async fn runtime_panic_has_no_payload_in_primary() {
        let result = finish_owned(
            async {
                assert!(
                    std::hint::black_box(false),
                    "synthetic private runtime marker"
                );
                Primary::Completed
            },
            std::future::pending(),
            |_, _| async { Ok(()) },
        )
        .await;
        assert!(matches!(result.primary, Primary::RuntimeTerminated));
        assert!(!result.completed_without_failure());
    }
    #[tokio::test]
    async fn primary_error_keeps_existing_public_redaction_and_no_automatic_mock_report() {
        let result = finish_owned(
            async { Primary::Init(InitError::UnknownHandler("synthetic/private".into())) },
            std::future::pending(),
            |_, _| async { Ok(()) },
        )
        .await;
        let Primary::Init(error) = result.primary else {
            panic!("missing primary")
        };
        assert!(
            !String::from_utf8(error.payload())
                .unwrap()
                .contains("synthetic/private")
        );
        assert!(result.init_report.is_none());
    }
    #[tokio::test]
    async fn shutdown_future_panic_still_calls_cleanup_once() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let result = finish_owned(
            std::future::pending(),
            async {
                assert!(std::hint::black_box(false), "synthetic shutdown panic");
            },
            move |kind, _| async move {
                assert_eq!(kind, ExitKind::RuntimeFailure);
                c.fetch_add(1, SeqCst);
                Ok(())
            },
        )
        .await;
        assert!(matches!(result.primary, Primary::RuntimeTerminated));
        assert_eq!(calls.load(SeqCst), 1);
        assert!(!result.completed_without_failure());
    }
    struct DropFault;
    impl Future for DropFault {
        type Output = Primary;
        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Primary> {
            Poll::Ready(Primary::Init(InitError::Backend))
        }
    }
    impl Drop for DropFault {
        fn drop(&mut self) {
            panic!("synthetic runtime drop panic");
        }
    }
    #[tokio::test]
    async fn runtime_drop_panic_does_not_skip_cleanup_or_replace_init_error() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let result = finish_owned(DropFault, std::future::pending(), move |_, _| async move {
            c.fetch_add(1, SeqCst);
            Ok(())
        })
        .await;
        assert!(matches!(result.primary, Primary::Init(InitError::Backend)));
        assert!(result.owned_future_drop_panicked);
        assert_eq!(calls.load(SeqCst), 1);
        assert!(!result.completed_without_failure());
    }
}
