#[path = "support/ops_db.rs"]
mod ops_db;

use axum::{body::to_bytes, response::IntoResponse};
use light_workflow::{publication_api, rule_api};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use uuid::Uuid;

const DEFINITION: &str = "document: {dsl: '1.0.3', namespace: step03, name: example, version: '1.0.0'}\nevaluate: {language: cel}\ndo:\n  - finish:\n      set: {status: done}\n      end: true\n";
const NEXT_DEFINITION: &str = "document: {dsl: '1.0.3', namespace: step03, name: example, version: '1.1.0'}\nevaluate: {language: cel}\ndo:\n  - finish:\n      set: {status: newer}\n      end: true\n";
const HUMAN_DEFINITION: &str = "document: {dsl: '1.0.3', namespace: step03, name: human, version: '1.0.0'}\nevaluate: {language: cel}\ndo:\n  - approval:\n      ask:\n        prompt: Approve?\n        mode: choice\n        options:\n          - {label: Approve, value: approve}\n          - {label: Reject, value: reject}\n";

fn pools() -> (PgPool, PgPool) {
    // Both variables are mandatory even when a case only uses the runtime role.
    (ops_db::runtime_pool(), ops_db::admin_pool())
}

fn save_input(
    host: Uuid,
    wf: Uuid,
    revision: i64,
    definition: &str,
    owner: Uuid,
    active: bool,
) -> Value {
    json!({"hostId":host,"wfDefId":wf,"sourceRevision":revision,"actor":"step03-test",
        "namespace":"step03","name":format!("definition-{wf}"),"version":"1.0.0",
        "definition":definition,"lifecycleStatus":"DRAFT","catalogVisible":false,
        "owner":{"userId":owner},"active":active})
}

async fn save(pool: &PgPool, input: &Value) -> Value {
    publication_api::save_definition_verified(pool, input)
        .await
        .expect("definition save")
}

fn publish_input(
    host: Uuid,
    wf: Uuid,
    version: &str,
    definition: &str,
    digest: &str,
    owner: Uuid,
    operation: Uuid,
) -> Value {
    json!({"hostId":host,"wfDefId":wf,"namespace":"step03","name":format!("definition-{wf}"),
        "version":version,"definition":definition,"expectedDefinitionDigest":digest,
        "owner":{"userId":owner},"operationId":operation})
}

async fn publish(pool: &PgPool, input: &Value, actor: Uuid) -> Value {
    publication_api::publish_definition_verified(pool, input, &actor.to_string(), &[], &|_| Ok(()))
        .await
        .expect("definition publish")
}

async fn error_value(error: rule_api::ApiError) -> Value {
    let response = error.into_response();
    serde_json::from_slice(
        &to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("error body"),
    )
    .expect("structured Workflow error")
}

async fn insert_binding(
    pool: &PgPool,
    host: Uuid,
    wf: Uuid,
    tool: Uuid,
    version: &str,
    digest: &str,
    status: &str,
    active: bool,
) -> Uuid {
    let binding = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO workflow_tool_binding_t(
        host_id,binding_id,tool_id,wf_def_id,workflow_version,definition_digest,schema_digest,
        invocation_mode,sync_wait_ms,total_deadline_ms,execution_class,result_text_mode,
        idempotency_policy,delegation_policy,response_policy_digest,runtime_bounds,policy_digest,
        revision_status,binding_digest,approval_digest,source_binding_id,requested_by,requested_ts,
        active,tool_name) VALUES($1,$2,$3,$4,$5,$6,$6,'sync',100,1000,'interactive',
        'compact-json','{}','{}',$6,'{}',$6,$7,$6,$6,$8,'step03-test',now(),$9,$10)",
    )
    .bind(host)
    .bind(binding)
    .bind(tool)
    .bind(wf)
    .bind(version)
    .bind(digest)
    .bind(status)
    .bind(Uuid::new_v4())
    .bind(active)
    .bind(format!("tool-{tool}"))
    .execute(pool)
    .await
    .expect("binding fixture");
    binding
}

