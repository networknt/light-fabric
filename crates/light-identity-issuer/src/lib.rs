//! Workload identity issuance core: CA signing, environment-scoped lifetime
//! policy, and CA-based leaf identity binding.
//!
//! See `docs/src/design/workload-identity-issuance.md` for the design this
//! crate implements. This crate is Phase 0 of
//! `implementation/light-fabric/2026-09-19-WorkloadIdentityIssuanceImplementationPlan.md`:
//! the signing core and the CSR-in/certificate-out contract. Bootstrap
//! authentication against the real long-lived Portal token, CLI pairing,
//! and distributed revocation lists are Phase 1 and land in a caller layer
//! on top of this crate, not here.

mod bootstrap;
mod issuance;
mod policy;
mod renewal;
mod revocation;
mod signer;
mod state;

pub use bootstrap::{
    BootstrapReplayGuard, CombinedAuthorizer, DecodingKeyResolver, InMemoryPairingCodes,
    InMemorySpentTokens, PairingCodeStore, PairingGrantAuthorizer, PortalTokenAuthorizer,
    SpentTokenStore,
};
pub use issuance::{
    AllowAllForTests, Authorized, BootstrapCredential, FirstIssuanceAuthorizer, WorkloadIssuer,
};
pub use policy::{EnvPolicy, IssuerPolicy};
pub use renewal::{MAX_PROOF_SKEW_SECONDS, RenewalProof, renewal_message};
pub use revocation::{InMemoryRevocationList, RevocationList};
pub use signer::{CaMaterial, CaSigner, IssuedCertificate, LeafIdentity, OnDiskCaSigner};
pub use state::FileSpentTokens;

use rcgen::Error as RcgenError;
use thiserror::Error;

/// Everything that can go wrong issuing or renewing a leaf certificate.
///
/// Deliberately does not distinguish "malformed CSR" from "signing failure"
/// in its variants beyond what is useful to a caller deciding what to do
/// next; both are terminal for the request.
#[derive(Debug, Error)]
pub enum IssuerError {
    #[error("unknown environment tag: {0}")]
    UnknownEnvironment(String),
    #[error("could not parse certificate signing request: {0}")]
    InvalidCsr(String),
    #[error("could not parse the presented certificate for renewal: {0}")]
    InvalidPresentedCertificate(String),
    #[error("invalid CA material: {0}")]
    InvalidCaMaterial(String),
    /// The presented certificate does not carry a valid signature from this issuer's CA.
    #[error("presented certificate was not issued by this issuer's CA")]
    PresentedCertificateNotIssuedByThisCa,
    #[error("the presented certificate has expired and cannot be renewed")]
    PresentedCertificateExpired,
    #[error("the presented certificate's key algorithm is not supported for renewal proofs")]
    UnsupportedKeyAlgorithm,
    #[error("renewal proof of possession did not verify")]
    PossessionProofInvalid,
    #[error("renewal proof timestamp is outside the accepted window")]
    PossessionProofStale,
    #[error("renewal request was already used")]
    ReplayedRenewal,
    /// The presented certificate was issued for a different environment than the renewal asks for,
    /// or carries none (issued before environments were bound in; enroll again).
    #[error("the presented certificate was not issued for this environment")]
    EnvironmentMismatch,
    /// The bootstrap credential did not authorize a first issuance. The
    /// reason is a fixed, non-sensitive category.
    #[error("bootstrap credential rejected: {0}")]
    Unauthorized(&'static str),
    /// The identity to bind into the certificate cannot be encoded safely.
    #[error("invalid certificate identity: {0}")]
    InvalidIdentity(&'static str),
    #[error("invalid certificate lifetime: {0}")]
    InvalidLifetime(&'static str),
    /// The durable record of spent bootstrap tokens could not be read or written.
    /// Issuance fails closed: without the record the once-only guarantee cannot
    /// be kept, so the request is refused and the token is not spent.
    #[error("issuer state storage failed: {0}")]
    Storage(String),
    #[error("install id is revoked: {0}")]
    Revoked(uuid::Uuid),
    #[error("signing failed: {0}")]
    Signing(#[from] RcgenError),
}
