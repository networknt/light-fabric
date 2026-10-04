use super::*;
use expression_completion::{ProfileDisposition, disposition, snapshot_context};
use sqlx::postgres::PgPoolOptions;
use workflow_expression::{Engine, WorkerConfig};

#[path = "profile_support_tests.rs"]
mod w6_tests;

fn claimed(task: Value, context: Value) -> ClaimedTask {
    let raw = json!({"document":{"dsl":"1.0.3","namespace":"test","name":"w4","version":"1.0.0","metadata":{"lightExpressionProfile":"cel-workflow-v2"}},"evaluate":{"language":"cel"},"do":[{"step":task}]});
    ClaimedTask {
        expression_profile: "cel-workflow-v2".into(),
        input_data: json!({"original":9}),
        task: ActiveTask {
            host_id: Uuid::new_v4(),
            task_id: Uuid::new_v4(),
            task_type: "set".into(),
            process_id: Uuid::new_v4(),
            wf_instance_id: Uuid::new_v4().to_string(),
            wf_task_id: "step".into(),
            status_code: "A".into(),
            result_code: None,
        },
        wf_def_id: Uuid::new_v4(),
        context_data: context,
        definition: serde_json::from_value(raw.clone()).unwrap(),
        raw_definition: serde_yaml::to_value(raw).unwrap(),
        host_lease: None,
        completion_guard: None,
    }
}
fn executor() -> (TaskExecutor, Engine) {
    let engine = crate::expression_test_support::engine(WorkerConfig {
        workers: 1,
        cache_entries: 32,
        cache_bytes: 1024 * 1024,
        ..WorkerConfig::default()
    })
    .unwrap();
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
        .unwrap();
    (
        TaskExecutor::new(pool).with_expression_engine(engine.clone()),
        engine,
    )
}
async fn close(engine: Engine) {
    tokio::task::spawn_blocking(move || engine.shutdown(Duration::from_secs(2)))
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn w4_set_and_assert_forms_immutable_input_and_bindings() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    let (executor, engine) = executor();
    let context = json!({"original":3,"workflow":{"input":"shadow"},"a":[{"v":1.25}],"integer":u64::MAX,"secret":"W4_CAPTURED_SENTINEL"});
    let c = claimed(
        json!({"set":{"whole":"${workflow.input.original}","embedded":"x=${string(context.original)}","literal":"plain","double":"${context.a[0].v}","integer":"${context.integer}"}}),
        context.clone(),
    );
    let result = executor.execute_task(&c).await.unwrap();
    assert_eq!(
        result.task_output,
        json!({"whole":9,"embedded":"x=3","literal":"plain","double":1.25,"integer":u64::MAX})
    );
    let c = claimed(json!({"set":"${context.a}"}), context.clone());
    assert_eq!(
        executor.execute_task(&c).await.unwrap().task_output,
        json!([{"v":1.25}])
    );
    for (site, assertion) in [
        ("value", json!({"value":"${context.original}","equals":3})),
        ("equals", json!({"value":3,"equals":"${context.original}"})),
        ("contains", json!({"value":"abc","contains":"${'b'}"})),
        (
            "json",
            json!({"value":"${context.a}","json":{"$[0].v":"${value == 1.25 && workflow.input.original == 9}"}}),
        ),
        (
            "object",
            json!({"value":{"text":"abc","num":3},"json":{"text":{"contains":"${'b'}","equals":"a${'b'}c"},"num":{"equals":"${context.original}"}}}),
        ),
    ] {
        let mut c = claimed(json!({"assert":assertion}), context.clone());
        c.task.task_type = "assert".into();
        assert_eq!(
            executor.execute_task(&c).await.unwrap().status_code,
            "C",
            "{site}"
        );
    }
    for source in ["${output.secret}", "${value}", "${original}"] {
        let c = claimed(json!({"set":source}), context.clone());
        let result = executor.execute_task(&c).await.unwrap();
        assert_eq!(result.status_code, "F");
        assert!(
            !result
                .task_output
                .to_string()
                .contains("W4_CAPTURED_SENTINEL")
        );
        assert!(result.task_output.get("error").is_none());
    }
    let c = claimed(
        json!({"set":{"z":"${missingZ}","a":[{"secret":"${context.secret / 0}"}]}}),
        context.clone(),
    );
    let result = executor.execute_task(&c).await.unwrap();
    assert_eq!(result.task_output["details"]["field"], "/set/a/0/secret");
    assert_eq!(result.task_output["details"]["spanIndex"], 0);
    assert!(result.task_output["details"]["offset"].is_number());
    assert!(
        !result
            .task_output
            .to_string()
            .contains("W4_CAPTURED_SENTINEL")
    );
    assert_eq!(c.context_data, context);
    close(engine).await;
}

#[tokio::test]
async fn w4_switch_order_type_default_and_no_match() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    let (executor, engine) = executor();
    for (cases, status, next) in [
        (
            json!([{"first":{"when":"${true}","then":"one"}},{"later":{"when":"${missing}","then":"two"}}]),
            "C",
            Some("one"),
        ),
        (
            json!([{"first":{"when":"${false}","then":"one"}},{"default":{"then":"fallback"}}]),
            "C",
            Some("fallback"),
        ),
        (
            json!([{"first":{"when":"${false}","then":"one"}}]),
            "F",
            None,
        ),
        (json!([{"first":{"when":"${1}","then":"one"}}]), "F", None),
        (
            json!([{"default":{"then":"one"}},{"last":{"when":"${true}","then":"two"}}]),
            "F",
            None,
        ),
        (json!([{"first":{"when":"true","then":"one"}}]), "F", None),
        (
            json!([{"first":{"when":"x=${true}","then":"one"}}]),
            "F",
            None,
        ),
    ] {
        let mut c = claimed(json!({"switch":cases}), json!({}));
        c.task.task_type = "switch".into();
        let result = executor.execute_task(&c).await.unwrap();
        assert_eq!(result.status_code, status);
        assert_eq!(result.next_task.as_deref(), next);
    }
    close(engine).await;
}

