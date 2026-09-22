use std::time::Duration;

use rcgen::{
    CertificateParams, CertificateSigningRequestParams, DistinguishedName, DnType, Issuer, KeyPair,
    KeyUsagePurpose, PKCS_ECDSA_P256_SHA256, PKCS_ECDSA_P384_SHA384, PKCS_ED25519, PublicKeyData,
    SanType,
};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::IssuerError;

/// The attributes bound into a leaf certificate. Encoded as a
/// `spiffe://`-shaped URI SAN, matching the convention already used by
/// `portal-config-loc/all-in-lt/workflow-actions/prepare.py`'s hand-minted
/// certificates, so a Gateway-side matcher can recognize either.
///
/// This is the substance of the CA-based peer trust variant proposed in
/// the Light CLI design: the Gateway-side rule this leaf satisfies is "any
/// certificate chaining to this CA whose URI SAN names this role and
/// service ID," not an exact-leaf fingerprint, so fleet size does not grow
/// the policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeafIdentity {
    pub install_id: Uuid,
    pub service_id: String,
    pub role: String,
    /// The environment this certificate was issued for, written into the subject (`O=`) at first
    /// issuance and read back at renewal, which must be for the same environment. `None` while an
    /// authorizer is still building the identity, and for a certificate that carries none.
    pub env_tag: Option<String>,
}

impl LeafIdentity {
    /// The certificate subject: `CN=<service id>, OU=<role>`, decided here and
    /// never taken from the CSR. It is for humans reading a certificate; trust
    /// decisions use the URI SAN, which also carries the install ID. The common
    /// name is cut to 64 characters, the X.520 limit; the SAN keeps the full
    /// service ID.
    fn subject(&self) -> DistinguishedName {
        let mut subject = DistinguishedName::new();
        subject.push(
            DnType::CommonName,
            self.service_id.chars().take(64).collect::<String>(),
        );
        subject.push(DnType::OrganizationalUnitName, self.role.clone());
        if let Some(env_tag) = &self.env_tag {
            subject.push(DnType::OrganizationName, env_tag.clone());
        }
        subject
    }

    pub fn spiffe_uri(&self) -> String {
        format!(
            "spiffe://lightapi.local/{}/{}/{}",
            self.role, self.service_id, self.install_id
        )
    }

    /// Recover the identity this crate bound into a leaf, from the URI SAN
    /// of a certificate a caller presents at renewal. This is the
    /// inverse of `spiffe_uri`, and is the only way renewal identifies
    /// "who is asking," since a renewal request carries only certificate
    /// bytes, not the original `LeafIdentity` value.
    fn parse_spiffe_uri(uri: &str) -> Option<Self> {
        let rest = uri.strip_prefix("spiffe://lightapi.local/")?;
        let mut parts = rest.splitn(3, '/');
        let role = parts.next()?.to_string();
        let service_id = parts.next()?.to_string();
        let install_id = Uuid::parse_str(parts.next()?).ok()?;
        Some(Self {
            install_id,
            service_id,
            role,
            env_tag: None,
        })
    }

    /// Reject an identity that cannot round-trip through the URI SAN. Each
    /// segment must be non-empty ASCII made of alphanumerics, `.`, `-` and
    /// `_`, so no segment can contain the `/` separator or anything that
    /// would let one field be read as another when the SAN is parsed back.
    pub fn validate(&self) -> Result<(), IssuerError> {
        fn segment_ok(segment: &str) -> bool {
            !segment.is_empty()
                && segment.len() <= 255
                && segment
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        }
        if !segment_ok(&self.role) {
            return Err(IssuerError::InvalidIdentity(
                "role is not a valid SAN segment",
            ));
        }
        if !segment_ok(&self.service_id) {
            return Err(IssuerError::InvalidIdentity(
                "service id is not a valid SAN segment",
            ));
        }
        if self
            .env_tag
            .as_deref()
            .is_some_and(|env_tag| !segment_ok(env_tag))
        {
            return Err(IssuerError::InvalidIdentity(
                "environment tag is not a valid segment",
            ));
        }
        Ok(())
    }

