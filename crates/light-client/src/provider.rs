use crate::config::{
    AuthServerConfig, ClientConfig, OAuthClientCredentialsConfig, OAuthKeyConfig, OAuthTokenConfig,
};
use std::collections::BTreeMap;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthProviderSection {
    ClientCredentials,
    Key,
    Sign,
    SignKey,
    Deref,
}

impl OAuthProviderSection {
    pub fn config_path(self) -> &'static str {
        match self {
            Self::ClientCredentials => "oauth.token.client_credentials.serviceIdAuthServers",
            Self::Key => "oauth.token.key.serviceIdAuthServers",
            Self::Sign => "oauth.sign",
            Self::SignKey => "oauth.sign.key",
            Self::Deref => "oauth.deref",
        }
    }
}

impl fmt::Display for OAuthProviderSection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.config_path())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OAuthProviderError {
    MissingServiceId {
        section: OAuthProviderSection,
    },
    MissingServiceProvider {
        section: OAuthProviderSection,
        service_id: String,
    },
    MissingProvider {
        section: OAuthProviderSection,
    },
}

impl fmt::Display for OAuthProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingServiceId { section } => {
                write!(f, "client.yml {section} requires a service id")
            }
            Self::MissingServiceProvider {
                section,
                service_id,
            } => {
                write!(f, "client.yml {section} is missing `{service_id}`")
            }
            Self::MissingProvider { section } => {
                write!(f, "client.yml {section} has no configured providers")
            }
        }
    }
}