fn grant(host: Uuid, wf: Uuid, revision: i64, grant_id: Uuid, tool: Uuid) -> Value {
    json!({"hostId":host,"wfDefId":wf,"sourceRevision":revision,"actor":"step03-test",
        "grants":[{"grantId":grant_id,"toolId":tool,"toolVersion":"1.0.0",
            "lightapiDigest":format!("sha256:{}", "a".repeat(64)),"allowedEnvironments":["dev"]}]})
}

#[tokio::test]
#[ignore = "requires the isolated workflow-invoke scratch database"]
async fn save_then_start_digest_pin_rejects_stale_head() {
    let (pool, _admin) = pools();
    let (host, wf, owner) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let saved = save(&pool, &save_input(host, wf, 1, DEFINITION, owner, true)).await;
    assert_eq!(saved["result"], "saved");
    let digest = saved["definitionDigest"].as_str().unwrap();
    let (version, text) = rule_api::load_saved_head_for_start(&pool, host, wf, Some(digest))
        .await
        .unwrap();
    assert_eq!(version, "1.0.0");
    assert_eq!(text, DEFINITION);
    let newer = save(
        &pool,
        &save_input(host, wf, 2, NEXT_DEFINITION, owner, true),
    )
    .await;
    let stored: String = sqlx::query_scalar(
        "SELECT definition FROM wf_definition_t WHERE host_id=$1 AND wf_def_id=$2",
    )
    .bind(host)
    .bind(wf)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored, NEXT_DEFINITION);
    let mismatch = rule_api::load_saved_head_for_start(&pool, host, wf, Some(digest))
        .await
        .unwrap_err();
    assert_eq!(
        error_value(mismatch).await["code"],
        "WORKFLOW_DEFINITION_MISMATCH"
    );
    rule_api::load_saved_head_for_start(
        &pool,
        host,
        wf,
        Some(newer["definitionDigest"].as_str().unwrap()),
    )
    .await
    .unwrap();
    let concurrent_wf = Uuid::new_v4();
    let first = save_input(host, concurrent_wf, 1, DEFINITION, owner, true);
    let second = first.clone();
    let (a, b) = tokio::join!(
        publication_api::save_definition_verified(&pool, &first),
        publication_api::save_definition_verified(&pool, &second)
    );
    let results = [
        a.unwrap()["result"].as_str().unwrap().to_string(),
        b.unwrap()["result"].as_str().unwrap().to_string(),
    ];
    assert!(results.contains(&"saved".to_string()) && results.contains(&"unchanged".to_string()));
}

#[tokio::test]
#[ignore = "requires the isolated workflow-invoke scratch database"]
async fn binding_admission_reads_pinned_version_after_new_publish() {
    let (pool, _admin) = pools();
    let (host, wf, owner, tool) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    let saved = save(&pool, &save_input(host, wf, 1, DEFINITION, owner, true)).await;
    let digest = saved["definitionDigest"].as_str().unwrap();
    publish(
        &pool,
        &publish_input(host, wf, "1.0.0", DEFINITION, digest, owner, Uuid::new_v4()),
        owner,
    )
    .await;
    insert_binding(&pool, host, wf, tool, "1.0.0", digest, "approved", true).await;
    let newer = save(
        &pool,
        &save_input(host, wf, 2, NEXT_DEFINITION, owner, true),
    )
    .await;
    publish(
        &pool,
        &publish_input(
            host,
            wf,
            "1.1.0",
            NEXT_DEFINITION,
            newer["definitionDigest"].as_str().unwrap(),
            owner,
            Uuid::new_v4(),
        ),
        owner,
    )
    .await;
    let row = rule_api::read_admissible_pinned_binding(&pool, host, tool)
        .await
        .unwrap();
    assert_eq!(
        row.try_get::<String, _>("workflow_version").unwrap(),
        "1.0.0"
    );
    assert_eq!(row.try_get::<String, _>("definition").unwrap(), DEFINITION);
    let denied = publication_api::retire_definition_verified(
        &pool,
        &json!({"hostId":host,"wfDefId":wf,"version":"1.0.0","operationId":Uuid::new_v4()}),
        &owner.to_string(),
    )
    .await
    .unwrap_err();
    let error = error_value(denied).await;
    assert_eq!(error["code"], "WORKFLOW_START_REJECTED");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains(&format!("tool-{tool}"))
    );
}

