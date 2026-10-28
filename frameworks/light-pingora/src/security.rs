use crate::policy_adapter::PolicyRequest;
pub use light_security::{
    AuthPrincipal, HandlerRejection, JwtExpiryMode, SECURITY_CONFIG_NAME, SECURITY_FILE,
    SECURITY_MODULE_ID, SecurityConfig, SecurityJwtConfig, SecurityRuntime, load_security_runtime,
    load_security_runtime_from_file, verify_jwt_token,
};
use pingora::prelude::Session;
pub async fn verify_jwt_request(
    session: &mut Session,
    runtime: &SecurityRuntime,
    request_path: &str,
) -> Result<Option<AuthPrincipal>, HandlerRejection> {
    gateway_policy::security::verify_jwt_request(&mut PolicyRequest(session), runtime, request_path)
        .await
}
pub async fn verify_jwt_request_with_service_id_override(
    session: &mut Session,
    runtime: &SecurityRuntime,
    request_path: &str,
    service_id_override: Option<&str>,
) -> Result<Option<AuthPrincipal>, HandlerRejection> {
    gateway_policy::security::verify_jwt_request_with_service_id_override(
        &mut PolicyRequest(session),
        runtime,
        request_path,
        service_id_override,
    )
    .await
}
pub async fn verify_jwt_request_with_service_ids(
    session: &mut Session,
    runtime: &SecurityRuntime,
    request_path: &str,
    service_ids: &[String],
) -> Result<Option<AuthPrincipal>, HandlerRejection> {
    gateway_policy::security::verify_jwt_request_with_service_ids(
        &mut PolicyRequest(session),
        runtime,
        request_path,
        service_ids,
    )
    .await
}
#[cfg(test)]
use gateway_policy::security::{required_authorization_bearer, verify_optional_scope};
#[cfg(test)]
use light_security::token_purpose::{TokenUse, validate_verified_purpose};
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
