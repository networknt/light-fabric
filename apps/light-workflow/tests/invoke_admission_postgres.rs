#[path = "support/ops_db.rs"]
mod ops_db;

use axum::{body::to_bytes, response::IntoResponse};
use chrono::{Duration, Utc};
use light_workflow::{
    invocation::{
        AcceptOutcome, AuthenticatedInvocationContext, PreparedInvocationStart, accept_invocation,
    },
    invoke_api::{self, InvokeAdmission},
    run_credential::RunCredentialVault,
};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use std::{path::Path, sync::Arc};
use uuid::Uuid;
use workflow_invocation_contract::{
    CANONICAL_INPUT_PROFILE, CONTRACT_VERSION, CancellationPolicy, ExecutionClass,
    IdempotencyBinding, IdempotencyKind, InvocationBudget, InvocationMode, StartInvocationRequest,
    canonical_sha256,
};

struct Fixture {
    pool: PgPool,
    host: Uuid,
    wf: Uuid,
    tool: Uuid,
    binding: Uuid,
    user: Uuid,
    vault: Arc<RunCredentialVault>,
    keyring: std::path::PathBuf,
}

fn digest() -> String {
    format!("sha256:{}", "a".repeat(64))
}
fn limits(concurrent: i64, per_user: i64) -> Value {
    json!({"maximumConcurrentRuns":concurrent,"maximumConcurrentRunsPerUser":per_user,"startsPerMinute":100,"startsPerMinutePerUser":100})
}

async fn fixture() -> Fixture {
    let pool = ops_db::runtime_pool();
    let _admin = ops_db::admin_pool();
    let host = Uuid::new_v4();
    let (wf, _) = ops_db::insert_workflow_definition(&pool, host)
        .await
        .unwrap();
    let tool = Uuid::new_v4();
    let binding = Uuid::new_v4();
    let user = Uuid::new_v4();
    sqlx::query("INSERT INTO workflow_tool_binding_t(host_id,binding_id,tool_id,wf_def_id,workflow_version,definition_digest,schema_digest,invocation_mode,sync_wait_ms,total_deadline_ms,execution_class,result_text_mode,idempotency_policy,delegation_policy,response_policy_digest,runtime_bounds,policy_digest,active,revision_status,source_binding_id,binding_digest,approval_digest,requested_by,requested_ts) VALUES($1,$2,$3,$4,'1.0.0',$5,$5,'sync',1000,30000,'interactive','compact-json','{}'::jsonb,'{}'::jsonb,$5,'{}'::jsonb,$5,true,'approved',$2,$5,$5,'invoke-test',CURRENT_TIMESTAMP)")
        .bind(host).bind(binding).bind(tool).bind(wf).bind(digest()).execute(&pool).await.unwrap();
    let keyring =
        std::env::temp_dir().join(format!("workflow-invoke-test-{}.json", Uuid::new_v4()));
    tokio::fs::write(
        &keyring,
        r#"{"activeKeyId":"test","keys":{"test":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="}}"#,
    )
    .await
    .unwrap();
    let vault = Arc::new(
        RunCredentialVault::load(Path::new("/"), Some(&keyring))
            .await
            .unwrap()
            .unwrap(),
    );
    Fixture {
        pool,
        host,
        wf,
        tool,
        binding,
        user,
        vault,
        keyring,
    }
}

