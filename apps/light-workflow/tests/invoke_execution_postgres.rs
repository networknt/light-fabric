//! Owned-PostgreSQL end-to-end Invoke cases. Never uses the shared Portal database.
#[path = "support/ops_db.rs"]
mod ops_db;

use axum::Router;
use light_security::SecurityRuntime;
use light_workflow::{
    configuration::WorkflowConfigManager,
    executor::TaskExecutor,
    rule_api::{WorkflowHealth, build_rule_api_router},
    run_credential::RunCredentialVault,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::{sync::Arc, time::Duration};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const KEY: &[u8] = b"step14-invoke-signing-key-32-bytes";
const VERSION: &str = "2026-07-28";
const SUCCESS: &str = "document: {dsl: '1.0.3', namespace: step14, name: invoke, version: '1.0.0'}\nevaluate: {language: cel}\ndo:\n  - prepare:\n      set: {status: ok}\n      end: true\n";
const FAILURE: &str = "document: {dsl: '1.0.3', namespace: step14, name: failure, version: '1.0.0'}\nevaluate: {language: cel}\ndo:\n  - reject:\n      assert: {value: actual, equals: expected}\n      end: true\n";

fn signed(claims: Value) -> String {
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
    header.kid = Some("step14".into());
    jsonwebtoken::encode(
        &header,
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(KEY),
    )
    .unwrap()
}

struct Fixture {
    pool: PgPool,
    host: Uuid,
    tool: Uuid,
    binding: Uuid,
    user: Uuid,
    definition_digest: String,
    binding_digest: String,
    endpoint: String,
    client: reqwest::Client,
    vault: Arc<RunCredentialVault>,
    security: Arc<SecurityRuntime>,
    server: tokio::task::JoinHandle<()>,
    directory: TempDir,
}

impl Fixture {
    async fn new(definition: &str, replay_ms: u64, wait_ms: i32, deadline_ms: i32) -> Self {
        Self::new_with_wait_database_url(definition, replay_ms, wait_ms, deadline_ms, None).await
    }

    async fn new_with_wait_database_url(
        definition: &str,
        replay_ms: u64,
        wait_ms: i32,
        deadline_ms: i32,
        wait_database_url: Option<&str>,
    ) -> Self {
        let pool = ops_db::runtime_pool();
        let _admin = ops_db::admin_pool(); // Fail closed for either missing URL.
        let host = Uuid::new_v4();
        let wf = Uuid::new_v4();
        let tool = Uuid::new_v4();
        let user = Uuid::new_v4();
        let binding = Uuid::new_v4();
        let source = Uuid::new_v4();
        let definition_digest = workflow_invocation_contract::canonical_sha256(
            &serde_yaml::from_str::<Value>(definition).unwrap(),
        )
        .unwrap();
        let binding_digest = format!("sha256:{}", "b".repeat(64));
        let schema_digest = format!("sha256:{}", "a".repeat(64));
        sqlx::query("INSERT INTO wf_definition_t(host_id,wf_def_id,namespace,name,version,definition,lifecycle_status,active) VALUES($1,$2,'step14','invoke','1.0.0',$3,'PUBLISHED',true)")
            .bind(host).bind(wf).bind(definition).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO wf_definition_version_t(host_id,wf_def_id,version,definition,definition_digest,schema_digest,published_by) VALUES($1,$2,'1.0.0',$3,$4,$5,'invoke-test')")
            .bind(host).bind(wf).bind(definition).bind(&definition_digest).bind(&schema_digest).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO workflow_tool_binding_t(host_id,binding_id,tool_id,wf_def_id,workflow_version,definition_digest,schema_digest,invocation_mode,sync_wait_ms,total_deadline_ms,execution_class,result_text_mode,idempotency_policy,delegation_policy,response_policy_digest,runtime_bounds,policy_digest,active,revision_status,source_binding_id,binding_digest,approval_digest,requested_by,requested_ts,caller_policy,admission_limits,tool_name) VALUES($1,$2,$3,$4,'1.0.0',$5,$6,'sync',$7,$8,'interactive','compact-json',$9,$10,$6,$11,$6,true,'approved',$12,$13,$13,'invoke-test',CURRENT_TIMESTAMP,'{}',$14,'invoke-test')")
            .bind(host).bind(binding).bind(tool).bind(wf).bind(&definition_digest).bind(&schema_digest)
            .bind(wait_ms).bind(deadline_ms).bind(json!({"kind":"derived","resultReplayMs":replay_ms}))
            .bind(json!({"maximumDelegationDepth":1}))
            .bind(json!({"maximumTaskAttempts":8,"maximumNestedCalls":8,"maximumParallelism":1,
                "maximumRequestBytes":1048576,"maximumIntermediateBytes":4194304,
                "maximumResultBytes":1048576,"maximumCostUnits":1000}))
            .bind(source).bind(&binding_digest)
            .bind(json!({"maximumConcurrentRuns":20,"maximumConcurrentRunsPerUser":5,
                "startsPerMinute":120,"startsPerMinutePerUser":20}))
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO workflow_tool_publication_t(host_id,tool_id,active_binding_id,aggregate_version) VALUES($1,$2,$3,1)")
            .bind(host).bind(tool).bind(binding).execute(&pool).await.unwrap();
        let directory = tempfile::tempdir().unwrap();
        let keyring = directory.path().join("keyring.json");
        tokio::fs::write(&keyring, r#"{"activeKeyId":"test","keys":{"test":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="}}"#).await.unwrap();
        let vault = Arc::new(
            RunCredentialVault::load(std::path::Path::new("/"), Some(&keyring))
                .await
                .unwrap()
                .unwrap(),
        );
        let security = Arc::new(SecurityRuntime::with_test_hs256_key("step14", KEY).await);
        let router: Router = build_rule_api_router(
            pool.clone(),
            wait_database_url
                .map(str::to_owned)
                .unwrap_or_else(|| std::env::var("DATABASE_URL").unwrap()),
            Arc::new(WorkflowConfigManager::for_publication_test(host)),
            security.clone(),
            "dev".into(),
            WorkflowHealth::default(),
            None,
            Default::default(),
            None,
            Some(vault.clone()),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            pool,
            host,
            tool,
            binding,
            user,
            definition_digest,
            binding_digest,
            endpoint,
            client: reqwest::Client::new(),
            vault,
            security,
            server,
            directory,
        }
    }

    fn token(&self) -> String {
        signed(json!({"iss":"step14","aud":"workflow","exp":4102444800u64,
            "token_use":"user","client_id":"portal-ui","uid":self.user,
            "user_id":self.user,"sub":self.user,"host":self.host,"roles":["user"]}))
    }

    async fn invoke(&self) -> Value {
        let scope = signed(json!({"iss":"step14","aud":"workflow","exp":4102444800u64,
            "sid":"gateway-a","host":self.host,"env":"dev"}));
        let response = self.client.post(format!("{}/mcp", self.endpoint))
            .header("authorization", format!("Bearer {}", self.token()))
            .header("x-scope-token", format!("Bearer {scope}"))
            .header("mcp-protocol-version", VERSION)
            .header("accept", "application/json, text/event-stream")
            .header("mcp-method", "tools/call")
            .header("mcp-name", "workflow_invoke")
            .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
                "name":"workflow_invoke","arguments":{"stableToolRef":self.tool,
                    "expectedBindingDigest":self.binding_digest,"expectedDefinitionDigest":self.definition_digest,
                    "input":{}},"_meta":{"io.modelcontextprotocol/clientCapabilities":{},
                    "io.modelcontextprotocol/protocolVersion":VERSION}}}))
            .send().await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        response.json::<Value>().await.unwrap()["result"].clone()
    }

    fn assert_error(&self, result: &Value, status: &str, code: &str, retryable: bool) -> Uuid {
        assert_eq!(result["isError"], true, "{result}");
        let body = &result["structuredContent"];
        let schema: Value =
            serde_json::from_str(include_str!("../contracts/workflow-admin/schemas.json")).unwrap();
        let compiled = jsonschema::validator_for(&json!({"$defs":schema["$defs"],
            "$ref":"#/$defs/WorkflowErrorResult"}))
        .unwrap();
        assert!(
            compiled.is_valid(body),
            "invalid WorkflowErrorResult: {body}"
        );
        assert_eq!(body["status"], status);
        assert_eq!(body["error"]["code"], code);
        assert_eq!(body["error"]["retryable"], retryable);
        assert!(body["error"]["afterEffect"].is_boolean());
        let expected = format!("{code}: {}", body["error"]["message"].as_str().unwrap());
        assert_eq!(result["content"][0]["text"], expected);
        serde_json::from_value(body["workflowInstanceId"].clone()).unwrap()
    }

    async fn executor(&self) -> (CancellationToken, tokio::task::JoinHandle<()>) {
        let key = self.directory.path().join("a2a.key");
        tokio::fs::write(&key, [7u8; 32]).await.unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let executor = TaskExecutor::new(self.pool.clone())
            .with_runtime_configuration(
                std::env::var("DATABASE_URL").unwrap(),
                1,
                "dev".into(),
                "test".into(),
                None,
                Default::default(),
                key,
                false,
            )
            .unwrap();
        executor
            .run_tokens
            .set(Arc::new(
                light_workflow::run_token::RunTokenSelector::new(
                    self.pool.clone(),
                    Some(self.vault.clone()),
                    None,
                    self.security.clone(),
                    60,
                )
                .unwrap(),
            ))
            .ok()
            .unwrap();
        let stop = CancellationToken::new();
        let cloned = stop.clone();
        let handle = tokio::spawn(async move { executor.run(cloned).await.unwrap() });
        (stop, handle)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

#[tokio::test]
#[ignore = "requires owned scratch DATABASE_URL and ADMIN_DATABASE_URL"]
async fn first_call_success_returns_stored_output() {
    let f = Fixture::new(SUCCESS, 0, 5000, 30000).await;
    let (stop, handle) = f.executor().await;
    let result = f.invoke().await;
    stop.cancel();
    handle.await.unwrap();
    assert_eq!(result["isError"], false, "{result}");
    assert_eq!(result["structuredContent"]["status"], "completed");
    assert_eq!(
        result["structuredContent"]["definitionDigest"],
        f.definition_digest
    );
    let run: Uuid =
        serde_json::from_value(result["structuredContent"]["workflowInstanceId"].clone()).unwrap();
    let stored: Value = sqlx::query_scalar("SELECT public_result FROM workflow_invocation_t WHERE host_id=$1 AND workflow_instance_id=$2")
        .bind(f.host).bind(run).fetch_one(&f.pool).await.unwrap();
    assert_eq!(result["structuredContent"]["output"], stored);
}

#[tokio::test]
#[ignore = "requires owned scratch DATABASE_URL and ADMIN_DATABASE_URL"]
async fn completed_invoke_does_not_require_a_new_listener_connection() {
    let f = Fixture::new_with_wait_database_url(
        SUCCESS,
        0,
        5000,
        30000,
        Some("postgres://unavailable:unavailable@127.0.0.1:1/unavailable"),
    )
    .await;
    let (stop, handle) = f.executor().await;
    let result = tokio::time::timeout(Duration::from_secs(3), f.invoke())
        .await
        .expect("completed invoke should not wait for a new listener connection");
    stop.cancel();
    handle.await.unwrap();
    assert_eq!(result["isError"], false, "{result}");
    assert_eq!(result["structuredContent"]["status"], "completed");
}

#[tokio::test]
#[ignore = "requires owned scratch DATABASE_URL and ADMIN_DATABASE_URL"]
async fn task_failure_returns_stored_message_promptly() {
    let f = Fixture::new(FAILURE, 0, 5000, 30000).await;
    let (stop, handle) = f.executor().await;
    let result = f.invoke().await;
    stop.cancel();
    handle.await.unwrap();
    let run = f.assert_error(&result, "failed", "WORKFLOW_TASK_FAILED", false);
    let stored: Value = sqlx::query_scalar("SELECT normalized_error FROM workflow_invocation_t WHERE host_id=$1 AND workflow_instance_id=$2")
        .bind(f.host).bind(run).fetch_one(&f.pool).await.unwrap();
    assert_eq!(
        result["structuredContent"]["error"]["message"],
        stored["message"]
    );
    assert_eq!(result["structuredContent"]["error"]["afterEffect"], false);
}

#[tokio::test]
#[ignore = "requires owned scratch DATABASE_URL and ADMIN_DATABASE_URL"]
async fn timeout_retry_attaches_to_same_instance() {
    let f = Fixture::new(SUCCESS, 60000, 50, 30000).await;
    let first = f.invoke().await;
    let run = f.assert_error(&first, "timeout", "WORKFLOW_TIMEOUT", true);
    let second = f.invoke().await;
    assert_eq!(
        f.assert_error(&second, "timeout", "WORKFLOW_TIMEOUT", true),
        run
    );
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM workflow_invocation_t WHERE host_id=$1")
            .bind(f.host)
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
#[ignore = "requires owned scratch DATABASE_URL and ADMIN_DATABASE_URL"]
async fn zero_replay_starts_new_run_and_positive_window_replays() {
    for (window, same) in [(0, false), (60000, true)] {
        let f = Fixture::new(SUCCESS, window, 5000, 30000).await;
        let (stop, handle) = f.executor().await;
        let first = f.invoke().await;
        assert_eq!(first["isError"], false, "{first}");
        let second = f.invoke().await;
        stop.cancel();
        handle.await.unwrap();
        assert_eq!(second["isError"], false, "{second}");
        assert_eq!(
            first["structuredContent"]["workflowInstanceId"]
                == second["structuredContent"]["workflowInstanceId"],
            same
        );
    }
}

#[tokio::test]
#[ignore = "requires owned scratch DATABASE_URL and ADMIN_DATABASE_URL"]
async fn clean_failure_restarts_and_confirmed_effect_failure_replays() {
    for (confirmed, same) in [(false, false), (true, true)] {
        let f = Fixture::new(FAILURE, 60000, 5000, 30000).await;
        // A confirmed effect is durable state from a task preceding the failure.
        // Here the test sets that state before the executor commits its failure.
        let (stop, handle) = if confirmed {
            let first = f.invoke().await;
            let run = f.assert_error(&first, "timeout", "WORKFLOW_TIMEOUT", true);
            sqlx::query("UPDATE workflow_invocation_t SET effect_state='confirmed' WHERE host_id=$1 AND workflow_instance_id=$2")
                .bind(f.host).bind(run).execute(&f.pool).await.unwrap();
            f.executor().await
        } else {
            f.executor().await
        };
        let first = f.invoke().await;
        let run1 = f.assert_error(&first, "failed", "WORKFLOW_TASK_FAILED", false);
        assert_eq!(
            first["structuredContent"]["error"]["afterEffect"],
            confirmed
        );
        let second = f.invoke().await;
        let run2 = if same {
            f.assert_error(&second, "failed", "WORKFLOW_TASK_FAILED", false)
        } else if second["isError"] == false {
            serde_json::from_value(second["structuredContent"]["workflowInstanceId"].clone())
                .unwrap()
        } else {
            serde_json::from_value(second["structuredContent"]["workflowInstanceId"].clone())
                .unwrap()
        };
        if same {
            assert_eq!(second["structuredContent"]["error"]["afterEffect"], true);
        }
        stop.cancel();
        handle.await.unwrap();
        assert_eq!(run1 == run2, same);
    }
}

#[tokio::test]
#[ignore = "requires owned scratch DATABASE_URL and ADMIN_DATABASE_URL"]
async fn deadline_expiry_uses_policy_cancellation_and_nonretryable_timeout() {
    let f = Fixture::new(SUCCESS, 60000, 50, 250).await;
    let first = f.invoke().await;
    let run = f.assert_error(&first, "timeout", "WORKFLOW_TIMEOUT", true);
    tokio::time::sleep(Duration::from_millis(270)).await;
    let second = f.invoke().await;
    assert_eq!(
        f.assert_error(&second, "timeout", "WORKFLOW_TIMEOUT", false),
        run
    );
    let state: String = sqlx::query_scalar(
        "SELECT state FROM workflow_invocation_t WHERE host_id=$1 AND workflow_instance_id=$2",
    )
    .bind(f.host)
    .bind(run)
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(state, "CANCELLED");

    let disabled = Fixture::new(SUCCESS, 60000, 50, 250).await;
    sqlx::query("UPDATE workflow_tool_binding_t SET cancellation_policy='disabled' WHERE host_id=$1 AND binding_id=$2")
        .bind(disabled.host).bind(disabled.binding).execute(&disabled.pool).await.unwrap();
    let first = disabled.invoke().await;
    let run = disabled.assert_error(&first, "timeout", "WORKFLOW_TIMEOUT", true);
    tokio::time::sleep(Duration::from_millis(270)).await;
    let second = disabled.invoke().await;
    assert_eq!(
        disabled.assert_error(&second, "timeout", "WORKFLOW_TIMEOUT", false),
        run
    );
    let policy: (String, Option<String>) = sqlx::query_as("SELECT state,non_cancellable_reason FROM workflow_invocation_t WHERE host_id=$1 AND workflow_instance_id=$2")
        .bind(disabled.host).bind(run).fetch_one(&disabled.pool).await.unwrap();
    assert_eq!(policy.0, "ACCEPTED");
    assert_eq!(policy.1.as_deref(), Some("CANCELLATION_DISABLED"));

    // Exercise the stored cancelled envelope separately from the timeout
    // returned to the caller whose deadline-triggered cancellation succeeds.
    let terminal = Fixture::new(SUCCESS, 60000, 50, 30000).await;
    let first = terminal.invoke().await;
    let run = terminal.assert_error(&first, "timeout", "WORKFLOW_TIMEOUT", true);
    sqlx::query("UPDATE workflow_invocation_t SET effect_state='confirmed' WHERE host_id=$1 AND workflow_instance_id=$2")
        .bind(terminal.host).bind(run).execute(&terminal.pool).await.unwrap();
    sqlx::query(
        "UPDATE workflow_invocation_t SET state='CANCELLED',terminal_ts=CURRENT_TIMESTAMP,
        user_authorization=NULL,user_authorization_exp=NULL,state_version=state_version+1
        WHERE host_id=$1 AND workflow_instance_id=$2",
    )
    .bind(terminal.host)
    .bind(run)
    .execute(&terminal.pool)
    .await
    .unwrap();
    let replay = terminal.invoke().await;
    assert_eq!(
        terminal.assert_error(&replay, "cancelled", "WORKFLOW_CANCELLED", false),
        run
    );
    assert_eq!(replay["structuredContent"]["error"]["afterEffect"], true);
}

#[tokio::test]
#[ignore = "requires owned scratch DATABASE_URL and ADMIN_DATABASE_URL"]
async fn encrypted_credential_is_present_while_running_and_removed_on_terminal() {
    let f = Fixture::new(SUCCESS, 60000, 50, 30000).await;
    let first = f.invoke().await;
    let run = f.assert_error(&first, "timeout", "WORKFLOW_TIMEOUT", true);
    let stored: (String, Vec<u8>) = sqlx::query_as("SELECT key_id,token_bytes FROM workflow_run_credential_t WHERE host_id=$1 AND workflow_instance_id=$2")
        .bind(f.host).bind(run).fetch_one(&f.pool).await.unwrap();
    assert_ne!(stored.0, "plaintext");
    assert!(!stored.1.is_empty());
    let (stop, handle) = f.executor().await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let state: String = sqlx::query_scalar("SELECT state FROM workflow_invocation_t WHERE host_id=$1 AND workflow_instance_id=$2")
                .bind(f.host).bind(run).fetch_one(&f.pool).await.unwrap();
            if state == "COMPLETED" { break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.unwrap();
    let second = f.invoke().await;
    stop.cancel();
    handle.await.unwrap();
    assert_eq!(second["isError"], false, "{second}");
    let remains: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workflow_run_credential_t WHERE host_id=$1 AND workflow_instance_id=$2)")
        .bind(f.host).bind(run).fetch_one(&f.pool).await.unwrap();
    assert!(!remains, "terminal trigger must remove the encrypted row");
}

#[tokio::test]
#[ignore = "requires owned scratch DATABASE_URL and ADMIN_DATABASE_URL"]
async fn owned_http_stub_receives_selected_original_user_token_hash() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!("http://{}/protected", listener.local_addr().unwrap());
    let definition = format!(
        "document: {{dsl: '1.0.3', namespace: step14, name: token, version: '1.0.0'}}\nevaluate: {{language: cel}}\ndo:\n  - fetch:\n      call: http\n      metadata: {{endpointRef: test-endpoint}}\n      with:\n        method: GET\n        endpoint: {{uri: '{uri}'}}\n      end: true\n"
    );
    let f = Fixture::new(&definition, 60000, 5000, 30000).await;
    sqlx::query("INSERT INTO workflow_endpoint_target_t(host_id,binding_id,endpoint_ref,endpoint_uri,allowed_methods,authorization_policy_digest) VALUES($1,$2,'test-endpoint',$3,ARRAY['GET'],$4)")
        .bind(f.host).bind(f.binding).bind(&uri)
        .bind(format!("sha256:{}", "a".repeat(64)))
        .execute(&f.pool).await.unwrap();
    let expected = Sha256::digest(f.token().as_bytes());
    let receiver = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut bytes = [0u8; 8192];
        let length = stream.read(&mut bytes).await.unwrap();
        let request = String::from_utf8_lossy(&bytes[..length]);
        let bearer = request
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .starts_with("authorization: bearer ")
                    .then(|| {
                        line.split_once(':')
                            .unwrap()
                            .1
                            .trim()
                            .strip_prefix("Bearer ")
                            .unwrap_or_default()
                            .to_owned()
                    })
            })
            .expect("protected stub must receive user authorization");
        stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 11\r\n\r\n{\"ok\":true}").await.unwrap();
        Sha256::digest(bearer.as_bytes())
    });
    let (stop, handle) = f.executor().await;
    let result = f.invoke().await;
    stop.cancel();
    handle.await.unwrap();
    assert_eq!(result["isError"], false, "{result}");
    assert_eq!(receiver.await.unwrap(), expected);
}
