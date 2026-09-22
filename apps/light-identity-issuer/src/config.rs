//! Configuration for the issuer service. Deliberately a plain YAML file
//! read directly, not `config-loader`/`light-runtime`: this service's own
//! settings (CA material paths, JWKS URL, per-environment lifetime policy)
//! are static at process start, and pulling in config-server's config
//! distribution client here would be circular for the one service other
//! workloads rely on to bootstrap their own trust.
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

fn default_bind_address() -> String {
    "0.0.0.0:9443".to_string()
}

#[derive(Debug, Deserialize)]
pub struct EnvPolicyConfig {
    pub leaf_lifetime_seconds: u64,
    pub renew_lead_seconds: u64,
}

/// Binds tokens for a family of services to the role their certificates carry.
/// Service IDs carry their version (`com.networknt.light-cli-1.0.0`), so one
/// prefix covers every release of a service.
#[derive(Debug, Deserialize)]
pub struct BootstrapRoleConfig {
    pub service_id_prefix: String,
    pub role: String,
}

#[derive(Debug, Deserialize)]
pub struct IssuerConfig {
    pub ca_certificate_path: PathBuf,
    pub ca_key_path: PathBuf,
    /// The `light-oauth` JWKS endpoint used to verify the long-lived Portal
    /// token presented for a service workload's first issuance.
    pub jwks_url: String,
    /// Extra CA certificate (PEM) to trust when fetching `jwks_url`, needed
    /// wherever `light-oauth` presents a cert not chained to the system
    /// trust store (e.g. the per-service dev certs `prepare.py` mints).
    #[serde(default)]
    pub jwks_ca_cert_path: Option<PathBuf>,
    pub portal_token_issuer: String,
    pub portal_token_audience: String,
    pub environments: BTreeMap<String, EnvPolicyConfig>,
    /// Which token `sid` prefixes may enroll, and the role each is bound to.
    /// Empty by default, which rejects every Portal-token first issuance.
    #[serde(default)]
    pub bootstrap_roles: Vec<BootstrapRoleConfig>,
    /// Serve the unauthenticated `POST /v1/pairing-codes` stub. Off by
    /// default; enable only on a development stack.
    #[serde(default)]
    pub enable_pairing_stub: bool,
    /// PEM certificate chain the issuer presents. Set together with
    /// `tls_key_path`.
    #[serde(default)]
    pub tls_certificate_path: Option<PathBuf>,
    #[serde(default)]
    pub tls_key_path: Option<PathBuf>,
    /// Serve plain HTTP. Off by default and refused alongside TLS settings: the
    /// bootstrap call carries a long-lived token, so cleartext is only for a
    /// throwaway local run.
    #[serde(default)]
    pub allow_insecure_http: bool,
    /// Directory for the issuer's durable state (the spent-token journal). The
    /// once-only guarantee for bootstrap tokens depends on it surviving a restart,
    /// so the service refuses to start without it, unless `allow_ephemeral_state`
    /// is set.
    #[serde(default)]
    pub state_dir: Option<PathBuf>,
    /// Keep the spent-token record in memory only: an issuer restart forgets every
    /// spend, so a used token can be used again. For a throwaway local run only;
    /// refused alongside `state_dir`.
    #[serde(default)]
    pub allow_ephemeral_state: bool,
    #[serde(default = "default_bind_address")]
    pub bind_address: String,
}

/// File name of the spent-token journal inside `state_dir`.
pub const SPENT_TOKENS_FILE: &str = "spent-tokens.jsonl";

/// Where spent tokens are recorded.
#[derive(Debug, PartialEq, Eq)]
pub enum StatePlan {
    Durable { journal: PathBuf },
    Ephemeral,
}

/// How the service listens.
#[derive(Debug, PartialEq, Eq)]
pub enum Transport {
    Tls { certificate: PathBuf, key: PathBuf },
    InsecureHttp,
}

