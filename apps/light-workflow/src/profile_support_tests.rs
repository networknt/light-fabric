use super::*;
use crate::profile_support::SupportedProfiles;

#[tokio::test]
async fn w6_direct_execution_defers_before_expression_or_dispatch_and_mismatch_wins() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    let (mut executor, engine) = executor();
    let mock = Arc::new(expression_requests::Mock::default());
    executor.w5_mock = Some(mock.clone());
    executor.supported_profiles = SupportedProfiles::from_evaluator(false);
    // Deliberately invalid expressions/targets would fail or contact the mock if
    // any executable arm ran. This exercises the actual direct-entry boundary.
    for task in [
        json!({"set":"${missing / 0}"}),
        json!({"assert":{"value":"${missing}","equals":1}}),
        json!({"switch":[{"a":{"when":"${missing}","then":"end"}}]}),
        json!({"wait":"PT1S"}),
        json!({"fork":{"compete":false,"branches":[{"a":{"set":"${missing}"}}]}}),
        json!({"ask":{"prompt":"${missing}"}}),
        json!({"call":"jsonrpc","with":{"endpoint":"https://rpc.invalid","method":"run","params":"${missing}"}}),
        json!({"call":"openrpc","with":{"document":{"endpoint":"https://docs.invalid"},"method":"run"}}),
        json!({"call":"http","with":{"method":"GET","endpoint":"https://http.invalid"}}),
        json!({"call":"agent","with":{"agent":"fixture","input":"${missing}"}}),
    ] {
        let c = claimed(task, json!({"secret":"W6_SECRET_SENTINEL"}));
        let error = executor
            .execute_task(&c)
            .await
            .err()
            .expect("unsupported direct entry must defer");
        assert!(
            error
                .to_string()
                .contains("WORKFLOW_EXPRESSION_UNAVAILABLE")
        );
        assert!(mock.requests.lock().unwrap().is_empty());
        let mut mismatch = c.clone();
        mismatch.expression_profile = "cel-workflow-v1".into();
        let failure = executor.execute_task(&mismatch).await.unwrap();
        assert_eq!(failure.status_code, "F");
        assert_eq!(failure.task_output["code"], "EVALUATOR_PROFILE_UNSUPPORTED");
        assert_eq!(failure.task_output["retryable"], false);
        assert!(
            failure
                .task_output
                .to_string()
                .contains("snapshot/profile mismatch")
        );
        assert!(
            !failure
                .task_output
                .to_string()
                .contains("W6_SECRET_SENTINEL")
        );
        assert!(mock.requests.lock().unwrap().is_empty());
    }
    close(engine).await;
}

#[test]
fn w6_integrity_precedes_compatibility_and_mutable_fallback_is_legacy_only() {
    let c = claimed(json!({"set":{}}), json!({}));
    let snapshot = serde_json::to_value(&c.raw_definition).unwrap();
    let digest = canonical_sha256(&snapshot).unwrap();
    let legacy = SupportedProfiles::from_evaluator(false);
    let both = SupportedProfiles::from_evaluator(true);
    assert_eq!(both.profiles(), vec!["cel-workflow-v1", "cel-workflow-v2"]);
    assert_eq!(
        legacy.check("cel-workflow-v2", Some(&snapshot), Some(&digest)),
        ProfileDisposition::Deferred
    );
    assert_eq!(
        both.check("cel-workflow-v2", Some(&snapshot), Some(&digest)),
        ProfileDisposition::V2
    );
    for worker in [&legacy, &both] {
        assert_eq!(
            worker.check("cel-workflow-v1", Some(&snapshot), Some(&digest)),
            ProfileDisposition::Corrupt
        );
        assert_eq!(
            worker.check("cel-workflow-v2", None, None),
            ProfileDisposition::Corrupt
        );
        assert_eq!(
            worker.check("cel-workflow-v2", Some(&snapshot), Some("wrong")),
            ProfileDisposition::Corrupt
        );
    }
    let future = json!({"document":{"metadata":{"lightExpressionProfile":"future-profile"}}});
    assert_eq!(
        legacy.check("future-profile", Some(&future), None),
        ProfileDisposition::Deferred
    );
    assert_eq!(
        legacy.check("other-profile", Some(&future), None),
        ProfileDisposition::Corrupt
    );
    assert_eq!(
        legacy.check("cel-workflow-v1", None, None),
        ProfileDisposition::Legacy
    );
    assert!(crate::profile_support::require_legacy_fallback("cel-workflow-v1").is_ok());
    for profile in ["cel-workflow-v2", "future-profile"] {
        assert!(crate::profile_support::require_legacy_fallback(profile).is_err());
    }
}