    /// Extract the bound identity and expiry from a previously issued
    /// certificate, presented as DER at renewal time.
    ///
    /// This only *parses*. It does not establish that this issuer signed the
    /// certificate (see `verify_issued_by`) or that the caller holds its key
    /// (see the renewal proof). The expiry is returned for the renewal path to enforce.
    pub fn from_certificate_der(der: &[u8]) -> Result<(Self, OffsetDateTime), IssuerError> {
        let (_, certificate) = x509_parser::parse_x509_certificate(der)
            .map_err(|err| IssuerError::InvalidPresentedCertificate(err.to_string()))?;

        let not_after = certificate.validity().not_after.to_datetime();

        let uri = certificate
            .subject_alternative_name()
            .ok()
            .flatten()
            .and_then(|ext| {
                ext.value.general_names.iter().find_map(|name| match name {
                    x509_parser::extensions::GeneralName::URI(uri) => Some(uri.to_string()),
                    _ => None,
                })
            })
            .ok_or_else(|| {
                IssuerError::InvalidPresentedCertificate("no spiffe:// URI SAN present".to_string())
            })?;

        let mut identity = Self::parse_spiffe_uri(&uri).ok_or_else(|| {
            IssuerError::InvalidPresentedCertificate(format!(
                "URI SAN is not a recognized identity: {uri}"
            ))
        })?;
        identity.env_tag = certificate
            .subject()
            .iter_organization()
            .next()
            .and_then(|attribute| attribute.as_str().ok())
            .map(str::to_string);

        Ok((identity, not_after))
    }
}

/// Verify that `certificate_der` carries a valid signature from the CA whose
/// certificate is `ca_certificate_der`. This is the check that makes the
/// identity in a presented certificate's SAN trustworthy: without it, anyone
/// could hand-build a certificate with a plausible `spiffe://` SAN.
///
/// Validity dates are intentionally not examined here.
pub(crate) fn verify_issued_by(
    certificate_der: &[u8],
    ca_certificate_der: &[u8],
) -> Result<(), IssuerError> {
    let (_, certificate) = x509_parser::parse_x509_certificate(certificate_der)
        .map_err(|err| IssuerError::InvalidPresentedCertificate(err.to_string()))?;
    let (_, ca) = x509_parser::parse_x509_certificate(ca_certificate_der)
        .map_err(|_| IssuerError::PresentedCertificateNotIssuedByThisCa)?;
    certificate
        .verify_signature(Some(ca.public_key()))
        .map_err(|_| IssuerError::PresentedCertificateNotIssuedByThisCa)
}

/// A signed leaf certificate and the identity it was issued to, in the
/// shapes a caller needs to hand back over the wire and to check on the
/// next renewal.
pub struct IssuedCertificate {
    pub identity: LeafIdentity,
    pub certificate_der: Vec<u8>,
    pub certificate_pem: String,
    /// PEM of the issuing CA certificate. A client presents `[leaf, CA]` in
    /// its TLS handshake: the Gateway derives `issuer_digest` from the second
    /// certificate in the chain, so a leaf-only chain never matches `caTrust`.
    pub ca_certificate_pem: String,
    pub not_after: OffsetDateTime,
    /// Policy deadline at which the workload must renew this certificate.
    pub renew_at: OffsetDateTime,
}

/// CA signing authority, isolated behind this one narrow interface so that
/// moving custody from an on-disk key (`OnDiskCaSigner`) to AWS KMS or
/// Google Cloud KMS/HSM later only requires a new implementation of this
/// trait — no change to bootstrap, renewal, or any consuming service. See
/// "CA key custody" in the design doc's Decisions section.
pub trait CaSigner: Send + Sync {
    /// Sign a certificate signing request, binding it to `identity` and
    /// `lifetime`. The CSR's own subject/SAN/key-usage requests are
    /// discarded in favor of the values this signer sets, matching the
    /// dual-identity contract's requirement that trust attributes come from
    /// the issuer's decision, never from what a caller claims about itself.
    fn sign(
        &self,
        csr_der: &[u8],
        identity: &LeafIdentity,
        lifetime: Duration,
    ) -> Result<IssuedCertificate, IssuerError>;

    /// DER of the CA certificate this signer issues under. Used to verify that
    /// a certificate presented at renewal really came from this issuer.
    fn ca_certificate_der(&self) -> &[u8];

    /// PEM of the same CA certificate, returned to clients so they can present
    /// a full `[leaf, CA]` chain.
    fn ca_certificate_pem(&self) -> &str;
}

/// CA material loaded from disk: a CA certificate and its private key.
/// Custody stays a file on disk for Phase 0/1; the design doc records this
/// as the deliberate starting point, with a KMS/HSM-backed signer as a
/// later, interface-compatible replacement.
pub struct CaMaterial {
    issuer: Issuer<'static, KeyPair>,
    certificate_der: Vec<u8>,
    certificate_pem: String,
}

