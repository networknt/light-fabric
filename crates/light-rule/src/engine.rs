use crate::action::ActionRegistry;
use crate::models::Rule;
use cel::extractors::This;
use cel::{Context as CelContext, ExecutionError as CelExecutionError};
use cel::{Program as CelProgram, Value as CelValue};
use serde_json::Value as JsonValue;
use std::any::Any;
use std::collections::HashMap;
use std::error::Error as StdError;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};
use thiserror::Error;
use tracing::{debug, error, warn};

/// The core Rule Engine for evaluating conditions and executing actions.
pub struct RuleEngine {
    action_registry: Arc<ActionRegistry>,
    cel_cache: Mutex<HashMap<String, Arc<CelProgram>>>,
}

const CONDITION_SECURITY_PROFILE_STRICT: &str = "strict";
const CONDITION_SECURITY_PROFILE_STANDARD: &str = "standard";
const CONDITION_SECURITY_PROFILE_INTERNAL_ADMIN: &str = "internal-admin";
const STRICT_CEL_ROOTS: &[&str] = &[
    "auditInfo",
    "headers",
    "toolArguments",
    "endpoint",
    "toolName",
    "correlationId",
    "roles",
    "row",
    "col",
    "statusCode",
    "permission",
];
const MAX_DIAGNOSTIC_NULL_PATHS: usize = 64;
const MAX_DIAGNOSTIC_JSON_DEPTH: usize = 64;
const MAX_DIAGNOSTIC_JSON_NODES: usize = 1_024;
const MAX_DIAGNOSTIC_CONTEXT_DEPTH: usize = 8;
const MAX_DIAGNOSTIC_CONTEXT_NODES: usize = 128;
const MAX_DIAGNOSTIC_CONTEXT_COLLECTION_ITEMS: usize = 10;
const MAX_DIAGNOSTIC_CONTEXT_STRING_CHARS: usize = 256;
const MAX_DIAGNOSTIC_CONTEXT_KEY_CHARS: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CelSecurityProfile {
    Strict,
    Standard,
    InternalAdmin,
}

impl CelSecurityProfile {
    fn requested(value: Option<&str>) -> Result<Self, RuleEngineError> {
        let value = value
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(CONDITION_SECURITY_PROFILE_STRICT)
            .to_lowercase();
        match value.as_str() {
            CONDITION_SECURITY_PROFILE_STRICT => Ok(Self::Strict),
            CONDITION_SECURITY_PROFILE_STANDARD => Ok(Self::Standard),
            CONDITION_SECURITY_PROFILE_INTERNAL_ADMIN => Ok(Self::InternalAdmin),
            other => Err(RuleEngineError::UnsupportedConditionSecurityProfile(
                other.to_string(),
            )),
        }
    }

    fn cache_name(self) -> &'static str {
        match self {
            Self::Strict => CONDITION_SECURITY_PROFILE_STRICT,
            Self::Standard => CONDITION_SECURITY_PROFILE_STANDARD,
            Self::InternalAdmin => CONDITION_SECURITY_PROFILE_INTERNAL_ADMIN,
        }
    }
}

