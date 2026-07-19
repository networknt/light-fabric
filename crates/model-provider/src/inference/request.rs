use super::content::Message;
use super::error::InferenceError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Tool { name: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ResponseFormat {
    Text,
    JsonObject,
    JsonSchema { name: String, schema: Value },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SamplingOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
}

impl TokenLimits {
    pub fn from_openai(
        max_tokens: Option<u32>,
        max_completion_tokens: Option<u32>,
    ) -> Result<Self, InferenceError> {
        match (max_tokens, max_completion_tokens) {
            (Some(left), Some(right)) if left != right => Err(InferenceError::invalid_request(
                "max_tokens conflicts with max_completion_tokens",
            )),
            (Some(value), _) | (_, Some(value)) => Ok(Self {
                max_output_tokens: Some(value),
            }),
            (None, None) => Ok(Self::default()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InferenceRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDefinition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_format: Option<ResponseFormat>,
    #[serde(default)]
    pub sampling: SamplingOptions,
    #[serde(default)]
    pub token_limits: TokenLimits,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, Value>,
}

impl InferenceRequest {
    pub fn text(model: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            messages: vec![Message::text(super::content::Role::User, text)],
            tools: Vec::new(),
            tool_choice: None,
            response_format: None,
            sampling: SamplingOptions::default(),
            token_limits: TokenLimits::default(),
            extensions: BTreeMap::new(),
        }
    }
}