#[tokio::test]
async fn w4_exports_old_values_order_all_or_nothing_and_numeric() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    let (executor, engine) = executor();
    let context = json!({"a":1,"b":2,"double":1.25,"integer":u64::MAX});
    let mut c = claimed(
        json!({"set":{},"export":{"as":{"a":"${context.b}","b":"${context.a}","double":"${context.double}","integer":"${context.integer}","input":"${workflow.input.original}","task":"${output.result}"}}}),
        context.clone(),
    );
    for task_type in ["set", "run"] {
        c.task.task_type = task_type.into();
        let merged = executor.w4_exports(&c, &json!({"result":7})).await.unwrap();
        assert_eq!(
            merged,
            json!({"a":2,"b":1,"double":1.25,"integer":u64::MAX,"input":9,"task":7})
        );
        assert_eq!(c.context_data, context);
    }
    let c = claimed(
        json!({"set":{},"export":{"as":{"z":"${missingZ}","a":[{"bad":"${missingA}"}],"before":1}}}),
        context.clone(),
    );
    let error = executor.w4_exports(&c, &json!({})).await.unwrap_err();
    assert_eq!(error.field, "/export/as/a/0/bad");
    assert_eq!(c.context_data, context);
    for template in [
        json!("literal"),
        json!("${value}"),
        json!("x=${output.result}"),
    ] {
        let c = claimed(
            json!({"set":{},"export":{"as":{"a":template}}}),
            context.clone(),
        );
        assert!(executor.w4_exports(&c, &json!({"result":1})).await.is_err());
        assert_eq!(c.context_data, context);
    }
    let c = claimed(
        json!({"set":{},"export":{"as":{"a":null,"b":[1,2],"new":{"literal":true}}}}),
        context,
    );
    assert_eq!(
        executor.w4_exports(&c, &json!({})).await.unwrap()["new"],
        json!({"literal":true})
    );
    close(engine).await;
}

#[tokio::test]
async fn w4_workflow_output_post_export_object_and_failure_mapping() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    let (executor, engine) = executor();
    let c = claimed(
        json!({"set":{},"export":{"as":{"a":"${output.a}"}}}),
        json!({"a":1}),
    );
    let merged = executor.w4_exports(&c, &json!({"a":2})).await.unwrap();
    let fields = vec![workflow_expression::StepField {
        path: "/output/as".into(),
        position: workflow_expression::Position::WorkflowOutput,
        template: json!({"a":"${context.a}","input":"${workflow.input.original}"}),
        value_from: None,
    }];
    let result = expression_runtime::batch(
        executor.expression_engine.as_ref(),
        &merged,
        &c.input_data,
        None,
        None,
        fields,
        false,
    )
    .await
    .unwrap();
    assert_eq!(result, vec![json!({"a":2,"input":9})]);
    for template in [
        json!("${1}"),
        json!("literal"),
        json!("${output.a}"),
        json!({"bad":"${missing}"}),
    ] {
        let fields = vec![workflow_expression::StepField {
            path: "/output/as".into(),
            position: workflow_expression::Position::WorkflowOutput,
            template,
            value_from: None,
        }];
        let error = expression_runtime::batch(
            executor.expression_engine.as_ref(),
            &merged,
            &c.input_data,
            None,
            None,
            fields,
            false,
        )
        .await
        .unwrap_err();
        let result = error.result(&c);
        assert_eq!(result.task_output["code"], "WORKFLOW_OUTPUT_INVALID");
        assert!(
            result.task_output["details"]["category"]
                .as_str()
                .unwrap()
                .starts_with("EXPRESSION_")
        );
        assert_eq!(c.context_data, json!({"a":1}));
    }
    close(engine).await;
}

#[test]
fn w4_profile_corruption_deferral_and_retained_parent_snapshot() {
    let c = claimed(json!({"set":{}}), json!({"a":1}));
    let raw = serde_json::to_value(&c.raw_definition).unwrap();
    let digest = canonical_sha256(&raw).unwrap();
    assert_eq!(
        disposition("cel-workflow-v2", Some(&raw), false),
        ProfileDisposition::Deferred
    );
    assert_eq!(
        disposition("cel-workflow-v2", None, false),
        ProfileDisposition::Corrupt
    );
    assert_eq!(
        disposition("unknown", Some(&raw), true),
        ProfileDisposition::Corrupt
    );
    assert_eq!(
        disposition("cel-workflow-v1", Some(&raw), true),
        ProfileDisposition::Corrupt
    );
    assert_eq!(
        expression_completion::identity("cel-workflow-v2", Some(&raw), Some("bad"), true),
        ProfileDisposition::Corrupt
    );
    let stored = json!({"format":"light-workflow-step-v1","taskId":c.task.task_id,"profile":c.expression_profile,"definitionDigest":digest,"context":{"old":1}});
    assert_eq!(
        snapshot_context(&stored, c.task.task_id, &c.expression_profile, &digest),
        Some(&json!({"old":1}))
    );
    assert!(snapshot_context(&stored, Uuid::new_v4(), &c.expression_profile, &digest).is_none());
    assert!(
        snapshot_context(
            &json!({"old":9}),
            c.task.task_id,
            &c.expression_profile,
            &digest
        )
        .is_none()
    );
}

#[tokio::test]
async fn w4_legacy_assertion_and_literal_fallback_unchanged() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    let (executor, engine) = executor();
    let mut c = claimed(json!({"set":"${missing}"}), json!({}));
    c.expression_profile = "cel-workflow-v1".into();
    // This is a consistent legacy fixture. Keeping the v2 selector would now
    // exercise the required W6 mismatch failure instead of legacy evaluation.
    c.raw_definition["document"]["metadata"]
        .as_mapping_mut()
        .unwrap()
        .remove(YamlValue::String("lightExpressionProfile".into()));
    c.definition = serde_yaml::from_value(c.raw_definition.clone()).unwrap();
    let result = executor.execute_task(&c).await.unwrap();
    assert_eq!(result.task_output, json!("missing"));
    let assertion: AssertDefinition =
        serde_json::from_value(json!({"value":1,"equals":2})).unwrap();
    let result = executor
        .execute_assert_task(&assertion, &json!({}))
        .unwrap();
    assert_eq!(result.status_code, "F");
    assert_eq!(result.task_output["status"], 400);
    assert!(result.task_output["data"]["failures"].is_array());
    close(engine).await;
}

#[test]
fn w4_cancellation_current_clock_lease_generation_and_replay_fences() {
    let before = Utc::now();
    let expiry = before + chrono::Duration::seconds(1);
    let after = expiry + chrono::Duration::milliseconds(1);
    let lease = HostTaskLease {
        owner: Uuid::new_v4(),
        fencing_token: 4,
    };
    assert!(expression_completion::lease_live(
        Some(lease.owner),
        4,
        Some(expiry),
        lease,
        before
    ));
    assert!(!expression_completion::lease_live(
        Some(lease.owner),
        4,
        Some(expiry),
        lease,
        after
    ));
    assert!(!expression_completion::lease_live(
        Some(lease.owner),
        5,
        Some(expiry),
        lease,
        before
    ));
    assert!(!expression_completion::parent_live(
        Some("CANCELLED"),
        Some(expiry),
        None,
        before
    ));
    assert!(!expression_completion::parent_live(
        None,
        Some(expiry),
        None,
        after
    ));
    assert!(!expression_completion::parent_live(
        None,
        None,
        Some(expiry),
        after
    ));
    assert!(expression_completion::completion_status_live(
        "C",
        Some("W4_APPROVAL_PENDING"),
        "C"
    ));
    assert!(expression_completion::completion_status_live(
        "C",
        Some("W4_FORK_PENDING"),
        "C"
    ));
    assert!(!expression_completion::completion_status_live(
        "C",
        Some("W4_STEP_DONE"),
        "C"
    ));
    assert!(!expression_completion::completion_status_live(
        "F", None, "A"
    ));
}

