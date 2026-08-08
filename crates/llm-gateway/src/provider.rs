use async_stream::try_stream;
use async_trait::async_trait;
use futures_util::StreamExt;
use model_provider::inference::{
    EmbeddingCapabilities, EmbeddingProvider, EmbeddingRequest, EmbeddingResponse,
    GenerationCapabilities, GenerationProvider, GenerationStream, InferenceError, InferenceRequest,
    InferenceResponse, ProviderProtocol, ProviderRequestContext, StreamDecoder,
};
use model_provider::providers::{
    anthropic::{AnthropicCodec, AnthropicStreamDecoder},
    openai::{
        OpenAiCodec, OpenAiEmbeddingsCodec, OpenAiResponsesCodec, OpenAiResponsesStreamDecoder,
        OpenAiStreamDecoder,
    },
};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use std::io;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use crate::config::ProviderConfig;
use crate::error::LlmGatewayError;

const MAX_GENERATION_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

pub struct HttpInferenceProvider {
    protocol: ProviderProtocol,
    base_url: String,
    client: reqwest::Client,
    headers: HeaderMap,
    capabilities: GenerationCapabilities,
}

pub struct HttpEmbeddingProvider {
    protocol: ProviderProtocol,
    base_url: String,
    client: reqwest::Client,
    headers: HeaderMap,
    capabilities: EmbeddingCapabilities,
}

impl HttpEmbeddingProvider {
    pub fn build(
        config: &ProviderConfig,
        secret: &str,
        capabilities: EmbeddingCapabilities,
        timeout: Duration,
        allow_non_public_networks: bool,
    ) -> Result<Self, LlmGatewayError> {
        let transport = HttpInferenceProvider::build(
            config,
            secret,
            GenerationCapabilities::default(),
            timeout,
            allow_non_public_networks,
        )?;
        Ok(Self {
            protocol: transport.protocol,
            base_url: transport.base_url,
            client: transport.client,
            headers: transport.headers,
            capabilities,
        })
    }

    fn endpoint(&self) -> String {
        format!("{}/embeddings", self.base_url)
    }
}

impl HttpInferenceProvider {
    pub fn build(
        config: &ProviderConfig,
        secret: &str,
        capabilities: GenerationCapabilities,
        timeout: Duration,
        allow_non_public_networks: bool,
    ) -> Result<Self, LlmGatewayError> {
        let parsed = url::Url::parse(&config.base_url)
            .map_err(|error| LlmGatewayError::Config(format!("invalid provider URL: {error}")))?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err(LlmGatewayError::Config(
                "provider URL must have an http(s) host".to_string(),
            ));
        }
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(LlmGatewayError::Config(
                "provider URL must not contain user information".to_string(),
            ));
        }
        if parsed.query().is_some() || parsed.fragment().is_some() {
            return Err(LlmGatewayError::Config(
                "provider base URL must not contain a query or fragment".to_string(),
            ));
        }
        if !allow_non_public_networks
            && parsed
                .host()
                .and_then(|host| match host {
                    url::Host::Ipv4(address) => Some(IpAddr::V4(address)),
                    url::Host::Ipv6(address) => Some(IpAddr::V6(address)),
                    url::Host::Domain(_) => None,
                })
                .is_some_and(forbidden_provider_address)
        {
            return Err(LlmGatewayError::Config(
                "provider URL resolves to a forbidden network".to_string(),
            ));
        }
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        match config.provider_protocol {
            ProviderProtocol::OpenAiChat
            | ProviderProtocol::OpenAiResponses
            | ProviderProtocol::OpenAiEmbeddings => {
                headers.insert(
                    AUTHORIZATION,
                    HeaderValue::from_str(&format!("Bearer {secret}")).map_err(|_| {
                        LlmGatewayError::Config(
                            "provider secret is not a valid header value".to_string(),
                        )
                    })?,
                );
            }
            ProviderProtocol::AnthropicMessages => {
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
            if !allowed_provider_header(name) {
                return Err(LlmGatewayError::Config(format!(
                    "provider header `{name}` is not in the safe outbound allowlist"
                )));
            }
            if looks_like_secret(value) {
                return Err(LlmGatewayError::Config(format!(
                    "provider header `{name}` contains credential-like material"
                )));
            }
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
            .redirect(reqwest::redirect::Policy::none())
            // Ambient HTTP(S)_PROXY settings would bypass this client's DNS
            // confinement and make credential routing process-environment
            // dependent. A reviewed explicit egress proxy is a future config
            // contract, not an inherited environment side effect.
            .no_proxy()
            .dns_resolver(Arc::new(ProviderDnsResolver {
                allow_non_public_networks,
            }))
            .build()
            .map_err(|error| {
                LlmGatewayError::Config(format!("provider client build failed: {error}"))
            })?;
        Ok(Self {
            protocol: config.provider_protocol,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            client,
            headers,
            capabilities,
        })
    }

    fn endpoint(&self) -> String {
        match self.protocol {
            ProviderProtocol::OpenAiChat => format!("{}/chat/completions", self.base_url),
            ProviderProtocol::AnthropicMessages => format!("{}/messages", self.base_url),
            ProviderProtocol::OpenAiResponses => format!("{}/responses", self.base_url),
            ProviderProtocol::OpenAiEmbeddings => format!("{}/embeddings", self.base_url),
        }
    }
}

