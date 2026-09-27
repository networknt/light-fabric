#[path = "support/ops_db.rs"]
mod ops_db;

use axum::{body::to_bytes, response::IntoResponse};
use light_workflow::{executor::TaskExecutor, publication_api, rule_api};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use std::sync::Arc;
use tokio::sync::Barrier;
use uuid::Uuid;

const READ: &str = "document: {dsl: '1.0.3', namespace: step05, name: read, version: '1.0.0'}\nevaluate: {language: cel}\ndo:\n  - lookup:\n      call: http\n      with:\n        method: GET\n        endpoint: {uri: 'https://example.invalid/read'}\n      metadata: {endpointRef: target-a}\n      end: true\n";
const WRITE: &str = "document: {dsl: '1.0.3', namespace: step05, name: write, version: '1.0.0'}\nevaluate: {language: cel}\ndo:\n  - update:\n      call: http\n      with:\n        method: POST\n        endpoint: {uri: 'https://example.invalid/write'}\n      metadata: {endpointRef: target-a, compensationTask: undoUpdate, approvalEvidenceDigest: 'sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'}\n      end: true\n";

fn pools() -> (PgPool, PgPool) {
    (ops_db::runtime_pool(), ops_db::admin_pool())
}
fn digest() -> String {
    format!("sha256:{}", "a".repeat(64))
}
async fn error_value(error: rule_api::ApiError) -> Value {
    serde_json::from_slice(
        &to_bytes(error.into_response().into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap()
}
fn validate(kind: &str, value: &Value) {
    let schemas: Value =
        serde_json::from_str(include_str!("../contracts/workflow-admin/schemas.json")).unwrap();
    let root = json!({"$ref":format!("#/$defs/{kind}"),"$defs":schemas["$defs"]});
    let validator = jsonschema::validator_for(&root).unwrap();
    let errors: Vec<_> = validator
        .iter_errors(value)
        .map(|error| error.to_string())
        .collect();
    assert!(errors.is_empty(), "{kind}: {errors:?}");
}
async fn definition(
    pool: &PgPool,
    host: Uuid,
    wf: Uuid,
    owner: Uuid,
    text: &str,
    version: &str,
    revision: i64,
    approval: &str,
) -> (String, String) {
    let saved = publication_api::save_definition_verified(pool,&json!({"hostId":host,"wfDefId":wf,
        "sourceRevision":revision,"actor":"step05","namespace":"step05","name":format!("definition-{wf}"),
        "version":version,"definition":text,"lifecycleStatus":"PUBLISHED","catalogVisible":false,
        "owner":{"userId":owner},"active":true})).await.unwrap();
    let def = saved["definitionDigest"].as_str().unwrap().to_owned();
    let published = publication_api::publish_definition_verified(
        pool,
        &json!({"hostId":host,"wfDefId":wf,
        "version":version,"definition":text,"expectedDefinitionDigest":def,
        "bindingApproval":approval,"operationId":Uuid::new_v4()}),
        &owner.to_string(),
        &[],
        &|_| Ok(()),
    )
    .await
    .unwrap();
    (def, published["schemaDigest"].as_str().unwrap().to_owned())
}
async fn fixture(pool: &PgPool) -> (Uuid, Uuid, Uuid, Uuid, String, String) {
    let (host, wf, owner, tool) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    let (def, schema) = definition(pool, host, wf, owner, READ, "1.0.0", 1, "carryOver").await;
    (host, wf, owner, tool, def, schema)
}
fn input(
    host: Uuid,
    wf: Uuid,
    tool: Uuid,
    def: &str,
    schema: &str,
    version: &str,
    expected: i64,
) -> Value {
    json!({"hostId":host,"binding":{"sourceBindingId":Uuid::new_v4(),"toolId":tool,
        "toolName":format!("tool-{tool}"),"wfDefId":wf,"workflowVersion":version,
        "definitionDigest":def,"schemaDigest":schema,"invocationMode":"sync","syncWaitMs":1000,
        "totalDeadlineMs":30000,"executionClass":"interactive","resultTextMode":"compact-json",
        "cancellationPolicy":"before-effects-only","idempotencyPolicy":{"kind":"derived","resultReplayMs":0},
        "delegationPolicy":{"maximumDelegationDepth":1},
        "runtimeBounds":{"maximumTaskAttempts":8,"maximumNestedCalls":8,"maximumParallelism":1,
            "maximumRequestBytes":1048576,"maximumIntermediateBytes":4194304,"maximumResultBytes":1048576,"maximumCostUnits":1000},
        "admissionLimits":{"maximumConcurrentRuns":20,"maximumConcurrentRunsPerUser":2,
            "startsPerMinute":120,"startsPerMinutePerUser":10},"callerPolicy":{},
        "toolAnnotations":{"readOnly":true,"destructive":false},
        "policyDigest":digest(),"responsePolicyDigest":digest()},
        "dependencies":[],"endpointTargets":[{"endpointRef":"target-a",
            "endpointUri":"https://example.invalid/read","allowedMethods":["GET"],
            "authorizationPolicyDigest":digest()}],
        "expectedAggregateVersion":expected,"operationId":Uuid::new_v4()})
}
fn with_dependency(mut value: Value) -> Value {
    value["dependencies"] = json!([{"nestedToolId":Uuid::new_v4(),"nestedToolVersion":"1.0.0",
        "contractDigest":digest(),"compatibilityPolicy":"exact","authorizationToolName":"claims.lookup",
        "authorizationEndpointKey":"claims.lookup@call","authorizationPolicyDigest":digest(),
        "lifecycleStatus":"active","dispatchTarget":{"targetType":"mcp","endpoint":"claims.lookup@call"}}]);
    value
}
fn write_input(
    host: Uuid,
    wf: Uuid,
    tool: Uuid,
    def: &str,
    schema: &str,
    version: &str,
    expected: i64,
) -> Value {
    let mut value = input(host, wf, tool, def, schema, version, expected);
    value["binding"]["toolAnnotations"]["readOnly"] = json!(false);
    value["binding"]["idempotencyPolicy"]["resultReplayMs"] = json!(600_000);
    value["endpointTargets"][0]["allowedMethods"] = json!(["POST"]);
    value["endpointTargets"][0]["endpointUri"] = json!("https://example.invalid/write");
    value
}
async fn publish(pool: &PgPool, request: &Value, actor: Uuid) -> Value {
    let receipt = publication_api::publish_binding_verified(pool, request, &actor.to_string(), &[], 16)
        .await
        .unwrap();
    validate("BindingPublishOutput", &receipt);
    if receipt["status"] == "active" {
        assert!(receipt.get("carryOverDeniedReason").is_none());
    }
    receipt
}

#[tokio::test]
#[ignore = "requires the isolated workflow-invoke scratch database"]
async fn binding_schemas_round_trip_with_published_digest() {
    let (pool, _) = pools();
    let (host, wf, owner, tool, def, schema) = fixture(&pool).await;
    let mut request = input(host, wf, tool, &def, &schema, "1.0.0", 0);
    request["binding"]["inputSchema"] = json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"]});
    request["binding"]["outputSchema"] = json!({"type":"object","properties":{"answer":{"type":"integer"}},"required":["answer"]});
    let receipt = publish(&pool, &request, owner).await;
    let view = publication_api::get_verified(&pool,
        &json!({"hostId":host,"bindingId":id(&receipt)}), &owner.to_string(), &[]).await.unwrap();
    validate("BindingGetOutput", &view);
    assert_eq!(view["revision"]["binding"]["inputSchema"], request["binding"]["inputSchema"]);
    assert_eq!(view["revision"]["binding"]["outputSchema"], request["binding"]["outputSchema"]);
    assert_eq!(view["revision"]["bindingDigest"], receipt["bindingDigest"]);
    let reconstructed = json!({"hostId":host,"binding":view["revision"]["binding"],
        "dependencies":view["revision"]["dependencies"],
        "endpointTargets":view["revision"]["endpointTargets"],
        "expectedAggregateVersion":receipt["aggregateVersion"],"operationId":Uuid::new_v4()});
    let unchanged = publish(&pool, &reconstructed, owner).await;
    assert_eq!(unchanged["result"], "unchanged");
    assert_eq!(unchanged["bindingDigest"], receipt["bindingDigest"]);
}

#[tokio::test]
#[ignore = "requires the isolated workflow-invoke scratch database"]
async fn normalized_boolean_output_schemas_publish_and_round_trip() {
    let (pool, _) = pools();
    let (host, wf, owner, tool, def, schema) = fixture(&pool).await;
    let mut request = input(host, wf, tool, &def, &schema, "1.0.0", 0);
    for (expected, output) in [(0, json!({})), (1, json!({"not": {}}))] {
        request["expectedAggregateVersion"] = json!(expected);
        request["operationId"] = json!(Uuid::new_v4());
        request["binding"]["outputSchema"] = output.clone();
        let receipt = publish(&pool, &request, owner).await;
        assert_eq!(receipt["aggregateVersion"], json!(expected + 1));
        let view = publication_api::get_verified(
            &pool,
            &json!({"hostId":host,"bindingId":id(&receipt)}),
            &owner.to_string(),
            &[],
        )
        .await
        .unwrap();
        assert_eq!(view["revision"]["binding"]["outputSchema"], output);
        assert_eq!(view["revision"]["bindingDigest"], receipt["bindingDigest"]);
    }
}

#[tokio::test]
#[ignore = "requires the isolated workflow-invoke scratch database"]
async fn repeatable_read_binding_snapshot_survives_concurrent_approval() {
    let (pool, _) = pools();
    let (host, wf, owner, tool, def, schema) = fixture(&pool).await;
    let active = publish(&pool, &input(host,wf,tool,&def,&schema,"1.0.0",0),owner).await;
    let mut next=input(host,wf,tool,&def,&schema,"1.0.0",1);
    next["binding"]["totalDeadlineMs"]=json!(29_000);
    let pending = publish(&pool, &next, Uuid::new_v4()).await;
    let mut read = pool.begin().await.unwrap();
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *read).await.unwrap();
    let before:(i64,Option<Uuid>)=sqlx::query_as("SELECT aggregate_version,active_binding_id FROM workflow_tool_publication_t WHERE host_id=$1 AND tool_id=$2")
        .bind(host).bind(tool).fetch_one(&mut *read).await.unwrap();
    assert_eq!(before.1,Some(id(&active)));
    let barrier=Arc::new(Barrier::new(2));
    let writer_pool=ops_db::runtime_pool();
    let writer_barrier=barrier.clone();
    let approval=review(host,&pending,"approve",None);
    let writer=tokio::spawn(async move {
        writer_barrier.wait().await;
        publication_api::decide_verified(&writer_pool,&approval,&owner.to_string(),&[],16).await
    });
    barrier.wait().await;
    let approved=tokio::time::timeout(std::time::Duration::from_secs(10),writer).await.unwrap().unwrap().unwrap();
    assert_eq!(approved["result"],"approved");
    let after:(i64,Option<Uuid>)=sqlx::query_as("SELECT aggregate_version,active_binding_id FROM workflow_tool_publication_t WHERE host_id=$1 AND tool_id=$2")
        .bind(host).bind(tool).fetch_one(&mut *read).await.unwrap();
    assert_eq!(after,before,"head and revision reads must share one snapshot");
    let states:Vec<(Uuid,String)>=sqlx::query_as("SELECT binding_id,revision_status FROM workflow_tool_binding_t WHERE host_id=$1 AND tool_id=$2 ORDER BY requested_ts")
        .bind(host).bind(tool).fetch_all(&mut *read).await.unwrap();
    assert_eq!(states,vec![(id(&active),"approved".into()),(id(&pending),"pendingApproval".into())]);
    let counts:Vec<(String,i64)>=sqlx::query_as("SELECT revision_status,count(*) FROM workflow_tool_binding_t WHERE host_id=$1 AND tool_id=$2 GROUP BY revision_status ORDER BY revision_status")
        .bind(host).bind(tool).fetch_all(&mut *read).await.unwrap();
    assert_eq!(counts,vec![("approved".into(),1),("pendingApproval".into(),1)]);
    read.commit().await.unwrap();
    let view=publication_api::get_verified(&pool,&json!({"hostId":host,"bindingId":id(&pending)}),&owner.to_string(),&[]).await.unwrap();
    assert_eq!(view["revision"]["revisionStatus"],"approved");
    assert_eq!(view["aggregateVersion"],approved["aggregateVersion"]);
    let list=publication_api::list_verified(&pool,&json!({"hostId":host,"role":"owner","limit":20}),&owner.to_string(),&[]).await.unwrap();
    assert_eq!(list["counts"]["approved"],1);
    assert_eq!(list["counts"]["superseded"],1);
}
fn id(value: &Value) -> Uuid {
    value["bindingId"].as_str().unwrap().parse().unwrap()
}
fn review(host: Uuid, binding: &Value, action: &str, comment: Option<&str>) -> Value {
    let mut value = json!({"hostId":host,"bindingId":id(binding),
        "expectedBindingDigest":binding["bindingDigest"],"decision":action,"operationId":Uuid::new_v4()});
    if let Some(comment) = comment {
        value["comment"] = json!(comment)
    }
    value
}
fn revoke(host: Uuid, binding: &Value) -> Value {
    json!({"hostId":host,"bindingId":id(binding),"expectedBindingDigest":binding["bindingDigest"],
        "comment":"owner revoked","operationId":Uuid::new_v4()})
}
async fn version_two(
    pool: &PgPool,
    host: Uuid,
    wf: Uuid,
    owner: Uuid,
    approval: &str,
) -> (String, String) {
    let text = READ.replace("version: '1.0.0'", "version: '1.1.0'");
    definition(pool, host, wf, owner, &text, "1.1.0", 2, approval).await
}

#[tokio::test]
#[ignore = "requires the isolated workflow-invoke scratch database"]
async fn non_owner_publish_owner_approval_supersedes_previous_active() {
    let (pool, _admin) = pools();
    let (host, wf, owner, tool, def, schema) = fixture(&pool).await;
    let first = publish(
        &pool,
        &input(host, wf, tool, &def, &schema, "1.0.0", 0),
        owner,
    )
    .await;
    let requester = Uuid::new_v4();
    let mut next = with_dependency(input(host, wf, tool, &def, &schema, "1.0.0", 1));
    next["binding"]["totalDeadlineMs"] = json!(29_000);
    let pending = publish(&pool, &next, requester).await;
    assert_eq!(pending["status"], "pendingApproval");
    let pending_view = publication_api::get_verified(
        &pool,
        &json!({"hostId":host,"bindingId":id(&pending)}),
        &requester.to_string(),
        &[],
    )
    .await
    .unwrap();
    validate("BindingGetOutput", &pending_view);
    assert_eq!(
        pending_view["revision"]["dependencies"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        pending_view["activeRevision"]["bindingId"],
        first["bindingId"]
    );
    assert!(
        publication_api::get_verified(
            &pool,
            &json!({"hostId":host,"bindingId":id(&pending)}),
            &Uuid::new_v4().to_string(),
            &[]
        )
        .await
        .is_err()
    );
    assert!(
        publication_api::decide_verified(
            &pool,
            &review(host, &pending, "approve", None),
            &requester.to_string(),
            &[],
            16
        )
        .await
        .is_err()
    );
    let approval_request=review(host,&pending,"approve",None);
    let approved = publication_api::decide_verified(
        &pool,
        &approval_request,
        &owner.to_string(),
        &[],
        16,
    )
    .await
    .unwrap();
    validate("BindingDecideOutput", &approved);
    assert_eq!(approved["result"], "approved");
    let states:Vec<(Uuid,String)>=sqlx::query_as("SELECT binding_id,revision_status FROM workflow_tool_binding_t WHERE host_id=$1 AND tool_id=$2 ORDER BY requested_ts")
        .bind(host).bind(tool).fetch_all(&pool).await.unwrap();
    assert_eq!(states.len(), 2);
    assert_eq!(states[0], (id(&first), "superseded".into()));
    assert_eq!(states[1], (id(&pending), "approved".into()));
    let decisions: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM workflow_tool_binding_decision_t WHERE host_id=$1 AND tool_id=$2",
    )
    .bind(host)
    .bind(tool)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(decisions, 3);
    let list = publication_api::list_verified(
        &pool,
        &json!({"hostId":host,"role":"owner","limit":1}),
        &owner.to_string(),
        &[],
    )
    .await
    .unwrap();
    validate("BindingListOutput", &list);
    assert_eq!(list["items"].as_array().unwrap().len(), 1);
    assert!(list["nextCursor"].is_string());
    assert_eq!(list["counts"]["approved"], 1);
    assert_eq!(list["counts"]["superseded"], 1);
    let page = publication_api::list_verified(
        &pool,
        &json!({"hostId":host,"role":"owner","limit":1,
        "cursor":list["nextCursor"]}),
        &owner.to_string(),
        &[],
    )
    .await
    .unwrap();
    validate("BindingListOutput", &page);
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    let requester_list = publication_api::list_verified(
        &pool,
        &json!({"hostId":host,"role":"requester",
        "limit":20,"status":"approved"}),
        &requester.to_string(),
        &[],
    )
    .await
    .unwrap();
    validate("BindingListOutput", &requester_list);
    assert_eq!(requester_list["items"].as_array().unwrap().len(), 1);
    assert_eq!(
        requester_list["items"][0]["bindingId"],
        pending["bindingId"]
    );
    let new_owner=Uuid::new_v4();
    publication_api::save_definition_verified(&pool,&json!({"hostId":host,"wfDefId":wf,
        "sourceRevision":2,"actor":"step05","namespace":"step05","name":format!("definition-{wf}"),
        "version":"1.0.0","definition":READ,"lifecycleStatus":"PUBLISHED","catalogVisible":false,
        "owner":{"userId":new_owner},"active":true})).await.unwrap();
    let replay=publication_api::decide_verified(&pool,&approval_request,&owner.to_string(),&[],16).await.unwrap();
    assert_eq!(replay,approved,"the original actor keeps the stored receipt after owner transfer");
    assert!(publication_api::decide_verified(&pool,&approval_request,&new_owner.to_string(),&[],16).await.is_err());
}

#[tokio::test]
#[ignore = "requires the isolated workflow-invoke scratch database"]
async fn stale_decision_digest_is_rejected() {
    let (pool, _) = pools();
    let (host, wf, owner, tool, def, schema) = fixture(&pool).await;
    let pending = publish(
        &pool,
        &input(host, wf, tool, &def, &schema, "1.0.0", 0),
        Uuid::new_v4(),
    )
    .await;
    let mut request = review(host, &pending, "approve", None);
    request["expectedBindingDigest"] = json!(digest());
    let error = publication_api::decide_verified(&pool, &request, &owner.to_string(), &[], 16)
        .await
        .unwrap_err();
    assert_eq!(
        error_value(error).await["code"],
        "WORKFLOW_DEFINITION_MISMATCH"
    );
    let status: String = sqlx::query_scalar(
        "SELECT revision_status FROM workflow_tool_binding_t WHERE host_id=$1 AND binding_id=$2",
    )
    .bind(host)
    .bind(id(&pending))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status, "pendingApproval");
}