#[tokio::test]
#[ignore = "requires the isolated workflow-invoke scratch database"]
async fn retired_version_refuses_admission() {
    let (pool, _admin) = pools();
    let (host, wf, owner, tool) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    let saved = save(&pool, &save_input(host, wf, 1, DEFINITION, owner, true)).await;
    let digest = saved["definitionDigest"].as_str().unwrap();
    publish(
        &pool,
        &publish_input(host, wf, "1.0.0", DEFINITION, digest, owner, Uuid::new_v4()),
        owner,
    )
    .await;
    insert_binding(&pool, host, wf, tool, "1.0.0", digest, "approved", false).await;
    let retire = json!({"hostId":host,"wfDefId":wf,"version":"1.0.0","operationId":Uuid::new_v4()});
    publication_api::retire_definition_verified(&pool, &retire, &owner.to_string())
        .await
        .unwrap();
    // Admission's active binding selector must reject a retired version even
    // if an old binding is reactivated by a stale projection.
    sqlx::query("UPDATE workflow_tool_binding_t SET active=true WHERE host_id=$1 AND tool_id=$2")
        .bind(host)
        .bind(tool)
        .execute(&pool)
        .await
        .unwrap();
    let error = rule_api::read_admissible_pinned_binding(&pool, host, tool)
        .await
        .unwrap_err();
    assert_eq!(
        error_value(error).await["code"],
        "WORKFLOW_DEFINITION_RETIRED"
    );
    let replay = publish(
        &pool,
        &publish_input(host, wf, "1.0.0", DEFINITION, digest, owner, Uuid::new_v4()),
        owner,
    )
    .await;
    assert_eq!(replay["result"], "unchanged");
    assert_eq!(replay["status"], "retired");
    let changed = save(
        &pool,
        &save_input(host, wf, 2, NEXT_DEFINITION, owner, true),
    )
    .await;
    let mismatch = publication_api::publish_definition_verified(
        &pool,
        &publish_input(
            host,
            wf,
            "1.0.0",
            NEXT_DEFINITION,
            changed["definitionDigest"].as_str().unwrap(),
            owner,
            Uuid::new_v4(),
        ),
        &owner.to_string(),
        &[],
        &|_| Ok(()),
    )
    .await
    .unwrap_err();
    assert_eq!(
        error_value(mismatch).await["code"],
        "WORKFLOW_DEFINITION_MISMATCH"
    );
}

