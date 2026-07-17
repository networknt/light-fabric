use crate::config::{ClientConfig, ClientRequestConfig, ClientTlsConfig, TlsVersion};
use std::fmt;
use std::io::{BufReader, Cursor};
use std::net::IpAddr;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Default)]
pub struct EndpointOptions {
    pub server_url: Option<String>,
    pub service_id: Option<String>,
    pub proxy_host: Option<String>,
    pub proxy_port: Option<u16>,
    pub enable_http2: Option<bool>,
    pub connect_timeout_ms: Option<u64>,
    pub timeout_ms: Option<u64>,
}

#[derive(Clone)]
pub struct ClientFactory {
    request: ClientRequestConfig,
    tls: ClientTlsConfig,
}

impl ClientFactory {
    pub fn new(config: Arc<ClientConfig>) -> Self {
        Self {
            request: config.request.clone(),
            tls: config.tls.clone(),
        }
    }

    pub fn from_config(config: &ClientConfig) -> Self {
        Self {
            request: config.request.clone(),
            tls: config.tls.clone(),
        }
    }

    pub fn from_parts(request: ClientRequestConfig, tls: ClientTlsConfig) -> Self {
        Self { request, tls }
    }

    pub fn reqwest_client(
        &self,
        options: EndpointOptions,
    ) -> Result<reqwest::Client, ClientBuildError> {
        build_reqwest_client(&self.request, &self.tls, options)
    }

    /// Builds a client whose authoritative connection-time DNS lookup rejects
    /// loopback, private, link-local, carrier-grade NAT, benchmark, metadata,
    /// unspecified, and multicast addresses. Callers must use a separately
    /// constructed ordinary client for explicitly approved internal targets.
    pub fn reqwest_client_public_dns_only(
        &self,
        options: EndpointOptions,
    ) -> Result<reqwest::Client, ClientBuildError> {
        build_reqwest_client_with_dns_policy(&self.request, &self.tls, options, true)
    }
}

#[derive(Debug)]
struct PublicDnsResolver;

impl reqwest::dns::Resolve for PublicDnsResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        Box::pin(async move {
            let addresses = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .collect::<Vec<_>>();
            if addresses.is_empty() {
                return Err(std::io::Error::other(format!(
                    "DNS lookup for `{host}` returned no addresses"
                ))
                .into());
            }
            if addresses
                .iter()
                .any(|address| is_blocked_public_target_ip(address.ip()))
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!("DNS lookup for `{host}` returned a non-public address"),
                )
                .into());
            }
            Ok(Box::new(addresses.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Shared public-target policy used by both preflight validation and the
/// connection-time resolver so DNS rebinding cannot bypass an earlier check.
pub fn is_blocked_public_target_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(address) => {
            let octets = address.octets();
            address.is_loopback()
                || address.is_private()
                || address.is_link_local()
                || address.is_unspecified()
                || octets == [169, 254, 169, 254]
                || octets[0] == 0
                || octets[0] >= 224
                || (octets[0] == 100 && (64..=127).contains(&octets[1]))
                || (octets[0] == 198 && matches!(octets[1], 18 | 19))
        }
        IpAddr::V6(address) => {
            let segment = address.segments()[0];
            address.is_loopback()
                || address.is_unspecified()
                || address.is_multicast()
                || (segment & 0xfe00) == 0xfc00
                || (segment & 0xffc0) == 0xfe80
        }
    }
}

#[derive(Debug)]
pub enum ClientBuildError {
    ClientIdentityIncomplete {
        cert_path: Option<PathBuf>,
        key_path: Option<PathBuf>,
    },
    ClientCertRead {
        path: PathBuf,
        source: std::io::Error,
    },
    ClientKeyRead {
        path: PathBuf,
        source: std::io::Error,
    },
    ClientIdentityParse {
        cert_path: PathBuf,
        key_path: PathBuf,
        source: reqwest::Error,
    },
    CaRead {
        path: PathBuf,
        source: std::io::Error,
    },
    CaParse {
        path: PathBuf,
        source: CaBundleError,
    },
    Proxy {
        proxy_url: String,
        source: reqwest::Error,
    },
    PublicDnsPolicyWithProxy,
    Build(reqwest::Error),
}

#[derive(Debug)]
pub enum CaBundleError {
    Empty,
    InvalidPem { source: std::io::Error },
    UnsupportedPemBlock { kind: String },
    InvalidCertificate { source: reqwest::Error },
}

