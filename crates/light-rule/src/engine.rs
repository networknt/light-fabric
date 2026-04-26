use crate::action::ActionRegistry;
use crate::models::{Rule, RuleCondition};
use regex::Regex;
use serde_json::Value;
use std::sync::Arc;
use tracing::{debug, error};

/// The core Rule Engine for evaluating conditions and executing actions.
pub struct RuleEngine {
    action_registry: Arc<ActionRegistry>,
}

impl RuleEngine {
    /// Create a new RuleEngine with the given ActionRegistry.
    pub fn new(action_registry: Arc<ActionRegistry>) -> Self {
        Self { action_registry }
    }

    /// Execute a single rule against a context Object (map).
    /// Returns true if rules passed and actions executed successfully, false otherwise.
    pub async fn execute_rule(
        &self,
        rule: &Rule,
        context: &mut Value,
    ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
        debug!("Executing rule {}", rule.rule_id);

        // 1. Evaluate Conditions
        if let Some(ref conditions) = rule.conditions {
            let mut result: Option<bool> = None;
            for (index, cond) in conditions.iter().enumerate() {
                let condition_passed = self.evaluate_condition(cond, context);
                result = Some(if index == 0 {
                    condition_passed
                } else {
                    match cond.join_code.as_deref().unwrap_or("AND") {
                        "OR" | "or" => result.unwrap_or(false) || condition_passed,
                        _ => result.unwrap_or(true) && condition_passed,
                    }
                });
            }

            if result == Some(false) {
                debug!("Conditions failed for rule {}", rule.rule_id);
                return Ok(false);
            }
        }

        // 2. Execute Actions
        if let Some(ref actions) = rule.actions {
            for action in actions {
                let plugin_opt = self.action_registry.get(&action.action_class_name);
                if let Some(plugin) = plugin_opt {
                    match plugin.execute(context, &action.action_values).await {
                        Ok(res) => {
                            if !res {
                                debug!(
                                    "Action {} returned false for rule {}",
                                    action.action_class_name, rule.rule_id
                                );
                                return Ok(false);
                            }
                        }
                        Err(e) => {
                            error!(
                                "Error executing action {} in rule {}: {}",
                                action.action_class_name, rule.rule_id, e
                            );
                            return Err(e);
                        }
                    }
                } else {
                    error!(
                        "Action class not found in registry: {}",
                        action.action_class_name
                    );
                    return Ok(false);
                }
            }
        }

        Ok(true)
    }

    fn evaluate_condition(&self, condition: &RuleCondition, context: &Value) -> bool {
        let operator = condition.operator.as_deref().unwrap_or("==");
        let operand_path = condition.operand.as_deref().unwrap_or("");
        let actual = self
            .lookup_operand(context, operand_path)
            .cloned()
            .unwrap_or(Value::Null);
        let expected = condition.expected.as_ref().unwrap_or(&Value::Null);

        match operator {
            "==" | "eq" => self.values_equal(&actual, expected),
            "!=" | "ne" => !self.values_equal(&actual, expected),
            ">" => self.compare_numbers(&actual, expected, |a, b| a > b),
            "<" => self.compare_numbers(&actual, expected, |a, b| a < b),
            ">=" => self.compare_numbers(&actual, expected, |a, b| a >= b),
            "<=" => self.compare_numbers(&actual, expected, |a, b| a <= b),
            "contains" => self.value_contains(&actual, expected),
            "matches" => expected
                .as_str()
                .and_then(|pattern| Regex::new(pattern).ok())
                .map(|regex| regex.is_match(&self.value_to_comparable_string(&actual)))
                .unwrap_or(false),
            "startsWith" => expected
                .as_str()
                .map(|expected| {
                    self.value_to_comparable_string(&actual)
                        .starts_with(expected)
                })
                .unwrap_or(false),
            "endsWith" => expected
                .as_str()
                .map(|expected| self.value_to_comparable_string(&actual).ends_with(expected))
                .unwrap_or(false),
            "exists" => !actual.is_null(),
            "notExists" => actual.is_null(),
            _ => {
                error!("Unknown operator: {}", operator);
                false
            }
        }
    }

    fn lookup_operand<'a>(&self, context: &'a Value, operand: &str) -> Option<&'a Value> {
        let operand = operand.trim();
        if operand.is_empty() {
            return Some(context);
        }
        if operand.starts_with('/') {
            return context.pointer(operand);
        }

        let path = operand.strip_prefix('$').unwrap_or(operand);
        let path = path.strip_prefix('.').unwrap_or(path);
        if path.is_empty() {
            return Some(context);
        }

        let mut current = context;
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

    fn values_equal(&self, actual: &Value, expected: &Value) -> bool {
        if actual == expected {
            return true;
        }
        match (actual, expected) {
            (Value::String(actual), expected) => {
                actual == &self.value_to_comparable_string(expected)
            }
            (actual, Value::String(expected)) => {
                &self.value_to_comparable_string(actual) == expected
            }
            _ => false,
        }
    }

    fn compare_numbers<F>(&self, actual: &Value, expected: &Value, compare: F) -> bool
    where
        F: Fn(f64, f64) -> bool,
    {
        match (self.value_to_f64(actual), self.value_to_f64(expected)) {
            (Some(actual), Some(expected)) => compare(actual, expected),
            _ => false,
        }
    }

    fn value_contains(&self, actual: &Value, expected: &Value) -> bool {
        match (actual, expected) {
            (Value::String(actual), Value::String(expected)) => actual.contains(expected),
            (Value::Array(values), expected) => values
                .iter()
                .any(|value| self.values_equal(value, expected)),
            (Value::Object(map), Value::String(key)) => map.contains_key(key),
            (Value::Object(map), Value::Object(expected)) => expected.iter().all(|(key, value)| {
                map.get(key)
                    .map(|actual| self.values_equal(actual, value))
                    .unwrap_or(false)
            }),
            _ => false,
        }
    }

    fn value_to_f64(&self, value: &Value) -> Option<f64> {
        match value {
            Value::Number(number) => number.as_f64(),
            Value::String(value) => value.parse::<f64>().ok(),
            _ => None,
        }
    }

    fn value_to_comparable_string(&self, value: &Value) -> String {
        match value {
            Value::String(value) => value.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        }
    }
}
