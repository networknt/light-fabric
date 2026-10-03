//! Single-owner bounded task phase: programs and converted envelopes never leave the worker.
use crate::{
    Bindings, Category, Limits, Position, Profile, Segment, WorkerError, compiler, json, scan,
};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone)]
pub struct StepField {
    pub path: String,
    pub position: Position,
    pub template: Value,
    /// Assertion predicate value selected from an earlier field result.
    pub value_from: Option<(usize, String)>,
}
pub struct StepRequest {
    pub bindings: Bindings,
    pub fields: Vec<StepField>,
    pub first_true: bool,
    #[cfg(test)]
    pub(crate) cancel_after_span: Option<usize>,
}
#[derive(Debug)]
pub struct StepError {
    pub path: String,
    pub error: WorkerError,
}
impl std::fmt::Display for StepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}
impl std::error::Error for StepError {}

pub(crate) fn run(
    request: StepRequest,
    cache: &mut compiler::CompileCache,
    cancelled: &AtomicBool,
) -> Result<Vec<Value>, StepError> {
    let limits = request.bindings.limits;
    let mut path = request
        .fields
        .first()
        .map(|f| f.path.clone())
        .unwrap_or_default();
    let mut resolver = Resolver {
        cache,
        cancelled,
        limits,
        path: &mut path,
        #[cfg(test)]
        completed_spans: 0,
        #[cfg(test)]
        cancel_after_span: request.cancel_after_span,
    };
    let result = (|| -> Result<Vec<Value>, WorkerError> {
        resolver.check()?;
        let mut authored = json::OutputBudget::new();
        for field in &request.fields {
            authored.add(&field.template, limits)?;
        }
        let mut bindings = json::Bindings::with_limits(
            &request.bindings.context,
            &request.bindings.input,
            request.bindings.output.as_ref(),
            request.bindings.value.as_ref(),
            limits,
        )?;
        crate::worker::witness("step-conversion");
        let mut values = Vec::new();
        let mut budget = json::OutputBudget::new();
        for field in &request.fields {
            resolver.path.clone_from(&field.path);
            resolver.check()?;
            if let Some((index, selector)) = &field.value_from {
                if field.position != Position::AssertJsonPredicate {
                    return Err(crate::error::runtime(Category::Invalid).into());
                }
                let source = values
                    .get(*index)
                    .ok_or_else(|| crate::error::runtime(Category::Invalid))?;
                let value = select(source, selector).unwrap_or(&Value::Null);
                json::validate_bindings(
                    &request.bindings.context,
                    &request.bindings.input,
                    request.bindings.output.as_ref(),
                    Some(value),
                    limits,
                )?;
                bindings.bind_value(value);
            }
            let value = resolver.resolve(&field.template, field.position, true, 0, &bindings)?;
            budget.add(&value, limits)?;
            let stop = request.first_true && value == Value::Bool(true);
            values.push(value);
            if stop {
                break;
            }
        }
        resolver.check()?;
        Ok(values)
    })();
    result.map_err(|error| StepError { path, error })
}

