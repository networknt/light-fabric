//! Public compatibility surface carries plain JSON/source segments and opaque IDs.
//! Every compilation, hit audit, evaluation and program drop is routed to a worker.
use crate::{
    Category, Engine, Kind, Limits, Position, Profile, Segment, WorkerConfig, WorkerError, scan,
};
use serde_json::Value;
pub type Compiled = crate::worker::Handle;
#[derive(Clone)]
pub struct Bindings {
    pub(crate) context: Value,
    pub(crate) input: Value,
    pub(crate) output: Option<Value>,
    pub(crate) value: Option<Value>,
    pub(crate) limits: Limits,
}
impl Bindings {
    pub fn new(
        context: &Value,
        input: &Value,
        output: Option<&Value>,
        value: Option<&Value>,
    ) -> Result<Self, crate::ExpressionError> {
        Self::with_limits(context, input, output, value, Limits::default())
    }
    pub fn with_limits(
        context: &Value,
        input: &Value,
        output: Option<&Value>,
        value: Option<&Value>,
        limits: Limits,
    ) -> Result<Self, crate::ExpressionError> {
        crate::json::validate_bindings(context, input, output, value, limits)?;
        Ok(Self {
            context: context.clone(),
            input: input.clone(),
            output: output.cloned(),
            value: value.cloned(),
            limits,
        })
    }
}
pub fn compile(engine: &Engine, source: &str, position: Position) -> Result<Compiled, WorkerError> {
    compile_with_limits(engine, source, position, Limits::default())
}
pub fn compile_with_limits(
    engine: &Engine,
    source: &str,
    position: Position,
    limits: Limits,
) -> Result<Compiled, WorkerError> {
    engine.compile(source, position, limits)
}
pub fn evaluate(
    compiled: &Compiled,
    bindings: &Bindings,
    kind: Kind,
) -> Result<Value, WorkerError> {
    compiled.evaluate(bindings, kind)
}
impl crate::worker::Handle {
    fn evaluate(&self, bindings: &Bindings, kind: Kind) -> Result<Value, WorkerError> {
        crate::worker::evaluate_handle(self, bindings, kind)
    }
}
pub struct CompileCache {
    engine: Engine,
}
impl CompileCache {
    pub const MAX_ENTRIES: usize = 128;
    pub const MAX_BYTES: usize = 16 * 1024 * 1024;
    pub fn new() -> Result<Self, WorkerError> {
        Self::with_budget(Self::MAX_ENTRIES, Self::MAX_BYTES)
    }
    pub fn with_budget(entries: usize, bytes: usize) -> Result<Self, WorkerError> {
        Ok(Self {
            engine: Engine::new(WorkerConfig {
                workers: 1,
                cache_entries: entries,
                cache_bytes: bytes,
                ..WorkerConfig::default()
            })?,
        })
    }
    pub fn len(&self) -> Result<usize, WorkerError> {
        self.engine.cache_stats().map(|stats| stats.0)
    }
    pub fn is_empty(&self) -> Result<bool, WorkerError> {
        self.len().map(|len| len == 0)
    }
    pub fn retained_bytes(&self) -> Result<usize, WorkerError> {
        self.engine.cache_stats().map(|stats| stats.1)
    }
    pub fn shutdown(&self, timeout: std::time::Duration) -> Result<(), WorkerError> {
        self.engine.shutdown(timeout)
    }
    pub fn compile(
        &mut self,
        profile: Profile,
        source: &str,
        position: Position,
    ) -> Result<Compiled, WorkerError> {
        self.compile_with_limits(profile, source, position, Limits::default())
    }
    pub fn compile_with_limits(
        &mut self,
        profile: Profile,
        source: &str,
        position: Position,
        limits: Limits,
    ) -> Result<Compiled, WorkerError> {
        self.engine
            .enqueue(profile, source.to_owned(), position, limits)?
            .wait()
    }
    pub fn template(
        &mut self,
        source: &str,
        position: Position,
    ) -> Result<CompiledTemplate, WorkerError> {
        template(&self.engine, source, position, true)
    }
    pub fn json_template(
        &mut self,
        value: &Value,
        position: Position,
    ) -> Result<CompiledJson, WorkerError> {
        json_template(&self.engine, value, position)
    }
}
pub struct CompiledTemplate {
    segments: Vec<Segment>,
    programs: Vec<Compiled>,
    whole: bool,
    position: Position,
}
enum JsonNode {
    Literal(Value),
    Template(CompiledTemplate),
    Array(Vec<JsonNode>),
    Object(Vec<(String, JsonNode)>),
}
pub struct CompiledJson {
    node: JsonNode,
    position: Position,
}
fn template(
    engine: &Engine,
    source: &str,
    position: Position,
    require_expression: bool,
) -> Result<CompiledTemplate, WorkerError> {
    let segments = scan(source)?;
    let whole =
        matches!(segments.as_slice(), [Segment::Span {offset:0,end,..}] if *end == source.len());
    if require_expression
        && (matches!(position.kind(), Kind::Predicate | Kind::Export)
            || position == Position::WorkflowOutput)
        && !whole
    {
        return Err(crate::error::admission(Category::Invalid).into());
    }
    let mut programs = Vec::new();
    for segment in &segments {
        if let Segment::Span {
            source,
            source_offset,
            index,
            ..
        } = segment
        {
            programs.push(
                engine
                    .compile(source, position, Limits::default())
                    .map_err(|e| e.at(*index, *source_offset))?,
            );
        }
    }
    Ok(CompiledTemplate {
        segments,
        programs,
        whole,
        position,
    })
}
fn json_template(
    engine: &Engine,
    value: &Value,
    position: Position,
) -> Result<CompiledJson, WorkerError> {
    if (position.kind() == Kind::Predicate && !value.is_string())
        || (position == Position::WorkflowOutput && !value.is_string() && !value.is_object())
    {
        return Err(crate::error::admission(Category::Invalid).into());
    }
    fn node(
        engine: &Engine,
        value: &Value,
        position: Position,
        root: bool,
        depth: usize,
    ) -> Result<JsonNode, WorkerError> {
        if depth > Limits::default().output_depth {
            return Err(crate::error::admission(Category::Limit).into());
        }
        Ok(match value {
            Value::String(s) => JsonNode::Template(template(
                engine,
                s,
                if position == Position::WorkflowOutput && !root {
                    Position::Set
                } else {
                    position
                },
                root,
            )?),
            Value::Array(a) => JsonNode::Array(
                a.iter()
                    .map(|v| node(engine, v, position, false, depth + 1))
                    .collect::<Result<_, _>>()?,
            ),
            Value::Object(o) => {
                let mut entries: Vec<_> = o.iter().collect();
                entries.sort_unstable_by_key(|(k, _)| *k);
                JsonNode::Object(
                    entries
                        .into_iter()
                        .map(|(k, v)| Ok((k.clone(), node(engine, v, position, false, depth + 1)?)))
                        .collect::<Result<_, WorkerError>>()?,
                )
            }
            _ if position.kind() == Kind::String => {
                return Err(crate::error::admission(Category::Invalid).into());
            }
            _ => JsonNode::Literal(value.clone()),
        })
    }
    Ok(CompiledJson {
        node: node(engine, value, position, true, 0)?,
        position,
    })
}
pub fn evaluate_template(
    template: &CompiledTemplate,
    bindings: &Bindings,
) -> Result<Value, WorkerError> {
    if template.whole {
        let Segment::Span {
            index,
            source_offset,
            ..
        } = &template.segments[0]
        else {
            unreachable!()
        };
        return evaluate(&template.programs[0], bindings, template.position.kind())
            .map_err(|e| e.at(*index, *source_offset));
    }
    let mut result = String::new();
    for segment in &template.segments {
        match segment {
            Segment::Literal { text, .. } => result.push_str(text),
            Segment::Span {
                index,
                source_offset,
                ..
            } => {
                let value = evaluate(
                    &template.programs[*index],
                    bindings,
                    template.position.kind(),
                )
                .map_err(|e| e.at(*index, *source_offset))?;
                let Value::String(text) = value else {
                    return Err(crate::error::runtime(Category::ResultType)
                        .at(*index, *source_offset)
                        .into());
                };
                result.push_str(&text);
            }
        }
        if result.len() > Limits::default().output_bytes {
            return Err(crate::error::runtime(Category::Limit).into());
        }
    }
    let value = Value::String(result);
    crate::json::encode(&value, Limits::default().output_bytes)?;
    Ok(value)
}
pub fn evaluate_json(compiled: &CompiledJson, bindings: &Bindings) -> Result<Value, WorkerError> {
    fn resolve(node: &JsonNode, bindings: &Bindings) -> Result<Value, WorkerError> {
        Ok(match node {
            JsonNode::Literal(v) => v.clone(),
            JsonNode::Template(t) => evaluate_template(t, bindings)?,
            JsonNode::Array(a) => Value::Array(
                a.iter()
                    .map(|v| resolve(v, bindings))
                    .collect::<Result<_, _>>()?,
            ),
            JsonNode::Object(o) => {
                let mut result = serde_json::Map::new();
                for (k, v) in o {
                    result.insert(k.clone(), resolve(v, bindings)?);
                }
                Value::Object(result)
            }
        })
    }
    let value = resolve(&compiled.node, bindings)?;
    if compiled.position == Position::WorkflowOutput && !value.is_object() {
        return Err(crate::error::runtime(Category::ResultType).into());
    }
    crate::json::validate_output(&value, Limits::default())?;
    Ok(value)
}
