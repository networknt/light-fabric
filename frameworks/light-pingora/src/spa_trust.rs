//! Explicit SPA token roles, separate from generic security.yml trust and caches.
use crate::{
    AuthPrincipal, HandlerRejection,
    spa_jwks::{JwksSource, invalid},
};
use light_runtime::{ModuleKind, RuntimeConfig, RuntimeError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

pub const SPA_TOKEN_TRUST_FILE: &str = "spa-token-trust.yml";
pub const SPA_TOKEN_TRUST_MODULE_ID: &str = "light-pingora/spa-token-trust";

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SpaTokenPolicy {
    #[serde(default)]
    pub issuer: String,
    #[serde(default)]
    pub audience: String,
    #[serde(default)]
    pub jwks_uri: String,
    #[serde(
        default,
        deserialize_with = "crate::config_util::deserialize_string_list"
    )]
    pub jwks_ca_paths: Vec<String>,
    #[serde(default)]
    pub tenant_id: String,
    #[serde(
        default,
        deserialize_with = "crate::config_util::deserialize_string_list"
    )]
    pub required_scopes: Vec<String>,
    #[serde(
        default,
        deserialize_with = "crate::config_util::deserialize_string_list"
    )]
    pub allowed_client_ids: Vec<String>,
    #[serde(default)]
    pub clock_skew_seconds: u64,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SpaTokenTrustConfig {
    #[serde(default)]
    pub internal: SpaTokenPolicy,
    #[serde(default)]
    pub microsoft_id: SpaTokenPolicy,
    #[serde(default)]
    pub microsoft_api: SpaTokenPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenRole {
    Internal,
    MicrosoftId,
    MicrosoftApi,
}

#[derive(Clone)]
pub struct SpaTokenTrust {
    policy: SpaTokenPolicy,
    role: TokenRole,
    pub(crate) source: Arc<JwksSource>,
}

fn uuid(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok_and(|id| !id.is_nil() && id.to_string() == value)
}

impl SpaTokenPolicy {
    pub(crate) fn validate(&self, role: TokenRole) -> Result<(), RuntimeError> {
        let invalid =
            || RuntimeError::Unsupported("Missing or invalid SPA token trust policy".into());
        if self.issuer.is_empty()
            || self.issuer.trim() != self.issuer
            || self.audience.is_empty()
            || self.audience.trim() != self.audience
            || self.issuer.chars().any(char::is_whitespace)
            || self.audience.chars().any(char::is_whitespace)
            || self.clock_skew_seconds > 300
        {
            return Err(invalid());
        }
        crate::spa_jwks::validate_endpoint(&self.jwks_uri)?;
        if role != TokenRole::Internal {
            let url = url::Url::parse(&self.issuer).map_err(|_| invalid())?;
            if !uuid(&self.tenant_id)
                || url.scheme() != "https"
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
                || ![
                    format!("/{}/", self.tenant_id),
                    format!("/{}/v2.0", self.tenant_id),
                ]
                .contains(&url.path().to_owned())
            {
                return Err(invalid());
            }
            if (role == TokenRole::MicrosoftId || self.issuer.ends_with("/v2.0"))
                && !uuid(&self.audience)
            {
                return Err(invalid());
            }
        }
        if role == TokenRole::MicrosoftApi
            && (self.required_scopes.is_empty()
                || self
                    .required_scopes
                    .iter()
                    .any(|v| v.is_empty() || v.chars().any(char::is_whitespace))
                || self.allowed_client_ids.is_empty()
                || self.allowed_client_ids.iter().any(|v| !uuid(v)))
        {
            return Err(invalid());
        }
        Ok(())
    }

    fn check(
        &self,
        role: TokenRole,
        claims: &Value,
        allow_expired: bool,
    ) -> Result<(), HandlerRejection> {
        let text = |name| claims.get(name).and_then(Value::as_str);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| invalid())?
            .as_secs();
        let audience = match claims.get("aud") {
            Some(Value::String(value)) => value == &self.audience,
            Some(Value::Array(values)) => {
                values.len() == 1 && values[0].as_str() == Some(&self.audience)
            }
            _ => false,
        };
        let expiry = claims
            .get("exp")
            .and_then(Value::as_u64)
            .ok_or_else(invalid)?;
        if text("iss") != Some(&self.issuer)
            || !audience
            || (!allow_expired && now >= expiry.saturating_add(self.clock_skew_seconds))
            || claims.get("nbf").is_some_and(|v| {
                v.as_u64()
                    .is_none_or(|nbf| nbf > now.saturating_add(self.clock_skew_seconds))
            })
        {
            return Err(invalid());
        }
        if role == TokenRole::Internal {
            if !["sub", "user_id", "uid"]
                .iter()
                .any(|name| text(name).is_some_and(|v| !v.trim().is_empty()))
                || !text("csrf").is_some_and(crate::csrf_security::bounded)
                || claims
                    .get("token_use")
                    .is_some_and(|v| !matches!(v.as_str(), Some("user" | "access_token")))
                || claims.get("nonce").is_some()
            {
                return Err(invalid());
            }
        } else {
            let version = if self.issuer.ends_with("/v2.0") {
                "2.0"
            } else {
                "1.0"
            };
            if text("tid") != Some(&self.tenant_id)
                || text("ver") != Some(version)
                || !text("sub").is_some_and(|v| !v.trim().is_empty())
            {
                return Err(invalid());
            }
            match role {
                TokenRole::MicrosoftId => {
                    // Presence is a purpose marker, not a gateway-generated nonce challenge.
                    if !text("nonce").is_some_and(|v| !v.trim().is_empty())
                        || claims.get("scp").is_some()
                        || claims
                            .get("idtyp")
                            .is_some_and(|v| v.as_str() != Some("user"))
                        || claims
                            .get("token_use")
                            .is_some_and(|v| !matches!(v.as_str(), Some("id" | "id_token")))
                    {
                        return Err(invalid());
                    }
                }
                TokenRole::MicrosoftApi => {
                    let scopes = text("scp").ok_or_else(invalid)?;
                    if self
                        .required_scopes
                        .iter()
                        .any(|scope| !scopes.split(' ').any(|s| s == scope))
                        || !text(if version == "2.0" { "azp" } else { "appid" }).is_some_and(|id| {
                            self.allowed_client_ids.iter().any(|allowed| allowed == id)
                        })
                        || claims
                            .get("idtyp")
                            .is_some_and(|v| v.as_str() != Some("user"))
                        || claims
                            .get("token_use")
                            .is_some_and(|v| !matches!(v.as_str(), Some("access" | "access_token")))
                    {
                        return Err(invalid());
                    }
                }
                TokenRole::Internal => unreachable!(),
            }
        }
        Ok(())
    }
}