#[tokio::test]
#[ignore = "requires explicitly authorized disposable W4_TEST_DATABASE_URL; W6 transaction seam matrix"]
async fn w6_postgres_success_seams_defer_unchanged_and_reject_mismatch() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    // Each named branch invokes the production seam, not a duplicate predicate.
    for seam in [
        "completion",
        "fork",
        "compensation",
        "ask",
        "transition",
        "runner-gate",
        "fixed-result",
        "approval-wait",
        "fork-branch",
        "workflow-output",
    ] {
        for corrupt in [false, true] {
            let mut g = PgGate::new(json!({"set":{},"then":"end"})).await;
            g.executor.supported_profiles = SupportedProfiles::from_evaluator(false);
            if corrupt {
                sqlx::query("UPDATE process_info_t SET expression_profile='cel-workflow-v1'")
                    .execute(&g.pool)
                    .await
                    .unwrap();
            }
            let old: Value = sqlx::query_scalar("SELECT to_jsonb(t) FROM task_info_t t")
                .fetch_one(&g.pool)
                .await
                .unwrap();
            let context = g.context().await;
            let mut tx = g.pool.begin().await.unwrap();
            let attempt = g.attempt();
            let result: Result<(), String> = match seam {
                "completion" => g
                    .executor
                    .finish_task(
                        &mut tx,
                        &g.claimed,
                        TaskExecutionResult {
                            status_code: "C",
                            task_output: json!({}),
                            next_task: None,
                            context_data: None,
                        },
                    )
                    .await
                    .map_err(|e| e.to_string()),
                "fork-branch" => g
                    .executor
                    .reconcile_fork_branch(&mut tx, &g.claimed, true, json!({}))
                    .await
                    .map_err(|e| e.to_string()),
                "workflow-output" => {
                    sqlx::query("UPDATE process_info_t SET status_code='C'")
                        .execute(&mut *tx)
                        .await
                        .unwrap();
                    g.executor
                        .sync_invocation_state(&mut tx, &g.claimed, &json!({}), None)
                        .await
                        .map_err(|e| e.to_string())
                }
                "fork" => g
                    .executor
                    .start_fork(&mut tx, &g.claimed)
                    .await
                    .map_err(|e| e.to_string()),
                "compensation" => g
                    .executor
                    .reconcile_compensation_task(&mut tx, &g.claimed, true, &json!({}))
                    .await
                    .map_err(|e| e.to_string()),
                "ask" => g
                    .executor
                    .write_ask_assignments(
                        &mut tx,
                        &g.claimed,
                        &serde_json::from_value(json!({"prompt":"${missing}"})).unwrap(),
                        false,
                    )
                    .await
                    .map_err(|e| e.to_string()),
                "transition" => g
                    .executor
                    .handle_transition(
                        &mut tx,
                        &g.claimed.task,
                        &g.claimed.definition,
                        &g.claimed.raw_definition,
                        context.clone(),
                        json!({}),
                        None,
                        None,
                    )
                    .await
                    .map_err(|e| e.to_string()),
                "runner-gate" => g
                    .executor
                    .w4_runner_gate(&mut tx, &attempt)
                    .await
                    .and_then(|o| {
                        if o == Some(RunnerReconciliation::Deferred) {
                            Err(sqlx::Error::Protocol(
                                "WORKFLOW_EXPRESSION_UNAVAILABLE".into(),
                            ))
                        } else {
                            Ok(())
                        }
                    })
                    .map_err(|e| e.to_string()),
                "fixed-result" => g
                    .executor
                    .reconcile_fixed_action_attempt(&mut tx, &attempt, Uuid::new_v4())
                    .await
                    .map(|_| ())
                    .map_err(|e| e.to_string()),
                "approval-wait" => {
                    let binding = workflow_policy::ApprovalBinding {
                        operation: "apply-patch".into(),
                        target: "fixture".into(),
                        ttl_seconds: 60,
                    };
                    g.executor
                        .finish_runner_task_waiting_approval(
                            &mut tx,
                            &g.claimed,
                            &attempt,
                            &json!({}),
                            "fixture",
                            &binding,
                            false,
                        )
                        .await
                        .map_err(|e| e.to_string())
                }
                _ => unreachable!(),
            };
            if corrupt {
                result.unwrap();
                tx.commit().await.unwrap();
                let output: Value = sqlx::query_scalar("SELECT task_output FROM task_info_t")
                    .fetch_one(&g.pool)
                    .await
                    .unwrap();
                assert_eq!(output["code"], "EVALUATOR_PROFILE_UNSUPPORTED", "{seam}");
                assert_eq!(output["retryable"], false, "{seam}");
            } else {
                assert!(
                    result
                        .unwrap_err()
                        .contains("WORKFLOW_EXPRESSION_UNAVAILABLE"),
                    "{seam}"
                );
                tx.rollback().await.unwrap();
                let now: Value = sqlx::query_scalar("SELECT to_jsonb(t) FROM task_info_t t")
                    .fetch_one(&g.pool)
                    .await
                    .unwrap();
                assert_eq!(old, now, "{seam}");
            }
            assert_eq!(g.context().await, context, "{seam}");
            g.close().await;
        }
    }
}

#[tokio::test]
#[ignore = "requires explicitly authorized disposable W4_TEST_DATABASE_URL; W6 timer deferral and cleanup"]
async fn w6_postgres_timer_defer_mismatch_and_deadline_cleanup() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    for mode in ["defer", "mismatch", "deadline"] {
        let mut g = PgGate::new(json!({"wait":"PT1S","then":"end"})).await;
        g.executor.supported_profiles = SupportedProfiles::from_evaluator(false);
        sqlx::query("UPDATE task_info_t SET status_code='W',task_type='wait',execution_placement='host',lease_fencing_token=1").execute(&g.pool).await.unwrap();
        sqlx::query("INSERT INTO workflow_task_timer_t(host_id,task_id,generation,task_fence,state,wake_at,retry_after_ts,effective_deadline) VALUES($1,$2,1,1,'ARMED',clock_timestamp(),clock_timestamp(),clock_timestamp()+interval '1 hour')").bind(g.claimed.task.host_id).bind(g.claimed.task.task_id).execute(&g.pool).await.unwrap();
        if mode == "mismatch" {
            sqlx::query("UPDATE process_info_t SET expression_profile='cel-workflow-v1'")
                .execute(&g.pool)
                .await
                .unwrap();
        }
        if mode == "deadline" {
            sqlx::query("UPDATE task_info_t SET deadline_ts=clock_timestamp()-interval '1 second'")
                .execute(&g.pool)
                .await
                .unwrap();
        }
        let n = g
            .executor
            .fire_durable_timer(
                g.claimed.task.host_id,
                g.claimed.task.task_id,
                g.claimed.task.process_id,
                1,
            )
            .await
            .unwrap();
        let (state, generation): (String, i64) =
            sqlx::query_as("SELECT state,generation FROM workflow_task_timer_t")
                .fetch_one(&g.pool)
                .await
                .unwrap();
        assert_eq!(generation, 1);
        if mode == "defer" {
            assert_eq!(n, 0);
            assert_eq!(state, "ARMED");
        }
        if mode == "deadline" {
            assert_ne!(state, "ARMED");
        }
        if mode == "mismatch" {
            let output: Value = sqlx::query_scalar("SELECT task_output FROM task_info_t")
                .fetch_one(&g.pool)
                .await
                .unwrap();
            assert_eq!(output["code"], "EVALUATOR_PROFILE_UNSUPPORTED");
        }
        g.close().await;
    }
}

