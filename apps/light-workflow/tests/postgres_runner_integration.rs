use std::time::Duration;

use light_workflow::executor::TaskExecutor;
use light_workflow::lease_reaper::LeaseReaper;
use light_workflow::repositories::NewProcess;
use light_workflow::repositories::ReservedRequest;
use light_workflow::repositories::WorkflowRepository;
use serde_json::json;
use sqlx::postgres::{PgListener, PgPoolOptions};
use uuid::Uuid;

struct Fixture {
    host_id: Uuid,
    wf_def_id: Uuid,
    policy_snapshot_id: Uuid,
    process_id: Uuid,
    task_id: Uuid,
    request_id: Uuid,
    execution_id: Uuid,
    lease_id: Uuid,
}

async fn pool() -> Option<sqlx::PgPool> {
    let url = std::env::var("DATABASE_URL").ok()?;
    Some(
        PgPoolOptions::new()
            .max_connections(8)
            .connect(&url)
            .await
            .expect("connect disposable PostgreSQL"),
    )
}

async fn insert_terminal_fixture(pool: &sqlx::PgPool, service_id: &str) -> Fixture {
    let host_id = Uuid::new_v4();
    let owner = Uuid::new_v4();
    let domain = format!("t{}.example", &host_id.simple().to_string()[..12]);
    let wf_def_id = Uuid::new_v4();
    let policy_snapshot_id = Uuid::new_v4();
    let process_id = Uuid::new_v4();
    let task_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    let request_id = Uuid::new_v4();
    let execution_id = Uuid::new_v4();
    let lease_id = Uuid::new_v4();
    let policy_digest = format!("{:064x}", 1);
    let definition_digest = format!("{:064x}", 2);
    let definition_yaml = include_str!("../examples/run-shell-mock-v1.yaml");
    let definition_snapshot: serde_json::Value = serde_yaml::from_str(definition_yaml).unwrap();

    let mut tx = pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO org_t(domain, org_name, org_desc, org_owner) VALUES ($1, 'test', 'test', $2)",
    )
    .bind(&domain)
    .bind(owner)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO host_t(host_id, domain, sub_domain, host_owner) VALUES ($1, $2, 'test', $3)",
    )
    .bind(host_id)
    .bind(&domain)
    .bind(owner)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query("INSERT INTO wf_definition_t(host_id, wf_def_id, namespace, name, version, definition) VALUES ($1, $2, 'test', $3, '1', $4)")
        .bind(host_id).bind(wf_def_id).bind(format!("wf-{task_id}")).bind(definition_yaml).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO workflow_execution_policy_t(policy_snapshot_id, host_id, definition_digest, profile_id, profile_version, resolved_policy, policy_digest, source, created_by) VALUES ($1, $2, $3, 'mock', 1, $4, $5, 'test', 'test')")
        .bind(policy_snapshot_id).bind(host_id).bind(&definition_digest).bind(json!({})).bind(&policy_digest).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO process_info_t(host_id, process_id, wf_def_id, wf_instance_id, app_id, process_type, status_code, ex_trigger_ts, definition_snapshot, definition_digest, policy_snapshot_id, policy_digest, source_event_id, execution_profile_id) VALUES ($1, $2, $3, $4, 'test', 'Workflow', 'A', CURRENT_TIMESTAMP, $5, $6, $7, $8, $9, 'mock')")
        .bind(host_id).bind(process_id).bind(wf_def_id).bind(format!("instance-{process_id}")).bind(&definition_snapshot).bind(&definition_digest).bind(policy_snapshot_id).bind(&policy_digest).bind(format!("event-{process_id}")).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO task_info_t(host_id, task_id, task_type, process_id, wf_instance_id, wf_task_id, status_code, locked, priority, task_input, execution_placement, task_policy_digest) VALUES ($1, $2, 'run', $3, $4, 'printMessage', 'A', 'N', 1, $5, 'runner', $6)")
        .bind(host_id).bind(task_id).bind(process_id).bind(format!("instance-{process_id}")).bind(json!({})).bind(&policy_digest).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO runner_session_t(host_id, session_id, runner_id, authenticated_subject, enrollment_id, runner_version, protocol_version, connection_generation, status, binary_digest, effective_config_digest, command_allowlist_digest, capability_document, compatibility_digest, maximum_concurrency, reported_available_capacity, watchdog_healthy, journal_healthy) VALUES ($1, $2, 'runner', 'subject', 'enrollment', '1', '1.0', 1, 'CONNECTED', 'binary', 'config', 'commands', $3, 'compat', 1, 1, TRUE, TRUE)")
        .bind(host_id).bind(session_id).bind(json!({})).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO runner_backend_t(host_id, session_id, backend_id, backend_version, boundary_class, host_exposure_class, supported_actions, supported_features, compatibility_digest, health, available_slots) VALUES ($1, $2, 'mock', '1', 'container', 'none', $3, $4, 'compat', 'HEALTHY', 1)")
        .bind(host_id).bind(session_id).bind(json!(["run.shell"])).bind(json!([])).execute(&mut *tx).await.unwrap();
    sqlx::query("INSERT INTO runner_scheduling_request_t(host_id, request_id, idempotency_key, origin_kind, origin_service_id, origin_instance_id, subject_kind, subject_id, process_id, task_id, policy_snapshot_id, policy_digest, normalized_requirements, execution_spec, fairness_key, state, selected_runner_session_id, selected_backend_id) VALUES ($1, $2, $3, 'workflow', $4, 'old-instance', 'workflow-task', $5, $6, $5, $7, $8, $9, $10, 'test', 'LEASED', $11, 'mock')")
        .bind(host_id).bind(request_id).bind(format!("workflow-task:{task_id}")).bind(service_id).bind(task_id).bind(process_id).bind(policy_snapshot_id).bind(&policy_digest).bind(json!({})).bind(json!({})).bind(session_id).execute(&mut *tx).await.unwrap();
    sqlx::query(
        "UPDATE task_info_t SET scheduling_request_id = $1 WHERE host_id = $2 AND task_id = $3",
    )
    .bind(request_id)
    .bind(host_id)
    .bind(task_id)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query("INSERT INTO execution_attempt_t(host_id, execution_id, request_id, origin_kind, origin_service_id, origin_instance_id, subject_kind, subject_id, attempt_number, process_id, task_id, lease_id, fencing_token, runner_session_id, connection_generation, backend_id, state, lease_issued_ts, lease_started_ts, lease_deadline_ts, cleanup_state) VALUES ($1, $2, $3, 'workflow', $4, 'old-instance', 'workflow-task', $5, 1, $6, $5, $7, 1, $8, 1, 'mock', 'STARTED', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP + INTERVAL '5 minutes', 'REQUIRED')")
        .bind(host_id).bind(execution_id).bind(request_id).bind(service_id).bind(task_id).bind(process_id).bind(lease_id).bind(session_id).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();

    Fixture {
        host_id,
        wf_def_id,
        policy_snapshot_id,
        process_id,
        task_id,
        request_id,
        execution_id,
        lease_id,
    }
}

