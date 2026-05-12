use crate::http::{ClientBuildError, ClientFactory, EndpointOptions};
use crate::provider::{ResolvedDerefProvider, ResolvedSignKeyProvider, ResolvedSignProvider};
use crate::{ClientConfig, ClientRequestConfig};
use reqwest::header::CONTENT_TYPE;
use serde::Serialize;
use serde_json::Value;
use std::fmt;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthEndpoint {
    Sign,
    SignKey,
    Deref,
}

impl OAuthEndpoint {
    fn label(self) -> &'static str {
        match self {
            Self::Sign => "oauth.sign",
            Self::SignKey => "oauth.sign.key",
            Self::Deref => "oauth.deref",
        }
    }
}

impl fmt::Display for OAuthEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug)]
pub enum OAuthClientError {
    ClientBuild(ClientBuildError),
    MissingServerUrl {
        endpoint: OAuthEndpoint,
        service_id: Option<String>,
    },
    InvalidUrl {
        endpoint: OAuthEndpoint,
        url: String,
        source: url::ParseError,
    },
    Request {
        endpoint: OAuthEndpoint,
        source: reqwest::Error,
    },
    ResponseBody {
        endpoint: OAuthEndpoint,
        source: reqwest::Error,
    },
    HttpStatus {
        endpoint: OAuthEndpoint,
        status: reqwest::StatusCode,
        body: String,
    },
}

impl fmt::Display for OAuthClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ClientBuild(source) => write!(f, "{source}"),
            Self::MissingServerUrl {
                endpoint,
                service_id,
            } => {
                if let Some(service_id) = service_id {
                    write!(
                        f,
                        "{endpoint} uses serviceId `{service_id}`, but direct OAuth calls require a resolved server_url"
                    )
                } else {
                    write!(f, "{endpoint} requires server_url")
                }
            }
            Self::InvalidUrl {
                endpoint,
                url,
                source,
            } => write!(f, "invalid {endpoint} URL `{url}`: {source}"),
            Self::Request { endpoint, source } => {
                write!(f, "{endpoint} request failed: {source}")
            }
            Self::ResponseBody { endpoint, source } => {
                write!(f, "{endpoint} response body failed: {source}")
            }
            Self::HttpStatus {
                endpoint,
                status,
                body,
            } => write!(f, "{endpoint} returned {status}: {body}"),
        }
    }
}

impl std::error::Error for OAuthClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ClientBuild(source) => Some(source),
            Self::InvalidUrl { source, .. } => Some(source),
            Self::Request { source, .. } => Some(source),
            Self::ResponseBody { source, .. } => Some(source),
            Self::MissingServerUrl { .. } | Self::HttpStatus { .. } => None,
        }
    }
}

impl From<ClientBuildError> for OAuthClientError {
    fn from(value: ClientBuildError) -> Self {
        Self::ClientBuild(value)
    }
}

#[derive(Clone)]
pub struct OAuthClient {
    factory: ClientFactory,
    request: ClientRequestConfig,
}

impl OAuthClient {
    pub fn from_config(config: &ClientConfig) -> Self {
        Self {
            factory: ClientFactory::from_config(config),
            request: config.request.clone(),
        }
    }

    pub fn from_factory(factory: ClientFactory, request: ClientRequestConfig) -> Self {
        Self { factory, request }
    }

    pub async fn sign_payload<T>(
        &self,
        provider: &ResolvedSignProvider,
        payload: &T,
    ) -> Result<String, OAuthClientError>
    where
        T: Serialize + ?Sized,
    {
        let endpoint = OAuthEndpoint::Sign;
        let url = endpoint_url(
            endpoint,
            provider.server_url.as_deref(),
            provider.sign_service_id.as_deref(),
            &provider.uri,
        )?;
        let client = self.factory.reqwest_client(EndpointOptions {
            server_url: provider.server_url.clone(),
            service_id: provider.sign_service_id.clone(),
            proxy_host: provider.proxy_host.clone(),
            proxy_port: provider.proxy_port,
            enable_http2: Some(provider.enable_http2),
            timeout_ms: Some(provider.timeout),
            ..EndpointOptions::default()
        })?;
        let response = self
            .send_with_retries(endpoint, || {
                apply_basic_auth(
                    client.post(url.clone()).json(payload),
                    &provider.client_id,
                    &provider.client_secret,
                )
            })
            .await?;
        read_response_string(endpoint, response, true).await
    }

