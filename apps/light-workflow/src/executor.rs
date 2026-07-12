use crate::repositories::{NewTask, TerminalAttempt, WorkflowRepository};
use execution_runner_protocol::canonical_sha256;
use light_rule::{ActionRegistry, MultiThreadRuleExecutor, RuleConfig, RuleEngine};
use model_provider::{
    AnthropicProvider, ChatMessage, ChatRequest, CompatibleProvider, GeminiProvider,
    OllamaProvider, OpenAiProvider, OpenRouterProvider, Provider,
};
use regex::Regex;
use serde_json::{Map as JsonMap, Number, Value, json};
use serde_yaml::Value as YamlValue;
use sqlx::{PgPool, Postgres, Transaction};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::io;
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info, warn};
use uuid::Uuid;
use workflow_core::models::task::{
    AgentArguments, AskDefinition, AssertComparison, AssertComparisonObject, AssertDefinition,
    CallTaskDefinition, HasLengthComparison, JsonRpcArguments, JsonRpcErrorPolicy, McpArguments,
    McpServerDefinition, OpenRpcArguments, SetValue, TaskDefinition, TaskDefinitionFields,
};
use workflow_core::models::workflow::WorkflowDefinition;
use workflow_policy::{ExecutionProfile, TaskKind, parse_security_policy, resolve_policy};

type DynError = Box<dyn std::error::Error + Send + Sync>;
static TEMPLATE_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\$\{\{\s*([^}]*(?:}[^}]+)*)\s*\}\}|\$\{\s*([^}]*)\s*\}")
        .expect("valid template regex")
});
const TASK_LOCK_TIMEOUT_MINUTES: i64 = 5;
const MAX_HTTP_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_AGENT_OUTPUT_BYTES: usize = 128 * 1024;
const AGENT_PROMPT_VERSION: u32 = 1;
const CLAIM_NEXT_HOST_TASK_SQL: &str = r#"
            UPDATE task_info_t
            SET locked = 'Y', update_ts = CURRENT_TIMESTAMP
            WHERE (host_id, task_id) IN (
                SELECT host_id, task_id FROM task_info_t
                WHERE (
                    (status_code = 'A' AND task_type IN ('ask', 'assert', 'call', 'set', 'switch'))
                    OR (
                        status_code = 'C'
                        AND task_type = 'ask'
                        AND completed_ts IS NOT NULL
                        AND (task_output IS NULL OR task_output->>'status' = 'waiting_for_input')
                    )
                  )
                  AND execution_placement = 'host'
                  AND (
                    locked = 'N'
                    OR (locked = 'Y' AND update_ts < CURRENT_TIMESTAMP - make_interval(mins => $1::int))
                  )
                ORDER BY priority DESC, started_ts ASC
                LIMIT 1
                FOR UPDATE SKIP LOCKED
            )
            RETURNING host_id, task_id, task_type, process_id, wf_instance_id, wf_task_id, status_code, result_code
            "#;

#[derive(sqlx::FromRow)]
pub struct ActiveTask {
    pub host_id: Uuid,
    pub task_id: Uuid,
    pub task_type: String,
    pub process_id: Uuid,
    pub wf_instance_id: String,
    pub wf_task_id: String,
    pub status_code: String,
    pub result_code: Option<String>,
}

struct ClaimedTask {
    task: ActiveTask,
    context_data: Value,
    definition: WorkflowDefinition,
    raw_definition: YamlValue,
}