#[tokio::test]
#[ignore = "requires explicitly authorized disposable W4_TEST_DATABASE_URL; fresh native acceptance before its first mutation"]
async fn w6_postgres_native_success_deferral_mismatch_and_exact_report_recovery() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    for corrupt in [false, true] {
        let mut g =
            PgGate::new(json!({"call":"agent","with":{"agent":"fixture","mode":"service"}})).await;
        g.executor.supported_profiles = SupportedProfiles::from_evaluator(false);
        sqlx::query("CREATE TABLE workflow_agent_job_t(host_id uuid,job_id uuid,agent_def_id uuid,workflow_process_id uuid,workflow_task_id uuid,report jsonb,state text DEFAULT 'PENDING',deadline_ts timestamptz DEFAULT clock_timestamp()+interval '5 minutes',cancellation_requested_ts timestamptz)").execute(&g.pool).await.unwrap();
        let agent = Uuid::new_v4();
        sqlx::query("INSERT INTO workflow_agent_job_t(host_id,job_id,agent_def_id,workflow_process_id,workflow_task_id) VALUES($1,$2,$3,$4,$2)").bind(g.claimed.task.host_id).bind(g.claimed.task.task_id).bind(agent).bind(g.claimed.task.process_id).execute(&g.pool).await.unwrap();
        if corrupt {
            sqlx::query("UPDATE process_info_t SET expression_profile='cel-workflow-v1'")
                .execute(&g.pool)
                .await
                .unwrap();
        }
        let request = light_client::workflow_job_transport::Report {
            host_id: g.claimed.task.host_id,
            job_id: g.claimed.task.task_id,
            state: "SUCCEEDED".into(),
            output: Some(json!({"sentinel":"W6_PRIVATE"})),
            error: None,
            cleanup: None,
        };
        let before: Value = sqlx::query_scalar("SELECT to_jsonb(j) FROM workflow_agent_job_t j")
            .fetch_one(&g.pool)
            .await
            .unwrap();
        let status = crate::job_authorization::persist_verified_report_with_profiles(
            &g.pool,
            None,
            "fixture",
            agent,
            request.clone(),
            g.executor.supported_profiles(),
        )
        .await;
        assert_eq!(
            status,
            Err(if corrupt {
                axum::http::StatusCode::UNPROCESSABLE_ENTITY
            } else {
                axum::http::StatusCode::SERVICE_UNAVAILABLE
            })
        );
        let after: Value = sqlx::query_scalar("SELECT to_jsonb(j) FROM workflow_agent_job_t j")
            .fetch_one(&g.pool)
            .await
            .unwrap();
        assert_eq!(before, after);
        let accepted: Option<i32> = sqlx::query_scalar("SELECT accepted_attempt FROM task_info_t")
            .fetch_one(&g.pool)
            .await
            .unwrap();
        assert_eq!(accepted, None);
        if corrupt {
            let output: Value = sqlx::query_scalar("SELECT task_output FROM task_info_t")
                .fetch_one(&g.pool)
                .await
                .unwrap();
            assert_eq!(output["code"], "EVALUATOR_PROFILE_UNSUPPORTED");
            assert!(!output.to_string().contains("W6_PRIVATE"));
        } else {
            // A compatible worker reaches fresh structural validation. The
            // deliberately invalid result stays unconsumed there too.
            assert_eq!(
                crate::job_authorization::persist_verified_report_with_profiles(
                    &g.pool,
                    None,
                    "fixture",
                    agent,
                    request.clone(),
                    &SupportedProfiles::from_evaluator(true)
                )
                .await,
                Err(axum::http::StatusCode::BAD_REQUEST)
            );
        }
        sqlx::query("UPDATE workflow_agent_job_t SET report=$1")
            .bind(serde_json::to_value(&request).unwrap())
            .execute(&g.pool)
            .await
            .unwrap();
        assert_eq!(
            crate::job_authorization::persist_verified_report_with_profiles(
                &g.pool,
                None,
                "fixture",
                agent,
                request.clone(),
                g.executor.supported_profiles()
            )
            .await,
            Ok(axum::http::StatusCode::NO_CONTENT)
        );
        let mut changed = request;
        changed.error = Some(json!({"different":true}));
        assert_eq!(
            crate::job_authorization::persist_verified_report_with_profiles(
                &g.pool,
                None,
                "fixture",
                agent,
                changed,
                g.executor.supported_profiles()
            )
            .await,
            Err(axum::http::StatusCode::CONFLICT)
        );
        g.close().await;
    }
}

#[tokio::test]
#[ignore = "requires explicitly authorized disposable W4_TEST_DATABASE_URL; runner failure cleanup with no evaluator"]
async fn w6_postgres_runner_failure_cleanup_without_support_or_snapshot_context() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    let mut g =
        PgGate::new(json!({"set":{},"export":{"as":{"old":"${missing}"}},"then":"end"})).await;
    g.executor.supported_profiles = SupportedProfiles::from_evaluator(false);
    let mut attempt = g.attempt();
    attempt.state = "FAILED".into();
    attempt.normalized_error = Some(json!({"class":"fixture"}));
    sqlx::query("UPDATE task_info_t SET scheduling_request_id=$1,task_input=NULL")
        .bind(attempt.request_id)
        .execute(&g.pool)
        .await
        .unwrap();
    let old = g.context().await;
    let mut tx = g.pool.begin().await.unwrap();
    assert_eq!(
        g.executor.w4_runner_gate(&mut tx, &attempt).await.unwrap(),
        None
    );
    assert_eq!(
        g.executor
            .reconcile_runner_attempt(&mut tx, &attempt)
            .await
            .unwrap(),
        RunnerReconciliation::Completed
    );
    tx.commit().await.unwrap();
    assert_eq!(g.context().await, old);
    let state: String = sqlx::query_scalar("SELECT status_code FROM task_info_t")
        .fetch_one(&g.pool)
        .await
        .unwrap();
    assert_eq!(state, "F");
    g.close().await;
}

