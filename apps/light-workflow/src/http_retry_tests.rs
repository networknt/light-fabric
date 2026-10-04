//! Durable HTTP retry regressions. Synthetic operational state, full repository
//! migrations, and actual executor completion/claim/reconciliation paths.
//! Requires an explicitly owned scratch database; all child databases are UUID named.
use super::*;
use sqlx::postgres::PgPoolOptions;
use workflow_expression::WorkerConfig;

fn policy() -> Value {
    json!({"limit":{"attempt":{"count":3}},"delay":{"seconds":2}})
}
fn simple(task: Value, next: bool) -> Value {
    let mut tasks = vec![json!({"fetch":task})];
    if next {
        tasks.push(json!({"next":{"set":{"done":true}}}));
    }
    json!({"document":{"dsl":"1.0.3","namespace":"retry","name":"http-retry","version":"1.0.0","metadata":{"lightExpressionProfile":"cel-workflow-v2"}},"evaluate":{"language":"cel"},"do":tasks})
}
fn definition(name: &str) -> Value {
    if name == "author" {
        json!({"document":{"dsl":"1.0.3","namespace":"retry","name":"native-compatibility","version":"1.0.0","metadata":{"lightExpressionProfile":"cel-workflow-v2"}},"evaluate":{"language":"cel"},"do":[
            {"author":{"call":"agent","with":{"agent":"fixture-agent","mode":"service","input":{"fixture":true},"outputSchema":{"type":"object","required":["finalMessage","workspace","codingThread"]}},"retry":policy(),"export":{"as":{"designResult":"${ output }"}}}},
            {"validate":{"assert":{"value":"${ jsonEncode(context.designResult.finalMessage).startsWith('\"') }","equals":true}}}
        ]})
    } else {
        simple(
            json!({"call":"http","with":{"method":"GET","endpoint":"http://127.0.0.1:1/never-dispatched"},"retry":policy(),"export":{"as":{"received":"${ output }"}}}),
            true,
        )
    }
}

