//! Unselected owned runner for a trusted deployment profile.
//! Optional typed notice/deadline core; no platform registration or defaults.
use crate::validator_deployment_profile::{ConfiguredProfile, ProfileShutdown};
use futures_util::{FutureExt, future::Shared};
use light_lambda_runtime::qualification_shutdown::Refusal;
use light_lambda_runtime::{
    init_error::InitError,
    owned_runtime::{self, Outcome},
    service::RequestGuard,
    validator_ipc::Failure,
};
use std::{
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::time::Instant;

const POLICY_ERROR: &str = "validator: lifecycle cleanup policy unavailable";
const HANDOFF_ERROR: &str = "validator: lifecycle ownership handoff unavailable";
fn invalid_handoff() -> InitError {
    InitError::InvalidConfiguration(HANDOFF_ERROR)
}

enum State<T> {
    Empty,
    Held(T),
    Drained,
}
/// Retains only a handle, not a second worker authority or process registry.
struct Handoff<T> {
    state: Mutex<State<T>>,
}
impl<T> Handoff<T> {
    fn new() -> Self {
        Self {
            state: Mutex::new(State::Empty),
        }
    }
    fn retain<I>(&self, prepared: Result<(I, T), InitError>) -> Result<I, InitError> {
        let (input, owner) = prepared?;
        let mut state = self.state.lock().map_err(|_| invalid_handoff())?;
        if !matches!(*state, State::Empty) {
            return Err(invalid_handoff());
        }
        *state = State::Held(owner);
        Ok(input)
    }
    fn take(&self) -> (Option<T>, bool) {
        // Even poison must not skip an available owner drain. The sticky flag
        // prevents the operation from being called clean after recovery.
        let (mut state, poisoned) = match self.state.lock() {
            Ok(state) => (state, false),
            Err(error) => (error.into_inner(), true),
        };
        match std::mem::replace(&mut *state, State::Drained) {
            State::Held(owner) => (Some(owner), poisoned),
            State::Empty => (None, poisoned),
            State::Drained => (None, true),
        }
    }
}
fn validate_budget(budget: Duration) -> Result<(), InitError> {
    if budget.is_zero() || Instant::now().checked_add(budget).is_none() {
        return Err(InitError::InvalidConfiguration(POLICY_ERROR));
    }
    Ok(())
}
async fn drain_retained<T, D, F>(
    handoff: Arc<Handoff<T>>,
    stopped_at: Instant,
    budget: Duration,
    drain: D,
) -> Result<(), Failure>
where
    D: FnOnce(T, Instant) -> F,
    F: Future<Output = Result<(), Failure>>,
{
    let (owner, invalid) = handoff.take();
    if let Some(owner) = owner {
        // Charge runtime/shutdown-future destruction to the original stop instant.
        // Overflow fails closed and still attempts the actual drain immediately.
        let deadline = stopped_at.checked_add(budget).unwrap_or(stopped_at);
        let result = tokio::time::timeout_at(deadline, drain(owner, deadline))
            .await
            .map_err(|_| Failure::Deadline)?;
        result?;
        // timeout_at can accept a synchronously ready future past its deadline.
        if Instant::now() >= deadline {
            return Err(Failure::Deadline);
        }
    }
    if invalid {
        return Err(Failure::AdapterInvariant);
    }
    Ok(())
}

/// Retain and await this owning future. Resolve `shutdown` to stop; do not abort
/// or drop the owning future and infer that its precision child was reaped.
/// `prepare` may only construct/attach a profile, not start independent work.
/// No signal handler, helper, production values or platform event is installed.
/// The supplied budget is trusted policy, not an AWS SHUTDOWN deadline clamp.
/// Actual platform integration must separately honor the platform's earlier
/// deadline and remain unavailable until that lifecycle contract is qualified.
/// Callback success is a control result, never independent child-wait evidence.
pub async fn run_initialized_owned_profile<P, S>(
    prepare: P,
    guard: Option<RequestGuard>,
    shutdown: S,
    cleanup_budget: Duration,
) -> Result<Outcome, InitError>
where
    P: FnOnce() -> Result<ConfiguredProfile, InitError>,
    S: Future<Output = ()>,
{
    validate_budget(cleanup_budget)?;
    let handoff = Arc::new(Handoff::<ProfileShutdown>::new());
    let prepared = handoff.clone();
    Ok(owned_runtime::run_initialized_owned_with_guard(
        move || prepared.retain(prepare().map(ConfiguredProfile::into_parts)),
        guard,
        shutdown,
        move |kind, stopped_at| {
            drain_retained(
                handoff,
                stopped_at,
                cleanup_budget,
                move |owner, deadline| async move {
                    owner.request_completion(kind);
                    owner.shutdown(deadline).await.map(|_| ())
                },
            )
        },
    )
    .await)
}

// A trusted adapter supplies a monotonic deadline or a typed notification refusal.
// No raw AWS/HTTP bytes, wall-clock conversion or transport authentication here.
type Notice = Result<Instant, Refusal>;
fn clamped_deadline(local: Instant, notice: Option<Notice>) -> Instant {
    match notice {
        Some(Ok(platform)) => local.min(platform),
        _ => local,
    }
}
async fn drain_with_platform<T, D, F, N>(
    handoff: Arc<Handoff<T>>,
    stopped_at: Instant,
    budget: Duration,
    notification: Shared<N>,
    drain: D,
) -> Result<(), Failure>
where
    D: FnOnce(T, Instant) -> F,
    F: Future<Output = Result<(), Failure>>,
    N: Future<Output = Notice>,
{
    let local = stopped_at.checked_add(budget).unwrap_or(stopped_at);
    let mut notice = notification.peek().copied();
    let first_deadline = clamped_deadline(local, notice);
    let mut receiving = Box::pin(notification);
    let mut draining = Box::pin(drain_retained(
        handoff,
        stopped_at,
        first_deadline.saturating_duration_since(stopped_at),
        drain,
    ));
    loop {
        let deadline = clamped_deadline(local, notice);
        let result = if notice.is_none() {
            tokio::select! { biased;
                value=receiving.as_mut()=>{ notice=Some(value); continue; },
                result=tokio::time::timeout_at(deadline, draining.as_mut())=>result,
            }
        } else {
            tokio::time::timeout_at(deadline, draining.as_mut()).await
        };
        result.map_err(|_| Failure::Deadline)??;
        if notice.is_some_and(|n| n.is_err()) {
            return Err(Failure::NoReady);
        }
        if Instant::now() >= clamped_deadline(local, notice) {
            return Err(Failure::Deadline);
        }
        return Ok(());
    }
}

/// Unselected deadline-aware composition. `notification` MUST originate from a
/// trusted adapter that has authenticated and validated the platform event and
/// converted its epoch deadline once with the reviewed reserve. It is not a
/// raw event API and neither registers nor authenticates an extension.
/// The shared notice remains owned after the SDK/shutdown future is dropped,
/// and is polled during drain, without a detached task or replacement worker.
/// A late earlier deadline cancels the pending drain WAIT, not the backend or
/// worker. A cancelled wait is failure, never cleanup/reap proof. The process
/// ownership registry still retains any unfinished actual work.
/// No bridge ACK is sent here. Transport completion and helper ownership remain
/// the adapter's duty; a control Outcome is not platform lifecycle qualification.
/// Await this owning future; abort/drop remains outside the cleanup guarantee.
pub async fn run_initialized_owned_profile_with_platform_notice<P, N>(
    prepare: P,
    guard: Option<RequestGuard>,
    notification: N,
    cleanup_budget: Duration,
) -> Result<Outcome, InitError>
where
    P: FnOnce() -> Result<ConfiguredProfile, InitError>,
    N: Future<Output = Result<Instant, Refusal>>,
{
    Ok(
        run_profile_with_notice_receipt(prepare, guard, notification, cleanup_budget)
            .await?
            .outcome,
    )
}
struct NoticeRun {
    outcome: Outcome,
    stopped_at: Option<Instant>,
}
async fn run_profile_with_notice_receipt<P, N>(
    prepare: P,
    guard: Option<RequestGuard>,
    notification: N,
    cleanup_budget: Duration,
) -> Result<NoticeRun, InitError>
where
    P: FnOnce() -> Result<ConfiguredProfile, InitError>,
    N: Future<Output = Result<Instant, Refusal>>,
{
    validate_budget(cleanup_budget)?;
    let notification = notification.shared();
    let stopping = notification.clone();
    let handoff = Arc::new(Handoff::<ProfileShutdown>::new());
    let prepared = handoff.clone();
    let stop = Arc::new(std::sync::OnceLock::new());
    let recording = stop.clone();
    let outcome = owned_runtime::run_initialized_owned_with_guard(
        move || prepared.retain(prepare().map(ConfiguredProfile::into_parts)),
        guard,
        async move {
            let _notice = stopping.await;
        },
        move |kind, stopped_at| async move {
            let duplicate = recording.set(stopped_at).is_err();
            let result = drain_with_platform(
                handoff,
                stopped_at,
                cleanup_budget,
                notification,
                move |owner, deadline| async move {
                    owner.request_completion(kind);
                    owner.shutdown(deadline).await.map(|_| ())
                },
            )
            .await;
            result?;
            if duplicate {
                return Err(Failure::AdapterInvariant);
            }
            Ok(())
        },
    )
    .await;
    Ok(NoticeRun {
        outcome,
        stopped_at: stop.get().copied(),
    })
}

/// Separate control result; ACK is neither a replacement for the primary error
/// nor proof the helper exited or its owning process reaped it.
pub struct AuthenticatedRun {
    pub outcome: Outcome,
    pub acknowledgement: Result<(), Refusal>,
}
impl AuthenticatedRun {
    pub fn completed_without_failure(&self) -> bool {
        self.outcome.completed_without_failure() && self.acknowledgement.is_ok()
    }
}
async fn finish_ack_until<F>(stopped_at: Instant, budget: Duration, ack: F) -> Result<(), Refusal>
where
    F: Future<Output = Result<(), Refusal>>,
{
    let deadline = stopped_at.checked_add(budget).unwrap_or(stopped_at);
    tokio::time::timeout_at(deadline, ack)
        .await
        .map_err(|_| Refusal::Transport)??;
    if Instant::now() >= deadline {
        return Err(Refusal::Transport);
    }
    Ok(())
}

/// Source-selected EXISTING experimental bridge only; no production policy or
/// registration/default selection. Channel originates from a unique Parent
/// whose constructor already verified manifest, images, peer UID/PID identity.
/// No signals selected here. Actual helper startup/owner waits remain separate.
pub async fn run_profile_with_authenticated_qualification_channel<P>(
    prepare: P,
    guard: Option<RequestGuard>,
    mut channel: light_lambda_runtime::qualification_extension_bridge::ProfileChannel,
    cleanup_budget: Duration,
) -> Result<AuthenticatedRun, InitError>
where
    P: FnOnce() -> Result<ConfiguredProfile, InitError>,
{
    validate_budget(cleanup_budget)?;
    // Explicitly reuse only the already-defined qualified transport cap; never
    // silently promote that experimental number into ordinary startup policy.
    if cleanup_budget > light_lambda_runtime::qualification_shutdown::CLEANUP_CAP {
        return Err(InitError::InvalidConfiguration(POLICY_ERROR));
    }
    let receipt =
        run_profile_with_notice_receipt(prepare, guard, channel.notice(), cleanup_budget).await?;
    // All shared notice futures are dropped before the exclusive channel is
    // borrowed for DONE/ACK. No concurrent readers or budget rebasing.
    let acknowledgement = match receipt.stopped_at {
        Some(stop) => {
            finish_ack_until(
                stop,
                cleanup_budget,
                channel.finish(receipt.outcome.completed_without_failure(), stop),
            )
            .await
        }
        None => Err(Refusal::Protocol),
    };
    Ok(AuthenticatedRun {
        outcome: receipt.outcome,
        acknowledgement,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    struct Witness(Arc<AtomicUsize>);
    impl Drop for Witness {
        fn drop(&mut self) {
            self.0.fetch_add(1, SeqCst);
        }
    }
    fn held() -> (Arc<Handoff<Witness>>, Arc<AtomicUsize>) {
        let slot = Arc::new(Handoff::new());
        let drops = Arc::new(AtomicUsize::new(0));
        assert_eq!(slot.retain(Ok((7, Witness(drops.clone())))).unwrap(), 7);
        (slot, drops)
    }
    #[test]
    fn handle_retained_before_input_returned() {
        let (slot, drops) = held();
        assert_eq!(drops.load(SeqCst), 0);
        assert!(matches!(*slot.state.lock().unwrap(), State::Held(_)));
    }
    #[test]
    fn preparation_failure_keeps_original_error_and_no_handle() {
        let slot = Handoff::<()>::new();
        let result = slot.retain::<()>(Err(InitError::InvalidConfiguration(
            "synthetic setup failure",
        )));
        assert!(matches!(
            result,
            Err(InitError::InvalidConfiguration("synthetic setup failure"))
        ));
        assert!(matches!(*slot.state.lock().unwrap(), State::Empty));
    }
    #[test]
    fn second_preparation_cannot_displace_retained_handle() {
        let (slot, drops) = held();
        assert!(slot.retain(Ok((9, Witness(drops.clone())))).is_err());
        assert_eq!(drops.load(SeqCst), 1);
        assert!(matches!(*slot.state.lock().unwrap(), State::Held(_)));
    }
    #[tokio::test]
    async fn cleanup_receives_original_stop_deadline_and_retained_handle_once() {
        let (slot, drops) = held();
        let calls = Arc::new(AtomicUsize::new(0));
        let called = calls.clone();
        let seen = drops.clone();
        let stop = Instant::now();
        let budget = Duration::from_secs(30);
        let result = drain_retained(slot.clone(), stop, budget, move |owner, end| async move {
            assert_eq!(end, stop + budget);
            assert_eq!(seen.load(SeqCst), 0);
            called.fetch_add(1, SeqCst);
            drop(owner);
            Ok(())
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(calls.load(SeqCst), 1);
        assert_eq!(drops.load(SeqCst), 1);
        let repeated = drain_retained(slot, stop, budget, |_, _| async {
            panic!("must not drain twice")
        })
        .await;
        assert_eq!(repeated, Err(Failure::AdapterInvariant));
    }
    #[tokio::test]
    async fn setup_failure_without_owner_does_not_invent_a_drain() {
        let slot = Arc::new(Handoff::<()>::new());
        let result = drain_retained(slot, Instant::now(), Duration::from_secs(1), |_, _| async {
            panic!("no owner to drain")
        })
        .await;
        assert!(result.is_ok());
    }
    #[tokio::test]
    async fn engine_failure_is_preserved() {
        let (slot, drops) = held();
        let result = drain_retained(
            slot,
            Instant::now(),
            Duration::from_secs(30),
            |_, _| async { Err(Failure::NoReady) },
        )
        .await;
        assert_eq!(result, Err(Failure::NoReady));
        assert_eq!(drops.load(SeqCst), 1);
    }
    #[tokio::test]
    async fn past_deadline_still_attempts_cleanup_but_cannot_report_success() {
        let (slot, _) = held();
        let calls = Arc::new(AtomicUsize::new(0));
        let called = calls.clone();
        let stop = Instant::now() - Duration::from_secs(10);
        let result = drain_retained(
            slot,
            stop,
            Duration::from_secs(1),
            move |_, end| async move {
                assert_eq!(end, stop + Duration::from_secs(1));
                called.fetch_add(1, SeqCst);
                Ok(())
            },
        )
        .await;
        assert_eq!(result, Err(Failure::Deadline));
        assert_eq!(calls.load(SeqCst), 1);
    }
    #[tokio::test]
    async fn pending_cleanup_timeout_is_not_reap_proof() {
        let (slot, drops) = held();
        let stop = Instant::now() - Duration::from_secs(10);
        let result = drain_retained(slot, stop, Duration::from_secs(1), |owner, _| async move {
            let _retained = owner;
            std::future::pending::<Result<(), Failure>>().await
        })
        .await;
        assert_eq!(result, Err(Failure::Deadline));
        // Dropping the synthetic handle is not labelled child cleanup/reap.
        assert_eq!(drops.load(SeqCst), 1);
    }
    #[tokio::test]
    async fn deadline_overflow_still_attempts_drain_and_is_not_success() {
        let (slot, _) = held();
        let calls = Arc::new(AtomicUsize::new(0));
        let called = calls.clone();
        let result = drain_retained(
            slot,
            Instant::now(),
            Duration::MAX,
            move |_, _| async move {
                called.fetch_add(1, SeqCst);
                Ok(())
            },
        )
        .await;
        assert_eq!(result, Err(Failure::Deadline));
        assert_eq!(calls.load(SeqCst), 1);
    }
    #[tokio::test]
    async fn poisoned_handoff_still_drains_but_fails_closed() {
        let (slot, drops) = held();
        let poisoned = slot.clone();
        assert!(
            std::thread::spawn(move || {
                let _lock = poisoned.state.lock().unwrap();
                panic!("synthetic poison");
            })
            .join()
            .is_err()
        );
        let result = drain_retained(
            slot,
            Instant::now(),
            Duration::from_secs(30),
            |_, _| async { Ok(()) },
        )
        .await;
        assert_eq!(result, Err(Failure::AdapterInvariant));
        assert_eq!(drops.load(SeqCst), 1);
    }
    #[tokio::test]
    async fn zero_budget_refuses_before_prepare_or_runtime() {
        let result = run_initialized_owned_profile(
            || panic!("must not prepare"),
            None,
            std::future::pending(),
            Duration::ZERO,
        )
        .await;
        assert!(matches!(
            result,
            Err(InitError::InvalidConfiguration(POLICY_ERROR))
        ));
    }
    #[tokio::test]
    async fn overflow_budget_refuses_before_prepare_or_runtime() {
        let result = run_initialized_owned_profile(
            || panic!("must not prepare"),
            None,
            std::future::pending(),
            Duration::MAX,
        )
        .await;
        assert!(matches!(
            result,
            Err(InitError::InvalidConfiguration(POLICY_ERROR))
        ));
    }
    #[test]
    fn platform_deadline_never_extends_local_budget() {
        let local = Instant::now() + Duration::from_secs(10);
        assert_eq!(
            clamped_deadline(local, Some(Ok(local + Duration::from_secs(1)))),
            local
        );
        assert_eq!(
            clamped_deadline(local, Some(Ok(local - Duration::from_secs(1)))),
            local - Duration::from_secs(1)
        );
        assert_eq!(clamped_deadline(local, None), local);
    }
    #[tokio::test]
    async fn already_observed_notice_passes_earlier_deadline_to_drain() {
        let (slot, _) = held();
        let stop = Instant::now();
        let end = stop + Duration::from_secs(5);
        let notice = std::future::ready(Ok(end)).shared();
        assert_eq!(notice.clone().now_or_never(), Some(Ok(end)));
        let result = drain_with_platform(
            slot,
            stop,
            Duration::from_secs(30),
            notice,
            move |_, deadline| async move {
                assert_eq!(deadline, end);
                Ok(())
            },
        )
        .await;
        assert!(result.is_ok());
    }
    #[tokio::test]
    async fn notice_survives_shutdown_clone_drop_and_polls_during_cleanup() {
        let (slot, _) = held();
        let stop = Instant::now();
        let end = stop + Duration::from_secs(5);
        let (send, receive) = tokio::sync::oneshot::channel();
        let notice = async move { receive.await.unwrap() }.shared();
        let stopping = notice.clone();
        drop(stopping);
        let result = drain_with_platform(
            slot,
            stop,
            Duration::from_secs(30),
            notice,
            move |_, _| async move {
                send.send(Ok(end)).unwrap();
                tokio::task::yield_now().await;
                Ok(())
            },
        )
        .await;
        assert!(result.is_ok());
    }
    #[tokio::test]
    async fn late_expired_notice_stops_pending_wait_without_claiming_reap() {
        let (slot, drops) = held();
        let stop = Instant::now();
        let (send, receive) = tokio::sync::oneshot::channel();
        let notice = async move { receive.await.unwrap() }.shared();
        let result = drain_with_platform(
            slot,
            stop,
            Duration::from_secs(30),
            notice,
            move |owner, _| async move {
                let _owner = owner;
                send.send(Ok(stop - Duration::from_secs(1))).unwrap();
                std::future::pending::<Result<(), Failure>>().await
            },
        )
        .await;
        assert_eq!(result, Err(Failure::Deadline));
        assert_eq!(drops.load(SeqCst), 1);
    }
    #[tokio::test]
    async fn malformed_notification_does_not_skip_cleanup_or_return_success() {
        let (slot, drops) = held();
        let result = drain_with_platform(
            slot,
            Instant::now(),
            Duration::from_secs(30),
            std::future::ready(Err(Refusal::Protocol)).shared(),
            |_, _| async { Ok(()) },
        )
        .await;
        assert_eq!(result, Err(Failure::NoReady));
        assert_eq!(drops.load(SeqCst), 1);
    }
    #[tokio::test]
    async fn pending_notice_does_not_block_already_completed_cleanup() {
        let (slot, _) = held();
        let result = drain_with_platform(
            slot,
            Instant::now(),
            Duration::from_secs(30),
            std::future::pending::<Notice>().shared(),
            |_, _| async { Ok(()) },
        )
        .await;
        assert!(result.is_ok());
    }
    #[tokio::test]
    async fn deadline_does_not_mask_existing_drain_failure() {
        let (slot, _) = held();
        let result = drain_with_platform(
            slot,
            Instant::now(),
            Duration::from_secs(30),
            std::future::pending::<Notice>().shared(),
            |_, _| async { Err(Failure::NoReady) },
        )
        .await;
        assert_eq!(result, Err(Failure::NoReady));
    }
    #[tokio::test]
    async fn invalid_platform_budget_rejects_before_prepare() {
        let result = run_initialized_owned_profile_with_platform_notice(
            || panic!("must not prepare"),
            None,
            std::future::pending::<Notice>(),
            Duration::ZERO,
        )
        .await;
        assert!(matches!(
            result,
            Err(InitError::InvalidConfiguration(POLICY_ERROR))
        ));
    }
    fn control_outcome() -> Outcome {
        Outcome {
            primary: owned_runtime::Primary::Completed,
            cleanup: Ok(()),
            init_report: None,
            owned_future_drop_panicked: false,
        }
    }
    #[test]
    fn successful_primary_with_failed_ack_is_not_success() {
        let result = AuthenticatedRun {
            outcome: control_outcome(),
            acknowledgement: Err(Refusal::Protocol),
        };
        assert!(!result.completed_without_failure());
        assert!(matches!(
            result.outcome.primary,
            owned_runtime::Primary::Completed
        ));
    }
    #[test]
    fn successful_ack_does_not_mask_primary_or_cleanup_failure() {
        let mut result = AuthenticatedRun {
            outcome: control_outcome(),
            acknowledgement: Ok(()),
        };
        result.outcome.primary = owned_runtime::Primary::RuntimeTerminated;
        assert!(!result.completed_without_failure());
        result.outcome.primary = owned_runtime::Primary::Completed;
        result.outcome.cleanup = Err(owned_runtime::CleanupFailure::Engine(Failure::NoReady));
        assert!(!result.completed_without_failure());
    }
    #[tokio::test]
    async fn ack_success_after_local_cap_is_not_accepted() {
        let stop = Instant::now() - Duration::from_secs(10);
        assert_eq!(
            finish_ack_until(stop, Duration::from_secs(1), std::future::ready(Ok(()))).await,
            Err(Refusal::Transport)
        );
    }
    #[tokio::test]
    async fn ack_wait_does_not_get_a_new_cleanup_budget() {
        let stop = Instant::now() - Duration::from_secs(10);
        assert_eq!(
            finish_ack_until(stop, Duration::from_secs(1), std::future::pending()).await,
            Err(Refusal::Transport)
        );
    }
}
