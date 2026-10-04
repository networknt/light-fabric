//! Required, explicitly invoked disposable PostgreSQL gates. No credentials or
//! production provider. Each test owns a fresh database on the loopback gate.
use super::*;
use crate::{durable_timer, invocation::*};
use workflow_invocation_contract::{StartInvocationRequest, canonical_sha256};

struct Db {
    pool: PgPool,
    database: super::scratch_database::ScratchDatabase,
}
impl Db {
    async fn new() -> Self {
        let url = std::env::var("P01_TIMER_TEST_DATABASE_URL")
            .expect("explicit P01 disposable database URL required");
        let parsed = url::Url::parse(&url).unwrap();
        assert_eq!(parsed.host_str(), Some("127.0.0.1"));
        assert_eq!(parsed.port(), Some(55431));
        assert_eq!(parsed.path(), "/p01_timer_gate");
        let database =
            super::scratch_database::ScratchDatabase::create(&parsed, "p01_timer", 12).await;
        let pool = database.pool.clone();
        sqlx::raw_sql("DO $$ BEGIN IF NOT EXISTS(SELECT FROM pg_roles WHERE rolname='operations_workflow_runtime') THEN CREATE ROLE operations_workflow_runtime; END IF; END $$").execute(&database.admin).await.unwrap();
        sqlx::raw_sql("DO $$ BEGIN IF NOT EXISTS(SELECT FROM pg_roles WHERE rolname='operations_workflow_migrator') THEN CREATE ROLE operations_workflow_migrator; END IF; END $$").execute(&database.admin).await.unwrap();
        sqlx::raw_sql("CREATE SCHEMA workflow_ops")
            .execute(&pool)
            .await
            .unwrap();
        database.migrate(None).await;
        sqlx::raw_sql("CREATE TABLE p01_mock_operation(process uuid PRIMARY KEY,operation uuid NOT NULL,starts integer NOT NULL DEFAULT 1,calls integer NOT NULL DEFAULT 1); CREATE TABLE p01_mock_poll(process uuid NOT NULL,attempt uuid PRIMARY KEY)")
            .execute(&pool).await.unwrap();
        Self { pool, database }
    }
    async fn close(self) {
        self.database.close().await;
    }
}

