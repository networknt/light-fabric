//! Gateway-side projection of Agent gatewayDelegation and OAuth registration.
//! This is trusted runtime material, not a second assignment store.
use http::HeaderMap;
use light_security::{AuthPrincipal, JwtExpiryMode, SecurityRuntime, verify_jwt_token};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentDelegationConfig {
    /// Exact endpoint IDs. true requires a workload independently of alias existence.
    pub endpoints: BTreeMap<String, bool>,
    pub bindings: Vec<AgentBinding>,
    pub user_issuer: String,
    pub user_audience: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentBinding {
    pub client_id: Uuid,
    pub agent_def_id: Uuid,
    pub host_id: Uuid,
    pub environment: String,
    pub issuer: String,
    pub audience: String,
    pub scopes: BTreeSet<String>,
    pub route_alias: String,
    /// Source publication content digest, including the authored contract.
    pub policy_digest: String,
    pub registration_version: u64,
    /// Accepted for old snapshots only; assignments last until replaced or revoked.
    #[serde(default, skip_serializing)]
    pub expires_at: i64,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AuthorizationAudit {
    pub user_id: Option<String>,
    pub user_issuer: Option<String>,
    pub workload_client_id: Option<String>,
    pub workload_issuer: Option<String>,
    pub agent_def_id: Option<String>,
    pub host_id: Option<String>,
    pub environment: Option<String>,
    pub policy_digest: Option<String>,
    pub registration_version: Option<u64>,
    pub correlation_id: Option<String>,
    pub decision: String,
    pub user_access_decision: Option<String>,
    pub agent_assignment_decision: Option<String>,
}
impl AuthorizationAudit {
    pub fn assignment_allowed(&self) -> Self {
        let mut audit = self.clone();
        audit.agent_assignment_decision = Some(
            if self.agent_def_id.is_some() {
                "allowed"
            } else {
                "not_applicable"
            }
            .into(),
        );
        audit.decision = "allowed".into();
        audit
    }
}
#[derive(Debug, Clone)]
pub struct VerifiedIdentity {
    pub user: AuthPrincipal,
    pub principal_id: String,
    pub billing_subject: String,
    pub bound_alias: Option<String>,
    pub audit: AuthorizationAudit,
}
#[derive(Debug)]
pub struct AuthorizationFailure {
    pub status: u16,
    pub reason: &'static str,
    pub audit: AuthorizationAudit,
}
fn failure(status: u16, reason: &'static str, audit: &AuthorizationAudit) -> AuthorizationFailure {
    let mut audit = audit.clone();
    audit.decision = reason.into();
    AuthorizationFailure {
        status,
        reason,
        audit,
    }
}
impl AgentDelegationConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.endpoints.is_empty() {
            return Err("agentDelegation requires endpoint profiles".into());
        }
        if self.user_issuer.trim().is_empty() || self.user_audience.trim().is_empty() {
            return Err("agentDelegation requires user issuer/audience".into());
        }
        for endpoint in self.endpoints.keys() {
            if !matches!(
                endpoint.as_str(),
                "/v1/chat/completions@post" | "/v1/responses@post" | "/anthropic/v1/messages@post"
            ) {
                return Err("agentDelegation endpoint is not a supported generation route".into());
            }
        }
        let mut seen = BTreeSet::new();
        for b in &self.bindings {
            if b.client_id.is_nil()
                || b.agent_def_id.is_nil()
                || b.host_id.is_nil()
                || b.environment.trim().is_empty()
                || b.issuer.trim().is_empty()
                || b.audience.trim().is_empty()
                || b.route_alias.trim().is_empty()
                || b.scopes.is_empty()
                || b.scopes
                    .iter()
                    .any(|s| s.is_empty() || s.chars().any(char::is_whitespace))
                || b.registration_version == 0
                || !b.policy_digest.starts_with("sha256:")
                || b.policy_digest.len() != 71
                || !b.policy_digest[7..].bytes().all(|c| c.is_ascii_hexdigit())
                || !seen.insert((b.issuer.clone(), b.client_id))
            {
                return Err("agentDelegation has invalid or ambiguous registration binding".into());
            }
        }
        Ok(())
    }

    pub async fn authenticate(
        &self,
        runtime: &SecurityRuntime,
        headers: &HeaderMap,
        required: bool,
        now: i64,
    ) -> Result<VerifiedIdentity, AuthorizationFailure> {
        let mut audit = AuthorizationAudit::default();
        // Parse BOTH headers before any verification; never use the legacy scope fallback.
        let user_token = bearer(headers, "authorization")
            .map_err(|r| failure(401, r, &audit))?
            .ok_or_else(|| failure(401, "user_token_required", &audit))?;
        let workload_token =
            bearer(headers, "x-scope-token").map_err(|r| failure(401, r, &audit))?;
        if user_token.starts_with("lad1.") || workload_token.is_some_and(|t| t.starts_with("lad1."))
        {
            return Err(failure(401, "delegation_kind_not_supported", &audit));
        }
        if !runtime.config.enable_verify_jwt || runtime.config.enable_mock_jwt {
            return Err(failure(503, "jwt_verification_unavailable", &audit));
        }
        let user = verify_jwt_token(runtime, user_token, JwtExpiryMode::Enforce)
            .await
            .map_err(|_| failure(401, "invalid_user_token", &audit))?;
        strict_claims(&user, &self.user_issuer, &self.user_audience, now)
            .map_err(|_| failure(401, "invalid_user_token", &audit))?;
        let uid = user
            .user_id
            .clone()
            .or_else(|| string(&user, "uid"))
            .filter(|v| !v.is_empty())
            .ok_or_else(|| failure(401, "user_identity_required", &audit))?;
        audit.user_id = Some(uid);
        audit.user_issuer = user.issuer.clone();
        audit.host_id = user.host.clone();
        let Some(token) = workload_token else {
            if required {
                return Err(failure(401, "workload_token_required", &audit));
            }
            let principal_id = user
                .client_id
                .clone()
                .or_else(|| user.user_id.clone())
                .unwrap_or_default();
            return Ok(VerifiedIdentity {
                billing_subject: string(&user, "billingSubject")
                    .unwrap_or_else(|| principal_id.clone()),
                bound_alias: string(&user, "routeAlias"),
                principal_id,
                user,
                audit,
            });
        };
        let workload = verify_jwt_token(runtime, token, JwtExpiryMode::Enforce)
            .await
            .map_err(|_| failure(401, "invalid_workload_token", &audit))?;
        // Legacy signature verification may allow expired tokens. Reject them before registration lookup.
        if workload
            .claims
            .get("exp")
            .and_then(|v| v.as_i64())
            .is_none_or(|v| v <= now)
        {
            return Err(failure(401, "invalid_workload_token", &audit));
        }
        // Trust identity only after signature AND strict temporal/profile verification.
        let b = self
            .bindings
            .iter()
            .find(|b| {
                workload.client_id.as_deref() == Some(b.client_id.to_string().as_str())
                    && workload.issuer.as_deref() == Some(b.issuer.as_str())
            })
            .ok_or_else(|| failure(403, "workload_registration_denied", &audit))?;
        strict_claims(&workload, &b.issuer, &b.audience, now)
            .map_err(|_| failure(401, "invalid_workload_token", &audit))?;
        audit.workload_client_id = workload.client_id.clone();
        audit.workload_issuer = workload.issuer.clone();
        audit.agent_def_id = Some(b.agent_def_id.to_string());
        audit.environment = Some(b.environment.clone());
        audit.policy_digest = Some(b.policy_digest.clone());
        audit.registration_version = Some(b.registration_version);
        if user.host.as_deref() != Some(b.host_id.to_string().as_str())
            || workload.host != user.host
            || string(&workload, "env")
                .or_else(|| string(&workload, "environment"))
                .as_deref()
                != Some(&b.environment)
        {
            return Err(failure(403, "workload_context_denied", &audit));
        }
        let scopes: BTreeSet<String> = match workload.claims.get("scp") {
            Some(serde_json::Value::Array(v)) => v
                .iter()
                .filter_map(|s| s.as_str().map(str::to_owned))
                .collect(),
            _ => string(&workload, "scope")
                .unwrap_or_default()
                .split_whitespace()
                .map(str::to_owned)
                .collect(),
        };
        if !b.scopes.is_subset(&scopes) {
            return Err(failure(403, "workload_scope_denied", &audit));
        }
        for restriction in [string(&user, "routeAlias"), string(&workload, "routeAlias")] {
            if restriction.as_deref().is_some_and(|v| v != b.route_alias) {
                return Err(failure(403, "route_restriction_conflict", &audit));
            }
        }
        let principal_id = b.agent_def_id.to_string();
        audit.decision = "identity_verified".into();
        Ok(VerifiedIdentity {
            billing_subject: string(&workload, "billingSubject")
                .unwrap_or_else(|| principal_id.clone()),
            principal_id,
            bound_alias: Some(b.route_alias.clone()),
            user,
            audit,
        })
    }
}
fn string(p: &AuthPrincipal, k: &str) -> Option<String> {
    p.claims
        .get(k)
        .and_then(|v| v.as_str())
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}
fn strict_claims(p: &AuthPrincipal, issuer: &str, audience: &str, now: i64) -> Result<(), ()> {
    let claims = &p.claims;
    let aud = claims.get("aud").is_some_and(|v| {
        v.as_str() == Some(audience)
            || v.as_array()
                .is_some_and(|a| a.iter().any(|x| x.as_str() == Some(audience)))
    });
    if p.issuer.as_deref() != Some(issuer)
        || !aud
        || claims
            .get("exp")
            .and_then(|v| v.as_i64())
            .is_none_or(|v| v <= now)
        || claims
            .get("nbf")
            .is_some_and(|v| v.as_i64().is_none_or(|v| v > now))
        || claims
            .get("iat")
            .is_some_and(|v| v.as_i64().is_none_or(|v| v > now))
    {
        return Err(());
    }
    Ok(())
}
fn bearer<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>, &'static str> {
    let mut values = headers.get_all(name).iter();
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err("duplicate_credential_header");
    }
    let raw = value.to_str().map_err(|_| "invalid_credential_header")?;
    let (scheme, token) = raw.split_once(' ').ok_or("invalid_credential_header")?;
    if !scheme.eq_ignore_ascii_case("bearer")
        || token.is_empty()
        || token.bytes().any(|b| b.is_ascii_whitespace() || b == b',')
    {
        return Err("invalid_credential_header");
    }
    Ok(Some(token))
}

