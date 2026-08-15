use async_stream::try_stream;
use async_trait::async_trait;
use futures_util::StreamExt;
use hyper_util::client::legacy::connect::{Connection, HttpInfo};
use ipnet::IpNet;
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
use std::error::Error;
use std::future::Future;
use std::io;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;
use tower::{Layer, Service};

use crate::config::{EndpointAuth, ProviderConfig};
use crate::error::LlmGatewayError;

const MAX_GENERATION_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
type BoxError = Box<dyn Error + Send + Sync>;

fn transport_error(error: reqwest::Error) -> InferenceError {
    if error.is_timeout() {
        return InferenceError::timeout_after_possible_acceptance();
    }
    let mut source: Option<&(dyn Error + 'static)> = Some(&error);
    while let Some(current) = source {
        if current
            .downcast_ref::<io::Error>()
            .is_some_and(|error| error.kind() == io::ErrorKind::PermissionDenied)
        {
            return InferenceError::security_invariant(
                "provider transport violated the compiled destination policy",
            );
        }
        let detail = current.to_string().to_ascii_lowercase();
        if detail.contains("certificate")
            || detail.contains("invalid peer")
            || detail.contains("unknown issuer")
            || detail.contains("not valid for")
            || detail.contains("tls alert")
        {
            return InferenceError::security_invariant(
                "provider TLS identity or trust validation failed",
            );
        }
        source = current.source();
    }
    InferenceError::network("provider transport failed")
}

#[derive(Debug, Clone)]
pub struct CompiledAddressPolicy {
    public_only: bool,
    legacy_unrestricted: bool,
    networks: Arc<Vec<IpNet>>,
}

impl CompiledAddressPolicy {
    pub fn public_tls() -> Self {
        Self {
            public_only: true,
            legacy_unrestricted: false,
            networks: Arc::new(Vec::new()),
        }
    }

    pub fn private(networks: Vec<IpNet>) -> Result<Self, LlmGatewayError> {
        if networks.is_empty() {
            return Err(LlmGatewayError::Config(
                "private network zone must contain at least one CIDR".to_string(),
            ));
        }
        Ok(Self {
            public_only: false,
            legacy_unrestricted: false,
            networks: Arc::new(networks),
        })
    }

    fn legacy(allow_non_public_networks: bool) -> Self {
        if allow_non_public_networks {
            Self {
                public_only: false,
                legacy_unrestricted: true,
                networks: Arc::new(vec!["0.0.0.0/0".parse().unwrap(), "::/0".parse().unwrap()]),
            }
        } else {
            Self::public_tls()
        }
    }

    pub(crate) fn development() -> Self {
        Self::legacy(true)
    }

    pub fn permits(&self, address: IpAddr) -> bool {
        if self.legacy_unrestricted {
            true
        } else if self.public_only {
            !forbidden_provider_address(address)
        } else {
            safe_private_provider_address(address)
                && self
                    .networks
                    .iter()
                    .any(|network| network.contains(&address))
        }
    }
}

/// Private zones are an additional allowlist, not an escape hatch from the
/// provider destination policy. Only explicitly private address space is
/// meaningful for a private provider endpoint; public, loopback, link-local,
/// metadata, multicast, documentation and unspecified destinations remain
/// forbidden even when a zone was configured too broadly.
fn safe_private_provider_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => address.is_private(),
        IpAddr::V6(address) => (address.segments()[0] & 0xfe00) == 0xfc00,
    }
}

#[derive(Debug, Clone)]
pub struct ProviderTransportMaterial {
    pub address_policy: CompiledAddressPolicy,
    pub trust_bundle_pem: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
struct PeerCheckLayer {
    policy: CompiledAddressPolicy,
}

impl<S> Layer<S> for PeerCheckLayer {
    type Service = PeerCheckService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        PeerCheckService {
            inner,
            policy: self.policy.clone(),
        }
    }
}

#[derive(Debug, Clone)]
struct PeerCheckService<S> {
    inner: S,
    policy: CompiledAddressPolicy,
}