#[tokio::test]
#[ignore = "requires separately authorized disposable W4_TEST_DATABASE_URL; reviewed native job-level fences"]
async fn w6_review_postgres_native_job_expiry_cancellation_and_historical_recovery() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    for mode in ["expired", "cancel-requested", "cancelled", "terminal"] {
        let g =
            PgGate::new(json!({"call":"agent","with":{"agent":"fixture","mode":"service"}})).await;
        sqlx::query("CREATE TABLE workflow_agent_job_t(host_id uuid,job_id uuid,agent_def_id uuid,workflow_process_id uuid,workflow_task_id uuid,report jsonb,state text DEFAULT 'RUNNING',deadline_ts timestamptz DEFAULT clock_timestamp()+interval '5 minutes',cancellation_requested_ts timestamptz)").execute(&g.pool).await.unwrap();
        // Consumption sentinels: no acceptance or ledger mutation may precede rejection.
        sqlx::query(
            "CREATE TABLE development_turn_t(logical_turn_id text,state text,result jsonb)",
        )
        .execute(&g.pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO development_turn_t VALUES('fixture','RUNNING',NULL)")
            .execute(&g.pool)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE development_finding_receipt_t(receipt jsonb)")
            .execute(&g.pool)
            .await
            .unwrap();
        let agent = Uuid::new_v4();
        sqlx::query("INSERT INTO workflow_agent_job_t(host_id,job_id,agent_def_id,workflow_process_id,workflow_task_id) VALUES($1,$2,$3,$4,$2)")
            .bind(g.claimed.task.host_id).bind(g.claimed.task.task_id).bind(agent).bind(g.claimed.task.process_id).execute(&g.pool).await.unwrap();
        let update = match mode {
            "expired" => "UPDATE workflow_agent_job_t SET deadline_ts=clock_timestamp()",
            "cancel-requested" => {
                "UPDATE workflow_agent_job_t SET cancellation_requested_ts=clock_timestamp()"
            }
            "cancelled" => "UPDATE workflow_agent_job_t SET state='CANCELLED'",
            _ => "UPDATE workflow_agent_job_t SET state='FAILED'",
        };
        sqlx::query(update).execute(&g.pool).await.unwrap();
        let request = light_client::workflow_job_transport::Report {
            host_id: g.claimed.task.host_id,
            job_id: g.claimed.task.task_id,
            state: "SUCCEEDED".into(),
            output: Some(json!({"fixture":true})),
            error: None,
            cleanup: None,
        };
        let before:Value=sqlx::query_scalar("SELECT jsonb_build_object('job',(SELECT to_jsonb(j) FROM workflow_agent_job_t j),'task',(SELECT to_jsonb(t) FROM task_info_t t),'turn',(SELECT to_jsonb(t) FROM development_turn_t t),'findings',(SELECT count(*) FROM development_finding_receipt_t))").fetch_one(&g.pool).await.unwrap();
        let live:bool=sqlx::query_scalar("SELECT p.active AND p.status_code='A' AND i.state='RUNNING' AND i.deadline_ts>clock_timestamp() FROM process_info_t p JOIN workflow_invocation_t i USING(host_id,process_id)").fetch_one(&g.pool).await.unwrap();
        assert!(live);
        assert_eq!(
            crate::job_authorization::persist_verified_report_with_profiles(
                &g.pool,
                None,
                "fixture",
                agent,
                request.clone(),
                g.executor.supported_profiles()
            )
            .await,
            Err(axum::http::StatusCode::CONFLICT),
            "{mode}"
        );
        let after:Value=sqlx::query_scalar("SELECT jsonb_build_object('job',(SELECT to_jsonb(j) FROM workflow_agent_job_t j),'task',(SELECT to_jsonb(t) FROM task_info_t t),'turn',(SELECT to_jsonb(t) FROM development_turn_t t),'findings',(SELECT count(*) FROM development_finding_receipt_t))").fetch_one(&g.pool).await.unwrap();
        assert_eq!(before, after);
        sqlx::query("UPDATE workflow_agent_job_t SET report=$1")
            .bind(serde_json::to_value(&request).unwrap())
            .execute(&g.pool)
            .await
            .unwrap();
        assert_eq!(
            crate::job_authorization::persist_verified_report_with_profiles(
                &g.pool,
                None,
                "fixture",
                agent,
                request,
                g.executor.supported_profiles()
            )
            .await,
            Ok(axum::http::StatusCode::NO_CONTENT)
        );
        let accepted: Option<i32> = sqlx::query_scalar("SELECT accepted_attempt FROM task_info_t")
            .fetch_one(&g.pool)
            .await
            .unwrap();
        assert_eq!(accepted, None);
        g.close().await;
    }
}

#[tokio::test]
#[ignore = "requires separately authorized disposable W4_TEST_DATABASE_URL; future definition agent/runner cleanup"]
async fn w6_review_postgres_future_definition_failure_cleanup_and_replay() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    for channel in ["agent", "runner"] {
        for state in ["FAILED", "UNKNOWN"] {
            let mut g = PgGate::new(json!({"set":{},"export":{"as":{"never":"${missing}"}}})).await;
            g.executor.supported_profiles = SupportedProfiles::from_evaluator(false);
            let future = json!({"document":{"metadata":{"lightExpressionProfile":"future-profile"}},"do":42,"futureExecutable":{"shape":"unsupported"}});
            assert!(
                serde_json::from_value::<workflow_core::models::workflow::WorkflowDefinition>(
                    future.clone()
                )
                .is_err()
            );
            sqlx::query("UPDATE process_info_t SET expression_profile='future-profile',definition_snapshot=$1,definition_digest=$2")
                .bind(&future).bind(canonical_sha256(&future).unwrap()).execute(&g.pool).await.unwrap();
            let context = g.context().await;
            if channel == "agent" {
                sqlx::query("CREATE TABLE workflow_agent_job_t(host_id uuid,job_id uuid,workflow_process_id uuid,workflow_task_id uuid,state text,public_output jsonb,error jsonb,output_schema jsonb)").execute(&g.pool).await.unwrap();
                sqlx::query("UPDATE task_info_t SET status_code='W',task_input=NULL")
                    .execute(&g.pool)
                    .await
                    .unwrap();
                sqlx::query(
                    "INSERT INTO workflow_agent_job_t VALUES($1,$2,$3,$2,$4,NULL,'{}','{}')",
                )
                .bind(g.claimed.task.host_id)
                .bind(g.claimed.task.task_id)
                .bind(g.claimed.task.process_id)
                .bind(state)
                .execute(&g.pool)
                .await
                .unwrap();
                assert!(
                    g.executor
                        .reconcile_agent_job(g.claimed.task.host_id, g.claimed.task.task_id)
                        .await
                        .unwrap()
                );
                assert!(
                    !g.executor
                        .reconcile_agent_job(g.claimed.task.host_id, g.claimed.task.task_id)
                        .await
                        .unwrap()
                );
            } else {
                let mut attempt = g.attempt();
                attempt.state = state.into();
                attempt.normalized_error = Some(json!({"class":"fixture"}));
                sqlx::query("UPDATE task_info_t SET scheduling_request_id=$1,task_input=NULL")
                    .bind(attempt.request_id)
                    .execute(&g.pool)
                    .await
                    .unwrap();
                let mut tx = g.pool.begin().await.unwrap();
                let outcome = g
                    .executor
                    .reconcile_runner_attempt(&mut tx, &attempt)
                    .await
                    .unwrap();
                if state == "UNKNOWN" {
                    assert_eq!(outcome, RunnerReconciliation::Deferred);
                    tx.rollback().await.unwrap();
                    let accepted: Option<i32> =
                        sqlx::query_scalar("SELECT accepted_attempt FROM task_info_t")
                            .fetch_one(&g.pool)
                            .await
                            .unwrap();
                    assert_eq!(accepted, None);
                    assert_eq!(g.context().await, context);
                    g.close().await;
                    continue;
                }
                assert_eq!(outcome, RunnerReconciliation::Completed);
                tx.commit().await.unwrap();
                let mut tx = g.pool.begin().await.unwrap();
                assert_eq!(
                    g.executor
                        .reconcile_runner_attempt(&mut tx, &attempt)
                        .await
                        .unwrap(),
                    RunnerReconciliation::Replay
                );
                tx.rollback().await.unwrap();
            }
            let (status, marker): (String, String) =
                sqlx::query_as("SELECT status_code,result_code FROM task_info_t")
                    .fetch_one(&g.pool)
                    .await
                    .unwrap();
            assert_eq!((status.as_str(), marker.as_str()), ("F", "W4_STEP_FAILED"));
            let invocation: String = sqlx::query_scalar("SELECT state FROM workflow_invocation_t")
                .fetch_one(&g.pool)
                .await
                .unwrap();
            assert_eq!(invocation, "FAILED");
            assert_eq!(g.context().await, context);
            g.close().await;
        }
    }
}

