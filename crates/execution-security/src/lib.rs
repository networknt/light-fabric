use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::IpAddr,
    path::{Component, Path},
};
use thiserror::Error;
use url::Url;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DestinationRule {
    pub scheme: String,
    pub host: String,
    pub ports: BTreeSet<u16>,
    pub allowed_addresses: BTreeSet<IpAddr>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DestinationPolicy {
    pub rules: Vec<DestinationRule>,
    pub allow_private_addresses: bool,
}

impl DestinationPolicy {
    pub fn authorize(
        &self,
        raw: &str,
        resolved: &BTreeSet<IpAddr>,
    ) -> Result<AuthorizedDestination, SecurityError> {
        let url = Url::parse(raw).map_err(|_| SecurityError::Destination)?;
        if url.username() != "" || url.password().is_some() || url.fragment().is_some() {
            return Err(SecurityError::Destination);
        }
        let scheme = url.scheme().to_ascii_lowercase();
        if !matches!(scheme.as_str(), "https" | "http") {
            return Err(SecurityError::Destination);
        }
        let host = url
            .host_str()
            .ok_or(SecurityError::Destination)?
            .trim_end_matches('.')
            .to_ascii_lowercase();
        let port = url
            .port_or_known_default()
            .ok_or(SecurityError::Destination)?;
        if resolved.is_empty()
            || (!self.allow_private_addresses && resolved.iter().any(is_non_public))
        {
            return Err(SecurityError::Address);
        }
        let rule = self
            .rules
            .iter()
            .find(|rule| {
                rule.scheme.eq_ignore_ascii_case(&scheme)
                    && rule.host.trim_end_matches('.').eq_ignore_ascii_case(&host)
                    && rule.ports.contains(&port)
            })
            .ok_or(SecurityError::Destination)?;
        if !rule.allowed_addresses.is_empty() && !resolved.is_subset(&rule.allowed_addresses) {
            return Err(SecurityError::Address);
        }
        Ok(AuthorizedDestination {
            scheme,
            host,
            port,
            addresses: resolved.clone(),
        })
    }

    pub fn authorize_redirect(
        &self,
        previous: &AuthorizedDestination,
        next: &str,
        resolved: &BTreeSet<IpAddr>,
    ) -> Result<AuthorizedDestination, SecurityError> {
        let next = self.authorize(next, resolved)?;
        if next.scheme != previous.scheme
            || next.host != previous.host
            || next.port != previous.port
        {
            return Err(SecurityError::Redirect);
        }
        Ok(next)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizedDestination {
    pub scheme: String,
    pub host: String,
    pub port: u16,
    pub addresses: BTreeSet<IpAddr>,
}

fn is_non_public(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => {
            v.is_private()
                || v.is_loopback()
                || v.is_link_local()
                || v.is_broadcast()
                || v.is_unspecified()
                || v.is_multicast()
        }
        IpAddr::V6(v) => {
            v.is_loopback()
                || v.is_unspecified()
                || v.is_multicast()
                || v.is_unique_local()
                || v.is_unicast_link_local()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AttemptGrant {
    pub grant_id: Uuid,
    pub execution_id: Uuid,
    pub fencing_token: u64,
    pub policy_digest: String,
    pub operation: String,
    pub destination: String,
    pub maximum_uses: u32,
    pub expires_at: DateTime<Utc>,
}

pub struct GrantLedger {
    grants: BTreeMap<Uuid, (AttemptGrant, u32)>,
}
impl GrantLedger {
    pub fn new() -> Self {
        Self {
            grants: BTreeMap::new(),
        }
    }
    pub fn issue(&mut self, grant: AttemptGrant) -> Result<(), SecurityError> {
        if grant.maximum_uses == 0 || grant.expires_at <= Utc::now() {
            return Err(SecurityError::Grant);
        }
        self.grants.insert(grant.grant_id, (grant, 0));
        Ok(())
    }
    pub fn consume(
        &mut self,
        id: Uuid,
        execution_id: Uuid,
        fence: u64,
        policy: &str,
        operation: &str,
        destination: &str,
    ) -> Result<(), SecurityError> {
        let (grant, uses) = self.grants.get_mut(&id).ok_or(SecurityError::Grant)?;
        if grant.execution_id != execution_id
            || grant.fencing_token != fence
            || grant.policy_digest != policy
            || grant.operation != operation
            || grant.destination != destination
            || grant.expires_at <= Utc::now()
            || *uses >= grant.maximum_uses
        {
            return Err(SecurityError::Grant);
        }
        *uses += 1;
        Ok(())
    }
    pub fn revoke_execution(&mut self, execution_id: Uuid) {
        self.grants
            .retain(|_, (grant, _)| grant.execution_id != execution_id);
    }
}
impl Default for GrantLedger {
    fn default() -> Self {
        Self::new()
    }
}

pub const DEFAULT_PROTECTED_PATHS: &[&str] = &[
    ".github/workflows",
    ".gitlab-ci.yml",
    ".circleci",
    "azure-pipelines.yml",
    "Jenkinsfile",
    "CODEOWNERS",
    ".git",
    ".env",
    "credentials",
    "secrets",
    "policy",
];

#[derive(Debug, Clone)]
pub struct ProtectedPathPolicy {
    protected: Vec<String>,
    explicit_allow: BTreeSet<String>,
    case_sensitive: bool,
}
impl ProtectedPathPolicy {
    pub fn new(
        protected: Vec<String>,
        explicit_allow: BTreeSet<String>,
        case_sensitive: bool,
    ) -> Result<Self, SecurityError> {
        let protected = protected
            .into_iter()
            .map(|p| normalize_relative(&p))
            .collect::<Result<Vec<_>, _>>()?;
        let explicit_allow = explicit_allow
            .into_iter()
            .map(|p| normalize_relative(&p))
            .collect::<Result<BTreeSet<_>, _>>()?;
        Ok(Self {
            protected,
            explicit_allow,
            case_sensitive,
        })
    }
    pub fn default_deny() -> Self {
        Self::new(
            DEFAULT_PROTECTED_PATHS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            BTreeSet::new(),
            false,
        )
        .expect("static paths")
    }
    pub fn validate_changes<'a>(
        &self,
        paths: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), SecurityError> {
        for raw in paths {
            let path = normalize_relative(raw)?;
            let compare = if self.case_sensitive {
                path.clone()
            } else {
                path.to_ascii_lowercase()
            };
            let allowed = self.explicit_allow.iter().any(|a| {
                let candidate = if self.case_sensitive {
                    a.clone()
                } else {
                    a.to_ascii_lowercase()
                };
                component_subsumes(&candidate, &compare)
            });
            if allowed {
                continue;
            }
            for protected in &self.protected {
                let p = if self.case_sensitive {
                    protected.clone()
                } else {
                    protected.to_ascii_lowercase()
                };
                if component_subsumes(&p, &compare) || component_subsumes(&compare, &p) {
                    return Err(SecurityError::Protected(path));
                }
            }
        }
        Ok(())
    }
}

fn normalize_relative(raw: &str) -> Result<String, SecurityError> {
    if raw.contains('\0') || raw.contains('\\') {
        return Err(SecurityError::Path);
    }
    let path = Path::new(raw);
    if path.is_absolute() {
        return Err(SecurityError::Path);
    }
    let mut parts = Vec::new();
    for c in path.components() {
        match c {
            Component::Normal(p) => {
                let s = p.to_str().ok_or(SecurityError::Path)?;
                if s.is_empty() || s == "." {
                    return Err(SecurityError::Path);
                }
                parts.push(s)
            }
            _ => return Err(SecurityError::Path),
        }
    }
    if parts.is_empty() {
        return Err(SecurityError::Path);
    }
    Ok(parts.join("/"))
}
fn component_subsumes(parent: &str, child: &str) -> bool {
    child == parent
        || child
            .strip_prefix(parent)
            .is_some_and(|rest| rest.starts_with('/'))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TrustBundle {
    pub bundle_id: String,
    pub version: u32,
    pub path: String,
    pub digest: String,
}
impl TrustBundle {
    pub fn verify_bytes(&self, bytes: &[u8]) -> Result<(), SecurityError> {
        let digest = format!("sha256:{}", hex::encode(Sha256::digest(bytes)));
        if digest != self.digest {
            return Err(SecurityError::TrustBundle);
        }
        Ok(())
    }
}

pub fn reject_secret_shaped(name: &str, value: &str) -> Result<(), SecurityError> {
    let key = name.to_ascii_lowercase();
    if [
        "token",
        "secret",
        "password",
        "api_key",
        "apikey",
        "private_key",
        "credential",
        "authorization",
    ]
    .iter()
    .any(|m| key.contains(m))
        && !value.trim().is_empty()
    {
        return Err(SecurityError::Secret);
    }
    if value.to_ascii_lowercase().contains("bearer ")
        || value.contains("-----BEGIN PRIVATE KEY-----")
    {
        return Err(SecurityError::Secret);
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimeToolBinding {
    pub stable_ref: Uuid,
    pub alias: String,
    pub placement: String,
    pub schema_digest: String,
    pub dispatcher_digest: String,
}

pub fn intersect_runner_tools(
    catalog: &[RuntimeToolBinding],
    profile: &BTreeSet<Uuid>,
    lease: &BTreeSet<Uuid>,
    manifest: &BTreeSet<Uuid>,
    live: &BTreeSet<Uuid>,
) -> Result<Vec<RuntimeToolBinding>, SecurityError> {
    let mut aliases = BTreeMap::new();
    let mut result = Vec::new();
    for tool in catalog {
        if tool.placement != "runner"
            || !profile.contains(&tool.stable_ref)
            || !lease.contains(&tool.stable_ref)
            || !manifest.contains(&tool.stable_ref)
            || !live.contains(&tool.stable_ref)
        {
            continue;
        }
        if aliases
            .insert(tool.alias.clone(), tool.stable_ref)
            .is_some()
        {
            return Err(SecurityError::Alias);
        }
        result.push(tool.clone())
    }
    Ok(result)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataBoundary {
    Saas,
    TenantRunner,
    PersonalEdge,
}
pub fn authorize_model_boundary(
    source: DataBoundary,
    model: DataBoundary,
    explicit_cross_boundary: bool,
) -> Result<(), SecurityError> {
    if source != model && !explicit_cross_boundary {
        return Err(SecurityError::DataBoundary);
    }
    Ok(())
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SecurityError {
    #[error("destination is not allowlisted")]
    Destination,
    #[error("resolved address is not allowed")]
    Address,
    #[error("redirect changes the authorized destination")]
    Redirect,
    #[error("attempt grant is invalid, expired, revoked, or exhausted")]
    Grant,
    #[error("path is invalid or non-normalized")]
    Path,
    #[error("protected path modification denied: {0}")]
    Protected(String),
    #[error("trust bundle digest mismatch")]
    TrustBundle,
    #[error("raw credential material is forbidden")]
    Secret,
    #[error("runner tool alias collision")]
    Alias,
    #[error("agent/model data boundary is not authorized")]
    DataBoundary,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn network_denies_private_redirect_and_unexpected_port() {
        let public: IpAddr = "8.8.8.8".parse().unwrap();
        let policy = DestinationPolicy {
            rules: vec![DestinationRule {
                scheme: "https".into(),
                host: "api.example.com".into(),
                ports: BTreeSet::from([443]),
                allowed_addresses: BTreeSet::from([public]),
            }],
            allow_private_addresses: false,
        };
        let allowed = policy
            .authorize("https://api.example.com/v1", &BTreeSet::from([public]))
            .unwrap();
        assert!(
            policy
                .authorize("https://api.example.com:8443", &BTreeSet::from([public]))
                .is_err()
        );
        assert!(
            policy
                .authorize(
                    "https://api.example.com",
                    &BTreeSet::from(["127.0.0.1".parse().unwrap()])
                )
                .is_err()
        );
        assert!(
            policy
                .authorize_redirect(
                    &allowed,
                    "https://evil.example/v1",
                    &BTreeSet::from([public])
                )
                .is_err()
        );
    }
    #[test]
    fn protected_paths_are_component_and_case_aware() {
        let policy = ProtectedPathPolicy::default_deny();
        assert!(
            policy
                .validate_changes([".GitHub/workflows/pwn.yml"])
                .is_err()
        );
        assert!(
            policy
                .validate_changes([".github/workflows-archive/readme"])
                .is_ok()
        );
        assert!(
            policy
                .validate_changes(["src/../.github/workflows/x"])
                .is_err()
        );
    }
    #[test]
    fn grants_are_exact_and_bounded() {
        let mut ledger = GrantLedger::new();
        let id = Uuid::now_v7();
        let exec = Uuid::now_v7();
        ledger
            .issue(AttemptGrant {
                grant_id: id,
                execution_id: exec,
                fencing_token: 2,
                policy_digest: "p".into(),
                operation: "call".into(),
                destination: "d".into(),
                maximum_uses: 1,
                expires_at: Utc::now() + chrono::Duration::minutes(1),
            })
            .unwrap();
        assert!(ledger.consume(id, exec, 2, "p", "call", "d").is_ok());
        assert!(ledger.consume(id, exec, 2, "p", "call", "d").is_err());
    }
}
