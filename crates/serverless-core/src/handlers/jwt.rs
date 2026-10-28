//! Lambda adapter around shared verification; no cryptography, token decoding or refresh gate here.
use crate::{
    compat::Compatibility,
    plan::{Binding, HandlerFactory, RequestCapabilities},
    unsupported::SecuritySettings,
    *,
};
use async_trait::async_trait;
use http::header::AUTHORIZATION;
use light_security::{
    AuthPrincipal, HandlerRejection, JwtExpiryMode, SecurityConfig, SecurityRuntime,
    verify_jwt_token_for_services_with_deadline,
};
use serde_json::Value;
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

pub struct JwtHandler {
    config: SecurityConfig,
    runtime: Option<Arc<SecurityRuntime>>,
    enabled: bool,
    required_scopes: Vec<String>,
}
impl JwtHandler {
    /// Scope requirements are trusted construction input, never request data or a new YAML key.
    pub fn new(
        settings: SecuritySettings,
        runtime: Option<Arc<SecurityRuntime>>,
        required_scopes: Vec<String>,
    ) -> Result<Self, InitError> {
        if settings.enabled
            && settings.config.enable_verify_jwt
            && !settings.config.enable_mock_jwt
            && runtime.as_ref().is_none_or(|r| r.config != settings.config)
        {
            return Err(InitError::Malformed(
                "security.yml: verified runtime/configuration mismatch",
            ));
        }
        if required_scopes
            .iter()
            .any(|s| s.is_empty() || s.chars().any(char::is_whitespace))
        {
            return Err(InitError::Malformed(
                "security: invalid trusted scope requirement",
            ));
        }
        Ok(Self {
            enabled: settings.enabled,
            config: settings.config,
            runtime,
            required_scopes,
        })
    }
    pub fn capabilities(&self) -> RequestCapabilities {
        RequestCapabilities {
            establishes_verified_identity: self.enabled
                && self.config.enable_verify_jwt
                && !self.config.enable_mock_jwt
                && self.config.skip_path_prefixes.is_empty(),
            requires_verified_identity: false,
        }
    }
    pub(crate) async fn authenticate(
        &self,
        inv: &Invocation,
        selected_services: &[String],
        strict_scheme: bool,
    ) -> Result<Option<Principal>, StatusError> {
        self.authenticate_inner(inv, selected_services, strict_scheme, None)
            .await
    }
    /// Only an immutable descriptor produced by trusted configuration may be used.
    pub(crate) async fn authenticate_for_operation(
        &self,
        inv: &Invocation,
        selected_services: &[String],
        strict_scheme: bool,
        operation: &gateway_policy::openapi_operation::OperationDescriptor,
    ) -> Result<Option<Principal>, StatusError> {
        self.authenticate_inner(inv, selected_services, strict_scheme, Some(operation))
            .await
    }
    async fn authenticate_inner(
        &self,
        inv: &Invocation,
        selected_services: &[String],
        strict_scheme: bool,
        operation: Option<&gateway_policy::openapi_operation::OperationDescriptor>,
    ) -> Result<Option<Principal>, StatusError> {
        // Explicit insecure-test mock mode is an unauthenticated bypass, never a fabricated principal.
        // Init requires the exact test exception; no token/claim is ever logged here.
        if !self.enabled
            || !self.config.enable_verify_jwt
            || self.config.enable_mock_jwt
            || self
                .config
                .skip_path_prefixes
                .iter()
                .any(|p| inv.path.starts_with(p))
        {
            return Ok(None);
        }
        let auth =
            header(inv, AUTHORIZATION.as_str())?.ok_or_else(StatusError::missing_identity)?;
        if auth.trim().len() < 6 {
            return Err(invalid());
        }
        let token = match bearer(auth) {
            Some(token) => token,
            None if strict_scheme => return Err(invalid()),
            None if self.config.enable_extract_scope_token
                && auth
                    .get(..6)
                    .is_some_and(|prefix| !prefix.eq_ignore_ascii_case("Bearer")) =>
            {
                header(inv, "x-scope-token")?
                    .and_then(bearer)
                    .ok_or_else(StatusError::missing_identity)?
            }
            None => return Err(StatusError::missing_identity()),
        };
        let runtime = self
            .runtime
            .as_ref()
            .ok_or_else(StatusError::handler_failure)?;
        let mut service_ids = selected_services.to_vec();
        if service_ids.is_empty()
            && let Some(id) = runtime.service_id_for_request(header(inv, "service_id")?, &inv.path)
        {
            service_ids.push(id);
        }
        let principal = verify(runtime, token, &service_ids, inv).await?;
        if self.config.enable_verify_scope {
            if let Some(scope_header) = header(inv, "x-scope-token")? {
                let token = bearer(scope_header).ok_or_else(|| secondary_error(false))?;
                let secondary =
                    verify(runtime, token, &service_ids, inv)
                        .await
                        .map_err(|error| {
                            if error.code == "ERR14017" {
                                error
                            } else {
                                secondary_error(error.code == "ERR10001")
                            }
                        })?;
                let secondary_scopes =
                    scopes(&secondary.claims).map_err(|_| secondary_error(false))?;
                if !matches_scope(&secondary_scopes, &self.required_scopes) {
                    return Err(scope_mismatch(
                        "ERR10006",
                        "SCOPE_TOKEN_SCOPE_MISMATCH",
                        &secondary_scopes,
                        &self.required_scopes,
                    ));
                }
                check_operation_scopes(
                    operation,
                    &secondary_scopes,
                    "ERR10006",
                    "SCOPE_TOKEN_SCOPE_MISMATCH",
                )?;
            } else if !self.required_scopes.is_empty() || operation.is_some() {
                let primary = scopes(&principal.claims)?;
                if !matches_scope(&primary, &self.required_scopes) {
                    return Err(scope_mismatch(
                        "ERR10005",
                        "AUTH_TOKEN_SCOPE_MISMATCH",
                        &primary,
                        &self.required_scopes,
                    ));
                }
                check_operation_scopes(
                    operation,
                    &primary,
                    "ERR10005",
                    "AUTH_TOKEN_SCOPE_MISMATCH",
                )?;
            }
        }
        if tokio::time::Instant::now() >= inv.deadline {
            return Err(StatusError::timeout());
        }
        Ok(Some(from_verified_principal(principal)))
    }
}
#[async_trait]
impl RequestHandler for JwtHandler {
    fn id(&self) -> HandlerId {
        HandlerId::Jwt
    }
    fn operation_authorization_role(&self) -> OperationAuthorizationRole {
        if self.enabled
            && self.config.enable_verify_jwt
            && self.config.enable_verify_scope
            && !self.config.enable_mock_jwt
        {
            OperationAuthorizationRole::JwtScopes {
                guaranteed: self.config.skip_path_prefixes.is_empty(),
            }
        } else {
            OperationAuthorizationRole::None
        }
    }
    async fn on_request(
        &self,
        inv: &mut Invocation,
        st: &mut ExchangeState,
    ) -> Result<HandlerOutcome, HandlerFailure> {
        st.principal = None;
        let result = match st.operation.as_deref() {
            Some(operation) => {
                self.authenticate_for_operation(inv, &[], false, operation)
                    .await
            }
            None => self.authenticate(inv, &[], false).await,
        };
        Ok(match result {
            Ok(principal) => {
                st.principal = principal;
                HandlerOutcome::Continue
            }
            Err(error) => HandlerOutcome::Reject(error),
        })
    }
}