#[test]
fn w4_stale_completion_is_rollback_not_success() {
    assert!(expression_completion::require_authority(true).is_ok());
    let error = expression_completion::require_authority(false).unwrap_err();
    assert!(expression_completion::rollback_completion(&error));
    assert!(matches!(error,sqlx::Error::Protocol(ref code) if code=="WORKFLOW_STALE_COMPLETION"));
    assert!(!expression_completion::rollback_completion(
        &sqlx::Error::Protocol("OTHER".into())
    ));
}

#[test]
fn w4_failed_competing_branch_then_successful_sibling_and_compensation_route() {
    use expression_completion::{FailureRoute, failure_route, fork_status};
    assert_eq!(failure_route(false, true), FailureRoute::Branch);
    assert_eq!(failure_route(true, true), FailureRoute::Compensation);
    assert_eq!(failure_route(true, false), FailureRoute::Compensation);
    assert_eq!(failure_route(false, false), FailureRoute::Process);
    let mut rows = vec![
        (
            "bad".into(),
            "FAILED".into(),
            Some(json!({"code":"EXPRESSION_INVALID"})),
        ),
        ("good".into(), "RUNNING".into(), None),
    ];
    assert_eq!(fork_status(2, true, &rows), (1, 1, false, false));
    rows[1].1 = "COMPLETED".into();
    rows[1].2 = Some(json!({"result":7}));
    assert_eq!(fork_status(2, true, &rows), (2, 1, true, true));
    assert_eq!(fork_status(2, false, &rows), (2, 1, true, false));
}

#[tokio::test]
async fn w4_competing_branch_expression_failure_and_sibling_success_are_task_local() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    let (executor, engine) = executor();
    let mut bad = claimed(
        json!({"fork":{"compete":true,"branches":[{"bad":{"assert":{"value":"${context.secret}","equals":"different"}}},{"good":{"set":{"result":"${workflow.input.original}"}}}]}}),
        json!({"secret":"W4_CORRECTION_SENTINEL"}),
    );
    bad.task.wf_task_id = "step::bad".into();
    bad.task.task_type = "assert".into();
    let failure = executor.execute_task(&bad).await.unwrap();
    assert_eq!(failure.status_code, "F");
    assert!(
        !failure
            .task_output
            .to_string()
            .contains("W4_CORRECTION_SENTINEL")
    );
    let mut good = bad.clone();
    good.task.wf_task_id = "step::good".into();
    good.task.task_type = "set".into();
    let success = executor.execute_task(&good).await.unwrap();
    assert_eq!(success.status_code, "C");
    assert_eq!(success.task_output, json!({"result":9}));
    assert_eq!(
        expression_completion::fork_status(
            2,
            true,
            &[
                ("bad".into(), "FAILED".into(), Some(failure.task_output)),
                ("good".into(), "COMPLETED".into(), Some(success.task_output))
            ]
        ),
        (2, 1, true, true)
    );
    close(engine).await;
}

#[tokio::test]
#[ignore = "requires explicitly authorized disposable W4_TEST_DATABASE_URL; deterministic authority expiry after actual worker evaluation rolls back timer FIRED"]
async fn w4_postgres_timer_authority_loss_after_evaluation_is_not_consumed() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    let mut gate = PgGate::new(
        json!({"wait":"PT1S","export":{"as":{"old":"${context.old + 1}"}},"then":"end"}),
    )
    .await;
    gate.executor.w4_expire_after_evaluation = true;
    let old = gate.context().await;
    sqlx::query("UPDATE task_info_t SET status_code='W',task_type='wait',execution_placement='host',lease_fencing_token=1").execute(&gate.pool).await.unwrap();
    sqlx::query("INSERT INTO workflow_task_timer_t(host_id,task_id,generation,task_fence,state,wake_at,retry_after_ts,effective_deadline) VALUES($1,$2,1,1,'ARMED',clock_timestamp(),clock_timestamp(),clock_timestamp()+interval '1 hour')").bind(gate.claimed.task.host_id).bind(gate.claimed.task.task_id).execute(&gate.pool).await.unwrap();
    assert_eq!(
        gate.executor
            .fire_durable_timer(
                gate.claimed.task.host_id,
                gate.claimed.task.task_id,
                gate.claimed.task.process_id,
                1
            )
            .await
            .unwrap(),
        0
    );
    let row: (String, Option<Uuid>) =
        sqlx::query_as("SELECT state,successor_task_id FROM workflow_task_timer_t")
            .fetch_one(&gate.pool)
            .await
            .unwrap();
    assert_eq!(row, ("ARMED".into(), None));
    let row: (String, Option<Value>, Option<chrono::DateTime<Utc>>) =
        sqlx::query_as("SELECT status_code,task_output,deadline_ts FROM task_info_t")
            .fetch_one(&gate.pool)
            .await
            .unwrap();
    assert_eq!(row, ("W".into(), None, None));
    assert_eq!(gate.context().await, old);
    gate.close().await;
}

