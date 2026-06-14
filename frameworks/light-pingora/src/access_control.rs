use crate::config_util::{deserialize_string_list, deserialize_typed_map};
use crate::security::AuthPrincipal;
use async_trait::async_trait;
use light_rule::{ActionRegistry, EndpointConfig, Rule, RuleActionPlugin, RuleEngine};
use light_runtime::{ModuleKind, RuntimeConfig, RuntimeError};
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue, json};
use serde_yaml::{Mapping as YamlMapping, Value as YamlValue};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::Arc;

pub const ACCESS_CONTROL_FILE: &str = "access-control.yml";
pub const ACCESS_CONTROL_LEGACY_FILE: &str = "access-control.yaml";
pub const ACCESS_CONTROL_MODULE_ID: &str = "light-pingora/access-control";
pub const ACCESS_CONTROL_CONFIG_NAME: &str = "access-control";

pub const RULE_FILE: &str = "rule.yml";
pub const RULE_LEGACY_FILE: &str = "rule.yaml";
pub const RULE_MODULE_ID: &str = "light-pingora/rule";
pub const RULE_CONFIG_NAME: &str = "rule";

const REQUEST_ACCESS: &str = "req-acc";
const RESPONSE_FILTER: &str = "res-fil";
const RESPONSE_BODY: &str = "responseBody";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessControlConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_access_rule_logic")]
    pub access_rule_logic: String,
    #[serde(default = "default_true")]
    pub default_deny: bool,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub skip_path_prefixes: Vec<String>,
}

impl Default for AccessControlConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            access_rule_logic: default_access_rule_logic(),
            default_deny: true,
            skip_path_prefixes: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleFileConfig {
    #[serde(default, deserialize_with = "deserialize_typed_map")]
    pub rule_bodies: BTreeMap<String, Rule>,
    #[serde(default, deserialize_with = "deserialize_typed_map")]
    pub endpoint_rules: BTreeMap<String, EndpointConfig>,
}

#[derive(Clone)]
pub struct AccessControlRuntime {
    access: Option<AccessControlConfig>,
    rules: RuleFileConfig,
    engine: Arc<RuleEngine>,
}

impl fmt::Debug for AccessControlRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccessControlRuntime")
            .field("access_enabled", &self.authorization_enabled())
            .field("default_deny", &self.default_deny())
            .field("rule_count", &self.rules.rule_bodies.len())
            .field("endpoint_count", &self.rules.endpoint_rules.len())
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessDecision {
    Allowed,
    Denied(String),
}

impl AccessControlRuntime {
    pub fn new(access: Option<AccessControlConfig>, rules: RuleFileConfig) -> Self {
        Self {
            access,
            rules,
            engine: Arc::new(RuleEngine::new(Arc::new(default_action_registry()))),
        }
    }

    pub fn authorization_enabled(&self) -> bool {
        self.access.as_ref().is_some_and(|config| config.enabled)
    }

    pub fn default_deny(&self) -> bool {
        self.access
            .as_ref()
            .map(|config| config.default_deny)
            .unwrap_or(false)
    }

    pub async fn authorize_tool(
        &self,
        tool_name: &str,
        endpoint: &str,
        headers: &[(String, String)],
        auth: Option<&AuthPrincipal>,
        arguments: &JsonValue,
        correlation_id: Option<&str>,
    ) -> AccessDecision {
        let Some(config) = self.access.as_ref().filter(|config| config.enabled) else {
            return AccessDecision::Allowed;
        };
        if config
            .skip_path_prefixes
            .iter()
            .any(|prefix| endpoint.starts_with(prefix.as_str()))
        {
            return AccessDecision::Allowed;
        }

        let Some((_service_entry, endpoint_rules)) = self.find_service_entry(endpoint) else {
            return if config.default_deny {
                AccessDecision::Denied(format!(
                    "Access denied: no access control rule defined for {endpoint}"
                ))
            } else {
                AccessDecision::Allowed
            };
        };
        let rule_ids = rule_ids_for(endpoint_rules, REQUEST_ACCESS);
        if rule_ids.is_empty() {
            return if config.default_deny {
                AccessDecision::Denied(format!(
                    "Access denied: no access control rule defined for {endpoint}"
                ))
            } else {
                AccessDecision::Allowed
            };
        }

        let permission = permission_for(endpoint_rules);
        let mut context = build_rule_context(
            tool_name,
            endpoint,
            headers,
            auth,
            arguments,
            correlation_id,
            permission.as_ref(),
        );
        let allowed = self
            .execute_rule_ids(&rule_ids, config.access_rule_logic.as_str(), &mut context)
            .await;
        if allowed {
            AccessDecision::Allowed
        } else {
            AccessDecision::Denied(format!(
                "Access denied by access control rule for {endpoint}"
            ))
        }
    }