struct Fixture {
    pool: PgPool,
    admin: PgPool,
    database: String,
    executor: TaskExecutor,
    claimed: ClaimedTask,
    deadline: chrono::DateTime<Utc>,
}
impl Fixture {
    async fn new(name: &str, context: Value) -> Self {
        // Explicit scratch URL only: never fall back to ambient application URLs.
        let url = env::var("W4_TEST_DATABASE_URL")
            .expect("explicit owned workflow_retry_* W4_TEST_DATABASE_URL required");
        let parsed = url::Url::parse(&url).unwrap();
        assert_eq!(parsed.scheme(), "postgres");
        assert_eq!(parsed.host_str(), Some("127.0.0.1"));
        assert!(parsed.path().starts_with("/workflow_retry_"));
        assert!(parsed.query().is_none());
        let admin = PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .unwrap();
        let database = format!("workflow_retry_case_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE {database}"))
            .execute(&admin)
            .await
            .unwrap();
        let mut child = parsed;
        child.set_path(&format!("/{database}"));
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .after_connect(|c, _| {
                Box::pin(async move {
                    sqlx::query("SET search_path TO workflow_ops,pg_catalog")
                        .execute(c)
                        .await?;
                    Ok(())
                })
            })
            .connect(child.as_str())
            .await
            .unwrap();
        sqlx::raw_sql("CREATE SCHEMA workflow_ops AUTHORIZATION operations_workflow_migrator")
            .execute(&pool)
            .await
            .unwrap();
        let migrations = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../crates/workflow-store/migrations/workflow-postgres");
        let mut files: Vec<_> = std::fs::read_dir(migrations)
            .unwrap()
            .map(|x| x.unwrap().path())
            .filter(|x| x.extension().is_some_and(|x| x == "sql"))
            .collect();
        files.sort();
        let mut conn = pool.acquire().await.unwrap();
        sqlx::raw_sql("SET ROLE operations_workflow_migrator")
            .execute(&mut *conn)
            .await
            .unwrap();
        for file in files {
            sqlx::raw_sql(&std::fs::read_to_string(&file).unwrap())
                .execute(&mut *conn)
                .await
                .unwrap_or_else(|e| panic!("{}: {e}", file.display()));
        }
        sqlx::raw_sql("RESET ROLE")
            .execute(&mut *conn)
            .await
            .unwrap();
        drop(conn);
        let snapshot = definition(name);
        let serialized = serde_json::to_string(&snapshot).unwrap();
        let definition: WorkflowDefinition = serde_json::from_value(snapshot.clone()).unwrap();
        let task_def = definition
            .do_
            .entries
            .iter()
            .find_map(|x| x.get(name))
            .unwrap();
        let kind = TaskExecutor::supported_task_type_name(task_def).unwrap();
        let host = Uuid::new_v4();
        let process = Uuid::new_v4();
        let task = Uuid::new_v4();
        let def = Uuid::new_v4();
        let instance = Uuid::new_v4();
        let owner = Uuid::new_v4();
        let binding = Uuid::new_v4();
        let digest = canonical_sha256(&snapshot).unwrap();
        let wire_digest = format!("sha256:{digest}");
        let deadline = Utc::now() + chrono::Duration::minutes(5);
        let input = json!({"fixture":true});
        // Test-only operational state, never publication or real caller authority.
        sqlx::query("INSERT INTO wf_definition_t(host_id,wf_def_id,namespace,name,version,definition) VALUES($1,$2,'retry','fixture','0.1.0',$3)")
            .bind(host).bind(def).bind(&serialized).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO workflow_tool_binding_t(host_id,binding_id,tool_id,wf_def_id,workflow_version,definition_digest,schema_digest,invocation_mode,sync_wait_ms,total_deadline_ms,execution_class,result_text_mode,idempotency_policy,delegation_policy,response_policy_digest,runtime_bounds,policy_digest,active) VALUES($1,$2,$2,$3,'0.1.0',$4,$4,'async',1,300000,'standard','compact-json','{}','{}',$4,'{}',$4,false)")
            .bind(host).bind(binding).bind(def).bind(&wire_digest).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO process_info_t(host_id,process_id,wf_def_id,wf_instance_id,app_id,process_type,status_code,ex_trigger_ts,definition_snapshot,definition_digest,expression_profile,context_data,input_data,deadline_ts) VALUES($1,$2,$3,$4,'retry-fixture','retry','A',now(),$5,$6,'cel-workflow-v2',$7,$8,$9)")
            .bind(host).bind(process).bind(def).bind(instance.to_string()).bind(&snapshot).bind(&digest).bind(&context).bind(&input).bind(deadline).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO task_info_t(host_id,task_id,process_id,task_type,wf_instance_id,wf_task_id,status_code,locked,priority,task_input,lease_owner,lease_fencing_token,lease_expires_ts,deadline_ts) VALUES($1,$2,$3,$4,$5,$6,'A','Y',0,$7,$8,1,$9,$9)")
            .bind(host).bind(task).bind(process).bind(kind).bind(instance.to_string()).bind(name).bind(&context).bind(owner).bind(deadline).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO workflow_invocation_t(host_id,workflow_instance_id,binding_id,process_id,stable_tool_ref,wf_def_id,workflow_version,definition_digest,schema_digest,policy_digest,response_policy_digest,principal_subject,end_user_subject,input,input_digest,canonical_input_profile,invocation_mode,execution_class,state,correlation_id,deadline_ts) VALUES($1,$2,$3,$4,$3,$5,'0.1.0',$6,$6,$6,$6,'retry-fixture','retry-fixture',$7,$8,'rfc8785-safe-json-v1','async','standard','RUNNING','retry-fixture',$9)")
            .bind(host).bind(instance).bind(binding).bind(process).bind(def).bind(&wire_digest).bind(&input).bind(format!("sha256:{}",canonical_sha256(&input).unwrap())).bind(deadline).execute(&pool).await.unwrap();
        let engine = crate::expression_test_support::engine(WorkerConfig {
            workers: 1,
            cache_entries: 128,
            cache_bytes: 4 * 1024 * 1024,
            ..WorkerConfig::default()
        })
        .unwrap();
        let executor = TaskExecutor::new(pool.clone()).with_expression_engine(engine);
        let mut claimed = ClaimedTask {
            expression_profile: "cel-workflow-v2".into(),
            input_data: input,
            task: ActiveTask {
                host_id: host,
                task_id: task,
                task_type: kind.into(),
                process_id: process,
                wf_instance_id: instance.to_string(),
                wf_task_id: name.into(),
                status_code: "A".into(),
                result_code: None,
            },
            wf_def_id: def,
            context_data: context,
            definition,
            raw_definition: serde_yaml::to_value(snapshot).unwrap(),
            host_lease: Some(HostTaskLease {
                owner,
                fencing_token: 1,
            }),
            completion_guard: None,
        };
        let mut tx = pool.begin().await.unwrap();
        assert!(
            expression_completion::capture_step(&mut tx, host, process, task)
                .await
                .unwrap()
        );
        assert!(executor.w4_context(&mut tx, &mut claimed).await.unwrap());
        tx.commit().await.unwrap();
        Self {
            pool,
            admin,
            database,
            executor,
            claimed,
            deadline,
        }
    }
    async fn finish(
        &self,
        status: &'static str,
        output: Value,
        commit: bool,
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let r = self
            .executor
            .finish_task(
                &mut tx,
                &self.claimed,
                TaskExecutionResult {
                    status_code: status,
                    task_output: output,
                    next_task: None,
                    context_data: None,
                },
            )
            .await;
        if r.is_ok() && commit {
            tx.commit().await?;
        } else {
            tx.rollback().await?;
        }
        r
    }
    async fn context(&self) -> Value {
        sqlx::query_scalar("SELECT context_data FROM process_info_t")
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }
    async fn count(&self, name: &str) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM task_info_t WHERE wf_task_id=$1")
            .bind(name)
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }
    async fn reacquire(&mut self) {
        self.claimed = self
            .executor
            .claim_next_task(Uuid::new_v4())
            .await
            .unwrap()
            .expect("due durable retry must be claimable");
    }
    async fn close(self) {
        self.executor
            .expression_engine
            .as_ref()
            .unwrap()
            .shutdown(Duration::from_secs(2))
            .unwrap();
        self.pool.close().await;
        sqlx::query(&format!("DROP DATABASE {} WITH (FORCE)", self.database))
            .execute(&self.admin)
            .await
            .unwrap();
        self.admin.close().await;
    }
}
async fn retry(exhaustion: bool) {
    let _permit = crate::expression_test_support::acquire().await;
    let mut f = Fixture::new(
        "fetch",
        json!({"owner":"fixture","repo":"repository","issue_number":1,"page":1,"requests":0}),
    )
    .await;
    let initial_context = f.context().await;
    let successor = "next";
    for attempt in 1..=3 {
        let actual: i32 = sqlx::query_scalar("SELECT attempt_no FROM task_info_t WHERE task_id=$1")
            .bind(f.claimed.task.task_id)
            .fetch_one(&f.pool)
            .await
            .unwrap();
        assert_eq!(actual, attempt);
        if attempt == 3 && !exhaustion {
            f.finish("C", json!({"id":1}), true).await.unwrap();
            break;
        }
        f.finish(
            "F",
            json!({"error":503,"message":"HTTP call failed","body":"fixture transient response"}),
            true,
        )
        .await
        .unwrap();
        assert_eq!(
            f.context().await,
            initial_context,
            "failed attempts must not export"
        );
        assert_eq!(
            f.count(successor).await,
            0,
            "failed attempts must not advance"
        );
        let row=sqlx::query("SELECT status_code::text,attempt_no,maximum_attempts,extract(epoch from next_attempt_ts-update_ts)::double precision AS delay FROM task_info_t WHERE task_id=$1").bind(f.claimed.task.task_id).fetch_one(&f.pool).await.unwrap();
        if attempt < 3 {
            assert_eq!(
                row.get::<String, _>("status_code"),
                "A",
                "v2 HTTP failure must enter production durable retry"
            );
            assert_eq!(row.get::<i32, _>("attempt_no"), attempt + 1);
            assert_eq!(row.get::<i32, _>("maximum_attempts"), 3);
            assert!((row.get::<f64, _>("delay") - 2.0).abs() < 0.01);
            assert!(
                f.executor
                    .claim_next_task(Uuid::new_v4())
                    .await
                    .unwrap()
                    .is_none(),
                "no early pickup"
            );
            // Reconstruct executor after committed retry to exercise persisted restart state.
            f.executor
                .expression_engine
                .as_ref()
                .unwrap()
                .shutdown(Duration::from_secs(2))
                .unwrap();
            f.executor = TaskExecutor::new(f.pool.clone()).with_expression_engine(
                crate::expression_test_support::engine(WorkerConfig {
                    workers: 1,
                    cache_entries: 128,
                    cache_bytes: 4 * 1024 * 1024,
                    ..WorkerConfig::default()
                })
                .unwrap(),
            );
            tokio::time::sleep(Duration::from_millis(2100)).await;
            f.reacquire().await;
        } else {
            assert_eq!(row.get::<String, _>("status_code"), "F");
            assert_eq!(row.get::<i32, _>("attempt_no"), 3);
        }
    }
    assert_eq!(f.count(successor).await, if exhaustion { 0 } else { 1 });
    let completed_context = f.context().await;
    let version: i64 = sqlx::query_scalar("SELECT state_version FROM workflow_invocation_t")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert!(
        f.finish(
            if exhaustion { "F" } else { "C" },
            json!({"error":503,"message":"HTTP call failed","body":"stale replay"}),
            true
        )
        .await
        .is_err()
    );
    assert_eq!(f.context().await, completed_context);
    assert_eq!(f.count(successor).await, if exhaustion { 0 } else { 1 });
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT state_version FROM workflow_invocation_t")
            .fetch_one(&f.pool)
            .await
            .unwrap(),
        version
    );
    f.close().await;
}
#[tokio::test]
#[ignore = "requires explicitly owned workflow_retry_* W4_TEST_DATABASE_URL"]
async fn v2_success_after_durable_retry() {
    retry(false).await;
}
#[tokio::test]
#[ignore = "requires explicitly owned workflow_retry_* W4_TEST_DATABASE_URL"]
async fn v2_durable_retry_exhaustion() {
    retry(true).await;
}