#[tokio::test]
#[ignore = "requires the isolated workflow-invoke scratch database"]
async fn rejection_requires_comment() {
    let (pool, _) = pools();
    let (host, wf, owner, tool, def, schema) = fixture(&pool).await;
    let pending = publish(
        &pool,
        &input(host, wf, tool, &def, &schema, "1.0.0", 0),
        Uuid::new_v4(),
    )
    .await;
    assert!(
        publication_api::decide_verified(
            &pool,
            &review(host, &pending, "reject", None),
            &owner.to_string(),
            &[],
            16
        )
        .await
        .is_err()
    );
    let rejected = publication_api::decide_verified(
        &pool,
        &review(host, &pending, "reject", Some("no")),
        &owner.to_string(),
        &[],
        16,
    )
    .await
    .unwrap();
    assert_eq!(rejected["result"], "rejected");
}

#[tokio::test]
#[ignore = "requires the isolated workflow-invoke scratch database"]
async fn revocation_denies_admission_and_retention_uses_creation_time() {
    let (pool, admin) = pools();
    let (host, wf, owner, tool, def, schema) = fixture(&pool).await;
    let active = publish(
        &pool,
        &input(host, wf, tool, &def, &schema, "1.0.0", 0),
        owner,
    )
    .await;
    let request = revoke(host, &active);
    assert!(
        publication_api::revoke_verified(&pool, &request, &Uuid::new_v4().to_string(), &[])
            .await
            .is_err()
    );
    let receipt = publication_api::revoke_verified(&pool, &request, &owner.to_string(), &[])
        .await
        .unwrap();
    validate("BindingRevokeOutput", &receipt);
    assert_eq!(receipt["result"], "revoked");
    let error = rule_api::read_admissible_pinned_binding(&pool, host, tool)
        .await
        .unwrap_err();
    assert_eq!(error_value(error).await["code"], "WORKFLOW_POLICY_DENIED");
    let old = Uuid::parse_str(request["operationId"].as_str().unwrap()).unwrap();
    sqlx::query("UPDATE workflow_publication_operation_t SET created_ts=CURRENT_TIMESTAMP-INTERVAL '31 days' WHERE host_id=$1 AND operation_id=$2")
        .bind(host).bind(old).execute(&admin).await.unwrap();
    let replay = publication_api::revoke_verified(&pool, &request, &owner.to_string(), &[])
        .await
        .unwrap();
    assert_eq!(replay, receipt);
    let created:chrono::DateTime<chrono::Utc>=sqlx::query_scalar("SELECT created_ts FROM workflow_publication_operation_t WHERE host_id=$1 AND operation_id=$2")
        .bind(host).bind(old).fetch_one(&admin).await.unwrap();
    assert!(created < chrono::Utc::now() - chrono::Duration::days(30));
    let removed = TaskExecutor::sweep_publication_operations(
        &pool,
        chrono::Utc::now() - chrono::Duration::days(30),
    )
    .await
    .unwrap();
    assert!(removed >= 1);
    let exists:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workflow_publication_operation_t WHERE host_id=$1 AND operation_id=$2)")
        .bind(host).bind(old).fetch_one(&pool).await.unwrap();
    assert!(!exists);
    let recent: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM workflow_publication_operation_t WHERE host_id=$1",
    )
    .bind(host)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(recent >= 2);
}

