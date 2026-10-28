//! Owned local qualification: OpenAPI schema locations, not key-name scanning.
// Experimental aggregate compiler reservation, distinct from wire frames.
const COMPILER_INPUT_BYTES: usize = 16 << 20;
struct CompilerCounter(usize);
impl std::io::Write for CompilerCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .filter(|n| *n <= COMPILER_INPUT_BYTES)
            .ok_or_else(|| std::io::Error::other("experimental aggregate compiler profile"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
use serde_json::{Value, json};
fn escape(s: &str) -> String {
    s.replace('~', "~0").replace('/', "~1")
}
fn child(path: &str, key: &str) -> String {
    format!("{path}/{}", escape(key))
}
struct Discovery {
    out: Vec<String>,
    bytes: usize,
    failed: bool,
}
impl Discovery {
    fn push_path(&mut self, path: &str, key: &str) {
        if self.failed {
            return;
        }
        let n = path.len()
            + 1
            + key
                .bytes()
                .map(|b| if b == b'~' || b == b'/' { 2 } else { 1 })
                .sum::<usize>();
        let Some(next) = self.bytes.checked_add(n) else {
            self.failed = true;
            return;
        };
        if next > COMPILER_INPUT_BYTES {
            self.failed = true;
            return;
        }
        self.bytes = next;
        self.out.push(child(path, key));
    }
}
fn content(value: &Value, path: &str, out: &mut Discovery) {
    if let Some(media) = value.as_object() {
        for (name, item) in media {
            if item.get("schema").is_some() {
                out.push_path(&child(path, name), "schema");
            }
            if let Some(headers) = item.get("encoding").and_then(Value::as_object) {
                for (name, encoding) in headers {
                    headers_at(
                        &encoding["headers"],
                        &child(&child(&child(path, name), "encoding"), name),
                        out,
                    );
                }
            }
        }
    }
}
fn parameter(value: &Value, path: &str, out: &mut Discovery) {
    if value.get("schema").is_some() {
        out.push_path(path, "schema");
    }
    content(&value["content"], &child(path, "content"), out);
}
fn headers_at(value: &Value, path: &str, out: &mut Discovery) {
    if let Some(headers) = value.as_object() {
        for (name, header) in headers {
            parameter(header, &child(path, name), out);
        }
    }
}
fn body(value: &Value, path: &str, out: &mut Discovery) {
    content(&value["content"], &child(path, "content"), out);
    headers_at(&value["headers"], &child(path, "headers"), out);
}
fn path_item(value: &Value, path: &str, out: &mut Discovery) {
    if let Some(parameters) = value["parameters"].as_array() {
        for (i, p) in parameters.iter().enumerate() {
            parameter(p, &format!("{path}/parameters/{i}"), out);
        }
    }
    for method in [
        "get", "put", "post", "delete", "options", "head", "patch", "trace",
    ] {
        let Some(op) = value.get(method) else {
            continue;
        };
        let path = child(path, method);
        if let Some(parameters) = op["parameters"].as_array() {
            for (i, p) in parameters.iter().enumerate() {
                parameter(p, &format!("{path}/parameters/{i}"), out);
            }
        }
        if let Some(b) = op.get("requestBody") {
            body(b, &child(&path, "requestBody"), out);
        }
        if let Some(responses) = op["responses"].as_object() {
            for (name, response) in responses {
                body(response, &child(&child(&path, "responses"), name), out);
            }
        }
        if let Some(callbacks) = op["callbacks"].as_object() {
            for (name, callback) in callbacks {
                if let Some(expressions) = callback.as_object() {
                    for (expression, item) in expressions {
                        if expression != "$ref" {
                            path_item(
                                item,
                                &child(&child(&child(&path, "callbacks"), name), expression),
                                out,
                            );
                        }
                    }
                }
            }
        }
    }
}
pub fn locations(document: &Value) -> Result<Vec<String>, String> {
    let mut out = Discovery {
        out: Vec::new(),
        bytes: 0,
        failed: false,
    };
    for section in ["paths", "webhooks"] {
        if let Some(paths) = document[section].as_object() {
            for (name, item) in paths {
                if !name.starts_with("x-") {
                    path_item(item, &child(&format!("/{section}"), name), &mut out);
                }
            }
        }
    }
    let c = &document["components"];
    if let Some(schemas) = c["schemas"].as_object() {
        for name in schemas.keys() {
            out.push_path("/components/schemas", name);
        }
    }
    for section in ["parameters", "headers"] {
        if let Some(items) = c[section].as_object() {
            for (name, item) in items {
                parameter(
                    item,
                    &child(&format!("/components/{section}"), name),
                    &mut out,
                );
            }
        }
    }
    for section in ["requestBodies", "responses"] {
        if let Some(items) = c[section].as_object() {
            for (name, item) in items {
                body(
                    item,
                    &child(&format!("/components/{section}"), name),
                    &mut out,
                );
            }
        }
    }
    if let Some(paths) = c["pathItems"].as_object() {
        for (name, item) in paths {
            path_item(item, &child("/components/pathItems", name), &mut out);
        }
    }
    if out.failed {
        return Err("experimental discovery pointer profile".into());
    }
    out.out.sort();
    out.out.dedup();
    Ok(out.out)
}
fn fragment(pointer: &str) -> String {
    let mut output = String::from("#");
    use std::fmt::Write;
    for byte in pointer.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            output.push(byte as char)
        } else {
            write!(&mut output, "%{byte:02X}").unwrap();
        }
    }
    output
}
pub fn project(document: &Value) -> Result<Value, String> {
    fn neutral(value: &mut Value) {
        match value {
            Value::Number(_) => *value = json!(0),
            Value::Array(a) => {
                for v in a {
                    neutral(v)
                }
            }
            Value::Object(m) => {
                for v in m.values_mut() {
                    neutral(v)
                }
            }
            _ => {}
        }
    }
    let mut metadata = document.clone();
    neutral(&mut metadata);
    Ok(metadata)
}