#[tokio::test]
#[ignore = "requires explicitly authorized disposable W4_TEST_DATABASE_URL; deterministic post-evaluation authority expiry rolls back runner acceptance and forbids acknowledgement"]
async fn w4_postgres_runner_authority_loss_no_acceptance_or_acknowledgement() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    let mut gate =
        PgGate::new(json!({"set":{},"export":{"as":{"old":"${context.old + 1}"}},"then":"end"}))
            .await;
    gate.executor.w4_expire_after_evaluation = true;
    let old = gate.context().await;
    let mut attempt = gate.attempt();
    attempt.normalized_result = None;
    sqlx::query("UPDATE task_info_t SET scheduling_request_id=$1")
        .bind(attempt.request_id)
        .execute(&gate.pool)
        .await
        .unwrap();
    let result = execution_runner_protocol::ExecutionResultView {
        host_id: attempt.host_id,
        execution_id: attempt.execution_id,
        request_id: attempt.request_id,
        origin_instance_id: "fixture".into(),
        subject_kind: "workflow-task".into(),
        subject_id: attempt.task_id,
        process_id: Some(attempt.process_id),
        task_id: Some(attempt.task_id),
        agent_session_id: None,
        agent_turn_id: None,
        agent_action_id: None,
        action_kind: "run-shell".into(),
        attempt_number: 1,
        lease_id: attempt.lease_id,
        state: "SUCCEEDED".into(),
        fencing_token: 7,
        normalized_result: None,
        normalized_error: None,
        retry_classification: None,
        terminal: true,
        accepted: false,
    };
    let acknowledgements = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = acknowledgements.clone();
    let router = axum::Router::new()
        .route(
            "/internal/execution/results/page",
            axum::routing::get(move || {
                let result = result.clone();
                async move {
                    axum::Json(execution_runner_protocol::ExecutionResultPage {
                        items: vec![result],
                        next_cursor: None,
                    })
                }
            }),
        )
        .route(
            "/internal/execution/results/{id}/ack",
            axum::routing::post(move || {
                let observed = observed.clone();
                async move {
                    observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    axum::Json(json!({}))
                }
            }),
        );
    let _ = rustls::crypto::ring::default_provider().install_default();
    let certificate = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let certificate_pem = certificate.cert.pem();
    let tls = axum_server::tls_rustls::RustlsConfig::from_pem(
        certificate_pem.as_bytes().to_vec(),
        certificate.signing_key.serialize_pem().into_bytes(),
    ).await.unwrap();
    drop(certificate);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    // Abort on scope exit (including failed assertions); no key material is
    // written to disk and the TLS configuration is owned by this server only.
    let server = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
        axum_server::from_tcp_rustls(listener, tls)
            .serve(router.into_make_service()).await.unwrap()
    }));
    let client = execution_client::ExecutionClient::new_with_bearer_token(
        &format!("https://{addr}/"),
        "w4-synthetic-fixture-token",
        Duration::from_secs(2),
        Some(certificate_pem.as_bytes()),
    )
    .unwrap();
    let pool = gate.pool.clone();
    let host = gate.claimed.task.host_id;
    let process = gate.claimed.task.process_id;
    let task = gate.claimed.task.task_id;
    let executor = Arc::new(gate.executor);
    let reconciler =
        crate::result_reconciler::ResultReconciler::for_test(pool.clone(), executor, client);
    let error = reconciler.run_once().await.unwrap_err();
    assert!(error.to_string().contains("WORKFLOW_STALE_COMPLETION"));
    assert_eq!(
        acknowledgements.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    let row:(Option<i32>,String,Option<chrono::DateTime<Utc>>)=sqlx::query_as("SELECT accepted_attempt,status_code,deadline_ts FROM task_info_t WHERE host_id=$1 AND task_id=$2").bind(host).bind(task).fetch_one(&pool).await.unwrap();
    assert_eq!(row, (None, "A".into(), None));
    let context: Value = sqlx::query_scalar(
        "SELECT context_data FROM process_info_t WHERE host_id=$1 AND process_id=$2",
    )
    .bind(host)
    .bind(process)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(context, old);
    server.abort();
    let _ = server.await;
    drop(reconciler);
    pool.close().await;
    close(gate.engine).await;
    sqlx::query(&format!("DROP SCHEMA {} CASCADE", gate.schema))
        .execute(&gate.admin)
        .await
        .unwrap();
}

