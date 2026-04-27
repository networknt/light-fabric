use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// Represents a configuration of rules and their endpoint trigger mappings.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RuleConfig {
    /// A map of all rule definitions, keyed by rule_id.
    pub rule_bodies: HashMap<String, Rule>,
    /// Maps service entries (e.g., endpoints) to rule types (e.g., req-tra, res-tra) to lists of rule_ids.
    pub endpoint_rules: HashMap<String, EndpointConfig>,
}

/// A wrapper for the configuration details of a specific endpoint or service entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EndpointConfig {
    /// Full map of rule types (e.g. req-tra, res-tra, access-control) mapping to lists of rule IDs.
    Map(HashMap<String, Value>),
}

/// The core Rule definition.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rule {
    pub rule_id: String,
    pub rule_name: String,
    pub rule_type: String,
    pub common: String,
    pub host_id: Option<String>,
    pub rule_desc: Option<String>,
    pub version: Option<String>,
    pub author: Option<String>,
    pub updated_at: Option<String>,
    pub conditions: Option<Vec<RuleCondition>>,
    pub actions: Option<Vec<RuleAction>>,
}

/// A condition evaluated within a rule.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleCondition {
    pub condition_id: Option<String>,
    pub condition_desc: Option<String>,
    pub operator: Option<String>,
    pub operand: Option<String>,
    pub expected: Option<Value>,
    pub join_code: Option<String>, // AND, OR
}

/// An action to execute if rule conditions pass.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleAction {
    pub action_id: Option<String>,
    pub action_desc: Option<String>,
    pub action_ref: String,
    pub action_values: Option<Value>,
}
