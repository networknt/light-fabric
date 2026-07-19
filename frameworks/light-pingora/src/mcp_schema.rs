use crate::mcp::McpToolConfig;
use jsonschema::{PatternOptions, Validator};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Deref;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
use tokio::sync::{Semaphore, oneshot};

pub(crate) const DIALECT_2020_12: &str = "https://json-schema.org/draft/2020-12/schema";
const DEFAULT_MAX_SCHEMA_BYTES: usize = 1_048_576;
const DEFAULT_MAX_SCHEMA_DEPTH: usize = 64;
const DEFAULT_MAX_SUBSCHEMAS: usize = 4_096;
const DEFAULT_MAX_CONCURRENT_VALIDATIONS: usize = 32;
const DEFAULT_VALIDATION_WATCHDOG_MS: u64 = 50;
const REGEX_SIZE_LIMIT_BYTES: usize = 1_048_576;
const REGEX_DFA_SIZE_LIMIT_BYTES: usize = 1_048_576;
const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpSchemaConfig {
    #[serde(default = "default_dialect")]
    pub default_dialect: String,
    #[serde(default)]
    pub allow_external_refs: bool,
    #[serde(default = "default_max_schema_bytes")]
    pub max_schema_bytes: usize,
    #[serde(default = "default_max_schema_depth")]
    pub max_depth: usize,
    #[serde(default = "default_max_subschemas")]
    pub max_subschemas: usize,
    #[serde(default = "default_max_concurrent_validations")]
    pub max_concurrent_validations: usize,
    #[serde(
        default = "default_validation_watchdog_ms",
        alias = "validationTimeoutMs"
    )]
    pub validation_watchdog_ms: u64,
}

