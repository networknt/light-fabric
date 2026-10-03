//! Bounded traversal state; visiting a row never acknowledges it.
use execution_runner_protocol::{ExecutionResultPage, ExecutionResultView};
use std::{collections::VecDeque, future::Future};

pub enum PollFailure<E> {
    Item(E),
    System(E),
    CursorRejected,
}

impl Sweep {
    pub async fn poll<E, F, FF, P, PF>(
        &mut self,
        shutdown: &tokio_util::sync::CancellationToken,
        fetch: F,
        mut process: P,
    ) -> Result<bool, E>
    where
        F: FnOnce(Option<String>) -> FF,
        FF: Future<Output = Result<ExecutionResultPage, PollFailure<E>>>,
        P: FnMut(ExecutionResultView) -> PF,
        PF: Future<Output = Result<bool, PollFailure<E>>>,
        E: std::fmt::Display,
    {
        if shutdown.is_cancelled() {
            return Ok(false);
        }
        if self.page.is_empty() {
            let page = match fetch(self.cursor.clone()).await {
                Ok(page) => page,
                Err(PollFailure::CursorRejected) => {
                    *self = Self::default();
                    tracing::warn!(
                        "execution result sweep restarted: EXECUTION_RESULT_CURSOR_REJECTED"
                    );
                    return Ok(false);
                }
                Err(PollFailure::Item(error) | PollFailure::System(error)) => return Err(error),
            };
            self.page = page.items.into();
            self.next_cursor = page.next_cursor;
        }
        let mut changed = false;
        // At most one bounded page per poll; retain current item on systemic failure/shutdown.
        while let Some(item) = self.page.front().cloned() {
            if shutdown.is_cancelled() {
                return Ok(changed);
            }
            match process(item).await {
                Ok(result) => changed |= result,
                Err(PollFailure::Item(_)) => {
                    tracing::warn!("execution result item deferred after item-specific failure")
                }
                Err(PollFailure::System(error)) => return Err(error),
                Err(PollFailure::CursorRejected) => unreachable!("only fetch rejects cursors"),
            }
            self.page.pop_front();
        }
        self.cursor = self.next_cursor.take();
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        executor::expression_completion::ProfileDisposition, profile_support::SupportedProfiles,
    };
    use std::sync::{Arc, Mutex};
    use uuid::Uuid;

    fn row(id: u128) -> ExecutionResultView {
        ExecutionResultView {
            host_id: Uuid::nil(),
            execution_id: Uuid::from_u128(id),
            request_id: Uuid::from_u128(id),
            origin_instance_id: "fixture".into(),
            subject_kind: "workflow-task".into(),
            subject_id: Uuid::from_u128(id),
            process_id: Some(Uuid::from_u128(id)),
            task_id: Some(Uuid::from_u128(id)),
            agent_session_id: None,
            agent_turn_id: None,
            agent_action_id: None,
            action_kind: "run-shell".into(),
            attempt_number: 1,
            lease_id: Uuid::from_u128(id),
            state: "SUCCEEDED".into(),
            fencing_token: 1,
            normalized_result: None,
            normalized_error: None,
            retry_classification: None,
            terminal: true,
            accepted: false,
        }
    }

    #[tokio::test]
    async fn w6_pagination_65_unsupported_then_compatible_progresses_on_poll_three() {
        let pending = Arc::new(Mutex::new((1..=66).map(row).collect::<Vec<_>>()));
        let untouched = pending.lock().unwrap()[..65]
            .iter()
            .map(|r| serde_json::to_value(r).unwrap())
            .collect::<Vec<_>>();
        let effects = Arc::new(Mutex::new(Vec::new()));
        let snapshot = serde_json::json!({"document":{"dsl":"1.0.3","namespace":"test","name":"page","version":"1.0.0","metadata":{"lightExpressionProfile":"cel-workflow-v2"}},"evaluate":{"language":"cel"},"do":[{"step":{"set":{"value":"${1}"}}}]});
        let digest = execution_runner_protocol::canonical_sha256(&snapshot).unwrap();
        let shutdown = tokio_util::sync::CancellationToken::new();
        let mut sweep = Sweep::default();
        for poll in 1..=3 {
            let pending_fetch = pending.clone();
            let pending_process = pending.clone();
            let effects = effects.clone();
            let snapshot = snapshot.clone();
            let digest = digest.clone();
            let changed = sweep
                .poll(
                    &shutdown,
                    move |cursor| async move {
                        let after = cursor.as_deref().unwrap_or("0").parse::<u128>().unwrap();
                        let rows = pending_fetch
                            .lock()
                            .unwrap()
                            .iter()
                            .filter(|r| r.execution_id.as_u128() > after)
                            .cloned()
                            .collect::<Vec<_>>();
                        let items = rows.iter().take(32).cloned().collect::<Vec<_>>();
                        let next_cursor = (rows.len() > 32)
                            .then(|| items.last().unwrap().execution_id.as_u128().to_string());
                        Ok::<_, PollFailure<&str>>(ExecutionResultPage { items, next_cursor })
                    },
                    move |item| {
                        let pending = pending_process.clone();
                        let effects = effects.clone();
                        let snapshot = snapshot.clone();
                        let digest = digest.clone();
                        async move {
                            let disposition = if item.execution_id.as_u128() <= 65 {
                                SupportedProfiles::from_evaluator(false).check(
                                    "cel-workflow-v2",
                                    Some(&snapshot),
                                    Some(&digest),
                                )
                            } else {
                                SupportedProfiles::from_evaluator(false).check(
                                    "cel-workflow-v1",
                                    None,
                                    None,
                                )
                            };
                            if disposition == ProfileDisposition::Deferred {
                                return Ok(false);
                            }
                            assert_eq!(disposition, ProfileDisposition::Legacy);
                            effects.lock().unwrap().push(item.execution_id); // mock evaluation/advancement/ACK seam
                            pending
                                .lock()
                                .unwrap()
                                .retain(|r| r.execution_id != item.execution_id);
                            Ok(true)
                        }
                    },
                )
                .await
                .unwrap();
            assert_eq!(changed, poll == 3);
            assert_eq!(
                pending.lock().unwrap().len(),
                if poll == 3 { 65 } else { 66 }
            );
        }
        assert_eq!(*effects.lock().unwrap(), vec![Uuid::from_u128(66)]);
        assert_eq!(
            pending
                .lock()
                .unwrap()
                .iter()
                .map(|r| serde_json::to_value(r).unwrap())
                .collect::<Vec<_>>(),
            untouched
        );
        // Next sweep revisits the deferred prefix, using an actually compatible profile set.
        let seen = Arc::new(Mutex::new(Vec::new()));
        let observed = seen.clone();
        sweep
            .poll(
                &shutdown,
                |_| async {
                    Ok::<_, PollFailure<&str>>(ExecutionResultPage {
                        items: vec![row(1)],
                        next_cursor: None,
                    })
                },
                move |item| {
                    assert_eq!(
                        SupportedProfiles::from_evaluator(true).check(
                            "cel-workflow-v2",
                            Some(&snapshot),
                            Some(&digest)
                        ),
                        ProfileDisposition::V2
                    );
                    observed.lock().unwrap().push(item.execution_id);
                    async { Ok(true) }
                },
            )
            .await
            .unwrap();
        assert_eq!(*seen.lock().unwrap(), vec![Uuid::from_u128(1)]);
    }

