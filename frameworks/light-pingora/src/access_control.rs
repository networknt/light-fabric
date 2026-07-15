use crate::config_util::{deserialize_string_list, deserialize_typed_map};
use crate::security::AuthPrincipal;
use async_trait::async_trait;
use light_rule::{
    ActionRegistry, EndpointConfig, Rule, RuleAction, RuleActionPlugin, RuleEngine,
    is_reserved_rule_context_key,
};
use light_runtime::{ModuleKind, RuntimeConfig, RuntimeError};
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue, json};
use serde_yaml::{Mapping as YamlMapping, Value as YamlValue};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::sync::Arc;
use tracing::{error, warn};

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
const RESPONSE_BODY_JSON: &str = "responseBodyJson";
const RESPONSE_ROW_FILTER_DENIED: &str = "responseRowFilterDenied";
const RESPONSE_COLUMN_FILTER_ACTION: &str = "ResponseColumnFilterAction";
const RESPONSE_ROW_FILTER_ACTION: &str = "ResponseRowFilterAction";
const RESPONSE_CEL_ROW_FILTER_ACTION: &str = "ResponseCelRowFilterAction";
const ROLE_BASED_ACCESS_CONTROL_ACTION: &str = "RoleBasedAccessControlAction";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessControlConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_access_rule_logic")]
    pub access_rule_logic: String,
    #[serde(default = "default_true")]
    pub default_deny: bool,
    #[serde(default)]
    pub default_include: bool,
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub skip_path_prefixes: Vec<String>,
    #[serde(default, alias = "claimMappings")]
    pub claim_mappings: BTreeMap<String, Vec<String>>,
    #[serde(default, alias = "toolsListAccessControl")]
    pub tools_list_access_control: ToolsListAccessControlConfig,
}

impl Default for AccessControlConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            access_rule_logic: default_access_rule_logic(),
            default_deny: true,
            default_include: false,
            skip_path_prefixes: Vec::new(),
            claim_mappings: BTreeMap::new(),
            tools_list_access_control: ToolsListAccessControlConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolsListAccessControlConfig {
    #[serde(default)]
    pub mode: ToolsListAccessControlMode,
    #[serde(default)]
    pub unknown_rule_fallback: ToolsListUnknownRuleFallback,
    #[serde(default = "default_max_cel_evaluations", alias = "maxCelEvaluations")]
    pub max_cel_evaluations: usize,
    #[serde(
        default = "default_tools_list_max_cache_entries",
        alias = "maxCacheEntries"
    )]
    pub max_cache_entries: usize,
    #[serde(default, alias = "claimMappings")]
    pub claim_mappings: BTreeMap<String, Vec<String>>,
}