fn http_failure(status: u16) -> Value {
    json!({"error":status,"message":"HTTP call failed","body":"{\"retryable\":false,\"code\":\"AUTHORITY_BLOCKED\"}"})
}
impl Fixture {
    async fn replace_snapshot(&mut self, mut snapshot: Value, legacy: bool) {
        if legacy {
            snapshot["document"]["metadata"]
                .as_object_mut()
                .unwrap()
                .remove("lightExpressionProfile");
        }
        self.claimed.definition = serde_json::from_value(snapshot.clone()).unwrap();
        self.claimed.raw_definition = serde_yaml::to_value(&snapshot).unwrap();
        self.claimed.expression_profile = if legacy {
            "cel-workflow-v1"
        } else {
            "cel-workflow-v2"
        }
        .into();
        let task = self
            .executor
            .find_task_definition(&self.claimed.definition, &self.claimed.task.wf_task_id)
            .unwrap();
        self.claimed.task.task_type = TaskExecutor::supported_task_type_name(task).unwrap().into();
        sqlx::query("UPDATE process_info_t SET definition_snapshot=$1,definition_digest=$2,expression_profile=$3")
            .bind(&snapshot).bind(canonical_sha256(&snapshot).unwrap()).bind(&self.claimed.expression_profile).execute(&self.pool).await.unwrap();
        sqlx::query("UPDATE task_info_t SET task_type=$1,task_input=$2,result_code=NULL")
            .bind(&self.claimed.task.task_type)
            .bind(&self.claimed.context_data)
            .execute(&self.pool)
            .await
            .unwrap();
        if !legacy {
            let mut tx = self.pool.begin().await.unwrap();
            assert!(
                expression_completion::capture_step(
                    &mut tx,
                    self.claimed.task.host_id,
                    self.claimed.task.process_id,
                    self.claimed.task.task_id
                )
                .await
                .unwrap()
            );
            assert!(
                self.executor
                    .w4_context(&mut tx, &mut self.claimed)
                    .await
                    .unwrap()
            );
            tx.commit().await.unwrap();
        }
    }
    async fn state(&self) -> (String, i32, String) {
        sqlx::query_as("SELECT t.status_code::text,t.attempt_no,i.state FROM task_info_t t JOIN workflow_invocation_t i USING(host_id,process_id) WHERE t.task_id=$1")
            .bind(self.claimed.task.task_id).fetch_one(&self.pool).await.unwrap()
    }
}
#[tokio::test]
#[ignore = "requires explicitly owned workflow_retry_* W4_TEST_DATABASE_URL"]
async fn retry_no_policy_profile_and_idempotency_guards() {
    let _permit = crate::expression_test_support::acquire().await;
    for mode in ["no-policy", "unsupported", "corrupt", "idempotent-effect"] {
        let mut f = Fixture::new("fetch", json!({"unchanged":true})).await;
        if mode == "no-policy" {
            f.replace_snapshot(simple(json!({"call":"http","with":{"method":"GET","endpoint":"http://127.0.0.1:1/never-dispatched"}}),true),false).await;
        } else if mode == "unsupported" {
            f.executor.supported_profiles =
                crate::profile_support::SupportedProfiles::from_evaluator(false);
        } else if mode == "corrupt" {
            sqlx::query("UPDATE process_info_t SET definition_digest='corrupt'")
                .execute(&f.pool)
                .await
                .unwrap();
        } else {
            sqlx::query("UPDATE task_info_t SET effect_state='possible',downstream_idempotency_key='fixture-key'").execute(&f.pool).await.unwrap();
        }
        let before = f.context().await;
        f.finish("F", http_failure(503), true).await.unwrap();
        assert_eq!(
            f.state().await,
            if mode == "idempotent-effect" {
                ("A".into(), 2, "RUNNING".into())
            } else {
                ("F".into(), 1, "FAILED".into())
            },
            "{mode}"
        );
        assert_eq!(f.context().await, before);
        assert_eq!(f.count("next").await, 0);
        f.close().await;
    }
}