#[tokio::test]
#[ignore = "requires the isolated workflow-invoke scratch database"]
async fn expanded_endpoint_reach_stays_pending_with_reason() {
    let (pool, admin) = pools();
    let (host, wf, owner, tool, def, schema) = fixture(&pool).await;
    publish(
        &pool,
        &input(host, wf, tool, &def, &schema, "1.0.0", 0),
        owner,
    )
    .await;
    let (def2, schema2) = version_two(&pool, host, wf, owner, "carryOver").await;
    let carried = publish(
        &pool,
        &input(host, wf, tool, &def2, &schema2, "1.1.0", 1),
        Uuid::new_v4(),
    )
    .await;
    assert_eq!(carried["status"], "active");
    let basis_id: Uuid = sqlx::query_scalar("SELECT approval_basis_id FROM workflow_tool_binding_t WHERE host_id=$1 AND binding_id=$2")
        .bind(host).bind(id(&carried)).fetch_one(&pool).await.unwrap();
    assert_ne!(basis_id,id(&carried));
    let mut next = input(host, wf, tool, &def2, &schema2, "1.1.0", 2);
    next["endpointTargets"]
        .as_array_mut()
        .unwrap()
        .push(json!({"endpointRef":"extra",
        "endpointUri":"https://example.invalid/extra","allowedMethods":["GET"],
        "authorizationPolicyDigest":digest()}));
    let requester = Uuid::new_v4();
    let pending = publish(&pool, &next, requester).await;
    assert_eq!(pending["status"], "pendingApproval");
    assert!(
        pending["carryOverDeniedReason"]
            .as_str()
            .unwrap()
            .contains("reach")
    );
    assert_eq!(publish(&pool, &next, requester).await, pending);
    let mut unchanged_request = next.clone();
    unchanged_request["expectedAggregateVersion"] = pending["aggregateVersion"].clone();
    unchanged_request["operationId"] = json!(Uuid::new_v4());
    let unchanged = publish(&pool, &unchanged_request, requester).await;
    assert_eq!(unchanged["result"], "unchanged");
    assert_eq!(unchanged["bindingId"], pending["bindingId"]);
    assert_eq!(unchanged["carryOverDeniedReason"], pending["carryOverDeniedReason"]);
    sqlx::query("UPDATE workflow_publication_operation_t SET created_ts=CURRENT_TIMESTAMP-INTERVAL '31 days' WHERE host_id=$1 AND tool_name='workflow_binding_publish'")
        .bind(host).execute(&admin).await.unwrap();
    TaskExecutor::sweep_publication_operations(&pool,
        chrono::Utc::now()-chrono::Duration::days(30)).await.unwrap();
    let retained_receipts:i64=sqlx::query_scalar("SELECT count(*) FROM workflow_publication_operation_t WHERE host_id=$1 AND tool_name='workflow_binding_publish'")
        .bind(host).fetch_one(&pool).await.unwrap();
    assert_eq!(retained_receipts,0);
    let view = publication_api::get_verified(
        &pool,
        &json!({"hostId":host,"bindingId":id(&pending)}),
        &owner.to_string(),
        &[],
    )
    .await
    .unwrap();
    validate("BindingGetOutput", &view);
    assert_eq!(
        view["carryOverDeniedReason"],
        pending["carryOverDeniedReason"]
    );
    let (write_host, write_wf, write_owner, write_tool) = (
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
        Uuid::new_v4(),
    );
    let (write_def, write_schema) = definition(
        &pool,
        write_host,
        write_wf,
        write_owner,
        WRITE,
        "1.0.0",
        1,
        "carryOver",
    )
    .await;
    let base = publish(
        &pool,
        &write_input(
            write_host,
            write_wf,
            write_tool,
            &write_def,
            &write_schema,
            "1.0.0",
            0,
        ),
        write_owner,
    )
    .await;
    let write_v2 = WRITE.replace("version: '1.0.0'", "version: '1.1.0'");
    let (write_def2, write_schema2) = definition(
        &pool,
        write_host,
        write_wf,
        write_owner,
        &write_v2,
        "1.1.0",
        2,
        "carryOver",
    )
    .await;
    let carried_write = publish(
        &pool,
        &write_input(
            write_host,
            write_wf,
            write_tool,
            &write_def2,
            &write_schema2,
            "1.1.0",
            1,
        ),
        Uuid::new_v4(),
    )
    .await;
    assert_eq!(carried_write["status"], "active");
    let basis_id: Uuid = sqlx::query_scalar("SELECT approval_basis_id FROM workflow_tool_binding_t WHERE host_id=$1 AND binding_id=$2")
        .bind(write_host).bind(id(&carried_write)).fetch_one(&pool).await.unwrap();
    assert_eq!(basis_id,id(&base));
    let evidence:i64=sqlx::query_scalar("SELECT count(*) FROM workflow_tool_approval_evidence_t WHERE host_id=$1 AND binding_id=$2 AND task_name='update'")
        .bind(write_host).bind(id(&carried_write)).fetch_one(&pool).await.unwrap();
    assert_eq!(evidence, 1);
}

