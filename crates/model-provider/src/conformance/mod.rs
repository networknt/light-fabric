//! Versioned provider conformance corpus and deployment eligibility results.

mod fixtures;
mod live;
mod runner;
mod signing;

pub use fixtures::{
    ConformanceCapability, CorpusFixture, CorpusManifest, FixtureKind, FixtureProvenance,
    FixtureReference, ProviderProfile,
};
pub use live::{LiveProbeReport, LiveQualificationSpec, build_live_conformance_result};
pub use runner::{
    CONFORMANCE_RESULT_SCHEMA_VERSION, CapabilityEvidence, CapabilityRequirements, CaseResult,
    ConformanceResult, ConformanceRunner, ConformanceState, DEPLOYMENT_DELTA_SCHEMA_VERSION,
    DeploymentDelta, DeploymentEligibility, EvidenceKind, LiveEvidenceBinding,
    PublicationAcknowledgement, RunnerVantage, SidecarEvidence, eligible_deployment_ids,
};
pub use signing::{
    Ed25519EvidenceSigner, EvidenceSignatureError, TrustedEvidenceKeySet, canonical_json_bytes,
    sha256_hex,
};
