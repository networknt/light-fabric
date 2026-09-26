#[path = "support/ops_db.rs"]
mod ops_db;

use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

const PRE_HOST: &str = "a0180000-0000-4000-8000-000000000001";
const PRE_DEFINITION: &str = "a0180000-0000-4000-8000-000000000002";
const PRE_TOOL: &str = "a0180000-0000-4000-8000-000000000003";
const PRE_LONG_AUTHORITY_RUN: &str = "a0180000-0000-4000-8000-000000000041";
const PRE_BROKER_AUTHORITY_RUN: &str = "a0180000-0000-4000-8000-000000000042";
const PRE_LONG_GRANT: &str = "a0180000-0000-4000-8000-000000000031";
const PRE_LONG_OWNER: &str = "a0180000-0000-4000-8000-000000000033";
const PRE_ENDPOINT: &str = "shared-pre0018-endpoint";

fn pools() -> (PgPool, PgPool) {
    // Construct both helpers for every case so a missing runtime or admin URL
    // always fails closed, even in cases whose assertions use only one role.
    (ops_db::runtime_pool(), ops_db::admin_pool())
}

fn assert_mode(expected: &str) {
    let actual = std::env::var("MIGRATION_0018_MODE")
        .expect("MIGRATION_0018_MODE is required (fresh or upgrade)");
    assert_eq!(
        actual, expected,
        "test invoked against the wrong database mode"
    );
}

fn digest(value: char) -> String {
    format!("sha256:{}", value.to_string().repeat(64))
}

async fn insert_definition(pool: &PgPool, host: Uuid) -> Uuid {
    ops_db::insert_workflow_definition(pool, host)
        .await
        .expect("insert Workflow definition")
        .0
}

async fn insert_binding(
    pool: &PgPool,
    host: Uuid,
    definition: Uuid,
    tool: Uuid,
    binding: Uuid,
    workflow_version: &str,
    revision_status: &str,
    active: bool,
    replay_ms: i64,
) {
    let d = digest('a');
    let source_binding = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO workflow_tool_binding_t(
            host_id,binding_id,tool_id,wf_def_id,workflow_version,
            definition_digest,schema_digest,invocation_mode,sync_wait_ms,total_deadline_ms,
            execution_class,result_text_mode,idempotency_policy,delegation_policy,
            response_policy_digest,runtime_bounds,policy_digest,
            revision_status,binding_digest,approval_digest,source_binding_id,requested_by,requested_ts,active
         ) VALUES (
            $1,$2,$3,$4,$5,$6,$6,'sync',100,1000,'interactive','compact-json',
            $7,'{}',$6,'{}',$6,$8,$6,$6,$9,'migration-test',now(),$10
         )",
    )
    .bind(host)
    .bind(binding)
    .bind(tool)
    .bind(definition)
    .bind(workflow_version)
    .bind(&d)
    .bind(json!({"kind":"derived","resultReplayMs":replay_ms}))
    .bind(revision_status)
    .bind(source_binding)
    .bind(active)
    .execute(pool)
    .await
    .expect("insert binding fixture");
}

async fn insert_endpoint(pool: &PgPool, host: Uuid, binding: Uuid, endpoint_ref: &str) {
    sqlx::query(
        "INSERT INTO workflow_endpoint_target_t(
            host_id,binding_id,endpoint_ref,endpoint_uri,allowed_methods,authorization_policy_digest
         ) VALUES($1,$2,$3,'https://example.invalid/tool',ARRAY['GET'],$4)",
    )
    .bind(host)
    .bind(binding)
    .bind(endpoint_ref)
    .bind(digest('f'))
    .execute(pool)
    .await
    .expect("insert endpoint target fixture");
}

async fn insert_invocation(
    pool: &PgPool,
    host: Uuid,
    instance: Uuid,
    binding: Uuid,
    definition: Uuid,
    tool: Uuid,
    state: &str,
    terminal: bool,
    effect_state: &str,
    definition_digest: &str,
) {
    sqlx::query(
        "INSERT INTO workflow_invocation_t(
            host_id,workflow_instance_id,binding_id,stable_tool_ref,wf_def_id,workflow_version,
            definition_digest,schema_digest,policy_digest,response_policy_digest,
            principal_subject,end_user_subject,input,input_digest,canonical_input_profile,
            invocation_mode,execution_class,state,effect_state,correlation_id,deadline_ts,
            terminal_ts,response_policy_snapshot
         ) VALUES(
            $1,$2,$3,$4,$5,'1.0.0',$6,$6,$6,$6,'client-a','end-user','{}',$6,
            'rfc8785-safe-json-v1','sync','interactive',$7,$8,'migration-0018',
            now()+interval '1 hour',CASE WHEN $9 THEN now() ELSE NULL END,'{}'
         )",
    )
    .bind(host)
    .bind(instance)
    .bind(binding)
    .bind(tool)
    .bind(definition)
    .bind(definition_digest)
    .bind(state)
    .bind(effect_state)
    .bind(terminal)
    .execute(pool)
    .await
    .expect("insert invocation fixture");
}

