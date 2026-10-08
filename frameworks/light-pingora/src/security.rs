use crate::config_util::request_header;
use light_security::token_purpose::{TokenUse, validate_verified_purpose};
use light_security::verify_jwt_token_for_services;
use pingora::prelude::Session;

pub use light_security::{
    AuthPrincipal, HandlerRejection, JwtExpiryMode, SECURITY_CONFIG_NAME, SECURITY_FILE,
    SECURITY_MODULE_ID, SecurityConfig, SecurityJwtConfig, SecurityRuntime, load_security_runtime,
    load_security_runtime_from_file, verify_jwt_token,
};

const AUTHORIZATION: &str = "authorization";
const SERVICE_ID_HEADER: &str = "service_id";
const SCOPE_TOKEN: &str = "X-Scope-Token";

pub async fn verify_jwt_request(
    session: &mut Session,
    runtime: &SecurityRuntime,
    request_path: &str,
) -> Result<Option<AuthPrincipal>, HandlerRejection> {
    verify_jwt_request_with_service_ids(session, runtime, request_path, &[]).await
}

pub async fn verify_jwt_request_with_service_id_override(
    session: &mut Session,
    runtime: &SecurityRuntime,
    request_path: &str,
    service_id_override: Option<&str>,
) -> Result<Option<AuthPrincipal>, HandlerRejection> {
    let service_ids = service_id_override
        .and_then(non_empty)
        .map(|id| vec![id.to_string()])
        .unwrap_or_default();
    verify_jwt_request_with_service_ids(session, runtime, request_path, &service_ids).await
}

pub async fn verify_jwt_request_with_service_ids(
    session: &mut Session,
    runtime: &SecurityRuntime,
    request_path: &str,
    service_ids: &[String],
) -> Result<Option<AuthPrincipal>, HandlerRejection> {
    let config = &runtime.config;
    if request_path_is_skipped(config, request_path) {
        return Ok(None);
    }
    if !config.enable_h2c && is_h2c_upgrade(session) {
        return Err(HandlerRejection::new(
            405,
            "ERR10048",
            "cleartext HTTP/2 upgrade is not allowed",
        ));
    }
    if config.enable_mock_jwt {
        return Ok(Some(mock_principal()));
    }
    if !config.enable_verify_jwt {
        return Ok(None);
    }

    let authorization = request_header(session, AUTHORIZATION);
    let token = required_authorization_bearer(authorization.as_deref())?;
    let scope_header = request_header(session, SCOPE_TOKEN);
    verify_optional_scope(runtime, scope_header.as_deref()).await?;
    let mut effective_service_ids = normalized_service_ids(service_ids);
    if effective_service_ids.is_empty()
        && let Some(service_id) = runtime.service_id_for_request(
            request_header(session, SERVICE_ID_HEADER).as_deref(),
            request_path,
        )
    {
        effective_service_ids.push(service_id);
    }
    let principal = verify_jwt_token_for_services(
        runtime,
        &token,
        JwtExpiryMode::Enforce,
        &effective_service_ids,
    )
    .await?;
    if request_path.starts_with("/mcp") {
        validate_verified_purpose(token, &principal, TokenUse::User, &[])?;
    }
    apply_pass_through_claims(session, config, &principal)?;
    Ok(Some(principal))
}

fn required_authorization_bearer(value: Option<&str>) -> Result<&str, HandlerRejection> {
    value.and_then(parse_bearer).ok_or_else(|| {
        HandlerRejection::unauthorized("user Authorization bearer token is required")
    })
}

async fn verify_optional_scope(
    runtime: &SecurityRuntime,
    header: Option<&str>,
) -> Result<(), HandlerRejection> {
    if let Some(header) = header {
        let token = parse_bearer(header)
            .ok_or_else(|| HandlerRejection::unauthorized("invalid X-Scope-Token bearer token"))?;
        // Preserve signed legacy scope credentials without a purpose marker,
        // but never accept an explicit user (or malformed) purpose as app scope.
        let principal = verify_jwt_token(runtime, token, JwtExpiryMode::Enforce).await?;
        if principal.claims.get("token_use").is_some() {
            validate_verified_purpose(token, &principal, TokenUse::App, &[])?;
        }
    }
    Ok(())
}

fn parse_bearer(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|token| !token.is_empty())
}

