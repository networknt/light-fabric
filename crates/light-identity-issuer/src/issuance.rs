use time::OffsetDateTime;
use uuid::Uuid;

use crate::renewal::{RenewalReplayGuard, verify_renewal_proof};
use crate::signer::verify_issued_by;
use crate::{
    CaSigner, IssuedCertificate, IssuerError, IssuerPolicy, LeafIdentity, RenewalProof,
    RevocationList,
};

/// The credential a caller presents for **first** issuance only. Real
/// verification of these — checking a Portal-issued long-lived token
/// against `light-oauth`, or redeeming a pairing code — plugs in via
/// `FirstIssuanceAuthorizer`; this crate defines the seam, and `bootstrap`
/// provides the implementations.
#[derive(Clone, Debug)]
pub enum BootstrapCredential {
    /// The long-lived token a service workload already obtains from Portal
    /// to authenticate to config-server and Controller today.
    PortalToken(String),
    /// A short-lived, single-use grant redeemed via Portal-initiated
    /// pairing, for a CLI install with no pre-provisioned service token.
    PairingGrant(String),
}

/// What a verified bootstrap credential authorizes: the identity to bind into
/// the certificate, decided by the issuer from the credential, never from what
/// the caller says about itself.
#[derive(Clone, Debug)]
pub struct Authorized {
    pub identity: LeafIdentity,
    /// Key the authorizer recorded as spent for this credential, if it tracks
    /// single use. Handed back to `refund` if signing then fails.
    pub spent_key: Option<String>,
}

/// Verifies a `BootstrapCredential` and resolves the identity it authorizes.
///
/// The service ID, role and install ID come from the credential and
/// server-side configuration. A caller cannot choose them, so a credential for
/// one service cannot be exchanged for a certificate naming another.
pub trait FirstIssuanceAuthorizer: Send + Sync {
    /// Verify `credential` for `env_tag` and return the identity it
    /// authorizes. May consume single-use state; if it does, it records the
    /// key in `Authorized::spent_key` so `refund` can undo it.
    fn authorize(
        &self,
        credential: &BootstrapCredential,
        env_tag: &str,
    ) -> Result<Authorized, IssuerError>;

    /// Undo a spend recorded by `authorize` after the issuer failed to sign,
    /// so a malformed CSR does not burn a single-use credential. No-op by
    /// default.
    fn refund(&self, _credential: &BootstrapCredential, _authorized: &Authorized) {}
}

/// Accepts any credential. Exists only for tests and for standing up a
/// throwaway issuer; never wire this into a real deployment.
#[doc(hidden)]
pub struct AllowAllForTests {
    identity: LeafIdentity,
    fresh_install_each_time: bool,
}

impl AllowAllForTests {
    /// Authorize any credential as `service_id`/`role`, with a new install ID
    /// each time.
    pub fn new(service_id: impl Into<String>, role: impl Into<String>) -> Self {
        Self {
            identity: LeafIdentity {
                env_tag: None,
                install_id: Uuid::nil(),
                service_id: service_id.into(),
                role: role.into(),
            },
            fresh_install_each_time: true,
        }
    }

    /// Authorize any credential as exactly `identity`.
    pub fn fixed(identity: LeafIdentity) -> Self {
        Self {
            identity,
            fresh_install_each_time: false,
        }
    }
}

impl FirstIssuanceAuthorizer for AllowAllForTests {
    fn authorize(
        &self,
        _credential: &BootstrapCredential,
        _env_tag: &str,
    ) -> Result<Authorized, IssuerError> {
        let mut identity = self.identity.clone();
        if self.fresh_install_each_time {
            identity.install_id = Uuid::new_v4();
        }
        Ok(Authorized {
            identity,
            spent_key: None,
        })
    }
}

/// Combines CA signing, environment-scoped lifetime policy, first-issuance
/// authorization, and revocation checking into the two operations a
/// workload actually performs: obtain a first certificate, and renew it.
pub struct WorkloadIssuer<S, R, A> {
    signer: S,
    policy: IssuerPolicy,
    revocation: R,
    authorizer: A,
    renewals: RenewalReplayGuard,
}