async fn insert_reservation(
    pool: &PgPool,
    host: Uuid,
    reservation: Uuid,
    scope: &str,
    tool: Uuid,
    principal: &str,
    end_user: &str,
    instance: Uuid,
    definition_digest: &str,
    input_digest: &str,
    expires_in: &str,
) {
    let sql = format!(
        "INSERT INTO workflow_invocation_idempotency_t(
            host_id,reservation_id,scope_digest,idempotency_kind,stable_tool_ref,
            principal_subject,end_user_subject,workflow_instance_id,definition_digest,
            input_digest,in_flight_until,result_replay_until
         ) VALUES($1,$2,$3,'EXPLICIT',$4,$5,$6,$7,$8,$9,
            now()+($10)::interval,now()+($10)::interval)"
    );
    sqlx::query(&sql)
        .bind(host)
        .bind(reservation)
        .bind(scope)
        .bind(tool)
        .bind(principal)
        .bind(end_user)
        .bind(instance)
        .bind(definition_digest)
        .bind(input_digest)
        .bind(expires_in)
        .execute(pool)
        .await
        .expect("insert idempotency fixture");
}

async fn claim(
    pool: &PgPool,
    host: Uuid,
    reservation: Uuid,
    scope: &str,
    tool: Uuid,
    principal: &str,
    end_user: &str,
    instance: Uuid,
    definition_digest: &str,
    input_digest: &str,
) -> (String, Uuid, i64) {
    sqlx::query_as(
        "SELECT outcome,accepted_workflow_instance_id,accepted_generation
           FROM workflow_claim_idempotency_v2(
             $1,$2,$3,'EXPLICIT',$4,$5,$6,$7,$8,$9,
             now()+interval '1 hour',now()+interval '1 hour')",
    )
    .bind(host)
    .bind(reservation)
    .bind(scope)
    .bind(tool)
    .bind(principal)
    .bind(end_user)
    .bind(instance)
    .bind(definition_digest)
    .bind(input_digest)
    .fetch_one(pool)
    .await
    .expect("call workflow_claim_idempotency_v2")
}

async fn claim_optional(
    pool: &PgPool,
    host: Uuid,
    reservation: Uuid,
    scope: &str,
    tool: Uuid,
    principal: &str,
    end_user: &str,
    instance: Uuid,
    definition_digest: &str,
    input_digest: &str,
) -> (String, Option<Uuid>, Option<i64>) {
    sqlx::query_as(
        "SELECT outcome,accepted_workflow_instance_id,accepted_generation
           FROM workflow_claim_idempotency_v2(
             $1,$2,$3,'EXPLICIT',$4,$5,$6,$7,$8,$9,
             now()+interval '1 hour',now()+interval '1 hour')",
    )
    .bind(host)
    .bind(reservation)
    .bind(scope)
    .bind(tool)
    .bind(principal)
    .bind(end_user)
    .bind(instance)
    .bind(definition_digest)
    .bind(input_digest)
    .fetch_one(pool)
    .await
    .expect("call workflow_claim_idempotency_v2 concurrently")
}

async fn assert_primary_key(pool: &PgPool) {
    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT array_agg(a.attname::text ORDER BY key.ordinality)
           FROM pg_constraint c
           CROSS JOIN LATERAL unnest(c.conkey) WITH ORDINALITY AS key(attnum,ordinality)
           JOIN pg_attribute a ON a.attrelid=c.conrelid AND a.attnum=key.attnum
          WHERE c.conrelid='workflow_ops.workflow_endpoint_target_t'::regclass
            AND c.contype='p'",
    )
    .fetch_one(pool)
    .await
    .expect("read endpoint target primary-key columns");
    assert_eq!(columns, ["host_id", "binding_id", "endpoint_ref"]);
}

