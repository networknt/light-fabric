#[path = "support/ops_db.rs"]
mod ops_db;
use chrono::{Duration, Utc};
use light_workflow::{
    executor::resolve_granted_endpoint,
    invocation::{
        AcceptOutcome, AuthenticatedInvocationContext, PreparedInvocationStart, accept_invocation,
    },
    publication_api,
};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;
use workflow_invocation_contract::*;

fn digest() -> String {
    format!("sha256:{}", "a".repeat(64))
}
fn policy(host: Uuid, tool: Uuid, capability: &str, revision: i64, enabled: bool) -> Value {
    json!({"hostId":host,"toolId":tool,"policyId":tool,"capabilityRef":capability,"toolVersion":"1.0.0",
        "lightapiDigest":digest(),"allowedEnvironments":["dev"],"allowedMethods":["GET"],"enabled":enabled,"sourceRevision":revision,"actor":"isolated-admin"})
}
async fn publish(pool: &PgPool, value: &Value) -> Value {
    publication_api::tool_access::publish_verified(pool, value)
        .await
        .unwrap()
}
async fn request(
    pool: &PgPool,
    host: Uuid,
    wf: Uuid,
    outer: Uuid,
    binding: Uuid,
) -> StartInvocationRequest {
    let now = Utc::now();
    let deadline = now + Duration::minutes(5);
    let input = json!({});
    let input_digest = canonical_sha256(&input).unwrap();
    let (definition_digest,schema_digest):(String,String)=sqlx::query_as("SELECT definition_digest,schema_digest FROM workflow_tool_binding_t WHERE host_id=$1 AND binding_id=$2").bind(host).bind(binding).fetch_one(pool).await.unwrap();
    StartInvocationRequest {
        contract_version: CONTRACT_VERSION,
        workflow_instance_id: Uuid::now_v7(),
        stable_tool_ref: outer,
        workflow_definition_id: wf,
        workflow_version: "1.0.0".into(),
        definition_digest,
        schema_digest,
        policy_digest: digest(),
        response_policy_digest: digest(),
        renewable_grant_id: None,
        parent_action_id: None,
        mode: InvocationMode::Sync,
        cancellation_policy: CancellationPolicy::BeforeEffectsOnly,
        execution_class: ExecutionClass::Interactive,
        permit_depth: 0,
        deadline_ts: deadline,
        canonical_input_profile: CANONICAL_INPUT_PROFILE.into(),
        normalized_input_digest: input_digest.clone(),
        input,
        caller_claims: json!({"sub":"isolated-user"}),
        idempotency: IdempotencyBinding {
            kind: IdempotencyKind::Derived,
            scoped_key_digest: canonical_sha256(&json!(Uuid::new_v4())).unwrap(),
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
fn definition(tools: &[(Uuid, &str)]) -> Value {
    json!({"document":{"dsl":"1.0.3","namespace":"isolated","name":"two-get","version":"1.0.0"},
        "evaluate":{"language":"cel"},"do":tools.iter().map(|(tool,cap)|json!({cap.split('/').last().unwrap():{"call":"http","with":{"method":"GET","endpoint":{"uri":format!("lightapi://{cap}")}},"metadata":{"workflowTool":{"toolId":tool,"capabilityRef":cap,"version":"1.0.0","lightapiDigest":digest(),"allowedEnvironments":["dev"]}},"end":true}})).collect::<Vec<_>>()})
}
async fn definition_and_binding(
    pool: &PgPool,
    host: Uuid,
    wf: Uuid,
    outer: Uuid,
    definition: &Value,
) -> Uuid {
    let source = Uuid::new_v4();
    let owner = Uuid::new_v4();
    let text = serde_yaml::to_string(definition).unwrap();
    let saved=publication_api::save_definition_verified(pool,&json!({"hostId":host,"wfDefId":wf,"sourceRevision":1,"actor":"isolated-admin","namespace":"isolated","name":format!("two-get-{wf}"),"version":"1.0.0","definition":text,"lifecycleStatus":"PUBLISHED","catalogVisible":false,"owner":{"userId":owner},"active":true})).await.unwrap();
    let def = saved["definitionDigest"].as_str().unwrap();
    let published=publication_api::publish_definition_verified(pool,&json!({"hostId":host,"wfDefId":wf,"version":"1.0.0","definition":text,"expectedDefinitionDigest":def,"operationId":Uuid::new_v4()}),&owner.to_string(),&[],&|_|Ok(())).await.unwrap();
    let targets=definition["do"].as_array().unwrap().iter().map(|entry|{
        let task=entry.as_object().unwrap().values().next().unwrap();let cap=task["metadata"]["workflowTool"]["capabilityRef"].as_str().unwrap();
        json!({"endpointRef":cap,"endpointUri":"https://light-gateway:8443","allowedMethods":["GET"],"authorizationPolicyDigest":digest(),
            "resolutionDocument":{"operations":{"read":{"endpointId":cap,"protocol":"http","method":"GET","endpoint":"/github/synthetic","authentication":{"type":"none"}}}}})
    }).collect::<Vec<_>>();
    let args = json!({"hostId":host,"binding":{"sourceBindingId":source,"toolId":outer,"toolName":format!("two-get-{outer}"),"wfDefId":wf,"workflowVersion":"1.0.0","definitionDigest":def,"schemaDigest":published["schemaDigest"],"invocationMode":"sync","syncWaitMs":1000,"totalDeadlineMs":30000,"executionClass":"interactive","resultTextMode":"compact-json","cancellationPolicy":"before-effects-only","idempotencyPolicy":{"kind":"derived","resultReplayMs":30000},"delegationPolicy":{"maximumDelegationDepth":1},"runtimeBounds":{"maximumTaskAttempts":8,"maximumNestedCalls":8,"maximumParallelism":1,"maximumRequestBytes":1048576,"maximumIntermediateBytes":4194304,"maximumResultBytes":1048576,"maximumCostUnits":1000},"admissionLimits":{"maximumConcurrentRuns":20,"maximumConcurrentRunsPerUser":2,"startsPerMinute":120,"startsPerMinutePerUser":10},"callerPolicy":{},"toolAnnotations":{"readOnly":true,"destructive":false},"policyDigest":digest(),"responsePolicyDigest":digest()},"dependencies":[],"endpointTargets":targets,"expectedAggregateVersion":0,"operationId":Uuid::new_v4()});
    let binding =
        publication_api::publish_binding_verified(pool, &args, &owner.to_string(), &[], 16)
            .await
            .unwrap();
    binding["bindingId"].as_str().unwrap().parse().unwrap()
}
async fn accept(
    pool: &PgPool,
    host: Uuid,
    _wf: Uuid,
    binding: Uuid,
    req: &StartInvocationRequest,
    definition: &Value,
    environment: &str,
) -> Result<(AcceptOutcome, Uuid), String> {
    let process = Uuid::new_v4();
    let mut tx = pool.begin().await.unwrap();
    let accepted = accept_in(
        &mut tx,
        host,
        binding,
        req,
        definition,
        environment,
        process,
    )
    .await?;
    tx.commit().await.unwrap();
    Ok((accepted, process))
}
async fn accept_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    host: Uuid,
    binding: Uuid,
    req: &StartInvocationRequest,
    definition: &Value,
    environment: &str,
    process: Uuid,
) -> Result<AcceptOutcome, String> {
    let auth = AuthenticatedInvocationContext {
        host_id: host,
        principal_subject: "isolated-user",
        end_user_subject: "isolated-user",
        update_user: "isolated-admin",
        user_authorization: None,
        user_authorization_exp: None,
    };
    let prepared = PreparedInvocationStart {
        binding_id: Some(binding),
        process_id: process,
        initial_task_id: Uuid::new_v4(),
        application_id: "isolated",
        initial_task_name: "getIssue",
        initial_task_type: "http",
        definition_snapshot: definition,
        execution_placement: "host",
        execution_profile_id: "host",
        admission_profile: "workflow_backed",
        tool_environment: environment,
        policy_snapshot_id: None,
        task_policy_digest: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        public_output_schema: None,
        expression_admission: None,
    };
    accept_invocation(tx, &auth, req, &prepared)
        .await
        .map_err(|e| e.to_string())
}

#[tokio::test]
#[ignore = "requires the mandatory isolated Host Tool PostgreSQL qualification lane"]
async fn before_workflows_two_get_publication_start_disable_recovery_and_specific_coexistence() {
    let pool = ops_db::runtime_pool();
    let host = Uuid::new_v4();
    let issue = Uuid::new_v4();
    let comments = Uuid::new_v4();
    let reviewed = [
        (issue, "GITHUB/getIssue"),
        (comments, "GITHUB/listIssueComments"),
    ];
    for (tool, cap) in reviewed {
        publish(&pool, &policy(host, tool, cap, 1, true)).await;
    }
    let zero: i64 = sqlx::query_scalar("SELECT count(*) FROM wf_definition_t WHERE host_id=$1")
        .bind(host)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(zero, 0);
    let wf = Uuid::new_v4();
    let outer = Uuid::new_v4();
    let definition = definition(&reviewed);
    let binding = definition_and_binding(&pool, host, wf, outer, &definition).await;
    let req = request(&pool, host, wf, outer, binding).await;
    let (outcome, process) = accept(&pool, host, wf, binding, &req, &definition, "dev")
        .await
        .unwrap();
    assert!(matches!(outcome, AcceptOutcome::Accepted { .. }));
    let pins:i64=sqlx::query_scalar("SELECT count(*) FROM workflow_accepted_tool_authority_t WHERE host_id=$1 AND process_id=$2 AND authorization_source='HOST_TOOL'").bind(host).bind(process).fetch_one(&pool).await.unwrap();
    assert_eq!(pins, 2);
    let grants: i64 =
        sqlx::query_scalar("SELECT count(*) FROM workflow_tool_grant_t WHERE host_id=$1")
            .bind(host)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(grants, 0);
    for (tool, cap) in reviewed {
        publish(&pool, &policy(host, tool, cap, 2, false)).await;
    }
    assert!(
        accept(
            &pool,
            host,
            wf,
            binding,
            &request(&pool, host, wf, outer, binding).await,
            &definition,
            "dev"
        )
        .await
        .unwrap_err()
        .contains("WORKFLOW_TOOL_ACCESS_DENIED")
    );
    for (tool, cap) in reviewed {
        assert!(
            resolve_granted_endpoint(
                &pool,
                host,
                wf,
                tool,
                "1.0.0",
                &digest(),
                "dev",
                process,
                cap,
                "GET"
            )
            .await
            .unwrap()
            .is_some()
        );
    }
    let (replay, _) = accept(&pool, host, wf, binding, &req, &definition, "dev")
        .await
        .unwrap();
    assert!(matches!(replay, AcceptOutcome::Replay { .. }));
    for (tool, _) in reviewed {
        sqlx::query("INSERT INTO workflow_tool_grant_t(host_id,grant_id,tool_id,wf_def_id,tool_version,lightapi_digest,allowed_environments,aggregate_version,active) VALUES($1,$2,$3,$4,'1.0.0',$5,ARRAY['dev'],1,true)").bind(host).bind(Uuid::new_v4()).bind(tool).bind(wf).bind(digest()).execute(&pool).await.unwrap();
    }
    let (_, specific_process) = accept(
        &pool,
        host,
        wf,
        binding,
        &request(&pool, host, wf, outer, binding).await,
        &definition,
        "dev",
    )
    .await
    .unwrap();
    let sources:Vec<String>=sqlx::query_scalar("SELECT authorization_source FROM workflow_accepted_tool_authority_t WHERE host_id=$1 AND process_id=$2").bind(host).bind(specific_process).fetch_all(&pool).await.unwrap();
    assert_eq!(sources, vec!["SPECIFIC_GRANT"; 2]);
    for (tool, cap) in reviewed {
        publish(&pool, &policy(host, tool, cap, 3, true)).await;
    }
    sqlx::query("UPDATE workflow_tool_grant_t SET active=false WHERE host_id=$1")
        .bind(host)
        .execute(&pool)
        .await
        .unwrap();
    accept(
        &pool,
        host,
        wf,
        binding,
        &request(&pool, host, wf, outer, binding).await,
        &definition,
        "dev",
    )
    .await
    .unwrap();
    assert!(
        accept(
            &pool,
            host,
            wf,
            binding,
            &request(&pool, host, wf, outer, binding).await,
            &definition,
            "test"
        )
        .await
        .is_err()
    );
    let mut wrong = definition.clone();
    wrong["do"][0]["getIssue"]["metadata"]["workflowTool"]["version"] = json!("2.0.0");
    assert!(
        accept(
            &pool,
            host,
            wf,
            binding,
            &request(&pool, host, wf, outer, binding).await,
            &wrong,
            "dev"
        )
        .await
        .is_err()
    );
    let mut wrong = definition.clone();
    wrong["do"][0]["getIssue"]["with"]["method"] = json!("POST");
    assert!(
        accept(
            &pool,
            host,
            wf,
            binding,
            &request(&pool, host, wf, outer, binding).await,
            &wrong,
            "dev"
        )
        .await
        .is_err()
    );
    assert!(
        resolve_granted_endpoint(
            &pool,
            Uuid::new_v4(),
            wf,
            issue,
            "1.0.0",
            &digest(),
            "dev",
            process,
            "GITHUB/getIssue",
            "GET"
        )
        .await
        .unwrap()
        .is_none()
    );
    // Broad authority never turns a legacy direct route into a Gateway bypass.
    sqlx::query("UPDATE workflow_endpoint_target_t SET endpoint_uri='https://registered.invalid' WHERE host_id=$1").bind(host).execute(&pool).await.unwrap();
    assert!(
        accept(
            &pool,
            host,
            wf,
            binding,
            &request(&pool, host, wf, outer, binding).await,
            &definition,
            "dev"
        )
        .await
        .is_err()
    );
    // An independently valid specific grant still authorizes its existing route.
    sqlx::query("UPDATE workflow_tool_grant_t SET active=true WHERE host_id=$1")
        .bind(host)
        .execute(&pool)
        .await
        .unwrap();
    let (_, legacy_route_process) = accept(
        &pool,
        host,
        wf,
        binding,
        &request(&pool, host, wf, outer, binding).await,
        &definition,
        "dev",
    )
    .await
    .unwrap();
    let sources:Vec<String>=sqlx::query_scalar("SELECT authorization_source FROM workflow_accepted_tool_authority_t WHERE host_id=$1 AND process_id=$2")
        .bind(host).bind(legacy_route_process).fetch_all(&pool).await.unwrap();
    assert_eq!(sources, vec!["SPECIFIC_GRANT"; 2]);
}

#[tokio::test]
#[ignore = "requires the mandatory isolated Host Tool PostgreSQL qualification lane"]
async fn duplicate_out_of_order_conflicting_and_renewed_publication() {
    let pool = ops_db::runtime_pool();
    let host = Uuid::new_v4();
    let tool = Uuid::new_v4();
    let enabled = policy(host, tool, "GITHUB/getIssue", 1, true);
    let first = publish(&pool, &enabled).await;
    let repeated = publish(&pool, &enabled).await;
    assert_eq!(first["publicationDigest"], repeated["publicationDigest"]);
    assert_eq!(repeated["result"], "unchanged");
    let disabled = policy(host, tool, "GITHUB/getIssue", 2, false);
    publish(&pool, &disabled).await;
    assert_eq!(publish(&pool, &enabled).await["result"], "stale");
    let mut conflict = disabled.clone();
    conflict["enabled"] = json!(true);
    assert!(
        publication_api::tool_access::publish_verified(&pool, &conflict)
            .await
            .is_err()
    );
    let mut renewed = policy(host, tool, "GITHUB/getIssue", 3, true);
    renewed["toolVersion"] = json!("2.0.0");
    renewed["lightapiDigest"] = json!(format!("sha256:{}", "b".repeat(64)));
    publish(&pool, &renewed).await;
    assert_eq!(publish(&pool, &disabled).await["appliedRevision"], 3);
    let state: (bool, String) = sqlx::query_as(
        "SELECT enabled,tool_version FROM tool_workflow_access_t WHERE host_id=$1 AND tool_id=$2",
    )
    .bind(host)
    .bind(tool)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(state, (true, "2.0.0".into()));
    let mut malformed = renewed.clone();
    malformed["sourceRevision"] = json!(4);
    malformed["allowedMethods"] = json!(["TRACE"]);
    assert!(
        publication_api::tool_access::publish_verified(&pool, &malformed)
            .await
            .is_err()
    );
}

#[tokio::test]
#[ignore = "requires the mandatory isolated Host Tool PostgreSQL qualification lane"]
async fn acceptance_disable_and_renewal_races_are_serialized_and_pins_are_immutable() {
    let pool = ops_db::runtime_pool();
    let host = Uuid::new_v4();
    let tool = Uuid::new_v4();
    let cap = "GITHUB/getIssue";
    let enabled = policy(host, tool, cap, 1, true);
    publish(&pool, &enabled).await;
    let wf = Uuid::new_v4();
    let outer = Uuid::new_v4();
    let def = definition(&[(tool, cap)]);
    let binding = definition_and_binding(&pool, host, wf, outer, &def).await;
    let req = request(&pool, host, wf, outer, binding).await;
    let process = Uuid::new_v4();
    let mut tx = pool.begin().await.unwrap();
    assert!(matches!(
        accept_in(&mut tx, host, binding, &req, &def, "dev", process)
            .await
            .unwrap(),
        AcceptOutcome::Accepted { .. }
    ));
    // Acceptance holds the shared advisory fence through its commit.
    let publisher_pool = pool.clone();
    let disabled = policy(host, tool, cap, 2, false);
    let disable = tokio::spawn(async move { publish(&publisher_pool, &disabled).await });
    // Observe the actual blocked PostgreSQL lock rather than assuming timing.
    let admin = ops_db::admin_pool();
    let mut blocked = false;
    for _ in 0..100 {
        let waiting:i64=sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND wait_event='advisory' AND query LIKE '%pg_advisory_xact_lock%'").fetch_one(&admin).await.unwrap();
        if waiting > 0 {
            blocked = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(blocked, "disable must wait for accepting transaction");
    assert!(!disable.is_finished());
    tx.commit().await.unwrap();
    assert_eq!(disable.await.unwrap()["appliedRevision"], 2);
    assert!(
        resolve_granted_endpoint(
            &pool,
            host,
            wf,
            tool,
            "1.0.0",
            &digest(),
            "dev",
            process,
            cap,
            "GET"
        )
        .await
        .unwrap()
        .is_some()
    );
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM process_info_t WHERE host_id=$1")
        .bind(host)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        accept(
            &pool,
            host,
            wf,
            binding,
            &request(&pool, host, wf, outer, binding).await,
            &def,
            "dev"
        )
        .await
        .is_err()
    );
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM process_info_t WHERE host_id=$1")
        .bind(host)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(before, after);
    let immutable=sqlx::query("UPDATE workflow_accepted_tool_authority_t SET authority_revision=99 WHERE host_id=$1 AND process_id=$2").bind(host).bind(process).execute(&pool).await;
    assert!(immutable.is_err());
    // Competing renewal/publication revisions converge on the newer disable regardless of scheduling.
    let mut renewed = policy(host, tool, cap, 3, true);
    renewed["toolVersion"] = json!("2.0.0");
    let newer = policy(host, tool, cap, 4, false);
    let (a, b) = tokio::join!(
        publication_api::tool_access::publish_verified(&pool, &renewed),
        publication_api::tool_access::publish_verified(&pool, &newer)
    );
    assert!(a.is_ok() && b.is_ok());
    let state:(i64,bool)=sqlx::query_as("SELECT source_revision,enabled FROM tool_workflow_access_t WHERE host_id=$1 AND tool_id=$2").bind(host).bind(tool).fetch_one(&pool).await.unwrap();
    assert_eq!(state, (4, false));
    assert!(
        resolve_granted_endpoint(
            &pool,
            host,
            wf,
            tool,
            "1.0.0",
            &digest(),
            "dev",
            process,
            cap,
            "GET"
        )
        .await
        .unwrap()
        .is_some()
    );
}

#[tokio::test]
#[ignore = "requires the mandatory isolated Host Tool PostgreSQL qualification lane"]
async fn portal_generated_exact_two_get_policies_publish_and_accept_without_specific_grants() {
    let pool = ops_db::runtime_pool();
    let path = std::env::var("HOST_ACCESS_HANDOFF").expect("Portal handoff fixture required");
    assert!(path.ends_with("host-tool-workflow-access-20261004-r1/portal-two-get-handoff.json"));
    let handoff: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let policies = handoff["policies"].as_array().unwrap();
    assert_eq!(policies.len(), 2);
    let host: Uuid = policies[0]["hostId"].as_str().unwrap().parse().unwrap();
    for policy in policies {
        publish(&pool, policy).await;
    }
    let wf = Uuid::new_v4();
    let outer = Uuid::new_v4();
    let definition = &handoff["definition"];
    let binding = definition_and_binding(&pool, host, wf, outer, definition).await;
    let (_, process) = accept(
        &pool,
        host,
        wf,
        binding,
        &request(&pool, host, wf, outer, binding).await,
        definition,
        "dev",
    )
    .await
    .unwrap();
    let pins:i64=sqlx::query_scalar("SELECT count(*) FROM workflow_accepted_tool_authority_t WHERE host_id=$1 AND process_id=$2 AND authorization_source='HOST_TOOL'")
        .bind(host).bind(process).fetch_one(&pool).await.unwrap();
    assert_eq!(pins, 2);
    let grants: i64 =
        sqlx::query_scalar("SELECT count(*) FROM workflow_tool_grant_t WHERE host_id=$1")
            .bind(host)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(grants, 0);
    for policy in policies {
        let tool = policy["toolId"].as_str().unwrap().parse().unwrap();
        assert!(
            resolve_granted_endpoint(
                &pool,
                host,
                wf,
                tool,
                policy["toolVersion"].as_str().unwrap(),
                policy["lightapiDigest"].as_str().unwrap(),
                "dev",
                process,
                policy["capabilityRef"].as_str().unwrap(),
                "GET"
            )
            .await
            .unwrap()
            .is_some()
        );
    }
}