fn request(f: &Fixture, input: Value, scope: String) -> StartInvocationRequest {
    let now = Utc::now();
    let deadline = now + Duration::minutes(5);
    let input_digest = canonical_sha256(&input).unwrap();
    StartInvocationRequest {
        renewable_grant_id: None,
        parent_action_id: None,
        contract_version: CONTRACT_VERSION,
        workflow_instance_id: Uuid::now_v7(),
        stable_tool_ref: f.tool,
        workflow_definition_id: f.wf,
        workflow_version: "1.0.0".into(),
        definition_digest: digest(),
        schema_digest: digest(),
        policy_digest: digest(),
        response_policy_digest: digest(),
        mode: InvocationMode::Sync,
        cancellation_policy: CancellationPolicy::BeforeEffectsOnly,
        execution_class: ExecutionClass::Interactive,
        permit_depth: 0,
        deadline_ts: deadline,
        canonical_input_profile: CANONICAL_INPUT_PROFILE.into(),
        normalized_input_digest: input_digest.clone(),
        input,
        caller_claims: json!({"sub":f.user.to_string()}),
        idempotency: IdempotencyBinding {
            kind: IdempotencyKind::Derived,
            scoped_key_digest: scope,
            input_digest,
            in_flight_until: deadline,
            result_replay_until: deadline,
        },
        budget: InvocationBudget {
            maximum_task_attempts: 8,
            maximum_nested_calls: 8,
            maximum_delegation_depth: 1,
            maximum_parallelism: 1,
            maximum_request_bytes: 1048576,
            maximum_intermediate_bytes: 4194304,
            maximum_result_bytes: 1048576,
            maximum_cost_units: 1000,
        },
        correlation_id: Uuid::new_v4().to_string(),
    }
}

async fn admit(
    f: &Fixture,
    req: &StartInvocationRequest,
    principal: &str,
    max: i64,
    per_user: i64,
    exp: i64,
    rollback: bool,
) -> Result<AcceptOutcome, String> {
    let sealed = f
        .vault
        .seal(req.workflow_instance_id, "test-token", exp)
        .unwrap();
    let hook = InvokeAdmission::new(
        f.binding,
        digest(),
        limits(max, per_user),
        f.vault.clone(),
        sealed,
        "test-token",
        exp,
    )
    .unwrap();
    let mut tx = f.pool.begin().await.map_err(|e| e.to_string())?;
    let user = f.user.to_string();
    let auth = AuthenticatedInvocationContext {
        host_id: f.host,
        principal_subject: principal,
        end_user_subject: &user,
        update_user: "invoke-test",
        user_authorization: Some("must-not-store"),
        user_authorization_exp: Some(exp),
    };
    let snapshot = json!({"document":{"dsl":"1.0.3"}});
    let bare_digest = "a".repeat(64);
    let prepared = PreparedInvocationStart {
        binding_id: Some(f.binding),
        process_id: Uuid::new_v4(),
        initial_task_id: Uuid::new_v4(),
        application_id: "invoke-test",
        initial_task_name: "task",
        initial_task_type: "http",
        definition_snapshot: &snapshot,
        execution_placement: "host",
        execution_profile_id: "host",
        admission_profile: "workflow_backed",
        policy_snapshot_id: None,
        task_policy_digest: &bare_digest,
        public_output_schema: None,
        expression_admission: None,
    };
    let outcome = accept_invocation(&mut tx, &auth, req, &prepared)
        .await
        .map_err(|e| e.to_string())?;
    hook.apply(&mut tx, f.host, &user, req, &outcome)
        .await
        .map_err(|e| format!("{e:?}"))?;
    if rollback {
        tx.rollback().await.map_err(|e| e.to_string())?;
    } else {
        tx.commit().await.map_err(|e| e.to_string())?;
    }
    Ok(outcome)
}

async fn cleanup(f: Fixture) {
    tokio::fs::remove_file(&f.keyring).await.unwrap();
}

#[tokio::test]
#[ignore = "requires owned scratch DATABASE_URL and ADMIN_DATABASE_URL"]
async fn accepted_and_rollback_are_atomic() {
    let f = fixture().await;
    let req = request(&f, json!({"n":1}), digest());
    admit(&f, &req, "client", 20, 20, 100, true).await.unwrap();
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM workflow_run_credential_t WHERE host_id=$1")
            .bind(f.host)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM workflow_action_authority_t WHERE host_id=$1")
            .bind(f.host)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
    admit(&f, &req, "client", 20, 20, 100, false).await.unwrap();
    let row = sqlx::query("SELECT c.key_id,c.token_bytes,a.credential_kind,a.action_limit FROM workflow_run_credential_t c JOIN workflow_action_authority_t a ON a.host_id=c.host_id AND a.run_id=c.workflow_instance_id WHERE c.host_id=$1")
        .bind(f.host).fetch_one(&f.pool).await.unwrap();
    let key: String = row.try_get("key_id").unwrap();
    let bytes: Vec<u8> = row.try_get("token_bytes").unwrap();
    assert_ne!(key, "plaintext");
    assert_ne!(bytes, b"test-token");
    assert_eq!(
        row.try_get::<String, _>("credential_kind").unwrap(),
        "invoke"
    );
    assert_eq!(row.try_get::<i64, _>("action_limit").unwrap(), 8);
    cleanup(f).await;
}