// No test below runs without an explicit --ignored invocation and disposable DB authorization.
struct PgGate {
    pool: PgPool,
    admin: PgPool,
    schema: String,
    executor: TaskExecutor,
    engine: Engine,
    claimed: ClaimedTask,
}
impl PgGate {
    async fn branch(&self, join: Uuid, name: &str, kind: &str) -> ClaimedTask {
        let mut c = self.claimed.clone();
        c.task.task_id = Uuid::new_v4();
        c.task.wf_task_id = format!("step::{name}");
        c.task.task_type = kind.into();
        c.task.status_code = "A".into();
        let owner = Uuid::new_v4();
        c.host_lease = Some(HostTaskLease {
            owner,
            fencing_token: 1,
        });
        c.completion_guard = None;
        sqlx::query("INSERT INTO task_info_t(host_id,task_id,process_id,task_type,wf_instance_id,wf_task_id,status_code,lease_owner,lease_fencing_token,lease_expires_ts,fork_join_id,branch_name,execution_placement) VALUES($1,$2,$3,$4,$5,$6,'A',$7,1,clock_timestamp()+interval '1 hour',$8,$9,'host')")
            .bind(c.task.host_id).bind(c.task.task_id).bind(c.task.process_id).bind(kind).bind(&c.task.wf_instance_id).bind(&c.task.wf_task_id).bind(owner).bind(join).bind(name).execute(&self.pool).await.unwrap();
        sqlx::query("INSERT INTO workflow_fork_branch_t(host_id,join_id,task_id,branch_name,state) VALUES($1,$2,$3,$4,'RUNNING')").bind(c.task.host_id).bind(join).bind(c.task.task_id).bind(name).execute(&self.pool).await.unwrap();
        let mut tx = self.pool.begin().await.unwrap();
        assert!(
            expression_completion::capture_step(
                &mut tx,
                c.task.host_id,
                c.task.process_id,
                c.task.task_id
            )
            .await
            .unwrap()
        );
        assert!(self.executor.w4_context(&mut tx, &mut c).await.unwrap());
        tx.commit().await.unwrap();
        c
    }
    async fn new(task: Value) -> Self {
        Self::new_definition(task, None).await
    }
    async fn new_definition(task: Value, output: Option<Value>) -> Self {
        let url = std::env::var("W4_TEST_DATABASE_URL")
            .expect("explicit disposable W4_TEST_DATABASE_URL required");
        let admin = PgPoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await
            .unwrap();
        let schema = format!("w4_component_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&admin)
            .await
            .unwrap();
        let search = schema.clone();
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .after_connect(move |c, _| {
                let search = search.clone();
                Box::pin(async move {
                    sqlx::query(&format!("SET search_path TO {search}"))
                        .execute(c)
                        .await?;
                    Ok(())
                })
            })
            .connect(&url)
            .await
            .unwrap();
        for ddl in [
            "CREATE TABLE process_info_t(host_id uuid,process_id uuid,wf_def_id uuid,status_code text,active bool DEFAULT true,deadline_ts timestamptz,expression_profile text,definition_snapshot jsonb,definition_digest text,context_data jsonb,input_data jsonb,error_info text,completed_ts timestamptz,custom_status_code text,ex_trigger_ts timestamptz)",
            "CREATE TABLE task_info_t(host_id uuid,task_id uuid,process_id uuid,task_type text,wf_instance_id text,wf_task_id text,status_code text,result_code text,active bool DEFAULT true,deadline_ts timestamptz,task_input jsonb,task_output jsonb,lease_owner uuid,lease_fencing_token bigint DEFAULT 0,lease_expires_ts timestamptz,locked text DEFAULT 'N',accepted_attempt int,scheduling_request_id uuid,execution_placement text DEFAULT 'runner',task_policy_digest text,completed_ts timestamptz,update_ts timestamptz,is_compensation bool DEFAULT false,fork_join_id uuid,branch_name text)",
            "CREATE TABLE workflow_invocation_t(host_id uuid,process_id uuid,workflow_instance_id uuid,binding_id uuid,state text,cancel_requested_ts timestamptz,deadline_ts timestamptz,response_policy_snapshot jsonb DEFAULT '{}'::jsonb,public_result jsonb,normalized_error jsonb,state_version bigint DEFAULT 0,terminal_ts timestamptz,user_authorization text,user_authorization_exp bigint,updated_ts timestamptz,effect_state text)",
            "CREATE TABLE workflow_action_authority_t(host_id uuid,run_id uuid,deadline timestamptz,active bool)",
            "CREATE TABLE workflow_invocation_budget_t(host_id uuid,workflow_instance_id uuid,result_byte_limit bigint)",
            "CREATE TABLE workflow_approval_t(host_id uuid,approval_id uuid,task_id uuid,consuming_execution_id uuid,state text,reason text)",
            "CREATE TABLE workflow_execution_policy_t(host_id uuid,policy_digest text,resolved_policy jsonb)",
            "CREATE TABLE workflow_fork_join_t(host_id uuid,join_id uuid,fork_task_id uuid,expected_branches int,compete bool,continuation_task text,state text,completed_branches int,failed_branches int,branch_results jsonb,completed_ts timestamptz)",
            "CREATE TABLE workflow_fork_branch_t(host_id uuid,join_id uuid,task_id uuid,branch_name text,state text,result jsonb,completed_ts timestamptz)",
            "CREATE TABLE workflow_task_timer_t(host_id uuid,task_id uuid,generation bigint,task_fence bigint,state text,effective_deadline timestamptz,successor_task_id uuid,wake_at timestamptz,retry_after_ts timestamptz,updated_ts timestamptz)",
        ] {
            sqlx::query(ddl).execute(&pool).await.unwrap();
        }
        let mut c = claimed(task, json!({"old":1,"double":1.25,"integer":u64::MAX}));
        if let Some(output) = output {
            c.definition.output = Some(serde_json::from_value(output.clone()).unwrap());
            c.raw_definition.as_mapping_mut().unwrap().insert(
                YamlValue::String("output".into()),
                serde_yaml::to_value(output).unwrap(),
            );
        }
        c.task.task_type = "run".into();
        let snapshot: Value = serde_json::to_value(&c.raw_definition).unwrap();
        let digest = canonical_sha256(&snapshot).unwrap();
        sqlx::query("INSERT INTO process_info_t(host_id,process_id,wf_def_id,status_code,expression_profile,definition_snapshot,definition_digest,context_data,input_data) VALUES($1,$2,$3,'A','cel-workflow-v2',$4,$5,$6,$7)")
            .bind(c.task.host_id).bind(c.task.process_id).bind(c.wf_def_id).bind(snapshot).bind(digest).bind(&c.context_data).bind(&c.input_data).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO task_info_t(host_id,task_id,process_id,task_type,wf_instance_id,wf_task_id,status_code,task_input) VALUES($1,$2,$3,'run',$4,'step','A',$5)")
            .bind(c.task.host_id).bind(c.task.task_id).bind(c.task.process_id).bind(&c.task.wf_instance_id).bind(json!({"old":999})).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO workflow_invocation_t(host_id,process_id,workflow_instance_id,binding_id,state,deadline_ts) VALUES($1,$2,$3,$4,'RUNNING',clock_timestamp()+interval '1 hour')")
            .bind(c.task.host_id).bind(c.task.process_id).bind(Uuid::new_v4()).bind(Uuid::new_v4()).execute(&pool).await.unwrap();
        let engine = crate::expression_test_support::engine(WorkerConfig {
            workers: 1,
            cache_entries: 32,
            cache_bytes: 1024 * 1024,
            ..WorkerConfig::default()
        })
        .unwrap();
        let executor = TaskExecutor::new(pool.clone()).with_expression_engine(engine.clone());
        let mut tx = pool.begin().await.unwrap();
        assert!(
            expression_completion::capture_step(
                &mut tx,
                c.task.host_id,
                c.task.process_id,
                c.task.task_id
            )
            .await
            .unwrap()
        );
        assert!(executor.w4_context(&mut tx, &mut c).await.unwrap());
        tx.commit().await.unwrap();
        Self {
            pool,
            admin,
            schema,
            executor,
            engine,
            claimed: c,
        }
    }
    async fn context(&self) -> Value {
        sqlx::query_scalar("SELECT context_data FROM process_info_t")
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }
    fn attempt(&self) -> TerminalAttempt {
        TerminalAttempt {
            host_id: self.claimed.task.host_id,
            process_id: self.claimed.task.process_id,
            task_id: self.claimed.task.task_id,
            request_id: Uuid::new_v4(),
            execution_id: Uuid::new_v4(),
            attempt_number: 1,
            lease_id: Uuid::new_v4(),
            fencing_token: 7,
            state: "SUCCEEDED".into(),
            normalized_result: Some(json!({"structuredOutput":{"amount":11}})),
            normalized_error: None,
        }
    }
    async fn run(&self, attempt: &TerminalAttempt) -> RunnerReconciliation {
        let mut tx = self.pool.begin().await.unwrap();
        let result = self
            .executor
            .reconcile_runner_attempt(&mut tx, attempt)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        result
    }
    async fn close(self) {
        self.pool.close().await;
        close(self.engine).await;
        sqlx::query(&format!("DROP SCHEMA {} CASCADE", self.schema))
            .execute(&self.admin)
            .await
            .unwrap();
    }
}

#[tokio::test]
#[ignore = "requires explicitly authorized disposable W4_TEST_DATABASE_URL; failed competing assertion branch preserves context and bookkeeping while sibling succeeds"]
async fn w4_postgres_failed_competing_branch_then_successful_sibling() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    let gate=PgGate::new(json!({"fork":{"compete":true,"branches":[{"bad":{"assert":{"value":1,"equals":2}}},{"good":{"set":{"result":7}}}]},"export":{"as":{"old":"${context.old}"}},"then":"end"})).await;
    let parent = gate.claimed.task.task_id;
    let join = Uuid::new_v4();
    let old = gate.context().await;
    sqlx::query(
        "UPDATE task_info_t SET status_code='C',result_code='W4_FORK_PENDING',task_type='fork'",
    )
    .execute(&gate.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO workflow_fork_join_t(host_id,join_id,fork_task_id,expected_branches,compete,state) VALUES($1,$2,$3,2,true,'RUNNING')").bind(gate.claimed.task.host_id).bind(join).bind(parent).execute(&gate.pool).await.unwrap();
    let bad = gate.branch(join, "bad", "assert").await;
    let good = gate.branch(join, "good", "set").await;
    let result = gate.executor.execute_task(&bad).await.unwrap();
    assert_eq!(result.status_code, "F");
    let mut tx = gate.pool.begin().await.unwrap();
    gate.executor
        .finish_task(&mut tx, &bad, result)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(gate.context().await, old);
    let status:(String,String,i32,i32)=sqlx::query_as("SELECT p.status_code,j.state,j.completed_branches,j.failed_branches FROM process_info_t p CROSS JOIN workflow_fork_join_t j").fetch_one(&gate.pool).await.unwrap();
    assert_eq!(status, ("A".into(), "RUNNING".into(), 1, 1));
    let result = gate.executor.execute_task(&good).await.unwrap();
    let mut tx = gate.pool.begin().await.unwrap();
    gate.executor
        .finish_task(&mut tx, &good, result)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let status: (String, String) = sqlx::query_as(
        "SELECT p.status_code,i.state FROM process_info_t p CROSS JOIN workflow_invocation_t i",
    )
    .fetch_one(&gate.pool)
    .await
    .unwrap();
    assert_eq!(status, ("C".into(), "COMPLETED".into()));
    let failed:(String,String,Value)=sqlx::query_as("SELECT t.status_code,b.state,t.task_output FROM task_info_t t JOIN workflow_fork_branch_t b USING(host_id,task_id) WHERE b.branch_name='bad'").fetch_one(&gate.pool).await.unwrap();
    assert_eq!(
        (&failed.0, &failed.1),
        (&"F".to_string(), &"FAILED".to_string())
    );
    assert_eq!(failed.2["retryable"], false);
    gate.close().await;
}

#[tokio::test]
#[ignore = "requires explicitly authorized disposable W4_TEST_DATABASE_URL; failed compensation retains sanitized failure bookkeeping without changing context"]
async fn w4_postgres_failed_compensation_bookkeeping() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    let mut gate = PgGate::new(
        json!({"assert":{"value":1,"equals":2},"export":{"as":{"must_not_run":"${missing}"}}}),
    )
    .await;
    let old = gate.context().await;
    let owner = Uuid::new_v4();
    gate.claimed.task.task_type = "assert".into();
    gate.claimed.host_lease = Some(HostTaskLease {
        owner,
        fencing_token: 1,
    });
    sqlx::query("UPDATE task_info_t SET is_compensation=true,task_type='assert',execution_placement='host',lease_owner=$1,lease_fencing_token=1,lease_expires_ts=clock_timestamp()+interval '1 hour'").bind(owner).execute(&gate.pool).await.unwrap();
    sqlx::query("UPDATE workflow_invocation_t SET state='COMPENSATING',cancel_requested_ts=clock_timestamp()").execute(&gate.pool).await.unwrap();
    let result = gate.executor.execute_task(&gate.claimed).await.unwrap();
    assert_eq!(result.status_code, "F");
    let expected = result.task_output.clone();
    let mut tx = gate.pool.begin().await.unwrap();
    gate.executor
        .finish_task(&mut tx, &gate.claimed, result)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(gate.context().await, old);
    let process: (String, String) =
        sqlx::query_as("SELECT status_code,custom_status_code FROM process_info_t")
            .fetch_one(&gate.pool)
            .await
            .unwrap();
    assert_eq!(process, ("F".into(), "WORKFLOW_COMPENSATION_FAILED".into()));
    let invocation: (String, Value) =
        sqlx::query_as("SELECT state,normalized_error FROM workflow_invocation_t")
            .fetch_one(&gate.pool)
            .await
            .unwrap();
    assert_eq!(invocation.0, "FAILED");
    assert_eq!(invocation.1["detail"], expected);
    assert_eq!(invocation.1["retryable"], false);
    let task: (String, String) = sqlx::query_as("SELECT status_code,result_code FROM task_info_t")
        .fetch_one(&gate.pool)
        .await
        .unwrap();
    assert_eq!(task, ("F".into(), "W4_STEP_FAILED".into()));
    gate.close().await;
}

