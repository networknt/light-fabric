#[path = "support/ops_db.rs"]
mod ops_db;

use axum::{body::to_bytes, response::IntoResponse};
use light_workflow::{publication_api, rule_api};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use uuid::Uuid;

const READ: &str = "document: {dsl: '1.0.3', namespace: step04, name: read, version: '1.0.0'}\nevaluate: {language: cel}\ndo:\n  - lookup:\n      call: http\n      with:\n        method: GET\n        endpoint: {uri: 'https://example.invalid/read'}\n      metadata: {endpointRef: target-a}\n      end: true\n";
const WRITE: &str = "document: {dsl: '1.0.3', namespace: step04, name: write, version: '1.0.0'}\nevaluate: {language: cel}\ndo:\n  - update:\n      call: http\n      with:\n        method: POST\n        endpoint: {uri: 'https://example.invalid/write'}\n      metadata: {endpointRef: target-a, compensationTask: undoUpdate, approvalEvidenceDigest: 'sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'}\n      end: true\n";

fn pools() -> (PgPool, PgPool) {
    (ops_db::runtime_pool(), ops_db::admin_pool())
}
async fn api_error(error: rule_api::ApiError) -> Value {
    serde_json::from_slice(
        &to_bytes(error.into_response().into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap()
}
fn digest() -> String {
    format!("sha256:{}", "a".repeat(64))
}
async fn definition(
    pool: &PgPool,
    host: Uuid,
    wf: Uuid,
    owner: Uuid,
    text: &str,
) -> (String, String) {
    let saved = publication_api::save_definition_verified(
        pool,
        &json!({"hostId":host,"wfDefId":wf,
        "sourceRevision":1,"actor":"step04","namespace":"step04","name":format!("definition-{wf}"),
        "version":"1.0.0","definition":text,"lifecycleStatus":"PUBLISHED","catalogVisible":false,
        "owner":{"userId":owner},"active":true}),
    )
    .await
    .unwrap();
    let definition_digest = saved["definitionDigest"].as_str().unwrap().to_owned();
    let published = publication_api::publish_definition_verified(
        pool,
        &json!({"hostId":host,"wfDefId":wf,
        "version":"1.0.0","definition":text,"expectedDefinitionDigest":definition_digest,
        "operationId":Uuid::new_v4()}),
        &owner.to_string(),
        &[],
        &|_| Ok(()),
    )
    .await
    .unwrap();
    (
        definition_digest,
        published["schemaDigest"].as_str().unwrap().to_owned(),
    )
}
fn input(
    host: Uuid,
    wf: Uuid,
    tool: Uuid,
    source: Uuid,
    definition_digest: &str,
    schema_digest: &str,
    read_only: bool,
    replay: u64,
    expected: i64,
    uri: &str,
) -> Value {
    json!({"hostId":host,"binding":{"sourceBindingId":source,"toolId":tool,"toolName":format!("tool-{tool}"),
        "wfDefId":wf,"workflowVersion":"1.0.0","definitionDigest":definition_digest,
        "schemaDigest":schema_digest,"invocationMode":"sync","syncWaitMs":1000,"totalDeadlineMs":30000,
        "executionClass":"interactive","resultTextMode":"compact-json",
        "cancellationPolicy":"before-effects-only",
        "idempotencyPolicy":{"kind":"derived","resultReplayMs":replay},
        "delegationPolicy":{"maximumDelegationDepth":1},
        "runtimeBounds":{"maximumTaskAttempts":8,"maximumNestedCalls":8,"maximumParallelism":1,
            "maximumRequestBytes":1048576,"maximumIntermediateBytes":4194304,"maximumResultBytes":1048576,"maximumCostUnits":1000},
        "admissionLimits":{"maximumConcurrentRuns":20,"maximumConcurrentRunsPerUser":2,
            "startsPerMinute":120,"startsPerMinutePerUser":10},"callerPolicy":{},
        "toolAnnotations":{"readOnly":read_only,"destructive":false},
        "policyDigest":digest(),"responsePolicyDigest":digest()},
        "dependencies":[],"endpointTargets":[{"endpointRef":"target-a","endpointUri":uri,
            "allowedMethods":[if read_only {"GET"}else{"POST"}],"authorizationPolicyDigest":digest()}],
        "expectedAggregateVersion":expected,"operationId":Uuid::new_v4()})
}
fn with_dependency(mut input: Value, target: &str) -> Value {
    input["dependencies"] = json!([{"nestedToolId":Uuid::new_v4(),"nestedToolVersion":"1.0.0",
        "contractDigest":digest(),"compatibilityPolicy":"exact","authorizationToolName":"nested",
        "authorizationEndpointKey":"nested@call","authorizationPolicyDigest":digest(),
        "lifecycleStatus":"active","dispatchTarget":{"target":target,"readOnly":true}}]);
    input
}
async fn publish(pool: &PgPool, input: &Value, actor: Uuid) -> Value {
    publication_api::publish_binding_verified(pool, input, &actor.to_string(), &[], 16)
        .await
        .unwrap()
}
async fn fixture(pool: &PgPool, text: &str) -> (Uuid, Uuid, Uuid, Uuid, String, String) {
    let (host, wf, owner, tool) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    let (def, schema) = definition(pool, host, wf, owner, text).await;
    (host, wf, owner, tool, def, schema)
}
async fn pin_run(
    pool: &PgPool,
    host: Uuid,
    wf: Uuid,
    tool: Uuid,
    binding: Uuid,
    definition_digest: &str,
    schema_digest: &str,
) -> Uuid {
    let process = Uuid::new_v4();
    let run = Uuid::new_v4();
    sqlx::query("INSERT INTO process_info_t(host_id,process_id,wf_def_id,wf_instance_id,app_id,process_type,status_code,ex_trigger_ts) VALUES($1,$2,$3,$4,'step04','Workflow','A',CURRENT_TIMESTAMP)")
        .bind(host).bind(process).bind(wf).bind(run.to_string()).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO workflow_invocation_t(host_id,workflow_instance_id,binding_id,process_id,stable_tool_ref,wf_def_id,workflow_version,definition_digest,schema_digest,policy_digest,response_policy_digest,principal_subject,end_user_subject,input,input_digest,canonical_input_profile,invocation_mode,execution_class,state,correlation_id,deadline_ts) VALUES($1,$2,$3,$4,$5,$6,'1.0.0',$7,$8,$9,$9,'step04','step04','{}'::jsonb,$9,'rfc8785-safe-json-v1','sync','interactive','RUNNING','step04',CURRENT_TIMESTAMP + INTERVAL '30 seconds')")
        .bind(host).bind(run).bind(binding).bind(process).bind(tool).bind(wf)
        .bind(definition_digest).bind(schema_digest).bind(digest()).execute(pool).await.unwrap();
    process
}

#[tokio::test]
#[ignore = "requires the isolated workflow-invoke scratch database"]
async fn immutable_invalid_requests_are_fenced_and_replayed() {
    let (pool, _admin) = pools();
    let (host, wf, owner, tool, def, schema) = fixture(&pool, READ).await;
    let mut request = input(host, wf, tool, Uuid::new_v4(), &def, &schema, true,
        600_000, -1, "https://example.invalid/read");
    let publish_operation = request["operationId"].as_str().unwrap().to_owned();
    for _ in 0..2 {
        let error = publication_api::publish_binding_verified(&pool, &request,
            &owner.to_string(), &[], 16).await.unwrap_err();
        let body = api_error(error).await;
        assert_eq!(body["code"], "WORKFLOW_INPUT_INVALID");
        assert_eq!(body["afterEffect"], false);
        assert_eq!(body["details"]["requestValidation"]["operationId"], publish_operation);
        assert_eq!(body["details"]["requestValidation"]["toolName"], "workflow_binding_publish");
    }
    let stored: Value = sqlx::query_scalar("SELECT receipt FROM workflow_publication_operation_t WHERE host_id=$1 AND operation_id=$2")
        .bind(host).bind(Uuid::parse_str(&publish_operation).unwrap()).fetch_one(&pool).await.unwrap();
    assert!(stored.get("_requestRejection").is_some());
    let mut legacy = stored.clone();
    legacy["_requestRejection"].as_object_mut().unwrap().remove("evidence");
    legacy["_requestRejection"].as_object_mut().unwrap().remove("code");
    sqlx::query("UPDATE workflow_publication_operation_t SET receipt=$3 WHERE host_id=$1 AND operation_id=$2")
        .bind(host).bind(Uuid::parse_str(&publish_operation).unwrap()).bind(&legacy)
        .execute(&pool).await.unwrap();
    let legacy_replay = api_error(publication_api::publish_binding_verified(&pool, &request,
        &owner.to_string(), &[], 16).await.unwrap_err()).await;
    assert_eq!(legacy_replay["details"]["requestValidation"]["version"], 1);
    assert_eq!(legacy_replay["details"]["requestValidation"]["discriminator"],
        "negativeExpectedAggregateVersion");
    // Fixture for a success committed under an older validator: replay must win
    // before the current negative-version rule runs.
    let old_success = json!({"result":"published","status":"active","operationId":publish_operation});
    sqlx::query("UPDATE workflow_publication_operation_t SET receipt=$3 WHERE host_id=$1 AND operation_id=$2")
        .bind(host).bind(Uuid::parse_str(&publish_operation).unwrap()).bind(&old_success)
        .execute(&pool).await.unwrap();
    assert_eq!(publication_api::publish_binding_verified(&pool, &request,
        &owner.to_string(), &[], 16).await.unwrap(), old_success);
    request["expectedAggregateVersion"] = json!(0);
    request["operationId"] = json!(Uuid::new_v4());
    let success = publish(&pool, &request, owner).await;
    assert_eq!(success["result"], "published");
    sqlx::query("UPDATE wf_definition_version_t SET version_status='retired',retired_ts=CURRENT_TIMESTAMP WHERE host_id=$1 AND wf_def_id=$2")
        .bind(host).bind(wf).execute(&pool).await.unwrap();
    assert_eq!(publish(&pool, &request, owner).await, success,
        "committed success must replay before the changed version state is checked");

    let mut concurrent = request.clone();
    concurrent["expectedAggregateVersion"] = json!(-1);
    concurrent["operationId"] = json!(Uuid::new_v4());
    let actor = owner.to_string();
    let (left, right) = tokio::join!(
        publication_api::publish_binding_verified(&pool, &concurrent, &actor, &[], 16),
        publication_api::publish_binding_verified(&pool, &concurrent, &actor, &[], 16));
    assert_eq!(api_error(left.unwrap_err()).await["code"], "WORKFLOW_INPUT_INVALID");
    assert_eq!(api_error(right.unwrap_err()).await["code"], "WORKFLOW_INPUT_INVALID");

    let retire_operation = Uuid::new_v4();
    let retire = json!({"hostId":host,"toolId":tool,"expectedAggregateVersion":-1,
        "operationId":retire_operation});
    for _ in 0..2 {
        let error = publication_api::retire_binding_verified(&pool, &retire,
            &owner.to_string(), &[]).await.unwrap_err();
        let body = api_error(error).await;
        assert_eq!(body["code"], "WORKFLOW_INPUT_INVALID");
        assert_eq!(body["details"]["requestValidation"]["operationId"], retire_operation.to_string());
        assert_eq!(body["details"]["requestValidation"]["toolName"], "workflow_binding_retire");
    }
}

#[tokio::test]
#[ignore = "requires the isolated workflow-invoke scratch database"]
async fn nonnegative_binding_field_and_reach_rejections_are_durable() {
    let (pool, _admin) = pools();
    let (host, wf, owner, tool, def, schema) = fixture(&pool, READ).await;
    let base = input(host, wf, tool, Uuid::new_v4(), &def, &schema, true,
        600_000, 0, "https://example.invalid/read");
    for (discriminator, section, field, bad) in [
        ("bindingFields", "binding", "/binding/toolName", json!(" ")),
        ("bindingFields", "binding", "/binding/policyDigest", json!("bad")),
        ("bindingReach", "reach", "/endpointTargets/0/endpointUri", json!("file:///tmp/read")),
        ("bindingReach", "reach", "/endpointTargets/0/allowedMethods", json!(["TRACE"])),
    ] {
        let mut request = base.clone();
        *request.pointer_mut(field).unwrap() = bad;
        request["operationId"] = json!(Uuid::new_v4());
        let actor = owner.to_string();
        let (left, right) = tokio::join!(
            publication_api::publish_binding_verified(&pool, &request, &actor, &[], 16),
            publication_api::publish_binding_verified(&pool, &request, &actor, &[], 16));
        let first = api_error(left.unwrap_err()).await;
        let second = api_error(right.unwrap_err()).await;
        assert_eq!(first["code"], "WORKFLOW_INPUT_INVALID");
        assert_eq!(second["message"], first["message"]);
        assert_eq!(second["details"], first["details"]);
        assert_eq!(first["details"]["requestValidation"]["version"], 2);
        assert_eq!(first["details"]["requestValidation"]["discriminator"], discriminator);
        assert_eq!(first["details"]["requestValidation"]["requestSection"], section);
        let operation = Uuid::parse_str(request["operationId"].as_str().unwrap()).unwrap();
        let stored: Value = sqlx::query_scalar("SELECT receipt FROM workflow_publication_operation_t WHERE host_id=$1 AND operation_id=$2")
            .bind(host).bind(operation).fetch_one(&pool).await.unwrap();
        assert_eq!(stored["_requestRejection"]["message"], first["message"]);
        assert_eq!(stored["_requestRejection"]["evidence"], first["details"]["requestValidation"]);
        if field == "/binding/toolName" {
            // Simulate a rejection committed by an older validator. The current
            // validation message must not replace its stored outcome.
            let mut older = stored.clone();
            older["_requestRejection"]["message"] = json!("older validator rejection");
            sqlx::query("UPDATE workflow_publication_operation_t SET receipt=$3 WHERE host_id=$1 AND operation_id=$2")
                .bind(host).bind(operation).bind(&older).execute(&pool).await.unwrap();
            let replay = api_error(publication_api::publish_binding_verified(&pool, &request,
                &actor, &[], 16).await.unwrap_err()).await;
            assert_eq!(replay["message"], "older validator rejection");
            assert_eq!(replay["details"], first["details"]);
        }
        let revision_count: i64 = sqlx::query_scalar("SELECT count(*) FROM workflow_tool_binding_t WHERE host_id=$1 AND tool_id=$2")
            .bind(host).bind(tool).fetch_one(&pool).await.unwrap();
        assert_eq!(revision_count, 0);
        let projection_count: i64 = sqlx::query_scalar("SELECT count(*) FROM workflow_tool_publication_t WHERE host_id=$1 AND tool_id=$2")
            .bind(host).bind(tool).fetch_one(&pool).await.unwrap();
        assert_eq!(projection_count, 0);
        let decision_count: i64 = sqlx::query_scalar("SELECT count(*) FROM workflow_tool_binding_decision_t WHERE host_id=$1 AND tool_id=$2")
            .bind(host).bind(tool).fetch_one(&pool).await.unwrap();
        assert_eq!(decision_count, 0);
    }
    // A corrected request is new work with a new operation ID.
    assert_eq!(publish(&pool, &base, owner).await["result"], "published");
}

#[tokio::test]
#[ignore = "requires the isolated workflow-invoke scratch database"]
async fn owner_publication_creates_active_revision_and_write_evidence() {
    let (pool, _admin) = pools();
    let (host, wf, owner, tool, def, schema) = fixture(&pool, WRITE).await;
    let request = input(
        host,
        wf,
        tool,
        Uuid::new_v4(),
        &def,
        &schema,
        false,
        600_000,
        0,
        "https://example.invalid/write",
    );
    let receipt = publish(&pool, &request, owner).await;
    assert_eq!(receipt["result"], "published");
    assert_eq!(receipt["status"], "active");
    assert_eq!(receipt["aggregateVersion"], 1);
    let mut same = request.clone();
    same["operationId"] = json!(Uuid::new_v4());
    same["expectedAggregateVersion"] = json!(1);
    same["binding"]["sourceBindingId"] = json!(Uuid::new_v4());
    let unchanged = publish(&pool, &same, owner).await;
    assert_eq!(unchanged["result"], "unchanged");
    assert_eq!(unchanged["status"], "active");
    assert_eq!(unchanged["bindingId"], receipt["bindingId"]);
    assert_eq!(unchanged["aggregateVersion"], 1);
    let binding = Uuid::parse_str(receipt["bindingId"].as_str().unwrap()).unwrap();
    let row=sqlx::query("SELECT active,revision_status,source_binding_id,owner_user_id FROM workflow_tool_binding_t WHERE host_id=$1 AND binding_id=$2")
        .bind(host).bind(binding).fetch_one(&pool).await.unwrap();
    assert!(row.try_get::<bool, _>("active").unwrap());
    assert_eq!(
        row.try_get::<String, _>("revision_status").unwrap(),
        "approved"
    );
    assert_eq!(row.try_get::<Uuid, _>("owner_user_id").unwrap(), owner);
    assert_eq!(
        row.try_get::<Uuid, _>("source_binding_id")
            .unwrap()
            .to_string(),
        request["binding"]["sourceBindingId"]
    );
    let evidence:(String,String)=sqlx::query_as("SELECT task_name,evidence_digest FROM workflow_tool_approval_evidence_t WHERE host_id=$1 AND binding_id=$2")
        .bind(host).bind(binding).fetch_one(&pool).await.unwrap();
    assert_eq!(evidence.0, "update");
    assert_eq!(evidence.1, digest());
    let decisions:i64=sqlx::query_scalar("SELECT count(*) FROM workflow_tool_binding_decision_t WHERE host_id=$1 AND binding_id=$2 AND action='selfApprove'")
        .bind(host).bind(binding).fetch_one(&pool).await.unwrap();
    assert_eq!(decisions, 1);
}

#[tokio::test]
#[ignore = "requires the isolated workflow-invoke scratch database"]
async fn non_owner_changed_deadline_is_pending_while_active_admits() {
    let (pool, _admin) = pools();
    let (host, wf, owner, tool, def, schema) = fixture(&pool, READ).await;
    let first = input(
        host,
        wf,
        tool,
        Uuid::new_v4(),
        &def,
        &schema,
        true,
        0,
        0,
        "https://example.invalid/a",
    );
    let active = publish(&pool, &first, owner).await;
    let mut next = input(
        host,
        wf,
        tool,
        Uuid::new_v4(),
        &def,
        &schema,
        true,
        0,
        1,
        "https://example.invalid/a",
    );
    next["binding"]["totalDeadlineMs"] = json!(29_000);
    let pending = publish(&pool, &next, Uuid::new_v4()).await;
    assert_eq!(pending["status"], "pendingApproval");
    assert_eq!(pending["aggregateVersion"], 2);
    let mut same_pending = next.clone();
    same_pending["operationId"] = json!(Uuid::new_v4());
    same_pending["expectedAggregateVersion"] = json!(2);
    let unchanged = publish(&pool, &same_pending, Uuid::new_v4()).await;
    assert_eq!(unchanged["result"], "unchanged");
    assert_eq!(unchanged["status"], "pendingApproval");
    assert_eq!(unchanged["bindingId"], pending["bindingId"]);
    assert_eq!(unchanged["aggregateVersion"], 2);
    let admitted = rule_api::read_admissible_pinned_binding(&pool, host, tool)
        .await
        .unwrap();
    assert_eq!(
        admitted
            .try_get::<Uuid, _>("binding_id")
            .unwrap()
            .to_string(),
        active["bindingId"]
    );
    let head=sqlx::query("SELECT active_binding_id,pending_binding_id FROM workflow_tool_publication_t WHERE host_id=$1 AND tool_id=$2")
        .bind(host).bind(tool).fetch_one(&pool).await.unwrap();
    assert_eq!(
        head.try_get::<Uuid, _>("active_binding_id")
            .unwrap()
            .to_string(),
        active["bindingId"]
    );
    assert_eq!(
        head.try_get::<Uuid, _>("pending_binding_id")
            .unwrap()
            .to_string(),
        pending["bindingId"]
    );
}

#[tokio::test]
#[ignore = "requires the isolated workflow-invoke scratch database"]
async fn pinned_run_keeps_a_targets_and_dependencies_after_b_activates() {
    let (pool, _admin) = pools();
    let (host, wf, owner, tool, def, schema) = fixture(&pool, READ).await;
    let a = publish(
        &pool,
        &with_dependency(
            input(
                host,
                wf,
                tool,
                Uuid::new_v4(),
                &def,
                &schema,
                true,
                0,
                0,
                "https://example.invalid/a",
            ),
            "a",
        ),
        owner,
    )
    .await;
    let b = publish(
        &pool,
        &with_dependency(
            input(
                host,
                wf,
                tool,
                Uuid::new_v4(),
                &def,
                &schema,
                true,
                0,
                1,
                "https://example.invalid/b",
            ),
            "b",
        ),
        owner,
    )
    .await;
    let a_id: Uuid = a["bindingId"].as_str().unwrap().parse().unwrap();
    let process = pin_run(&pool, host, wf, tool, a_id, &def, &schema).await;
    sqlx::query("INSERT INTO workflow_tool_grant_t(host_id,grant_id,tool_id,wf_def_id,tool_version,lightapi_digest,allowed_environments) VALUES($1,$2,$3,$4,'1.0.0',$5,ARRAY['test'])")
        .bind(host).bind(Uuid::new_v4()).bind(tool).bind(wf).bind(digest())
        .execute(&pool).await.unwrap();
    assert_ne!(a["bindingId"], b["bindingId"]);
    let old: bool = sqlx::query_scalar(
        "SELECT active FROM workflow_tool_binding_t WHERE host_id=$1 AND binding_id=$2",
    )
    .bind(host)
    .bind(a_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!old);
    let target =
        light_workflow::executor::pinned_endpoint_uri(&pool, host, a_id, "target-a", "GET")
            .await
            .unwrap();
    assert_eq!(target.as_deref(), Some("https://example.invalid/a"));
    let pinned = light_workflow::executor::resolve_granted_endpoint(
        &pool,
        host,
        wf,
        tool,
        "1.0.0",
        &digest(),
        "test",
        process,
        "target-a",
        "GET",
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(pinned.2, "https://example.invalid/a");
    let unpinned_process = Uuid::new_v4();
    let unpinned = light_workflow::executor::resolve_granted_endpoint(
        &pool,
        host,
        wf,
        tool,
        "1.0.0",
        &digest(),
        "test",
        unpinned_process,
        "target-a",
        "GET",
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(unpinned.2, "https://example.invalid/b");
    sqlx::query(
        "UPDATE workflow_endpoint_target_t SET active=false WHERE host_id=$1 AND binding_id=$2",
    )
    .bind(host)
    .bind(a_id)
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        light_workflow::executor::resolve_granted_endpoint(
            &pool,
            host,
            wf,
            tool,
            "1.0.0",
            &digest(),
            "test",
            process,
            "target-a",
            "GET",
        )
        .await
        .unwrap()
        .unwrap()
        .2,
        "https://example.invalid/a"
    );
    assert_eq!(
        light_workflow::executor::resolve_granted_endpoint(
            &pool,
            host,
            wf,
            tool,
            "1.0.0",
            &digest(),
            "test",
            unpinned_process,
            "target-a",
            "GET",
        )
        .await
        .unwrap()
        .unwrap()
        .2,
        "https://example.invalid/b"
    );
    let b_id: Uuid = b["bindingId"].as_str().unwrap().parse().unwrap();
    sqlx::query(
        "UPDATE workflow_endpoint_target_t SET active=false WHERE host_id=$1 AND binding_id=$2",
    )
    .bind(host)
    .bind(b_id)
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        light_workflow::executor::resolve_granted_endpoint(
            &pool,
            host,
            wf,
            tool,
            "1.0.0",
            &digest(),
            "test",
            unpinned_process,
            "target-a",
            "GET",
        )
        .await
        .unwrap()
        .is_none()
    );
    assert_eq!(
        light_workflow::executor::pinned_endpoint_for_process(
            &pool, host, process, "target-a", "GET"
        )
        .await
        .unwrap()
        .as_deref(),
        Some("https://example.invalid/a")
    );
    let dependencies = publication_api::pinned_dependencies(&pool, host, a_id)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&dependencies[0]).unwrap()["dispatchTarget"]["target"],
        "a"
    );
}