#[tokio::test]
#[ignore = "requires the isolated workflow-invoke scratch database"]
async fn reapprove_version_stays_pending() {
    let (pool, _) = pools();
    let (host, wf, owner, tool, def, schema) = fixture(&pool).await;
    publish(
        &pool,
        &input(host, wf, tool, &def, &schema, "1.0.0", 0),
        owner,
    )
    .await;
    let (def2, schema2) = version_two(&pool, host, wf, owner, "reapprove").await;
    let pending = publish(
        &pool,
        &input(host, wf, tool, &def2, &schema2, "1.1.0", 1),
        Uuid::new_v4(),
    )
    .await;
    assert_eq!(pending["status"], "pendingApproval");
    assert_eq!(
        pending["carryOverDeniedReason"],
        "definition version requires reapproval"
    );
}

#[tokio::test]
#[ignore = "requires the isolated workflow-invoke scratch database"]
async fn owner_change_denies_carry_over_and_new_owner_sees_pending() {
    let (pool, _) = pools();
    let (host, wf, owner, tool, def, schema) = fixture(&pool).await;
    publish(
        &pool,
        &input(host, wf, tool, &def, &schema, "1.0.0", 0),
        owner,
    )
    .await;
    let new_owner = Uuid::new_v4();
    let (def2, schema2) = version_two(&pool, host, wf, new_owner, "carryOver").await;
    let pending = publish(
        &pool,
        &input(host, wf, tool, &def2, &schema2, "1.1.0", 1),
        Uuid::new_v4(),
    )
    .await;
    assert_eq!(pending["status"], "pendingApproval");
    assert!(
        pending["carryOverDeniedReason"]
            .as_str()
            .unwrap()
            .contains("owner changed")
    );
    let view = publication_api::get_verified(
        &pool,
        &json!({"hostId":host,"bindingId":id(&pending)}),
        &new_owner.to_string(),
        &[],
    )
    .await
    .unwrap();
    validate("BindingGetOutput", &view);
    let list = publication_api::list_verified(
        &pool,
        &json!({"hostId":host,"role":"owner","limit":20,
        "status":"pendingApproval","wfDefId":wf}),
        &new_owner.to_string(),
        &[],
    )
    .await
    .unwrap();
    assert_eq!(list["items"].as_array().unwrap().len(), 1);
}

