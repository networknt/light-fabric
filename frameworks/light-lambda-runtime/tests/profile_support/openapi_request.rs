//! Owned adapter qualification; no production registration or lazy compilation.
use jsonschema::{
    Draft, Retrieve, Uri, Validator,
    error::{TypeKind, ValidationErrorKind},
};
use serde_json::{Value, json};
use serverless_core::StatusError;
struct Deny;
impl Retrieve for Deny {
    fn retrieve(&self, _: &Uri<String>) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        Err("uncatalogued resource".into())
    }
}
struct Compiled {
    validator: Validator,
    coercion: Option<String>,
    schema: Value,
    java_overlay_error: Option<String>,
}
struct Parameter {
    location: String,
    name: String,
    required: bool,
    schema: Option<Compiled>,
}
struct Operation {
    path: String,
    method: String,
    parameters: Vec<Parameter>,
    body: Option<Compiled>,
    has_body: bool,
}
pub struct Prepared {
    operations: Vec<Operation>,
    skip: Vec<String>,
    skip_body: bool,
    enabled: bool,
}
fn invalid() -> String {
    "validator: invalid captured specification".into()
}
fn normalise(schema: &mut Value, oas30: bool, nullable: bool) {
    let Some(map) = schema.as_object_mut() else {
        return;
    };
    if oas30 && map.contains_key("$ref") {
        let reference = map["$ref"].clone();
        map.clear();
        map.insert("$ref".into(), reference);
        return;
    }
    if oas30 {
        // Owner-approved dialect distinction: Java's OAS3.0 overlay drops
        // these fields. OAS3.1 deliberately evaluates their 2020-12 meaning.
        for key in [
            "contains",
            "propertyNames",
            "dependentRequired",
            "prefixItems",
        ] {
            map.remove(key);
        }
        for (exclusive, bound) in [
            ("exclusiveMinimum", "minimum"),
            ("exclusiveMaximum", "maximum"),
        ] {
            if let Some(flag) = map.get(exclusive).and_then(Value::as_bool) {
                map.remove(exclusive);
                if flag && let Some(value) = map.remove(bound) {
                    map.insert(exclusive.into(), value);
                }
            }
        }
    }
    for key in [
        "properties",
        "patternProperties",
        "$defs",
        "definitions",
        "dependentSchemas",
    ] {
        if let Some(values) = map.get_mut(key).and_then(Value::as_object_mut) {
            for child in values.values_mut() {
                normalise(child, oas30, nullable);
            }
        }
    }
    for key in [
        "items",
        "additionalProperties",
        "contains",
        "propertyNames",
        "not",
        "if",
        "then",
        "else",
        "unevaluatedProperties",
        "unevaluatedItems",
    ] {
        if let Some(child) = map.get_mut(key) {
            normalise(child, oas30, nullable);
        }
    }
    for key in ["allOf", "anyOf", "oneOf", "prefixItems"] {
        if let Some(children) = map.get_mut(key).and_then(Value::as_array_mut) {
            for child in children {
                normalise(child, oas30, nullable);
            }
        }
    }
    if nullable && map.get("nullable") == Some(&Value::Bool(true)) {
        map.remove("nullable");
        let original = std::mem::take(schema);
        *schema = json!({"anyOf":[original,{"type":"null"}]});
    }
}
fn compile(
    schema: &Value,
    components: &Value,
    oas30: bool,
    nullable: bool,
) -> Result<Compiled, String> {
    let java_overlay_error = if oas30 {
        legacy_overlay_error(schema, "#")
    } else {
        None
    };
    let mut schema = schema.clone();
    normalise(&mut schema, oas30, nullable);
    let mut components = components.clone();
    if let Some(schemas) = components["schemas"].as_object_mut() {
        for child in schemas.values_mut() {
            normalise(child, oas30, nullable);
        }
    }
    let mut root = schema.as_object().cloned().ok_or_else(invalid)?;
    root.insert("components".into(), components);
    let root = Value::Object(root);
    let coercion = schema["type"].as_str().map(str::to_owned);
    let validator = jsonschema::options()
        .with_draft(Draft::Draft202012)
        .should_validate_formats(false)
        .with_retriever(Deny)
        .build(&root)
        .map_err(|_| invalid())?;
    Ok(Compiled {
        validator,
        coercion,
        schema: root,
        java_overlay_error,
    })
}
fn legacy_overlay_error(schema: &Value, path: &str) -> Option<String> {
    let map = schema.as_object()?;
    if map.contains_key("$ref") {
        return None;
    }
    if !map.is_empty()
        && map.keys().all(|key| {
            [
                "contains",
                "propertyNames",
                "dependentRequired",
                "prefixItems",
            ]
            .contains(&key.as_str())
        })
    {
        return Some(format!(
            "Schema at {path} must be object or boolean but was MISSING"
        ));
    }
    for key in [
        "properties",
        "patternProperties",
        "$defs",
        "definitions",
        "dependentSchemas",
    ] {
        if let Some(children) = map.get(key).and_then(Value::as_object) {
            for (name, child) in children {
                let name = name.replace('~', "~0").replace('/', "~1");
                if let Some(error) = legacy_overlay_error(child, &format!("{path}/{key}/{name}")) {
                    return Some(error);
                }
            }
        }
    }
    for key in [
        "items",
        "additionalProperties",
        "not",
        "if",
        "then",
        "else",
        "unevaluatedProperties",
        "unevaluatedItems",
    ] {
        if let Some(child) = map.get(key)
            && let Some(error) = legacy_overlay_error(child, &format!("{path}/{key}"))
        {
            return Some(error);
        }
    }
    for key in ["allOf", "anyOf", "oneOf"] {
        if let Some(children) = map.get(key).and_then(Value::as_array) {
            for (index, child) in children.iter().enumerate() {
                if let Some(error) = legacy_overlay_error(child, &format!("{path}/{key}/{index}")) {
                    return Some(error);
                }
            }
        }
    }
    None
}
fn status(code: &str, message: &str, description: impl AsRef<str>) -> StatusError {
    StatusError::new(400, code, message, description.as_ref())
}
fn content_error() -> StatusError {
    status(
        "ERR10015",
        "CONTENT_TYPE_MISMATCH",
        "Either the Content-Type header application/json does not match the body, or the body was serialized incorrectly.",
    )
}
fn kind(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}
fn validate(
    compiled: &Compiled,
    mut value: Value,
    prefix: &str,
    loose: bool,
) -> Result<Option<StatusError>, String> {
    if let Some(error) = &compiled.java_overlay_error {
        return Ok(Some(status(
            "ERR11003",
            "VALIDATOR_SCHEMA_INVALID_JSON",
            format!("Unable to parse JSON - {error}"),
        )));
    }
    if loose && let Some(text) = value.as_str() {
        value = match compiled.coercion.as_deref() {
            Some("integer") => text.parse::<i64>().map(Value::from).unwrap_or(value),
            Some("number") => text
                .parse::<serde_json::Number>()
                .map(Value::Number)
                .unwrap_or(value),
            Some("boolean") => match text {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                _ => value,
            },
            _ => value,
        };
    }
    let Err(error) = compiled.validator.validate(&value) else {
        return Ok(None);
    };
    let mut error = &error;
    while let ValidationErrorKind::AnyOf { context } = error.kind() {
        let Some(first) = context.first().and_then(|errors| errors.first()) else {
            break;
        };
        error = first;
    }
    let location = format!("{prefix}{}", error.instance_path());
    let detail = match error.kind() {
        ValidationErrorKind::Contains => {
            let schema = compiled
                .schema
                .pointer(error.schema_path().as_str())
                .ok_or_else(invalid)?;
            format!("must contain at least 1 element(s) that passes these validations: {schema}")
        }
        ValidationErrorKind::PropertyNames { error: inner } => {
            let property = inner.instance().as_str().ok_or_else(invalid)?;
            let ValidationErrorKind::Pattern { pattern } = inner.kind() else {
                return Err("unqualified propertyNames error".into());
            };
            format!(
                "property '{property}' name is not valid: does not match the regex pattern {pattern}"
            )
        }
        ValidationErrorKind::Required { property }
            if error.schema_path().as_str().ends_with("/dependentRequired") =>
        {
            let dependencies = compiled
                .schema
                .pointer(error.schema_path().as_str())
                .and_then(Value::as_object)
                .ok_or_else(invalid)?;
            let instance = value
                .pointer(error.instance_path().as_str())
                .and_then(Value::as_object)
                .ok_or_else(invalid)?;
            let trigger = dependencies
                .iter()
                .find(|(key, required)| {
                    instance.contains_key(*key)
                        && required
                            .as_array()
                            .is_some_and(|items| items.contains(property))
                })
                .map(|(key, _)| key)
                .ok_or_else(invalid)?;
            format!(
                "has a missing property '{}' which is dependent required because '{trigger}' is present",
                property.as_str().ok_or_else(invalid)?
            )
        }
        ValidationErrorKind::FalseSchema
            if error
                .schema_path()
                .as_str()
                .ends_with("/additionalProperties") =>
        {
            let path = error.schema_path().as_str();
            let parent = compiled
                .schema
                .pointer(
                    path.strip_suffix("/additionalProperties")
                        .ok_or_else(invalid)?,
                )
                .ok_or_else(invalid)?;
            // The engine's no-properties specialization reports FalseSchema on
            // the containing object. Other shapes retain explicit qualification.
            if parent.get("properties").is_some() || parent.get("patternProperties").is_some() {
                return Err("unqualified additionalProperties specialization".into());
            }
            let property = value
                .pointer(error.instance_path().as_str())
                .and_then(Value::as_object)
                .and_then(|map| map.keys().next())
                .ok_or_else(invalid)?;
            format!(
                "property '{property}' is not defined in the schema and the schema does not allow additional properties"
            )
        }
        ValidationErrorKind::Type {
            kind: TypeKind::Single(expected),
        } => format!("{} found, {expected} expected", kind(error.instance())),
        ValidationErrorKind::ExclusiveMinimum { limit } => {
            format!("must have an exclusive minimum value of {limit}")
        }
        ValidationErrorKind::ExclusiveMaximum { limit } => {
            format!("must have an exclusive maximum value of {limit}")
        }
        ValidationErrorKind::MinLength { limit } => {
            format!("must be at least {limit} characters long")
        }
        ValidationErrorKind::MaxLength { limit } => {
            format!("must be at most {limit} characters long")
        }
        ValidationErrorKind::Minimum { limit } => format!("must have a minimum value of {limit}"),
        ValidationErrorKind::Maximum { limit } => format!("must have a maximum value of {limit}"),
        ValidationErrorKind::Pattern { pattern } => {
            format!("does not match the regex pattern {pattern}")
        }
        ValidationErrorKind::MinItems { limit } => format!(
            "must have at least {limit} items but found {}",
            error.instance().as_array().ok_or_else(invalid)?.len()
        ),
        ValidationErrorKind::MaxItems { limit } => format!(
            "must have at most {limit} items but found {}",
            error.instance().as_array().ok_or_else(invalid)?.len()
        ),
        ValidationErrorKind::UniqueItems => "must have only unique items in the array".into(),
        ValidationErrorKind::MinProperties { limit } => {
            format!("must have at least {limit} properties")
        }
        ValidationErrorKind::MaxProperties { limit } => {
            format!("must have at most {limit} properties")
        }
        ValidationErrorKind::AdditionalProperties { unexpected } => format!(
            "property '{}' is not defined in the schema and the schema does not allow additional properties",
            unexpected.first().ok_or_else(invalid)?
        ),
        ValidationErrorKind::UnevaluatedProperties { unexpected } => format!(
            "property '{}' is not evaluated and the schema does not allow unevaluated properties",
            unexpected.first().ok_or_else(invalid)?
        ),
        ValidationErrorKind::Not { schema } => format!("must not be valid to the schema {schema}"),
        ValidationErrorKind::OneOfNotValid { .. } => {
            "must be valid to one and only one schema, but 0 are valid".into()
        }
        ValidationErrorKind::OneOfMultipleValid { context } => {
            let indexes: Vec<_> = context
                .iter()
                .enumerate()
                .filter(|(_, errors)| errors.is_empty())
                .map(|(i, _)| i.to_string())
                .collect();
            format!(
                "must be valid to one and only one schema, but {} are valid with indexes '{}'",
                indexes.len(),
                indexes.join(", ")
            )
        }
        ValidationErrorKind::Required { property } => format!(
            "required property '{}' not found",
            property.as_str().ok_or_else(invalid)?
        ),
        ValidationErrorKind::MultipleOf { multiple_of } => {
            format!("must be multiple of {multiple_of}")
        }
        ValidationErrorKind::Enum { options } => format!(
            "does not have a value in the enumeration [{}]",
            options
                .as_array()
                .ok_or_else(invalid)?
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        other => {
            return Err(format!(
                "unqualified Java error formatting: {} ({other:?}), schema={}, instance={}",
                error.kind().keyword(),
                error.schema_path(),
                error.instance_path()
            ));
        }
    };
    Ok(Some(status(
        "ERR11004",
        "VALIDATOR_SCHEMA",
        format!("Schema Validation Error - {location}: {detail}"),
    )))
}
// Reference Objects are distinct from Schema Objects: schema-sibling dialect
// behavior remains in the schema normalizer. Only captured document pointers
// are resolved here, with cycle detection and no filesystem/network access.
fn reference_object<'a>(document: &'a Value, mut value: &'a Value) -> Result<&'a Value, String> {
    let mut visited = std::collections::BTreeSet::new();
    while let Some(reference) = value.get("$ref") {
        let reference = reference.as_str().ok_or_else(invalid)?;
        if !visited.insert(reference) {
            return Err("cyclic reference object".into());
        }
        let pointer = reference
            .strip_prefix('#')
            .ok_or_else(|| "uncatalogued resource".to_string())?;
        // URI fragment decoding precedes JSON Pointer's ~0/~1 decoding.
        // This never opens a file or resolves a network resource.
        let mut decoded = Vec::with_capacity(pointer.len());
        let mut bytes = pointer.as_bytes().iter().copied();
        while let Some(byte) = bytes.next() {
            decoded.push(if byte == b'%' {
                let digit = |byte: u8| match byte {
                    b'0'..=b'9' => Some(byte - b'0'),
                    b'a'..=b'f' => Some(byte - b'a' + 10),
                    b'A'..=b'F' => Some(byte - b'A' + 10),
                    _ => None,
                };
                let high = bytes.next().and_then(digit).ok_or_else(invalid)?;
                let low = bytes.next().and_then(digit).ok_or_else(invalid)?;
                high * 16 + low
            } else {
                byte
            });
        }
        let pointer = std::str::from_utf8(&decoded).map_err(|_| invalid())?;
        value = document.pointer(pointer).ok_or_else(invalid)?;
    }
    Ok(value)
}

impl Prepared {
    pub fn schema_count(&self) -> usize {
        self.operations
            .iter()
            .map(|op| {
                op.parameters.iter().filter(|p| p.schema.is_some()).count()
                    + usize::from(op.body.is_some())
            })
            .sum()
    }
    pub fn compile(document: &Value, settings: &Value) -> Result<Self, String> {
        let oas30 = document["openapi"]
            .as_str()
            .ok_or_else(invalid)?
            .starts_with("3.0.");
        let mut operations = Vec::new();
        for (path, item) in document["paths"].as_object().ok_or_else(invalid)? {
            for (method, operation) in item.as_object().ok_or_else(invalid)? {
                if ![
                    "get", "post", "put", "patch", "delete", "head", "options", "trace",
                ]
                .contains(&method.as_str())
                {
                    continue;
                }
                let mut parameters = Vec::new();
                // Native path-level headers precede operation-level headers.
                for location in ["path", "query", "header"] {
                    let lists = if location == "header" {
                        vec![&item["parameters"], &operation["parameters"]]
                    } else {
                        vec![&operation["parameters"]]
                    };
                    for list in lists {
                        if let Some(list) = list.as_array() {
                            for p in list {
                                let p = reference_object(document, p)?;
                                if p["in"] != location {
                                    continue;
                                }
                                parameters.push(Parameter {
                                    location: location.into(),
                                    name: p["name"].as_str().ok_or_else(invalid)?.into(),
                                    required: p["required"] == true,
                                    schema: if p.get("schema").is_some() {
                                        Some(compile(
                                            &p["schema"],
                                            &document["components"],
                                            oas30,
                                            false,
                                        )?)
                                    } else {
                                        None
                                    },
                                });
                            }
                        }
                    }
                }
                let request_body = reference_object(document, &operation["requestBody"])?;
                let body = request_body["content"]["application/json"]
                    .get("schema")
                    .map(|s| {
                        compile(
                            s,
                            &document["components"],
                            oas30,
                            settings["handleNullableField"] == true,
                        )
                    })
                    .transpose()?;
                operations.push(Operation {
                    path: path.clone(),
                    method: method.to_ascii_uppercase(),
                    parameters,
                    body,
                    has_body: operation.get("requestBody").is_some(),
                });
            }
        }
        Ok(Self {
            operations,
            skip: settings["skipPathPrefixes"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            skip_body: settings["skipBodyValidation"] == true,
            enabled: settings["validateRequest"] != false,
        })
    }
    pub fn validate(&self, event: &Value) -> Result<Option<StatusError>, String> {
        let path = event["path"].as_str().ok_or_else(invalid)?;
        if !self.enabled || self.skip.iter().any(|p| path.starts_with(p)) {
            return Ok(None);
        }
        let Some(op) = self.operations.iter().find(|op| {
            op.method == event["httpMethod"].as_str().unwrap_or("")
                && match_path(&op.path, path).is_some()
        }) else {
            return Err("no qualified operation metadata".into());
        };
        let path_params = match_path(&op.path, path).ok_or_else(invalid)?;
        for parameter in &op.parameters {
            let value = match parameter.location.as_str() {
                "path" => event["pathParameters"][&parameter.name]
                    .as_str()
                    .or_else(|| path_params.get(&parameter.name).map(String::as_str)),
                "query" => event["queryStringParameters"][&parameter.name].as_str(),
                _ => event["headers"]
                    .as_object()
                    .and_then(|headers| {
                        headers.get(&parameter.name).or_else(|| {
                            headers
                                .iter()
                                .find(|(k, _)| k.eq_ignore_ascii_case(&parameter.name))
                                .map(|(_, v)| v)
                        })
                    })
                    .and_then(Value::as_str),
            };
            if let Some(value) = value {
                if let Some(schema) = &parameter.schema
                    && let Some(error) = validate(
                        schema,
                        Value::String(value.into()),
                        &format!("/{}", parameter.name),
                        true,
                    )?
                {
                    return Ok(Some(error));
                }
            } else if parameter.required {
                let (code, message, label) = match parameter.location.as_str() {
                    "query" => (
                        "ERR11000",
                        "VALIDATOR_REQUEST_PARAMETER_QUERY_MISSING",
                        "Query",
                    ),
                    "header" => (
                        "ERR11017",
                        "VALIDATOR_REQUEST_PARAMETER_HEADER_MISSING",
                        "Header",
                    ),
                    // The native request path visits actual path matches; a
                    // declaration without a matching template value is skipped.
                    "path" => continue,
                    _ => return Err(invalid()),
                };
                return Ok(Some(status(
                    code,
                    message,
                    format!(
                        "{label} parameter {} is required on path {path} but not found in request.",
                        parameter.name
                    ),
                )));
            }
        }
        let media = event["headers"]["Content-Type"].as_str();
        if media.is_some_and(|m| !m.starts_with("application/json")) || self.skip_body {
            return Ok(None);
        }
        let Some(body) = event["body"].as_str() else {
            return Ok(None);
        };
        if !op.has_body {
            // The measured native middleware does not validate a body when the
            // operation has no requestBody; do not invent ERR11013 here.
            return Ok(None);
        }
        let trimmed = body.trim();
        if !matches!(trimmed.as_bytes().first(), Some(b'{' | b'[')) {
            return Ok(Some(content_error()));
        }
        let body: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(_) => return Ok(Some(content_error())),
        };
        let Some(schema) = &op.body else {
            return Err("missing application/json schema not yet qualified".into());
        };
        validate(schema, body, "", false)
    }
}
fn match_path(template: &str, path: &str) -> Option<std::collections::BTreeMap<String, String>> {
    let t = template.split('/').collect::<Vec<_>>();
    let p = path.split('/').collect::<Vec<_>>();
    if t.len() != p.len() {
        return None;
    }
    let mut captures = std::collections::BTreeMap::new();
    for (t, p) in t.iter().zip(p) {
        if t.starts_with('{') && t.ends_with('}') {
            captures.insert(t[1..t.len() - 1].into(), p.into());
        } else if *t != p {
            return None;
        }
    }
    Some(captures)
}
