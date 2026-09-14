//! Explicit opt-in for the A1 purpose contract; A2 wires each receiving route.
use crate::{AuthPrincipal, HandlerRejection, JwtExpiryMode, SecurityRuntime, verify_jwt_token};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
pub use oauth_workflow_contract::TokenUse;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LegacyLongLivedAppKey {
    pub issuer: String,
    pub kid: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    // The purpose helper accepts a cryptographically verified principal; these
    // tests verify the signature before exercising its purpose-only contract.
    fn verified(claims: serde_json::Value) -> (String, AuthPrincipal) {
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
        header.kid = Some("development-long-lived".into());
        let key = b"test-only-purpose-contract-key-32-bytes";
        let token = jsonwebtoken::encode(
            &header,
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(key),
        )
        .unwrap();
        let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256);
        validation.set_issuer(&["test-issuer"]);
        let claims = jsonwebtoken::decode::<serde_json::Value>(
            &token,
            &jsonwebtoken::DecodingKey::from_secret(key),
            &validation,
        )
        .unwrap()
        .claims;
        let principal = AuthPrincipal {
            client_id: None,
            user_id: None,
            issuer: Some("test-issuer".into()),
            email: None,
            host: None,
            role: None,
            claims,
        };
        (token, principal)
    }
    #[test]
    fn signed_app_with_user_claims_never_authenticates_as_user() {
        let (token, principal) = verified(json!({"iss":"test-issuer","exp":4102444800u64,
            "token_use":"app","uid":"user","role":"admin"}));
        assert!(validate_verified_purpose(&token, &principal, TokenUse::App, &[]).is_ok());
        assert!(validate_verified_purpose(&token, &principal, TokenUse::User, &[]).is_err());
    }
    #[test]
    fn legacy_exception_is_app_only_and_cannot_override_an_explicit_marker() {
        let keys = [LegacyLongLivedAppKey {
            issuer: "test-issuer".into(),
            kid: "development-long-lived".into(),
        }];
        let (token, principal) =
            verified(json!({"iss":"test-issuer","exp":4102444800u64,"uid":"user"}));
        assert!(validate_verified_purpose(&token, &principal, TokenUse::App, &keys).is_ok());
        assert!(validate_verified_purpose(&token, &principal, TokenUse::User, &keys).is_err());
        assert!(validate_verified_purpose(&token, &principal, TokenUse::App, &[]).is_err());
        for marker in [json!(null), json!("invalid"), json!("user")] {
            let (token, principal) =
                verified(json!({"iss":"test-issuer","exp":4102444800u64,"token_use":marker}));
            assert!(validate_verified_purpose(&token, &principal, TokenUse::App, &keys).is_err());
        }
    }
}

pub async fn verify_with_purpose(
    runtime: &SecurityRuntime,
    token: &str,
    expected: TokenUse,
    legacy_app_keys: &[LegacyLongLivedAppKey],
) -> Result<AuthPrincipal, HandlerRejection> {
    let principal = verify_jwt_token(runtime, token, JwtExpiryMode::Enforce).await?;
    validate_verified_purpose(token, &principal, expected, legacy_app_keys)?;
    Ok(principal)
}

/// The principal MUST have been validated from this exact token, including issuer,
/// signature and expiry. Legacy keys are trusted configuration, never token data.
pub fn validate_verified_purpose(
    token: &str,
    principal: &AuthPrincipal,
    expected: TokenUse,
    legacy_app_keys: &[LegacyLongLivedAppKey],
) -> Result<(), HandlerRejection> {
    let denied = || HandlerRejection::unauthorized("access token purpose rejected");
    let payload = token.split('.').nth(1).ok_or_else(denied)?;
    let payload = URL_SAFE_NO_PAD.decode(payload).map_err(|_| denied())?;
    match oauth_workflow_contract::verified_payload_purpose(&payload).map_err(|_| denied())? {
        Some(purpose) if purpose == expected => Ok(()),
        None if expected == TokenUse::App => {
            let header = jsonwebtoken::decode_header(token).map_err(|_| denied())?;
            let allowed = legacy_app_keys.iter().any(|key| {
                !key.issuer.is_empty()
                    && !key.kid.is_empty()
                    && principal.issuer.as_deref() == Some(key.issuer.as_str())
                    && header.kid.as_deref() == Some(key.kid.as_str())
            });
            if allowed { Ok(()) } else { Err(denied()) }
        }
        _ => Err(denied()),
    }
}