impl fmt::Display for CaBundleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "CA bundle contains no certificates"),
            Self::InvalidPem { source } => write!(f, "invalid PEM block: {source}"),
            Self::UnsupportedPemBlock { kind } => {
                write!(f, "unsupported PEM block in CA bundle: {kind}")
            }
            Self::InvalidCertificate { source } => {
                write!(
                    f,
                    "failed to parse PEM-encoded CA certificate bundle: {source}"
                )
            }
        }
    }
}

impl std::error::Error for CaBundleError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Empty | Self::UnsupportedPemBlock { .. } => None,
            Self::InvalidPem { source } => Some(source),
            Self::InvalidCertificate { source } => Some(source),
        }
    }
}

impl fmt::Display for ClientBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ClientIdentityIncomplete {
                cert_path,
                key_path,
            } => {
                write!(
                    f,
                    "client TLS identity requires both clientCertPath and clientKeyPath; got cert={:?}, key={:?}",
                    cert_path, key_path
                )
            }
            Self::ClientCertRead { path, source } => {
                write!(
                    f,
                    "failed to read client TLS certificate `{}`: {source}",
                    path.display()
                )
            }
            Self::ClientKeyRead { path, source } => {
                write!(
                    f,
                    "failed to read client TLS key `{}`: {source}",
                    path.display()
                )
            }
            Self::ClientIdentityParse {
                cert_path,
                key_path,
                source,
            } => {
                write!(
                    f,
                    "invalid client TLS identity cert=`{}` key=`{}`: {source}",
                    cert_path.display(),
                    key_path.display()
                )
            }
            Self::CaRead { path, source } => {
                write!(
                    f,
                    "failed to read client CA certificate `{}`: {source}",
                    path.display()
                )
            }
            Self::CaParse { path, source } => {
                write!(
                    f,
                    "invalid client CA certificate bundle `{}`: {source}",
                    path.display()
                )
            }
            Self::Proxy { proxy_url, source } => {
                write!(f, "invalid client proxy `{proxy_url}`: {source}")
            }
            Self::PublicDnsPolicyWithProxy => write!(
                f,
                "public-target connection-time DNS policy cannot be combined with an HTTP proxy"
            ),
            Self::Build(source) => write!(f, "invalid HTTP client: {source}"),
        }
    }
}

impl std::error::Error for ClientBuildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ClientIdentityIncomplete { .. } | Self::PublicDnsPolicyWithProxy => None,
            Self::ClientCertRead { source, .. } => Some(source),
            Self::ClientKeyRead { source, .. } => Some(source),
            Self::ClientIdentityParse { source, .. } => Some(source),
            Self::CaRead { source, .. } => Some(source),
            Self::CaParse { source, .. } => Some(source),
            Self::Proxy { source, .. } => Some(source),
            Self::Build(source) => Some(source),
        }
    }
}

pub fn build_reqwest_client(
    request: &ClientRequestConfig,
    tls: &ClientTlsConfig,
    options: EndpointOptions,
) -> Result<reqwest::Client, ClientBuildError> {
    build_reqwest_client_with_dns_policy(request, tls, options, false)
}

fn build_reqwest_client_with_dns_policy(
    request: &ClientRequestConfig,
    tls: &ClientTlsConfig,
    options: EndpointOptions,
    public_dns_only: bool,
) -> Result<reqwest::Client, ClientBuildError> {
    if public_dns_only
        && options
            .proxy_host
            .as_deref()
            .is_some_and(|host| !host.trim().is_empty())
    {
        // An HTTP proxy resolves the origin independently, so this process
        // cannot enforce the public-only address policy at connect time.
        return Err(ClientBuildError::PublicDnsPolicyWithProxy);
    }

    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(
            options
                .connect_timeout_ms
                .unwrap_or(request.connect_timeout),
        ))
        .timeout(Duration::from_millis(
            options.timeout_ms.unwrap_or(request.timeout),
        ));

    if public_dns_only {
        builder = builder.dns_resolver(Arc::new(PublicDnsResolver));
    }

    if request.connection_expire_time == 0 {
        builder = builder.pool_idle_timeout(None);
    } else {
        builder = builder.pool_idle_timeout(Duration::from_millis(request.connection_expire_time));
    }
    if request.max_connection_num_per_host > 0 {
        builder = builder.pool_max_idle_per_host(request.max_connection_num_per_host as usize);
    }

    builder = configure_reqwest_tls(builder, tls)?;

    let enable_http2 = options.enable_http2.unwrap_or(request.enable_http2);
    if !enable_http2 {
        builder = builder.http1_only();
    }

    if let Some(proxy_host) = options
        .proxy_host
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let proxy_url = format!(
            "http://{}:{}",
            proxy_host,
            options.proxy_port.unwrap_or(443)
        );
        let proxy =
            reqwest::Proxy::all(proxy_url.as_str()).map_err(|source| ClientBuildError::Proxy {
                proxy_url: proxy_url.clone(),
                source,
            })?;
        builder = builder.proxy(proxy);
    }

    builder.build().map_err(ClientBuildError::Build)
}