#[tokio::test]
#[ignore = "requires explicitly authorized disposable W4_TEST_DATABASE_URL; final output failure rolls back prepared exports and synchronizes sanitized invocation failure"]
async fn w4_postgres_final_output_failure_preserves_context() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    let gate = PgGate::new_definition(
        json!({"set":{},"export":{"as":{"old":"${output.amount}"}},"then":"end"}),
        Some(json!({"as":"${1}"})),
    )
    .await;
    let old = gate.context().await;
    let attempt = gate.attempt();
    sqlx::query("UPDATE task_info_t SET scheduling_request_id=$1")
        .bind(attempt.request_id)
        .execute(&gate.pool)
        .await
        .unwrap();
    gate.run(&attempt).await;
    assert_eq!(gate.context().await, old);
    let failure: Value = sqlx::query_scalar("SELECT normalized_error FROM workflow_invocation_t")
        .fetch_one(&gate.pool)
        .await
        .unwrap();
    assert_eq!(failure["code"], "WORKFLOW_OUTPUT_INVALID");
    assert_eq!(failure["details"]["category"], "EXPRESSION_RESULT_TYPE");
    assert_eq!(gate.run(&attempt).await, RunnerReconciliation::Replay);
    gate.close().await;
}

#[tokio::test]
#[ignore = "requires explicitly authorized disposable W4_TEST_DATABASE_URL; transactional merge/failure and competing/replayed completion"]
async fn w4_postgres_atomic_merge_failure_and_competing_completion() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    let gate=PgGate::new(json!({"set":{},"export":{"as":{"old":"${context.old + output.amount}","double":"${context.double}","integer":"${context.integer}"}},"then":"end"})).await;
    let attempt = gate.attempt();
    sqlx::query("UPDATE task_info_t SET scheduling_request_id=$1")
        .bind(attempt.request_id)
        .execute(&gate.pool)
        .await
        .unwrap();
    let mut winner = gate.pool.begin().await.unwrap();
    assert!(
        gate.executor
            .w4_runner_gate(&mut winner, &attempt)
            .await
            .unwrap()
            .is_none()
    );
    let mut competitor = gate.pool.begin().await.unwrap();
    assert!(
        gate.executor
            .w4_runner_gate(&mut competitor, &attempt)
            .await
            .is_err()
    );
    competitor.rollback().await.unwrap();
    assert_eq!(
        gate.executor
            .reconcile_runner_attempt(&mut winner, &attempt)
            .await
            .unwrap(),
        RunnerReconciliation::Completed
    );
    winner.commit().await.unwrap();
    assert_eq!(
        gate.context().await,
        json!({"old":12,"double":1.25,"integer":u64::MAX})
    );
    assert_eq!(gate.run(&attempt).await, RunnerReconciliation::Replay);
    let state: String = sqlx::query_scalar("SELECT state FROM workflow_invocation_t")
        .fetch_one(&gate.pool)
        .await
        .unwrap();
    assert_eq!(state, "COMPLETED");
    gate.close().await;
    let gate = PgGate::new(
        json!({"set":{},"export":{"as":{"a":"${output.amount}","z":"${missing}"}},"then":"end"}),
    )
    .await;
    let old = gate.context().await;
    let attempt = gate.attempt();
    sqlx::query("UPDATE task_info_t SET scheduling_request_id=$1")
        .bind(attempt.request_id)
        .execute(&gate.pool)
        .await
        .unwrap();
    gate.run(&attempt).await;
    assert_eq!(gate.context().await, old);
    let failure: Value = sqlx::query_scalar("SELECT normalized_error FROM workflow_invocation_t")
        .fetch_one(&gate.pool)
        .await
        .unwrap();
    assert_eq!(failure["details"]["field"], "/export/as/z");
    gate.close().await;
}