impl Default for McpSchemaConfig {
    fn default() -> Self {
        Self {
            default_dialect: default_dialect(),
            allow_external_refs: false,
            max_schema_bytes: default_max_schema_bytes(),
            max_depth: default_max_schema_depth(),
            max_subschemas: default_max_subschemas(),
            max_concurrent_validations: default_max_concurrent_validations(),
            validation_watchdog_ms: default_validation_watchdog_ms(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HeaderValueKind {
    String,
    Integer,
    Boolean,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HeaderExtraction {
    pub header_name: String,
    pub property_path: Vec<String>,
    pub value_kind: HeaderValueKind,
}

#[derive(Clone)]
pub(crate) struct PreparedMcpTool {
    pub config: McpToolConfig,
    pub input_validator: Arc<Validator>,
    pub output_validator: Option<Arc<Validator>>,
    pub header_extractions: Arc<[HeaderExtraction]>,
}

impl std::fmt::Debug for PreparedMcpTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedMcpTool")
            .field("name", &self.config.name)
            .field("has_output_validator", &self.output_validator.is_some())
            .field("header_extractions", &self.header_extractions)
            .finish()
    }
}

impl Deref for PreparedMcpTool {
    type Target = McpToolConfig;

    fn deref(&self) -> &Self::Target {
        &self.config
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SchemaDiagnostic {
    pub path: String,
    pub constraint: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ValidationOutcome {
    Valid,
    Invalid(Vec<SchemaDiagnostic>),
    Overloaded,
    WorkerFailed,
}

struct ValidationJob {
    validator: Arc<Validator>,
    instance: JsonValue,
    response: oneshot::Sender<ValidationOutcome>,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

#[derive(Clone)]
pub(crate) struct SchemaValidationPool {
    sender: mpsc::SyncSender<ValidationJob>,
    admission: Arc<Semaphore>,
}

impl SchemaValidationPool {
    pub fn new(config: &McpSchemaConfig) -> Result<Self, String> {
        if config.max_concurrent_validations == 0 {
            return Err("schema.maxConcurrentValidations must be greater than 0".to_string());
        }
        let worker_count = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
            .min(config.max_concurrent_validations)
            .max(1);
        let (sender, receiver) =
            mpsc::sync_channel::<ValidationJob>(config.max_concurrent_validations);
        let receiver = Arc::new(Mutex::new(receiver));
        let watchdog = Duration::from_millis(config.validation_watchdog_ms);
        for index in 0..worker_count {
            let receiver = Arc::clone(&receiver);
            std::thread::Builder::new()
                .name(format!("mcp-schema-{index}"))
                .spawn(move || validation_worker(receiver, watchdog))
                .map_err(|error| format!("failed to start MCP schema worker: {error}"))?;
        }
        Ok(Self {
            sender,
            admission: Arc::new(Semaphore::new(config.max_concurrent_validations)),
        })
    }

    pub async fn validate(
        &self,
        validator: Arc<Validator>,
        instance: JsonValue,
    ) -> ValidationOutcome {
        let Ok(permit) = Arc::clone(&self.admission).try_acquire_owned() else {
            return ValidationOutcome::Overloaded;
        };
        let (response, result) = oneshot::channel();
        let job = ValidationJob {
            validator,
            instance,
            response,
            _permit: permit,
        };
        if self.sender.try_send(job).is_err() {
            return ValidationOutcome::Overloaded;
        }
        result.await.unwrap_or(ValidationOutcome::WorkerFailed)
    }
}

fn validation_worker(receiver: Arc<Mutex<mpsc::Receiver<ValidationJob>>>, watchdog: Duration) {
    loop {
        let job = {
            let Ok(receiver) = receiver.lock() else {
                return;
            };
            match receiver.recv() {
                Ok(job) => job,
                Err(_) => return,
            }
        };
        let ValidationJob {
            validator,
            instance,
            response,
            _permit,
        } = job;
        let started = Instant::now();
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let diagnostics = validator
                .iter_errors(&instance)
                .take(3)
                .map(|error| SchemaDiagnostic {
                    path: bounded(error.instance_path().to_string(), 256),
                    constraint: bounded(
                        error
                            .schema_path()
                            .to_string()
                            .rsplit('/')
                            .next()
                            .filter(|value| !value.is_empty())
                            .unwrap_or("schema")
                            .to_string(),
                        64,
                    ),
                })
                .collect::<Vec<_>>();
            if diagnostics.is_empty() {
                ValidationOutcome::Valid
            } else {
                ValidationOutcome::Invalid(diagnostics)
            }
        }))
        .unwrap_or(ValidationOutcome::WorkerFailed);
        drop(_permit);
        if started.elapsed() > watchdog {
            tracing::warn!(
                target: "light_pingora::mcp",
                elapsed_ms = started.elapsed().as_millis(),
                watchdog_ms = watchdog.as_millis(),
                "MCP schema validation exceeded observational watchdog"
            );
        }
        let _ = response.send(outcome);
    }
}

pub(crate) fn prepare_tools(
    tools: &[McpToolConfig],
    schema_config: &McpSchemaConfig,
    enforce_stateless_names: bool,
) -> Result<BTreeMap<String, PreparedMcpTool>, String> {
    validate_schema_config(schema_config)?;
    let mut prepared = BTreeMap::new();
    for tool in tools {
        if enforce_stateless_names {
            validate_stateless_tool_name(&tool.name)?;
        }
        let input_validator = compile_schema(
            &tool.input_schema,
            schema_config,
            SchemaRoot::InputObject,
            &tool.name,
        )?;
        let output_validator = tool
            .output_schema
            .as_ref()
            .map(|schema| {
                compile_schema(schema, schema_config, SchemaRoot::Any, &tool.name).map(Arc::new)
            })
            .transpose()?;
        let header_extractions = prepare_header_extractions(&tool.input_schema, &tool.name)?;
        let value = PreparedMcpTool {
            config: tool.clone(),
            input_validator: Arc::new(input_validator),
            output_validator,
            header_extractions: header_extractions.into(),
        };
        if prepared.insert(tool.name.clone(), value).is_some() {
            return Err(format!("duplicate mcp-router tool `{}`", tool.name));
        }
    }
    Ok(prepared)
}

enum SchemaRoot {
    InputObject,
    Any,
}

fn compile_schema(
    schema: &JsonValue,
    config: &McpSchemaConfig,
    root: SchemaRoot,
    tool_name: &str,
) -> Result<Validator, String> {
    preflight_schema(schema, config, tool_name)?;
    if matches!(root, SchemaRoot::InputObject)
        && !has_object_root(schema, schema, &mut BTreeSet::new())
    {
        return Err(format!(
            "mcp-router tool `{tool_name}` inputSchema must have object root type"
        ));
    }
    jsonschema::draft202012::options()
        .with_pattern_options(
            PatternOptions::regex()
                .size_limit(REGEX_SIZE_LIMIT_BYTES)
                .dfa_size_limit(REGEX_DFA_SIZE_LIMIT_BYTES),
        )
        .build(schema)
        .map_err(|error| {
            format!("mcp-router tool `{tool_name}` schema compilation failed: {error}")
        })
}

fn has_object_root(
    root: &JsonValue,
    schema: &JsonValue,
    active_refs: &mut BTreeSet<String>,
) -> bool {
    if schema.get("type").and_then(JsonValue::as_str) == Some("object") {
        return true;
    }
    let Some(reference) = schema.get("$ref").and_then(JsonValue::as_str) else {
        return false;
    };
    if !active_refs.insert(reference.to_string()) {
        return false;
    }
    let result = resolve_local_reference(root, reference)
        .is_some_and(|target| has_object_root(root, target, active_refs));
    active_refs.remove(reference);
    result
}

fn preflight_schema(
    schema: &JsonValue,
    config: &McpSchemaConfig,
    tool_name: &str,
) -> Result<(), String> {
    let bytes = serde_json::to_vec(schema)
        .map_err(|error| format!("schema serialization failed: {error}"))?
        .len();
    if bytes > config.max_schema_bytes {
        return Err(format!(
            "mcp-router tool `{tool_name}` schema exceeds maxSchemaBytes"
        ));
    }
    if let Some(dialect) = schema.get("$schema").and_then(JsonValue::as_str)
        && dialect != config.default_dialect
    {
        return Err(format!(
            "mcp-router tool `{tool_name}` uses unsupported JSON Schema dialect `{dialect}`"
        ));
    }
    let mut count = 0_usize;
    let mut pending = vec![(schema, 0_usize)];
    while let Some((value, depth)) = pending.pop() {
        if depth > config.max_depth {
            return Err(format!(
                "mcp-router tool `{tool_name}` schema exceeds maxDepth"
            ));
        }
        match value {
            JsonValue::Object(object) => {
                count += 1;
                if count > config.max_subschemas {
                    return Err(format!(
                        "mcp-router tool `{tool_name}` schema exceeds maxSubschemas"
                    ));
                }
                for keyword in ["$ref", "$dynamicRef"] {
                    if let Some(reference) = object.get(keyword).and_then(JsonValue::as_str)
                        && !reference.starts_with('#')
                    {
                        return Err(format!(
                            "mcp-router tool `{tool_name}` external JSON Schema reference is disabled"
                        ));
                    }
                }
                pending.extend(object.values().map(|child| (child, depth + 1)));
            }
            JsonValue::Array(values) => {
                pending.extend(values.iter().map(|child| (child, depth + 1)));
            }
            _ => {}
        }
    }
    Ok(())
}

fn prepare_header_extractions(
    schema: &JsonValue,
    tool_name: &str,
) -> Result<Vec<HeaderExtraction>, String> {
    let total_annotations = count_key(schema, "x-mcp-header");
    let mut plan = Vec::new();
    let mut names = BTreeSet::new();
    collect_property_headers(
        schema,
        schema,
        &mut Vec::new(),
        &mut BTreeSet::new(),
        &mut names,
        &mut plan,
        tool_name,
    )?;
    if plan.len() != total_annotations {
        return Err(format!(
            "mcp-router tool `{tool_name}` x-mcp-header must annotate a statically reachable property"
        ));
    }
    Ok(plan)
}

fn collect_property_headers(
    root: &JsonValue,
    schema: &JsonValue,
    path: &mut Vec<String>,
    active_refs: &mut BTreeSet<String>,
    names: &mut BTreeSet<String>,
    plan: &mut Vec<HeaderExtraction>,
    tool_name: &str,
) -> Result<(), String> {
    if let Some(reference) = schema.get("$ref").and_then(JsonValue::as_str)
        && active_refs.insert(reference.to_string())
    {
        let target = resolve_local_reference(root, reference).ok_or_else(|| {
            format!("mcp-router tool `{tool_name}` has unresolved local header schema reference")
        })?;
        collect_property_headers(root, target, path, active_refs, names, plan, tool_name)?;
        active_refs.remove(reference);
    }
    let Some(properties) = schema.get("properties").and_then(JsonValue::as_object) else {
        return Ok(());
    };
    for (property, property_schema) in properties {
        path.push(property.clone());
        if let Some(header) = property_schema.get("x-mcp-header") {
            let header = header
                .as_str()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    format!("mcp-router tool `{tool_name}` x-mcp-header must be a non-empty string")
                })?;
            if !is_http_field_name_token(header) {
                return Err(format!(
                    "mcp-router tool `{tool_name}` x-mcp-header `{header}` is not an HTTP token"
                ));
            }
            if is_protected_generated_header(header) {
                return Err(format!(
                    "mcp-router tool `{tool_name}` x-mcp-header `{header}` is gateway-owned or unsafe"
                ));
            }
            if !names.insert(header.to_ascii_lowercase()) {
                return Err(format!(
                    "mcp-router tool `{tool_name}` has duplicate x-mcp-header `{header}`"
                ));
            }
            if property_schema
                .get("x-mask")
                .and_then(JsonValue::as_bool)
                .unwrap_or(false)
                || property_schema
                    .get("x-sensitive")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(false)
            {
                return Err(format!(
                    "mcp-router tool `{tool_name}` x-mcp-header cannot expose a sensitive property"
                ));
            }
            let value_kind = match property_schema.get("type").and_then(JsonValue::as_str) {
                Some("string") => HeaderValueKind::String,
                Some("integer") => HeaderValueKind::Integer,
                Some("boolean") => HeaderValueKind::Boolean,
                Some("number") => {
                    return Err(format!(
                        "mcp-router tool `{tool_name}` x-mcp-header cannot annotate number"
                    ));
                }
                _ => {
                    return Err(format!(
                        "mcp-router tool `{tool_name}` x-mcp-header requires string, integer, or boolean"
                    ));
                }
            };
            if value_kind == HeaderValueKind::Integer {
                let minimum = property_schema.get("minimum").and_then(JsonValue::as_i64);
                let maximum = property_schema.get("maximum").and_then(JsonValue::as_i64);
                if minimum.is_none_or(|value| value < -MAX_SAFE_INTEGER)
                    || maximum.is_none_or(|value| value > MAX_SAFE_INTEGER)
                {
                    return Err(format!(
                        "mcp-router tool `{tool_name}` x-mcp-header integer requires a safe minimum and maximum"
                    ));
                }
            }
            plan.push(HeaderExtraction {
                header_name: header.to_string(),
                property_path: path.clone(),
                value_kind,
            });
        }
        collect_property_headers(
            root,
            property_schema,
            path,
            active_refs,
            names,
            plan,
            tool_name,
        )?;
        path.pop();
    }
    Ok(())
}

fn resolve_local_reference<'a>(root: &'a JsonValue, reference: &str) -> Option<&'a JsonValue> {
    let pointer = reference.strip_prefix('#')?;
    if pointer.is_empty() {
        Some(root)
    } else if pointer.starts_with('/') {
        root.pointer(pointer)
    } else {
        None
    }
}