fn check_operation_scopes(
    operation: Option<&gateway_policy::openapi_operation::OperationDescriptor>,
    actual: &[String],
    code: &str,
    name: &str,
) -> Result<(), StatusError> {
    let Some(operation) = operation else {
        return Ok(());
    };
    // A sorted Value is not source-order provenance. This is a local configuration
    // fault, never a token-scope fallback or an anonymous authorization success.
    let order = operation
        .ordered_oauth2_names()
        .ok_or_else(StatusError::handler_failure)?;
    for requirement in operation.security_requirements().unwrap_or_default() {
        for scheme in order {
            if let Some(required) = requirement.get(scheme) {
                return if matches_scope(actual, required) {
                    Ok(())
                } else {
                    Err(scope_mismatch(code, name, actual, required))
                };
            }
        }
    }
    Ok(())
}
impl HandlerFactory for JwtHandler {
    fn validator_media_role(&self) -> crate::plan::ValidatorMediaRole {
        crate::plan::ValidatorMediaRole::Preserves
    }
    fn build(&self, _: &Compatibility, _: &str) -> Result<Binding, InitError> {
        // Clone only immutable policy and the shared runtime Arc; no request state is retained.
        let handler = Self {
            config: self.config.clone(),
            runtime: self.runtime.clone(),
            enabled: self.enabled,
            required_scopes: self.required_scopes.clone(),
        };
        Ok(Binding::Request(Arc::new(handler), self.capabilities()))
    }
}

