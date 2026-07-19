//! Versioned provider conformance corpus and deployment eligibility results.

mod fixtures;
mod runner;

pub use fixtures::{
    ConformanceCapability, CorpusFixture, CorpusManifest, FixtureKind, FixtureProvenance,
    FixtureReference, ProviderProfile,
};
pub use runner::{
    CapabilityEvidence, CapabilityRequirements, CaseResult, ConformanceResult, ConformanceRunner,
    ConformanceState, DeploymentDelta, DeploymentEligibility, PublicationAcknowledgement,
    eligible_deployment_ids,
};
