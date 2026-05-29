use crate::action::ActionRegistry;
use crate::models::{Rule, RuleCondition};
use cel_interpreter::extractors::This;
use cel_interpreter::{Context as CelContext, ExecutionError as CelExecutionError};
use cel_interpreter::{Program as CelProgram, Value as CelValue};
use regex::Regex;
use serde_json::Value as JsonValue;
use std::error::Error as StdError;
use std::sync::Arc;
use thiserror::Error;
use tracing::{debug, error};

/// The core Rule Engine for evaluating conditions and executing actions.
pub struct RuleEngine {
    action_registry: Arc<ActionRegistry>,
}

#[derive(Debug, Error)]
enum RuleEngineError {
    #[error("CEL expression is required when conditionLanguage is cel")]
    MissingCelExpression,
    #[error("Unsupported conditionLanguage: {0}")]
    UnsupportedConditionLanguage(String),
    #[error("Failed to compile CEL expression: {0}")]
    CelCompile(String),
    #[error("Failed to evaluate CEL expression: {0}")]
    CelEvaluate(String),
    #[error("CEL expression must return a boolean, got {0}")]
    CelNonBoolean(String),
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
        context: &mut JsonValue,
    ) -> Result<bool, Box<dyn StdError + Send + Sync>> {
        debug!("Executing rule {}", rule.rule_id);

        // 1. Evaluate Conditions
        let condition_language = rule
            .condition_language
            .as_deref()
            .unwrap_or("native")
            .trim()
            .to_lowercase();
        let conditions_passed = match condition_language.as_str() {
            "native" | "" => self.evaluate_native_conditions(rule, context),
            "cel" => self.evaluate_cel_expression(rule.expression.as_deref(), context)?,
            other => {
                return Err(Box::new(RuleEngineError::UnsupportedConditionLanguage(
                    other.to_string(),
                )));
            }
        };

        if !conditions_passed {
            debug!("Conditions failed for rule {}", rule.rule_id);
            return Ok(false);
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

    fn evaluate_native_conditions(&self, rule: &Rule, context: &JsonValue) -> bool {
        let Some(ref conditions) = rule.conditions else {
            return true;
        };
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
        result.unwrap_or(true)
    }

    fn evaluate_cel_expression(
        &self,
        expression: Option<&str>,
        context: &JsonValue,
    ) -> Result<bool, Box<dyn StdError + Send + Sync>> {
        let expression = expression
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or(RuleEngineError::MissingCelExpression)?;
        let program = CelProgram::compile(expression)
            .map_err(|err| RuleEngineError::CelCompile(err.to_string()))?;
        let mut cel_context = CelContext::default();
        cel_context.add_function("contains_ignore_case", cel_contains_ignore_case);
        cel_context.add_function("containsIgnoreCase", cel_contains_ignore_case);

        if let JsonValue::Object(map) = context {
            for (key, value) in map {
                if is_cel_identifier(key) && key != "context" {
                    cel_context.add_variable(key.as_str(), value.clone())?;
                }
            }
        }
        cel_context.add_variable("context", context.clone())?;

        match program
            .execute(&cel_context)
            .map_err(|err| RuleEngineError::CelEvaluate(err.to_string()))?
        {
            CelValue::Bool(result) => Ok(result),
            other => Err(Box::new(RuleEngineError::CelNonBoolean(format!(
                "{other:?}"
            )))),
        }
    }

    fn evaluate_condition(&self, condition: &RuleCondition, context: &JsonValue) -> bool {
        let operator = condition.operator.as_deref().unwrap_or("==");
        let operand_path = condition.operand.as_deref().unwrap_or("");
        let actual = self
            .lookup_operand(context, operand_path)
            .cloned()
            .unwrap_or(JsonValue::Null);
        let expected = condition.expected.as_ref().unwrap_or(&JsonValue::Null);

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

    fn lookup_operand<'a>(&self, context: &'a JsonValue, operand: &str) -> Option<&'a JsonValue> {
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

    fn values_equal(&self, actual: &JsonValue, expected: &JsonValue) -> bool {
        if actual == expected {
            return true;
        }
        match (actual, expected) {
            (JsonValue::String(actual), expected) => {
                actual == &self.value_to_comparable_string(expected)
            }
            (actual, JsonValue::String(expected)) => {
                &self.value_to_comparable_string(actual) == expected
            }
            _ => false,
        }
    }

    fn compare_values<F>(&self, actual: &JsonValue, expected: &JsonValue, compare: F) -> bool
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

    fn compare_length<F>(&self, actual: &JsonValue, expected: &JsonValue, compare: F) -> bool
    where
        F: Fn(usize, usize) -> bool,
    {
        match self.value_to_usize(expected) {
            Some(expected) => compare(self.value_length(actual), expected),
            None => false,
        }
    }

    fn value_in_list(&self, actual: &JsonValue, expected: &JsonValue) -> bool {
        self.expected_values(expected)
            .iter()
            .any(|value| self.values_equal(actual, value))
    }

    fn value_contains_any(&self, actual: &JsonValue, expected: &JsonValue) -> bool {
        self.expected_values(expected)
            .iter()
            .any(|expected| self.value_contains(actual, expected))
    }

    fn value_contains_all(&self, actual: &JsonValue, expected: &JsonValue) -> bool {
        let expected_values = self.expected_values(expected);
        !expected_values.is_empty()
            && expected_values
                .iter()
                .all(|expected| self.value_contains(actual, expected))
    }

    fn expected_values(&self, expected: &JsonValue) -> Vec<JsonValue> {
        match expected {
            JsonValue::Array(values) => values.clone(),
            JsonValue::String(values) => {
                let trimmed = values.trim();
                if trimmed.starts_with('[') && trimmed.ends_with(']') {
                    if let Ok(JsonValue::Array(values)) = serde_json::from_str::<JsonValue>(trimmed)
                    {
                        return values;
                    }
                }
                values
                    .split(',')
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(|value| JsonValue::String(value.to_string()))
                    .collect()
            }
            value => vec![value.clone()],
        }
    }

    fn value_is_empty(&self, actual: &JsonValue) -> bool {
        match actual {
            JsonValue::Null => true,
            JsonValue::String(value) => value.is_empty(),
            JsonValue::Array(values) => values.is_empty(),
            JsonValue::Object(map) => map.is_empty(),
            _ => false,
        }
    }

    fn value_length(&self, actual: &JsonValue) -> usize {
        match actual {
            JsonValue::String(value) => value.chars().count(),
            JsonValue::Array(values) => values.len(),
            JsonValue::Object(map) => map.len(),
            JsonValue::Null => 0,
            other => other.to_string().chars().count(),
        }
    }

    fn value_contains(&self, actual: &JsonValue, expected: &JsonValue) -> bool {
        match (actual, expected) {
            (JsonValue::String(actual), JsonValue::String(expected)) => actual.contains(expected),
            (JsonValue::Array(values), expected) => values
                .iter()
                .any(|value| self.values_equal(value, expected)),
            (JsonValue::Object(map), JsonValue::String(key)) => map.contains_key(key),
            (JsonValue::Object(map), JsonValue::Object(expected)) => {
                expected.iter().all(|(key, value)| {
                    map.get(key)
                        .map(|actual| self.values_equal(actual, value))
                        .unwrap_or(false)
                })
            }
            _ => false,
        }
    }

    fn value_to_f64(&self, value: &JsonValue) -> Option<f64> {
        match value {
            JsonValue::Number(number) => number.as_f64(),
            JsonValue::String(value) => value.parse::<f64>().ok(),
            _ => None,
        }
    }

    fn value_to_usize(&self, value: &JsonValue) -> Option<usize> {
        match value {
            JsonValue::Number(number) => number.as_u64().map(|value| value as usize),
            JsonValue::String(value) => value.parse::<usize>().ok(),
            _ => None,
        }
    }

    fn value_to_comparable_string(&self, value: &JsonValue) -> String {
        match value {
            JsonValue::String(value) => value.clone(),
            JsonValue::Null => String::new(),
            other => other.to_string(),
        }
    }
}

fn is_cel_identifier(value: &str) -> bool {
    if matches!(value, "true" | "false" | "null" | "in") {
        return false;
    }
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if first != '_' && !first.is_ascii_alphabetic() {
        return false;
    }
    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn cel_contains_ignore_case(
    This(this): This<CelValue>,
    expected: CelValue,
) -> Result<CelValue, CelExecutionError> {
    let CelValue::String(actual) = this else {
        return Ok(CelValue::Bool(false));
    };
    let CelValue::String(expected) = expected else {
        return Ok(CelValue::Bool(false));
    };
    Ok(CelValue::Bool(
        actual.to_lowercase().contains(&expected.to_lowercase()),
    ))
}
