//! W5: prepare authored data once, before dispatch. Credentials never enter this module.
use super::*;
use expression_runtime::{Failure, batch, failure};
use workflow_expression::{Category, Position, StepField};

pub(super) struct RpcRequest<'a> {
    pub uri: &'a str,
    pub method: &'a str,
    pub params: Option<&'a Value>,
    pub id: Option<&'a Value>,
    pub notification: bool,
    pub headers: Option<&'a Value>,
    pub request_timeout: Option<&'a OneOfDurationOrIso8601Expression>,
    pub output: Option<&'a str>,
    pub error_policy: Option<&'a workflow_core::models::task::JsonRpcErrorPolicy>,
}

pub(super) struct OpenRpcTarget {
    pub configured: String,
    pub template: String,
    pub span_offsets: Vec<usize>,
}
impl OpenRpcTarget {
    fn literal(uri: &str) -> Self {
        Self {
            configured: uri.into(),
            template: uri.into(),
            span_offsets: Vec::new(),
        }
    }
    pub(super) fn original_error(&self, mut error: Failure) -> Failure {
        if let workflow_expression::WorkerError::Expression(e) = &mut error.error
            && let Some(offset) = e.span_index.and_then(|i| self.span_offsets.get(i))
        {
            e.offset = Some(*offset);
        }
        error
    }
}