#[tokio::test]
#[ignore = "requires the isolated workflow-invoke scratch database"]
async fn superseded_revision_keeps_pinned_child_rows() {
    let (pool, _admin) = pools();
    let (host, wf, owner, tool, def, schema) = fixture(&pool, READ).await;
    let a = publish(
        &pool,
        &with_dependency(
            input(
                host,
                wf,
                tool,
                Uuid::new_v4(),
                &def,
                &schema,
                true,
                0,
                0,
                "https://example.invalid/a",
            ),
            "a",
        ),
        owner,
    )
    .await;
    let b = publish(
        &pool,
        &with_dependency(
            input(
                host,
                wf,
                tool,
                Uuid::new_v4(),
                &def,
                &schema,
                true,
                0,
                1,
                "https://example.invalid/b",
            ),
            "b",
        ),
        owner,
    )
    .await;
    let a_id: Uuid = a["bindingId"].as_str().unwrap().parse().unwrap();
    let process = pin_run(&pool, host, wf, tool, a_id, &def, &schema).await;
    let status: String = sqlx::query_scalar(
        "SELECT revision_status FROM workflow_tool_binding_t WHERE host_id=$1 AND binding_id=$2",
    )
    .bind(host)
    .bind(a_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "superseded");
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM workflow_endpoint_target_t WHERE host_id=$1 AND binding_id=$2",
    )
    .bind(host)
    .bind(a_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(rows, 1);
    sqlx::query(
        "UPDATE workflow_endpoint_target_t SET active=false WHERE host_id=$1 AND binding_id=$2",
    )
    .bind(host)
    .bind(a_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE workflow_tool_dependency_t SET active=false WHERE host_id=$1 AND outer_binding_id=$2")
        .bind(host).bind(a_id).execute(&pool).await.unwrap();
    assert_eq!(
        light_workflow::executor::pinned_endpoint_for_process(
            &pool, host, process, "target-a", "GET"
        )
        .await
        .unwrap()
        .as_deref(),
        Some("https://example.invalid/a")
    );
    let dependencies = publication_api::pinned_dependencies(&pool, host, a_id)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&dependencies[0]).unwrap()["dispatchTarget"]["target"],
        "a"
    );
    assert_ne!(a["bindingId"], b["bindingId"]);
}

