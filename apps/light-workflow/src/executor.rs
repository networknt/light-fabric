use crate::configuration::WorkflowConfigManager;
use crate::repositories::{NewTask, TerminalAttempt, WorkflowRepository};
use a2a_core::{AuthorizedInvocation, Direction, sign_authorized_invocation};
use agent_delegation::{DelegationClaims, DelegationKind, DelegationSigner};
use chrono::Utc;
use execution_runner_protocol::canonical_sha256;
use light_rule::{ActionRegistry, MultiThreadRuleExecutor, RuleConfig, RuleEngine};
use model_provider::{
    AnthropicProvider, ChatMessage, ChatRequest, CompatibleProvider, GeminiProvider,
    OllamaProvider, OpenAiProvider, OpenRouterProvider, Provider,
};
use regex::Regex;
use serde_json::{Map as JsonMap, Number, Value, json};
use serde_yaml::Value as YamlValue;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Row, Transaction, postgres::PgListener};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info, warn};
use uuid::Uuid;
use workflow_core::models::duration::OneOfDurationOrIso8601Expression;
use workflow_core::models::task::{
    A2aArguments, AgentArguments, AskDefinition, AssertComparison, AssertComparisonObject,
    AssertDefinition, CallTaskDefinition, HasLengthComparison, JsonRpcArguments,
    JsonRpcErrorPolicy, McpArguments, McpServerDefinition, OpenRpcArguments, SetValue,
    TaskDefinition, TaskDefinitionFields,
};
use workflow_core::models::workflow::WorkflowDefinition;
use workflow_policy::{ExecutionProfile, TaskKind, parse_security_policy, resolve_policy};

type DynError = Box<dyn std::error::Error + Send + Sync>;
static TEMPLATE_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\$\{\{\s*([^}]*(?:}[^}]+)*)\s*\}\}|\$\{\s*([^}]*)\s*\}")
        .expect("valid template regex")
});
static OPENAPI_PATH_PLACEHOLDER_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\{([A-Za-z0-9_.-]+)\}").expect("valid OpenAPI path placeholder regex")
});
static LIGHTAPI_ENV_EXPRESSION_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\$\{env\.([A-Za-z0-9_.-]+)\}")
        .expect("valid LightAPI environment expression regex")
});
const DEFAULT_HOST_EXECUTOR_CONCURRENCY: usize = 8;
const DEFAULT_HOST_TASK_LEASE_MS: i32 = 30_000;
const MAX_HTTP_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_AGENT_OUTPUT_BYTES: usize = 128 * 1024;
const AGENT_PROMPT_VERSION: u32 = 1;

fn read_private_a2a_key(path: &Path) -> Result<Vec<u8>, String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect Workflow A2A context key: {error}"))?;
    #[cfg(unix)]
    let permissions_are_private = {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o037 == 0
    };
    #[cfg(not(unix))]
    let permissions_are_private = true;
    if metadata.file_type().is_symlink() || !metadata.is_file() || !permissions_are_private {
        return Err("Workflow A2A context key must be a private regular non-symlink file".into());
    }
    let key = std::fs::read(path)
        .map_err(|error| format!("cannot read Workflow A2A context key: {error}"))?;
    if key.len() < 32 {
        return Err("Workflow A2A context key must contain at least 32 bytes".into());
    }
    Ok(key)
}

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
    wf_def_id: Uuid,
    context_data: Value,
    definition: WorkflowDefinition,
    raw_definition: YamlValue,
    host_lease: Option<HostTaskLease>,
}

#[derive(Debug, Clone, Copy)]
struct HostTaskLease {
    owner: Uuid,
    fencing_token: i64,
}

#[derive(sqlx::FromRow)]
struct ClaimedHostTask {
    host_id: Uuid,
    task_id: Uuid,
    task_type: String,
    process_id: Uuid,
    wf_instance_id: String,
    wf_task_id: String,
    status_code: String,
    result_code: Option<String>,
    lease_owner: Uuid,
    lease_fencing_token: i64,
}

struct TaskExecutionResult {
    status_code: &'static str,
    task_output: Value,
    next_task: Option<String>,
    context_data: Option<Value>,
}

struct EffectClaim {
    idempotency_key: String,
    request_digest: String,
    replayed_result: Option<Value>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct A2aBindingProjection {
    binding_id: Uuid,
    publication_id: Uuid,
    policy_digest: String,
    gateway_uri: String,
    audience: String,
}

#[derive(sqlx::FromRow)]
struct RetryTaskState {
    attempt_no: i32,
    effect_state: String,
    downstream_idempotency_key: Option<String>,
    deadline_ts: Option<chrono::DateTime<Utc>>,
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
    value_engine: Arc<RuleEngine>,
    workflow_delegation_signer: Option<Arc<DelegationSigner>>,
    execution_profiles: BTreeMap<String, ExecutionProfile>,
    database_url: Option<String>,
    host_executor_concurrency: usize,
    environment: String,
    service_authorization: Option<String>,
    agent_provider_base_urls: BTreeMap<String, String>,
    a2a_authorization_key: Option<Arc<Vec<u8>>>,
    managed_configuration: bool,
}

impl TaskExecutor {
    pub async fn reconcile_agent_job(
        &self,
        host_id: Uuid,
        job_id: Uuid,
    ) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let row=sqlx::query("SELECT j.workflow_process_id,j.workflow_task_id,j.state,j.public_output,j.error,j.output_schema
            FROM agent_job_t j JOIN task_info_t t ON t.host_id=j.host_id AND t.task_id=j.workflow_task_id
            WHERE j.host_id=$1 AND j.job_id=$2 AND j.state IN('SUCCEEDED','FAILED','CANCELLED','UNKNOWN')
              AND t.status_code='W' FOR UPDATE OF j,t")
            .bind(host_id).bind(job_id).fetch_optional(&mut *tx).await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(false);
        };
        let process_id: Uuid = row.try_get("workflow_process_id")?;
        let task_id: Uuid = row.try_get("workflow_task_id")?;
        let state: String = row.try_get("state")?;
        let task=sqlx::query_as::<_,ActiveTask>("SELECT host_id,task_id,task_type,process_id,wf_instance_id,
            wf_task_id,status_code,result_code FROM task_info_t WHERE host_id=$1 AND task_id=$2 AND process_id=$3")
            .bind(host_id).bind(task_id).bind(process_id).fetch_one(&mut *tx).await?;
        let (context_data, wf_def_id, snapshot) = self
            .get_context_data(&mut tx, &host_id, &process_id)
            .await?;
        let (definition, raw_definition) = if let Some(snapshot) = snapshot {
            (
                serde_json::from_value(snapshot.clone())
                    .map_err(|e| sqlx::Error::Protocol(e.to_string()))?,
                serde_yaml::to_value(snapshot).map_err(|e| sqlx::Error::Protocol(e.to_string()))?,
            )
        } else {
            let dsl = self
                .get_workflow_definition(&mut tx, &host_id, &wf_def_id)
                .await?;
            (
                serde_yaml::from_str(&dsl).map_err(|e| sqlx::Error::Protocol(e.to_string()))?,
                serde_yaml::from_str(&dsl).map_err(|e| sqlx::Error::Protocol(e.to_string()))?,
            )
        };
        let output: Option<Value> = row.try_get("public_output")?;
        let result = if state == "SUCCEEDED" {
            let output = output.unwrap_or(Value::Null);
            let schema: Value = row.try_get("output_schema")?;
            match crate::agent_job::validate_public_output(&schema, &output) {
                Ok(()) => TaskExecutionResult {
                    status_code: "C",
                    task_output: output,
                    next_task: None,
                    context_data: None,
                },
                Err(e) => TaskExecutionResult {
                    status_code: "F",
                    task_output: json!({"agentJobId":job_id,"class":"INVALID_PUBLIC_OUTPUT","message":e.to_string()}),
                    next_task: None,
                    context_data: None,
                },
            }
        } else {
            TaskExecutionResult {
                status_code: "F",
                task_output: json!({"agentJobId":job_id,"state":state,"error":row.try_get::<Option<Value>,_>("error")?}),
                next_task: None,
                context_data: None,
            }
        };
        let claimed = ClaimedTask {
            task,
            wf_def_id,
            context_data,
            definition,
            raw_definition,
            host_lease: None,
        };
        self.finish_task(&mut tx, &claimed, result).await?;
        tx.commit().await?;
        Ok(true)
    }
    fn supported_task_type_name(task_def: &TaskDefinition) -> Option<&'static str> {
        match task_def {
            TaskDefinition::Ask(_) => Some("ask"),
            TaskDefinition::Assert(_) => Some("assert"),
            TaskDefinition::Call(_) => Some("call"),
            TaskDefinition::Fork(_) => Some("fork"),
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
            TaskDefinition::Fork(_) => Ok(TaskKind::Fork),
            TaskDefinition::Set(_) => Ok(TaskKind::Set),
            TaskDefinition::Switch(_) => Ok(TaskKind::Switch),
            TaskDefinition::Call(call) => match call {
                CallTaskDefinition::Agent(_) => Ok(TaskKind::CallAgent),
                CallTaskDefinition::A2a(_) => Ok(TaskKind::CallA2a),
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
        let rule_executor = Arc::new(MultiThreadRuleExecutor::new(
            RuleConfig::default(),
            engine.clone(),
        ));
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
            value_engine: engine,
            workflow_delegation_signer: None,
            execution_profiles: BTreeMap::new(),
            database_url: None,
            host_executor_concurrency: DEFAULT_HOST_EXECUTOR_CONCURRENCY,
            environment: "local".to_string(),
            service_authorization: None,
            agent_provider_base_urls: BTreeMap::new(),
            a2a_authorization_key: None,
            managed_configuration: false,
        }
    }

    pub fn with_runtime_configuration(
        mut self,
        database_url: String,
        host_executor_concurrency: usize,
        environment: String,
        service_authorization: String,
        delegation_secret: Option<String>,
        agent_provider_base_urls: BTreeMap<String, String>,
        a2a_authorization_context_key_file: PathBuf,
        managed_configuration: bool,
    ) -> Result<Self, String> {
        self.database_url = Some(database_url);
        self.host_executor_concurrency = host_executor_concurrency;
        self.environment = environment;
        self.service_authorization = Some(service_authorization);
        self.agent_provider_base_urls = agent_provider_base_urls;
        self.a2a_authorization_key = Some(Arc::new(read_private_a2a_key(
            &a2a_authorization_context_key_file,
        )?));
        self.managed_configuration = managed_configuration;
        self.workflow_delegation_signer = delegation_secret
            .map(|secret| {
                DelegationSigner::new(secret.as_bytes(), "light-workflow")
                    .map(Arc::new)
                    .map_err(|error| format!("WORKFLOW_DELEGATION_SECRET is invalid: {error}"))
            })
            .transpose()?;
        Ok(self)
    }

    pub fn with_execution_profiles(
        mut self,
        execution_profiles: BTreeMap<String, ExecutionProfile>,
    ) -> Self {
        self.execution_profiles = execution_profiles;
        self
    }

    pub async fn run(&self, shutdown: tokio_util::sync::CancellationToken) -> Result<(), DynError> {
        let concurrency = self.host_executor_concurrency;
        info!(concurrency, "Starting TaskExecutor workers");
        futures_util::future::try_join_all(
            (0..concurrency).map(|_| self.run_worker(Uuid::now_v7(), shutdown.clone())),
        )
        .await?;
        Ok(())
    }

    pub async fn run_dynamic(
        self: Arc<Self>,
        runtime_config: Arc<WorkflowConfigManager>,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<(), DynError> {
        let mut updates = runtime_config.subscribe();
        let mut workers = HashMap::<Uuid, tokio_util::sync::CancellationToken>::new();
        let mut joins = tokio::task::JoinSet::<(Uuid, Result<(), DynError>)>::new();

        loop {
            let desired = runtime_config.load().config.host_executor_concurrency;
            let active = workers
                .values()
                .filter(|token| !token.is_cancelled())
                .count();
            if active < desired {
                for _ in active..desired {
                    let worker_id = Uuid::now_v7();
                    let worker_shutdown = shutdown.child_token();
                    workers.insert(worker_id, worker_shutdown.clone());
                    let executor = Arc::clone(&self);
                    joins.spawn(async move {
                        let result = executor.run_worker(worker_id, worker_shutdown).await;
                        (worker_id, result)
                    });
                }
                info!(desired, "TaskExecutor worker capacity activated");
            } else if active > desired {
                for token in workers
                    .values()
                    .filter(|token| !token.is_cancelled())
                    .take(active - desired)
                {
                    token.cancel();
                }
                info!(desired, "TaskExecutor worker capacity draining");
            }

            tokio::select! {
                _ = shutdown.cancelled() => {
                    for token in workers.values() {
                        token.cancel();
                    }
                    while let Some(joined) = joins.join_next().await {
                        let (_, result) = joined.map_err(|error| -> DynError { Box::new(error) })?;
                        result?;
                    }
                    return Ok(());
                }
                changed = updates.changed() => {
                    if changed.is_err() {
                        return Err(io::Error::new(io::ErrorKind::BrokenPipe, "workflow runtime configuration channel closed").into());
                    }
                }
                joined = joins.join_next(), if !workers.is_empty() => {
                    let (worker_id, result) = joined
                        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "workflow executor worker set closed"))?
                        .map_err(|error| -> DynError { Box::new(error) })?;
                    let expected_stop = workers
                        .remove(&worker_id)
                        .is_some_and(|token| token.is_cancelled());
                    if !expected_stop {
                        result?;
                        return Err(io::Error::other(format!("TaskExecutor worker {worker_id} exited unexpectedly")).into());
                    }
                    result?;
                }
            }
        }
    }