#[tokio::test]
#[ignore = "requires owned scratch DATABASE_URL and ADMIN_DATABASE_URL"]
async fn workflow_backed_authorization_is_null() {
    let f = fixture().await;
    let req = request(&f, json!({"n":1}), digest());
    admit(&f, &req, "client", 20, 20, 100, false).await.unwrap();
    let stored: Option<String> =
        sqlx::query_scalar("SELECT user_authorization FROM workflow_invocation_t WHERE host_id=$1")
            .bind(f.host)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert!(stored.is_none());
    cleanup(f).await;
}

#[tokio::test]
#[ignore = "requires owned scratch DATABASE_URL and ADMIN_DATABASE_URL"]
async fn replay_keeps_instance_and_refreshes_exp_only_forward() {
    let f = fixture().await;
    let req = request(&f, json!({"n":1}), digest());
    let first = admit(&f, &req, "client", 1, 1, 100, false).await.unwrap();
    let retry = request(&f, json!({"n":1}), digest());
    let second = admit(&f, &retry, "client", 1, 1, 200, false).await.unwrap();
    assert!(matches!(first, AcceptOutcome::Accepted { .. }));
    assert!(
        matches!(second,AcceptOutcome::Replay{workflow_instance_id,..} if workflow_instance_id==req.workflow_instance_id)
    );
    let exp: i64 =
        sqlx::query_scalar("SELECT token_exp FROM workflow_run_credential_t WHERE host_id=$1")
            .bind(f.host)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(exp, 200);
    let _ = admit(
        &f,
        &request(&f, json!({"n":1}), digest()),
        "client",
        1,
        1,
        150,
        false,
    )
    .await
    .unwrap();
    let exp: i64 =
        sqlx::query_scalar("SELECT token_exp FROM workflow_run_credential_t WHERE host_id=$1")
            .bind(f.host)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(exp, 200);
    cleanup(f).await;
}

#[tokio::test]
#[ignore = "requires owned scratch DATABASE_URL and ADMIN_DATABASE_URL"]
async fn different_client_conflicts_until_terminal_window_expires() {
    let f = fixture().await;
    let req = request(&f, json!({"n":1}), digest());
    admit(&f, &req, "client-a", 20, 20, 100, false)
        .await
        .unwrap();
    assert!(
        admit(
            &f,
            &request(&f, json!({"n":1}), digest()),
            "client-b",
            20,
            20,
            100,
            false
        )
        .await
        .unwrap_err()
        .contains("IDEMPOTENCY_CONFLICT")
    );
    sqlx::query("UPDATE workflow_invocation_t SET state='COMPLETED',terminal_ts=CURRENT_TIMESTAMP WHERE host_id=$1")
        .bind(f.host).execute(&f.pool).await.unwrap();
    sqlx::query("UPDATE workflow_invocation_idempotency_t SET in_flight_until=CURRENT_TIMESTAMP-interval '1 second',result_replay_until=CURRENT_TIMESTAMP-interval '1 second' WHERE host_id=$1")
        .bind(f.host).execute(&f.pool).await.unwrap();
    assert!(matches!(
        admit(
            &f,
            &request(&f, json!({"n":1}), digest()),
            "client-b",
            20,
            20,
            100,
            false
        )
        .await
        .unwrap(),
        AcceptOutcome::Accepted { .. }
    ));
    cleanup(f).await;
}

