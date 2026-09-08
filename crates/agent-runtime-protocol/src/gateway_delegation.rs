//! Published gateway delegation contract. Agent and gateway runtimes enforce activation.
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use url::Url;
use uuid::Uuid;

/// Empty legacy projections serialize back to exactly `{}` (digest compatibility).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GatewayDelegationPolicy {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_policy"
    )]
    pub dual_token: Option<DualTokenPolicy>,
}

fn present_policy<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<DualTokenPolicy>, D::Error> {
    DualTokenPolicy::deserialize(deserializer).map(Some)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DualTokenPolicy {
    pub schema_version: u32,
    pub profile: String,
    pub gateway_url: String,
    pub user_issuer: String,
    pub user_audience: String,
    pub workload_issuer: String,
    pub workload_audience: String,
    pub token_endpoint: String,
    pub client_id: Uuid,
    pub client_secret_file: String,
    pub scopes: Vec<String>,
    pub refresh_before_seconds: u32,
    pub route_alias: String,
}

impl GatewayDelegationPolicy {
    pub fn validate(&self, gateway_url: &str, route_alias: &str) -> Result<(), String> {
        let Some(p) = &self.dual_token else {
            return Ok(());
        };
        if p.schema_version != 1 || p.profile != "user-agent-dual-token-v1" {
            return Err("gatewayDelegation: unsupported schemaVersion/profile".into());
        }
        for value in [&p.gateway_url, &p.token_endpoint] {
            let u = Url::parse(value).map_err(|_| "gatewayDelegation: invalid HTTPS URL")?;
            if u.scheme() != "https"
                || u.host_str().is_none()
                || !u.username().is_empty()
                || u.password().is_some()
                || u.query().is_some()
                || u.fragment().is_some()
            {
                return Err(
                    "gatewayDelegation: URL must be HTTPS without credentials/query/fragment"
                        .into(),
                );
            }
        }
        if p.gateway_url != gateway_url
            || p.route_alias != route_alias
            || p.route_alias.trim().is_empty()
        {
            return Err("gatewayDelegation: gatewayUrl/routeAlias must match model policy".into());
        }
        for value in [
            &p.user_issuer,
            &p.user_audience,
            &p.workload_issuer,
            &p.workload_audience,
        ] {
            if value.trim().is_empty()
                || value.trim() != value
                || value.chars().any(char::is_control)
            {
                return Err(
                    "gatewayDelegation: explicit issuer and audience required for both tokens"
                        .into(),
                );
            }
        }
        if p.client_id.is_nil() || p.refresh_before_seconds == 0 || p.refresh_before_seconds >= 600
        {
            return Err(
                "gatewayDelegation: nonnil clientId and refresh margin 1..599 required".into(),
            );
        }
        let path = &p.client_secret_file;
        if !path.starts_with("/run/secrets/")
            || path.len() == "/run/secrets/".len()
            || path
                .split('/')
                .skip(1)
                .any(|v| v.is_empty() || v == "." || v == "..")
            || path.chars().any(char::is_control)
        {
            return Err(
                "gatewayDelegation: clientSecretFile must be a normalized /run/secrets path".into(),
            );
        }
        let mut seen = BTreeSet::new();
        if p.scopes.is_empty()
            || p.scopes.iter().any(|s| {
                s.is_empty()
                    || !s.bytes().all(|b| {
                        b == 0x21 || (0x23..=0x5b).contains(&b) || (0x5d..=0x7e).contains(&b)
                    })
                    || !seen.insert(s)
            })
        {
            return Err("gatewayDelegation: distinct OAuth scope tokens required".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical_digest;
    fn fixture() -> serde_json::Value {
        serde_json::from_str(include_str!("../tests/fixtures/gateway-delegation-v1.json")).unwrap()
    }
    #[test]
    fn gateway_delegation_wire_and_digest_are_stable() {
        let raw = fixture();
        let policy: GatewayDelegationPolicy = serde_json::from_value(raw.clone()).unwrap();
        policy
            .validate("https://llm-gateway:8443/v1", "assistant-dev")
            .unwrap();
        assert_eq!(serde_json::to_value(&policy).unwrap(), raw);
        assert_eq!(
            canonical_digest(&policy).unwrap(),
            canonical_digest(&raw).unwrap()
        );
        let empty: GatewayDelegationPolicy = serde_json::from_str("{}").unwrap();
        assert_eq!(serde_json::to_string(&empty).unwrap(), "{}");
    }
    #[test]
    fn gateway_delegation_rejects_incomplete_unsafe_or_unknown_contracts() {
        for (field, value) in [
            ("schemaVersion", serde_json::json!(2)),
            ("profile", serde_json::json!("fallback")),
            ("userAudience", serde_json::json!("")),
            ("workloadAudience", serde_json::json!("")),
            ("gatewayUrl", serde_json::json!("http://llm-gateway/v1")),
            (
                "tokenEndpoint",
                serde_json::json!("https://user:secret@oauth/token"),
            ),
            (
                "clientSecretFile",
                serde_json::json!("/run/secrets/../token"),
            ),
            ("refreshBeforeSeconds", serde_json::json!(600)),
            ("routeAlias", serde_json::json!("another-agent")),
            ("scopes", serde_json::json!(["portal.r", "portal.r"])),
        ] {
            let mut raw = fixture();
            raw["dualToken"][field] = value;
            let p: GatewayDelegationPolicy = serde_json::from_value(raw).unwrap();
            assert!(
                p.validate("https://llm-gateway:8443/v1", "assistant-dev")
                    .is_err(),
                "{field}"
            );
        }
        let mut raw = fixture();
        raw["dualToken"]["clientSecret"] = "do-not-publish".into();
        assert!(serde_json::from_value::<GatewayDelegationPolicy>(raw).is_err());
        assert!(serde_json::from_str::<GatewayDelegationPolicy>(r#"{"dualToken":{}}"#).is_err());
        assert!(serde_json::from_str::<GatewayDelegationPolicy>(r#"{"dualToken":null}"#).is_err());
    }
}