fn add(fields: &mut Vec<StepField>, task: &Value, path: &str, position: Position) {
    if let Some(value) = task.pointer(path) {
        fields.push(StepField {
            path: path.into(),
            position,
            template: value.clone(),
            value_from: None,
        });
    }
}
fn endpoint(fields: &mut Vec<StepField>, task: &Value, path: &str, position: Position) {
    let path = if task.pointer(path).is_some_and(Value::is_object) {
        format!("{path}/uri")
    } else {
        path.into()
    };
    add(fields, task, &path, position);
}
pub(super) fn fields(task: &Value) -> Vec<StepField> {
    let mut result = Vec::new();
    add(
        &mut result,
        task,
        "/idempotencyKey",
        Position::IdempotencyKey,
    );
    match task.get("call").and_then(Value::as_str) {
        Some("http") => {
            add(&mut result, task, "/with/body", Position::HttpBody);
            for (name, position) in [
                ("query", Position::HttpQuery),
                ("headers", Position::HttpHeader),
            ] {
                if let Some(map) = task
                    .pointer(&format!("/with/{name}"))
                    .and_then(Value::as_object)
                {
                    for key in map.keys() {
                        add(
                            &mut result,
                            task,
                            &format!("/with/{name}/{}", workflow_expression::escape(key)),
                            position,
                        );
                    }
                }
            }
        }
        Some("jsonrpc" | "openrpc") => {
            add(&mut result, task, "/with/params", Position::JsonRpcParams);
            add(&mut result, task, "/with/headers", Position::JsonRpcHeader);
            if task["call"] == "jsonrpc" {
                endpoint(&mut result, task, "/with/endpoint", Position::JsonRpcUri);
            } else {
                endpoint(
                    &mut result,
                    task,
                    "/with/document/endpoint",
                    Position::OpenRpcEndpoint,
                );
                // A server name remains a selector. An expression or literal URI is a String site.
                if task
                    .pointer("/with/server")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s.contains("://") || s.contains("${"))
                {
                    add(&mut result, task, "/with/server", Position::JsonRpcUri);
                }
                add(&mut result, task, "/with/server/url", Position::JsonRpcUri);
                endpoint(
                    &mut result,
                    task,
                    "/with/server/endpoint",
                    Position::JsonRpcUri,
                );
            }
        }
        Some("mcp") => {
            add(&mut result, task, "/with/parameters", Position::McpParams);
            add(&mut result, task, "/with/arguments", Position::McpParams);
            add(
                &mut result,
                task,
                "/with/resource",
                Position::McpResourceUri,
            );
            endpoint(
                &mut result,
                task,
                "/with/server/endpoint",
                Position::JsonRpcUri,
            );
            endpoint(
                &mut result,
                task,
                "/with/transport/http/endpoint",
                Position::JsonRpcUri,
            );
            add(
                &mut result,
                task,
                "/with/transport/http/headers",
                Position::JsonRpcHeader,
            );
        }
        Some("a2a") => add(
            &mut result,
            task,
            "/with/parameters",
            Position::A2aParameters,
        ),
        Some("agent") => {
            for (name, position) in [
                ("input", Position::AgentInput),
                ("mockOutput", Position::AgentMockOutput),
                ("instructions", Position::AgentInstructions),
                ("prompt", Position::AgentPrompt),
            ] {
                add(&mut result, task, &format!("/with/{name}"), position);
            }
        }
        _ => {}
    }
    for (name, position) in [
        ("categoryCode", Position::AskCategory),
        ("reasonCode", Position::AskReason),
        ("assigneeId", Position::AskAssignee),
        ("roleId", Position::AskRole),
    ] {
        add(
            &mut result,
            task,
            &format!("/ask/assignment/{name}"),
            position,
        );
    }
    result.sort_unstable_by(|a, b| a.path.cmp(&b.path));
    result
}
pub(super) fn supported(task: &TaskDefinition) -> bool {
    matches!(
        task,
        TaskDefinition::Ask(_)
            | TaskDefinition::Call(
                CallTaskDefinition::Http(_)
                    | CallTaskDefinition::JsonRpc(_)
                    | CallTaskDefinition::OpenRpc(_)
                    | CallTaskDefinition::Mcp(_)
                    | CallTaskDefinition::A2a(_)
                    | CallTaskDefinition::Agent(_)
            )
    )
}
impl TaskExecutor {
    pub(super) fn w5_mocked(&self) -> bool {
        #[cfg(test)]
        return self.w5_mock.is_some();
        #[cfg(not(test))]
        false
    }
    pub(super) async fn w5_invocation(
        &self,
        claimed: &ClaimedTask,
    ) -> Result<Option<(Option<String>, String, Option<String>)>, sqlx::Error> {
        #[cfg(test)]
        if self.w5_mock.is_some() {
            return Ok(None);
        }
        sqlx::query_as("SELECT user_authorization,state,response_policy_snapshot->>'acceptedAdmissionProfile' FROM workflow_invocation_t WHERE host_id=$1 AND process_id=$2")
            .bind(claimed.task.host_id).bind(claimed.task.process_id).fetch_optional(&self.pool).await
    }
    pub(super) async fn w5_admission(
        &self,
        claimed: &ClaimedTask,
    ) -> Result<InvocationAdmissionProfile, sqlx::Error> {
        #[cfg(test)]
        if self.w5_mock.is_some() {
            return Ok(InvocationAdmissionProfile::PortalExecution);
        }
        self.admission_profile_for_process(claimed.task.host_id, claimed.task.process_id)
            .await
    }
    pub(super) async fn w5_catalog(
        &self,
        host: &Uuid,
        args: &AgentArguments,
    ) -> Result<AgentCatalog, DynError> {
        #[cfg(test)]
        if self.w5_mock.is_some() {
            return Ok(AgentCatalog {
                agent: AgentDefinitionRecord {
                    agent_def_id: Uuid::nil(),
                    agent_name: None,
                    model_provider: "mock".into(),
                    model_name: "mock".into(),
                    api_key_ref: None,
                    temperature: 0.0,
                    max_tokens: None,
                    aggregate_version: 1,
                },
                skills: vec![],
                tools: vec![],
            });
        }
        self.load_agent_catalog(host, &args.agent, args.skill.as_deref())
            .await
    }
    pub(super) async fn w5_prepare(
        &self,
        claimed: &ClaimedTask,
        task: &TaskDefinition,
    ) -> Result<TaskDefinition, Failure> {
        let mut authored =
            serde_json::to_value(task).map_err(|_| failure(Category::Invalid, "/task"))?;
        // Resolve referenced MCP endpoint configuration before the single unchanged-envelope batch.
        if let TaskDefinition::Call(CallTaskDefinition::Mcp(call)) = task {
            let server = self
                .resolve_mcp_server(&call.with, &claimed.definition)
                .map_err(|_| failure(Category::Invalid, "/with/server"))?;
            authored["with"]["server"] = serde_json::to_value(server)
                .map_err(|_| failure(Category::Invalid, "/with/server"))?;
        }
        // Fixed authored OpenRPC variables belong to original literal chunks,
        // before the batch. Prepared CEL results must never be substituted again.
        let authored_target = if authored.get("call").and_then(Value::as_str) == Some("openrpc")
            && authored.pointer("/with/server/url").is_some()
        {
            let target =
                self.w5_openrpc_target(&json!({}), authored.pointer("/with/server"), false)?;
            authored["with"]["server"]["url"] = json!(target.template);
            Some(target)
        } else {
            None
        };
        let fields = fields(&authored);
        let values = batch(
            self.expression_engine.as_ref(),
            &claimed.context_data,
            &claimed.input_data,
            None,
            None,
            fields.clone(),
            false,
        )
        .await
        .map_err(|e| {
            if e.field == "/with/server/url" {
                if let Some(target) = &authored_target {
                    target.original_error(e)
                } else {
                    e
                }
            } else {
                e
            }
        })?;
        for (field, value) in fields.into_iter().zip(values) {
            if matches!(
                field.position,
                Position::JsonRpcUri | Position::OpenRpcEndpoint
            ) {
                self.validate_resolved_uri(
                    field
                        .template
                        .as_str()
                        .ok_or_else(|| failure(Category::ResultType, &field.path))?,
                    value
                        .as_str()
                        .ok_or_else(|| failure(Category::ResultType, &field.path))?,
                )
                .map_err(|_| failure(Category::ResultType, &field.path))?;
            }
            *authored
                .pointer_mut(&field.path)
                .ok_or_else(|| failure(Category::Invalid, &field.path))? = value;
        }
        if authored
            .get("idempotencyKey")
            .and_then(Value::as_str)
            .is_some_and(|key| key.trim().is_empty() || key.len() > 255)
        {
            return Err(failure(Category::ResultType, "/idempotencyKey"));
        }
        // Header syntax is checked before effect ledgers, token exchange or transports.
        for path in ["/with/headers", "/with/transport/http/headers"] {
            if let Some(headers) = authored.pointer(path).and_then(Value::as_object) {
                for (key, value) in headers {
                    reqwest::header::HeaderName::from_bytes(key.as_bytes())
                        .map_err(|_| failure(Category::ResultType, path))?;
                    reqwest::header::HeaderValue::from_str(
                        value
                            .as_str()
                            .ok_or_else(|| failure(Category::ResultType, path))?,
                    )
                    .map_err(|_| failure(Category::ResultType, path))?;
                }
            }
        }
        serde_json::from_value(authored).map_err(|_| failure(Category::ResultType, "/task"))
    }
    pub(super) async fn w5_uri(
        &self,
        claimed: &ClaimedTask,
        uri: String,
        path: &str,
    ) -> Result<String, Failure> {
        let value = batch(
            self.expression_engine.as_ref(),
            &claimed.context_data,
            &claimed.input_data,
            None,
            None,
            vec![StepField {
                path: path.into(),
                position: Position::JsonRpcUri,
                template: json!(uri),
                value_from: None,
            }],
            false,
        )
        .await?
        .remove(0);
        value
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| failure(Category::ResultType, path))
    }
    pub(super) fn w5_openrpc_target(
        &self,
        document: &Value,
        selector: Option<&Value>,
        prepared: bool,
    ) -> Result<OpenRpcTarget, Failure> {
        let selected = if let Some(selector) = selector {
            if let Some(name) = selector.as_str() {
                if name.starts_with("http://") || name.starts_with("https://") {
                    return Ok(OpenRpcTarget::literal(name));
                }
                self.find_openrpc_server_by_name(document, name)
            } else if selector.get("url").is_some() || selector.get("endpoint").is_some() {
                Some(selector)
            } else {
                selector
                    .get("name")
                    .and_then(Value::as_str)
                    .and_then(|name| self.find_openrpc_server_by_name(document, name))
            }
        } else {
            document
                .get("servers")
                .and_then(Value::as_array)
                .and_then(|servers| servers.first())
        };
        let selected = selected.ok_or_else(|| failure(Category::ResultType, "/with/server"))?;
        if let Some(endpoint) = selected.get("endpoint") {
            let uri = endpoint
                .as_str()
                .or_else(|| endpoint.get("uri").and_then(Value::as_str))
                .ok_or_else(|| failure(Category::ResultType, "/with/server/endpoint"))?;
            return Ok(OpenRpcTarget::literal(uri));
        }
        let url = selected
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| failure(Category::ResultType, "/with/server/url"))?;
        if prepared {
            return Ok(OpenRpcTarget::literal(url));
        }
        let mut variables = BTreeMap::new();
        if let Some(defaults) = selected.get("variables").and_then(Value::as_object) {
            for (key, value) in defaults {
                if let Some(value) = value.get("default").and_then(Value::as_str) {
                    variables.insert(key.clone(), value.to_owned());
                }
            }
        }
        if let Some(overrides) = selector
            .and_then(|v| v.get("variables"))
            .and_then(Value::as_object)
        {
            for (key, value) in overrides {
                variables.insert(key.clone(), self.stringify_json_value(value));
            }
        }
        // Only authored document spans are executable. Variable values remain literal
        // data, even when they contain CEL syntax or another variable placeholder.
        let segments = workflow_expression::scan(url).map_err(|e| Failure {
            error: e.into(),
            field: "/with/server/url".into(),
        })?;
        let placeholders = regex::Regex::new(r"\{([^{}]*)\}").unwrap();
        let mut span_offsets = Vec::new();
        let mut configured = String::new();
        let mut template = String::new();
        for segment in segments {
            match segment {
                workflow_expression::Segment::Span {
                    offset,
                    source_offset,
                    end,
                    ..
                } => {
                    span_offsets.push(source_offset);
                    configured.push_str(&url[offset..end]);
                    template.push_str(&url[offset..end]);
                }
                workflow_expression::Segment::Literal { text, .. } => {
                    let literal =
                        placeholders.replace_all(&text, |captures: &regex::Captures<'_>| {
                            let start = captures.get(0).unwrap().start();
                            if text[..start].ends_with('$') {
                                return captures[0].to_owned();
                            }
                            variables
                                .get(&captures[1])
                                .cloned()
                                .unwrap_or_else(|| captures[0].into())
                        });
                    configured.push_str(&literal);
                    // Scanner escaping keeps substituted bytes out of CEL source.
                    template.push_str(&literal.replace("${", "$${"));
                }
            }
        }
        Ok(OpenRpcTarget {
            configured,
            template,
            span_offsets,
        })
    }

    pub(super) async fn w5_fence(&self, claimed: &ClaimedTask) -> Result<(), DynError> {
        if claimed.expression_profile != "cel-workflow-v2" {
            return Ok(());
        }
        #[cfg(test)]
        if let Some(mock) = &self.w5_mock {
            return if mock.stale.load(std::sync::atomic::Ordering::SeqCst) {
                Err(sqlx::Error::Protocol("WORKFLOW_STALE_COMPLETION".into()).into())
            } else {
                Ok(())
            };
        }
        let mut tx = self.pool.begin().await?;
        if !self.w4_authority(&mut tx, claimed).await? {
            return Err(sqlx::Error::Protocol("WORKFLOW_STALE_COMPLETION".into()).into());
        }
        tx.rollback().await?;
        Ok(())
    }
    pub(super) async fn w5_send(
        &self,
        claimed: Option<&ClaimedTask>,
        request: reqwest::RequestBuilder,
        fetch: bool,
    ) -> Result<reqwest::Response, DynError> {
        if let Some(claimed) = claimed {
            self.w5_fence(claimed).await?;
        }
        #[cfg(test)]
        if let Some(mock) = &self.w5_mock {
            let request = request.build()?;
            mock.requests.lock().unwrap().push((fetch, request));
            let body = if fetch {
                mock.document.clone()
            } else {
                mock.response.clone()
            };
            return Ok(reqwest::Response::from(
                axum::http::Response::builder()
                    .status(200)
                    .body(body.to_string())
                    .unwrap(),
            ));
        }
        let _ = fetch;
        Ok(request.send().await?)
    }
}
pub(super) fn request_json(
    executor: &TaskExecutor,
    value: &Value,
    context: &Value,
    prepared: bool,
) -> Value {
    if prepared {
        value.clone()
    } else {
        executor.resolve_json_value(value, context)
    }
}
pub(super) fn request_string(
    executor: &TaskExecutor,
    value: &str,
    context: &Value,
    prepared: bool,
) -> String {
    if prepared {
        value.into()
    } else {
        executor.resolve_template_to_string(value, context)
    }
}
pub(super) fn request_error(
    error: DynError,
    claimed: Option<&ClaimedTask>,
    field: &str,
) -> DynError {
    if claimed.is_some() {
        failure(Category::ResultType, field).into()
    } else {
        error
    }
}
#[cfg(test)]
#[derive(Default)]
pub(super) struct Mock {
    pub stale: std::sync::atomic::AtomicBool,
    pub requests: std::sync::Mutex<Vec<(bool, reqwest::Request)>>,
    pub document: Value,
    pub response: Value,
    pub jobs: std::sync::Mutex<Vec<Value>>,
}