struct Run {
    host: Uuid,
    process: Uuid,
    task: Uuid,
    owner: Uuid,
}
async fn start(pool: &PgPool, yaml: &str, attempts: i64) -> Run {
    start_profile(pool, yaml, attempts, "workflow_backed").await
}
async fn start_profile(pool: &PgPool, yaml: &str, attempts: i64, profile: &str) -> Run {
    start_host(pool, yaml, attempts, profile, None).await
}
async fn start_host(
    pool: &PgPool,
    yaml: &str,
    attempts: i64,
    profile: &str,
    host: Option<Uuid>,
) -> Run {
    let definition: Value = serde_yaml::from_str(yaml).unwrap();
    let typed: WorkflowDefinition = serde_json::from_value(definition.clone()).unwrap();
    crate::runtime_definition::validate_runtime_definition(&typed, 1).unwrap();
    let first = typed.do_.entries[0].iter().next().unwrap();
    let kind = TaskExecutor::supported_task_type_name(first.1).unwrap();
    let host = host.unwrap_or_else(Uuid::new_v4);
    let process = Uuid::new_v4();
    let task = Uuid::new_v4();
    let binding = Uuid::new_v4();
    let def = Uuid::new_v4();
    let tool = Uuid::new_v4();
    let owner = Uuid::new_v4();
    let deadline = Utc::now() + chrono::Duration::seconds(60);
    let digest = canonical_sha256(&definition).unwrap();
    let input = json!({"requestKey":"fixture-operation"});
    let input_digest = canonical_sha256(&input).unwrap();
    let request: StartInvocationRequest = serde_json::from_value(json!({
        "contractVersion":1,"workflowInstanceId":Uuid::new_v4(),"stableToolRef":tool,
        "workflowDefinitionId":def,"workflowVersion":"1.0.0","definitionDigest":digest,
        "schemaDigest":digest,"policyDigest":digest,"responsePolicyDigest":digest,
        "mode":"async","executionClass":"standard","permitDepth":0,"deadlineTs":deadline,
        "canonicalInputProfile":"rfc8785-safe-json-v1","normalizedInputDigest":input_digest,
        "input":input,"callerClaims":{"sub":"fixture-owner"},"correlationId":"p01-gate",
        "idempotency":{"kind":"EXPLICIT","scopedKeyDigest":canonical_sha256(&json!([host,process])).unwrap(),"inputDigest":input_digest,"inFlightUntil":deadline,"resultReplayUntil":deadline+chrono::Duration::hours(1)},
        "budget":{"maximumTaskAttempts":attempts,"maximumNestedCalls":10,"maximumDelegationDepth":2,"maximumParallelism":1,"maximumRequestBytes":65536,"maximumIntermediateBytes":65536,"maximumResultBytes":65536,"maximumCostUnits":100}
    })).unwrap();
    sqlx::query("INSERT INTO wf_definition_t(host_id,wf_def_id,namespace,name,version,definition) VALUES($1,$2,'p01',$4,'1.0.0',$3)")
        .bind(host).bind(def).bind(serde_json::to_string(&definition).unwrap()).bind(format!("fixture-{def}")).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO workflow_tool_binding_t(host_id,binding_id,tool_id,wf_def_id,workflow_version,definition_digest,schema_digest,policy_digest,response_policy_digest,invocation_mode,sync_wait_ms,total_deadline_ms,execution_class,result_text_mode,idempotency_policy,delegation_policy,runtime_bounds,revision_status,source_binding_id,binding_digest,approval_digest,requested_by,requested_ts) VALUES($1,$2,$3,$4,'1.0.0',$5,$5,$5,$5,'async',1000,60000,'standard','compact-json','{}','{}','{}','approved',$2,$5,$5,'fixture',now())")
        .bind(host).bind(binding).bind(tool).bind(def).bind(&digest).execute(pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    accept_invocation(
        &mut tx,
        &AuthenticatedInvocationContext {
            host_id: host,
            principal_subject: "fixture-gateway",
            end_user_subject: "fixture-owner",
            update_user: "p01",
            user_authorization: None,
            user_authorization_exp: None,
        },
        &request,
        &PreparedInvocationStart {
            binding_id: Some(binding),
            process_id: process,
            initial_task_id: task,
            application_id: "timer",
            initial_task_name: first.0,
            initial_task_type: kind,
            definition_snapshot: &definition,
            execution_placement: "host",
            execution_profile_id: "",
            admission_profile: profile,
            policy_snapshot_id: None,
            task_policy_digest: digest.trim_start_matches("sha256:"),
            public_output_schema: None,
            expression_admission: None,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    Run {
        host,
        process,
        task,
        owner,
    }
}
const WAIT: &str = "document:\n  dsl: '1.0.3'\n  namespace: p01\n  name: timer\n  version: '1.0.0'\nevaluate:\n  language: cel\ndo:\n  - delay:\n      wait: PT1S\n      then: successor\n  - successor:\n      set:\n        result: done\n      end: true\n";

#[test]
fn runtime_validation_requires_literal_whole_seconds_and_standalone_wait() {
    for value in ["PT1S", "PT600S"] {
        let d: WorkflowDefinition = serde_yaml::from_str(&WAIT.replace("PT1S", value)).unwrap();
        crate::runtime_definition::validate_runtime_definition(&d, 1).unwrap();
        let wait = d.do_.entries[0].values().next().unwrap();
        assert_eq!(TaskExecutor::supported_task_type_name(wait), Some("wait"));
        assert_eq!(
            TaskExecutor::policy_task_kind(wait).unwrap(),
            TaskKind::Wait
        );
    }
    for value in ["PT0S", "PT601S", "PT01S", "PT1.0S", "PT1M", "'${ .delay }'"] {
        let d: WorkflowDefinition = serde_yaml::from_str(&WAIT.replace("PT1S", value)).unwrap();
        assert!(
            crate::runtime_definition::validate_runtime_definition(&d, 1).is_err(),
            "{value}"
        );
    }
    let yaml = "document:\n  dsl: '1.0.3'\n  namespace: p01\n  name: fork\n  version: '1.0.0'\nevaluate:\n  language: cel\ndo:\n  - fork:\n      fork:\n        compete: false\n        branches:\n          - delay:\n              wait: PT1S\n";
    let d: WorkflowDefinition = serde_yaml::from_str(yaml).unwrap();
    assert!(crate::runtime_definition::validate_runtime_definition(&d, 2).is_err());
}

#[tokio::test]
#[ignore = "requires P01_TIMER_TEST_DATABASE_URL at owned loopback disposable PostgreSQL"]
async fn wake_transaction_rollback_and_postwake_cancellation() {
    let db = Db::new().await;
    let r = start(&db.pool, WAIT, 20).await;
    let e = TaskExecutor::new(db.pool.clone());
    e.process_next_task(r.owner).await.unwrap();
    due(&db.pool, &r).await;
    sqlx::raw_sql("CREATE FUNCTION p01_fail_successor() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.wf_task_id='successor' THEN RAISE EXCEPTION 'P01 injected successor failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER p01_fail_successor BEFORE INSERT ON task_info_t FOR EACH ROW EXECUTE FUNCTION p01_fail_successor()")
        .execute(&db.pool).await.unwrap();
    assert_eq!(e.sweep_durable_timers().await.unwrap(), 0);
    assert_eq!(state(&db.pool, &r).await, "ARMED");
    assert_eq!(successors(&db.pool, &r).await, 0);
    sqlx::raw_sql(
        "DROP TRIGGER p01_fail_successor ON task_info_t; DROP FUNCTION p01_fail_successor()",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    drop(e);
    let e = TaskExecutor::new(db.pool.clone());
    assert_eq!(e.sweep_durable_timers().await.unwrap(), 1);
    let mut tx = db.pool.begin().await.unwrap();
    durable_timer::lock_parent(&mut tx, r.host, r.process)
        .await
        .unwrap();
    sqlx::query("UPDATE workflow_invocation_t SET state='CANCELLED',terminal_ts=clock_timestamp(),cancel_requested_ts=clock_timestamp() WHERE host_id=$1 AND process_id=$2").bind(r.host).bind(r.process).execute(&mut *tx).await.unwrap();
    sqlx::query("UPDATE process_info_t SET status_code='F' WHERE host_id=$1 AND process_id=$2")
        .bind(r.host)
        .bind(r.process)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE task_info_t SET status_code='F',locked='N',lease_owner=NULL,lease_expires_ts=NULL,result_code='WORKFLOW_CANCELLED' WHERE host_id=$1 AND process_id=$2 AND status_code IN('A','W')").bind(r.host).bind(r.process).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    assert!(!e.process_next_task(r.owner).await.unwrap());
    assert_eq!(state(&db.pool, &r).await, "FIRED");
    assert_eq!(successors(&db.pool, &r).await, 1);
    db.close().await;
}

#[tokio::test]
#[ignore = "requires P01_TIMER_TEST_DATABASE_URL at owned loopback disposable PostgreSQL"]
async fn revoked_authority_and_stale_task_fence_do_not_wake() {
    let db = Db::new().await;
    for authority in [true, false] {
        let r = start(&db.pool, WAIT, 20).await;
        let e = TaskExecutor::new(db.pool.clone());
        e.process_next_task(r.owner).await.unwrap();
        due(&db.pool, &r).await;
        if authority {
            sqlx::query("INSERT INTO workflow_action_authority_t(host_id,run_id,grant_id,user_id,grant_generation,run_generation,budget_generation,active,deadline,action_limit,credential_kind) SELECT host_id,workflow_instance_id,$3,$4,1,1,1,false,deadline_ts,10,'invoke' FROM workflow_invocation_t WHERE host_id=$1 AND process_id=$2").bind(r.host).bind(r.process).bind(Uuid::new_v4()).bind(Uuid::new_v4()).execute(&db.pool).await.unwrap();
        } else {
            sqlx::query("UPDATE task_info_t SET lease_fencing_token=lease_fencing_token+1 WHERE host_id=$1 AND task_id=$2").bind(r.host).bind(r.task).execute(&db.pool).await.unwrap();
        }
        assert_eq!(e.sweep_durable_timers().await.unwrap(), 0);
        assert_eq!(state(&db.pool, &r).await, "CANCELLED");
        assert_eq!(successors(&db.pool, &r).await, 0);
    }
    db.close().await;
}

#[tokio::test]
#[ignore = "requires P01_TIMER_TEST_DATABASE_URL at owned loopback disposable PostgreSQL"]
async fn deadline_is_rechecked_after_authority_lock_contention() {
    let db = Db::new().await;
    let r = start(&db.pool, WAIT, 20).await;
    let e = TaskExecutor::new(db.pool.clone());
    let claimed = e.claim_next_task(r.owner).await.unwrap().unwrap();
    let fence = claimed.host_lease.unwrap().fencing_token;
    sqlx::query("INSERT INTO workflow_action_authority_t(host_id,run_id,grant_id,user_id,grant_generation,run_generation,budget_generation,active,deadline,action_limit,credential_kind) SELECT host_id,workflow_instance_id,$3,$4,1,1,1,true,deadline_ts,10,'invoke' FROM workflow_invocation_t WHERE host_id=$1 AND process_id=$2")
        .bind(r.host).bind(r.process).bind(Uuid::new_v4()).bind(Uuid::new_v4()).execute(&db.pool).await.unwrap();
    let mut blocker = db.pool.begin().await.unwrap();
    sqlx::query("SELECT run_id FROM workflow_action_authority_t WHERE host_id=$1 FOR UPDATE")
        .bind(r.host)
        .fetch_one(&mut *blocker)
        .await
        .unwrap();
    sqlx::query("UPDATE process_info_t SET deadline_ts=clock_timestamp()+interval '200 milliseconds' WHERE host_id=$1 AND process_id=$2")
        .bind(r.host).bind(r.process).execute(&db.pool).await.unwrap();
    let mut arm = db.pool.begin().await.unwrap();
    let arm_future = durable_timer::arm(&mut arm, r.host, r.process, r.task, r.owner, fence, 1);
    let release = async {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        blocker.commit().await.unwrap();
    };
    let (result, ()) = tokio::join!(arm_future, release);
    result.unwrap();
    arm.commit().await.unwrap();
    assert_eq!(state(&db.pool, &r).await, "EXPIRED");
    assert_eq!(successors(&db.pool, &r).await, 0);
    db.close().await;
}

#[tokio::test]
#[ignore = "requires P01_TIMER_TEST_DATABASE_URL at owned loopback disposable PostgreSQL"]
async fn ordinary_deadline_and_timer_permissions_are_present() {
    let db = Db::new().await;
    let yaml = WAIT.replace("PT1S", "PT600S");
    let r = start(&db.pool, &yaml, 20).await;
    sqlx::query("UPDATE process_info_t SET started_ts=clock_timestamp()-interval '599 seconds' WHERE host_id=$1 AND process_id=$2")
        .bind(r.host).bind(r.process).execute(&db.pool).await.unwrap();
    let e = TaskExecutor::new(db.pool.clone());
    e.process_next_task(r.owner).await.unwrap();
    let correct: bool = sqlx::query_scalar("SELECT t.effective_deadline=i.deadline_ts AND t.wake_at=t.effective_deadline FROM workflow_task_timer_t t JOIN workflow_invocation_t i USING(host_id,process_id) WHERE t.host_id=$1 AND t.task_id=$2")
        .bind(r.host).bind(r.task).fetch_one(&db.pool).await.unwrap();
    assert!(correct);
    let privileges: (bool,bool,bool) = sqlx::query_as("SELECT has_table_privilege('operations_workflow_runtime','workflow_task_timer_t','SELECT'),has_table_privilege('operations_workflow_runtime','workflow_task_timer_t','INSERT'),has_table_privilege('operations_workflow_runtime','workflow_task_timer_t','UPDATE')")
        .fetch_one(&db.pool).await.unwrap();
    assert_eq!(privileges, (true, true, true));
    db.close().await;
}

#[tokio::test]
#[ignore = "requires P01_TIMER_TEST_DATABASE_URL at owned loopback disposable PostgreSQL"]
async fn revoked_poll_dependency_blocks_timer_successor() {
    let db = Db::new().await;
    let mut definition: Value =
        serde_yaml::from_str(include_str!("../tests/fixtures/p01-polling.yaml")).unwrap();
    let mut status = definition["do"][1].clone();
    status["status"]["then"] = json!("end");
    definition["do"] = json!([{"delay":{"wait":"PT1S","then":"status"}}, status]);
    let r = start(&db.pool, &serde_yaml::to_string(&definition).unwrap(), 20).await;
    let e = TaskExecutor::new(db.pool.clone());
    e.process_next_task(r.owner).await.unwrap();
    due(&db.pool, &r).await;
    sqlx::query("INSERT INTO workflow_tool_dependency_t(host_id,outer_binding_id,nested_tool_id,nested_tool_version,contract_digest,compatibility_policy,authorization_tool_name,authorization_endpoint_key,authorization_policy_digest,lifecycle_status,dispatch_target) SELECT host_id,binding_id,$3,'1.0.0',definition_digest,'exact','mock_operation_status','fixture',policy_digest,'revoked','{}' FROM workflow_invocation_t WHERE host_id=$1 AND process_id=$2")
        .bind(r.host).bind(r.process).bind(Uuid::new_v4()).execute(&db.pool).await.unwrap();
    assert_eq!(e.sweep_durable_timers().await.unwrap(), 0);
    assert_eq!(state(&db.pool, &r).await, "CANCELLED");
    let polls:i64 = sqlx::query_scalar("SELECT count(*) FROM task_info_t WHERE host_id=$1 AND process_id=$2 AND wf_task_id='status'")
        .bind(r.host).bind(r.process).fetch_one(&db.pool).await.unwrap();
    assert_eq!(polls, 0);
    db.close().await;
}
async fn state(pool: &PgPool, run: &Run) -> String {
    sqlx::query_scalar("SELECT state FROM workflow_task_timer_t WHERE host_id=$1 AND task_id=$2")
        .bind(run.host)
        .bind(run.task)
        .fetch_one(pool)
        .await
        .unwrap()
}
async fn due(pool: &PgPool, run: &Run) {
    // Controlled persisted clock boundary, avoiding flaky wall-clock sleeps.
    sqlx::query("UPDATE workflow_task_timer_t SET wake_at=clock_timestamp()-interval '1 second' WHERE host_id=$1 AND task_id=$2")
        .bind(run.host).bind(run.task).execute(pool).await.unwrap();
}
async fn successors(pool: &PgPool, run: &Run) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM task_info_t WHERE host_id=$1 AND process_id=$2 AND wf_task_id='successor'")
        .bind(run.host).bind(run.process).fetch_one(pool).await.unwrap()
}

#[tokio::test]
#[ignore = "requires P01_TIMER_TEST_DATABASE_URL at owned loopback disposable PostgreSQL"]
async fn restart_precision_worker_release_and_duplicate_wakes() {
    let db = Db::new().await;
    let r = start(&db.pool, WAIT, 20).await;
    let e = TaskExecutor::new(db.pool.clone());
    assert!(e.process_next_task(r.owner).await.unwrap());
    assert_eq!(state(&db.pool, &r).await, "ARMED");
    let timing: (chrono::DateTime<Utc>, chrono::DateTime<Utc>) = sqlx::query_as(
        "SELECT armed_at,wake_at FROM workflow_task_timer_t WHERE host_id=$1 AND task_id=$2",
    )
    .bind(r.host)
    .bind(r.task)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!((timing.1 - timing.0).num_microseconds(), Some(1_000_000));
    let released:(Option<Uuid>,Option<chrono::DateTime<Utc>>,Option<chrono::DateTime<Utc>>,String)=sqlx::query_as("SELECT lease_owner,lease_expires_ts,completed_ts,locked::text FROM task_info_t WHERE host_id=$1 AND task_id=$2").bind(r.host).bind(r.task).fetch_one(&db.pool).await.unwrap();
    assert_eq!(released, (None, None, None, "N".into()));
    let outstanding:i64=sqlx::query_scalar("SELECT count(*) FROM workflow_invocation_budget_reservation_t WHERE host_id=$1 AND state='RESERVED'").bind(r.host).fetch_one(&db.pool).await.unwrap();
    assert_eq!(outstanding, 0);
    assert_eq!(e.sweep_durable_timers().await.unwrap(), 0);
    drop(e);
    let a = TaskExecutor::new(db.pool.clone());
    let b = TaskExecutor::new(db.pool.clone());
    let persisted: (chrono::DateTime<Utc>, chrono::DateTime<Utc>) = sqlx::query_as(
        "SELECT armed_at,wake_at FROM workflow_task_timer_t WHERE host_id=$1 AND task_id=$2",
    )
    .bind(r.host)
    .bind(r.task)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(persisted, timing);
    // A second run executes while the first worker has released its timer.
    let other = start(&db.pool, WAIT, 20).await;
    assert!(a.process_next_task(other.owner).await.unwrap());
    due(&db.pool, &r).await;
    let (x, y) = tokio::join!(a.sweep_durable_timers(), b.sweep_durable_timers());
    assert_eq!(x.unwrap() + y.unwrap(), 1);
    assert_eq!(successors(&db.pool, &r).await, 1);
    assert_eq!(state(&db.pool, &r).await, "FIRED");
    let successor: Option<Uuid> = sqlx::query_scalar(
        "SELECT successor_task_id FROM workflow_task_timer_t WHERE host_id=$1 AND task_id=$2",
    )
    .bind(r.host)
    .bind(r.task)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert!(successor.is_some());
    assert_eq!(a.sweep_durable_timers().await.unwrap(), 0);
    db.close().await;
}

#[tokio::test]
#[ignore = "requires P01_TIMER_TEST_DATABASE_URL at owned loopback disposable PostgreSQL"]
async fn cancellation_and_deadline_equality_prevent_successors() {
    let db = Db::new().await;
    for cancel in [true, false] {
        let r = start(&db.pool, WAIT, 20).await;
        let e = TaskExecutor::new(db.pool.clone());
        e.process_next_task(r.owner).await.unwrap();
        if cancel {
            let mut tx = db.pool.begin().await.unwrap();
            // Same parent-first transaction and task terminalization as cancel API;
            // API authentication is outside P01.
            durable_timer::lock_parent(&mut tx, r.host, r.process)
                .await
                .unwrap();
            sqlx::query("UPDATE workflow_invocation_t SET state='CANCELLED',terminal_ts=clock_timestamp(),cancel_requested_ts=clock_timestamp() WHERE host_id=$1 AND process_id=$2").bind(r.host).bind(r.process).execute(&mut *tx).await.unwrap();
            sqlx::query(
                "UPDATE process_info_t SET status_code='F' WHERE host_id=$1 AND process_id=$2",
            )
            .bind(r.host)
            .bind(r.process)
            .execute(&mut *tx)
            .await
            .unwrap();
            sqlx::query("UPDATE task_info_t SET status_code='F',result_code='WORKFLOW_CANCELLED' WHERE host_id=$1 AND task_id=$2").bind(r.host).bind(r.task).execute(&mut *tx).await.unwrap();
            assert_eq!(
                sqlx::query_scalar::<_, String>(
                    "SELECT state FROM workflow_task_timer_t WHERE host_id=$1 AND task_id=$2"
                )
                .bind(r.host)
                .bind(r.task)
                .fetch_one(&mut *tx)
                .await
                .unwrap(),
                "CANCELLED"
            );
            tx.commit().await.unwrap();
        } else {
            sqlx::query("UPDATE workflow_task_timer_t SET wake_at=statement_timestamp()-interval '1 second',effective_deadline=statement_timestamp()-interval '1 second' WHERE host_id=$1 AND task_id=$2").bind(r.host).bind(r.task).execute(&db.pool).await.unwrap();
        }
        assert_eq!(e.sweep_durable_timers().await.unwrap(), 0);
        assert_eq!(successors(&db.pool, &r).await, 0);
        assert_eq!(
            state(&db.pool, &r).await,
            if cancel { "CANCELLED" } else { "EXPIRED" }
        );
    }
    db.close().await;
}

#[tokio::test]
#[ignore = "requires P01_TIMER_TEST_DATABASE_URL at owned loopback disposable PostgreSQL"]
async fn arm_rollback_fencing_and_absolute_deadline_clamp() {
    let db = Db::new().await;
    let r = start(&db.pool, WAIT, 20).await;
    let e = TaskExecutor::new(db.pool.clone());
    let claim = e.claim_next_task(r.owner).await.unwrap().unwrap();
    let fence = claim.host_lease.unwrap().fencing_token;
    let mut tx = db.pool.begin().await.unwrap();
    assert!(
        durable_timer::arm(&mut tx, r.host, r.process, r.task, r.owner, fence + 1, 1)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
    let mut tx = db.pool.begin().await.unwrap();
    durable_timer::arm(&mut tx, r.host, r.process, r.task, r.owner, fence, 1)
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM workflow_task_timer_t")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    sqlx::query("UPDATE process_info_t SET deadline_ts=clock_timestamp()+interval '500 milliseconds' WHERE host_id=$1 AND process_id=$2").bind(r.host).bind(r.process).execute(&db.pool).await.unwrap();
    let mut tx = db.pool.begin().await.unwrap();
    durable_timer::arm(&mut tx, r.host, r.process, r.task, r.owner, fence, 1)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let equal:bool=sqlx::query_scalar("SELECT wake_at=effective_deadline AND wake_at<armed_at+interval '1 second' FROM workflow_task_timer_t").fetch_one(&db.pool).await.unwrap();
    assert!(equal);
    tokio::time::sleep(std::time::Duration::from_millis(600)).await;
    assert_eq!(e.sweep_durable_timers().await.unwrap(), 0);
    assert_eq!(state(&db.pool, &r).await, "EXPIRED");
    assert_eq!(successors(&db.pool, &r).await, 0);
    db.close().await;
}

struct MockGateway {
    pool: PgPool,
    ready_after: i64,
    lose_reply: bool,
}
#[async_trait::async_trait]
impl crate::bound_mcp::Dispatch for MockGateway {
    async fn authorize_private_run(&self, _: Uuid, _: Uuid) -> Result<(), DynError> {
        Ok(())
    }
    async fn authorize_agent(
        &self,
        _: Uuid,
        _: Uuid,
        _: Uuid,
    ) -> Result<(chrono::DateTime<Utc>, i32, i32), DynError> {
        Err(io::Error::other("not a P01 agent fixture").into())
    }
    async fn call(
        &self,
        _: Uuid,
        process: Uuid,
        attempt: Uuid,
        alias: &str,
        params: Value,
    ) -> Result<Value, DynError> {
        match alias {
            "mock_operation_start" => {
                let (operation,calls):(Uuid,i32)=sqlx::query_as("INSERT INTO p01_mock_operation(process,operation) VALUES($1,$2) ON CONFLICT(process) DO UPDATE SET calls=p01_mock_operation.calls+1 RETURNING operation,calls").bind(process).bind(Uuid::new_v4()).fetch_one(&self.pool).await?;
                if self.lose_reply && calls == 1 {
                    return Err(
                        io::Error::other("mock lost operation reply after durable start").into(),
                    );
                }
                Ok(json!({"structuredContent":{"operationId":operation}}))
            }
            "mock_operation_status" => {
                let operation: Uuid =
                    sqlx::query_scalar("SELECT operation FROM p01_mock_operation WHERE process=$1")
                        .bind(process)
                        .fetch_one(&self.pool)
                        .await?;
                assert_eq!(params["arguments"]["operationId"], operation.to_string());
                sqlx::query("INSERT INTO p01_mock_poll(process,attempt) VALUES($1,$2)")
                    .bind(process)
                    .bind(attempt)
                    .execute(&self.pool)
                    .await?;
                let n: i64 =
                    sqlx::query_scalar("SELECT count(*) FROM p01_mock_poll WHERE process=$1")
                        .bind(process)
                        .fetch_one(&self.pool)
                        .await?;
                Ok(
                    json!({"structuredContent":{"state":if n>=self.ready_after{"READY"}else{"PENDING"},"resultId":"mock-result","resultDigest":format!("sha256:{}","a".repeat(64))}}),
                )
            }
            _ => Err(io::Error::other("unexpected P01 alias").into()),
        }
    }
}
fn mock_executor(pool: &PgPool, ready_after: i64, lose_reply: bool) -> TaskExecutor {
    let e = TaskExecutor::new(pool.clone());
    e.bound_mcp
        .set(Arc::new(MockGateway {
            pool: pool.clone(),
            ready_after,
            lose_reply,
        }))
        .ok()
        .unwrap();
    e
}
#[tokio::test]
#[ignore = "requires P01_TIMER_TEST_DATABASE_URL at owned loopback disposable PostgreSQL"]
async fn polling_restart_reply_loss_distinct_polls_and_bounded_attempts() {
    let db = Db::new().await;
    for bounded in [false, true] {
        let r = start(
            &db.pool,
            include_str!("../tests/fixtures/p01-polling.yaml"),
            if bounded { 8 } else { 30 },
        )
        .await;
        let mut e = mock_executor(&db.pool, if bounded { 100 } else { 3 }, true);
        let mut restarted = false;
        for _ in 0..40 {
            let status: String = sqlx::query_scalar(
                "SELECT status_code::text FROM process_info_t WHERE host_id=$1 AND process_id=$2",
            )
            .bind(r.host)
            .bind(r.process)
            .fetch_one(&db.pool)
            .await
            .unwrap();
            if matches!(status.as_str(), "C" | "F") {
                break;
            }
            e.process_next_task(r.owner).await.unwrap();
            let armed:bool=sqlx::query_scalar("SELECT EXISTS(SELECT FROM workflow_task_timer_t WHERE host_id=$1 AND process_id=$2 AND state='ARMED')").bind(r.host).bind(r.process).fetch_one(&db.pool).await.unwrap();
            if armed {
                if !restarted {
                    drop(e);
                    e = mock_executor(&db.pool, if bounded { 100 } else { 3 }, true);
                    restarted = true;
                }
                tokio::time::sleep(std::time::Duration::from_millis(1050)).await;
            }
        }
        assert!(restarted);
        let starts: (i32, i32) =
            sqlx::query_as("SELECT starts,calls FROM p01_mock_operation WHERE process=$1")
                .bind(r.process)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(starts, (1, 2));
        let polls: (i64, i64) = sqlx::query_as(
            "SELECT count(*),count(DISTINCT attempt) FROM p01_mock_poll WHERE process=$1",
        )
        .bind(r.process)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert!(polls.0 >= 2);
        assert_eq!(polls.0, polls.1);
        let status: String = sqlx::query_scalar(
            "SELECT status_code::text FROM process_info_t WHERE host_id=$1 AND process_id=$2",
        )
        .bind(r.host)
        .bind(r.process)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(status, if bounded { "F" } else { "C" });
        let terminal_tasks: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM task_info_t WHERE host_id=$1 AND process_id=$2",
        )
        .bind(r.host)
        .bind(r.process)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        for _ in 0..3 {
            e.process_next_task(r.owner).await.unwrap();
        }
        let after: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM task_info_t WHERE host_id=$1 AND process_id=$2",
        )
        .bind(r.host)
        .bind(r.process)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(terminal_tasks, after);
    }
    db.close().await;
}

async fn authority(pool: &PgPool, r: &Run) {
    sqlx::query("INSERT INTO workflow_action_authority_t(host_id,run_id,grant_id,user_id,grant_generation,run_generation,budget_generation,active,deadline,action_limit,credential_kind) SELECT host_id,workflow_instance_id,$3,$4,1,1,1,true,clock_timestamp()+interval '60 seconds',10,'invoke' FROM workflow_invocation_t WHERE host_id=$1 AND process_id=$2")
        .bind(r.host).bind(r.process).bind(Uuid::new_v4()).bind(Uuid::new_v4()).execute(pool).await.unwrap();
}
const ORDINARY: &str = "document:\n  dsl: '1.0.3'\n  namespace: p01\n  name: ordinary\n  version: '1.0.0'\nevaluate:\n  language: cel\ndo:\n  - work:\n      set:\n        result: ordinary\n      end: true\n";
async fn completed(pool: &PgPool, r: &Run) -> bool {
    sqlx::query_scalar(
        "SELECT status_code='C' FROM process_info_t WHERE host_id=$1 AND process_id=$2",
    )
    .bind(r.host)
    .bind(r.process)
    .fetch_one(pool)
    .await
    .unwrap()
}
#[tokio::test]
#[ignore = "requires P01_TIMER_TEST_DATABASE_URL at owned loopback disposable PostgreSQL"]
async fn review_locked_timer_never_blocks_another_host() {
    let db = Db::new().await;
    for same_host in [false, true] {
        for locked in [
            "future",
            "invocation",
            "process",
            "task",
            "timer",
            "authority",
        ] {
            let r = start(&db.pool, &WAIT.replace("PT1S", "PT600S"), 20).await;
            let e = TaskExecutor::new(db.pool.clone());
            authority(&db.pool, &r).await;
            e.process_next_task(r.owner).await.unwrap();
            if locked != "future" {
                due(&db.pool, &r).await;
            }
            let other = start_host(
                &db.pool,
                ORDINARY,
                20,
                "workflow_backed",
                same_host.then_some(r.host),
            )
            .await;
            assert_eq!(r.host == other.host, same_host);
            let mut lock = db.pool.begin().await.unwrap();
            let (table, column, id) = match locked {
                "future" | "invocation" => ("workflow_invocation_t", "process_id", r.process),
                "process" => ("process_info_t", "process_id", r.process),
                "task" => ("task_info_t", "task_id", r.task),
                "timer" => ("workflow_task_timer_t", "task_id", r.task),
                "authority" => ("workflow_action_authority_t", "run_id", sqlx::query_scalar("SELECT workflow_instance_id FROM workflow_invocation_t WHERE host_id=$1 AND process_id=$2").bind(r.host).bind(r.process).fetch_one(&db.pool).await.unwrap()),
                _ => unreachable!(),
            };
            sqlx::query(&format!(
                "SELECT {column} FROM {table} WHERE host_id=$1 AND {column}=$2 FOR UPDATE"
            ))
            .bind(r.host)
            .bind(id)
            .fetch_one(&mut *lock)
            .await
            .unwrap();
            let a = TaskExecutor::new(db.pool.clone());
            let b = TaskExecutor::new(db.pool.clone());
            let result = tokio::time::timeout(Duration::from_millis(750), async {
                tokio::join!(
                    a.process_next_task(Uuid::new_v4()),
                    b.process_next_task(Uuid::new_v4())
                )
            })
            .await;
            lock.rollback().await.unwrap();
            let (x, y) = result.unwrap_or_else(|_| {
                panic!("locked {locked} (same Host {same_host}) stalled unrelated workers")
            });
            let x = x.unwrap();
            let y = y.unwrap();
            assert!(x || y);
            assert!(completed(&db.pool, &other).await);
            assert_eq!(state(&db.pool, &r).await, "ARMED");
            let failures:i16 = sqlx::query_scalar("SELECT wake_failure_count FROM workflow_task_timer_t WHERE host_id=$1 AND task_id=$2")
                .bind(r.host).bind(r.task).fetch_one(&db.pool).await.unwrap();
            assert_eq!(failures, 0, "contention must not consume a retry");
            sqlx::query("UPDATE task_info_t SET status_code='F',result_code='WORKFLOW_CANCELLED' WHERE host_id=$1 AND task_id=$2")
                .bind(r.host).bind(r.task).execute(&db.pool).await.unwrap();
        }
    }
    db.close().await;
}

#[tokio::test]
#[ignore = "requires P01_TIMER_TEST_DATABASE_URL at owned loopback disposable PostgreSQL"]
async fn review_broken_timer_does_not_abort_normal_claiming() {
    let db = Db::new().await;
    let r = start(&db.pool, WAIT, 20).await;
    let mut e = TaskExecutor::new(db.pool.clone());
    e.process_next_task(r.owner).await.unwrap();
    due(&db.pool, &r).await;
    let original: (chrono::DateTime<Utc>, chrono::DateTime<Utc>, Option<chrono::DateTime<Utc>>) =
        sqlx::query_as("SELECT armed_at,wake_at,effective_deadline FROM workflow_task_timer_t WHERE host_id=$1 AND task_id=$2")
        .bind(r.host).bind(r.task).fetch_one(&db.pool).await.unwrap();
    sqlx::raw_sql("CREATE FUNCTION p01_fail_timer() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.wf_task_id='successor' THEN RAISE EXCEPTION 'P01 injected persistent timer failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER p01_fail_timer BEFORE INSERT ON task_info_t FOR EACH ROW EXECUTE FUNCTION p01_fail_timer()")
        .execute(&db.pool).await.unwrap();
    for failure in 1..=3 {
        if failure > 1 {
            tokio::time::sleep(Duration::from_millis(if failure == 2 {
                1100
            } else {
                2100
            }))
            .await;
            drop(e);
            e = TaskExecutor::new(db.pool.clone());
        }
        let other = start_host(
            &db.pool,
            ORDINARY,
            20,
            "workflow_backed",
            if failure == 2 { Some(r.host) } else { None },
        )
        .await;
        assert!(
            tokio::time::timeout(Duration::from_millis(750), e.process_next_task(other.owner))
                .await
                .expect("failed timer must not stall ordinary worker")
                .expect("failed timer must not abort normal claiming")
        );
        assert!(completed(&db.pool, &other).await);
        let recorded: (i16,bool,Option<String>) = sqlx::query_as("SELECT wake_failure_count,retry_after_ts>clock_timestamp(),last_failure_code FROM workflow_task_timer_t WHERE host_id=$1 AND task_id=$2")
            .bind(r.host).bind(r.task).fetch_one(&db.pool).await.unwrap();
        assert_eq!(
            recorded,
            (failure, true, Some("WORKFLOW_TIMER_WAKE_FAILED".into()))
        );
        // Another iteration during backoff neither retries nor charges failure.
        assert!(!e.process_next_task(Uuid::new_v4()).await.unwrap());
        let count: i16 = sqlx::query_scalar(
            "SELECT wake_failure_count FROM workflow_task_timer_t WHERE host_id=$1 AND task_id=$2",
        )
        .bind(r.host)
        .bind(r.task)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(count, failure);
        assert_eq!(
            state(&db.pool, &r).await,
            if failure == 3 { "FAILED" } else { "ARMED" }
        );
    }
    assert_eq!(successors(&db.pool, &r).await, 0);
    let final_intent: (chrono::DateTime<Utc>, chrono::DateTime<Utc>, Option<chrono::DateTime<Utc>>) =
        sqlx::query_as("SELECT armed_at,wake_at,effective_deadline FROM workflow_task_timer_t WHERE host_id=$1 AND task_id=$2")
        .bind(r.host).bind(r.task).fetch_one(&db.pool).await.unwrap();
    assert_eq!(original, final_intent);
    let error: Option<Value> = sqlx::query_scalar(
        "SELECT normalized_error FROM workflow_invocation_t WHERE host_id=$1 AND process_id=$2",
    )
    .bind(r.host)
    .bind(r.process)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(error.unwrap()["code"], "WORKFLOW_TIMER_FAILED");
    db.close().await;
}

#[tokio::test]
#[ignore = "requires P01_TIMER_TEST_DATABASE_URL at owned loopback disposable PostgreSQL"]
async fn review_private_lifetime_ignores_expired_invocation_deadline() {
    let db = Db::new().await;
    let r = start_profile(&db.pool, WAIT, 20, "portal_execution").await;
    authority(&db.pool, &r).await;
    sqlx::query("UPDATE workflow_invocation_t SET deadline_ts=clock_timestamp()-interval '1 second' WHERE host_id=$1 AND process_id=$2")
        .bind(r.host).bind(r.process).execute(&db.pool).await.unwrap();
    let e = TaskExecutor::new(db.pool.clone());
    e.process_next_task(r.owner).await.unwrap();
    assert_eq!(state(&db.pool, &r).await, "ARMED");
    due(&db.pool, &r).await;
    assert_eq!(e.sweep_durable_timers().await.unwrap(), 1);
    assert_eq!(successors(&db.pool, &r).await, 1);
    assert!(e.process_next_task(r.owner).await.unwrap());
    assert!(completed(&db.pool, &r).await);
    db.close().await;
}

#[tokio::test]
#[ignore = "requires P01_TIMER_TEST_DATABASE_URL at owned loopback disposable PostgreSQL"]
async fn review_private_lifetime_still_enforces_real_deadlines_and_revocation() {
    let db = Db::new().await;
    for expired in [
        "process",
        "task",
        "authority",
        "revoked",
        "old-process",
        "ordinary-invocation",
    ] {
        let yaml = WAIT;
        let private = expired != "ordinary-invocation";
        let r = start_profile(
            &db.pool,
            &yaml,
            20,
            if private {
                "portal_execution"
            } else {
                "workflow_backed"
            },
        )
        .await;
        authority(&db.pool, &r).await;
        if private {
            sqlx::query("UPDATE workflow_invocation_t SET deadline_ts=clock_timestamp()-interval '1 second' WHERE host_id=$1 AND process_id=$2")
                .bind(r.host).bind(r.process).execute(&db.pool).await.unwrap();
        }
        if expired == "old-process" {
            sqlx::query("UPDATE process_info_t SET started_ts=clock_timestamp()-interval '700 seconds' WHERE host_id=$1 AND process_id=$2")
                .bind(r.host).bind(r.process).execute(&db.pool).await.unwrap();
        }
        let e = TaskExecutor::new(db.pool.clone());
        e.process_next_task(r.owner).await.unwrap();
        assert_eq!(state(&db.pool, &r).await, "ARMED");
        match expired {
            "process" => {
                sqlx::query("UPDATE process_info_t SET deadline_ts=clock_timestamp()-interval '1 second' WHERE host_id=$1 AND process_id=$2").bind(r.host).bind(r.process).execute(&db.pool).await.unwrap();
            }
            "task" => {
                sqlx::query("UPDATE task_info_t SET deadline_ts=clock_timestamp()-interval '1 second' WHERE host_id=$1 AND task_id=$2").bind(r.host).bind(r.task).execute(&db.pool).await.unwrap();
            }
            "authority" => {
                sqlx::query("UPDATE workflow_action_authority_t SET deadline=clock_timestamp()-interval '1 second' WHERE host_id=$1").bind(r.host).execute(&db.pool).await.unwrap();
            }
            "revoked" => {
                sqlx::query("UPDATE workflow_action_authority_t SET active=false WHERE host_id=$1")
                    .bind(r.host)
                    .execute(&db.pool)
                    .await
                    .unwrap();
            }
            "old-process" => {
                due(&db.pool, &r).await;
                assert_eq!(e.sweep_durable_timers().await.unwrap(), 1);
                assert_eq!(state(&db.pool, &r).await, "FIRED");
                assert_eq!(successors(&db.pool, &r).await, 1);
                continue;
            }
            "ordinary-invocation" => {
                sqlx::query("UPDATE workflow_invocation_t SET deadline_ts=clock_timestamp()-interval '1 second' WHERE host_id=$1 AND process_id=$2").bind(r.host).bind(r.process).execute(&db.pool).await.unwrap();
            }
            _ => unreachable!(),
        }
        assert_eq!(e.sweep_durable_timers().await.unwrap(), 0);
        assert_eq!(
            state(&db.pool, &r).await,
            if expired == "revoked" {
                "CANCELLED"
            } else {
                "EXPIRED"
            },
            "{expired}"
        );
        assert_eq!(successors(&db.pool, &r).await, 0);
    }
    db.close().await;
}
#[tokio::test]
#[ignore = "requires P01_TIMER_TEST_DATABASE_URL at owned loopback disposable PostgreSQL"]
async fn review_private_uses_current_authority_without_resetting_timer_intent() {
    let db = Db::new().await;
    let r = start_profile(&db.pool, WAIT, 20, "portal_execution").await;
    authority(&db.pool, &r).await;
    sqlx::query("UPDATE workflow_action_authority_t SET deadline=clock_timestamp()+interval '500 milliseconds' WHERE host_id=$1").bind(r.host).execute(&db.pool).await.unwrap();
    sqlx::query("UPDATE workflow_invocation_t SET deadline_ts=clock_timestamp()-interval '1 second' WHERE host_id=$1 AND process_id=$2").bind(r.host).bind(r.process).execute(&db.pool).await.unwrap();
    let e = TaskExecutor::new(db.pool.clone());
    e.process_next_task(r.owner).await.unwrap();
    assert_eq!(state(&db.pool, &r).await, "ARMED");
    let original:(chrono::DateTime<Utc>,chrono::DateTime<Utc>,Option<chrono::DateTime<Utc>>)=sqlx::query_as("SELECT armed_at,wake_at,effective_deadline FROM workflow_task_timer_t WHERE host_id=$1 AND task_id=$2").bind(r.host).bind(r.task).fetch_one(&db.pool).await.unwrap();
    assert_eq!((original.1 - original.0).num_seconds(), 1);
    assert!(original.2.is_none());
    sqlx::query("UPDATE workflow_action_authority_t SET deadline=clock_timestamp()+interval '60 seconds' WHERE host_id=$1").bind(r.host).execute(&db.pool).await.unwrap();
    drop(e);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let e = TaskExecutor::new(db.pool.clone());
    assert_eq!(e.sweep_durable_timers().await.unwrap(), 1);
    let recovered:(chrono::DateTime<Utc>,chrono::DateTime<Utc>,Option<chrono::DateTime<Utc>>)=sqlx::query_as("SELECT armed_at,wake_at,effective_deadline FROM workflow_task_timer_t WHERE host_id=$1 AND task_id=$2").bind(r.host).bind(r.task).fetch_one(&db.pool).await.unwrap();
    assert_eq!(original, recovered);
    assert_eq!(successors(&db.pool, &r).await, 1);
    db.close().await;
}

#[tokio::test]
#[ignore = "requires P01_TIMER_TEST_DATABASE_URL at owned loopback disposable PostgreSQL"]
async fn review_slow_timer_and_scan_failure_do_not_gate_ordinary_work() {
    let db = Db::new().await;
    let r = start(&db.pool, WAIT, 20).await;
    let e = TaskExecutor::new(db.pool.clone());
    e.process_next_task(r.owner).await.unwrap();
    due(&db.pool, &r).await;
    sqlx::raw_sql("CREATE FUNCTION p01_slow_timer() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.wf_task_id='successor' THEN PERFORM pg_sleep(5); END IF; RETURN NEW; END $$; CREATE TRIGGER p01_slow_timer BEFORE INSERT ON task_info_t FOR EACH ROW EXECUTE FUNCTION p01_slow_timer()")
        .execute(&db.pool).await.unwrap();
    let other = start(&db.pool, ORDINARY, 20).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(750), e.process_next_task(other.owner))
            .await
            .expect("statement timeout must isolate slow timer")
            .unwrap()
    );
    assert!(completed(&db.pool, &other).await);
    assert_eq!(successors(&db.pool, &r).await, 0);
    let failures: i16 = sqlx::query_scalar(
        "SELECT wake_failure_count FROM workflow_task_timer_t WHERE host_id=$1 AND task_id=$2",
    )
    .bind(r.host)
    .bind(r.task)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(failures, 1);
    // Deliberately break candidate enumeration inside this disposable schema.
    // Normal task claiming must run even when the entire scan returns an error.
    sqlx::query("ALTER TABLE workflow_task_timer_t RENAME TO p01_unavailable_timer")
        .execute(&db.pool)
        .await
        .unwrap();
    let other = start(&db.pool, ORDINARY, 20).await;
    assert!(
        tokio::time::timeout(Duration::from_millis(750), e.process_next_task(other.owner))
            .await
            .expect("scan failure must not gate ordinary work")
            .unwrap()
    );
    assert!(completed(&db.pool, &other).await);
    sqlx::query("ALTER TABLE p01_unavailable_timer RENAME TO workflow_task_timer_t")
        .execute(&db.pool)
        .await
        .unwrap();
    db.close().await;
}