#[async_trait]
impl EmbeddingProvider for HttpEmbeddingProvider {
    fn protocol(&self) -> ProviderProtocol {
        self.protocol
    }

    fn capabilities(&self) -> EmbeddingCapabilities {
        self.capabilities.clone()
    }

    async fn embed(
        &self,
        context: ProviderRequestContext,
        request: EmbeddingRequest,
    ) -> Result<EmbeddingResponse, InferenceError> {
        context.check_active()?;
        if self.protocol != ProviderProtocol::OpenAiEmbeddings {
            return Err(InferenceError::unsupported(
                "embedding executor has a non-embedding provider protocol",
            ));
        }
        let expected_count = request.inputs.len();
        let expected_dimensions = request.dimensions;
        let outbound = self
            .client
            .post(self.endpoint())
            .headers(self.headers.clone())
            .json(&OpenAiEmbeddingsCodec.encode_request(&request));
        let response = tokio::select! {
            _ = context.cancellation.cancelled() => return Err(InferenceError::cancelled()),
            response = outbound.send() => response.map_err(|error| {
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
        let bytes = read_bounded_response(
            response,
            self.capabilities.max_response_bytes,
            &context.cancellation,
        )
        .await?;
        if !(200..300).contains(&status) {
            return Err(OpenAiCodec.decode_error(status, retry_after.as_deref(), &bytes));
        }
        let json = serde_json::from_slice(&bytes).map_err(|_| {
            InferenceError::provider_protocol(Some(status), "provider returned invalid JSON")
        })?;
        let response =
            OpenAiEmbeddingsCodec.decode_response(&json, expected_count, expected_dimensions)?;
        if response.vectors.iter().any(|vector| {
            u32::try_from(vector.values.len()).map_or(true, |dimensions| {
                !self.capabilities.supported_dimensions.contains(&dimensions)
            })
        }) {
            return Err(InferenceError::provider_protocol(
                Some(status),
                "provider returned an undeclared embedding dimension",
            ));
        }
        Ok(response)
    }
}

async fn read_bounded_response(
    response: reqwest::Response,
    limit: usize,
    cancellation: &tokio_util::sync::CancellationToken,
) -> Result<Vec<u8>, InferenceError> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(InferenceError::provider_protocol(
            Some(502),
            "provider response exceeds the configured byte limit",
        ));
    }
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    loop {
        let next = tokio::select! {
            _ = cancellation.cancelled() => return Err(InferenceError::cancelled()),
            next = stream.next() => next,
        };
        let Some(chunk) = next else { break };
        let chunk = chunk.map_err(|_| InferenceError::network("provider response body failed"))?;
        if chunk.len() > limit.saturating_sub(body.len()) {
            return Err(InferenceError::provider_protocol(
                Some(502),
                "provider response exceeds the configured byte limit",
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[derive(Debug)]
struct ProviderDnsResolver {
    allow_non_public_networks: bool,
}

impl Resolve for ProviderDnsResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        let allow_non_public_networks = self.allow_non_public_networks;
        Box::pin(async move {
            let addresses = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|error| Box::new(error) as Box<dyn std::error::Error + Send + Sync>)?
                .collect::<Vec<_>>();
            if addresses.is_empty() {
                return Err(Box::new(io::Error::new(
                    io::ErrorKind::NotFound,
                    "provider DNS returned no addresses",
                ))
                    as Box<dyn std::error::Error + Send + Sync>);
            }
            if !allow_non_public_networks
                && addresses
                    .iter()
                    .any(|address| forbidden_provider_address(address.ip()))
            {
                return Err(Box::new(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "provider DNS returned a forbidden address",
                ))
                    as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(addresses.into_iter()) as Addrs)
        })
    }
}

pub(crate) fn forbidden_provider_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            let [first, second, third, _] = address.octets();
            address.is_unspecified()
                || address.is_private()
                || address.is_loopback()
                || address.is_link_local()
                || address.is_multicast()
                || address.is_broadcast()
                || address.is_documentation()
                || first == 0
                || (first == 100 && (64..=127).contains(&second))
                || (first == 192 && second == 0 && third == 0)
                || (first == 198 && (second == 18 || second == 19))
                || first >= 240
        }
        IpAddr::V6(address) => {
            let segments = address.segments();
            let globally_routable = (segments[0] & 0xe000) == 0x2000;
            let documentation = segments[0] == 0x2001 && segments[1] == 0x0db8;
            !globally_routable || documentation
        }
    }
}