    pub async fn sign_key(
        &self,
        provider: &ResolvedSignKeyProvider,
        key_id: Option<&str>,
    ) -> Result<String, OAuthClientError> {
        let endpoint = OAuthEndpoint::SignKey;
        let mut url = endpoint_url_with_optional_segment(
            endpoint,
            provider.server_url.as_deref(),
            provider.key_service_id.as_deref(),
            &provider.uri,
            "{kid}",
            key_id,
        )?;
        if let Some(audience) = provider
            .audience
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            url.query_pairs_mut().append_pair("audience", audience);
        }
        let client = self.factory.reqwest_client(EndpointOptions {
            server_url: provider.server_url.clone(),
            service_id: provider.key_service_id.clone(),
            enable_http2: Some(provider.enable_http2),
            ..EndpointOptions::default()
        })?;
        let response = self
            .send_with_retries(endpoint, || {
                apply_basic_auth(
                    client.get(url.clone()),
                    &provider.client_id,
                    &provider.client_secret,
                )
            })
            .await?;
        read_response_string(endpoint, response, false).await
    }

    pub async fn deref_token(
        &self,
        provider: &ResolvedDerefProvider,
        token: &str,
    ) -> Result<String, OAuthClientError> {
        let endpoint = OAuthEndpoint::Deref;
        let url = endpoint_url_with_optional_segment(
            endpoint,
            provider.server_url.as_deref(),
            provider.deref_service_id.as_deref(),
            &provider.uri,
            "{token}",
            Some(token),
        )?;
        let client = self.factory.reqwest_client(EndpointOptions {
            server_url: provider.server_url.clone(),
            service_id: provider.deref_service_id.clone(),
            proxy_host: provider.proxy_host.clone(),
            proxy_port: provider.proxy_port,
            enable_http2: Some(provider.enable_http2),
            ..EndpointOptions::default()
        })?;
        let response = self
            .send_with_retries(endpoint, || {
                apply_basic_auth(
                    client.get(url.clone()),
                    &provider.client_id,
                    &provider.client_secret,
                )
            })
            .await?;
        read_response_string(endpoint, response, true).await
    }

    async fn send_with_retries<F>(
        &self,
        endpoint: OAuthEndpoint,
        mut build_request: F,
    ) -> Result<reqwest::Response, OAuthClientError>
    where
        F: FnMut() -> reqwest::RequestBuilder,
    {
        let mut retries = 0;
        loop {
            match build_request().send().await {
                Ok(response)
                    if response.status().is_server_error()
                        && retries < self.request.max_request_retry =>
                {
                    retries += 1;
                    self.retry_delay().await;
                }
                Ok(response) => return Ok(response),
                Err(source) if retries < self.request.max_request_retry => {
                    retries += 1;
                    self.retry_delay().await;
                    drop(source);
                }
                Err(source) => return Err(OAuthClientError::Request { endpoint, source }),
            }
        }
    }

    async fn retry_delay(&self) {
        if self.request.request_retry_delay > 0 {
            tokio::time::sleep(Duration::from_millis(self.request.request_retry_delay)).await;
        }
    }
}

fn apply_basic_auth(
    request: reqwest::RequestBuilder,
    client_id: &str,
    client_secret: &str,
) -> reqwest::RequestBuilder {
    if client_id.trim().is_empty() {
        request
    } else {
        request.basic_auth(client_id.to_string(), Some(client_secret.to_string()))
    }
}

async fn read_response_string(
    endpoint: OAuthEndpoint,
    response: reqwest::Response,
    extract_token: bool,
) -> Result<String, OAuthClientError> {
    let status = response.status();
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let body = response
        .text()
        .await
        .map_err(|source| OAuthClientError::ResponseBody { endpoint, source })?;
    if !status.is_success() {
        return Err(OAuthClientError::HttpStatus {
            endpoint,
            status,
            body,
        });
    }
    if extract_token && (content_type.contains("json") || body.trim_start().starts_with('{')) {
        return Ok(extract_string_value(&body).unwrap_or(body));
    }
    Ok(body)
}