pub fn load_ca_cert_bundle(path: &Path) -> Result<Vec<reqwest::Certificate>, ClientBuildError> {
    let pem = std::fs::read(path).map_err(|source| ClientBuildError::CaRead {
        path: path.to_path_buf(),
        source,
    })?;
    parse_ca_cert_bundle(&pem).map_err(|source| ClientBuildError::CaParse {
        path: path.to_path_buf(),
        source,
    })
}

pub fn parse_ca_cert_bundle(pem: &[u8]) -> Result<Vec<reqwest::Certificate>, CaBundleError> {
    validate_ca_cert_bundle_pem(pem)?;
    let certificates = reqwest::Certificate::from_pem_bundle(pem)
        .map_err(|source| CaBundleError::InvalidCertificate { source })?;
    if certificates.is_empty() {
        return Err(CaBundleError::Empty);
    }
    Ok(certificates)
}

fn validate_ca_cert_bundle_pem(pem: &[u8]) -> Result<(), CaBundleError> {
    validate_ca_cert_bundle_pem_labels(pem)?;

    let mut reader = BufReader::new(Cursor::new(pem));
    let mut certificate_count = 0usize;

    loop {
        let Some(item) = rustls_pemfile::read_one(&mut reader)
            .map_err(|source| CaBundleError::InvalidPem { source })?
        else {
            break;
        };

        match item {
            rustls_pemfile::Item::X509Certificate(_) => certificate_count += 1,
            other => {
                return Err(CaBundleError::UnsupportedPemBlock {
                    kind: format!("{other:?}"),
                });
            }
        }
    }

    if certificate_count == 0 {
        return Err(CaBundleError::Empty);
    }

    Ok(())
}

fn validate_ca_cert_bundle_pem_labels(pem: &[u8]) -> Result<(), CaBundleError> {
    let text = std::str::from_utf8(pem).map_err(|source| CaBundleError::InvalidPem {
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, source),
    })?;

    for line in text.lines().map(str::trim) {
        let Some(label) = line
            .strip_prefix("-----BEGIN ")
            .and_then(|value| value.strip_suffix("-----"))
        else {
            continue;
        };
        if label != "CERTIFICATE" {
            return Err(CaBundleError::UnsupportedPemBlock {
                kind: label.to_string(),
            });
        }
    }

    Ok(())
}