#[derive(Debug, Error)]
enum RuleEngineError {
    #[error("CEL expression is required when conditionLanguage is cel")]
    MissingCelExpression,
    #[error("Unsupported conditionLanguage: {0}")]
    UnsupportedConditionLanguage(String),
    #[error("Unsupported conditionSecurityProfile: {0}")]
    UnsupportedConditionSecurityProfile(String),
    #[error("CEL conditionSecurityProfile internal-admin is disabled by runtime policy")]
    InternalAdminProfileDisabled,
    #[error("CEL security policy violation: {0}")]
    CelSecurityPolicy(String),
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
        Self {
            action_registry,
            cel_cache: Mutex::new(HashMap::new()),
        }
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
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_lowercase);
        let has_expression = rule
            .expression
            .as_deref()
            .map(str::trim)
            .is_some_and(|value| !value.is_empty());
        let has_native_conditions = rule
            .conditions
            .as_ref()
            .is_some_and(|conditions| !conditions.is_empty());
        let conditions_passed = match condition_language.as_deref() {
            Some("cel") => self.evaluate_cel_expression(rule, context)?,
            None if has_expression => self.evaluate_cel_expression(rule, context)?,
            None if !has_native_conditions => true,
            Some("native") | None => {
                return Err(Box::new(RuleEngineError::UnsupportedConditionLanguage(
                    "native".to_string(),
                )));
            }
            Some(other) => {
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

    fn evaluate_cel_expression(
        &self,
        rule: &Rule,
        context: &JsonValue,
    ) -> Result<bool, Box<dyn StdError + Send + Sync>> {
        let expression = rule
            .expression
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or(RuleEngineError::MissingCelExpression)?;
        self.evaluate_cel_predicate(
            &rule.rule_id,
            expression,
            rule.condition_security_profile.as_deref(),
            &rule.rule_type,
            context,
        )
    }

    pub fn retain_cel_predicate_rows(
        &self,
        expression_id: &str,
        expression: &str,
        requested_profile: Option<&str>,
        rule_type: &str,
        base_context: &JsonValue,
        items: &mut Vec<JsonValue>,
    ) -> Result<(), Box<dyn StdError + Send + Sync>> {
        let expression = expression.trim();
        if expression.is_empty() {
            return Err(Box::new(RuleEngineError::MissingCelExpression));
        }
        let requested_profile = CelSecurityProfile::requested(requested_profile)?;
        if requested_profile == CelSecurityProfile::InternalAdmin {
            return Err(Box::new(RuleEngineError::InternalAdminProfileDisabled));
        }
        let effective_profile = effective_cel_profile(requested_profile, rule_type);
        let program = self.compile_cel_program(expression_id, expression, effective_profile)?;
        self.validate_cel_program(&program, effective_profile)?;
        let cel_context = build_cel_context(effective_profile, base_context)?;
        let mut logged_evaluation_failure = false;
        let mut evaluation_failure_count = 0usize;

        items.retain(|item| {
            let mut row_context = cel_context.new_inner_scope();
            if row_context.add_variable("row", item.clone()).is_err() {
                return false;
            }
            match execute_cel_safely(
                expression_id,
                expression,
                !logged_evaluation_failure,
                || program.execute(&row_context),
                || collect_exposed_null_paths(effective_profile, base_context, Some(item)),
            ) {
                Ok(CelValue::Bool(result)) => result,
                Ok(_) => false,
                Err(_) => {
                    logged_evaluation_failure = true;
                    evaluation_failure_count += 1;
                    false
                }
            }
        });
        if evaluation_failure_count > 1 {
            warn!(
                target: "light_rule::cel",
                expressionId = expression_id,
                suppressedEvaluationFailures = evaluation_failure_count - 1,
                "Additional CEL row evaluation failures suppressed"
            );
        }
        Ok(())
    }

    pub fn evaluate_cel_predicate(
        &self,
        expression_id: &str,
        expression: &str,
        requested_profile: Option<&str>,
        rule_type: &str,
        context: &JsonValue,
    ) -> Result<bool, Box<dyn StdError + Send + Sync>> {
        let expression = expression.trim();
        if expression.is_empty() {
            return Err(Box::new(RuleEngineError::MissingCelExpression));
        }
        let requested_profile = CelSecurityProfile::requested(requested_profile)?;
        if requested_profile == CelSecurityProfile::InternalAdmin {
            return Err(Box::new(RuleEngineError::InternalAdminProfileDisabled));
        }
        let effective_profile = effective_cel_profile(requested_profile, rule_type);
        let program = self.compile_cel_program(expression_id, expression, effective_profile)?;
        self.validate_cel_program(&program, effective_profile)?;
        let cel_context = build_cel_context(effective_profile, context)?;

        match execute_cel_safely(
            expression_id,
            expression,
            true,
            || program.execute(&cel_context),
            || collect_exposed_null_paths(effective_profile, context, None),
        )? {
            CelValue::Bool(result) => Ok(result),
            other => Err(Box::new(RuleEngineError::CelNonBoolean(format!(
                "{other:?}"
            )))),
        }
    }

    fn compile_cel_program(
        &self,
        rule_id: &str,
        expression: &str,
        profile: CelSecurityProfile,
    ) -> Result<Arc<CelProgram>, Box<dyn StdError + Send + Sync>> {
        let cache_key = format!("{rule_id}:{}:{expression}", profile.cache_name());
        let mut cache = self
            .cel_cache
            .lock()
            .map_err(|_| RuleEngineError::CelCompile("compile cache lock poisoned".into()))?;
        if let Some(program) = cache.get(&cache_key) {
            return Ok(program.clone());
        }
        let program = Arc::new(
            CelProgram::compile(expression)
                .map_err(|err| RuleEngineError::CelCompile(err.to_string()))?,
        );
        cache.insert(cache_key, program.clone());
        Ok(program)
    }

    fn validate_cel_program(
        &self,
        program: &CelProgram,
        profile: CelSecurityProfile,
    ) -> Result<(), Box<dyn StdError + Send + Sync>> {
        if profile != CelSecurityProfile::Strict {
            return Ok(());
        }
        let references = program.references();
        if references.has_variable("context") {
            return Err(Box::new(RuleEngineError::CelSecurityPolicy(
                "strict profile does not expose full context; use curated root variables".into(),
            )));
        }
        if references.has_function("matches") {
            return Err(Box::new(RuleEngineError::CelSecurityPolicy(
                "strict profile does not allow regex matches".into(),
            )));
        }
        Ok(())
    }
}

fn effective_cel_profile(requested: CelSecurityProfile, rule_type: &str) -> CelSecurityProfile {
    if is_response_phase(rule_type) {
        CelSecurityProfile::Strict
    } else {
        requested
    }
}

fn is_response_phase(rule_type: &str) -> bool {
    rule_type.eq_ignore_ascii_case("res-tra") || rule_type.eq_ignore_ascii_case("res-fil")
}

fn build_cel_context(
    profile: CelSecurityProfile,
    context: &JsonValue,
) -> Result<CelContext<'static>, Box<dyn StdError + Send + Sync>> {
    let mut cel_context = match profile {
        CelSecurityProfile::Strict => CelContext::empty(),
        CelSecurityProfile::Standard | CelSecurityProfile::InternalAdmin => CelContext::default(),
    };
    cel_context.add_function("contains_ignore_case", cel_contains_ignore_case);
    cel_context.add_function("containsIgnoreCase", cel_contains_ignore_case);

    if let JsonValue::Object(map) = context {
        for (key, value) in map {
            if !is_cel_identifier(key) || key == "context" {
                continue;
            }
            match profile {
                CelSecurityProfile::Strict => {
                    if STRICT_CEL_ROOTS.contains(&key.as_str()) {
                        cel_context.add_variable(key.as_str(), value.clone())?;
                    }
                }
                CelSecurityProfile::Standard | CelSecurityProfile::InternalAdmin => {
                    cel_context.add_variable(key.as_str(), value.clone())?;
                }
            }
        }
        if profile == CelSecurityProfile::Strict && !map.contains_key("permission") {
            cel_context.add_variable("permission", JsonValue::Object(serde_json::Map::new()))?;
        }
    } else if profile == CelSecurityProfile::Strict {
        cel_context.add_variable("permission", JsonValue::Object(serde_json::Map::new()))?;
    }
    if profile != CelSecurityProfile::Strict {
        cel_context.add_variable("context", context.clone())?;
    }
    Ok(cel_context)
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

#[derive(Debug, Default)]
struct NullPathDiagnostics {
    paths: Vec<String>,
    truncated: bool,
    visited_nodes: usize,
    evaluation_context: JsonValue,
    evaluation_context_truncated: bool,
}

fn execute_cel_safely<F, D>(
    expression_id: &str,
    expression: &str,
    log_failure: bool,
    execute: F,
    diagnostics: D,
) -> Result<CelValue, RuleEngineError>
where
    F: FnOnce() -> Result<CelValue, CelExecutionError>,
    D: FnOnce() -> NullPathDiagnostics,
{
    match catch_unwind(AssertUnwindSafe(execute)) {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(execution_error)) => {
            if log_failure {
                let diagnostics = collect_diagnostics_safely(diagnostics);
                warn!(
                    target: "light_rule::cel",
                    expressionId = expression_id,
                    expression,
                    error = %execution_error,
                    evaluationContext = %diagnostics.evaluation_context,
                    evaluationContextTruncated = diagnostics.evaluation_context_truncated,
                    candidateNullPaths = ?diagnostics.paths,
                    candidateNullPathsTruncated = diagnostics.truncated,
                    "CEL expression evaluation failed"
                );
            }
            Err(RuleEngineError::CelEvaluate(execution_error.to_string()))
        }
        Err(panic_payload) => {
            let panic_message = panic_payload_message(panic_payload.as_ref());
            if log_failure {
                let diagnostics = collect_diagnostics_safely(diagnostics);
                error!(
                    target: "light_rule::cel",
                    expressionId = expression_id,
                    expression,
                    panic = %panic_message,
                    evaluationContext = %diagnostics.evaluation_context,
                    evaluationContextTruncated = diagnostics.evaluation_context_truncated,
                    candidateNullPaths = ?diagnostics.paths,
                    candidateNullPathsTruncated = diagnostics.truncated,
                    "CEL interpreter panic contained"
                );
            }
            Err(RuleEngineError::CelEvaluate(format!(
                "CEL interpreter panicked: {panic_message}"
            )))
        }
    }
}