impl CaMaterial {
    /// Load CA material from a PEM certificate and a PEM private key,
    /// exactly the two files `prepare.py` already produces (`ca.pem`,
    /// `ca.key`), so migrating existing environments to this issuer does
    /// not require re-keying the CA itself.
    pub fn from_pem(ca_certificate_pem: &str, ca_key_pem: &str) -> Result<Self, IssuerError> {
        let key = KeyPair::from_pem(ca_key_pem)
            .map_err(|err| IssuerError::InvalidCaMaterial(err.to_string()))?;
        let key_public_der = key.subject_public_key_info();

        // Keep the CA certificate itself, and prove now that it parses, so a
        // later renewal never has to handle a CA that cannot be read.
        let (_, pem) = x509_parser::pem::parse_x509_pem(ca_certificate_pem.as_bytes())
            .map_err(|err| IssuerError::InvalidCaMaterial(err.to_string()))?;
        let (_, certificate) = x509_parser::parse_x509_certificate(&pem.contents)
            .map_err(|err| IssuerError::InvalidCaMaterial(err.to_string()))?;
        if certificate.public_key().raw != key_public_der.as_slice() {
            return Err(IssuerError::InvalidCaMaterial(
                "CA certificate does not match the private key".into(),
            ));
        }
        let issuer = Issuer::from_ca_cert_pem(ca_certificate_pem, key)
            .map_err(|err| IssuerError::InvalidCaMaterial(err.to_string()))?;
        let mut certificate_pem = ca_certificate_pem.trim_end().to_string();
        certificate_pem.push('\n');

        Ok(Self {
            issuer,
            certificate_der: pem.contents,
            certificate_pem,
        })
    }

    /// Generate a fresh, self-signed CA in memory. Used by this crate's own
    /// tests and by any integration test standing up a throwaway issuer;
    /// not intended for anything long-lived, since the key it generates is
    /// never persisted.
    #[doc(hidden)]
    pub fn generate_for_tests() -> (String, String, Self) {
        use rcgen::{BasicConstraints, IsCa};

        let mut ca_params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
        ];
        let ca_key = KeyPair::generate().expect("CA key");
        let ca_certificate = ca_params.self_signed(&ca_key).expect("CA certificate");
        let ca_certificate_pem = ca_certificate.pem();
        let ca_key_pem = ca_key.serialize_pem();

        let material = Self::from_pem(&ca_certificate_pem, &ca_key_pem)
            .expect("freshly generated CA material parses");
        (ca_certificate_pem, ca_key_pem, material)
    }
}

/// The on-disk `CaSigner`, matching Phase 0's recorded custody decision.
pub struct OnDiskCaSigner {
    material: CaMaterial,
}

impl OnDiskCaSigner {
    pub fn new(material: CaMaterial) -> Self {
        Self { material }
    }
}

impl CaSigner for OnDiskCaSigner {
    fn sign(
        &self,
        csr_der: &[u8],
        identity: &LeafIdentity,
        lifetime: Duration,
    ) -> Result<IssuedCertificate, IssuerError> {
        identity.validate()?;
        let csr_der = csr_der.to_vec().into();
        let mut request = CertificateSigningRequestParams::from_der(&csr_der)
            .map_err(|err| IssuerError::InvalidCsr(err.to_string()))?;

        let algorithm = request.public_key.algorithm();
        if algorithm != &PKCS_ED25519
            && algorithm != &PKCS_ECDSA_P256_SHA256
            && algorithm != &PKCS_ECDSA_P384_SHA384
        {
            return Err(IssuerError::UnsupportedKeyAlgorithm);
        }

        let now = OffsetDateTime::now_utc();
        if lifetime.is_zero() {
            return Err(IssuerError::InvalidLifetime(
                "leaf lifetime must be greater than zero",
            ));
        }
        let lifetime = time::Duration::try_from(lifetime)
            .map_err(|_| IssuerError::InvalidLifetime("leaf lifetime is too large"))?;
        let not_after = now
            .checked_add(lifetime)
            .ok_or(IssuerError::InvalidLifetime("leaf lifetime is too large"))?;
        request.params.not_before = now;
        request.params.not_after = not_after;
        // Subject and SAN are both the issuer's decision: whatever the CSR asked
        // for is discarded.
        request.params.distinguished_name = identity.subject();
        request.params.subject_alt_names =
            vec![SanType::URI(identity.spiffe_uri().try_into().map_err(
                |_| IssuerError::InvalidCsr("identity URI is not valid Ia5".into()),
            )?)];
        request.params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        request.params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];

