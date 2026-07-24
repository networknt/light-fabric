use crate::action::ActionRegistry;
use crate::models::Rule;
// Reference projection is coupled to the public `cel 0.14` AST and operator
// names. Revalidate this walker whenever the pinned CEL crate is upgraded.
use cel::common::ast::operators::{INDEX, OPT_INDEX, OPT_SELECT};
use cel::common::ast::{EntryExpr, Expr, IdedExpr, LiteralValue};
use cel::extractors::This;
use cel::{Context as CelContext, ExecutionError as CelExecutionError};
use cel::{Program as CelProgram, Value as CelValue};
use serde_json::Value as JsonValue;
use std::any::Any;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::error::Error as StdError;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};
use thiserror::Error;
use tracing::{debug, error, trace, warn};

/// The core Rule Engine for evaluating conditions and executing actions.
pub struct RuleEngine {
    action_registry: Arc<ActionRegistry>,
    cel_cache: Mutex<HashMap<String, Arc<CelProgram>>>,
    log_full_cel_context: bool,
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
            log_full_cel_context: false,
        }
    }

    /// Enables full-value CEL context fields in trace diagnostics.
    ///
    /// The default emits only structural metadata for statically referenced
    /// context properties. Full values are intended for local development.
    pub fn with_log_full_cel_context(mut self, enabled: bool) -> Self {
        self.log_full_cel_context = enabled;
        self
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
        let mut logged_condition_non_match = false;
        let mut condition_non_match_count = 0usize;
        let trace_context = tracing::enabled!(target: "light_rule::cel", tracing::Level::TRACE);

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
                || {
                    collect_referenced_diagnostics(
                        &program,
                        effective_profile,
                        base_context,
                        Some(item),
                        self.log_full_cel_context,
                    )
                },
            ) {
                Ok(CelValue::Bool(result)) => {
                    if !result {
                        condition_non_match_count += 1;
                        if trace_context && !logged_condition_non_match {
                            let diagnostics = collect_referenced_diagnostics(
                                &program,
                                effective_profile,
                                base_context,
                                Some(item),
                                self.log_full_cel_context,
                            );
                            trace_cel_context(
                                expression_id,
                                expression,
                                "condition_not_matched",
                                &diagnostics,
                            );
                            logged_condition_non_match = true;
                        }
                    }
                    result
                }
                Ok(_) => {
                    condition_non_match_count += 1;
                    if trace_context && !logged_condition_non_match {
                        let diagnostics = collect_referenced_diagnostics(
                            &program,
                            effective_profile,
                            base_context,
                            Some(item),
                            self.log_full_cel_context,
                        );
                        trace_cel_context(
                            expression_id,
                            expression,
                            "non_boolean_result",
                            &diagnostics,
                        );
                        logged_condition_non_match = true;
                    }
                    false
                }
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
        if trace_context && condition_non_match_count > 1 {
            trace!(
                target: "light_rule::cel",
                expressionId = expression_id,
                suppressedConditionNonMatches = condition_non_match_count - 1,
                "Additional CEL row condition non-matches suppressed"
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
            || {
                collect_referenced_diagnostics(
                    &program,
                    effective_profile,
                    context,
                    None,
                    self.log_full_cel_context,
                )
            },
        )? {
            CelValue::Bool(result) => {
                if !result && tracing::enabled!(target: "light_rule::cel", tracing::Level::TRACE) {
                    let diagnostics = collect_referenced_diagnostics(
                        &program,
                        effective_profile,
                        context,
                        None,
                        self.log_full_cel_context,
                    );
                    trace_cel_context(
                        expression_id,
                        expression,
                        "condition_not_matched",
                        &diagnostics,
                    );
                }
                Ok(result)
            }
            other => {
                if tracing::enabled!(target: "light_rule::cel", tracing::Level::TRACE) {
                    let diagnostics = collect_referenced_diagnostics(
                        &program,
                        effective_profile,
                        context,
                        None,
                        self.log_full_cel_context,
                    );
                    trace_cel_context(
                        expression_id,
                        expression,
                        "non_boolean_result",
                        &diagnostics,
                    );
                }
                Err(Box::new(RuleEngineError::CelNonBoolean(format!(
                    "{other:?}"
                ))))
            }
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
    referenced_paths: Vec<String>,
    reference_analysis_incomplete: bool,
    context_mode: &'static str,
}

#[derive(Debug, Default)]
struct CelReferenceAnalysis {
    paths: BTreeSet<Vec<String>>,
    incomplete: bool,
}

fn analyze_cel_references(expression: &IdedExpr) -> CelReferenceAnalysis {
    let mut analysis = CelReferenceAnalysis::default();
    collect_cel_references(expression, &HashSet::new(), &mut analysis);
    analysis
}

fn collect_cel_references(
    expression: &IdedExpr,
    locals: &HashSet<String>,
    analysis: &mut CelReferenceAnalysis,
) {
    if let Some(path) = static_cel_reference_path(expression, locals) {
        analysis.paths.insert(path);
        return;
    }

    match &expression.expr {
        Expr::Unspecified | Expr::Literal(_) => {}
        Expr::Ident(name) => {
            if !name.starts_with('@') && !locals.contains(name) {
                analysis.paths.insert(vec![name.clone()]);
            }
        }
        Expr::Select(select) => {
            analysis.incomplete = true;
            collect_cel_references(&select.operand, locals, analysis);
        }
        Expr::Call(call) => {
            if matches!(call.func_name.as_str(), INDEX | OPT_INDEX)
                && let Some(target) = call.args.first()
            {
                if let Some(prefix) = static_cel_reference_path(target, locals) {
                    analysis.paths.insert(prefix);
                } else {
                    collect_cel_references(target, locals, analysis);
                }
                for index in call.args.iter().skip(1) {
                    collect_cel_references(index, locals, analysis);
                }
                analysis.incomplete = true;
                return;
            }
            if let Some(target) = call.target.as_deref() {
                collect_cel_references(target, locals, analysis);
            }
            for argument in &call.args {
                collect_cel_references(argument, locals, analysis);
            }
        }
        Expr::Comprehension(comprehension) => {
            collect_cel_references(&comprehension.iter_range, locals, analysis);
            collect_cel_references(&comprehension.accu_init, locals, analysis);
            let mut inner_locals = locals.clone();
            inner_locals.insert(comprehension.iter_var.clone());
            if let Some(iter_var2) = comprehension.iter_var2.as_ref() {
                inner_locals.insert(iter_var2.clone());
            }
            inner_locals.insert(comprehension.accu_var.clone());
            collect_cel_references(&comprehension.loop_cond, &inner_locals, analysis);
            collect_cel_references(&comprehension.loop_step, &inner_locals, analysis);
            collect_cel_references(&comprehension.result, &inner_locals, analysis);
        }
        Expr::List(list) => {
            for element in &list.elements {
                collect_cel_references(element, locals, analysis);
            }
        }
        Expr::Map(map) => {
            for entry in &map.entries {
                collect_entry_references(&entry.expr, locals, analysis);
            }
        }
        Expr::Struct(structure) => {
            for entry in &structure.entries {
                collect_entry_references(&entry.expr, locals, analysis);
            }
        }
    }
}

fn collect_entry_references(
    entry: &EntryExpr,
    locals: &HashSet<String>,
    analysis: &mut CelReferenceAnalysis,
) {
    match entry {
        EntryExpr::StructField(field) => collect_cel_references(&field.value, locals, analysis),
        EntryExpr::MapEntry(entry) => {
            collect_cel_references(&entry.key, locals, analysis);
            collect_cel_references(&entry.value, locals, analysis);
        }
    }
}

fn static_cel_reference_path(
    expression: &IdedExpr,
    locals: &HashSet<String>,
) -> Option<Vec<String>> {
    match &expression.expr {
        Expr::Ident(name) if !name.starts_with('@') && !locals.contains(name) => {
            Some(vec![name.clone()])
        }
        Expr::Select(select) => {
            let mut path = static_cel_reference_path(&select.operand, locals)?;
            path.push(select.field.clone());
            Some(path)
        }
        Expr::Call(call)
            if matches!(call.func_name.as_str(), INDEX | OPT_INDEX | OPT_SELECT)
                && call.args.len() == 2 =>
        {
            let mut path = static_cel_reference_path(&call.args[0], locals)?;
            let Expr::Literal(LiteralValue::String(field)) = &call.args[1].expr else {
                return None;
            };
            path.push(field.inner().to_string());
            Some(path)
        }
        _ => None,
    }
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
                warn!(
                    target: "light_rule::cel",
                    expressionId = expression_id,
                    expression,
                    error = %execution_error,
                    "CEL expression evaluation failed"
                );
                if tracing::enabled!(target: "light_rule::cel", tracing::Level::TRACE) {
                    let diagnostics = collect_diagnostics_safely(diagnostics);
                    trace_cel_context(expression_id, expression, "evaluation_error", &diagnostics);
                }
            }
            Err(RuleEngineError::CelEvaluate(execution_error.to_string()))
        }
        Err(panic_payload) => {
            let panic_message = panic_payload_message(panic_payload.as_ref());
            if log_failure {
                error!(
                    target: "light_rule::cel",
                    expressionId = expression_id,
                    expression,
                    panic = %panic_message,
                    "CEL interpreter panic contained"
                );
                if tracing::enabled!(target: "light_rule::cel", tracing::Level::TRACE) {
                    let diagnostics = collect_diagnostics_safely(diagnostics);
                    trace_cel_context(expression_id, expression, "interpreter_panic", &diagnostics);
                }
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

fn collect_referenced_diagnostics(
    program: &CelProgram,
    profile: CelSecurityProfile,
    context: &JsonValue,
    row: Option<&JsonValue>,
    include_values: bool,
) -> NullPathDiagnostics {
    let analysis = analyze_cel_references(program.expression());
    let mut budget = DiagnosticContextBudget::new();
    let mut projected = serde_json::Map::new();
    let mut diagnostics = NullPathDiagnostics {
        reference_analysis_incomplete: analysis.incomplete,
        context_mode: if include_values { "full" } else { "metadata" },
        ..NullPathDiagnostics::default()
    };

    for path in analysis.paths {
        if profile == CelSecurityProfile::Strict
            && !path
                .first()
                .is_some_and(|root| STRICT_CEL_ROOTS.contains(&root.as_str()))
        {
            continue;
        }
        if budget.exhausted() {
            budget.truncated = true;
            break;
        }
        let display_path = display_reference_path(&path);
        let value = resolve_reference_value(context, row, &path);
        diagnostics.referenced_paths.push(display_path.clone());
        let projected_value = if include_values {
            value.cloned().unwrap_or(JsonValue::Null)
        } else {
            reference_value_metadata(value)
        };
        insert_diagnostic_context_value(
            &mut projected,
            display_path.as_str(),
            &projected_value,
            &mut budget,
        );

        if let Some(value) = value {
            collect_null_paths(value, format!("$.{display_path}"), 0, &mut diagnostics);
        }
    }

    diagnostics.evaluation_context = JsonValue::Object(projected);
    diagnostics.evaluation_context_truncated = budget.truncated;
    diagnostics
}

fn resolve_reference_value<'a>(
    context: &'a JsonValue,
    row: Option<&'a JsonValue>,
    path: &[String],
) -> Option<&'a JsonValue> {
    let (mut value, remaining) = match path.first().map(String::as_str) {
        Some("context") => (context, &path[1..]),
        Some("row") => (row?, &path[1..]),
        Some(root) => (context.get(root)?, &path[1..]),
        None => return None,
    };
    for segment in remaining {
        value = value.get(segment)?;
    }
    Some(value)
}

fn display_reference_path(path: &[String]) -> String {
    let mut display = "$".to_string();
    for segment in path {
        display = json_object_path(display.as_str(), segment);
    }
    display
        .strip_prefix("$.")
        .or_else(|| display.strip_prefix('$'))
        .unwrap_or(display.as_str())
        .to_string()
}

fn reference_value_metadata(value: Option<&JsonValue>) -> JsonValue {
    let Some(value) = value else {
        return serde_json::json!({"present": false});
    };
    let (value_type, size) = match value {
        JsonValue::Null => ("null", None),
        JsonValue::Bool(_) => ("boolean", None),
        JsonValue::Number(_) => ("number", None),
        JsonValue::String(value) => ("string", Some(value.chars().count())),
        JsonValue::Array(value) => ("array", Some(value.len())),
        JsonValue::Object(value) => ("object", Some(value.len())),
    };
    let mut metadata = serde_json::Map::new();
    metadata.insert("present".to_string(), JsonValue::Bool(true));
    metadata.insert(
        "type".to_string(),
        JsonValue::String(value_type.to_string()),
    );
    if let Some(size) = size {
        metadata.insert("size".to_string(), JsonValue::Number(size.into()));
    }
    JsonValue::Object(metadata)
}

fn trace_cel_context(
    expression_id: &str,
    expression: &str,
    outcome: &str,
    diagnostics: &NullPathDiagnostics,
) {
    trace!(
        target: "light_rule::cel",
        expressionId = expression_id,
        expression,
        outcome,
        contextMode = diagnostics.context_mode,
        referencedPaths = ?diagnostics.referenced_paths,
        referenceAnalysisIncomplete = diagnostics.reference_analysis_incomplete,
        referencedContext = %diagnostics.evaluation_context,
        contextTruncated = diagnostics.evaluation_context_truncated,
        candidateNullPaths = ?diagnostics.paths,
        candidateNullPathsTruncated = diagnostics.truncated,
        "CEL expression context"
    );
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
    use std::cell::Cell;

    #[test]
    fn extracts_static_context_paths_and_ignores_comprehension_locals() {
        let program = CelProgram::compile(
            "permission.roles.exists(r, r in auditInfo.subject_claims.ClaimsMap.roles)",
        )
        .expect("compile CEL expression");

        let analysis = analyze_cel_references(program.expression());

        assert_eq!(
            analysis.paths,
            BTreeSet::from([
                vec![
                    "auditInfo".to_string(),
                    "subject_claims".to_string(),
                    "ClaimsMap".to_string(),
                    "roles".to_string(),
                ],
                vec!["permission".to_string(), "roles".to_string()],
            ])
        );
        assert!(!analysis.incomplete);
    }

    #[test]
    fn literal_indexes_are_exact_and_dynamic_indexes_fall_back_to_prefix() {
        let literal =
            CelProgram::compile("auditInfo.subject_claims.ClaimsMap['roles'] == ['admin']")
                .expect("compile literal-index expression");
        let literal_analysis = analyze_cel_references(literal.expression());
        assert!(literal_analysis.paths.contains(&vec![
            "auditInfo".to_string(),
            "subject_claims".to_string(),
            "ClaimsMap".to_string(),
            "roles".to_string(),
        ]));
        assert!(!literal_analysis.incomplete);

        let dynamic = CelProgram::compile("auditInfo.subject_claims.ClaimsMap[claimName]")
            .expect("compile dynamic-index expression");
        let dynamic_analysis = analyze_cel_references(dynamic.expression());
        assert!(dynamic_analysis.paths.contains(&vec![
            "auditInfo".to_string(),
            "subject_claims".to_string(),
            "ClaimsMap".to_string(),
        ]));
        assert!(
            dynamic_analysis
                .paths
                .contains(&vec!["claimName".to_string()])
        );
        assert!(dynamic_analysis.incomplete);
    }

    #[test]
    fn dynamic_index_full_projection_intentionally_includes_the_static_parent() {
        let program = CelProgram::compile(
            "auditInfo.subject_claims.ClaimsMap[permission.claimName] == 'admin'",
        )
        .expect("compile dynamic-index expression");
        let context = json!({
            "permission": {"claimName": "role"},
            "auditInfo": {
                "subject_claims": {
                    "ClaimsMap": {
                        "role": "admin",
                        "department": "engineering"
                    }
                }
            }
        });

        let diagnostics = collect_referenced_diagnostics(
            &program,
            CelSecurityProfile::Strict,
            &context,
            None,
            true,
        );

        assert!(diagnostics.reference_analysis_incomplete);
        assert_eq!(
            diagnostics.evaluation_context["auditInfo.subject_claims.ClaimsMap"],
            json!({"role": "admin", "department": "engineering"})
        );
        assert_eq!(
            diagnostics.evaluation_context["permission.claimName"],
            "role"
        );
    }

    #[test]
    fn projects_only_referenced_context_with_metadata_or_full_values() {
        let program =
            CelProgram::compile("permission.roles == auditInfo.subject_claims.ClaimsMap.roles")
                .expect("compile CEL expression");
        let context = json!({
            "permission": {"roles": ["admin"], "groups": ["hidden"]},
            "auditInfo": {
                "subject_claims": {
                    "ClaimsMap": {
                        "roles": ["developer"],
                        "email": "hidden@example.com"
                    }
                }
            },
            "headers": {"x-unrelated": "hidden"}
        });

        let metadata = collect_referenced_diagnostics(
            &program,
            CelSecurityProfile::Strict,
            &context,
            None,
            false,
        );
        assert_eq!(
            metadata.evaluation_context["permission.roles"],
            json!({"present": true, "type": "array", "size": 1})
        );
        assert!(
            metadata
                .evaluation_context
                .get("auditInfo.subject_claims.ClaimsMap.email")
                .is_none()
        );
        assert!(metadata.evaluation_context.get("headers").is_none());

        let full = collect_referenced_diagnostics(
            &program,
            CelSecurityProfile::Strict,
            &context,
            None,
            true,
        );
        assert_eq!(
            full.evaluation_context["permission.roles"],
            json!(["admin"])
        );
        assert_eq!(
            full.evaluation_context["auditInfo.subject_claims.ClaimsMap.roles"],
            json!(["developer"])
        );
        assert!(
            !full
                .evaluation_context
                .to_string()
                .contains("hidden@example.com")
        );
    }

    #[test]
    fn strict_projection_excludes_roots_unavailable_to_the_evaluator() {
        let program =
            CelProgram::compile("internalState.secret == 'value'").expect("compile CEL expression");
        let context = json!({
            "internalState": {"secret": "value"},
        });

        let diagnostics = collect_referenced_diagnostics(
            &program,
            CelSecurityProfile::Strict,
            &context,
            None,
            true,
        );

        assert!(diagnostics.referenced_paths.is_empty());
        assert_eq!(diagnostics.evaluation_context, json!({}));
    }

    #[test]
    fn contains_interpreter_panics_and_reports_candidate_null_paths() {
        let program = CelProgram::compile("permission.roles.exists(r, true)")
            .expect("compile CEL expression");
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
            || {
                collect_referenced_diagnostics(
                    &program,
                    CelSecurityProfile::Strict,
                    &context,
                    None,
                    true,
                )
            },
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("CEL interpreter panicked: synthetic CEL panic")
        );
    }

    #[test]
    fn trace_disabled_skips_error_context_collection() {
        let diagnostics_collected = Cell::new(false);

        let result = execute_cel_safely(
            "trace-disabled-test",
            "permission.missing",
            true,
            || {
                Err(CelExecutionError::UndeclaredReference(Arc::new(
                    "missing".to_string(),
                )))
            },
            || {
                diagnostics_collected.set(true);
                NullPathDiagnostics::default()
            },
        );

        assert!(result.is_err());
        assert!(!diagnostics_collected.get());
    }

    #[test]
    fn bounds_diagnostic_evaluation_context() {
        let program =
            CelProgram::compile("toolArguments.description == '' || size(toolArguments.items) > 0")
                .expect("compile CEL expression");
        let context = json!({
            "toolArguments": {
                "description": "x".repeat(MAX_DIAGNOSTIC_CONTEXT_STRING_CHARS + 100),
                "items": (0..100).collect::<Vec<_>>()
            }
        });

        let diagnostics = collect_referenced_diagnostics(
            &program,
            CelSecurityProfile::Standard,
            &context,
            None,
            true,
        );
        let description = diagnostics.evaluation_context["toolArguments.description"]
            .as_str()
            .unwrap();
        let items = diagnostics.evaluation_context["toolArguments.items"]
            .as_array()
            .unwrap();

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
        let program = CelProgram::compile("toolArguments").expect("compile CEL expression");
        let context = json!({
            "toolArguments": (0..MAX_DIAGNOSTIC_JSON_NODES + 100).collect::<Vec<_>>()
        });

        let diagnostics = collect_referenced_diagnostics(
            &program,
            CelSecurityProfile::Strict,
            &context,
            None,
            true,
        );

        assert_eq!(diagnostics.visited_nodes, MAX_DIAGNOSTIC_JSON_NODES);
        assert!(diagnostics.truncated);
    }
}