async fn schema_and_cancellation_case(pool: &PgPool) {
    assert_primary_key(pool).await;

    for table in [
        "wf_definition_version_t",
        "workflow_tool_publication_t",
        "workflow_publication_operation_t",
        "workflow_tool_binding_decision_t",
        "workflow_run_credential_t",
        "workflow_definition_grant_sync_t",
    ] {
        let exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
            .bind(format!("workflow_ops.{table}"))
            .fetch_one(pool)
            .await
            .expect("check new migration table");
        assert!(exists, "missing table workflow_ops.{table}");
    }
    for (table, column) in [
        ("workflow_tool_binding_t", "source_binding_id"),
        ("workflow_tool_binding_t", "revision_status"),
        ("workflow_tool_binding_t", "cancellation_policy"),
        ("workflow_action_authority_t", "credential_kind"),
        ("wf_definition_t", "source_revision"),
    ] {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM information_schema.columns
              WHERE table_schema='workflow_ops' AND table_name=$1 AND column_name=$2)",
        )
        .bind(table)
        .bind(column)
        .fetch_one(pool)
        .await
        .expect("check new migration column");
        assert!(exists, "missing column workflow_ops.{table}.{column}");
    }
    let function_exists: bool = sqlx::query_scalar(
        "SELECT to_regprocedure('workflow_ops.workflow_claim_idempotency_v2(uuid,uuid,character varying,character varying,uuid,character varying,character varying,uuid,character varying,character varying,timestamptz,timestamptz)') IS NOT NULL",
    )
    .fetch_one(pool)
    .await
    .expect("check v2 claim function");
    assert!(function_exists, "workflow_claim_idempotency_v2 is missing");

    let host = Uuid::new_v4();
    let definition = insert_definition(pool, host).await;
    let binding = Uuid::new_v4();
    insert_binding(
        pool,
        host,
        definition,
        Uuid::new_v4(),
        binding,
        "1.0.0",
        "approved",
        false,
        0,
    )
    .await;
    let default_policy: String = sqlx::query_scalar(
        "SELECT cancellation_policy FROM workflow_tool_binding_t WHERE host_id=$1 AND binding_id=$2",
    )
    .bind(host)
    .bind(binding)
    .fetch_one(pool)
    .await
    .expect("read cancellation default");
    assert_eq!(default_policy, "before-effects-only");
    for accepted in ["before-effects-only", "cooperative", "disabled"] {
        sqlx::query(
            "UPDATE workflow_tool_binding_t SET cancellation_policy=$3 WHERE host_id=$1 AND binding_id=$2",
        )
        .bind(host)
        .bind(binding)
        .bind(accepted)
        .execute(pool)
        .await
        .unwrap_or_else(|error| panic!("accepted binding cancellation value {accepted:?}: {error}"));
    }
    for rejected in ["BEFORE_EFFECTS_ONLY", "COOPERATIVE", "DISABLED", "unknown"] {
        let result = sqlx::query(
            "UPDATE workflow_tool_binding_t SET cancellation_policy=$3 WHERE host_id=$1 AND binding_id=$2",
        )
        .bind(host)
        .bind(binding)
        .bind(rejected)
        .execute(pool)
        .await;
        assert!(
            result.is_err(),
            "invalid cancellation value {rejected:?} was accepted"
        );
    }
}

#[tokio::test]
#[ignore = "requires the isolated migration 0018 scratch database"]
async fn fresh_case_01_schema_primary_key_and_cancellation_policy() {
    assert_mode("fresh");
    let (_, admin) = pools();
    schema_and_cancellation_case(&admin).await;
}

#[tokio::test]
#[ignore = "requires the isolated migration 0018 upgrade scratch database and pre-0018 fixture"]
async fn upgrade_case_01_schema_primary_key_and_cancellation_policy() {
    assert_mode("upgrade");
    let (_, admin) = pools();
    schema_and_cancellation_case(&admin).await;
}

#[tokio::test]
#[ignore = "requires the isolated migration 0018 upgrade scratch database and pre-0018 fixture"]
async fn upgrade_case_02_legacy_binding_and_authority_migration() {
    assert_mode("upgrade");
    let (_, admin) = pools();
    let host = Uuid::parse_str(PRE_HOST).unwrap();
    let rows: Vec<(String, bool)> = sqlx::query_as(
        "SELECT revision_status,active FROM workflow_tool_binding_t
          WHERE host_id=$1 AND tool_id=$2 AND binding_id IN ($3,$4)
          ORDER BY workflow_version",
    )
    .bind(host)
    .bind(Uuid::parse_str(PRE_TOOL).unwrap())
    .bind(Uuid::parse_str("a0180000-0000-4000-8000-000000000011").unwrap())
    .bind(Uuid::parse_str("a0180000-0000-4000-8000-000000000012").unwrap())
    .fetch_all(&admin)
    .await
    .expect("read migrated legacy bindings");
    assert_eq!(rows, [("legacy".into(), false), ("legacy".into(), false)]);
    let authority: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT DISTINCT run_id,credential_kind FROM workflow_action_authority_t
          WHERE host_id=$1 AND grant_id IN ($2,$3) ORDER BY run_id",
    )
    .bind(host)
    .bind(Uuid::parse_str(PRE_LONG_GRANT).unwrap())
    .bind(Uuid::parse_str("a0180000-0000-4000-8000-000000000043").unwrap())
    .fetch_all(&admin)
    .await
    .expect("read migrated authority rows");
    assert!(authority.contains(&(
        Uuid::parse_str(PRE_LONG_AUTHORITY_RUN).unwrap(),
        "long".into()
    )));
    assert!(authority.contains(&(
        Uuid::parse_str(PRE_BROKER_AUTHORITY_RUN).unwrap(),
        "broker".into()
    )));
    let deps: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM workflow_tool_dependency_t WHERE host_id=$1 AND outer_binding_id IN
           ('a0180000-0000-4000-8000-000000000011','a0180000-0000-4000-8000-000000000012')",
    )
    .bind(host)
    .fetch_one(&admin)
    .await
    .unwrap();
    let endpoints: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM workflow_endpoint_target_t WHERE host_id=$1 AND endpoint_ref=$2
           AND binding_id IN ('a0180000-0000-4000-8000-000000000011','a0180000-0000-4000-8000-000000000012')",
    )
    .bind(host)
    .bind(PRE_ENDPOINT)
    .fetch_one(&admin)
    .await
    .unwrap();
    assert_eq!(deps, 2);
    assert_eq!(endpoints, 2);
}

