use async_stream::try_stream;
use async_trait::async_trait;
use futures_util::StreamExt;
use model_provider::inference::{
    InferenceError, InferenceProvider, InferenceRequest, InferenceResponse, InferenceStream,
    ProviderCapabilities, ProviderFormat, ProviderRequestContext, StreamDecoder,
};
use model_provider::providers::{
    anthropic::{AnthropicCodec, AnthropicStreamDecoder},
    openai::{OpenAiCodec, OpenAiStreamDecoder},
};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use std::io;
use std::net::IpAddr;
use std::sync::Arc;
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
        context: ProviderRequestContext,
        request: InferenceRequest,
    ) -> Result<InferenceStream, InferenceError> {
        context.check_active()?;
        let body = match self.format {
            ProviderFormat::OpenAi => OpenAiCodec.encode_request(&request, true)?,
            ProviderFormat::Anthropic => AnthropicCodec.encode_request(&request, true)?,
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
            let bytes = response
                .bytes()
                .await
                .map_err(|_| InferenceError::network("provider error body failed"))?;
            return Err(match self.format {
                ProviderFormat::OpenAi => {
                    OpenAiCodec.decode_error(status, retry_after.as_deref(), &bytes)
                }
                ProviderFormat::Anthropic => {
                    AnthropicCodec.decode_error(status, retry_after.as_deref(), &bytes)
                }
            });
        }
        let mut decoder: Box<dyn StreamDecoder + Send> = match self.format {
            ProviderFormat::OpenAi => Box::new(OpenAiStreamDecoder::default()),
            ProviderFormat::Anthropic => Box::new(AnthropicStreamDecoder::default()),
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
    use std::collections::BTreeMap;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn config(base_url: &str) -> ProviderConfig {
        ProviderConfig {
            format: ProviderFormat::OpenAi,
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
                    ProviderCapabilities::default(),
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
                ProviderCapabilities::default(),
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
                ProviderCapabilities::default(),
                Duration::from_secs(1),
                false,
            )
            .is_err()
        );
    }
}