    async fn run_worker(
        &self,
        worker_id: Uuid,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Result<(), DynError> {
        // LISTEN connections are intentionally kept outside the query pool. A
        // worker retains this connection for its lifetime and must still be
        // able to acquire a pooled connection to claim and commit work.
        let database_url = self.database_url.as_deref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "executor database URL is not configured",
            )
        })?;
        let mut listener = PgListener::connect(database_url).await?;
        listener.listen("workflow_task_ready_v1").await?;
        loop {
            if shutdown.is_cancelled() {
                return Ok(());
            }
            match self.process_next_task(worker_id).await {
                Ok(true) => {}
                Ok(false) => {
                    if let Err(error) = self.expire_interactive_deadlines().await {
                        error!(
                            worker_id = %worker_id,
                            "Error expiring interactive workflow deadlines: {error}"
                        );
                        tokio::select! { _ = shutdown.cancelled() => return Ok(()), _ = sleep(Duration::from_millis(250)) => {} }
                        continue;
                    }
                    tokio::select! { _ = shutdown.cancelled() => return Ok(()), _ = tokio::time::timeout(Duration::from_millis(500), listener.recv()) => {} }
                }
                Err(e) => {
                    error!(worker_id = %worker_id, "Error in TaskExecutor: {}", e);
                    tokio::select! { _ = shutdown.cancelled() => return Ok(()), _ = sleep(Duration::from_millis(250)) => {} }
                }
            }
        }
    }

    async fn expire_interactive_deadlines(&self) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let expired: Vec<(Uuid, Uuid)> = sqlx::query_as(
            "UPDATE workflow_invocation_t SET state='FAILED',terminal_ts=CURRENT_TIMESTAMP,
                    user_authorization=NULL,user_authorization_exp=NULL,
                    updated_ts=CURRENT_TIMESTAMP,state_version=state_version+1,
                    normalized_error=jsonb_build_object(
                      'code','WORKFLOW_TIMEOUT','message','workflow deadline expired',
                      'retryable',false)
              WHERE execution_class='interactive' AND deadline_ts<=CURRENT_TIMESTAMP
                AND state IN ('ACCEPTED','RUNNING','WAITING')
              RETURNING host_id,process_id",
        )
        .fetch_all(&mut *tx)
        .await?;
        for (host_id, process_id) in expired {
            sqlx::query(
                "UPDATE process_info_t SET status_code='F',custom_status_code='WORKFLOW_TIMEOUT',
                        completed_ts=CURRENT_TIMESTAMP
                  WHERE host_id=$1 AND process_id=$2 AND status_code IN ('A','W')",
            )
            .bind(host_id)
            .bind(process_id)
            .execute(&mut *tx)
            .await?;
            sqlx::query(
                "UPDATE task_info_t SET status_code='F',result_code='WORKFLOW_TIMEOUT',
                        locked='N',lease_owner=NULL,lease_expires_ts=NULL,
                        completed_ts=CURRENT_TIMESTAMP
                  WHERE host_id=$1 AND process_id=$2 AND status_code IN ('A','W')",
            )
            .bind(host_id)
            .bind(process_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    async fn process_next_task(&self, worker_id: Uuid) -> Result<bool, DynError> {
        let claimed = match self.claim_next_task(worker_id).await? {
            Some(claimed) => claimed,
            None => return Ok(false),
        };
        let attempt_bytes =
            i64::try_from(serde_json::to_vec(&claimed.context_data)?.len()).unwrap_or(i64::MAX);
        let attempt_cost = self
            .find_task_definition(&claimed.definition, &claimed.task.wf_task_id)
            .and_then(|task| self.common_fields(task).metadata.as_ref())
            .and_then(|metadata| metadata.get("costUnits"))
            .and_then(Value::as_u64)
            .map(|value| i64::try_from(value).unwrap_or(i64::MAX))
            .unwrap_or(0);
        let attempt_budget = if let Some(lease) = claimed.host_lease {
            let ledger: Option<(Uuid, i64)> = sqlx::query_as(
                "SELECT budget.ledger_id,budget.generation
                   FROM workflow_invocation_t invocation
                   JOIN workflow_invocation_budget_t budget
                     ON budget.host_id=invocation.host_id
                    AND budget.workflow_instance_id=invocation.workflow_instance_id
                  WHERE invocation.host_id=$1 AND invocation.process_id=$2",
            )
            .bind(claimed.task.host_id)
            .bind(claimed.task.process_id)
            .fetch_optional(&self.pool)
            .await?;
            if let Some((ledger_id, generation)) = ledger {
                let reservation_id = Uuid::now_v7();
                let reserved: bool = sqlx::query_scalar(
                    "SELECT workflow_reserve_budget_v1($1,$2,$3,$4,$5,1,0,$6,$7)",
                )
                .bind(claimed.task.host_id)
                .bind(ledger_id)
                .bind(reservation_id)
                .bind(generation)
                .bind(lease.fencing_token)
                .bind(attempt_bytes)
                .bind(attempt_cost)
                .fetch_one(&self.pool)
                .await?;
                if !reserved {
                    self.fail_invocation_budget(&claimed, lease).await?;
                    return Ok(true);
                }
                Some((
                    claimed.task.host_id,
                    reservation_id,
                    lease.fencing_token,
                    attempt_bytes,
                    attempt_cost,
                    ledger_id,
                    generation,
                ))
            } else {
                None
            }
        } else {
            None
        };

        let (heartbeat_stop, heartbeat_handle) = if let Some(lease) = claimed.host_lease {
            let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
            let pool = self.pool.clone();
            let host_id = claimed.task.host_id;
            let task_id = claimed.task.task_id;
            let handle = tokio::spawn(async move {
                Self::renew_host_task_lease(pool, host_id, task_id, lease, stop_rx).await
            });
            (Some(stop_tx), Some(handle))
        } else {
            (None, None)
        };

        info!(
            ">>> Executor processing task: {} ({})",
            claimed.task.wf_task_id, claimed.task.task_type
        );

        let mut result = if claimed.task.status_code == "C" && claimed.task.task_type == "ask" {
            self.completed_ask_result(&claimed)
        } else {
            let execution = async {
                if let Some(limit) = self.task_execution_timeout(&claimed).await? {
                    match tokio::time::timeout(limit, self.execute_task(&claimed)).await {
                        Ok(result) => result,
                        Err(_) => Ok(TaskExecutionResult {
                            status_code: "F",
                            task_output: json!({
                                "code":"WORKFLOW_TASK_TIMEOUT",
                                "message":"workflow task exceeded its task or workflow deadline"
                            }),
                            next_task: None,
                            context_data: None,
                        }),
                    }
                } else {
                    self.execute_task(&claimed).await
                }
            };
            match execution.await {
                Ok(result) => result,
                Err(e) => TaskExecutionResult {
                    status_code: "F",
                    task_output: json!({ "error": e.to_string() }),
                    next_task: None,
                    context_data: None,
                },
            }
        };

        if let Some((host_id, reservation_id, fencing_token, bytes, cost, ledger_id, generation)) =
            attempt_budget
        {
            let output_bytes =
                i64::try_from(serde_json::to_vec(&result.task_output)?.len()).unwrap_or(i64::MAX);
            let output_reservation_id = Uuid::now_v7();
            let output_reserved: bool =
                sqlx::query_scalar("SELECT workflow_reserve_budget_v1($1,$2,$3,$4,$5,0,0,$6,0)")
                    .bind(host_id)
                    .bind(ledger_id)
                    .bind(output_reservation_id)
                    .bind(generation)
                    .bind(fencing_token)
                    .bind(output_bytes)
                    .fetch_one(&self.pool)
                    .await?;
            if output_reserved {
                let reconciled: bool =
                    sqlx::query_scalar("SELECT workflow_reconcile_budget_v1($1,$2,$3,$4,0)")
                        .bind(host_id)
                        .bind(output_reservation_id)
                        .bind(fencing_token)
                        .bind(output_bytes)
                        .fetch_one(&self.pool)
                        .await?;
                if !reconciled {
                    return Err(io::Error::other(
                        "WORKFLOW_BUDGET_EXHAUSTED: output reconciliation failed",
                    )
                    .into());
                }
            } else {
                let effect_state: Option<String> = sqlx::query_scalar(
                    "SELECT effect_state FROM workflow_invocation_t WHERE host_id=$1 AND process_id=$2",
                )
                .bind(host_id)
                .bind(claimed.task.process_id)
                .fetch_optional(&self.pool)
                .await?;
                let code = if effect_state.as_deref() == Some("confirmed") {
                    "WORKFLOW_BUDGET_EXHAUSTED_AFTER_EFFECT"
                } else {
                    "WORKFLOW_BUDGET_EXHAUSTED"
                };
                result = TaskExecutionResult {
                    status_code: "F",
                    task_output: json!({
                        "code":code,
                        "message":"workflow intermediate byte budget is exhausted",
                        "retryable":false
                    }),
                    next_task: None,
                    context_data: None,
                };
            }
            let reconciled: bool =
                sqlx::query_scalar("SELECT workflow_reconcile_budget_v1($1,$2,$3,$4,$5)")
                    .bind(host_id)
                    .bind(reservation_id)
                    .bind(fencing_token)
                    .bind(bytes)
                    .bind(cost)
                    .fetch_one(&self.pool)
                    .await?;
            if !reconciled {
                return Err(io::Error::other(
                    "WORKFLOW_BUDGET_EXHAUSTED: task-attempt reconciliation failed",
                )
                .into());
            }
        }

        if let Some(stop) = heartbeat_stop {
            let _ = stop.send(());
        }
        if let Some(handle) = heartbeat_handle {
            handle.await.map_err(|error| {
                io::Error::other(format!("host task lease heartbeat failed to join: {error}"))
            })??;
        }

        let mut tx = self.pool.begin().await?;
        self.finish_task(&mut tx, &claimed, result).await?;
        tx.commit().await?;

        Ok(true)
    }

    async fn fail_invocation_budget(
        &self,
        claimed: &ClaimedTask,
        lease: HostTaskLease,
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let updated = sqlx::query(
            "UPDATE task_info_t SET status_code='F',locked='N',completed_ts=CURRENT_TIMESTAMP,
                    result_code='WORKFLOW_BUDGET_EXHAUSTED',lease_owner=NULL,lease_expires_ts=NULL
              WHERE host_id=$1 AND task_id=$2 AND lease_owner=$3
                AND lease_fencing_token=$4 AND lease_expires_ts>CURRENT_TIMESTAMP",
        )
        .bind(claimed.task.host_id)
        .bind(claimed.task.task_id)
        .bind(lease.owner)
        .bind(lease.fencing_token)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(sqlx::Error::Protocol(
                "WORKFLOW_STALE_HOST_TASK_FENCE".to_string(),
            ));
        }
        sqlx::query(
            "UPDATE process_info_t SET status_code='F',completed_ts=CURRENT_TIMESTAMP,
                    custom_status_code='WORKFLOW_BUDGET_EXHAUSTED'
              WHERE host_id=$1 AND process_id=$2 AND status_code IN ('A','W')",
        )
        .bind(claimed.task.host_id)
        .bind(claimed.task.process_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE workflow_invocation_t SET state='FAILED',terminal_ts=CURRENT_TIMESTAMP,
                    user_authorization=NULL,user_authorization_exp=NULL,
                    updated_ts=CURRENT_TIMESTAMP,state_version=state_version+1,
                    normalized_error=jsonb_build_object(
                      'code',CASE WHEN effect_state='confirmed'
                        THEN 'WORKFLOW_BUDGET_EXHAUSTED_AFTER_EFFECT'
                        ELSE 'WORKFLOW_BUDGET_EXHAUSTED' END,
                      'message','workflow task-attempt budget is exhausted','retryable',false)
              WHERE host_id=$1 AND process_id=$2
                AND state NOT IN ('CANCELLED','COMPLETED','FAILED')",
        )
        .bind(claimed.task.host_id)
        .bind(claimed.task.process_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn renew_host_task_lease(
        pool: PgPool,
        host_id: Uuid,
        task_id: Uuid,
        lease: HostTaskLease,
        mut stop: tokio::sync::oneshot::Receiver<()>,
    ) -> Result<(), sqlx::Error> {
        let heartbeat = Duration::from_millis((DEFAULT_HOST_TASK_LEASE_MS as u64) / 3);
        loop {
            tokio::select! {
                _ = &mut stop => return Ok(()),
                _ = sleep(heartbeat) => {
                    let renewed = sqlx::query(
                        "UPDATE task_info_t SET
                            lease_expires_ts=LEAST(COALESCE(deadline_ts,'infinity'::timestamptz),
                                CURRENT_TIMESTAMP+make_interval(secs=>$1::double precision/1000.0)),
                            update_ts=CURRENT_TIMESTAMP
                          WHERE host_id=$2 AND task_id=$3 AND locked='Y'
                            AND lease_owner=$4 AND lease_fencing_token=$5
                            AND lease_expires_ts>CURRENT_TIMESTAMP",
                    )
                    .bind(DEFAULT_HOST_TASK_LEASE_MS)
                    .bind(host_id)
                    .bind(task_id)
                    .bind(lease.owner)
                    .bind(lease.fencing_token)
                    .execute(&pool)
                    .await?;
                    if renewed.rows_affected()!=1 {
                        return Err(sqlx::Error::Protocol("WORKFLOW_STALE_HOST_TASK_FENCE".into()));
                    }
                }
            }
        }
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
        if attempt.state == "UNKNOWN" {
            sqlx::query("UPDATE process_info_t SET status_code='W',custom_status_code='FIXED_ACTION_UNKNOWN',
                        error_info=$1 WHERE host_id=$2 AND process_id=$3")
                .bind(attempt.normalized_error.as_ref().map(Value::to_string))
                .bind(attempt.host_id).bind(attempt.process_id).execute(&mut **tx).await?;
            return Ok(true);
        }
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
        let provenance_digest = attempt
            .normalized_result
            .as_ref()
            .and_then(|value| value.get("evidence"))
            .and_then(|value| value.get("provenanceDigest"))
            .and_then(Value::as_str)
            .map(str::to_string);
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
        let _ = hold_eligible;
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
            wf_def_id,
            context_data,
            definition,
            raw_definition,
            host_lease: None,
        })
    }

    async fn claim_next_task(&self, worker_id: Uuid) -> Result<Option<ClaimedTask>, DynError> {
        let mut tx = self.pool.begin().await?;

        let task_res = sqlx::query_as::<_, ClaimedHostTask>(
            "SELECT host_id,task_id,task_type,process_id,wf_instance_id,wf_task_id,
                    status_code,result_code,lease_owner,lease_fencing_token
               FROM workflow_claim_host_task_v1($1,$2)",
        )
        .bind(worker_id)
        .bind(DEFAULT_HOST_TASK_LEASE_MS)
        .fetch_optional(&mut *tx)
        .await?;

        let claimed_task = match task_res {
            Some(task) => task,
            None => {
                tx.commit().await?;
                return Ok(None);
            }
        };
        let lease = HostTaskLease {
            owner: claimed_task.lease_owner,
            fencing_token: claimed_task.lease_fencing_token,
        };
        let task = ActiveTask {
            host_id: claimed_task.host_id,
            task_id: claimed_task.task_id,
            task_type: claimed_task.task_type,
            process_id: claimed_task.process_id,
            wf_instance_id: claimed_task.wf_instance_id,
            wf_task_id: claimed_task.wf_task_id,
            status_code: claimed_task.status_code,
            result_code: claimed_task.result_code,
        };
        sqlx::query(
            "UPDATE workflow_invocation_t SET state='RUNNING',updated_ts=CURRENT_TIMESTAMP,
                    state_version=state_version+1
              WHERE host_id=$1 AND process_id=$2 AND state='ACCEPTED'",
        )
        .bind(task.host_id)
        .bind(task.process_id)
        .execute(&mut *tx)
        .await?;

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
            wf_def_id,
            context_data,
            definition,
            raw_definition,
            host_lease: Some(lease),
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
            TaskDefinition::Ask(ask_task) => {
                let mut ask = serde_json::to_value(&ask_task.ask)?;
                if let (Some(ask), Some(human_task)) = (
                    ask.as_object_mut(),
                    ask_task
                        .common
                        .metadata
                        .as_ref()
                        .and_then(|metadata| metadata.get("humanTask"))
                        .and_then(Value::as_object),
                ) {
                    for field in ["action", "commentRequired"] {
                        if let Some(value) = human_task.get(field) {
                            ask.insert(field.to_string(), value.clone());
                        }
                    }
                }
                Ok(TaskExecutionResult {
                    status_code: "W",
                    task_output: json!({
                        "status": "waiting_for_input",
                        "ask": ask,
                        "message": "Task is waiting for human input"
                    }),
                    next_task: None,
                    context_data: None,
                })
            }
            TaskDefinition::Assert(assert_task) => {
                self.execute_assert_task(&assert_task.assert, &claimed.context_data)
            }
            TaskDefinition::Fork(_) => Ok(TaskExecutionResult {
                status_code: "C",
                task_output: json!({"status":"branches_scheduled"}),
                next_task: None,
                context_data: None,
            }),
            TaskDefinition::Call(CallTaskDefinition::Http(http_call)) => {
                let inline_uri = match &http_call.with.endpoint {
                    workflow_core::models::resource::OneOfEndpointDefinitionOrUri::Uri(uri) => {
                        uri.clone()
                    }
                    workflow_core::models::resource::OneOfEndpointDefinitionOrUri::Endpoint(
                        endpoint,
                    ) => endpoint.uri.clone(),
                };
                let endpoint_ref = http_call
                    .common
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("endpointRef"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty());
                let workflow_tool = http_call
                    .common
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("workflowTool"))
                    .and_then(Value::as_object);
                let logical_tool_uri = inline_uri.starts_with("lightapi://");
                let granted_uri: Option<String> = if logical_tool_uri || workflow_tool.is_some() {
                    let pin = workflow_tool.ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "logical LightAPI call requires metadata.workflowTool",
                        )
                    })?;
                    let capability_ref = pin
                        .get("capabilityRef")
                        .and_then(Value::as_str)
                        .filter(|value| !value.trim().is_empty())
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::PermissionDenied,
                                "workflow Tool capabilityRef is required",
                            )
                        })?;
                    let tool_id = pin
                        .get("toolId")
                        .and_then(Value::as_str)
                        .and_then(|value| Uuid::parse_str(value).ok())
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::PermissionDenied,
                                "workflow Tool toolId is required and must be a UUID",
                            )
                        })?;
                    let tool_version =
                        pin.get("version").and_then(Value::as_str).ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::PermissionDenied,
                                "workflow Tool version is required",
                            )
                        })?;
                    let lightapi_digest = pin
                        .get("lightapiDigest")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::PermissionDenied,
                                "workflow Tool digest is required",
                            )
                        })?;
                    if inline_uri != format!("lightapi://{capability_ref}") {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "logical URI and capabilityRef do not match",
                        )
                        .into());
                    }
                    let environment = self.environment.clone();
                    let resolved: Option<(Uuid, Uuid, Value, String)> = sqlx::query_as(
                        "SELECT g.grant_id,t.tool_id,t.lightapi_document,
                                (av.protocol || '://' || av.target_host) AS base_uri
                           FROM workflow_tool_grant_t g
                           JOIN tool_t t ON t.host_id=g.host_id AND t.tool_id=g.tool_id
                           JOIN api_endpoint_t e ON e.host_id=t.host_id AND e.endpoint_id=t.endpoint_id
                           JOIN api_version_t av ON av.host_id=e.host_id AND av.api_version_id=e.api_version_id
                           JOIN api_t a ON a.host_id=av.host_id AND a.api_id=av.api_id
                          WHERE g.host_id=$1 AND g.wf_def_id=$2 AND g.active
                            AND g.tool_id=$3 AND g.tool_version=$4 AND g.lightapi_digest=$5
                            AND $6=ANY(g.allowed_environments)
                            AND t.capability_ref=$7 AND t.version=g.tool_version AND t.lightapi_digest=g.lightapi_digest
                            AND t.lightapi_validation_status='VALID' AND t.active AND t.lifecycle_status='active'
                            AND e.active AND e.lifecycle_status='active' AND av.active AND a.active
                            AND upper(e.http_method)=upper($8) AND av.target_host IS NOT NULL
                            AND NOT EXISTS (SELECT 1 FROM api_endpoint_scope_t scope
                                             WHERE scope.host_id=e.host_id AND scope.endpoint_id=e.endpoint_id AND scope.active)
                            AND NOT EXISTS (SELECT 1 FROM api_endpoint_rule_t endpoint_rule
                                             WHERE endpoint_rule.host_id=e.host_id AND endpoint_rule.endpoint_id=e.endpoint_id AND endpoint_rule.active)
                            AND NOT EXISTS (SELECT 1 FROM role_permission_t permission
                                             WHERE permission.host_id=e.host_id AND permission.endpoint_id=e.endpoint_id AND permission.active)
                            AND NOT EXISTS (SELECT 1 FROM group_permission_t permission
                                             WHERE permission.host_id=e.host_id AND permission.endpoint_id=e.endpoint_id AND permission.active)
                            AND NOT EXISTS (SELECT 1 FROM user_permission_t permission
                                             WHERE permission.host_id=e.host_id AND permission.endpoint_id=e.endpoint_id AND permission.active)
                            AND NOT EXISTS (SELECT 1 FROM position_permission_t permission
                                             WHERE permission.host_id=e.host_id AND permission.endpoint_id=e.endpoint_id AND permission.active)
                            AND NOT EXISTS (SELECT 1 FROM attribute_permission_t permission
                                             WHERE permission.host_id=e.host_id AND permission.endpoint_id=e.endpoint_id AND permission.active)"
                    )
                    .bind(claimed.task.host_id)
                    .bind(claimed.wf_def_id)
                    .bind(tool_id)
                    .bind(tool_version)
                    .bind(lightapi_digest)
                    .bind(&environment)
                    .bind(capability_ref)
                    .bind(&http_call.with.method)
                    .fetch_optional(&self.pool).await?;
                    let (grant_id, tool_id, lightapi_document, base_uri) = resolved.ok_or_else(|| io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "workflow Tool grant is missing, inactive, stale, protected by an unsupported endpoint policy, or not allowed in this environment",
                    ))?;
                    let endpoint_uri = resolve_lightapi_http_endpoint(
                        &lightapi_document,
                        capability_ref,
                        &environment,
                        &http_call.with.method,
                        &base_uri,
                    )?;
                    info!(host_id=%claimed.task.host_id, workflow_definition_id=%claimed.wf_def_id,
                        %grant_id, %tool_id, capability_ref, lightapi_digest, environment,
                        "workflow Tool capability resolved");
                    Some(endpoint_uri)
                } else {
                    None
                };
                let registered_uri: Option<String> = if let Some(endpoint_ref) = endpoint_ref {
                    sqlx::query_scalar(
                        "SELECT target.endpoint_uri
                           FROM workflow_invocation_t invocation
                           JOIN workflow_endpoint_target_t target ON target.host_id=invocation.host_id
                          WHERE invocation.host_id=$1 AND invocation.process_id=$2
                            AND target.endpoint_ref=$3 AND target.active
                            AND $4=ANY(target.allowed_methods)",
                    )
                    .bind(claimed.task.host_id)
                    .bind(claimed.task.process_id)
                    .bind(endpoint_ref)
                    .bind(http_call.with.method.to_ascii_uppercase())
                    .fetch_optional(&self.pool)
                    .await?
                } else {
                    None
                };
                let invocation_authorization: Option<(Option<String>, String)> = sqlx::query_as(
                    "SELECT user_authorization,state FROM workflow_invocation_t
                      WHERE host_id=$1 AND process_id=$2",
                )
                .bind(claimed.task.host_id)
                .bind(claimed.task.process_id)
                .fetch_optional(&self.pool)
                .await?;
                let workflow_backed = invocation_authorization.is_some();
                if invocation_authorization.as_ref().is_some_and(|(_, state)| {
                    matches!(state.as_str(), "CANCELLED" | "COMPLETED" | "FAILED")
                }) {
                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "workflow invocation became terminal before HTTP task dispatch",
                    )
                    .into());
                }
                let user_authorization = invocation_authorization
                    .as_ref()
                    .and_then(|(authorization, _)| authorization.as_deref());
                if workflow_http_requires_registered_target(
                    workflow_backed,
                    granted_uri.is_some(),
                    registered_uri.is_some(),
                ) {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "workflow-backed direct HTTP call requires an active registered endpointRef and allowed method",
                    )
                    .into());
                }
                let configured_uri = granted_uri.or(registered_uri).unwrap_or(inline_uri);
                let configured_template = OPENAPI_PATH_PLACEHOLDER_REGEX
                    .replace_all(&configured_uri, |captures: &regex::Captures<'_>| {
                        format!("${{{{ {} }}}}", &captures[1])
                    })
                    .into_owned();
                let resolved_uri =
                    self.resolve_template_to_string(&configured_template, &claimed.context_data);
                let validated_uri =
                    self.validate_resolved_uri(&configured_template, &resolved_uri)?;

                let method = reqwest::Method::from_bytes(http_call.with.method.as_bytes())
                    .map_err(|err| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("invalid HTTP method '{}': {}", http_call.with.method, err),
                        )
                    })?;
                let read_only = matches!(method, reqwest::Method::GET | reqwest::Method::HEAD);
                let resolved_body = http_call
                    .with
                    .body
                    .as_ref()
                    .map(|body| self.resolve_json_value(body, &claimed.context_data));
                let resolved_query = self
                    .resolve_http_string_map(http_call.with.query.as_ref(), &claimed.context_data);
                let resolved_headers = self.resolve_http_string_map(
                    http_call.with.headers.as_ref(),
                    &claimed.context_data,
                );
                let workflow_authorization = if workflow_backed {
                    let scope_authorization = self.service_authorization.as_deref();
                    Some(workflow_http_authorization_headers(
                        user_authorization,
                        scope_authorization,
                    )?)
                } else {
                    None
                };
                let effect_claim = if read_only {
                    None
                } else {
                    let key_template =
                        http_call.common.idempotency_key.as_deref().ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "write-capable HTTP workflow task requires idempotencyKey",
                            )
                        })?;
                    let idempotency_key =
                        self.resolve_template_to_string(key_template, &claimed.context_data);
                    let request_digest = format!(
                        "sha256:{}",
                        canonical_sha256(&json!({
                            "method":method.as_str(),
                            "uri":validated_uri.as_str(),
                            "query":resolved_query,
                            "headers":resolved_headers,
                            "body":resolved_body.clone()
                        }))?
                    );
                    let claim = self
                        .claim_task_effect(claimed, idempotency_key, request_digest)
                        .await?;
                    if let Some(result) = claim.replayed_result.clone() {
                        return Ok(TaskExecutionResult {
                            status_code: "C",
                            task_output: result,
                            next_task: None,
                            context_data: None,
                        });
                    }
                    Some(claim)
                };
                let mut req_builder = self.http_client.request(method, validated_uri.clone());

                if let Some((user_authorization, scope_authorization)) = workflow_authorization {
                    req_builder = req_builder
                        .header(reqwest::header::AUTHORIZATION, user_authorization)
                        .header("X-Scope-Token", scope_authorization);
                }

                if !resolved_query.is_empty() {
                    req_builder = req_builder.query(&resolved_query);
                }
                for (name, value) in &resolved_headers {
                    let name = reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(
                        |error| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                format!("invalid HTTP header name '{name}': {error}"),
                            )
                        },
                    )?;
                    if is_protected_workflow_http_header(&name, workflow_backed) {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            format!("workflow HTTP call cannot override protected header '{name}'"),
                        )
                        .into());
                    }
                    let value = reqwest::header::HeaderValue::from_str(value).map_err(|error| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("invalid HTTP header value for '{name}': {error}"),
                        )
                    })?;
                    req_builder = req_builder.header(name, value);
                }

                if let Some(body) = &resolved_body {
                    req_builder = req_builder.json(body);
                }
                if let Some(claim) = &effect_claim {
                    req_builder = req_builder.header("Idempotency-Key", &claim.idempotency_key);
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

                if status.is_success()
                    && let Some(claim) = effect_claim
                {
                    self.confirm_task_effect(claimed, &claim, &task_output)
                        .await?;
                }
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
                self.execute_mcp_call(&mcp_call.with, &mcp_call.common, claimed)
                    .await
            }
            TaskDefinition::Call(CallTaskDefinition::A2a(a2a_call)) => {
                self.execute_a2a_call(&a2a_call.with, &a2a_call.common, claimed)
                    .await
            }
            TaskDefinition::Call(CallTaskDefinition::Agent(agent_call)) => {
                self.execute_agent_call(
                    &agent_call.with,
                    &claimed.context_data,
                    &claimed.raw_definition,
                    &claimed.task.host_id,
                    claimed.task.process_id,
                    claimed.task.task_id,
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
            None,
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
        common: &TaskDefinitionFields,
        claimed: &ClaimedTask,
    ) -> Result<TaskExecutionResult, DynError> {
        let definition = &claimed.definition;
        let context = &claimed.context_data;
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
        let mut configured_uri = self.endpoint_to_uri(endpoint);
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

        let (method, mut params) = if let Some(method) = &args.method {
            (
                method.clone(),
                args.parameters.clone().unwrap_or_else(|| json!({})),
            )
        } else if let Some(tool) = &args.tool {
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

        let tool_alias = args.tool.clone().or_else(|| {
            (method == "tools/call")
                .then(|| {
                    params
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .flatten()
        });

        let mut delegation_headers = None;
        let mut budget_reservation = None;
        if let Some(tool_alias) = tool_alias.as_deref() {
            #[allow(clippy::type_complexity)]
            let invocation: Option<(
                Uuid,
                String,
                String,
                Value,
                Uuid,
                String,
                String,
                String,
                i32,
                chrono::DateTime<Utc>,
                Uuid,
                i64,
                Uuid,
                String,
                Value,
            )> = sqlx::query_as(
                "SELECT i.workflow_instance_id,i.principal_subject,i.end_user_subject,
                        i.subject_claims,i.stable_tool_ref,i.policy_digest,
                        i.response_policy_digest,i.execution_class,i.permit_depth,i.deadline_ts,
                        budget.ledger_id,budget.generation,dependency.nested_tool_id,
                        dependency.contract_digest,
                        dependency.dispatch_target
                   FROM workflow_invocation_t i
                   JOIN workflow_invocation_budget_t budget
                     ON budget.host_id=i.host_id AND budget.workflow_instance_id=i.workflow_instance_id
                   JOIN workflow_tool_dependency_t dependency
                     ON dependency.host_id=i.host_id AND dependency.outer_binding_id=i.binding_id
                    AND dependency.authorization_tool_name=$3 AND dependency.active
                    AND dependency.lifecycle_status<>'revoked'
                  WHERE i.host_id=$1 AND i.process_id=$2",
            )
            .bind(claimed.task.host_id)
            .bind(claimed.task.process_id)
            .bind(tool_alias)
            .fetch_optional(&self.pool)
            .await?;
            let workflow_backed: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM workflow_invocation_t
                  WHERE host_id=$1 AND process_id=$2)",
            )
            .bind(claimed.task.host_id)
            .bind(claimed.task.process_id)
            .fetch_one(&self.pool)
            .await?;
            if workflow_backed && invocation.is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "workflow-backed MCP call is not present in the published dependency registry",
                )
                .into());
            }
            if let Some((
                invocation_id,
                principal_subject,
                end_user_subject,
                caller_claims,
                _outer_tool_ref,
                policy_digest,
                response_policy_digest,
                execution_class,
                permit_depth,
                deadline_ts,
                ledger_id,
                budget_generation,
                nested_tool_ref,
                nested_contract_digest,
                dispatch_target,
            )) = invocation
            {
                if dispatch_target
                    .get("contractDigest")
                    .and_then(Value::as_str)
                    .is_some_and(|digest| digest != nested_contract_digest)
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "pinned MCP dependency contract digest drifted before dispatch",
                    )
                    .into());
                }
                configured_uri = dispatch_target
                    .get("endpoint")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "pinned MCP dependency has no registered endpoint",
                        )
                    })?
                    .to_string();
                let dispatch_tool_name = dispatch_target
                    .get("toolName")
                    .or_else(|| dispatch_target.get("targetName"))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "pinned MCP dependency has no private version target name",
                        )
                    })?;
                if let Some(object) = params.as_object_mut() {
                    object.insert(
                        "name".to_string(),
                        Value::String(dispatch_tool_name.to_string()),
                    );
                }
                let signer = self.workflow_delegation_signer.as_ref().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "nested workflow MCP call requires WORKFLOW_DELEGATION_SECRET",
                    )
                })?;
                let lease = claimed.host_lease.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "nested workflow MCP call requires a fenced host-task lease",
                    )
                })?;
                let nested_reservation_id = Uuid::now_v7();
                let nested_request_bytes =
                    i64::try_from(serde_json::to_vec(&params)?.len()).unwrap_or(i64::MAX);
                let reserved: bool = sqlx::query_scalar(
                    "SELECT workflow_reserve_budget_v1($1,$2,$3,$4,$5,0,1,$6,0)",
                )
                .bind(claimed.task.host_id)
                .bind(ledger_id)
                .bind(nested_reservation_id)
                .bind(budget_generation)
                .bind(lease.fencing_token)
                .bind(nested_request_bytes)
                .fetch_one(&self.pool)
                .await?;
                if !reserved {
                    return Err(io::Error::other(
                        "WORKFLOW_BUDGET_EXHAUSTED: nested call budget is unavailable",
                    )
                    .into());
                }
                let now = Utc::now().timestamp();
                let token = signer.mint(DelegationClaims {
                    token_id: Uuid::now_v7(),
                    kind: DelegationKind::ToolCall,
                    issuer: String::new(),
                    audience: "light-gateway".to_string(),
                    caller_subject: end_user_subject.clone(),
                    caller_claims,
                    subject_id: end_user_subject,
                    subject_type: "USER".to_string(),
                    groups: None,
                    organizations: None,
                    agent_actor: principal_subject,
                    agent_def_id: None,
                    agent_policy_version: 0,
                    host_id: claimed.task.host_id,
                    environment: None,
                    session_id: invocation_id,
                    turn_id: claimed.task.process_id,
                    action_attempt_id: Some(claimed.task.task_id),
                    tool_ref: Some(nested_tool_ref),
                    tool_alias: Some(dispatch_tool_name.to_string()),
                    destination: Some("mcp".to_string()),
                    workflow_invocation_id: Some(invocation_id),
                    workflow_permit_depth: Some(u16::try_from(permit_depth).unwrap_or(u16::MAX)),
                    workflow_execution_class: Some(execution_class),
                    workflow_budget_ledger_id: Some(ledger_id),
                    workflow_budget_generation: Some(
                        u64::try_from(budget_generation).unwrap_or_default(),
                    ),
                    data_boundary_digest: response_policy_digest,
                    policy_digest,
                    replay_id: Uuid::now_v7(),
                    issued_at: now,
                    expires_at: deadline_ts.timestamp().min(now + 300),
                })?;
                delegation_headers = Some(json!({"authorization": format!("Bearer {token}")}));
                budget_reservation = Some((
                    claimed.task.host_id,
                    nested_reservation_id,
                    lease.fencing_token,
                    nested_request_bytes,
                ));
            }
        }

        let write_capable = common
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("readOnly"))
            .and_then(Value::as_bool)
            == Some(false);
        let effect_claim = if write_capable {
            let key_template = common.idempotency_key.as_deref().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "write-capable MCP workflow task requires idempotencyKey",
                )
            })?;
            let idempotency_key =
                self.resolve_template_to_string(key_template, &claimed.context_data);
            let request_digest = format!(
                "sha256:{}",
                canonical_sha256(&json!({
                    "endpoint":configured_uri,
                    "method":method,
                    "params":params
                }))?
            );
            let claim = self
                .claim_task_effect(claimed, idempotency_key, request_digest)
                .await?;
            if let Some(result) = claim.replayed_result.clone() {
                if let Some((host_id, reservation_id, fencing_token, bytes)) = budget_reservation {
                    let _: bool =
                        sqlx::query_scalar("SELECT workflow_reconcile_budget_v1($1,$2,$3,$4,0)")
                            .bind(host_id)
                            .bind(reservation_id)
                            .bind(fencing_token)
                            .bind(bytes)
                            .fetch_one(&self.pool)
                            .await?;
                }
                return Ok(TaskExecutionResult {
                    status_code: "C",
                    task_output: result,
                    next_task: None,
                    context_data: None,
                });
            }
            if let Some(object) = params.as_object_mut() {
                object.insert(
                    "_meta".to_string(),
                    json!({"idempotencyKey":claim.idempotency_key}),
                );
            }
            Some(claim)
        } else {
            None
        };

        let mut request_headers = args
            .transport
            .as_ref()
            .and_then(|transport| transport.http.as_ref())
            .and_then(|http| http.headers.as_ref())
            .map(|headers| {
                Value::Object(
                    headers
                        .iter()
                        .map(|(name, value)| (name.clone(), Value::String(value.clone())))
                        .collect(),
                )
            });
        if let Some(Value::Object(delegation)) = delegation_headers {
            let target = request_headers.get_or_insert_with(|| Value::Object(JsonMap::new()));
            if let Value::Object(headers) = target {
                headers.extend(delegation);
            }
        }

        let execution = self
            .execute_jsonrpc_request(
                &configured_uri,
                &method,
                Some(&params),
                None,
                false,
                request_headers.as_ref(),
                args.timeout.as_ref(),
                args.output.as_deref().or(Some("result")),
                None,
                context,
            )
            .await;
        if let Some((host_id, reservation_id, fencing_token, bytes)) = budget_reservation {
            let reconciled: bool =
                sqlx::query_scalar("SELECT workflow_reconcile_budget_v1($1,$2,$3,$4,0)")
                    .bind(host_id)
                    .bind(reservation_id)
                    .bind(fencing_token)
                    .bind(bytes)
                    .fetch_one(&self.pool)
                    .await?;
            if !reconciled {
                return Err(io::Error::other(
                    "WORKFLOW_BUDGET_EXHAUSTED: nested call reconciliation failed",
                )
                .into());
            }
        }
        let result = execution?;
        if result.status_code == "C"
            && let Some(claim) = effect_claim
        {
            self.confirm_task_effect(claimed, &claim, &result.task_output)
                .await?;
        }
        Ok(result)
    }

    async fn execute_agent_call(
        &self,
        args: &AgentArguments,
        context: &Value,
        raw_definition: &YamlValue,
        host_id: &Uuid,
        process_id: Uuid,
        task_id: Uuid,
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
        if args.mode == workflow_core::models::task::AgentCallMode::Service {
            let deadline: Option<chrono::DateTime<Utc>> = sqlx::query_scalar(
                "SELECT deadline_ts FROM task_info_t WHERE host_id=$1 AND task_id=$2",
            )
            .bind(host_id)
            .bind(task_id)
            .fetch_one(&self.pool)
            .await?;
            let deadline = deadline.unwrap_or_else(|| Utc::now() + chrono::Duration::minutes(10));
            let input_schema_digest = execution_runner_protocol::canonical_sha256(&task_input)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            let output_schema = output_schema.unwrap_or_else(|| json!({"type":"object"}));
            let (policy_digest,data_boundary_digest):(String,String)=sqlx::query_as(
                "SELECT p.policy_digest,p.data_boundary_digest FROM agent_definition_t d
                 JOIN agent_policy_snapshot_t p ON p.host_id=d.host_id AND p.policy_snapshot_id=d.policy_snapshot_id
                 WHERE d.host_id=$1 AND d.agent_def_id=$2 AND p.revoked_ts IS NULL",
            ).bind(host_id).bind(catalog.agent.agent_def_id).fetch_one(&self.pool).await?;
            let job_id = Uuid::now_v7();
            let inserted: Uuid = sqlx::query_scalar(
                "INSERT INTO agent_job_t(host_id,job_id,workflow_process_id,workflow_task_id,
                   agent_def_id,idempotency_key,input,input_schema_digest,output_schema,policy_digest,
                   data_boundary_digest,deadline_ts,token_budget,cost_budget_micros,delegation_depth,
                   maximum_delegation_depth,memory_mode,state)
                 VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,0,$15,'ISOLATED','PENDING')
                 ON CONFLICT(host_id,idempotency_key) DO UPDATE SET updated_ts=agent_job_t.updated_ts
                 RETURNING job_id",
            ).bind(host_id).bind(job_id).bind(process_id).bind(task_id)
             .bind(catalog.agent.agent_def_id).bind(format!("workflow:{process_id}:{task_id}"))
             .bind(task_input).bind(input_schema_digest).bind(output_schema).bind(policy_digest)
             .bind(data_boundary_digest).bind(deadline).bind(args.token_budget.unwrap_or(65_536) as i64)
             .bind(args.cost_budget_micros.unwrap_or(0) as i64)
             .bind(args.maximum_delegation_depth.unwrap_or(4) as i32).fetch_one(&self.pool).await?;
            return Ok(TaskExecutionResult {
                status_code: "W",
                task_output: json!({"agentJobId":inserted,"state":"PENDING"}),
                next_task: None,
                context_data: None,
            });
        }
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

    async fn execute_a2a_call(
        &self,
        args: &A2aArguments,
        common: &TaskDefinitionFields,
        claimed: &ClaimedTask,
    ) -> Result<TaskExecutionResult, DynError> {
        if args.agent_card.is_some() || args.server.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "WORKFLOW_A2A_RAW_DESTINATION_FORBIDDEN: use a stable agentRef",
            )
            .into());
        }
        let agent_ref = args
            .agent_ref
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "WORKFLOW_A2A_AGENT_REF_REQUIRED",
                )
            })?;
        match args.method.as_str() {
            "message/send" | "message/stream" | "tasks/get" | "tasks/cancel" => {}
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "WORKFLOW_A2A_METHOD_NOT_ALLOWED",
                )
                .into());
            }
        }

        let binding = sqlx::query_as::<_, A2aBindingProjection>(
            "SELECT binding_id,publication_id,policy_digest,gateway_uri,audience
               FROM workflow_a2a_binding_t
              WHERE host_id=$1 AND agent_ref=$2 AND active=TRUE",
        )
        .bind(claimed.task.host_id)
        .bind(agent_ref)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "WORKFLOW_A2A_BINDING_NOT_FOUND",
            )
        })?;
        if binding.audience != "light-a2a" && binding.audience != "light-agent" {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "WORKFLOW_A2A_AUDIENCE_DENIED",
            )
            .into());
        }
        let key = self.a2a_authorization_key.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "WORKFLOW_A2A_AUTHORIZATION_KEY_UNAVAILABLE",
            )
        })?;
        let params = args
            .parameters
            .as_ref()
            .map(|value| self.resolve_json_value(value, &claimed.context_data))
            .unwrap_or_else(|| json!({}));
        let request_id = claimed.task.task_id.to_string();
        let body = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": args.method,
            "params": params,
        }))?;
        let request_digest = format!("sha256:{}", hex::encode(Sha256::digest(&body)));
        let idempotency_key = common
            .idempotency_key
            .as_deref()
            .map(|value| self.resolve_template_to_string(value, &claimed.context_data))
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| {
                format!(
                    "workflow:{}:{}:{}",
                    claimed.task.process_id, claimed.task.task_id, args.method
                )
            });
        if idempotency_key.len() > 255 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "WORKFLOW_A2A_IDEMPOTENCY_KEY_TOO_LONG",
            )
            .into());
        }
        let now = Utc::now();
        let invocation = AuthorizedInvocation {
            host_id: claimed.task.host_id,
            audience: binding.audience.clone(),
            principal_subject: format!("workflow:{}", claimed.task.process_id),
            caller_agent_ref: format!("workflow:{}", claimed.wf_def_id),
            target_agent_ref: agent_ref.to_string(),
            binding_id: binding.binding_id,
            policy_digest: binding.policy_digest,
            publication_id: binding.publication_id,
            direction: if binding.audience == "light-a2a" {
                Direction::Outbound
            } else {
                Direction::Inbound
            },
            idempotency_key,
            request_digest,
            issued_at: now,
            expires_at: now + chrono::Duration::minutes(5),
        };
        let (encoded_context, encoded_signature) =
            sign_authorized_invocation(&invocation, &body, key.as_slice()).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!("WORKFLOW_A2A_SIGNING_FAILED: {error}"),
                )
            })?;
        let mut endpoint = url::Url::parse(&binding.gateway_uri).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("WORKFLOW_A2A_PROJECTED_ENDPOINT_INVALID: {error}"),
            )
        })?;
        endpoint
            .path_segments_mut()
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "WORKFLOW_A2A_PROJECTED_ENDPOINT_CANNOT_BE_BASE",
                )
            })?
            .pop_if_empty()
            .push(agent_ref);
        let response = self
            .http_client
            .post(endpoint)
            .header("content-type", "application/json")
            .header("x-light-a2a-context", encoded_context)
            .header("x-light-a2a-signature", encoded_signature)
            .body(body)
            .send()
            .await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        if bytes.len() > MAX_HTTP_RESPONSE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "WORKFLOW_A2A_RESPONSE_TOO_LARGE",
            )
            .into());
        }
        let rpc: Value = serde_json::from_slice(&bytes).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("WORKFLOW_A2A_RESPONSE_INVALID: {error}"),
            )
        })?;
        let error = rpc.get("error").cloned();
        let succeeded = status.is_success() && error.is_none();
        Ok(TaskExecutionResult {
            status_code: if succeeded { "C" } else { "F" },
            task_output: if succeeded {
                rpc.get("result").cloned().unwrap_or(Value::Null)
            } else {
                json!({
                    "error": "a2a_call_failed",
                    "httpStatus": status.as_u16(),
                    "rpcError": error,
                })
            },
            next_task: None,
            context_data: None,
        })
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
        let api_key = self.resolve_agent_api_key(agent)?;
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
                        "compatible agent provider requires workflow.agentProviders.compatible.baseUrl",
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

    fn resolve_agent_api_key(
        &self,
        agent: &AgentDefinitionRecord,
    ) -> Result<Option<String>, DynError> {
        if let Some(api_key_ref) = agent
            .api_key_ref
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            if let Some(value) = api_key_ref.strip_prefix("literal:") {
                if self.managed_configuration {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "managed workflow agent api_key_ref must not use literal: credentials",
                    )
                    .into());
                }
                return Ok(Some(value.to_string()));
            }

            let env_name = api_key_ref.strip_prefix("env:").unwrap_or(api_key_ref);
            match env::var(env_name) {
                Ok(value) if !value.trim().is_empty() => return Ok(Some(value)),
                _ => warn!(
                    "Agent api_key_ref '{}' was not found as an environment variable",
                    api_key_ref
                ),
            }
        }

        for env_name in Self::provider_api_key_env_names(&agent.model_provider) {
            if let Ok(value) = env::var(env_name) {
                if !value.trim().is_empty() {
                    return Ok(Some(value));
                }
            }
        }

        Ok(None)
    }

    fn provider_base_url(&self, provider: &str) -> Option<String> {
        self.agent_provider_base_urls
            .get(&provider.to_ascii_lowercase())
            .cloned()
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
        request_timeout: Option<&workflow_core::models::duration::OneOfDurationOrIso8601Expression>,
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
        let request_timeout_ms = request_timeout.and_then(|duration| match duration {
            OneOfDurationOrIso8601Expression::Duration(duration) => {
                Some(duration.total_milliseconds())
            }
            OneOfDurationOrIso8601Expression::Iso8601Expression(value) => {
                parse_iso8601_duration_ms(value)
            }
        });
        if let Some(timeout_ms) = request_timeout_ms {
            req_builder = req_builder.timeout(Duration::from_millis(timeout_ms.max(1)));
        }
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
        if let Some(transport) = &args.transport {
            if transport.stdio.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "canonical MCP stdio transport is not supported by the durable executor",
                )
                .into());
            }
            if let Some(http) = &transport.http {
                return Ok(McpServerDefinition {
                    endpoint: Some(http.endpoint.clone()),
                    transport: Some("streamable-http".to_string()),
                    ..McpServerDefinition::default()
                });
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "canonical MCP call requires transport.http or transport.stdio",
            )
            .into());
        }

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
        if result.status_code == "F"
            && self
                .schedule_retry_if_allowed(tx, claimed, &result.task_output)
                .await?
        {
            return Ok(());
        }
        let updated = if let Some(lease) = claimed.host_lease {
            sqlx::query(
                "UPDATE task_info_t SET status_code=$1,locked='N',completed_ts=CURRENT_TIMESTAMP,
                        task_output=$2,lease_owner=NULL,lease_expires_ts=NULL
                  WHERE host_id=$3 AND task_id=$4 AND lease_owner=$5
                    AND lease_fencing_token=$6 AND lease_expires_ts>CURRENT_TIMESTAMP",
            )
            .bind(result.status_code)
            .bind(&result.task_output)
            .bind(claimed.task.host_id)
            .bind(claimed.task.task_id)
            .bind(lease.owner)
            .bind(lease.fencing_token)
            .execute(&mut **tx)
            .await?
        } else {
            sqlx::query(
                "UPDATE task_info_t SET status_code=$1,locked='N',completed_ts=CURRENT_TIMESTAMP,
                        task_output=$2,lease_owner=NULL,lease_expires_ts=NULL
                  WHERE host_id=$3 AND task_id=$4",
            )
            .bind(result.status_code)
            .bind(&result.task_output)
            .bind(claimed.task.host_id)
            .bind(claimed.task.task_id)
            .execute(&mut **tx)
            .await?
        };
        if updated.rows_affected() != 1 {
            return Err(sqlx::Error::Protocol(
                "WORKFLOW_STALE_HOST_TASK_FENCE".to_string(),
            ));
        }

        let invocation_output = result.task_output.clone();
        if self.is_compensation_task(tx, claimed).await? {
            self.reconcile_compensation_task(
                tx,
                claimed,
                result.status_code == "C",
                &result.task_output,
            )
            .await?;
            return Ok(());
        }
        if result.status_code == "C" {
            if self.is_fork_branch(tx, claimed).await? {
                self.reconcile_fork_branch(tx, claimed, true, result.task_output)
                    .await?;
            } else if matches!(
                self.find_task_definition(&claimed.definition, &claimed.task.wf_task_id),
                Some(TaskDefinition::Fork(_))
            ) {
                self.start_fork(tx, claimed).await?;
            } else {
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
            }
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
            if self.is_fork_branch(tx, claimed).await? {
                self.reconcile_fork_branch(tx, claimed, false, result.task_output)
                    .await?;
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
        }

        self.sync_invocation_state(tx, claimed, &invocation_output)
            .await?;

        Ok(())
    }

    async fn schedule_retry_if_allowed(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        claimed: &ClaimedTask,
        failure: &Value,
    ) -> Result<bool, sqlx::Error> {
        use workflow_core::models::retry::OneOfRetryPolicyDefinitionOrReference;

        if failure
            .get("code")
            .and_then(Value::as_str)
            .is_some_and(|code| {
                code.starts_with("WORKFLOW_BUDGET_EXHAUSTED")
                    || code == "WORKFLOW_TASK_TIMEOUT"
                    || code == "WORKFLOW_OUTPUT_INVALID_AFTER_EFFECT"
            })
        {
            return Ok(false);
        }

        let Some(task_def) =
            self.find_task_definition(&claimed.definition, &claimed.task.wf_task_id)
        else {
            return Ok(false);
        };
        let Some(retry) = self.common_fields(task_def).retry.as_ref() else {
            return Ok(false);
        };
        let policy = match retry {
            OneOfRetryPolicyDefinitionOrReference::Retry(policy) => Some(policy),
            OneOfRetryPolicyDefinitionOrReference::Reference(reference) => claimed
                .definition
                .use_
                .as_ref()
                .and_then(|components| components.retries.as_ref())
                .and_then(|policies| policies.get(reference)),
        };
        let Some(policy) = policy else {
            return Ok(false);
        };
        let maximum_attempts = policy
            .limit
            .as_ref()
            .and_then(|limit| limit.attempt.as_ref())
            .and_then(|attempt| attempt.count)
            .unwrap_or(1)
            .max(1);
        let delay_ms = policy
            .delay
            .as_ref()
            .map(|duration| duration.total_milliseconds())
            .or_else(|| {
                policy
                    .limit
                    .as_ref()
                    .and_then(|limit| limit.duration.as_ref())
                    .map(|duration| duration.total_milliseconds())
            })
            .unwrap_or(0);
        let current: Option<RetryTaskState> = sqlx::query_as(
            "SELECT attempt_no,effect_state,downstream_idempotency_key,deadline_ts
                   FROM task_info_t WHERE host_id=$1 AND task_id=$2 FOR UPDATE",
        )
        .bind(claimed.task.host_id)
        .bind(claimed.task.task_id)
        .fetch_optional(&mut **tx)
        .await?;
        let Some(current) = current else {
            return Ok(false);
        };
        if current.attempt_no >= i32::from(maximum_attempts)
            || (current.effect_state != "none" && current.downstream_idempotency_key.is_none())
            || current.deadline_ts.is_some_and(|deadline| {
                deadline
                    <= Utc::now()
                        + chrono::Duration::milliseconds(
                            i64::try_from(delay_ms).unwrap_or(i64::MAX),
                        )
            })
        {
            return Ok(false);
        }
        let Some(lease) = claimed.host_lease else {
            return Ok(false);
        };
        let updated = sqlx::query(
            "UPDATE task_info_t SET status_code='A',locked='N',completed_ts=NULL,
                    task_output=$1,result_code='RETRY_SCHEDULED',attempt_no=attempt_no+1,
                    maximum_attempts=$2,next_attempt_ts=CURRENT_TIMESTAMP+
                      make_interval(secs=>$3::double precision/1000.0),
                    lease_owner=NULL,lease_expires_ts=NULL,update_ts=CURRENT_TIMESTAMP
              WHERE host_id=$4 AND task_id=$5 AND lease_owner=$6
                AND lease_fencing_token=$7 AND lease_expires_ts>CURRENT_TIMESTAMP",
        )
        .bind(failure)
        .bind(i32::from(maximum_attempts))
        .bind(i64::try_from(delay_ms).unwrap_or(i64::MAX))
        .bind(claimed.task.host_id)
        .bind(claimed.task.task_id)
        .bind(lease.owner)
        .bind(lease.fencing_token)
        .execute(&mut **tx)
        .await?;
        if updated.rows_affected() == 1 {
            sqlx::query(
                "UPDATE workflow_invocation_t SET state='RUNNING',updated_ts=CURRENT_TIMESTAMP,
                        state_version=state_version+1
                  WHERE host_id=$1 AND process_id=$2
                    AND state NOT IN ('CANCELLED','COMPLETED','FAILED')",
            )
            .bind(claimed.task.host_id)
            .bind(claimed.task.process_id)
            .execute(&mut **tx)
            .await?;
            return Ok(true);
        }
        Ok(false)
    }

    async fn is_fork_branch(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        claimed: &ClaimedTask,
    ) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM workflow_fork_branch_t WHERE host_id=$1 AND task_id=$2)",
        )
        .bind(claimed.task.host_id)
        .bind(claimed.task.task_id)
        .fetch_one(&mut **tx)
        .await
    }

    async fn is_compensation_task(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        claimed: &ClaimedTask,
    ) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT is_compensation FROM task_info_t WHERE host_id=$1 AND task_id=$2",
        )
        .bind(claimed.task.host_id)
        .bind(claimed.task.task_id)
        .fetch_one(&mut **tx)
        .await
    }

    async fn reconcile_compensation_task(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        claimed: &ClaimedTask,
        succeeded: bool,
        result: &Value,
    ) -> Result<(), sqlx::Error> {
        if !succeeded {
            sqlx::query(
                "UPDATE workflow_invocation_t SET state='FAILED',terminal_ts=CURRENT_TIMESTAMP,
                        user_authorization=NULL,user_authorization_exp=NULL,
                        updated_ts=CURRENT_TIMESTAMP,state_version=state_version+1,
                        normalized_error=jsonb_build_object(
                          'code','WORKFLOW_TASK_FAILED','message','workflow compensation failed',
                          'retryable',false,'detail',$1::jsonb)
                  WHERE host_id=$2 AND process_id=$3 AND state='COMPENSATING'",
            )
            .bind(result)
            .bind(claimed.task.host_id)
            .bind(claimed.task.process_id)
            .execute(&mut **tx)
            .await?;
            sqlx::query(
                "UPDATE process_info_t SET status_code='F',completed_ts=CURRENT_TIMESTAMP,
                        custom_status_code='WORKFLOW_COMPENSATION_FAILED'
                  WHERE host_id=$1 AND process_id=$2",
            )
            .bind(claimed.task.host_id)
            .bind(claimed.task.process_id)
            .execute(&mut **tx)
            .await?;
            return Ok(());
        }
        let remaining: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM task_info_t WHERE host_id=$1 AND process_id=$2
                AND is_compensation AND status_code IN ('A','W')",
        )
        .bind(claimed.task.host_id)
        .bind(claimed.task.process_id)
        .fetch_one(&mut **tx)
        .await?;
        if remaining == 0 {
            sqlx::query(
                "UPDATE workflow_invocation_t SET state='CANCELLED',terminal_ts=CURRENT_TIMESTAMP,
                        user_authorization=NULL,user_authorization_exp=NULL,
                        updated_ts=CURRENT_TIMESTAMP,state_version=state_version+1,
                        non_cancellable_reason=NULL
                  WHERE host_id=$1 AND process_id=$2 AND state='COMPENSATING'",
            )
            .bind(claimed.task.host_id)
            .bind(claimed.task.process_id)
            .execute(&mut **tx)
            .await?;
            sqlx::query(
                "UPDATE process_info_t SET status_code='F',completed_ts=CURRENT_TIMESTAMP,
                        custom_status_code='CANCELLED_AFTER_COMPENSATION'
                  WHERE host_id=$1 AND process_id=$2",
            )
            .bind(claimed.task.host_id)
            .bind(claimed.task.process_id)
            .execute(&mut **tx)
            .await?;
        }
        Ok(())
    }

    async fn start_fork(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        claimed: &ClaimedTask,
    ) -> Result<(), sqlx::Error> {
        let Some(TaskDefinition::Fork(fork)) =
            self.find_task_definition(&claimed.definition, &claimed.task.wf_task_id)
        else {
            return Err(sqlx::Error::Protocol(
                "fork definition is unavailable".into(),
            ));
        };
        let workflow_instance_id = Uuid::parse_str(&claimed.task.wf_instance_id)
            .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
        let continuation = fork.common.then.clone().or_else(|| {
            self.get_next_sequential_task(&claimed.definition, &claimed.task.wf_task_id)
        });
        let join_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO workflow_fork_join_t(
                host_id,join_id,workflow_instance_id,process_id,fork_task_id,fork_task_name,
                continuation_task,compete,expected_branches)
             VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)",
        )
        .bind(claimed.task.host_id)
        .bind(join_id)
        .bind(workflow_instance_id)
        .bind(claimed.task.process_id)
        .bind(claimed.task.task_id)
        .bind(&claimed.task.wf_task_id)
        .bind(&continuation)
        .bind(fork.fork.compete)
        .bind(i32::try_from(fork.fork.branches.entries.len()).unwrap_or(i32::MAX))
        .execute(&mut **tx)
        .await?;
        let security = parse_security_policy(&claimed.raw_definition)
            .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
        let definition_digest: Option<String> = sqlx::query_scalar(
            "SELECT definition_digest FROM process_info_t WHERE host_id=$1 AND process_id=$2",
        )
        .bind(claimed.task.host_id)
        .bind(claimed.task.process_id)
        .fetch_one(&mut **tx)
        .await?;
        let definition_digest = definition_digest.ok_or_else(|| {
            sqlx::Error::Protocol("fork process has no definition digest".to_string())
        })?;
        for branch in &fork.fork.branches.entries {
            let Some((branch_name, branch_task)) = branch.iter().next() else {
                return Err(sqlx::Error::Protocol("fork branch is empty".into()));
            };
            let Some(task_type) = Self::supported_task_type_name(branch_task) else {
                return Err(sqlx::Error::Protocol(format!(
                    "fork branch `{branch_name}` uses an unsupported task type"
                )));
            };
            let task_kind = Self::policy_task_kind(branch_task)?;
            let resolved_policy =
                resolve_policy(task_kind, security.as_ref(), &self.execution_profiles)
                    .map_err(|error| sqlx::Error::Protocol(error.to_string()))?;
            WorkflowRepository::store_policy_snapshot(
                tx,
                claimed.task.host_id,
                &definition_digest,
                &resolved_policy,
                "light-workflow",
            )
            .await?;
            let task_id = Uuid::now_v7();
            let synthetic_name = format!("{}::{branch_name}", claimed.task.wf_task_id);
            WorkflowRepository::insert_task(
                tx,
                &NewTask {
                    host_id: claimed.task.host_id,
                    task_id,
                    task_type,
                    process_id: claimed.task.process_id,
                    wf_instance_id: claimed.task.wf_instance_id.clone(),
                    wf_task_id: &synthetic_name,
                    task_input: &claimed.context_data,
                    placement: resolved_policy.placement,
                    policy_digest: &resolved_policy.policy_digest,
                },
            )
            .await?;
            sqlx::query(
                "UPDATE task_info_t SET fork_join_id=$1,branch_name=$2,
                        deadline_ts=(SELECT deadline_ts FROM workflow_invocation_t
                                      WHERE host_id=$3 AND process_id=$4)
                  WHERE host_id=$3 AND task_id=$5",
            )
            .bind(join_id)
            .bind(branch_name)
            .bind(claimed.task.host_id)
            .bind(claimed.task.process_id)
            .bind(task_id)
            .execute(&mut **tx)
            .await?;
            sqlx::query(
                "INSERT INTO workflow_fork_branch_t(host_id,join_id,branch_name,task_id)
                 VALUES($1,$2,$3,$4)",
            )
            .bind(claimed.task.host_id)
            .bind(join_id)
            .bind(branch_name)
            .bind(task_id)
            .execute(&mut **tx)
            .await?;
        }
        Ok(())
    }

    async fn reconcile_fork_branch(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        claimed: &ClaimedTask,
        succeeded: bool,
        result: Value,
    ) -> Result<(), sqlx::Error> {
        let branch: (Uuid, String) = sqlx::query_as(
            "SELECT join_id,branch_name FROM workflow_fork_branch_t
              WHERE host_id=$1 AND task_id=$2 FOR UPDATE",
        )
        .bind(claimed.task.host_id)
        .bind(claimed.task.task_id)
        .fetch_one(&mut **tx)
        .await?;
        sqlx::query(
            "UPDATE workflow_fork_branch_t SET state=$1,result=$2,completed_ts=CURRENT_TIMESTAMP
              WHERE host_id=$3 AND join_id=$4 AND branch_name=$5 AND state='RUNNING'",
        )
        .bind(if succeeded { "COMPLETED" } else { "FAILED" })
        .bind(&result)
        .bind(claimed.task.host_id)
        .bind(branch.0)
        .bind(&branch.1)
        .execute(&mut **tx)
        .await?;
        let join: (i32, bool, Uuid, Option<String>, String) = sqlx::query_as(
            "SELECT expected_branches,compete,fork_task_id,continuation_task,state
               FROM workflow_fork_join_t WHERE host_id=$1 AND join_id=$2 FOR UPDATE",
        )
        .bind(claimed.task.host_id)
        .bind(branch.0)
        .fetch_one(&mut **tx)
        .await?;
        if join.4 != "RUNNING" {
            return Ok(());
        }
        let rows: Vec<(String, String, Option<Value>)> = sqlx::query_as(
            "SELECT branch_name,state,result FROM workflow_fork_branch_t
              WHERE host_id=$1 AND join_id=$2 ORDER BY branch_name",
        )
        .bind(claimed.task.host_id)
        .bind(branch.0)
        .fetch_all(&mut **tx)
        .await?;
        let completed = rows
            .iter()
            .filter(|(_, state, _)| state != "RUNNING")
            .count();
        let failed = rows
            .iter()
            .filter(|(_, state, _)| state == "FAILED")
            .count();
        let winner = rows.iter().find(|(_, state, _)| state == "COMPLETED");
        let terminal = if join.1 {
            winner.is_some() || completed == usize::try_from(join.0).unwrap_or(usize::MAX)
        } else {
            completed == usize::try_from(join.0).unwrap_or(usize::MAX)
        };
        let mut results = serde_json::Map::new();
        for (name, state, output) in &rows {
            if state != "RUNNING" {
                results.insert(name.clone(), output.clone().unwrap_or(Value::Null));
            }
        }
        sqlx::query(
            "UPDATE workflow_fork_join_t SET completed_branches=$1,failed_branches=$2,
                    branch_results=$3 WHERE host_id=$4 AND join_id=$5",
        )
        .bind(i32::try_from(completed).unwrap_or(i32::MAX))
        .bind(i32::try_from(failed).unwrap_or(i32::MAX))
        .bind(Value::Object(results.clone()))
        .bind(claimed.task.host_id)
        .bind(branch.0)
        .execute(&mut **tx)
        .await?;
        if !terminal {
            return Ok(());
        }
        let success = if join.1 {
            winner.is_some()
        } else {
            failed == 0
        };
        if join.1 && success {
            sqlx::query(
                "UPDATE workflow_fork_branch_t SET state='CANCELLED',completed_ts=CURRENT_TIMESTAMP
                  WHERE host_id=$1 AND join_id=$2 AND state='RUNNING'",
            )
            .bind(claimed.task.host_id)
            .bind(branch.0)
            .execute(&mut **tx)
            .await?;
            sqlx::query(
                "UPDATE task_info_t SET status_code='F',result_code='FORK_COMPETE_CANCELLED',
                        completed_ts=CURRENT_TIMESTAMP,locked='N',lease_owner=NULL,lease_expires_ts=NULL
                  WHERE host_id=$1 AND fork_join_id=$2 AND status_code='A'",
            )
            .bind(claimed.task.host_id)
            .bind(branch.0)
            .execute(&mut **tx)
            .await?;
        }
        sqlx::query(
            "UPDATE workflow_fork_join_t SET state=$1,completed_ts=CURRENT_TIMESTAMP
              WHERE host_id=$2 AND join_id=$3",
        )
        .bind(if success { "COMPLETED" } else { "FAILED" })
        .bind(claimed.task.host_id)
        .bind(branch.0)
        .execute(&mut **tx)
        .await?;
        if !success {
            sqlx::query(
                "UPDATE process_info_t SET status_code='F',completed_ts=CURRENT_TIMESTAMP,
                        error_info='WORKFLOW_FORK_FAILED'
                  WHERE host_id=$1 AND process_id=$2",
            )
            .bind(claimed.task.host_id)
            .bind(claimed.task.process_id)
            .execute(&mut **tx)
            .await?;
            return Ok(());
        }
        let parent = sqlx::query_as::<_, ActiveTask>(
            "SELECT host_id,task_id,task_type,process_id,wf_instance_id,wf_task_id,
                    status_code,result_code FROM task_info_t WHERE host_id=$1 AND task_id=$2",
        )
        .bind(claimed.task.host_id)
        .bind(join.2)
        .fetch_one(&mut **tx)
        .await?;
        let output = if join.1 {
            winner
                .and_then(|(_, _, output)| output.clone())
                .unwrap_or(Value::Object(results))
        } else {
            Value::Object(results)
        };
        self.handle_transition(
            tx,
            &parent,
            &claimed.definition,
            &claimed.raw_definition,
            claimed.context_data.clone(),
            output,
            join.3,
            None,
        )
        .await
    }

    async fn sync_invocation_state(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        claimed: &ClaimedTask,
        task_output: &Value,
    ) -> Result<(), sqlx::Error> {
        let process_status: Option<(String, Option<String>)> = sqlx::query_as(
            "SELECT status_code::text,error_info FROM process_info_t
              WHERE host_id=$1 AND process_id=$2",
        )
        .bind(claimed.task.host_id)
        .bind(claimed.task.process_id)
        .fetch_optional(&mut **tx)
        .await?;
        let Some((status, error_info)) = process_status else {
            return Ok(());
        };
        let state = match status.as_str() {
            "C" => "COMPLETED",
            "F" => "FAILED",
            "W" => "WAITING",
            _ => "RUNNING",
        };
        let mut public_result = match task_output {
            Value::Object(_) => task_output.clone(),
            value => json!({"value":value}),
        };
        let mut normalized_error = error_info.map(|message| {
            let code = if message.contains("WORKFLOW_BUDGET_EXHAUSTED_AFTER_EFFECT") {
                "WORKFLOW_BUDGET_EXHAUSTED_AFTER_EFFECT"
            } else if message.contains("WORKFLOW_BUDGET_EXHAUSTED") {
                "WORKFLOW_BUDGET_EXHAUSTED"
            } else {
                "WORKFLOW_TASK_FAILED"
            };
            json!({
                "code":code,
                "message":message,
                "retryable":false
            })
        });
        let mut state = state;
        if state == "COMPLETED" {
            let context: Value = sqlx::query_scalar(
                "SELECT context_data FROM process_info_t WHERE host_id=$1 AND process_id=$2",
            )
            .bind(claimed.task.host_id)
            .bind(claimed.task.process_id)
            .fetch_one(&mut **tx)
            .await?;
            if let Some(output) = claimed
                .definition
                .output
                .as_ref()
                .and_then(|value| value.as_.as_ref())
            {
                public_result = if let Some(expression) = output.as_str() {
                    match self.value_engine.evaluate_cel_value(
                        "workflow-public-output",
                        expression,
                        &context,
                    ) {
                        Ok(value) => value,
                        Err(error) => {
                            state = "FAILED";
                            normalized_error = Some(json!({
                                "code":"WORKFLOW_OUTPUT_INVALID",
                                "message":error.to_string(),
                                "retryable":false
                            }));
                            json!({})
                        }
                    }
                } else {
                    self.resolve_json_value(output, &context)
                };
            }
            if !public_result.is_object() {
                state = "FAILED";
                normalized_error = Some(json!({
                    "code":"WORKFLOW_OUTPUT_INVALID",
                    "message":"workflow public output must be an object",
                    "retryable":false
                }));
            }
            let output_schema: Option<Value> = sqlx::query_scalar(
                "SELECT response_policy_snapshot->'publicOutputSchema'
                   FROM workflow_invocation_t WHERE host_id=$1 AND process_id=$2",
            )
            .bind(claimed.task.host_id)
            .bind(claimed.task.process_id)
            .fetch_optional(&mut **tx)
            .await?
            .flatten();
            if let Some(schema) = output_schema {
                match jsonschema::Validator::new(&schema) {
                    Ok(validator) if validator.is_valid(&public_result) => {}
                    Ok(_) => {
                        state = "FAILED";
                        normalized_error = Some(json!({
                            "code":"WORKFLOW_OUTPUT_INVALID",
                            "message":"workflow public output does not match the published schema",
                            "retryable":false
                        }));
                    }
                    Err(error) => {
                        state = "FAILED";
                        normalized_error = Some(json!({
                            "code":"WORKFLOW_OUTPUT_INVALID",
                            "message":format!("published output schema is invalid: {error}"),
                            "retryable":false
                        }));
                    }
                }
            }
            let result_byte_limit: Option<i64> = sqlx::query_scalar(
                "SELECT budget.result_byte_limit
                   FROM workflow_invocation_t invocation
                   JOIN workflow_invocation_budget_t budget
                     ON budget.host_id=invocation.host_id
                    AND budget.workflow_instance_id=invocation.workflow_instance_id
                  WHERE invocation.host_id=$1 AND invocation.process_id=$2",
            )
            .bind(claimed.task.host_id)
            .bind(claimed.task.process_id)
            .fetch_optional(&mut **tx)
            .await?;
            let result_bytes = serde_json::to_vec(&public_result)
                .map(|value| i64::try_from(value.len()).unwrap_or(i64::MAX))
                .unwrap_or(i64::MAX);
            if result_byte_limit.is_some_and(|limit| result_bytes > limit) {
                state = "FAILED";
                normalized_error = Some(json!({
                    "code":"WORKFLOW_OUTPUT_INVALID",
                    "message":"workflow public output exceeds the declared byte limit",
                    "retryable":false
                }));
            }
        }
        if state == "FAILED"
            && normalized_error
                .as_ref()
                .and_then(|error| error.get("code"))
                .and_then(Value::as_str)
                == Some("WORKFLOW_OUTPUT_INVALID")
        {
            let effect_state: Option<String> = sqlx::query_scalar(
                "SELECT effect_state FROM workflow_invocation_t WHERE host_id=$1 AND process_id=$2",
            )
            .bind(claimed.task.host_id)
            .bind(claimed.task.process_id)
            .fetch_optional(&mut **tx)
            .await?;
            if effect_state.as_deref() == Some("confirmed")
                && let Some(error) = normalized_error.as_mut()
            {
                error["code"] = Value::String("WORKFLOW_OUTPUT_INVALID_AFTER_EFFECT".to_string());
                error["retryable"] = Value::Bool(false);
            }
        }
        let terminal = matches!(state, "COMPLETED" | "FAILED");
        sqlx::query(
            "UPDATE workflow_invocation_t SET state=$1,updated_ts=CURRENT_TIMESTAMP,
                    state_version=state_version+1,
                    terminal_ts=CASE WHEN $2 THEN CURRENT_TIMESTAMP ELSE NULL END,
                    user_authorization=CASE WHEN $2 THEN NULL ELSE user_authorization END,
                    user_authorization_exp=CASE WHEN $2 THEN NULL ELSE user_authorization_exp END,
                    public_result=CASE WHEN $1='COMPLETED' THEN $3 ELSE public_result END,
                    normalized_error=CASE WHEN $1='FAILED' THEN $4 ELSE normalized_error END
              WHERE host_id=$5 AND process_id=$6 AND state NOT IN ('CANCELLED','COMPLETED','FAILED')",
        )
        .bind(state)
        .bind(terminal)
        .bind(public_result)
        .bind(normalized_error)
        .bind(claimed.task.host_id)
        .bind(claimed.task.process_id)
        .execute(&mut **tx)
        .await?;
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
        if let Some((fork_name, branch_name)) = name.split_once("::") {
            let fork = def
                .do_
                .entries
                .iter()
                .find_map(|entry| entry.get(fork_name));
            if let Some(TaskDefinition::Fork(fork)) = fork {
                return fork
                    .fork
                    .branches
                    .entries
                    .iter()
                    .find_map(|entry| entry.get(branch_name));
            }
        }
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

        let next_task_name = self.resolve_next_task_name(
            definition,
            raw_definition,
            &task.wf_task_id,
            task_def,
            next_task_override,
        );

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

    fn resolve_next_task_name(
        &self,
        definition: &WorkflowDefinition,
        raw_definition: &YamlValue,
        task_name: &str,
        task_definition: &TaskDefinition,
        next_task_override: Option<String>,
    ) -> Option<String> {
        if self.task_ends_workflow(raw_definition, task_name) {
            return None;
        }

        match next_task_override
            .or_else(|| self.get_then_directive(task_definition).clone())
            .as_deref()
        {
            Some("end" | "exit") => None,
            None | Some("continue") => self.get_next_sequential_task(definition, task_name),
            Some(task_name) => Some(task_name.to_string()),
        }
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
            TaskDefinition::LegacyAgent(task) => &task.common,
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

    async fn task_execution_timeout(
        &self,
        claimed: &ClaimedTask,
    ) -> Result<Option<Duration>, DynError> {
        use workflow_core::models::duration::OneOfDurationOrIso8601Expression;
        use workflow_core::models::timeout::OneOfTimeoutDefinitionOrReference;

        let task_def = self
            .find_task_definition(&claimed.definition, &claimed.task.wf_task_id)
            .ok_or_else(|| io::Error::other("workflow task definition is unavailable"))?;
        let task_timeout = self.common_fields(task_def).timeout.as_ref();
        let timeout = task_timeout.or(claimed.definition.timeout.as_ref());
        let configured = match timeout {
            Some(OneOfTimeoutDefinitionOrReference::Timeout(timeout)) => Some(&timeout.after),
            Some(OneOfTimeoutDefinitionOrReference::Reference(reference)) => claimed
                .definition
                .use_
                .as_ref()
                .and_then(|components| components.timeouts.as_ref())
                .and_then(|timeouts| timeouts.get(reference))
                .map(|timeout| &timeout.after),
            None => None,
        };
        let configured_ms = configured.and_then(|duration| match duration {
            OneOfDurationOrIso8601Expression::Duration(duration) => {
                Some(duration.total_milliseconds())
            }
            OneOfDurationOrIso8601Expression::Iso8601Expression(value) => {
                parse_iso8601_duration_ms(value)
            }
        });
        let deadline: Option<chrono::DateTime<Utc>> = sqlx::query_scalar(
            "SELECT deadline_ts FROM workflow_invocation_t WHERE host_id=$1 AND process_id=$2",
        )
        .bind(claimed.task.host_id)
        .bind(claimed.task.process_id)
        .fetch_optional(&self.pool)
        .await?
        .flatten();
        let deadline_ms = deadline.map(|deadline| {
            u64::try_from((deadline - Utc::now()).num_milliseconds().max(1)).unwrap_or(1)
        });
        Ok(match (configured_ms, deadline_ms) {
            (Some(configured), Some(deadline)) => {
                Some(Duration::from_millis(configured.min(deadline).max(1)))
            }
            (Some(configured), None) => Some(Duration::from_millis(configured.max(1))),
            (None, Some(deadline)) => Some(Duration::from_millis(deadline.max(1))),
            (None, None) => None,
        })
    }

    async fn claim_task_effect(
        &self,
        claimed: &ClaimedTask,
        idempotency_key: String,
        request_digest: String,
    ) -> Result<EffectClaim, DynError> {
        if idempotency_key.trim().is_empty() || idempotency_key.len() > 255 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "resolved task idempotency key must contain between 1 and 255 bytes",
            )
            .into());
        }
        let workflow_instance_id = Uuid::parse_str(&claimed.task.wf_instance_id).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "workflow-backed task instance ID must be a UUID",
            )
        })?;
        let (_, replayed, result, _): (bool, bool, Option<Value>, String) = sqlx::query_as(
            "SELECT claimed,replayed,result,effect_state FROM workflow_claim_task_effect_v1($1,$2,$3,$4,$5)",
        )
        .bind(claimed.task.host_id)
        .bind(workflow_instance_id)
        .bind(&claimed.task.wf_task_id)
        .bind(&idempotency_key)
        .bind(&request_digest)
        .fetch_one(&self.pool)
        .await?;
        let compensation_task = self
            .find_task_definition(&claimed.definition, &claimed.task.wf_task_id)
            .and_then(|task| self.common_fields(task).metadata.as_ref())
            .and_then(|metadata| metadata.get("compensationTask"))
            .and_then(Value::as_str);
        sqlx::query(
            "UPDATE task_info_t SET effect_state=CASE WHEN $1 THEN 'confirmed' ELSE 'possible' END,
                    downstream_idempotency_key=$2,compensation_task=$5,update_ts=CURRENT_TIMESTAMP
              WHERE host_id=$3 AND task_id=$4",
        )
        .bind(replayed)
        .bind(&idempotency_key)
        .bind(claimed.task.host_id)
        .bind(claimed.task.task_id)
        .bind(compensation_task)
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "UPDATE workflow_invocation_t SET
                    effect_state=CASE WHEN $1 THEN 'confirmed' ELSE
                      CASE WHEN effect_state='none' THEN 'possible' ELSE effect_state END END,
                    updated_ts=CURRENT_TIMESTAMP,state_version=state_version+1
              WHERE host_id=$2 AND process_id=$3",
        )
        .bind(replayed)
        .bind(claimed.task.host_id)
        .bind(claimed.task.process_id)
        .execute(&self.pool)
        .await?;
        Ok(EffectClaim {
            idempotency_key,
            request_digest,
            replayed_result: replayed.then(|| result.unwrap_or_else(|| json!({}))),
        })
    }

    async fn confirm_task_effect(
        &self,
        claimed: &ClaimedTask,
        claim: &EffectClaim,
        result: &Value,
    ) -> Result<(), DynError> {
        let workflow_instance_id = Uuid::parse_str(&claimed.task.wf_instance_id)?;
        let confirmed: bool =
            sqlx::query_scalar("SELECT workflow_confirm_task_effect_v1($1,$2,$3,$4,$5,$6)")
                .bind(claimed.task.host_id)
                .bind(workflow_instance_id)
                .bind(&claimed.task.wf_task_id)
                .bind(&claim.idempotency_key)
                .bind(&claim.request_digest)
                .bind(result)
                .fetch_one(&self.pool)
                .await?;
        if !confirmed {
            return Err(io::Error::other("WORKFLOW_TASK_EFFECT_CONFIRMATION_FAILED").into());
        }
        sqlx::query(
            "UPDATE task_info_t SET effect_state='confirmed',update_ts=CURRENT_TIMESTAMP
              WHERE host_id=$1 AND task_id=$2",
        )
        .bind(claimed.task.host_id)
        .bind(claimed.task.task_id)
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "UPDATE workflow_invocation_t SET effect_state='confirmed',updated_ts=CURRENT_TIMESTAMP,
                    state_version=state_version+1 WHERE host_id=$1 AND process_id=$2",
        )
        .bind(claimed.task.host_id)
        .bind(claimed.task.process_id)
        .execute(&self.pool)
        .await?;
        Ok(())
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

    fn resolve_http_string_map(
        &self,
        values: Option<&HashMap<String, String>>,
        context: &Value,
    ) -> Vec<(String, String)> {
        let mut resolved = values
            .into_iter()
            .flat_map(HashMap::iter)
            .map(|(name, value)| {
                (
                    name.clone(),
                    self.resolve_template_to_string(value, context),
                )
            })
            .collect::<Vec<_>>();
        resolved.sort_by(|left, right| left.0.cmp(&right.0));
        resolved
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

        if !expression.starts_with('.')
            && let Ok(value) =
                self.value_engine
                    .evaluate_cel_value("workflow-runtime-value", expression, context)
        {
            return Some(value);
        }

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

        if !expression.starts_with('.')
            && let Ok(result) = self.value_engine.evaluate_cel_predicate(
                "workflow-runtime-predicate",
                expression,
                Some("standard"),
                "workflow",
                context,
            )
        {
            return Ok(result);
        }

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

fn resolve_lightapi_http_endpoint(
    document: &Value,
    capability_ref: &str,
    environment: &str,
    method: &str,
    fallback_base_uri: &str,
) -> Result<String, DynError> {
    let operations = document
        .get("operations")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "validated LightAPI document has no operations object",
            )
        })?;
    let matches_portal_operation = |operation: &Value| {
        operation.get("endpointId").and_then(Value::as_str) == Some(capability_ref)
            && operation.get("protocol").and_then(Value::as_str) == Some("http")
            && operation
                .get("method")
                .and_then(Value::as_str)
                .is_some_and(|operation_method| operation_method.eq_ignore_ascii_case(method))
            && operation
                .get("lifecycle")
                .and_then(Value::as_str)
                .is_none_or(|lifecycle| lifecycle == "active")
    };
    let mut matching_operations: Vec<_> = operations
        .iter()
        .filter(|(_, operation)| matches_portal_operation(operation))
        .collect();
    matching_operations.sort_by(|(left, _), (right, _)| left.cmp(right));
    let operation = matching_operations
        .iter()
        .find(|(_, operation)| !lightapi_authentication_required(document, operation))
        .map(|(_, operation)| *operation);
    let Some(operation) = operation else {
        if matching_operations
            .iter()
            .any(|(_, operation)| lightapi_authentication_required(document, operation))
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "authenticated LightAPI operations require delegated credential support",
            )
            .into());
        }
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "LightAPI operation protocol, method, or lifecycle is not callable",
        )
        .into());
    };
    let endpoint = operation
        .get("endpoint")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "validated LightAPI HTTP operation has no endpoint",
            )
        })?;
    let variables = lightapi_environment_variables(document, environment)?;
    let resolved = LIGHTAPI_ENV_EXPRESSION_REGEX
        .replace_all(endpoint, |captures: &regex::Captures<'_>| {
            variables
                .get(&captures[1])
                .cloned()
                .unwrap_or_else(|| captures[0].to_string())
        })
        .into_owned();
    if LIGHTAPI_ENV_EXPRESSION_REGEX.is_match(&resolved) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("LightAPI environment '{environment}' does not resolve the endpoint"),
        )
        .into());
    }
    let base = reqwest::Url::parse(&format!("{}/", fallback_base_uri.trim_end_matches('/')))
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid Portal API-version target '{fallback_base_uri}': {error}"),
            )
        })?;
    if !matches!(base.scheme(), "http" | "https") || base.host_str().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Portal API-version target '{fallback_base_uri}' is not an HTTP origin"),
        )
        .into());
    }
    let (destination, absolute_endpoint) = match reqwest::Url::parse(&resolved) {
        Ok(destination) => (destination, true),
        Err(_) => (base.join(&resolved)?, false),
    };
    let same_authority = destination.scheme() == base.scheme()
        && destination.host_str() == base.host_str()
        && destination.port_or_known_default() == base.port_or_known_default()
        && destination.username().is_empty()
        && destination.password().is_none();
    if !same_authority {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "LightAPI operation endpoint must remain on the Portal API-version target authority",
        )
        .into());
    }
    Ok(if absolute_endpoint {
        resolved
    } else {
        restore_openapi_path_placeholders(destination.to_string(), &resolved)
    })
}