fn collect_diagnostics_safely<D>(diagnostics: D) -> NullPathDiagnostics
where
    D: FnOnce() -> NullPathDiagnostics,
{
    catch_unwind(AssertUnwindSafe(diagnostics)).unwrap_or_default()
}

fn panic_payload_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

fn collect_exposed_null_paths(
    profile: CelSecurityProfile,
    context: &JsonValue,
    row: Option<&JsonValue>,
) -> NullPathDiagnostics {
    let (evaluation_context, evaluation_context_truncated) =
        diagnostic_evaluation_context(profile, context, row);
    let mut diagnostics = NullPathDiagnostics {
        evaluation_context,
        evaluation_context_truncated,
        ..NullPathDiagnostics::default()
    };

    if let Some(row) = row {
        collect_null_paths(row, "$.row".to_string(), 0, &mut diagnostics);
    }

    if diagnostics.truncated {
        return diagnostics;
    }

    if profile != CelSecurityProfile::Strict {
        collect_null_paths(context, "$".to_string(), 0, &mut diagnostics);
    } else if let JsonValue::Object(map) = context {
        for (key, value) in map {
            if !is_cel_identifier(key) || key == "context" {
                continue;
            }
            if !STRICT_CEL_ROOTS.contains(&key.as_str()) {
                continue;
            }
            let path = json_object_path("$", key);
            collect_null_paths(value, path, 0, &mut diagnostics);
            if diagnostics.truncated {
                break;
            }
        }
    }

    diagnostics
}