impl SpaTokenTrust {
    #[cfg(test)]
    pub(crate) fn test_internal() -> Self {
        let policy = SpaTokenPolicy {
            issuer: "https://internal.example".into(),
            audience: "portal".into(),
            jwks_uri: "https://internal.example/keys".into(),
            ..Default::default()
        };
        let trust = Self::new(policy, TokenRole::Internal).unwrap();
        trust
            .source
            .seed_test_keys(include_str!("../tests/fixtures/msal/signing-key.jwks.json"));
        trust
    }

    pub fn new(policy: SpaTokenPolicy, role: TokenRole) -> Result<Self, RuntimeError> {
        policy.validate(role)?;
        let source = Arc::new(JwksSource::with_roots(
            &policy.jwks_uri,
            &policy.jwks_ca_paths,
        )?);
        Ok(Self {
            policy,
            role,
            source,
        })
    }

    pub(crate) async fn verify_for(
        &self,
        role: TokenRole,
        token: &str,
        allow_expired: bool,
    ) -> Result<AuthPrincipal, HandlerRejection> {
        if self.role != role {
            return Err(invalid());
        }
        self.verify(token, allow_expired).await
    }

    pub(crate) async fn verify(
        &self,
        token: &str,
        allow_expired: bool,
    ) -> Result<AuthPrincipal, HandlerRejection> {
        let claims = self
            .source
            .verify_expiry(token, self.policy.clock_skew_seconds, allow_expired)
            .await?;
        self.policy.check(self.role, &claims, allow_expired)?;
        let text = |name| claims.get(name).and_then(Value::as_str).map(str::to_owned);
        Ok(AuthPrincipal {
            client_id: text("client_id").or_else(|| text("cid")),
            user_id: text("user_id")
                .or_else(|| text("uid"))
                .or_else(|| text("sub")),
            issuer: text("iss"),
            email: text("email").or_else(|| text("eml")),
            host: text("host"),
            role: text("role"),
            claims,
        })
    }
    pub(crate) async fn bootstrap(&self) -> Result<(), HandlerRejection> {
        self.source.bootstrap().await
    }
}

pub(crate) struct SpaTrustBundle {
    policies: SpaTokenTrustConfig,
    pub(crate) internal: Option<SpaTokenTrust>,
    pub(crate) id: Option<SpaTokenTrust>,
    pub(crate) api: Option<SpaTokenTrust>,
}

pub(crate) fn load_spa_trust(
    config: &RuntimeConfig,
    internal: bool,
    id: bool,
    api: bool,
) -> Result<SpaTrustBundle, RuntimeError> {
    let policies: SpaTokenTrustConfig = config
        .module_registry
        .load_config(config, SPA_TOKEN_TRUST_FILE)?;
    let mut sources: BTreeMap<(String, Vec<String>), Arc<JwksSource>> = BTreeMap::new();
    let mut build = |policy: &SpaTokenPolicy,
                     role: TokenRole,
                     needed: bool|
     -> Result<Option<SpaTokenTrust>, RuntimeError> {
        if !needed {
            return Ok(None);
        }
        policy.validate(role)?;
        let identity = (policy.jwks_uri.clone(), policy.jwks_ca_paths.clone());
        let source = if let Some(source) = sources.get(&identity) {
            source.clone()
        } else {
            let source = Arc::new(JwksSource::with_roots(
                &policy.jwks_uri,
                &policy.jwks_ca_paths,
            )?);
            sources.insert(identity, source.clone());
            source
        };
        Ok(Some(SpaTokenTrust {
            policy: policy.clone(),
            role,
            source,
        }))
    };
    let bundle = SpaTrustBundle {
        policies: policies.clone(),
        internal: build(&policies.internal, TokenRole::Internal, internal)?,
        id: build(&policies.microsoft_id, TokenRole::MicrosoftId, id)?,
        api: build(&policies.microsoft_api, TokenRole::MicrosoftApi, api)?,
    };
    Ok(bundle)
}

impl SpaTrustBundle {
    pub(crate) fn register(&self, config: &RuntimeConfig) -> Result<(), RuntimeError> {
        config.module_registry.register_loaded_config(
            SPA_TOKEN_TRUST_MODULE_ID,
            "spa-token-trust",
            ModuleKind::Framework,
            &self.policies,
            [],
            true,
            Some(true),
            false,
        )?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "spa_trust_tests.rs"]
mod tests;