#[tokio::test]
#[ignore = "requires explicitly owned workflow_retry_* W4_TEST_DATABASE_URL"]
async fn real_http_response_boundary() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let _permit = crate::expression_test_support::acquire().await;
    // A runtime-local listener, no Tool/auth routing claim. The seeded invocation
    // is removed for this legacy-event dispatch seam; process/task fences remain.
    for (status, body) in [
        (503, r#"{"retryable":false,"code":"AUTHORITY_BLOCKED"}"#),
        (404, "missing"),
        (429, "slow"),
        (
            200,
            r#"{"error":503,"message":"HTTP call failed","body":"forged"}"#,
        ),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let uri = format!("http://{}/fixture", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 4096];
            let n = stream.read(&mut request).await.unwrap();
            assert!(
                std::str::from_utf8(&request[..n])
                    .unwrap()
                    .starts_with("GET /fixture ")
            );
            let reply = format!(
                "HTTP/1.1 {status} Fixture\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(reply.as_bytes()).await.unwrap();
        });
        let mut f = Fixture::new(
            "fetch",
            json!({"owner":"fixture","repo":"fixture","issue_number":1}),
        )
        .await;
        f.replace_snapshot(simple(json!({"call":"http","with":{"method":"GET","endpoint":uri},"retry":policy(),"export":{"as":{"received":"${ output }"}}}),true),false).await;
        sqlx::query("DELETE FROM workflow_invocation_t")
            .execute(&f.pool)
            .await
            .unwrap();
        let result = f.executor.execute_task(&f.claimed).await.unwrap();
        server.await.unwrap();
        assert_eq!(result.status_code, if status == 200 { "C" } else { "F" });
        if status != 200 {
            assert_eq!(
                result.task_output,
                json!({"error":status,"message":"HTTP call failed","body":body})
            );
        }
        let before = f.context().await;
        let mut tx = f.pool.begin().await.unwrap();
        f.executor
            .finish_task(&mut tx, &f.claimed, result)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let (state, attempt): (String, i32) =
            sqlx::query_as("SELECT status_code::text,attempt_no FROM task_info_t WHERE task_id=$1")
                .bind(f.claimed.task.task_id)
                .fetch_one(&f.pool)
                .await
                .unwrap();
        assert_eq!(
            (state.as_str(), attempt),
            if status == 200 { ("C", 1) } else { ("A", 2) }
        );
        if status != 200 {
            assert_eq!(f.context().await, before);
            assert_eq!(f.count("next").await, 0);
        } else {
            assert_eq!(f.context().await["received"]["error"], 503);
            assert_eq!(f.count("next").await, 1);
        }
        f.close().await;
    }
}