/// The sole local adapter accepts shared verifier output, not a request or encoded token.
/// AuthPrincipal is a trusted-code boundary, as is the existing core Principal constructor.
fn from_verified_principal(principal: AuthPrincipal) -> Principal {
    let subject = principal
        .claims
        .get("sub")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| principal.user_id.clone())
        .or_else(|| principal.client_id.clone())
        .unwrap_or_default();
    Principal::from_verified(
        subject,
        principal.client_id,
        principal.user_id,
        principal.claims,
    )
}
pub(crate) fn invalid() -> StatusError {
    StatusError::new(
        401,
        "ERR10000",
        "INVALID_AUTH_TOKEN",
        "Incorrect signature or malformed token in authorization header",
    )
}
fn secondary_error(expired: bool) -> StatusError {
    if expired {
        StatusError::new(
            401,
            "ERR10004",
            "SCOPE_TOKEN_EXPIRED",
            "Jwt token in X-Scope-Token expired",
        )
    } else {
        StatusError::new(
            401,
            "ERR10003",
            "INVALID_SCOPE_TOKEN",
            "Incorrect signature or malformed token in X-Scope-Token header",
        )
    }
}
pub(crate) fn escaped(value: &str) -> String {
    let json = serde_json::to_string(value).expect("string serialization");
    json[1..json.len() - 1].to_owned()
}
fn scope_mismatch(
    code: &str,
    message: &str,
    actual: &[String],
    required: &[String],
) -> StatusError {
    StatusError::new(
        403,
        code,
        message,
        &escaped(&format!(
            "Scopes [{}] in {} and specification scopes [{}] are not matched",
            actual.join(", "),
            if code == "ERR10006" {
                "scope token"
            } else {
                "authorization token"
            },
            required.join(", ")
        )),
    )
}
fn matches_scope(actual: &[String], required: &[String]) -> bool {
    required.is_empty() || required.iter().any(|s| actual.contains(s))
}
fn scopes(claims: &Value) -> Result<Vec<String>, StatusError> {
    let primary = parse_scopes(claims.get("scope"))?;
    if primary.is_empty() {
        parse_scopes(claims.get("scp"))
    } else {
        Ok(primary)
    }
}
fn parse_scopes(value: Option<&Value>) -> Result<Vec<String>, StatusError> {
    match value {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::String(s)) => Ok(s.split_whitespace().map(str::to_owned).collect()),
        Some(Value::Array(a)) => a
            .iter()
            .map(|s| s.as_str().map(str::to_owned).ok_or_else(invalid))
            .collect(),
        _ => Err(invalid()),
    }
}
pub(crate) fn header<'a>(inv: &'a Invocation, name: &str) -> Result<Option<&'a str>, StatusError> {
    let mut values = inv.headers.get_all(name).iter();
    let result = values
        .next()
        .map(|v| v.to_str().map_err(|_| invalid()))
        .transpose()?;
    if values.next().is_some() {
        return Err(invalid());
    }
    Ok(result)
}
fn bearer(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    (scheme.eq_ignore_ascii_case("bearer") && !token.trim().is_empty()).then(|| token.trim())
}
async fn verify(
    runtime: &SecurityRuntime,
    token: &str,
    services: &[String],
    inv: &Invocation,
) -> Result<AuthPrincipal, StatusError> {
    match verify_jwt_token_for_services_with_deadline(
        runtime,
        token,
        JwtExpiryMode::Enforce,
        services,
        Some(inv.deadline),
    )
    .await
    {
        Ok(principal) => Ok(principal),
        Err(error) if error.code == "ERR14017" => Err(StatusError::timeout()),
        Err(error) => {
            if error.status == 401
                && error.message == "JWT validation failed"
                && !runtime.config.ignore_jwt_expiry
            {
                // Shared API intentionally hides JWT error kind. Diagnose only with successfully
                // reverified signed claims; never decode an untrusted token or authenticate this result.
                let diagnostic = light_security::verify_jwt_token_for_services_without_refresh(
                    runtime,
                    token,
                    JwtExpiryMode::Ignore,
                    services,
                    Some(inv.deadline),
                )
                .await;
                match diagnostic {
                    Ok(principal) => {
                        let now = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map_err(|_| StatusError::handler_failure())?
                            .as_secs();
                        if principal
                            .claims
                            .get("exp")
                            .and_then(Value::as_u64)
                            .is_some_and(|exp| {
                                exp.saturating_add(runtime.config.jwt.clock_skew_in_seconds) < now
                            })
                        {
                            return Err(StatusError::new(
                                401,
                                "ERR10001",
                                "AUTH_TOKEN_EXPIRED",
                                "Jwt token in authorization header expired",
                            ));
                        }
                    }
                    Err(HandlerRejection { code, .. }) if code == "ERR14017" => {
                        return Err(StatusError::timeout());
                    }
                    _ => {}
                }
            }
            Err(invalid())
        }
    }
}