fn count_key(value: &JsonValue, key: &str) -> usize {
    match value {
        JsonValue::Object(object) => {
            usize::from(object.contains_key(key))
                + object
                    .values()
                    .map(|value| count_key(value, key))
                    .sum::<usize>()
        }
        JsonValue::Array(values) => values.iter().map(|value| count_key(value, key)).sum(),
        _ => 0,
    }
}

fn is_http_field_name_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

fn is_protected_generated_header(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        "host"
            | "content-length"
            | "transfer-encoding"
            | "connection"
            | "keep-alive"
            | "te"
            | "trailer"
            | "upgrade"
            | "proxy-authorization"
            | "proxy-authenticate"
            | "authorization"
            | "cookie"
            | "set-cookie"
            | "accept-encoding"
            | "mcp-session-id"
            | "mcp-protocol-version"
            | "mcp-method"
            | "mcp-name"
    )
}

fn validate_stateless_tool_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 128
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(format!(
            "stateless MCP tool name `{name}` must be 1-128 ASCII letters, digits, dot, hyphen, or underscore"
        ));
    }
    Ok(())
}

fn validate_schema_config(config: &McpSchemaConfig) -> Result<(), String> {
    if config.default_dialect != DIALECT_2020_12 {
        return Err(format!(
            "unsupported default JSON Schema dialect `{}`",
            config.default_dialect
        ));
    }
    if config.allow_external_refs {
        return Err("schema.allowExternalRefs must remain false".to_string());
    }
    if config.validation_watchdog_ms == 0 {
        return Err("schema.validationWatchdogMs must be greater than 0".to_string());
    }
    for (name, value) in [
        ("maxSchemaBytes", config.max_schema_bytes),
        ("maxDepth", config.max_depth),
        ("maxSubschemas", config.max_subschemas),
        (
            "maxConcurrentValidations",
            config.max_concurrent_validations,
        ),
    ] {
        if value == 0 {
            return Err(format!("schema.{name} must be greater than 0"));
        }
    }
    Ok(())
}

