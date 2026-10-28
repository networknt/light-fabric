pub mod support;
use async_trait::async_trait;
use bytes::Bytes;
use serverless_core::{blocking::*, plan::*, *};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use support::*;
use tokio::time::Instant;

struct FiniteOwnedWork {
    input: Vec<u8>,
    done: std::sync::mpsc::Sender<()>,
}
impl BoundedWork for FiniteOwnedWork {
    type Output = Vec<u8>;
    fn input_bytes(&self) -> usize {
        self.input.len()
    }
    fn step(&mut self) -> Result<WorkStep<Vec<u8>>, BlockingError> {
        // Explicit test-only finite wall-clock work outliving the caller's deadline.
        std::thread::sleep(Duration::from_millis(150));
        self.done.send(()).unwrap();
        Ok(WorkStep::Ready(std::mem::take(&mut self.input)))
    }
}
struct BlockingResponse {
    done: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    counts: Arc<Counts>,
}
#[async_trait]
impl ResponseHandler for BlockingResponse {
    fn id(&self) -> HandlerId {
        HandlerId::ResponseTransformer
    }
    async fn on_response(
        &self,
        inv: &Invocation,
        _: &mut ExchangeState,
        resp: &mut GatewayResponse,
    ) -> Result<(), HandlerFailure> {
        self.counts
            .response_entries
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let done = self.done.lock().unwrap().take().unwrap();
        let result = BlockingExecutor::process()
            .execute(
                FiniteOwnedWork {
                    input: b"late output".to_vec(),
                    done,
                },
                inv.response_deadline,
            )
            .await
            .map_err(|_| HandlerFailure)?;
        self.counts
            .response_completions
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        resp.body = Some(Bytes::from(result));
        Ok(())
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn late_owned_blocking_result_cannot_change_finalized_response() {
    let c = Arc::new(Counts::default());
    let mut r = registry();
    let (done, completed) = std::sync::mpsc::channel();
    install(
        &mut r,
        HandlerId::ResponseTransformer,
        Binding::Response(Arc::new(BlockingResponse {
            done: Mutex::new(Some(done)),
            counts: c.clone(),
        })),
    );
    let p = Pipeline::new(
        compile(
            config(
                &[
                    (HandlerId::Proxy, "p"),
                    (HandlerId::ResponseTransformer, "r"),
                ],
                &["p", "r"],
            ),
            &r,
        ),
        vec![c.clone()],
    );
    let mut inv = invocation();
    inv.deadline = Instant::now() + Duration::from_millis(30);
    inv.response_deadline = Instant::now() + Duration::from_millis(50);
    let response = p.execute(inv, &FakeBackend::new(&c)).await;
    assert_eq!(response, StatusError::timeout().response());
    assert_eq!(n(&c.backend), 1);
    assert_eq!(n(&c.finalized), 1);
    assert_eq!(n(&c.response_entries), 1);
    assert_eq!(n(&c.response_completions), 0);
    assert!(completed.try_recv().is_err());
    tokio::task::spawn_blocking(move || completed.recv_timeout(Duration::from_secs(2)).unwrap())
        .await
        .unwrap();
    assert_eq!(
        c.finalized_responses.lock().unwrap().as_slice(),
        std::slice::from_ref(&response)
    );
    assert_eq!(response, StatusError::timeout().response());
    assert_eq!(n(&c.finalized), 1);
    assert_eq!(n(&c.backend), 1);
    assert_eq!(n(&c.response_completions), 0);
}
