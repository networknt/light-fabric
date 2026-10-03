//! Task-only terminal bookkeeping. No executable snapshot or evaluator is accepted here.
use super::*;
use expression_completion::{CompletionGuard, CompletionState, FailureRoute, ProfileDisposition};

pub(super) fn required(profile: ProfileDisposition, state: &str) -> bool {
    matches!(
        profile,
        ProfileDisposition::V2 | ProfileDisposition::Deferred
    ) && matches!(state, "FAILED" | "CANCELLED" | "UNKNOWN" | "TIMED_OUT")
}

#[async_trait::async_trait]
pub(super) trait CleanupStore {
    async fn authority(
        &mut self,
        task: &ActiveTask,
        guard: &CompletionGuard,
    ) -> Result<bool, sqlx::Error>;
    async fn route(&mut self, task: &ActiveTask) -> Result<FailureRoute, sqlx::Error>;
    async fn process(&mut self, task: &ActiveTask, output: &Value) -> Result<(), sqlx::Error>;
    async fn mark_task(&mut self, task: &ActiveTask, output: &Value) -> Result<(), sqlx::Error>;
    async fn compensation(&mut self, task: &ActiveTask, output: &Value) -> Result<(), sqlx::Error>;
    async fn branch(&mut self, task: &ActiveTask, output: &Value) -> Result<(), sqlx::Error>;
}

pub(super) async fn complete_if_required(
    store: &mut impl CleanupStore,
    profile: ProfileDisposition,
    state: &str,
    task: &ActiveTask,
    guard: &CompletionGuard,
    output: &Value,
) -> Result<bool, sqlx::Error> {
    if !required(profile, state) {
        return Ok(false);
    }
    expression_completion::require_authority(store.authority(task, guard).await?)?;
    match store.route(task).await? {
        FailureRoute::Process => store.process(task, output).await?,
        FailureRoute::Compensation => {
            store.mark_task(task, output).await?;
            store.compensation(task, output).await?;
        }
        FailureRoute::Branch => {
            store.mark_task(task, output).await?;
            store.branch(task, output).await?;
        }
    }
    Ok(true)
}

pub(super) struct PostgresCleanup<'a, 't> {
    pub executor: &'a TaskExecutor,
    pub tx: &'a mut Transaction<'t, Postgres>,
}