#[tokio::test]
#[ignore = "requires explicitly authorized disposable W4_TEST_DATABASE_URL; approval/replay, original snapshot and final invocation synchronization"]
async fn w4_postgres_approved_runner_snapshot_once_and_final_output() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    let mut gate = PgGate::new(
        json!({"set":{},"export":{"as":{"old":"${context.old + output.amount}"}},"then":"end"}),
    )
    .await;
    let output = json!({"as":{"old":"${context.old}","input":"${workflow.input.original}"}});
    gate.claimed.definition.output = Some(serde_json::from_value(output.clone()).unwrap());
    gate.claimed
        .raw_definition
        .as_mapping_mut()
        .unwrap()
        .insert(
            YamlValue::String("output".into()),
            serde_yaml::to_value(output).unwrap(),
        );
    // Updating the fixture pin invalidates the old step envelope: capture the matching
    // pin only before dispatch, then retain it throughout the approval phase.
    let snapshot: Value = serde_json::to_value(&gate.claimed.raw_definition).unwrap();
    let digest = canonical_sha256(&snapshot).unwrap();
    sqlx::query("UPDATE process_info_t SET definition_snapshot=$1,definition_digest=$2")
        .bind(snapshot)
        .bind(&digest)
        .execute(&gate.pool)
        .await
        .unwrap();
    let envelope = json!({"format":"light-workflow-step-v1","taskId":gate.claimed.task.task_id,"profile":"cel-workflow-v2","definitionDigest":digest,"context":gate.claimed.context_data});
    sqlx::query("UPDATE task_info_t SET task_input=$1,status_code='C',result_code='W4_APPROVAL_PENDING',task_output=$2,accepted_attempt=1").bind(envelope).bind(json!({"amount":11})).execute(&gate.pool).await.unwrap();
    let old = gate.context().await;
    assert_eq!(old["old"], 1);
    sqlx::query("UPDATE process_info_t SET status_code='W',context_data=context_data || '{\"old\":99,\"retained\":2}'::jsonb").execute(&gate.pool).await.unwrap();
    let attempt = gate.attempt();
    let approval = Uuid::new_v4();
    sqlx::query("INSERT INTO workflow_approval_t(host_id,approval_id,task_id,consuming_execution_id,state) VALUES($1,$2,$3,$4,'CONSUMED')").bind(attempt.host_id).bind(approval).bind(attempt.task_id).bind(attempt.execution_id).execute(&gate.pool).await.unwrap();
    assert_eq!(gate.run(&attempt).await, RunnerReconciliation::Completed);
    assert_eq!(gate.context().await["old"], 12);
    assert_eq!(gate.context().await["retained"], 2);
    assert_eq!(gate.run(&attempt).await, RunnerReconciliation::Replay);
    let public: Value = sqlx::query_scalar("SELECT public_result FROM workflow_invocation_t")
        .fetch_one(&gate.pool)
        .await
        .unwrap();
    assert_eq!(public, json!({"old":12,"input":9}));
    gate.close().await;
}