#[tokio::test]
#[ignore = "requires the isolated workflow-invoke scratch database"]
async fn retirement_withdraws_pending_revision_and_records_decision() {
    let (pool, _admin) = pools();
    let (host, wf, owner, tool) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    let saved = save(&pool, &save_input(host, wf, 1, DEFINITION, owner, true)).await;
    publish(
        &pool,
        &publish_input(
            host,
            wf,
            "1.0.0",
            DEFINITION,
            saved["definitionDigest"].as_str().unwrap(),
            owner,
            Uuid::new_v4(),
        ),
        owner,
    )
    .await;
    let binding = insert_binding(
        &pool,
        host,
        wf,
        tool,
        "1.0.0",
        saved["definitionDigest"].as_str().unwrap(),
        "pendingApproval",
        false,
    )
    .await;
    // A real head may also have an active binding to another version.
    let next = save(
        &pool,
        &save_input(host, wf, 2, NEXT_DEFINITION, owner, true),
    )
    .await;
    publish(
        &pool,
        &publish_input(
            host,
            wf,
            "1.1.0",
            NEXT_DEFINITION,
            next["definitionDigest"].as_str().unwrap(),
            owner,
            Uuid::new_v4(),
        ),
        owner,
    )
    .await;
    let active = insert_binding(
        &pool,
        host,
        wf,
        tool,
        "1.1.0",
        next["definitionDigest"].as_str().unwrap(),
        "approved",
        true,
    )
    .await;
    sqlx::query("INSERT INTO workflow_tool_publication_t(host_id,tool_id,aggregate_version,active_binding_id,pending_binding_id) VALUES($1,$2,7,$3,$4)")
        .bind(host).bind(tool).bind(active).bind(binding).execute(&pool).await.unwrap();
    let new_owner = Uuid::new_v4();
    save(&pool, &save_input(host, wf, 3, DEFINITION, new_owner, true)).await;
    let pending_owner: Uuid = sqlx::query_scalar(
        "SELECT owner_user_id FROM workflow_tool_binding_t WHERE host_id=$1 AND binding_id=$2",
    )
    .bind(host)
    .bind(binding)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        pending_owner, new_owner,
        "pending approval follows the new owner"
    );
    let operation = Uuid::new_v4();
    let receipt = publication_api::retire_definition_verified(
        &pool,
        &json!({"hostId":host,"wfDefId":wf,"version":"1.0.0","operationId":operation}),
        &owner.to_string(),
    )
    .await
    .unwrap();
    assert_eq!(receipt["result"], "retired");
    assert_eq!(receipt["withdrawnBindingIds"][0], binding.to_string());
    let row = sqlx::query("SELECT b.revision_status,d.action,d.operation_id FROM workflow_tool_binding_t b JOIN workflow_tool_binding_decision_t d ON d.host_id=b.host_id AND d.binding_id=b.binding_id WHERE b.host_id=$1 AND b.binding_id=$2")
        .bind(host).bind(binding).fetch_one(&pool).await.unwrap();
    assert_eq!(
        row.try_get::<String, _>("revision_status").unwrap(),
        "withdrawn"
    );
    assert_eq!(row.try_get::<String, _>("action").unwrap(), "withdraw");
    assert_eq!(row.try_get::<Uuid, _>("operation_id").unwrap(), operation);
    let head = sqlx::query("SELECT aggregate_version,active_binding_id,pending_binding_id FROM workflow_tool_publication_t WHERE host_id=$1 AND tool_id=$2")
        .bind(host).bind(tool).fetch_one(&pool).await.unwrap();
    assert_eq!(head.try_get::<i64, _>("aggregate_version").unwrap(), 8);
    assert_eq!(
        head.try_get::<Option<Uuid>, _>("active_binding_id")
            .unwrap(),
        Some(active)
    );
    assert_eq!(
        head.try_get::<Option<Uuid>, _>("pending_binding_id")
            .unwrap(),
        None
    );
}

#[tokio::test]
#[ignore = "requires the isolated workflow-invoke scratch database"]
async fn operation_replay_returns_receipt_and_reuse_conflicts() {
    let (pool, _admin) = pools();
    let (host, wf, owner, operation) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    let saved = save(&pool, &save_input(host, wf, 1, DEFINITION, owner, true)).await;
    let request = publish_input(
        host,
        wf,
        "1.0.0",
        DEFINITION,
        saved["definitionDigest"].as_str().unwrap(),
        owner,
        operation,
    );
    let first = publish(&pool, &request, owner).await;
    let replay = publish(&pool, &request, owner).await;
    assert_eq!(replay, first);
    let mut changed = request.clone();
    changed["bindingApproval"] = json!("reapprove");
    let conflict = publication_api::publish_definition_verified(
        &pool,
        &changed,
        &owner.to_string(),
        &[],
        &|_| Ok(()),
    )
    .await
    .unwrap_err();
    assert_eq!(
        error_value(conflict).await["code"],
        "WORKFLOW_IDEMPOTENCY_CONFLICT"
    );
    let stored: Value = sqlx::query_scalar(
        "SELECT receipt FROM workflow_publication_operation_t WHERE host_id=$1 AND operation_id=$2",
    )
    .bind(host)
    .bind(operation)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored, first);
    let other_wf = Uuid::new_v4();
    let other_saved = save(
        &pool,
        &save_input(host, other_wf, 1, DEFINITION, owner, true),
    )
    .await;
    let mut reapprove = publish_input(
        host,
        other_wf,
        "1.0.0",
        DEFINITION,
        other_saved["definitionDigest"].as_str().unwrap(),
        owner,
        Uuid::new_v4(),
    );
    reapprove["bindingApproval"] = json!("reapprove");
    let non_owner = Uuid::new_v4();
    let receipt = publish(&pool, &reapprove, non_owner).await;
    assert_eq!(receipt["bindingApproval"], "carryOver");
    let owner_receipt = publish(
        &pool,
        &publish_input(
            host,
            other_wf,
            "1.0.0",
            DEFINITION,
            other_saved["definitionDigest"].as_str().unwrap(),
            owner,
            Uuid::new_v4(),
        ),
        owner,
    )
    .await;
    assert_eq!(
        owner_receipt["bindingApproval"], "carryOver",
        "unchanged receipt reflects the stored approval"
    );
    let human_wf = Uuid::new_v4();
    let human_saved = save(
        &pool,
        &save_input(host, human_wf, 1, HUMAN_DEFINITION, owner, true),
    )
    .await;
    let mut human_request = publish_input(
        host,
        human_wf,
        "1.0.0",
        HUMAN_DEFINITION,
        human_saved["definitionDigest"].as_str().unwrap(),
        owner,
        Uuid::new_v4(),
    );
    human_request["bindingApproval"] = json!("reapprove");
    let human_receipt = publish(&pool, &human_request, owner).await;
    assert_eq!(
        human_receipt["result"], "published",
        "a generic valid human workflow may be published before binding-specific sync validation"
    );
    assert_eq!(human_receipt["bindingApproval"], "reapprove");
}