#[tokio::test]
#[ignore = "requires the isolated migration 0018 fresh scratch database"]
async fn fresh_case_03_active_pending_revision_is_rejected() {
    assert_mode("fresh");
    let (_, admin) = pools();
    let host = Uuid::new_v4();
    let definition = insert_definition(&admin, host).await;
    let binding = Uuid::new_v4();
    insert_binding(
        &admin,
        host,
        definition,
        Uuid::new_v4(),
        binding,
        "1.0.0",
        "approved",
        false,
        0,
    )
    .await;
    let result = sqlx::query("UPDATE workflow_tool_binding_t SET revision_status='pendingApproval',active=true WHERE host_id=$1 AND binding_id=$2")
        .bind(host).bind(binding).execute(&admin).await;
    assert!(result.is_err(), "active pending revision was accepted");
}

#[tokio::test]
#[ignore = "requires the isolated migration 0018 fresh scratch database"]
async fn fresh_case_04_second_active_revision_is_rejected() {
    assert_mode("fresh");
    let (_, admin) = pools();
    let host = Uuid::new_v4();
    let definition = insert_definition(&admin, host).await;
    let tool = Uuid::new_v4();
    insert_binding(
        &admin,
        host,
        definition,
        tool,
        Uuid::new_v4(),
        "1.0.0",
        "approved",
        true,
        0,
    )
    .await;
    let d = insert_definition(&admin, host).await;
    let result = sqlx::query("INSERT INTO workflow_tool_binding_t(host_id,binding_id,tool_id,wf_def_id,workflow_version,definition_digest,schema_digest,invocation_mode,sync_wait_ms,total_deadline_ms,execution_class,result_text_mode,idempotency_policy,delegation_policy,response_policy_digest,runtime_bounds,policy_digest,revision_status,binding_digest,approval_digest,source_binding_id,requested_by,requested_ts,active) SELECT host_id,$3,tool_id,$4,'1.0.0',definition_digest,schema_digest,invocation_mode,sync_wait_ms,total_deadline_ms,execution_class,result_text_mode,idempotency_policy,delegation_policy,response_policy_digest,runtime_bounds,policy_digest,'approved',binding_digest,approval_digest,$5,'fixture',now(),true FROM workflow_tool_binding_t WHERE host_id=$1 AND tool_id=$2")
        .bind(host).bind(tool).bind(Uuid::new_v4()).bind(d).bind(Uuid::new_v4())
        .execute(&admin).await;
    assert!(
        result.is_err(),
        "second active revision for a Tool was accepted"
    );
}

#[tokio::test]
#[ignore = "requires the isolated migration 0018 fresh scratch database"]
async fn fresh_case_05_second_pending_revision_is_rejected() {
    assert_mode("fresh");
    let (_, admin) = pools();
    let host = Uuid::new_v4();
    let definition = insert_definition(&admin, host).await;
    let tool = Uuid::new_v4();
    insert_binding(
        &admin,
        host,
        definition,
        tool,
        Uuid::new_v4(),
        "1.0.0",
        "pendingApproval",
        false,
        0,
    )
    .await;
    let d = insert_definition(&admin, host).await;
    let result = sqlx::query("INSERT INTO workflow_tool_binding_t(host_id,binding_id,tool_id,wf_def_id,workflow_version,definition_digest,schema_digest,invocation_mode,sync_wait_ms,total_deadline_ms,execution_class,result_text_mode,idempotency_policy,delegation_policy,response_policy_digest,runtime_bounds,policy_digest,revision_status,binding_digest,approval_digest,source_binding_id,requested_by,requested_ts,active) SELECT host_id,$3,tool_id,$4,'1.0.0',definition_digest,schema_digest,invocation_mode,sync_wait_ms,total_deadline_ms,execution_class,result_text_mode,idempotency_policy,delegation_policy,response_policy_digest,runtime_bounds,policy_digest,'pendingApproval',binding_digest,approval_digest,$5,'fixture',now(),false FROM workflow_tool_binding_t WHERE host_id=$1 AND tool_id=$2")
        .bind(host).bind(tool).bind(Uuid::new_v4()).bind(d).bind(Uuid::new_v4())
        .execute(&admin).await;
    assert!(
        result.is_err(),
        "second pending revision for a Tool was accepted"
    );
}

