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
    /// Empty when this profile trusts peers by `ca_trust` instead; never
    /// weakened or removed by the presence of `ca_trust` on the same profile.
    #[serde(default)]
    pub peer_sha256: Vec<String>,
    /// CA-chain-plus-attribute trust, additive to `peer_sha256`. A peer
    /// matches if its certificate's immediate issuer digest equals
    /// `issuer_sha256`, its subject organization names the configured environment, and its
    /// `spiffe://`-shaped URI SAN names the role and app token's service ID. This is how a fleet
    /// the size of light-identity-issuer's issued population is trusted
    /// without growing this policy one fingerprint per install. See
    /// `docs/src/design/workload-identity-issuance.md`.
    #[serde(default)]
    pub ca_trust: Option<CaTrust>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CaTrust {
    /// SHA-256 digest of the CA certificate that must have directly signed
    /// the peer's leaf, hex-encoded like `peer_sha256`.
    pub issuer_sha256: String,
    /// Environment tag in the leaf subject's `O=` attribute, e.g. `prod`.
    pub environment: String,
    /// The role segment a trusted leaf's URI SAN must carry, e.g. `"cli"`.
    pub role: String,
}
impl CaTrust {
    fn matches(
        &self,
        issuer_sha256: Option<&str>,
        environment: Option<&str>,
        uri_san: Option<&str>,
        service_id: &str,
    ) -> bool {
        issuer_sha256 == Some(self.issuer_sha256.as_str())
            && environment == Some(self.environment.as_str())
            && uri_san.is_some_and(|uri| {
                let Some(rest) = uri.strip_prefix("spiffe://lightapi.local/") else {
                    return false;
                };
                let mut segments = rest.split('/');
                segments.next() == Some(self.role.as_str())
                    && segments.next() == Some(service_id)
                    && segments
                        .next()
                        .is_some_and(|install_id| !install_id.is_empty())
                    && segments.next().is_none()
            })
    }
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
    /// Off by default. When on, a caller that presents **no application credential** at all
    /// (no `x-scope-token`) is admitted on its user token alone, as an interactive caller with
    /// no action reference. This is for open, downloadable clients such as the Light CLI, which
    /// cannot keep a secret or a certificate: what such a caller may do is decided by the
    /// user's own roles and the route's ACL, never by a claim about which program it is.
    ///
    /// It never weakens the strict contract for anyone who does present an application
    /// credential: a request with `x-scope-token` is authenticated exactly as before. A request
    /// with an action reference (`x-workflow-action`) but no application credential is refused,
    /// so a Workflow-origin claim cannot be made without an application identity.
    #[serde(default)]
    pub interactive_user_only: bool,
}
/// Which contract a request is to be authenticated under on a given route.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallerKind {
    /// Application credential, verified transport peer and user token: [`authenticate`].
    Application,
    /// The user token alone: [`authenticate_user`].
    UserOnly,
}
/// Decide which contract applies. Only a request with no `x-scope-token` header at all can be
/// [`CallerKind::UserOnly`], and only when the route policy allows it; a header that is present
/// but empty or repeated is an application attempt and is judged (and refused) as one.
pub fn caller_kind(
    policy: &RoutePolicy,
    headers: &HeaderMap,
) -> Result<CallerKind, HandlerRejection> {
    if headers.contains_key("x-scope-token") || !policy.interactive_user_only {
        return Ok(CallerKind::Application);
    }
    if headers.contains_key("x-workflow-action") {
        return Err(denied());
    }
    Ok(CallerKind::UserOnly)
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
                fn is_sha256_hex(s: &str) -> bool {
                    s.len() == 64
                        && s.bytes()
                            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
                }
                sid.is_empty()
                    || (p.peer_sha256.is_empty() && p.ca_trust.is_none())
                    || p.peer_sha256.iter().any(|s| !is_sha256_hex(s))
                    || p.ca_trust.as_ref().is_some_and(|ca| {
                        !is_sha256_hex(&ca.issuer_sha256)
                            || ca.environment.is_empty()
                            || ca.role.is_empty()
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
/// Everything a caller can assert about the transport peer, sourced ONLY
/// from verified TLS connection context, never a header, body or forwarded
/// peer assertion. `sha256` alone is the existing exact-leaf path;
/// `issuer_sha256`/`environment`/`uri_san` are additionally required for the CA-based
/// path (`AppProfile::ca_trust`). Existing callers that only ever had a
/// leaf fingerprint keep compiling unchanged via `From<Option<&str>>`.
#[derive(Clone, Copy, Debug, Default)]
pub struct TlsPeer<'a> {
    pub sha256: Option<&'a str>,
    pub issuer_sha256: Option<&'a str>,
    pub environment: Option<&'a str>,
    pub uri_san: Option<&'a str>,
}
impl<'a> From<Option<&'a str>> for TlsPeer<'a> {
    fn from(sha256: Option<&'a str>) -> Self {
        Self {
            sha256,
            issuer_sha256: None,
            environment: None,
            uri_san: None,
        }
    }
}
impl<'a> From<Option<&'a String>> for TlsPeer<'a> {
    fn from(sha256: Option<&'a String>) -> Self {
        sha256.map(String::as_str).into()
    }
}
fn profile_trusts_peer(profile: &AppProfile, peer: &TlsPeer<'_>, service_id: &str) -> bool {
    peer.sha256
        .is_some_and(|sha| profile.peer_sha256.iter().any(|p| p == sha))
        || profile.ca_trust.as_ref().is_some_and(|ca| {
            ca.matches(
                peer.issuer_sha256,
                peer.environment,
                peer.uri_san,
                service_id,
            )
        })
}
/// Authenticate a caller that has no application identity, on the user token alone. Only for
/// a route whose policy sets [`RoutePolicy::interactive_user_only`]; anything else is refused.
///
/// The user token must be a verified **user** token (an application token is refused) for
/// this route's issuer, audience and host. The same strictness as [`authenticate`] applies to
/// the runtime: permissive profiles (expiry ignored, mock JWTs, verification off) are refused.
pub async fn authenticate_user(
    runtime: &SecurityRuntime,
    policy: &RoutePolicy,
    headers: &HeaderMap,
) -> Result<AuthPrincipal, HandlerRejection> {
    policy.validate()?;
    if !policy.interactive_user_only
        || runtime.config.ignore_jwt_expiry
        || runtime.config.enable_mock_jwt
        || !runtime.config.enable_verify_jwt
        || headers.contains_key("x-scope-token")
        || headers.contains_key("x-workflow-action")
    {
        return Err(denied());
    }
    let user = verify_with_purpose(
        runtime,
        bearer(headers, "authorization")?,
        TokenUse::User,
        &[],
    )
    .await?;
    check_claims(&user, policy, true)?;
    Ok(user)
}
/// Who was admitted, and under which contract.
#[derive(Debug)]
pub enum Admission {
    /// The strict contract: application credential, verified peer and user token.
    Application(Box<Identities>),
    /// The user token alone, on a route that allows it: interactive, no action reference.
    UserOnly(AuthPrincipal),
}
/// Authenticate a request under whichever contract applies to it: [`caller_kind`] decides, then
/// [`authenticate`] or [`authenticate_user`] does the work. This is the one entry point a
/// gateway route should use.
pub async fn admit<'a>(
    runtime: &SecurityRuntime,
    policy: &RoutePolicy,
    headers: &HeaderMap,
    tls_peer: impl Into<TlsPeer<'a>>,
) -> Result<Admission, HandlerRejection> {
    match caller_kind(policy, headers)? {
        CallerKind::UserOnly => authenticate_user(runtime, policy, headers)
            .await
            .map(Admission::UserOnly),
        CallerKind::Application => authenticate(runtime, policy, headers, tls_peer)
            .await
            .map(|identity| Admission::Application(Box::new(identity))),
    }
}
/// Authentication of the action reference still requires Workflow's online API.
pub async fn authenticate<'a>(
    runtime: &SecurityRuntime,
    policy: &RoutePolicy,
    headers: &HeaderMap,
    tls_peer: impl Into<TlsPeer<'a>>,
) -> Result<Identities, HandlerRejection> {
    let tls_peer = tls_peer.into();
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
    if tls_peer.sha256.is_none() && tls_peer.issuer_sha256.is_none() {
        return Err(denied());
    }
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
    if !profile_trusts_peer(profile, &tls_peer, &sid)
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
    fn ca_trust_profile() -> AppProfile {
        AppProfile {
            origin: Origin::Interactive,
            peer_sha256: Vec::new(),
            ca_trust: Some(CaTrust {
                issuer_sha256: "a".repeat(64),
                environment: "dev".to_string(),
                role: "cli".to_string(),
            }),
        }
    }
    fn policy(interactive_user_only: bool) -> RoutePolicy {
        RoutePolicy {
            issuer: "issuer".into(),
            audience: "audience".into(),
            host_id: Uuid::from_u128(7),
            apps: BTreeMap::from([("com.networknt.cli.dev-1.0.0".into(), ca_trust_profile())]),
            legacy_long_lived_app_keys: Vec::new(),
            interactive_user_only,
        }
    }
    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (name, value) in pairs {
            h.append(
                http::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        h
    }
    #[test]
    fn user_only_is_off_unless_the_route_policy_turns_it_on() {
        for pairs in [
            vec![],
            vec![("authorization", "Bearer u")],
            vec![("x-scope-token", "Bearer a"), ("authorization", "Bearer u")],
            vec![("x-workflow-action", "00000000-0000-0000-0000-000000000001")],
        ] {
            assert_eq!(
                caller_kind(&policy(false), &headers(&pairs)).unwrap(),
                CallerKind::Application,
                "{pairs:?}"
            );
        }
        // And the default of a policy read from configuration is off.
        let read: RoutePolicy = serde_json::from_value(serde_json::json!({
            "issuer": "i", "audience": "a", "hostId": Uuid::from_u128(7),
            "apps": {"w": {"origin": "workflow", "peerSha256": ["a".repeat(64)]}},
        }))
        .unwrap();
        assert!(!read.interactive_user_only);
    }
    #[test]
    fn with_it_on_only_a_request_with_no_application_credential_is_user_only() {
        let on = policy(true);
        assert_eq!(
            caller_kind(&on, &headers(&[("authorization", "Bearer u")])).unwrap(),
            CallerKind::UserOnly
        );
        // Any application credential, even a useless one, keeps the strict contract.
        for value in ["Bearer a", "", "garbage"] {
            assert_eq!(
                caller_kind(
                    &on,
                    &headers(&[("x-scope-token", value), ("authorization", "Bearer u")])
                )
                .unwrap(),
                CallerKind::Application,
                "x-scope-token {value:?}"
            );
        }
        assert_eq!(
            caller_kind(
                &on,
                &headers(&[("x-scope-token", "a"), ("x-scope-token", "b")])
            )
            .unwrap(),
            CallerKind::Application,
            "a repeated credential is an application attempt, refused later"
        );
    }
    #[test]
    fn an_action_reference_cannot_be_claimed_without_an_application_identity() {
        let on = policy(true);
        let action = "00000000-0000-0000-0000-000000000001";
        assert!(
            caller_kind(
                &on,
                &headers(&[("authorization", "Bearer u"), ("x-workflow-action", action)])
            )
            .is_err()
        );
        // With an application credential the reference is judged by the strict contract, as before.
        assert_eq!(
            caller_kind(
                &on,
                &headers(&[("x-scope-token", "Bearer a"), ("x-workflow-action", action)])
            )
            .unwrap(),
            CallerKind::Application
        );
    }
    // A runtime that verifies HS256 tokens signed with this key, for the user-only tests.
    const KEY: &[u8] = b"user-only-contract-test-key-32-bytes!!";
    async fn runtime(permissive: bool) -> SecurityRuntime {
        use base64::Engine;
        let client = crate::ClientTokenConfig::default();
        let mut config = crate::SecurityConfig::default();
        config.enable_verify_jwt = true;
        config.ignore_jwt_expiry = permissive;
        let runtime = SecurityRuntime {
            config,
            jwk_source: Some(std::sync::Arc::new(crate::JwkSource {
                request: client.request.clone(),
                tls: client.tls.clone(),
                client,
                direct_registry: Default::default(),
                registry_client: None,
            })),
            jwks: Default::default(),
            cache: Default::default(),
        };
        let jwk: jsonwebtoken::jwk::Jwk = serde_json::from_value(serde_json::json!({
            "kty": "oct", "kid": "k1", "alg": "HS256",
            "k": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(KEY),
        }))
        .unwrap();
        runtime.jwks.write().await.insert("k1".into(), jwk);
        runtime
    }
    fn token(claims: serde_json::Value) -> String {
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
        header.kid = Some("k1".into());
        jsonwebtoken::encode(
            &header,
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(KEY),
        )
        .unwrap()
    }
    fn user_claims() -> serde_json::Value {
        serde_json::json!({
            "iss": "issuer", "aud": "audience", "exp": 4102444800u64, "token_use": "user",
            "uid": Uuid::from_u128(9).to_string(), "host": Uuid::from_u128(7).to_string(),
        })
    }
    fn with(
        mut claims: serde_json::Value,
        key: &str,
        value: serde_json::Value,
    ) -> serde_json::Value {
        claims[key] = value;
        claims
    }
    fn bearer_of(token: String) -> HeaderMap {
        headers(&[("authorization", &format!("Bearer {token}"))])
    }
    #[tokio::test]
    async fn a_verified_user_token_alone_is_accepted_when_the_route_allows_it() {
        let user = authenticate_user(
            &runtime(false).await,
            &policy(true),
            &bearer_of(token(user_claims())),
        )
        .await
        .expect("accepted");
        assert_eq!(
            user.user_id.as_deref(),
            Some(Uuid::from_u128(9).to_string().as_str())
        );
    }
    #[tokio::test]
    async fn everything_else_is_refused() {
        let runtime = runtime(false).await;
        let on = policy(true);
        let good = token(user_claims());
        // The route does not allow it.
        assert!(
            authenticate_user(&runtime, &policy(false), &bearer_of(good.clone()))
                .await
                .is_err(),
            "route off"
        );
        // Not a user token: an application token, or no purpose at all.
        assert!(
            authenticate_user(
                &runtime,
                &on,
                &bearer_of(token(with(user_claims(), "token_use", "app".into())))
            )
            .await
            .is_err(),
            "app token"
        );
        assert!(authenticate_user(&runtime, &on, &bearer_of(token(serde_json::json!({"iss":"issuer","aud":"audience","exp":4102444800u64,"uid":Uuid::from_u128(9).to_string(),"host":Uuid::from_u128(7).to_string()})))).await.is_err(), "no purpose");
        // Wrong issuer, audience, host; missing or nil user.
        for (key, value) in [
            ("iss", serde_json::json!("other")),
            ("aud", serde_json::json!("other")),
            ("host", serde_json::json!(Uuid::from_u128(8).to_string())),
            ("uid", serde_json::json!(Uuid::nil().to_string())),
            ("uid", serde_json::json!("not-a-uuid")),
        ] {
            assert!(
                authenticate_user(
                    &runtime,
                    &on,
                    &bearer_of(token(with(user_claims(), key, value.clone())))
                )
                .await
                .is_err(),
                "{key}={value}"
            );
        }
        // Expired, unsigned by our key, malformed and missing credentials.
        assert!(
            authenticate_user(
                &runtime,
                &on,
                &bearer_of(token(with(user_claims(), "exp", 1.into())))
            )
            .await
            .is_err(),
            "expired"
        );
        let forged = {
            let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
            header.kid = Some("k1".into());
            jsonwebtoken::encode(
                &header,
                &user_claims(),
                &jsonwebtoken::EncodingKey::from_secret(b"some other key entirely, 32 bytes!"),
            )
            .unwrap()
        };
        assert!(
            authenticate_user(&runtime, &on, &bearer_of(forged))
                .await
                .is_err(),
            "forged"
        );
        assert!(
            authenticate_user(&runtime, &on, &headers(&[]))
                .await
                .is_err(),
            "no credential"
        );
        assert!(
            authenticate_user(
                &runtime,
                &on,
                &headers(&[
                    ("authorization", &format!("Bearer {good}")),
                    ("authorization", &format!("Bearer {good}"))
                ])
            )
            .await
            .is_err(),
            "two credentials"
        );
        assert!(
            authenticate_user(&runtime, &on, &headers(&[("authorization", &good)]))
                .await
                .is_err(),
            "no Bearer scheme"
        );
        // An application identity or an action reference is not this contract.
        assert!(
            authenticate_user(
                &runtime,
                &on,
                &headers(&[
                    ("authorization", &format!("Bearer {good}")),
                    ("x-scope-token", "Bearer a")
                ])
            )
            .await
            .is_err(),
            "scope token"
        );
        assert!(
            authenticate_user(
                &runtime,
                &on,
                &headers(&[
                    ("authorization", &format!("Bearer {good}")),
                    ("x-workflow-action", "00000000-0000-0000-0000-000000000001")
                ])
            )
            .await
            .is_err(),
            "action"
        );
    }
    #[tokio::test]
    async fn admit_routes_each_request_to_its_own_contract() {
        let runtime = runtime(false).await;
        let good = token(user_claims());
        let user_only = bearer_of(good.clone());
        // No application credential on a route that allows it: admitted as user-only.
        match admit(&runtime, &policy(true), &user_only, TlsPeer::default()).await {
            Ok(Admission::UserOnly(user)) => {
                assert_eq!(
                    user.user_id.as_deref(),
                    Some(Uuid::from_u128(9).to_string().as_str())
                )
            }
            other => panic!("expected user-only, got {other:?}"),
        }
        // The same request on a route that does not allow it: refused (the strict contract
        // needs an application credential and a verified peer).
        assert!(
            admit(&runtime, &policy(false), &user_only, TlsPeer::default())
                .await
                .is_err()
        );
        // With an application credential the strict contract decides, so a bad one is refused
        // even though the user token is good and the route allows user-only callers.
        let with_app = headers(&[
            ("authorization", &format!("Bearer {good}")),
            ("x-scope-token", "Bearer nope"),
        ]);
        assert!(
            admit(&runtime, &policy(true), &with_app, TlsPeer::default())
                .await
                .is_err()
        );
        // A complete application request is admitted under the strict contract, on the same
        // route and the same user token: its peer, application token and user all count.
        let app = token(serde_json::json!({
            "iss": "issuer", "aud": "audience", "exp": 4102444800u64, "token_use": "app", "sid": "com.networknt.cli.dev-1.0.0",
        }));
        let strict = headers(&[
            ("authorization", &format!("Bearer {good}")),
            ("x-scope-token", &format!("Bearer {app}")),
        ]);
        let peer = TlsPeer {
            sha256: None,
            issuer_sha256: Some(&"a".repeat(64)),
            environment: Some("dev"),
            uri_san: Some("spiffe://lightapi.local/cli/com.networknt.cli.dev-1.0.0/install-id"),
        };
        match admit(&runtime, &policy(true), &strict, peer).await {
            Ok(Admission::Application(identity)) => {
                assert_eq!(
                    (identity.service_id.as_str(), identity.origin),
                    ("com.networknt.cli.dev-1.0.0", Origin::Interactive)
                );
            }
            other => panic!("expected the strict contract, got {other:?}"),
        }
        // ...and the same application request from an unverified peer is refused.
        assert!(
            admit(&runtime, &policy(true), &strict, TlsPeer::default())
                .await
                .is_err()
        );
        // A certificate issued under the right CA and role still cannot be mixed with an app
        // token for another service.
        let other_service_peer = TlsPeer {
            sha256: None,
            issuer_sha256: Some(&"a".repeat(64)),
            environment: Some("dev"),
            uri_san: Some(
                "spiffe://lightapi.local/cli/com.networknt.other-service-1.0.0/install-id",
            ),
        };
        assert!(
            admit(&runtime, &policy(true), &strict, other_service_peer)
                .await
                .is_err(),
            "CA trust must bind the certificate service ID to the app-token sid"
        );
        // An action reference without an application identity is refused outright.
        let claim = headers(&[
            ("authorization", &format!("Bearer {good}")),
            ("x-workflow-action", "00000000-0000-0000-0000-000000000001"),
        ]);
        assert!(
            admit(&runtime, &policy(true), &claim, TlsPeer::default())
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn a_permissive_runtime_cannot_weaken_it() {
        let good = token(user_claims());
        assert!(
            authenticate_user(
                &runtime(true).await,
                &policy(true),
                &bearer_of(good.clone())
            )
            .await
            .is_err(),
            "expiry ignored"
        );
        let mut mock = runtime(false).await;
        mock.config.enable_mock_jwt = true;
        assert!(
            authenticate_user(&mock, &policy(true), &bearer_of(good.clone()))
                .await
                .is_err(),
            "mock jwt"
        );
        let mut off = runtime(false).await;
        off.config.enable_verify_jwt = false;
        assert!(
            authenticate_user(&off, &policy(true), &bearer_of(good))
                .await
                .is_err(),
            "verification off"
        );
    }
    #[test]
    fn ca_trust_matches_issuer_digest_and_role_prefixed_uri_san() {
        let profile = ca_trust_profile();
        let peer = TlsPeer {
            sha256: None,
            issuer_sha256: Some(&"a".repeat(64)),
            environment: Some("dev"),
            uri_san: Some("spiffe://lightapi.local/cli/com.networknt.cli.dev-1.0.0/install-id"),
        };
        assert!(profile_trusts_peer(
            &profile,
            &peer,
            "com.networknt.cli.dev-1.0.0"
        ));
    }
    #[test]
    fn a_ca_trust_only_profile_does_not_need_an_empty_peer_list_in_config() {
        let profile: AppProfile = serde_json::from_value(serde_json::json!({
            "origin": "interactive",
            "caTrust": {
                "issuerSha256": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "environment": "dev",
                "role": "cli"
            }
        }))
        .expect("CA-only profile parses");
        assert!(profile.peer_sha256.is_empty());
        assert!(profile.ca_trust.is_some());
    }
    #[test]
    fn ca_trust_rejects_wrong_issuer_digest() {
        let profile = ca_trust_profile();
        let peer = TlsPeer {
            sha256: None,
            issuer_sha256: Some(&"b".repeat(64)),
            environment: Some("dev"),
            uri_san: Some("spiffe://lightapi.local/cli/com.networknt.cli.dev-1.0.0/install-id"),
        };
        assert!(!profile_trusts_peer(
            &profile,
            &peer,
            "com.networknt.cli.dev-1.0.0"
        ));
    }
    #[test]
    fn ca_trust_rejects_wrong_role_segment() {
        let profile = ca_trust_profile();
        let peer = TlsPeer {
            sha256: None,
            issuer_sha256: Some(&"a".repeat(64)),
            environment: Some("dev"),
            uri_san: Some("spiffe://lightapi.local/agent/com.networknt.cli.dev-1.0.0/install-id"),
        };
        assert!(!profile_trusts_peer(
            &profile,
            &peer,
            "com.networknt.cli.dev-1.0.0"
        ));
    }
    #[test]
    fn ca_trust_rejects_a_certificate_from_another_environment() {
        let profile = ca_trust_profile();
        let peer = TlsPeer {
            sha256: None,
            issuer_sha256: Some(&"a".repeat(64)),
            environment: Some("prod"),
            uri_san: Some("spiffe://lightapi.local/cli/com.networknt.cli.dev-1.0.0/install-id"),
        };
        assert!(!profile_trusts_peer(
            &profile,
            &peer,
            "com.networknt.cli.dev-1.0.0"
        ));
    }
    #[test]
    fn ca_trust_rejects_a_leaf_for_another_token_service() {
        let profile = ca_trust_profile();
        let peer = TlsPeer {
            sha256: None,
            issuer_sha256: Some(&"a".repeat(64)),
            environment: Some("dev"),
            uri_san: Some("spiffe://lightapi.local/cli/com.networknt.service-a-1.0.0/install-id"),
        };
        assert!(!profile_trusts_peer(
            &profile,
            &peer,
            "com.networknt.service-b-1.0.0"
        ));
    }
    #[test]
    fn exact_leaf_path_is_unaffected_by_a_missing_ca_trust() {
        let profile = AppProfile {
            origin: Origin::Interactive,
            peer_sha256: vec!["c".repeat(64)],
            ca_trust: None,
        };
        assert!(profile_trusts_peer(
            &profile,
            &TlsPeer::from(Some("c".repeat(64).as_str())),
            "any-service-id"
        ));
        assert!(!profile_trusts_peer(
            &profile,
            &TlsPeer::from(Some("d".repeat(64).as_str())),
            "any-service-id"
        ));
    }
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
    #[tokio::test]
    async fn gateway_job_app_token_is_verified_without_a_client_certificate() {
        let app = token(serde_json::json!({
            "iss":"issuer","aud":"audience","exp":4102444800u64,
            "token_use":"app","sid":"com.networknt.cli.dev-1.0.0"
        }));
        let headers = headers(&[("x-scope-token", &format!("Bearer {app}"))]);
        let (_, sid, origin) =
            authenticate_application_token(&runtime(false).await, &policy(false), &headers)
                .await
                .unwrap();
        assert_eq!(sid, "com.networknt.cli.dev-1.0.0");
        assert_eq!(origin, Origin::Interactive);
        let mut duplicate = headers.clone();
        duplicate.append("x-scope-token", "Bearer another".parse().unwrap());
        assert!(
            authenticate_application_token(&runtime(false).await, &policy(false), &duplicate,)
                .await
                .is_err()
        );
    }
}

/// Service-only evidence reporting. This profile cannot disclose user results or
/// authorize target work; its callers must expose only fixed completion routes.
pub async fn authenticate_application<'a>(
    runtime: &SecurityRuntime,
    policy: &RoutePolicy,
    headers: &HeaderMap,
    tls_peer: impl Into<TlsPeer<'a>>,
) -> Result<(AuthPrincipal, String, Origin), HandlerRejection> {
    let tls_peer = tls_peer.into();
    policy.validate()?;
    if runtime.config.ignore_jwt_expiry
        || runtime.config.enable_mock_jwt
        || !runtime.config.enable_verify_jwt
    {
        return Err(denied());
    }
    if tls_peer.sha256.is_none() && tls_peer.issuer_sha256.is_none() {
        return Err(denied());
    }
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
    if !profile_trusts_peer(profile, &tls_peer, &sid) {
        return Err(denied());
    }
    Ok((app, sid, profile.origin))
}

/// Bearer-only application authentication for a Gateway-routed internal job
/// bridge. Callers must pin the resulting service ID to the stored job owner
/// and keep the backend route unreachable to unauthenticated clients.
pub async fn authenticate_application_token(
    runtime: &SecurityRuntime,
    policy: &RoutePolicy,
    headers: &HeaderMap,
) -> Result<(AuthPrincipal, String, Origin), HandlerRejection> {
    policy.validate()?;
    if runtime.config.ignore_jwt_expiry
        || runtime.config.enable_mock_jwt
        || !runtime.config.enable_verify_jwt
    {
        return Err(denied());
    }
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
    Ok((app, sid, profile.origin))
}