// One captured-document registry and eager map per required nullable profile.
// Validators returned to operation adapters share the compiled graph.
struct Profile {
    document: std::sync::Arc<Value>,
    map: jsonschema::ValidatorMap,
}
pub struct Pool {
    profiles: std::collections::BTreeMap<bool, Profile>,
    keys: std::collections::BTreeMap<String, String>,
    bytes: usize,
    reserved: usize,
    addresses: std::collections::BTreeMap<usize, String>,
}
impl Pool {
    pub fn compile(
        document: &Value,
        oas30: bool,
        index_nullable: bool,
        body_nullable: bool,
    ) -> Result<Self, String> {
        let locations = locations(document)?;
        if oas30 {
            for path in &locations {
                crate::request::legacy_init_admission(
                    document.pointer(path).ok_or("missing captured schema")?,
                )?;
            }
        }
        let keys: std::collections::BTreeMap<_, _> = locations
            .iter()
            .enumerate()
            .map(|(i, p)| (p.clone(), format!("#/$defs/s{i}")))
            .collect();
        let wrapper_bytes = 12usize
            + locations.len().saturating_sub(1)
            + locations
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    1 + i.to_string().len()
                        + 14
                        + "urn:wp12:captured".len()
                        + 1
                        + p.bytes()
                            .map(|b| {
                                if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
                                    1
                                } else {
                                    3
                                }
                            })
                            .sum::<usize>()
                })
                .sum::<usize>();
        if wrapper_bytes > COMPILER_INPUT_BYTES {
            return Err("experimental aggregate compiler profile".into());
        }
        let definitions: serde_json::Map<String, Value> = locations
            .iter()
            .enumerate()
            .map(|(i, p)| {
                (
                    format!("s{i}"),
                    json!({"$ref":format!("urn:wp12:captured{}",fragment(p))}),
                )
            })
            .collect();
        let wrapper = json!({"$defs":definitions});
        let mut wrapper_count = CompilerCounter(0);
        serde_json::to_writer(&mut wrapper_count, &wrapper)
            .map_err(|_| "experimental aggregate compiler profile")?;
        assert_eq!(wrapper_bytes, wrapper_count.0);
        let mut profiles = std::collections::BTreeMap::new();
        let mut bytes = 0usize;
        let mut reserved = 0usize;
        for nullable in if index_nullable || body_nullable {
            vec![false, true]
        } else {
            vec![false]
        } {
            // Both the document copy and index are charged before cloning/compiling.
            let mut charge = CompilerCounter(0);
            serde_json::to_writer(&mut charge, document)
                .map_err(|_| "experimental aggregate compiler profile")?;
            serde_json::to_writer(&mut charge, &wrapper)
                .map_err(|_| "experimental aggregate compiler profile")?;
            fn nullable_count(v: &Value) -> usize {
                match v {
                    Value::Object(m) => {
                        usize::from(m.get("nullable") == Some(&Value::Bool(true)))
                            + m.values().map(nullable_count).sum::<usize>()
                    }
                    Value::Array(a) => a.iter().map(nullable_count).sum(),
                    _ => 0,
                }
            }
            let growth = if nullable {
                40 * nullable_count(document)
            } else {
                0
            };
            reserved = reserved
                .checked_add(charge.0 + growth)
                .ok_or("experimental aggregate compiler profile")?;
            if reserved > COMPILER_INPUT_BYTES {
                return Err("experimental aggregate compiler profile".into());
            }
            let mut normalized = document.clone();
            for path in &locations {
                if let Some(schema) = normalized.pointer_mut(path) {
                    crate::request::normalise(schema, oas30, nullable);
                }
            }
            let mut actual = CompilerCounter(0);
            serde_json::to_writer(&mut actual, &normalized)
                .map_err(|_| "experimental aggregate compiler profile")?;
            bytes += actual.0 + wrapper_count.0;
            let map = crate::precision::compile_map(&wrapper, &normalized)
                .map_err(|e| format!("catalogue compile: {e}"))?;
            if keys.values().any(|key| !map.contains_key(key)) {
                return Err("incomplete catalogue readiness".into());
            }
            profiles.insert(
                nullable,
                Profile {
                    document: std::sync::Arc::new(normalized),
                    map,
                },
            );
        }
        let addresses = locations
            .iter()
            .map(|p| {
                (
                    document.pointer(p).unwrap() as *const Value as usize,
                    p.clone(),
                )
            })
            .collect();
        Ok(Self {
            profiles,
            keys,
            bytes,
            reserved,
            addresses,
        })
    }
    pub fn get(
        &self,
        document: &Value,
        schema: &Value,
        nullable: bool,
    ) -> Result<(jsonschema::Validator, std::sync::Arc<Value>), String> {
        let _ = document;
        let pointer = self
            .addresses
            .get(&(schema as *const Value as usize))
            .ok_or("uncaptured schema")?;
        self.at(pointer, nullable)
    }
    pub fn at(
        &self,
        pointer: &str,
        nullable: bool,
    ) -> Result<(jsonschema::Validator, std::sync::Arc<Value>), String> {
        let profile = self
            .profiles
            .get(&nullable)
            .ok_or("unprepared nullable profile")?;
        let key = self.keys.get(pointer).ok_or("uncatalogued schema")?;
        Ok((
            profile
                .map
                .get(key)
                .ok_or("missing prepared schema")?
                .clone(),
            profile.document.clone(),
        ))
    }
    pub fn bytes(&self) -> usize {
        self.bytes
    }
    pub fn reserved(&self) -> usize {
        self.reserved
    }
    pub fn roots(&self) -> impl Iterator<Item = &str> {
        self.keys.keys().map(String::as_str)
    }
    pub fn cache_entries(&self) -> usize {
        self.profiles.values().map(|p| p.map.len()).sum()
    }
}