#[async_trait::async_trait]
impl CleanupStore for PostgresCleanup<'_, '_> {
    async fn authority(
        &mut self,
        task: &ActiveTask,
        guard: &CompletionGuard,
    ) -> Result<bool, sqlx::Error> {
        self.executor
            .w6_state_authority(
                self.tx,
                &CompletionState {
                    task,
                    host_lease: None,
                    completion_guard: Some(guard),
                },
                false,
            )
            .await
    }
    async fn route(&mut self, task: &ActiveTask) -> Result<FailureRoute, sqlx::Error> {
        let (compensation,branch):(bool,bool)=sqlx::query_as("SELECT is_compensation,EXISTS(SELECT 1 FROM workflow_fork_branch_t b WHERE b.host_id=t.host_id AND b.task_id=t.task_id) FROM task_info_t t WHERE t.host_id=$1 AND t.task_id=$2")
            .bind(task.host_id).bind(task.task_id).fetch_one(&mut **self.tx).await?;
        Ok(expression_completion::failure_route(compensation, branch))
    }
    async fn process(&mut self, task: &ActiveTask, output: &Value) -> Result<(), sqlx::Error> {
        self.executor.w4_failure(self.tx, task, output).await
    }
    async fn mark_task(&mut self, task: &ActiveTask, output: &Value) -> Result<(), sqlx::Error> {
        let updated=sqlx::query("UPDATE task_info_t SET status_code='F',result_code='W4_STEP_FAILED',task_output=$3,locked='N',lease_owner=NULL,lease_expires_ts=NULL,completed_ts=clock_timestamp() WHERE host_id=$1 AND task_id=$2 AND status_code=$4 AND result_code IS DISTINCT FROM 'W4_STEP_DONE' AND result_code IS DISTINCT FROM 'W4_STEP_FAILED'")
            .bind(task.host_id).bind(task.task_id).bind(output).bind(&task.status_code).execute(&mut **self.tx).await?;
        expression_completion::require_authority(updated.rows_affected() == 1)
    }
    async fn compensation(&mut self, task: &ActiveTask, output: &Value) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE workflow_invocation_t SET state='FAILED',terminal_ts=CURRENT_TIMESTAMP,
            user_authorization=NULL,user_authorization_exp=NULL,updated_ts=CURRENT_TIMESTAMP,state_version=state_version+1,
            normalized_error=jsonb_build_object('code','WORKFLOW_TASK_FAILED','message','workflow compensation failed','retryable',false,'detail',$1::jsonb)
            WHERE host_id=$2 AND process_id=$3 AND state='COMPENSATING'")
            .bind(output).bind(task.host_id).bind(task.process_id).execute(&mut **self.tx).await?;
        sqlx::query("UPDATE process_info_t SET status_code='F',completed_ts=CURRENT_TIMESTAMP,custom_status_code='WORKFLOW_COMPENSATION_FAILED' WHERE host_id=$1 AND process_id=$2")
            .bind(task.host_id).bind(task.process_id).execute(&mut **self.tx).await?;
        Ok(())
    }
    async fn branch(&mut self, task: &ActiveTask, output: &Value) -> Result<(), sqlx::Error> {
        let (join,name):(Uuid,String)=sqlx::query_as("SELECT join_id,branch_name FROM workflow_fork_branch_t WHERE host_id=$1 AND task_id=$2 FOR UPDATE")
            .bind(task.host_id).bind(task.task_id).fetch_one(&mut **self.tx).await?;
        sqlx::query("UPDATE workflow_fork_branch_t SET state='FAILED',result=$1,completed_ts=CURRENT_TIMESTAMP WHERE host_id=$2 AND join_id=$3 AND branch_name=$4 AND state='RUNNING'")
            .bind(output).bind(task.host_id).bind(join).bind(name).execute(&mut **self.tx).await?;
        let (expected,compete,state):(i32,bool,String)=sqlx::query_as("SELECT expected_branches,compete,state FROM workflow_fork_join_t WHERE host_id=$1 AND join_id=$2 FOR UPDATE")
            .bind(task.host_id).bind(join).fetch_one(&mut **self.tx).await?;
        if state == "RUNNING" {
            let rows:Vec<(String,String,Option<Value>)>=sqlx::query_as("SELECT branch_name,state,result FROM workflow_fork_branch_t WHERE host_id=$1 AND join_id=$2 ORDER BY branch_name")
                .bind(task.host_id).bind(join).fetch_all(&mut **self.tx).await?;
            let (completed, failed, terminal, success) =
                expression_completion::fork_status(expected, compete, &rows);
            let results: serde_json::Map<String, Value> = rows
                .into_iter()
                .filter(|(_, state, _)| state != "RUNNING")
                .map(|(name, _, output)| (name, output.unwrap_or(Value::Null)))
                .collect();
            sqlx::query("UPDATE workflow_fork_join_t SET completed_branches=$1,failed_branches=$2,branch_results=$3 WHERE host_id=$4 AND join_id=$5")
                .bind(i32::try_from(completed).unwrap_or(i32::MAX)).bind(i32::try_from(failed).unwrap_or(i32::MAX)).bind(Value::Object(results)).bind(task.host_id).bind(join).execute(&mut **self.tx).await?;
            // Counting failure is cleanup; a successful continuation needs execution authority.
            if terminal && !success {
                sqlx::query("UPDATE workflow_fork_join_t SET state='FAILED',completed_ts=CURRENT_TIMESTAMP WHERE host_id=$1 AND join_id=$2 AND state='RUNNING'")
                    .bind(task.host_id).bind(join).execute(&mut **self.tx).await?;
                sqlx::query("UPDATE process_info_t SET status_code='F',completed_ts=CURRENT_TIMESTAMP,error_info='WORKFLOW_FORK_FAILED' WHERE host_id=$1 AND process_id=$2")
                    .bind(task.host_id).bind(task.process_id).execute(&mut **self.tx).await?;
            }
        }
        let (status,error):(String,Option<String>)=sqlx::query_as("SELECT status_code::text,error_info FROM process_info_t WHERE host_id=$1 AND process_id=$2")
            .bind(task.host_id).bind(task.process_id).fetch_one(&mut **self.tx).await?;
        let state = match status.as_str() {
            "A" => "RUNNING",
            "W" => "WAITING",
            "F" => "FAILED",
            _ => return Err(sqlx::Error::Protocol("WORKFLOW_STALE_COMPLETION".into())),
        };
        let error = error.map(
            |message| json!({"code":"WORKFLOW_TASK_FAILED","message":message,"retryable":false}),
        );
        let updated=sqlx::query("UPDATE workflow_invocation_t SET state=$1,updated_ts=CURRENT_TIMESTAMP,state_version=state_version+1,
            terminal_ts=CASE WHEN $1='FAILED' THEN CURRENT_TIMESTAMP ELSE NULL END,
            user_authorization=CASE WHEN $1='FAILED' THEN NULL ELSE user_authorization END,
            user_authorization_exp=CASE WHEN $1='FAILED' THEN NULL ELSE user_authorization_exp END,
            normalized_error=CASE WHEN $1='FAILED' THEN $2 ELSE normalized_error END
            WHERE host_id=$3 AND process_id=$4 AND state NOT IN ('CANCELLED','COMPLETED','FAILED')")
            .bind(state).bind(error).bind(task.host_id).bind(task.process_id).execute(&mut **self.tx).await?;
        if updated.rows_affected() == 0 {
            let exists:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workflow_invocation_t WHERE host_id=$1 AND process_id=$2)")
                .bind(task.host_id).bind(task.process_id).fetch_one(&mut **self.tx).await?;
            expression_completion::require_authority(!exists)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct MockCleanup {
        route: u8,
        live: bool,
        calls: Arc<Mutex<Vec<&'static str>>>,
    }
    #[async_trait::async_trait]
    impl CleanupStore for MockCleanup {
        async fn authority(
            &mut self,
            _task: &ActiveTask,
            guard: &CompletionGuard,
        ) -> Result<bool, sqlx::Error> {
            assert!(matches!(
                guard,
                CompletionGuard::Agent { .. } | CompletionGuard::Runner { .. }
            ));
            self.calls.lock().unwrap().push("authority");
            Ok(self.live)
        }
        async fn route(&mut self, _task: &ActiveTask) -> Result<FailureRoute, sqlx::Error> {
            Ok(match self.route {
                1 => FailureRoute::Branch,
                2 => FailureRoute::Compensation,
                _ => FailureRoute::Process,
            })
        }
        async fn process(
            &mut self,
            _task: &ActiveTask,
            _output: &Value,
        ) -> Result<(), sqlx::Error> {
            self.calls.lock().unwrap().push("process-failed");
            Ok(())
        }
        async fn mark_task(
            &mut self,
            _task: &ActiveTask,
            _output: &Value,
        ) -> Result<(), sqlx::Error> {
            self.calls.lock().unwrap().push("task-failed");
            Ok(())
        }
        async fn compensation(
            &mut self,
            _task: &ActiveTask,
            _output: &Value,
        ) -> Result<(), sqlx::Error> {
            self.calls.lock().unwrap().push("compensation-failed");
            Ok(())
        }
        async fn branch(&mut self, _task: &ActiveTask, _output: &Value) -> Result<(), sqlx::Error> {
            self.calls.lock().unwrap().push("branch-failed");
            Ok(())
        }
    }
    fn task() -> ActiveTask {
        ActiveTask {
            host_id: Uuid::new_v4(),
            task_id: Uuid::new_v4(),
            process_id: Uuid::new_v4(),
            task_type: "future-task".into(),
            wf_instance_id: Uuid::new_v4().to_string(),
            wf_task_id: "future".into(),
            status_code: "W".into(),
            result_code: None,
        }
    }
    fn future_profile() -> ProfileDisposition {
        let snapshot = json!({"document":{"metadata":{"lightExpressionProfile":"future-profile"}},"do":42,"futureExecutable":{"newSyntax":true}});
        assert!(serde_json::from_value::<WorkflowDefinition>(snapshot.clone()).is_err());
        crate::profile_support::SupportedProfiles::from_evaluator(false).check(
            "future-profile",
            Some(&snapshot),
            None,
        )
    }

    #[tokio::test]
    async fn w6_review_agent_future_definition_failure_cleanup_never_parses_executable() {
        let profile = future_profile();
        assert_eq!(profile, ProfileDisposition::Deferred);
        let task = task();
        let guard = CompletionGuard::Agent { job: task.task_id };
        for state in ["FAILED", "UNKNOWN", "CANCELLED"] {
            for route in 0..3 {
                let calls = Arc::new(Mutex::new(Vec::new()));
                let mut store = MockCleanup {
                    route,
                    live: true,
                    calls: calls.clone(),
                };
                assert!(
                    complete_if_required(
                        &mut store,
                        profile,
                        state,
                        &task,
                        &guard,
                        &json!({"state":state})
                    )
                    .await
                    .unwrap()
                );
                let expected = match route {
                    1 => vec!["authority", "task-failed", "branch-failed"],
                    2 => vec!["authority", "task-failed", "compensation-failed"],
                    _ => vec!["authority", "process-failed"],
                };
                assert_eq!(*calls.lock().unwrap(), expected);
            }
        }
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut store = MockCleanup {
            route: 0,
            live: false,
            calls: calls.clone(),
        };
        assert!(
            !complete_if_required(&mut store, profile, "SUCCEEDED", &task, &guard, &json!({}))
                .await
                .unwrap()
        );
        assert!(calls.lock().unwrap().is_empty());
        let error = complete_if_required(&mut store, profile, "FAILED", &task, &guard, &json!({}))
            .await
            .unwrap_err();
        assert!(matches!(error,sqlx::Error::Protocol(code) if code=="WORKFLOW_STALE_COMPLETION"));
        assert_eq!(*calls.lock().unwrap(), vec!["authority"]);
    }

    fn result(
        task: &ActiveTask,
        id: u128,
        state: &str,
    ) -> execution_runner_protocol::ExecutionResultView {
        execution_runner_protocol::ExecutionResultView {
            host_id: task.host_id,
            execution_id: Uuid::from_u128(id),
            request_id: Uuid::from_u128(id),
            origin_instance_id: "fixture".into(),
            subject_kind: "workflow-task".into(),
            subject_id: task.task_id,
            process_id: Some(task.process_id),
            task_id: Some(task.task_id),
            agent_session_id: None,
            agent_turn_id: None,
            agent_action_id: None,
            action_kind: "run-shell".into(),
            attempt_number: 1,
            lease_id: Uuid::from_u128(id),
            state: state.into(),
            fencing_token: 1,
            normalized_result: None,
            normalized_error: None,
            retry_classification: None,
            terminal: true,
            accepted: false,
        }
    }
    #[tokio::test]
    async fn w6_review_runner_future_definition_cleanup_does_not_block_later_result() {
        for state in ["FAILED", "CANCELLED", "TIMED_OUT"] {
            let task = task();
            let failed = result(&task, 1, state);
            let later = result(&task, 2, "SUCCEEDED");
            let calls = Arc::new(Mutex::new(Vec::new()));
            let observed = calls.clone();
            let mut sweep = crate::result_sweep::Sweep::default();
            let changed = sweep
                .poll(
                    &tokio_util::sync::CancellationToken::new(),
                    |_| async {
                        Ok::<_, crate::result_sweep::PollFailure<sqlx::Error>>(
                            execution_runner_protocol::ExecutionResultPage {
                                items: vec![failed, later],
                                next_cursor: None,
                            },
                        )
                    },
                    move |result| {
                        let task = task.clone();
                        let calls = observed.clone();
                        async move {
                            if result.execution_id.as_u128() == 1 {
                                let mut store = MockCleanup {
                                    route: 0,
                                    live: true,
                                    calls: calls.clone(),
                                };
                                complete_if_required(
                                    &mut store,
                                    future_profile(),
                                    &result.state,
                                    &task,
                                    &CompletionGuard::Runner {
                                        request: result.request_id,
                                        attempt: result.attempt_number,
                                    },
                                    &json!({"state":result.state}),
                                )
                                .await
                                .map_err(crate::result_sweep::PollFailure::System)?;
                                calls.lock().unwrap().push("failure-ack");
                            } else {
                                calls.lock().unwrap().push("later-compatible-result");
                            }
                            Ok(true)
                        }
                    },
                )
                .await
                .unwrap();
            assert!(changed);
            assert_eq!(
                *calls.lock().unwrap(),
                vec![
                    "authority",
                    "process-failed",
                    "failure-ack",
                    "later-compatible-result"
                ]
            );
        }
    }
}