    #[tokio::test]
    async fn w6_pagination_item_error_continues_system_error_retains_item() {
        let shutdown = tokio_util::sync::CancellationToken::new();
        let mut sweep = Sweep::default();
        let visits = Arc::new(Mutex::new(Vec::new()));
        let observed = visits.clone();
        let error = sweep
            .poll(
                &shutdown,
                |_| async {
                    Ok(ExecutionResultPage {
                        items: vec![row(1), row(2), row(3)],
                        next_cursor: Some("next".into()),
                    })
                },
                move |item| {
                    observed.lock().unwrap().push(item.execution_id.as_u128());
                    async move {
                        match item.execution_id.as_u128() {
                            1 => Err(PollFailure::Item("bad payload")),
                            2 => Err(PollFailure::System("database unavailable")),
                            _ => Ok(true),
                        }
                    }
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error, "database unavailable");
        assert_eq!(*visits.lock().unwrap(), vec![1, 2]);
        let observed = visits.clone();
        sweep
            .poll(
                &shutdown,
                |_| async {
                    panic!("retained page must not refetch");
                    #[allow(unreachable_code)]
                    Ok::<_, PollFailure<&str>>(ExecutionResultPage {
                        items: vec![],
                        next_cursor: None,
                    })
                },
                move |item| {
                    observed.lock().unwrap().push(item.execution_id.as_u128());
                    async { Ok(false) }
                },
            )
            .await
            .unwrap();
        assert_eq!(*visits.lock().unwrap(), vec![1, 2, 2, 3]);
        assert_eq!(sweep.cursor.as_deref(), Some("next"));
    }

    #[tokio::test]
    async fn w6_pagination_cursor_rejection_network_retry_and_shutdown() {
        let shutdown = tokio_util::sync::CancellationToken::new();
        let mut sweep = Sweep {
            cursor: Some("old".into()),
            ..Default::default()
        };
        let result = sweep
            .poll(
                &shutdown,
                |cursor| async move {
                    assert_eq!(cursor.as_deref(), Some("old"));
                    Err(PollFailure::System("network"))
                },
                |_| async { Ok(false) },
            )
            .await;
        assert_eq!(result, Err("network"));
        assert_eq!(sweep.cursor.as_deref(), Some("old"));
        sweep
            .poll(
                &shutdown,
                |_| async { Err::<ExecutionResultPage, _>(PollFailure::<&str>::CursorRejected) },
                |_| async { Ok(false) },
            )
            .await
            .unwrap();
        assert!(sweep.cursor.is_none());
        let cancel = shutdown.clone();
        sweep
            .poll(
                &shutdown,
                |cursor| async move {
                    assert!(cursor.is_none());
                    Ok::<_, PollFailure<&str>>(ExecutionResultPage {
                        items: vec![row(1), row(2)],
                        next_cursor: None,
                    })
                },
                move |_| {
                    cancel.cancel();
                    async { Ok(true) }
                },
            )
            .await
            .unwrap();
        assert_eq!(sweep.page.front().unwrap().execution_id, Uuid::from_u128(2));
        sweep
            .poll(
                &shutdown,
                |_| async {
                    panic!("cancelled fetch");
                    #[allow(unreachable_code)]
                    Ok::<_, PollFailure<&str>>(ExecutionResultPage {
                        items: vec![],
                        next_cursor: None,
                    })
                },
                |_| async {
                    panic!("cancelled item");
                    #[allow(unreachable_code)]
                    Ok(false)
                },
            )
            .await
            .unwrap();
        assert_eq!(sweep.page.len(), 1);
    }
}

#[derive(Default)]
pub struct Sweep {
    cursor: Option<String>,
    page: VecDeque<ExecutionResultView>,
    next_cursor: Option<String>,
}