#[tokio::test]
#[ignore = "requires separately authorized disposable W4_TEST_DATABASE_URL; future-profile fork/compensation accounting"]
async fn w6_review_postgres_future_failure_branch_compensation_without_advancement() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    for mode in ["partial", "terminal", "winner", "compensation"] {
        let mut g = PgGate::new(json!({"set":{}})).await;
        g.executor.supported_profiles = SupportedProfiles::from_evaluator(false);
        let future =
            json!({"document":{"metadata":{"lightExpressionProfile":"future-profile"}},"do":42});
        sqlx::query("UPDATE process_info_t SET expression_profile='future-profile',definition_snapshot=$1,definition_digest=$2")
            .bind(&future).bind(canonical_sha256(&future).unwrap()).execute(&g.pool).await.unwrap();
        sqlx::query("CREATE TABLE workflow_agent_job_t(host_id uuid,job_id uuid,workflow_process_id uuid,workflow_task_id uuid,state text,public_output jsonb,error jsonb,output_schema jsonb)").execute(&g.pool).await.unwrap();
        sqlx::query("UPDATE task_info_t SET status_code='W'")
            .execute(&g.pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO workflow_agent_job_t VALUES($1,$2,$3,$2,'UNKNOWN',NULL,'{}','{}')",
        )
        .bind(g.claimed.task.host_id)
        .bind(g.claimed.task.task_id)
        .bind(g.claimed.task.process_id)
        .execute(&g.pool)
        .await
        .unwrap();
        let join = Uuid::new_v4();
        if mode == "compensation" {
            sqlx::query("UPDATE task_info_t SET is_compensation=true")
                .execute(&g.pool)
                .await
                .unwrap();
            sqlx::query("UPDATE workflow_invocation_t SET state='COMPENSATING',cancel_requested_ts=clock_timestamp()")
                .execute(&g.pool).await.unwrap();
        } else {
            sqlx::query("INSERT INTO workflow_fork_join_t(host_id,join_id,fork_task_id,expected_branches,compete,state) VALUES($1,$2,$3,$4,$5,'RUNNING')")
                .bind(g.claimed.task.host_id).bind(join).bind(Uuid::new_v4()).bind(if mode=="terminal"{1}else{2}).bind(mode=="winner").execute(&g.pool).await.unwrap();
            sqlx::query("INSERT INTO workflow_fork_branch_t(host_id,join_id,task_id,branch_name,state) VALUES($1,$2,$3,'failed','RUNNING')")
                .bind(g.claimed.task.host_id).bind(join).bind(g.claimed.task.task_id).execute(&g.pool).await.unwrap();
            if mode != "terminal" {
                sqlx::query("INSERT INTO workflow_fork_branch_t(host_id,join_id,task_id,branch_name,state,result) VALUES($1,$2,$3,'other',$4,'{}')")
                    .bind(g.claimed.task.host_id).bind(join).bind(Uuid::new_v4()).bind(if mode=="winner"{"COMPLETED"}else{"RUNNING"}).execute(&g.pool).await.unwrap();
            }
        }
        let context = g.context().await;
        assert!(
            g.executor
                .reconcile_agent_job(g.claimed.task.host_id, g.claimed.task.task_id)
                .await
                .unwrap()
        );
        assert_eq!(g.context().await, context);
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM task_info_t")
            .fetch_one(&g.pool)
            .await
            .unwrap();
        assert_eq!(count, 1);
        let invocation: String = sqlx::query_scalar("SELECT state FROM workflow_invocation_t")
            .fetch_one(&g.pool)
            .await
            .unwrap();
        if mode == "compensation" || mode == "terminal" {
            assert_eq!(invocation, "FAILED");
        } else {
            assert_eq!(invocation, "RUNNING");
        }
        if mode != "compensation" {
            let (state, failed): (String, i32) =
                sqlx::query_as("SELECT state,failed_branches FROM workflow_fork_join_t")
                    .fetch_one(&g.pool)
                    .await
                    .unwrap();
            assert_eq!(failed, 1);
            assert_eq!(
                state,
                if mode == "terminal" {
                    "FAILED"
                } else {
                    "RUNNING"
                }
            );
        }
        g.close().await;
    }
}

#[tokio::test]
#[ignore = "requires explicitly authorized disposable W4_TEST_DATABASE_URL; native direct enqueue and terminal reconciliation"]
async fn w6_postgres_native_enqueue_and_agent_result_boundaries() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    for mode in ["defer", "enqueue-mismatch", "agent-mismatch", "failure"] {
        let mut g=PgGate::new(json!({"call":"agent","with":{"agent":"fixture","mode":"service"},"export":{"as":{"old":"${missing}"}}})).await;
        g.executor.supported_profiles = SupportedProfiles::from_evaluator(false);
        sqlx::query("ALTER TABLE workflow_invocation_t ADD COLUMN principal_subject text,ADD COLUMN end_user_subject text").execute(&g.pool).await.unwrap();
        sqlx::query("CREATE TABLE workflow_agent_job_t(host_id uuid,job_id uuid,workflow_process_id uuid,workflow_task_id uuid,state text,public_output jsonb,error jsonb,output_schema jsonb)").execute(&g.pool).await.unwrap();
        if mode.ends_with("mismatch") {
            sqlx::query("UPDATE process_info_t SET expression_profile='cel-workflow-v1'")
                .execute(&g.pool)
                .await
                .unwrap();
        }
        if mode == "defer" || mode == "enqueue-mismatch" {
            let error = crate::native_jobs::enqueue_with_artifacts_supported(
                &g.pool,
                g.claimed.task.host_id,
                g.claimed.task.process_id,
                g.claimed.task.task_id,
                "step",
                Uuid::new_v4(),
                json!({}),
                json!({"type":"object"}),
                Utc::now() + chrono::Duration::minutes(5),
                100,
                0,
                0,
                1,
                None,
                g.executor.supported_profiles(),
                None,
            )
            .await
            .unwrap_err();
            assert!(error.to_string().contains(if mode == "defer" {
                "WORKFLOW_EXPRESSION_UNAVAILABLE"
            } else {
                "EVALUATOR_PROFILE_UNSUPPORTED"
            }));
            let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM workflow_agent_job_t")
                .fetch_one(&g.pool)
                .await
                .unwrap();
            assert_eq!(jobs, 0);
            if mode.ends_with("mismatch") {
                g.close().await;
                continue;
            }
        }
        let state = if mode == "failure" {
            "FAILED"
        } else {
            "SUCCEEDED"
        };
        sqlx::query("UPDATE task_info_t SET status_code='W'")
            .execute(&g.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO workflow_agent_job_t VALUES($1,$2,$3,$2,$4,'{}',NULL,'{}')")
            .bind(g.claimed.task.host_id)
            .bind(g.claimed.task.task_id)
            .bind(g.claimed.task.process_id)
            .bind(state)
            .execute(&g.pool)
            .await
            .unwrap();
        let old = g.context().await;
        assert_eq!(
            g.executor
                .reconcile_agent_job(g.claimed.task.host_id, g.claimed.task.task_id)
                .await
                .unwrap(),
            mode != "defer"
        );
        let task: String = sqlx::query_scalar("SELECT status_code FROM task_info_t")
            .fetch_one(&g.pool)
            .await
            .unwrap();
        assert_eq!(task, if mode != "defer" { "F" } else { "W" });
        assert_eq!(g.context().await, old);
        g.close().await;
    }
}