#[tokio::test]
#[ignore = "requires the isolated workflow-invoke scratch database"]
async fn read_only_tool_with_http_post_is_rejected() {
    let (pool, _admin) = pools();
    let (host, wf, owner, tool, def, schema) = fixture(&pool, WRITE).await;
    let request = input(
        host,
        wf,
        tool,
        Uuid::new_v4(),
        &def,
        &schema,
        true,
        600_000,
        0,
        "https://example.invalid/write",
    );
    let error =
        publication_api::publish_binding_verified(&pool, &request, &owner.to_string(), &[], 16)
            .await
            .unwrap_err();
    assert_eq!(api_error(error).await["code"], "WORKFLOW_INPUT_INVALID");
    let revisions: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM workflow_tool_binding_t WHERE host_id=$1 AND tool_id=$2",
    )
    .bind(host)
    .bind(tool)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(revisions, 0);
}

#[tokio::test]
#[ignore = "requires the isolated workflow-invoke scratch database"]
async fn write_tool_replay_window_and_evidence_admission() {
    let (pool, _admin) = pools();
    let (host, wf, owner, tool, def, schema) = fixture(&pool, WRITE).await;
    let bad = input(
        host,
        wf,
        tool,
        Uuid::new_v4(),
        &def,
        &schema,
        false,
        0,
        0,
        "https://example.invalid/write",
    );
    let denied =
        publication_api::publish_binding_verified(&pool, &bad, &owner.to_string(), &[], 16)
            .await
            .unwrap_err();
    assert_eq!(api_error(denied).await["code"], "WORKFLOW_INPUT_INVALID");
    let good = input(
        host,
        wf,
        tool,
        Uuid::new_v4(),
        &def,
        &schema,
        false,
        600_000,
        0,
        "https://example.invalid/write",
    );
    let receipt = publish(&pool, &good, owner).await;
    assert_eq!(receipt["status"], "active");
    let id: Uuid = receipt["bindingId"].as_str().unwrap().parse().unwrap();
    let evidence:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workflow_tool_approval_evidence_t WHERE host_id=$1 AND binding_id=$2 AND task_name='update' AND evidence_digest=$3 AND active)")
        .bind(host).bind(id).bind(digest()).fetch_one(&pool).await.unwrap();
    assert!(evidence);
    let definition = serde_yaml::from_str(WRITE).unwrap();
    publication_api::pinned_evidence(&pool, host, id, &definition, 1)
        .await
        .unwrap();
    sqlx::query("UPDATE workflow_tool_approval_evidence_t SET active=false WHERE host_id=$1 AND binding_id=$2")
        .bind(host).bind(id).execute(&pool).await.unwrap();
    assert!(
        publication_api::pinned_evidence(&pool, host, id, &definition, 1)
            .await
            .is_err()
    );
    for kind in ["derived", "explicit", "business"] {
        let (host, wf, owner, tool, def, schema) = fixture(&pool, READ).await;
        let mut declared_write = input(
            host,
            wf,
            tool,
            Uuid::new_v4(),
            &def,
            &schema,
            false,
            0,
            0,
            "https://example.invalid/read",
        );
        declared_write["binding"]["idempotencyPolicy"]["kind"] = json!(kind);
        declared_write["endpointTargets"][0]["allowedMethods"] = json!(["GET"]);
        let denied = publication_api::publish_binding_verified(
            &pool,
            &declared_write,
            &owner.to_string(),
            &[],
            16,
        )
        .await
        .unwrap_err();
        assert_eq!(
            api_error(denied).await["code"],
            "WORKFLOW_INPUT_INVALID",
            "{kind}"
        );
        declared_write["binding"]["idempotencyPolicy"]["resultReplayMs"] = json!(600_000);
        declared_write["operationId"] = json!(Uuid::new_v4());
        let approved = publish(&pool, &declared_write, owner).await;
        let binding: Uuid = approved["bindingId"].as_str().unwrap().parse().unwrap();
        let read_definition = serde_yaml::from_str(READ).unwrap();
        publication_api::pinned_evidence(&pool, host, binding, &read_definition, 1)
            .await
            .unwrap();
        sqlx::query("UPDATE workflow_tool_binding_t SET idempotency_policy=jsonb_set(idempotency_policy,'{resultReplayMs}','0'::jsonb) WHERE host_id=$1 AND binding_id=$2")
            .bind(host).bind(binding).execute(&pool).await.unwrap();
        assert!(
            publication_api::pinned_evidence(&pool, host, binding, &read_definition, 1)
                .await
                .is_err(),
            "{kind}"
        );
    }
}

