use crate::error::runtime;
use crate::{Category, ExpressionError, Kind, Limits, Position, compiler::Compiled};
use cel::objects::{Key, Map};
use cel::{Context, ExecutionError, Value};
use serde_json::Value as Json;
use std::{
    collections::HashMap,
    io::{self, Write},
    sync::Arc,
};

struct Capped {
    bytes: Vec<u8>,
    cap: usize,
}
impl Write for Capped {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.cap.saturating_sub(self.bytes.len()) {
            return Err(io::Error::other("limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
pub(crate) fn encode(value: &Json, cap: usize) -> Result<Vec<u8>, ExpressionError> {
    let mut writer = Capped {
        bytes: Vec::new(),
        cap,
    };
    serde_json::to_writer(&mut writer, value).map_err(|_| runtime(Category::Limit))?;
    Ok(writer.bytes)
}
pub(crate) fn measure(
    value: &Json,
    start_depth: usize,
    nodes: &mut usize,
    limits: Limits,
) -> Result<(), ExpressionError> {
    let mut todo = vec![(value, start_depth)];
    while let Some((value, depth)) = todo.pop() {
        *nodes += 1;
        if *nodes > limits.input_nodes || depth > limits.input_depth {
            return Err(runtime(Category::Limit));
        }
        match value {
            Json::Array(values) => todo.extend(values.iter().rev().map(|v| (v, depth + 1))),
            Json::Object(values) => {
                let mut entries: Vec<_> = values.iter().collect();
                entries.sort_unstable_by_key(|(k, _)| *k);
                todo.extend(entries.into_iter().rev().map(|(_, v)| (v, depth + 1)));
            }
            _ => (),
        }
    }
    Ok(())
}
fn from_json(value: &Json) -> Value {
    match value {
        Json::Null => Value::Null,
        Json::Bool(v) => Value::Bool(*v),
        Json::Number(n) => {
            if let Some(v) = n.as_i64() {
                Value::Int(v)
            } else if let Some(v) = n.as_u64() {
                Value::UInt(v)
            } else {
                Value::Float(n.as_f64().expect("JSON finite number"))
            }
        }
        Json::String(v) => Value::String(Arc::new(v.clone())),
        Json::Array(v) => Value::List(Arc::new(v.iter().map(from_json).collect())),
        Json::Object(v) => {
            let mut entries: Vec<_> = v.iter().collect();
            entries.sort_unstable_by_key(|(k, _)| *k);
            Value::Map(Map {
                map: Arc::new(
                    entries
                        .into_iter()
                        .map(|(k, v)| (Key::String(Arc::new(k.clone())), from_json(v)))
                        .collect::<HashMap<_, _>>(),
                ),
            })
        }
    }
}
/// Converted once, after validating the entire borrowed envelope before copying values.
pub struct Bindings {
    context: Context<'static>,
    output: bool,
    value: bool,
}
impl Bindings {
    pub(crate) fn bind_value(&mut self, value: &Json) {
        self.context
            .add_variable_from_value("value", from_json(value));
        self.value = true;
    }
    pub fn with_limits(
        context: &Json,
        workflow_input: &Json,
        output: Option<&Json>,
        value: Option<&Json>,
        limits: Limits,
    ) -> Result<Self, ExpressionError> {
        validate_bindings(context, workflow_input, output, value, limits)?;
        let mut ctx = crate::functions::context(limits);
        ctx.add_variable_from_value("context", from_json(context));
        let workflow = Value::Map(Map {
            map: Arc::new(HashMap::from([(
                Key::String(Arc::new("input".into())),
                from_json(workflow_input),
            )])),
        });
        ctx.add_variable_from_value("workflow", workflow);
        if let Some(v) = output {
            ctx.add_variable_from_value("output", from_json(v));
        }
        if let Some(v) = value {
            ctx.add_variable_from_value("value", from_json(v));
        }
        Ok(Self {
            context: ctx,
            output: output.is_some(),
            value: value.is_some(),
        })
    }
}
pub(crate) fn to_json(
    value: &Value,
    limits: Limits,
    depth: usize,
    nodes: &mut usize,
) -> Result<Json, ExpressionError> {
    *nodes += 1;
    if *nodes > limits.output_nodes || depth > limits.output_depth {
        return Err(runtime(Category::Limit));
    }
    Ok(match value {
        Value::Null => Json::Null,
        Value::Bool(v) => Json::Bool(*v),
        Value::Int(v) => Json::from(*v),
        Value::UInt(v) => Json::from(*v),
        Value::Float(v) => Json::Number(
            serde_json::Number::from_f64(*v).ok_or_else(|| runtime(Category::JsonProfile))?,
        ),
        Value::String(v) => {
            if v.len() > limits.output_bytes {
                return Err(runtime(Category::Limit));
            }
            Json::String((**v).clone())
        }
        Value::List(v) => Json::Array(
            v.iter()
                .map(|v| to_json(v, limits, depth + 1, nodes))
                .collect::<Result<_, _>>()?,
        ),
        Value::Map(v) => {
            let mut entries: Vec<_> = v.map.iter().collect();
            entries.sort_unstable_by_key(|(k, _)| *k);
            let mut out = serde_json::Map::new();
            for (k, v) in entries {
                let Key::String(k) = k else {
                    return Err(runtime(Category::JsonProfile));
                };
                out.insert((**k).clone(), to_json(v, limits, depth + 1, nodes)?);
            }
            Json::Object(out)
        }
        _ => return Err(runtime(Category::JsonProfile)),
    })
}
fn execution_error(error: ExecutionError) -> ExpressionError {
    let category = match error {
        ExecutionError::FunctionError { message, .. } if message == "EXPRESSION_LIMIT" => {
            Category::Limit
        }
        ExecutionError::FunctionError { message, .. } if message == "EXPRESSION_JSON_PROFILE" => {
            Category::JsonProfile
        }
        _ => Category::Evaluation,
    };
    runtime(category)
}
pub fn evaluate(
    compiled: &Compiled,
    bindings: &Bindings,
    kind: Kind,
) -> Result<Json, ExpressionError> {
    crate::compiler::audit(
        compiled.program.expression(),
        compiled.position,
        compiled.limits,
    )?;
    crate::compiler::audit(
        compiled.program.expression(),
        compiled.position,
        compiled.limits,
    )?;
    if kind != compiled.position.kind()
        && !(compiled.position.kind() == Kind::Export && kind == Kind::Json)
    {
        return Err(runtime(Category::ResultType));
    }
    if (matches!(compiled.position, Position::Export | Position::RunnerExport) && !bindings.output)
        || (compiled.position == Position::AssertJsonPredicate && !bindings.value)
    {
        return Err(runtime(Category::Evaluation));
    }
    let value = compiled
        .program
        .execute(&bindings.context)
        .map_err(execution_error)?;
    if (kind == Kind::String && !matches!(value, Value::String(_)))
        || (kind == Kind::Predicate && !matches!(value, Value::Bool(_)))
    {
        return Err(runtime(Category::ResultType));
    }
    let json = to_json(&value, compiled.limits, 0, &mut 0)?;
    if compiled.position == Position::WorkflowOutput && !json.is_object() {
        return Err(runtime(Category::ResultType));
    }
    encode(&json, compiled.limits.output_bytes)?;
    Ok(json)
}

pub(crate) fn validate_bindings(
    context: &Json,
    workflow_input: &Json,
    output: Option<&Json>,
    value: Option<&Json>,
    limits: Limits,
) -> Result<(), ExpressionError> {
    if !context.is_object() {
        return Err(runtime(Category::Evaluation));
    }
    let mut nodes = 2; // envelope and workflow wrapper; root depth is zero.
    if nodes > limits.input_nodes || limits.input_depth < 1 {
        return Err(runtime(Category::Limit));
    }
    measure(context, 1, &mut nodes, limits)?;
    measure(workflow_input, 2, &mut nodes, limits)?;
    for v in [output, value].into_iter().flatten() {
        measure(v, 1, &mut nodes, limits)?;
    }
    let mut writer = Capped {
        bytes: Vec::new(),
        cap: limits.input_bytes,
    };
    let checked = (|| -> io::Result<()> {
        writer.write_all(b"{\"context\":")?;
        serde_json::to_writer(&mut writer, context)?;
        writer.write_all(b",\"workflow\":{\"input\":")?;
        serde_json::to_writer(&mut writer, workflow_input)?;
        writer.write_all(b"}")?;
        for (key, v) in [
            (b",\"output\":".as_slice(), output),
            (b",\"value\":".as_slice(), value),
        ] {
            if let Some(v) = v {
                writer.write_all(key)?;
                serde_json::to_writer(&mut writer, v)?;
            }
        }
        writer.write_all(b"}")
    })();
    checked.map_err(|_| runtime(Category::Limit))?;
    Ok(())
}
pub(crate) fn validate_output(value: &Json, limits: Limits) -> Result<(), ExpressionError> {
    measure(
        value,
        0,
        &mut 0,
        Limits {
            input_nodes: limits.output_nodes,
            input_depth: limits.output_depth,
            ..limits
        },
    )?;
    encode(value, limits.output_bytes)?;
    Ok(())
}

/// Aggregate output accounting without cloning or retaining a second JSON tree.
pub(crate) struct OutputBudget {
    nodes: usize,
    bytes: usize,
}
impl OutputBudget {
    pub(crate) fn new() -> Self {
        Self { nodes: 0, bytes: 0 }
    }
    pub(crate) fn add(&mut self, value: &Json, limits: Limits) -> Result<(), ExpressionError> {
        measure(
            value,
            0,
            &mut self.nodes,
            Limits {
                input_nodes: limits.output_nodes,
                input_depth: limits.output_depth,
                ..limits
            },
        )?;
        let bytes = encode(value, limits.output_bytes.saturating_sub(self.bytes))?.len();
        self.bytes = self
            .bytes
            .checked_add(bytes)
            .ok_or_else(|| runtime(Category::Limit))?;
        if self.bytes > limits.output_bytes {
            return Err(runtime(Category::Limit));
        }
        Ok(())
    }
}

#[cfg(test)]
mod merged_limit_tests {
    use super::*;
    #[test]
    fn merged_object_accounts_keys_and_container_bytes() {
        let limits = Limits {
            output_bytes: 20,
            ..Limits::default()
        };
        assert!(validate_output(&serde_json::json!({"a":"1234"}), limits).is_ok());
        assert!(validate_output(&serde_json::json!({"b":"5678"}), limits).is_ok());
        assert!(validate_output(&serde_json::json!({"a":"1234","b":"5678"}), limits).is_err());
    }
}