#[tokio::test]
#[ignore = "requires explicitly authorized disposable W4_TEST_DATABASE_URL; support-filtered candidate window and compatible pickup"]
async fn w6_postgres_selection_does_not_starve_compatible_pending_candidates() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    let g = PgGate::new(json!({"set":{}})).await;
    sqlx::query("ALTER TABLE process_info_t ADD COLUMN priority int DEFAULT 100")
        .execute(&g.pool)
        .await
        .unwrap();
    let legacy = Uuid::new_v4();
    sqlx::query("INSERT INTO process_info_t(host_id,process_id,status_code,expression_profile,priority) VALUES($1,$2,'A','cel-workflow-v1',1)").bind(g.claimed.task.host_id).bind(legacy).execute(&g.pool).await.unwrap();
    let query = format!(
        "SELECT process_id FROM process_info_t p WHERE {} ORDER BY priority DESC LIMIT 1",
        crate::profile_support::eligible("p", "$1")
    );
    let before: Value =
        sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(p) ORDER BY priority) FROM process_info_t p")
            .fetch_one(&g.pool)
            .await
            .unwrap();
    let selected: Uuid = sqlx::query_scalar(&query)
        .bind(SupportedProfiles::from_evaluator(false).profiles())
        .fetch_one(&g.pool)
        .await
        .unwrap();
    assert_eq!(selected, legacy);
    let selected: Uuid = sqlx::query_scalar(&query)
        .bind(SupportedProfiles::from_evaluator(true).profiles())
        .fetch_one(&g.pool)
        .await
        .unwrap();
    assert_eq!(selected, g.claimed.task.process_id);
    let after: Value =
        sqlx::query_scalar("SELECT jsonb_agg(to_jsonb(p) ORDER BY priority) FROM process_info_t p")
            .fetch_one(&g.pool)
            .await
            .unwrap();
    assert_eq!(before, after);
    sqlx::query(
        "UPDATE process_info_t SET expression_profile='cel-workflow-v1' WHERE priority=100",
    )
    .execute(&g.pool)
    .await
    .unwrap();
    let selected: Uuid = sqlx::query_scalar(&query)
        .bind(SupportedProfiles::from_evaluator(false).profiles())
        .fetch_one(&g.pool)
        .await
        .unwrap();
    assert_eq!(selected, g.claimed.task.process_id);
    g.close().await;
}

#[tokio::test]
#[ignore = "requires explicitly authorized disposable W4_TEST_DATABASE_URL; approval rejection before artifact checks or new intent"]
async fn w6_postgres_approval_deferral_and_mismatch_preserve_intent() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    for corrupt in [false, true] {
        let g = PgGate::new(json!({"set":{}})).await;
        sqlx::query("ALTER TABLE workflow_approval_t ADD COLUMN process_id uuid,ADD COLUMN preceding_execution_id uuid,ADD COLUMN artifact_digest_set jsonb,ADD COLUMN provenance_digest text,ADD COLUMN target text,ADD COLUMN operation text,ADD COLUMN policy_digest text,ADD COLUMN expires_ts timestamptz").execute(&g.pool).await.unwrap();
        sqlx::query("CREATE TABLE execution_attempt_t(host_id uuid,execution_id uuid,request_id uuid,normalized_result jsonb)").execute(&g.pool).await.unwrap();
        sqlx::query("CREATE TABLE runner_scheduling_request_t(host_id uuid,request_id uuid,normalized_requirements jsonb,execution_spec jsonb,policy_snapshot_id uuid)").execute(&g.pool).await.unwrap();
        let approval = Uuid::new_v4();
        let execution = Uuid::new_v4();
        let request = Uuid::new_v4();
        sqlx::query("INSERT INTO workflow_approval_t(host_id,approval_id,process_id,task_id,preceding_execution_id,state,artifact_digest_set,target,operation,policy_digest,expires_ts) VALUES($1,$2,$3,$4,$5,'REQUESTED','[]','fixture','apply-patch','policy',clock_timestamp()+interval '1 hour')").bind(g.claimed.task.host_id).bind(approval).bind(g.claimed.task.process_id).bind(g.claimed.task.task_id).bind(execution).execute(&g.pool).await.unwrap();
        sqlx::query("INSERT INTO execution_attempt_t VALUES($1,$2,$3,'{}')")
            .bind(g.claimed.task.host_id)
            .bind(execution)
            .bind(request)
            .execute(&g.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO runner_scheduling_request_t VALUES($1,$2,'{}','{}',$3)")
            .bind(g.claimed.task.host_id)
            .bind(request)
            .bind(Uuid::new_v4())
            .execute(&g.pool)
            .await
            .unwrap();
        if corrupt {
            sqlx::query("UPDATE process_info_t SET expression_profile='cel-workflow-v1'")
                .execute(&g.pool)
                .await
                .unwrap();
        }
        let service =
            crate::approval::WorkflowApprovalService::new(g.pool.clone(), "fixture", "fixture");
        let decision = crate::approval::ApprovalDecision {
            host_id: g.claimed.task.host_id,
            approval_id: approval,
            actor: "fixture".into(),
            reason: None,
            artifact_digest_set: json!([]),
            provenance_digest: None,
            target: "fixture".into(),
            operation: "apply-patch".into(),
            policy_digest: "policy".into(),
        };
        let error = service.approve(&decision).await.unwrap_err();
        assert!(error.to_string().contains(if corrupt {
            "EVALUATOR_PROFILE_UNSUPPORTED"
        } else {
            "WORKFLOW_EXPRESSION_UNAVAILABLE"
        }));
        let state: String = sqlx::query_scalar("SELECT state FROM workflow_approval_t")
            .fetch_one(&g.pool)
            .await
            .unwrap();
        assert_eq!(state, "REQUESTED");
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM execution_attempt_t")
            .fetch_one(&g.pool)
            .await
            .unwrap();
        assert_eq!(n, 1);
        g.close().await;
    }
}