struct TaskExecutionResult {
    status_code: &'static str,
    task_output: Value,
    next_task: Option<String>,
    context_data: Option<Value>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct AgentDefinitionRecord {
    agent_def_id: Uuid,
    agent_name: Option<String>,
    model_provider: String,
    model_name: String,
    api_key_ref: Option<String>,
    temperature: f64,
    max_tokens: Option<i32>,
    aggregate_version: i64,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct AgentSkillRecord {
    skill_id: Uuid,
    name: String,
    description: Option<String>,
    content_markdown: String,
    priority: Option<i32>,
    sequence_id: Option<i32>,
    aggregate_version: i64,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct AgentToolRecord {
    skill_id: Uuid,
    tool_id: Uuid,
    name: String,
    description: String,
    access_level: Option<String>,
    response_schema: Option<Value>,
    params: Value,
}

struct AgentCatalog {
    agent: AgentDefinitionRecord,
    skills: Vec<AgentSkillRecord>,
    tools: Vec<AgentToolRecord>,
}

pub struct TaskExecutor {
    pool: PgPool,
    http_client: reqwest::Client,
    rule_executor: Arc<MultiThreadRuleExecutor>,
    execution_profiles: BTreeMap<String, ExecutionProfile>,
}

impl TaskExecutor {
    fn supported_task_type_name(task_def: &TaskDefinition) -> Option<&'static str> {
        match task_def {
            TaskDefinition::Ask(_) => Some("ask"),
            TaskDefinition::Assert(_) => Some("assert"),
            TaskDefinition::Call(_) => Some("call"),
            TaskDefinition::Set(_) => Some("set"),
            TaskDefinition::Switch(_) => Some("switch"),
            TaskDefinition::Run(_) => Some("run"),
            _ => None,
        }
    }

    fn policy_task_kind(task_def: &TaskDefinition) -> Result<TaskKind, sqlx::Error> {
        match task_def {
            TaskDefinition::Ask(_) => Ok(TaskKind::Ask),
            TaskDefinition::Assert(_) => Ok(TaskKind::Assert),
            TaskDefinition::Set(_) => Ok(TaskKind::Set),
            TaskDefinition::Switch(_) => Ok(TaskKind::Switch),
            TaskDefinition::Call(call) => match call {
                CallTaskDefinition::Agent(_) => Ok(TaskKind::CallAgent),
                CallTaskDefinition::Mcp(_) => Ok(TaskKind::CallMcp),
                _ => Ok(TaskKind::CallHttp),
            },
            TaskDefinition::Run(run) if run.run.shell.is_some() => Ok(TaskKind::RunShell),
            TaskDefinition::Run(run) if run.run.container.is_some() => Ok(TaskKind::RunContainer),
            TaskDefinition::Run(run) if run.run.script.is_some() => Ok(TaskKind::RunScript),
            TaskDefinition::Run(_) => Err(sqlx::Error::Protocol(
                "run.workflow is not supported by the execution runner".to_string(),
            )),
            _ => Err(sqlx::Error::Protocol(
                "task type is not supported by light-workflow".to_string(),
            )),
        }
    }

    pub fn new(pool: PgPool) -> Self {
        let registry = ActionRegistry::new();
        let engine = Arc::new(RuleEngine::new(Arc::new(registry)));
        let rule_executor = Arc::new(MultiThreadRuleExecutor::new(RuleConfig::default(), engine));
        let http_client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("failed to build reqwest HTTP client with timeouts and redirects disabled");

        Self {
            pool,
            http_client,
            rule_executor,
            execution_profiles: BTreeMap::new(),
        }
    }

    pub fn with_execution_profiles(
        mut self,
        execution_profiles: BTreeMap<String, ExecutionProfile>,
    ) -> Self {
        self.execution_profiles = execution_profiles;
        self
    }

    pub async fn run(&self) -> Result<(), DynError> {
        info!("Starting TaskExecutor loop");
        loop {
            match self.process_next_task().await {
                Ok(true) => {}
                Ok(false) => {
                    sleep(Duration::from_millis(500)).await;
                }
                Err(e) => {
                    error!("Error in TaskExecutor: {}", e);
                    sleep(Duration::from_secs(5)).await;
                }
            }
        }
    }

    async fn process_next_task(&self) -> Result<bool, DynError> {
        let claimed = match self.claim_next_task().await? {
            Some(claimed) => claimed,
            None => return Ok(false),
        };

        info!(
            ">>> Executor processing task: {} ({})",
            claimed.task.wf_task_id, claimed.task.task_type
        );

        let result = if claimed.task.status_code == "C" && claimed.task.task_type == "ask" {
            self.completed_ask_result(&claimed)
        } else {
            match self.execute_task(&claimed).await {
                Ok(result) => result,
                Err(e) => TaskExecutionResult {
                    status_code: "F",
                    task_output: json!({ "error": e.to_string() }),
                    next_task: None,
                    context_data: None,
                },
            }
        };

        let mut tx = self.pool.begin().await?;
        self.finish_task(&mut tx, &claimed, result).await?;
        tx.commit().await?;

        Ok(true)
    }

    pub async fn reconcile_runner_attempt(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        attempt: &TerminalAttempt,
    ) -> Result<bool, DynError> {
        if let Some(approval_id) = sqlx::query_scalar::<_, Uuid>(
            "SELECT approval_id FROM workflow_approval_t
             WHERE host_id=$1 AND consuming_execution_id=$2 AND state='CONSUMED'",
        )
        .bind(attempt.host_id)
        .bind(attempt.execution_id)
        .fetch_optional(&mut **tx)
        .await?
        {
            return self
                .reconcile_fixed_action_attempt(tx, attempt, approval_id)
                .await;
        }
        if !WorkflowRepository::conditionally_accept_terminal_attempt(tx, attempt).await? {
            return Ok(false);
        }
        let claimed = self.load_runner_task(tx, attempt).await?;
        let succeeded = attempt.state == "SUCCEEDED";
        let task_output = if succeeded {
            attempt
                .normalized_result
                .clone()
                .and_then(|result| result.get("structuredOutput").cloned().or(Some(result)))
                .unwrap_or_else(|| json!({}))
        } else {
            json!({
                "executionId": attempt.execution_id,
                "state": attempt.state,
                "error": attempt.normalized_error
            })
        };
        let approval: Option<(Value,)> = sqlx::query_as(
            "SELECT resolved_policy FROM workflow_execution_policy_t p
             JOIN task_info_t t ON t.host_id = p.host_id AND t.task_policy_digest = p.policy_digest
             WHERE t.host_id = $1 AND t.task_id = $2",
        )
        .bind(attempt.host_id)
        .bind(attempt.task_id)
        .fetch_optional(&mut **tx)
        .await?;
        let approval = approval
            .and_then(|row| {
                serde_json::from_value::<workflow_policy::ResolvedExecutionPolicy>(row.0).ok()
            })
            .filter(|policy| policy.approval_required)
            .and_then(|policy| {
                let hold_eligible = policy.persistence == workflow_policy::PersistenceMode::Session
                    && policy.credential_classes.is_empty();
                policy
                    .approval
                    .map(|binding| (policy.policy_digest, binding, hold_eligible))
            });
        if succeeded {
            if let Some((policy_digest, binding, hold_eligible)) = approval {
                self.finish_runner_task_waiting_approval(
                    tx,
                    &claimed,
                    attempt,
                    &task_output,
                    &policy_digest,
                    &binding,
                    hold_eligible,
                )
                .await?;
                return Ok(true);
            }
        }
        self.finish_task(
            tx,
            &claimed,
            TaskExecutionResult {
                status_code: if succeeded { "C" } else { "F" },
                task_output,
                next_task: None,
                context_data: None,
            },
        )
        .await?;
        Ok(true)
    }

    async fn reconcile_fixed_action_attempt(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        attempt: &TerminalAttempt,
        approval_id: Uuid,
    ) -> Result<bool, DynError> {
        let accepted = sqlx::query(
            "UPDATE execution_attempt_t SET accepted_by_origin_ts=CURRENT_TIMESTAMP,
                    updated_ts=CURRENT_TIMESTAMP
             WHERE host_id=$1 AND execution_id=$2 AND lease_id=$3 AND fencing_token=$4
               AND accepted_by_origin_ts IS NULL",
        )
        .bind(attempt.host_id)
        .bind(attempt.execution_id)
        .bind(attempt.lease_id)
        .bind(attempt.fencing_token)
        .execute(&mut **tx)
        .await?;
        if accepted.rows_affected() != 1 {
            return Ok(false);
        }
        sqlx::query(
            "UPDATE runner_scheduling_request_t SET state='SATISFIED',updated_ts=CURRENT_TIMESTAMP
                    WHERE host_id=$1 AND request_id=$2 AND state='ATTEMPT_CREATED'",
        )
        .bind(attempt.host_id)
        .bind(attempt.request_id)
        .execute(&mut **tx)
        .await?;
        if attempt.state != "SUCCEEDED" {
            sqlx::query("UPDATE process_info_t SET status_code='F',custom_status_code='FIXED_ACTION_FAILED',
                        completed_ts=CURRENT_TIMESTAMP,error_info=$1 WHERE host_id=$2 AND process_id=$3")
                .bind(attempt.normalized_error.as_ref().map(Value::to_string))
                .bind(attempt.host_id).bind(attempt.process_id).execute(&mut **tx).await?;
            return Ok(true);
        }
        let task = sqlx::query_as::<_, ActiveTask>(
            "SELECT host_id,task_id,task_type,process_id,wf_instance_id,wf_task_id,status_code,result_code
             FROM task_info_t WHERE host_id=$1 AND task_id=$2 AND process_id=$3 AND status_code='C'
             FOR UPDATE",
        ).bind(attempt.host_id).bind(attempt.task_id).bind(attempt.process_id)
         .fetch_one(&mut **tx).await?;
        let (context_data, wf_def_id, definition_snapshot) = self
            .get_context_data(tx, &task.host_id, &task.process_id)
            .await?;
        let (definition, raw_definition) = if let Some(snapshot) = definition_snapshot {
            (
                serde_json::from_value(snapshot.clone())?,
                serde_yaml::to_value(snapshot)?,
            )
        } else {
            let dsl = self
                .get_workflow_definition(tx, &task.host_id, &wf_def_id)
                .await?;
            (serde_yaml::from_str(&dsl)?, serde_yaml::from_str(&dsl)?)
        };
        let task_output: Value = sqlx::query_scalar(
            "SELECT task_output FROM task_info_t WHERE host_id=$1 AND task_id=$2",
        )
        .bind(task.host_id)
        .bind(task.task_id)
        .fetch_one(&mut **tx)
        .await?;
        sqlx::query(
            "UPDATE process_info_t SET status_code='A',custom_status_code=NULL
                    WHERE host_id=$1 AND process_id=$2 AND status_code='W'",
        )
        .bind(task.host_id)
        .bind(task.process_id)
        .execute(&mut **tx)
        .await?;
        self.handle_transition(
            tx,
            &task,
            &definition,
            &raw_definition,
            context_data,
            task_output,
            None,
            None,
        )
        .await?;
        sqlx::query(
            "UPDATE workflow_approval_t SET reason=COALESCE(reason,'fixed action completed')
                    WHERE host_id=$1 AND approval_id=$2 AND state='CONSUMED'",
        )
        .bind(attempt.host_id)
        .bind(approval_id)
        .execute(&mut **tx)
        .await?;
        Ok(true)
    }

    async fn finish_runner_task_waiting_approval(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        claimed: &ClaimedTask,
        attempt: &TerminalAttempt,
        task_output: &Value,
        policy_digest: &str,
        binding: &workflow_policy::ApprovalBinding,
        hold_eligible: bool,
    ) -> Result<(), sqlx::Error> {
        let artifact_digests: Value = sqlx::query_scalar(
            "SELECT COALESCE(jsonb_agg(content_digest ORDER BY content_digest), '[]'::jsonb)
             FROM workflow_artifact_t
             WHERE host_id = $1 AND execution_id = $2 AND verification_state = 'VERIFIED'",
        )
        .bind(attempt.host_id)
        .bind(attempt.execution_id)
        .fetch_one(&mut **tx)
        .await?;
        let provenance_digest: Option<String> = sqlx::query_scalar(
            "SELECT statement_digest FROM execution_provenance_t
             WHERE host_id = $1 AND execution_id = $2 AND trusted_generator <> ''
             ORDER BY created_ts DESC LIMIT 1",
        )
        .bind(attempt.host_id)
        .bind(attempt.execution_id)
        .fetch_optional(&mut **tx)
        .await?
        .flatten();
        let approval_id = Uuid::now_v7();
        sqlx::query(
            "UPDATE task_info_t SET status_code = 'C', locked = 'N',
                    completed_ts = CURRENT_TIMESTAMP, task_output = $1
             WHERE host_id = $2 AND task_id = $3 AND accepted_attempt = $4",
        )
        .bind(task_output)
        .bind(attempt.host_id)
        .bind(attempt.task_id)
        .bind(attempt.attempt_number)
        .execute(&mut **tx)
        .await?;
        let new_context = self.apply_exports(
            &claimed.raw_definition,
            &claimed.task.wf_task_id,
            claimed.context_data.clone(),
            task_output,
        );
        sqlx::query(
            "INSERT INTO workflow_approval_t (
                host_id, approval_id, process_id, task_id, preceding_execution_id,
                artifact_digest_set, provenance_digest, target, operation,
                policy_digest, state, expires_ts
             ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,'REQUESTED',
                       CURRENT_TIMESTAMP + make_interval(secs => $11))",
        )
        .bind(attempt.host_id)
        .bind(approval_id)
        .bind(attempt.process_id)
        .bind(attempt.task_id)
        .bind(attempt.execution_id)
        .bind(artifact_digests)
        .bind(provenance_digest)
        .bind(&binding.target)
        .bind(&binding.operation)
        .bind(policy_digest)
        .bind(binding.ttl_seconds as i64)
        .execute(&mut **tx)
        .await?;
        if hold_eligible {
            sqlx::query("UPDATE execution_session_t SET state='IDLE_APPROVAL_HOLD',hold_id=$1,
                        hold_reason='approval',hold_until_ts=LEAST(effective_expires_ts,
                          CURRENT_TIMESTAMP+make_interval(secs=>$2)),hold_policy_digest=$3,
                        session_version=session_version+1,session_fence=session_fence+1,
                        retained_resource_evidence=jsonb_build_object('reason','approval','checkpointRequired',true),
                        updated_ts=CURRENT_TIMESTAMP WHERE host_id=$4 AND subject_id=$5
                        AND policy_digest=$3 AND state='IDLE' AND cleanup_status='NOT_REQUESTED'")
                .bind(approval_id).bind(binding.ttl_seconds as i64).bind(policy_digest)
                .bind(attempt.host_id).bind(attempt.task_id).execute(&mut **tx).await?;
        }
        sqlx::query(
            "UPDATE process_info_t SET status_code = 'W',
                    custom_status_code = 'WAITING_APPROVAL', context_data = $1,
                    ex_trigger_ts = CURRENT_TIMESTAMP
             WHERE host_id = $2 AND process_id = $3 AND status_code = 'A'",
        )
        .bind(new_context)
        .bind(attempt.host_id)
        .bind(attempt.process_id)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    async fn load_runner_task(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        attempt: &TerminalAttempt,
    ) -> Result<ClaimedTask, DynError> {
        let task = sqlx::query_as::<_, ActiveTask>(
            "SELECT host_id, task_id, task_type, process_id, wf_instance_id,
                    wf_task_id, status_code, result_code
             FROM task_info_t
             WHERE host_id = $1 AND task_id = $2 AND process_id = $3
               AND execution_placement = 'runner' AND status_code = 'A'
               AND accepted_attempt = $4
             FOR UPDATE",
        )
        .bind(attempt.host_id)
        .bind(attempt.task_id)
        .bind(attempt.process_id)
        .bind(attempt.attempt_number)
        .fetch_one(&mut **tx)
        .await?;
        let (context_data, wf_def_id, definition_snapshot) = self
            .get_context_data(tx, &task.host_id, &task.process_id)
            .await?;
        let (definition, raw_definition) = if let Some(snapshot) = definition_snapshot {
            (
                serde_json::from_value::<WorkflowDefinition>(snapshot.clone())?,
                serde_yaml::to_value(snapshot)?,
            )
        } else {
            warn!(
                host_id = %task.host_id,
                process_id = %task.process_id,
                "runner result used mutable legacy definition because no snapshot exists"
            );
            let dsl_yaml = self
                .get_workflow_definition(tx, &task.host_id, &wf_def_id)
                .await?;
            (
                serde_yaml::from_str(&dsl_yaml)?,
                serde_yaml::from_str(&dsl_yaml)?,
            )
        };
        Ok(ClaimedTask {
            task,
            context_data,
            definition,
            raw_definition,
        })
    }

    async fn claim_next_task(&self) -> Result<Option<ClaimedTask>, DynError> {
        let mut tx = self.pool.begin().await?;

        let task_res = sqlx::query_as::<_, ActiveTask>(CLAIM_NEXT_HOST_TASK_SQL)
            .bind(TASK_LOCK_TIMEOUT_MINUTES)
            .fetch_optional(&mut *tx)
            .await?;

        let task = match task_res {
            Some(task) => task,
            None => {
                tx.commit().await?;
                return Ok(None);
            }
        };

        let (context_data, wf_def_id, definition_snapshot) = self
            .get_context_data(&mut tx, &task.host_id, &task.process_id)
            .await?;
        let (definition, raw_definition) = if let Some(snapshot) = definition_snapshot {
            let definition = serde_json::from_value::<WorkflowDefinition>(snapshot.clone())?;
            let raw_definition = serde_yaml::to_value(snapshot)?;
            (definition, raw_definition)
        } else {
            warn!(
                host_id = %task.host_id,
                process_id = %task.process_id,
                "workflow process has no definition snapshot; using mutable legacy definition"
            );
            let dsl_yaml = self
                .get_workflow_definition(&mut tx, &task.host_id, &wf_def_id)
                .await?;
            (
                serde_yaml::from_str(&dsl_yaml)?,
                serde_yaml::from_str(&dsl_yaml)?,
            )
        };
        tx.commit().await?;

        Ok(Some(ClaimedTask {
            task,
            context_data,
            definition,
            raw_definition,
        }))
    }

    async fn execute_task(&self, claimed: &ClaimedTask) -> Result<TaskExecutionResult, DynError> {
        let task_def = self
            .find_task_definition(&claimed.definition, &claimed.task.wf_task_id)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("task definition not found: {}", claimed.task.wf_task_id),
                )
            })?;

        match task_def {
            TaskDefinition::Ask(ask_task) => Ok(TaskExecutionResult {
                status_code: "W",
                task_output: json!({
                    "status": "waiting_for_input",
                    "ask": ask_task.ask,
                    "message": "Task is waiting for human input"
                }),
                next_task: None,
                context_data: None,
            }),
            TaskDefinition::Assert(assert_task) => {
                self.execute_assert_task(&assert_task.assert, &claimed.context_data)
            }
            TaskDefinition::Call(CallTaskDefinition::Http(http_call)) => {
                let configured_uri = match &http_call.with.endpoint {
                    workflow_core::models::resource::OneOfEndpointDefinitionOrUri::Uri(uri) => {
                        uri.clone()
                    }
                    workflow_core::models::resource::OneOfEndpointDefinitionOrUri::Endpoint(
                        endpoint,
                    ) => endpoint.uri.clone(),
                };
                let resolved_uri =
                    self.resolve_template_to_string(&configured_uri, &claimed.context_data);
                let validated_uri = self.validate_resolved_uri(&configured_uri, &resolved_uri)?;

                let method = reqwest::Method::from_bytes(http_call.with.method.as_bytes())
                    .map_err(|err| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("invalid HTTP method '{}': {}", http_call.with.method, err),
                        )
                    })?;
                let mut req_builder = self.http_client.request(method, validated_uri.clone());

                if let Some(body) = &http_call.with.body {
                    req_builder =
                        req_builder.json(&self.resolve_json_value(body, &claimed.context_data));
                }

                info!(">>> Making HTTP request to: {}", validated_uri);
                let mut resp = req_builder.send().await?;
                let status = resp.status();
                if resp.content_length().unwrap_or(0) > MAX_HTTP_RESPONSE_BYTES as u64 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "HTTP response too large: {} bytes exceeds {} byte limit",
                            resp.content_length().unwrap_or(0),
                            MAX_HTTP_RESPONSE_BYTES
                        ),
                    )
                    .into());
                }
                let mut body = Vec::new();
                while let Some(chunk) = resp.chunk().await? {
                    let new_len = body.len().saturating_add(chunk.len());
                    if new_len > MAX_HTTP_RESPONSE_BYTES {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "HTTP response too large: more than {} bytes",
                                MAX_HTTP_RESPONSE_BYTES
                            ),
                        )
                        .into());
                    }
                    body.extend_from_slice(&chunk);
                }

                let task_output = if status.is_success() {
                    if body.is_empty() {
                        json!({ "status": "success" })
                    } else if let Ok(json_body) = serde_json::from_slice::<Value>(&body) {
                        json_body
                    } else {
                        json!({
                            "status": "success",
                            "body": String::from_utf8_lossy(&body).to_string()
                        })
                    }
                } else {
                    json!({
                        "error": status.as_u16(),
                        "message": "HTTP call failed",
                        "body": String::from_utf8_lossy(&body).to_string()
                    })
                };

                Ok(TaskExecutionResult {
                    status_code: if status.is_success() { "C" } else { "F" },
                    task_output,
                    next_task: None,
                    context_data: None,
                })
            }
            TaskDefinition::Call(CallTaskDefinition::JsonRpc(jsonrpc_call)) => {
                self.execute_jsonrpc_call(&jsonrpc_call.with, &claimed.context_data)
                    .await
            }
            TaskDefinition::Call(CallTaskDefinition::OpenRpc(openrpc_call)) => {
                self.execute_openrpc_call(&openrpc_call.with, &claimed.context_data)
                    .await
            }
            TaskDefinition::Call(CallTaskDefinition::Mcp(mcp_call)) => {
                self.execute_mcp_call(&mcp_call.with, &claimed.definition, &claimed.context_data)
                    .await
            }
            TaskDefinition::Call(CallTaskDefinition::Agent(agent_call)) => {
                self.execute_agent_call(
                    &agent_call.with,
                    &claimed.context_data,
                    &claimed.raw_definition,
                    &claimed.task.host_id,
                    &claimed.task.wf_task_id,
                )
                .await
            }
            TaskDefinition::Call(CallTaskDefinition::Rule(rule_call)) => {
                let rule_id = &rule_call.with.rule_id;
                info!(">>> Making Rule Engine call to: {}", rule_id);

                let mut context = claimed.context_data.clone();
                match self.rule_executor.execute_rule(rule_id, &mut context).await {
                    Ok(passed) => Ok(TaskExecutionResult {
                        status_code: "C",
                        task_output: json!({ "passed": passed, "mutated_context": context }),
                        next_task: None,
                        context_data: Some(context),
                    }),
                    Err(e) => Ok(TaskExecutionResult {
                        status_code: "F",
                        task_output: json!({ "error": 500, "message": format!("Rule engine failed: {}", e) }),
                        next_task: None,
                        context_data: None,
                    }),
                }
            }
            TaskDefinition::Set(set_task) => {
                let output = match &set_task.set {
                    SetValue::Map(values) => {
                        let mut resolved = JsonMap::new();
                        for (key, value) in values {
                            resolved.insert(
                                key.clone(),
                                self.resolve_json_value(value, &claimed.context_data),
                            );
                        }
                        Value::Object(resolved)
                    }
                    SetValue::Expression(expression) => self.resolve_json_value(
                        &Value::String(expression.clone()),
                        &claimed.context_data,
                    ),
                };

                Ok(TaskExecutionResult {
                    status_code: "C",
                    task_output: output,
                    next_task: None,
                    context_data: None,
                })
            }
            TaskDefinition::Switch(switch_task) => {
                let mut next_task = None;
                let mut default_next = None;

                for entry in &switch_task.switch.entries {
                    for (case_name, case_def) in entry {
                        if case_name.eq_ignore_ascii_case("default") && default_next.is_none() {
                            default_next = case_def.then.clone();
                            continue;
                        }

                        let when = case_def.when.as_deref().or_else(|| {
                            (!case_name.eq_ignore_ascii_case("default"))
                                .then_some(case_name.as_str())
                        });

                        if let Some(when) = when {
                            if self.evaluate_condition(when, &claimed.context_data)? {
                                next_task = case_def.then.clone();
                                break;
                            }
                        }
                    }

                    if next_task.is_some() {
                        break;
                    }
                }

                Ok(TaskExecutionResult {
                    status_code: "C",
                    task_output: json!({
                        "matched": next_task.is_some(),
                        "nextTask": next_task.clone().or(default_next.clone())
                    }),
                    next_task: next_task.or(default_next),
                    context_data: None,
                })
            }
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "unsupported executable task type for {}: {}",
                    claimed.task.wf_task_id, claimed.task.task_type
                ),
            )
            .into()),
        }
    }

    fn completed_ask_result(&self, claimed: &ClaimedTask) -> TaskExecutionResult {
        let task_output = claimed
            .task
            .result_code
            .as_ref()
            .map(|result_code| {
                serde_json::from_str::<Value>(result_code)
                    .unwrap_or_else(|_| Value::String(result_code.clone()))
            })
            .unwrap_or_else(|| Value::String("completed".to_string()));

        TaskExecutionResult {
            status_code: "C",
            task_output,
            next_task: None,
            context_data: None,
        }
    }

    async fn execute_jsonrpc_call(
        &self,
        args: &JsonRpcArguments,
        context: &Value,
    ) -> Result<TaskExecutionResult, DynError> {
        let configured_uri = self.endpoint_to_uri(&args.endpoint);
        self.execute_jsonrpc_request(
            &configured_uri,
            &args.method,
            args.params.as_ref(),
            args.id.as_ref(),
            args.notification.unwrap_or(false),
            args.headers.as_ref(),
            args.output.as_deref(),
            args.error_policy.as_ref(),
            context,
        )
        .await
    }

    async fn execute_openrpc_call(
        &self,
        args: &OpenRpcArguments,
        context: &Value,
    ) -> Result<TaskExecutionResult, DynError> {
        let document = self.fetch_external_json(&args.document, context).await?;
        let method_definition = self.find_openrpc_method(&document, &args.method)?;
        let resolved_params = args
            .params
            .as_ref()
            .map(|params| self.resolve_json_value(params, context));
        self.validate_openrpc_params(method_definition, &args.method, resolved_params.as_ref())?;
        let configured_uri = self.resolve_openrpc_server_uri(&document, args.server.as_ref())?;
        self.execute_jsonrpc_request(
            &configured_uri,
            &args.method,
            resolved_params.as_ref(),
            args.id.as_ref(),
            args.notification.unwrap_or(false),
            None,
            args.output.as_deref(),
            args.error_policy.as_ref(),
            context,
        )
        .await
    }

    async fn execute_mcp_call(
        &self,
        args: &McpArguments,
        definition: &WorkflowDefinition,
        context: &Value,
    ) -> Result<TaskExecutionResult, DynError> {
        let server = self.resolve_mcp_server(args, definition)?;
        if let Some(transport) = server.transport.as_deref() {
            if !matches!(transport, "http" | "streamable-http") {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("unsupported MCP transport '{}'", transport),
                )
                .into());
            }
        }

        let endpoint = server.endpoint.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "MCP call requires an endpoint from with.server, with.session, or with.serverRef",
            )
        })?;
        let configured_uri = self.endpoint_to_uri(endpoint);
        let arguments = args
            .arguments
            .as_ref()
            .map(|arguments| {
                Value::Object(
                    arguments
                        .iter()
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect(),
                )
            })
            .unwrap_or_else(|| json!({}));

        let (method, params) = if let Some(tool) = &args.tool {
            (
                "tools/call".to_string(),
                json!({
                    "name": tool,
                    "arguments": arguments
                }),
            )
        } else if let Some(resource) = &args.resource {
            (
                "resources/read".to_string(),
                json!({
                    "uri": self.resolve_template_to_string(resource, context)
                }),
            )
        } else if let Some(prompt) = &args.prompt {
            (
                "prompts/get".to_string(),
                json!({
                    "name": prompt,
                    "arguments": arguments
                }),
            )
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "MCP call requires one of tool, resource, or prompt",
            )
            .into());
        };

        self.execute_jsonrpc_request(
            &configured_uri,
            &method,
            Some(&params),
            None,
            false,
            None,
            args.output.as_deref().or(Some("result")),
            None,
            context,
        )
        .await
    }

    async fn execute_agent_call(
        &self,
        args: &AgentArguments,
        context: &Value,
        raw_definition: &YamlValue,
        host_id: &Uuid,
        task_name: &str,
    ) -> Result<TaskExecutionResult, DynError> {
        let catalog = self
            .load_agent_catalog(host_id, &args.agent, args.skill.as_deref())
            .await?;
        let task_input = args
            .input
            .as_ref()
            .map(|input| self.resolve_json_value(input, context))
            .unwrap_or_else(|| context.clone());
        let output_schema = self.resolve_agent_output_schema(args, raw_definition)?;
        let retry_count = args
            .on_invalid_output
            .as_ref()
            .and_then(|policy| policy.retry)
            .unwrap_or(0);
        let max_attempts = retry_count.saturating_add(1);
        let mut last_error = None;

        info!(
            ">>> Executing agent task {} with agent {}",
            task_name, args.agent
        );

        for attempt in 1..=max_attempts {
            let raw_output = if let Some(mock_output) = &args.mock_output {
                serde_json::to_string(&self.resolve_json_value(mock_output, context))?
            } else if Self::is_mock_provider(&catalog.agent.model_provider) {
                serde_json::to_string(&Self::mock_agent_output(output_schema.as_ref()))?
            } else {
                self.execute_agent_model_call(
                    args,
                    &catalog,
                    &task_input,
                    context,
                    output_schema.as_ref(),
                )
                .await?
            };

            if raw_output.len() > MAX_AGENT_OUTPUT_BYTES {
                last_error = Some(format!(
                    "agent output exceeded {} bytes",
                    MAX_AGENT_OUTPUT_BYTES
                ));
                warn!(
                    "Agent task {} attempt {} produced oversized output",
                    task_name, attempt
                );
                continue;
            }

            match Self::parse_agent_json_output(&raw_output)
                .and_then(|output| Self::validate_agent_output(output, output_schema.as_ref()))
            {
                Ok(mut output) => {
                    let audit = Self::agent_audit_output(&catalog, attempt, &output, None);
                    Self::attach_agent_audit(&mut output, audit);
                    return Ok(TaskExecutionResult {
                        status_code: "C",
                        task_output: output,
                        next_task: None,
                        context_data: None,
                    });
                }
                Err(err) => {
                    let message = err.to_string();
                    warn!(
                        "Agent task {} attempt {} returned invalid output: {}",
                        task_name, attempt, message
                    );
                    last_error = Some(message);
                }
            }
        }

        let detail = last_error.unwrap_or_else(|| "agent output was invalid".to_string());
        let error_output = json!({
            "error": "invalid_agent_output",
            "detail": detail,
            "_agentAudit": Self::agent_audit_output(&catalog, max_attempts, &json!({}), Some("invalid_agent_output")),
        });

        if let Some(next_task) = args
            .on_invalid_output
            .as_ref()
            .and_then(|policy| policy.then.clone())
        {
            Ok(TaskExecutionResult {
                status_code: "C",
                task_output: error_output,
                next_task: Some(next_task),
                context_data: None,
            })
        } else {
            Ok(TaskExecutionResult {
                status_code: "F",
                task_output: error_output,
                next_task: None,
                context_data: None,
            })
        }
    }

    async fn load_agent_catalog(
        &self,
        host_id: &Uuid,
        agent_ref: &str,
        skill_ref: Option<&str>,
    ) -> Result<AgentCatalog, DynError> {
        let agent = sqlx::query_as::<_, AgentDefinitionRecord>(
            r#"
            SELECT ad.agent_def_id,
                   a.api_name AS agent_name,
                   ad.model_provider,
                   ad.model_name,
                   ad.api_key_ref,
                   COALESCE(ad.temperature, 0.7)::float8 AS temperature,
                   ad.max_tokens,
                   ad.aggregate_version
            FROM agent_definition_t ad
            LEFT JOIN api_version_t av
              ON av.host_id = ad.host_id
             AND av.api_version_id = ad.agent_def_id
             AND av.active = TRUE
            LEFT JOIN api_t a
              ON a.host_id = av.host_id
             AND a.api_id = av.api_id
             AND a.active = TRUE
            WHERE ad.host_id = $1
              AND ad.active = TRUE
              AND (ad.agent_def_id::text = $2 OR LOWER(COALESCE(a.api_name, '')) = LOWER($2))
            LIMIT 1
            "#,
        )
        .bind(host_id)
        .bind(agent_ref)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("active agent definition not found: {}", agent_ref),
            )
        })?;

        let skills = sqlx::query_as::<_, AgentSkillRecord>(
            r#"
            SELECT s.skill_id,
                   s.name,
                   s.description,
                   s.content_markdown,
                   ag.priority,
                   ag.sequence_id,
                   GREATEST(ag.aggregate_version, s.aggregate_version) AS aggregate_version
            FROM agent_skill_t ag
            JOIN skill_t s
              ON s.host_id = ag.host_id
             AND s.skill_id = ag.skill_id
            WHERE ag.host_id = $1
              AND ag.agent_def_id = $2
              AND ag.active = TRUE
              AND s.active = TRUE
              AND ($3::text IS NULL OR s.skill_id::text = $3 OR LOWER(s.name) = LOWER($3))
            ORDER BY COALESCE(ag.sequence_id, 0), COALESCE(ag.priority, 0) DESC, s.name
            "#,
        )
        .bind(host_id)
        .bind(agent.agent_def_id)
        .bind(skill_ref)
        .fetch_all(&self.pool)
        .await?;

        if skills.is_empty() {
            let message = match skill_ref {
                Some(skill) => format!(
                    "active skill '{}' is not attached to agent {}",
                    skill, agent_ref
                ),
                None => format!("agent {} has no active skills", agent_ref),
            };
            return Err(io::Error::new(io::ErrorKind::NotFound, message).into());
        }

        let skill_ids: Vec<Uuid> = skills.iter().map(|skill| skill.skill_id).collect();
        let tools = sqlx::query_as::<_, AgentToolRecord>(
            r#"
            SELECT st.skill_id,
                   t.tool_id,
                   t.name,
                   t.description,
                   st.access_level,
                   t.response_schema,
                   COALESCE(
                     jsonb_agg(
                       jsonb_build_object(
                         'name', tp.name,
                         'type', tp.param_type,
                         'required', tp.required,
                         'description', tp.description,
                         'validationSchema', tp.validation_schema
                       )
                       ORDER BY tp.order_index
                     ) FILTER (WHERE tp.param_id IS NOT NULL),
                     '[]'::jsonb
                   ) AS params
            FROM skill_tool_t st
            JOIN tool_t t
              ON t.host_id = st.host_id
             AND t.tool_id = st.tool_id
            LEFT JOIN tool_param_t tp
              ON tp.host_id = t.host_id
             AND tp.tool_id = t.tool_id
             AND tp.active = TRUE
            WHERE st.host_id = $1
              AND st.skill_id = ANY($2)
              AND st.active = TRUE
              AND t.active = TRUE
            GROUP BY st.skill_id, t.tool_id, t.name, t.description, st.access_level, t.response_schema
            ORDER BY st.skill_id, t.name
            "#,
        )
        .bind(host_id)
        .bind(&skill_ids)
        .fetch_all(&self.pool)
        .await?;

        Ok(AgentCatalog {
            agent,
            skills,
            tools,
        })
    }

    async fn execute_agent_model_call(
        &self,
        args: &AgentArguments,
        catalog: &AgentCatalog,
        task_input: &Value,
        context: &Value,
        output_schema: Option<&Value>,
    ) -> Result<String, DynError> {
        let provider = self.build_agent_provider(&catalog.agent)?;
        let messages =
            self.build_agent_messages(args, catalog, task_input, context, output_schema)?;
        let response = provider
            .chat(
                ChatRequest {
                    messages: &messages,
                    tools: None,
                },
                &catalog.agent.model_name,
                catalog.agent.temperature,
            )
            .await?;

        response.text.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "agent provider returned no text content",
            )
            .into()
        })
    }

    fn build_agent_provider(
        &self,
        agent: &AgentDefinitionRecord,
    ) -> Result<Box<dyn Provider>, DynError> {
        let api_key = self.resolve_agent_api_key(agent);
        let base_url = self.provider_base_url(&agent.model_provider);
        let max_tokens = agent
            .max_tokens
            .and_then(|value| u32::try_from(value).ok())
            .filter(|value| *value > 0);
        let provider_name = agent.model_provider.to_ascii_lowercase();

        match provider_name.as_str() {
            "openai" | "open-ai" => Ok(Box::new(
                OpenAiProvider::new(base_url.as_deref(), api_key.as_deref())?
                    .with_max_tokens(max_tokens),
            )),
            "anthropic" | "claude" => {
                let mut provider = AnthropicProvider::new(base_url.as_deref(), api_key.as_deref())?;
                if let Some(max_tokens) = max_tokens {
                    provider = provider.with_max_tokens(max_tokens);
                }
                Ok(Box::new(provider))
            }
            "gemini" | "google" | "google-gemini" => {
                let mut provider = GeminiProvider::new(base_url.as_deref(), api_key.as_deref())?;
                if let Some(max_tokens) = max_tokens {
                    provider = provider.with_max_tokens(max_tokens);
                }
                Ok(Box::new(provider))
            }
            "ollama" => Ok(Box::new(OllamaProvider::new(
                base_url.as_deref(),
                api_key.as_deref(),
            )?)),
            "openrouter" | "open-router" => Ok(Box::new(
                OpenRouterProvider::new(base_url.as_deref(), api_key.as_deref())?
                    .with_max_tokens(max_tokens),
            )),
            "compatible" | "openai-compatible" | "open-ai-compatible" => {
                let base_url = base_url.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "compatible agent provider requires LIGHT_WORKFLOW_AGENT_COMPATIBLE_BASE_URL or COMPATIBLE_BASE_URL",
                    )
                })?;
                Ok(Box::new(
                    CompatibleProvider::new(&agent.model_provider, &base_url, api_key.as_deref())?
                        .with_max_tokens(max_tokens),
                ))
            }
            other => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unsupported agent model provider '{}'", other),
            )
            .into()),
        }
    }

    fn build_agent_messages(
        &self,
        args: &AgentArguments,
        catalog: &AgentCatalog,
        task_input: &Value,
        context: &Value,
        output_schema: Option<&Value>,
    ) -> Result<Vec<ChatMessage>, DynError> {
        let mut system = String::from(
            "You are executing a bounded light-workflow agent task. Workflow context is authoritative. Do not use private memory for cross-step state. Return only one JSON object and no markdown.",
        );

        system.push_str("\n\nSelected skills:");
        for skill in &catalog.skills {
            system.push_str(&format!(
                "\n\n## {} ({})\nsequence: {:?}, priority: {:?}",
                skill.name, skill.skill_id, skill.sequence_id, skill.priority
            ));
            if let Some(description) = &skill.description {
                system.push_str(&format!("\ndescription: {}", description));
            }
            system.push_str("\n");
            system.push_str(&skill.content_markdown);
        }

        if !catalog.tools.is_empty() {
            system.push_str(
                "\n\nPermitted skill tools are listed for context and future tool routing. In this runtime phase, API orchestration remains explicit workflow tasks; do not invent unlisted tools.",
            );
            let tool_catalog: Vec<Value> = catalog
                .tools
                .iter()
                .map(|tool| {
                    json!({
                        "skillId": tool.skill_id,
                        "toolId": tool.tool_id,
                        "name": tool.name,
                        "description": tool.description,
                        "accessLevel": tool.access_level,
                        "params": tool.params,
                        "responseSchema": tool.response_schema,
                    })
                })
                .collect();
            system.push_str("\n");
            system.push_str(&serde_json::to_string_pretty(&tool_catalog)?);
        }

        if let Some(instructions) = &args.instructions {
            system.push_str("\n\nAdditional instructions:\n");
            system.push_str(&self.resolve_template_to_string(instructions, context));
        }

        if let Some(output_schema) = output_schema {
            system.push_str("\n\nOutput JSON schema subset:\n");
            system.push_str(&serde_json::to_string_pretty(output_schema)?);
        }

        let mut user_payload = json!({
            "taskInput": task_input,
            "workflowContext": context,
        });
        if let Some(prompt) = &args.prompt {
            user_payload["prompt"] =
                Value::String(self.resolve_template_to_string(prompt, context));
        }

        Ok(vec![
            ChatMessage::system(system),
            ChatMessage::user(serde_json::to_string_pretty(&user_payload)?),
        ])
    }

    fn resolve_agent_api_key(&self, agent: &AgentDefinitionRecord) -> Option<String> {
        if let Some(api_key_ref) = agent
            .api_key_ref
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            if let Some(value) = api_key_ref.strip_prefix("literal:") {
                return Some(value.to_string());
            }

            let env_name = api_key_ref.strip_prefix("env:").unwrap_or(api_key_ref);
            match env::var(env_name) {
                Ok(value) if !value.trim().is_empty() => return Some(value),
                _ => warn!(
                    "Agent api_key_ref '{}' was not found as an environment variable",
                    api_key_ref
                ),
            }
        }

        for env_name in Self::provider_api_key_env_names(&agent.model_provider) {
            if let Ok(value) = env::var(env_name) {
                if !value.trim().is_empty() {
                    return Some(value);
                }
            }
        }

        None
    }

    fn provider_base_url(&self, provider: &str) -> Option<String> {
        let normalized = provider
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_uppercase()
                } else {
                    '_'
                }
            })
            .collect::<String>();
        let keys = [
            format!("LIGHT_WORKFLOW_AGENT_{}_BASE_URL", normalized),
            format!("{}_BASE_URL", normalized),
        ];

        keys.iter().find_map(|key| {
            env::var(key)
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
    }

    fn provider_api_key_env_names(provider: &str) -> Vec<&'static str> {
        match provider.to_ascii_lowercase().as_str() {
            "openai" | "open-ai" => vec!["OPENAI_API_KEY"],
            "anthropic" | "claude" => vec!["ANTHROPIC_API_KEY"],
            "gemini" | "google" | "google-gemini" => vec!["GEMINI_API_KEY", "GOOGLE_API_KEY"],
            "ollama" => vec!["OLLAMA_API_KEY"],
            "openrouter" | "open-router" => vec!["OPENROUTER_API_KEY"],
            "compatible" | "openai-compatible" | "open-ai-compatible" => {
                vec!["COMPATIBLE_API_KEY", "OPENAI_API_KEY"]
            }
            _ => Vec::new(),
        }
    }

    fn is_mock_provider(provider: &str) -> bool {
        matches!(
            provider.to_ascii_lowercase().as_str(),
            "mock" | "stub" | "echo"
        )
    }

    fn resolve_agent_output_schema(
        &self,
        args: &AgentArguments,
        raw_definition: &YamlValue,
    ) -> Result<Option<Value>, DynError> {
        if let Some(schema) = &args.output_schema {
            return Ok(Some(schema.clone()));
        }

        let Some(schema_ref) = args.output_schema_ref.as_deref() else {
            return Ok(None);
        };

        for parent in [
            raw_definition.get("agentSchemas"),
            raw_definition.get("outputSchemas"),
            raw_definition.get("schemas"),
            raw_definition
                .get("use")
                .and_then(|use_| use_.get("agentSchemas")),
            raw_definition
                .get("use")
                .and_then(|use_| use_.get("schemas")),
        ]
        .into_iter()
        .flatten()
        {
            if let Some(schema) = parent.get(schema_ref) {
                return serde_json::to_value(schema).map(Some).map_err(|err| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid outputSchemaRef '{}': {}", schema_ref, err),
                    )
                    .into()
                });
            }
        }

        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("outputSchemaRef '{}' not found", schema_ref),
        )
        .into())
    }

    fn parse_agent_json_output(output: &str) -> Result<Value, DynError> {
        let output = output.trim();
        if output.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "agent output is empty").into());
        }

        if let Ok(value) = serde_json::from_str::<Value>(output) {
            return Ok(value);
        }

        if let Some(fence_start) = output.find("```") {
            let mut fenced = &output[fence_start + 3..];
            fenced = fenced.trim_start();
            if let Some(rest) = fenced.strip_prefix("json") {
                fenced = rest.trim_start();
            }
            if let Some(fence_end) = fenced.find("```") {
                if let Ok(value) = serde_json::from_str::<Value>(fenced[..fence_end].trim()) {
                    return Ok(value);
                }
            }
        }

        if let (Some(start), Some(end)) = (output.find('{'), output.rfind('}')) {
            if start < end {
                return serde_json::from_str::<Value>(&output[start..=end]).map_err(|err| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("agent output is not valid JSON: {}", err),
                    )
                    .into()
                });
            }
        }

        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "agent output did not contain a JSON object",
        )
        .into())
    }

    fn validate_agent_output(
        output: Value,
        output_schema: Option<&Value>,
    ) -> Result<Value, DynError> {
        if !output.is_object() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "agent output must be a JSON object",
            )
            .into());
        }

        if let Some(schema) = output_schema {
            Self::validate_json_schema_subset("$", schema, &output).map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("agent output failed schema validation: {}", err),
                )
            })?;
        }

        Ok(output)
    }

    fn validate_json_schema_subset(
        path: &str,
        schema: &Value,
        value: &Value,
    ) -> Result<(), String> {
        if let Some(enum_values) = schema.get("enum").and_then(Value::as_array) {
            if !enum_values.iter().any(|candidate| candidate == value) {
                return Err(format!("{} value {} is not in enum", path, value));
            }
        }

        if let Some(schema_type) = schema.get("type").and_then(Value::as_str) {
            let type_matches = match schema_type {
                "object" => value.is_object(),
                "array" => value.is_array(),
                "string" => value.is_string(),
                "boolean" => value.is_boolean(),
                "number" => value.is_number(),
                "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
                "null" => value.is_null(),
                _ => true,
            };
            if !type_matches {
                return Err(format!("{} expected {}, got {}", path, schema_type, value));
            }
        }

        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            let object = value
                .as_object()
                .ok_or_else(|| format!("{} required fields need an object", path))?;
            for field in required {
                let Some(field) = field.as_str() else {
                    continue;
                };
                if !object.contains_key(field) || object.get(field).is_some_and(Value::is_null) {
                    return Err(format!("{} missing required field {}", path, field));
                }
            }
        }

        if let (Some(properties), Some(object)) = (
            schema.get("properties").and_then(Value::as_object),
            value.as_object(),
        ) {
            for (property, property_schema) in properties {
                if let Some(property_value) = object.get(property) {
                    Self::validate_json_schema_subset(
                        &format!("{}.{}", path, property),
                        property_schema,
                        property_value,
                    )?;
                }
            }
        }

        if let (Some(items_schema), Some(values)) = (schema.get("items"), value.as_array()) {
            for (index, item) in values.iter().enumerate() {
                Self::validate_json_schema_subset(
                    &format!("{}[{}]", path, index),
                    items_schema,
                    item,
                )?;
            }
        }

        Ok(())
    }

    fn mock_agent_output(output_schema: Option<&Value>) -> Value {
        let Some(schema) = output_schema else {
            return json!({ "status": "MOCK_COMPLETED" });
        };
        let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
            return json!({ "status": "MOCK_COMPLETED" });
        };

        let mut output = JsonMap::new();
        for required in schema
            .get("required")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if let Some(property_schema) = properties.get(required) {
                output.insert(
                    required.to_string(),
                    Self::mock_value_for_schema(property_schema),
                );
            }
        }

        if output.is_empty() {
            output.insert(
                "status".to_string(),
                Value::String("MOCK_COMPLETED".to_string()),
            );
        }

        Value::Object(output)
    }

    fn mock_value_for_schema(schema: &Value) -> Value {
        match schema.get("type").and_then(Value::as_str) {
            Some("boolean") => Value::Bool(false),
            Some("integer") => json!(0),
            Some("number") => json!(0.0),
            Some("array") => Value::Array(Vec::new()),
            Some("object") => Value::Object(JsonMap::new()),
            _ => schema
                .get("enum")
                .and_then(Value::as_array)
                .and_then(|values| values.first())
                .cloned()
                .unwrap_or_else(|| Value::String("MOCK".to_string())),
        }
    }

    fn attach_agent_audit(output: &mut Value, audit: Value) {
        if let Some(object) = output.as_object_mut() {
            object.insert("_agentAudit".to_string(), audit);
        }
    }

    fn agent_audit_output(
        catalog: &AgentCatalog,
        attempts: u32,
        output: &Value,
        error: Option<&str>,
    ) -> Value {
        let catalog_version = catalog
            .skills
            .iter()
            .map(|skill| skill.aggregate_version)
            .chain(std::iter::once(catalog.agent.aggregate_version))
            .max()
            .unwrap_or(catalog.agent.aggregate_version);
        json!({
            "agentDefId": catalog.agent.agent_def_id,
            "agentName": catalog.agent.agent_name,
            "modelProvider": catalog.agent.model_provider,
            "modelName": catalog.agent.model_name,
            "promptVersion": AGENT_PROMPT_VERSION,
            "skillIds": catalog.skills.iter().map(|skill| skill.skill_id).collect::<Vec<_>>(),
            "skillNames": catalog.skills.iter().map(|skill| skill.name.clone()).collect::<Vec<_>>(),
            "toolIds": catalog.tools.iter().map(|tool| tool.tool_id).collect::<Vec<_>>(),
            "toolNames": catalog.tools.iter().map(|tool| tool.name.clone()).collect::<Vec<_>>(),
            "attempts": attempts,
            "catalogAggregateVersion": catalog_version,
            "error": error,
            "outputSummary": Self::agent_output_summary(output),
        })
    }

    fn agent_output_summary(output: &Value) -> Value {
        if let Some(object) = output.as_object() {
            let keys: Vec<String> = object
                .keys()
                .filter(|key| key.as_str() != "_agentAudit")
                .take(8)
                .cloned()
                .collect();
            json!({
                "type": "object",
                "keys": keys,
                "status": object
                    .get("status")
                    .or_else(|| object.get("decision"))
                    .or_else(|| object.get("recommendation"))
                    .cloned()
                    .unwrap_or(Value::Null),
            })
        } else {
            json!({ "type": "non_object" })
        }
    }

    async fn execute_jsonrpc_request(
        &self,
        configured_uri: &str,
        method: &str,
        params: Option<&Value>,
        id: Option<&Value>,
        notification: bool,
        headers: Option<&Value>,
        output: Option<&str>,
        error_policy: Option<&JsonRpcErrorPolicy>,
        context: &Value,
    ) -> Result<TaskExecutionResult, DynError> {
        let resolved_uri = self.resolve_template_to_string(&configured_uri, context);
        let validated_uri = self.validate_resolved_uri(&configured_uri, &resolved_uri)?;

        let mut request = JsonMap::new();
        request.insert("jsonrpc".to_string(), Value::String("2.0".to_string()));
        request.insert("method".to_string(), Value::String(method.to_string()));
        if let Some(params) = params {
            request.insert(
                "params".to_string(),
                self.resolve_json_value(params, context),
            );
        }
        if !notification {
            request.insert("id".to_string(), id.cloned().unwrap_or_else(|| json!(1)));
        }

        let mut req_builder = self.http_client.post(validated_uri.clone());
        if let Some(headers) = headers {
            if let Value::Object(headers) = self.resolve_json_value(headers, context) {
                for (key, value) in headers {
                    req_builder = req_builder.header(key, self.stringify_json_value(&value));
                }
            }
        }

        info!(">>> Making JSON-RPC request to: {}", validated_uri);
        let resp = req_builder.json(&Value::Object(request)).send().await?;
        let status = resp.status();
        let body = resp.bytes().await?;
        if body.len() > MAX_HTTP_RESPONSE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "JSON-RPC response too large: more than {} bytes",
                    MAX_HTTP_RESPONSE_BYTES
                ),
            )
            .into());
        }

        if notification {
            return Ok(TaskExecutionResult {
                status_code: if status.is_success() { "C" } else { "F" },
                task_output: json!({ "status": status.as_u16() }),
                next_task: None,
                context_data: None,
            });
        }

        let response = if body.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice::<Value>(&body).unwrap_or_else(|_| {
                json!({
                    "error": status.as_u16(),
                    "body": String::from_utf8_lossy(&body).to_string()
                })
            })
        };

        let has_jsonrpc_error = response.get("error").is_some();
        let throw_on_error = error_policy.and_then(|policy| policy.throw).unwrap_or(true);
        if has_jsonrpc_error && throw_on_error {
            let mut output = json!({
                "type": error_policy
                    .and_then(|policy| policy.error_type.clone())
                    .unwrap_or_else(|| "https://agentic-workflow.org/errors/jsonrpc-error".to_string()),
                "status": 400,
                "title": "JSON-RPC error",
                "detail": "JSON-RPC response contained an error"
            });
            if error_policy
                .and_then(|policy| policy.include_response)
                .unwrap_or(true)
            {
                output["response"] = response;
            }
            return Ok(TaskExecutionResult {
                status_code: "F",
                task_output: output,
                next_task: None,
                context_data: None,
            });
        }

        let task_output = match output.unwrap_or("result") {
            "raw" | "response" => response,
            "result" => response
                .get("result")
                .cloned()
                .unwrap_or_else(|| response.clone()),
            _ => response,
        };

        Ok(TaskExecutionResult {
            status_code: if status.is_success() { "C" } else { "F" },
            task_output,
            next_task: None,
            context_data: None,
        })
    }

    async fn fetch_external_json(
        &self,
        resource: &workflow_core::models::resource::ExternalResourceDefinition,
        context: &Value,
    ) -> Result<Value, DynError> {
        let configured_uri = self.endpoint_to_uri(&resource.endpoint);
        let resolved_uri = self.resolve_template_to_string(&configured_uri, context);
        let validated_uri = self.validate_resolved_uri(&configured_uri, &resolved_uri)?;

        let resp = self.http_client.get(validated_uri.clone()).send().await?;
        let status = resp.status();
        let body = resp.bytes().await?;
        if body.len() > MAX_HTTP_RESPONSE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "external resource response too large: more than {} bytes",
                    MAX_HTTP_RESPONSE_BYTES
                ),
            )
            .into());
        }
        if !status.is_success() {
            return Err(io::Error::other(format!(
                "failed to fetch external resource {}: HTTP {}",
                validated_uri, status
            ))
            .into());
        }

        serde_json::from_slice::<Value>(&body)
            .or_else(|_| serde_yaml::from_slice::<Value>(&body))
            .map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("external resource is not valid JSON or YAML: {}", err),
                )
                .into()
            })
    }

    fn resolve_openrpc_server_uri(
        &self,
        document: &Value,
        server_selector: Option<&Value>,
    ) -> Result<String, DynError> {
        let selected_server = if let Some(selector) = server_selector {
            if let Some(url) = selector.as_str() {
                if url.starts_with("http://") || url.starts_with("https://") {
                    return Ok(url.to_string());
                }
                self.find_openrpc_server_by_name(document, url)
            } else if selector.get("url").is_some() || selector.get("endpoint").is_some() {
                Some(selector)
            } else if let Some(name) = selector.get("name").and_then(Value::as_str) {
                self.find_openrpc_server_by_name(document, name)
            } else {
                None
            }
        } else {
            document
                .get("servers")
                .and_then(Value::as_array)
                .and_then(|servers| servers.first())
        };

        let selected_server = selected_server.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "OpenRPC call requires with.server or at least one document servers[].url",
            )
        })?;

        self.openrpc_server_url(selected_server, server_selector)
    }

    fn openrpc_server_url(
        &self,
        server: &Value,
        server_selector: Option<&Value>,
    ) -> Result<String, DynError> {
        if let Some(endpoint) = server.get("endpoint") {
            let endpoint: workflow_core::models::resource::OneOfEndpointDefinitionOrUri =
                serde_json::from_value(endpoint.clone()).map_err(|err| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("invalid OpenRPC server endpoint: {}", err),
                    )
                })?;
            return Ok(self.endpoint_to_uri(&endpoint));
        }

        let mut url = server
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "OpenRPC server requires url or endpoint",
                )
            })?
            .to_string();

        let mut variables = HashMap::new();
        if let Some(defaults) = server.get("variables").and_then(Value::as_object) {
            for (name, definition) in defaults {
                if let Some(default) = definition.get("default").and_then(Value::as_str) {
                    variables.insert(name.clone(), default.to_string());
                }
            }
        }
        if let Some(selector) = server_selector {
            if let Some(overrides) = selector.get("variables").and_then(Value::as_object) {
                for (name, value) in overrides {
                    variables.insert(name.clone(), self.stringify_json_value(value));
                }
            }
        }

        for (name, value) in variables {
            url = url.replace(&format!("{{{}}}", name), &value);
        }

        if url.contains('{') || url.contains('}') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("OpenRPC server URL has unresolved variables: {}", url),
            )
            .into());
        }

        Ok(url)
    }

    fn find_openrpc_server_by_name<'a>(
        &self,
        document: &'a Value,
        name: &str,
    ) -> Option<&'a Value> {
        document
            .get("servers")?
            .as_array()?
            .iter()
            .find(|server| server.get("name").and_then(Value::as_str) == Some(name))
    }

    fn find_openrpc_method<'a>(
        &self,
        document: &'a Value,
        method_name: &str,
    ) -> Result<&'a Value, DynError> {
        document
            .get("methods")
            .and_then(Value::as_array)
            .and_then(|methods| {
                methods
                    .iter()
                    .find(|method| method.get("name").and_then(Value::as_str) == Some(method_name))
            })
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("OpenRPC method '{}' not found in document", method_name),
                )
                .into()
            })
    }

    fn validate_openrpc_params(
        &self,
        method_definition: &Value,
        method_name: &str,
        params: Option<&Value>,
    ) -> Result<(), DynError> {
        let Some(descriptors) = method_definition.get("params").and_then(Value::as_array) else {
            return Ok(());
        };

        for (index, descriptor) in descriptors.iter().enumerate() {
            let name = descriptor
                .get("name")
                .and_then(Value::as_str)
                .map(ToString::to_string)
                .unwrap_or_else(|| index.to_string());
            let value = match params {
                Some(Value::Object(map)) => map.get(&name),
                Some(Value::Array(values)) => values.get(index),
                Some(Value::Null) | None => None,
                Some(other) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "OpenRPC method '{}' params must be an object or array, got {}",
                            method_name, other
                        ),
                    )
                    .into());
                }
            };

            let required = descriptor
                .get("required")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if required && value.is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "OpenRPC method '{}' is missing required param '{}'",
                        method_name, name
                    ),
                )
                .into());
            }

            if let Some(value) = value {
                self.validate_openrpc_schema_type(method_name, &name, value, descriptor)?;
            }
        }

        Ok(())
    }

    fn validate_openrpc_schema_type(
        &self,
        method_name: &str,
        param_name: &str,
        value: &Value,
        descriptor: &Value,
    ) -> Result<(), DynError> {
        let Some(schema_type) = descriptor
            .get("schema")
            .and_then(|schema| schema.get("type"))
            .and_then(Value::as_str)
        else {
            return Ok(());
        };

        let type_matches = match schema_type {
            "string" => value.is_string(),
            "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
            "number" => value.is_number(),
            "boolean" => value.is_boolean(),
            "array" => value.is_array(),
            "object" => value.is_object(),
            _ => true,
        };

        if type_matches {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "OpenRPC method '{}' param '{}' expected type '{}', got {}",
                    method_name, param_name, schema_type, value
                ),
            )
            .into())
        }
    }

    fn resolve_mcp_server(
        &self,
        args: &McpArguments,
        definition: &WorkflowDefinition,
    ) -> Result<McpServerDefinition, DynError> {
        if let Some(server) = &args.server {
            return Ok(server.clone());
        }

        let session_name = args.session.as_deref().or(args.server_ref.as_deref());
        if let Some(session_name) = session_name {
            let sessions = definition
                .use_
                .as_ref()
                .and_then(|use_| use_.mcp_sessions.as_ref())
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "MCP session '{}' referenced but use.mcpSessions is not defined",
                            session_name
                        ),
                    )
                })?;
            let session = sessions.get(session_name).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("MCP session '{}' not found", session_name),
                )
            })?;

            if let Some(server_name) = session.server.as_str() {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!(
                        "MCP session '{}' references server '{}' by name, but named server resolution is not implemented yet",
                        session_name, server_name
                    ),
                )
                .into());
            }

            return serde_json::from_value::<McpServerDefinition>(session.server.clone()).map_err(
                |err| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "invalid MCP session '{}' server definition: {}",
                            session_name, err
                        ),
                    )
                    .into()
                },
            );
        }

        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "MCP call requires with.server, with.session, or with.serverRef",
        )
        .into())
    }

    fn execute_assert_task(
        &self,
        assertion: &AssertDefinition,
        context: &Value,
    ) -> Result<TaskExecutionResult, DynError> {
        let value = assertion
            .value
            .as_ref()
            .map(|value| self.resolve_json_value(value, context))
            .unwrap_or_else(|| context.clone());

        let mut failures = Vec::new();
        self.evaluate_assert_field(
            "equals",
            &value,
            assertion.equals.as_ref(),
            context,
            &mut failures,
        );
        self.evaluate_assert_field(
            "contains",
            &value,
            assertion.contains.as_ref(),
            context,
            &mut failures,
        );
        if let Some(pattern) = &assertion.matches {
            let regex = Regex::new(pattern).map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid assert.matches regex '{}': {}", pattern, err),
                )
            })?;
            if !value.as_str().map(|s| regex.is_match(s)).unwrap_or(false) {
                failures.push(format!("value does not match pattern {}", pattern));
            }
        }
        if let Some(expected_exists) = assertion.exists {
            let exists = !value.is_null();
            if exists != expected_exists {
                failures.push(format!(
                    "exists expected {}, got {}",
                    expected_exists, exists
                ));
            }
        }
        if let Some(json_assertions) = &assertion.json {
            for (path, comparison) in json_assertions {
                let selected = self
                    .lookup_json_path(&value, path)
                    .cloned()
                    .unwrap_or(Value::Null);
                if let Err(err) = self.evaluate_assert_comparison(&selected, comparison, context) {
                    failures.push(format!("{}: {}", path, err));
                }
            }
        }

        if failures.is_empty() {
            Ok(TaskExecutionResult {
                status_code: "C",
                task_output: json!({ "passed": true, "value": value }),
                next_task: None,
                context_data: None,
            })
        } else {
            Ok(TaskExecutionResult {
                status_code: "F",
                task_output: json!({
                    "type": "https://agentic-workflow.org/errors/assertion-failed",
                    "status": 400,
                    "title": "Assertion failed",
                    "detail": failures.join("; "),
                    "data": {
                        "failures": failures,
                        "actual": value
                    }
                }),
                next_task: None,
                context_data: None,
            })
        }
    }

    fn evaluate_assert_field(
        &self,
        operator: &str,
        actual: &Value,
        expected: Option<&Value>,
        context: &Value,
        failures: &mut Vec<String>,
    ) {
        if let Some(expected) = expected {
            let expected = self.resolve_json_value(expected, context);
            let passed = match operator {
                "equals" => actual == &expected,
                "contains" => self.value_contains(actual, &expected),
                _ => true,
            };
            if !passed {
                failures.push(format!(
                    "{} expected {}, got {}",
                    operator, expected, actual
                ));
            }
        }
    }

    fn evaluate_assert_comparison(
        &self,
        actual: &Value,
        comparison: &AssertComparison,
        context: &Value,
    ) -> Result<(), String> {
        match comparison {
            AssertComparison::Expression(expression) => {
                if self
                    .evaluate_condition(expression, actual)
                    .map_err(|err| err.to_string())?
                {
                    Ok(())
                } else {
                    Err(format!("expression evaluated to false: {}", expression))
                }
            }
            AssertComparison::Object(comparison) => {
                self.evaluate_assert_comparison_object(actual, comparison, context)
            }
        }
    }

    fn evaluate_assert_comparison_object(
        &self,
        actual: &Value,
        comparison: &AssertComparisonObject,
        context: &Value,
    ) -> Result<(), String> {
        if let Some(expected) = &comparison.equals {
            let expected = self.resolve_json_value(expected, context);
            if actual != &expected {
                return Err(format!("equals expected {}, got {}", expected, actual));
            }
        }
        if let Some(expected) = &comparison.contains {
            let expected = self.resolve_json_value(expected, context);
            if !self.value_contains(actual, &expected) {
                return Err(format!("contains expected {}, got {}", expected, actual));
            }
        }
        if let Some(pattern) = &comparison.matches {
            let regex = Regex::new(pattern).map_err(|err| err.to_string())?;
            if !actual.as_str().map(|s| regex.is_match(s)).unwrap_or(false) {
                return Err(format!("matches expected {}, got {}", pattern, actual));
            }
        }
        if let Some(expected_exists) = comparison.exists {
            let exists = !actual.is_null();
            if exists != expected_exists {
                return Err(format!(
                    "exists expected {}, got {}",
                    expected_exists, exists
                ));
            }
        }
        if let Some(has_length) = &comparison.has_length {
            let len = self.value_length(actual).ok_or_else(|| {
                format!(
                    "hasLength requires string, array, or object, got {}",
                    actual
                )
            })?;
            match has_length {
                HasLengthComparison::Exact(expected) => {
                    if len != *expected {
                        return Err(format!("hasLength expected {}, got {}", expected, len));
                    }
                }
                HasLengthComparison::Range(range) => {
                    if let Some(gt) = range.gt {
                        if len <= gt {
                            return Err(format!("hasLength expected > {}, got {}", gt, len));
                        }
                    }
                    if let Some(gte) = range.gte {
                        if len < gte {
                            return Err(format!("hasLength expected >= {}, got {}", gte, len));
                        }
                    }
                    if let Some(lt) = range.lt {
                        if len >= lt {
                            return Err(format!("hasLength expected < {}, got {}", lt, len));
                        }
                    }
                    if let Some(lte) = range.lte {
                        if len > lte {
                            return Err(format!("hasLength expected <= {}, got {}", lte, len));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn endpoint_to_uri(
        &self,
        endpoint: &workflow_core::models::resource::OneOfEndpointDefinitionOrUri,
    ) -> String {
        match endpoint {
            workflow_core::models::resource::OneOfEndpointDefinitionOrUri::Uri(uri) => uri.clone(),
            workflow_core::models::resource::OneOfEndpointDefinitionOrUri::Endpoint(endpoint) => {
                endpoint.uri.clone()
            }
        }
    }

    fn value_contains(&self, actual: &Value, expected: &Value) -> bool {
        match (actual, expected) {
            (Value::String(actual), Value::String(expected)) => actual.contains(expected),
            (Value::Array(values), expected) => values.iter().any(|value| value == expected),
            (Value::Object(map), Value::String(key)) => map.contains_key(key),
            (Value::Object(map), Value::Object(expected)) => expected
                .iter()
                .all(|(key, value)| map.get(key).map(|actual| actual == value).unwrap_or(false)),
            _ => false,
        }
    }

    fn value_length(&self, value: &Value) -> Option<u64> {
        match value {
            Value::String(value) => Some(value.chars().count() as u64),
            Value::Array(values) => Some(values.len() as u64),
            Value::Object(values) => Some(values.len() as u64),
            _ => None,
        }
    }

    async fn finish_task(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        claimed: &ClaimedTask,
        result: TaskExecutionResult,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            UPDATE task_info_t
            SET status_code = $1, locked = 'N', completed_ts = CURRENT_TIMESTAMP, task_output = $2
            WHERE host_id = $3 AND task_id = $4
            "#,
        )
        .bind(result.status_code)
        .bind(&result.task_output)
        .bind(claimed.task.host_id)
        .bind(claimed.task.task_id)
        .execute(&mut **tx)
        .await?;

        if result.status_code == "C" {
            self.handle_transition(
                tx,
                &claimed.task,
                &claimed.definition,
                &claimed.raw_definition,
                claimed.context_data.clone(),
                result.task_output,
                result.next_task,
                result.context_data,
            )
            .await?;
        } else if result.status_code == "W" {
            if let Some(TaskDefinition::Ask(ask_task)) =
                self.find_task_definition(&claimed.definition, &claimed.task.wf_task_id)
            {
                self.ensure_ask_assignments(tx, claimed, &ask_task.ask)
                    .await?;
            }
            info!(
                ">>> Workflow task waiting for input: {} ({})",
                claimed.task.wf_task_id, claimed.task.wf_instance_id
            );
        } else {
            sqlx::query(
                "UPDATE process_info_t
                 SET status_code = 'F', completed_ts = CURRENT_TIMESTAMP,
                     error_info = $1
                 WHERE host_id = $2 AND process_id = $3",
            )
            .bind(result.task_output.to_string())
            .bind(claimed.task.host_id)
            .bind(claimed.task.process_id)
            .execute(&mut **tx)
            .await?;
        }

        Ok(())
    }

    async fn ensure_ask_assignments(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        claimed: &ClaimedTask,
        ask: &AskDefinition,
    ) -> Result<(), sqlx::Error> {
        let Some(assignment) = ask.assignment.as_ref() else {
            warn!(
                "Ask task {} is waiting without an assignment definition",
                claimed.task.wf_task_id
            );
            return Ok(());
        };

        let category_code = assignment
            .category_code
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(|value| self.resolve_template_to_string(value, &claimed.context_data))
            .unwrap_or_else(|| "(all)".to_string());
        let reason_code = assignment
            .reason_code
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(|value| self.resolve_template_to_string(value, &claimed.context_data))
            .unwrap_or_else(|| "ask".to_string());

        let mut assignment_targets = Vec::new();
        let mut seen = HashSet::new();

        if let Some(assignee_id) = assignment
            .assignee_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(|value| self.resolve_template_to_string(value, &claimed.context_data))
        {
            let key = format!("USER:{assignee_id}");
            if seen.insert(key) {
                assignment_targets.push(("USER", assignee_id));
            }
        }

        if let Some(role_id) = assignment
            .role_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(|value| self.resolve_template_to_string(value, &claimed.context_data))
        {
            let key = format!("ROLE:{role_id}");
            if seen.insert(key) {
                assignment_targets.push(("ROLE", role_id));
            }
        }

        if assignment_targets.is_empty() {
            warn!(
                "Ask task {} has an assignment definition but no resolved assignees",
                claimed.task.wf_task_id
            );
            return Ok(());
        }

        for (assignment_type, assignment_id) in assignment_targets {
            sqlx::query(
                r#"
                INSERT INTO task_asst_t (
                    host_id, task_asst_id, task_id, assigned_ts, assignee_id,
                    assignment_type, assignment_id, reason_code, category_code, update_user, update_ts,
                    aggregate_version, active
                )
                SELECT $1, $2, $3, CURRENT_TIMESTAMP, $4, $5, $6, $7, $8,
                       'light-workflow', CURRENT_TIMESTAMP, 1, TRUE
                WHERE NOT EXISTS (
                    SELECT 1
                    FROM task_asst_t
                    WHERE host_id = $1
                      AND task_id = $3
                      AND assignment_type = $5
                      AND assignment_id = $6
                      AND COALESCE(category_code, '') = COALESCE($8, '')
                      AND active = TRUE
                )
                "#,
            )
            .bind(claimed.task.host_id)
            .bind(Uuid::new_v4())
            .bind(claimed.task.task_id)
            .bind(&assignment_id)
            .bind(assignment_type)
            .bind(&assignment_id)
            .bind(&reason_code)
            .bind(&category_code)
            .execute(&mut **tx)
            .await?;
        }

        Ok(())
    }

    fn find_task_definition<'a>(
        &self,
        def: &'a WorkflowDefinition,
        name: &str,
    ) -> Option<&'a TaskDefinition> {
        for entry in &def.do_.entries {
            if let Some(task_def) = entry.get(name) {
                return Some(task_def);
            }
        }
        None
    }

    async fn handle_transition(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        task: &ActiveTask,
        definition: &WorkflowDefinition,
        raw_definition: &YamlValue,
        context_data: Value,
        task_output: Value,
        next_task_override: Option<String>,
        context_data_override: Option<Value>,
    ) -> Result<(), sqlx::Error> {
        let task_def = match self.find_task_definition(definition, &task.wf_task_id) {
            Some(task_def) => task_def,
            None => return Ok(()),
        };

        let base_context = context_data_override.unwrap_or(context_data);
        let new_context =
            self.apply_exports(raw_definition, &task.wf_task_id, base_context, &task_output);

        sqlx::query(
            "UPDATE process_info_t SET context_data = $1 WHERE host_id = $2 AND process_id = $3",
        )
        .bind(&new_context)
        .bind(task.host_id)
        .bind(task.process_id)
        .execute(&mut **tx)
        .await?;

        let next_task_name = if self.task_ends_workflow(raw_definition, &task.wf_task_id) {
            None
        } else {
            next_task_override
                .or_else(|| self.get_then_directive(task_def).clone())
                .or_else(|| self.get_next_sequential_task(definition, &task.wf_task_id))
        };

        if let Some(next_name) = next_task_name {
            if let Some(next_def) = self.find_task_definition(definition, &next_name) {
                let next_type = match Self::supported_task_type_name(next_def) {
                    Some(next_type) => next_type,
                    None => {
                        let message = format!(
                            "unsupported next task type for workflow {}: task '{}' transitions to unsupported task '{}'",
                            task.wf_instance_id, task.wf_task_id, next_name
                        );
                        error!("{}", message);
                        self.fail_process(tx, task, &message).await?;
                        return Ok(());
                    }
                };
                let new_task_id = Uuid::new_v4();
                let task_kind = Self::policy_task_kind(next_def)?;
                let security = parse_security_policy(raw_definition)
                    .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
                let resolved_policy =
                    resolve_policy(task_kind, security.as_ref(), &self.execution_profiles)
                        .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
                let definition_digest: Option<String> = sqlx::query_scalar(
                    "SELECT definition_digest FROM process_info_t
                     WHERE host_id = $1 AND process_id = $2",
                )
                .bind(task.host_id)
                .bind(task.process_id)
                .fetch_one(&mut **tx)
                .await?;
                let definition_digest = match definition_digest {
                    Some(definition_digest) => definition_digest,
                    None => {
                        let definition_value = serde_json::to_value(raw_definition)
                            .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
                        canonical_sha256(&definition_value)
                            .map_err(|error| sqlx::Error::Protocol(error.to_string()))?
                    }
                };
                WorkflowRepository::store_policy_snapshot(
                    tx,
                    task.host_id,
                    &definition_digest,
                    &resolved_policy,
                    "light-workflow",
                )
                .await?;
                WorkflowRepository::insert_task(
                    tx,
                    &NewTask {
                        host_id: task.host_id,
                        task_id: new_task_id,
                        task_type: next_type,
                        process_id: task.process_id,
                        wf_instance_id: task.wf_instance_id.clone(),
                        wf_task_id: &next_name,
                        task_input: &new_context,
                        placement: resolved_policy.placement,
                        policy_digest: &resolved_policy.policy_digest,
                    },
                )
                .await?;

                info!(
                    ">>> Transitioned to Next Task: {} ({}, {:?})",
                    next_name, next_type, resolved_policy.placement
                );
            } else {
                let message = format!(
                    "invalid next task reference '{}' from task {} in workflow {}",
                    next_name, task.wf_task_id, task.wf_instance_id
                );
                error!("{}", message);
                self.fail_process(tx, task, &message).await?;
            }
        } else {
            info!(">>> Workflow Completed: {}", task.wf_instance_id);
            sqlx::query(
                "UPDATE process_info_t SET status_code = 'C', completed_ts = CURRENT_TIMESTAMP, ex_trigger_ts = CURRENT_TIMESTAMP WHERE host_id = $1 AND process_id = $2",
            )
            .bind(task.host_id)
            .bind(task.process_id)
            .execute(&mut **tx)
            .await?;
        }

        Ok(())
    }

    async fn fail_process(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        task: &ActiveTask,
        reason: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE process_info_t SET status_code = 'F', completed_ts = CURRENT_TIMESTAMP, ex_trigger_ts = CURRENT_TIMESTAMP, context_data = jsonb_set(COALESCE(context_data, '{}'::jsonb), '{error}', to_jsonb($3::text), true) WHERE host_id = $1 AND process_id = $2",
        )
        .bind(task.host_id)
        .bind(task.process_id)
        .bind(reason)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    fn apply_exports(
        &self,
        raw_definition: &YamlValue,
        task_name: &str,
        context_data: Value,
        task_output: &Value,
    ) -> Value {
        let mut new_context = match context_data {
            Value::Object(map) => map,
            Value::Null => JsonMap::new(),
            other => {
                let mut map = JsonMap::new();
                map.insert("value".to_string(), other);
                map
            }
        };

        if let Some(export_map) = self.get_export_map(raw_definition, task_name) {
            for (key, path) in export_map {
                let exported_value = if path == ".output" {
                    Some(task_output.clone())
                } else if let Some(stripped) = path.strip_prefix(".output.") {
                    self.lookup_path(task_output, stripped).cloned()
                } else {
                    self.evaluate_expression_to_value(&path, &Value::Object(new_context.clone()))
                };

                if let Some(value) = exported_value {
                    new_context.insert(key, value);
                }
            }
        }

        Value::Object(new_context)
    }

    fn task_ends_workflow(&self, raw_definition: &YamlValue, task_name: &str) -> bool {
        self.find_raw_task_definition(raw_definition, task_name)
            .and_then(|task_node| task_node.get("end"))
            .and_then(|end| end.as_bool())
            .unwrap_or(false)
    }

    fn get_export_map(
        &self,
        raw_definition: &YamlValue,
        task_name: &str,
    ) -> Option<HashMap<String, String>> {
        let task_node = self.find_raw_task_definition(raw_definition, task_name)?;
        let export_node = task_node.get("export")?;
        let export_map = export_node.get("as").unwrap_or(export_node);
        let mapping = export_map.as_mapping()?;

        let mut result = HashMap::new();
        for (key, value) in mapping {
            let key = key.as_str()?.to_string();
            let value = value.as_str()?.to_string();
            result.insert(key, value);
        }

        Some(result)
    }

    fn find_raw_task_definition<'a>(
        &self,
        raw_definition: &'a YamlValue,
        task_name: &str,
    ) -> Option<&'a YamlValue> {
        let tasks = raw_definition.get("do")?.as_sequence()?;
        for task_entry in tasks {
            let mapping = task_entry.as_mapping()?;
            for (key, value) in mapping {
                if key.as_str()? == task_name {
                    return Some(value);
                }
            }
        }
        None
    }

    fn common_fields<'a>(&self, task_def: &'a TaskDefinition) -> &'a TaskDefinitionFields {
        match task_def {
            TaskDefinition::Ask(task) => &task.common,
            TaskDefinition::Assert(task) => &task.common,
            TaskDefinition::Call(call) => call.common(),
            TaskDefinition::Do(task) => &task.common,
            TaskDefinition::Emit(task) => &task.common,
            TaskDefinition::For(task) => &task.common,
            TaskDefinition::Fork(task) => &task.common,
            TaskDefinition::Listen(task) => &task.common,
            TaskDefinition::Raise(task) => &task.common,
            TaskDefinition::Run(task) => &task.common,
            TaskDefinition::Set(task) => &task.common,
            TaskDefinition::Switch(task) => &task.common,
            TaskDefinition::Try(task) => &task.common,
            TaskDefinition::Wait(task) => &task.common,
        }
    }

    fn get_then_directive<'a>(&self, task_def: &'a TaskDefinition) -> &'a Option<String> {
        &self.common_fields(task_def).then
    }

    fn get_next_sequential_task(&self, def: &WorkflowDefinition, current: &str) -> Option<String> {
        let mut found_current = false;
        for entry in &def.do_.entries {
            for key in entry.keys() {
                if found_current {
                    return Some(key.clone());
                }
                if key == current {
                    found_current = true;
                }
            }
        }
        None
    }

    async fn get_context_data(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        host_id: &Uuid,
        process_id: &Uuid,
    ) -> Result<(Value, Uuid, Option<Value>), sqlx::Error> {
        let row: (Option<Value>, Uuid, Option<Value>) = sqlx::query_as(
            "SELECT context_data, wf_def_id, definition_snapshot
             FROM process_info_t WHERE host_id = $1 AND process_id = $2",
        )
        .bind(host_id)
        .bind(process_id)
        .fetch_one(&mut **tx)
        .await?;
        let context_data = match row.0 {
            Some(Value::Null) | None => json!({}),
            Some(value) => value,
        };
        Ok((context_data, row.1, row.2))
    }

    async fn get_workflow_definition(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        host_id: &Uuid,
        wf_def_id: &Uuid,
    ) -> Result<String, sqlx::Error> {
        let row: (String,) = sqlx::query_as(
            "SELECT definition FROM wf_definition_t WHERE host_id = $1 AND wf_def_id = $2",
        )
        .bind(host_id)
        .bind(wf_def_id)
        .fetch_one(&mut **tx)
        .await?;
        Ok(row.0)
    }

    fn parse_configured_destination_uri(
        &self,
        configured_uri: &str,
    ) -> Result<reqwest::Url, DynError> {
        let scheme_separator = "://";
        let scheme_end = configured_uri.find(scheme_separator).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "invalid configured endpoint URI '{}': missing scheme",
                    configured_uri
                ),
            )
        })?;
        let scheme = &configured_uri[..scheme_end];
        let remainder = &configured_uri[scheme_end + scheme_separator.len()..];
        let authority_end = remainder
            .find(|c| matches!(c, '/' | '?' | '#'))
            .unwrap_or(remainder.len());
        let authority = &remainder[..authority_end];

        if authority.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "invalid configured endpoint URI '{}': missing host",
                    configured_uri
                ),
            )
            .into());
        }

        if authority.contains("${") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "invalid configured endpoint URI '{}': templating is not allowed in host or port",
                    configured_uri
                ),
            )
            .into());
        }

        let destination_uri = format!("{scheme}://{authority}/");
        reqwest::Url::parse(&destination_uri).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "invalid configured endpoint URI '{}': {}",
                    configured_uri, e
                ),
            )
            .into()
        })
    }

    fn validate_resolved_uri(
        &self,
        configured_uri: &str,
        resolved_uri: &str,
    ) -> Result<reqwest::Url, DynError> {
        let configured = self.parse_configured_destination_uri(configured_uri)?;
        let resolved = reqwest::Url::parse(resolved_uri).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid resolved endpoint URI '{}': {}", resolved_uri, e),
            )
        })?;

        let destination_unchanged = matches!(resolved.scheme(), "http" | "https")
            && configured.scheme() == resolved.scheme()
            && configured.host_str() == resolved.host_str()
            && configured.port_or_known_default() == resolved.port_or_known_default();

        if destination_unchanged {
            Ok(resolved)
        } else {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "resolved endpoint changed destination or used a disallowed scheme: {}",
                    resolved_uri
                ),
            )
            .into())
        }
    }

    fn resolve_json_value(&self, value: &Value, context: &Value) -> Value {
        match value {
            Value::String(template) => self.resolve_template_value(template, context),
            Value::Array(values) => Value::Array(
                values
                    .iter()
                    .map(|value| self.resolve_json_value(value, context))
                    .collect(),
            ),
            Value::Object(map) => {
                let mut resolved = JsonMap::new();
                for (key, value) in map {
                    resolved.insert(key.clone(), self.resolve_json_value(value, context));
                }
                Value::Object(resolved)
            }
            _ => value.clone(),
        }
    }

    fn resolve_template_to_string(&self, template: &str, context: &Value) -> String {
        self.stringify_json_value(&self.resolve_template_value(template, context))
    }

    fn resolve_template_value(&self, template: &str, context: &Value) -> Value {
        if let Some(captures) = TEMPLATE_REGEX.captures(template) {
            if captures.get(0).map(|m| m.as_str()) == Some(template) {
                let expression = captures
                    .get(1)
                    .or_else(|| captures.get(2))
                    .map(|m| m.as_str())
                    .unwrap_or_default();
                return self
                    .evaluate_expression_to_value(expression, context)
                    .unwrap_or_else(|| Value::String(template.to_string()));
            }
        }

        let replaced = TEMPLATE_REGEX.replace_all(template, |caps: &regex::Captures<'_>| {
            let expression = caps
                .get(1)
                .or_else(|| caps.get(2))
                .map(|m| m.as_str())
                .unwrap_or_default();
            self.evaluate_expression_to_value(expression, context)
                .map(|value| self.stringify_json_value(&value))
                .unwrap_or_else(|| {
                    caps.get(0)
                        .map(|m| m.as_str().to_string())
                        .unwrap_or_default()
                })
        });

        Value::String(replaced.into_owned())
    }

    fn evaluate_expression_to_value(&self, expression: &str, context: &Value) -> Option<Value> {
        let expression = expression.trim();

        if self.has_comparison_operator(expression) {
            return self
                .evaluate_condition(expression, context)
                .ok()
                .map(Value::Bool);
        }

        if let Some(path) = expression.strip_prefix('.') {
            return self.lookup_path(context, path).cloned();
        }

        if expression.eq_ignore_ascii_case("true") {
            return Some(Value::Bool(true));
        }
        if expression.eq_ignore_ascii_case("false") {
            return Some(Value::Bool(false));
        }
        if expression.eq_ignore_ascii_case("null") {
            return Some(Value::Null);
        }
        if let Some(unquoted) = Self::parse_quoted_string(expression) {
            return Some(Value::String(unquoted));
        }
        if let Ok(number) = expression.parse::<f64>() {
            return Number::from_f64(number).map(Value::Number);
        }

        Some(Value::String(expression.to_string()))
    }

    fn evaluate_condition(&self, expression: &str, context: &Value) -> Result<bool, DynError> {
        let expression = expression
            .trim()
            .trim_start_matches("${{")
            .trim_end_matches("}}")
            .trim();

        for operator in ["<=", ">=", "==", "!=", "<", ">"] {
            if let Some((lhs, rhs)) = expression.split_once(operator) {
                let lhs = self
                    .evaluate_expression_to_value(lhs.trim(), context)
                    .unwrap_or(Value::Null);
                let rhs = self
                    .evaluate_expression_to_value(rhs.trim(), context)
                    .unwrap_or(Value::Null);
                return self.compare_values(&lhs, &rhs, operator);
            }
        }

        let value = self
            .evaluate_expression_to_value(expression, context)
            .unwrap_or(Value::Bool(false));
        Ok(self.is_truthy(&value))
    }

    fn compare_values(&self, lhs: &Value, rhs: &Value, operator: &str) -> Result<bool, DynError> {
        if let (Some(lhs), Some(rhs)) = (lhs.as_f64(), rhs.as_f64()) {
            return Ok(match operator {
                "<" => lhs < rhs,
                "<=" => lhs <= rhs,
                ">" => lhs > rhs,
                ">=" => lhs >= rhs,
                "==" => lhs == rhs,
                "!=" => lhs != rhs,
                _ => false,
            });
        }

        if let (Some(lhs), Some(rhs)) = (lhs.as_str(), rhs.as_str()) {
            return Ok(match operator {
                "==" => lhs == rhs,
                "!=" => lhs != rhs,
                "<" => lhs < rhs,
                "<=" => lhs <= rhs,
                ">" => lhs > rhs,
                ">=" => lhs >= rhs,
                _ => false,
            });
        }

        if let (Some(lhs), Some(rhs)) = (lhs.as_bool(), rhs.as_bool()) {
            return Ok(match operator {
                "==" => lhs == rhs,
                "!=" => lhs != rhs,
                _ => false,
            });
        }

        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("cannot compare values {:?} and {:?}", lhs, rhs),
        )
        .into())
    }

    fn has_comparison_operator(&self, expression: &str) -> bool {
        ["<=", ">=", "==", "!=", "<", ">"]
            .iter()
            .any(|operator| expression.contains(operator))
    }

    fn lookup_path<'a>(&self, value: &'a Value, path: &str) -> Option<&'a Value> {
        let mut current = value;
        for segment in path.split('.') {
            if segment.is_empty() {
                continue;
            }
            current = current.get(segment)?;
        }
        Some(current)
    }

    fn lookup_json_path<'a>(&self, value: &'a Value, path: &str) -> Option<&'a Value> {
        let path = path.trim().strip_prefix('$').unwrap_or(path.trim());
        let path = path.strip_prefix('.').unwrap_or(path);
        if path.is_empty() {
            return Some(value);
        }

        let mut current = value;
        for segment in path.split('.') {
            if segment.is_empty() {
                continue;
            }
            let mut remainder = segment;
            if let Some(field_end) = remainder.find('[') {
                let field = &remainder[..field_end];
                if !field.is_empty() {
                    current = current.get(field)?;
                }
                remainder = &remainder[field_end..];
            } else {
                current = current.get(remainder)?;
                continue;
            }

            while let Some(index_start) = remainder.find('[') {
                let index_end = remainder[index_start + 1..].find(']')? + index_start + 1;
                let index: usize = remainder[index_start + 1..index_end].parse().ok()?;
                current = current.get(index)?;
                remainder = &remainder[index_end + 1..];
            }
        }
        Some(current)
    }

    fn stringify_json_value(&self, value: &Value) -> String {
        match value {
            Value::Null => String::new(),
            Value::String(s) => s.clone(),
            other => other.to_string(),
        }
    }

    fn is_truthy(&self, value: &Value) -> bool {
        match value {
            Value::Null => false,
            Value::Bool(value) => *value,
            Value::Number(number) => number.as_f64().unwrap_or_default() != 0.0,
            Value::String(value) => !value.is_empty(),
            Value::Array(values) => !values.is_empty(),
            Value::Object(values) => !values.is_empty(),
        }
    }

    fn parse_quoted_string(value: &str) -> Option<String> {
        let quoted = (value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\''));
        quoted.then(|| value[1..value.len() - 1].to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::postgres::PgPoolOptions;

    fn executor() -> TaskExecutor {
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://characterization:characterization@localhost/characterization")
            .expect("test URL is syntactically valid");
        TaskExecutor::new(pool)
    }

    fn claimed_from_yaml(yaml: &str, task_name: &str, task_type: &str) -> ClaimedTask {
        ClaimedTask {
            task: ActiveTask {
                host_id: Uuid::nil(),
                task_id: Uuid::nil(),
                task_type: task_type.to_string(),
                process_id: Uuid::nil(),
                wf_instance_id: "characterization".to_string(),
                wf_task_id: task_name.to_string(),
                status_code: "A".to_string(),
                result_code: None,
            },
            context_data: json!({"requestId": "REQ-1", "summary": "review"}),
            definition: serde_yaml::from_str(yaml).expect("fixture must be a workflow"),
            raw_definition: serde_yaml::from_str(yaml).expect("fixture must be YAML"),
        }
    }

    #[test]
    fn parse_agent_json_output_accepts_fenced_json() {
        let parsed = TaskExecutor::parse_agent_json_output(
            r#"
            Here is the result:
            ```json
            {"decision":"REVIEW","requiresHumanReview":true}
            ```
            "#,
        )
        .expect("fenced JSON should parse");

        assert_eq!(parsed["decision"], "REVIEW");
        assert_eq!(parsed["requiresHumanReview"], true);
    }

    #[test]
    fn validate_agent_output_rejects_missing_required_fields() {
        let schema = json!({
            "type": "object",
            "required": ["decision", "confidence"],
            "properties": {
                "decision": { "type": "string" },
                "confidence": { "type": "number" }
            }
        });
        let output = json!({ "decision": "APPROVE" });

        let result = TaskExecutor::validate_agent_output(output, Some(&schema));

        assert!(result.is_err());
    }

    #[test]
    fn validate_agent_output_adds_audit_after_schema_validation() {
        let schema = json!({
            "type": "object",
            "required": ["decision"],
            "properties": {
                "decision": { "type": "string", "enum": ["APPROVE", "REVIEW"] }
            }
        });
        let mut output =
            TaskExecutor::validate_agent_output(json!({ "decision": "APPROVE" }), Some(&schema))
                .expect("output should match schema");

        TaskExecutor::attach_agent_audit(&mut output, json!({ "attempts": 1 }));

        assert_eq!(output["decision"], "APPROVE");
        assert_eq!(output["_agentAudit"]["attempts"], 1);
    }

    #[test]
    fn host_claim_is_placement_scoped_and_characterizes_five_minute_reclaim() {
        assert_eq!(TASK_LOCK_TIMEOUT_MINUTES, 5);
        assert!(CLAIM_NEXT_HOST_TASK_SQL.contains("execution_placement = 'host'"));
        assert!(CLAIM_NEXT_HOST_TASK_SQL.contains("make_interval(mins => $1::int)"));
        assert!(CLAIM_NEXT_HOST_TASK_SQL.contains("FOR UPDATE SKIP LOCKED"));
        assert!(!CLAIM_NEXT_HOST_TASK_SQL.contains("task_type IN ('run'"));
    }

    #[tokio::test]
    async fn ask_task_waits_and_completed_answer_is_forwarded_once() {
        let executor = executor();
        let mut claimed = claimed_from_yaml(
            include_str!("../examples/human-approval.yaml"),
            "requestApproval",
            "ask",
        );

        let waiting = executor
            .execute_task(&claimed)
            .await
            .expect("ask execution is local");
        assert_eq!(waiting.status_code, "W");
        assert_eq!(waiting.task_output["status"], "waiting_for_input");

        claimed.task.status_code = "C".to_string();
        claimed.task.result_code = Some(r#"{"decision":"APPROVED"}"#.to_string());
        let completed = executor.completed_ask_result(&claimed);
        assert_eq!(completed.status_code, "C");
        assert_eq!(completed.task_output["decision"], "APPROVED");
    }

    #[tokio::test]
    async fn completion_merges_exports_and_selects_the_next_task() {
        let executor = executor();
        let yaml = include_str!("../examples/simple-set-assert.yaml");
        let definition: WorkflowDefinition = serde_yaml::from_str(yaml).unwrap();
        let raw: YamlValue = serde_yaml::from_str(yaml).unwrap();
        let output = json!({"applicantId": "A-1", "status": "APPROVED"});

        let merged = executor.apply_exports(
            &raw,
            "initializeDecision",
            json!({"existing": true}),
            &output,
        );
        assert_eq!(merged["existing"], true);
        assert_eq!(merged["decision"], output);
        assert_eq!(
            executor.get_next_sequential_task(&definition, "initializeDecision"),
            Some("verifyDecision".to_string())
        );
    }

    #[tokio::test]
    async fn failed_assert_is_a_terminal_task_failure() {
        let executor = executor();
        let yaml = include_str!("../examples/simple-set-assert.yaml");
        let definition: WorkflowDefinition = serde_yaml::from_str(yaml).unwrap();
        let TaskDefinition::Assert(task) = executor
            .find_task_definition(&definition, "verifyDecision")
            .expect("fixture has assert task")
        else {
            panic!("verifyDecision must be assert");
        };

        let result = executor
            .execute_assert_task(&task.assert, &json!({"decision": {"status": "DENIED"}}))
            .expect("a false assertion is a normalized task result");
        assert_eq!(result.status_code, "F");
        assert_eq!(result.task_output["status"], 400);
        assert!(
            result.task_output["data"]["failures"]
                .as_array()
                .is_some_and(|failures| !failures.is_empty())
        );
    }
}
