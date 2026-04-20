use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct JsonRpcMessage {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    pub data: Option<Value>,
}

impl JsonRpcMessage {
    pub fn new_request(id: Value, method: &str, params: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: Some(id),
            method: Some(method.to_string()),
            params: Some(params),
            result: None,
            error: None,
        }
    }

    pub fn new_notification(method: &str, params: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id: None,
            method: Some(method.to_string()),
            params: Some(params),
            result: None,
            error: None,
        }
    }

    pub fn is_request(&self) -> bool {
        self.id.is_some() && self.method.is_some()
    }

    pub fn is_notification(&self) -> bool {
        self.id.is_none() && self.method.is_some()
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ServiceRegistrationParams {
    #[serde(rename = "serviceId")]
    pub service_id: String,
    pub version: String,
    pub protocol: String,
    pub address: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub tags: HashMap<String, String>,
    #[serde(rename = "envTag", skip_serializing_if = "Option::is_none")]
    pub env_tag: Option<String>,
    pub jwt: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub struct ServiceMetadataUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<HashMap<String, String>>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct RegistrationResponse {
    #[serde(rename = "runtimeInstanceId")]
    pub runtime_instance_id: Uuid,
    pub status: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SkillSearchRequest {
    pub query: String,
    pub limit: Option<i32>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SkillSummary {
    pub skill_id: Uuid,
    pub name: String,
    pub description: String,
    pub tool_name: String,
    pub input_schema: Value,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SkillSearchResponse {
    pub skills: Vec<SkillSummary>,
}