#[tokio::test]
#[ignore = "requires explicitly authorized disposable W4_TEST_DATABASE_URL; dormant fixed dispatch/inspection and UNKNOWN cleanup"]
async fn w6_postgres_fixed_action_deferral_mismatch_and_unknown_cleanup() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    for mode in [
        "dispatch-defer",
        "dispatch-mismatch",
        "inspect-defer",
        "inspect-mismatch",
        "cleanup",
    ] {
        let g = PgGate::new(json!({"set":{}})).await;
        sqlx::query("ALTER TABLE workflow_approval_t ADD COLUMN process_id uuid,ADD COLUMN operation text,ADD COLUMN target text,ADD COLUMN policy_digest text,ADD COLUMN provenance_digest text,ADD COLUMN artifact_digest_set jsonb").execute(&g.pool).await.unwrap();
        sqlx::query("CREATE TABLE execution_attempt_t(host_id uuid,execution_id uuid,state text,lease_deadline_ts timestamptz,terminal_ts timestamptz,normalized_error jsonb,cleanup_state text,retry_classification text,updated_ts timestamptz)").execute(&g.pool).await.unwrap();
        sqlx::query("CREATE TABLE execution_fixed_action_t(host_id uuid,fixed_action_id uuid,execution_id uuid,approval_id uuid,repository_reference text DEFAULT 'fixture',base_commit text,repository_object_format text DEFAULT 'sha1',target_ref text DEFAULT 'fixture',patch_artifact_reference text DEFAULT 'fixture',artifact_digest text DEFAULT 'digest',policy_digest text DEFAULT 'policy',changed_paths jsonb DEFAULT '[]',action_kind text DEFAULT 'create-branch',action_spec jsonb DEFAULT '{\"target\":\"fixture\"}',provenance_digest text,idempotency_key text,state text,created_ts timestamptz DEFAULT clock_timestamp(),updated_ts timestamptz DEFAULT clock_timestamp(),unknown_since_ts timestamptz DEFAULT clock_timestamp(),next_reconcile_ts timestamptz,reconciliation_claim_token uuid,reconciliation_lease_expires_ts timestamptz,reconciliation_attempt_count int DEFAULT 0,result_evidence jsonb)").execute(&g.pool).await.unwrap();
        let approval = Uuid::new_v4();
        let execution = Uuid::new_v4();
        let action = Uuid::new_v4();
        sqlx::query("INSERT INTO workflow_approval_t(host_id,approval_id,task_id,process_id,state,consuming_execution_id,operation,target,policy_digest,artifact_digest_set) VALUES($1,$2,$3,$4,'CONSUMED',$5,'create-branch','fixture','policy','[\"digest\"]')").bind(g.claimed.task.host_id).bind(approval).bind(g.claimed.task.task_id).bind(g.claimed.task.process_id).bind(execution).execute(&g.pool).await.unwrap();
        let dispatch = mode.starts_with("dispatch");
        sqlx::query("INSERT INTO execution_attempt_t(host_id,execution_id,state) VALUES($1,$2,$3)")
            .bind(g.claimed.task.host_id)
            .bind(execution)
            .bind(if dispatch { "CREATED" } else { "STARTED" })
            .execute(&g.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO execution_fixed_action_t(host_id,fixed_action_id,execution_id,approval_id,state) VALUES($1,$2,$3,$4,$5)").bind(g.claimed.task.host_id).bind(action).bind(execution).bind(approval).bind(if dispatch {"REQUESTED"}else{"UNKNOWN"}).execute(&g.pool).await.unwrap();
        if mode.ends_with("mismatch") {
            sqlx::query("UPDATE process_info_t SET expression_profile='cel-workflow-v1'")
                .execute(&g.pool)
                .await
                .unwrap();
        }
        if mode == "cleanup" {
            sqlx::query("UPDATE execution_fixed_action_t SET unknown_since_ts=clock_timestamp()-interval '25 hours'").execute(&g.pool).await.unwrap();
        }
        let before: Value =
            sqlx::query_scalar("SELECT to_jsonb(f) FROM execution_fixed_action_t f")
                .fetch_one(&g.pool)
                .await
                .unwrap();
        let paths =
            execution_security::ProtectedPathPolicy::new(vec![], Default::default(), true).unwrap();
        // These paths don't exist: any artifact/provider execution would fail
        // instead of quietly satisfying the unchanged-row assertions.
        let executor = crate::fixed_action::FixedActionExecutor::new(
            g.pool.clone(),
            "/unused/w6-work".into(),
            "/unused/w6-artifacts".into(),
            "fixture",
            paths,
        );
        let progressed = executor.run_once().await.unwrap();
        assert_eq!(progressed, mode == "cleanup" || mode.ends_with("mismatch"));
        let after: Value = sqlx::query_scalar("SELECT to_jsonb(f) FROM execution_fixed_action_t f")
            .fetch_one(&g.pool)
            .await
            .unwrap();
        assert_eq!(after["reconciliation_attempt_count"], 0);
        assert!(after["reconciliation_claim_token"].is_null());
        if mode != "cleanup" {
            assert_eq!(before, after);
        }
        if mode.ends_with("mismatch") {
            let output: Value = sqlx::query_scalar("SELECT task_output FROM task_info_t")
                .fetch_one(&g.pool)
                .await
                .unwrap();
            assert_eq!(output["code"], "EVALUATOR_PROFILE_UNSUPPORTED");
        }
        if mode == "cleanup" {
            assert_eq!(after["result_evidence"]["operatorActionRequired"], true);
            let state: String = sqlx::query_scalar("SELECT state FROM execution_attempt_t")
                .fetch_one(&g.pool)
                .await
                .unwrap();
            assert_eq!(state, "UNKNOWN");
        }
        if mode.ends_with("mismatch") {
            assert!(
                !executor.run_once().await.unwrap(),
                "terminal mismatches must not occupy the next candidate window"
            );
        }
        g.close().await;
    }
}

