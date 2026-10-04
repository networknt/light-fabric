//! W4 runtime boundary. No raw evaluator messages or binding values in failures.
use super::*;
use workflow_expression::{
    Bindings, Category, Engine, Phase, Position, StepField, StepRequest, WorkerError,
};

#[derive(Debug)]
pub(super) struct Failure {
    pub error: WorkerError,
    pub field: String,
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for Failure {}
impl Failure {
    pub fn result(&self, claimed: &ClaimedTask) -> TaskExecutionResult {
        let details = match &self.error {
            WorkerError::Expression(e) => {
                json!({"category":e.category.code(),"phase":match e.phase {Phase::Admission=>"admission",Phase::Runtime=>"runtime"},
                "definitionId":claimed.wf_def_id,"taskId":claimed.task.task_id,"field":self.field,"spanIndex":e.span_index,"offset":e.offset})
            }
            _ => {
                json!({"definitionId":claimed.wf_def_id,"taskId":claimed.task.task_id,"field":self.field})
            }
        };
        TaskExecutionResult {
            retry_eligibility: RetryEligibility::None,
            status_code: "F",
            task_output: json!({"code":match &self.error {
            WorkerError::Expression(_) if self.field.starts_with("/output/as")=>"WORKFLOW_OUTPUT_INVALID",
            WorkerError::Expression(e)=>e.category.code(), _=>"WORKFLOW_EXPRESSION_UNAVAILABLE"},"details":details,"retryable":false}),
            next_task: None,
            context_data: None,
        }
    }
}
pub(super) fn failure(category: Category, field: &str) -> Failure {
    Failure {
        error: WorkerError::Expression(workflow_expression::ExpressionError {
            category,
            phase: Phase::Runtime,
            span_index: None,
            offset: None,
        }),
        field: field.into(),
    }
}
pub(super) async fn batch(
    engine: Option<&Engine>,
    context: &Value,
    input: &Value,
    output: Option<&Value>,
    value: Option<&Value>,
    fields: Vec<StepField>,
    first_true: bool,
) -> Result<Vec<Value>, Failure> {
    let bindings = Bindings::new(context, input, output, value).map_err(|e| Failure {
        error: e.into(),
        field: fields.first().map(|f| f.path.clone()).unwrap_or_default(),
    })?;
    let engine = engine.ok_or(Failure {
        error: WorkerError::Unavailable,
        field: String::new(),
    })?;
    engine
        .step(StepRequest {
            bindings,
            fields,
            first_true,
        })
        .await
        .map_err(|e| Failure {
            error: e.error,
            field: e.path,
        })
}
fn field(path: &str, position: Position, template: Value) -> StepField {
    StepField {
        path: path.into(),
        position,
        template,
        value_from: None,
    }
}
impl TaskExecutor {
    pub(super) async fn w4_execute(
        &self,
        claimed: &ClaimedTask,
        task: &TaskDefinition,
    ) -> Result<TaskExecutionResult, Failure> {
        let output = match task {
            TaskDefinition::Set(set) => {
                let template = match &set.set {
                    SetValue::Map(map) => {
                        Value::Object(map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                    }
                    SetValue::Expression(s) => Value::String(s.clone()),
                };
                batch(
                    self.expression_engine.as_ref(),
                    &claimed.context_data,
                    &claimed.input_data,
                    None,
                    None,
                    vec![field("/set", Position::Set, template)],
                    false,
                )
                .await?
                .remove(0)
            }
            TaskDefinition::Switch(switch) => {
                let mut predicates = Vec::new();
                let mut targets = Vec::new();
                let mut default = None;
                for (i, entry) in switch.switch.entries.iter().enumerate() {
                    if entry.len() != 1 {
                        return Err(failure(Category::Invalid, &format!("/switch/{i}")));
                    }
                    for (name, case) in entry {
                        if name == "default" {
                            if i + 1 != switch.switch.entries.len()
                                || case.when.is_some()
                                || case.then.is_none()
                            {
                                return Err(failure(
                                    Category::Invalid,
                                    &format!("/switch/{i}/default"),
                                ));
                            }
                            default = case.then.clone();
                            continue;
                        }
                        if case.then.is_none() {
                            return Err(failure(
                                Category::Invalid,
                                &format!("/switch/{i}/{name}/then"),
                            ));
                        }
                        let when = case
                            .when
                            .as_ref()
                            .ok_or_else(|| failure(Category::Invalid, "/switch"))?;
                        predicates.push(field(
                            &format!("/switch/{i}/{name}/when"),
                            Position::SwitchWhen,
                            Value::String(when.clone()),
                        ));
                        targets.push(case.then.clone());
                    }
                }
                let values = batch(
                    self.expression_engine.as_ref(),
                    &claimed.context_data,
                    &claimed.input_data,
                    None,
                    None,
                    predicates,
                    true,
                )
                .await?;
                let next = values
                    .iter()
                    .position(|v| v == &Value::Bool(true))
                    .and_then(|i| targets[i].clone())
                    .or(default)
                    .ok_or_else(|| failure(Category::ResultType, "/switch"))?;
                return Ok(TaskExecutionResult {
                    retry_eligibility: RetryEligibility::None,
                    status_code: "C",
                    task_output: json!({"nextTask":next}),
                    next_task: Some(next),
                    context_data: None,
                });
            }
            TaskDefinition::Assert(assertion) => {
                return self.w4_assert(claimed, &assertion.assert).await;
            }
            _ => return Err(failure(Category::Unsupported, "/task")),
        };
        Ok(TaskExecutionResult {
            retry_eligibility: RetryEligibility::None,
            status_code: "C",
            task_output: output,
            next_task: None,
            context_data: None,
        })
    }
    async fn w4_assert(
        &self,
        claimed: &ClaimedTask,
        assertion: &AssertDefinition,
    ) -> Result<TaskExecutionResult, Failure> {
        let context = &claimed.context_data;
        let mut fields = vec![field(
            "/assert/value",
            Position::AssertValue,
            assertion
                .value
                .clone()
                .unwrap_or_else(|| json!("${context}")),
        )];
        let mut comparisons = Vec::new();
        for (name, position, expected) in [
            (
                "contains",
                Position::AssertContains,
                assertion.contains.as_ref(),
            ),
            ("equals", Position::AssertEquals, assertion.equals.as_ref()),
        ] {
            if let Some(expected) = expected {
                comparisons.push(field(
                    &format!("/assert/{name}"),
                    position,
                    expected.clone(),
                ));
            }
        }
        if let Some(items) = &assertion.json {
            for (selector, comparison) in items {
                let path = format!("/assert/json/{}", workflow_expression::escape(selector));
                match comparison {
                    AssertComparison::Expression(source) => {
                        let mut f = field(&path, Position::AssertJsonPredicate, json!(source));
                        f.value_from = Some((0, selector.clone()));
                        comparisons.push(f);
                    }
                    AssertComparison::Object(object) => {
                        for (name, position, expected) in [
                            (
                                "contains",
                                Position::AssertJsonContains,
                                object.contains.as_ref(),
                            ),
                            ("equals", Position::AssertJsonEquals, object.equals.as_ref()),
                        ] {
                            if let Some(expected) = expected {
                                comparisons.push(field(
                                    &format!("{path}/{name}"),
                                    position,
                                    expected.clone(),
                                ));
                            }
                        }
                    }
                }
            }
        }
        comparisons.sort_unstable_by(|a, b| a.path.cmp(&b.path));
        fields.extend(comparisons);
        let paths: Vec<_> = fields.iter().map(|f| f.path.clone()).collect();
        let values = batch(
            self.expression_engine.as_ref(),
            context,
            &claimed.input_data,
            None,
            None,
            fields,
            false,
        )
        .await?;
        let actual = values[0].clone();
        let resolved: BTreeMap<_, _> = paths
            .iter()
            .zip(&values)
            .map(|(k, v)| (k.as_str(), v))
            .collect();
        let mut passed = true;
        for (path, _pos, expected, contains) in [
            (
                "/assert/equals",
                Position::AssertEquals,
                assertion.equals.as_ref(),
                false,
            ),
            (
                "/assert/contains",
                Position::AssertContains,
                assertion.contains.as_ref(),
                true,
            ),
        ] {
            if expected.is_some() {
                let expected = resolved[path];
                passed &= if contains {
                    self.value_contains(&actual, expected)
                } else {
                    &actual == expected
                };
            }
        }
        if let Some(pattern) = &assertion.matches {
            passed &= Regex::new(pattern)
                .map_err(|_| failure(Category::Invalid, "/assert/matches"))?
                .is_match(actual.as_str().unwrap_or(""))
                && actual.is_string();
        }
        if let Some(exists) = assertion.exists {
            passed &= exists != actual.is_null();
        }
        if let Some(comparisons) = &assertion.json {
            let mut sorted: Vec<_> = comparisons.iter().collect();
            sorted.sort_unstable_by_key(|(k, _)| *k);
            for (selector, comparison) in sorted {
                let selected = self
                    .lookup_json_path(&actual, selector)
                    .cloned()
                    .unwrap_or(Value::Null);
                let path = format!("/assert/json/{}", workflow_expression::escape(selector));
                match comparison {
                    AssertComparison::Expression(source) => {
                        let _ = source; // evaluated in the ordered worker batch with selected value.
                        passed &= resolved[path.as_str()] == &Value::Bool(true);
                    }
                    AssertComparison::Object(object) => {
                        for (name, _pos, expected, contains) in [
                            (
                                "equals",
                                Position::AssertJsonEquals,
                                object.equals.as_ref(),
                                false,
                            ),
                            (
                                "contains",
                                Position::AssertJsonContains,
                                object.contains.as_ref(),
                                true,
                            ),
                        ] {
                            if expected.is_some() {
                                let expected = resolved[format!("{path}/{name}").as_str()];
                                passed &= if contains {
                                    self.value_contains(&selected, expected)
                                } else {
                                    &selected == expected
                                };
                            }
                        }
                        if let Some(pattern) = &object.matches {
                            passed &= Regex::new(pattern)
                                .map_err(|_| failure(Category::Invalid, &path))?
                                .is_match(selected.as_str().unwrap_or(""))
                                && selected.is_string();
                        }
                        if let Some(exists) = object.exists {
                            passed &= exists != selected.is_null();
                        }
                        if let Some(length) = &object.has_length {
                            let n = self
                                .value_length(&selected)
                                .ok_or_else(|| failure(Category::ResultType, &path))?;
                            passed &= match length {
                                HasLengthComparison::Exact(expected) => n == *expected,
                                HasLengthComparison::Range(range) => {
                                    range.gt.is_none_or(|min| n > min)
                                        && range.gte.is_none_or(|min| n >= min)
                                        && range.lt.is_none_or(|max| n < max)
                                        && range.lte.is_none_or(|max| n <= max)
                                }
                            };
                        }
                    }
                }
            }
        }
        Ok(TaskExecutionResult {
            retry_eligibility: RetryEligibility::None,
            status_code: if passed { "C" } else { "F" },
            task_output: if passed {
                json!({"passed":true})
            } else {
                json!({"code":"WORKFLOW_ASSERTION_FAILED","retryable":false})
            },
            next_task: None,
            context_data: None,
        })
    }
    pub(super) async fn w4_exports(
        &self,
        claimed: &ClaimedTask,
        output: &Value,
    ) -> Result<Value, Failure> {
        let exports = self
            .find_raw_task_definition(&claimed.raw_definition, &claimed.task.wf_task_id)
            .and_then(|node| node.get("export"))
            .and_then(|node| node.get("as"));
        let Some(exports) = exports else {
            return Ok(claimed.context_data.clone());
        };
        let map: Value =
            serde_json::to_value(exports).map_err(|_| failure(Category::Invalid, "/export/as"))?;
        let map = map
            .as_object()
            .ok_or_else(|| failure(Category::Invalid, "/export/as"))?;
        let mut entries: Vec<_> = map.iter().collect();
        entries.sort_unstable_by_key(|(key, _)| *key);
        let position = if claimed.task.task_type == "run" {
            Position::RunnerExport
        } else {
            Position::Export
        };
        let fields = entries
            .iter()
            .map(|(key, v)| {
                field(
                    &format!("/export/as/{}", workflow_expression::escape(key)),
                    position,
                    (*v).clone(),
                )
            })
            .collect();
        let values = batch(
            self.expression_engine.as_ref(),
            &claimed.context_data,
            &claimed.input_data,
            Some(output),
            None,
            fields,
            false,
        )
        .await?;
        let mut context = claimed
            .context_data
            .as_object()
            .cloned()
            .ok_or_else(|| failure(Category::ResultType, "/context"))?;
        for ((key, _), value) in entries.into_iter().zip(values) {
            context.insert(key.clone(), value);
        }
        let merged = Value::Object(context);
        workflow_expression::validate_output(&merged).map_err(|e| Failure {
            error: e.into(),
            field: "/export/as".into(),
        })?;
        Ok(merged)
    }
}
