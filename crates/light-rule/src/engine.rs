use crate::action::ActionRegistry;
use crate::models::Rule;
use cel_interpreter::extractors::This;
use cel_interpreter::{Context as CelContext, ExecutionError as CelExecutionError};
use cel_interpreter::{Program as CelProgram, Value as CelValue};
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::error::Error as StdError;
use std::sync::{Arc, Mutex};
use thiserror::Error;
use tracing::{debug, error};

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

        items.retain(|item| {
            let mut row_context = cel_context.new_inner_scope();
            if row_context.add_variable("row", item.clone()).is_err() {
                return false;
            }
            matches!(program.execute(&row_context), Ok(CelValue::Bool(true)))
        });
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
