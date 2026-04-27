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
                let plugin_opt = self.action_registry.get(&action.action_ref);
                if let Some(plugin) = plugin_opt {
                    match plugin.execute(context, &action.action_values).await {
                        Ok(res) => {
                            if !res {
                                debug!(
                                    "Action {} returned false for rule {}",
                                    action.action_ref, rule.rule_id
                                );
                                return Ok(false);
                            }
                        }
                        Err(e) => {
                            error!(
                                "Error executing action {} in rule {}: {}",
                                action.action_ref, rule.rule_id, e
                            );
                            return Err(e);
                        }
                    }
                } else {
                    error!(
                        "Action reference not found in registry: {}",
                        action.action_ref
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
            "==" | "eq" | "equals" => self.values_equal(&actual, expected),
            "!=" | "ne" | "notEquals" => !self.values_equal(&actual, expected),
            ">" | "greaterThan" => self.compare_values(&actual, expected, |a, b| a > b),
            "<" | "lessThan" => self.compare_values(&actual, expected, |a, b| a < b),
            ">=" | "greaterThanOrEqual" => self.compare_values(&actual, expected, |a, b| a >= b),
            "<=" | "lessThanOrEqual" => self.compare_values(&actual, expected, |a, b| a <= b),
            "contains" => self.value_contains(&actual, expected),
            "notContains" => !self.value_contains(&actual, expected),
            "containsIgnoreCase" => self
                .value_to_comparable_string(&actual)
                .to_lowercase()
                .contains(&self.value_to_comparable_string(expected).to_lowercase()),
            "inList" => self.value_in_list(&actual, expected),
            "notInList" => !self.value_in_list(&actual, expected),
            "containsAny" => self.value_contains_any(&actual, expected),
            "containsAll" => self.value_contains_all(&actual, expected),
            "containsNone" => !self.value_contains_any(&actual, expected),
            "matches" | "match" => expected
                .as_str()
                .and_then(|pattern| Regex::new(pattern).ok())
                .map(|regex| regex.is_match(&self.value_to_comparable_string(&actual)))
                .unwrap_or(false),
            "notMatch" => !expected
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
            "lengthEquals" => self.compare_length(&actual, expected, |a, b| a == b),
            "lengthGreaterThan" => self.compare_length(&actual, expected, |a, b| a > b),
            "lengthLessThan" => self.compare_length(&actual, expected, |a, b| a < b),
            "exists" | "isNotNull" => !actual.is_null(),
            "notExists" | "isNull" => actual.is_null(),
            "isEmpty" => self.value_is_empty(&actual),
            "isNotEmpty" => !self.value_is_empty(&actual),
            "isBlank" => self.value_to_comparable_string(&actual).trim().is_empty(),
            "isNotBlank" => !self.value_to_comparable_string(&actual).trim().is_empty(),
            "before" => self.compare_values(&actual, expected, |a, b| a < b),
            "after" => self.compare_values(&actual, expected, |a, b| a > b),
            "on" => self.compare_values(&actual, expected, |a, b| a == b),
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

    fn compare_values<F>(&self, actual: &Value, expected: &Value, compare: F) -> bool
    where
        F: Fn(f64, f64) -> bool,
    {
        if let (Some(actual), Some(expected)) =
            (self.value_to_f64(actual), self.value_to_f64(expected))
        {
            return compare(actual, expected);
        }

        let actual = self.value_to_comparable_string(actual);
        let expected = self.value_to_comparable_string(expected);
        compare(actual.as_str().cmp(expected.as_str()) as i32 as f64, 0.0)
    }

    fn compare_length<F>(&self, actual: &Value, expected: &Value, compare: F) -> bool
    where
        F: Fn(usize, usize) -> bool,
    {
        match self.value_to_usize(expected) {
            Some(expected) => compare(self.value_length(actual), expected),
            None => false,
        }
    }

    fn value_in_list(&self, actual: &Value, expected: &Value) -> bool {
        self.expected_values(expected)
            .iter()
            .any(|value| self.values_equal(actual, value))
    }

    fn value_contains_any(&self, actual: &Value, expected: &Value) -> bool {
        self.expected_values(expected)
            .iter()
            .any(|expected| self.value_contains(actual, expected))
    }

    fn value_contains_all(&self, actual: &Value, expected: &Value) -> bool {
        let expected_values = self.expected_values(expected);
        !expected_values.is_empty()
            && expected_values
                .iter()
                .all(|expected| self.value_contains(actual, expected))
    }

    fn expected_values(&self, expected: &Value) -> Vec<Value> {
        match expected {
            Value::Array(values) => values.clone(),
            Value::String(values) => {
                let trimmed = values.trim();
                if trimmed.starts_with('[') && trimmed.ends_with(']') {
                    if let Ok(Value::Array(values)) = serde_json::from_str::<Value>(trimmed) {
                        return values;
                    }
                }
                values
                    .split(',')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(|value| Value::String(value.to_string()))
                    .collect()
            }
            value => vec![value.clone()],
        }
    }

    fn value_is_empty(&self, actual: &Value) -> bool {
        match actual {
            Value::Null => true,
            Value::String(value) => value.is_empty(),
            Value::Array(values) => values.is_empty(),
            Value::Object(map) => map.is_empty(),
            _ => false,
        }
    }

    fn value_length(&self, actual: &Value) -> usize {
        match actual {
            Value::String(value) => value.chars().count(),
            Value::Array(values) => values.len(),
            Value::Object(map) => map.len(),
            Value::Null => 0,
            other => other.to_string().chars().count(),
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

    fn value_to_usize(&self, value: &Value) -> Option<usize> {
        match value {
            Value::Number(number) => number.as_u64().map(|value| value as usize),
            Value::String(value) => value.parse::<usize>().ok(),
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