fn workflow_http_authorization_headers(
    user_authorization: Option<&str>,
    scope_authorization: Option<&str>,
) -> Result<(String, String), io::Error> {
    let user_authorization = user_authorization
        .and_then(normalize_bearer_header)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "workflow invocation has no current user Authorization token",
            )
        })?;
    let scope_authorization = scope_authorization
        .and_then(service_bearer_header)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "workflow service X-Scope-Token is unavailable",
            )
        })?;
    Ok((user_authorization, scope_authorization))
}

fn normalize_bearer_header(value: &str) -> Option<String> {
    let (scheme, token) = value.trim().split_once(char::is_whitespace)?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then(|| format!("Bearer {token}"))
}

fn service_bearer_header(value: &str) -> Option<String> {
    let value = value.trim();
    if let Some(header) = normalize_bearer_header(value) {
        return Some(header);
    }
    (!value.is_empty() && !value.chars().any(char::is_whitespace))
        .then(|| format!("Bearer {value}"))
}

fn restore_openapi_path_placeholders(mut destination: String, source: &str) -> String {
    for captures in OPENAPI_PATH_PLACEHOLDER_REGEX.captures_iter(source) {
        let Some(placeholder) = captures.get(0) else {
            continue;
        };
        let encoded = format!("%7B{}%7D", &captures[1]);
        destination = destination.replace(&encoded, placeholder.as_str());
        destination = destination.replace(&encoded.to_ascii_lowercase(), placeholder.as_str());
    }
    destination
}

