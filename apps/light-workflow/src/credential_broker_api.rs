//! Enrollment returns references only. Runtime token acquisition is internal.
use crate::credential_broker::{CredentialBroker, EnrollmentChallenge};
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
};
use light_security::{
    SecurityRuntime,
    token_purpose::{TokenUse, verify_with_purpose},
};
use serde::Deserialize;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Clone)]
struct ApiState {
    broker: Arc<CredentialBroker>,
    security: Arc<SecurityRuntime>,
    callback: String,
    callers: Vec<String>,
    legacy_app_keys: Vec<light_security::token_purpose::LegacyLongLivedAppKey>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Callback {
    state: String,
    code: String,
}

async fn callback(
    State(broker): State<Arc<CredentialBroker>>,
    axum::extract::Query(request): axum::extract::Query<Callback>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let (status,message)=match broker.complete_browser_callback(&request.state,&request.code).await {
        Ok(id)=>(StatusCode::OK,format!("<h1>Workflow authorized</h1><p>Grant reference: <code>{id}</code></p><p>You can close this page.</p>")),
        Err(crate::credential_broker::BrokerError::Retryable)=>(StatusCode::SERVICE_UNAVAILABLE,"<h1>Issuer temporarily unavailable</h1><p>No token request was sent. Retry this page shortly.</p>".into()),
        Err(_)=>(StatusCode::CONFLICT,"<h1>Authorization could not be completed</h1><p>Return to the workflow and start authorization again.</p>".into()),
    };
    (status,[("cache-control","no-store"),("referrer-policy","no-referrer"),
        ("content-security-policy","default-src 'none'; base-uri 'none'; frame-ancestors 'none'")],
        axum::response::Html(format!("<!doctype html><html lang=en><meta charset=utf-8><title>Workflow authorization</title><main>{message}</main></html>"))).into_response()
}

/// The browser callback authenticates the stored OAuth state and backend PKCE,
/// without requiring the original (potentially expired) browser access token.
pub fn callback_router(broker: Arc<CredentialBroker>) -> Router {
    Router::new()
        .route(
            "/workflow/credentials/callback",
            axum::routing::get(callback),
        )
        .with_state(broker)
}

/// Bind during startup so a bad certificate or occupied port fails readiness.
pub async fn prepare_callback_listener(
    config: &crate::credential_broker::CallbackTls,
    dir: &std::path::Path,
    broker: Arc<CredentialBroker>,
) -> std::io::Result<impl std::future::Future<Output = std::io::Result<()>> + Send + 'static> {
    let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(
        dir.join(&config.certificate_file),
        dir.join(&config.private_key_file),
    )
    .await?;
    let socket = std::net::TcpListener::bind(config.address)?;
    socket.set_nonblocking(true)?;
    Ok(async move {
        axum_server::from_tcp_rustls(socket, tls)
            .serve(callback_router(broker).into_make_service())
            .await
    })
}

pub fn router(
    broker: Arc<CredentialBroker>,
    security: Arc<SecurityRuntime>,
    callback: String,
    callers: Vec<String>,
    legacy_app_keys: Vec<light_security::token_purpose::LegacyLongLivedAppKey>,
) -> Router {
    let browser_callback = callback_router(broker.clone());
    Router::new()
        .route("/workflow/credentials/enroll", post(enroll))
        .route("/workflow/credentials/complete", post(complete))
        .route("/workflow/credentials/revoke", post(revoke))
        .with_state(ApiState {
            broker,
            security,
            callback,
            callers,
            legacy_app_keys,
        })
        .merge(browser_callback)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Revoke {
    grant_id: Uuid,
}

async fn revoke(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(request): Json<Revoke>,
) -> Result<StatusCode, StatusCode> {
    let (_, host, user) = identity(&state, &headers).await?;
    state
        .broker
        .revoke_grant(request.grant_id, host, user)
        .await
        .map_err(|_| StatusCode::CONFLICT)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn identity(
    state: &ApiState,
    headers: &HeaderMap,
) -> Result<(String, Uuid, Uuid), StatusCode> {
    fn bearer<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, StatusCode> {
        if headers.get_all(name).iter().count() != 1 {
            return Err(StatusCode::UNAUTHORIZED);
        }
        headers
            .get(name)
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
            .filter(|s| !s.is_empty())
            .ok_or(StatusCode::UNAUTHORIZED)
    }
    let app = verify_with_purpose(
        &state.security,
        bearer(headers, "x-scope-token")?,
        TokenUse::App,
        &state.legacy_app_keys,
    )
    .await
    .map_err(|_| StatusCode::UNAUTHORIZED)?;
    let sid = app
        .claims
        .get("sid")
        .and_then(serde_json::Value::as_str)
        .ok_or(StatusCode::FORBIDDEN)?;
    if !state.callers.iter().any(|c| c == sid) {
        return Err(StatusCode::FORBIDDEN);
    }
    let token = bearer(headers, "authorization")?;
    let user = verify_with_purpose(&state.security, token, TokenUse::User, &[])
        .await
        .map_err(|_| StatusCode::UNAUTHORIZED)?;
    let host = user
        .host
        .as_deref()
        .and_then(|s| s.parse::<Uuid>().ok())
        .ok_or(StatusCode::FORBIDDEN)?;
    let uid = user
        .user_id
        .as_deref()
        .and_then(|s| s.parse::<Uuid>().ok())
        .ok_or(StatusCode::FORBIDDEN)?;
    Ok((format!("Bearer {token}"), host, uid))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Enrollment {
    scope: String,
    binding: serde_json::Value,
    expires_at: chrono::DateTime<chrono::Utc>,
}
async fn enroll(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(request): Json<Enrollment>,
) -> Result<Json<EnrollmentChallenge>, StatusCode> {
    let (authorization, host, user) = identity(&state, &headers).await?;
    state
        .broker
        .begin_enrollment(
            &authorization,
            host,
            user,
            &state.callback,
            &request.scope,
            request.binding,
            request.expires_at,
        )
        .await
        .map(Json)
        .map_err(|_| StatusCode::CONFLICT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Completion {
    state: String,
    code: String,
}
async fn complete(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(request): Json<Completion>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let (_, host, user) = identity(&state, &headers).await?;
    let grant = state
        .broker
        .complete_enrollment(&request.state, &request.code, host, user)
        .await
        .map_err(|error| match error {
            crate::credential_broker::BrokerError::Retryable
            | crate::credential_broker::BrokerError::Store => StatusCode::SERVICE_UNAVAILABLE,
            _ => StatusCode::CONFLICT,
        })?;
    Ok(Json(serde_json::json!({"grantId":grant})))
}
