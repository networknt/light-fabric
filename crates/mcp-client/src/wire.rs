//! Transport helpers shared by HTTP clients without a Pingora dependency.
use anyhow::{Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::Value;
use std::collections::BTreeSet;

pub const MODERN_VERSION: &str = "2026-07-28";
pub const VERSION_META: &str = "io.modelcontextprotocol/protocolVersion";
pub const CAPABILITIES_META: &str = "io.modelcontextprotocol/clientCapabilities";
pub const CLIENT_META: &str = "io.modelcontextprotocol/clientInfo";

pub fn encode_header(value: &str) -> Result<String> {
    if !value.is_empty()
        && value.trim() == value
        && value.bytes().all(|b| (0x20..=0x7e).contains(&b))
        && !value.starts_with("=?")
        && !value.ends_with("?=")
    {
        return Ok(value.into());
    }
    Ok(format!("=?base64?{}?=", STANDARD.encode(value)))
}

pub fn decode_header(value: &str) -> Result<String> {
    if let Some(encoded) = value
        .strip_prefix("=?base64?")
        .and_then(|v| v.strip_suffix("?="))
    {
        return Ok(String::from_utf8(STANDARD.decode(encoded)?)?);
    }
    if value.starts_with("=?")
        || value.ends_with("?=")
        || value.is_empty()
        || value.trim() != value
        || !value.bytes().all(|b| (0x20..=0x7e).contains(&b))
    {
        bail!("malformed MCP semantic header");
    }
    Ok(value.to_owned())
}

pub fn response(body: &[u8], content_type: &str, id: &Value) -> Result<Value> {
    let result = if content_type.split(';').next().unwrap_or("").trim() == "text/event-stream" {
        let text = std::str::from_utf8(body)?.replace("\r\n", "\n");
        let mut found = None;
        for event in text.split("\n\n") {
            let data = event
                .lines()
                .filter_map(|line| {
                    line.strip_prefix("data:")
                        .map(|s| s.strip_prefix(' ').unwrap_or(s))
                })
                .collect::<Vec<_>>();
            if data.is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(&data.join("\n"))?;
            if value["jsonrpc"] != "2.0" {
                bail!("invalid SSE JSON-RPC envelope");
            }
            if value.get("method").is_some() {
                if value.get("id").is_some() {
                    bail!("independent server requests are unsupported");
                }
                continue;
            }
            if found.replace(value).is_some() {
                bail!("multiple final responses");
            }
        }
        found.ok_or_else(|| anyhow::anyhow!("SSE ended before final response"))?
    } else if content_type.split(';').next().unwrap_or("").trim() == "application/json" {
        serde_json::from_slice(body)?
    } else {
        bail!("unsupported MCP response content type");
    };
    if result["jsonrpc"] != "2.0"
        || result.get("id") != Some(id)
        || result.get("result").is_some() == result.get("error").is_some()
    {
        bail!("invalid MCP response envelope or mismatched id");
    }
    Ok(result)
}

#[derive(Clone, Debug)]
pub struct ParameterHeader {
    pub name: String,
    pub path: Vec<String>,
    pub kind: String,
}

pub fn parameter_headers(schema: &Value) -> Result<Vec<ParameterHeader>> {
    fn visit(
        schema: &Value,
        path: Vec<String>,
        direct: bool,
        out: &mut Vec<ParameterHeader>,
        names: &mut BTreeSet<String>,
        depth: usize,
    ) -> Result<()> {
        if depth > 64 {
            bail!("schema depth exceeds client limit");
        }
        let Some(object) = schema.as_object() else {
            return Ok(());
        };
        if let Some(annotation) = object.get("x-mcp-header") {
            if !direct || path.is_empty() {
                bail!("x-mcp-header requires a direct properties path");
            }
            let suffix = annotation
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("invalid header annotation"))?;
            let name = format!("Mcp-Param-{suffix}");
            if suffix.is_empty()
                || reqwest::header::HeaderName::from_bytes(name.as_bytes()).is_err()
                || !names.insert(name.to_ascii_lowercase())
            {
                bail!("invalid or duplicate parameter header");
            }
            let kind = object.get("type").and_then(Value::as_str).unwrap_or("");
            if !matches!(kind, "string" | "boolean" | "integer") {
                bail!("unsupported parameter header type");
            }
            out.push(ParameterHeader {
                name,
                path: path.clone(),
                kind: kind.into(),
            });
        }
        for (key, value) in object {
            match key.as_str() {
                "properties" | "$defs" | "definitions" | "patternProperties"
                | "dependentSchemas" => {
                    if let Some(map) = value.as_object() {
                        for (name, child) in map {
                            let mut child_path = path.clone();
                            child_path.push(name.clone());
                            visit(
                                child,
                                child_path,
                                direct && key == "properties",
                                out,
                                names,
                                depth + 1,
                            )?;
                        }
                    }
                }
                "allOf" | "anyOf" | "oneOf" | "prefixItems" => {
                    if let Some(children) = value.as_array() {
                        for child in children {
                            visit(child, path.clone(), false, out, names, depth + 1)?;
                        }
                    }
                }
                "items"
                | "contains"
                | "additionalProperties"
                | "unevaluatedProperties"
                | "unevaluatedItems"
                | "propertyNames"
                | "not"
                | "if"
                | "then"
                | "else" => visit(value, path.clone(), false, out, names, depth + 1)?,
                _ => {}
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    visit(schema, Vec::new(), true, &mut out, &mut BTreeSet::new(), 0)?;
    Ok(out)
}

pub fn argument_headers(
    plan: &[ParameterHeader],
    arguments: &Value,
) -> Result<Vec<(String, String)>> {
    let mut headers = Vec::new();
    for header in plan {
        let mut value = arguments;
        for part in &header.path {
            value = value.get(part).unwrap_or(&Value::Null);
        }
        if value.is_null() {
            continue;
        }
        let text = match header.kind.as_str() {
            "string" => value.as_str().map(str::to_string),
            "boolean" => value.as_bool().map(|v| v.to_string()),
            "integer" => value
                .as_i64()
                .filter(|v| (-9007199254740991..=9007199254740991).contains(v))
                .map(|v| v.to_string()),
            _ => None,
        }
        .ok_or_else(|| anyhow::anyhow!("invalid parameter header argument"))?;
        headers.push((header.name.clone(), encode_header(&text)?));
    }
    Ok(headers)
}

#[cfg(test)]
mod review_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn unbounded_integer_and_empty_string_headers() {
        let plan = parameter_headers(&json!({"type":"object","properties":{
            "count":{"type":"integer","x-mcp-header":"Count"},
            "text":{"type":"string","x-mcp-header":"Text"}}}))
        .unwrap();
        let headers = argument_headers(&plan, &json!({"count":42,"text":""})).unwrap();
        assert_eq!(
            headers,
            vec![
                ("Mcp-Param-Count".into(), "42".into()),
                ("Mcp-Param-Text".into(), "=?base64??=".into())
            ]
        );
        assert!(argument_headers(&plan, &json!({"count":9007199254740992i64})).is_err());
        assert!(argument_headers(&plan, &json!({"count":-9007199254740992i64})).is_err());
        for value in ["", "classifyLiability", " café ", "=?literal?="] {
            assert_eq!(
                decode_header(&encode_header(value).unwrap()).unwrap(),
                value
            );
        }
        assert_eq!(decode_header("=?base64?YQ==?=").unwrap(), "a");
        for invalid in ["=?base64?!!?=", "=?base64?/w==?=", "=?other?YQ==?="] {
            assert!(decode_header(invalid).is_err());
        }
    }
}