fn is_protected_workflow_http_header(
    name: &reqwest::header::HeaderName,
    workflow_backed: bool,
) -> bool {
    matches!(
        name,
        &reqwest::header::HOST
            | &reqwest::header::CONTENT_LENGTH
            | &reqwest::header::TRANSFER_ENCODING
            | &reqwest::header::CONNECTION
    ) || (workflow_backed
        && (name == reqwest::header::AUTHORIZATION
            || name.as_str().eq_ignore_ascii_case("x-scope-token")))
}

fn workflow_http_requires_registered_target(
    workflow_backed: bool,
    granted_uri_available: bool,
    registered_uri_available: bool,
) -> bool {
    workflow_backed && !granted_uri_available && !registered_uri_available
}

fn lightapi_authentication_required(document: &Value, operation: &Value) -> bool {
    match operation.get("authentication") {
        None | Some(Value::Null) => true,
        Some(Value::Object(authentication)) => {
            authentication.get("type").and_then(Value::as_str) != Some("none")
        }
        Some(Value::String(reference)) => {
            document
                .get("authentications")
                .and_then(Value::as_object)
                .and_then(|authentications| authentications.get(reference))
                .and_then(|authentication| authentication.get("type"))
                .and_then(Value::as_str)
                != Some("none")
        }
        Some(_) => true,
    }
}