#[tokio::test]
#[ignore = "requires explicitly owned workflow_retry_* W4_TEST_DATABASE_URL"]
async fn terminal_failures_with_retry() {
    let _permit = crate::expression_test_support::acquire().await;
    // Execute real expression/assert/export/output failures, rather than calling
    // the retry predicate with synthetic codes alone.
    for mode in ["expression", "assertion", "export", "output"] {
        let mut f = Fixture::new("fetch", json!({"unchanged":true})).await;
        let task = match mode {
            "expression" => json!({"set":{"x":"${ context.missing }"},"retry":policy()}),
            "assertion" => json!({"assert":{"value":"${ false }","equals":true},"retry":policy()}),
            "export" => {
                json!({"set":{"x":1},"retry":policy(),"export":{"as":{"x":"${ output.missing }"}}})
            }
            _ => json!({"set":{"x":1},"retry":policy()}),
        };
        f.replace_snapshot(simple(task, mode != "output"), false)
            .await;
        if mode == "output" {
            sqlx::query("UPDATE workflow_invocation_t SET response_policy_snapshot=jsonb_build_object('publicOutputSchema','{\"type\":\"object\",\"required\":[\"never\"]}'::jsonb)").execute(&f.pool).await.unwrap();
        }
        let before = f.context().await;
        let result = f.executor.execute_task(&f.claimed).await.unwrap();
        let mut tx = f.pool.begin().await.unwrap();
        f.executor
            .finish_task(&mut tx, &f.claimed, result)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(f.state().await, ("F".into(), 1, "FAILED".into()), "{mode}");
        assert_eq!(f.context().await, before);
        assert_eq!(f.count("next").await, 0);
        f.close().await;
    }
    // An HTTP task's unrelated failures retain terminal outcomes as well.
    for code in [
        "EXPRESSION_EVALUATION",
        "WORKFLOW_REQUEST_FAILED",
        "AUTHORITY_BLOCKED",
        "WORKFLOW_TASK_TIMEOUT",
        "WORKFLOW_BUDGET_EXHAUSTED",
        "WORKFLOW_OUTPUT_INVALID_AFTER_EFFECT",
    ] {
        let f = Fixture::new("fetch", json!({})).await;
        f.finish("F", json!({"code":code,"retryable":true}), true)
            .await
            .unwrap();
        assert_eq!(f.state().await, ("F".into(), 1, "FAILED".into()), "{code}");
        f.close().await;
    }
}