#[tokio::test]
#[ignore = "requires the isolated workflow-invoke scratch database"]
async fn concurrent_first_publication_creates_one_revision() {
    let (pool, _) = pools();
    let (host, wf, owner, tool, def, schema) = fixture(&pool).await;
    let barrier = Arc::new(Barrier::new(3));
    let mut tasks = Vec::new();
    for _ in 0..2 {
        let pool = ops_db::runtime_pool();
        let barrier = barrier.clone();
        let request = input(host, wf, tool, &def, &schema, "1.0.0", 0);
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            publication_api::publish_binding_verified(&pool, &request, &owner.to_string(), &[], 16)
                .await
        }));
    }
    barrier.wait().await;
    let a = tokio::time::timeout(std::time::Duration::from_secs(10), tasks.remove(0))
        .await
        .unwrap()
        .unwrap();
    let b = tokio::time::timeout(std::time::Duration::from_secs(10), tasks.remove(0))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(a.is_ok() as u8 + b.is_ok() as u8, 1);
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM workflow_tool_binding_t WHERE host_id=$1 AND tool_id=$2",
    )
    .bind(host)
    .bind(tool)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
    assert_head(&pool, host, tool).await;
}

#[tokio::test]
#[ignore = "requires the isolated workflow-invoke scratch database"]
async fn concurrent_approval_and_publication_keep_head_consistent() {
    let (pool, _) = pools();
    let (host, wf, owner, tool, def, schema) = fixture(&pool).await;
    let pending = publish(
        &pool,
        &input(host, wf, tool, &def, &schema, "1.0.0", 0),
        Uuid::new_v4(),
    )
    .await;
    let mut next = input(host, wf, tool, &def, &schema, "1.0.0", 1);
    next["binding"]["totalDeadlineMs"] = json!(29_000);
    let decide = review(host, &pending, "approve", None);
    let barrier = Arc::new(Barrier::new(3));
    let p1 = ops_db::runtime_pool();
    let b1 = barrier.clone();
    let owner_id = owner.to_string();
    let first = tokio::spawn(async move {
        b1.wait().await;
        publication_api::decide_verified(&p1, &decide, &owner_id, &[], 16).await
    });
    let p2 = ops_db::runtime_pool();
    let b2 = barrier.clone();
    let second = tokio::spawn(async move {
        b2.wait().await;
        publication_api::publish_binding_verified(&p2, &next, &Uuid::new_v4().to_string(), &[], 16)
            .await
    });
    barrier.wait().await;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), first)
        .await
        .unwrap()
        .unwrap();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), second)
        .await
        .unwrap()
        .unwrap();
    assert_head(&pool, host, tool).await;
}