#[tokio::test]
#[ignore = "requires the isolated migration 0018 fresh scratch database"]
async fn fresh_case_06_endpoint_reference_is_isolated_per_revision() {
    assert_mode("fresh");
    let (_, admin) = pools();
    let host = Uuid::new_v4();
    let definition = insert_definition(&admin, host).await;
    let tool = Uuid::new_v4();
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    insert_binding(
        &admin, host, definition, tool, first, "1.0.0", "approved", false, 0,
    )
    .await;
    insert_binding(
        &admin, host, definition, tool, second, "1.0.0", "retired", false, 0,
    )
    .await;
    insert_endpoint(&admin, host, first, "same-endpoint").await;
    insert_endpoint(&admin, host, second, "same-endpoint").await;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM workflow_endpoint_target_t WHERE host_id=$1 AND endpoint_ref='same-endpoint'")
        .bind(host).fetch_one(&admin).await.unwrap();
    assert_eq!(count, 2);
}

#[tokio::test]
#[ignore = "requires the isolated migration 0018 fresh scratch database"]
async fn fresh_case_07_reject_decision_comment_and_runtime_privilege() {
    assert_mode("fresh");
    let (runtime, admin) = pools();
    let host = Uuid::new_v4();
    let definition = insert_definition(&admin, host).await;
    let binding = Uuid::new_v4();
    insert_binding(
        &admin,
        host,
        definition,
        Uuid::new_v4(),
        binding,
        "1.0.0",
        "approved",
        false,
        0,
    )
    .await;
    let invalid = sqlx::query("INSERT INTO workflow_tool_binding_decision_t(host_id,decision_id,tool_id,binding_id,action,actor,approval_digest) VALUES($1,$2,$3,$4,'reject','fixture',$5)")
        .bind(host).bind(Uuid::new_v4()).bind(Uuid::new_v4()).bind(binding).bind(digest('a'))
        .execute(&admin).await;
    assert!(
        invalid.is_err(),
        "reject decision without a comment was accepted"
    );
    let decision = Uuid::new_v4();
    sqlx::query("INSERT INTO workflow_tool_binding_decision_t(host_id,decision_id,tool_id,binding_id,action,actor,comment,approval_digest) VALUES($1,$2,$3,$4,'reject','fixture','reason',$5)")
        .bind(host).bind(decision).bind(Uuid::new_v4()).bind(binding).bind(digest('a'))
        .execute(&admin).await.unwrap();
    let update_privilege: bool = sqlx::query_scalar("SELECT has_table_privilege(current_user,'workflow_ops.workflow_tool_binding_decision_t','UPDATE')")
        .fetch_one(&runtime).await.expect("check privilege using runtime role");
    assert!(
        !update_privilege,
        "runtime role has UPDATE privilege on append-only decisions"
    );
    let denied = sqlx::query("UPDATE workflow_tool_binding_decision_t SET comment='changed' WHERE host_id=$1 AND decision_id=$2")
        .bind(host).bind(decision).execute(&runtime).await;
    assert!(
        denied.is_err(),
        "runtime role updated append-only decision row"
    );
}

#[tokio::test]
#[ignore = "requires the isolated migration 0018 fresh scratch database"]
async fn fresh_case_08_run_credential_checks_and_terminal_cleanup() {
    assert_mode("fresh");
    let (_, admin) = pools();
    let host = Uuid::new_v4();
    let definition = insert_definition(&admin, host).await;
    let binding = Uuid::new_v4();
    let tool = Uuid::new_v4();
    insert_binding(
        &admin, host, definition, tool, binding, "1.0.0", "approved", false, 0,
    )
    .await;
    let instance = Uuid::new_v4();
    let d = digest('a');
    insert_invocation(
        &admin, host, instance, binding, definition, tool, "ACCEPTED", false, "none", &d,
    )
    .await;
    let invalid = sqlx::query("INSERT INTO workflow_run_credential_t(host_id,workflow_instance_id,key_id,token_bytes,token_exp,expires_ts) VALUES($1,$2,'plaintext',decode('00','hex'),1,now()+interval '1 hour')")
        .bind(host).bind(instance).execute(&admin).await;
    assert!(
        invalid.is_err(),
        "plaintext run credential key id was accepted"
    );
    sqlx::query("INSERT INTO workflow_run_credential_t(host_id,workflow_instance_id,key_id,token_bytes,token_exp,expires_ts) VALUES($1,$2,'fixture-key',decode('00','hex'),1,now()+interval '1 hour')")
        .bind(host).bind(instance).execute(&admin).await.unwrap();
    sqlx::query("UPDATE workflow_invocation_t SET state='COMPLETED',terminal_ts=now() WHERE host_id=$1 AND workflow_instance_id=$2")
        .bind(host).bind(instance).execute(&admin).await.unwrap();
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workflow_run_credential_t WHERE host_id=$1 AND workflow_instance_id=$2)")
        .bind(host).bind(instance).fetch_one(&admin).await.unwrap();
    assert!(!exists, "terminal invocation retained its run credential");
}