#[tokio::test]
#[ignore = "requires explicitly owned workflow_retry_* W4_TEST_DATABASE_URL"]
async fn failed_retry_authority_and_effect_guards() {
    let _permit = crate::expression_test_support::acquire().await;
    for mutation in [
        "UPDATE workflow_invocation_t SET cancel_requested_ts=clock_timestamp()",
        "UPDATE process_info_t SET deadline_ts=clock_timestamp()-interval '1 second'",
        "UPDATE workflow_invocation_t SET deadline_ts=clock_timestamp()-interval '1 second'",
        "UPDATE task_info_t SET lease_fencing_token=2",
        "UPDATE task_info_t SET lease_owner=NULL",
        "UPDATE task_info_t SET lease_expires_ts=clock_timestamp()-interval '1 second'",
    ] {
        let f = Fixture::new("fetch", json!({"unchanged":true})).await;
        let before = f.context().await;
        sqlx::raw_sql(mutation).execute(&f.pool).await.unwrap();
        assert!(
            f.finish("F", http_failure(503), true).await.is_err(),
            "{mutation}"
        );
        assert_eq!(f.state().await, ("A".into(), 1, "RUNNING".into()));
        assert_eq!(f.context().await, before);
        assert_eq!(f.count("next").await, 0);
        f.close().await;
    }
    for mutation in [
        "UPDATE task_info_t SET effect_state='possible',downstream_idempotency_key=NULL",
        "UPDATE task_info_t SET deadline_ts=clock_timestamp()+interval '1 second'",
    ] {
        let f = Fixture::new("fetch", json!({})).await;
        sqlx::raw_sql(mutation).execute(&f.pool).await.unwrap();
        f.finish("F", http_failure(503), true).await.unwrap();
        assert_eq!(f.state().await, ("F".into(), 1, "FAILED".into()));
        f.close().await;
    }
}

