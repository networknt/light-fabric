//! Pure future supervision, not a child/executor substitute. Actual ownership
//! observations come only from the qualification app's controller.
use light_lambda_runtime::validator_ipc::lifecycle::CompletionCause;
use std::{
    any::Any,
    future::Future,
    panic::{AssertUnwindSafe, catch_unwind},
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
pub enum Outcome<T> {
    Success(T),
    AssertionFailure(Box<dyn Any + Send>),
    Timeout,
}
struct Catch<F>(Pin<Box<F>>);
impl<F: Future> Future for Catch<F> {
    type Output = Result<F::Output, Box<dyn Any + Send>>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match catch_unwind(AssertUnwindSafe(|| self.0.as_mut().poll(cx))) {
            Ok(Poll::Ready(value)) => Poll::Ready(Ok(value)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(panic) => Poll::Ready(Err(panic)),
        }
    }
}
pub async fn run<F, C, G>(body: F, timeout: Duration, cleanup: C) -> (Outcome<F::Output>, G::Output)
where
    F: Future,
    C: FnOnce(CompletionCause) -> G,
    G: Future,
{
    let outcome = match tokio::time::timeout(timeout, Catch(Box::pin(body))).await {
        Ok(Ok(value)) => Outcome::Success(value),
        Ok(Err(panic)) => Outcome::AssertionFailure(panic),
        Err(_) => Outcome::Timeout,
    };
    let cause = match &outcome {
        Outcome::Success(_) => CompletionCause::Success,
        Outcome::AssertionFailure(_) => CompletionCause::AssertionFailure,
        Outcome::Timeout => CompletionCause::Timeout,
    };
    let cleaned = cleanup(cause).await;
    (outcome, cleaned)
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    #[tokio::test]
    async fn success_runs_cleanup_once_before_return() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let out = calls.clone();
        let (outcome, ()) = run(
            async { 42 },
            Duration::from_secs(1),
            move |cause| async move { out.lock().unwrap().push(cause) },
        )
        .await;
        assert!(matches!(outcome, Outcome::Success(42)));
        assert_eq!(*calls.lock().unwrap(), vec![CompletionCause::Success]);
    }
    #[tokio::test]
    async fn assertion_failure_is_retained_until_cleanup_finishes() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let out = calls.clone();
        let (outcome, ()) = run(
            async {
                assert_eq!(1, 2, "synthetic assertion");
            },
            Duration::from_secs(1),
            move |cause| async move {
                tokio::task::yield_now().await;
                out.lock().unwrap().push(cause)
            },
        )
        .await;
        assert!(matches!(outcome, Outcome::AssertionFailure(_)));
        assert_eq!(
            *calls.lock().unwrap(),
            vec![CompletionCause::AssertionFailure]
        );
    }
    #[tokio::test]
    async fn timeout_drops_body_before_cleanup_and_does_not_skip_it() {
        struct Mark(Arc<Mutex<Vec<&'static str>>>);
        impl Drop for Mark {
            fn drop(&mut self) {
                self.0.lock().unwrap().push("bodyDropped");
            }
        }
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mark = Mark(calls.clone());
        let out = calls.clone();
        let (outcome, ()) = run(
            async move {
                let _mark = mark;
                std::future::pending::<()>().await
            },
            Duration::from_millis(1),
            move |cause| async move {
                assert_eq!(cause, CompletionCause::Timeout);
                out.lock().unwrap().push("cleanup")
            },
        )
        .await;
        assert!(matches!(outcome, Outcome::Timeout));
        assert_eq!(*calls.lock().unwrap(), vec!["bodyDropped", "cleanup"]);
    }
    #[tokio::test]
    async fn cleanup_error_is_not_lost_even_after_assertion_failure() {
        let (outcome, cleaned) = run(
            async { panic!("synthetic assertion") },
            Duration::from_secs(1),
            |_| async { Err::<(), _>("cleanup incomplete") },
        )
        .await;
        assert!(matches!(outcome, Outcome::AssertionFailure(_)));
        assert_eq!(cleaned, Err("cleanup incomplete"));
    }
}