fn normalized_service_ids(service_ids: &[String]) -> Vec<String> {
    service_ids
        .iter()
        .map(|service_id| service_id.trim())
        .filter(|service_id| !service_id.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn apply_pass_through_claims(
    session: &mut Session,
    config: &SecurityConfig,
    principal: &AuthPrincipal,
) -> Result<(), HandlerRejection> {
    for (claim_name, header_name) in &config.pass_through_claims {
        let Some(value) = claim_string(&principal.claims, claim_name) else {
            continue;
        };
        session
            .req_header_mut()
            .insert_header(header_name.to_string(), value)
            .map_err(|_| HandlerRejection::new(500, "ERR10001", "invalid pass-through header"))?;
    }
    Ok(())
}

fn claim_string(claims: &serde_json::Value, name: &str) -> Option<String> {
    let value = claims.get(name)?;
    value
        .as_str()
        .map(ToOwned::to_owned)
        .or_else(|| (value.is_number() || value.is_boolean()).then(|| value.to_string()))
}

fn request_path_is_skipped(config: &SecurityConfig, request_path: &str) -> bool {
    config
        .skip_path_prefixes
        .iter()
        .any(|prefix| request_path.starts_with(prefix))
}

fn is_h2c_upgrade(session: &Session) -> bool {
    let Some(upgrade) = request_header(session, "upgrade") else {
        return false;
    };
    upgrade.eq_ignore_ascii_case("h2c")
        && request_header(session, "connection")
            .is_some_and(|value| value.to_ascii_lowercase().contains("upgrade"))
}

fn mock_principal() -> AuthPrincipal {
    AuthPrincipal {
        client_id: Some("mock-client".into()),
        user_id: Some("mock-user".into()),
        issuer: Some("mock".into()),
        claims: serde_json::json!({
            "client_id": "mock-client",
            "user_id": "mock-user",
            "iss": "mock"
        }),
        ..AuthPrincipal::default()
    }
}

fn non_empty(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const KEY: &[u8] = b"gateway-dual-token-test-signing-key";

    fn signed(purpose: &str) -> String {
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
        header.kid = Some("gateway-dual".into());
        jsonwebtoken::encode(
            &header,
            &json!({"iss":"gateway-dual","aud":"workflow","exp":4102444800u64,
                "token_use":purpose,"sub":"caller"}),
            &jsonwebtoken::EncodingKey::from_secret(KEY),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn user_authorization_is_required_and_optional_scope_is_verified() {
        let runtime = SecurityRuntime::with_test_hs256_key("gateway-dual", KEY).await;
        let user = signed("user");
        let app = signed("app");
        assert!(required_authorization_bearer(None).is_err());
        assert!(required_authorization_bearer(Some("Bearer ")).is_err());
        assert_eq!(
            required_authorization_bearer(Some(&format!("Bearer {user}"))).unwrap(),
            user
        );
        assert!(verify_optional_scope(&runtime, None).await.is_ok());
        assert!(
            verify_optional_scope(&runtime, Some(&format!("Bearer {app}")))
                .await
                .is_ok()
        );
        assert!(
            verify_optional_scope(&runtime, Some(&format!("Bearer {user}")))
                .await
                .is_err()
        );
        assert!(
            verify_optional_scope(&runtime, Some("Bearer invalid"))
                .await
                .is_err()
        );
        assert!(verify_optional_scope(&runtime, Some(&app)).await.is_err());
        let app_principal = verify_jwt_token(&runtime, &app, JwtExpiryMode::Enforce)
            .await
            .unwrap();
        assert!(validate_verified_purpose(&app, &app_principal, TokenUse::User, &[]).is_err());
        for purpose in [json!(null), json!("invalid"), json!(false), json!([])] {
            let token = signed_scope_claims(
                json!({"iss":"gateway-dual","exp":4102444800u64,"token_use":purpose}),
                KEY,
            );
            assert!(
                verify_optional_scope(&runtime, Some(&format!("Bearer {token}")))
                    .await
                    .is_err()
            );
        }
    }

    fn signed_scope_claims(claims: serde_json::Value, key: &[u8]) -> String {
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
        header.kid = Some("gateway-dual".into());
        jsonwebtoken::encode(
            &header,
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(key),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn legacy_scope_without_purpose_is_verified() {
        let runtime = SecurityRuntime::with_test_hs256_key("gateway-dual", KEY).await;
        let token = signed_scope_claims(
            json!({"iss":"gateway-dual","aud":"workflow","exp":4102444800u64,
                "sub":"workflow-service","scope":"portal.r portal.w"}),
            KEY,
        );
        assert!(
            verify_optional_scope(&runtime, Some(&format!("Bearer {token}")))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn scope_without_purpose_still_rejects_bad_signature() {
        let runtime = SecurityRuntime::with_test_hs256_key("gateway-dual", KEY).await;
        let token = signed_scope_claims(
            json!({"iss":"gateway-dual","exp":4102444800u64}),
            b"different-untrusted-test-signing-key",
        );
        assert!(
            verify_optional_scope(&runtime, Some(&format!("Bearer {token}")))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn scope_without_purpose_still_rejects_expiry() {
        let runtime = SecurityRuntime::with_test_hs256_key("gateway-dual", KEY).await;
        let token = signed_scope_claims(json!({"iss":"gateway-dual","exp":1u64}), KEY);
        assert!(
            verify_optional_scope(&runtime, Some(&format!("Bearer {token}")))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn scope_without_purpose_still_rejects_wrong_issuer() {
        let mut runtime = SecurityRuntime::with_test_hs256_key("gateway-dual", KEY).await;
        runtime.config.issuer = "gateway-dual".into();
        let token = signed_scope_claims(json!({"iss":"untrusted","exp":4102444800u64}), KEY);
        assert!(
            verify_optional_scope(&runtime, Some(&format!("Bearer {token}")))
                .await
                .is_err()
        );
    }
}