impl IssuerConfig {
    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        let contents = std::fs::read_to_string(path)
            .map_err(|err| format!("could not read issuer config {}: {err}", path.display()))?;
        serde_yaml::from_str(&contents).map_err(|err| format!("invalid issuer config: {err}"))
    }

    /// Decide how to listen, refusing an ambiguous or cleartext-by-accident
    /// configuration.
    pub fn transport(&self) -> Result<Transport, String> {
        match (&self.tls_certificate_path, &self.tls_key_path, self.allow_insecure_http) {
            (Some(certificate), Some(key), false) => Ok(Transport::Tls {
                certificate: certificate.clone(),
                key: key.clone(),
            }),
            (Some(_), Some(_), true) => Err(
                "allow_insecure_http cannot be combined with tls_certificate_path/tls_key_path".into(),
            ),
            (Some(_), None, _) | (None, Some(_), _) => {
                Err("tls_certificate_path and tls_key_path must be set together".into())
            }
            (None, None, true) => Ok(Transport::InsecureHttp),
            (None, None, false) => Err(
                "TLS is required: set tls_certificate_path and tls_key_path (allow_insecure_http: true \
                 is only for a throwaway local run)"
                    .into(),
            ),
        }
    }

    /// Decide where spent tokens live, refusing a configuration that would
    /// silently lose them.
    pub fn state(&self) -> Result<StatePlan, String> {
        match (&self.state_dir, self.allow_ephemeral_state) {
            (Some(dir), false) => Ok(StatePlan::Durable {
                journal: dir.join(SPENT_TOKENS_FILE),
            }),
            (Some(_), true) => Err("state_dir and allow_ephemeral_state: true contradict each other".into()),
            (None, true) => Ok(StatePlan::Ephemeral),
            (None, false) => Err(
                "durable state is required: set state_dir so spent bootstrap tokens survive a restart \
                 (allow_ephemeral_state: true is only for a throwaway local run)"
                    .into(),
            ),
        }
    }

    pub fn issuer_policy(&self) -> Result<light_identity_issuer::IssuerPolicy, String> {
        let mut policy = light_identity_issuer::IssuerPolicy::new();
        for (env_tag, env) in &self.environments {
            if env.leaf_lifetime_seconds == 0 {
                return Err(format!(
                    "environments.{env_tag}.leaf_lifetime_seconds must be greater than zero"
                ));
            }
            if env.renew_lead_seconds >= env.leaf_lifetime_seconds {
                return Err(format!(
                    "environments.{env_tag}.renew_lead_seconds must be less than leaf_lifetime_seconds"
                ));
            }
            let lifetime = Duration::from_secs(env.leaf_lifetime_seconds);
            let lifetime = time::Duration::try_from(lifetime).map_err(|_| {
                format!("environments.{env_tag}.leaf_lifetime_seconds is too large")
            })?;
            if time::OffsetDateTime::now_utc()
                .checked_add(lifetime)
                .is_none()
            {
                return Err(format!(
                    "environments.{env_tag}.leaf_lifetime_seconds is too large"
                ));
            }
            policy = policy.with_env(
                env_tag,
                light_identity_issuer::EnvPolicy::new(
                    Duration::from_secs(env.leaf_lifetime_seconds),
                    Duration::from_secs(env.renew_lead_seconds),
                ),
            );
        }
        Ok(policy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(extra: &str) -> IssuerConfig {
        serde_yaml::from_str(&format!(
            "ca_certificate_path: /pki/ca.pem\nca_key_path: /pki/ca.key\njwks_url: https://o/keys\n\
             portal_token_issuer: i\nportal_token_audience: a\nenvironments: {{}}\n{extra}"
        ))
        .expect("config parses")
    }

    #[test]
    fn shipped_example_is_a_complete_accepted_configuration() {
        let c: IssuerConfig = serde_yaml::from_str(include_str!("../config/issuer.yml.example"))
            .expect("example deserializes");
        assert!(matches!(c.transport(), Ok(Transport::Tls { .. })));
        assert!(matches!(c.state(), Ok(StatePlan::Durable { .. })));
        assert_eq!(
            c.issuer_policy()
                .unwrap()
                .policy_for("loc")
                .unwrap()
                .renew_lead,
            Duration::from_secs(86_400)
        );
        assert!(c.jwks_url.ends_with("/keys"));
    }

    #[test]
    fn tls_settings_select_tls() {
        let c = config("tls_certificate_path: /tls/cert.pem\ntls_key_path: /tls/key.pem\n");
        assert_eq!(
            c.transport().unwrap(),
            Transport::Tls {
                certificate: "/tls/cert.pem".into(),
                key: "/tls/key.pem".into()
            }
        );
    }

    #[test]
    fn with_no_tls_settings_the_service_refuses_to_start() {
        let error = config("").transport().unwrap_err();
        assert!(error.contains("TLS is required"), "{error}");
    }

    #[test]
    fn durable_state_is_required_unless_explicitly_ephemeral() {
        assert_eq!(
            config("state_dir: /data\n").state().unwrap(),
            StatePlan::Durable {
                journal: "/data/spent-tokens.jsonl".into()
            }
        );
        assert_eq!(
            config("allow_ephemeral_state: true\n").state().unwrap(),
            StatePlan::Ephemeral
        );
        assert!(
            config("")
                .state()
                .unwrap_err()
                .contains("durable state is required")
        );
        assert!(
            config("state_dir: /data\nallow_ephemeral_state: true\n")
                .state()
                .is_err()
        );
    }

    #[test]
    fn plain_http_needs_an_explicit_opt_in() {
        assert_eq!(
            config("allow_insecure_http: true\n").transport().unwrap(),
            Transport::InsecureHttp
        );
    }

    #[test]
    fn half_a_tls_configuration_is_an_error() {
        assert!(
            config("tls_certificate_path: /tls/cert.pem\n")
                .transport()
                .is_err()
        );
        assert!(config("tls_key_path: /tls/key.pem\n").transport().is_err());
    }

    #[test]
    fn tls_settings_and_the_insecure_opt_in_together_are_ambiguous() {
        let c = config("tls_certificate_path: /a\ntls_key_path: /b\nallow_insecure_http: true\n");
        assert!(c.transport().is_err());
    }

    #[test]
    fn certificate_lifetime_policy_must_be_usable() {
        for (lifetime, lead) in [(0, 0), (60, 60), (u64::MAX, 1)] {
            let mut c = config("");
            c.environments.insert(
                "loc".into(),
                EnvPolicyConfig {
                    leaf_lifetime_seconds: lifetime,
                    renew_lead_seconds: lead,
                },
            );
            assert!(c.issuer_policy().is_err(), "{lifetime}/{lead}");
        }
    }
}