#[tokio::test]
#[ignore = "requires explicitly authorized disposable W4_TEST_DATABASE_URL; cancellation/current-clock lease loss, timer fence, unsupported deferral and corruption"]
async fn w4_postgres_cancellation_lease_loss_deferral_and_corruption() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    let mut gate=PgGate::new(json!({"set":{"a":"${context.old}"},"export":{"as":{"old":"${output.a + 1}"}},"then":"end"})).await;
    let old = gate.context().await;
    let owner = Uuid::new_v4();
    gate.claimed.host_lease = Some(HostTaskLease {
        owner,
        fencing_token: 1,
    });
    sqlx::query("UPDATE task_info_t SET lease_owner=$1,lease_fencing_token=1,lease_expires_ts=clock_timestamp()+interval '1 hour',execution_placement='host'").bind(owner).execute(&gate.pool).await.unwrap();
    let result = gate.executor.execute_task(&gate.claimed).await.unwrap();
    sqlx::query(
        "UPDATE workflow_invocation_t SET cancel_requested_ts=clock_timestamp(),state='CANCELLED'",
    )
    .execute(&gate.pool)
    .await
    .unwrap();
    let persisted_sql = "SELECT jsonb_build_object(
        'processes',(SELECT jsonb_agg(to_jsonb(p)) FROM process_info_t p),
        'tasks',(SELECT jsonb_agg(to_jsonb(t)) FROM task_info_t t),
        'invocations',(SELECT jsonb_agg(to_jsonb(i)) FROM workflow_invocation_t i))";
    let cancelled_state: Value = sqlx::query_scalar(persisted_sql)
        .fetch_one(&gate.pool).await.unwrap();
    let mut tx = gate.pool.begin().await.unwrap();
    // Prove an earlier write in the accepting transaction is also rolled back.
    sqlx::query("UPDATE task_info_t SET update_ts=clock_timestamp()")
        .execute(&mut *tx).await.unwrap();
    let error = gate.executor
        .finish_task(&mut tx, &gate.claimed, result)
        .await
        .unwrap_err();
    assert!(matches!(error, sqlx::Error::Protocol(ref code) if code == "WORKFLOW_STALE_COMPLETION"));
    tx.rollback().await.unwrap();
    let after_cancel: Value = sqlx::query_scalar(persisted_sql)
        .fetch_one(&gate.pool).await.unwrap();
    assert_eq!(after_cancel, cancelled_state);
    assert_eq!(gate.context().await, old);
    sqlx::query("UPDATE workflow_invocation_t SET cancel_requested_ts=NULL,state='RUNNING'")
        .execute(&gate.pool)
        .await
        .unwrap();
    let live_state: Value = sqlx::query_scalar(persisted_sql)
        .fetch_one(&gate.pool).await.unwrap();
    let mut tx = gate.pool.begin().await.unwrap();
    sqlx::query("UPDATE task_info_t SET lease_expires_ts=clock_timestamp()")
        .execute(&mut *tx)
        .await
        .unwrap();
    let result = gate.executor.execute_task(&gate.claimed).await.unwrap();
    let error = gate.executor
        .finish_task(&mut tx, &gate.claimed, result)
        .await
        .unwrap_err();
    assert!(matches!(error, sqlx::Error::Protocol(ref code) if code == "WORKFLOW_STALE_COMPLETION"));
    tx.rollback().await.unwrap();
    let after_expiry: Value = sqlx::query_scalar(persisted_sql)
        .fetch_one(&gate.pool).await.unwrap();
    assert_eq!(after_expiry, live_state);
    assert_eq!(gate.context().await, old);
    gate.claimed.host_lease = None;
    gate.claimed.completion_guard = Some(expression_completion::CompletionGuard::Timer {
        generation: 1,
        fence: 1,
    });
    sqlx::query("INSERT INTO workflow_task_timer_t(host_id,task_id,generation,task_fence,state) VALUES($1,$2,2,1,'FIRED')").bind(gate.claimed.task.host_id).bind(gate.claimed.task.task_id).execute(&gate.pool).await.unwrap();
    let mut tx = gate.pool.begin().await.unwrap();
    assert!(
        !gate
            .executor
            .w4_authority(&mut tx, &gate.claimed)
            .await
            .unwrap()
    );
    tx.rollback().await.unwrap();
    gate.claimed.completion_guard = Some(expression_completion::CompletionGuard::Timer {
        generation: 2,
        fence: 1,
    });
    let mut tx = gate.pool.begin().await.unwrap();
    assert!(
        gate.executor
            .w4_authority(&mut tx, &gate.claimed)
            .await
            .unwrap()
    );
    tx.rollback().await.unwrap();
    sqlx::query("UPDATE workflow_task_timer_t SET effective_deadline=clock_timestamp()")
        .execute(&gate.pool)
        .await
        .unwrap();
    let mut tx = gate.pool.begin().await.unwrap();
    assert!(
        !gate
            .executor
            .w4_authority(&mut tx, &gate.claimed)
            .await
            .unwrap()
    );
    tx.rollback().await.unwrap();
    let attempt = gate.attempt();
    let unavailable = TaskExecutor::new(gate.pool.clone());
    let mut tx = gate.pool.begin().await.unwrap();
    assert_eq!(
        unavailable
            .reconcile_runner_attempt(&mut tx, &attempt)
            .await
            .unwrap(),
        RunnerReconciliation::Deferred
    );
    tx.rollback().await.unwrap();
    let accepted: Option<i32> = sqlx::query_scalar("SELECT accepted_attempt FROM task_info_t")
        .fetch_one(&gate.pool)
        .await
        .unwrap();
    assert_eq!(accepted, None);
    sqlx::query("UPDATE process_info_t SET definition_snapshot=jsonb_set(definition_snapshot,'{document,metadata,lightExpressionProfile}','\"invalid\"')").execute(&gate.pool).await.unwrap();
    assert_eq!(gate.run(&attempt).await, RunnerReconciliation::Failed);
    assert_eq!(gate.context().await, old);
    let failure: Value = sqlx::query_scalar("SELECT normalized_error FROM workflow_invocation_t")
        .fetch_one(&gate.pool)
        .await
        .unwrap();
    assert_eq!(failure["code"], "EVALUATOR_PROFILE_UNSUPPORTED");
    gate.close().await;
}

#[tokio::test]
#[ignore = "requires explicitly authorized disposable W4_TEST_DATABASE_URL; parent-fork retained snapshot and exactly-once join"]
async fn w4_postgres_parent_fork_exports_use_parent_step_snapshot() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    let gate=PgGate::new(json!({"fork":{"branches":[{"branch":{"set":{"result":1}}}],"compete":false},"export":{"as":{"old":"${context.old}"}},"then":"end"})).await;
    let parent = gate.claimed.task.task_id;
    let branch = Uuid::new_v4();
    let join = Uuid::new_v4();
    sqlx::query(
        "UPDATE task_info_t SET status_code='C',result_code='W4_FORK_PENDING',task_type='fork'",
    )
    .execute(&gate.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO task_info_t(host_id,task_id,process_id,task_type,wf_instance_id,wf_task_id,status_code,task_output) VALUES($1,$2,$3,'set',$4,'step::branch','C','{}')").bind(gate.claimed.task.host_id).bind(branch).bind(gate.claimed.task.process_id).bind(&gate.claimed.task.wf_instance_id).execute(&gate.pool).await.unwrap();
    sqlx::query("INSERT INTO workflow_fork_join_t(host_id,join_id,fork_task_id,expected_branches,compete,state) VALUES($1,$2,$3,1,false,'RUNNING')").bind(gate.claimed.task.host_id).bind(join).bind(parent).execute(&gate.pool).await.unwrap();
    sqlx::query("INSERT INTO workflow_fork_branch_t(host_id,join_id,task_id,branch_name,state) VALUES($1,$2,$3,'branch','RUNNING')").bind(gate.claimed.task.host_id).bind(join).bind(branch).execute(&gate.pool).await.unwrap();
    sqlx::query("UPDATE process_info_t SET context_data=context_data || '{\"old\":99,\"retained\":2}'::jsonb").execute(&gate.pool).await.unwrap();
    let mut child = gate.claimed.clone();
    child.task.task_id = branch;
    child.task.wf_task_id = "step::branch".into();
    child.task.status_code = "C".into();
    let mut tx = gate.pool.begin().await.unwrap();
    gate.executor
        .reconcile_fork_branch(&mut tx, &child, true, json!({"result":11}))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(gate.context().await["old"], 1);
    assert_eq!(gate.context().await["retained"], 2);
    let mut tx = gate.pool.begin().await.unwrap();
    gate.executor
        .reconcile_fork_branch(&mut tx, &child, true, json!({"result":22}))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(gate.context().await["old"], 1);
    gate.close().await;
}

#[tokio::test]
async fn w4_runner_approval_preserves_context_until_once_only_success() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    let (executor, engine) = executor();
    let mut c = claimed(
        json!({"set":{},"export":{"as":{"old":"${context.old + output.amount}"}}}),
        json!({"old":1}),
    );
    c.task.task_type = "run".into();
    let current = json!({"old":99,"retained":2});
    assert_eq!(
        executor.runner_approval_context(&c, current.clone(), &json!({"amount":11})),
        current
    );
    c.task.status_code = "C".into();
    c.task.result_code = Some("W4_APPROVAL_PENDING".into());
    assert!(expression_completion::completion_status_live(
        "C",
        c.task.result_code.as_deref(),
        "C"
    ));
    assert_eq!(
        executor
            .w4_exports(&c, &json!({"amount":11}))
            .await
            .unwrap()["old"],
        12
    );
    c.task.result_code = Some("W4_STEP_DONE".into());
    assert!(!expression_completion::completion_status_live(
        "C",
        c.task.result_code.as_deref(),
        "C"
    ));
    close(engine).await;
}

include!("expression_requests_tests.rs");
