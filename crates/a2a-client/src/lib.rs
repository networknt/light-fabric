use http::HeaderMap;
use reqwest::{Client, Response, redirect::Policy};
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;
use thiserror::Error;
use tokio::net::lookup_host;
use url::{Host, Url};

const FORWARDED_HEADERS: [&str; 3] = ["a2a-version", "a2a-extensions", "accept"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedEndpoint {
    url: Url,
}

impl ValidatedEndpoint {
    pub fn parse(value: &str) -> Result<Self, ClientError> {
        let url = Url::parse(value).map_err(|_| ClientError::InvalidEndpoint)?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(ClientError::InvalidEndpoint);
        }
        let host = url.host_str().ok_or(ClientError::InvalidEndpoint)?;
        if host.eq_ignore_ascii_case("localhost") || host.ends_with(".localhost") {
            return Err(ClientError::PrivateDestination);
        }
        match url.host() {
            Some(Host::Ipv4(ip)) if !public_ip(IpAddr::V4(ip)) => {
                return Err(ClientError::PrivateDestination);
            }
            Some(Host::Ipv6(ip)) if !public_ip(IpAddr::V6(ip)) => {
                return Err(ClientError::PrivateDestination);
            }
            _ => {}
        }
        Ok(Self { url })
    }

    pub fn as_str(&self) -> &str {
        self.url.as_str()
    }

    async fn resolve_public(&self) -> Result<Vec<SocketAddr>, ClientError> {
        let host = self.url.host_str().ok_or(ClientError::InvalidEndpoint)?;
        let port = self
            .url
            .port_or_known_default()
            .ok_or(ClientError::InvalidEndpoint)?;
        let addresses = lookup_host((host, port))
            .await
            .map_err(ClientError::Resolution)?
            .collect::<Vec<_>>();
        if addresses.is_empty() || addresses.iter().any(|address| !public_ip(address.ip())) {
            return Err(ClientError::PrivateDestination);
        }
        Ok(addresses)
    }
}

#[derive(Debug, Clone)]
pub struct A2aClient {
    timeout: Duration,
}

impl A2aClient {
    pub fn new(timeout: Duration) -> Result<Self, ClientError> {
        if timeout.is_zero() {
            return Err(ClientError::InvalidTimeout);
        }
        Ok(Self { timeout })
    }

    pub async fn post(
        &self,
        endpoint: &ValidatedEndpoint,
        headers: &HeaderMap,
        body: Vec<u8>,
        bearer_token: Option<&str>,
    ) -> Result<Response, ClientError> {
        let host = endpoint
            .url
            .host_str()
            .ok_or(ClientError::InvalidEndpoint)?;
        let addresses = endpoint.resolve_public().await?;
        let client = Client::builder()
            .redirect(Policy::none())
            .timeout(self.timeout)
            .resolve_to_addrs(host, &addresses)
            .build()
            .map_err(ClientError::Build)?;
        let mut request = client
            .post(endpoint.as_str())
            .header("content-type", "application/json")
            .body(body);
        if let Some(token) = bearer_token {
            if token.trim().is_empty() || token.contains(['\r', '\n']) {
                return Err(ClientError::InvalidCredential);
            }
            request = request.bearer_auth(token);
        }
        for name in FORWARDED_HEADERS {
            if let Some(value) = headers.get(name) {
                request = request.header(name, value);
            }
        }
        request.send().await.map_err(ClientError::Request)
    }

    pub async fn post_signed_callback(
        &self,
        endpoint: &ValidatedEndpoint,
        body: Vec<u8>,
        delivery_id: &str,
        delivery_nonce: &str,
        timestamp: &str,
        signature: &str,
    ) -> Result<Response, ClientError> {
        for value in [delivery_id, delivery_nonce, timestamp, signature] {
            if value.trim().is_empty() || value.contains(['\r', '\n']) {
                return Err(ClientError::InvalidCredential);
            }
        }
        let host = endpoint
            .url
            .host_str()
            .ok_or(ClientError::InvalidEndpoint)?;
        let addresses = endpoint.resolve_public().await?;
        let client = Client::builder()
            .redirect(Policy::none())
            .timeout(self.timeout)
            .resolve_to_addrs(host, &addresses)
            .build()
            .map_err(ClientError::Build)?;
        client
            .post(endpoint.as_str())
            .header("content-type", "application/a2a+json")
            .header("x-light-a2a-delivery-id", delivery_id)
            .header("x-light-a2a-delivery-nonce", delivery_nonce)
            .header("x-light-a2a-delivery-timestamp", timestamp)
            .header("x-light-a2a-signature", signature)
            .body(body)
            .send()
            .await
            .map_err(ClientError::Request)
    }
}

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let octets = ip.octets();
            let shared = octets[0] == 100 && (64..=127).contains(&octets[1]);
            let protocol_assignment = octets[0] == 192 && octets[1] == 0 && octets[2] == 0;
            let benchmarking = octets[0] == 198 && matches!(octets[1], 18 | 19);
            !(ip.is_private()
                || ip.is_loopback()
                || ip.is_link_local()
                || ip.is_broadcast()
                || ip.is_documentation()
                || ip.is_unspecified()
                || octets[0] == 0
                || octets[0] >= 224
                || shared
                || protocol_assignment
                || benchmarking)
        }
        IpAddr::V6(ip) => {
            !(ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_unique_local()
                || ip.is_unicast_link_local()
                || ip
                    .to_ipv4_mapped()
                    .is_some_and(|mapped| !public_ip(IpAddr::V4(mapped))))
        }
    }
}

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("invalid A2A endpoint")]
    InvalidEndpoint,
    #[error("A2A endpoint resolves to a private or reserved destination")]
    PrivateDestination,
    #[error("A2A request timeout must be positive")]
    InvalidTimeout,
    #[error("invalid server-owned A2A credential")]
    InvalidCredential,
    #[error("resolve A2A endpoint: {0}")]
    Resolution(#[source] std::io::Error),
    #[error("build A2A client: {0}")]
    Build(#[source] reqwest::Error),
    #[error("send A2A request: {0}")]
    Request(#[source] reqwest::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_contract_rejects_unsafe_destinations() {
        for value in [
            "http://agents.example/a2a",
            "https://127.0.0.1/a2a",
            "https://[::1]/a2a",
            "https://user:secret@agents.example/a2a",
            "https://agents.example/a2a?target=other",
        ] {
            assert!(ValidatedEndpoint::parse(value).is_err(), "{value}");
        }
        assert!(ValidatedEndpoint::parse("https://agents.example/a2a").is_ok());
    }

    #[test]
    fn resolver_rejects_shared_protocol_and_benchmarking_ranges() {
        for ip in [
            "100.64.0.1",
            "100.127.255.254",
            "192.0.0.9",
            "198.18.0.1",
            "198.19.255.254",
        ] {
            assert!(!public_ip(ip.parse().unwrap()), "{ip}");
        }
        assert!(public_ip("8.8.8.8".parse().unwrap()));
    }
}