fn configure_reqwest_tls(
    mut builder: reqwest::ClientBuilder,
    tls: &ClientTlsConfig,
) -> Result<reqwest::ClientBuilder, ClientBuildError> {
    if let Some(path) = tls
        .ca_cert_path
        .as_ref()
        .filter(|path| !path.as_os_str().is_empty())
    {
        let certificates = load_ca_cert_bundle(path)?;
        let certificate_count = certificates.len();
        for certificate in certificates {
            builder = builder.add_root_certificate(certificate);
        }
        tracing::info!(
            ca_cert_path = %path.display(),
            ca_cert_count = certificate_count,
            "loaded client CA certificate bundle"
        );
    }

    if !tls.verify_hostname {
        builder = builder.danger_accept_invalid_hostnames(true);
    }

    if let Some(tls_version) = tls.tls_version {
        builder = builder.min_tls_version(match tls_version {
            TlsVersion::TlsV1_2 => reqwest::tls::Version::TLS_1_2,
            TlsVersion::TlsV1_3 => reqwest::tls::Version::TLS_1_3,
        });
    }

    let client_cert_path = tls
        .client_cert_path
        .as_ref()
        .filter(|path| !path.as_os_str().is_empty());
    let client_key_path = tls
        .client_key_path
        .as_ref()
        .filter(|path| !path.as_os_str().is_empty());
    match (client_cert_path, client_key_path) {
        (Some(cert_path), Some(key_path)) => {
            let mut identity_pem =
                std::fs::read(cert_path).map_err(|source| ClientBuildError::ClientCertRead {
                    path: cert_path.clone(),
                    source,
                })?;
            if !identity_pem.ends_with(b"\n") {
                identity_pem.push(b'\n');
            }
            let key_pem =
                std::fs::read(key_path).map_err(|source| ClientBuildError::ClientKeyRead {
                    path: key_path.clone(),
                    source,
                })?;
            identity_pem.extend_from_slice(&key_pem);
            let identity = reqwest::Identity::from_pem(&identity_pem).map_err(|source| {
                ClientBuildError::ClientIdentityParse {
                    cert_path: cert_path.clone(),
                    key_path: key_path.clone(),
                    source,
                }
            })?;
            builder = builder.identity(identity);
        }
        (Some(cert_path), None) => {
            return Err(ClientBuildError::ClientIdentityIncomplete {
                cert_path: Some(cert_path.clone()),
                key_path: None,
            });
        }
        (None, Some(key_path)) => {
            return Err(ClientBuildError::ClientIdentityIncomplete {
                cert_path: None,
                key_path: Some(key_path.clone()),
            });
        }
        (None, None) => {}
    }

    Ok(builder)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_CA_PEM: &[u8] = include_bytes!("../../../apps/light-gateway/config/ca.pem");

    #[test]
    fn parse_ca_cert_bundle_accepts_single_certificate() {
        let certificates = parse_ca_cert_bundle(TEST_CA_PEM).expect("single certificate bundle");

        assert_eq!(certificates.len(), 1);
    }

    #[test]
    fn parse_ca_cert_bundle_accepts_multiple_certificates() {
        let mut bundle = Vec::from(TEST_CA_PEM);
        bundle.extend_from_slice(TEST_CA_PEM);

        let certificates = parse_ca_cert_bundle(&bundle).expect("multi certificate bundle");

        assert_eq!(certificates.len(), 2);
    }

    #[test]
    fn parse_ca_cert_bundle_rejects_empty_bundle() {
        let error = parse_ca_cert_bundle(b"# comment only\n")
            .expect_err("empty CA bundle should fail")
            .to_string();

        assert!(error.contains("contains no certificates"));
    }

    #[test]
    fn parse_ca_cert_bundle_rejects_non_certificate_pem_blocks() {
        let mut bundle = Vec::from(TEST_CA_PEM);
        bundle.extend_from_slice(
            b"-----BEGIN PRIVATE KEY-----\nnot-a-valid-key\n-----END PRIVATE KEY-----\n",
        );

        let error = parse_ca_cert_bundle(&bundle)
            .expect_err("private key in CA bundle should fail")
            .to_string();

        assert!(error.contains("unsupported PEM block"));
    }

    #[test]
    fn mtls_requires_cert_and_key() {
        let tls = ClientTlsConfig {
            client_cert_path: Some(PathBuf::from("client.pem")),
            ..ClientTlsConfig::default()
        };

        let error = build_reqwest_client(
            &ClientRequestConfig::default(),
            &tls,
            EndpointOptions::default(),
        )
        .expect_err("client identity should require both files");

        assert!(matches!(
            error,
            ClientBuildError::ClientIdentityIncomplete { .. }
        ));
    }

    #[tokio::test]
    async fn public_dns_policy_rejects_non_public_connection_addresses() {
        for address in [
            "127.0.0.1",
            "10.0.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "198.18.0.1",
            "::1",
            "fc00::1",
            "fe80::1",
        ] {
            assert!(is_blocked_public_target_ip(
                address.parse().expect("IP address")
            ));
        }
        assert!(!is_blocked_public_target_ip(
            "8.8.8.8".parse().expect("public IP")
        ));

        let result = reqwest::dns::Resolve::resolve(
            &PublicDnsResolver,
            "localhost".parse().expect("DNS name"),
        )
        .await;
        let error = match result {
            Ok(_) => panic!("localhost must be rejected at connection-time resolution"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("non-public address"));

        let factory = ClientFactory::from_config(&ClientConfig::default());
        let result = factory.reqwest_client_public_dns_only(EndpointOptions {
            proxy_host: Some("proxy.example".to_string()),
            ..EndpointOptions::default()
        });
        assert!(matches!(
            result,
            Err(ClientBuildError::PublicDnsPolicyWithProxy)
        ));
    }
}
