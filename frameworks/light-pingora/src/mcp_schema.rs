use crate::mcp::McpToolConfig;
use jsonschema::{PatternOptions, Validator};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Deref;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};
use tokio::sync::{Semaphore, oneshot};

pub(crate) const DIALECT_2020_12: &str = "https://json-schema.org/draft/2020-12/schema";
const DEFAULT_MAX_SCHEMA_BYTES: usize = 1_048_576;
const DEFAULT_MAX_SCHEMA_DEPTH: usize = 64;
const DEFAULT_MAX_SUBSCHEMAS: usize = 4_096;
const DEFAULT_MAX_COMPOSITION_BRANCHES: usize = 256;
const DEFAULT_MAX_SCHEMA_GRAPH_VISITS: usize = 4_096;
const DEFAULT_MAX_CONCURRENT_VALIDATIONS: usize = 32;
const DEFAULT_VALIDATION_WATCHDOG_MS: u64 = 50;
const REGEX_SIZE_LIMIT_BYTES: usize = 1_048_576;
const REGEX_DFA_SIZE_LIMIT_BYTES: usize = 1_048_576;
const MAX_SAFE_INTEGER: i64 = 9_007_199_254_740_991;
static VALIDATION_WATCHDOG_EXCEEDED: AtomicU64 = AtomicU64::new(0);

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
    #[serde(default = "default_max_composition_branches")]
    pub max_composition_branches: usize,
    #[serde(default = "default_max_schema_graph_visits")]
    pub max_schema_graph_visits: usize,
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
            max_composition_branches: default_max_composition_branches(),
            max_schema_graph_visits: default_max_schema_graph_visits(),
            max_concurrent_validations: default_max_concurrent_validations(),
            validation_watchdog_ms: default_validation_watchdog_ms(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SchemaKind {
    Input,
    Output,
    Configuration,
}

impl SchemaKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Input => "inputSchema",
            Self::Output => "outputSchema",
            Self::Configuration => "configuration",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SchemaPreparationError {
    pub tool_name: String,
    pub schema_kind: SchemaKind,
    pub json_pointer: String,
    pub reason_code: &'static str,
    pub reason: String,
}

impl SchemaPreparationError {
    fn new(
        tool_name: impl Into<String>,
        schema_kind: SchemaKind,
        json_pointer: impl Into<String>,
        reason_code: &'static str,
        reason: impl Into<String>,
    ) -> Self {
        let json_pointer = json_pointer.into();
        Self {
            tool_name: tool_name.into(),
            schema_kind,
            json_pointer: if json_pointer.is_empty() {
                "/".to_string()
            } else {
                json_pointer
            },
            reason_code,
            reason: reason.into(),
        }
    }

    fn configuration(reason_code: &'static str, reason: impl Into<String>) -> Self {
        Self::new(
            "<configuration>",
            SchemaKind::Configuration,
            "/",
            reason_code,
            reason,
        )
    }
}

impl std::fmt::Display for SchemaPreparationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "mcp-router tool `{}` {} at `{}` [{}]: {}",
            self.tool_name,
            self.schema_kind.as_str(),
            self.json_pointer,
            self.reason_code,
            self.reason
        )
    }
}

impl std::error::Error for SchemaPreparationError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
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

#[derive(Debug, Clone)]
pub(crate) enum MaskPathSegment {
    Property(String),
    PropertyPattern(Regex),
    AnyIndex,
    Index(usize),
}

