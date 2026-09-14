//! Route-specific A2 user/app/transport contracts. Signature verification and
//! purpose checks never replace the receiving route's user policy/ACL check.
use crate::{
    AuthPrincipal, HandlerRejection, SecurityRuntime,
    token_purpose::{LegacyLongLivedAppKey, TokenUse, verify_with_purpose},
};
use http::HeaderMap;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Origin {
    Interactive,
    Workflow,
    Gateway,
    Receiver,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AppProfile {
    pub origin: Origin,
    /// Exact SHA-256 leaf fingerprints from administrator-approved mTLS peers.
    /// The TLS acceptor must have verified certificate chain, usage and validity.
    pub peer_sha256: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RoutePolicy {
    pub issuer: String,
    pub audience: String,
    pub host_id: Uuid,
    pub apps: BTreeMap<String, AppProfile>,
    #[serde(default)]
    pub legacy_long_lived_app_keys: Vec<LegacyLongLivedAppKey>,
}
#[derive(Debug)]
pub struct Identities {
    pub user: AuthPrincipal,
    pub app: AuthPrincipal,
    pub service_id: String,
    pub origin: Origin,
    pub action_reference: Option<Uuid>,
}
fn denied() -> HandlerRejection {
    HandlerRejection::forbidden("user/application route contract denied")
}
impl RoutePolicy {
    pub fn validate(&self) -> Result<(), HandlerRejection> {
        if self.issuer.is_empty()
            || self.audience.is_empty()
            || self.host_id.is_nil()
            || self.apps.is_empty()
            || self.apps.iter().any(|(sid, p)| {
                sid.is_empty()
                    || p.peer_sha256.is_empty()
                    || p.peer_sha256.iter().any(|s| {
                        s.len() != 64
                            || !s
                                .bytes()
                                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
                    })
            })
        {
            return Err(denied());
        }
        Ok(())
    }
}
/// Extract before flattening headers; comma-joined, repeated, empty and malformed
/// credentials cannot be silently converted into a single accepted token.
pub fn bearer<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, HandlerRejection> {
    let values = headers.get_all(name);
    if values.iter().count() != 1 {
        return Err(HandlerRejection::unauthorized(
            "exactly one credential is required",
        ));
    }
    let value = values
        .iter()
        .next()
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|v| !v.is_empty() && !v.bytes().any(|c| c.is_ascii_whitespace() || c == b','))
        .ok_or_else(|| HandlerRejection::unauthorized("malformed bearer credential"))?;
    Ok(value)
}
fn action_reference(headers: &HeaderMap) -> Result<Option<Uuid>, HandlerRejection> {
    let values = headers.get_all("x-workflow-action");
    match values.iter().count() {
        0 => Ok(None),
        1 => values
            .iter()
            .next()
            .and_then(|v| v.to_str().ok())
            .and_then(|v| Uuid::parse_str(v).ok())
            .filter(|v| !v.is_nil())
            .map(Some)
            .ok_or_else(denied),
        _ => Err(denied()),
    }
}
fn check_claims(
    principal: &AuthPrincipal,
    policy: &RoutePolicy,
    user: bool,
) -> Result<(), HandlerRejection> {
    let c = &principal.claims;
    let audience = c.get("aud").is_some_and(|a| {
        a.as_str() == Some(&policy.audience)
            || a.as_array()
                .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(&policy.audience)))
    });
    if principal.issuer.as_deref() != Some(&policy.issuer) || !audience {
        return Err(denied());
    }
    if user
        && (principal
            .host
            .as_deref()
            .and_then(|s| s.parse::<Uuid>().ok())
            != Some(policy.host_id)
            || principal
                .user_id
                .as_deref()
                .and_then(|s| s.parse::<Uuid>().ok())
                .is_none_or(|u| u.is_nil()))
    {
        return Err(denied());
    }
    Ok(())
}
/// `tls_peer_sha256` comes ONLY from verified TLS connection context, never a
/// header, body or forwarded peer assertion. A missing transport peer fails closed.
/// Authentication of the action reference still requires Workflow's online API.
pub async fn authenticate(
    runtime: &SecurityRuntime,
    policy: &RoutePolicy,
    headers: &HeaderMap,
    tls_peer_sha256: Option<&str>,
) -> Result<Identities, HandlerRejection> {
    policy.validate()?;
    // Existing permissive runtime profiles must not weaken this explicitly strict
    // route, including via an already cached principal.
    if runtime.config.ignore_jwt_expiry
        || runtime.config.enable_mock_jwt
        || !runtime.config.enable_verify_jwt
    {
        return Err(denied());
    }
    // Reject ordinary listeners before any issuer/JWKS work. A forwarded peer
    // header never creates this connection extension.
    let peer = tls_peer_sha256.ok_or_else(denied)?;
    let reference = action_reference(headers)?;
    let app = verify_with_purpose(
        runtime,
        bearer(headers, "x-scope-token")?,
        TokenUse::App,
        &policy.legacy_long_lived_app_keys,
    )
    .await?;
    let user = verify_with_purpose(
        runtime,
        bearer(headers, "authorization")?,
        TokenUse::User,
        &[],
    )
    .await?;
    check_claims(&user, policy, true)?;
    check_claims(&app, policy, false)?;
    let sid = app
        .claims
        .get("sid")
        .and_then(|s| s.as_str())
        .ok_or_else(denied)?
        .to_owned();
    let profile = policy.apps.get(&sid).ok_or_else(denied)?;
    if !profile.peer_sha256.iter().any(|p| p == peer)
        || (matches!(profile.origin, Origin::Workflow | Origin::Receiver) && reference.is_none())
    {
        return Err(denied());
    }
    Ok(Identities {
        user,
        app,
        service_id: sid,
        origin: profile.origin,
        action_reference: reference,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reject_duplicate_and_coalesced_credentials_before_verification() {
        let mut h = HeaderMap::new();
        h.append("authorization", "Bearer a".parse().unwrap());
        assert_eq!(bearer(&h, "authorization").unwrap(), "a");
        h.append("authorization", "Bearer b".parse().unwrap());
        assert!(bearer(&h, "authorization").is_err());
        for v in ["Bearer a, Bearer b", "Bearer ", "Basic x", "Bearer a b"] {
            h.clear();
            h.insert("authorization", v.parse().unwrap());
            assert!(bearer(&h, "authorization").is_err());
        }
    }
    #[test]
    fn duplicate_or_nil_action_references_are_never_roots() {
        let mut h = HeaderMap::new();
        assert_eq!(action_reference(&h).unwrap(), None);
        h.insert(
            "x-workflow-action",
            Uuid::nil().to_string().parse().unwrap(),
        );
        assert!(action_reference(&h).is_err());
        h.insert(
            "x-workflow-action",
            Uuid::now_v7().to_string().parse().unwrap(),
        );
        assert!(action_reference(&h).unwrap().is_some());
        h.append(
            "x-workflow-action",
            Uuid::now_v7().to_string().parse().unwrap(),
        );
        assert!(action_reference(&h).is_err());
    }
}

/// Service-only evidence reporting. This profile cannot disclose user results or
/// authorize target work; its callers must expose only fixed completion routes.
pub async fn authenticate_application(
    runtime: &SecurityRuntime,
    policy: &RoutePolicy,
    headers: &HeaderMap,
    tls_peer_sha256: Option<&str>,
) -> Result<(AuthPrincipal, String, Origin), HandlerRejection> {
    policy.validate()?;
    if runtime.config.ignore_jwt_expiry
        || runtime.config.enable_mock_jwt
        || !runtime.config.enable_verify_jwt
    {
        return Err(denied());
    }
    let peer = tls_peer_sha256.ok_or_else(denied)?;
    let app = verify_with_purpose(
        runtime,
        bearer(headers, "x-scope-token")?,
        TokenUse::App,
        &policy.legacy_long_lived_app_keys,
    )
    .await?;
    check_claims(&app, policy, false)?;
    let sid = app
        .claims
        .get("sid")
        .and_then(|s| s.as_str())
        .ok_or_else(denied)?
        .to_owned();
    let profile = policy.apps.get(&sid).ok_or_else(denied)?;
    if !profile.peer_sha256.iter().any(|p| p == peer) {
        return Err(denied());
    }
    Ok((app, sid, profile.origin))
}