fn allowed_provider_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "accept" | "user-agent" | "openai-organization" | "openai-project" | "anthropic-beta"
    )
}

fn looks_like_secret(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed.starts_with("Bearer ")
        || trimmed.starts_with("sk-")
        || trimmed.starts_with("sk_")
        || trimmed.to_ascii_lowercase().contains("api_key=")
}

#[async_trait]
impl GenerationProvider for HttpInferenceProvider {
    fn protocol(&self) -> ProviderProtocol {
        self.protocol
    }
    fn capabilities(&self) -> GenerationCapabilities {
        self.capabilities.clone()
    }

    async fn generate(
        &self,
        context: ProviderRequestContext,
        request: InferenceRequest,
    ) -> Result<InferenceResponse, InferenceError> {
        context.check_active()?;
        let body = match self.protocol {
            ProviderProtocol::OpenAiChat => OpenAiCodec.encode_request(&request, false)?,
            ProviderProtocol::AnthropicMessages => {
                AnthropicCodec.encode_request(&request, false)?
            }
            ProviderProtocol::OpenAiResponses => {
                OpenAiResponsesCodec.encode_request(&request, false)?
            }
            ProviderProtocol::OpenAiEmbeddings => {
                return Err(InferenceError::unsupported(
                    "embedding protocol cannot execute generation",
                ));
            }
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
        let bytes = read_bounded_response(
            response,
            MAX_GENERATION_RESPONSE_BYTES,
            &context.cancellation,
        )
        .await?;
        if !(200..300).contains(&status) {
            return Err(match self.protocol {
                ProviderProtocol::OpenAiChat => {
                    OpenAiCodec.decode_error(status, retry_after.as_deref(), &bytes)
                }
                ProviderProtocol::AnthropicMessages => {
                    AnthropicCodec.decode_error(status, retry_after.as_deref(), &bytes)
                }
                ProviderProtocol::OpenAiResponses => {
                    OpenAiResponsesCodec.decode_error(status, retry_after.as_deref(), &bytes)
                }
                ProviderProtocol::OpenAiEmbeddings => {
                    InferenceError::unsupported("embedding protocol cannot execute generation")
                }
            });
        }
        let json = serde_json::from_slice(&bytes).map_err(|_| {
            InferenceError::provider_protocol(Some(status), "provider returned invalid JSON")
        })?;
        match self.protocol {
            ProviderProtocol::OpenAiChat => OpenAiCodec.decode_response(&json),
            ProviderProtocol::AnthropicMessages => AnthropicCodec.decode_response(&json),
            ProviderProtocol::OpenAiResponses => OpenAiResponsesCodec.decode_response(&json),
            ProviderProtocol::OpenAiEmbeddings => Err(InferenceError::unsupported(
                "embedding protocol cannot execute generation",
            )),
        }
    }

    async fn generate_stream(
        &self,
        context: ProviderRequestContext,
        request: InferenceRequest,
    ) -> Result<GenerationStream, InferenceError> {
        context.check_active()?;
        let body = match self.protocol {
            ProviderProtocol::OpenAiChat => OpenAiCodec.encode_request(&request, true)?,
            ProviderProtocol::AnthropicMessages => AnthropicCodec.encode_request(&request, true)?,
            ProviderProtocol::OpenAiResponses => {
                OpenAiResponsesCodec.encode_request(&request, true)?
            }
            ProviderProtocol::OpenAiEmbeddings => {
                return Err(InferenceError::unsupported(
                    "embedding protocol cannot execute generation",
                ));
            }
        };
        let outbound = self
            .client
            .post(self.endpoint())
            .headers(self.headers.clone())
            .json(&body);
        let response = tokio::select! {
            _ = context.cancellation.cancelled() => return Err(InferenceError::cancelled()),
            response = outbound.send() => response.map_err(|error| {
                if error.is_timeout() { InferenceError::timeout_after_possible_acceptance() }
                else { InferenceError::network("provider stream transport failed") }
            })?,
        };
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        if !(200..300).contains(&status) {
            let bytes = read_bounded_response(
                response,
                MAX_GENERATION_RESPONSE_BYTES,
                &context.cancellation,
            )
            .await?;
            return Err(match self.protocol {
                ProviderProtocol::OpenAiChat => {
                    OpenAiCodec.decode_error(status, retry_after.as_deref(), &bytes)
                }
                ProviderProtocol::AnthropicMessages => {
                    AnthropicCodec.decode_error(status, retry_after.as_deref(), &bytes)
                }
                ProviderProtocol::OpenAiResponses => {
                    OpenAiResponsesCodec.decode_error(status, retry_after.as_deref(), &bytes)
                }
                ProviderProtocol::OpenAiEmbeddings => {
                    InferenceError::unsupported("embedding protocol cannot execute generation")
                }
            });
        }
        let mut decoder: Box<dyn StreamDecoder + Send> = match self.protocol {
            ProviderProtocol::OpenAiChat => Box::new(OpenAiStreamDecoder::default()),
            ProviderProtocol::AnthropicMessages => Box::new(AnthropicStreamDecoder::default()),
            ProviderProtocol::OpenAiResponses => Box::new(OpenAiResponsesStreamDecoder::default()),
            ProviderProtocol::OpenAiEmbeddings => {
                return Err(InferenceError::unsupported(
                    "embedding protocol cannot execute generation",
                ));
            }
        };
        let cancellation = context.cancellation;
        let bytes = response.bytes_stream();
        let output = try_stream! {
            futures_util::pin_mut!(bytes);
            loop {
                let next = tokio::select! {
                    _ = cancellation.cancelled() => None,
                    next = bytes.next() => Some(next),
                };
                let Some(next) = next else {
                    Err(InferenceError::cancelled())?;
                    unreachable!();
                };
                let Some(chunk) = next else { break };
                let chunk = chunk.map_err(|_| InferenceError::network("provider stream body failed"))?;
                for event in decoder.push(&chunk)? {
                    yield event;
                }
            }
            for event in decoder.finish()? {
                yield event;
            }
        };
        Ok(Box::pin(output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, routing::post};
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use model_provider::inference::{EmbeddingEncoding, GenerateOutputItem};
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::collections::BTreeSet;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn config(base_url: &str) -> ProviderConfig {
        ProviderConfig {
            provider_protocol: ProviderProtocol::OpenAiChat,
            base_url: base_url.to_string(),
            secret_ref: "credential://provider/test".to_string(),
            headers: BTreeMap::new(),
            quota_group_id: None,
        }
    }

    #[test]
    fn production_provider_rejects_private_metadata_and_credential_urls() {
        for url in [
            "http://127.0.0.1/v1",
            "https://10.0.0.1/v1",
            "https://169.254.169.254/latest",
            "https://[::1]/v1",
            "https://user:password@provider.example/v1",
            "https://provider.example/v1?token=secret",
        ] {
            assert!(
                HttpInferenceProvider::build(
                    &config(url),
                    "secret",
                    GenerationCapabilities::default(),
                    Duration::from_secs(1),
                    false,
                )
                .is_err(),
                "unsafe provider URL accepted: {url}"
            );
        }
    }

    #[test]
    fn provider_network_classification_is_fail_closed() {
        for address in [
            IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(192, 0, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(198, 18, 0, 1)),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            "2001:db8::1".parse().unwrap(),
        ] {
            assert!(forbidden_provider_address(address), "accepted {address}");
        }
        assert!(!forbidden_provider_address("8.8.8.8".parse().unwrap()));
        assert!(!forbidden_provider_address(
            "2606:4700:4700::1111".parse().unwrap()
        ));
    }

    #[test]
    fn provider_headers_are_allowlisted_and_reject_credential_values() {
        let mut unsafe_name = config("https://provider.example/v1");
        unsafe_name
            .headers
            .insert("authorization".to_string(), "opaque".to_string());
        assert!(
            HttpInferenceProvider::build(
                &unsafe_name,
                "secret",
                GenerationCapabilities::default(),
                Duration::from_secs(1),
                false,
            )
            .is_err()
        );

        let mut unsafe_value = config("https://provider.example/v1");
        unsafe_value.headers.insert(
            "openai-organization".to_string(),
            "Bearer leaked".to_string(),
        );
        assert!(
            HttpInferenceProvider::build(
                &unsafe_value,
                "secret",
                GenerationCapabilities::default(),
                Duration::from_secs(1),
                false,
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn openai_embedding_provider_posts_and_decodes_bounded_base64() {
        let app = Router::new().route(
            "/v1/embeddings",
            post(
                |headers: axum::http::HeaderMap, Json(body): Json<serde_json::Value>| async move {
                    assert_eq!(headers["authorization"], "Bearer secret");
                    assert_eq!(body["encoding_format"], "float");
                    let encoded = STANDARD.encode(
                        [0.25_f32, -0.5_f32]
                            .into_iter()
                            .flat_map(f32::to_le_bytes)
                            .collect::<Vec<_>>(),
                    );
                    Json(json!({
                        "object":"list",
                        "data":[{"object":"embedding","index":0,"embedding":encoded}],
                        "model":"physical-embed",
                        "usage":{"prompt_tokens":4,"total_tokens":4}
                    }))
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut provider_config = config(&format!("http://{address}/v1"));
        provider_config.provider_protocol = ProviderProtocol::OpenAiEmbeddings;
        let provider = HttpEmbeddingProvider::build(
            &provider_config,
            "secret",
            EmbeddingCapabilities {
                max_batch_items: 8,
                max_input_tokens_per_item: 128,
                max_aggregate_input_tokens: 1024,
                supported_dimensions: BTreeSet::from([2]),
                supported_encodings: BTreeSet::from([
                    EmbeddingEncoding::Float,
                    EmbeddingEncoding::Base64,
                ]),
                max_response_bytes: 4096,
                space: None,
            },
            Duration::from_secs(1),
            true,
        )
        .unwrap();
        let response = provider
            .embed(
                ProviderRequestContext::with_timeout("attempt", Duration::from_secs(1)),
                EmbeddingRequest {
                    model: "physical-embed".to_string(),
                    inputs: vec!["hello".to_string()],
                    dimensions: Some(2),
                },
            )
            .await
            .unwrap();
        assert_eq!(response.vectors[0].values, vec![0.25, -0.5]);
        assert_eq!(response.usage.input_tokens, Some(4));
    }

    #[tokio::test]
    async fn openai_responses_provider_posts_and_decodes_typed_output() {
        let app = Router::new().route(
            "/v1/responses",
            post(
                |headers: axum::http::HeaderMap, Json(body): Json<serde_json::Value>| async move {
                    assert_eq!(headers["authorization"], "Bearer secret");
                    assert_eq!(body["store"], false);
                    assert_eq!(body["input"][0]["type"], "message");
                    Json(json!({
                        "id":"resp_private","model":"physical-responses","status":"completed",
                        "output":[{"id":"msg_private","type":"message","status":"completed","content":[{"type":"output_text","text":"ok"}]}],
                        "usage":{"input_tokens":3,"output_tokens":1,"total_tokens":4}
                    }))
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut provider_config = config(&format!("http://{address}/v1"));
        provider_config.provider_protocol = ProviderProtocol::OpenAiResponses;
        let provider = HttpInferenceProvider::build(
            &provider_config,
            "secret",
            GenerationCapabilities {
                content: model_provider::inference::ContentCapabilities {
                    text: true,
                    ..Default::default()
                },
                streaming: true,
            },
            Duration::from_secs(1),
            true,
        )
        .unwrap();
        let response = provider
            .generate(
                ProviderRequestContext::with_timeout("attempt", Duration::from_secs(1)),
                InferenceRequest::text("physical-responses", "hello"),
            )
            .await
            .unwrap();
        assert!(matches!(
            response.output[0],
            GenerateOutputItem::Message { .. }
        ));
        assert_eq!(response.usage.unwrap().output_tokens, Some(1));
    }
}