impl<S, R, A> WorkloadIssuer<S, R, A>
where
    S: CaSigner,
    R: RevocationList,
    A: FirstIssuanceAuthorizer,
{
    pub fn new(signer: S, policy: IssuerPolicy, revocation: R, authorizer: A) -> Self {
        Self {
            signer,
            policy,
            revocation,
            authorizer,
            renewals: RenewalReplayGuard::default(),
        }
    }

    /// First issuance: the only call that accepts a `BootstrapCredential`.
    /// Every subsequent renewal uses `renew`, which accepts only proof of
    /// possession of the previously issued certificate's key — never this
    /// credential again, so it cannot become a standing renewal secret.
    ///
    /// The identity in the certificate is whatever the authorizer resolved
    /// from the credential. The caller supplies only the CSR.
    pub fn issue_first(
        &self,
        env_tag: &str,
        credential: BootstrapCredential,
        csr_der: &[u8],
    ) -> Result<IssuedCertificate, IssuerError> {
        let policy = self
            .policy
            .policy_for(env_tag)
            .ok_or_else(|| IssuerError::UnknownEnvironment(env_tag.to_string()))?;

        let authorized = self.authorizer.authorize(&credential, env_tag)?;

        if self.revocation.is_revoked(&authorized.identity.install_id) {
            self.authorizer.refund(&credential, &authorized);
            return Err(IssuerError::Revoked(authorized.identity.install_id));
        }

        // Bind the environment the credential was authorized for into the certificate, so renewal
        // cannot later choose a different one (and with it a different lifetime policy).
        let mut identity = authorized.identity.clone();
        identity.env_tag = Some(env_tag.to_string());
        match self.signer.sign(csr_der, &identity, policy.leaf_lifetime) {
            Ok(mut issued) => {
                issued.renew_at = issued.not_after - policy.renew_lead;
                Ok(issued)
            }
            Err(error) => {
                self.authorizer.refund(&credential, &authorized);
                Err(error)
            }
        }
    }

    /// Renewal, authenticated by the **old key and certificate only**.
    ///
    /// The presented certificate must carry a valid signature from this
    /// issuer's CA, and `proof` must be a signature over the new CSR made with
    /// that certificate's private key, with a fresh timestamp and an unused
    /// nonce. The identity is recovered from the verified certificate, never
    /// from the request. The presented certificate must still be valid: managed workloads
    /// renew before `renew_at`, and an expired identity must enroll again. `csr_der` should
    /// carry a newly generated key, so renewal replaces the key pair as well.
    pub fn renew(
        &self,
        env_tag: &str,
        presented_certificate_der: &[u8],
        csr_der: &[u8],
        proof: &RenewalProof,
    ) -> Result<IssuedCertificate, IssuerError> {
        let policy = self
            .policy
            .policy_for(env_tag)
            .ok_or_else(|| IssuerError::UnknownEnvironment(env_tag.to_string()))?;

        // Without this, anyone could hand-build a certificate with a
        // plausible identity in its SAN.
        verify_issued_by(presented_certificate_der, self.signer.ca_certificate_der())?;

        let (identity, not_after) = LeafIdentity::from_certificate_der(presented_certificate_der)?;

        // The environment is the one this certificate was issued for, not one the request picks: a
        // production holder must not renew into a development policy's longer lifetime.
        if identity.env_tag.as_deref() != Some(env_tag) {
            return Err(IssuerError::EnvironmentMismatch);
        }
        if not_after <= OffsetDateTime::now_utc() {
            return Err(IssuerError::PresentedCertificateExpired);
        }

        if self.revocation.is_revoked(&identity.install_id) {
            return Err(IssuerError::Revoked(identity.install_id));
        }

        let now = OffsetDateTime::now_utc().unix_timestamp();
        verify_renewal_proof(presented_certificate_der, env_tag, csr_der, proof, now)?;

        // Recorded only now, after every other check, so a request that fails
        // for any other reason cannot burn a nonce.
        if !self.renewals.record(&proof.nonce, now) {
            return Err(IssuerError::ReplayedRenewal);
        }

        let mut issued = self.signer.sign(csr_der, &identity, policy.leaf_lifetime)?;
        issued.renew_at = issued.not_after - policy.renew_lead;
        Ok(issued)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CaMaterial, EnvPolicy, InMemoryRevocationList, IssuerPolicy, MAX_PROOF_SKEW_SECONDS,
        OnDiskCaSigner, renewal_message,
    };
    use rcgen::{
        CertificateParams, KeyPair, PKCS_ECDSA_P384_SHA384, PKCS_ED25519, SanType, SigningKey,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    type TestIssuer = WorkloadIssuer<OnDiskCaSigner, Arc<InMemoryRevocationList>, AllowAllForTests>;

    struct Fixture {
        issuer: TestIssuer,
        revocation: Arc<InMemoryRevocationList>,
        ca_pem: String,
    }

    fn csr_for(key: &KeyPair) -> Vec<u8> {
        CertificateParams::new(Vec::<String>::new())
            .expect("leaf params")
            .serialize_request(key)
            .expect("csr")
            .der()
            .to_vec()
    }

    fn fresh_key_and_csr() -> (KeyPair, Vec<u8>) {
        let key = KeyPair::generate().expect("leaf key");
        let csr = csr_for(&key);
        (key, csr)
    }

    fn fixture() -> Fixture {
        let (ca_pem, _ca_key_pem, material) = CaMaterial::generate_for_tests();
        let revocation = Arc::new(InMemoryRevocationList::new());
        let issuer = WorkloadIssuer::new(
            OnDiskCaSigner::new(material),
            IssuerPolicy::new()
                .with_env(
                    "loc",
                    EnvPolicy::new(Duration::from_secs(3_600), Duration::from_secs(600)),
                )
                // A longer policy than "loc": what a holder of a "loc" certificate must not reach.
                .with_env(
                    "long",
                    EnvPolicy::new(Duration::from_secs(864_000), Duration::from_secs(86_400)),
                ),
            Arc::clone(&revocation),
            AllowAllForTests::new("com.networknt.light-cli-1.0.0", "cli"),
        );
        Fixture {
            issuer,
            revocation,
            ca_pem,
        }
    }

    fn now() -> i64 {
        OffsetDateTime::now_utc().unix_timestamp()
    }

    fn nonce() -> String {
        Uuid::new_v4().simple().to_string()
    }

    fn proof_at(
        old_key: &KeyPair,
        env_tag: &str,
        new_csr: &[u8],
        timestamp: i64,
        nonce: &str,
    ) -> RenewalProof {
        RenewalProof {
            timestamp,
            nonce: nonce.to_string(),
            signature: old_key
                .sign(&renewal_message(env_tag, new_csr, timestamp, nonce))
                .expect("sign proof"),
        }
    }

    fn proof(old_key: &KeyPair, env_tag: &str, new_csr: &[u8]) -> RenewalProof {
        proof_at(old_key, env_tag, new_csr, now(), &nonce())
    }

    fn token() -> BootstrapCredential {
        BootstrapCredential::PortalToken("test-token".into())
    }

    fn pem_to_der(pem: &str) -> Vec<u8> {
        x509_parser::pem::parse_x509_pem(pem.as_bytes())
            .expect("pem")
            .1
            .contents
    }

    fn first_issue(fx: &Fixture, env_tag: &str, key: &KeyPair) -> IssuedCertificate {
        fx.issuer
            .issue_first(env_tag, token(), &csr_for(key))
            .expect("first issuance succeeds")
    }

    /// Phase 0's exit gate: first issuance, then renewal that replaces the key.
    #[test]
    fn first_issuance_then_renewal_replaces_the_key() {
        let fx = fixture();
        let (old_key, _) = fresh_key_and_csr();
        let first = first_issue(&fx, "loc", &old_key);
        assert_eq!(first.identity.role, "cli");
        assert_eq!(first.identity.service_id, "com.networknt.light-cli-1.0.0");

        let (_new_key, new_csr) = fresh_key_and_csr();
        let renewed = fx
            .issuer
            .renew(
                "loc",
                &first.certificate_der,
                &new_csr,
                &proof(&old_key, "loc", &new_csr),
            )
            .expect("renewal succeeds on proof of possession of the old key");

        assert_eq!(renewed.identity, first.identity);
        assert_ne!(renewed.certificate_der, first.certificate_der);
    }

    #[test]
    fn issued_certificate_carries_the_ca_it_verifies_against() {
        let fx = fixture();
        let (key, _) = fresh_key_and_csr();
        let issued = first_issue(&fx, "loc", &key);

        assert_eq!(issued.ca_certificate_pem.trim(), fx.ca_pem.trim());
        verify_issued_by(
            &issued.certificate_der,
            &pem_to_der(&issued.ca_certificate_pem),
        )
        .expect("leaf verifies against the returned CA certificate");
    }

    #[test]
    fn renewal_rejects_an_expired_certificate() {
        let fx = fixture();
        let (old_key, csr) = fresh_key_and_csr();
        let expired = fx
            .issuer
            .signer
            .sign(
                &csr,
                &LeafIdentity {
                    env_tag: Some("loc".into()),
                    install_id: Uuid::new_v4(),
                    service_id: "com.networknt.light-cli-1.0.0".into(),
                    role: "cli".into(),
                },
                Duration::from_nanos(1),
            )
            .expect("short-lived certificate signs");

        let (_, certificate) =
            x509_parser::parse_x509_certificate(&expired.certificate_der).expect("parse");
        assert!(
            certificate.validity().not_after.to_datetime() <= OffsetDateTime::now_utc(),
            "the fixture certificate must already be expired for this test to mean anything"
        );

        let (_new_key, new_csr) = fresh_key_and_csr();
        let result = fx.issuer.renew(
            "loc",
            &expired.certificate_der,
            &new_csr,
            &proof(&old_key, "loc", &new_csr),
        );
        assert!(matches!(
            result,
            Err(IssuerError::PresentedCertificateExpired)
        ));
    }

    #[test]
    fn a_certificate_renews_only_for_the_environment_it_was_issued_for() {
        let fx = fixture();
        let (old_key, _) = fresh_key_and_csr();
        let first = first_issue(&fx, "loc", &old_key);
        assert_eq!(first.identity.env_tag.as_deref(), Some("loc"));
        let (from_certificate, _) =
            LeafIdentity::from_certificate_der(&first.certificate_der).expect("parses");
        assert_eq!(
            from_certificate.env_tag.as_deref(),
            Some("loc"),
            "the environment is in the certificate"
        );

        // Asking for another environment, whose policy is longer, is refused, with a valid proof for
        // exactly what was asked.
        let (_new_key, new_csr) = fresh_key_and_csr();
        let shared_nonce = nonce();
        let stolen = fx.issuer.renew(
            "long",
            &first.certificate_der,
            &new_csr,
            &proof_at(&old_key, "long", &new_csr, now(), &shared_nonce),
        );
        assert!(
            matches!(stolen, Err(IssuerError::EnvironmentMismatch)),
            "renewing into another environment"
        );

        // The refusal did not burn a nonce, and the right environment keeps its own lifetime.
        let renewed = fx
            .issuer
            .renew(
                "loc",
                &first.certificate_der,
                &new_csr,
                &proof_at(&old_key, "loc", &new_csr, now(), &shared_nonce),
            )
            .expect("renewal in the same environment");
        assert_eq!(renewed.identity.env_tag.as_deref(), Some("loc"));
        let lifetime = renewed.not_after - OffsetDateTime::now_utc();
        assert!(
            lifetime <= time::Duration::seconds(3_600),
            "lifetime of the loc policy, not long: {lifetime}"
        );
    }

    #[test]
    fn a_certificate_that_names_no_environment_cannot_be_renewed() {
        let fx = fixture();
        let (old_key, csr) = fresh_key_and_csr();
        let without = fx
            .issuer
            .signer
            .sign(
                &csr,
                &LeafIdentity {
                    env_tag: None,
                    install_id: Uuid::new_v4(),
                    service_id: "com.networknt.light-cli-1.0.0".into(),
                    role: "cli".into(),
                },
                Duration::from_secs(3_600),
            )
            .expect("signs");
        let (_new_key, new_csr) = fresh_key_and_csr();
        let result = fx.issuer.renew(
            "loc",
            &without.certificate_der,
            &new_csr,
            &proof(&old_key, "loc", &new_csr),
        );
        assert!(matches!(result, Err(IssuerError::EnvironmentMismatch)));
    }

    #[test]
    fn renewal_supports_p384_and_ed25519_keys() {
        for algorithm in [&PKCS_ECDSA_P384_SHA384, &PKCS_ED25519] {
            let fx = fixture();
            let old_key = KeyPair::generate_for(algorithm).expect("key");
            let first = first_issue(&fx, "loc", &old_key);

            let (_new_key, new_csr) = fresh_key_and_csr();
            fx.issuer
                .renew(
                    "loc",
                    &first.certificate_der,
                    &new_csr,
                    &proof(&old_key, "loc", &new_csr),
                )
                .expect("renewal with this key algorithm succeeds");
        }
    }

    #[test]
    fn renewal_rejects_a_certificate_signed_by_a_different_ca() {
        let fx = fixture();
        let (_other_pem, _other_key_pem, other_material) = CaMaterial::generate_for_tests();
        let (key, csr) = fresh_key_and_csr();
        let foreign = OnDiskCaSigner::new(other_material)
            .sign(
                &csr,
                &LeafIdentity {
                    env_tag: None,
                    install_id: Uuid::new_v4(),
                    service_id: "com.networknt.light-cli-1.0.0".into(),
                    role: "cli".into(),
                },
                Duration::from_secs(3_600),
            )
            .expect("foreign CA signs its own certificate");

        let (_new_key, new_csr) = fresh_key_and_csr();
        let result = fx.issuer.renew(
            "loc",
            &foreign.certificate_der,
            &new_csr,
            &proof(&key, "loc", &new_csr),
        );
        assert!(matches!(
            result,
            Err(IssuerError::PresentedCertificateNotIssuedByThisCa)
        ));
    }

    #[test]
    fn renewal_rejects_a_self_made_certificate_with_a_plausible_identity() {
        let fx = fixture();
        let key = KeyPair::generate().expect("key");
        let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
        params.subject_alt_names = vec![SanType::URI(
            format!(
                "spiffe://lightapi.local/cli/com.networknt.light-cli-1.0.0/{}",
                Uuid::new_v4()
            )
            .try_into()
            .expect("ia5"),
        )];
        let forged = params.self_signed(&key).expect("self-signed");

        let (_new_key, new_csr) = fresh_key_and_csr();
        let result = fx
            .issuer
            .renew("loc", forged.der(), &new_csr, &proof(&key, "loc", &new_csr));
        assert!(matches!(
            result,
            Err(IssuerError::PresentedCertificateNotIssuedByThisCa)
        ));
    }

    #[test]
    fn renewal_rejects_a_proof_made_with_a_different_key() {
        let fx = fixture();
        let (old_key, _) = fresh_key_and_csr();
        let first = first_issue(&fx, "loc", &old_key);

        let (_new_key, new_csr) = fresh_key_and_csr();
        let (impostor, _) = fresh_key_and_csr();
        let result = fx.issuer.renew(
            "loc",
            &first.certificate_der,
            &new_csr,
            &proof(&impostor, "loc", &new_csr),
        );
        assert!(matches!(result, Err(IssuerError::PossessionProofInvalid)));
    }

    #[test]
    fn renewal_rejects_a_proof_made_for_a_different_csr() {
        let fx = fixture();
        let (old_key, _) = fresh_key_and_csr();
        let first = first_issue(&fx, "loc", &old_key);

        let (_key_a, csr_a) = fresh_key_and_csr();
        let (_key_b, csr_b) = fresh_key_and_csr();
        let result = fx.issuer.renew(
            "loc",
            &first.certificate_der,
            &csr_b,
            &proof(&old_key, "loc", &csr_a),
        );
        assert!(matches!(result, Err(IssuerError::PossessionProofInvalid)));
    }

    #[test]
    fn renewal_rejects_a_proof_made_for_a_different_environment() {
        let fx = fixture();
        let (old_key, _) = fresh_key_and_csr();
        let first = first_issue(&fx, "loc", &old_key);

        let (_new_key, new_csr) = fresh_key_and_csr();
        let result = fx.issuer.renew(
            "loc",
            &first.certificate_der,
            &new_csr,
            &proof(&old_key, "instant", &new_csr),
        );
        assert!(matches!(result, Err(IssuerError::PossessionProofInvalid)));
    }

    #[test]
    fn renewal_rejects_a_replayed_request() {
        let fx = fixture();
        let (old_key, _) = fresh_key_and_csr();
        let first = first_issue(&fx, "loc", &old_key);

        let (_new_key, new_csr) = fresh_key_and_csr();
        let request = proof(&old_key, "loc", &new_csr);
        fx.issuer
            .renew("loc", &first.certificate_der, &new_csr, &request)
            .expect("first use succeeds");
        let replay = fx
            .issuer
            .renew("loc", &first.certificate_der, &new_csr, &request);
        assert!(matches!(replay, Err(IssuerError::ReplayedRenewal)));
    }

    #[test]
    fn renewal_rejects_a_stale_or_future_proof() {
        let fx = fixture();
        let (old_key, _) = fresh_key_and_csr();
        let first = first_issue(&fx, "loc", &old_key);
        let (_new_key, new_csr) = fresh_key_and_csr();

        for offset in [-(MAX_PROOF_SKEW_SECONDS + 60), MAX_PROOF_SKEW_SECONDS + 60] {
            let result = fx.issuer.renew(
                "loc",
                &first.certificate_der,
                &new_csr,
                &proof_at(&old_key, "loc", &new_csr, now() + offset, &nonce()),
            );
            assert!(
                matches!(result, Err(IssuerError::PossessionProofStale)),
                "offset {offset}"
            );
        }
    }

    #[test]
    fn a_rejected_renewal_does_not_burn_its_nonce() {
        let fx = fixture();
        let (old_key, _) = fresh_key_and_csr();
        let first = first_issue(&fx, "loc", &old_key);
        let (_new_key, new_csr) = fresh_key_and_csr();
        let (impostor, _) = fresh_key_and_csr();

        let shared_nonce = nonce();
        let bad = fx.issuer.renew(
            "loc",
            &first.certificate_der,
            &new_csr,
            &proof_at(&impostor, "loc", &new_csr, now(), &shared_nonce),
        );
        assert!(matches!(bad, Err(IssuerError::PossessionProofInvalid)));

        // The legitimate holder can still use that nonce: only a verified
        // request records one.
        fx.issuer
            .renew(
                "loc",
                &first.certificate_der,
                &new_csr,
                &proof_at(&old_key, "loc", &new_csr, now(), &shared_nonce),
            )
            .expect("nonce was not burned by the failed attempt");
    }

    #[test]
    fn revoked_install_is_denied_at_renewal() {
        let fx = fixture();
        let (old_key, _) = fresh_key_and_csr();
        let first = first_issue(&fx, "loc", &old_key);
        fx.revocation.revoke(first.identity.install_id);

        let (_new_key, new_csr) = fresh_key_and_csr();
        let result = fx.issuer.renew(
            "loc",
            &first.certificate_der,
            &new_csr,
            &proof(&old_key, "loc", &new_csr),
        );
        assert!(matches!(result, Err(IssuerError::Revoked(_))));
    }

    #[test]
    fn revoked_install_id_is_denied_at_first_issuance() {
        let (_ca_pem, _ca_key_pem, material) = CaMaterial::generate_for_tests();
        let identity = LeafIdentity {
            env_tag: None,
            install_id: Uuid::new_v4(),
            service_id: "com.networknt.agent.codex-personal-workflow-1.0.0".into(),
            role: "agent".into(),
        };
        let revocation = InMemoryRevocationList::new();
        revocation.revoke(identity.install_id);
        let issuer = WorkloadIssuer::new(
            OnDiskCaSigner::new(material),
            IssuerPolicy::with_recorded_defaults(),
            revocation,
            AllowAllForTests::fixed(identity),
        );

        let (_key, csr) = fresh_key_and_csr();
        let result = issuer.issue_first("loc", token(), &csr);
        assert!(matches!(result, Err(IssuerError::Revoked(_))));
    }

    #[test]
    fn unknown_environment_is_rejected_on_both_paths() {
        let fx = fixture();
        let (key, csr) = fresh_key_and_csr();
        assert!(matches!(
            fx.issuer.issue_first("no-such-env", token(), &csr),
            Err(IssuerError::UnknownEnvironment(_))
        ));

        let first = first_issue(&fx, "loc", &key);
        assert!(matches!(
            fx.issuer.renew(
                "no-such-env",
                &first.certificate_der,
                &csr,
                &proof(&key, "no-such-env", &csr)
            ),
            Err(IssuerError::UnknownEnvironment(_))
        ));
    }

    #[test]
    fn an_identity_that_cannot_round_trip_through_the_san_is_rejected() {
        let (_ca_pem, _ca_key_pem, material) = CaMaterial::generate_for_tests();
        let issuer = WorkloadIssuer::new(
            OnDiskCaSigner::new(material),
            IssuerPolicy::with_recorded_defaults(),
            InMemoryRevocationList::new(),
            AllowAllForTests::new("com.networknt/other-service", "cli"),
        );
        let (_key, csr) = fresh_key_and_csr();
        assert!(matches!(
            issuer.issue_first("loc", token(), &csr),
            Err(IssuerError::InvalidIdentity(_))
        ));
    }

    /// Records refunds so the test can see the spend being undone.
    struct Recording {
        refunds: Arc<AtomicUsize>,
    }

    impl FirstIssuanceAuthorizer for Recording {
        fn authorize(
            &self,
            _credential: &BootstrapCredential,
            _env_tag: &str,
        ) -> Result<Authorized, IssuerError> {
            Ok(Authorized {
                identity: LeafIdentity {
                    env_tag: None,
                    install_id: Uuid::new_v4(),
                    service_id: "com.networknt.light-cli-1.0.0".into(),
                    role: "cli".into(),
                },
                spent_key: Some("jti-1".into()),
            })
        }

        fn refund(&self, _credential: &BootstrapCredential, authorized: &Authorized) {
            assert_eq!(authorized.spent_key.as_deref(), Some("jti-1"));
            self.refunds.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn a_failed_signing_refunds_the_spent_credential() {
        let (_ca_pem, _ca_key_pem, material) = CaMaterial::generate_for_tests();
        let refunds = Arc::new(AtomicUsize::new(0));
        let issuer = WorkloadIssuer::new(
            OnDiskCaSigner::new(material),
            IssuerPolicy::with_recorded_defaults(),
            InMemoryRevocationList::new(),
            Recording {
                refunds: Arc::clone(&refunds),
            },
        );

        let result = issuer.issue_first("loc", token(), b"this is not a csr");
        assert!(matches!(result, Err(IssuerError::InvalidCsr(_))));
        assert_eq!(refunds.load(Ordering::SeqCst), 1, "spend was refunded");

        let (_key, csr) = fresh_key_and_csr();
        issuer
            .issue_first("loc", token(), &csr)
            .expect("a good CSR still succeeds");
        assert_eq!(refunds.load(Ordering::SeqCst), 1, "no refund on success");
    }
}