        let certificate = request.signed_by(&self.material.issuer)?;
        Ok(IssuedCertificate {
            identity: identity.clone(),
            certificate_der: certificate.der().to_vec(),
            certificate_pem: certificate.pem(),
            ca_certificate_pem: self.material.certificate_pem.clone(),
            not_after,
            // The signer does not own environment policy. `WorkloadIssuer` replaces this with
            // the configured renewal deadline before returning the certificate to a caller.
            renew_at: not_after,
        })
    }

    fn ca_certificate_der(&self) -> &[u8] {
        &self.material.certificate_der
    }

    fn ca_certificate_pem(&self) -> &str {
        &self.material.certificate_pem
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::CertificateParams as LeafParams;

    fn leaf_csr() -> (KeyPair, Vec<u8>) {
        let key = KeyPair::generate().expect("leaf key");
        let params = LeafParams::new(Vec::<String>::new()).expect("leaf params");
        let csr = params.serialize_request(&key).expect("csr");
        (key, csr.der().to_vec())
    }

    #[test]
    fn ca_certificate_must_match_its_private_key() {
        let (ca_pem, _ca_key_pem, _material) = CaMaterial::generate_for_tests();
        let (_other_ca_pem, other_key_pem, _other_material) = CaMaterial::generate_for_tests();

        let error = match CaMaterial::from_pem(&ca_pem, &other_key_pem) {
            Ok(_) => panic!("mismatched CA material was accepted"),
            Err(error) => error,
        };
        assert!(
            matches!(error, IssuerError::InvalidCaMaterial(ref message) if message.contains("does not match")),
            "{error}"
        );
    }

    #[test]
    fn the_environment_goes_into_the_subject_and_must_be_a_plain_segment() {
        let (_ca_pem, _ca_key_pem, material) = CaMaterial::generate_for_tests();
        let signer = OnDiskCaSigner::new(material);
        let (_key, csr_der) = leaf_csr();
        let identity = |env_tag: Option<&str>| LeafIdentity {
            env_tag: env_tag.map(str::to_string),
            install_id: Uuid::new_v4(),
            service_id: "com.networknt.cli.dev-1.0.0".into(),
            role: "cli".into(),
        };

        let issued = signer
            .sign(&csr_der, &identity(Some("prod")), Duration::from_secs(60))
            .expect("signs");
        let (_, certificate) =
            x509_parser::parse_x509_certificate(&issued.certificate_der).expect("parses");
        let organization: Vec<_> = certificate
            .subject()
            .iter_organization()
            .filter_map(|a| a.as_str().ok())
            .collect();
        assert_eq!(organization, ["prod"]);

        let none = signer
            .sign(&csr_der, &identity(None), Duration::from_secs(60))
            .expect("signs");
        let (parsed, _) =
            LeafIdentity::from_certificate_der(&none.certificate_der).expect("parses");
        assert_eq!(parsed.env_tag, None);

        for bad in ["", "a/b", "a b", "ü"] {
            assert!(
                matches!(
                    signer.sign(&csr_der, &identity(Some(bad)), Duration::from_secs(60)),
                    Err(IssuerError::InvalidIdentity(_))
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn signs_a_leaf_bound_to_the_requested_identity() {
        let (_ca_pem, _ca_key_pem, material) = CaMaterial::generate_for_tests();
        let signer = OnDiskCaSigner::new(material);
        let (_key, csr_der) = leaf_csr();

        let identity = LeafIdentity {
            env_tag: None,
            install_id: Uuid::new_v4(),
            service_id: "com.networknt.cli.dev-1.0.0".into(),
            role: "cli".into(),
        };

        let issued = signer
            .sign(&csr_der, &identity, Duration::from_secs(86_400))
            .expect("signing succeeds");

        assert!(issued.certificate_pem.contains("BEGIN CERTIFICATE"));
        assert!(issued.not_after > OffsetDateTime::now_utc());
        assert_eq!(issued.not_after, issued.renew_at);
        assert_eq!(issued.identity, identity);
    }

    #[test]
    fn rejects_zero_and_overflowing_lifetimes_without_panicking() {
        let (_ca_pem, _ca_key_pem, material) = CaMaterial::generate_for_tests();
        let signer = OnDiskCaSigner::new(material);
        let (_key, csr_der) = leaf_csr();
        let identity = LeafIdentity {
            env_tag: Some("loc".into()),
            install_id: Uuid::new_v4(),
            service_id: "com.networknt.agent.test-1.0.0".into(),
            role: "agent".into(),
        };

        for lifetime in [Duration::ZERO, Duration::from_secs(u64::MAX)] {
            assert!(matches!(
                signer.sign(&csr_der, &identity, lifetime),
                Err(IssuerError::InvalidLifetime(_))
            ));
        }
    }

    #[test]
    fn discards_attributes_the_csr_itself_requested() {
        // The CSR asks for no SANs of its own; confirm the issued leaf
        // carries the identity the signer decided, not something a caller
        // could have smuggled into the CSR.
        let (_ca_pem, _ca_key_pem, material) = CaMaterial::generate_for_tests();
        let signer = OnDiskCaSigner::new(material);
        let (_key, csr_der) = leaf_csr();

        let identity = LeafIdentity {
            env_tag: None,
            install_id: Uuid::new_v4(),
            service_id: "com.networknt.agent.codex-personal-workflow-1.0.0".into(),
            role: "agent".into(),
        };

        let issued = signer
            .sign(&csr_der, &identity, Duration::from_secs(3_600))
            .expect("signing succeeds");

        // The SAN is ASCII inside the DER (unlike the base64-encoded PEM),
        // so this confirms the issuer's chosen identity landed in the
        // certificate rather than merely being echoed back in the struct.
        let der_as_text = String::from_utf8_lossy(&issued.certificate_der);
        assert!(der_as_text.contains(&identity.spiffe_uri()));
    }

    fn names(der: &[u8]) -> (Vec<String>, Vec<String>, usize, Vec<String>) {
        let (_, cert) = x509_parser::parse_x509_certificate(der).expect("parse");
        let text = |it: Vec<&str>| it.into_iter().map(str::to_string).collect::<Vec<_>>();
        let subject = cert.subject();
        let cn = text(
            subject
                .iter_common_name()
                .filter_map(|a| a.as_str().ok())
                .collect(),
        );
        let ou = text(
            subject
                .iter_organizational_unit()
                .filter_map(|a| a.as_str().ok())
                .collect(),
        );
        let orgs = subject.iter_organization().count();
        let sans = cert
            .subject_alternative_name()
            .ok()
            .flatten()
            .map(|ext| {
                ext.value
                    .general_names
                    .iter()
                    .map(|n| n.to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        (cn, ou, orgs, sans)
    }

    #[test]
    fn the_subject_is_the_issuers_decision_not_the_csrs() {
        // A CSR that asks to be `CN=admin, O=Evil Corp` with its own DNS name.
        let key = KeyPair::generate().expect("key");
        let mut params = LeafParams::new(vec!["evil.example.com".to_string()]).expect("params");
        params.distinguished_name.push(DnType::CommonName, "admin");
        params
            .distinguished_name
            .push(DnType::OrganizationName, "Evil Corp");
        let csr_der = params.serialize_request(&key).expect("csr").der().to_vec();

        let (_ca_pem, _ca_key_pem, material) = CaMaterial::generate_for_tests();
        let identity = LeafIdentity {
            env_tag: None,
            install_id: Uuid::new_v4(),
            service_id: "com.networknt.light-cli-1.0.0".into(),
            role: "cli".into(),
        };
        let issued = OnDiskCaSigner::new(material)
            .sign(&csr_der, &identity, Duration::from_secs(3_600))
            .expect("signs");

        let (cn, ou, orgs, sans) = names(&issued.certificate_der);
        assert_eq!(cn, vec!["com.networknt.light-cli-1.0.0"]);
        assert_eq!(ou, vec!["cli"]);
        assert_eq!(orgs, 0, "the CSR's organisation is discarded");
        assert_eq!(sans.len(), 1, "only the issuer's URI SAN: {sans:?}");
        assert!(sans[0].contains(&identity.install_id.to_string()));
    }

    #[test]
    fn a_long_service_id_is_cut_in_the_common_name_but_kept_in_the_san() {
        let long = format!("com.networknt.{}-1.0.0", "x".repeat(80));
        let (_ca_pem, _ca_key_pem, material) = CaMaterial::generate_for_tests();
        let (_key, csr_der) = leaf_csr();
        let identity = LeafIdentity {
            env_tag: None,
            install_id: Uuid::new_v4(),
            service_id: long.clone(),
            role: "agent".into(),
        };
        let issued = OnDiskCaSigner::new(material)
            .sign(&csr_der, &identity, Duration::from_secs(3_600))
            .expect("signs");

        let (cn, _ou, _orgs, sans) = names(&issued.certificate_der);
        assert_eq!(cn[0].len(), 64);
        assert!(long.starts_with(&cn[0]));
        assert!(sans[0].contains(&long), "the SAN keeps the full service id");
    }
}