#[tokio::test]
#[ignore = "requires the isolated migration 0018 fresh scratch database"]
async fn fresh_case_09_terminal_replay_window_trigger() {
    assert_mode("fresh");
    let (_, admin) = pools();
    let host = Uuid::new_v4();
    let definition = insert_definition(&admin, host).await;
    let tool = Uuid::new_v4();
    let failed_binding = Uuid::new_v4();
    let completed_binding = Uuid::new_v4();
    insert_binding(
        &admin,
        host,
        definition,
        tool,
        failed_binding,
        "1.0.0",
        "approved",
        false,
        600_000,
    )
    .await;
    insert_binding(
        &admin,
        host,
        definition,
        tool,
        completed_binding,
        "1.0.1",
        "retired",
        false,
        600_000,
    )
    .await;
    let d = digest('a');
    let failed_run = Uuid::new_v4();
    let completed_run = Uuid::new_v4();
    insert_invocation(
        &admin,
        host,
        failed_run,
        failed_binding,
        definition,
        tool,
        "ACCEPTED",
        false,
        "none",
        &d,
    )
    .await;
    insert_invocation(
        &admin,
        host,
        completed_run,
        completed_binding,
        definition,
        tool,
        "ACCEPTED",
        false,
        "none",
        &d,
    )
    .await;
    for (run, reservation) in [
        (failed_run, Uuid::new_v4()),
        (completed_run, Uuid::new_v4()),
    ] {
        insert_reservation(
            &admin,
            host,
            reservation,
            &digest(if run == failed_run { 'b' } else { 'c' }),
            tool,
            "client-a",
            "end-user",
            run,
            &d,
            &d,
            "1 hour",
        )
        .await;
    }
    sqlx::query("UPDATE workflow_invocation_t SET state='FAILED',terminal_ts=now(),effect_state='none' WHERE host_id=$1 AND workflow_instance_id=$2")
        .bind(host).bind(failed_run).execute(&admin).await.unwrap();
    sqlx::query("UPDATE workflow_invocation_t SET state='COMPLETED',terminal_ts=now() WHERE host_id=$1 AND workflow_instance_id=$2")
        .bind(host).bind(completed_run).execute(&admin).await.unwrap();
    let failed_window: (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>) =
        sqlx::query_as(
            "SELECT i.result_replay_until,v.terminal_ts FROM workflow_invocation_idempotency_t i
          JOIN workflow_invocation_t v USING(host_id,workflow_instance_id)
         WHERE i.host_id=$1 AND i.workflow_instance_id=$2",
        )
        .bind(host)
        .bind(failed_run)
        .fetch_one(&admin)
        .await
        .unwrap();
    assert_eq!(
        failed_window.0, failed_window.1,
        "clean FAILED run must have no replay extension"
    );
    let completed_window: (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>) =
        sqlx::query_as(
            "SELECT i.result_replay_until,v.terminal_ts FROM workflow_invocation_idempotency_t i
          JOIN workflow_invocation_t v USING(host_id,workflow_instance_id)
         WHERE i.host_id=$1 AND i.workflow_instance_id=$2",
        )
        .bind(host)
        .bind(completed_run)
        .fetch_one(&admin)
        .await
        .unwrap();
    assert_eq!(
        completed_window.0,
        completed_window.1 + chrono::Duration::minutes(10)
    );
}

#[tokio::test]
#[ignore = "requires the isolated migration 0018 fresh scratch database"]
async fn fresh_case_10_credential_kind_has_no_default() {
    assert_mode("fresh");
    let (_, admin) = pools();
    let default: Option<String> = sqlx::query_scalar(
        "SELECT column_default FROM information_schema.columns
          WHERE table_schema='workflow_ops' AND table_name='workflow_action_authority_t'
            AND column_name='credential_kind'",
    )
    .fetch_one(&admin)
    .await
    .unwrap();
    assert_eq!(default, None, "credential_kind must not have a default");
    let result = sqlx::query("INSERT INTO workflow_action_authority_t(host_id,run_id,grant_id,user_id,grant_generation,run_generation,budget_generation,active,deadline,action_limit) VALUES($1,$2,$3,$4,1,1,1,true,now()+interval '1 hour',1)")
        .bind(Uuid::new_v4()).bind(Uuid::new_v4()).bind(Uuid::new_v4()).bind(Uuid::new_v4())
        .execute(&admin).await;
    assert!(
        result.is_err(),
        "authority insert without credential_kind was accepted"
    );
}