fn diagnostic_evaluation_context(
    profile: CelSecurityProfile,
    context: &JsonValue,
    row: Option<&JsonValue>,
) -> (JsonValue, bool) {
    let mut budget = DiagnosticContextBudget::new();
    let mut evaluation_context = serde_json::Map::new();

    // A row predicate most often fails because of the current row, so preserve
    // it before the shared context consumes the diagnostic budget.
    if let Some(row) = row {
        insert_diagnostic_context_value(&mut evaluation_context, "row", row, &mut budget);
    }

    // After a row snapshot, preserve permission before the remaining shared
    // context roots because it is the most common access-rule failure surface.
    if profile == CelSecurityProfile::Strict {
        if let Some(permission) = context.get("permission") {
            insert_diagnostic_context_value(
                &mut evaluation_context,
                "permission",
                permission,
                &mut budget,
            );
        } else {
            insert_diagnostic_context_value(
                &mut evaluation_context,
                "permission",
                &JsonValue::Object(serde_json::Map::new()),
                &mut budget,
            );
        }
    }

    if let JsonValue::Object(map) = context {
        for (key, value) in map {
            if (row.is_some() && key == "row")
                || (profile == CelSecurityProfile::Strict && key == "permission")
            {
                continue;
            }
            if profile == CelSecurityProfile::Strict
                && (!is_cel_identifier(key)
                    || key == "context"
                    || !STRICT_CEL_ROOTS.contains(&key.as_str()))
            {
                continue;
            }
            if budget.exhausted() {
                budget.truncated = true;
                break;
            }
            insert_diagnostic_context_value(&mut evaluation_context, key, value, &mut budget);
        }
    } else if profile != CelSecurityProfile::Strict {
        insert_diagnostic_context_value(&mut evaluation_context, "context", context, &mut budget);
    }

    (JsonValue::Object(evaluation_context), budget.truncated)
}

