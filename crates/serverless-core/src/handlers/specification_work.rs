//! Request-only bounded normalization, shared across every Specification instance.
use crate::StatusError;
use gateway_policy::openapi_operation::{BasePathProfile, MAX_NORMALIZATION_PATH_BYTES};
use std::sync::{Arc, LazyLock};
use tokio::{sync::Semaphore, time::Instant};
use tokio_util::sync::CancellationToken;

static WORKERS: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(2)));

pub(super) fn budget(deadline: Instant, cancel: &CancellationToken) -> Result<(), StatusError> {
    if cancel.is_cancelled() || Instant::now() >= deadline {
        Err(StatusError::timeout())
    } else {
        Ok(())
    }
}

pub(super) async fn normalize(
    profile: Arc<BasePathProfile>,
    path: &str,
    deadline: Instant,
    cancel: &CancellationToken,
) -> Result<String, StatusError> {
    run(path, deadline, cancel, move |input| {
        #[cfg(test)]
        tests::panic_if_requested(&input);
        #[cfg(test)]
        tests::wait_if_requested(&input);
        profile
            .normalize_bounded_path(&input)
            .map_err(|_| StatusError::specification_path_too_large())
    })
    .await
}

// A private shared orchestration seam lets lifecycle tests control actual jobs.
// No configuration-selected actions/plugins or worker injection API exists.
async fn run<F>(
    path: &str,
    deadline: Instant,
    cancel: &CancellationToken,
    job: F,
) -> Result<String, StatusError>
where
    F: FnOnce(String) -> Result<String, StatusError> + Send + 'static,
{
    budget(deadline, cancel)?;
    if path.len() > MAX_NORMALIZATION_PATH_BYTES {
        return Err(StatusError::specification_path_too_large());
    }
    let permit = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(StatusError::timeout()),
        _ = tokio::time::sleep_until(deadline) => return Err(StatusError::timeout()),
        permit = WORKERS.clone().acquire_owned() => permit.map_err(|_| StatusError::handler_failure())?,
    };
    budget(deadline, cancel)?;
    #[cfg(test)]
    tests::COPIES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let input = path.to_owned();
    // Recheck immediately before submission; no task on cancellation during copy.
    budget(deadline, cancel)?;
    let worker = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        job(input)
    });
    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(StatusError::timeout()),
        _ = tokio::time::sleep_until(deadline) => return Err(StatusError::timeout()),
        result = worker => result.map_err(|_| StatusError::handler_failure())?,
    };
    budget(deadline, cancel)?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ExchangeState, HandlerOutcome, Invocation, MultiMap, RequestHandler, SourceMetadata,
        handlers::specification::SpecificationHandler,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Condvar, Mutex};
    use std::time::Duration;
    pub(super) static COPIES: AtomicUsize = AtomicUsize::new(0);
    static PANIC_INPUT: LazyLock<Mutex<Option<String>>> = LazyLock::new(|| Mutex::new(None));
    pub(super) fn panic_if_requested(input: &str) {
        let should_panic = {
            let mut expected = PANIC_INPUT.lock().unwrap();
            if expected.as_deref() == Some(input) {
                expected.take();
                true
            } else {
                false
            }
        };
        assert!(!should_panic, "controlled worker failure");
    }
    type WaitingInput = (String, Gate, tokio::sync::oneshot::Sender<()>);
    static WAIT_INPUT: LazyLock<Mutex<Option<WaitingInput>>> = LazyLock::new(|| Mutex::new(None));
    pub(super) fn wait_if_requested(input: &str) {
        let waiting = {
            let mut value = WAIT_INPUT.lock().unwrap();
            if value.as_ref().is_some_and(|(path, _, _)| path == input) {
                value.take()
            } else {
                None
            }
        };
        if let Some((_, gate, started)) = waiting {
            let _ = started.send(());
            let lock = gate.0.lock().unwrap();
            let (lock, timeout) = gate
                .1
                .wait_timeout_while(lock, Duration::from_secs(2), |open| !*open)
                .unwrap();
            assert!(
                !timeout.timed_out() && *lock,
                "finite handler gate not released"
            );
        }
    }
    fn invocation() -> Invocation {
        let deadline = Instant::now() + Duration::from_secs(2);
        Invocation {
            method: http::Method::GET,
            raw_path: "/api/pets".into(),
            path: "/api/pets".into(),
            raw_query: None,
            query: MultiMap::default(),
            headers: http::HeaderMap::new(),
            body: None,
            body_was_base64: false,
            source: SourceMetadata::default(),
            request_id: "test-only".into(),
            deadline,
            response_deadline: deadline,
            cancel: CancellationToken::new(),
            path_params: Default::default(),
            route: None,
            original: serde_json::json!({}),
        }
    }
    type Gate = Arc<(Mutex<bool>, Condvar)>;
    fn release(gate: &Gate) {
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
    }
    fn waiting(
        gate: Gate,
        started: tokio::sync::oneshot::Sender<()>,
        input: String,
    ) -> Result<String, StatusError> {
        started.send(()).unwrap();
        let lock = gate.0.lock().unwrap();
        let (lock, timeout) = gate
            .1
            .wait_timeout_while(lock, Duration::from_secs(2), |open| !*open)
            .unwrap();
        assert!(
            !timeout.timed_out() && *lock,
            "finite test worker gate not released"
        );
        Ok(input)
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_pool_lifecycle_admission_races_and_cross_handler_saturation() {
        let gate: Gate = Arc::new((Mutex::new(false), Condvar::new()));
        let token = CancellationToken::new();
        let mut callers = Vec::new();
        for _ in 0..2 {
            let (started, rx) = tokio::sync::oneshot::channel();
            let gate = gate.clone();
            let token = token.clone();
            callers.push(tokio::spawn(async move {
                run(
                    "/bounded",
                    Instant::now() + Duration::from_secs(2),
                    &token,
                    move |input| waiting(gate, started, input),
                )
                .await
            }));
            rx.await.unwrap();
        }
        // Real separate handlers share the occupied process pool. No operation
        // appears after timeout/cancellation, and ingress remains unchanged.
        for mode in ["deadline", "cancel"] {
            let source = format!(
                r#"{{"openapi":"3.0.3","servers":[{{"url":"/api"}}],"paths":{{"/pets":{{"get":{{"operationId":"{mode}"}}}}}}}}"#
            );
            let handler = SpecificationHandler::from_source(
                source.as_bytes(),
                gateway_policy::openapi_operation::SourceFormat::Json,
            )
            .unwrap();
            let mut inv = invocation();
            let mut state = ExchangeState::default();
            inv.deadline = Instant::now() + Duration::from_millis(20);
            if mode == "cancel" {
                let cancel = inv.cancel.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    cancel.cancel();
                });
            }
            let HandlerOutcome::Reject(error) =
                handler.on_request(&mut inv, &mut state).await.unwrap()
            else {
                panic!("saturated handler must reject")
            };
            assert_eq!(error.code, "ERR14017");
            assert!(state.operation.is_none());
            assert_eq!(inv.path, "/api/pets");
        }
        // Admission happens even with both permits occupied; panic closure must
        // never execute (hence no copied/submitted job on this seam).
        let too_big = "x".repeat(8193);
        let copies = COPIES.load(Ordering::SeqCst);
        assert_eq!(
            run(
                &too_big,
                Instant::now() + Duration::from_secs(1),
                &CancellationToken::new(),
                |_| panic!("oversize job submitted")
            )
            .await
            .unwrap_err()
            .code,
            "ERR14029"
        );
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert_eq!(
            run(
                &too_big,
                Instant::now() + Duration::from_secs(1),
                &cancelled,
                |_| panic!("cancelled job submitted")
            )
            .await
            .unwrap_err()
            .code,
            "ERR14017"
        );
        assert_eq!(
            run(
                &too_big,
                Instant::now(),
                &CancellationToken::new(),
                |_| panic!("expired job submitted")
            )
            .await
            .unwrap_err()
            .code,
            "ERR14017"
        );
        assert_eq!(
            COPIES.load(Ordering::SeqCst),
            copies,
            "rejected input must never be copied into a worker"
        );
        // Dropping callers cannot release permits owned by still-running jobs.
        token.cancel();
        for task in callers {
            assert_eq!(task.await.unwrap().unwrap_err().code, "ERR14017");
        }
        assert_eq!(
            run(
                "/x",
                Instant::now() + Duration::from_millis(15),
                &CancellationToken::new(),
                Ok
            )
            .await
            .unwrap_err()
            .code,
            "ERR14017"
        );
        release(&gate);
        // Successful acquisition proves worker completion actually releases the
        // global permits; no late result can reach the abandoned callers.
        assert_eq!(
            run(
                "/next",
                Instant::now() + Duration::from_secs(1),
                &CancellationToken::new(),
                Ok
            )
            .await
            .unwrap(),
            "/next"
        );
        assert_eq!(
            run(
                "/panic-private",
                Instant::now() + Duration::from_secs(1),
                &CancellationToken::new(),
                |_| panic!("synthetic-private-worker")
            )
            .await
            .unwrap_err(),
            StatusError::handler_failure()
        );
        assert_eq!(
            run(
                "/after-panic",
                Instant::now() + Duration::from_secs(1),
                &CancellationToken::new(),
                Ok
            )
            .await
            .unwrap(),
            "/after-panic"
        );
        let h=SpecificationHandler::from_source(br#"{"openapi":"3.1.0","servers":[{"url":"/api"}],"paths":{"/failure":{"get":{"operationId":"trusted"}}}}"#,gateway_policy::openapi_operation::SourceFormat::Json).unwrap();
        let mut inv = invocation();
        inv.path = "/api/failure".into();
        let mut state = ExchangeState::default();
        assert!(matches!(
            h.on_request(&mut inv, &mut state).await.unwrap(),
            HandlerOutcome::Continue
        ));
        assert!(state.operation.is_some());
        *PANIC_INPUT.lock().unwrap() = Some(inv.path.clone());
        let HandlerOutcome::Reject(error) = h.on_request(&mut inv, &mut state).await.unwrap()
        else {
            panic!("worker panic must reject")
        };
        assert_eq!(error, StatusError::handler_failure());
        assert!(state.operation.is_none());
        assert_eq!(inv.path, "/api/failure");
        assert!(
            !String::from_utf8(error.to_bytes().to_vec())
                .unwrap()
                .contains("controlled")
        );
        let copies = COPIES.load(Ordering::SeqCst);
        inv.path = too_big.clone();
        let HandlerOutcome::Reject(error) = h.on_request(&mut inv, &mut state).await.unwrap()
        else {
            panic!("oversize must reject")
        };
        assert_eq!(error.code, "ERR14029");
        assert!(state.operation.is_none());
        assert_eq!(COPIES.load(Ordering::SeqCst), copies);
        // Cancel a real handler with its actual job still running.
        let gate: Gate = Arc::new((Mutex::new(false), Condvar::new()));
        let (started, rx) = tokio::sync::oneshot::channel();
        *WAIT_INPUT.lock().unwrap() = Some(("/api/failure".into(), gate.clone(), started));
        let mut running = invocation();
        running.path = "/api/failure".into();
        let signal = running.cancel.clone();
        let handler = h.clone();
        let task = tokio::spawn(async move {
            let mut state = ExchangeState::default();
            let result = handler.on_request(&mut running, &mut state).await.unwrap();
            (result, running, state)
        });
        rx.await.unwrap();
        signal.cancel();
        let (result, running, state) = task.await.unwrap();
        let HandlerOutcome::Reject(error) = result else {
            panic!("running cancellation must reject")
        };
        assert_eq!(error.code, "ERR14017");
        assert!(state.operation.is_none());
        assert_eq!(running.path, "/api/failure");
        release(&gate);
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(
            state.operation.is_none(),
            "late job cannot access returned exchange state"
        );
        // Complete and cancel in the same worker turn. Biased arbitration plus
        // post-completion budget must prevent successful result publication.
        for _ in 0..32 {
            let cancel = CancellationToken::new();
            let signal = cancel.clone();
            assert_eq!(
                run(
                    "/race",
                    Instant::now() + Duration::from_secs(1),
                    &cancel,
                    move |s| {
                        signal.cancel();
                        Ok(s)
                    }
                )
                .await
                .unwrap_err()
                .code,
                "ERR14017"
            );
        }
        let deadline = Instant::now() + Duration::from_millis(10);
        assert_eq!(
            run(
                "/deadline-race",
                deadline,
                &CancellationToken::new(),
                move |s| {
                    std::thread::sleep(Duration::from_millis(15));
                    Ok(s)
                }
            )
            .await
            .unwrap_err()
            .code,
            "ERR14017"
        );
        // Abort/drop (rather than cooperative token) also retains a worker permit.
        let gate: Gate = Arc::new((Mutex::new(false), Condvar::new()));
        let (started, rx) = tokio::sync::oneshot::channel();
        let worker_gate = gate.clone();
        let task = tokio::spawn(async move {
            run(
                "/drop",
                Instant::now() + Duration::from_secs(1),
                &CancellationToken::new(),
                move |s| waiting(worker_gate, started, s),
            )
            .await
        });
        rx.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let second: Gate = Arc::new((Mutex::new(false), Condvar::new()));
        let (started, rx) = tokio::sync::oneshot::channel();
        let second_gate = second.clone();
        let second_task = tokio::spawn(async move {
            run(
                "/second",
                Instant::now() + Duration::from_secs(1),
                &CancellationToken::new(),
                move |s| waiting(second_gate, started, s),
            )
            .await
        });
        rx.await.unwrap();
        assert_eq!(
            run(
                "/third",
                Instant::now() + Duration::from_millis(10),
                &CancellationToken::new(),
                Ok
            )
            .await
            .unwrap_err()
            .code,
            "ERR14017",
            "abort must not free worker's permit"
        );
        release(&gate);
        release(&second);
        assert_eq!(second_task.await.unwrap().unwrap(), "/second");
        assert_eq!(
            run(
                "/clean",
                Instant::now() + Duration::from_secs(1),
                &CancellationToken::new(),
                Ok
            )
            .await
            .unwrap(),
            "/clean"
        );
        let mut handlers = Vec::new();
        for i in 0..16 {
            let base = if i % 2 == 0 {
                "/((a{64}){64}){7}|/a*"
            } else {
                "/((a|aa){0,64}){0,64}z"
            };
            let source=serde_json::to_vec(&serde_json::json!({"openapi":"3.1.0","servers":[{"url":base}],"paths":{"/{id}":{"get":{"operationId":format!("handler-{i}")}}}})).unwrap();
            let handler = SpecificationHandler::from_source(
                &source,
                gateway_policy::openapi_operation::SourceFormat::Json,
            )
            .unwrap();
            handlers.push((i, handler));
        }
        let mut jobs = Vec::new();
        let start = Instant::now();
        for (i, handler) in handlers {
            jobs.push(tokio::spawn(async move {
                let mut inv = invocation();
                inv.path = "/".to_string() + &"a".repeat(8190) + "!";
                inv.deadline = Instant::now() + Duration::from_secs(5);
                let mut state = ExchangeState::default();
                let started = Instant::now();
                assert!(matches!(
                    handler.on_request(&mut inv, &mut state).await.unwrap(),
                    HandlerOutcome::Continue
                ));
                assert_eq!(
                    state.operation.as_ref().unwrap().operation_id(),
                    Some(format!("handler-{i}").as_str())
                );
                started.elapsed()
            }));
        }
        let mut max_tick = Duration::ZERO;
        let mut last = Instant::now();
        while jobs.iter().any(|job| !job.is_finished()) {
            tokio::time::sleep(Duration::from_millis(1)).await;
            let now = Instant::now();
            max_tick = max_tick.max(now - last);
            last = now;
        }
        let mut max_request = Duration::ZERO;
        for job in jobs {
            max_request = max_request.max(job.await.unwrap());
        }
        println!(
            "real_handler_stress jobs=16 input_bytes=8192 process_permits=2 total_ms={} max_request_ms={} max_scheduler_tick_us={}",
            start.elapsed().as_millis(),
            max_request.as_millis(),
            max_tick.as_micros()
        );
    }
}
