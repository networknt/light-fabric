use crate::config::{ClientConfig, ClientRequestConfig, ClientTlsConfig, TlsVersion};
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
            Self::ClientIdentityIncomplete { .. } => None,
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
    let mut builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_millis(
            options
                .connect_timeout_ms
                .unwrap_or(request.connect_timeout),
        ))
        .timeout(Duration::from_millis(
            options.timeout_ms.unwrap_or(request.timeout),
        ));

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
}