fn bounded(mut value: String, max: usize) -> String {
    if value.len() <= max {
        return value;
    }
    let mut boundary = max;
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    value
}

fn default_dialect() -> String {
    DIALECT_2020_12.to_string()
}
fn default_max_schema_bytes() -> usize {
    DEFAULT_MAX_SCHEMA_BYTES
}
fn default_max_schema_depth() -> usize {
    DEFAULT_MAX_SCHEMA_DEPTH
}
fn default_max_subschemas() -> usize {
    DEFAULT_MAX_SUBSCHEMAS
}
fn default_max_concurrent_validations() -> usize {
    DEFAULT_MAX_CONCURRENT_VALIDATIONS
}
fn default_validation_watchdog_ms() -> u64 {
    DEFAULT_VALIDATION_WATCHDOG_MS
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(input: JsonValue, output: Option<JsonValue>) -> McpToolConfig {
        McpToolConfig {
            name: "test.tool".to_string(),
            endpoint_name: None,
            description: String::new(),
            protocol: None,
            service_id: None,
            env_tag: None,
            target_host: Some("https://example.com".to_string()),
            path: "/tool".to_string(),
            method: Default::default(),
            endpoint: None,
            api_type: Default::default(),
            backend_mcp_protocol: None,
            session_independent: false,
            backend_credential_mode: None,
            backend_resource: None,
            input_schema: input,
            output_schema: output,
            input_schema_configured: true,
            tool_metadata: json!({}),
        }
    }

    #[test]
    fn bounded_compile_supports_local_refs_and_arbitrary_output_roots() {
        let prepared = prepare_tools(
            &[tool(
                json!({
                    "$ref":"#/$defs/request",
                    "$defs":{
                        "id":{"type":"string"},
                        "request":{"type":"object","properties":{"id":{"$ref":"#/$defs/id"}}}
                    }
                }),
                Some(json!({"type":["array", "null"]})),
            )],
            &McpSchemaConfig::default(),
            true,
        )
        .expect("prepare");
        let tool = prepared.get("test.tool").expect("tool");
        assert!(tool.input_validator.is_valid(&json!({"id":"a"})));
        assert!(!tool.input_validator.is_valid(&json!({"id":1})));
        assert!(
            tool.output_validator
                .as_ref()
                .expect("output")
                .is_valid(&json!([]))
        );
        assert!(
            tool.output_validator
                .as_ref()
                .expect("output")
                .is_valid(&JsonValue::Null)
        );
    }

    #[test]
    fn invalid_schema_limits_dialects_roots_and_external_refs_fail_closed() {
        let cases = [
            json!({"type":"string"}),
            json!({"$schema":"http://json-schema.org/draft-07/schema#","type":"object"}),
            json!({"type":"object","properties":{"x":{"$ref":"https://example.com/x"}}}),
        ];
        for input in cases {
            assert!(
                prepare_tools(&[tool(input, None)], &McpSchemaConfig::default(), false).is_err()
            );
        }
    }

    #[test]
    fn header_plan_is_typed_unique_reachable_and_non_sensitive() {
        let prepared = prepare_tools(
            &[tool(
                json!({
                    "type":"object",
                    "$defs":{"requestId":{"type":"object","properties":{
                        "id":{"type":"string","x-mcp-header":"Mcp-Param-Request-Id"}
                    }}},
                    "properties":{
                        "region":{"type":"string","x-mcp-header":"Mcp-Param-Region"},
                        "request":{"$ref":"#/$defs/requestId"},
                        "nested":{"type":"object","properties":{
                            "active":{"type":"boolean","x-mcp-header":"Mcp-Param-Active"}
                        }}
                    }
                }),
                None,
            )],
            &McpSchemaConfig::default(),
            true,
        )
        .expect("prepare");
        let plan = &prepared["test.tool"].header_extractions;
        assert_eq!(plan.len(), 3);
        assert!(
            plan.iter()
                .any(|entry| entry.property_path == ["nested", "active"])
        );
        assert!(
            plan.iter()
                .any(|entry| entry.property_path == ["request", "id"])
        );

        for invalid in [
            json!({"type":"object","x-mcp-header":"Bad"}),
            json!({"type":"object","properties":{"x":{"type":"number","x-mcp-header":"X"}}}),
            json!({"type":"object","properties":{"x":{"type":"string","x-mask":true,"x-mcp-header":"X"}}}),
            json!({"type":"object","properties":{"x":{"type":"string","x-mcp-header":"Same"},"y":{"type":"string","x-mcp-header":"same"}}}),
            json!({"type":"object","properties":{"x":{"type":"integer","x-mcp-header":"X"}}}),
            json!({"type":"object","properties":{"x":{"type":"string","x-mcp-header":"Connection"}}}),
            json!({"type":"object","properties":{"x":{"type":"string","x-mcp-header":"mcp-session-id"}}}),
            json!({"type":"object","properties":{"x":{"type":"string","x-mcp-header":":path"}}}),
        ] {
            assert!(
                prepare_tools(&[tool(invalid, None)], &McpSchemaConfig::default(), false).is_err()
            );
        }
    }

    #[test]
    fn adversarial_schema_and_header_corpus_never_panics() {
        let mut corpus = vec![
            JsonValue::Null,
            json!(true),
            json!({"type":"object","properties":{"\u{1f600}":{"type":"string","x-mcp-header":"X-Emoji"}}}),
            json!({"type":"object","$defs":{"loop":{"$ref":"#/$defs/loop"}}}),
            json!({"type":"object","properties":{"x":{"type":"integer","minimum":-9007199254740991_i64,"maximum":9007199254740991_i64,"x-mcp-header":"X-Integer"}}}),
        ];
        let mut nested = json!({"type":"string"});
        for _ in 0..70 {
            nested = json!({"type":"object","properties":{"next":nested}});
        }
        corpus.push(nested);
        for schema in corpus {
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                prepare_tools(&[tool(schema, None)], &McpSchemaConfig::default(), false)
            }));
            assert!(outcome.is_ok());
        }
    }

    #[tokio::test]
    async fn dedicated_pool_returns_bounded_diagnostics_and_releases_permits() {
        let config = McpSchemaConfig {
            max_concurrent_validations: 1,
            ..McpSchemaConfig::default()
        };
        let pool = SchemaValidationPool::new(&config).expect("pool");
        let validator = Arc::new(
            jsonschema::draft202012::new(&json!({
                "type":"object",
                "required":["name"],
                "properties":{"name":{"type":"string"}}
            }))
            .expect("validator"),
        );
        for _ in 0..2 {
            let outcome = pool.validate(Arc::clone(&validator), json!({})).await;
            let ValidationOutcome::Invalid(diagnostics) = &outcome else {
                panic!("expected invalid outcome, got {outcome:?}")
            };
            assert!(!diagnostics.is_empty());
            assert!(diagnostics.len() <= 3);
            assert!(diagnostics.iter().all(|item| item.path.len() <= 256));
        }
    }
}