struct DiagnosticContextBudget {
    remaining_nodes: usize,
    truncated: bool,
}

impl DiagnosticContextBudget {
    fn new() -> Self {
        Self {
            remaining_nodes: MAX_DIAGNOSTIC_CONTEXT_NODES,
            truncated: false,
        }
    }

    fn exhausted(&self) -> bool {
        self.remaining_nodes == 0
    }
}

fn insert_diagnostic_context_value(
    output: &mut serde_json::Map<String, JsonValue>,
    key: &str,
    value: &JsonValue,
    budget: &mut DiagnosticContextBudget,
) {
    if budget.exhausted() {
        budget.truncated = true;
        return;
    }

    let key = bounded_diagnostic_text(key, MAX_DIAGNOSTIC_CONTEXT_KEY_CHARS, budget);
    let key = unique_diagnostic_key(output, key);
    output.insert(key, bounded_diagnostic_value(value, 0, budget));
}

fn bounded_diagnostic_value(
    value: &JsonValue,
    depth: usize,
    budget: &mut DiagnosticContextBudget,
) -> JsonValue {
    if budget.exhausted() {
        budget.truncated = true;
        return JsonValue::String("<truncated>".to_string());
    }
    budget.remaining_nodes -= 1;

    if depth >= MAX_DIAGNOSTIC_CONTEXT_DEPTH {
        budget.truncated = true;
        return JsonValue::String("<truncated: maximum depth>".to_string());
    }

    match value {
        JsonValue::Null => JsonValue::Null,
        JsonValue::Bool(value) => JsonValue::Bool(*value),
        JsonValue::Number(value) => JsonValue::Number(value.clone()),
        JsonValue::String(value) => JsonValue::String(bounded_diagnostic_text(
            value,
            MAX_DIAGNOSTIC_CONTEXT_STRING_CHARS,
            budget,
        )),
        JsonValue::Array(values) => {
            let mut output =
                Vec::with_capacity(values.len().min(MAX_DIAGNOSTIC_CONTEXT_COLLECTION_ITEMS));
            for value in values.iter().take(MAX_DIAGNOSTIC_CONTEXT_COLLECTION_ITEMS) {
                if budget.exhausted() {
                    budget.truncated = true;
                    break;
                }
                output.push(bounded_diagnostic_value(value, depth + 1, budget));
            }
            if output.len() < values.len() {
                budget.truncated = true;
            }
            JsonValue::Array(output)
        }
        JsonValue::Object(values) => {
            let mut output = serde_json::Map::new();
            for (key, value) in values.iter().take(MAX_DIAGNOSTIC_CONTEXT_COLLECTION_ITEMS) {
                if budget.exhausted() {
                    budget.truncated = true;
                    break;
                }
                let key = bounded_diagnostic_text(key, MAX_DIAGNOSTIC_CONTEXT_KEY_CHARS, budget);
                let key = unique_diagnostic_key(&output, key);
                output.insert(key, bounded_diagnostic_value(value, depth + 1, budget));
            }
            if output.len() < values.len() {
                budget.truncated = true;
            }
            JsonValue::Object(output)
        }
    }
}

fn bounded_diagnostic_text(
    value: &str,
    max_chars: usize,
    budget: &mut DiagnosticContextBudget,
) -> String {
    let mut chars = value.chars();
    let prefix: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        budget.truncated = true;
        format!("{prefix}…")
    } else {
        prefix
    }
}