#[tokio::test]
#[ignore = "requires the isolated workflow-invoke scratch database"]
async fn stale_expected_aggregate_version_conflicts_without_revision() {
    let (pool, _admin) = pools();
    let (host, wf, owner, tool, def, schema) = fixture(&pool, READ).await;
    let original = input(
            host,
            wf,
            tool,
            Uuid::new_v4(),
            &def,
            &schema,
            true,
            0,
            0,
            "https://example.invalid/a",
        );
    let first = publish(&pool, &original, owner).await;
    let stale = input(
        host,
        wf,
        tool,
        Uuid::new_v4(),
        &def,
        &schema,
        true,
        0,
        0,
        "https://example.invalid/b",
    );
    let error =
        publication_api::publish_binding_verified(&pool, &stale, &owner.to_string(), &[], 16)
            .await
            .unwrap_err();
    let error = api_error(error).await;
    assert_eq!(error["code"], "VERSION_CONFLICT");
    assert_eq!(error["retryable"], true);
    assert_eq!(error["details"]["expectedAggregateVersion"], 0);
    assert_eq!(error["details"]["aggregateVersion"], 1);
    // The stored receipt wins over the now-stale expected version for the same operation.
    let replay = publication_api::publish_binding_verified(
        &pool, &original, &owner.to_string(), &[], 16,
    )
    .await
    .unwrap();
    assert_eq!(replay, first);
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM workflow_tool_binding_t WHERE host_id=$1 AND tool_id=$2",
    )
    .bind(host)
    .bind(tool)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