#[tokio::test]
#[ignore = "requires the isolated migration 0018 upgrade scratch database and pre-0018 fixture"]
async fn upgrade_case_11_same_tool_version_accepts_three_new_revisions() {
    assert_mode("upgrade");
    let (_, admin) = pools();
    let host = Uuid::parse_str(PRE_HOST).unwrap();
    let definition = Uuid::parse_str(PRE_DEFINITION).unwrap();
    let tool = Uuid::parse_str(PRE_TOOL).unwrap();
    let mut bindings = Vec::new();
    for _ in 0..3 {
        let binding = Uuid::new_v4();
        insert_binding(
            &admin, host, definition, tool, binding, "1.0.0", "approved", false, 0,
        )
        .await;
        insert_endpoint(&admin, host, binding, PRE_ENDPOINT).await;
        bindings.push(binding);
    }
    let inserted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM workflow_tool_binding_t WHERE host_id=$1 AND tool_id=$2
            AND workflow_version='1.0.0' AND binding_id=ANY($3)",
    )
    .bind(host)
    .bind(tool)
    .bind(&bindings)
    .fetch_one(&admin)
    .await
    .unwrap();
    assert_eq!(
        inserted, 3,
        "three revisions for the same Tool/version must coexist"
    );
    let endpoints: i64 = sqlx::query_scalar("SELECT count(*) FROM workflow_endpoint_target_t WHERE host_id=$1 AND endpoint_ref=$2 AND binding_id=ANY($3)")
        .bind(host).bind(PRE_ENDPOINT).bind(&bindings).fetch_one(&admin).await.unwrap();
    assert_eq!(
        endpoints, 3,
        "each revision must retain its same-ref endpoint target"
    );
}

#[tokio::test]
#[ignore = "requires the isolated migration 0018 fresh scratch database"]
async fn fresh_case_12_expired_nonterminal_reservation_is_never_accepted_again() {
    assert_mode("fresh");
    let (_, admin) = pools();
    let host = Uuid::new_v4();
    let definition = insert_definition(&admin, host).await;
    let binding = Uuid::new_v4();
    let tool = Uuid::new_v4();
    insert_binding(
        &admin, host, definition, tool, binding, "1.0.0", "approved", false, 0,
    )
    .await;
    let instance = Uuid::new_v4();
    let d = digest('a');
    let input = digest('b');
    insert_invocation(
        &admin, host, instance, binding, definition, tool, "RUNNING", false, "none", &d,
    )
    .await;
    let scope = digest('c');
    let reservation = Uuid::new_v4();
    insert_reservation(
        &admin,
        host,
        reservation,
        &scope,
        tool,
        "client-a",
        "end-user",
        instance,
        &d,
        &input,
        "-1 hour",
    )
    .await;
    let replay = claim(
        &admin,
        host,
        Uuid::new_v4(),
        &scope,
        tool,
        "client-a",
        "end-user",
        Uuid::new_v4(),
        &d,
        &input,
    )
    .await;
    assert_eq!(replay, ("REPLAY".into(), instance, 1));
    let conflict = claim(
        &admin,
        host,
        Uuid::new_v4(),
        &scope,
        tool,
        "client-a",
        "end-user",
        Uuid::new_v4(),
        &d,
        &digest('d'),
    )
    .await;
    assert_eq!(conflict, ("CONFLICT".into(), instance, 1));
}