impl std::error::Error for OAuthProviderError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedClientCredentialsProvider {
    pub server_url: Option<String>,
    pub token_service_id: Option<String>,
    pub uri: String,
    pub client_id: String,
    pub client_secret: String,
    pub scope: Vec<String>,
    pub proxy_host: Option<String>,
    pub proxy_port: Option<u16>,
    pub enable_http2: bool,
    pub token_renew_before_expired: u64,
    pub expired_refresh_retry_delay: u64,
    pub early_refresh_retry_delay: u64,
    pub cache_service_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedKeyProvider {
    pub server_url: Option<String>,
    pub key_service_id: Option<String>,
    pub uri: String,
    pub client_id: String,
    pub client_secret: String,
    pub proxy_host: Option<String>,
    pub proxy_port: Option<u16>,
    pub enable_http2: bool,
    pub audience: Option<String>,
    pub cache_service_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSignProvider {
    pub server_url: Option<String>,
    pub sign_service_id: Option<String>,
    pub uri: String,
    pub timeout: u64,
    pub client_id: String,
    pub client_secret: String,
    pub proxy_host: Option<String>,
    pub proxy_port: Option<u16>,
    pub enable_http2: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSignKeyProvider {
    pub server_url: Option<String>,
    pub key_service_id: Option<String>,
    pub uri: String,
    pub client_id: String,
    pub client_secret: String,
    pub enable_http2: bool,
    pub audience: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedDerefProvider {
    pub server_url: Option<String>,
    pub deref_service_id: Option<String>,
    pub uri: String,
    pub client_id: String,
    pub client_secret: String,
    pub proxy_host: Option<String>,
    pub proxy_port: Option<u16>,
    pub enable_http2: bool,
}

pub struct OAuthProviderResolver<'a> {
    client: &'a ClientConfig,
}

impl<'a> OAuthProviderResolver<'a> {
    pub fn new(client: &'a ClientConfig) -> Self {
        Self { client }
    }

    pub fn service_for_path(&self, path: &str) -> Option<&'a str> {
        best_path_prefix(&self.client.path_prefix_services, path).map(String::as_str)
    }

    pub fn service_id_for_request(
        &self,
        explicit_service_id: Option<&str>,
        request_path: &str,
    ) -> Option<String> {
        explicit_service_id
            .and_then(non_empty_str)
            .map(str::to_string)
            .or_else(|| self.service_for_path(request_path).map(str::to_string))
    }

    pub fn client_credentials_provider(
        &self,
        service_id: Option<&str>,
    ) -> Result<ResolvedClientCredentialsProvider, OAuthProviderError> {
        let token = &self.client.oauth.token;
        let base = &token.client_credentials;
        let service_id = service_id.and_then(non_empty_str);

        if self.client_credentials_multi_provider() {
            let service_id = service_id.ok_or(OAuthProviderError::MissingServiceId {
                section: OAuthProviderSection::ClientCredentials,
            })?;
            let auth_server = base
                .service_id_auth_servers
                .get(service_id)
                .ok_or_else(|| OAuthProviderError::MissingServiceProvider {
                    section: OAuthProviderSection::ClientCredentials,
                    service_id: service_id.to_string(),
                })?;
            return Ok(merge_client_credentials_provider(
                token,
                base,
                Some(auth_server),
                Some(service_id.to_string()),
            ));
        }

        Ok(merge_client_credentials_provider(
            token,
            base,
            None,
            service_id
                .map(str::to_string)
                .or_else(|| non_empty_option(token.service_id.clone())),
        ))
    }

    pub fn key_provider(
        &self,
        service_id: Option<&str>,
    ) -> Result<ResolvedKeyProvider, OAuthProviderError> {
        let key = &self.client.oauth.token.key;
        let service_id = service_id.and_then(non_empty_str);

        if self.key_multi_provider() {
            let service_id = service_id.ok_or(OAuthProviderError::MissingServiceId {
                section: OAuthProviderSection::Key,
            })?;
            let auth_server = key.service_id_auth_servers.get(service_id).ok_or_else(|| {
                OAuthProviderError::MissingServiceProvider {
                    section: OAuthProviderSection::Key,
                    service_id: service_id.to_string(),
                }
            })?;
            return Ok(merge_key_provider(
                key,
                Some(auth_server),
                Some(service_id.to_string()),
            ));
        }

        if !key_provider_is_configured(key, None) {
            return Err(OAuthProviderError::MissingProvider {
                section: OAuthProviderSection::Key,
            });
        }

        Ok(merge_key_provider(key, None, None))
    }

    pub fn key_providers(&self) -> Result<Vec<ResolvedKeyProvider>, OAuthProviderError> {
        let key = &self.client.oauth.token.key;
        if self.key_multi_provider() {
            if key.service_id_auth_servers.is_empty() {
                return Err(OAuthProviderError::MissingProvider {
                    section: OAuthProviderSection::Key,
                });
            }
            return Ok(key
                .service_id_auth_servers
                .iter()
                .map(|(service_id, auth_server)| {
                    merge_key_provider(key, Some(auth_server), Some(service_id.clone()))
                })
                .collect());
        }

        if !key_provider_is_configured(key, None) {
            return Ok(Vec::new());
        }
        Ok(vec![merge_key_provider(key, None, None)])
    }

    pub fn has_key_providers(&self) -> bool {
        let key = &self.client.oauth.token.key;
        !key.service_id_auth_servers.is_empty() || key_provider_is_configured(key, None)
    }

    pub fn sign_provider(&self) -> Result<ResolvedSignProvider, OAuthProviderError> {
        let sign = &self.client.oauth.sign;
        if non_empty_str_option(sign.server_url.as_deref())
            .or_else(|| non_empty_str_option(sign.service_id.as_deref()))
            .is_none()
        {
            return Err(OAuthProviderError::MissingProvider {
                section: OAuthProviderSection::Sign,
            });
        }

        Ok(ResolvedSignProvider {
            server_url: non_empty_option(sign.server_url.clone()),
            sign_service_id: non_empty_option(sign.service_id.clone()),
            uri: sign.uri.clone(),
            timeout: sign.timeout,
            client_id: sign.client_id.clone(),
            client_secret: sign.client_secret.clone(),
            proxy_host: non_empty_option(sign.proxy_host.clone()),
            proxy_port: sign.proxy_port,
            enable_http2: sign.enable_http2,
        })
    }

    pub fn sign_key_provider(&self) -> Result<ResolvedSignKeyProvider, OAuthProviderError> {
        let sign = &self.client.oauth.sign;
        let key = &sign.key;
        let server_url = non_empty_option(key.server_url.clone())
            .or_else(|| non_empty_option(sign.server_url.clone()));
        let key_service_id = non_empty_option(key.service_id.clone())
            .or_else(|| non_empty_option(sign.service_id.clone()));
        if server_url.is_none() && key_service_id.is_none() {
            return Err(OAuthProviderError::MissingProvider {
                section: OAuthProviderSection::SignKey,
            });
        }

        Ok(ResolvedSignKeyProvider {
            server_url,
            key_service_id,
            uri: key.uri.clone(),
            client_id: non_empty_string(key.client_id.clone())
                .unwrap_or_else(|| sign.client_id.clone()),
            client_secret: non_empty_string(key.client_secret.clone())
                .unwrap_or_else(|| sign.client_secret.clone()),
            enable_http2: key.enable_http2,
            audience: non_empty_option(key.audience.clone()),
        })
    }

    pub fn deref_provider(&self) -> Result<ResolvedDerefProvider, OAuthProviderError> {
        let deref = &self.client.oauth.deref;
        if non_empty_str_option(deref.server_url.as_deref())
            .or_else(|| non_empty_str_option(deref.service_id.as_deref()))
            .is_none()
        {
            return Err(OAuthProviderError::MissingProvider {
                section: OAuthProviderSection::Deref,
            });
        }

        Ok(ResolvedDerefProvider {
            server_url: non_empty_option(deref.server_url.clone()),
            deref_service_id: non_empty_option(deref.service_id.clone()),
            uri: deref.uri.clone(),
            client_id: deref.client_id.clone(),
            client_secret: deref.client_secret.clone(),
            proxy_host: non_empty_option(deref.proxy_host.clone()),
            proxy_port: deref.proxy_port,
            enable_http2: deref.enable_http2,
        })
    }

    pub fn client_credentials_multi_provider(&self) -> bool {
        self.client.oauth.multiple_auth_servers
            || !self
                .client
                .oauth
                .token
                .client_credentials
                .service_id_auth_servers
                .is_empty()
    }

    pub fn key_multi_provider(&self) -> bool {
        self.client.oauth.multiple_auth_servers
            || !self
                .client
                .oauth
                .token
                .key
                .service_id_auth_servers
                .is_empty()
    }
}

fn merge_client_credentials_provider(
    token: &OAuthTokenConfig,
    base: &OAuthClientCredentialsConfig,
    auth_server: Option<&AuthServerConfig>,
    cache_service_id: Option<String>,
) -> ResolvedClientCredentialsProvider {
    ResolvedClientCredentialsProvider {
        server_url: auth_server
            .and_then(|auth| non_empty_option(auth.server_url.clone()))
            .or_else(|| non_empty_option(token.server_url.clone())),
        token_service_id: auth_server
            .and_then(|auth| non_empty_option(auth.service_id.clone()))
            .or_else(|| non_empty_option(token.service_id.clone())),
        uri: auth_server
            .and_then(|auth| non_empty_option(auth.uri.clone()))
            .unwrap_or_else(|| base.uri.clone()),
        client_id: auth_server
            .and_then(|auth| non_empty_option(auth.client_id.clone()))
            .unwrap_or_else(|| base.client_id.clone()),
        client_secret: auth_server
            .and_then(|auth| non_empty_option(auth.client_secret.clone()))
            .unwrap_or_else(|| base.client_secret.clone()),
        scope: auth_server
            .filter(|auth| !auth.scope.is_empty())
            .map(|auth| auth.scope.clone())
            .unwrap_or_else(|| base.scope.clone()),
        proxy_host: auth_server
            .and_then(|auth| non_empty_option(auth.proxy_host.clone()))
            .or_else(|| non_empty_option(token.proxy_host.clone())),
        proxy_port: auth_server
            .and_then(|auth| auth.proxy_port)
            .or(token.proxy_port),
        enable_http2: auth_server
            .and_then(|auth| auth.enable_http2)
            .unwrap_or(token.enable_http2),
        token_renew_before_expired: auth_server
            .and_then(|auth| auth.token_renew_before_expired)
            .unwrap_or(token.token_renew_before_expired),
        expired_refresh_retry_delay: auth_server
            .and_then(|auth| auth.expired_refresh_retry_delay)
            .unwrap_or(token.expired_refresh_retry_delay),
        early_refresh_retry_delay: auth_server
            .and_then(|auth| auth.early_refresh_retry_delay)
            .unwrap_or(token.early_refresh_retry_delay),
        cache_service_id: cache_service_id.and_then(non_empty_string),
    }
}

fn merge_key_provider(
    key: &OAuthKeyConfig,
    auth_server: Option<&AuthServerConfig>,
    cache_service_id: Option<String>,
) -> ResolvedKeyProvider {
    ResolvedKeyProvider {
        server_url: auth_server
            .and_then(|auth| non_empty_option(auth.server_url.clone()))
            .or_else(|| non_empty_option(key.server_url.clone())),
        key_service_id: auth_server
            .and_then(|auth| non_empty_option(auth.service_id.clone()))
            .or_else(|| non_empty_option(key.service_id.clone())),
        uri: auth_server
            .and_then(|auth| non_empty_option(auth.uri.clone()))
            .unwrap_or_else(|| key.uri.clone()),
        client_id: auth_server
            .and_then(|auth| non_empty_option(auth.client_id.clone()))
            .unwrap_or_else(|| key.client_id.clone()),
        client_secret: auth_server
            .and_then(|auth| non_empty_option(auth.client_secret.clone()))
            .unwrap_or_else(|| key.client_secret.clone()),
        proxy_host: auth_server.and_then(|auth| non_empty_option(auth.proxy_host.clone())),
        proxy_port: auth_server.and_then(|auth| auth.proxy_port),
        enable_http2: auth_server
            .and_then(|auth| auth.enable_http2)
            .unwrap_or(key.enable_http2),
        audience: auth_server
            .and_then(|auth| non_empty_option(auth.audience.clone()))
            .or_else(|| non_empty_option(key.audience.clone())),
        cache_service_id: cache_service_id.and_then(non_empty_string),
    }
}

fn key_provider_is_configured(
    key: &OAuthKeyConfig,
    auth_server: Option<&AuthServerConfig>,
) -> bool {
    auth_server
        .and_then(|auth| non_empty_str_option(auth.server_url.as_deref()))
        .or_else(|| auth_server.and_then(|auth| non_empty_str_option(auth.service_id.as_deref())))
        .or_else(|| non_empty_str_option(key.server_url.as_deref()))
        .or_else(|| non_empty_str_option(key.service_id.as_deref()))
        .is_some()
}

fn best_path_prefix<'a, T>(mapping: &'a BTreeMap<String, T>, request_path: &str) -> Option<&'a T> {
    let request_path = normalize_path(request_path);
    mapping
        .iter()
        .filter_map(|(prefix, value)| {
            let prefix = normalize_prefix(prefix);
            path_matches_prefix(request_path.as_str(), prefix.as_str())
                .then_some((prefix.len(), value))
        })
        .max_by_key(|(len, _)| *len)
        .map(|(_, value)| value)
}