    pub async fn filter_mcp_response(
        &self,
        tool_name: &str,
        endpoint: &str,
        headers: &[(String, String)],
        auth: Option<&AuthPrincipal>,
        arguments: &JsonValue,
        correlation_id: Option<&str>,
        result: JsonValue,
    ) -> JsonValue {
        let Some((service_entry, endpoint_rules)) = self.find_service_entry(endpoint) else {
            return result;
        };
        let rule_ids = rule_ids_for(endpoint_rules, RESPONSE_FILTER);
        if rule_ids.is_empty() {
            return result;
        }
        let Some(target) = FilterTarget::from_result(&result) else {
            return result;
        };

        let permission = permission_for(endpoint_rules);
        let mut context = build_rule_context(
            tool_name,
            service_entry,
            headers,
            auth,
            arguments,
            correlation_id,
            permission.as_ref(),
        );
        if let JsonValue::Object(map) = &mut context {
            map.insert(
                RESPONSE_BODY.to_string(),
                JsonValue::String(target.response_body.clone()),
            );
            map.insert("statusCode".to_string(), json!(200));
        }

        for rule_id in rule_ids {
            let Some(rule) = self.rules.rule_bodies.get(rule_id.as_str()) else {
                return result;
            };
            let Ok(true) = self.engine.execute_rule(rule, &mut context).await else {
                return result;
            };
        }

        let Some(filtered_body) = context
            .get(RESPONSE_BODY)
            .and_then(JsonValue::as_str)
            .map(str::to_string)
        else {
            return result;
        };
        target.apply(result, filtered_body)
    }

    async fn execute_rule_ids(
        &self,
        rule_ids: &[String],
        logic: &str,
        context: &mut JsonValue,
    ) -> bool {
        if rule_ids.is_empty() {
            return true;
        }
        if logic.eq_ignore_ascii_case("all") {
            for rule_id in rule_ids {
                if !self.execute_rule_id(rule_id, context).await {
                    return false;
                }
            }
            return true;
        }

        for rule_id in rule_ids {
            let mut candidate = context.clone();
            if self.execute_rule_id(rule_id, &mut candidate).await {
                *context = candidate;
                return true;
            }
        }
        false
    }

    async fn execute_rule_id(&self, rule_id: &str, context: &mut JsonValue) -> bool {
        let Some(rule) = self.rules.rule_bodies.get(rule_id) else {
            return false;
        };
        self.engine
            .execute_rule(rule, context)
            .await
            .unwrap_or(false)
    }

    fn find_service_entry<'a>(
        &'a self,
        endpoint: &str,
    ) -> Option<(&'a str, &'a HashMap<String, JsonValue>)> {
        if let Some((service_entry, EndpointConfig::Map(map))) =
            self.rules.endpoint_rules.get_key_value(endpoint)
        {
            return Some((service_entry.as_str(), map));
        }
        self.rules
            .endpoint_rules
            .iter()
            .filter_map(|(service_entry, config)| {
                let EndpointConfig::Map(map) = config;
                endpoint_pattern_matches(service_entry, endpoint)
                    .then_some((service_entry.as_str(), map))
            })
            .max_by_key(|(service_entry, _)| service_entry.len())
    }
}

pub fn load_access_control_runtime(
    runtime_config: &RuntimeConfig,
    active: bool,
) -> Result<Option<AccessControlRuntime>, RuntimeError> {
    if !active {
        return Ok(None);
    }

    let access = load_config_any::<AccessControlConfig>(
        runtime_config,
        &[ACCESS_CONTROL_FILE, ACCESS_CONTROL_LEGACY_FILE],
    )?;
    let access = match access {
        Some(config) => Some(config),
        None => {
            load_values_config::<AccessControlConfig>(runtime_config, ACCESS_CONTROL_CONFIG_NAME)?
        }
    };
    if let Some((_, config)) = &access {
        runtime_config.module_registry.register_loaded_config(
            ACCESS_CONTROL_MODULE_ID,
            ACCESS_CONTROL_CONFIG_NAME,
            ModuleKind::Framework,
            config,
            [],
            config.enabled,
            Some(config.enabled),
            true,
        )?;
    }

    let rules =
        match load_config_any::<RuleFileConfig>(runtime_config, &[RULE_FILE, RULE_LEGACY_FILE])? {
            Some(config) => Some(config),
            None => load_values_config::<RuleFileConfig>(runtime_config, RULE_CONFIG_NAME)?,
        };
    if let Some((_, config)) = &rules {
        runtime_config.module_registry.register_loaded_config(
            RULE_MODULE_ID,
            RULE_CONFIG_NAME,
            ModuleKind::Framework,
            config,
            [],
            !config.rule_bodies.is_empty() || !config.endpoint_rules.is_empty(),
            Some(!config.rule_bodies.is_empty() || !config.endpoint_rules.is_empty()),
            true,
        )?;
    }

    let mut access_config = access.map(|(_, config)| config);
    let rule_config = rules.map(|(_, config)| config).unwrap_or_default();
    if access_config.is_none()
        && (!rule_config.rule_bodies.is_empty() || !rule_config.endpoint_rules.is_empty())
    {
        access_config = Some(AccessControlConfig::default());
    }
    if access_config.is_none()
        && rule_config.rule_bodies.is_empty()
        && rule_config.endpoint_rules.is_empty()
    {
        return Ok(None);
    }

    Ok(Some(AccessControlRuntime::new(access_config, rule_config)))
}