#[tokio::test]
#[ignore = "requires explicitly owned workflow_retry_* W4_TEST_DATABASE_URL"]
async fn legacy_http_retry_success_and_exhaustion() {
    let _permit = crate::expression_test_support::acquire().await;
    for exhaustion in [false, true] {
        let mut f = Fixture::new(
            "fetch",
            json!({"owner":"fixture","repo":"fixture","issue_number":1}),
        )
        .await;
        let snapshot = definition("fetch");
        f.replace_snapshot(snapshot, true).await;
        for attempt in 1..=3 {
            let success = attempt == 3 && !exhaustion;
            f.finish(
                if success { "C" } else { "F" },
                if success {
                    json!({"id":1})
                } else {
                    http_failure(503)
                },
                true,
            )
            .await
            .unwrap();
            assert_eq!(f.state().await.1, if attempt < 3 { attempt + 1 } else { 3 });
            if attempt < 3 {
                assert_eq!(f.state().await.0, "A");
                assert!(
                    f.executor
                        .claim_next_task(Uuid::new_v4())
                        .await
                        .unwrap()
                        .is_none()
                );
                let delay:f64=sqlx::query_scalar("SELECT extract(epoch FROM next_attempt_ts-update_ts)::double precision FROM task_info_t WHERE task_id=$1").bind(f.claimed.task.task_id).fetch_one(&f.pool).await.unwrap();
                assert!((delay - 2.0).abs() < 0.01);
                tokio::time::sleep(Duration::from_millis(2100)).await;
                f.reacquire().await;
            }
        }
        assert_eq!(f.state().await.0, if exhaustion { "F" } else { "C" });
        assert_eq!(f.count("next").await, if exhaustion { 0 } else { 1 });
        f.close().await;
    }
}