struct Resolver<'a> {
    cache: &'a mut compiler::CompileCache,
    cancelled: &'a AtomicBool,
    limits: Limits,
    path: &'a mut String,
    #[cfg(test)]
    completed_spans: usize,
    #[cfg(test)]
    cancel_after_span: Option<usize>,
}
impl Resolver<'_> {
    fn check(&self) -> Result<(), WorkerError> {
        if self.cancelled.load(Ordering::Acquire) {
            Err(WorkerError::Unavailable)
        } else {
            Ok(())
        }
    }
    fn resolve(
        &mut self,
        value: &Value,
        position: Position,
        root: bool,
        depth: usize,
        bindings: &json::Bindings,
    ) -> Result<Value, WorkerError> {
        self.check()?;
        if depth > self.limits.output_depth {
            return Err(crate::error::runtime(Category::Limit).into());
        }
        if root
            && ((position.kind() == crate::Kind::Predicate && !value.is_string())
                || (position == Position::WorkflowOutput
                    && !value.is_string()
                    && !value.is_object()))
        {
            return Err(crate::error::runtime(Category::ResultType).into());
        }
        let result = match value {
            Value::String(source) => {
                let segments = scan(source)?;
                let whole = matches!(segments.as_slice(), [Segment::Span {offset:0,end,..}] if *end == source.len());
                if root
                    && (matches!(
                        position.kind(),
                        crate::Kind::Predicate | crate::Kind::Export
                    ) || position == Position::WorkflowOutput)
                    && !whole
                {
                    return Err(crate::error::admission(Category::Invalid).into());
                }
                let span_position = if position == Position::WorkflowOutput && !root {
                    Position::Set
                } else {
                    position
                };
                if whole {
                    let Segment::Span {
                        source,
                        index,
                        source_offset,
                        ..
                    } = &segments[0]
                    else {
                        unreachable!()
                    };
                    self.span(source, *index, *source_offset, span_position, bindings)?
                } else {
                    let mut text = String::new();
                    for segment in segments {
                        self.check()?;
                        match segment {
                            Segment::Literal { text: s, .. } => {
                                if text.len().saturating_add(s.len()) > self.limits.output_bytes {
                                    return Err(crate::error::runtime(Category::Limit).into());
                                }
                                text.push_str(&s);
                            }
                            Segment::Span {
                                source,
                                index,
                                source_offset,
                                ..
                            } => {
                                let value = self.span(
                                    &source,
                                    index,
                                    source_offset,
                                    span_position,
                                    bindings,
                                )?;
                                let s = value.as_str().ok_or_else(|| {
                                    WorkerError::Expression(
                                        crate::error::runtime(Category::ResultType)
                                            .at(index, source_offset),
                                    )
                                })?;
                                if text.len().saturating_add(s.len()) > self.limits.output_bytes {
                                    return Err(crate::error::runtime(Category::Limit)
                                        .at(index, source_offset)
                                        .into());
                                }
                                text.push_str(s);
                            }
                        }
                    }
                    Value::String(text)
                }
            }
            Value::Array(a) => {
                let mut result = Vec::new();
                for (i, v) in a.iter().enumerate() {
                    let len = self.path.len();
                    self.path.push('/');
                    self.path.push_str(&i.to_string());
                    let value = self.resolve(v, position, false, depth + 1, bindings)?;
                    self.path.truncate(len);
                    result.push(value);
                }
                Value::Array(result)
            }
            Value::Object(o) => {
                let mut entries: Vec<_> = o.iter().collect();
                entries.sort_unstable_by_key(|(k, _)| *k);
                let mut map = serde_json::Map::new();
                for (k, v) in entries {
                    let len = self.path.len();
                    self.path.push('/');
                    self.path.push_str(&escape(k));
                    let value = self.resolve(v, position, false, depth + 1, bindings)?;
                    self.path.truncate(len);
                    map.insert(k.clone(), value);
                }
                Value::Object(map)
            }
            other => other.clone(),
        };
        self.check()?;
        if root {
            json::validate_output(&result, self.limits)?;
        }
        if root && position == Position::WorkflowOutput && !result.is_object() {
            return Err(crate::error::runtime(Category::ResultType).into());
        }
        if root && position == Position::JsonRpcHeader {
            let map = result
                .as_object()
                .ok_or_else(|| crate::error::runtime(Category::ResultType))?;
            for (key, value) in map {
                if !value.is_string() {
                    self.path.push('/');
                    self.path.push_str(&escape(key));
                    return Err(crate::error::runtime(Category::ResultType).into());
                }
            }
        }
        Ok(result)
    }
    fn span(
        &mut self,
        source: &str,
        index: usize,
        offset: usize,
        position: Position,
        bindings: &json::Bindings,
    ) -> Result<Value, WorkerError> {
        self.check()?;
        let program = self
            .cache
            .compile_with_limits(Profile::CelWorkflowV2, source, position, self.limits)
            .map_err(|e| WorkerError::Expression(e.at(index, offset)))?;
        self.check()?;
        crate::worker::witness("execute");
        let result = json::evaluate(&program, bindings, position.kind())
            .map_err(|e| WorkerError::Expression(e.at(index, offset)))?;
        #[cfg(test)]
        {
            self.completed_spans += 1;
            if self.cancel_after_span == Some(self.completed_spans) {
                self.cancelled.store(true, Ordering::Release);
            }
        }
        self.check()?;
        Ok(result)
    }
}
pub fn escape(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

fn select<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let path = path.trim().strip_prefix('$').unwrap_or(path.trim());
    let path = path.strip_prefix('.').unwrap_or(path);
    let mut current = value;
    for segment in path.split('.').filter(|s| !s.is_empty()) {
        let mut remainder = segment;
        if let Some(end) = remainder.find('[') {
            let field = &remainder[..end];
            if !field.is_empty() {
                current = current.get(field)?;
            }
            remainder = &remainder[end..];
        } else {
            current = current.get(remainder)?;
            continue;
        }
        while let Some(start) = remainder.find('[') {
            let end = remainder[start + 1..].find(']')? + start + 1;
            let index = remainder[start + 1..end].parse::<usize>().ok()?;
            current = current.get(index)?;
            remainder = &remainder[end + 1..];
        }
    }
    Some(current)
}

/// Plain-JSON output validation before a completion transaction can mutate state.
pub fn validate_output(value: &Value) -> Result<(), crate::ExpressionError> {
    json::validate_output(value, Limits::default())
}