#[cfg(test)]
mod tests {
    #[test]
    fn assignment_snapshot_does_not_require_or_emit_expiry() {
        let mut value = serde_json::to_value(policy()).unwrap();
        for binding in value["bindings"].as_array_mut().unwrap() {
            binding.as_object_mut().unwrap().remove("expiresAt");
        }
        let policy: super::AgentDelegationConfig = serde_json::from_value(value).unwrap();
        policy.validate().unwrap();
        assert!(
            !serde_json::to_string(&policy)
                .unwrap()
                .contains("expiresAt")
        );
    }
    use super::*;
    fn policy() -> AgentDelegationConfig {
        serde_json::from_str(include_str!(
            "../tests/fixtures/authorization/gateway-agent-bindings-v1.json"
        ))
        .unwrap()
    }
    #[test]
    fn published_agent_registration_contract_is_strict() {
        let p = policy();
        p.validate().unwrap();
        assert_eq!(
            p.bindings[0].agent_def_id.to_string(),
            "019d82bf-ab5e-791a-885c-d08aafa2b614"
        );
        assert_ne!(p.bindings[0].client_id, p.bindings[0].agent_def_id);
        let mut duplicate = p.clone();
        duplicate.bindings.push(p.bindings[0].clone());
        assert!(duplicate.validate().is_err());
        let mut bad = p.clone();
        bad.user_audience.clear();
        assert!(bad.validate().is_err());
        let mut bad = p.clone();
        bad.endpoints.insert("/mcp@post".into(), true);
        assert!(bad.validate().is_err());
        let mut bad = p.clone();
        bad.bindings[0].policy_digest = "sha256:invalid".into();
        assert!(bad.validate().is_err());
        let mut empty = p.clone();
        empty.bindings.clear();
        empty.validate().unwrap();
        assert_eq!(empty.endpoints, p.endpoints);
    }
    #[test]
    fn credentials_have_one_route_selected_wire_form() {
        let mut headers = HeaderMap::new();
        assert_eq!(bearer(&headers, "x-scope-token"), Ok(None));
        for value in [
            "raw.jwt.token",
            "Bearer ",
            "Basic value",
            "Bearer a,b",
            "Bearer a b",
        ] {
            headers.insert("x-scope-token", value.parse().unwrap());
            assert!(bearer(&headers, "x-scope-token").is_err());
        }
        headers.insert("x-scope-token", "Bearer a.b.c".parse().unwrap());
        assert_eq!(bearer(&headers, "x-scope-token"), Ok(Some("a.b.c")));
        headers.append("x-scope-token", "Bearer d.e.f".parse().unwrap());
        assert!(bearer(&headers, "x-scope-token").is_err());
    }
    #[test]
    fn profile_checks_expiry_audience_and_not_before_independently_of_legacy_config() {
        let mut p = AuthPrincipal::default();
        p.issuer = Some("issuer".into());
        p.claims = serde_json::json!({"aud":["other","gateway"],"exp":200,"nbf":90});
        assert!(strict_claims(&p, "issuer", "gateway", 100).is_ok());
        assert!(strict_claims(&p, "issuer", "gateway", 200).is_err());
        assert!(strict_claims(&p, "issuer", "gateway", 89).is_err());
        assert!(strict_claims(&p, "issuer", "missing", 100).is_err());
        assert!(strict_claims(&p, "wrong", "gateway", 100).is_err());
        p.claims["exp"] = serde_json::Value::Null;
        assert!(strict_claims(&p, "issuer", "gateway", 100).is_err());
    }
}
