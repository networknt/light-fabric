use async_trait::async_trait;
use model_provider::inference::{
    InferenceError, InferenceProvider, InferenceRequest, InferenceResponse, InferenceStream,
    ProviderCapabilities, ProviderFormat, ProviderRequestContext,
};
use model_provider::providers::{anthropic::AnthropicCodec, openai::OpenAiCodec};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use std::time::Duration;

use crate::config::ProviderConfig;
use crate::error::LlmGatewayError;

pub struct HttpInferenceProvider {
    format: ProviderFormat,
    base_url: String,
    client: reqwest::Client,
    headers: HeaderMap,
    capabilities: ProviderCapabilities,
}

impl HttpInferenceProvider {
    pub fn build(
        config: &ProviderConfig,
        secret: &str,
        capabilities: ProviderCapabilities,
        timeout: Duration,
    ) -> Result<Self, LlmGatewayError> {
        let parsed = url::Url::parse(&config.base_url)
            .map_err(|error| LlmGatewayError::Config(format!("invalid provider URL: {error}")))?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err(LlmGatewayError::Config(
                "provider URL must have an http(s) host".to_string(),
            ));
        }
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        match config.format {
            ProviderFormat::OpenAi => {
                headers.insert(
                    AUTHORIZATION,
                    HeaderValue::from_str(&format!("Bearer {secret}")).map_err(|_| {
                        LlmGatewayError::Config(
                            "provider secret is not a valid header value".to_string(),
                        )
                    })?,
                );
            }
            ProviderFormat::Anthropic => {
                headers.insert(
                    "x-api-key",
                    HeaderValue::from_str(secret).map_err(|_| {
                        LlmGatewayError::Config(
                            "provider secret is not a valid header value".to_string(),
                        )
                    })?,
                );
                headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
            }
        }
        for (name, value) in &config.headers {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                LlmGatewayError::Config(format!("invalid provider header `{name}`"))
            })?;
            let value = HeaderValue::from_str(value).map_err(|_| {
                LlmGatewayError::Config("invalid provider header value".to_string())
            })?;
            headers.insert(name, value);
        }
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|error| {
                LlmGatewayError::Config(format!("provider client build failed: {error}"))
            })?;
        Ok(Self {
            format: config.format,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            client,
            headers,
            capabilities,
        })
    }

    fn endpoint(&self) -> String {
        match self.format {
            ProviderFormat::OpenAi => format!("{}/chat/completions", self.base_url),
            ProviderFormat::Anthropic => format!("{}/messages", self.base_url),
        }
    }
}

#[async_trait]
impl InferenceProvider for HttpInferenceProvider {
    fn format(&self) -> ProviderFormat {
        self.format
    }
    fn capabilities(&self) -> ProviderCapabilities {
        self.capabilities.clone()
    }

    async fn infer(
        &self,
        context: ProviderRequestContext,
        request: InferenceRequest,
    ) -> Result<InferenceResponse, InferenceError> {
        context.check_active()?;
        let body = match self.format {
            ProviderFormat::OpenAi => OpenAiCodec.encode_request(&request, false)?,
            ProviderFormat::Anthropic => AnthropicCodec.encode_request(&request, false)?,
        };
        let request = self
            .client
            .post(self.endpoint())
            .headers(self.headers.clone())
            .json(&body);
        let response = tokio::select! {
            _ = context.cancellation.cancelled() => return Err(InferenceError::cancelled()),
            response = request.send() => response.map_err(|error| {
                if error.is_timeout() { InferenceError::timeout_after_possible_acceptance() }
                else { InferenceError::network("provider transport failed") }
            })?,
        };
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let bytes = response
            .bytes()
            .await
            .map_err(|_| InferenceError::network("provider response body failed"))?;
        if !(200..300).contains(&status) {
            return Err(match self.format {
                ProviderFormat::OpenAi => {
                    OpenAiCodec.decode_error(status, retry_after.as_deref(), &bytes)
                }
                ProviderFormat::Anthropic => {
                    AnthropicCodec.decode_error(status, retry_after.as_deref(), &bytes)
                }
            });
        }
        let json = serde_json::from_slice(&bytes).map_err(|_| {
            InferenceError::provider_protocol(Some(status), "provider returned invalid JSON")
        })?;
        match self.format {
            ProviderFormat::OpenAi => OpenAiCodec.decode_response(&json),
            ProviderFormat::Anthropic => AnthropicCodec.decode_response(&json),
        }
    }

    async fn stream(
        &self,
        _context: ProviderRequestContext,
        _request: InferenceRequest,
    ) -> Result<InferenceStream, InferenceError> {
        Err(InferenceError::unsupported(
            "buffered HTTP runtime does not expose streaming",
        ))
    }
}