#[tokio::test]
#[ignore = "requires explicitly owned workflow_retry_* W4_TEST_DATABASE_URL"]
async fn retry_exhaustion_replay_and_failure_routing() {
    let _permit = crate::expression_test_support::acquire().await;
    for mode in ["process", "branch", "compensation"] {
        let f = Fixture::new("fetch", json!({"unchanged":true})).await;
        sqlx::query("UPDATE task_info_t SET attempt_no=3")
            .execute(&f.pool)
            .await
            .unwrap();
        if mode == "compensation" {
            sqlx::raw_sql("UPDATE task_info_t SET is_compensation=true; UPDATE workflow_invocation_t SET state='COMPENSATING'").execute(&f.pool).await.unwrap();
        }
        if mode == "branch" {
            let join = Uuid::new_v4();
            sqlx::query("INSERT INTO workflow_fork_join_t(host_id,join_id,workflow_instance_id,process_id,fork_task_id,fork_task_name,expected_branches,compete,state) SELECT $1,$2,workflow_instance_id,$3,$4,'fixture-fork',1,false,'RUNNING' FROM workflow_invocation_t").bind(f.claimed.task.host_id).bind(join).bind(f.claimed.task.process_id).bind(f.claimed.task.task_id).execute(&f.pool).await.unwrap();
            sqlx::query("INSERT INTO workflow_fork_branch_t(host_id,join_id,task_id,branch_name,state) VALUES($1,$2,$3,'fixture-branch','RUNNING')").bind(f.claimed.task.host_id).bind(join).bind(f.claimed.task.task_id).execute(&f.pool).await.unwrap();
        }
        let before = f.context().await;
        f.finish("F", http_failure(503), true).await.unwrap();
        assert_eq!(f.state().await, ("F".into(), 3, "FAILED".into()), "{mode}");
        assert_eq!(f.context().await, before);
        assert_eq!(f.count("next").await, 0);
        let version: i64 = sqlx::query_scalar("SELECT state_version FROM workflow_invocation_t")
            .fetch_one(&f.pool)
            .await
            .unwrap();
        assert!(f.finish("F", http_failure(503), true).await.is_err());
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT state_version FROM workflow_invocation_t")
                .fetch_one(&f.pool)
                .await
                .unwrap(),
            version
        );
        if mode == "branch" {
            assert_eq!(
                sqlx::query_as::<_, (String, i32)>(
                    "SELECT state,failed_branches FROM workflow_fork_join_t"
                )
                .fetch_one(&f.pool)
                .await
                .unwrap(),
                ("FAILED".into(), 1)
            );
        }
        f.close().await;
    }
}
async fn native(invalid: bool) {
    let _permit = crate::expression_test_support::acquire().await;
    let f = Fixture::new(
        "author",
        json!({"comments":[],"requests":1,"designInstruction":"fixture only"}),
    )
    .await;
    let c = &f.claimed;
    let agent = Uuid::new_v4();
    let schema = definition("author")["do"][0]["author"]["with"]["outputSchema"].clone();
    for _ in 0..2 {
        assert_eq!(
            crate::native_jobs::enqueue_with_artifacts_supported(
                &f.pool,
                c.task.host_id,
                c.task.process_id,
                c.task.task_id,
                "author",
                agent,
                json!({"workspace":{"fixture":true}}),
                schema.clone(),
                f.deadline,
                8192,
                0,
                0,
                1,
                None,
                f.executor.supported_profiles(),
                Some((Some((c.host_lease.unwrap().owner, 1)), "cel-workflow-v2"))
            )
            .await
            .unwrap(),
            c.task.task_id
        );
    }
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM workflow_agent_job_t")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(n, 1);
    f.finish(
        "W",
        json!({"agentJobId":c.task.task_id,"state":"PENDING"}),
        true,
    )
    .await
    .unwrap();
    assert!(f.context().await.get("designResult").is_none());
    assert_eq!(f.count("validate").await, 0);
    assert!(
        !f.executor
            .reconcile_agent_job(c.task.host_id, c.task.task_id)
            .await
            .unwrap()
    );
    let output = if invalid {
        json!({"finalMessage":7,"workspace":{},"codingThread":{}})
    } else {
        json!({"finalMessage":"fixture design","workspace":{},"codingThread":{}})
    };
    sqlx::query(
        "UPDATE workflow_agent_job_t SET state='SUCCEEDED',public_output=$1 WHERE job_id=$2",
    )
    .bind(output)
    .bind(c.task.task_id)
    .execute(&f.pool)
    .await
    .unwrap();
    assert!(
        f.executor
            .reconcile_agent_job(c.task.host_id, c.task.task_id)
            .await
            .unwrap()
    );
    assert_eq!(f.count("validate").await, 1);
    let next = f
        .executor
        .claim_next_task(Uuid::new_v4())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(next.task.wf_task_id, "validate");
    let result = f.executor.execute_task(&next).await.unwrap();
    assert_eq!(
        result.status_code,
        if invalid { "F" } else { "C" },
        "{}",
        result.task_output
    );
    let mut tx = f.pool.begin().await.unwrap();
    f.executor
        .finish_task(&mut tx, &next, result)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(
        !f.executor
            .reconcile_agent_job(c.task.host_id, c.task.task_id)
            .await
            .unwrap()
    );
    assert_eq!(f.count("validate").await, 1);
    f.close().await;
}
#[tokio::test]
#[ignore = "requires explicitly owned workflow_retry_* W4_TEST_DATABASE_URL"]
async fn native_pending_completion_replay() {
    native(false).await
}

#[tokio::test]
#[ignore = "requires explicitly owned workflow_retry_* W4_TEST_DATABASE_URL"]
async fn native_failed_completion_cannot_use_http_retry_authority() {
    let _permit = crate::expression_test_support::acquire().await;
    let f = Fixture::new("author", json!({"unchanged":true})).await;
    let before = f.context().await;
    f.finish("F", http_failure(503), true).await.unwrap();
    assert_eq!(f.state().await, ("F".into(), 1, "FAILED".into()));
    assert_eq!(f.context().await, before);
    assert_eq!(f.count("validate").await, 0);
    f.close().await;
}
#[tokio::test]
#[ignore = "requires explicitly owned workflow_retry_* W4_TEST_DATABASE_URL"]
async fn native_invalid_output_assertion() {
    native(true).await
}
