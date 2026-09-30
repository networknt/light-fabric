//! Opt-in end-user provenance for accepted development input profiles.
//! Verified user credentials are trusted for their valid lifetime.
use light_security::{
    SecurityRuntime,
    token_purpose::{TokenUse, verify_with_purpose},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

pub(crate) fn denied() -> Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "verified caller authority denied",
    )
    .into()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VerifiedUser {
    pub host_id: Uuid,
    pub user_id: Uuid,
    pub issuer: String,
    pub principal: String,
    pub purpose: String,
    pub claims_digest: String,
    pub expires_at: i64,
}

pub fn accepted_profile(definition: &Value) -> Result<Option<&str>, Error> {
    let value = definition
        .pointer("/document/metadata/developmentInputProfile")
        .or_else(|| definition.pointer("/metadata/developmentInputProfile"));
    match value {
        None => Ok(None),
        Some(Value::String(profile))
            if matches!(profile.as_str(), "capture-v1" | "supplied-input-v2") =>
        {
            Ok(Some(profile))
        }
        _ => Err(denied()),
    }
}

pub async fn verify_user(
    runtime: &SecurityRuntime,
    exact_token: &str,
    host: Uuid,
) -> Result<VerifiedUser, Error> {
    if !runtime.config.enable_verify_jwt || runtime.config.enable_mock_jwt {
        return Err(denied());
    }
    let principal = verify_with_purpose(runtime, exact_token, TokenUse::User, &[])
        .await
        .map_err(|_| denied())?;
    let claims = &principal.claims;
    if claims.get("grant_type").and_then(Value::as_str) == Some("client_credentials")
        || claims.get("userType").and_then(Value::as_str) == Some("F")
    {
        return Err(denied());
    }
    // Do not use AuthPrincipal's normalized/fallback user identity here.
    let snake = claims.get("user_id");
    let camel = claims.get("userId");
    if snake.is_some() && camel.is_some() && snake != camel {
        return Err(denied());
    }
    let user = snake
        .or(camel)
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<Uuid>().ok())
        .filter(|id| !id.is_nil())
        .ok_or_else(denied)?;
    let token_host = principal
        .host
        .as_deref()
        .or_else(|| claims.get("hostId").and_then(Value::as_str))
        .or_else(|| claims.get("host_id").and_then(Value::as_str))
        .and_then(|s| s.parse::<Uuid>().ok());
    if token_host != Some(host) || host.is_nil() {
        return Err(denied());
    }
    let subject = principal
        .client_id
        .as_deref()
        .or_else(|| claims.get("sub").and_then(Value::as_str))
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(denied)?;
    let issuer = principal
        .issuer
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(denied)?;
    let expires_at = claims
        .get("exp")
        .and_then(Value::as_i64)
        .filter(|exp| *exp > chrono::Utc::now().timestamp())
        .ok_or_else(denied)?;
    Ok(VerifiedUser {
        host_id: host,
        user_id: user,
        issuer: issuer.to_owned(),
        principal: subject.to_owned(),
        purpose: "user".into(),
        expires_at,
        claims_digest: workflow_invocation_contract::canonical_sha256(
            &workflow_invocation_contract::stable_subject_claims(claims),
        )?,
    })
}