impl<S, Request> Service<Request> for PeerCheckService<S>
where
    S: Service<Request, Error = BoxError> + Clone + Send + Sync + 'static,
    S::Response: Connection + Send + 'static,
    S::Future: Send + 'static,
    Request: Send + 'static,
{
    type Response = S::Response;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(context)
    }

    fn call(&mut self, request: Request) -> Self::Future {
        // `poll_ready` and `call` must address the same service instance. Keep
        // a fresh clone in `self` for the next request and move the instance
        // that was actually polled into this future.
        let replacement = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, replacement);
        let policy = self.policy.clone();
        Box::pin(async move {
            let connection = inner.call(request).await?;
            let mut extensions = http::Extensions::new();
            connection.connected().get_extras(&mut extensions);
            let peer = extensions
                .get::<HttpInfo>()
                .map(HttpInfo::remote_addr)
                .ok_or_else(|| {
                    Box::new(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "provider connector did not expose a peer address",
                    )) as BoxError
                })?;
            if !policy.permits(peer.ip()) {
                return Err(Box::new(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "provider connected peer is outside the compiled address policy",
                )) as BoxError);
            }
            Ok(connection)
        })
    }
}

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
        Self::build_with_auth(
            config,
            Some(secret),
            capabilities,
            timeout,
            allow_non_public_networks,
        )
    }

    pub fn build_with_auth(
        config: &ProviderConfig,
        secret: Option<&str>,
        capabilities: EmbeddingCapabilities,
        timeout: Duration,
        allow_non_public_networks: bool,
    ) -> Result<Self, LlmGatewayError> {
        let transport = HttpInferenceProvider::build_with_material(
            config,
            secret,
            GenerationCapabilities::default(),
            timeout,
            ProviderTransportMaterial {
                address_policy: CompiledAddressPolicy::legacy(allow_non_public_networks),
                trust_bundle_pem: None,
            },
        )?;
        Ok(Self {
            protocol: transport.protocol,
            base_url: transport.base_url,
            client: transport.client,
            headers: transport.headers,
            capabilities,
        })
    }

    pub fn build_with_material(
        config: &ProviderConfig,
        secret: Option<&str>,
        capabilities: EmbeddingCapabilities,
        timeout: Duration,
        material: ProviderTransportMaterial,
    ) -> Result<Self, LlmGatewayError> {
        let transport = HttpInferenceProvider::build_with_material(
            config,
            secret,
            GenerationCapabilities::default(),
            timeout,
            material,
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
        Self::build_with_auth(
            config,
            Some(secret),
            capabilities,
            timeout,
            allow_non_public_networks,
        )
    }

    pub fn build_with_auth(
        config: &ProviderConfig,
        secret: Option<&str>,
        capabilities: GenerationCapabilities,
        timeout: Duration,
        allow_non_public_networks: bool,
    ) -> Result<Self, LlmGatewayError> {
        Self::build_with_material(
            config,
            secret,
            capabilities,
            timeout,
            ProviderTransportMaterial {
                address_policy: CompiledAddressPolicy::legacy(allow_non_public_networks),
                trust_bundle_pem: None,
            },
        )
    }

    pub fn build_with_material(
        config: &ProviderConfig,
        secret: Option<&str>,
        capabilities: GenerationCapabilities,
        timeout: Duration,
        material: ProviderTransportMaterial,
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
        if parsed
            .host()
            .and_then(|host| match host {
                url::Host::Ipv4(address) => Some(IpAddr::V4(address)),
                url::Host::Ipv6(address) => Some(IpAddr::V6(address)),
                url::Host::Domain(_) => None,
            })
            .is_some_and(|address| !material.address_policy.permits(address))
        {
            return Err(LlmGatewayError::Config(
                "provider URL resolves to a forbidden network".to_string(),
            ));
        }
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        match (&config.endpoint_auth, config.provider_protocol) {
            (EndpointAuth::None, _) => {}
            (EndpointAuth::Bearer { .. }, _) => {
                let secret = secret.ok_or_else(|| {
                    LlmGatewayError::Config(
                        "provider endpoint bearer credential was not resolved".to_string(),
                    )
                })?;
                headers.insert(
                    AUTHORIZATION,
                    HeaderValue::from_str(&format!("Bearer {secret}")).map_err(|_| {
                        LlmGatewayError::Config(
                            "provider secret is not a valid header value".to_string(),
                        )
                    })?,
                );
            }
            (EndpointAuth::ApiKey { header, .. }, _) => {
                let secret = secret.ok_or_else(|| {
                    LlmGatewayError::Config(
                        "provider endpoint API-key credential was not resolved".to_string(),
                    )
                })?;
                headers.insert(
                    HeaderName::from_static(header.wire_name()),
                    HeaderValue::from_str(secret).map_err(|_| {
                        LlmGatewayError::Config(
                            "provider secret is not a valid header value".to_string(),
                        )
                    })?,
                );
            }
            (EndpointAuth::BedrockApiKey { .. } | EndpointAuth::AwsSigV4 { .. }, _) => {
                return Err(LlmGatewayError::Config(
                    "Bedrock endpoint auth must use BedrockConverseProvider".to_string(),
                ));
            }
        }
        if config.provider_protocol == ProviderProtocol::AnthropicMessages {
            headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
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
        let mut builder = reqwest::Client::builder()
            .timeout(timeout)
            .pool_idle_timeout(Duration::from_millis(
                config.network_profile.connection.pool_idle_timeout_ms,
            ))
            .redirect(reqwest::redirect::Policy::none())
            // Ambient HTTP(S)_PROXY settings would bypass this client's DNS
            // confinement and make credential routing process-environment
            // dependent. A reviewed explicit egress proxy is a future config
            // contract, not an inherited environment side effect.
            .no_proxy()
            .dns_resolver(Arc::new(ProviderDnsResolver {
                policy: material.address_policy.clone(),
            }))
            .connector_layer(PeerCheckLayer {
                policy: material.address_policy,
            });
        if let Some(pem) = material.trust_bundle_pem {
            let certificates = reqwest::Certificate::from_pem_bundle(&pem).map_err(|_| {
                LlmGatewayError::Config("provider trust bundle is not valid PEM".to_string())
            })?;
            if certificates.is_empty() {
                return Err(LlmGatewayError::Config(
                    "provider trust bundle contains no certificates".to_string(),
                ));
            }
            // A private trust bundle is authoritative. Adding it to the
            // platform roots would let an unrelated public CA authenticate a
            // private endpoint with the same DNS name.
            builder = builder.tls_built_in_root_certs(false);
            for certificate in certificates {
                builder = builder.add_root_certificate(certificate);
            }
        }
        let client = builder.build().map_err(|error| {
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
            ProviderProtocol::BedrockConverse => self.base_url.clone(),
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
            response = outbound.send() => response.map_err(transport_error)?,
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
    policy: CompiledAddressPolicy,
}

impl Resolve for ProviderDnsResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        let policy = self.policy.clone();
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
            let addresses = addresses
                .into_iter()
                .filter(|address| policy.permits(address.ip()))
                .collect::<Vec<_>>();
            if addresses.is_empty() {
                return Err(Box::new(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "provider DNS returned no address allowed by the compiled policy",
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
            ProviderProtocol::BedrockConverse => {
                return Err(InferenceError::unsupported(
                    "Bedrock protocol requires BedrockConverseProvider",
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
            response = request.send() => response.map_err(transport_error)?,
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
                ProviderProtocol::BedrockConverse => {
                    InferenceError::unsupported("Bedrock protocol requires BedrockConverseProvider")
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
            ProviderProtocol::BedrockConverse => Err(InferenceError::unsupported(
                "Bedrock protocol requires BedrockConverseProvider",
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
            ProviderProtocol::BedrockConverse => {
                return Err(InferenceError::unsupported(
                    "Bedrock protocol requires BedrockConverseProvider",
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
                ProviderProtocol::BedrockConverse => {
                    InferenceError::unsupported("Bedrock protocol requires BedrockConverseProvider")
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
            ProviderProtocol::BedrockConverse => {
                return Err(InferenceError::unsupported(
                    "Bedrock protocol requires BedrockConverseProvider",
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
    use rcgen::{
        BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
        KeyUsagePurpose, date_time_ymd,
    };
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::collections::BTreeSet;
    use std::net::{Ipv4Addr, Ipv6Addr};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::task::JoinHandle;
    use tokio_rustls::TlsAcceptor;

    fn config(base_url: &str) -> ProviderConfig {
        ProviderConfig {
            provider_account_id: "provider-test".to_string(),
            provider_type: Default::default(),
            provider_protocol: ProviderProtocol::OpenAiChat,
            aws_region: None,
            material_generation: 1,
            base_url: base_url.to_string(),
            endpoint_auth: EndpointAuth::Bearer {
                credential_ref: "credential://provider/test".to_string(),
            },
            network_profile: Default::default(),
            headers: BTreeMap::new(),
            quota_group_id: None,
        }
    }

    struct TestCertificate {
        ca_pem: Vec<u8>,
        certificate: CertificateDer<'static>,
        private_key: PrivateKeyDer<'static>,
    }

    fn test_certificate(subject_alt_name: &str, expired: bool) -> TestCertificate {
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
        ];
        let ca_key = KeyPair::generate().unwrap();
        let ca_certificate = ca_params.self_signed(&ca_key).unwrap();
        let issuer = Issuer::new(ca_params, ca_key);

        let mut leaf_params = CertificateParams::new(vec![subject_alt_name.to_string()]).unwrap();
        leaf_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        if expired {
            leaf_params.not_before = date_time_ymd(2018, 1, 1);
            leaf_params.not_after = date_time_ymd(2019, 1, 1);
        }
        let leaf_key = KeyPair::generate().unwrap();
        let leaf_certificate = leaf_params.signed_by(&leaf_key, &issuer).unwrap();
        TestCertificate {
            ca_pem: ca_certificate.pem().into_bytes(),
            certificate: leaf_certificate.der().clone(),
            private_key: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der())),
        }
    }

    async fn start_tls_provider(
        certificate: TestCertificate,
    ) -> (
        std::net::SocketAddr,
        Arc<AtomicUsize>,
        JoinHandle<()>,
        Vec<u8>,
    ) {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let server = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![certificate.certificate], certificate.private_key)
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let request_bytes = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&request_bytes);
        let task = tokio::spawn(async move {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let Ok(mut stream) = TlsAcceptor::from(Arc::new(server)).accept(stream).await else {
                return;
            };
            let mut request = Vec::new();
            let mut buffer = [0_u8; 2048];
            loop {
                let Ok(read) = stream.read(&mut buffer).await else {
                    return;
                };
                if read == 0 {
                    return;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            observed.store(request.len(), Ordering::SeqCst);
            let body = br#"{"id":"chatcmpl-tls","object":"chat.completion","model":"physical","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            if stream.write_all(response.as_bytes()).await.is_ok() {
                let _ = stream.write_all(body).await;
                let _ = stream.shutdown().await;
            }
        });
        (address, request_bytes, task, certificate.ca_pem)
    }

    fn private_tls_client(
        host: &str,
        port: u16,
        policy: CompiledAddressPolicy,
        trust_bundle_pem: Option<Vec<u8>>,
    ) -> HttpInferenceProvider {
        let mut provider_config = config(&format!("https://{host}:{port}/v1"));
        provider_config.endpoint_auth = EndpointAuth::None;
        HttpInferenceProvider::build_with_material(
            &provider_config,
            None,
            GenerationCapabilities {
                content: model_provider::inference::ContentCapabilities {
                    text: true,
                    ..Default::default()
                },
                streaming: false,
            },
            Duration::from_secs(2),
            ProviderTransportMaterial {
                address_policy: policy,
                trust_bundle_pem,
            },
        )
        .unwrap()
    }

    async fn tls_generate(
        provider: &HttpInferenceProvider,
    ) -> Result<InferenceResponse, InferenceError> {
        provider
            .generate(
                ProviderRequestContext::with_timeout("tls-attempt", Duration::from_secs(2)),
                InferenceRequest::text("physical", "qualification"),
            )
            .await
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

        let approved_ula = CompiledAddressPolicy::private(vec!["fd42::/16".parse().unwrap()])
            .expect("approved ULA policy");
        assert!(approved_ula.permits("fd42::1".parse().unwrap()));
        assert!(!approved_ula.permits("fd43::1".parse().unwrap()));

        let dangerously_broad = CompiledAddressPolicy::private(vec![
            "0.0.0.0/0".parse().unwrap(),
            "::/0".parse().unwrap(),
        ])
        .unwrap();
        assert!(dangerously_broad.permits("10.1.2.3".parse().unwrap()));
        assert!(dangerously_broad.permits("fd42::1".parse().unwrap()));
        for forbidden in [
            "8.8.8.8",
            "100.100.100.200",
            "169.254.169.254",
            "127.0.0.1",
            "::1",
            "ff02::1",
        ] {
            assert!(
                !dangerously_broad.permits(forbidden.parse().unwrap()),
                "broad private zone admitted {forbidden}"
            );
        }
    }

    #[tokio::test]
    async fn private_tls_enforces_ca_san_expiry_ip_san_and_zone() {
        let loopback_policy = || CompiledAddressPolicy::legacy(true);

        let (address, observed, task, ca_pem) =
            start_tls_provider(test_certificate("localhost", false)).await;
        let provider =
            private_tls_client("localhost", address.port(), loopback_policy(), Some(ca_pem));
        tls_generate(&provider).await.unwrap();
        task.await.unwrap();
        assert!(observed.load(Ordering::SeqCst) > 0);

        let (address, observed, task, ca_pem) =
            start_tls_provider(test_certificate("other.invalid", false)).await;
        let provider =
            private_tls_client("localhost", address.port(), loopback_policy(), Some(ca_pem));
        assert!(tls_generate(&provider).await.is_err());
        task.await.unwrap();
        assert_eq!(observed.load(Ordering::SeqCst), 0);

        let (address, observed, task, ca_pem) =
            start_tls_provider(test_certificate("localhost", true)).await;
        let provider =
            private_tls_client("localhost", address.port(), loopback_policy(), Some(ca_pem));
        assert!(tls_generate(&provider).await.is_err());
        task.await.unwrap();
        assert_eq!(observed.load(Ordering::SeqCst), 0);

        let (address, observed, task, _ca_pem) =
            start_tls_provider(test_certificate("localhost", false)).await;
        let provider = private_tls_client("localhost", address.port(), loopback_policy(), None);
        assert!(tls_generate(&provider).await.is_err());
        task.await.unwrap();
        assert_eq!(observed.load(Ordering::SeqCst), 0);

        let (address, observed, task, ca_pem) =
            start_tls_provider(test_certificate("127.0.0.1", false)).await;
        let provider =
            private_tls_client("127.0.0.1", address.port(), loopback_policy(), Some(ca_pem));
        tls_generate(&provider).await.unwrap();
        task.await.unwrap();
        assert!(observed.load(Ordering::SeqCst) > 0);

        let (address, observed, task, ca_pem) =
            start_tls_provider(test_certificate("localhost", false)).await;
        let out_of_zone =
            CompiledAddressPolicy::private(vec!["10.0.0.0/8".parse().unwrap()]).unwrap();
        let provider = private_tls_client("localhost", address.port(), out_of_zone, Some(ca_pem));
        assert!(tls_generate(&provider).await.is_err());
        assert_eq!(observed.load(Ordering::SeqCst), 0);
        task.abort();
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
    async fn endpoint_auth_none_emits_no_credential_header() {
        let app = Router::new().route(
            "/v1/chat/completions",
            post(
                |headers: axum::http::HeaderMap, Json(_body): Json<serde_json::Value>| async move {
                    assert!(!headers.contains_key("authorization"));
                    assert!(!headers.contains_key("x-api-key"));
                    Json(json!({
                        "id":"chatcmpl-none","object":"chat.completion","model":"physical",
                        "choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],
                        "usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}
                    }))
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut provider_config = config(&format!("http://{address}/v1"));
        provider_config.endpoint_auth = EndpointAuth::None;
        let provider = HttpInferenceProvider::build_with_auth(
            &provider_config,
            None,
            GenerationCapabilities {
                content: model_provider::inference::ContentCapabilities {
                    text: true,
                    ..Default::default()
                },
                streaming: false,
            },
            Duration::from_secs(1),
            true,
        )
        .unwrap();
        let response = provider
            .generate(
                ProviderRequestContext::with_timeout("attempt", Duration::from_secs(1)),
                InferenceRequest::text("physical", "qualification"),
            )
            .await
            .unwrap();
        assert_eq!(response.output.len(), 1);
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