#[tokio::test]
#[ignore = "requires the isolated workflow-invoke scratch database"]
async fn grant_revocation_stays_removed_after_delayed_sync() {
    let (pool, admin) = pools();
    let (host, wf, owner, grant_id, tool) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    let saved = save(&pool, &save_input(host, wf, 1, DEFINITION, owner, true)).await;
    let binding = insert_binding(
        &pool,
        host,
        wf,
        Uuid::new_v4(),
        "1.0.0",
        saved["definitionDigest"].as_str().unwrap(),
        "approved",
        true,
    )
    .await;
    sqlx::query("INSERT INTO workflow_endpoint_target_t(host_id,binding_id,endpoint_ref,endpoint_uri,allowed_methods,authorization_policy_digest)
        VALUES($1,$2,'nested-step03','https://example.invalid/nested',ARRAY['GET'],$3)")
        .bind(host).bind(binding).bind(format!("sha256:{}","b".repeat(64)))
        .execute(&pool).await.unwrap();
    let prior = grant(host, wf, 2, grant_id, tool);
    let first = publication_api::sync_grants_verified(&pool, &prior)
        .await
        .unwrap();
    let unchanged = publication_api::sync_grants_verified(&pool, &prior)
        .await
        .unwrap();
    assert_eq!(unchanged["result"], "unchanged");
    assert_eq!(unchanged["grantSetDigest"], first["grantSetDigest"]);
    let executor_join = "SELECT EXISTS(
        SELECT 1 FROM workflow_tool_grant_t g
        JOIN workflow_tool_binding_t binding ON binding.host_id=g.host_id AND binding.wf_def_id=g.wf_def_id AND binding.active
        JOIN workflow_endpoint_target_t target ON target.host_id=binding.host_id AND target.binding_id=binding.binding_id
        WHERE g.host_id=$1 AND g.wf_def_id=$2 AND g.active AND g.tool_id=$3
          AND g.tool_version='1.0.0' AND g.lightapi_digest=$4
          AND 'dev'=ANY(g.allowed_environments) AND target.endpoint_ref='nested-step03'
          AND target.active AND 'GET'=ANY(target.allowed_methods))";
    let nested_digest = format!("sha256:{}", "a".repeat(64));
    let callable_before: bool = sqlx::query_scalar(executor_join)
        .bind(host)
        .bind(wf)
        .bind(tool)
        .bind(&nested_digest)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        callable_before,
        "the executor join finds the granted nested Tool before revocation"
    );
    let removed =
        json!({"hostId":host,"wfDefId":wf,"sourceRevision":3,"actor":"step03-test","grants":[]});
    let synced = publication_api::sync_grants_verified(&pool, &removed)
        .await
        .unwrap();
    assert_eq!(synced["result"], "synced");
    let stale = publication_api::sync_grants_verified(&pool, &prior)
        .await
        .unwrap();
    assert_eq!(stale["result"], "stale");
    assert_eq!(stale["appliedRevision"], 3);
    assert_eq!(stale["grantSetDigest"], synced["grantSetDigest"]);
    let live: bool = sqlx::query_scalar(executor_join)
        .bind(host)
        .bind(wf)
        .bind(tool)
        .bind(&nested_digest)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        !live,
        "the executor's live grant predicate must no longer find the grant"
    );
    let other_wf = Uuid::new_v4();
    save(
        &pool,
        &save_input(host, other_wf, 1, DEFINITION, owner, true),
    )
    .await;
    let hijack =
        publication_api::sync_grants_verified(&pool, &grant(host, other_wf, 1, grant_id, tool))
            .await
            .unwrap_err();
    assert_eq!(error_value(hijack).await["code"], "WORKFLOW_INPUT_INVALID");
    let grant_owner: Uuid = sqlx::query_scalar(
        "SELECT wf_def_id FROM workflow_tool_grant_t WHERE host_id=$1 AND grant_id=$2",
    )
    .bind(host)
    .bind(grant_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(grant_owner, wf);
    // Both definitions start with independent sync rows and race to insert
    // the same previously absent grant ID.
    let race_a = Uuid::new_v4();
    let race_b = Uuid::new_v4();
    save(&pool, &save_input(host, race_a, 1, DEFINITION, owner, true)).await;
    save(&pool, &save_input(host, race_b, 1, DEFINITION, owner, true)).await;
    let racing_id = Uuid::new_v4();
    let start = std::sync::Arc::new(tokio::sync::Barrier::new(3));
    let left = grant(host, race_a, 1, racing_id, Uuid::new_v4());
    let right = grant(host, race_b, 1, racing_id, Uuid::new_v4());
    let left_pool = pool.clone();
    let right_pool = pool.clone();
    let left_barrier = start.clone();
    let right_barrier = start.clone();
    // Hold both first INSERTs at the same table lock so the absent-key race
    // is exercised on every run, independent of task scheduling.
    let mut gate = admin.begin().await.unwrap();
    sqlx::query("LOCK TABLE workflow_ops.workflow_tool_grant_t IN SHARE MODE")
        .execute(&mut *gate)
        .await
        .unwrap();
    let left_task = tokio::spawn(async move {
        left_barrier.wait().await;
        publication_api::sync_grants_verified(&left_pool, &left).await
    });
    let right_task = tokio::spawn(async move {
        right_barrier.wait().await;
        publication_api::sync_grants_verified(&right_pool, &right).await
    });
    start.wait().await;
    let wait_result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let waiting: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND wait_event='relation'")
                .fetch_one(&admin).await.unwrap();
            if waiting >= 2 { break; }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await;
    if wait_result.is_err() {
        let states: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT state,wait_event_type,wait_event FROM pg_stat_activity WHERE datname=current_database() AND pid<>pg_backend_pid() ORDER BY pid")
            .fetch_all(&admin).await.unwrap();
        gate.rollback().await.unwrap();
        panic!("both grant sync inserts must reach the first-insert lock: {states:?}");
    }
    gate.commit().await.unwrap();
    let (left_result, right_result) = (left_task.await.unwrap(), right_task.await.unwrap());
    assert_eq!(left_result.is_ok() as u8 + right_result.is_ok() as u8, 1);
    let winner = if left_result.is_ok() { race_a } else { race_b };
    let loser = if winner == race_a { race_b } else { race_a };
    let stored_owner: Uuid = sqlx::query_scalar(
        "SELECT wf_def_id FROM workflow_tool_grant_t WHERE host_id=$1 AND grant_id=$2",
    )
    .bind(host)
    .bind(racing_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored_owner, winner);
    let losing_revision: Option<i64> = sqlx::query_scalar("SELECT source_revision FROM workflow_definition_grant_sync_t WHERE host_id=$1 AND wf_def_id=$2")
        .bind(host).bind(loser).fetch_optional(&pool).await.unwrap();
    assert!(losing_revision.is_none_or(|revision| revision == 0));
}