fn lightapi_environment_variables(
    document: &Value,
    environment: &str,
) -> Result<HashMap<String, String>, DynError> {
    let environments = document.get("environments").and_then(Value::as_object);
    let Some(environments) = environments else {
        return Ok(HashMap::new());
    };
    if environments.is_empty() {
        return Ok(HashMap::new());
    }
    let mut variables = HashMap::new();
    let mut chain = Vec::new();
    let mut current = environment;
    let mut visited = HashSet::new();
    loop {
        if !visited.insert(current.to_string()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "LightAPI environment inheritance contains a cycle",
            )
            .into());
        }
        let value = environments.get(current).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("LightAPI environment '{environment}' is not defined"),
            )
        })?;
        chain.push(value);
        let Some(parent) = value.get("extends").and_then(Value::as_str) else {
            break;
        };
        current = parent;
    }
    for value in chain.into_iter().rev() {
        if let Some(entries) = value.get("variables").and_then(Value::as_object) {
            for (name, value) in entries {
                let value = match value {
                    Value::String(value) => value.clone(),
                    Value::Null => String::new(),
                    value => value.to_string(),
                };
                variables.insert(name.clone(), value);
            }
        }
    }
    Ok(variables)
}

fn parse_iso8601_duration_ms(value: &str) -> Option<u64> {
    let value = value.strip_prefix("PT")?;
    if value.is_empty() {
        return None;
    }
    let mut total = 0_u64;
    let mut digits = String::new();
    for character in value.chars() {
        if character.is_ascii_digit() {
            digits.push(character);
            continue;
        }
        let amount = digits.parse::<u64>().ok()?;
        digits.clear();
        let multiplier = match character {
            'H' => 3_600_000,
            'M' => 60_000,
            'S' => 1_000,
            _ => return None,
        };
        total = total.saturating_add(amount.saturating_mul(multiplier));
    }
    (digits.is_empty() && total > 0).then_some(total)
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

    #[tokio::test]
    async fn managed_agent_credentials_reject_literal_references() {
        let mut executor = executor();
        executor.managed_configuration = true;
        let agent = AgentDefinitionRecord {
            agent_def_id: Uuid::nil(),
            agent_name: Some("test".to_string()),
            model_provider: "openai".to_string(),
            model_name: "test".to_string(),
            api_key_ref: Some("literal:must-not-enter-config".to_string()),
            temperature: 0.0,
            max_tokens: None,
            aggregate_version: 1,
        };
        let error = executor
            .resolve_agent_api_key(&agent)
            .expect_err("managed literal credential must fail");
        assert!(error.to_string().contains("must not use literal:"));
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
            wf_def_id: Uuid::nil(),
            context_data: json!({"requestId": "REQ-1", "summary": "review"}),
            definition: serde_yaml::from_str(yaml).expect("fixture must be a workflow"),
            raw_definition: serde_yaml::from_str(yaml).expect("fixture must be YAML"),
            host_lease: None,
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
    fn interactive_host_execution_uses_bounded_concurrency_and_short_leases() {
        let concurrency = DEFAULT_HOST_EXECUTOR_CONCURRENCY;
        let lease_ms = DEFAULT_HOST_TASK_LEASE_MS;
        assert!(concurrency > 1);
        assert!(lease_ms <= 30_000);
    }

    #[test]
    fn phase2_iso8601_deadlines_are_bounded_and_deterministic() {
        assert_eq!(parse_iso8601_duration_ms("PT2M5S"), Some(125_000));
        assert_eq!(parse_iso8601_duration_ms("PT1H"), Some(3_600_000));
        assert_eq!(parse_iso8601_duration_ms("P1D"), None);
        assert_eq!(parse_iso8601_duration_ms("PT0S"), None);
    }

    #[tokio::test]
    async fn canonical_mcp_http_transport_resolves_to_runtime_server() {
        let executor = executor();
        let args: McpArguments = serde_json::from_value(json!({
            "method": "tools/list",
            "transport": {
                "http": {
                    "endpoint": "https://gateway.example/mcp",
                    "headers": { "x-tenant": "demo" }
                }
            }
        }))
        .unwrap();

        let server = executor
            .resolve_mcp_server(&args, &WorkflowDefinition::default())
            .expect("canonical HTTP transport must normalize");

        assert_eq!(server.transport.as_deref(), Some("streamable-http"));
        assert_eq!(
            server
                .endpoint
                .as_ref()
                .map(|endpoint| executor.endpoint_to_uri(endpoint)),
            Some("https://gateway.example/mcp".to_string())
        );
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
    async fn workflow_tool_access_fixture_preserves_typed_approval_contract() {
        let executor = executor();
        let claimed = claimed_from_yaml(
            include_str!("../examples/grant-tools-to-workflow.yaml"),
            "reviewToolAccess",
            "ask",
        );

        let waiting = executor
            .execute_task(&claimed)
            .await
            .expect("approval ask is local");
        assert_eq!(waiting.status_code, "W");
        assert_eq!(
            waiting.task_output["ask"]["action"],
            "workflow-tool-access-decision"
        );
        assert_eq!(
            waiting.task_output["ask"]["assignment"]["roleId"],
            "genai-admin"
        );
        assert_eq!(waiting.task_output["ask"]["options"][0]["value"], "APPROVE");
        assert_eq!(waiting.task_output["ask"]["options"][1]["value"], "REJECT");
    }

    #[tokio::test]
    async fn logical_lightapi_http_call_without_a_tool_pin_fails_before_dispatch() {
        let executor = executor();
        let claimed = claimed_from_yaml(
            r#"
document:
  dsl: "1.0.3"
  namespace: test
  name: grant-check
  version: "1.0.0"
do:
  - denied:
      call: http
      with:
        method: GET
        endpoint:
          uri: lightapi://customer-api/customer.get
"#,
            "denied",
            "call",
        );

        let error = match executor.execute_task(&claimed).await {
            Ok(_) => panic!("missing pin must fail"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("metadata.workflowTool"));
    }

    #[tokio::test]
    async fn logical_lightapi_http_call_requires_exact_tool_id_before_database_access() {
        let executor = executor();
        let claimed = claimed_from_yaml(
            r#"
document: { dsl: "1.0.3", namespace: test, name: grant-check, version: "1.0.0" }
do:
  - denied:
      call: http
      with:
        method: GET
        endpoint: { uri: "lightapi://customer-api/customer.get" }
      metadata:
        workflowTool:
          capabilityRef: customer-api/customer.get
          version: "1.0.0"
          lightapiDigest: "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
          allowedEnvironments: [local]
"#,
            "denied",
            "call",
        );

        let error = match executor.execute_task(&claimed).await {
            Ok(_) => panic!("missing toolId must fail"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("toolId"));
    }

    #[test]
    fn workflow_tool_grant_satisfies_workflow_backed_http_target_authorization() {
        assert!(!workflow_http_requires_registered_target(true, true, false));
    }

    #[test]
    fn workflow_tool_grant_sql_keeps_definition_wide_bind_contract() {
        let source = include_str!("executor.rs");
        let start = source
            .find("SELECT g.grant_id,t.tool_id,t.lightapi_document")
            .expect("workflow Tool grant SQL must exist");
        let end = source[start..]
            .find(".fetch_optional(&self.pool)")
            .map(|offset| start + offset)
            .expect("workflow Tool grant query must execute");
        let query = &source[start..end];
        for expected in [
            "g.host_id=$1",
            "g.wf_def_id=$2",
            "g.tool_id=$3",
            "g.tool_version=$4",
            "g.lightapi_digest=$5",
            "$6=ANY(g.allowed_environments)",
            "t.capability_ref=$7",
            "upper(e.http_method)=upper($8)",
        ] {
            assert!(
                query.contains(expected),
                "missing positional predicate {expected}"
            );
        }
        for expected in [
            ".bind(claimed.task.host_id)",
            ".bind(claimed.wf_def_id)",
            ".bind(tool_id)",
            ".bind(tool_version)",
            ".bind(lightapi_digest)",
            ".bind(&environment)",
            ".bind(capability_ref)",
            ".bind(&http_call.with.method)",
        ] {
            assert!(
                query.contains(expected),
                "missing positional bind {expected}"
            );
        }
        assert!(!query.contains("workflow_version"));
    }

    #[test]
    fn workflow_http_preserves_user_authorization_and_adds_scope_token() {
        let headers = workflow_http_authorization_headers(
            Some("bEaReR current-user-jwt"),
            Some("workflow-service-token"),
        )
        .unwrap();
        assert_eq!(headers.0, "Bearer current-user-jwt");
        assert_eq!(headers.1, "Bearer workflow-service-token");
        assert!(workflow_http_authorization_headers(None, Some("scope-token")).is_err());
    }

    #[test]
    fn workflow_backed_direct_http_call_still_requires_registered_endpoint() {
        assert!(workflow_http_requires_registered_target(true, false, false));
        assert!(!workflow_http_requires_registered_target(true, false, true));
        assert!(!workflow_http_requires_registered_target(
            false, false, false
        ));
    }

    #[test]
    fn fork_transition_uses_host_orchestration_policy() {
        let definition: WorkflowDefinition = serde_yaml::from_str(
            r#"
document: { dsl: 1.0.3, namespace: test, name: fork-policy, version: 1.0.0 }
evaluate: { language: cel }
do:
  - load:
      fork:
        branches:
          - profile:
              set: { source: profile }
        compete: false
"#,
        )
        .unwrap();
        let fork = definition.do_.entries[0].get("load").unwrap();
        assert_eq!(
            TaskExecutor::policy_task_kind(fork).unwrap(),
            TaskKind::Fork
        );
        let TaskDefinition::Fork(fork_definition) = fork else {
            panic!("expected fork task");
        };
        let branch = fork_definition.fork.branches.entries[0]
            .get("profile")
            .unwrap();
        let branch_policy = resolve_policy(
            TaskExecutor::policy_task_kind(branch).unwrap(),
            None,
            &BTreeMap::new(),
        )
        .unwrap();
        let fork_policy = resolve_policy(TaskKind::Fork, None, &BTreeMap::new()).unwrap();
        assert_eq!(
            branch_policy.placement,
            workflow_policy::ExecutionPlacement::Host
        );
        assert_eq!(branch_policy.action_kind, "set");
        assert_ne!(branch_policy.policy_digest, fork_policy.policy_digest);
    }

    #[test]
    fn lightapi_http_endpoint_uses_the_selected_environment() {
        let document = json!({
            "environments": {
                "base": {"variables": {"serviceBaseUrl": "https://base.example"}},
                "local": {"extends": "base", "variables": {"serviceBaseUrl": "http://customer-api:8080"}}
            },
            "operations": {
                "preferences": {
                    "endpointId": "customer-api/preferences.get",
                    "protocol": "http",
                    "method": "GET",
                    "endpoint": "${env.serviceBaseUrl}/customers/{customerId}/preferences",
                    "authentication": {"type": "none"}
                }
            }
        });

        let endpoint = resolve_lightapi_http_endpoint(
            &document,
            "customer-api/preferences.get",
            "local",
            "GET",
            "http://customer-api:8080",
        )
        .expect("selected environment must resolve");

        assert_eq!(
            endpoint,
            "http://customer-api:8080/customers/{customerId}/preferences"
        );
    }

    #[tokio::test]
    async fn lightapi_relative_http_endpoint_preserves_path_placeholders() {
        let document = json!({
            "operations": {
                "profile": {
                    "endpointId": "customer-api/profile.get",
                    "protocol": "http",
                    "method": "GET",
                    "endpoint": "/customers/{customerId}",
                    "authentication": {"type": "none"}
                }
            }
        });

        let endpoint = resolve_lightapi_http_endpoint(
            &document,
            "customer-api/profile.get",
            "local",
            "GET",
            "http://customer-api:8080",
        )
        .expect("relative endpoint must resolve without encoding its path placeholder");

        assert_eq!(endpoint, "http://customer-api:8080/customers/{customerId}");

        let executor = executor();
        let configured_template = OPENAPI_PATH_PLACEHOLDER_REGEX
            .replace_all(&endpoint, |captures: &regex::Captures<'_>| {
                format!("${{{{ {} }}}}", &captures[1])
            })
            .into_owned();
        assert_eq!(
            executor.resolve_template_to_string(
                &configured_template,
                &json!({"customerId": "CUST-1001"}),
            ),
            "http://customer-api:8080/customers/CUST-1001"
        );
    }

    #[test]
    fn lightapi_http_endpoint_rejects_absolute_foreign_authority() {
        let document = json!({
            "operations": {
                "preferences": {
                    "endpointId": "customer-api/preferences.get",
                    "protocol": "http",
                    "method": "GET",
                    "endpoint": "https://evil.example/preferences",
                    "authentication": {"type": "none"}
                }
            }
        });

        let error = resolve_lightapi_http_endpoint(
            &document,
            "customer-api/preferences.get",
            "local",
            "GET",
            "https://customer-api.example",
        )
        .expect_err("an absolute foreign authority must not bypass the Portal target");

        assert!(
            error
                .to_string()
                .contains("Portal API-version target authority")
        );
    }

    #[test]
    fn lightapi_http_endpoint_rejects_protocol_relative_foreign_authority() {
        let document = json!({
            "operations": {
                "preferences": {
                    "endpointId": "customer-api/preferences.get",
                    "protocol": "http",
                    "method": "GET",
                    "endpoint": "//evil.example/preferences",
                    "authentication": {"type": "none"}
                }
            }
        });

        let error = resolve_lightapi_http_endpoint(
            &document,
            "customer-api/preferences.get",
            "local",
            "GET",
            "https://customer-api.example",
        )
        .expect_err("a protocol-relative foreign authority must not bypass the Portal target");

        assert!(
            error
                .to_string()
                .contains("Portal API-version target authority")
        );
    }

    #[test]
    fn lightapi_http_endpoint_selects_the_same_sorted_callable_operation_as_portal() {
        let document = json!({
            "operations": {
                "a-wrong-method": {
                    "endpointId": "customer-api/preferences.get",
                    "protocol": "http",
                    "method": "POST",
                    "endpoint": "/wrong",
                    "authentication": {"type": "none"}
                },
                "b-callable": {
                    "endpointId": "customer-api/preferences.get",
                    "protocol": "http",
                    "method": "GET",
                    "endpoint": "/preferences",
                    "authentication": {"type": "none"}
                },
                "c-callable": {
                    "endpointId": "customer-api/preferences.get",
                    "protocol": "http",
                    "method": "GET",
                    "endpoint": "/later",
                    "authentication": {"type": "none"}
                }
            }
        });

        let endpoint = resolve_lightapi_http_endpoint(
            &document,
            "customer-api/preferences.get",
            "local",
            "GET",
            "https://customer-api.example",
        )
        .expect("the first sorted operation matching Portal's full predicate must be selected");

        assert_eq!(endpoint, "https://customer-api.example/preferences");
    }

    #[test]
    fn protected_lightapi_http_endpoint_fails_closed() {
        let document = json!({
            "operations": {
                "preferences": {
                    "endpointId": "customer-api/preferences.get",
                    "protocol": "http",
                    "method": "GET",
                    "endpoint": "/preferences",
                    "authentication": {"type": "oauth2"}
                }
            }
        });

        let error = resolve_lightapi_http_endpoint(
            &document,
            "customer-api/preferences.get",
            "local",
            "GET",
            "http://customer-api:8080",
        )
        .expect_err("authenticated operation must not bypass delegated credentials");

        assert!(error.to_string().contains("delegated credential"));
    }

    #[test]
    fn lightapi_http_endpoint_without_explicit_authentication_fails_closed() {
        let document = json!({
            "operations": {
                "preferences": {
                    "endpointId": "customer-api/preferences.get",
                    "protocol": "http",
                    "method": "GET",
                    "endpoint": "/preferences"
                }
            }
        });

        let error = resolve_lightapi_http_endpoint(
            &document,
            "customer-api/preferences.get",
            "local",
            "GET",
            "http://customer-api:8080",
        )
        .expect_err("missing authentication metadata must fail closed");

        assert!(error.to_string().contains("delegated credential"));
    }

    #[test]
    fn non_active_lightapi_http_endpoint_fails_closed() {
        let document = json!({
            "operations": {
                "preferences": {
                    "endpointId": "customer-api/preferences.get",
                    "protocol": "http",
                    "method": "GET",
                    "endpoint": "/preferences",
                    "lifecycle": "draft",
                    "authentication": {"type": "none"}
                }
            }
        });

        let error = resolve_lightapi_http_endpoint(
            &document,
            "customer-api/preferences.get",
            "local",
            "GET",
            "http://customer-api:8080",
        )
        .expect_err("non-active operation must not be callable");

        assert!(error.to_string().contains("lifecycle"));
    }

    #[tokio::test]
    async fn http_query_and_header_templates_resolve_from_context() {
        let executor = executor();
        let context = json!({"channel": "mobile", "requestId": "request-1"});
        let values = HashMap::from([
            ("channel".to_string(), "${{ channel }}".to_string()),
            ("requestId".to_string(), "${{ requestId }}".to_string()),
        ]);
        let resolved: HashMap<_, _> = executor
            .resolve_http_string_map(Some(&values), &context)
            .into_iter()
            .collect();

        assert_eq!(resolved.get("channel").map(String::as_str), Some("mobile"));
        assert_eq!(
            resolved.get("requestId").map(String::as_str),
            Some("request-1")
        );
    }

    #[test]
    fn workflow_http_protected_headers_cannot_be_overridden() {
        for name in [
            "host",
            "authorization",
            "x-scope-token",
            "content-length",
            "transfer-encoding",
            "connection",
        ] {
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes()).unwrap();
            assert!(is_protected_workflow_http_header(&name, true), "{name}");
        }
        assert!(!is_protected_workflow_http_header(
            &reqwest::header::HeaderName::from_static("x-request-id"),
            true,
        ));
        assert!(!is_protected_workflow_http_header(
            &reqwest::header::AUTHORIZATION,
            false,
        ));
        assert!(!is_protected_workflow_http_header(
            &reqwest::header::HeaderName::from_static("x-scope-token"),
            false,
        ));
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
    async fn open_workflow_flow_directives_are_control_tokens() {
        let executor = executor();
        for (directive, expected) in [
            ("continue", Some("second")),
            ("second", Some("second")),
            ("end", None),
            ("exit", None),
        ] {
            let yaml = format!(
                "document: {{ dsl: 1.0.3, namespace: test, name: flow, version: 1.0.0 }}\ndo:\n  - first:\n      set: {{ value: 1 }}\n      then: {directive}\n  - second:\n      set: {{ value: 2 }}"
            );
            let definition: WorkflowDefinition = serde_yaml::from_str(&yaml).unwrap();
            let raw: YamlValue = serde_yaml::from_str(&yaml).unwrap();
            let first = executor
                .find_task_definition(&definition, "first")
                .expect("fixture has first task");

            assert_eq!(
                executor
                    .resolve_next_task_name(&definition, &raw, "first", first, None)
                    .as_deref(),
                expected
            );
        }
    }

    #[tokio::test]
    async fn legacy_end_true_remains_a_read_compatibility_marker() {
        let executor = executor();
        let yaml = "document: { dsl: 1.0.3, namespace: test, name: legacy, version: 1.0.0 }\ndo:\n  - first:\n      set: { value: 1 }\n      end: true\n  - second:\n      set: { value: 2 }";
        let definition: WorkflowDefinition = serde_yaml::from_str(yaml).unwrap();
        let raw: YamlValue = serde_yaml::from_str(yaml).unwrap();
        let first = executor
            .find_task_definition(&definition, "first")
            .expect("fixture has first task");

        assert_eq!(
            executor.resolve_next_task_name(&definition, &raw, "first", first, None),
            None
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
