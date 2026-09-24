//! Private, sessionless MCP adapter for Workflow-owned administration tools.
//! Authentication and business authorization remain in the shared handlers.

use crate::rule_api::{RuleApiState, dispatch_native_tool};
use axum::{
    Json, Router,
    body::to_bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use serde_json::{Value, json};
use std::sync::OnceLock;

const VERSION: &str = "2026-07-28";
const CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
const CLIENT_INFO: &str = "io.modelcontextprotocol/clientInfo";

pub(crate) fn routes() -> Router<RuleApiState> {
    Router::new().route("/mcp", post(handle))
}

fn manifest() -> &'static Value {
    static VALUE: OnceLock<Value> = OnceLock::new();
    VALUE.get_or_init(|| {
        serde_json::from_str(include_str!(
            "../contracts/workflow-admin/tool-manifest.json"
        ))
        .expect("embedded workflow admin tool manifest must be valid")
    })
}

fn shared_schemas() -> &'static Value {
    static VALUE: OnceLock<Value> = OnceLock::new();
    VALUE.get_or_init(|| {
        serde_json::from_str(include_str!("../contracts/workflow-admin/schemas.json"))
            .expect("embedded workflow admin shared schemas must be valid")
    })
}

fn resolve_contract_refs(value: &Value) -> Value {
    if let Some(reference) = value.get("$ref").and_then(Value::as_str) {
        let pointer = reference
            .strip_prefix("schemas.json#")
            .or_else(|| reference.strip_prefix('#'))
            .unwrap_or_else(|| panic!("unsupported workflow admin schema reference: {reference}"));
        let resolved = shared_schemas()
            .pointer(pointer)
            .unwrap_or_else(|| panic!("unresolved workflow admin schema reference: {reference}"));
        return resolve_contract_refs(resolved);
    }
    match value {
        Value::Array(items) => Value::Array(items.iter().map(resolve_contract_refs).collect()),
        Value::Object(values) => Value::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), resolve_contract_refs(value)))
                .collect(),
        ),
        _ => value.clone(),
    }
}

fn advertised_tools() -> Vec<Value> {
    manifest()["tools"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|tool| {
            json!({
                "name":tool["name"],
                "description":tool.get("description").cloned().unwrap_or_else(|| json!("Workflow lifecycle operation")),
                "inputSchema":resolve_contract_refs(&tool["inputSchema"]),
                "outputSchema":resolve_contract_refs(&tool["outputSchema"]),
                "annotations":{
                    "readOnlyHint":!tool["sideEffect"].as_bool().unwrap_or(false),
                    "destructiveHint":tool["sideEffect"].as_bool().unwrap_or(false)
                }
            })
        })
        .collect()
}

fn rpc_error(id: Value, status: StatusCode, code: i64, message: &str) -> Response {
    let mut response = (
        status,
        Json(json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})),
    )
        .into_response();
    response
        .headers_mut()
        .insert("mcp-protocol-version", VERSION.parse().unwrap());
    response
}

async fn handler_error(id: Value, response: Response) -> Result<Value, Response> {
    let status = response.status();
    let challenge = response.headers().get("www-authenticate").cloned();
    let retry_after = response.headers().get("retry-after").cloned();
    let bytes = to_bytes(response.into_body(), 64 * 1024)
        .await
        .unwrap_or_default();
    let detail: Value = serde_json::from_slice(&bytes).unwrap_or_else(
        |_| json!({"code":"WORKFLOW_REJECTED","message":"workflow operation was rejected"}),
    );
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        let mut denied = rpc_error(id, status, -32001, "workflow authorization denied");
        if let Some(value) = challenge {
            denied.headers_mut().insert("www-authenticate", value);
        }
        return Err(denied);
    }
    let mut result = json!({"resultType":"complete","content":[{"type":"text","text":detail.to_string()}],"structuredContent":{"status":status.as_u16(),"error":detail},"isError":true});
    if let Some(value) = retry_after.and_then(|value| value.to_str().ok().map(str::to_owned)) {
        result["_meta"] = json!({"retryAfter":value});
    }
    Ok(result)
}

fn one_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let first = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(first)
}

