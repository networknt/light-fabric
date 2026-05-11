use crate::config::{ClientConfig, ClientRequestConfig, ClientTlsConfig};
use std::fmt;
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
}

#[derive(Debug)]
pub enum ClientBuildError {
    CaRead {
        path: PathBuf,
        source: std::io::Error,
    },
    CaParse {
        path: PathBuf,
        source: reqwest::Error,
    },
    Proxy {
        proxy_url: String,
        source: reqwest::Error,
    },
    Build(reqwest::Error),
}

impl fmt::Display for ClientBuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
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
                    "invalid client CA certificate `{}`: {source}",
                    path.display()
                )
            }
            Self::Proxy { proxy_url, source } => {
                write!(f, "invalid client proxy `{proxy_url}`: {source}")
            }
            Self::Build(source) => write!(f, "invalid HTTP client: {source}"),
        }
    }
}

impl std::error::Error for ClientBuildError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
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
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(
            options
                .connect_timeout_ms
                .unwrap_or(request.connect_timeout),
        ))
        .timeout(Duration::from_millis(
            options.timeout_ms.unwrap_or(request.timeout),
        ));

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

fn configure_reqwest_tls(
    mut builder: reqwest::ClientBuilder,
    tls: &ClientTlsConfig,
) -> Result<reqwest::ClientBuilder, ClientBuildError> {
    if let Some(path) = tls
        .ca_cert_path
        .as_ref()
        .filter(|path| !path.as_os_str().is_empty())
    {
        let pem = std::fs::read(path).map_err(|source| ClientBuildError::CaRead {
            path: path.clone(),
            source,
        })?;
        let certificates = reqwest::Certificate::from_pem_bundle(&pem).map_err(|source| {
            ClientBuildError::CaParse {
                path: path.clone(),
                source,
            }
        })?;
        for certificate in certificates {
            builder = builder.add_root_certificate(certificate);
        }
    }

    if !tls.verify_hostname {
        builder = builder.danger_accept_invalid_hostnames(true);
    }

    Ok(builder)
}