#[tokio::test]
#[ignore = "requires explicitly authorized disposable W4_TEST_DATABASE_URL; native pending delivery, cleanup and compatible pickup"]
async fn w6_postgres_native_delivery_deferral_mismatch_cleanup_and_pickup() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    for mode in ["defer", "mismatch", "cancel", "expiry"] {
        let g =
            PgGate::new(json!({"call":"agent","with":{"agent":"fixture","mode":"service"}})).await;
        sqlx::query("ALTER TABLE workflow_invocation_t ADD COLUMN end_user_subject varchar(255)")
            .execute(&g.pool)
            .await
            .unwrap();
        let subject = format!("w6-fixture-user-{}", Uuid::new_v4());
        let updated = sqlx::query("UPDATE workflow_invocation_t SET end_user_subject=$1 WHERE host_id=$2 AND process_id=$3")
            .bind(&subject).bind(g.claimed.task.host_id).bind(g.claimed.task.process_id)
            .execute(&g.pool).await.unwrap();
        assert_eq!(updated.rows_affected(), 1);
        sqlx::query("ALTER TABLE workflow_invocation_t ALTER COLUMN end_user_subject SET NOT NULL")
            .execute(&g.pool)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE workflow_agent_job_t(host_id uuid,job_id uuid,agent_def_id uuid,workflow_process_id uuid,workflow_task_id uuid,state text,created_ts timestamptz DEFAULT clock_timestamp(),cancellation_requested_ts timestamptz,deadline_ts timestamptz DEFAULT clock_timestamp()+interval '1 hour',input jsonb DEFAULT '{}',input_schema_digest text DEFAULT 'digest',output_schema jsonb DEFAULT '{}',token_budget bigint DEFAULT 100,cost_budget_micros bigint DEFAULT 0,delegation_depth int DEFAULT 0,maximum_delegation_depth int DEFAULT 1)").execute(&g.pool).await.unwrap();
        let agent = Uuid::new_v4();
        sqlx::query("INSERT INTO workflow_agent_job_t(host_id,job_id,agent_def_id,workflow_process_id,workflow_task_id,state) VALUES($1,$2,$3,$4,$2,'PENDING')").bind(g.claimed.task.host_id).bind(g.claimed.task.task_id).bind(agent).bind(g.claimed.task.process_id).execute(&g.pool).await.unwrap();
        if mode == "mismatch" {
            sqlx::query("UPDATE process_info_t SET expression_profile='cel-workflow-v1'")
                .execute(&g.pool)
                .await
                .unwrap();
        }
        if mode == "cancel" {
            sqlx::query("UPDATE workflow_invocation_t SET cancel_requested_ts=clock_timestamp()")
                .execute(&g.pool)
                .await
                .unwrap();
        }
        if mode == "expiry" {
            sqlx::query(
                "UPDATE workflow_agent_job_t SET deadline_ts=clock_timestamp()-interval '1 second'",
            )
            .execute(&g.pool)
            .await
            .unwrap();
        }
        let old: Value = sqlx::query_scalar("SELECT to_jsonb(j) FROM workflow_agent_job_t j")
            .fetch_one(&g.pool)
            .await
            .unwrap();
        let mut calls = 0;
        let jobs = crate::job_authorization::pending_jobs_guarded(
            &g.pool,
            &SupportedProfiles::from_evaluator(false),
            g.claimed.task.host_id,
            agent,
            |_| {
                calls += 1;
                std::future::ready(Ok(()))
            },
        )
        .await
        .unwrap();
        assert_eq!(calls, 0);
        assert_eq!(
            jobs.len(),
            usize::from(mode == "cancel" || mode == "expiry")
        );
        for job in jobs {
            assert!(job.cancellation_requested);
            assert_eq!(job.end_user_subject, subject);
        }
        let after: Value = sqlx::query_scalar("SELECT to_jsonb(j) FROM workflow_agent_job_t j")
            .fetch_one(&g.pool)
            .await
            .unwrap();
        assert_eq!(old, after);
        if mode == "mismatch" {
            let output: Value = sqlx::query_scalar("SELECT task_output FROM task_info_t")
                .fetch_one(&g.pool)
                .await
                .unwrap();
            assert_eq!(output["code"], "EVALUATOR_PROFILE_UNSUPPORTED");
        }
        if mode == "defer" {
            let jobs = crate::job_authorization::pending_jobs_guarded(
                &g.pool,
                &SupportedProfiles::from_evaluator(true),
                g.claimed.task.host_id,
                agent,
                |_| {
                    calls += 1;
                    std::future::ready(Ok(()))
                },
            )
            .await
            .unwrap();
            assert_eq!(calls, 1);
            assert_eq!(jobs.len(), 1);
            assert!(!jobs[0].cancellation_requested);
            assert_eq!(jobs[0].end_user_subject, subject);
        }
        g.close().await;
    }
}

#[tokio::test]
#[ignore = "requires explicitly authorized disposable W4_TEST_DATABASE_URL; after-start UNKNOWN recovery stays evaluator-independent"]
async fn w6_postgres_lease_reaper_unknown_is_cleanup_without_new_authority() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    for corrupt in [false, true] {
        let g = PgGate::new(json!({"set":"${missing}"})).await;
        sqlx::query("CREATE TABLE execution_attempt_t(host_id uuid,execution_id uuid,request_id uuid,origin_service_id text,origin_instance_id text,subject_id uuid,lease_id uuid,fencing_token bigint,lease_started_ts timestamptz,lease_deadline_ts timestamptz,state text,terminal_ts timestamptz,normalized_error jsonb,retry_classification text,updated_ts timestamptz)").execute(&g.pool).await.unwrap();
        sqlx::query("CREATE TABLE execution_runtime_audit_t(host_id uuid,origin_kind text,origin_service_id text,origin_instance_id text,subject_kind text,subject_id uuid,execution_id uuid,actor text,event_type text,redacted_payload jsonb)").execute(&g.pool).await.unwrap();
        sqlx::query(
            "CREATE TABLE runner_scheduling_request_t(request_id uuid,state text,retry_count int)",
        )
        .execute(&g.pool)
        .await
        .unwrap();
        let execution = Uuid::new_v4();
        let request = Uuid::new_v4();
        let lease = Uuid::new_v4();
        sqlx::query("INSERT INTO execution_attempt_t(host_id,execution_id,request_id,origin_service_id,origin_instance_id,subject_id,lease_id,fencing_token,lease_started_ts,lease_deadline_ts,state) VALUES($1,$2,$3,'fixture','fixture',$4,$5,7,clock_timestamp(),clock_timestamp()-interval '1 second','STARTED')").bind(g.claimed.task.host_id).bind(execution).bind(request).bind(g.claimed.task.task_id).bind(lease).execute(&g.pool).await.unwrap();
        sqlx::query("INSERT INTO runner_scheduling_request_t VALUES($1,'ATTEMPT_CREATED',0)")
            .bind(request)
            .execute(&g.pool)
            .await
            .unwrap();
        if corrupt {
            sqlx::query("UPDATE process_info_t SET expression_profile='cel-workflow-v1'")
                .execute(&g.pool)
                .await
                .unwrap();
        }
        let old = g.context().await;
        let reaper = crate::lease_reaper::LeaseReaper::new(g.pool.clone());
        assert!(reaper.run_once().await.unwrap());
        assert!(!reaper.run_once().await.unwrap());
        let (state, class, fence): (String, String, i64) = sqlx::query_as(
            "SELECT state,retry_classification,fencing_token FROM execution_attempt_t",
        )
        .fetch_one(&g.pool)
        .await
        .unwrap();
        assert_eq!(
            (state, class, fence),
            ("UNKNOWN".into(), "inspect-required".into(), 7)
        );
        let (state, retries): (String, i32) =
            sqlx::query_as("SELECT state,retry_count FROM runner_scheduling_request_t")
                .fetch_one(&g.pool)
                .await
                .unwrap();
        assert_eq!((state, retries), ("ATTEMPT_CREATED".into(), 0));
        assert_eq!(g.context().await, old);
        g.close().await;
    }
}