async fn assert_head(pool: &PgPool, host: Uuid, tool: Uuid) {
    let row=sqlx::query("SELECT active_binding_id,pending_binding_id FROM workflow_tool_publication_t WHERE host_id=$1 AND tool_id=$2")
        .bind(host).bind(tool).fetch_one(pool).await.unwrap();
    let active: Option<Uuid> = row.try_get("active_binding_id").unwrap();
    let pending: Option<Uuid> = row.try_get("pending_binding_id").unwrap();
    let active_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM workflow_tool_binding_t WHERE host_id=$1 AND tool_id=$2 AND active",
    )
    .bind(host)
    .bind(tool)
    .fetch_one(pool)
    .await
    .unwrap();
    let pending_count:i64=sqlx::query_scalar("SELECT count(*) FROM workflow_tool_binding_t WHERE host_id=$1 AND tool_id=$2 AND revision_status='pendingApproval'")
        .bind(host).bind(tool).fetch_one(pool).await.unwrap();
    assert_eq!(active_count, active.is_some() as i64);
    assert_eq!(pending_count, pending.is_some() as i64);
    if let Some(id) = active {
        let status:String=sqlx::query_scalar("SELECT revision_status FROM workflow_tool_binding_t WHERE host_id=$1 AND binding_id=$2")
        .bind(host).bind(id).fetch_one(pool).await.unwrap();
        assert_eq!(status, "approved");
    }
    if let Some(id) = pending {
        let status:String=sqlx::query_scalar("SELECT revision_status FROM workflow_tool_binding_t WHERE host_id=$1 AND binding_id=$2")
        .bind(host).bind(id).fetch_one(pool).await.unwrap();
        assert_eq!(status, "pendingApproval");
    }
}