fn extract_string_value(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    [
        "signed_token",
        "signedToken",
        "signature",
        "token",
        "access_token",
        "accessToken",
        "jwt",
        "value",
    ]
    .iter()
    .find_map(|key| value.get(*key).and_then(Value::as_str).map(str::to_string))
}

fn endpoint_url(
    endpoint: OAuthEndpoint,
    server_url: Option<&str>,
    service_id: Option<&str>,
    uri: &str,
) -> Result<reqwest::Url, OAuthClientError> {
    let Some(server_url) = server_url.filter(|value| !value.trim().is_empty()) else {
        return Err(OAuthClientError::MissingServerUrl {
            endpoint,
            service_id: service_id.map(str::to_string),
        });
    };
    let base = normalize_base_url(endpoint, server_url)?;
    let uri = uri.trim();
    let joined = if uri.starts_with("http://") || uri.starts_with("https://") {
        reqwest::Url::parse(uri)
    } else {
        base.join(uri.trim_start_matches('/'))
    };
    joined.map_err(|source| OAuthClientError::InvalidUrl {
        endpoint,
        url: format!("{server_url}{uri}"),
        source,
    })
}

fn endpoint_url_with_optional_segment(
    endpoint: OAuthEndpoint,
    server_url: Option<&str>,
    service_id: Option<&str>,
    uri: &str,
    placeholder: &str,
    segment: Option<&str>,
) -> Result<reqwest::Url, OAuthClientError> {
    if let Some(segment) = segment
        && uri.contains(placeholder)
    {
        return endpoint_url(
            endpoint,
            server_url,
            service_id,
            &uri.replace(placeholder, segment),
        );
    }

    let mut url = endpoint_url(endpoint, server_url, service_id, uri)?;
    if let Some(segment) = segment.filter(|value| !value.is_empty()) {
        let url_string = url.to_string();
        url.path_segments_mut()
            .map_err(|_| OAuthClientError::InvalidUrl {
                endpoint,
                url: url_string,
                source: url::ParseError::RelativeUrlWithCannotBeABaseBase,
            })?
            .push(segment);
    }
    Ok(url)
}

fn normalize_base_url(
    endpoint: OAuthEndpoint,
    server_url: &str,
) -> Result<reqwest::Url, OAuthClientError> {
    let mut base =
        reqwest::Url::parse(server_url.trim()).map_err(|source| OAuthClientError::InvalidUrl {
            endpoint,
            url: server_url.to_string(),
            source,
        })?;
    if !base.path().ends_with('/') {
        let path = format!("{}/", base.path());
        base.set_path(&path);
    }
    Ok(base)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ResolvedDerefProvider;

    #[test]
    fn deref_url_appends_encoded_token_segment() {
        let url = endpoint_url_with_optional_segment(
            OAuthEndpoint::Deref,
            Some("https://oauth.example"),
            None,
            "/oauth2/deref",
            "{token}",
            Some("abc/123"),
        )
        .expect("url");

        assert_eq!(url.as_str(), "https://oauth.example/oauth2/deref/abc%2F123");
    }

    #[test]
    fn endpoint_reports_service_id_without_server_url() {
        let provider = ResolvedDerefProvider {
            server_url: None,
            deref_service_id: Some("oauth-service".to_string()),
            uri: "/oauth2/deref".to_string(),
            client_id: String::new(),
            client_secret: String::new(),
            proxy_host: None,
            proxy_port: None,
            enable_http2: true,
        };

        let error = endpoint_url(
            OAuthEndpoint::Deref,
            provider.server_url.as_deref(),
            provider.deref_service_id.as_deref(),
            &provider.uri,
        )
        .expect_err("direct client needs server url");

        assert!(matches!(
            error,
            OAuthClientError::MissingServerUrl {
                endpoint: OAuthEndpoint::Deref,
                service_id: Some(_)
            }
        ));
    }

    #[test]
    fn token_extraction_accepts_common_response_fields() {
        assert_eq!(
            extract_string_value(r#"{"signedToken":"jwt-value"}"#).as_deref(),
            Some("jwt-value")
        );
        assert_eq!(
            extract_string_value(r#"{"access_token":"token-value"}"#).as_deref(),
            Some("token-value")
        );
    }
}