fn normalize_path(path: &str) -> String {
    let path = path.trim();
    if path.is_empty() {
        "/".to_string()
    } else if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    }
}

fn normalize_prefix(prefix: &str) -> String {
    let prefix = prefix.trim();
    if prefix.is_empty() || prefix == "/" {
        return "/".to_string();
    }
    let prefix = prefix.trim_end_matches('/');
    if prefix.starts_with('/') {
        prefix.to_string()
    } else {
        format!("/{prefix}")
    }
}

fn path_matches_prefix(path: &str, prefix: &str) -> bool {
    prefix == "/"
        || path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn non_empty_string(value: String) -> Option<String> {
    (!value.trim().is_empty()).then_some(value)
}

fn non_empty_option(value: Option<String>) -> Option<String> {
    value.and_then(non_empty_string)
}

fn non_empty_str(value: &str) -> Option<&str> {
    (!value.trim().is_empty()).then_some(value)
}

fn non_empty_str_option(value: Option<&str>) -> Option<&str> {
    value.and_then(non_empty_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ClientOauthConfig, OAuthTokenConfig};

    #[test]
    fn service_for_path_uses_longest_boundary_prefix() {
        let mut client = ClientConfig::default();
        client
            .path_prefix_services
            .insert("/api".to_string(), "api".to_string());
        client
            .path_prefix_services
            .insert("/api/orders".to_string(), "orders".to_string());
        let resolver = OAuthProviderResolver::new(&client);

        assert_eq!(resolver.service_for_path("/api"), Some("api"));
        assert_eq!(resolver.service_for_path("/api/orders/1"), Some("orders"));
        assert_eq!(resolver.service_for_path("/api-v2"), None);
        assert_eq!(
            resolver.service_id_for_request(Some("header-service"), "/api/orders/1"),
            Some("header-service".to_string())
        );
    }

    #[test]
    fn client_credentials_provider_inherits_global_defaults() {
        let mut client = ClientConfig {
            oauth: ClientOauthConfig {
                multiple_auth_servers: true,
                token: OAuthTokenConfig {
                    server_url: Some("https://global-oauth".to_string()),
                    token_renew_before_expired: 10,
                    client_credentials: OAuthClientCredentialsConfig {
                        uri: "/global-token".to_string(),
                        client_id: "global-client".to_string(),
                        client_secret: "global-secret".to_string(),
                        scope: vec!["global.r".to_string()],
                        ..OAuthClientCredentialsConfig::default()
                    },
                    ..OAuthTokenConfig::default()
                },
                ..ClientOauthConfig::default()
            },
            ..ClientConfig::default()
        };
        client
            .oauth
            .token
            .client_credentials
            .service_id_auth_servers
            .insert(
                "petstore".to_string(),
                AuthServerConfig {
                    client_id: Some("pet-client".to_string()),
                    scope: vec!["pet.r".to_string()],
                    ..AuthServerConfig::default()
                },
            );

        let provider = OAuthProviderResolver::new(&client)
            .client_credentials_provider(Some("petstore"))
            .expect("provider");

        assert_eq!(provider.server_url.as_deref(), Some("https://global-oauth"));
        assert_eq!(provider.uri, "/global-token");
        assert_eq!(provider.client_id, "pet-client");
        assert_eq!(provider.client_secret, "global-secret");
        assert_eq!(provider.scope, vec!["pet.r"]);
        assert_eq!(provider.token_renew_before_expired, 10);
        assert_eq!(provider.cache_service_id.as_deref(), Some("petstore"));
    }

    #[test]
    fn key_provider_inherits_global_defaults_and_audience() {
        let mut client = ClientConfig::default();
        client.oauth.multiple_auth_servers = true;
        client.oauth.token.key.server_url = Some("https://global-key".to_string());
        client.oauth.token.key.uri = "/oauth2/key".to_string();
        client.oauth.token.key.audience = Some("global-audience".to_string());
        client.oauth.token.key.service_id_auth_servers.insert(
            "petstore".to_string(),
            AuthServerConfig {
                server_url: Some("https://pet-key".to_string()),
                audience: Some("pet-audience".to_string()),
                ..AuthServerConfig::default()
            },
        );

        let provider = OAuthProviderResolver::new(&client)
            .key_provider(Some("petstore"))
            .expect("provider");

        assert_eq!(provider.server_url.as_deref(), Some("https://pet-key"));
        assert_eq!(provider.uri, "/oauth2/key");
        assert_eq!(provider.audience.as_deref(), Some("pet-audience"));
        assert_eq!(provider.cache_service_id.as_deref(), Some("petstore"));
    }

    #[test]
    fn sign_and_sign_key_providers_inherit_sign_defaults() {
        let mut client = ClientConfig::default();
        client.oauth.sign.server_url = Some("https://oauth".to_string());
        client.oauth.sign.service_id = Some("oauth-service".to_string());
        client.oauth.sign.uri = "/oauth2/signing".to_string();
        client.oauth.sign.timeout = 1500;
        client.oauth.sign.client_id = "sign-client".to_string();
        client.oauth.sign.client_secret = "sign-secret".to_string();
        client.oauth.sign.key.uri = "/oauth2/key".to_string();
        client.oauth.sign.key.audience = Some("petstore".to_string());

        let resolver = OAuthProviderResolver::new(&client);
        let sign = resolver.sign_provider().expect("sign provider");
        let key = resolver.sign_key_provider().expect("sign key provider");

        assert_eq!(sign.server_url.as_deref(), Some("https://oauth"));
        assert_eq!(sign.sign_service_id.as_deref(), Some("oauth-service"));
        assert_eq!(sign.uri, "/oauth2/signing");
        assert_eq!(sign.timeout, 1500);
        assert_eq!(key.server_url.as_deref(), Some("https://oauth"));
        assert_eq!(key.key_service_id.as_deref(), Some("oauth-service"));
        assert_eq!(key.client_id, "sign-client");
        assert_eq!(key.client_secret, "sign-secret");
        assert_eq!(key.audience.as_deref(), Some("petstore"));
    }

    #[test]
    fn deref_provider_resolves_direct_endpoint() {
        let mut client = ClientConfig::default();
        client.oauth.deref.server_url = Some("https://oauth".to_string());
        client.oauth.deref.service_id = Some("oauth-service".to_string());
        client.oauth.deref.uri = "/oauth2/deref".to_string();
        client.oauth.deref.client_id = "deref-client".to_string();
        client.oauth.deref.client_secret = "deref-secret".to_string();

        let provider = OAuthProviderResolver::new(&client)
            .deref_provider()
            .expect("deref provider");

        assert_eq!(provider.server_url.as_deref(), Some("https://oauth"));
        assert_eq!(provider.deref_service_id.as_deref(), Some("oauth-service"));
        assert_eq!(provider.uri, "/oauth2/deref");
        assert_eq!(provider.client_id, "deref-client");
        assert_eq!(provider.client_secret, "deref-secret");
    }
}