#[tokio::test]
#[ignore = "requires owned scratch DATABASE_URL and ADMIN_DATABASE_URL"]
async fn per_user_capacity_refuses_new_input_but_allows_replay() {
    let f = fixture().await;
    let req = request(&f, json!({"n":1}), digest());
    admit(&f, &req, "client", 20, 1, 100, false).await.unwrap();
    let other = request(&f, json!({"n":2}), format!("sha256:{}", "b".repeat(64)));
    assert!(
        admit(&f, &other, "client", 20, 1, 100, false)
            .await
            .unwrap_err()
            .contains("Capacity")
    );
    assert!(matches!(
        admit(
            &f,
            &request(&f, json!({"n":1}), digest()),
            "client",
            20,
            1,
            100,
            false
        )
        .await
        .unwrap(),
        AcceptOutcome::Replay { .. }
    ));
    cleanup(f).await;
}

#[tokio::test]
#[ignore = "requires owned scratch DATABASE_URL and ADMIN_DATABASE_URL"]
async fn five_callers_respect_two_run_limit() {
    let f = Arc::new(fixture().await);
    let mut jobs = Vec::new();
    for n in 0..5 {
        let f = f.clone();
        jobs.push(tokio::spawn(async move {
            let scope = format!("sha256:{n:064x}");
            let req = request(&f, json!({"n":n}), scope);
            admit(&f, &req, &format!("client-{n}"), 2, 5, 100, false)
                .await
                .is_ok()
        }));
    }
    let mut accepted = 0;
    for job in jobs {
        if job.await.unwrap() {
            accepted += 1;
        }
    }
    assert_eq!(accepted, 2);
    let f = Arc::try_unwrap(f).ok().unwrap();
    cleanup(f).await;
}

async fn code(error: light_workflow::rule_api::ApiError) -> String {
    let bytes = to_bytes(error.into_response().into_body(), usize::MAX)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    value["code"].as_str().unwrap().to_owned()
}

#[tokio::test]
#[ignore = "requires owned scratch DATABASE_URL and ADMIN_DATABASE_URL"]
async fn stale_pin_policy_and_parent_are_rejected() {
    let f = fixture().await;
    assert_eq!(
        code(
            invoke_api::verify_gateway_pins("stale", &digest(), &digest(), &digest()).unwrap_err()
        )
        .await,
        "WORKFLOW_DEFINITION_MISMATCH"
    );
    assert_eq!(
        code(
            invoke_api::verify_caller_policy(
                &json!({"anyRole":["owner"]}),
                &json!({"roles":["user"]})
            )
            .unwrap_err()
        )
        .await,
        "WORKFLOW_POLICY_DENIED"
    );
    assert_eq!(
        code(invoke_api::reject_parent_action(Some(Uuid::new_v4())).unwrap_err()).await,
        "WORKFLOW_POLICY_DENIED"
    );
    cleanup(f).await;
}

#[tokio::test]
#[ignore = "requires owned scratch DATABASE_URL and ADMIN_DATABASE_URL"]
async fn absent_vault_refuses_before_any_write() {
    let f = fixture().await;
    assert_eq!(
        code(invoke_api::require_vault(None).err().unwrap()).await,
        "WORKFLOW_START_REJECTED"
    );
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM workflow_invocation_t WHERE host_id=$1")
            .bind(f.host)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
    cleanup(f).await;
}

#[tokio::test]
#[ignore = "requires owned scratch DATABASE_URL and ADMIN_DATABASE_URL"]
async fn vault_admission_does_not_require_long_registration() {
    let f = fixture().await;
    let req = request(&f, json!({"n":1}), digest());
    admit(&f, &req, "client", 20, 20, 100, false).await.unwrap();
    let long_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM workflow_long_credential_t WHERE host_id=$1")
            .bind(f.host)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(long_count, 0);
    let invoke_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM workflow_run_credential_t WHERE host_id=$1")
            .bind(f.host)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(invoke_count, 1);
    cleanup(f).await;
}
