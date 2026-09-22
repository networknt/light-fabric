use std::collections::BTreeMap;
use std::time::Duration;

/// Leaf lifetime and renewal lead time for one deployment environment.
///
/// The lead time is read as "renewal must be complete by this long before
/// expiry," not "start attempting renewal at this point" — a caller's
/// renewal loop should begin retrying well before `renew_lead` is reached so
/// a transient failure has room to resolve. See the "Decisions" section of
/// `docs/src/design/workload-identity-issuance.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EnvPolicy {
    pub leaf_lifetime: Duration,
    pub renew_lead: Duration,
}

impl EnvPolicy {
    pub const fn new(leaf_lifetime: Duration, renew_lead: Duration) -> Self {
        Self {
            leaf_lifetime,
            renew_lead,
        }
    }

    /// The recorded pilot default: `portal-config-loc`, a single VM.
    pub const fn pilot_default() -> Self {
        Self::new(
            Duration::from_secs(10 * 86_400),
            Duration::from_secs(86_400),
        )
    }

    /// The recorded production default. Deliberately 1 day, not 1 hour:
    /// revocation is enforced by the revocation list at renewal and by
    /// CA-chain/attribute checking at every handshake, not by short
    /// lifetime, so lifetime can be tuned for issuer load rather than for
    /// revocation responsiveness. See "Decisions" in the design doc.
    pub const fn production_default() -> Self {
        Self::new(Duration::from_secs(86_400), Duration::from_secs(3_600))
    }
}

/// Per-`envTag` policy table, matching the existing `loc`/`dev`/`prod`
/// convention already used for `startup.yml`'s `envTag`.
#[derive(Clone, Debug, Default)]
pub struct IssuerPolicy {
    by_env: BTreeMap<String, EnvPolicy>,
}

impl IssuerPolicy {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_env(mut self, env_tag: impl Into<String>, policy: EnvPolicy) -> Self {
        self.by_env.insert(env_tag.into(), policy);
        self
    }

    /// The recorded defaults for `loc` (pilot) and `prod`, so a caller does
    /// not have to restate the figures from the design doc.
    pub fn with_recorded_defaults() -> Self {
        Self::new()
            .with_env("loc", EnvPolicy::pilot_default())
            .with_env("prod", EnvPolicy::production_default())
    }

    pub fn policy_for(&self, env_tag: &str) -> Option<EnvPolicy> {
        self.by_env.get(env_tag).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recorded_defaults_match_the_design_doc() {
        let policy = IssuerPolicy::with_recorded_defaults();

        let loc = policy.policy_for("loc").expect("loc policy present");
        assert_eq!(loc.leaf_lifetime, Duration::from_secs(10 * 86_400));
        assert_eq!(loc.renew_lead, Duration::from_secs(86_400));

        let prod = policy.policy_for("prod").expect("prod policy present");
        assert_eq!(prod.leaf_lifetime, Duration::from_secs(86_400));
        assert_eq!(prod.renew_lead, Duration::from_secs(3_600));

        assert!(policy.policy_for("nonexistent-env").is_none());
    }
}