impl MaskPathSegment {
    pub(crate) fn matches_property_name(&self, name: &str) -> bool {
        match self {
            Self::Property(expected) => expected == name,
            Self::PropertyPattern(regex) => regex.is_match(name),
            Self::AnyIndex | Self::Index(_) => false,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct MaskRule {
    pub path: Vec<MaskPathSegment>,
    pub pattern: Option<String>,
}

impl MaskRule {
    fn covers_fixed_property_path(&self, path: &[String]) -> bool {
        self.path.len() <= path.len()
            && self
                .path
                .iter()
                .zip(path)
                .all(|(segment, name)| segment.matches_property_name(name))
    }
}

#[derive(Clone)]
pub(crate) struct PreparedMcpTool {
    pub config: McpToolConfig,
    pub input_validator: Arc<Validator>,
    pub input_schema_for_validation: Arc<JsonValue>,
    pub advertised_input_schema: JsonValue,
    pub output_validator: Option<Arc<Validator>>,
    pub output_schema_for_validation: Option<Arc<JsonValue>>,
    pub advertised_output_schema: Option<JsonValue>,
    pub header_extractions: Arc<[HeaderExtraction]>,
    pub mask_plan: Arc<[MaskRule]>,
    /// Finite top-level argument names reachable through composition and local refs.
    pub routing_properties: Arc<[String]>,
    /// True when a schema-valued wildcard can admit names that cannot be enumerated.
    pub routing_properties_open_ended: bool,
    /// True when schema validation rejects undeclared root arguments.
    pub rejects_unmapped_arguments: bool,
}

impl std::fmt::Debug for PreparedMcpTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedMcpTool")
            .field("name", &self.config.name)
            .field("has_output_validator", &self.output_validator.is_some())
            .field("header_extractions", &self.header_extractions)
            .field("mask_rules", &self.mask_plan.len())
            .field("routing_properties", &self.routing_properties)
            .field(
                "routing_properties_open_ended",
                &self.routing_properties_open_ended,
            )
            .field(
                "rejects_unmapped_arguments",
                &self.rejects_unmapped_arguments,
            )
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
    schema: Option<Arc<JsonValue>>,
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

    pub async fn validate_with_schema(
        &self,
        validator: Arc<Validator>,
        schema: Option<Arc<JsonValue>>,
        instance: JsonValue,
    ) -> ValidationOutcome {
        let Ok(permit) = Arc::clone(&self.admission).try_acquire_owned() else {
            return ValidationOutcome::Overloaded;
        };
        let (response, result) = oneshot::channel();
        let job = ValidationJob {
            validator,
            schema,
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
            schema,
            instance,
            response,
            _permit,
        } = job;
        let started = Instant::now();
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let mut diagnostics = Vec::new();
            let mut error_schema_paths = Vec::new();
            for error in validator.iter_errors(&instance).take(16) {
                error_schema_paths.push(error.schema_path().to_string());
                if diagnostics.len() < 3 {
                    diagnostics.push(SchemaDiagnostic {
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
                    });
                }
            }
            if let Some(schema) = schema.as_deref() {
                prepend_composition_diagnostic(
                    schema,
                    &instance,
                    &error_schema_paths,
                    &mut diagnostics,
                );
                diagnostics.truncate(3);
            }
            if diagnostics.is_empty() {
                ValidationOutcome::Valid
            } else {
                ValidationOutcome::Invalid(diagnostics)
            }
        }))
        .unwrap_or(ValidationOutcome::WorkerFailed);
        drop(_permit);
        if started.elapsed() > watchdog {
            VALIDATION_WATCHDOG_EXCEEDED.fetch_add(1, Ordering::Relaxed);
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

pub(crate) fn validation_watchdog_exceeded_count() -> u64 {
    VALIDATION_WATCHDOG_EXCEEDED.load(Ordering::Relaxed)
}

fn prepend_composition_diagnostic(
    schema: &JsonValue,
    instance: &JsonValue,
    error_schema_paths: &[String],
    diagnostics: &mut Vec<SchemaDiagnostic>,
) {
    let Some((keyword, branches, node)) = first_failing_composition(schema, error_schema_paths)
    else {
        return;
    };
    let mut composition = SchemaDiagnostic {
        path: String::new(),
        constraint: bounded(format!("{keyword} ({branches} branches)"), 64),
    };
    if let Some((path, constraint)) = matching_discriminator_requirement(node, instance) {
        composition.path = path;
        diagnostics.insert(
            0,
            SchemaDiagnostic {
                path: composition.path.clone(),
                constraint,
            },
        );
    }
    diagnostics.insert(0, composition);
}

fn first_failing_composition<'a>(
    schema: &'a JsonValue,
    error_schema_paths: &[String],
) -> Option<(&'static str, usize, &'a JsonValue)> {
    for path in error_schema_paths {
        for keyword in ["oneOf", "anyOf", "allOf"] {
            let marker = format!("/{keyword}");
            let mut search_from = 0;
            while let Some(relative) = path[search_from..].find(&marker) {
                let position = search_from + relative;
                let after = position + marker.len();
                if after < path.len() && path.as_bytes().get(after) != Some(&b'/') {
                    search_from = after;
                    continue;
                }
                let node_pointer = path[..position].trim_start_matches('#');
                let node = if node_pointer.is_empty() {
                    schema
                } else if let Some(node) = schema.pointer(node_pointer) {
                    node
                } else {
                    search_from = after;
                    continue;
                };
                if let Some(branches) = node.get(keyword).and_then(JsonValue::as_array) {
                    return Some((keyword, branches.len(), node));
                }
                search_from = after;
            }
        }
    }
    None
}

fn matching_discriminator_requirement(
    composition: &JsonValue,
    instance: &JsonValue,
) -> Option<(String, String)> {
    let instance = instance.as_object()?;
    for keyword in ["oneOf", "anyOf"] {
        let Some(branches) = composition.get(keyword).and_then(JsonValue::as_array) else {
            continue;
        };
        for branch in branches {
            let Some(properties) = branch.get("properties").and_then(JsonValue::as_object) else {
                continue;
            };
            let matches = properties.iter().any(|(name, property)| {
                let Some(value) = instance.get(name) else {
                    return false;
                };
                property.get("const") == Some(value)
                    || property
                        .get("enum")
                        .and_then(JsonValue::as_array)
                        .is_some_and(|values| values.len() == 1 && values.first() == Some(value))
            });
            if !matches {
                continue;
            }
            if let Some(missing) = branch
                .get("required")
                .and_then(JsonValue::as_array)
                .into_iter()
                .flatten()
                .filter_map(JsonValue::as_str)
                .find(|name| !instance.contains_key(*name))
            {
                return Some((
                    format!("/{}", escape_json_pointer(missing)),
                    "required".into(),
                ));
            }
        }
    }
    None
}

pub(crate) fn prepare_tools(
    tools: &[McpToolConfig],
    schema_config: &McpSchemaConfig,
    enforce_stateless_names: bool,
) -> Result<BTreeMap<String, PreparedMcpTool>, SchemaPreparationError> {
    validate_schema_config(schema_config)
        .map_err(|reason| SchemaPreparationError::configuration("SCHEMA_CONFIG_INVALID", reason))?;
    let mut prepared = BTreeMap::new();
    for tool in tools {
        if enforce_stateless_names {
            validate_stateless_tool_name(&tool.name).map_err(|reason| {
                SchemaPreparationError::new(
                    &tool.name,
                    SchemaKind::Configuration,
                    "/name",
                    "TOOL_NAME_INVALID",
                    reason,
                )
            })?;
        }
        let input_validator = compile_schema(
            &tool.input_schema,
            schema_config,
            SchemaRoot::InputObject,
            &tool.name,
            SchemaKind::Input,
        )?;
        let output_validator = tool
            .output_schema
            .as_ref()
            .map(|schema| {
                compile_schema(
                    schema,
                    schema_config,
                    SchemaRoot::Any,
                    &tool.name,
                    SchemaKind::Output,
                )
                .map(Arc::new)
            })
            .transpose()?;
        let (header_extractions, mask_plan) =
            prepare_gateway_annotations(&tool.input_schema, schema_config, &tool.name).map_err(
                |reason| {
                    SchemaPreparationError::new(
                        &tool.name,
                        SchemaKind::Input,
                        "/",
                        "SCHEMA_ANNOTATION_INVALID",
                        reason,
                    )
                },
            )?;
        let (routing_properties, routing_properties_open_ended) =
            prepare_routing_properties(&tool.input_schema, schema_config).map_err(|reason| {
                SchemaPreparationError::new(
                    &tool.name,
                    SchemaKind::Input,
                    "/",
                    "SCHEMA_ROUTING_ANALYSIS_FAILED",
                    reason,
                )
            })?;
        let value = PreparedMcpTool {
            input_schema_for_validation: Arc::new(tool.input_schema.clone()),
            output_schema_for_validation: tool.output_schema.clone().map(Arc::new),
            advertised_input_schema: advertised_schema(&tool.input_schema),
            advertised_output_schema: tool.output_schema.as_ref().map(advertised_schema),
            config: tool.clone(),
            input_validator: Arc::new(input_validator),
            output_validator,
            header_extractions: header_extractions.into(),
            mask_plan: mask_plan.into(),
            routing_properties: routing_properties.into(),
            routing_properties_open_ended,
            rejects_unmapped_arguments: schema_rejects_unmapped_root_arguments(&tool.input_schema),
        };
        if prepared.insert(tool.name.clone(), value).is_some() {
            return Err(SchemaPreparationError::new(
                &tool.name,
                SchemaKind::Configuration,
                "/tools",
                "DUPLICATE_TOOL_NAME",
                format!("duplicate mcp-router tool `{}`", tool.name),
            ));
        }
    }
    Ok(prepared)
}

fn prepare_routing_properties(
    schema: &JsonValue,
    config: &McpSchemaConfig,
) -> Result<(Vec<String>, bool), String> {
    let mut properties = BTreeSet::new();
    let mut open_ended = false;
    walk_reachable_schema_graph(
        schema,
        config.max_schema_graph_visits,
        false,
        WalkLocationPolicy::RoutingProperties,
        |node, _, location| {
            let InstanceLocation::Mappable(path) = location else {
                return Ok(());
            };
            match path.first() {
                Some(LogicalPathSegment::Property(name)) => {
                    properties.insert(name.clone());
                }
                Some(LogicalPathSegment::PropertyPattern(_)) if node != &JsonValue::Bool(false) => {
                    open_ended = true;
                }
                _ => {}
            }
            Ok(())
        },
    )?;
    Ok((properties.into_iter().collect(), open_ended))
}

fn schema_rejects_unmapped_root_arguments(schema: &JsonValue) -> bool {
    let Some(object) = schema.as_object() else {
        return false;
    };
    object.get("unevaluatedProperties") == Some(&JsonValue::Bool(false))
        || object.get("additionalProperties") == Some(&JsonValue::Bool(false))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SchemaKeywordShape {
    PropertyMap,
    PatternPropertyMap,
    DefinitionMap,
    SameLocationMap,
    SameLocationArray,
    TupleArray,
    ArrayItems,
    AnyProperty,
    SameLocation,
    AssertionOnly,
}

fn schema_keyword_shape(keyword: &str) -> Option<SchemaKeywordShape> {
    match keyword {
        "properties" => Some(SchemaKeywordShape::PropertyMap),
        "patternProperties" => Some(SchemaKeywordShape::PatternPropertyMap),
        "$defs" | "definitions" => Some(SchemaKeywordShape::DefinitionMap),
        "dependentSchemas" => Some(SchemaKeywordShape::SameLocationMap),
        "allOf" | "anyOf" | "oneOf" => Some(SchemaKeywordShape::SameLocationArray),
        "prefixItems" => Some(SchemaKeywordShape::TupleArray),
        "items" | "additionalItems" | "unevaluatedItems" | "contains" => {
            Some(SchemaKeywordShape::ArrayItems)
        }
        "additionalProperties" | "unevaluatedProperties" => Some(SchemaKeywordShape::AnyProperty),
        "then" | "else" => Some(SchemaKeywordShape::SameLocation),
        "contentSchema" | "propertyNames" | "not" | "if" => Some(SchemaKeywordShape::AssertionOnly),
        _ => None,
    }
}

fn advertised_schema(schema: &JsonValue) -> JsonValue {
    let JsonValue::Object(object) = schema else {
        return schema.clone();
    };
    let mut advertised = serde_json::Map::with_capacity(object.len());
    for (key, value) in object {
        if matches!(key.as_str(), "x-mask" | "x-mask-pattern" | "x-sensitive") {
            continue;
        }
        let value = match schema_keyword_shape(key) {
            Some(
                SchemaKeywordShape::PropertyMap
                | SchemaKeywordShape::PatternPropertyMap
                | SchemaKeywordShape::DefinitionMap
                | SchemaKeywordShape::SameLocationMap,
            ) => value.as_object().map_or_else(
                || value.clone(),
                |children| {
                    JsonValue::Object(
                        children
                            .iter()
                            .map(|(name, child)| (name.clone(), advertised_schema(child)))
                            .collect(),
                    )
                },
            ),
            Some(SchemaKeywordShape::SameLocationArray | SchemaKeywordShape::TupleArray) => {
                value.as_array().map_or_else(
                    || value.clone(),
                    |children| JsonValue::Array(children.iter().map(advertised_schema).collect()),
                )
            }
            Some(
                SchemaKeywordShape::ArrayItems
                | SchemaKeywordShape::AnyProperty
                | SchemaKeywordShape::SameLocation
                | SchemaKeywordShape::AssertionOnly,
            ) => advertised_schema(value),
            _ => value.clone(),
        };
        advertised.insert(key.clone(), value);
    }
    JsonValue::Object(advertised)
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
    schema_kind: SchemaKind,
) -> Result<Validator, SchemaPreparationError> {
    preflight_schema(schema, config, tool_name, schema_kind)?;
    if matches!(root, SchemaRoot::InputObject) {
        match schema {
            JsonValue::Object(_) => {}
            JsonValue::Bool(_) => {
                return Err(SchemaPreparationError::new(
                    tool_name,
                    schema_kind,
                    "/",
                    "INPUT_SCHEMA_BOOLEAN_ROOT",
                    "boolean schemas are not supported; declare root `type: object`",
                ));
            }
            value => {
                return Err(SchemaPreparationError::new(
                    tool_name,
                    schema_kind,
                    "/",
                    "INPUT_SCHEMA_NOT_OBJECT_DOCUMENT",
                    format!(
                        "must be a JSON object schema document; found {}",
                        json_value_kind(value)
                    ),
                ));
            }
        }
        if !has_object_root(schema, schema, &mut BTreeSet::new()) {
            return Err(SchemaPreparationError::new(
                tool_name,
                schema_kind,
                "/",
                "INPUT_SCHEMA_MISSING_OBJECT_ROOT",
                "must declare root `type: object`; allOf, anyOf, oneOf, conditionals, and local references may be used alongside it",
            ));
        }
    }
    jsonschema::draft202012::options()
        .with_pattern_options(
            PatternOptions::regex()
                .size_limit(REGEX_SIZE_LIMIT_BYTES)
                .dfa_size_limit(REGEX_DFA_SIZE_LIMIT_BYTES),
        )
        .build(schema)
        .map_err(|error| {
            SchemaPreparationError::new(
                tool_name,
                schema_kind,
                "/",
                "SCHEMA_COMPILATION_FAILED",
                format!("schema compilation failed: {error}"),
            )
        })
}

fn json_value_kind(value: &JsonValue) -> &'static str {
    match value {
        JsonValue::Null => "null",
        JsonValue::Bool(_) => "boolean",
        JsonValue::Number(_) => "number",
        JsonValue::String(_) => "string",
        JsonValue::Array(_) => "array",
        JsonValue::Object(_) => "object",
    }
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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum LogicalPathSegment {
    Property(String),
    PropertyPattern(String),
    AnyIndex,
    Index(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum InstanceLocation {
    Mappable(Vec<LogicalPathSegment>),
    AssertionOnly,
    Definition,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WalkLocationPolicy {
    GatewayAnnotations,
    RoutingProperties,
}

struct SchemaChild<'a> {
    schema: &'a JsonValue,
    keyword: &'a str,
    pointer_suffix: String,
    shape: SchemaKeywordShape,
    name: Option<String>,
    index: Option<usize>,
}

fn schema_children(
    object: &serde_json::Map<String, JsonValue>,
    include_definitions: bool,
) -> Vec<SchemaChild<'_>> {
    let mut children = Vec::new();
    for (keyword, value) in object {
        let Some(shape) = schema_keyword_shape(keyword) else {
            continue;
        };
        if shape == SchemaKeywordShape::DefinitionMap && !include_definitions {
            continue;
        }
        match shape {
            SchemaKeywordShape::PropertyMap
            | SchemaKeywordShape::PatternPropertyMap
            | SchemaKeywordShape::DefinitionMap
            | SchemaKeywordShape::SameLocationMap => {
                if let Some(values) = value.as_object() {
                    children.extend(values.iter().map(|(name, schema)| SchemaChild {
                        schema,
                        keyword,
                        pointer_suffix: format!(
                            "/{}/{}",
                            escape_json_pointer(keyword),
                            escape_json_pointer(name)
                        ),
                        shape,
                        name: Some(name.clone()),
                        index: None,
                    }));
                }
            }
            SchemaKeywordShape::SameLocationArray | SchemaKeywordShape::TupleArray => {
                if let Some(values) = value.as_array() {
                    children.extend(
                        values
                            .iter()
                            .enumerate()
                            .map(|(index, schema)| SchemaChild {
                                schema,
                                keyword,
                                pointer_suffix: format!(
                                    "/{}/{index}",
                                    escape_json_pointer(keyword)
                                ),
                                shape,
                                name: None,
                                index: Some(index),
                            }),
                    );
                }
            }
            SchemaKeywordShape::ArrayItems
            | SchemaKeywordShape::AnyProperty
            | SchemaKeywordShape::SameLocation
            | SchemaKeywordShape::AssertionOnly => children.push(SchemaChild {
                schema: value,
                keyword,
                pointer_suffix: format!("/{}", escape_json_pointer(keyword)),
                shape,
                name: None,
                index: None,
            }),
        }
    }
    children
}

fn child_instance_location(
    parent: &InstanceLocation,
    child: &SchemaChild<'_>,
    policy: WalkLocationPolicy,
) -> InstanceLocation {
    if matches!(parent, InstanceLocation::AssertionOnly) {
        return InstanceLocation::AssertionOnly;
    }
    if matches!(parent, InstanceLocation::Definition)
        || child.shape == SchemaKeywordShape::DefinitionMap
    {
        return InstanceLocation::Definition;
    }
    let InstanceLocation::Mappable(path) = parent else {
        return InstanceLocation::AssertionOnly;
    };
    let mut path = path.clone();
    match child.shape {
        SchemaKeywordShape::PropertyMap => {
            path.push(LogicalPathSegment::Property(
                child.name.clone().expect("property child name"),
            ));
        }
        SchemaKeywordShape::PatternPropertyMap => {
            path.push(LogicalPathSegment::PropertyPattern(
                child.name.clone().expect("pattern property child name"),
            ));
        }
        SchemaKeywordShape::TupleArray => {
            path.push(LogicalPathSegment::Index(
                child.index.expect("tuple child index"),
            ));
        }
        SchemaKeywordShape::ArrayItems => path.push(LogicalPathSegment::AnyIndex),
        SchemaKeywordShape::AnyProperty => {
            path.push(LogicalPathSegment::PropertyPattern(".*".to_string()));
        }
        SchemaKeywordShape::AssertionOnly
            if policy == WalkLocationPolicy::RoutingProperties && child.keyword == "if" => {}
        SchemaKeywordShape::AssertionOnly => return InstanceLocation::AssertionOnly,
        SchemaKeywordShape::DefinitionMap => return InstanceLocation::Definition,
        SchemaKeywordShape::SameLocationMap
        | SchemaKeywordShape::SameLocationArray
        | SchemaKeywordShape::SameLocation => {}
    }
    InstanceLocation::Mappable(path)
}

fn walk_reachable_schema_graph<F>(
    root: &JsonValue,
    visit_budget: usize,
    reject_annotated_cycles: bool,
    location_policy: WalkLocationPolicy,
    visitor: F,
) -> Result<(), String>
where
    F: FnMut(&JsonValue, &str, &InstanceLocation) -> Result<(), String>,
{
    struct ActiveReference {
        reference: String,
        saw_annotation: bool,
        saw_cycle: bool,
    }

    struct WalkState<'a, F> {
        root: &'a JsonValue,
        visit_budget: usize,
        visits: usize,
        reject_annotated_cycles: bool,
        location_policy: WalkLocationPolicy,
        active_refs: Vec<ActiveReference>,
        visitor: F,
    }

    fn visit<F>(
        state: &mut WalkState<'_, F>,
        schema: &JsonValue,
        pointer: &str,
        location: &InstanceLocation,
    ) -> Result<(), String>
    where
        F: FnMut(&JsonValue, &str, &InstanceLocation) -> Result<(), String>,
    {
        state.visits += 1;
        if state.visits > state.visit_budget {
            return Err("schema graph traversal exceeds maxSchemaGraphVisits".to_string());
        }
        let has_gateway_annotation = schema.as_object().is_some_and(|object| {
            object.contains_key("x-mcp-header")
                || object
                    .get("x-mask")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(false)
                || object
                    .get("x-sensitive")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(false)
        });
        if has_gateway_annotation {
            for active in &mut state.active_refs {
                active.saw_annotation = true;
            }
        }
        (state.visitor)(schema, pointer, location)?;
        let JsonValue::Object(object) = schema else {
            return Ok(());
        };
        if let Some(reference) = object.get("$ref").and_then(JsonValue::as_str)
            && !(reference == "#" && pointer.is_empty())
        {
            if let Some(active) = state
                .active_refs
                .iter_mut()
                .rev()
                .find(|active| active.reference == reference)
            {
                active.saw_cycle = true;
            } else {
                let target = resolve_local_reference(state.root, reference).ok_or_else(|| {
                    format!("unresolved local JSON Schema reference `{reference}`")
                })?;
                state.active_refs.push(ActiveReference {
                    reference: reference.to_string(),
                    saw_annotation: false,
                    saw_cycle: false,
                });
                visit(state, target, &format!("{pointer}/$ref"), location)?;
                let completed = state.active_refs.pop().expect("active reference frame");
                if state.reject_annotated_cycles && completed.saw_cycle && completed.saw_annotation
                {
                    return Err(format!(
                        "recursive reference `{reference}` produces a non-finite gateway annotation plan"
                    ));
                }
            }
        }
        for child in schema_children(object, false) {
            let child_location = child_instance_location(location, &child, state.location_policy);
            visit(
                state,
                child.schema,
                &format!("{pointer}{}", child.pointer_suffix),
                &child_location,
            )?;
        }
        Ok(())
    }

    let mut state = WalkState {
        root,
        visit_budget,
        reject_annotated_cycles,
        location_policy,
        visits: 0,
        active_refs: Vec::new(),
        visitor,
    };
    visit(
        &mut state,
        root,
        "",
        &InstanceLocation::Mappable(Vec::new()),
    )
}

fn preflight_schema(
    schema: &JsonValue,
    config: &McpSchemaConfig,
    tool_name: &str,
    schema_kind: SchemaKind,
) -> Result<(), SchemaPreparationError> {
    let bytes = serde_json::to_vec(schema)
        .map_err(|error| {
            SchemaPreparationError::new(
                tool_name,
                schema_kind,
                "/",
                "SCHEMA_SERIALIZATION_FAILED",
                format!("schema serialization failed: {error}"),
            )
        })?
        .len();
    if bytes > config.max_schema_bytes {
        return Err(schema_policy_error(
            tool_name,
            schema_kind,
            "/",
            "SCHEMA_BUDGET_EXCEEDED",
            "schema exceeds maxSchemaBytes",
        ));
    }
    if let Some(dialect) = schema.get("$schema").and_then(JsonValue::as_str)
        && dialect != config.default_dialect
    {
        return Err(schema_policy_error(
            tool_name,
            schema_kind,
            "/$schema",
            "UNSUPPORTED_DIALECT",
            &format!("uses unsupported JSON Schema dialect `{dialect}`"),
        ));
    }
    let mut count = 0_usize;
    let mut pending = vec![(schema, 0_usize, String::new())];
    while let Some((value, depth, pointer)) = pending.pop() {
        if depth > config.max_depth {
            return Err(schema_policy_error(
                tool_name,
                schema_kind,
                &pointer,
                "SCHEMA_BUDGET_EXCEEDED",
                "schema exceeds maxDepth",
            ));
        }
        if let JsonValue::Object(object) = value {
            count += 1;
            if count > config.max_subschemas {
                return Err(schema_policy_error(
                    tool_name,
                    schema_kind,
                    &pointer,
                    "SCHEMA_BUDGET_EXCEEDED",
                    "schema exceeds maxSubschemas",
                ));
            }
            if object.contains_key("$anchor") || object.contains_key("$dynamicAnchor") {
                return Err(schema_policy_error(
                    tool_name,
                    schema_kind,
                    &pointer,
                    "ANCHOR_REFERENCE_UNSUPPORTED",
                    "JSON Schema anchors are unsupported",
                ));
            }
            if object.contains_key("$dynamicRef") {
                return Err(schema_policy_error(
                    tool_name,
                    schema_kind,
                    &pointer,
                    "DYNAMIC_REFERENCE_UNSUPPORTED",
                    "dynamic JSON Schema references are unsupported",
                ));
            }
            if !pointer.is_empty() && object.contains_key("$id") {
                return Err(schema_policy_error(
                    tool_name,
                    schema_kind,
                    &pointer,
                    "NESTED_ID_UNSUPPORTED",
                    "nested JSON Schema $id is unsupported",
                ));
            }
            if !pointer.is_empty() && object.contains_key("$schema") {
                return Err(schema_policy_error(
                    tool_name,
                    schema_kind,
                    &pointer,
                    "NESTED_DIALECT_UNSUPPORTED",
                    "nested JSON Schema dialect declarations are unsupported",
                ));
            }
            if let Some(reference) = object.get("$ref").and_then(JsonValue::as_str) {
                if !reference.starts_with('#') {
                    return Err(schema_policy_error(
                        tool_name,
                        schema_kind,
                        &format!("{pointer}/$ref"),
                        "EXTERNAL_REFERENCE_DISABLED",
                        "external JSON Schema reference is disabled",
                    ));
                }
                if reference != "#" && !reference.starts_with("#/") {
                    return Err(schema_policy_error(
                        tool_name,
                        schema_kind,
                        &format!("{pointer}/$ref"),
                        "ANCHOR_REFERENCE_UNSUPPORTED",
                        "fragment-name JSON Schema references are unsupported",
                    ));
                }
                if resolve_local_reference(schema, reference).is_none() {
                    return Err(schema_policy_error(
                        tool_name,
                        schema_kind,
                        &format!("{pointer}/$ref"),
                        "UNRESOLVED_LOCAL_REFERENCE",
                        &format!("unresolved local JSON Schema reference `{reference}`"),
                    ));
                }
            }
            // Descend only through the shared JSON Schema position model. Values
            // under default, const, examples, and extension payloads are instance
            // data, so schema keywords inside them must not affect preflight.
            pending.extend(schema_children(object, true).into_iter().map(|child| {
                (
                    child.schema,
                    depth + 1,
                    format!("{pointer}{}", child.pointer_suffix),
                )
            }));
        }
    }
    let mut composition_branches = 0_usize;
    walk_reachable_schema_graph(
        schema,
        config.max_schema_graph_visits,
        false,
        WalkLocationPolicy::GatewayAnnotations,
        |node, _, _| {
            if let Some(object) = node.as_object() {
                for keyword in ["allOf", "anyOf", "oneOf"] {
                    if let Some(branches) = object.get(keyword).and_then(JsonValue::as_array) {
                        composition_branches = composition_branches.saturating_add(branches.len());
                    }
                }
            }
            Ok(())
        },
    )
    .map_err(|reason| {
        schema_policy_error(
            tool_name,
            schema_kind,
            "/",
            "SCHEMA_GRAPH_VISIT_LIMIT_EXCEEDED",
            &reason,
        )
    })?;
    if composition_branches > config.max_composition_branches {
        return Err(schema_policy_error(
            tool_name,
            schema_kind,
            "/",
            "COMPOSITION_BRANCH_LIMIT_EXCEEDED",
            &format!(
                "schema exceeds maxCompositionBranches ({composition_branches} > {})",
                config.max_composition_branches
            ),
        ));
    }
    Ok(())
}

fn schema_policy_error(
    tool_name: &str,
    schema_kind: SchemaKind,
    pointer: &str,
    code: &'static str,
    reason: &str,
) -> SchemaPreparationError {
    SchemaPreparationError::new(tool_name, schema_kind, pointer, code, reason)
}

fn escape_json_pointer(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

#[derive(Debug, Clone)]
struct HeaderCandidate {
    header_name: String,
    value_kind: HeaderValueKind,
}

fn prepare_gateway_annotations(
    schema: &JsonValue,
    config: &McpSchemaConfig,
    tool_name: &str,
) -> Result<(Vec<HeaderExtraction>, Vec<MaskRule>), String> {
    let mut headers = BTreeMap::<Vec<LogicalPathSegment>, Vec<HeaderCandidate>>::new();
    let mut masks = BTreeMap::<Vec<LogicalPathSegment>, BTreeSet<Option<String>>>::new();

    walk_reachable_schema_graph(
        schema,
        config.max_schema_graph_visits,
        true,
        WalkLocationPolicy::GatewayAnnotations,
        |node, pointer, location| {
            let Some(object) = node.as_object() else {
                return Ok(());
            };
            let is_sensitive = object
                .get("x-sensitive")
                .and_then(JsonValue::as_bool)
                .unwrap_or(false);
            let is_masked = is_sensitive
                || object
                    .get("x-mask")
                    .and_then(JsonValue::as_bool)
                    .unwrap_or(false);
            let mappable_path = match location {
                InstanceLocation::Mappable(path) => Some(path.clone()),
                InstanceLocation::AssertionOnly | InstanceLocation::Definition => None,
            };
            if is_masked {
                let path = mappable_path.clone().ok_or_else(|| {
                    format!(
                        "mcp-router tool `{tool_name}` masking annotation at `{}` cannot map to an instance value",
                        display_schema_pointer(pointer)
                    )
                })?;
                masks.entry(path).or_default().insert(if is_sensitive {
                    None
                } else {
                    object
                        .get("x-mask-pattern")
                        .and_then(JsonValue::as_str)
                        .map(ToString::to_string)
                });
            }
            if let Some(header) = object.get("x-mcp-header") {
                let path = mappable_path
                    .as_ref()
                    .and_then(|path| fixed_property_path(path))
                    .filter(|path| !path.is_empty())
                    .ok_or_else(|| {
                        format!(
                            "mcp-router tool `{tool_name}` x-mcp-header at `{}` must annotate a statically reachable fixed property",
                            display_schema_pointer(pointer)
                        )
                    })?;
                let header = header
                    .as_str()
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        format!(
                            "mcp-router tool `{tool_name}` x-mcp-header must be a non-empty string"
                        )
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
                let value_kind = match object.get("type").and_then(JsonValue::as_str) {
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
                    let minimum = object.get("minimum").and_then(JsonValue::as_i64);
                    let maximum = object.get("maximum").and_then(JsonValue::as_i64);
                    if minimum.is_none_or(|value| value < -MAX_SAFE_INTEGER)
                        || maximum.is_none_or(|value| value > MAX_SAFE_INTEGER)
                    {
                        return Err(format!(
                            "mcp-router tool `{tool_name}` x-mcp-header integer requires a safe minimum and maximum"
                        ));
                    }
                }
                headers
                    .entry(path.into_iter().map(LogicalPathSegment::Property).collect())
                    .or_default()
                    .push(HeaderCandidate {
                        header_name: header.to_string(),
                        value_kind,
                    });
            }
            Ok(())
        },
    )?;

    let mut mask_plan = Vec::new();
    for (logical_path, patterns) in masks {
        let full_mask = patterns.contains(&None);
        if !full_mask && patterns.len() > 1 {
            return Err(format!(
                "mcp-router tool `{tool_name}` has conflicting mask patterns for `{}`",
                display_instance_path(&logical_path)
            ));
        }
        let path = logical_path
            .into_iter()
            .map(|segment| match segment {
                LogicalPathSegment::Property(name) => Ok(MaskPathSegment::Property(name)),
                LogicalPathSegment::PropertyPattern(source) => Regex::new(&source)
                    .map(MaskPathSegment::PropertyPattern)
                    .map_err(|error| {
                        format!(
                            "mcp-router tool `{tool_name}` invalid patternProperties mask path: {error}"
                        )
                    }),
                LogicalPathSegment::AnyIndex => Ok(MaskPathSegment::AnyIndex),
                LogicalPathSegment::Index(index) => Ok(MaskPathSegment::Index(index)),
            })
            .collect::<Result<Vec<_>, _>>()?;
        mask_plan.push(MaskRule {
            path,
            pattern: if full_mask {
                None
            } else {
                patterns.into_iter().next().flatten()
            },
        });
    }

    let mut header_names = BTreeMap::<String, Vec<LogicalPathSegment>>::new();
    let mut header_plan = Vec::new();
    for (logical_path, candidates) in headers {
        let first = candidates.first().expect("header candidate");
        if candidates.iter().any(|candidate| {
            !candidate
                .header_name
                .eq_ignore_ascii_case(&first.header_name)
                || candidate.value_kind != first.value_kind
        }) {
            return Err(format!(
                "mcp-router tool `{tool_name}` has conflicting x-mcp-header declarations for `{}`",
                display_instance_path(&logical_path)
            ));
        }
        let property_path = fixed_property_path(&logical_path).expect("fixed header path");
        if mask_plan
            .iter()
            .any(|rule| rule.covers_fixed_property_path(&property_path))
        {
            return Err(format!(
                "mcp-router tool `{tool_name}` x-mcp-header cannot expose sensitive property `{}`",
                display_instance_path(&logical_path)
            ));
        }
        let normalized_name = first.header_name.to_ascii_lowercase();
        if let Some(existing_path) = header_names.insert(normalized_name, logical_path.clone())
            && existing_path != logical_path
        {
            return Err(format!(
                "mcp-router tool `{tool_name}` has duplicate x-mcp-header `{}`",
                first.header_name
            ));
        }
        header_plan.push(HeaderExtraction {
            header_name: first.header_name.clone(),
            property_path,
            value_kind: first.value_kind,
        });
    }
    Ok((header_plan, mask_plan))
}

fn fixed_property_path(path: &[LogicalPathSegment]) -> Option<Vec<String>> {
    path.iter()
        .map(|segment| match segment {
            LogicalPathSegment::Property(name) => Some(name.clone()),
            LogicalPathSegment::PropertyPattern(_)
            | LogicalPathSegment::AnyIndex
            | LogicalPathSegment::Index(_) => None,
        })
        .collect()
}

fn display_schema_pointer(pointer: &str) -> &str {
    if pointer.is_empty() { "/" } else { pointer }
}

fn display_instance_path(path: &[LogicalPathSegment]) -> String {
    if path.is_empty() {
        return "/".to_string();
    }
    path.iter()
        .map(|segment| match segment {
            LogicalPathSegment::Property(name) => format!("/{}", escape_json_pointer(name)),
            LogicalPathSegment::PropertyPattern(pattern) => format!("/<{pattern}>"),
            LogicalPathSegment::AnyIndex => "/*".to_string(),
            LogicalPathSegment::Index(index) => format!("/{index}"),
        })
        .collect()
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
        ("maxCompositionBranches", config.max_composition_branches),
        ("maxSchemaGraphVisits", config.max_schema_graph_visits),
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
fn default_max_composition_branches() -> usize {
    DEFAULT_MAX_COMPOSITION_BRANCHES
}
fn default_max_schema_graph_visits() -> usize {
    DEFAULT_MAX_SCHEMA_GRAPH_VISITS
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
            execution_placement: Default::default(),
            workflow_binding: None,
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
    fn input_schema_startup_errors_distinguish_document_shape_and_root_contract() {
        for (input, kind) in [
            (json!(""), "string"),
            (json!(1), "number"),
            (json!([]), "array"),
            (JsonValue::Null, "null"),
        ] {
            let error = prepare_tools(&[tool(input, None)], &McpSchemaConfig::default(), false)
                .expect_err("non-object schema document must fail");
            assert_eq!(error.reason_code, "INPUT_SCHEMA_NOT_OBJECT_DOCUMENT");
            assert!(
                error.reason.contains(&format!(
                    "must be a JSON object schema document; found {kind}"
                )),
                "unexpected error: {error}"
            );
        }

        for input in [json!(true), json!(false)] {
            let error = prepare_tools(&[tool(input, None)], &McpSchemaConfig::default(), false)
                .expect_err("boolean input schema must fail");
            assert_eq!(error.reason_code, "INPUT_SCHEMA_BOOLEAN_ROOT");
            assert!(error.reason.contains("boolean schemas are not supported"));
        }

        let error = prepare_tools(
            &[tool(
                json!({
                    "$schema":"https://json-schema.org/draft/2020-12/schema",
                    "allOf":[{"type":"object"}]
                }),
                None,
            )],
            &McpSchemaConfig::default(),
            false,
        )
        .expect_err("composition-only root must fail");
        assert_eq!(error.reason_code, "INPUT_SCHEMA_MISSING_OBJECT_ROOT");
        assert!(error.reason.contains("must declare root `type: object`"));
        assert!(
            error
                .reason
                .contains("allOf, anyOf, oneOf, conditionals, and local references")
        );
    }

    #[test]
    fn object_root_accepts_composition_siblings() {
        let prepared = prepare_tools(
            &[tool(
                json!({
                    "$schema":"https://json-schema.org/draft/2020-12/schema",
                    "type":"object",
                    "allOf":[
                        {"properties":{"name":{"type":"string"}},"required":["name"]},
                        {"properties":{"tag":{"type":"string"}}}
                    ]
                }),
                None,
            )],
            &McpSchemaConfig::default(),
            false,
        )
        .expect("object root with composition must compile");
        let validator = &prepared["test.tool"].input_validator;
        assert!(validator.is_valid(&json!({"name":"Ada"})));
        assert!(!validator.is_valid(&json!({"tag":"friend"})));
    }

    #[test]
    fn portal_phase1_generated_schemas_compile() {
        let fixture = std::env::var_os("PORTAL_PHASE1_SCHEMA_ARTIFACT").map_or_else(
            || include_str!("../testdata/mcp/phase1-generated-mcp-schemas.json").to_string(),
            |path| std::fs::read_to_string(path).expect("read Portal Phase 1 schema artifact"),
        );
        let artifact: JsonValue = serde_json::from_str(&fixture).expect("parse Portal artifact");
        assert_eq!(artifact["contractVersion"], "1.2.0");
        let tools = artifact["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 3);
        for generated in tools {
            let name = generated["name"].as_str().expect("tool name");
            let schema = &generated["inputSchema"];
            compile_schema(
                schema,
                &McpSchemaConfig::default(),
                SchemaRoot::InputObject,
                name,
                SchemaKind::Input,
            )
            .unwrap_or_else(|error| panic!("Portal schema for {name} must compile: {error}"));
        }
    }

    #[test]
    fn phase0_contract_matches_gateway_compiler_and_validator() {
        let contract: JsonValue =
            serde_json::from_str(include_str!("../testdata/mcp/mcp-schema-contract-v1.json"))
                .expect("Phase 0 contract");
        assert_eq!(contract["contractVersion"], "1.2.0");
        for case in contract["schemaCases"].as_array().expect("schema cases") {
            let id = case["id"].as_str().expect("case id");
            let schema = &case["schema"];
            let schema_kind = case["schemaKind"].as_str().expect("schema kind");
            let gateway_expected = case["expected"]["gatewayPrepare"]
                .as_str()
                .expect("gateway expectation");
            let root = if schema_kind == "input" {
                SchemaRoot::InputObject
            } else {
                SchemaRoot::Any
            };
            let kind = if schema_kind == "input" {
                SchemaKind::Input
            } else {
                SchemaKind::Output
            };
            let compiled = compile_schema(schema, &McpSchemaConfig::default(), root, id, kind);
            if gateway_expected == "reject" {
                assert!(compiled.is_err(), "{id} must fail Gateway preparation");
            } else {
                assert!(
                    compiled.is_ok(),
                    "{id} must compile for Gateway: {:?}",
                    compiled.err()
                );
            }

            let crate_expected = case["expected"]["jsonschema"]
                .as_str()
                .expect("jsonschema expectation");
            let crate_validator = jsonschema::draft202012::new(schema);
            if crate_expected == "reject" {
                assert!(
                    crate_validator.is_err(),
                    "{id} must fail jsonschema compile"
                );
                continue;
            }
            let crate_validator = crate_validator
                .unwrap_or_else(|error| panic!("{id} must compile in jsonschema: {error}"));
            for instance in case["instances"].as_array().expect("instances") {
                assert_eq!(
                    crate_validator.is_valid(&instance["value"]),
                    instance["valid"].as_bool().expect("valid outcome"),
                    "recorded outcome drifted for {id}/{}",
                    instance["id"].as_str().expect("instance id")
                );
            }
        }
    }

    #[test]
    fn advertised_schema_strips_only_private_schema_annotations() {
        let schema = json!({
            "type":"object",
            "required":["x-mask"],
            "properties":{
                "x-mask":{"type":"string"},
                "secret":{"type":"string","x-mask":true,"x-mask-pattern":".*","x-sensitive":true,"x-mcp-header":"Mcp-Param-Secret"},
                "payload":{"type":"object","default":{"x-sensitive":"data"}}
            },
            "$defs":{"x-sensitive":{"type":"string","const":"x-mask"}},
            "examples":[{"x-mask":"ordinary","x-sensitive":"data"}]
        });
        let advertised = advertised_schema(&schema);
        assert_eq!(advertised["properties"]["x-mask"]["type"], "string");
        assert_eq!(advertised["$defs"]["x-sensitive"]["const"], "x-mask");
        assert_eq!(
            advertised["properties"]["payload"]["default"]["x-sensitive"],
            "data"
        );
        assert_eq!(advertised["examples"][0]["x-sensitive"], "data");
        assert_eq!(
            advertised["properties"]["secret"]["x-mcp-header"],
            "Mcp-Param-Secret"
        );
        assert!(advertised["properties"]["secret"].get("x-mask").is_none());
        assert!(
            advertised["properties"]["secret"]
                .get("x-mask-pattern")
                .is_none()
        );
        assert!(
            advertised["properties"]["secret"]
                .get("x-sensitive")
                .is_none()
        );
        let original = jsonschema::draft202012::new(&schema).expect("original validator");
        let public = jsonschema::draft202012::new(&advertised).expect("advertised validator");
        for instance in [json!({"x-mask":"ok"}), json!({}), json!({"x-mask":1})] {
            assert_eq!(original.is_valid(&instance), public.is_valid(&instance));
        }
    }

    #[test]
    fn preflight_rejects_unsupported_resource_and_reference_forms_with_codes() {
        for (schema, code) in [
            (
                json!({"type":"object","$defs":{"x":{"$anchor":"x","type":"object"}}}),
                "ANCHOR_REFERENCE_UNSUPPORTED",
            ),
            (
                json!({"type":"object","$dynamicRef":"#/$defs/x","$defs":{"x":{"type":"object"}}}),
                "DYNAMIC_REFERENCE_UNSUPPORTED",
            ),
            (
                json!({"type":"object","properties":{"x":{"$id":"nested.json","type":"object"}}}),
                "NESTED_ID_UNSUPPORTED",
            ),
            (
                json!({"type":"object","properties":{"x":{"$schema":DIALECT_2020_12,"type":"object"}}}),
                "NESTED_DIALECT_UNSUPPORTED",
            ),
            (
                json!({"type":"object","$ref":"#named","$defs":{"x":{"$anchor":"named","type":"object"}}}),
                "ANCHOR_REFERENCE_UNSUPPORTED",
            ),
            (
                json!({"type":"object","$ref":"#/$defs/missing"}),
                "UNRESOLVED_LOCAL_REFERENCE",
            ),
            (
                json!({"type":"object","$ref":"https://example.com/schema"}),
                "EXTERNAL_REFERENCE_DISABLED",
            ),
        ] {
            let error = compile_schema(
                &schema,
                &McpSchemaConfig::default(),
                SchemaRoot::InputObject,
                "policy",
                SchemaKind::Input,
            )
            .expect_err("policy rejection");
            assert_eq!(error.reason_code, code, "unexpected error: {error}");
        }
    }

    #[test]
    fn preflight_ignores_schema_keywords_inside_instance_data_positions() {
        let schema = json!({
            "type":"object",
            "properties":{
                "cfg":{
                    "type":"object",
                    "default":{"$id":"ordinary-data"},
                    "examples":[{"$schema":"ordinary-data"}],
                    "const":{"$ref":"#/not-a-schema-reference"}
                }
            }
        });
        let prepared = prepare_tools(&[tool(schema, None)], &McpSchemaConfig::default(), false)
            .expect("keywords in default, examples, and const are instance data");
        assert!(
            prepared["test.tool"]
                .input_validator
                .is_valid(&json!({"cfg":{"$ref":"#/not-a-schema-reference"}}))
        );
    }

    #[test]
    fn composition_branch_budget_is_transitive_and_bounded() {
        let schema = json!({
            "type":"object",
            "allOf":[
                {"oneOf":[{"type":"object"},{"type":"object"}]},
                {"anyOf":[{"type":"object"},{"type":"object"}]}
            ]
        });
        let config = McpSchemaConfig {
            max_composition_branches: 5,
            ..McpSchemaConfig::default()
        };
        let error = compile_schema(
            &schema,
            &config,
            SchemaRoot::InputObject,
            "bounded",
            SchemaKind::Input,
        )
        .expect_err("six reachable branches exceed limit five");
        assert_eq!(error.reason_code, "COMPOSITION_BRANCH_LIMIT_EXCEEDED");
        assert!(error.reason.contains("maxCompositionBranches (6 > 5)"));
    }

    #[test]
    fn schema_graph_visit_budget_is_independent_from_subschema_count() {
        let schema = json!({
            "type":"object",
            "allOf":[
                {"$ref":"#/$defs/shared"},
                {"$ref":"#/$defs/shared"}
            ],
            "$defs":{"shared":{"type":"object"}}
        });
        let config = McpSchemaConfig {
            max_schema_graph_visits: 2,
            ..McpSchemaConfig::default()
        };
        let error = compile_schema(
            &schema,
            &config,
            SchemaRoot::InputObject,
            "bounded-graph",
            SchemaKind::Input,
        )
        .expect_err("reference expansion must use its own visit budget");
        assert_eq!(error.reason_code, "SCHEMA_GRAPH_VISIT_LIMIT_EXCEEDED");
        assert!(error.reason.contains("maxSchemaGraphVisits"));
    }

    #[tokio::test]
    async fn composed_diagnostics_are_bounded_and_prefer_matching_discriminator() {
        let schema = Arc::new(json!({
            "type":"object",
            "oneOf":[
                {"properties":{"model":{"const":"events"},"eventType":{"type":"string"}},"required":["model","eventType"]},
                {"properties":{"model":{"const":"persons"},"personType":{"type":"string"}},"required":["model","personType"]}
            ]
        }));
        let validator = Arc::new(jsonschema::draft202012::new(&schema).expect("validator"));
        let pool = SchemaValidationPool::new(&McpSchemaConfig::default()).expect("pool");
        let outcome = pool
            .validate_with_schema(validator, Some(schema), json!({"model":"persons"}))
            .await;
        let ValidationOutcome::Invalid(diagnostics) = outcome else {
            panic!("expected invalid outcome")
        };
        assert!(diagnostics.len() <= 3);
        assert_eq!(diagnostics[0].constraint, "oneOf (2 branches)");
        assert!(
            diagnostics
                .iter()
                .any(|item| item.path == "/personType" && item.constraint == "required")
        );
        assert!(!format!("{diagnostics:?}").contains("persons"));

        let all_of_schema = Arc::new(json!({
            "type":"object",
            "allOf":[
                {"required":["id"]},
                {"required":["name"]}
            ]
        }));
        let all_of_validator =
            Arc::new(jsonschema::draft202012::new(&all_of_schema).expect("allOf validator"));
        let ValidationOutcome::Invalid(all_of_diagnostics) = pool
            .validate_with_schema(all_of_validator, Some(all_of_schema), json!({}))
            .await
        else {
            panic!("expected invalid allOf outcome")
        };
        assert_eq!(all_of_diagnostics[0].constraint, "allOf (2 branches)");
    }

    #[test]
    fn composition_diagnostic_skips_unresolvable_candidate_paths() {
        let schema = json!({
            "oneOf":[{"type":"string"},{"type":"integer"}]
        });
        let paths = vec![
            "/missing/oneOf/0/type".to_string(),
            "/oneOf/1/type".to_string(),
        ];
        let (keyword, branches, node) =
            first_failing_composition(&schema, &paths).expect("later resolvable composition path");
        assert_eq!(keyword, "oneOf");
        assert_eq!(branches, 2);
        assert_eq!(node, &schema);
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
    fn composed_header_annotations_deduplicate_and_conflicts_fail_closed() {
        let prepared = prepare_tools(
            &[tool(
                json!({
                    "type":"object",
                    "oneOf":[
                        {"properties":{"kind":{"const":"a"},"requestId":{"type":"string","x-mcp-header":"Mcp-Param-Request-Id"}},"required":["kind","requestId"]},
                        {"properties":{"kind":{"const":"b"},"requestId":{"type":"string","x-mcp-header":"mcp-param-request-id"}},"required":["kind","requestId"]}
                    ]
                }),
                None,
            )],
            &McpSchemaConfig::default(),
            false,
        )
        .expect("equivalent branch annotations deduplicate");
        let headers = &prepared["test.tool"].header_extractions;
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].property_path, ["requestId"]);

        for schema in [
            json!({
                "type":"object",
                "oneOf":[
                    {"properties":{"requestId":{"type":"string","x-mcp-header":"Mcp-Param-A"}}},
                    {"properties":{"requestId":{"type":"string","x-mcp-header":"Mcp-Param-B"}}}
                ]
            }),
            json!({
                "type":"object",
                "oneOf":[
                    {"properties":{"requestId":{"type":"string","x-mcp-header":"Mcp-Param-Request-Id"}}},
                    {"properties":{"requestId":{"type":"integer","minimum":0,"maximum":10,"x-mcp-header":"Mcp-Param-Request-Id"}}}
                ]
            }),
        ] {
            let error = prepare_tools(&[tool(schema, None)], &McpSchemaConfig::default(), false)
                .expect_err("conflicting branch annotations must fail");
            assert_eq!(error.reason_code, "SCHEMA_ANNOTATION_INVALID");
            assert!(error.reason.contains("conflicting x-mcp-header"));
        }
    }

    #[test]
    fn reachable_annotation_walk_handles_refs_conditionals_and_unreachable_definitions() {
        let prepared = prepare_tools(
            &[tool(
                json!({
                    "type":"object",
                    "$defs":{
                        "shared":{"type":"string","x-mask":true},
                        "unused":{"type":"number","x-mcp-header":"not a header"}
                    },
                    "properties":{
                        "left":{"$ref":"#/$defs/shared"},
                        "right":{"$ref":"#/$defs/shared"},
                        "metadata":{
                            "type":"object",
                            "default":{"x-mcp-header":"not a header","x-mask":true},
                            "examples":[{"x-sensitive":true}]
                        }
                    },
                    "if":{"properties":{"kind":{"const":"region"}}},
                    "then":{"properties":{"region":{"type":"string","x-mcp-header":"Mcp-Param-Region"}}},
                    "else":{"properties":{"account":{"type":"string","x-sensitive":true}}}
                }),
                None,
            )],
            &McpSchemaConfig::default(),
            false,
        )
        .expect("reachable annotations compile");
        let prepared = &prepared["test.tool"];
        assert_eq!(prepared.header_extractions.len(), 1);
        assert_eq!(prepared.header_extractions[0].property_path, ["region"]);
        assert_eq!(prepared.mask_plan.len(), 3);
    }

    #[test]
    fn non_mappable_headers_and_conflicting_masks_fail_closed() {
        for schema in [
            json!({"type":"object","patternProperties":{"^x-":{"type":"string","x-mcp-header":"Mcp-Param-X"}}}),
            json!({"type":"object","not":{"properties":{"secret":{"type":"string","x-mcp-header":"Mcp-Param-Secret"}}}}),
            json!({"type":"object","if":{"properties":{"secret":{"type":"string","x-mcp-header":"Mcp-Param-Secret"}}}}),
            json!({
                "type":"object",
                "oneOf":[
                    {"properties":{"secret":{"type":"string","x-mask":true,"x-mask-pattern":"^(.).*$"}}},
                    {"properties":{"secret":{"type":"string","x-mask":true,"x-mask-pattern":"^(..).*$"}}}
                ]
            }),
        ] {
            let error = prepare_tools(&[tool(schema, None)], &McpSchemaConfig::default(), false)
                .expect_err("ambiguous annotation must fail");
            assert_eq!(error.reason_code, "SCHEMA_ANNOTATION_INVALID");
        }
    }

    #[test]
    fn pattern_wildcard_and_ancestor_masks_block_fixed_headers() {
        for schema in [
            json!({
                "type":"object",
                "properties":{"secret_key":{"type":"string","x-mcp-header":"X-Secret-Key"}},
                "patternProperties":{"^secret_":{"x-sensitive":true}}
            }),
            json!({
                "type":"object",
                "properties":{"requestId":{"type":"string","x-mcp-header":"Mcp-Param-Request-Id"}},
                "additionalProperties":{"x-sensitive":true}
            }),
            json!({
                "type":"object",
                "properties":{
                    "credentials":{
                        "type":"object",
                        "x-sensitive":true,
                        "properties":{
                            "token":{"type":"string","x-mcp-header":"Mcp-Param-Token"}
                        }
                    }
                }
            }),
        ] {
            let error = prepare_tools(&[tool(schema, None)], &McpSchemaConfig::default(), false)
                .expect_err("a matching mask path must block fixed header export");
            assert_eq!(error.reason_code, "SCHEMA_ANNOTATION_INVALID");
            assert!(error.reason.contains("cannot expose sensitive property"));
        }
    }

    #[test]
    fn full_mask_takes_precedence_over_partial_branch_patterns() {
        let prepared = prepare_tools(
            &[tool(
                json!({
                    "type":"object",
                    "oneOf":[
                        {"properties":{"secret":{"type":"string","x-mask":true,"x-mask-pattern":"^(.{2}).*$"}}},
                        {"properties":{"secret":{"type":"string","x-sensitive":true}}}
                    ]
                }),
                None,
            )],
            &McpSchemaConfig::default(),
            false,
        )
        .expect("full masking safely dominates a partial pattern");
        let plan = &prepared["test.tool"].mask_plan;
        assert_eq!(plan.len(), 1);
        assert!(plan[0].pattern.is_none());
    }

    #[test]
    fn shared_walker_terminates_cycles_and_bounds_adversarial_diamonds() {
        let cyclic = json!({
            "type":"object",
            "$defs":{"node":{"type":"object","properties":{"next":{"$ref":"#/$defs/node"}}}},
            "properties":{"root":{"$ref":"#/$defs/node"}}
        });
        prepare_tools(&[tool(cyclic, None)], &McpSchemaConfig::default(), false)
            .expect("unannotated recursive schema remains valid");

        let annotated_cycle = json!({
            "type":"object",
            "$defs":{
                "node":{
                    "type":"object",
                    "x-sensitive":true,
                    "properties":{"next":{"$ref":"#/$defs/node"}}
                }
            },
            "properties":{"root":{"$ref":"#/$defs/node"}}
        });
        let error = prepare_tools(
            &[tool(annotated_cycle, None)],
            &McpSchemaConfig::default(),
            false,
        )
        .expect_err("recursive annotations cannot produce a finite mask plan");
        assert_eq!(error.reason_code, "SCHEMA_ANNOTATION_INVALID");
        assert!(error.reason.contains("non-finite gateway annotation plan"));

        let mut definitions = serde_json::Map::new();
        definitions.insert("leaf".to_string(), json!({"type":"object"}));
        for level in 0..8 {
            let target = if level == 0 {
                "leaf".to_string()
            } else {
                format!("level-{}", level - 1)
            };
            definitions.insert(
                format!("level-{level}"),
                json!({"allOf":[{"$ref":format!("#/$defs/{target}")},{"$ref":format!("#/$defs/{target}")}]}),
            );
        }
        let diamond = json!({
            "type":"object",
            "$defs":definitions,
            "allOf":[{"$ref":"#/$defs/level-7"}]
        });
        let config = McpSchemaConfig {
            max_schema_graph_visits: 64,
            max_composition_branches: 1_024,
            ..McpSchemaConfig::default()
        };
        let error = prepare_tools(&[tool(diamond, None)], &config, false)
            .expect_err("diamond expansion must hit the monotonic visit budget");
        assert_eq!(error.reason_code, "SCHEMA_GRAPH_VISIT_LIMIT_EXCEEDED");
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
            let outcome = pool
                .validate_with_schema(Arc::clone(&validator), None, json!({}))
                .await;
            let ValidationOutcome::Invalid(diagnostics) = &outcome else {
                panic!("expected invalid outcome, got {outcome:?}")
            };
            assert!(!diagnostics.is_empty());
            assert!(diagnostics.len() <= 3);
            assert!(diagnostics.iter().all(|item| item.path.len() <= 256));
        }
    }

    #[test]
    #[ignore = "Phase 2 qualification benchmark; run explicitly from the deployment gate"]
    fn phase2_composed_validation_benchmark() {
        let branches = (0..64)
            .map(|index| {
                json!({
                    "properties":{"kind":{"const":format!("kind-{index}")}},
                    "required":["kind"]
                })
            })
            .collect::<Vec<_>>();
        let schema = json!({
            "type":"object",
            "oneOf":branches,
            "unevaluatedProperties":false
        });
        let validator = jsonschema::draft202012::new(&schema).expect("benchmark validator");
        let instance = json!({"kind":"kind-63"});
        let iterations = 10_000_u128;
        let started = Instant::now();
        for _ in 0..iterations {
            assert!(validator.is_valid(&instance));
        }
        let elapsed = started.elapsed();
        let per_second = iterations * 1_000_000_000 / elapsed.as_nanos().max(1);
        eprintln!(
            "phase2 composed validation: branches=64 iterations={iterations} elapsed_ms={} validations_per_second={per_second}",
            elapsed.as_millis()
        );
        assert!(per_second > 0);
    }
}