#[tokio::test]
#[ignore = "requires the isolated workflow-invoke scratch database"]
async fn concurrent_retirement_and_approval_allow_one_success() {
    let (pool, _) = pools();
    let (host, wf, owner, tool, def, schema) = fixture(&pool).await;
    let pending = publish(
        &pool,
        &input(host, wf, tool, &def, &schema, "1.0.0", 0),
        Uuid::new_v4(),
    )
    .await;
    let decide = review(host, &pending, "approve", None);
    let barrier = Arc::new(Barrier::new(3));
    let p1 = ops_db::runtime_pool();
    let b1 = barrier.clone();
    let actor = owner.to_string();
    let first = tokio::spawn(async move {
        b1.wait().await;
        publication_api::decide_verified(&p1, &decide, &actor, &[], 16).await
    });
    let p2 = ops_db::runtime_pool();
    let b2 = barrier.clone();
    let actor = owner.to_string();
    let second = tokio::spawn(async move {
        b2.wait().await;
        publication_api::retire_definition_verified(
            &p2,
            &json!({"hostId":host,"wfDefId":wf,"version":"1.0.0","operationId":Uuid::new_v4()}),
            &actor,
        )
        .await
    });
    barrier.wait().await;
    let a = tokio::time::timeout(std::time::Duration::from_secs(10), first)
        .await
        .unwrap()
        .unwrap();
    let b = tokio::time::timeout(std::time::Duration::from_secs(10), second)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(a.is_ok() as u8 + b.is_ok() as u8, 1);
    assert_head(&pool, host, tool).await;
}
