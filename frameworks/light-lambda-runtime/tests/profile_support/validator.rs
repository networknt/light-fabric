//! Qualification candidate ONLY. No provider calls or production admission.
use jsonschema::{Draft, PatternOptions, Retrieve, Uri, Validator};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
pub const SCHEMA_BYTES: usize = 32768;
pub const SCHEMA_NODES: usize = 128;
pub const DEPTH: usize = 16;
pub const INSTANCE_BYTES: usize = 32768;
pub const INSTANCE_NODES: usize = 1024;
pub const STRING_BYTES: usize = 4096;
pub const REGEX_BYTES: usize = 128;
pub const REGEX_PROGRAM_BYTES: usize = 16384;
pub const REGEX_CACHE_BYTES: usize = 1048576;
#[derive(Clone)]
struct Deny(Arc<AtomicUsize>);
impl Retrieve for Deny {
    fn retrieve(&self, _: &Uri<String>) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err("uncatalogued schema resource".into())
    }
}
pub struct Candidate {
    validator: Validator,
    pub retrievals: Arc<AtomicUsize>,
}
fn rejected() -> String {
    "candidate profile rejected".into()
}
fn scalar_number(value: &Value) -> Result<(), String> {
    if value.is_number()
        && !value
            .as_i64()
            .is_some_and(|n| (-9007199254740991..=9007199254740991).contains(&n))
    {
        return Err(rejected());
    }
    Ok(())
}
fn expand(
    schema: &Value,
    root: &Value,
    stack: &mut BTreeSet<String>,
    nodes: &mut usize,
    depth: usize,
    oas30: bool,
) -> Result<Value, String> {
    *nodes += 1;
    if *nodes > SCHEMA_NODES || depth > DEPTH {
        return Err(rejected());
    }
    if schema.is_boolean() {
        return Ok(schema.clone());
    }
    let map = schema.as_object().ok_or_else(rejected)?;
    if let Some(reference) = map.get("$ref") {
        let reference = reference.as_str().ok_or_else(rejected)?;
        if map.len() != 1 || !reference.starts_with("#/") || !stack.insert(reference.into()) {
            return Err(rejected());
        }
        let target = root.pointer(&reference[1..]).ok_or_else(rejected)?;
        let output = expand(target, root, stack, nodes, depth + 1, oas30);
        stack.remove(reference);
        return output;
    }
    let mut output = serde_json::Map::new();
    let repeats = regex::Regex::new(r"\{([0-9]+)(?:,([0-9]*))?\}").map_err(|_| rejected())?;
    for (key, value) in map {
        match key.as_str() {
            "properties" => {
                let properties = value.as_object().ok_or_else(rejected)?;
                if properties.len() > 64 {
                    return Err(rejected());
                }
                let mut p = serde_json::Map::new();
                for (name, v) in properties {
                    if name.len() > 256 {
                        return Err(rejected());
                    }
                    p.insert(
                        name.clone(),
                        expand(v, root, stack, nodes, depth + 1, oas30)?,
                    );
                }
                output.insert(key.clone(), Value::Object(p));
            }
            "items" => {
                output.insert(
                    key.clone(),
                    expand(value, root, stack, nodes, depth + 1, oas30)?,
                );
            }
            "additionalProperties" => {
                if !value.is_boolean() {
                    return Err(rejected());
                }
                output.insert(key.clone(), value.clone());
            }
            "pattern" => {
                let p = value.as_str().ok_or_else(rejected)?;
                if p.len() > REGEX_BYTES
                    || !p.is_ascii()
                    || ["(?", "\\", "$", ".", "&&", "--", "~~", "[[:"]
                        .iter()
                        .any(|s| p.contains(s))
                {
                    return Err(rejected());
                }
                regex::RegexBuilder::new(p)
                    .unicode(false)
                    .size_limit(REGEX_PROGRAM_BYTES)
                    .dfa_size_limit(REGEX_CACHE_BYTES)
                    .build()
                    .map_err(|_| rejected())?;
                for m in repeats.captures_iter(p) {
                    for n in m
                        .iter()
                        .skip(1)
                        .flatten()
                        .filter(|n| !n.as_str().is_empty())
                    {
                        if n.as_str().parse::<usize>().map_err(|_| rejected())? > 32 {
                            return Err(rejected());
                        }
                    }
                }
                output.insert(key.clone(), value.clone());
            }
            "minimum" | "maximum" | "multipleOf" => {
                scalar_number(value)?;
                if !value.is_number() {
                    return Err(rejected());
                }
                output.insert(key.clone(), value.clone());
            }
            "exclusiveMinimum" | "exclusiveMaximum" => {
                if oas30 {
                    if !value.is_boolean() {
                        return Err(rejected());
                    }
                } else {
                    scalar_number(value)?;
                    if !value.is_number() {
                        return Err(rejected());
                    }
                }
                output.insert(key.clone(), value.clone());
            }
            "minLength" | "maxLength" | "minItems" | "maxItems" | "minProperties"
            | "maxProperties" => {
                if !value.as_u64().is_some_and(|n| n <= 4096) {
                    return Err(rejected());
                }
                output.insert(key.clone(), value.clone());
            }
            "required" => {
                let a = value.as_array().ok_or_else(rejected)?;
                if a.len() > 64
                    || a.iter()
                        .any(|v| !v.as_str().is_some_and(|s| s.len() <= 256))
                {
                    return Err(rejected());
                }
                output.insert(key.clone(), value.clone());
            }
            "type" => {
                let types = if let Some(a) = value.as_array() {
                    a.clone()
                } else {
                    vec![value.clone()]
                };
                if types.len() > 7
                    || types.iter().any(|t| {
                        !t.as_str().is_some_and(|s| {
                            ["null", "boolean", "object", "array", "string", "integer"].contains(&s)
                        })
                    })
                {
                    return Err(rejected());
                }
                output.insert(key.clone(), value.clone());
            }
            "enum" | "const" => {
                let values = if key == "enum" {
                    value.as_array().ok_or_else(rejected)?.clone()
                } else {
                    vec![value.clone()]
                };
                if values.len() > 32
                    || values.iter().any(|v| {
                        v.is_array()
                            || v.is_object()
                            || v.as_str().is_some_and(|s| s.len() > 256)
                            || scalar_number(v).is_err()
                    })
                {
                    return Err(rejected());
                }
                output.insert(key.clone(), value.clone());
            }
            "nullable" => {
                if !oas30 || !value.is_boolean() {
                    return Err(rejected());
                }
            }
            "description" | "title" | "default" | "example" => {
                if value.to_string().len() > 4096 {
                    return Err(rejected());
                }
            }
            _ => return Err(rejected()),
        }
    }
    if oas30
        && map.get("nullable") == Some(&Value::Bool(true))
        && let Some(kind) = output.get("type").cloned()
    {
        let mut types = kind.as_array().cloned().unwrap_or_else(|| vec![kind]);
        if !types.contains(&Value::String("null".into())) {
            types.push(Value::String("null".into()));
        }
        output.insert("type".into(), Value::Array(types));
    }
    Ok(Value::Object(output))
}
fn instance(value: &Value, nodes: &mut usize, depth: usize) -> Result<(), String> {
    *nodes += 1;
    if *nodes > INSTANCE_NODES || depth > DEPTH {
        return Err(rejected());
    }
    scalar_number(value)?;
    match value {
        Value::String(s) if s.len() > STRING_BYTES => return Err(rejected()),
        Value::Array(a) => {
            if a.len() > 256 {
                return Err(rejected());
            }
            for v in a {
                instance(v, nodes, depth + 1)?;
            }
        }
        Value::Object(m) => {
            if m.len() > 64 {
                return Err(rejected());
            }
            for (k, v) in m {
                if k.len() > 256 {
                    return Err(rejected());
                }
                instance(v, nodes, depth + 1)?;
            }
        }
        _ => {}
    }
    Ok(())
}
impl Candidate {
    pub fn validate_bytes(&self, bytes: &[u8]) -> Result<bool, String> {
        if bytes.len() > INSTANCE_BYTES {
            return Err(rejected());
        }
        let value = serde_json::from_slice(bytes).map_err(|_| rejected())?;
        self.validate(&value)
    }
    pub fn compile(schema: &Value, resources: &Value, oas30: bool) -> Result<Self, String> {
        if schema.to_string().len() > SCHEMA_BYTES || resources.to_string().len() > 1048576 {
            return Err(rejected());
        }
        let expanded = expand(schema, resources, &mut BTreeSet::new(), &mut 0, 0, oas30)?;
        let retrievals = Arc::new(AtomicUsize::new(0));
        let validator = jsonschema::options()
            .with_draft(if oas30 {
                Draft::Draft4
            } else {
                Draft::Draft202012
            })
            .with_retriever(Deny(retrievals.clone()))
            .with_pattern_options(
                PatternOptions::regex()
                    .size_limit(REGEX_PROGRAM_BYTES)
                    .dfa_size_limit(REGEX_CACHE_BYTES),
            )
            .build(&expanded)
            .map_err(|_| rejected())?;
        Ok(Self {
            validator,
            retrievals,
        })
    }
    pub fn validate(&self, value: &Value) -> Result<bool, String> {
        if value.to_string().len() > INSTANCE_BYTES {
            return Err(rejected());
        }
        instance(value, &mut 0, 0)?;
        Ok(self.validator.is_valid(value))
    }
}