#[ignore = "requires the isolated workflow-invoke scratch database"]
async fn retirement_updates_revisions_decisions_head_and_version() {
    let (pool, _admin) = pools();
    let (host, wf, owner, tool, def, schema) = fixture(&pool, READ).await;
    let active = publish(
        &pool,
        &input(
            host,
            wf,
            tool,
            Uuid::new_v4(),
            &def,
            &schema,
            true,
            0,
            0,
            "https://example.invalid/a",
        ),
        owner,
    )
    .await;
    let pending = publish(
        &pool,
        &input(
            host,
            wf,
            tool,
            Uuid::new_v4(),
            &def,
            &schema,
            true,
            0,
            1,
            "https://example.invalid/b",
        ),
        Uuid::new_v4(),
    )
    .await;
    let request = json!({"hostId":host,"toolId":tool,"expectedAggregateVersion":2,"operationId":Uuid::new_v4()});
    let retired =
        publication_api::retire_binding_verified(&pool, &request, &owner.to_string(), &[])
            .await
            .unwrap();
    assert_eq!(retired["result"], "retired");
    assert_eq!(retired["aggregateVersion"], 3);
    let rows=sqlx::query("SELECT binding_id,revision_status,active FROM workflow_tool_binding_t WHERE host_id=$1 AND tool_id=$2 ORDER BY requested_ts")
        .bind(host).bind(tool).fetch_all(&pool).await.unwrap();
    assert_eq!(rows.len(), 2);
    let states: Vec<String> = rows
        .iter()
        .map(|r| r.try_get("revision_status").unwrap())
        .collect();
    assert!(states.contains(&"retired".to_string()) && states.contains(&"withdrawn".to_string()));
    assert!(
        rows.iter()
            .all(|r| !r.try_get::<bool, _>("active").unwrap())
    );
    let head=sqlx::query("SELECT aggregate_version,active_binding_id,pending_binding_id FROM workflow_tool_publication_t WHERE host_id=$1 AND tool_id=$2")
        .bind(host).bind(tool).fetch_one(&pool).await.unwrap();
    assert_eq!(head.try_get::<i64, _>("aggregate_version").unwrap(), 3);
    assert_eq!(
        head.try_get::<Option<Uuid>, _>("active_binding_id")
            .unwrap(),
        None
    );
    assert_eq!(
        head.try_get::<Option<Uuid>, _>("pending_binding_id")
            .unwrap(),
        None
    );
    let actions:Vec<String>=sqlx::query_scalar("SELECT action FROM workflow_tool_binding_decision_t WHERE host_id=$1 AND tool_id=$2 ORDER BY action")
        .bind(host).bind(tool).fetch_all(&pool).await.unwrap();
    assert!(actions.contains(&"retire".into()) && actions.contains(&"withdraw".into()));
    assert_ne!(active["bindingId"], pending["bindingId"]);
    let replay = publication_api::retire_binding_verified(&pool, &request, &owner.to_string(), &[])
        .await
        .unwrap();
    assert_eq!(replay, retired);
    let next = json!({"hostId":host,"toolId":tool,"expectedAggregateVersion":3,
        "operationId":Uuid::new_v4()});
    let unchanged = publication_api::retire_binding_verified(&pool, &next, &owner.to_string(), &[])
        .await
        .unwrap();
    assert_eq!(unchanged["result"], "unchanged");
    assert_eq!(unchanged["aggregateVersion"], 3);
    assert_eq!(unchanged["bindingId"], pending["bindingId"]);
    assert_eq!(unchanged["revisionStatus"], "withdrawn");
    let pending_id: Uuid = pending["bindingId"].as_str().unwrap().parse().unwrap();
    let stored:(Uuid,String,chrono::DateTime<chrono::Utc>)=sqlx::query_as(
        "SELECT decision_id,actor,decided_ts FROM workflow_tool_binding_decision_t WHERE host_id=$1 AND binding_id=$2 AND action='withdraw'")
        .bind(host).bind(pending_id).fetch_one(&pool).await.unwrap();
    assert_eq!(unchanged["decisionId"], json!(stored.0));
    assert_eq!(unchanged["decidedBy"], stored.1);
    assert_eq!(unchanged["decidedTs"], json!(stored.2));
}
