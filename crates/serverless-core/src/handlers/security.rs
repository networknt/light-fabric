//! Java prefix/Authorization precedence with explicitly bound Lambda factors.
use crate::handlers::factors::{ApiKeyHandler, BasicHandler, invalid_scheme};
use crate::{
    compat::Compatibility,
    handlers::jwt::{JwtHandler, escaped, header, invalid},
    plan::{Binding, HandlerFactory, RequestCapabilities},
    unsupported::{boolean, object, strings},
    *,
};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

pub struct PrefixAuth {
    pub prefix: String,
    pub basic: bool,
    pub jwt: bool,
    pub apikey: bool,
    pub services: Vec<String>,
}
pub struct UnifiedSettings {
    pub enabled: bool,
    pub anonymous: Vec<String>,
    pub prefixes: Vec<PrefixAuth>,
}
impl UnifiedSettings {
    pub fn from_value(value: &Value) -> Result<Self, InitError> {
        let map = object(value, "unified-security.yml: object required")?;
        if map
            .keys()
            .any(|k| !["enabled", "anonymousPrefixes", "pathPrefixAuths"].contains(&k.as_str()))
        {
            return Err(InitError::Malformed("unified-security.yml: unknown key"));
        }
        let anonymous = strings(map.get("anonymousPrefixes"))?;
        if anonymous.iter().any(|p| !p.starts_with('/')) {
            return Err(InitError::Malformed(
                "unified-security.yml: invalid anonymous prefix",
            ));
        }
        let value = match map.get("pathPrefixAuths") {
            None | Some(Value::Null) => Value::Array(vec![]),
            Some(Value::String(s)) if s.trim().is_empty() => Value::Array(vec![]),
            Some(Value::String(s)) => serde_json::from_str(s)
                .map_err(|_| InitError::Malformed("unified-security.yml: prefix list"))?,
            Some(v) => v.clone(),
        };
        let mut prefixes = Vec::new();
        for entry in value
            .as_array()
            .ok_or(InitError::Malformed("unified-security.yml: prefix list"))?
        {
            let entry = object(entry, "unified-security.yml: prefix object")?;
            if entry.keys().any(|k| {
                ![
                    "prefix",
                    "jwt",
                    "basic",
                    "apikey",
                    "swt",
                    "sjwt",
                    "ldap",
                    "jwkServiceIds",
                    "sjwkServiceIds",
                    "swtServiceIds",
                ]
                .contains(&k.as_str())
            }) {
                return Err(InitError::Malformed(
                    "unified-security.yml: unknown factor/key",
                ));
            }
            for factor in ["swt", "sjwt", "ldap"] {
                if boolean(entry, factor, false)? {
                    return Err(InitError::Malformed(
                        "unified-security.yml: factor not supported until WP12",
                    ));
                }
            }
            for key in ["sjwkServiceIds", "swtServiceIds"] {
                if !strings(entry.get(key))?.is_empty() {
                    return Err(InitError::Malformed(
                        "unified-security.yml: service factor not supported until WP12",
                    ));
                }
            }
            let prefix = entry
                .get("prefix")
                .and_then(Value::as_str)
                .filter(|p| p.starts_with('/'))
                .ok_or(InitError::Malformed("unified-security.yml: invalid prefix"))?;
            let jwt = boolean(entry, "jwt", false)?;
            let basic = boolean(entry, "basic", false)?;
            let apikey = boolean(entry, "apikey", false)?;
            let services = strings(entry.get("jwkServiceIds"))?;
            if !jwt && !services.is_empty() {
                return Err(InitError::Malformed(
                    "unified-security.yml: services without JWT",
                ));
            }
            prefixes.push(PrefixAuth {
                prefix: prefix.to_owned(),
                basic,
                jwt,
                apikey,
                services,
            });
        }
        Ok(Self {
            enabled: boolean(map, "enabled", true)?,
            anonymous,
            prefixes,
        })
    }
}
pub struct UnifiedHandler {
    settings: Arc<UnifiedSettings>,
    jwt: Arc<JwtHandler>,
    basic: Option<Arc<BasicHandler>>,
    apikey: Option<Arc<ApiKeyHandler>>,
}
impl UnifiedHandler {
    pub fn new(settings: UnifiedSettings, jwt: Arc<JwtHandler>) -> Self {
        Self {
            settings: Arc::new(settings),
            jwt,
            basic: None,
            apikey: None,
        }
    }
    /// Consuming construction only: immutable qualified bindings are retained
    /// by every compiled clone. Existing new(settings, jwt) stays JWT-compatible.
    pub fn with_factor_bindings(
        mut self,
        basic: Option<Arc<BasicHandler>>,
        apikey: Option<Arc<ApiKeyHandler>>,
    ) -> Result<Self, InitError> {
        self.basic = basic;
        self.apikey = apikey;
        self.validate_factor_bindings()?;
        Ok(self)
    }
    fn validate_factor_bindings(&self) -> Result<(), InitError> {
        if self.settings.prefixes.iter().any(|p| p.basic)
            && self.basic.as_ref().is_none_or(|b| !b.enabled())
        {
            return Err(InitError::Malformed(
                "unified-security.yml: enabled Basic binding required",
            ));
        }
        if self.settings.prefixes.iter().any(|p| p.apikey)
            && self.apikey.as_ref().is_none_or(|a| !a.enabled())
        {
            return Err(InitError::Malformed(
                "unified-security.yml: enabled API-key binding required",
            ));
        }
        Ok(())
    }
}
#[async_trait]
impl RequestHandler for UnifiedHandler {
    fn id(&self) -> HandlerId {
        HandlerId::Security
    }
    fn operation_authorization_role(&self) -> OperationAuthorizationRole {
        if !self.settings.enabled || !self.settings.prefixes.iter().any(|p| p.jwt) {
            return OperationAuthorizationRole::None;
        }
        match self.jwt.operation_authorization_role() {
            OperationAuthorizationRole::JwtScopes { guaranteed } => {
                OperationAuthorizationRole::JwtScopes {
                    guaranteed: guaranteed
                        && self.settings.anonymous.is_empty()
                        && self
                            .settings
                            .prefixes
                            .iter()
                            .all(|p| p.jwt && !p.basic && !p.apikey),
                }
            }
            _ => OperationAuthorizationRole::None,
        }
    }
    fn operation_authorization_role_for_selection(
        &self,
        selection: &crate::plan::OperationSelection,
    ) -> OperationAuthorizationRole {
        let global = self.operation_authorization_role();
        if global != (OperationAuthorizationRole::JwtScopes { guaranteed: false })
            || self.jwt.operation_authorization_role()
                != (OperationAuthorizationRole::JwtScopes { guaranteed: true })
            || self
                .settings
                .anonymous
                .iter()
                .any(|prefix| selection.may_match_prefix(prefix))
        {
            return global;
        }
        // All possibly earlier branches must be JWT-only. A covering branch
        // then proves the complete selected language under declaration order.
        for prefix in &self.settings.prefixes {
            if !selection.may_match_prefix(&prefix.prefix) {
                continue;
            }
            if !prefix.jwt || prefix.basic || prefix.apikey {
                return global;
            }
            if selection.is_subset_of_prefix(&prefix.prefix) {
                return OperationAuthorizationRole::JwtScopes { guaranteed: true };
            }
        }
        global
    }
    async fn on_request(
        &self,
        inv: &mut Invocation,
        st: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        st.principal = None;
        // Factory validation is the Init guarantee. Direct trusted core callers
        // that bypass it still cannot turn an incomplete factor into a success.
        if self.validate_factor_bindings().is_err() {
            return Ok(HandlerOutcome::Reject(StatusError::handler_failure()));
        }
        if !self.settings.enabled
            || self
                .settings
                .anonymous
                .iter()
                .any(|p| inv.path.starts_with(p))
        {
            return Ok(HandlerOutcome::Continue);
        }
        let mut found = false;
        for prefix in &self.settings.prefixes {
            if !inv.path.starts_with(&prefix.prefix) {
                continue;
            }
            found = true;
            if prefix.basic || prefix.jwt {
                let auth = match header(inv, "authorization") {
                    Ok(Some(auth)) => auth,
                    Ok(None) => {
                        if prefix.basic {
                            st.response_headers.insert(
                                "www-authenticate",
                                "Basic realm=\"Default Realm\""
                                    .parse()
                                    .expect("static header"),
                            );
                        }
                        return Ok(HandlerOutcome::Reject(StatusError::missing_identity()));
                    }
                    Err(error) => return Ok(HandlerOutcome::Reject(error)),
                };
                let scheme = auth.split_once(' ').filter(|(_, c)| !c.trim().is_empty());
                if let Some((scheme, _)) = scheme
                    && scheme.eq_ignore_ascii_case("basic")
                {
                    // Approved S-12a exception: never use a Basic identity on a
                    // JWT-only prefix merely because a Basic alias exists.
                    if !prefix.basic {
                        return Ok(HandlerOutcome::Reject(invalid()));
                    }
                    let Some(basic) = &self.basic else {
                        return Ok(HandlerOutcome::Reject(StatusError::handler_failure()));
                    };
                    return Ok(match basic.authenticate_basic(&inv.path, auth) {
                        Ok(()) => HandlerOutcome::Continue,
                        Err(e) => HandlerOutcome::Reject(e),
                    });
                }
                if !scheme.is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer")) {
                    // Keep the previously qualified JWT-only rejection bytes.
                    return Ok(HandlerOutcome::Reject(if prefix.basic {
                        invalid_scheme(if auth.eq_ignore_ascii_case("basic") {
                            "BASIC"
                        } else {
                            "unknown"
                        })
                    } else {
                        invalid()
                    }));
                }
                if !prefix.jwt {
                    return Ok(HandlerOutcome::Reject(invalid()));
                }
            } else if prefix.apikey {
                let Some(apikey) = &self.apikey else {
                    return Ok(HandlerOutcome::Reject(StatusError::handler_failure()));
                };
                // Preserve S-12 precedence: Authorization factors return before
                // this branch; they do not turn legacy flags into allOf.
                return Ok(match apikey.authenticate(inv, false).await {
                    Ok(()) => HandlerOutcome::Continue,
                    Err(e) => HandlerOutcome::Reject(e),
                });
            }
            if !prefix.jwt {
                continue;
            }
            let result = match header(inv, "authorization") {
                Ok(None) => Err(StatusError::missing_identity()),
                Ok(Some(value))
                    if !value
                        .split_once(' ')
                        .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer")) =>
                {
                    Err(invalid())
                }
                Err(error) => Err(error),
                _ => match st.operation.as_deref() {
                    Some(operation) => {
                        self.jwt
                            .authenticate_for_operation(inv, &prefix.services, true, operation)
                            .await
                    }
                    None => self.jwt.authenticate(inv, &prefix.services, true).await,
                },
            };
            return Ok(match result {
                Ok(principal) => {
                    st.principal = principal;
                    HandlerOutcome::Continue
                }
                Err(error) => HandlerOutcome::Reject(error),
            });
        }
        Ok(if found {
            HandlerOutcome::Continue
        } else {
            HandlerOutcome::Reject(StatusError::new(
                400,
                "ERR10078",
                "MISSING_PATH_PREFIX_AUTH",
                &escaped(&format!(
                    "Missing path prefix auth entry for request path {}.",
                    inv.path
                )),
            ))
        })
    }
}
impl HandlerFactory for UnifiedHandler {
    fn validator_media_role(&self) -> crate::plan::ValidatorMediaRole {
        crate::plan::ValidatorMediaRole::Preserves
    }
    fn build(&self, _: &Compatibility, _: &str) -> Result<Binding, InitError> {
        self.validate_factor_bindings()?;
        Ok(Binding::Request(
            Arc::new(Self {
                settings: self.settings.clone(),
                jwt: self.jwt.clone(),
                basic: self.basic.clone(),
                apikey: self.apikey.clone(),
            }),
            RequestCapabilities {
                establishes_verified_identity: self.settings.enabled
                    && self.settings.anonymous.is_empty()
                    && !self.settings.prefixes.is_empty()
                    && self
                        .settings
                        .prefixes
                        .iter()
                        .all(|p| p.jwt && !p.basic && !p.apikey)
                    && self.jwt.capabilities().establishes_verified_identity,
                requires_verified_identity: false,
            },
        ))
    }
}
