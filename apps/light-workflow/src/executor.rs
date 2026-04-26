use light_rule::{ActionRegistry, MultiThreadRuleExecutor, RuleConfig, RuleEngine};
use regex::Regex;
use serde_json::{Map as JsonMap, Number, Value, json};
use serde_yaml::Value as YamlValue;
use sqlx::{PgPool, Postgres, Transaction};
use std::collections::HashMap;
use std::io;
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info};
use uuid::Uuid;
use workflow_core::models::task::{
    AssertComparison, AssertComparisonObject, AssertDefinition, CallTaskDefinition,
    HasLengthComparison, JsonRpcArguments, JsonRpcErrorPolicy, McpArguments, McpServerDefinition,
    OpenRpcArguments, SetValue, TaskDefinition, TaskDefinitionFields,
};
use workflow_core::models::workflow::WorkflowDefinition;

type DynError = Box<dyn std::error::Error + Send + Sync>;
static TEMPLATE_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\$\{\{\s*([^}]*(?:}[^}]+)*)\s*\}\}|\$\{\s*([^}]*)\s*\}")
        .expect("valid template regex")
});
const TASK_LOCK_TIMEOUT_MINUTES: i64 = 5;
const MAX_HTTP_RESPONSE_BYTES: usize = 1024 * 1024;

#[derive(sqlx::FromRow)]
pub struct ActiveTask {
    pub host_id: Uuid,
    pub task_id: Uuid,
    pub task_type: String,
    pub process_id: Uuid,
    pub wf_instance_id: String,
    pub wf_task_id: String,
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

pub struct TaskExecutor {
    pool: PgPool,
    http_client: reqwest::Client,
    rule_executor: Arc<MultiThreadRuleExecutor>,
}

impl TaskExecutor {
    fn supported_task_type_name(task_def: &TaskDefinition) -> Option<&'static str> {
        match task_def {
            TaskDefinition::Ask(_) => Some("ask"),
            TaskDefinition::Assert(_) => Some("assert"),
            TaskDefinition::Call(_) => Some("call"),
            TaskDefinition::Set(_) => Some("set"),
            TaskDefinition::Switch(_) => Some("switch"),
            _ => None,
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
        }
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

        let result = match self.execute_task(&claimed).await {
            Ok(result) => result,
            Err(e) => TaskExecutionResult {
                status_code: "F",
                task_output: json!({ "error": e.to_string() }),
                next_task: None,
                context_data: None,
            },
        };

        let mut tx = self.pool.begin().await?;
        self.finish_task(&mut tx, &claimed, result).await?;
        tx.commit().await?;

        Ok(true)
    }

    async fn claim_next_task(&self) -> Result<Option<ClaimedTask>, DynError> {
        let mut tx = self.pool.begin().await?;

        let task_res = sqlx::query_as::<_, ActiveTask>(
            r#"
            UPDATE task_info_t
            SET locked = 'Y', update_ts = CURRENT_TIMESTAMP
            WHERE (host_id, task_id) IN (
                SELECT host_id, task_id FROM task_info_t
                WHERE status_code = 'A'
                  AND (
                    locked = 'N'
                    OR (locked = 'Y' AND update_ts < CURRENT_TIMESTAMP - make_interval(mins => $1))
                  )
                  AND task_type IN ('ask', 'assert', 'call', 'set', 'switch')
                ORDER BY priority DESC, started_ts ASC
                LIMIT 1
                FOR UPDATE SKIP LOCKED
            )
            RETURNING host_id, task_id, task_type, process_id, wf_instance_id, wf_task_id
            "#,
        )
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

        let (context_data, wf_def_id) = self
            .get_context_data(&mut tx, &task.host_id, &task.process_id)
            .await?;
        let dsl_yaml = self
            .get_workflow_definition(&mut tx, &task.host_id, &wf_def_id)
            .await?;
        let definition: WorkflowDefinition = serde_yaml::from_str(&dsl_yaml)?;
        let raw_definition: YamlValue = serde_yaml::from_str(&dsl_yaml)?;
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
            info!(
                ">>> Workflow task waiting for input: {} ({})",
                claimed.task.wf_task_id, claimed.task.wf_instance_id
            );
        } else {
            sqlx::query(
                "UPDATE process_info_t SET status_code = 'F', completed_ts = CURRENT_TIMESTAMP WHERE host_id = $1 AND process_id = $2",
            )
            .bind(claimed.task.host_id)
            .bind(claimed.task.process_id)
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

                sqlx::query(
                    r#"
                    INSERT INTO task_info_t (
                        host_id, task_id, task_type, process_id, wf_instance_id,
                        wf_task_id, status_code, started_ts, locked, priority, task_input
                    )
                    VALUES ($1, $2, $3, $4, $5, $6, $7, CURRENT_TIMESTAMP, $8, $9, $10)
                    "#,
                )
                .bind(task.host_id)
                .bind(new_task_id)
                .bind(next_type)
                .bind(task.process_id)
                .bind(&task.wf_instance_id)
                .bind(&next_name)
                .bind("A")
                .bind("N")
                .bind(1)
                .bind(&new_context)
                .execute(&mut **tx)
                .await?;

                info!(
                    ">>> Transitioned to Next Task: {} ({})",
                    next_name, next_type
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
    ) -> Result<(Value, Uuid), sqlx::Error> {
        let row: (Value, Uuid) = sqlx::query_as(
            "SELECT context_data, wf_def_id FROM process_info_t WHERE host_id = $1 AND process_id = $2",
        )
        .bind(host_id)
        .bind(process_id)
        .fetch_one(&mut **tx)
        .await?;
        Ok((row.0, row.1))
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
