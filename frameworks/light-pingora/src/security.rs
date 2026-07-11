use crate::config_util::request_header;
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

    let token = bearer_token(session).or_else(|| {
        config
            .enable_extract_scope_token
            .then(|| request_header(session, SCOPE_TOKEN))
            .flatten()
    });
    let token = token.ok_or_else(|| HandlerRejection::unauthorized("missing bearer token"))?;
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
    apply_pass_through_claims(session, config, &principal)?;
    Ok(Some(principal))
}

fn bearer_token(session: &Session) -> Option<String> {
    let value = request_header(session, AUTHORIZATION)?;
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim().to_string())
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
