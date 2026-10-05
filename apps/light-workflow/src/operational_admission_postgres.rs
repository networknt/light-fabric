//! Prepared real PostgreSQL gates. Ignored unless separately authorized.
use super::*;
use crate::invocation::{
    AuthenticatedInvocationContext, PreparedInvocationStart, accept_invocation,
};
use chrono::{Duration, Utc};
use workflow_invocation_contract::*;

async fn pools() -> (PgPool, PgPool) {
    let runtime = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .after_connect(|c, _| {
            Box::pin(async move {
                sqlx::query("SET ROLE operations_workflow_runtime")
                    .execute(&mut *c)
                    .await?;
                sqlx::query("SET search_path TO workflow_ops,pg_catalog")
                    .execute(c)
                    .await?;
                Ok(())
            })
        })
        .connect(
            &std::env::var("WORKFLOW_ROLE_TEST_DATABASE_URL")
                .expect("explicit disposable runtime URL required"),
        )
        .await
        .unwrap();
    let admin = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .after_connect(|c, _| {
            Box::pin(async move {
                sqlx::query("SET ROLE operations_workflow_migrator")
                    .execute(c)
                    .await?;
                Ok(())
            })
        })
        .connect(
            &std::env::var("ADMIN_DATABASE_URL")
                .expect("explicit disposable migrator URL required"),
        )
        .await
        .unwrap();
    for pool in [&runtime, &admin] {
        let database: String = sqlx::query_scalar("SELECT current_database()")
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(
            database, "workflow_e04_w3c_test",
            "refuse any other database"
        );
    }
    let role: String = sqlx::query_scalar("SELECT current_user")
        .fetch_one(&runtime)
        .await
        .unwrap();
    assert_eq!(role, "operations_workflow_runtime");
    (runtime, admin)
}
fn identity() -> InvocationIdentity {
    InvocationIdentity {
        host_id: Uuid::now_v7(),
        principal_subject: "w3c-gate".into(),
        end_user_subject: "w3c-user".into(),
        caller_claims_digest: "unused".into(),
        caller_claims: json!({}),
        user_authorization: "unused".into(),
        user_authorization_exp: 0,
    }
}
#[tokio::test]
#[ignore = "requires separately authorized migrated workflow_e04_w3c_test and runtime/migrator URLs"]
async fn effective_privileges_and_immutable_evidence() {
    let (pool, admin) = pools().await;
    for privilege in ["SELECT", "INSERT", "UPDATE", "DELETE", "TRUNCATE"] {
        let granted:bool=sqlx::query_scalar("SELECT has_table_privilege(current_user,'workflow_ops.workflow_operation_receipt_t',$1)")
            .bind(privilege).fetch_one(&pool).await.unwrap();
        assert_eq!(
            granted,
            matches!(privilege, "SELECT" | "INSERT"),
            "effective inherited/direct privilege {privilege}"
        );
    }
    let id = identity();
    let op = Operation::new(&id, "workflow_start", "immutable", &json!({"x":1})).unwrap();
    let mut tx = pool.begin().await.unwrap();
    op.lock(&mut tx).await.unwrap();
    op.store(&mut tx, &json!({}), &json!({"original":true}), None)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    // Migrator default grants and owner privilege must not defeat the triggers.
    for statement in [
        "UPDATE workflow_ops.workflow_operation_receipt_t SET receipt='{}' WHERE host_id=$1",
        "DELETE FROM workflow_ops.workflow_operation_receipt_t WHERE host_id=$1",
    ] {
        let mut tx = admin.begin().await.unwrap();
        assert!(
            sqlx::query(statement)
                .bind(id.host_id)
                .execute(&mut *tx)
                .await
                .is_err()
        );
        tx.rollback().await.unwrap();
    }
    let mut tx = admin.begin().await.unwrap();
    assert!(
        sqlx::query("TRUNCATE workflow_ops.workflow_operation_receipt_t")
            .execute(&mut *tx)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
    assert_eq!(
        op.recover(&pool).await.unwrap().unwrap().receipt,
        json!({"original":true})
    );
}
#[tokio::test]
#[ignore = "requires separately authorized migrated workflow_e04_w3c_test and runtime/migrator URLs"]
async fn same_key_concurrency_and_changed_content_conflict() {
    let (pool, _) = pools().await;
    let id = identity();
    let op = Operation::new(&id, "workflow_start", "concurrent", &json!({"pin":"first"})).unwrap();
    let mut first = pool.begin().await.unwrap();
    op.lock(&mut first).await.unwrap();
    assert!(op.locked(&mut first).await.unwrap().is_none());
    let second_op = op.clone();
    let second_pool = pool.clone();
    let mut second = tokio::spawn(async move {
        let mut tx = second_pool.begin().await.unwrap();
        second_op.lock(&mut tx).await.unwrap();
        let receipt = second_op.locked(&mut tx).await.unwrap().unwrap().receipt;
        tx.rollback().await.unwrap();
        receipt
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut second)
            .await
            .is_err()
    );
    op.store(
        &mut first,
        &json!({}),
        &json!({"acceptedAt":"original"}),
        None,
    )
    .await
    .unwrap();
    first.commit().await.unwrap();
    assert_eq!(second.await.unwrap(), json!({"acceptedAt":"original"}));
    let changed = Operation::new(
        &id,
        "workflow_start",
        "concurrent",
        &json!({"pin":"changed"}),
    )
    .unwrap();
    assert_eq!(changed.scope(), op.scope());
    assert!(changed.recover(&pool).await.is_err());
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM workflow_ops.workflow_operation_receipt_t WHERE host_id=$1",
    )
    .bind(id.host_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
}
#[tokio::test]
#[ignore = "requires separately authorized migrated workflow_e04_w3c_test and runtime/migrator URLs; run serially"]
async fn policy_toggle_lock_orders_and_missing_policy() {
    let (pool, admin) = pools().await;
    sqlx::query("UPDATE workflow_ops.workflow_expression_profile_policy_t SET admission_enabled=true WHERE profile_id='cel-workflow-v2'").execute(&admin).await.unwrap();
    let mut accepting = pool.begin().await.unwrap();
    policy(&mut accepting).await.unwrap();
    let admin2 = admin.clone();
    let mut toggle = tokio::spawn(async move {
        sqlx::query("UPDATE workflow_ops.workflow_expression_profile_policy_t SET admission_enabled=false WHERE profile_id='cel-workflow-v2'").execute(&admin2).await.unwrap();
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut toggle)
            .await
            .is_err()
    );
    accepting.rollback().await.unwrap();
    toggle.await.unwrap();
    let mut toggling = admin.begin().await.unwrap();
    sqlx::query("UPDATE workflow_ops.workflow_expression_profile_policy_t SET admission_enabled=false WHERE profile_id='cel-workflow-v2'").execute(&mut *toggling).await.unwrap();
    let waiting_pool = pool.clone();
    let mut waiting = tokio::spawn(async move {
        let mut tx = waiting_pool.begin().await.unwrap();
        let refused = policy(&mut tx).await.is_err();
        tx.rollback().await.unwrap();
        refused
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), &mut waiting)
            .await
            .is_err()
    );
    toggling.commit().await.unwrap();
    assert!(waiting.await.unwrap());
    let mut accepting = pool.begin().await.unwrap();
    assert!(policy(&mut accepting).await.is_err());
    accepting.rollback().await.unwrap();
    // Missing row case is isolated inside a migrator transaction and restored.
    let mut missing = admin.begin().await.unwrap();
    sqlx::query("DELETE FROM workflow_ops.workflow_expression_profile_policy_t WHERE profile_id='cel-workflow-v2'").execute(&mut *missing).await.unwrap();
    assert!(policy(&mut missing).await.is_err());
    missing.rollback().await.unwrap();
}
#[tokio::test]
#[ignore = "requires separately authorized migrated workflow_e04_w3c_test and runtime/migrator URLs; run serially"]
async fn acceptance_rollback_snapshot_profile_and_off_recovery() {
    let _expression_fixture = crate::expression_test_support::acquire().await;
    let (pool, admin) = pools().await;
    let id = identity();
    let raw = json!({"document":{"dsl":"1.0.3","name":"gate","version":"1.0.0","metadata":{"lightExpressionProfile":"cel-workflow-v2"}},"evaluate":{"language":"cel"},"do":[{"t":{"set":{"x":"${ 1 }"}}}]});
    // Compilation completes before opening the accepting transaction.
    let engine =
        crate::expression_test_support::engine(workflow_expression::WorkerConfig::default())
            .unwrap();
    let plan = DefinitionValidation::from_raw(&raw).unwrap();
    plan.validate(&engine).await.unwrap();
    let validated = Validated {
        raw: raw.clone(),
        profile: plan.profile,
    };
    let now = Utc::now();
    let digest = format!("sha256:{}", "a".repeat(64));
    let input = json!({});
    let input_digest = canonical_sha256(&input).unwrap();
    let request = StartInvocationRequest {
        renewable_grant_id: None,
        parent_action_id: None,
        contract_version: CONTRACT_VERSION,
        workflow_instance_id: Uuid::now_v7(),
        stable_tool_ref: Uuid::now_v7(),
        workflow_definition_id: Uuid::now_v7(),
        workflow_version: "1.0.0".into(),
        definition_digest: canonical_sha256(&validated.raw).unwrap(),
        schema_digest: digest.clone(),
        policy_digest: digest.clone(),
        response_policy_digest: digest.clone(),
        mode: InvocationMode::Async,
        cancellation_policy: CancellationPolicy::BeforeEffectsOnly,
        execution_class: ExecutionClass::Interactive,
        permit_depth: 0,
        deadline_ts: now + Duration::minutes(5),
        canonical_input_profile: CANONICAL_INPUT_PROFILE.into(),
        normalized_input_digest: input_digest.clone(),
        input,
        caller_claims: json!({"sub":"w3c-user"}),
        idempotency: IdempotencyBinding {
            kind: IdempotencyKind::Explicit,
            scoped_key_digest: digest.clone(),
            input_digest,
            in_flight_until: now + Duration::minutes(5),
            result_replay_until: now + Duration::days(1),
        },
        budget: InvocationBudget {
            maximum_task_attempts: 10,
            maximum_nested_calls: 10,
            maximum_delegation_depth: 1,
            maximum_parallelism: 1,
            maximum_request_bytes: 65536,
            maximum_intermediate_bytes: 65536,
            maximum_result_bytes: 65536,
            maximum_cost_units: 100,
        },
        correlation_id: "w3c-gate".into(),
    };
    // The real process FK requires its referenced definition to exist before
    // acceptance. Keep this prerequisite outside the transactions whose full
    // rollback is asserted below, and use the actual validated document.
    sqlx::query(
        "INSERT INTO workflow_ops.wf_definition_t
         (host_id,wf_def_id,namespace,name,version,definition)
         VALUES($1,$2,'w3c-gate',$3,$4,$5)",
    )
    .bind(id.host_id)
    .bind(request.workflow_definition_id)
    .bind(validated.raw["document"]["name"].as_str().unwrap())
    .bind(&request.workflow_version)
    .bind(validated.raw.to_string())
    .execute(&admin)
    .await
    .unwrap();
    let prepared = PreparedInvocationStart {
        binding_id: None,
        process_id: Uuid::now_v7(),
        initial_task_id: Uuid::now_v7(),
        application_id: "w3c-gate",
        initial_task_name: "t",
        initial_task_type: "set",
        definition_snapshot: &raw,
        execution_placement: "host",
        execution_profile_id: "host",
        admission_profile: "portal_execution",
        tool_environment: "dev",
        policy_snapshot_id: None,
        task_policy_digest: digest.trim_start_matches("sha256:"),
        public_output_schema: None,
        expression_admission: Some(&validated),
    };
    let auth = AuthenticatedInvocationContext {
        host_id: id.host_id,
        principal_subject: &id.principal_subject,
        end_user_subject: &id.end_user_subject,
        update_user: "w3c-gate",
        user_authorization: None,
        user_authorization_exp: None,
    };
    let op = Operation::new(
        &id,
        "workflow_start",
        "rollback",
        &json!({"input":{},"pin":"original"}),
    )
    .unwrap();
    sqlx::query("UPDATE workflow_ops.workflow_expression_profile_policy_t SET admission_enabled=true WHERE profile_id='cel-workflow-v2'").execute(&admin).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    op.lock(&mut tx).await.unwrap();
    policy(&mut tx).await.unwrap();
    accept_invocation(&mut tx, &auth, &request, &prepared)
        .await
        .unwrap();
    op.store(
        &mut tx,
        &json!({"definitionSnapshot":raw}),
        &json!({"acceptedAt":"original"}),
        None,
    )
    .await
    .unwrap();
    // Failure at the receipt boundary aborts process/invocation/work together.
    assert!(
        op.store(&mut tx, &json!({}), &json!({}), None)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
    for table in [
        "process_info_t",
        "task_info_t",
        "workflow_invocation_t",
        "workflow_invocation_idempotency_t",
        "workflow_operation_receipt_t",
    ] {
        let count: i64 = sqlx::query_scalar(&format!(
            "SELECT count(*) FROM workflow_ops.{table} WHERE host_id=$1"
        ))
        .bind(id.host_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 0, "{table}");
    }
    let mut tx = pool.begin().await.unwrap();
    op.lock(&mut tx).await.unwrap();
    policy(&mut tx).await.unwrap();
    accept_invocation(&mut tx, &auth, &request, &prepared)
        .await
        .unwrap();
    op.store(
        &mut tx,
        &json!({"definitionSnapshot":raw}),
        &json!({"acceptedAt":"original"}),
        None,
    )
    .await
    .unwrap();
    let mut competing = pool.begin().await.unwrap();
    {
        let mut waiting = Box::pin(op.lock(&mut competing));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut waiting)
                .await
                .is_err()
        );
        tx.commit().await.unwrap();
        waiting.await.unwrap();
    }
    assert_eq!(
        op.locked(&mut competing).await.unwrap().unwrap().receipt,
        json!({"acceptedAt":"original"})
    );
    competing.rollback().await.unwrap();
    let stored:(String,Value)=sqlx::query_as("SELECT expression_profile,definition_snapshot FROM workflow_ops.process_info_t WHERE host_id=$1 AND process_id=$2").bind(id.host_id).bind(prepared.process_id).fetch_one(&pool).await.unwrap();
    assert_eq!(stored, ("cel-workflow-v2".into(), raw));
    sqlx::query("UPDATE workflow_ops.workflow_expression_profile_policy_t SET admission_enabled=false WHERE profile_id='cel-workflow-v2'").execute(&admin).await.unwrap();
    engine.shutdown(std::time::Duration::from_secs(2)).unwrap();
    assert_eq!(
        op.recover(&pool).await.unwrap().unwrap().receipt,
        json!({"acceptedAt":"original"})
    );
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM workflow_ops.task_info_t WHERE host_id=$1")
            .bind(id.host_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 1);
}