#[tokio::test]
async fn duplicate_source_event_creates_exactly_one_process() {
    let Some(pool) = pool().await else {
        return;
    };
    let fixture = insert_terminal_fixture(&pool, "duplicate-event-test").await;
    let source_event_id = format!("duplicate-{}", Uuid::new_v4());
    let policy_digest = format!("{:064x}", 1);
    let definition_digest = format!("{:064x}", 2);
    let input = json!({"request": "same"});
    let snapshot = json!({"marker": "original"});

    async fn insert(
        pool: &sqlx::PgPool,
        fixture: &Fixture,
        source_event_id: &str,
        input: &serde_json::Value,
        snapshot: &serde_json::Value,
        definition_digest: &str,
        policy_digest: &str,
    ) -> bool {
        let mut tx = pool.begin().await.unwrap();
        let process = NewProcess {
            host_id: fixture.host_id,
            process_id: Uuid::new_v4(),
            wf_def_id: fixture.wf_def_id,
            wf_instance_id: Uuid::new_v4().to_string(),
            app_id: "test",
            input_data: input,
            definition_snapshot: snapshot,
            definition_digest,
            policy_snapshot_id: fixture.policy_snapshot_id,
            policy_digest,
            source_event_id,
            execution_profile_id: "mock",
        };
        let inserted = WorkflowRepository::insert_process_if_absent(&mut tx, &process)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        inserted
    }

    let (first, second) = tokio::join!(
        insert(
            &pool,
            &fixture,
            &source_event_id,
            &input,
            &snapshot,
            &definition_digest,
            &policy_digest
        ),
        insert(
            &pool,
            &fixture,
            &source_event_id,
            &input,
            &snapshot,
            &definition_digest,
            &policy_digest
        )
    );
    assert_ne!(
        first, second,
        "the source-event uniqueness fence must admit one writer"
    );
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM process_info_t WHERE host_id = $1 AND wf_def_id = $2 AND source_event_id = $3",
    )
    .bind(fixture.host_id)
    .bind(fixture.wf_def_id)
    .bind(&source_event_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn process_definition_snapshot_is_immutable_when_definition_is_edited() {
    let Some(pool) = pool().await else {
        return;
    };
    let fixture = insert_terminal_fixture(&pool, "snapshot-test").await;

    sqlx::query("UPDATE wf_definition_t SET definition = $1 WHERE host_id = $2 AND wf_def_id = $3")
        .bind(r#"{"marker":"edited"}"#)
        .bind(fixture.host_id)
        .bind(fixture.wf_def_id)
        .execute(&pool)
        .await
        .unwrap();

    let (snapshot, mutable): (serde_json::Value, String) = sqlx::query_as(
        "SELECT p.definition_snapshot, d.definition FROM process_info_t p JOIN wf_definition_t d ON d.host_id = p.host_id AND d.wf_def_id = p.wf_def_id WHERE p.host_id = $1 AND p.process_id = $2",
    )
    .bind(fixture.host_id)
    .bind(fixture.process_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(snapshot["document"]["name"], "run-shell-mock-v1");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&mutable).unwrap()["marker"],
        "edited"
    );
}

#[tokio::test]
async fn terminal_result_survives_origin_restart_and_is_accepted_once() {
    let Some(pool) = pool().await else {
        return;
    };
    let service_id = format!("workflow-{}", Uuid::new_v4());
    let fixture = insert_terminal_fixture(&pool, &service_id).await;
    sqlx::query("UPDATE execution_attempt_t SET state = 'SUCCEEDED', terminal_ts = CURRENT_TIMESTAMP, normalized_result = $1 WHERE host_id = $2 AND execution_id = $3")
        .bind(json!({"output":{"value":42}})).bind(fixture.host_id).bind(fixture.execution_id).execute(&pool).await.unwrap();

    let repository = WorkflowRepository::new(pool.clone());
    let attempts = repository
        .pending_terminal_attempts(&service_id, 10)
        .await
        .unwrap();
    assert_eq!(
        attempts.len(),
        1,
        "new service instance must see old-instance result"
    );
    let attempt = attempts[0].clone();
    let (first, second) = tokio::join!(
        async {
            let mut tx = pool.begin().await.unwrap();
            let accepted =
                WorkflowRepository::conditionally_accept_terminal_attempt(&mut tx, &attempt)
                    .await
                    .unwrap();
            tx.commit().await.unwrap();
            accepted
        },
        async {
            let mut tx = pool.begin().await.unwrap();
            let accepted =
                WorkflowRepository::conditionally_accept_terminal_attempt(&mut tx, &attempt)
                    .await
                    .unwrap();
            tx.commit().await.unwrap();
            accepted
        }
    );
    assert_ne!(first, second);
    assert!(first || second);
    let accepted_attempt: Option<i32> = sqlx::query_scalar(
        "SELECT accepted_attempt FROM task_info_t WHERE host_id = $1 AND task_id = $2",
    )
    .bind(fixture.host_id)
    .bind(fixture.task_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(accepted_attempt, Some(1));
    let state: String = sqlx::query_scalar(
        "SELECT state FROM runner_scheduling_request_t WHERE host_id = $1 AND request_id = $2",
    )
    .bind(fixture.host_id)
    .bind(fixture.request_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(state, "SATISFIED");
}

#[tokio::test]
async fn successful_runner_result_uses_existing_workflow_transition_transaction() {
    let Some(pool) = pool().await else {
        return;
    };
    let service_id = format!("workflow-{}", Uuid::new_v4());
    let fixture = insert_terminal_fixture(&pool, &service_id).await;
    sqlx::query("UPDATE execution_attempt_t SET state = 'SUCCEEDED', terminal_ts = CURRENT_TIMESTAMP, normalized_result = $1 WHERE host_id = $2 AND execution_id = $3")
        .bind(json!({"structuredOutput":{"message":"completed in sandbox"}}))
        .bind(fixture.host_id)
        .bind(fixture.execution_id)
        .execute(&pool)
        .await
        .unwrap();
    let mut attempts = WorkflowRepository::new(pool.clone())
        .pending_terminal_attempts(&service_id, 1)
        .await
        .unwrap();
    let attempt = attempts.remove(0);
    let executor = TaskExecutor::new(pool.clone());
    let mut tx = pool.begin().await.unwrap();
    assert!(
        executor
            .reconcile_runner_attempt(&mut tx, &attempt)
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();

    let task: (String, serde_json::Value) = sqlx::query_as(
        "SELECT status_code::text, task_output FROM task_info_t WHERE host_id = $1 AND task_id = $2",
    )
    .bind(fixture.host_id)
    .bind(fixture.task_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(task.0, "C");
    assert_eq!(task.1["message"], "completed in sandbox");
    let process_status: String = sqlx::query_scalar(
        "SELECT status_code::text FROM process_info_t WHERE host_id = $1 AND process_id = $2",
    )
    .bind(fixture.host_id)
    .bind(fixture.process_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(process_status, "C");
}

#[tokio::test]
async fn stale_terminal_result_cannot_cross_a_newer_fencing_token() {
    let Some(pool) = pool().await else {
        return;
    };
    let service_id = format!("workflow-{}", Uuid::new_v4());
    let fixture = insert_terminal_fixture(&pool, &service_id).await;
    sqlx::query("UPDATE execution_attempt_t SET state = 'SUCCEEDED', terminal_ts = CURRENT_TIMESTAMP, normalized_result = $1 WHERE host_id = $2 AND execution_id = $3")
        .bind(json!({"output":{"stale":true}})).bind(fixture.host_id).bind(fixture.execution_id).execute(&pool).await.unwrap();
    let repository = WorkflowRepository::new(pool.clone());
    let stale = repository
        .pending_terminal_attempts(&service_id, 10)
        .await
        .unwrap()
        .into_iter()
        .next()
        .unwrap();

    // Simulate a newer durable fence winning before the origin accepts this result.
    sqlx::query("UPDATE execution_attempt_t SET fencing_token = fencing_token + 1 WHERE host_id = $1 AND execution_id = $2")
        .bind(fixture.host_id)
        .bind(fixture.execution_id)
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    let error = WorkflowRepository::conditionally_accept_terminal_attempt(&mut tx, &stale)
        .await
        .expect_err("stale fencing token must lose acceptance race");
    assert!(error.to_string().contains("fencing acceptance race"));
    tx.rollback().await.unwrap();

    let accepted_attempt: Option<i32> = sqlx::query_scalar(
        "SELECT accepted_attempt FROM task_info_t WHERE host_id = $1 AND task_id = $2",
    )
    .bind(fixture.host_id)
    .bind(fixture.task_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(accepted_attempt, None);
}

#[tokio::test]
async fn terminal_commit_emits_identifiers_only_notification() {
    let Some(pool) = pool().await else {
        return;
    };
    let service_id = format!("workflow-{}", Uuid::new_v4());
    let fixture = insert_terminal_fixture(&pool, &service_id).await;
    let mut listener = PgListener::connect_with(&pool).await.unwrap();
    listener.listen("execution_result_ready_v1").await.unwrap();

    sqlx::query("UPDATE execution_attempt_t SET state = 'FAILED', terminal_ts = CURRENT_TIMESTAMP, normalized_error = $1 WHERE host_id = $2 AND execution_id = $3 AND lease_id = $4 AND fencing_token = 1")
        .bind(json!({"secret":"must-not-be-notified"})).bind(fixture.host_id).bind(fixture.execution_id).bind(fixture.lease_id).execute(&pool).await.unwrap();
    let notification = tokio::time::timeout(Duration::from_secs(10), listener.recv())
        .await
        .unwrap()
        .unwrap();
    let payload: serde_json::Value = serde_json::from_str(notification.payload()).unwrap();
    assert_eq!(payload["executionId"], fixture.execution_id.to_string());
    assert!(!notification.payload().contains("must-not-be-notified"));
    assert!(notification.payload().len() < 1024);
}

#[tokio::test]
async fn concurrent_scheduler_claims_create_one_fenced_attempt() {
    let Some(pool) = pool().await else {
        return;
    };
    let service_id = format!("workflow-{}", Uuid::new_v4());
    let fixture = insert_terminal_fixture(&pool, &service_id).await;
    sqlx::query("DELETE FROM execution_attempt_t WHERE host_id = $1 AND execution_id = $2")
        .bind(fixture.host_id)
        .bind(fixture.execution_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE runner_scheduling_request_t SET state = 'RESERVED', reservation_token_hash = 'token', reservation_expires_ts = CURRENT_TIMESTAMP + INTERVAL '1 minute' WHERE host_id = $1 AND request_id = $2")
        .bind(fixture.host_id).bind(fixture.request_id).execute(&pool).await.unwrap();

    async fn claim_and_create(pool: &sqlx::PgPool, service_id: &str) -> bool {
        let mut tx = pool.begin().await.unwrap();
        let request: Option<ReservedRequest> =
            WorkflowRepository::claim_reserved_request(&mut tx, service_id)
                .await
                .unwrap();
        let Some(request) = request else {
            tx.commit().await.unwrap();
            return false;
        };
        let created = WorkflowRepository::create_attempt_from_reservation(
            &mut tx,
            &request,
            "token",
            chrono::Utc::now() + chrono::Duration::minutes(5),
        )
        .await
        .unwrap()
        .is_some();
        tx.commit().await.unwrap();
        created
    }

    let (first, second) = tokio::join!(
        claim_and_create(&pool, &service_id),
        claim_and_create(&pool, &service_id)
    );
    assert_ne!(first, second);
    let attempts: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM execution_attempt_t WHERE host_id = $1 AND request_id = $2",
    )
    .bind(fixture.host_id)
    .bind(fixture.request_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(attempts, 1);
    let fence: i64 = sqlx::query_scalar(
        "SELECT fencing_token FROM execution_attempt_t WHERE host_id = $1 AND request_id = $2",
    )
    .bind(fixture.host_id)
    .bind(fixture.request_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(fence, 1);
}

#[tokio::test]
async fn expired_unstarted_lease_is_safely_retried_not_reconciled() {
    let Some(pool) = pool().await else {
        return;
    };
    let service_id = format!("workflow-{}", Uuid::new_v4());
    let fixture = insert_terminal_fixture(&pool, &service_id).await;
    sqlx::query("UPDATE execution_attempt_t SET state = 'CREATED', lease_started_ts = NULL, lease_deadline_ts = CURRENT_TIMESTAMP - INTERVAL '1 second' WHERE host_id = $1 AND execution_id = $2")
        .bind(fixture.host_id).bind(fixture.execution_id).execute(&pool).await.unwrap();
    sqlx::query("UPDATE runner_scheduling_request_t SET state = 'ATTEMPT_CREATED' WHERE host_id = $1 AND request_id = $2")
        .bind(fixture.host_id).bind(fixture.request_id).execute(&pool).await.unwrap();

    assert!(LeaseReaper::new(pool.clone()).run_once().await.unwrap());
    let row: (String, Option<chrono::DateTime<chrono::Utc>>, String) = sqlx::query_as(
        "SELECT state, accepted_by_origin_ts, retry_classification FROM execution_attempt_t WHERE host_id = $1 AND execution_id = $2",
    )
    .bind(fixture.host_id)
    .bind(fixture.execution_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.0, "CANCELLED");
    assert!(row.1.is_some());
    assert_eq!(row.2, "safe");
    let request_state: String = sqlx::query_scalar(
        "SELECT state FROM runner_scheduling_request_t WHERE host_id = $1 AND request_id = $2",
    )
    .bind(fixture.host_id)
    .bind(fixture.request_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(request_state, "PENDING_CAPACITY");
    assert!(
        WorkflowRepository::new(pool.clone())
            .pending_terminal_attempts(&service_id, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn expired_started_lease_becomes_unknown_and_blocks_automatic_retry() {
    let Some(pool) = pool().await else {
        return;
    };
    let service_id = format!("workflow-{}", Uuid::new_v4());
    let fixture = insert_terminal_fixture(&pool, &service_id).await;
    sqlx::query("UPDATE execution_attempt_t SET lease_deadline_ts = CURRENT_TIMESTAMP - INTERVAL '1 second' WHERE host_id = $1 AND execution_id = $2")
        .bind(fixture.host_id)
        .bind(fixture.execution_id)
        .execute(&pool)
        .await
        .unwrap();

    assert!(LeaseReaper::new(pool.clone()).run_once().await.unwrap());
    let row: (String, String, i64) = sqlx::query_as(
        "SELECT state, retry_classification, fencing_token FROM execution_attempt_t WHERE host_id = $1 AND execution_id = $2",
    )
    .bind(fixture.host_id)
    .bind(fixture.execution_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.0, "UNKNOWN");
    assert_eq!(row.1, "inspect-required");
    assert_eq!(row.2, 1, "UNKNOWN must not mint a retry fence");
    let request_state: String = sqlx::query_scalar(
        "SELECT state FROM runner_scheduling_request_t WHERE host_id = $1 AND request_id = $2",
    )
    .bind(fixture.host_id)
    .bind(fixture.request_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(request_state, "LEASED");
    assert!(
        WorkflowRepository::new(pool.clone())
            .pending_terminal_attempts(&service_id, 10)
            .await
            .unwrap()
            .is_empty()
    );
    let audit_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM execution_runtime_audit_t WHERE host_id = $1 AND execution_id = $2 AND event_type = 'LEASE_EXPIRED_UNKNOWN'",
    )
    .bind(fixture.host_id)
    .bind(fixture.execution_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audit_count, 1);
}

#[tokio::test]
async fn failed_runner_result_fails_task_and_process_transactionally() {
    let Some(pool) = pool().await else {
        return;
    };
    let service_id = format!("workflow-{}", Uuid::new_v4());
    let fixture = insert_terminal_fixture(&pool, &service_id).await;
    sqlx::query("UPDATE execution_attempt_t SET state = 'FAILED', terminal_ts = CURRENT_TIMESTAMP, normalized_error = $1 WHERE host_id = $2 AND execution_id = $3")
        .bind(json!({"failureClass":"non_zero_exit","exitCode":17}))
        .bind(fixture.host_id)
        .bind(fixture.execution_id)
        .execute(&pool)
        .await
        .unwrap();
    let attempt = WorkflowRepository::new(pool.clone())
        .pending_terminal_attempts(&service_id, 1)
        .await
        .unwrap()
        .remove(0);
    let mut tx = pool.begin().await.unwrap();
    assert!(
        TaskExecutor::new(pool.clone())
            .reconcile_runner_attempt(&mut tx, &attempt)
            .await
            .unwrap()
    );
    tx.commit().await.unwrap();

    let task: (String, serde_json::Value) = sqlx::query_as(
        "SELECT status_code::text, task_output FROM task_info_t WHERE host_id = $1 AND task_id = $2",
    )
    .bind(fixture.host_id)
    .bind(fixture.task_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(task.0, "F");
    assert_eq!(task.1["error"]["failureClass"], "non_zero_exit");
    let process: (String, Option<String>) = sqlx::query_as(
        "SELECT status_code::text, error_info FROM process_info_t WHERE host_id = $1 AND process_id = $2",
    )
    .bind(fixture.host_id)
    .bind(fixture.process_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(process.0, "F");
    assert!(process.1.is_some());
}