impl Default for ToolsListAccessControlConfig {
    fn default() -> Self {
        Self {
            mode: ToolsListAccessControlMode::None,
            unknown_rule_fallback: ToolsListUnknownRuleFallback::Hidden,
            max_cel_evaluations: default_max_cel_evaluations(),
            max_cache_entries: default_tools_list_max_cache_entries(),
            claim_mappings: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolsListAccessControlMode {
    #[default]
    None,
    Permission,
    Cel,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolsListUnknownRuleFallback {
    #[default]
    Hidden,
    Visible,
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
    claim_mappings: Arc<BTreeMap<String, Vec<String>>>,
}

impl fmt::Debug for AccessControlRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccessControlRuntime")
            .field("access_enabled", &self.authorization_enabled())
            .field("default_deny", &self.default_deny())
            .field("default_include", &self.default_include())
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessControlResponseFilterError {
    InvalidJson(String),
    RuleNotFound(String),
    RuleRejected(String),
    RuleExecution { rule_id: String, message: String },
    MissingFilteredBody,
    Serialization(String),
}

impl fmt::Display for AccessControlResponseFilterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidJson(message) => {
                write!(formatter, "response body is not valid JSON: {message}")
            }
            Self::RuleNotFound(rule_id) => {
                write!(formatter, "response filter rule body not found: {rule_id}")
            }
            Self::RuleRejected(rule_id) => {
                write!(formatter, "response filter rule returned false: {rule_id}")
            }
            Self::RuleExecution { rule_id, message } => {
                write!(
                    formatter,
                    "response filter rule execution failed for {rule_id}: {message}"
                )
            }
            Self::MissingFilteredBody => {
                formatter.write_str("filtered response body is missing from rule context")
            }
            Self::Serialization(message) => {
                write!(
                    formatter,
                    "filtered response body serialization failed: {message}"
                )
            }
        }
    }
}

impl std::error::Error for AccessControlResponseFilterError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolVisibility {
    Visible,
    Hidden,
}

impl AccessControlRuntime {
    pub fn new(access: Option<AccessControlConfig>, rules: RuleFileConfig) -> Self {
        let claim_mappings = Arc::new(effective_claim_mappings(access.as_ref()));
        Self {
            access,
            rules,
            engine: Arc::new(RuleEngine::new(Arc::new(default_action_registry(
                claim_mappings.clone(),
            )))),
            claim_mappings,
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

    pub fn default_include(&self) -> bool {
        self.access
            .as_ref()
            .map(|config| config.default_include)
            .unwrap_or(false)
    }

    pub fn tools_list_access_control(&self) -> ToolsListAccessControlConfig {
        let mut config = self
            .access
            .as_ref()
            .map(|config| config.tools_list_access_control.clone())
            .unwrap_or_default();
        config.claim_mappings = self.claim_mappings.as_ref().clone();
        config
    }

    pub fn normalized_claims_for_visibility(&self, auth: Option<&AuthPrincipal>) -> JsonValue {
        normalized_claims(auth)
    }

    fn active_config_for_endpoint(&self, endpoint: &str) -> Option<&AccessControlConfig> {
        let config = self.access.as_ref().filter(|config| config.enabled)?;
        if Self::config_skips_target(config, endpoint) {
            return None;
        }
        Some(config)
    }

    fn active_config_for_mcp_tool(
        &self,
        tool_name: &str,
        endpoint: &str,
    ) -> Option<&AccessControlConfig> {
        let config = self.active_config_for_endpoint(endpoint)?;
        if Self::config_skips_target(config, tool_name) {
            return None;
        }
        Some(config)
    }

    fn config_skips_target(config: &AccessControlConfig, target: &str) -> bool {
        config
            .skip_path_prefixes
            .iter()
            .any(|prefix| target.starts_with(prefix.as_str()))
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
        let Some(config) = self.active_config_for_mcp_tool(tool_name, endpoint) else {
            return AccessDecision::Allowed;
        };

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

    pub fn tool_visible(
        &self,
        tool_name: &str,
        endpoint: &str,
        auth: Option<&AuthPrincipal>,
    ) -> ToolVisibility {
        let Some(config) = self.active_config_for_mcp_tool(tool_name, endpoint) else {
            return ToolVisibility::Visible;
        };
        if config.tools_list_access_control.mode == ToolsListAccessControlMode::None {
            return ToolVisibility::Visible;
        }

        let Some((_service_entry, endpoint_rules)) = self.find_service_entry(endpoint) else {
            return if config.default_deny {
                ToolVisibility::Hidden
            } else {
                ToolVisibility::Visible
            };
        };

        if let Some(visibility) = endpoint_rules.get("visibility") {
            return if permission_matches_claims(visibility, auth, self.claim_mappings.as_ref()) {
                ToolVisibility::Visible
            } else {
                ToolVisibility::Hidden
            };
        }

        let rule_ids = rule_ids_for(endpoint_rules, REQUEST_ACCESS);
        if rule_ids.is_empty() {
            return if config.default_deny {
                ToolVisibility::Hidden
            } else {
                ToolVisibility::Visible
            };
        }

        let permission = permission_for(endpoint_rules)
            .map(JsonValue::Object)
            .unwrap_or_else(|| json!({}));
        let mut outcomes = Vec::new();
        for rule_id in rule_ids {
            let Some(rule) = self.rules.rule_bodies.get(rule_id.as_str()) else {
                outcomes.push(unknown_rule_visible(config));
                continue;
            };
            match rule_visibility_match(rule, &permission, auth, self.claim_mappings.as_ref()) {
                RuleVisibilityMatch::Matched(value) => outcomes.push(value),
                RuleVisibilityMatch::Ignored => {}
                RuleVisibilityMatch::Unknown => outcomes.push(unknown_rule_visible(config)),
            }
        }

        if outcomes.is_empty() {
            return if config.default_deny {
                ToolVisibility::Hidden
            } else {
                ToolVisibility::Visible
            };
        }

        let visible = if config.access_rule_logic.eq_ignore_ascii_case("all") {
            outcomes.iter().all(|allowed| *allowed)
        } else {
            outcomes.iter().any(|allowed| *allowed)
        };
        if visible {
            ToolVisibility::Visible
        } else {
            ToolVisibility::Hidden
        }
    }

    pub async fn authorize_http_endpoint(
        &self,
        endpoint: &str,
        headers: &[(String, String)],
        auth: Option<&AuthPrincipal>,
        request_data: &JsonValue,
        correlation_id: Option<&str>,
    ) -> AccessDecision {
        let Some(config) = self.active_config_for_endpoint(endpoint) else {
            return AccessDecision::Allowed;
        };

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
            "http",
            endpoint,
            headers,
            auth,
            request_data,
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

    pub fn has_response_filter(&self, endpoint: &str) -> bool {
        if self.active_config_for_endpoint(endpoint).is_none() {
            return false;
        }
        self.find_service_entry(endpoint)
            .map(|(_, endpoint_rules)| !self.response_filter_rule_ids(endpoint_rules).is_empty())
            .unwrap_or(false)
    }

    pub async fn filter_http_response(
        &self,
        endpoint: &str,
        headers: &[(String, String)],
        auth: Option<&AuthPrincipal>,
        request_data: &JsonValue,
        correlation_id: Option<&str>,
        status_code: u16,
        response_body: &[u8],
    ) -> Result<Option<Vec<u8>>, AccessControlResponseFilterError> {
        if self.active_config_for_endpoint(endpoint).is_none() {
            return Ok(None);
        }
        let Some((service_entry, endpoint_rules)) = self.find_service_entry(endpoint) else {
            return Ok(None);
        };
        let rule_ids = self.response_filter_rule_ids(endpoint_rules);
        if rule_ids.is_empty() {
            return Ok(None);
        }
        if response_body.is_empty() {
            return Ok(Some(Vec::new()));
        }
        let body_json = serde_json::from_slice::<JsonValue>(response_body)
            .map_err(|error| AccessControlResponseFilterError::InvalidJson(error.to_string()))?;
        let permission = permission_for(endpoint_rules);
        let mut context = build_rule_context(
            "http",
            service_entry,
            headers,
            auth,
            request_data,
            correlation_id,
            permission.as_ref(),
        );
        if let JsonValue::Object(map) = &mut context {
            map.insert(
                RESPONSE_BODY.to_string(),
                JsonValue::String(String::from_utf8_lossy(response_body).into_owned()),
            );
            map.insert(RESPONSE_BODY_JSON.to_string(), body_json);
            map.insert("statusCode".to_string(), json!(status_code));
            insert_access_control_context(map, config_default_include(self.access.as_ref()));
        }

        for rule_id in rule_ids {
            let Some(rule) = self.rules.rule_bodies.get(rule_id.as_str()) else {
                warn!(
                    endpoint,
                    rule_id = rule_id.as_str(),
                    "Access control response filter failed: rule body not found"
                );
                return Err(AccessControlResponseFilterError::RuleNotFound(rule_id));
            };
            match self.engine.execute_rule(rule, &mut context).await {
                Ok(true) => {}
                Ok(false) => {
                    warn!(
                        endpoint,
                        rule_id = rule_id.as_str(),
                        "Access control response filter failed: rule returned false"
                    );
                    return Err(AccessControlResponseFilterError::RuleRejected(rule_id));
                }
                Err(error) => {
                    error!(
                        endpoint,
                        rule_id = rule_id.as_str(),
                        error = %error,
                        "Access control response filter failed: rule execution error"
                    );
                    return Err(AccessControlResponseFilterError::RuleExecution {
                        rule_id,
                        message: error.to_string(),
                    });
                }
            }
        }
        let filtered_body = context
            .get(RESPONSE_BODY_JSON)
            .ok_or(AccessControlResponseFilterError::MissingFilteredBody)?;
        serde_json::to_vec(filtered_body)
            .map(Some)
            .map_err(|error| AccessControlResponseFilterError::Serialization(error.to_string()))
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
        if self
            .active_config_for_mcp_tool(tool_name, endpoint)
            .is_none()
        {
            return result;
        }
        let Some((service_entry, endpoint_rules)) = self.find_service_entry(endpoint) else {
            return result;
        };
        let rule_ids = self.response_filter_rule_ids(endpoint_rules);
        if rule_ids.is_empty() {
            return result;
        }
        let Ok(target) = FilterTarget::from_result(&result) else {
            warn!(
                tool_name,
                endpoint,
                "Access control response filter failed: MCP result is not a filterable JSON payload"
            );
            return mcp_filter_error_result("Access control response filter failed");
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
                JsonValue::String(target.response_body_string()),
            );
            map.insert(RESPONSE_BODY_JSON.to_string(), target.body.clone());
            map.insert("statusCode".to_string(), json!(200));
            insert_access_control_context(map, config_default_include(self.access.as_ref()));
        }

        for rule_id in rule_ids {
            let Some(rule) = self.rules.rule_bodies.get(rule_id.as_str()) else {
                warn!(
                    tool_name,
                    endpoint,
                    rule_id = rule_id.as_str(),
                    "Access control response filter failed: rule body not found"
                );
                return mcp_filter_error_result("Access control response filter failed");
            };
            match self.engine.execute_rule(rule, &mut context).await {
                Ok(true) => {}
                Ok(false) => {
                    warn!(
                        tool_name,
                        endpoint,
                        rule_id = rule_id.as_str(),
                        "Access control response filter failed: rule returned false"
                    );
                    return mcp_filter_error_result("Access control response filter failed");
                }
                Err(error) => {
                    error!(
                        tool_name,
                        endpoint,
                        rule_id = rule_id.as_str(),
                        error = %error,
                        "Access control response filter failed: rule execution error"
                    );
                    return mcp_filter_error_result("Access control response filter failed");
                }
            }
        }

        if response_row_filter_denied(&context) {
            warn!(
                tool_name,
                endpoint, "Access control response row filter denied a top-level MCP object"
            );
            return mcp_filter_error_result("Access denied by response filter");
        }

        let Some(filtered_body) = context.get(RESPONSE_BODY_JSON).cloned() else {
            warn!(
                tool_name,
                endpoint,
                "Access control response filter failed: filtered response body missing from rule context"
            );
            return mcp_filter_error_result("Access control response filter failed");
        };
        target.apply(result, filtered_body)
    }

    fn response_filter_rule_ids(&self, endpoint_rules: &HashMap<String, JsonValue>) -> Vec<String> {
        let mut rule_ids = rule_ids_for(endpoint_rules, RESPONSE_FILTER);
        rule_ids.sort_by_key(|rule_id| self.response_filter_priority(rule_id));
        rule_ids
    }

    fn response_filter_priority(&self, rule_id: &str) -> u8 {
        let Some(rule) = self.rules.rule_bodies.get(rule_id) else {
            return 2;
        };
        let Some(actions) = rule.actions.as_ref() else {
            return 2;
        };
        if actions
            .iter()
            .any(|action| is_row_filter_action(&action.action_ref))
        {
            return 0;
        }
        if actions
            .iter()
            .any(|action| is_column_filter_action(&action.action_ref))
        {
            return 1;
        }
        2
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
    if access_config
        .as_ref()
        .is_some_and(|config| config.enabled && config.default_include)
    {
        warn!(
            "access-control defaultInclude is enabled; unmatched response row-filter claims will retain all rows"
        );
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
        insert_values_config_field(&mut config, field_name, value.clone());
    }

    (!config.is_empty()).then_some(YamlValue::Mapping(config))
}

fn insert_values_config_field(config: &mut YamlMapping, field_name: &str, value: YamlValue) {
    let path = field_name
        .split('.')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    if path.is_empty() {
        return;
    }
    insert_nested_yaml_value(config, path.as_slice(), value);
}

fn insert_nested_yaml_value(mapping: &mut YamlMapping, path: &[&str], value: YamlValue) {
    let key = YamlValue::String(path[0].to_string());
    if path.len() == 1 {
        mapping.insert(key, value);
        return;
    }
    if !mapping.contains_key(&key) {
        mapping.insert(key.clone(), YamlValue::Mapping(YamlMapping::new()));
    }
    let Some(child) = mapping.get_mut(&key) else {
        return;
    };
    if !child.is_mapping() {
        *child = YamlValue::Mapping(YamlMapping::new());
    }
    if let YamlValue::Mapping(child) = child {
        insert_nested_yaml_value(child, &path[1..], value);
    }
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

fn default_action_registry(claim_mappings: Arc<BTreeMap<String, Vec<String>>>) -> ActionRegistry {
    let mut registry = ActionRegistry::new();
    let rbac = Arc::new(RoleBasedAccessControlAction {
        claim_mappings: claim_mappings.clone(),
    });
    registry.register(
        "com.networknt.rule.RoleBasedAccessControlAction",
        rbac.clone(),
    );
    registry.register("RoleBasedAccessControlAction", rbac);

    let column = Arc::new(ResponseColumnFilterAction {
        claim_mappings: claim_mappings.clone(),
    });
    registry.register(
        "com.networknt.rule.ResponseColumnFilterAction",
        column.clone(),
    );
    registry.register("ResponseColumnFilterAction", column);

    let row = Arc::new(ResponseRowFilterAction { claim_mappings });
    registry.register("com.networknt.rule.ResponseRowFilterAction", row.clone());
    registry.register("ResponseRowFilterAction", row);

    let cel_row = Arc::new(ResponseCelRowFilterAction::default());
    registry.register(
        "com.networknt.rule.ResponseCelRowFilterAction",
        cel_row.clone(),
    );
    registry.register("ResponseCelRowFilterAction", cel_row);
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
            if is_reserved_rule_context_key(key) {
                warn!(
                    permission_key = key.as_str(),
                    "Skipping permission key that collides with a reserved rule-context field"
                );
                continue;
            }
            context.insert(key.clone(), value.clone());
        }
    }
    JsonValue::Object(context)
}

fn insert_access_control_context(context: &mut JsonMap<String, JsonValue>, default_include: bool) {
    context.insert(
        "accessControl".to_string(),
        json!({
            "defaultInclude": default_include
        }),
    );
}

fn config_default_include(config: Option<&AccessControlConfig>) -> bool {
    config.map(|config| config.default_include).unwrap_or(false)
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

fn is_row_filter_action(action_ref: &str) -> bool {
    action_ref == RESPONSE_ROW_FILTER_ACTION
        || action_ref == RESPONSE_CEL_ROW_FILTER_ACTION
        || action_ref.ends_with(&format!(".{RESPONSE_ROW_FILTER_ACTION}"))
        || action_ref.ends_with(&format!(".{RESPONSE_CEL_ROW_FILTER_ACTION}"))
}

fn is_column_filter_action(action_ref: &str) -> bool {
    action_ref == RESPONSE_COLUMN_FILTER_ACTION
        || action_ref.ends_with(&format!(".{RESPONSE_COLUMN_FILTER_ACTION}"))
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuleVisibilityMatch {
    Matched(bool),
    Ignored,
    Unknown,
}

fn rule_visibility_match(
    rule: &Rule,
    permission: &JsonValue,
    auth: Option<&AuthPrincipal>,
    claim_mappings: &BTreeMap<String, Vec<String>>,
) -> RuleVisibilityMatch {
    if rule.rule_type != REQUEST_ACCESS {
        return RuleVisibilityMatch::Ignored;
    }
    if !rule_has_authorizing_effect(rule) {
        return RuleVisibilityMatch::Ignored;
    }
    if is_role_access_rule(rule) {
        let claim_names = claim_names_for_permission("roles", claim_mappings);
        return RuleVisibilityMatch::Matched(permission_dimension_matches(
            permission,
            "roles",
            auth,
            claim_names,
        ));
    }
    if is_group_access_rule(rule) {
        let claim_names = claim_names_for_permission("groups", claim_mappings);
        return RuleVisibilityMatch::Matched(permission_dimension_matches(
            permission,
            "groups",
            auth,
            claim_names,
        ));
    }
    RuleVisibilityMatch::Unknown
}

fn rule_has_authorizing_effect(rule: &Rule) -> bool {
    !matches!(
        rule.access_control_effect
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("telemetry" | "none")
    )
}

fn is_role_access_rule(rule: &Rule) -> bool {
    rule.rule_id.contains("role")
        || rule
            .expression
            .as_deref()
            .is_some_and(|expression| expression.contains("permission.roles"))
        || rule_has_action(rule, ROLE_BASED_ACCESS_CONTROL_ACTION)
}

fn is_group_access_rule(rule: &Rule) -> bool {
    rule.rule_id.contains("group")
        || rule.rule_id.contains("scp")
        || rule
            .expression
            .as_deref()
            .is_some_and(|expression| expression.contains("permission.groups"))
}

fn rule_has_action(rule: &Rule, action_name: &str) -> bool {
    rule.actions.as_ref().is_some_and(|actions| {
        actions
            .iter()
            .any(|action| action_matches(action, action_name))
    })
}

fn action_matches(action: &RuleAction, action_name: &str) -> bool {
    action.action_ref == action_name || action.action_ref.ends_with(&format!(".{action_name}"))
}

fn permission_matches_claims(
    permission: &JsonValue,
    auth: Option<&AuthPrincipal>,
    claim_mappings: &BTreeMap<String, Vec<String>>,
) -> bool {
    let checks = ["roles", "groups", "positions", "attributes", "users"];
    let mut matched_any_dimension = false;
    for permission_key in checks {
        if permission.get(permission_key).is_some() {
            matched_any_dimension = true;
            let claim_names = claim_names_for_permission(permission_key, claim_mappings);
            if !permission_dimension_matches(permission, permission_key, auth, claim_names) {
                return false;
            }
        }
    }
    matched_any_dimension
}

fn permission_dimension_matches(
    permission: &JsonValue,
    permission_key: &str,
    auth: Option<&AuthPrincipal>,
    claim_names: Vec<String>,
) -> bool {
    let permission_values = permission
        .get(permission_key)
        .map(values_to_token_set)
        .unwrap_or_default();
    if permission_values.is_empty() {
        return false;
    }
    let claims = normalized_claims(auth);
    let claim_values = claim_names
        .iter()
        .filter_map(|name| claims.get(name.as_str()))
        .flat_map(values_to_token_set)
        .collect::<BTreeSet<_>>();
    !claim_values.is_empty()
        && permission_values
            .iter()
            .any(|permission| claim_values.contains(permission))
}

fn claim_names_for_permission(
    permission_key: &str,
    claim_mappings: &BTreeMap<String, Vec<String>>,
) -> Vec<String> {
    let configured = claim_mappings
        .get(permission_key)
        .into_iter()
        .flatten()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    if !configured.is_empty() {
        return configured;
    }
    default_claim_names_for_permission(permission_key)
        .iter()
        .map(|value| (*value).to_string())
        .collect()
}

fn default_claim_names_for_permission(permission_key: &str) -> &'static [&'static str] {
    match permission_key {
        "roles" => &["role", "roles"],
        "groups" => &["scp", "grp", "group", "groups"],
        "positions" => &["pos", "position", "positions"],
        "attributes" => &["att", "attribute", "attributes"],
        "users" => &["uid", "user_id", "sub"],
        _ => &[],
    }
}

fn effective_claim_mappings(config: Option<&AccessControlConfig>) -> BTreeMap<String, Vec<String>> {
    let Some(config) = config else {
        return BTreeMap::new();
    };
    let mut mappings = config.tools_list_access_control.claim_mappings.clone();
    mappings.extend(config.claim_mappings.clone());
    mappings
}

fn values_to_token_set(value: &JsonValue) -> BTreeSet<String> {
    match value {
        JsonValue::Array(values) => values.iter().flat_map(values_to_token_set).collect(),
        JsonValue::String(value) => list_tokens(value).into_iter().collect(),
        JsonValue::Number(_) | JsonValue::Bool(_) => value_to_string(value)
            .map(|value| BTreeSet::from([value]))
            .unwrap_or_default(),
        JsonValue::Object(_) | JsonValue::Null => BTreeSet::new(),
    }
}

fn unknown_rule_visible(config: &AccessControlConfig) -> bool {
    config.tools_list_access_control.unknown_rule_fallback == ToolsListUnknownRuleFallback::Visible
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
    body: JsonValue,
}

#[derive(Debug)]
enum FilterTargetKind {
    Structured,
    Text(usize),
}

impl FilterTarget {
    fn from_result(result: &JsonValue) -> Result<Self, ()> {
        let object = result.as_object().ok_or(())?;
        if let Some(structured) = object.get("structuredContent") {
            return Ok(Self {
                kind: FilterTargetKind::Structured,
                body: structured.clone(),
            });
        }
        let content = object.get("content").ok_or(())?.as_array().ok_or(())?;
        if content.len() != 1 {
            return Err(());
        }
        let item = content[0].as_object().ok_or(())?;
        if item.get("type").and_then(JsonValue::as_str) != Some("text") {
            return Err(());
        }
        let text = item.get("text").ok_or(())?.as_str().ok_or(())?;
        Ok(Self {
            kind: FilterTargetKind::Text(0),
            body: serde_json::from_str::<JsonValue>(text).map_err(|_| ())?,
        })
    }

    fn response_body_string(&self) -> String {
        serde_json::to_string(&self.body).unwrap_or_default()
    }

    fn apply(&self, mut result: JsonValue, filtered_body: JsonValue) -> JsonValue {
        let filtered_text = serde_json::to_string(&filtered_body).unwrap_or_default();
        match self.kind {
            FilterTargetKind::Structured => {
                if let JsonValue::Object(map) = &mut result {
                    map.insert("structuredContent".to_string(), filtered_body);
                }
                update_text_content(&mut result, filtered_text);
                result
            }
            FilterTargetKind::Text(index) => {
                if let Some(item) = result
                    .get_mut("content")
                    .and_then(JsonValue::as_array_mut)
                    .and_then(|content| content.get_mut(index))
                    .and_then(JsonValue::as_object_mut)
                {
                    item.insert("text".to_string(), JsonValue::String(filtered_text));
                }
                result
            }
        }
    }
}

fn mcp_filter_error_result(message: &str) -> JsonValue {
    json!({
        "isError": true,
        "content": [
            {
                "type": "text",
                "text": message
            }
        ]
    })
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

struct RoleBasedAccessControlAction {
    claim_mappings: Arc<BTreeMap<String, Vec<String>>>,
}

#[async_trait]
impl RuleActionPlugin for RoleBasedAccessControlAction {
    async fn execute(
        &self,
        rule_context: &mut JsonValue,
        _action_values: &Option<JsonValue>,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let caller_roles = claim_for_dimension(rule_context, "role", &self.claim_mappings);
        let endpoint_roles = rule_context.get("roles").and_then(value_to_string);
        Ok(has_any_configured_permission(
            caller_roles.as_deref(),
            endpoint_roles.as_deref(),
        ))
    }
}

struct ResponseColumnFilterAction {
    claim_mappings: Arc<BTreeMap<String, Vec<String>>>,
}

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
        let filter_specs = matching_column_filters(rule_context, &col_config, &self.claim_mappings);
        let Some(body) = response_body_json_mut(rule_context) else {
            return Ok(false);
        };
        apply_column_filter_specs(body, &filter_specs);
        Ok(true)
    }
}

struct ResponseRowFilterAction {
    claim_mappings: Arc<BTreeMap<String, Vec<String>>>,
}

#[async_trait]
impl RuleActionPlugin for ResponseRowFilterAction {
    async fn execute(
        &self,
        rule_context: &mut JsonValue,
        _action_values: &Option<JsonValue>,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let Some(row_config) = rule_context.get("row").cloned() else {
            return Ok(true);
        };
        let filter_groups = matching_row_filters(rule_context, &row_config, &self.claim_mappings);
        let default_include = rule_context
            .pointer("/accessControl/defaultInclude")
            .and_then(JsonValue::as_bool)
            .unwrap_or(false);
        let denied = {
            let Some(body) = response_body_json_mut(rule_context) else {
                return Ok(false);
            };
            apply_row_filter_groups(body, &filter_groups, default_include)
        };
        if denied {
            mark_response_row_filter_denied(rule_context);
        }
        Ok(true)
    }
}

struct ResponseCelRowFilterAction {
    engine: RuleEngine,
}

impl Default for ResponseCelRowFilterAction {
    fn default() -> Self {
        Self {
            engine: RuleEngine::new(Arc::new(ActionRegistry::new())),
        }
    }
}

#[async_trait]
impl RuleActionPlugin for ResponseCelRowFilterAction {
    async fn execute(
        &self,
        rule_context: &mut JsonValue,
        action_values: &Option<JsonValue>,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        let Some(row_expression) = action_values
            .as_ref()
            .and_then(|values| values.get("rowExpression"))
            .and_then(JsonValue::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return Ok(false);
        };
        let condition_security_profile = action_values
            .as_ref()
            .and_then(|values| values.get("conditionSecurityProfile"))
            .and_then(JsonValue::as_str);
        let base_context = row_expression_base_context(rule_context);
        let denied = {
            let Some(body) = response_body_json_mut(rule_context) else {
                return Ok(false);
            };
            apply_cel_row_filter(
                body,
                &base_context,
                &self.engine,
                row_expression,
                condition_security_profile,
            )?
        };
        if denied {
            mark_response_row_filter_denied(rule_context);
        }
        Ok(true)
    }
}

fn response_body_json_mut(context: &mut JsonValue) -> Option<&mut JsonValue> {
    context.as_object_mut()?.get_mut(RESPONSE_BODY_JSON)
}

fn mark_response_row_filter_denied(context: &mut JsonValue) {
    if let Some(access_control) = context
        .get_mut("accessControl")
        .and_then(JsonValue::as_object_mut)
    {
        access_control.insert(
            RESPONSE_ROW_FILTER_DENIED.to_string(),
            JsonValue::Bool(true),
        );
    }
}

fn response_row_filter_denied(context: &JsonValue) -> bool {
    context
        .get("accessControl")
        .and_then(|access_control| access_control.get(RESPONSE_ROW_FILTER_DENIED))
        .and_then(JsonValue::as_bool)
        .unwrap_or(false)
}

fn matching_column_filters(
    context: &JsonValue,
    col_config: &JsonValue,
    claim_mappings: &BTreeMap<String, Vec<String>>,
) -> Vec<(bool, Vec<String>)> {
    let mut specs = Vec::new();
    let Some(dimensions) = col_config.as_object() else {
        return specs;
    };
    for (dimension, configured_permissions) in dimensions {
        let Some(claim) = claim_for_dimension(context, dimension, claim_mappings) else {
            continue;
        };
        let Some(permission_map) = configured_permissions.as_object() else {
            continue;
        };
        for (permission, fields) in permission_map {
            if !permission_matches(Some(claim.as_str()), permission) {
                continue;
            }
            let Some((remove, field_names)) = column_field_list(fields) else {
                continue;
            };
            specs.push((remove, field_names));
        }
    }
    specs
}

fn apply_column_filter_specs(body: &mut JsonValue, specs: &[(bool, Vec<String>)]) {
    if let Some(items) = body.as_array_mut() {
        for item in items.iter_mut().filter_map(JsonValue::as_object_mut) {
            apply_column_specs_to_object(item, specs);
        }
        return;
    }
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    if let Some(items) = obj.get_mut("items").and_then(JsonValue::as_array_mut) {
        for item in items.iter_mut().filter_map(JsonValue::as_object_mut) {
            apply_column_specs_to_object(item, specs);
        }
        return;
    }
    apply_column_specs_to_object(obj, specs);
}

fn apply_column_specs_to_object(
    item: &mut JsonMap<String, JsonValue>,
    specs: &[(bool, Vec<String>)],
) {
    for (remove, field_names) in specs {
        if *remove {
            for field in field_names {
                item.remove(field);
            }
        } else {
            item.retain(|key, _| field_names.iter().any(|field| field == key));
        }
    }
}

fn matching_row_filters(
    context: &JsonValue,
    row_config: &JsonValue,
    claim_mappings: &BTreeMap<String, Vec<String>>,
) -> Vec<Vec<RowFilter>> {
    let mut filter_groups = Vec::new();
    let Some(dimensions) = row_config.as_object() else {
        return filter_groups;
    };
    for (dimension, configured_permissions) in dimensions {
        let Some(claim) = claim_for_dimension(context, dimension, claim_mappings) else {
            continue;
        };
        let Some(permission_map) = configured_permissions.as_object() else {
            continue;
        };
        for (permission, filters) in permission_map {
            if !permission_matches(Some(claim.as_str()), permission) {
                continue;
            }
            let filters = row_filter_list(filters, context).unwrap_or_else(|| {
                warn!(
                    dimension,
                    permission,
                    "Access control row filter configuration is invalid; matched rows will be denied"
                );
                Vec::new()
            });
            filter_groups.push(filters);
        }
    }
    filter_groups
}

fn apply_row_filter_groups(
    body: &mut JsonValue,
    filter_groups: &[Vec<RowFilter>],
    default_include: bool,
) -> bool {
    if let Some(items) = row_filter_items_mut(body) {
        items.retain(|item| row_matches_filter_groups(item, filter_groups, default_include));
        return false;
    }

    if body.is_object() && !row_matches_filter_groups(body, filter_groups, default_include) {
        *body = JsonValue::Object(JsonMap::new());
        return true;
    }
    false
}

fn row_matches_filter_groups(
    item: &JsonValue,
    filter_groups: &[Vec<RowFilter>],
    default_include: bool,
) -> bool {
    if filter_groups.is_empty() {
        return default_include;
    }
    filter_groups
        .iter()
        .all(|filters| row_matches(item, filters))
}

fn row_filter_items_mut(body: &mut JsonValue) -> Option<&mut Vec<JsonValue>> {
    if body.is_array() {
        return body.as_array_mut();
    }
    body.as_object_mut()?
        .get_mut("items")
        .and_then(JsonValue::as_array_mut)
}

fn row_matches(item: &JsonValue, filters: &[RowFilter]) -> bool {
    let Some(map) = item.as_object() else {
        return false;
    };
    if filters.is_empty() {
        return false;
    }
    filters.iter().all(|filter| {
        let Some(value) = map.get(filter.col_name.as_str()) else {
            return false;
        };
        compare_row_value(value, filter.operator.as_str(), filter.col_value.as_str())
    })
}

fn apply_cel_row_filter(
    body: &mut JsonValue,
    base_context: &JsonValue,
    engine: &RuleEngine,
    row_expression: &str,
    condition_security_profile: Option<&str>,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    if let Some(items) = row_filter_items_mut(body) {
        engine.retain_cel_predicate_rows(
            "ResponseCelRowFilterAction",
            row_expression,
            condition_security_profile,
            RESPONSE_FILTER,
            base_context,
            items,
        )?;
        return Ok(false);
    }
    if !body.is_object() {
        return Ok(false);
    }

    let mut items = vec![body.take()];
    engine.retain_cel_predicate_rows(
        "ResponseCelRowFilterAction",
        row_expression,
        condition_security_profile,
        RESPONSE_FILTER,
        base_context,
        &mut items,
    )?;
    let denied = items.is_empty();
    *body = items
        .pop()
        .unwrap_or_else(|| JsonValue::Object(JsonMap::new()));
    Ok(denied)
}

fn row_expression_base_context(context: &JsonValue) -> JsonValue {
    let mut base = JsonMap::new();
    let Some(map) = context.as_object() else {
        return JsonValue::Object(base);
    };
    for key in [
        "auditInfo",
        "headers",
        "endpoint",
        "toolName",
        "toolArguments",
        "correlationId",
        "permission",
        "roles",
        "col",
    ] {
        if let Some(value) = map.get(key) {
            base.insert(key.to_string(), value.clone());
        }
    }
    JsonValue::Object(base)
}

fn compare_row_value(value: &JsonValue, operator: &str, expected: &str) -> bool {
    if let Some(actual) = value.as_f64() {
        if operator == "range" {
            let bounds = list_tokens(expected);
            if bounds.len() != 2 {
                return false;
            }
            let (Ok(min), Ok(max)) = (bounds[0].parse::<f64>(), bounds[1].parse::<f64>()) else {
                return false;
            };
            return actual >= min && actual <= max;
        }
        if matches!(operator, "=" | "!=" | "<" | ">" | "<=" | ">=") {
            let Ok(expected) = expected.parse::<f64>() else {
                return false;
            };
            return match operator {
                "=" => actual == expected,
                "!=" => actual != expected,
                "<" => actual < expected,
                ">" => actual > expected,
                "<=" => actual <= expected,
                ">=" => actual >= expected,
                _ => false,
            };
        }
    }

    let actual = match value {
        JsonValue::String(value) => value.clone(),
        JsonValue::Number(value) => value.to_string(),
        JsonValue::Bool(value) => value.to_string(),
        JsonValue::Null | JsonValue::Array(_) | JsonValue::Object(_) => return false,
    };
    match operator {
        "=" => actual == expected,
        "!=" => actual != expected,
        "in" => list_tokens(expected).iter().any(|item| item == &actual),
        "not in" => !list_tokens(expected).iter().any(|item| item == &actual),
        _ => false,
    }
}

#[derive(Debug)]
struct RowFilter {
    col_name: String,
    operator: String,
    col_value: String,
}

fn row_filter_list(value: &JsonValue, context: &JsonValue) -> Option<Vec<RowFilter>> {
    let filters = value.as_array()?;
    if filters.is_empty() {
        return None;
    }
    filters
        .iter()
        .map(|filter| {
            let col_value = value_to_string(filter.get("colValue")?)?;
            let col_value = if let Some(claim) = col_value.strip_prefix('@') {
                claim_value(context, &[claim])?
            } else {
                col_value
            };
            let col_name = filter.get("colName")?.as_str()?.trim();
            if col_name.is_empty() {
                return None;
            }
            let operator = match filter.get("operator") {
                Some(operator) => operator.as_str()?,
                None => "=",
            };
            if !matches!(
                operator,
                "=" | "!=" | "<" | ">" | "<=" | ">=" | "in" | "not in" | "range"
            ) {
                return None;
            }
            Some(RowFilter {
                col_name: col_name.to_string(),
                operator: operator.to_string(),
                col_value,
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

fn claim_for_dimension(
    context: &JsonValue,
    dimension: &str,
    claim_mappings: &BTreeMap<String, Vec<String>>,
) -> Option<String> {
    let permission_key = match dimension {
        "role" => "roles",
        "group" => "groups",
        "position" => "positions",
        "attribute" => "attributes",
        "user" => "users",
        dimension => dimension,
    };
    let mut claim_names = claim_names_for_permission(permission_key, claim_mappings);
    if claim_names.is_empty() {
        claim_names.push(dimension.to_string());
    }
    let claim_values = claim_names
        .iter()
        .filter_map(|name| claim_value(context, &[name.as_str()]))
        .flat_map(|value| list_tokens(value.as_str()))
        .collect::<BTreeSet<_>>();
    (!claim_values.is_empty()).then(|| claim_values.into_iter().collect::<Vec<_>>().join(" "))
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

fn default_max_cel_evaluations() -> usize {
    100
}

fn default_tools_list_max_cache_entries() -> usize {
    2000
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

    #[test]
    fn permission_fields_cannot_overwrite_reserved_rule_context() {
        let permission = json!({
            "auditInfo": {"forged": true},
            "headers": {"forged": true},
            "endpoint": "forged@endpoint",
            "toolName": "forged-tool",
            "toolArguments": {"forged": true},
            "correlationId": "forged-correlation",
            "permission": {"forged": true},
            "responseBody": "forged-body",
            "responseBodyJson": {"forged": true},
            "statusCode": 201,
            "accessControl": {"defaultInclude": true},
            "roles": ["admin"],
            "row": {"role": {"admin": []}},
            "col": {"role": {"admin": "[id]"}}
        });
        let permission = permission.as_object().expect("permission object");
        let context = build_rule_context(
            "real-tool",
            "real@endpoint",
            &[("X-Test".to_string(), "real-header".to_string())],
            Some(&auth("user")),
            &json!({"real": true}),
            Some("real-correlation"),
            Some(permission),
        );

        assert_eq!(
            context["auditInfo"]["subject_claims"]["ClaimsMap"]["role"],
            "user"
        );
        assert_eq!(context["headers"]["x-test"], "real-header");
        assert_eq!(context["endpoint"], "real@endpoint");
        assert_eq!(context["toolName"], "real-tool");
        assert_eq!(context["toolArguments"], json!({"real": true}));
        assert_eq!(context["correlationId"], "real-correlation");
        assert_eq!(context["permission"], JsonValue::Object(permission.clone()));
        for key in [
            "responseBody",
            "responseBodyJson",
            "statusCode",
            "accessControl",
        ] {
            assert!(
                context.get(key).is_none(),
                "reserved key {key} was promoted"
            );
        }
        assert_eq!(context["roles"], json!(["admin"]));
        assert_eq!(context["row"], permission["row"]);
        assert_eq!(context["col"], permission["col"]);
    }

    fn policy_for_filter(rule_type: &str, permission: JsonValue) -> AccessControlRuntime {
        policy_for_filter_with_access(rule_type, permission, AccessControlConfig::default())
    }

    fn policy_for_filter_with_access(
        rule_type: &str,
        permission: JsonValue,
        access_config: AccessControlConfig,
    ) -> AccessControlRuntime {
        let permission_yaml = serde_yaml::to_string(&permission)
            .expect("permission yaml")
            .lines()
            .map(|line| format!("      {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        AccessControlRuntime::new(
            Some(access_config),
            serde_yaml::from_str::<RuleFileConfig>(
                format!(
                    r#"
ruleBodies:
  filter:
    common: Y
    ruleId: filter
    ruleName: Filter
    ruleType: res-fil
    expression: "{rule_type} != null"
    conditionLanguage: cel
    conditionSecurityProfile: strict
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

    fn policy_for_cel_row_filter(
        row_expression: &str,
        permission: JsonValue,
    ) -> AccessControlRuntime {
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
    expression: "true"
    conditionLanguage: cel
    conditionSecurityProfile: strict
    actions:
      - actionClassName: com.networknt.rule.ResponseCelRowFilterAction
        actionValues:
          rowExpression: "{row_expression}"
          conditionSecurityProfile: strict
endpointRules:
  /v1/accounts@get:
    res-fil:
      - filter
    permission:
{permission_yaml}
"#
                )
                .as_str(),
            )
            .expect("rule config"),
        )
    }

    #[tokio::test]
    async fn authorize_http_endpoint_uses_endpoint_rule() {
        let policy = AccessControlRuntime::new(
            Some(AccessControlConfig::default()),
            serde_yaml::from_str::<RuleFileConfig>(
                r#"
ruleBodies:
  allow-role:
    common: Y
    ruleId: allow-role
    ruleName: Allow Role
    ruleType: req-acc
    expression: "true"
    conditionLanguage: cel
    conditionSecurityProfile: strict
endpointRules:
  lightapi.net/service/getApi/0.1.0:
    req-acc:
      - allow-role
    permission:
      roles: api-admin
"#,
            )
            .expect("rule config"),
        );

        let decision = policy
            .authorize_http_endpoint(
                "lightapi.net/service/getApi/0.1.0",
                &[],
                Some(&auth("api-admin")),
                &json!({"hostId":"host-1"}),
                None,
            )
            .await;
        assert_eq!(decision, AccessDecision::Allowed);

        let denied = policy
            .authorize_http_endpoint(
                "lightapi.net/service/deleteApi/0.1.0",
                &[],
                Some(&auth("api-admin")),
                &json!({"hostId":"host-1"}),
                None,
            )
            .await;
        assert!(matches!(denied, AccessDecision::Denied(_)));
    }

    #[tokio::test]
    async fn role_action_uses_configured_claim_mapping() {
        let policy = AccessControlRuntime::new(
            Some(AccessControlConfig {
                claim_mappings: BTreeMap::from([(
                    "roles".to_string(),
                    vec!["custom_roles".to_string()],
                )]),
                ..AccessControlConfig::default()
            }),
            serde_yaml::from_str::<RuleFileConfig>(
                r#"
ruleBodies:
  allow-role:
    common: Y
    ruleId: allow-role
    ruleName: Allow Role
    ruleType: req-acc
    expression: "true"
    conditionLanguage: cel
    conditionSecurityProfile: strict
    actions:
      - actionClassName: com.networknt.rule.RoleBasedAccessControlAction
endpointRules:
  /v1/reports@get:
    req-acc:
      - allow-role
    permission:
      roles: auditor
"#,
            )
            .expect("rule config"),
        );
        let principal = AuthPrincipal {
            claims: json!({"custom_roles": ["auditor"]}),
            ..AuthPrincipal::default()
        };

        assert_eq!(
            policy
                .authorize_http_endpoint(
                    "/v1/reports@get",
                    &[],
                    Some(&principal),
                    &json!({}),
                    None,
                )
                .await,
            AccessDecision::Allowed
        );
    }

    #[tokio::test]
    async fn filter_http_response_applies_column_filter_for_endpoint() {
        let policy = policy_for_filter(
            "col",
            json!({
                "col": {
                    "role": {
                        "teller": "[\"accountNo\",\"firstName\"]"
                    }
                }
            }),
        );

        let filtered = policy
            .filter_http_response(
                "/v1/accounts@get",
                &[],
                Some(&auth("teller")),
                &json!({}),
                None,
                200,
                br#"[{"accountNo":"1","firstName":"A","ssn":"secret"}]"#,
            )
            .await
            .expect("response filter execution")
            .expect("filtered response");
        let value = serde_json::from_slice::<JsonValue>(&filtered).expect("json");
        assert_eq!(value[0]["accountNo"], "1");
        assert_eq!(value[0]["firstName"], "A");
        assert!(value[0].get("ssn").is_none());
    }

    #[tokio::test]
    async fn filter_http_response_denies_non_matching_top_level_object() {
        let policy = policy_for_filter(
            "row",
            json!({
                "row": {
                    "role": {
                        "teller": [{
                            "colName": "accountType",
                            "operator": "=",
                            "colValue": "C"
                        }]
                    }
                }
            }),
        );

        let filtered = policy
            .filter_http_response(
                "/v1/accounts@get",
                &[],
                Some(&auth("teller")),
                &json!({}),
                None,
                200,
                br#"{"accountType":"S","ssn":"secret"}"#,
            )
            .await
            .expect("response filter execution")
            .expect("filtered response");

        assert_eq!(
            serde_json::from_slice::<JsonValue>(&filtered).expect("json"),
            json!({})
        );
    }

    #[tokio::test]
    async fn response_filters_use_configured_claim_mapping() {
        let policy = AccessControlRuntime::new(
            Some(AccessControlConfig {
                claim_mappings: BTreeMap::from([(
                    "roles".to_string(),
                    vec!["custom_roles".to_string()],
                )]),
                ..AccessControlConfig::default()
            }),
            serde_yaml::from_str::<RuleFileConfig>(
                r#"
ruleBodies:
  row-filter:
    common: Y
    ruleId: row-filter
    ruleName: Row Filter
    ruleType: res-fil
    expression: "true"
    actions:
      - actionClassName: com.networknt.rule.ResponseRowFilterAction
  column-filter:
    common: Y
    ruleId: column-filter
    ruleName: Column Filter
    ruleType: res-fil
    expression: "true"
    actions:
      - actionClassName: com.networknt.rule.ResponseColumnFilterAction
endpointRules:
  /v1/reports@get:
    res-fil:
      - row-filter
      - column-filter
    permission:
      row:
        role:
          auditor:
            - colName: status
              operator: "="
              colValue: open
      col:
        role:
          auditor: '["id","status"]'
"#,
            )
            .expect("rule config"),
        );
        let principal = AuthPrincipal {
            claims: json!({"custom_roles": "auditor"}),
            ..AuthPrincipal::default()
        };

        let filtered = policy
            .filter_http_response(
                "/v1/reports@get",
                &[],
                Some(&principal),
                &json!({}),
                None,
                200,
                br#"[{"id":1,"status":"open","secret":"a"},{"id":2,"status":"closed","secret":"b"}]"#,
            )
            .await
            .expect("response filter execution")
            .expect("filtered response");

        assert_eq!(
            serde_json::from_slice::<JsonValue>(&filtered).expect("json"),
            json!([{"id": 1, "status": "open"}])
        );
    }

    #[tokio::test]
    async fn filter_http_response_rejects_non_json_when_configured() {
        let policy = policy_for_filter("col", json!({"col": {}}));

        let error = policy
            .filter_http_response(
                "/v1/accounts@get",
                &[],
                Some(&auth("teller")),
                &json!({}),
                None,
                200,
                b"not-json",
            )
            .await
            .expect_err("configured response filter must reject non-JSON bodies");

        assert!(matches!(
            error,
            AccessControlResponseFilterError::InvalidJson(_)
        ));
    }

    #[tokio::test]
    async fn filter_http_response_preserves_empty_body_when_configured() {
        let policy = policy_for_filter("col", json!({"col": {}}));

        let filtered = policy
            .filter_http_response(
                "/v1/accounts@get",
                &[],
                Some(&auth("teller")),
                &json!({}),
                None,
                204,
                b"",
            )
            .await
            .expect("empty response filter execution")
            .expect("configured filter result");

        assert!(filtered.is_empty());
    }

    #[tokio::test]
    async fn filter_http_response_rejects_missing_rule_body() {
        let mut policy = policy_for_filter("col", json!({"col": {}}));
        policy.rules.rule_bodies.remove("filter");

        let error = policy
            .filter_http_response(
                "/v1/accounts@get",
                &[],
                Some(&auth("teller")),
                &json!({}),
                None,
                200,
                br#"[{"accountNo":"1","ssn":"secret"}]"#,
            )
            .await
            .expect_err("configured response filter must reject a missing rule body");

        assert_eq!(
            error,
            AccessControlResponseFilterError::RuleNotFound("filter".to_string())
        );
    }

    #[tokio::test]
    async fn filter_http_response_rejects_rule_returning_false() {
        let mut policy = policy_for_filter("col", json!({"col": {}}));
        policy
            .rules
            .rule_bodies
            .get_mut("filter")
            .expect("filter rule")
            .expression = Some("false".to_string());

        let error = policy
            .filter_http_response(
                "/v1/accounts@get",
                &[],
                Some(&auth("teller")),
                &json!({}),
                None,
                200,
                br#"[{"accountNo":"1","ssn":"secret"}]"#,
            )
            .await
            .expect_err("configured response filter must reject a false rule");

        assert_eq!(
            error,
            AccessControlResponseFilterError::RuleRejected("filter".to_string())
        );
    }

    #[tokio::test]
    async fn filter_http_response_rejects_rule_execution_error() {
        let mut policy = policy_for_filter("col", json!({"col": {}}));
        policy
            .rules
            .rule_bodies
            .get_mut("filter")
            .expect("filter rule")
            .expression = None;

        let error = policy
            .filter_http_response(
                "/v1/accounts@get",
                &[],
                Some(&auth("teller")),
                &json!({}),
                None,
                200,
                br#"[{"accountNo":"1","ssn":"secret"}]"#,
            )
            .await
            .expect_err("configured response filter must reject rule execution errors");

        assert!(matches!(
            error,
            AccessControlResponseFilterError::RuleExecution { rule_id, .. }
                if rule_id == "filter"
        ));
    }

    #[tokio::test]
    async fn disabled_access_control_disables_response_filters() {
        let policy = policy_for_filter_with_access(
            "col",
            json!({
                "col": {
                    "role": {
                        "teller": "[\"accountNo\",\"firstName\"]"
                    }
                }
            }),
            AccessControlConfig {
                enabled: false,
                ..AccessControlConfig::default()
            },
        );

        assert!(!policy.authorization_enabled());
        assert!(!policy.has_response_filter("/v1/accounts@get"));

        let filtered = policy
            .filter_http_response(
                "/v1/accounts@get",
                &[],
                Some(&auth("teller")),
                &json!({}),
                None,
                200,
                br#"[{"accountNo":"1","firstName":"A","ssn":"secret"}]"#,
            )
            .await
            .expect("disabled response filtering");
        assert!(filtered.is_none());

        let result = policy
            .filter_mcp_response(
                "accounts",
                "/v1/accounts@get",
                &[],
                Some(&auth("teller")),
                &json!({}),
                None,
                json!({
                    "content": [
                        {
                            "type": "text",
                            "text": "[{\"accountNo\":\"1\",\"firstName\":\"A\",\"ssn\":\"secret\"}]"
                        }
                    ],
                    "structuredContent": [
                        {
                            "accountNo": "1",
                            "firstName": "A",
                            "ssn": "secret"
                        }
                    ]
                }),
            )
            .await;

        assert_eq!(result["structuredContent"][0]["ssn"], "secret");
        assert_eq!(
            result["content"][0]["text"],
            JsonValue::String(
                "[{\"accountNo\":\"1\",\"firstName\":\"A\",\"ssn\":\"secret\"}]".to_string()
            )
        );
    }

    #[tokio::test]
    async fn skipped_endpoint_disables_response_filters() {
        let policy = policy_for_filter_with_access(
            "col",
            json!({
                "col": {
                    "role": {
                        "teller": "[\"accountNo\",\"firstName\"]"
                    }
                }
            }),
            AccessControlConfig {
                skip_path_prefixes: vec!["/v1/accounts".to_string()],
                ..AccessControlConfig::default()
            },
        );

        assert!(policy.authorization_enabled());
        assert_eq!(
            policy
                .authorize_tool(
                    "accounts",
                    "/v1/accounts@get",
                    &[],
                    Some(&auth("teller")),
                    &json!({}),
                    None,
                )
                .await,
            AccessDecision::Allowed
        );
        assert!(!policy.has_response_filter("/v1/accounts@get"));

        let filtered = policy
            .filter_http_response(
                "/v1/accounts@get",
                &[],
                Some(&auth("teller")),
                &json!({}),
                None,
                200,
                br#"[{"accountNo":"1","firstName":"A","ssn":"secret"}]"#,
            )
            .await
            .expect("skipped response filtering");
        assert!(filtered.is_none());

        let result = policy
            .filter_mcp_response(
                "accounts",
                "/v1/accounts@get",
                &[],
                Some(&auth("teller")),
                &json!({}),
                None,
                json!({
                    "content": [
                        {
                            "type": "text",
                            "text": "[{\"accountNo\":\"1\",\"firstName\":\"A\",\"ssn\":\"secret\"}]"
                        }
                    ],
                    "structuredContent": [
                        {
                            "accountNo": "1",
                            "firstName": "A",
                            "ssn": "secret"
                        }
                    ]
                }),
            )
            .await;

        assert_eq!(result["structuredContent"][0]["ssn"], "secret");
        assert_eq!(
            result["content"][0]["text"],
            JsonValue::String(
                "[{\"accountNo\":\"1\",\"firstName\":\"A\",\"ssn\":\"secret\"}]".to_string()
            )
        );
    }

    #[tokio::test]
    async fn skipped_mcp_tool_name_disables_mcp_access_control_only() {
        let policy = policy_for_filter_with_access(
            "col",
            json!({
                "col": {
                    "role": {
                        "teller": "[\"accountNo\",\"firstName\"]"
                    }
                }
            }),
            AccessControlConfig {
                skip_path_prefixes: vec!["local_mcp".to_string()],
                ..AccessControlConfig::default()
            },
        );

        assert!(policy.authorization_enabled());
        assert!(policy.has_response_filter("/v1/accounts@get"));
        assert_eq!(
            policy
                .authorize_tool(
                    "local_mcp_echo",
                    "/v1/accounts@get",
                    &[],
                    Some(&auth("teller")),
                    &json!({}),
                    None,
                )
                .await,
            AccessDecision::Allowed
        );

        let http_filtered = policy
            .filter_http_response(
                "/v1/accounts@get",
                &[],
                Some(&auth("teller")),
                &json!({}),
                None,
                200,
                br#"[{"accountNo":"1","firstName":"A","ssn":"secret"}]"#,
            )
            .await
            .expect("response filter execution")
            .expect("http response remains filtered by endpoint");
        let http_value = serde_json::from_slice::<JsonValue>(&http_filtered).expect("json");
        assert!(http_value[0].get("ssn").is_none());

        let result = policy
            .filter_mcp_response(
                "local_mcp_echo",
                "/v1/accounts@get",
                &[],
                Some(&auth("teller")),
                &json!({}),
                None,
                json!({
                    "content": [
                        {
                            "type": "text",
                            "text": "[{\"accountNo\":\"1\",\"firstName\":\"A\",\"ssn\":\"secret\"}]"
                        }
                    ],
                    "structuredContent": [
                        {
                            "accountNo": "1",
                            "firstName": "A",
                            "ssn": "secret"
                        }
                    ]
                }),
            )
            .await;

        assert_eq!(result["structuredContent"][0]["ssn"], "secret");
        assert_eq!(
            result["content"][0]["text"],
            JsonValue::String(
                "[{\"accountNo\":\"1\",\"firstName\":\"A\",\"ssn\":\"secret\"}]".to_string()
            )
        );
    }

    #[test]
    fn tool_visibility_permission_mode_matches_role_and_group_rules() {
        let policy = AccessControlRuntime::new(
            Some(AccessControlConfig {
                tools_list_access_control: ToolsListAccessControlConfig {
                    mode: ToolsListAccessControlMode::Permission,
                    ..ToolsListAccessControlConfig::default()
                },
                ..AccessControlConfig::default()
            }),
            serde_yaml::from_str::<RuleFileConfig>(
                r#"
ruleBodies:
  allow-role:
    common: Y
    ruleId: allow-role
    ruleName: Allow role
    ruleType: req-acc
    expression: "'role' in auditInfo.subject_claims.ClaimsMap && 'roles' in permission && permission.roles in auditInfo.subject_claims.ClaimsMap.role"
    actions:
      - actionClassName: com.networknt.rule.RoleBasedAccessControlAction
  allow-group:
    common: Y
    ruleId: allow-group
    ruleName: Allow group
    ruleType: req-acc
    expression: "'scp' in auditInfo.subject_claims.ClaimsMap && 'groups' in permission && permission.groups in auditInfo.subject_claims.ClaimsMap.scp"
endpointRules:
  echo@call:
    req-acc:
      - allow-role
    permission:
      roles: account-manager
  /offers@get:
    req-acc:
      - allow-group
    permission:
      groups: portal.w
"#,
            )
            .expect("rule config"),
        );
        let principal = AuthPrincipal {
            role: Some("account-manager".to_string()),
            claims: json!({
                "role": "account-manager",
                "scp": ["portal.w"]
            }),
            ..AuthPrincipal::default()
        };

        assert_eq!(
            policy.tool_visible("local_mcp_echo", "echo@call", Some(&principal)),
            ToolVisibility::Visible
        );
        assert_eq!(
            policy.tool_visible(
                "demo_offer_decision_api_search_offers",
                "/offers@get",
                Some(&principal)
            ),
            ToolVisibility::Visible
        );
        assert_eq!(
            policy.tool_visible(
                "local_mcp_get_random_number",
                "getRandomNumber@call",
                Some(&principal)
            ),
            ToolVisibility::Hidden
        );
    }

    #[test]
    fn access_control_config_accepts_tools_list_access_control() {
        let config = serde_yaml::from_str::<AccessControlConfig>(
            r#"
enabled: true
toolsListAccessControl:
  mode: cel
  unknownRuleFallback: visible
  maxCelEvaluations: 25
  maxCacheEntries: 50
  claimMappings:
    roles:
      - custom_roles
    groups:
      - custom_scope
"#,
        )
        .expect("config");

        assert_eq!(
            config.tools_list_access_control.mode,
            ToolsListAccessControlMode::Cel
        );
        assert_eq!(
            config.tools_list_access_control.unknown_rule_fallback,
            ToolsListUnknownRuleFallback::Visible
        );
        assert_eq!(config.tools_list_access_control.max_cel_evaluations, 25);
        assert_eq!(config.tools_list_access_control.max_cache_entries, 50);
        assert_eq!(
            config
                .tools_list_access_control
                .claim_mappings
                .get("roles")
                .expect("roles mapping"),
            &vec!["custom_roles".to_string()]
        );
    }

    #[test]
    fn global_claim_mappings_override_legacy_tools_list_mappings() {
        let config = serde_yaml::from_str::<AccessControlConfig>(
            r#"
enabled: true
claimMappings:
  roles:
    - global_roles
toolsListAccessControl:
  claimMappings:
    roles:
      - legacy_roles
    groups:
      - legacy_groups
"#,
        )
        .expect("config");
        let policy = AccessControlRuntime::new(Some(config), RuleFileConfig::default());
        let effective = policy.tools_list_access_control().claim_mappings;

        assert_eq!(effective["roles"], vec!["global_roles".to_string()]);
        assert_eq!(effective["groups"], vec!["legacy_groups".to_string()]);
    }

    #[test]
    fn load_access_control_runtime_reads_tools_list_access_control_from_values() {
        let mut values = HashMap::new();
        values.insert("access-control.enabled".to_string(), YamlValue::Bool(true));
        values.insert(
            "access-control.toolsListAccessControl.mode".to_string(),
            YamlValue::String("permission".to_string()),
        );
        values.insert(
            "access-control.toolsListAccessControl.maxCelEvaluations".to_string(),
            YamlValue::Number(serde_yaml::Number::from(25)),
        );
        values.insert(
            "access-control.toolsListAccessControl.maxCacheEntries".to_string(),
            YamlValue::Number(serde_yaml::Number::from(75)),
        );

        let (_config_dir, runtime_config) = runtime_config_with_values(values);
        let policy = load_access_control_runtime(&runtime_config, true)
            .expect("load policy")
            .expect("policy");

        let config = policy.tools_list_access_control();
        assert_eq!(config.mode, ToolsListAccessControlMode::Permission);
        assert_eq!(config.max_cel_evaluations, 25);
        assert_eq!(config.max_cache_entries, 75);
    }

    #[test]
    fn load_access_control_runtime_reads_global_claim_mappings_from_values() {
        let mut values = HashMap::new();
        values.insert("access-control.enabled".to_string(), YamlValue::Bool(true));
        values.insert(
            "access-control.claimMappings.roles".to_string(),
            serde_yaml::from_str("[custom_roles]").expect("roles mapping"),
        );

        let (_config_dir, runtime_config) = runtime_config_with_values(values);
        let policy = load_access_control_runtime(&runtime_config, true)
            .expect("load policy")
            .expect("policy");

        assert_eq!(
            policy.tools_list_access_control().claim_mappings["roles"],
            vec!["custom_roles".to_string()]
        );
    }

    #[test]
    fn tool_visibility_uses_default_deny_fallbacks() {
        let rules = serde_yaml::from_str::<RuleFileConfig>(
            r#"
endpointRules:
  /health@get:
    permission: {}
"#,
        )
        .expect("rule config");
        let deny_policy = AccessControlRuntime::new(
            Some(AccessControlConfig {
                tools_list_access_control: ToolsListAccessControlConfig {
                    mode: ToolsListAccessControlMode::Permission,
                    ..ToolsListAccessControlConfig::default()
                },
                ..AccessControlConfig::default()
            }),
            rules.clone(),
        );
        let allow_policy = AccessControlRuntime::new(
            Some(AccessControlConfig {
                default_deny: false,
                tools_list_access_control: ToolsListAccessControlConfig {
                    mode: ToolsListAccessControlMode::Permission,
                    ..ToolsListAccessControlConfig::default()
                },
                ..AccessControlConfig::default()
            }),
            rules,
        );

        assert_eq!(
            deny_policy.tool_visible("missing", "/missing@get", None),
            ToolVisibility::Hidden
        );
        assert_eq!(
            allow_policy.tool_visible("missing", "/missing@get", None),
            ToolVisibility::Visible
        );
        assert_eq!(
            deny_policy.tool_visible("health", "/health@get", None),
            ToolVisibility::Hidden
        );
        assert_eq!(
            allow_policy.tool_visible("health", "/health@get", None),
            ToolVisibility::Visible
        );
    }

    #[test]
    fn tool_visibility_uses_explicit_visibility_metadata() {
        let policy = AccessControlRuntime::new(
            Some(AccessControlConfig {
                tools_list_access_control: ToolsListAccessControlConfig {
                    mode: ToolsListAccessControlMode::Permission,
                    ..ToolsListAccessControlConfig::default()
                },
                ..AccessControlConfig::default()
            }),
            serde_yaml::from_str::<RuleFileConfig>(
                r#"
ruleBodies:
  custom:
    common: Y
    ruleId: custom
    ruleName: Custom
    ruleType: req-acc
    expression: "toolArguments.accountId == auditInfo.subject_claims.ClaimsMap.uid"
endpointRules:
  accounts@call:
    req-acc:
      - custom
    visibility:
      groups: portal.w
"#,
            )
            .expect("rule config"),
        );
        let principal = AuthPrincipal {
            claims: json!({"scp": "portal.w"}),
            ..AuthPrincipal::default()
        };

        assert_eq!(
            policy.tool_visible("accounts", "accounts@call", Some(&principal)),
            ToolVisibility::Visible
        );
    }

    #[test]
    fn tool_visibility_uses_configured_claim_mappings() {
        let policy = AccessControlRuntime::new(
            Some(AccessControlConfig {
                tools_list_access_control: ToolsListAccessControlConfig {
                    mode: ToolsListAccessControlMode::Permission,
                    claim_mappings: BTreeMap::from([(
                        "roles".to_string(),
                        vec!["custom_roles".to_string()],
                    )]),
                    ..ToolsListAccessControlConfig::default()
                },
                ..AccessControlConfig::default()
            }),
            serde_yaml::from_str::<RuleFileConfig>(
                r#"
ruleBodies:
  allow-role:
    common: Y
    ruleId: allow-role
    ruleName: Allow role
    ruleType: req-acc
    expression: "'custom_roles' in auditInfo.subject_claims.ClaimsMap && 'roles' in permission"
endpointRules:
  reports@call:
    req-acc:
      - allow-role
    permission:
      roles: auditor
"#,
            )
            .expect("rule config"),
        );
        let principal = AuthPrincipal {
            claims: json!({"custom_roles": "auditor"}),
            ..AuthPrincipal::default()
        };

        assert_eq!(
            policy.tool_visible("reports", "reports@call", Some(&principal)),
            ToolVisibility::Visible
        );
    }

    #[test]
    fn tool_visibility_ignores_non_authorizing_rules() {
        let policy = AccessControlRuntime::new(
            Some(AccessControlConfig {
                default_deny: false,
                tools_list_access_control: ToolsListAccessControlConfig {
                    mode: ToolsListAccessControlMode::Permission,
                    ..ToolsListAccessControlConfig::default()
                },
                ..AccessControlConfig::default()
            }),
            serde_yaml::from_str::<RuleFileConfig>(
                r#"
ruleBodies:
  audit-only:
    common: Y
    ruleId: audit-only
    ruleName: Audit only
    ruleType: req-acc
    accessControlEffect: telemetry
    expression: "false"
  unknown-authorizing:
    common: Y
    ruleId: unknown-authorizing
    ruleName: Unknown authorizing
    ruleType: req-acc
    expression: "false"
endpointRules:
  audit@call:
    req-acc:
      - audit-only
  protected@call:
    req-acc:
      - unknown-authorizing
"#,
            )
            .expect("rule config"),
        );

        assert_eq!(
            policy.tool_visible("audit", "audit@call", None),
            ToolVisibility::Visible
        );
        assert_eq!(
            policy.tool_visible("protected", "protected@call", None),
            ToolVisibility::Hidden
        );
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
  expression: "'role' in auditInfo.subject_claims.ClaimsMap"
  conditionLanguage: cel
  conditionSecurityProfile: strict
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
    async fn response_column_filter_handles_top_level_objects() {
        let policy = policy_for_filter(
            "col",
            json!({
                "col": {
                    "role": {
                        "mcp-reader": "[\"id\",\"name\"]"
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
                            "text": "{\"id\":1,\"name\":\"acct\",\"active\":true}"
                        }
                    ],
                    "structuredContent": {
                        "id": 1,
                        "name": "acct",
                        "active": true
                    }
                }),
            )
            .await;

        assert_eq!(result["structuredContent"]["id"], 1);
        assert_eq!(result["structuredContent"]["name"], "acct");
        assert!(result["structuredContent"].get("active").is_none());
        assert_eq!(
            result["content"][0]["text"],
            JsonValue::String("{\"id\":1,\"name\":\"acct\"}".to_string())
        );
    }

    #[tokio::test]
    async fn empty_top_level_object_from_column_filter_is_not_an_mcp_error() {
        let policy = policy_for_filter(
            "col",
            json!({
                "col": {
                    "role": {
                        "mcp-reader": "missing"
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
                    "content": [{
                        "type": "text",
                        "text": r#"{"secret":"value"}"#
                    }],
                    "structuredContent": {
                        "secret": "value"
                    }
                }),
            )
            .await;

        assert!(result.get("isError").is_none());
        assert_eq!(result["structuredContent"], json!({}));
        assert_eq!(
            result["content"][0]["text"],
            JsonValue::String("{}".to_string())
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
    async fn mcp_response_filters_support_custom_mapped_dimension() {
        let policy = AccessControlRuntime::new(
            Some(AccessControlConfig {
                claim_mappings: BTreeMap::from([(
                    "tenant".to_string(),
                    vec!["tenant_id".to_string()],
                )]),
                ..AccessControlConfig::default()
            }),
            serde_yaml::from_str::<RuleFileConfig>(
                r#"
ruleBodies:
  row-filter:
    common: Y
    ruleId: row-filter
    ruleName: Row Filter
    ruleType: res-fil
    expression: "true"
    actions:
      - actionClassName: com.networknt.rule.ResponseRowFilterAction
  column-filter:
    common: Y
    ruleId: column-filter
    ruleName: Column Filter
    ruleType: res-fil
    expression: "true"
    actions:
      - actionClassName: com.networknt.rule.ResponseColumnFilterAction
endpointRules:
  reports@call:
    res-fil:
      - row-filter
      - column-filter
    permission:
      row:
        tenant:
          acme:
            - colName: tenant
              operator: "="
              colValue: acme
      col:
        tenant:
          acme: '["id","tenant"]'
"#,
            )
            .expect("rule config"),
        );
        let principal = AuthPrincipal {
            claims: json!({"tenant_id": "acme"}),
            ..AuthPrincipal::default()
        };

        let result = policy
            .filter_mcp_response(
                "reports",
                "reports@call",
                &[],
                Some(&principal),
                &json!({}),
                None,
                json!({
                    "content": [{
                        "type": "text",
                        "text": "[{\"id\":1,\"tenant\":\"acme\",\"secret\":\"a\"},{\"id\":2,\"tenant\":\"other\",\"secret\":\"b\"}]"
                    }]
                }),
            )
            .await;

        assert_eq!(
            result["content"][0]["text"],
            JsonValue::String("[{\"id\":1,\"tenant\":\"acme\"}]".to_string())
        );
    }

    #[test]
    fn row_matches_rejects_unknown_string_and_numeric_operators() {
        let string_filter = RowFilter {
            col_name: "status".to_string(),
            operator: "==".to_string(),
            col_value: "O".to_string(),
        };
        let numeric_filter = RowFilter {
            col_name: "priority".to_string(),
            operator: "approximately".to_string(),
            col_value: "50".to_string(),
        };

        assert!(!row_matches(&json!({"status": "O"}), &[string_filter]));
        assert!(!row_matches(&json!({"priority": 50}), &[numeric_filter]));
    }

    #[test]
    fn row_matches_applies_numeric_range_operator() {
        let filters = [RowFilter {
            col_name: "priority".to_string(),
            operator: "range".to_string(),
            col_value: "[25, 75]".to_string(),
        }];

        assert!(row_matches(&json!({"priority": 25}), &filters));
        assert!(row_matches(&json!({"priority": 75}), &filters));
        assert!(!row_matches(&json!({"priority": 24}), &filters));
        assert!(!row_matches(&json!({"priority": 76}), &filters));
        assert!(!row_matches(
            &json!({"priority": 50}),
            &[RowFilter {
                col_name: "priority".to_string(),
                operator: "range".to_string(),
                col_value: "[invalid, 75]".to_string(),
            }]
        ));
    }

    #[test]
    fn row_matches_rejects_missing_columns_and_non_object_rows() {
        let filters = [RowFilter {
            col_name: "status".to_string(),
            operator: "=".to_string(),
            col_value: "O".to_string(),
        }];

        assert!(!row_matches(&json!({"other": "O"}), &filters));
        assert!(!row_matches(&json!("O"), &filters));
    }

    #[test]
    fn row_filter_list_rejects_malformed_filters_and_missing_claim_values() {
        assert!(
            row_filter_list(&json!([{"operator": "=", "colValue": "O"}]), &json!({})).is_none()
        );
        assert!(
            row_filter_list(
                &json!([{"colName": "status", "operator": "==", "colValue": "O"}]),
                &json!({})
            )
            .is_none()
        );
        assert!(
            row_filter_list(
                &json!([{"colName": "status", "operator": "=", "colValue": "@region"}]),
                &json!({})
            )
            .is_none()
        );
    }

    #[test]
    fn empty_matched_row_filter_group_denies_all_rows() {
        let mut body = json!([{"status": "O"}, {"status": "C"}]);

        apply_row_filter_groups(&mut body, &[Vec::new()], true);

        assert_eq!(body, json!([]));
    }

    #[test]
    fn row_filter_handles_top_level_objects() {
        let filter_groups = [vec![RowFilter {
            col_name: "status".to_string(),
            operator: "=".to_string(),
            col_value: "O".to_string(),
        }]];
        let mut allowed = json!({"status": "O", "secret": "allowed"});
        let mut denied = json!({"status": "C", "secret": "denied"});
        let mut unmatched = json!({"status": "O", "secret": "unmatched"});
        let mut legacy = unmatched.clone();

        let allowed_was_denied = apply_row_filter_groups(&mut allowed, &filter_groups, false);
        let denied_was_denied = apply_row_filter_groups(&mut denied, &filter_groups, false);
        let unmatched_was_denied = apply_row_filter_groups(&mut unmatched, &[], false);
        let legacy_was_denied = apply_row_filter_groups(&mut legacy, &[], true);

        assert!(!allowed_was_denied);
        assert!(denied_was_denied);
        assert!(unmatched_was_denied);
        assert!(!legacy_was_denied);
        assert_eq!(allowed, json!({"status": "O", "secret": "allowed"}));
        assert_eq!(denied, json!({}));
        assert_eq!(unmatched, json!({}));
        assert_eq!(legacy, json!({"status": "O", "secret": "unmatched"}));
    }

    #[test]
    fn every_matched_row_filter_group_must_match() {
        let mut body = json!([
            {"status": "O", "region": "CA"},
            {"status": "O", "region": "US"},
            {"status": "C", "region": "CA"}
        ]);
        let filter_groups = [
            vec![RowFilter {
                col_name: "status".to_string(),
                operator: "=".to_string(),
                col_value: "O".to_string(),
            }],
            vec![RowFilter {
                col_name: "region".to_string(),
                operator: "=".to_string(),
                col_value: "CA".to_string(),
            }],
        ];

        apply_row_filter_groups(&mut body, &filter_groups, false);

        assert_eq!(body, json!([{"status": "O", "region": "CA"}]));
    }

    #[tokio::test]
    async fn response_row_filter_default_include_false_returns_empty_when_no_claim_matches() {
        let policy = policy_for_filter(
            "row",
            json!({
                "row": {
                    "role": {
                        "teller": [
                            {
                                "colName": "accountType",
                                "operator": "=",
                                "colValue": "C"
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
                Some(&auth("manager")),
                &json!({}),
                None,
                json!({
                    "content": [
                        {
                            "type": "text",
                            "text": r#"[{"accountType":"C"},{"accountType":"S"}]"#
                        }
                    ],
                    "structuredContent": [
                        {
                            "accountType": "C"
                        },
                        {
                            "accountType": "S"
                        }
                    ]
                }),
            )
            .await;

        assert_eq!(result["structuredContent"], json!([]));
        assert_eq!(
            result["content"][0]["text"],
            JsonValue::String("[]".to_string())
        );
    }

    #[tokio::test]
    async fn response_row_filter_default_include_true_preserves_legacy_include_all() {
        let policy = policy_for_filter_with_access(
            "row",
            json!({
                "row": {
                    "role": {
                        "teller": [
                            {
                                "colName": "accountType",
                                "operator": "=",
                                "colValue": "C"
                            }
                        ]
                    }
                }
            }),
            AccessControlConfig {
                default_include: true,
                ..AccessControlConfig::default()
            },
        );

        let result = policy
            .filter_mcp_response(
                "accounts",
                "/v1/accounts@get",
                &[],
                Some(&auth("manager")),
                &json!({}),
                None,
                json!({
                    "content": [
                        {
                            "type": "text",
                            "text": r#"[{"accountType":"C"},{"accountType":"S"}]"#
                        }
                    ],
                    "structuredContent": [
                        {
                            "accountType": "C"
                        },
                        {
                            "accountType": "S"
                        }
                    ]
                }),
            )
            .await;

        assert_eq!(
            result["structuredContent"],
            json!([
                {
                    "accountType": "C"
                },
                {
                    "accountType": "S"
                }
            ])
        );
        assert_eq!(
            result["content"][0]["text"],
            JsonValue::String(r#"[{"accountType":"C"},{"accountType":"S"}]"#.to_string())
        );
    }

    #[tokio::test]
    async fn response_row_filter_matching_claim_still_filters_rows() {
        let policy = policy_for_filter(
            "row",
            json!({
                "row": {
                    "role": {
                        "teller": [
                            {
                                "colName": "accountType",
                                "operator": "=",
                                "colValue": "C"
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
                Some(&auth("teller")),
                &json!({}),
                None,
                json!({
                    "content": [
                        {
                            "type": "text",
                            "text": r#"[{"accountType":"C"},{"accountType":"S"}]"#
                        }
                    ],
                    "structuredContent": [
                        {
                            "accountType": "C"
                        },
                        {
                            "accountType": "S"
                        }
                    ]
                }),
            )
            .await;

        assert_eq!(
            result["structuredContent"],
            json!([
                {
                    "accountType": "C"
                }
            ])
        );
        assert_eq!(
            result["content"][0]["text"],
            JsonValue::String(r#"[{"accountType":"C"}]"#.to_string())
        );
    }

    #[tokio::test]
    async fn response_row_filter_denies_non_matching_top_level_object() {
        let policy = policy_for_filter(
            "row",
            json!({
                "row": {
                    "role": {
                        "teller": [
                            {
                                "colName": "accountType",
                                "operator": "=",
                                "colValue": "C"
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
                Some(&auth("teller")),
                &json!({}),
                None,
                json!({
                    "content": [{
                        "type": "text",
                        "text": r#"{"accountType":"S","ssn":"secret"}"#
                    }],
                    "structuredContent": {
                        "accountType": "S",
                        "ssn": "secret"
                    }
                }),
            )
            .await;

        assert_eq!(
            result,
            json!({
                "isError": true,
                "content": [{
                    "type": "text",
                    "text": "Access denied by response filter"
                }]
            })
        );
    }

    #[tokio::test]
    async fn response_row_filter_no_permission_row_block_does_not_empty_response() {
        let policy = AccessControlRuntime::new(
            Some(AccessControlConfig::default()),
            serde_yaml::from_str::<RuleFileConfig>(
                r#"
ruleBodies:
  allow-all:
    common: Y
    ruleId: allow-all
    ruleName: Allow all
    ruleType: req-acc
    expression: "true"
  row-filter:
    common: Y
    ruleId: row-filter
    ruleName: Row filter
    ruleType: res-fil
    actions:
      - actionClassName: com.networknt.rule.ResponseRowFilterAction
endpointRules:
  /v1/accounts@get:
    req-acc:
      - allow-all
    res-fil:
      - row-filter
    permission:
      column:
        role:
          teller: ["accountType"]
"#,
            )
            .expect("rule config"),
        );

        let result = policy
            .filter_mcp_response(
                "accounts",
                "/v1/accounts@get",
                &[],
                Some(&auth("manager")),
                &json!({}),
                None,
                json!({
                    "content": [
                        {
                            "type": "text",
                            "text": r#"[{"accountType":"C"},{"accountType":"S"}]"#
                        }
                    ],
                    "structuredContent": [
                        {
                            "accountType": "C"
                        },
                        {
                            "accountType": "S"
                        }
                    ]
                }),
            )
            .await;

        assert_eq!(
            result["structuredContent"],
            json!([
                {
                    "accountType": "C"
                },
                {
                    "accountType": "S"
                }
            ])
        );
        assert_eq!(
            result["content"][0]["text"],
            JsonValue::String(r#"[{"accountType":"C"},{"accountType":"S"}]"#.to_string())
        );
    }

    #[tokio::test]
    async fn response_filters_apply_row_before_column_even_when_configured_after_column() {
        let policy = AccessControlRuntime::new(
            Some(AccessControlConfig::default()),
            serde_yaml::from_str::<RuleFileConfig>(
                r#"
ruleBodies:
  column:
    common: Y
    ruleId: column
    ruleName: Column Filter
    ruleType: res-fil
    expression: "col != null"
    conditionLanguage: cel
    conditionSecurityProfile: strict
    actions:
      - actionClassName: com.networknt.rule.ResponseColumnFilterAction
  row:
    common: Y
    ruleId: row
    ruleName: Row Filter
    ruleType: res-fil
    expression: "row != null"
    conditionLanguage: cel
    conditionSecurityProfile: strict
    actions:
      - actionClassName: com.networknt.rule.ResponseRowFilterAction
endpointRules:
  /v1/offers@get:
    res-fil:
      - column
      - row
    permission:
      col:
        role:
          teller: '["offerId","title"]'
      row:
        role:
          teller:
            - colName: active
              operator: "="
              colValue: "true"
"#,
            )
            .expect("rule config"),
        );

        let result = policy
            .filter_mcp_response(
                "offers",
                "/v1/offers@get",
                &[],
                Some(&auth("teller")),
                &json!({}),
                None,
                json!({
                    "content": [
                        {
                            "type": "text",
                            "text": r#"[{"offerId":1,"title":"A","active":true},{"offerId":2,"title":"B","active":false},{"offerId":3,"title":"C","active":true}]"#
                        }
                    ]
                }),
            )
            .await;

        assert_eq!(
            result["content"][0]["text"],
            JsonValue::String(
                r#"[{"offerId":1,"title":"A"},{"offerId":3,"title":"C"}]"#.to_string()
            )
        );
    }

    #[tokio::test]
    async fn response_cel_row_filter_drops_non_matching_and_error_rows() {
        let policy = policy_for_cel_row_filter(
            "row.priority < 50 && row.active == true",
            json!({
                "row": {
                    "role": {
                        "mcp-reader": []
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
                            "text": "[{\"id\":1,\"priority\":10,\"active\":true},{\"id\":2,\"priority\":75,\"active\":true},{\"id\":3,\"priority\":20},{\"id\":4,\"priority\":20,\"active\":false}]"
                        }
                    ]
                }),
            )
            .await;

        assert_eq!(
            result["content"][0]["text"],
            JsonValue::String("[{\"active\":true,\"id\":1,\"priority\":10}]".to_string())
        );
    }

    #[tokio::test]
    async fn response_cel_row_filter_denies_non_matching_top_level_object() {
        let policy = policy_for_cel_row_filter(
            "row.priority < 50 && row.active == true",
            json!({
                "row": {
                    "role": {
                        "mcp-reader": []
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
                    "content": [{
                        "type": "text",
                        "text": r#"{"id":2,"priority":75,"active":true}"#
                    }],
                    "structuredContent": {
                        "id": 2,
                        "priority": 75,
                        "active": true
                    }
                }),
            )
            .await;

        assert_eq!(
            result,
            json!({
                "isError": true,
                "content": [{
                    "type": "text",
                    "text": "Access denied by response filter"
                }]
            })
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