async fn start_native(pool: &PgPool, yaml: &str, attempts: i64) -> Run {
    let definition: Value = serde_yaml::from_str(yaml).unwrap();
    let typed: WorkflowDefinition = serde_json::from_value(definition.clone()).unwrap();
    crate::runtime_definition::validate_runtime_definition(&typed, 1).unwrap();
    let first = typed.do_.entries[0].iter().next().unwrap();
    let kind = TaskExecutor::supported_task_type_name(first.1).unwrap();
    let host = Uuid::new_v4();
    let process = Uuid::new_v4();
    let task = Uuid::new_v4();
    let def = Uuid::new_v4();
    let tool = Uuid::new_v4();
    let owner = Uuid::new_v4();
    let deadline = Utc::now() + chrono::Duration::seconds(60);
    let digest = canonical_sha256(&definition).unwrap();
    let input = json!({"requestKey":"fixture-operation"});
    let input_digest = canonical_sha256(&input).unwrap();
    let request: StartInvocationRequest = serde_json::from_value(json!({
        "contractVersion":1,"workflowInstanceId":Uuid::new_v4(),"stableToolRef":tool,
        "workflowDefinitionId":def,"workflowVersion":"1.0.0","definitionDigest":digest,
        "schemaDigest":digest,"policyDigest":digest,"responsePolicyDigest":digest,
        "mode":"async","executionClass":"standard","permitDepth":0,"deadlineTs":deadline,
        "canonicalInputProfile":"rfc8785-safe-json-v1","normalizedInputDigest":input_digest,
        "input":input,"callerClaims":{"sub":"fixture-owner"},"correlationId":"p01-gate",
        "idempotency":{"kind":"EXPLICIT","scopedKeyDigest":canonical_sha256(&json!([host,process])).unwrap(),"inputDigest":input_digest,"inFlightUntil":deadline,"resultReplayUntil":deadline+chrono::Duration::hours(1)},
        "budget":{"maximumTaskAttempts":attempts,"maximumNestedCalls":10,"maximumDelegationDepth":2,"maximumParallelism":1,"maximumRequestBytes":65536,"maximumIntermediateBytes":65536,"maximumResultBytes":65536,"maximumCostUnits":100}
    })).unwrap();
    sqlx::query("INSERT INTO wf_definition_t(host_id,wf_def_id,namespace,name,version,definition) VALUES($1,$2,'p01',$4,'1.0.0',$3)")
        .bind(host).bind(def).bind(serde_json::to_string(&definition).unwrap()).bind(format!("fixture-{def}")).execute(pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    accept_invocation(
        &mut tx,
        &AuthenticatedInvocationContext {
            host_id: host,
            principal_subject: "fixture-gateway",
            end_user_subject: "fixture-owner",
            update_user: "p01",
            user_authorization: None,
            user_authorization_exp: None,
        },
        &request,
        &PreparedInvocationStart {
            binding_id: None,
            process_id: process,
            initial_task_id: task,
            application_id: "timer",
            initial_task_name: first.0,
            initial_task_type: kind,
            definition_snapshot: &definition,
            execution_placement: "host",
            execution_profile_id: "",
            admission_profile: "portal_execution",
            policy_snapshot_id: None,
            task_policy_digest: digest.trim_start_matches("sha256:"),
            public_output_schema: None,
            expression_admission: None,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    Run {
        host,
        process,
        task,
        owner,
    }
}

#[tokio::test]
#[ignore = "requires P01_TIMER_TEST_DATABASE_URL at owned loopback disposable PostgreSQL"]
async fn native_null_binding_wait() {
    let db = Db::new().await;
    let r = start_native(&db.pool, WAIT, 20).await;
    authority(&db.pool, &r).await;
    let binding: Option<Uuid> = sqlx::query_scalar(
        "SELECT binding_id FROM workflow_invocation_t WHERE host_id=$1 AND process_id=$2",
    )
    .bind(r.host)
    .bind(r.process)
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(binding, None);
    let e = TaskExecutor::new(db.pool.clone());
    assert!(e.process_next_task(r.owner).await.unwrap());
    assert_eq!(state(&db.pool, &r).await, "ARMED");
    due(&db.pool, &r).await;
    drop(e);
    let e = TaskExecutor::new(db.pool.clone());
    assert_eq!(e.sweep_durable_timers().await.unwrap(), 1);
    assert_eq!(successors(&db.pool, &r).await, 1);
    assert!(e.process_next_task(r.owner).await.unwrap());
    assert!(completed(&db.pool, &r).await);
    assert_eq!(e.sweep_durable_timers().await.unwrap(), 0);
    db.close().await;
}