#[tokio::test]
#[ignore = "requires the isolated migration 0018 fresh scratch database"]
async fn fresh_case_13_expired_terminal_reservation_accepts_new_generations() {
    assert_mode("fresh");
    let (_, admin) = pools();
    let host = Uuid::new_v4();
    let definition = insert_definition(&admin, host).await;
    let binding = Uuid::new_v4();
    let tool = Uuid::new_v4();
    insert_binding(
        &admin, host, definition, tool, binding, "1.0.0", "approved", false, 0,
    )
    .await;
    let d = digest('a');
    let input = digest('b');
    let scope = digest('c');
    let first = Uuid::new_v4();
    let first_reservation = Uuid::new_v4();
    insert_invocation(
        &admin,
        host,
        first,
        binding,
        definition,
        tool,
        "COMPLETED",
        true,
        "none",
        &d,
    )
    .await;
    insert_reservation(
        &admin,
        host,
        first_reservation,
        &scope,
        tool,
        "client-a",
        "end-user",
        first,
        &d,
        &input,
        "1 hour",
    )
    .await;
    let client_conflict = claim(
        &admin,
        host,
        Uuid::new_v4(),
        &scope,
        tool,
        "client-b",
        "end-user",
        Uuid::new_v4(),
        &d,
        &input,
    )
    .await;
    assert_eq!(client_conflict.0, "CONFLICT");
    let definition_conflict = claim(
        &admin,
        host,
        Uuid::new_v4(),
        &scope,
        tool,
        "client-a",
        "end-user",
        Uuid::new_v4(),
        &digest('e'),
        &input,
    )
    .await;
    assert_eq!(definition_conflict.0, "CONFLICT");

    sqlx::query("UPDATE workflow_invocation_idempotency_t SET in_flight_until=now()-interval '1 second',result_replay_until=now()-interval '1 second' WHERE host_id=$1 AND reservation_id=$2")
        .bind(host).bind(first_reservation).execute(&admin).await.unwrap();
    let second = Uuid::new_v4();
    let second_reservation = Uuid::new_v4();
    let next = claim(
        &admin,
        host,
        second_reservation,
        &scope,
        tool,
        "client-b",
        "end-user",
        second,
        &d,
        &input,
    )
    .await;
    assert_eq!(next, ("ACCEPTED".into(), second, 2));

    insert_invocation(
        &admin,
        host,
        second,
        binding,
        definition,
        tool,
        "COMPLETED",
        true,
        "none",
        &d,
    )
    .await;
    sqlx::query("UPDATE workflow_invocation_idempotency_t SET in_flight_until=now()-interval '1 second',result_replay_until=now()-interval '1 second' WHERE host_id=$1 AND reservation_id=$2")
        .bind(host).bind(second_reservation).execute(&admin).await.unwrap();
    let third = Uuid::new_v4();
    let third_reservation = Uuid::new_v4();
    let new_definition = digest('f');
    let next_definition = claim(
        &admin,
        host,
        third_reservation,
        &scope,
        tool,
        "client-b",
        "end-user",
        third,
        &new_definition,
        &input,
    )
    .await;
    assert_eq!(next_definition, ("ACCEPTED".into(), third, 3));

    // Race two matching claims against one expired terminal reservation.
    let concurrent_scope = digest('9');
    let expired_instance = Uuid::new_v4();
    let expired_reservation = Uuid::new_v4();
    insert_invocation(
        &admin,
        host,
        expired_instance,
        binding,
        definition,
        tool,
        "COMPLETED",
        true,
        "none",
        &d,
    )
    .await;
    insert_reservation(
        &admin,
        host,
        expired_reservation,
        &concurrent_scope,
        tool,
        "client-a",
        "end-user",
        expired_instance,
        &d,
        &input,
        "-1 hour",
    )
    .await;

    // Hold the exact scope lock until both claim statements are waiting on it,
    // so the test always exercises simultaneous claim contention.
    let mut lock_holder = admin.begin().await.unwrap();
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended(
             'workflow_claim_idempotency_v2:' || $1::text || ':' || $2, 0))",
    )
    .bind(host)
    .bind(&concurrent_scope)
    .execute(&mut *lock_holder)
    .await
    .unwrap();

    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(3));
    let mut callers = Vec::new();
    for _ in 0..2 {
        let pool = admin.clone();
        let barrier = barrier.clone();
        let scope = concurrent_scope.clone();
        let definition_digest = d.clone();
        let input_digest = input.clone();
        let proposed_instance = Uuid::new_v4();
        let reservation = Uuid::new_v4();
        callers.push(tokio::spawn(async move {
            barrier.wait().await;
            claim_optional(
                &pool,
                host,
                reservation,
                &scope,
                tool,
                "client-a",
                "end-user",
                proposed_instance,
                &definition_digest,
                &input_digest,
            )
            .await
        }));
    }
    barrier.wait().await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let waiters: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_locks
                  WHERE locktype='advisory' AND mode='ExclusiveLock' AND NOT granted",
            )
            .fetch_one(&admin)
            .await
            .unwrap();
            if waiters >= 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("both matching callers wait on the scope lock");
    lock_holder.commit().await.unwrap();
    let results = [
        callers.remove(0).await.unwrap(),
        callers.remove(0).await.unwrap(),
    ];
    let accepted: Vec<_> = results
        .iter()
        .filter(|result| result.0 == "ACCEPTED")
        .collect();
    let replayed: Vec<_> = results
        .iter()
        .filter(|result| result.0 == "REPLAY")
        .collect();
    assert_eq!(
        accepted.len(),
        1,
        "exactly one concurrent claim is accepted"
    );
    assert_eq!(replayed.len(), 1, "the matching concurrent claim replays");
    let winning_instance = accepted[0]
        .1
        .expect("accepted claim returns a non-null instance id");
    let winning_generation = accepted[0]
        .2
        .expect("accepted claim returns a non-null generation");
    assert_eq!(replayed[0].1, Some(winning_instance));
    assert_eq!(replayed[0].2, Some(winning_generation));
    assert_eq!(winning_generation, 2);
    let active_reservations: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM workflow_invocation_idempotency_t
          WHERE host_id=$1 AND scope_digest=$2 AND active",
    )
    .bind(host)
    .bind(&concurrent_scope)
    .fetch_one(&admin)
    .await
    .unwrap();
    assert_eq!(active_reservations, 1);
}