fn unique_diagnostic_key(output: &serde_json::Map<String, JsonValue>, key: String) -> String {
    if !output.contains_key(&key) {
        return key;
    }

    for suffix in 2usize.. {
        let candidate = format!("{key}#{suffix}");
        if !output.contains_key(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

fn collect_null_paths(
    value: &JsonValue,
    path: String,
    depth: usize,
    diagnostics: &mut NullPathDiagnostics,
) {
    if diagnostics.paths.len() >= MAX_DIAGNOSTIC_NULL_PATHS
        || depth >= MAX_DIAGNOSTIC_JSON_DEPTH
        || diagnostics.visited_nodes >= MAX_DIAGNOSTIC_JSON_NODES
    {
        diagnostics.truncated = true;
        return;
    }
    diagnostics.visited_nodes += 1;

    match value {
        JsonValue::Null => diagnostics.paths.push(path),
        JsonValue::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                collect_null_paths(value, format!("{path}[{index}]"), depth + 1, diagnostics);
                if diagnostics.truncated {
                    break;
                }
            }
        }
        JsonValue::Object(values) => {
            for (key, value) in values {
                collect_null_paths(
                    value,
                    json_object_path(path.as_str(), key),
                    depth + 1,
                    diagnostics,
                );
                if diagnostics.truncated {
                    break;
                }
            }
        }
        _ => {}
    }
}

fn json_object_path(parent: &str, key: &str) -> String {
    if is_cel_identifier(key) {
        format!("{parent}.{key}")
    } else {
        let escaped = key.replace('\\', "\\\\").replace('\'', "\\'");
        format!("{parent}['{escaped}']")
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn contains_interpreter_panics_and_reports_candidate_null_paths() {
        let context = json!({
            "permission": {
                "roles": null
            }
        });

        let error = execute_cel_safely(
            "panic-test",
            "permission.roles.exists(r, true)",
            true,
            || -> Result<CelValue, CelExecutionError> { panic!("synthetic CEL panic") },
            || collect_exposed_null_paths(CelSecurityProfile::Strict, &context, None),
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("CEL interpreter panicked: synthetic CEL panic")
        );
    }

    #[test]
    fn collects_exposed_evaluation_context_and_null_paths() {
        let context = json!({
            "permission": {
                "roles": null
            },
            "headers": {
                "x-owner-id": null
            },
            "internalState": {
                "secret": null
            }
        });

        let diagnostics = collect_exposed_null_paths(CelSecurityProfile::Strict, &context, None);

        assert!(
            diagnostics
                .paths
                .contains(&"$.permission.roles".to_string())
        );
        assert!(
            diagnostics
                .paths
                .contains(&"$.headers['x-owner-id']".to_string())
        );
        assert!(
            diagnostics
                .paths
                .iter()
                .all(|path| !path.contains("internalState") && !path.contains("secret"))
        );
        assert_eq!(
            diagnostics.evaluation_context["permission"]["roles"],
            JsonValue::Null
        );
        assert_eq!(
            diagnostics.evaluation_context["headers"]["x-owner-id"],
            JsonValue::Null
        );
        assert!(
            diagnostics
                .evaluation_context
                .get("internalState")
                .is_none()
        );
        assert!(!diagnostics.truncated);
        assert!(!diagnostics.evaluation_context_truncated);
    }

    #[test]
    fn bounds_diagnostic_evaluation_context() {
        let context = json!({
            "toolArguments": {
                "description": "x".repeat(MAX_DIAGNOSTIC_CONTEXT_STRING_CHARS + 100),
                "items": (0..100).collect::<Vec<_>>()
            }
        });

        let diagnostics = collect_exposed_null_paths(CelSecurityProfile::Standard, &context, None);
        let tool_arguments = &diagnostics.evaluation_context["toolArguments"];
        let description = tool_arguments["description"].as_str().unwrap();
        let items = tool_arguments["items"].as_array().unwrap();

        assert_eq!(
            description.chars().count(),
            MAX_DIAGNOSTIC_CONTEXT_STRING_CHARS + 1
        );
        assert!(description.ends_with('…'));
        assert_eq!(items.len(), MAX_DIAGNOSTIC_CONTEXT_COLLECTION_ITEMS);
        assert!(diagnostics.evaluation_context_truncated);
    }

    #[test]
    fn bounds_null_path_diagnostic_traversal() {
        let context = json!({
            "toolArguments": (0..MAX_DIAGNOSTIC_JSON_NODES + 100).collect::<Vec<_>>()
        });

        let diagnostics = collect_exposed_null_paths(CelSecurityProfile::Strict, &context, None);

        assert_eq!(diagnostics.visited_nodes, MAX_DIAGNOSTIC_JSON_NODES);
        assert!(diagnostics.truncated);
    }
}