fn validate(headers: &HeaderMap, request: &Value) -> Result<(Value, String, Value), Response> {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    if headers.contains_key("mcp-session-id") {
        return Err(rpc_error(
            id,
            StatusCode::BAD_REQUEST,
            -32600,
            "Mcp-Session-Id is not valid for the stateless profile",
        ));
    }
    if one_header(headers, "mcp-protocol-version") != Some(VERSION) {
        return Err(rpc_error(
            id,
            StatusCode::BAD_REQUEST,
            -32600,
            "unsupported MCP protocol version",
        ));
    }
    let accept = one_header(headers, "accept").unwrap_or_default();
    if !accept.contains("application/json") || !accept.contains("text/event-stream") {
        return Err(rpc_error(
            id,
            StatusCode::BAD_REQUEST,
            -32600,
            "Accept must list application/json and text/event-stream",
        ));
    }
    if request.get("jsonrpc").and_then(Value::as_str) != Some("2.0") || id.is_null() {
        return Err(rpc_error(
            id,
            StatusCode::BAD_REQUEST,
            -32600,
            "invalid JSON-RPC request",
        ));
    }
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if one_header(headers, "mcp-method") != Some(method) {
        return Err(rpc_error(
            id,
            StatusCode::BAD_REQUEST,
            -32020,
            "Mcp-Method header does not match JSON-RPC method",
        ));
    }
    let params = request
        .get("params")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            rpc_error(
                id.clone(),
                StatusCode::BAD_REQUEST,
                -32602,
                "params must be an object",
            )
        })?;
    let meta = params
        .get("_meta")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            rpc_error(
                id.clone(),
                StatusCode::BAD_REQUEST,
                -32602,
                "params._meta must be an object",
            )
        })?;
    if !meta.get(CAPABILITIES).is_some_and(Value::is_object)
        || meta
            .get("io.modelcontextprotocol/protocolVersion")
            .and_then(Value::as_str)
            != Some(VERSION)
        || meta.get(CLIENT_INFO).is_some_and(|v| !v.is_object())
    {
        return Err(rpc_error(
            id,
            StatusCode::BAD_REQUEST,
            -32602,
            "invalid stateless request metadata",
        ));
    }
    if headers
        .keys()
        .any(|name| name.as_str().starts_with("mcp-param-"))
    {
        return Err(rpc_error(
            id,
            StatusCode::BAD_REQUEST,
            -32020,
            "unexpected Mcp-Param header",
        ));
    }
    if method == "tools/call" {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if one_header(headers, "mcp-name") != Some(name) {
            return Err(rpc_error(
                id,
                StatusCode::BAD_REQUEST,
                -32020,
                "Mcp-Name header does not match params.name",
            ));
        }
    } else if headers.contains_key("mcp-name") {
        return Err(rpc_error(
            id,
            StatusCode::BAD_REQUEST,
            -32020,
            "Mcp-Name is only valid for tools/call",
        ));
    }
    Ok((id, method.to_string(), Value::Object(params.clone())))
}

async fn handle(
    State(state): State<RuleApiState>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> Response {
    let (id, method, params) = match validate(&headers, &request) {
        Ok(v) => v,
        Err(e) => return e,
    };
    let result = match method.as_str() {
        "server/discover" => {
            json!({"supportedVersions":[VERSION],"capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"light-workflow","version":env!("CARGO_PKG_VERSION")},"ttlMs":30000,"cacheScope":"private","resultType":"complete"})
        }
        "tools/list" => {
            let tools = advertised_tools();
            json!({"tools":tools,"ttlMs":30000,"cacheScope":"private","resultType":"complete"})
        }
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            if !manifest()["tools"]
                .as_array()
                .is_some_and(|tools| tools.iter().any(|tool| tool["name"] == name))
            {
                return rpc_error(
                    id,
                    StatusCode::NOT_FOUND,
                    -32601,
                    "workflow tool is unavailable",
                );
            }
            match dispatch_native_tool(name, state, headers, arguments).await {
                Ok(value) => {
                    json!({"resultType":"complete","content":[{"type":"text","text":value.to_string()}],"structuredContent":value,"isError":false})
                }
                Err(response) => match handler_error(id.clone(), response).await {
                    Ok(result) => result,
                    Err(response) => return response,
                },
            }
        }
        _ => return rpc_error(id, StatusCode::NOT_FOUND, -32601, "method not found"),
    };
    let mut response = Json(json!({"jsonrpc":"2.0","id":id,"result":result})).into_response();
    response
        .headers_mut()
        .insert("mcp-protocol-version", VERSION.parse().unwrap());
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contains_external_ref(value: &Value) -> bool {
        match value {
            Value::Array(items) => items.iter().any(contains_external_ref),
            Value::Object(values) => {
                values
                    .get("$ref")
                    .and_then(Value::as_str)
                    .is_some_and(|reference| !reference.starts_with('#'))
                    || values.values().any(contains_external_ref)
            }
            _ => false,
        }
    }

    #[test]
    fn embedded_catalog_is_complete_and_unique() {
        let tools = manifest()["tools"].as_array().unwrap();
        let names = tools
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(names.len(), 15);
        assert_eq!(names.len(), tools.len());
    }

    #[test]
    fn advertised_catalog_has_self_contained_compilable_schemas_and_valid_examples() {
        let tools = advertised_tools();
        let examples: Value =
            serde_json::from_str(include_str!("../contracts/workflow-admin/examples.json"))
                .unwrap();
        assert_eq!(tools.len(), 15);
        for tool in tools {
            let name = tool["name"].as_str().unwrap();
            for (schema_name, example_name) in
                [("inputSchema", "input"), ("outputSchema", "output")]
            {
                let schema = &tool[schema_name];
                assert!(
                    !contains_external_ref(schema),
                    "{name} {schema_name} contains an external reference"
                );
                let validator = jsonschema::validator_for(schema).unwrap_or_else(|error| {
                    panic!("{name} {schema_name} does not compile: {error}")
                });
                assert!(
                    validator.is_valid(&examples[name][example_name]),
                    "{name} {example_name} does not match its advertised schema"
                );
            }
        }
    }

    #[tokio::test]
    async fn authentication_denial_preserves_http_status_and_challenge() {
        let mut source = (StatusCode::UNAUTHORIZED, Json(json!({"code":"DENIED"}))).into_response();
        source
            .headers_mut()
            .insert("www-authenticate", "Bearer".parse().unwrap());
        let response = handler_error(json!(7), source).await.unwrap_err();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(response.headers()["www-authenticate"], "Bearer");
        assert_eq!(response.headers()["mcp-protocol-version"], VERSION);
    }
}