#[tokio::test]
#[ignore = "requires the isolated workflow-invoke scratch database"]
async fn delayed_save_and_equal_revision_conflicts_report_stored_pair() {
    let (pool, _admin) = pools();
    let (host, wf, old_owner, new_owner, grant_id, tool) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    let old = save_input(host, wf, 1, DEFINITION, old_owner, true);
    save(&pool, &old).await;
    let mut private_published = save_input(host, wf, 2, DEFINITION, old_owner, true);
    private_published["lifecycleStatus"] = json!("PUBLISHED");
    save(&pool, &private_published).await;
    let visible: bool = sqlx::query_scalar(
        "SELECT catalog_visible FROM wf_definition_t WHERE host_id=$1 AND wf_def_id=$2",
    )
    .bind(host)
    .bind(wf)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!visible, "published definitions can remain private");
    let mut visibility_update = private_published.clone();
    visibility_update["sourceRevision"] = json!(3);
    visibility_update["catalogVisible"] = json!(true);
    let visible_receipt = save(&pool, &visibility_update).await;
    assert_eq!(
        visible_receipt["definitionDigest"],
        save(&pool, &private_published).await["definitionDigest"]
    );
    let visibility_conflict = publication_api::save_definition_verified(
        &pool,
        &json!({
        "hostId":host,"wfDefId":wf,"sourceRevision":3,"actor":"step03-test",
        "namespace":"step03","name":format!("definition-{wf}"),"version":"1.0.0",
        "definition":DEFINITION,"lifecycleStatus":"PUBLISHED","catalogVisible":false,
        "owner":{"userId":old_owner},"active":true}),
    )
    .await
    .unwrap_err();
    assert_eq!(
        error_value(visibility_conflict).await["code"],
        "WORKFLOW_IDEMPOTENCY_CONFLICT"
    );
    let mut latest = save_input(host, wf, 4, NEXT_DEFINITION, new_owner, false);
    latest["catalogVisible"] = json!(true);
    let newer = save(&pool, &latest).await;
    let stale = save(&pool, &old).await;
    assert_eq!(stale["result"], "stale");
    assert_eq!(stale["appliedRevision"], 4);
    assert_eq!(stale["definitionDigest"], newer["definitionDigest"]);
    let row = sqlx::query("SELECT definition,owner_user_id,active,catalog_visible FROM wf_definition_t WHERE host_id=$1 AND wf_def_id=$2")
        .bind(host).bind(wf).fetch_one(&pool).await.unwrap();
    assert_eq!(
        row.try_get::<String, _>("definition").unwrap(),
        NEXT_DEFINITION
    );
    assert_eq!(row.try_get::<Uuid, _>("owner_user_id").unwrap(), new_owner);
    assert!(!row.try_get::<bool, _>("active").unwrap());
    assert!(row.try_get::<bool, _>("catalog_visible").unwrap());
    let save_conflict = publication_api::save_definition_verified(
        &pool,
        &save_input(host, wf, 4, DEFINITION, old_owner, true),
    )
    .await
    .unwrap_err();
    let details = error_value(save_conflict).await;
    assert_eq!(details["code"], "WORKFLOW_IDEMPOTENCY_CONFLICT");
    assert_eq!(details["details"]["appliedRevision"], 4);
    assert_eq!(
        details["details"]["definitionDigest"],
        newer["definitionDigest"]
    );
    let mut oversized = latest.clone();
    oversized["sourceRevision"] = json!(5);
    oversized["name"] = json!("n".repeat(127));
    let rejected = publication_api::save_definition_verified(&pool, &oversized)
        .await
        .unwrap_err();
    assert_eq!(
        error_value(rejected).await["code"],
        "WORKFLOW_INPUT_INVALID"
    );
    let applied: i64 = sqlx::query_scalar(
        "SELECT source_revision FROM wf_definition_t WHERE host_id=$1 AND wf_def_id=$2",
    )
    .bind(host)
    .bind(wf)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(applied, 4);
    let prior = grant(host, wf, 3, grant_id, tool);
    let granted = publication_api::sync_grants_verified(&pool, &prior)
        .await
        .unwrap();
    let grant_conflict = publication_api::sync_grants_verified(
        &pool,
        &json!({"hostId":host,"wfDefId":wf,"sourceRevision":3,"actor":"step03-test","grants":[]}),
    )
    .await
    .unwrap_err();
    let details = error_value(grant_conflict).await;
    assert_eq!(details["code"], "WORKFLOW_IDEMPOTENCY_CONFLICT");
    assert_eq!(details["details"]["appliedRevision"], 3);
    assert_eq!(
        details["details"]["grantSetDigest"],
        granted["grantSetDigest"]
    );
}