fn load_values_config<T>(
    runtime_config: &RuntimeConfig,
    config_name: &str,
) -> Result<Option<(String, T)>, RuntimeError>
where
    T: serde::de::DeserializeOwned,
{
    let Some(value) = values_config_value(runtime_config, config_name) else {
        return Ok(None);
    };
    let config = serde_yaml::from_value::<T>(value)?;
    Ok(Some((format!("values.yml:{config_name}"), config)))
}

fn values_config_value(runtime_config: &RuntimeConfig, config_name: &str) -> Option<YamlValue> {
    if let Some(value) = runtime_config.resolved_values.get(config_name) {
        return Some(value.clone());
    }

    let prefix = format!("{config_name}.");
    let mut config = YamlMapping::new();
    for (key, value) in &runtime_config.resolved_values {
        let Some(field_name) = key.strip_prefix(prefix.as_str()) else {
            continue;
        };
        if field_name.is_empty() {
            continue;
        }
        config.insert(YamlValue::String(field_name.to_string()), value.clone());
    }

    (!config.is_empty()).then_some(YamlValue::Mapping(config))
}

fn load_config_any<T>(
    runtime_config: &RuntimeConfig,
    files: &[&str],
) -> Result<Option<(String, T)>, RuntimeError>
where
    T: serde::de::DeserializeOwned,
{
    for file in files {
        match runtime_config
            .module_registry
            .load_config::<T>(runtime_config, file)
        {
            Ok(config) => return Ok(Some(((*file).to_string(), config))),
            Err(RuntimeError::MissingConfig(missing)) if missing == *file => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

fn default_action_registry() -> ActionRegistry {
    let mut registry = ActionRegistry::new();
    let rbac = Arc::new(RoleBasedAccessControlAction);
    registry.register(
        "com.networknt.rule.RoleBasedAccessControlAction",
        rbac.clone(),
    );
    registry.register("RoleBasedAccessControlAction", rbac);

    let column = Arc::new(ResponseColumnFilterAction);
    registry.register(
        "com.networknt.rule.ResponseColumnFilterAction",
        column.clone(),
    );
    registry.register("ResponseColumnFilterAction", column);

    let row = Arc::new(ResponseRowFilterAction);
    registry.register("com.networknt.rule.ResponseRowFilterAction", row.clone());
    registry.register("ResponseRowFilterAction", row);
    registry
}

fn build_rule_context(
    tool_name: &str,
    endpoint: &str,
    headers: &[(String, String)],
    auth: Option<&AuthPrincipal>,
    arguments: &JsonValue,
    correlation_id: Option<&str>,
    permission: Option<&JsonMap<String, JsonValue>>,
) -> JsonValue {
    let mut context = JsonMap::new();
    context.insert("auditInfo".to_string(), audit_info(auth, correlation_id));
    context.insert("headers".to_string(), headers_to_json(headers));
    context.insert(
        "endpoint".to_string(),
        JsonValue::String(endpoint.to_string()),
    );
    context.insert(
        "toolName".to_string(),
        JsonValue::String(tool_name.to_string()),
    );
    context.insert("toolArguments".to_string(), arguments.clone());
    if let Some(correlation_id) = correlation_id {
        context.insert(
            "correlationId".to_string(),
            JsonValue::String(correlation_id.to_string()),
        );
    }
    let permission_val = match permission {
        Some(perm) => JsonValue::Object(perm.clone()),
        None => JsonValue::Object(JsonMap::new()),
    };
    context.insert("permission".to_string(), permission_val);
    if let Some(permission) = permission {
        for (key, value) in permission {
            context.insert(key.clone(), value.clone());
        }
    }
    JsonValue::Object(context)
}

fn audit_info(auth: Option<&AuthPrincipal>, correlation_id: Option<&str>) -> JsonValue {
    let claims = normalized_claims(auth);
    json!({
        "subject_claims": {
            "ClaimsMap": claims
        },
        "correlationId": correlation_id
    })
}

fn normalized_claims(auth: Option<&AuthPrincipal>) -> JsonValue {
    let mut claims = auth
        .map(|principal| principal.claims.clone())
        .unwrap_or_else(|| json!({}));
    if let Some(principal) = auth
        && let JsonValue::Object(map) = &mut claims
    {
        if let Some(role) = principal.role.as_deref() {
            map.entry("role".to_string())
                .or_insert_with(|| JsonValue::String(role.to_string()));
        }
        if let Some(user_id) = principal.user_id.as_deref() {
            map.entry("uid".to_string())
                .or_insert_with(|| JsonValue::String(user_id.to_string()));
        }
        if let Some(client_id) = principal.client_id.as_deref() {
            map.entry("client_id".to_string())
                .or_insert_with(|| JsonValue::String(client_id.to_string()));
        }
    }
    claims
}

fn headers_to_json(headers: &[(String, String)]) -> JsonValue {
    let mut values = JsonMap::new();
    for (name, value) in headers {
        values.insert(name.to_ascii_lowercase(), JsonValue::String(value.clone()));
    }
    JsonValue::Object(values)
}

fn rule_ids_for(endpoint_rules: &HashMap<String, JsonValue>, rule_type: &str) -> Vec<String> {
    let Some(value) = endpoint_rules.get(rule_type) else {
        return Vec::new();
    };
    match value {
        JsonValue::Array(values) => values
            .iter()
            .filter_map(|value| {
                value.as_str().map(str::to_string).or_else(|| {
                    value
                        .get("ruleId")
                        .and_then(JsonValue::as_str)
                        .map(str::to_string)
                })
            })
            .collect(),
        JsonValue::String(value) => parse_string_rule_ids(value),
        JsonValue::Object(value) => value
            .get("ruleId")
            .and_then(JsonValue::as_str)
            .map(|rule_id| vec![rule_id.to_string()])
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn parse_string_rule_ids(value: &str) -> Vec<String> {
    let value = value.trim();
    if value.is_empty() {
        return Vec::new();
    }
    if value.starts_with('[')
        && let Ok(JsonValue::Array(values)) = serde_json::from_str::<JsonValue>(value)
    {
        return values
            .iter()
            .filter_map(|value| value.as_str().map(str::to_string))
            .collect();
    }
    value
        .split([',', ' ', '\n', '\t'])
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn permission_for(
    endpoint_rules: &HashMap<String, JsonValue>,
) -> Option<JsonMap<String, JsonValue>> {
    endpoint_rules
        .get("permission")
        .and_then(JsonValue::as_object)
        .cloned()
}

fn endpoint_pattern_matches(pattern: &str, endpoint: &str) -> bool {
    if pattern == endpoint {
        return true;
    }
    let (pattern_path, pattern_method) = split_endpoint(pattern);
    let (endpoint_path, endpoint_method) = split_endpoint(endpoint);
    if !pattern_method.eq_ignore_ascii_case(endpoint_method) {
        return false;
    }
    if pattern_path == endpoint_path {
        return true;
    }
    if endpoint_path
        .strip_prefix(pattern_path)
        .is_some_and(|suffix| suffix.starts_with('/'))
    {
        return true;
    }
    path_template_matches(pattern_path, endpoint_path)
}

fn split_endpoint(endpoint: &str) -> (&str, &str) {
    endpoint
        .rsplit_once('@')
        .map_or((endpoint, "call"), |(path, method)| (path, method))
}

fn path_template_matches(pattern: &str, path: &str) -> bool {
    let pattern_segments = pattern
        .trim_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    let path_segments = path
        .trim_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if pattern_segments.len() != path_segments.len() {
        return false;
    }
    pattern_segments
        .iter()
        .zip(path_segments.iter())
        .all(|(pattern, segment)| {
            (pattern.starts_with('{') && pattern.ends_with('}')) || pattern == segment
        })
}

#[derive(Debug)]
struct FilterTarget {
    kind: FilterTargetKind,
    response_body: String,
}

#[derive(Debug)]
enum FilterTargetKind {
    Structured,
    Text(usize),
}

impl FilterTarget {
    fn from_result(result: &JsonValue) -> Option<Self> {
        let object = result.as_object()?;
        if let Some(structured) = object.get("structuredContent") {
            return Some(Self {
                kind: FilterTargetKind::Structured,
                response_body: serde_json::to_string(structured).ok()?,
            });
        }
        let content = object.get("content")?.as_array()?;
        if content.len() != 1 {
            return None;
        }
        let item = content[0].as_object()?;
        if item.get("type").and_then(JsonValue::as_str) != Some("text") {
            return None;
        }
        Some(Self {
            kind: FilterTargetKind::Text(0),
            response_body: item.get("text")?.as_str()?.to_string(),
        })
    }

    fn apply(&self, mut result: JsonValue, filtered_body: String) -> JsonValue {
        match self.kind {
            FilterTargetKind::Structured => {
                let Ok(filtered) = serde_json::from_str::<JsonValue>(&filtered_body) else {
                    return result;
                };
                if let JsonValue::Object(map) = &mut result {
                    map.insert("structuredContent".to_string(), filtered);
                }
                update_text_content(&mut result, filtered_body);
                result
            }
            FilterTargetKind::Text(index) => {
                if let Some(item) = result
                    .get_mut("content")
                    .and_then(JsonValue::as_array_mut)
                    .and_then(|content| content.get_mut(index))
                    .and_then(JsonValue::as_object_mut)
                {
                    item.insert("text".to_string(), JsonValue::String(filtered_body));
                }
                result
            }
        }
    }
}

fn update_text_content(result: &mut JsonValue, text: String) {
    if let Some(item) = result
        .get_mut("content")
        .and_then(JsonValue::as_array_mut)
        .and_then(|content| content.get_mut(0))
        .and_then(JsonValue::as_object_mut)
        && item.get("type").and_then(JsonValue::as_str) == Some("text")
    {
        item.insert("text".to_string(), JsonValue::String(text));
    }
}

struct RoleBasedAccessControlAction;

#[async_trait]
impl RuleActionPlugin for RoleBasedAccessControlAction {
    async fn execute(
        &self,
        rule_context: &mut JsonValue,
        _action_values: &Option<JsonValue>,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let caller_roles = claim_for_dimension(rule_context, "role");
        let endpoint_roles = rule_context.get("roles").and_then(value_to_string);
        Ok(has_any_configured_permission(
            caller_roles.as_deref(),
            endpoint_roles.as_deref(),
        ))
    }
}

struct ResponseColumnFilterAction;

#[async_trait]
impl RuleActionPlugin for ResponseColumnFilterAction {
    async fn execute(
        &self,
        rule_context: &mut JsonValue,
        _action_values: &Option<JsonValue>,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let Some(col_config) = rule_context.get("col").cloned() else {
            return Ok(false);
        };
        let Some(response_body) = rule_context.get(RESPONSE_BODY).and_then(JsonValue::as_str)
        else {
            return Ok(false);
        };
        let mut body = match serde_json::from_str::<JsonValue>(response_body) {
            Ok(body) => body,
            Err(_) => return Ok(true),
        };
        apply_column_filters(&mut body, rule_context, &col_config);
        set_response_body(rule_context, &body);
        Ok(true)
    }
}

struct ResponseRowFilterAction;

#[async_trait]
impl RuleActionPlugin for ResponseRowFilterAction {
    async fn execute(
        &self,
        rule_context: &mut JsonValue,
        _action_values: &Option<JsonValue>,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let Some(row_config) = rule_context.get("row").cloned() else {
            return Ok(false);
        };
        let Some(response_body) = rule_context.get(RESPONSE_BODY).and_then(JsonValue::as_str)
        else {
            return Ok(false);
        };
        let mut body = match serde_json::from_str::<JsonValue>(response_body) {
            Ok(body) => body,
            Err(_) => return Ok(true),
        };
        apply_row_filters(&mut body, rule_context, &row_config);
        set_response_body(rule_context, &body);
        Ok(true)
    }
}

fn set_response_body(context: &mut JsonValue, body: &JsonValue) {
    if let JsonValue::Object(map) = context {
        map.insert(
            RESPONSE_BODY.to_string(),
            JsonValue::String(serde_json::to_string(body).unwrap_or_default()),
        );
    }
}

fn apply_column_filters(body: &mut JsonValue, context: &JsonValue, col_config: &JsonValue) {
    let items = if body.is_array() {
        body.as_array_mut().unwrap()
    } else if let Some(obj) = body.as_object_mut() {
        if let Some(arr) = obj.get_mut("items").and_then(|v| v.as_array_mut()) {
            arr
        } else {
            return;
        }
    } else {
        return;
    };
    for dimension in ["role", "group", "position", "attribute", "user"] {
        let Some(claim) = claim_for_dimension(context, dimension) else {
            continue;
        };
        let Some(permission_map) = col_config.get(dimension).and_then(JsonValue::as_object) else {
            continue;
        };
        for (permission, fields) in permission_map {
            if !permission_matches(Some(claim.as_str()), permission) {
                continue;
            }
            let Some((remove, field_names)) = column_field_list(fields) else {
                continue;
            };
            for item in items.iter_mut().filter_map(JsonValue::as_object_mut) {
                if remove {
                    for field in &field_names {
                        item.remove(field);
                    }
                } else {
                    item.retain(|key, _| field_names.iter().any(|field| field == key));
                }
            }
        }
    }
}

fn apply_row_filters(body: &mut JsonValue, context: &JsonValue, row_config: &JsonValue) {
    let items = if body.is_array() {
        body.as_array_mut().unwrap()
    } else if let Some(obj) = body.as_object_mut() {
        if let Some(arr) = obj.get_mut("items").and_then(|v| v.as_array_mut()) {
            arr
        } else {
            return;
        }
    } else {
        return;
    };
    for dimension in ["role", "group", "position", "attribute", "user"] {
        let Some(claim) = claim_for_dimension(context, dimension) else {
            continue;
        };
        let Some(permission_map) = row_config.get(dimension).and_then(JsonValue::as_object) else {
            continue;
        };
        for (permission, filters) in permission_map {
            if !permission_matches(Some(claim.as_str()), permission) {
                continue;
            }
            let filters = row_filter_list(filters);
            items.retain(|item| row_matches(item, context, &filters));
        }
    }
}

fn row_matches(item: &JsonValue, context: &JsonValue, filters: &[RowFilter]) -> bool {
    let Some(map) = item.as_object() else {
        return true;
    };
    filters.iter().all(|filter| {
        let Some(value) = map.get(filter.col_name.as_str()) else {
            return true;
        };
        let expected = filter
            .col_value
            .strip_prefix('@')
            .and_then(|claim| claim_value(context, &[claim]))
            .unwrap_or_else(|| filter.col_value.clone());
        compare_row_value(value, filter.operator.as_str(), expected.as_str())
    })
}

fn compare_row_value(value: &JsonValue, operator: &str, expected: &str) -> bool {
    if let Some(actual) = value.as_f64()
        && let Ok(expected) = expected.parse::<f64>()
    {
        return match operator {
            "=" => actual == expected,
            "!=" => actual != expected,
            "<" => actual < expected,
            ">" => actual > expected,
            "<=" => actual <= expected,
            ">=" => actual >= expected,
            _ => true,
        };
    }

    let actual = value_to_string(value).unwrap_or_default();
    match operator {
        "=" => actual == expected,
        "!=" => actual != expected,
        "in" => list_tokens(expected).iter().any(|item| item == &actual),
        "not in" => !list_tokens(expected).iter().any(|item| item == &actual),
        _ => true,
    }
}

#[derive(Debug)]
struct RowFilter {
    col_name: String,
    operator: String,
    col_value: String,
}

fn row_filter_list(value: &JsonValue) -> Vec<RowFilter> {
    let Some(filters) = value.as_array() else {
        return Vec::new();
    };
    filters
        .iter()
        .filter_map(|filter| {
            Some(RowFilter {
                col_name: filter.get("colName")?.as_str()?.to_string(),
                operator: filter
                    .get("operator")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("=")
                    .to_string(),
                col_value: value_to_string(filter.get("colValue")?)?,
            })
        })
        .collect()
}

fn column_field_list(value: &JsonValue) -> Option<(bool, Vec<String>)> {
    let value = value_to_string(value)?;
    let (remove, value) = value
        .strip_prefix('!')
        .map_or((false, value.as_str()), |value| (true, value));
    let fields = list_tokens(value);
    (!fields.is_empty()).then_some((remove, fields))
}

fn claim_for_dimension(context: &JsonValue, dimension: &str) -> Option<String> {
    match dimension {
        "role" => claim_value(context, &["role"]),
        "group" => claim_value(context, &["grp", "group"]),
        "position" => claim_value(context, &["pos", "position"]),
        "attribute" => claim_value(context, &["att", "attribute"]),
        "user" => claim_value(context, &["uid", "user_id", "sub"]),
        claim => claim_value(context, &[claim]),
    }
}

fn claim_value(context: &JsonValue, names: &[&str]) -> Option<String> {
    for name in names {
        let pointer = format!("/auditInfo/subject_claims/ClaimsMap/{name}");
        if let Some(value) = context.pointer(pointer.as_str()).and_then(value_to_string) {
            return Some(value);
        }
    }
    None
}

fn value_to_string(value: &JsonValue) -> Option<String> {
    match value {
        JsonValue::String(value) => Some(value.clone()),
        JsonValue::Number(value) => Some(value.to_string()),
        JsonValue::Bool(value) => Some(value.to_string()),
        JsonValue::Array(_) | JsonValue::Object(_) => serde_json::to_string(value).ok(),
        JsonValue::Null => None,
    }
}

fn has_any_configured_permission(actual: Option<&str>, configured: Option<&str>) -> bool {
    let Some(actual) = actual else {
        return false;
    };
    let Some(configured) = configured else {
        return false;
    };
    list_tokens(configured)
        .iter()
        .any(|permission| permission_matches(Some(actual), permission))
}

fn permission_matches(actual: Option<&str>, required: &str) -> bool {
    let Some(actual) = actual else {
        return false;
    };
    let actual = list_tokens(actual);
    actual.iter().any(|permission| permission == required)
}

fn list_tokens(value: &str) -> Vec<String> {
    let value = value.trim();
    if value.is_empty() {
        return Vec::new();
    }
    if value.starts_with('[')
        && let Ok(JsonValue::Array(values)) = serde_json::from_str::<JsonValue>(value)
    {
        return values
            .iter()
            .filter_map(value_to_string)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .collect();
    }
    value
        .split([',', ' ', '\n', '\t'])
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn default_true() -> bool {
    true
}

fn default_access_rule_logic() -> String {
    "any".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use light_runtime::{
        BootstrapConfig, DirectRegistryConfig, ModuleRegistry, PortalRegistryConfig, ServerConfig,
        ServiceIdentity, config::ClientConfig,
    };
    use tempfile::TempDir;

    fn auth(role: &str) -> AuthPrincipal {
        AuthPrincipal {
            role: Some(role.to_string()),
            claims: json!({ "role": role }),
            ..AuthPrincipal::default()
        }
    }

    fn policy_for_filter(rule_type: &str, permission: JsonValue) -> AccessControlRuntime {
        let permission_yaml = serde_yaml::to_string(&permission)
            .expect("permission yaml")
            .lines()
            .map(|line| format!("      {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        AccessControlRuntime::new(
            Some(AccessControlConfig::default()),
            serde_yaml::from_str::<RuleFileConfig>(
                format!(
                    r#"
ruleBodies:
  filter:
    common: Y
    ruleId: filter
    ruleName: Filter
    ruleType: res-fil
    conditions:
      - operatorCode: isNotNull
        propertyPath: {rule_type}
    actions:
      - actionClassName: com.networknt.rule.Response{}FilterAction
endpointRules:
  /v1/accounts@get:
    res-fil:
      - filter
    permission:
{permission_yaml}
"#,
                    if rule_type == "col" { "Column" } else { "Row" }
                )
                .as_str(),
            )
            .expect("rule config"),
        )
    }

    fn runtime_config_with_values(values: HashMap<String, YamlValue>) -> (TempDir, RuntimeConfig) {
        let config_dir = TempDir::new().expect("config temp dir");
        let runtime_config = RuntimeConfig {
            bootstrap: BootstrapConfig::default(),
            server: ServerConfig::default(),
            client: None::<ClientConfig>,
            portal_registry: None::<PortalRegistryConfig>,
            direct_registry: DirectRegistryConfig::default(),
            service_identity: ServiceIdentity::default(),
            config_dir: config_dir.path().to_path_buf(),
            external_config_dir: config_dir.path().join("external"),
            resolved_values: values,
            default_config_dir: None,
            embedded_config: &[],
            module_registry: Arc::new(ModuleRegistry::new()),
            cache_registry: None,
            registry_client: None,
        };
        (config_dir, runtime_config)
    }

    #[tokio::test]
    async fn load_access_control_runtime_reads_rules_from_values_without_rule_file() {
        let mut values = HashMap::new();
        values.insert("access-control.enabled".to_string(), YamlValue::Bool(true));
        values.insert(
            "access-control.accessRuleLogic".to_string(),
            YamlValue::String("any".to_string()),
        );
        values.insert(
            "rule.ruleBodies".to_string(),
            serde_yaml::from_str(
                r#"
allow-account-role:
  common: Y
  ruleId: allow-account-role
  ruleName: Allow account role
  ruleType: req-acc
  conditions:
    - operatorCode: isNotNull
      propertyPath: auditInfo.subject_claims.ClaimsMap.role
  actions:
    - actionClassName: com.networknt.rule.RoleBasedAccessControlAction
"#,
            )
            .expect("rule bodies"),
        );
        values.insert(
            "rule.endpointRules".to_string(),
            serde_yaml::from_str(
                r#"
/v1/accounts@get:
  req-acc:
    - allow-account-role
  permission:
    roles: account-manager teller
"#,
            )
            .expect("endpoint rules"),
        );

        let (_config_dir, runtime_config) = runtime_config_with_values(values);
        let policy = load_access_control_runtime(&runtime_config, true)
            .expect("load policy")
            .expect("policy");

        assert_eq!(
            policy
                .authorize_tool(
                    "getAccounts",
                    "/v1/accounts@get",
                    &[],
                    Some(&auth("account-manager")),
                    &json!({}),
                    None,
                )
                .await,
            AccessDecision::Allowed
        );
        assert_eq!(
            policy
                .authorize_tool(
                    "getAccounts",
                    "/v1/accounts@get",
                    &[],
                    Some(&auth("user")),
                    &json!({}),
                    None,
                )
                .await,
            AccessDecision::Denied(
                "Access denied by access control rule for /v1/accounts@get".to_string()
            )
        );
    }

    #[tokio::test]
    async fn response_column_filter_keeps_allowed_fields_in_structured_content() {
        let policy = policy_for_filter(
            "col",
            json!({
                "col": {
                    "role": {
                        "mcp-reader": "[\"id\"]"
                    }
                }
            }),
        );

        let result = policy
            .filter_mcp_response(
                "accounts",
                "/v1/accounts@get",
                &[],
                Some(&auth("mcp-reader")),
                &json!({}),
                None,
                json!({
                    "content": [
                        {
                            "type": "text",
                            "text": "[{\"id\":1,\"secret\":\"x\"}]"
                        }
                    ],
                    "structuredContent": [
                        {
                            "id": 1,
                            "secret": "x"
                        }
                    ]
                }),
            )
            .await;

        assert_eq!(result["structuredContent"][0]["id"], 1);
        assert!(result["structuredContent"][0].get("secret").is_none());
        assert_eq!(
            result["content"][0]["text"],
            JsonValue::String("[{\"id\":1}]".to_string())
        );
    }

    #[tokio::test]
    async fn response_column_filter_handles_wrapped_arrays() {
        let policy = policy_for_filter(
            "col",
            json!({
                "col": {
                    "role": {
                        "mcp-reader": "id"
                    }
                }
            }),
        );

        let result = policy
            .filter_mcp_response(
                "accounts",
                "/v1/accounts@get",
                &[],
                Some(&auth("mcp-reader")),
                &json!({}),
                None,
                json!({
                    "content": [
                        {
                            "type": "text",
                            "text": "{\"items\":[{\"id\":1,\"secret\":\"x\"}]}"
                        }
                    ],
                    "structuredContent": {
                        "items": [
                            {
                                "id": 1,
                                "secret": "x"
                            }
                        ]
                    }
                }),
            )
            .await;

        assert_eq!(result["structuredContent"]["items"][0]["id"], 1);
        assert!(
            result["structuredContent"]["items"][0]
                .get("secret")
                .is_none()
        );
        assert_eq!(
            result["content"][0]["text"],
            JsonValue::String("{\"items\":[{\"id\":1}]}".to_string())
        );
    }

    #[tokio::test]
    async fn response_row_filter_updates_text_content() {
        let policy = policy_for_filter(
            "row",
            json!({
                "row": {
                    "role": {
                        "mcp-reader": [
                            {
                                "colName": "status",
                                "operator": "=",
                                "colValue": "O"
                            }
                        ]
                    }
                }
            }),
        );

        let result = policy
            .filter_mcp_response(
                "accounts",
                "/v1/accounts@get",
                &[],
                Some(&auth("mcp-reader")),
                &json!({}),
                None,
                json!({
                    "content": [
                        {
                            "type": "text",
                            "text": "[{\"status\":\"O\"},{\"status\":\"C\"}]"
                        }
                    ]
                }),
            )
            .await;

        assert_eq!(
            result["content"][0]["text"],
            JsonValue::String("[{\"status\":\"O\"}]".to_string())
        );
    }

    #[tokio::test]
    async fn test_pingora_access_control_strict_cel_dynamic_permission() {
        let mut values = HashMap::new();
        values.insert("access-control.enabled".to_string(), YamlValue::Bool(true));
        values.insert(
            "access-control.accessRuleLogic".to_string(),
            YamlValue::String("any".to_string()),
        );
        values.insert(
            "rule.ruleBodies".to_string(),
            serde_yaml::from_str(
                r#"
allow-scp-claim-group-access-control.lightapi.net:
  common: Y
  ruleId: allow-scp-claim-group-access-control.lightapi.net
  ruleName: Group-based access control to match endpoint group with jwt scp claim
  ruleType: req-acc
  expression: "'scp' in auditInfo.subject_claims.ClaimsMap && 'groups' in permission && permission.groups in auditInfo.subject_claims.ClaimsMap.scp"
  conditionLanguage: cel
  conditionSecurityProfile: strict
"#,
            )
            .expect("rule bodies"),
        );
        values.insert(
            "rule.endpointRules".to_string(),
            serde_yaml::from_str(
                r#"
/v1/accounts@get:
  req-acc:
    - allow-scp-claim-group-access-control.lightapi.net
  permission:
    groups: portal.w
"#,
            )
            .expect("endpoint rules"),
        );

        let (_config_dir, runtime_config) = runtime_config_with_values(values);
        let policy = load_access_control_runtime(&runtime_config, true)
            .expect("load policy")
            .expect("policy");

        let auth_allowed = AuthPrincipal {
            role: Some("admin".to_string()),
            claims: json!({
                "scp": ["portal.r", "portal.w"]
            }),
            ..AuthPrincipal::default()
        };

        let auth_denied = AuthPrincipal {
            role: Some("admin".to_string()),
            claims: json!({
                "scp": ["portal.r"]
            }),
            ..AuthPrincipal::default()
        };

        assert_eq!(
            policy
                .authorize_tool(
                    "getAccounts",
                    "/v1/accounts@get",
                    &[],
                    Some(&auth_allowed),
                    &json!({}),
                    None,
                )
                .await,
            AccessDecision::Allowed
        );

        assert_eq!(
            policy
                .authorize_tool(
                    "getAccounts",
                    "/v1/accounts@get",
                    &[],
                    Some(&auth_denied),
                    &json!({}),
                    None,
                )
                .await,
            AccessDecision::Denied(
                "Access denied by access control rule for /v1/accounts@get".to_string()
            )
        );
    }
}
