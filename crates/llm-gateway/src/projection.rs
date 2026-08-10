//! Off-request-path production projection delivery.
//!
//! Config server materializes one immutable manifest plus immutable resources
//! in `config-cache`. This module verifies that frozen contract, compiles a
//! complete candidate, performs one root swap, and acknowledges only an
//! applied root. It never performs control-plane or secret I/O on a request.

use crate::config::{
    AliasCapabilityRequirements, AliasConfig, AuditMode, DeploymentConfig, EmbeddingWorkloadLane,
    EndpointAuth, LlmRouterConfig, NetworkProfile, NetworkZone, PricingBasis, ProviderConfig,
    RuntimeCapacity, SidecarExpectation,
};
use crate::error::LlmGatewayError;
use crate::runtime::{LlmCompiler, LlmSnapshotStore, PublishOutcome};
use crate::usage::{EmbeddingPrice, GenerationPrice, OperationPrice};
use chrono::{SecondsFormat, Utc};
use model_provider::inference::{Operation, ProviderCapabilities, ProviderProtocol};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::warn;

pub const PROJECTION_SCHEMA_VERSION: &str = "2";
pub const PROJECTION_SCHEMA_VERSION_V3: &str = "3";
pub const PROJECTION_ACKNOWLEDGEMENT_SCHEMA_VERSION: &str = "2";
pub const GATEWAY_COMPILER_VERSION: &str = env!("CARGO_PKG_VERSION");
const SUPPORTED_ROUTING_FEATURE: &str = "ordered-routing";
const MAX_PROJECTION_RESOURCES: usize = 1_024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionManifest {
    pub schema_version: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub publication_id: String,
    pub host_id: String,
    pub environment: String,
    pub sequence: u64,
    pub minimum_gateway_version: String,
    #[serde(default)]
    pub enabled_routing_features: BTreeSet<String>,
    pub resources: Vec<ProjectionResourceReference>,
    pub root_digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionResourceReference {
    pub resource_type: String,
    pub resource_id: String,
    pub resource_version: String,
    pub digest: String,
    #[serde(default)]
    pub tombstone: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionResource {
    pub schema_version: String,
    pub host_id: String,
    pub environment: String,
    pub resource_type: String,
    pub resource_id: String,
    pub resource_version: String,
    pub sequence: u64,
    pub digest: String,
    pub payload: Value,
}

#[derive(Debug, Clone)]
pub struct ProjectionBundle {
    pub manifest: ProjectionManifest,
    pub resources: BTreeMap<String, ProjectionResource>,
}

pub trait LlmProjectionSource: Send + Sync {
    fn load_latest(&self) -> Result<ProjectionBundle, ProjectionError>;
}

pub trait ProjectionAcknowledgementSink: Send + Sync {
    fn acknowledge(
        &self,
        acknowledgement: &ProjectionAcknowledgement,
    ) -> Result<(), ProjectionError>;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionAcknowledgement {
    pub schema_version: String,
    pub gateway_publication_id: String,
    pub host_id: String,
    pub environment: String,
    pub inventory_id: String,
    pub inventory_generation: u64,
    pub inventory_digest: String,
    pub sequence: u64,
    pub root_digest: String,
    pub applied_schema_version: String,
    pub applied_at: String,
    pub gateway_version: String,
    pub reader_version: String,
    pub gateway_instance: String,
    pub material_generation: u64,
    pub resolved_trust_digest: String,
    pub evidence_key_set_version: String,
    pub evidence_key_set_digest: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_code: Option<String>,
    pub acknowledgement_digest: String,
}

impl ProjectionAcknowledgement {
    pub fn refresh_digest(&mut self) -> Result<(), ProjectionError> {
        self.acknowledgement_digest.clear();
        self.acknowledgement_digest =
            canonical_digest(&serde_json::to_value(&*self).map_err(contract_json)?)?;
        Ok(())
    }

    pub fn verify_digest(&self) -> bool {
        let mut candidate = self.clone();
        let expected = candidate.acknowledgement_digest.clone();
        candidate.refresh_digest().is_ok() && candidate.acknowledgement_digest == expected
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionCheckpoint {
    pub sequence: u64,
    pub root_digest: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectionStatus {
    pub last_applied_sequence: u64,
    pub last_root_digest: String,
    pub credential_generation: u64,
    pub applied_roots: u64,
    pub duplicate_roots: u64,
    pub rejected_roots: u64,
    pub rejected_by_class: BTreeMap<ProjectionRejectionClass, u64>,
    pub acknowledgement_failures: u64,
    pub resyncs: u64,
}

/// Fixed labels keep projection rejection metrics bounded while the event
/// retains enough metadata to prove which serving root was preserved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProjectionRejectionClass {
    Transport,
    Contract,
    SequenceGap,
    Compile,
}

impl ProjectionRejectionClass {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Transport => "transport",
            Self::Contract => "contract",
            Self::SequenceGap => "sequence_gap",
            Self::Compile => "compile",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionRejectionEvent {
    pub reason_class: ProjectionRejectionClass,
    pub sanitized_reason: String,
    pub rejected_sequence: Option<u64>,
    pub rejected_digest: Option<String>,
    pub retained_generation: u64,
    pub retained_digest: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionApplyOutcome {
    Published,
    MaterializationUnchanged,
    Duplicate,
}

#[derive(Debug, thiserror::Error)]
pub enum ProjectionError {
    #[error("projection transport failed: {0}")]
    Transport(String),
    #[error("projection contract rejected: {0}")]
    Contract(String),
    #[error("projection sequence gap: expected {expected}, received {received}")]
    SequenceGap { expected: u64, received: u64 },
    #[error("projection compilation failed: {0}")]
    Compile(String),
    #[error("projection acknowledgement failed: {0}")]
    Acknowledgement(String),
}

/// Config-server `/files` projection source. The supplied root must be the
/// application-owned `config-cache` directory, never an arbitrary request path.
pub struct FileProjectionSource {
    root: PathBuf,
    max_artifact_bytes: usize,
}

impl FileProjectionSource {
    pub fn new(root: impl Into<PathBuf>, max_artifact_bytes: usize) -> Self {
        Self {
            root: root.into(),
            max_artifact_bytes: max_artifact_bytes.max(1),
        }
    }

    fn read_json<T: for<'de> Deserialize<'de>>(&self, path: &Path) -> Result<T, ProjectionError> {
        let canonical_root = fs::canonicalize(&self.root).map_err(transport)?;
        let canonical_path = fs::canonicalize(path).map_err(transport)?;
        if !canonical_path.starts_with(&canonical_root) {
            return Err(ProjectionError::Contract(
                "projection artifact resolves outside config-cache root".to_string(),
            ));
        }
        let metadata = fs::metadata(&canonical_path).map_err(transport)?;
        if !metadata.is_file() || metadata.len() > self.max_artifact_bytes as u64 {
            return Err(ProjectionError::Contract(
                "projection artifact is not a bounded regular file".to_string(),
            ));
        }
        let bytes = fs::read(canonical_path).map_err(transport)?;
        serde_json::from_slice(&bytes)
            .map_err(|error| ProjectionError::Contract(format!("invalid projection JSON: {error}")))
    }
}

impl LlmProjectionSource for FileProjectionSource {
    fn load_latest(&self) -> Result<ProjectionBundle, ProjectionError> {
        let manifest: ProjectionManifest = self.read_json(&self.root.join("manifest.json"))?;
        validate_manifest(&manifest)?;
        let mut resources = BTreeMap::new();
        for reference in manifest.resources.iter().filter(|item| !item.tombstone) {
            validate_path_component(&reference.resource_type)?;
            validate_path_component(&reference.resource_id)?;
            validate_path_component(&reference.resource_version)?;
            let path = self
                .root
                .join("resources")
                .join(&reference.resource_type)
                .join(&reference.resource_id)
                .join(format!("{}.json", reference.resource_version));
            let resource: ProjectionResource = self.read_json(&path)?;
            let key = resource_key(&resource.resource_type, &resource.resource_id);
            if resources.insert(key.clone(), resource).is_some() {
                return Err(ProjectionError::Contract(format!(
                    "duplicate projection resource `{key}`"
                )));
            }
        }
        Ok(ProjectionBundle {
            manifest,
            resources,
        })
    }
}

/// One immutable acknowledgement file per replica. Config distribution can
/// collect these independently, so a fast replica never masks a lagging one.
pub struct FileAcknowledgementSink {
    directory: PathBuf,
}

impl FileAcknowledgementSink {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
        }
    }
}

impl ProjectionAcknowledgementSink for FileAcknowledgementSink {
    fn acknowledge(
        &self,
        acknowledgement: &ProjectionAcknowledgement,
    ) -> Result<(), ProjectionError> {
        validate_path_component(&acknowledgement.gateway_instance)?;
        if !acknowledgement.verify_digest() {
            return Err(ProjectionError::Contract(
                "projection acknowledgement digest mismatch".to_string(),
            ));
        }
        fs::create_dir_all(&self.directory).map_err(transport)?;
        let publication_directory = self.directory.join(format!(
            "publication-{}-{}",
            acknowledgement.sequence, acknowledgement.root_digest
        ));
        fs::create_dir_all(&publication_directory).map_err(transport)?;
        let path = publication_directory.join(format!("{}.json", acknowledgement.gateway_instance));
        atomic_write_json(&path, acknowledgement)
    }
}

pub struct LlmProjectionWorker {
    source: Arc<dyn LlmProjectionSource>,
    acknowledgements: Arc<dyn ProjectionAcknowledgementSink>,
    compiler: Arc<LlmCompiler>,
    store: Arc<LlmSnapshotStore>,
    checkpoint_path: PathBuf,
    gateway_instance: String,
    base_config: LlmRouterConfig,
    current_config: Option<LlmRouterConfig>,
    last_acknowledgement: Option<ProjectionAcknowledgement>,
    pending_acknowledgement: Option<ProjectionAcknowledgement>,
    candidate_identity: Option<(u64, String)>,
    last_rejection: Option<ProjectionRejectionEvent>,
    status: ProjectionStatus,
}

impl LlmProjectionWorker {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        source: Arc<dyn LlmProjectionSource>,
        acknowledgements: Arc<dyn ProjectionAcknowledgementSink>,
        compiler: Arc<LlmCompiler>,
        store: Arc<LlmSnapshotStore>,
        checkpoint_path: impl Into<PathBuf>,
        gateway_instance: impl Into<String>,
        base_config: LlmRouterConfig,
    ) -> Result<Self, ProjectionError> {
        let checkpoint_path = checkpoint_path.into();
        let checkpoint = load_checkpoint(&checkpoint_path)?;
        Ok(Self {
            source,
            acknowledgements,
            compiler,
            store,
            checkpoint_path,
            gateway_instance: gateway_instance.into(),
            base_config,
            current_config: None,
            last_acknowledgement: None,
            pending_acknowledgement: None,
            candidate_identity: None,
            last_rejection: None,
            status: ProjectionStatus {
                last_applied_sequence: checkpoint.sequence,
                last_root_digest: checkpoint.root_digest,
                ..ProjectionStatus::default()
            },
        })
    }

    pub fn status(&self) -> &ProjectionStatus {
        &self.status
    }

    pub fn last_rejection(&self) -> Option<&ProjectionRejectionEvent> {
        self.last_rejection.as_ref()
    }

    pub fn apply_latest(&mut self) -> Result<ProjectionApplyOutcome, ProjectionError> {
        self.apply(false)
    }

    /// Bounded full-root recovery after a detected delta gap.
    pub fn resync_latest(&mut self) -> Result<ProjectionApplyOutcome, ProjectionError> {
        self.status.resyncs += 1;
        self.apply(true)
    }

    fn apply(&mut self, full_resync: bool) -> Result<ProjectionApplyOutcome, ProjectionError> {
        if let Err(error) = self.retry_pending_acknowledgement() {
            self.status.acknowledgement_failures =
                self.status.acknowledgement_failures.saturating_add(1);
            return Err(error);
        }
        self.candidate_identity = None;
        let result = self.apply_inner(full_resync);
        if let Err(error) = &result
            && !matches!(error, ProjectionError::Acknowledgement(_))
        {
            self.record_rejection(error);
        }
        if matches!(result, Err(ProjectionError::Acknowledgement(_))) {
            self.status.acknowledgement_failures =
                self.status.acknowledgement_failures.saturating_add(1);
        }
        result
    }

    fn record_rejection(&mut self, error: &ProjectionError) {
        let reason_class = match error {
            ProjectionError::Transport(_) => ProjectionRejectionClass::Transport,
            ProjectionError::Contract(_) => ProjectionRejectionClass::Contract,
            ProjectionError::SequenceGap { .. } => ProjectionRejectionClass::SequenceGap,
            ProjectionError::Compile(_) => ProjectionRejectionClass::Compile,
            ProjectionError::Acknowledgement(_) => return,
        };
        self.status.rejected_roots = self.status.rejected_roots.saturating_add(1);
        let class_count = self
            .status
            .rejected_by_class
            .entry(reason_class)
            .or_default();
        *class_count = class_count.saturating_add(1);
        let retained = self.store.load();
        let (rejected_sequence, rejected_digest) = self
            .candidate_identity
            .clone()
            .map_or((None, None), |(sequence, digest)| {
                (Some(sequence), Some(digest))
            });
        let event = ProjectionRejectionEvent {
            reason_class,
            sanitized_reason: error.to_string(),
            rejected_sequence,
            rejected_digest,
            retained_generation: retained.generation,
            retained_digest: retained.digest.clone(),
        };
        warn!(
            target: "llm_projection_audit",
            event_kind = "projection_candidate_rejected",
            metric_name = "llm_projection_rejected_roots_total",
            metric_increment = 1_u64,
            reason_class = reason_class.as_str(),
            retained_last_valid_root = true,
            rejected_sequence = ?event.rejected_sequence,
            rejected_digest = ?event.rejected_digest,
            retained_generation = event.retained_generation,
            retained_digest = %event.retained_digest,
            reason = %event.sanitized_reason,
            "LLM projection candidate rejected; retained last valid root"
        );
        self.last_rejection = Some(event);
    }

    fn retry_pending_acknowledgement(&mut self) -> Result<(), ProjectionError> {
        let Some(acknowledgement) = self.pending_acknowledgement.as_ref() else {
            return Ok(());
        };
        self.acknowledgements
            .acknowledge(acknowledgement)
            .map_err(|error| ProjectionError::Acknowledgement(error.to_string()))?;
        self.pending_acknowledgement = None;
        Ok(())
    }

    fn apply_inner(
        &mut self,
        full_resync: bool,
    ) -> Result<ProjectionApplyOutcome, ProjectionError> {
        let bundle = self.source.load_latest()?;
        self.candidate_identity = Some((
            bundle.manifest.sequence,
            normalize_digest(&bundle.manifest.root_digest),
        ));
        validate_bundle(&bundle)?;
        let manifest = &bundle.manifest;
        let config = assemble_config(&self.base_config, &bundle)?;
        if manifest.sequence == self.status.last_applied_sequence {
            if normalize_digest(&manifest.root_digest) != self.status.last_root_digest {
                return Err(ProjectionError::Contract(
                    "conflicting projection root at an applied sequence".to_string(),
                ));
            }
            if self.store.load().digest == self.status.last_root_digest {
                self.current_config = Some(config);
                self.status.duplicate_roots += 1;
                return Ok(ProjectionApplyOutcome::Duplicate);
            }
            // A process restart may retain the minimal checkpoint while the
            // in-memory root is new. Rebuild the full root before serving.
        }
        if self.status.last_applied_sequence != 0
            && manifest.sequence != self.status.last_applied_sequence + 1
            && !full_resync
        {
            return Err(ProjectionError::SequenceGap {
                expected: self.status.last_applied_sequence + 1,
                received: manifest.sequence,
            });
        }
        if manifest.sequence < self.status.last_applied_sequence {
            return Err(ProjectionError::Contract(
                "projection sequence moved backwards".to_string(),
            ));
        }
        let previous = self.store.load();
        let mut candidate = self
            .compiler
            .compile(&config, manifest.sequence, Some(&previous))
            .map_err(compile_error)?;
        candidate.digest = normalize_digest(&manifest.root_digest);
        let outcome = self.store.publish(candidate);
        let checkpoint = ProjectionCheckpoint {
            sequence: manifest.sequence,
            root_digest: normalize_digest(&manifest.root_digest),
        };
        atomic_write_json(&self.checkpoint_path, &checkpoint)?;
        self.status.last_applied_sequence = checkpoint.sequence;
        self.status.last_root_digest = checkpoint.root_digest;
        self.status.applied_roots += 1;
        let mut acknowledgement = ProjectionAcknowledgement {
            schema_version: PROJECTION_ACKNOWLEDGEMENT_SCHEMA_VERSION.to_string(),
            gateway_publication_id: manifest.publication_id.clone(),
            host_id: manifest.host_id.clone(),
            environment: manifest.environment.clone(),
            inventory_id: config.production_projection.replica_inventory_id.clone(),
            inventory_generation: config.production_projection.replica_inventory_generation,
            inventory_digest: normalize_digest(
                &config.production_projection.replica_inventory_digest,
            ),
            sequence: manifest.sequence,
            root_digest: self.status.last_root_digest.clone(),
            applied_schema_version: manifest.schema_version.clone(),
            applied_at: unix_timestamp_string(),
            gateway_version: GATEWAY_COMPILER_VERSION.to_string(),
            // Closed, sorted set of projection schemas accepted by this dual
            // reader. It is independent of the schema applied by this root.
            reader_version: "projection-reader-v3".to_string(),
            gateway_instance: self.gateway_instance.clone(),
            material_generation: self.status.credential_generation,
            resolved_trust_digest: aggregate_resolved_trust_digest(&config),
            evidence_key_set_version: config
                .production_projection
                .evidence_key_set_version
                .clone(),
            evidence_key_set_digest: normalize_digest(
                &config.production_projection.evidence_key_set_digest,
            ),
            state: "applied".to_string(),
            failure_category: None,
            failure_code: None,
            acknowledgement_digest: String::new(),
        };
        acknowledgement.refresh_digest()?;
        self.current_config = Some(config);
        self.last_acknowledgement = Some(acknowledgement.clone());
        self.pending_acknowledgement = Some(acknowledgement);
        self.retry_pending_acknowledgement()?;
        Ok(match outcome {
            PublishOutcome::Published => ProjectionApplyOutcome::Published,
            PublishOutcome::Unchanged => ProjectionApplyOutcome::MaterializationUnchanged,
        })
    }

    /// Rematerializes credentials after the application runtime config sends a
    /// bounded rotation notification. No projection or request-time lookup is
    /// involved, and unchanged providers/accounts retain their stable Arcs.
    pub fn reload_secrets(&mut self) -> Result<ProjectionApplyOutcome, ProjectionError> {
        let config = self.current_config.clone().ok_or_else(|| {
            ProjectionError::Contract("cannot rotate before a root is loaded".to_string())
        })?;
        let previous = self.store.load();
        let mut candidate = self
            .compiler
            .compile(
                &config,
                previous.generation.saturating_add(1),
                Some(&previous),
            )
            .map_err(compile_error)?;
        candidate.digest = previous.digest.clone();
        let outcome = self.store.publish(candidate);
        if matches!(outcome, PublishOutcome::Published) {
            self.status.credential_generation = self.status.credential_generation.saturating_add(1);
            let mut acknowledgement = self.last_acknowledgement.clone().ok_or_else(|| {
                ProjectionError::Contract(
                    "cannot acknowledge rotated material before a root is loaded".to_string(),
                )
            })?;
            acknowledgement.material_generation = self.status.credential_generation;
            acknowledgement.resolved_trust_digest = aggregate_resolved_trust_digest(&config);
            acknowledgement.applied_at = unix_timestamp_string();
            acknowledgement.refresh_digest()?;
            self.last_acknowledgement = Some(acknowledgement.clone());
            self.pending_acknowledgement = Some(acknowledgement);
            self.retry_pending_acknowledgement()?;
            Ok(ProjectionApplyOutcome::Published)
        } else {
            Ok(ProjectionApplyOutcome::MaterializationUnchanged)
        }
    }
}

fn validate_bundle(bundle: &ProjectionBundle) -> Result<(), ProjectionError> {
    validate_manifest(&bundle.manifest)?;
    let manifest = &bundle.manifest;
    let mut manifest_value = serde_json::to_value(manifest).map_err(contract_json)?;
    remove_field(&mut manifest_value, "rootDigest")?;
    let calculated_root = canonical_digest(&manifest_value)?;
    if calculated_root != normalize_digest(&manifest.root_digest) {
        return Err(ProjectionError::Contract(
            "projection manifest root digest mismatch".to_string(),
        ));
    }
    let mut referenced = BTreeSet::new();
    for reference in &manifest.resources {
        let key = resource_key(&reference.resource_type, &reference.resource_id);
        if !referenced.insert(key.clone()) {
            return Err(ProjectionError::Contract(format!(
                "manifest references `{key}` more than once"
            )));
        }
        if reference.tombstone {
            if bundle.resources.contains_key(&key) {
                return Err(ProjectionError::Contract(format!(
                    "tombstoned resource `{key}` has a payload"
                )));
            }
            continue;
        }
        let resource = bundle.resources.get(&key).ok_or_else(|| {
            ProjectionError::Contract(format!("manifest resource `{key}` is missing"))
        })?;
        if resource.host_id != manifest.host_id
            || resource.environment != manifest.environment
            || resource.schema_version != manifest.schema_version
            || resource.sequence > manifest.sequence
            || resource.resource_version != reference.resource_version
            || normalize_digest(&resource.digest) != normalize_digest(&reference.digest)
        {
            return Err(ProjectionError::Contract(format!(
                "manifest/resource contract mismatch for `{key}`"
            )));
        }
        let mut resource_value = serde_json::to_value(resource).map_err(contract_json)?;
        remove_field(&mut resource_value, "digest")?;
        if canonical_digest(&resource_value)? != normalize_digest(&resource.digest) {
            return Err(ProjectionError::Contract(format!(
                "projection resource digest mismatch for `{key}`"
            )));
        }
    }
    if bundle.resources.keys().any(|key| !referenced.contains(key)) {
        return Err(ProjectionError::Contract(
            "projection contains an unreferenced resource".to_string(),
        ));
    }
    Ok(())
}

fn validate_manifest(manifest: &ProjectionManifest) -> Result<(), ProjectionError> {
    if !matches!(
        manifest.schema_version.as_str(),
        PROJECTION_SCHEMA_VERSION | PROJECTION_SCHEMA_VERSION_V3
    ) || manifest.host_id.trim().is_empty()
        || manifest.environment.trim().is_empty()
        || manifest.sequence == 0
        || manifest.resources.is_empty()
        || manifest.resources.len() > MAX_PROJECTION_RESOURCES
    {
        return Err(ProjectionError::Contract(
            "invalid projection manifest envelope".to_string(),
        ));
    }
    if manifest.schema_version == PROJECTION_SCHEMA_VERSION_V3
        && manifest.publication_id.trim().is_empty()
    {
        return Err(ProjectionError::Contract(
            "projection v3 manifest requires publicationId".to_string(),
        ));
    }
    if version_is_newer(&manifest.minimum_gateway_version, GATEWAY_COMPILER_VERSION)? {
        return Err(ProjectionError::Contract(
            "projection requires a newer gateway compiler".to_string(),
        ));
    }
    if manifest
        .enabled_routing_features
        .iter()
        .any(|feature| feature != SUPPORTED_ROUTING_FEATURE)
    {
        return Err(ProjectionError::Contract(
            "projection requests an unsupported routing feature".to_string(),
        ));
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DeploymentPayload {
    deployment_id: String,
    provider_id: String,
    provider_protocol: ProviderProtocol,
    base_url: String,
    credential_ref: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    quota_group_id: Option<String>,
    model: String,
    #[serde(default = "default_deployment_concurrency")]
    concurrency: usize,
    #[serde(default)]
    declared_capabilities: Option<ProviderCapabilities>,
    /// One-release read bridge for stored V2 publication resources.
    #[serde(default, rename = "conformanceDigest")]
    _legacy_conformance_digest: Option<Value>,
    #[serde(default, rename = "conformanceResult")]
    legacy_conformance_result: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProviderEndpointPayloadV3 {
    provider_endpoint_id: String,
    provider_account_id: String,
    provider_protocol: ProviderProtocol,
    base_url: String,
    endpoint_auth: EndpointAuth,
    network_profile: NetworkProfile,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    #[serde(default)]
    quota_group_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DeploymentPayloadV3 {
    deployment_id: String,
    deployment_revision_id: String,
    provider_endpoint_id: String,
    model: String,
    #[serde(default = "default_deployment_concurrency")]
    concurrency: usize,
    runtime_capacity: RuntimeCapacity,
    #[serde(default)]
    sidecar: Option<SidecarExpectation>,
    pricing_basis: PricingBasis,
    #[serde(default)]
    declared_capabilities: Option<ProviderCapabilities>,
    /// One-release read bridge for stored V3 publication resources.
    #[serde(default, rename = "conformanceDigest")]
    _legacy_conformance_digest: Option<Value>,
    #[serde(default, rename = "conformanceResult")]
    legacy_conformance_result: Option<Value>,
}

fn projected_capabilities(
    declared: Option<ProviderCapabilities>,
    legacy_conformance_result: Option<Value>,
) -> Result<ProviderCapabilities, ProjectionError> {
    if let Some(capabilities) = declared {
        return Ok(capabilities);
    }
    legacy_conformance_result
        .and_then(|result| result.get("capabilities").cloned())
        .map(serde_json::from_value)
        .transpose()
        .map_err(contract_json)?
        .ok_or_else(|| {
            ProjectionError::Contract("deployment has no declared capabilities".to_string())
        })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RoutePayload {
    alias_name: String,
    operations: BTreeSet<Operation>,
    deployments: Vec<String>,
    #[serde(default = "default_attempts")]
    max_attempts: usize,
    #[serde(default = "default_alias_concurrency")]
    concurrency: usize,
    #[serde(default)]
    max_input_tokens: Option<u64>,
    #[serde(default)]
    max_output_tokens: Option<u64>,
    #[serde(default)]
    max_cost_micros: Option<u64>,
    #[serde(default)]
    internal: bool,
    #[serde(default)]
    bound_principal: Option<String>,
    #[serde(default)]
    audit: AuditMode,
    #[serde(default)]
    pii: crate::pii::PiiProfile,
    #[serde(default)]
    required_capabilities: AliasCapabilityRequirements,
    #[serde(default)]
    require_expected_embedding_space: bool,
    #[serde(default)]
    embedding_workload_lane: EmbeddingWorkloadLane,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PricingPayload {
    deployment_id: String,
    operation: Operation,
    price_version: u64,
    input_micros_per_million: u64,
    #[serde(default, deserialize_with = "deserialize_optional_non_null_u64")]
    output_micros_per_million: Option<u64>,
}

fn deserialize_optional_non_null_u64<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    u64::deserialize(deserializer).map(Some)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PolicyPayload {
    #[serde(default)]
    global_concurrency: Option<usize>,
    #[serde(default)]
    global_stream_concurrency: Option<usize>,
    #[serde(default)]
    stream_channel_capacity: Option<usize>,
    #[serde(default)]
    max_stream_response_bytes: Option<usize>,
    #[serde(default)]
    stream_write_timeout_ms: Option<u64>,
    #[serde(default)]
    max_replay_bytes: Option<usize>,
    #[serde(default)]
    request_timeout_ms: Option<u64>,
}

fn assemble_config(
    base: &LlmRouterConfig,
    bundle: &ProjectionBundle,
) -> Result<LlmRouterConfig, ProjectionError> {
    match bundle.manifest.schema_version.as_str() {
        PROJECTION_SCHEMA_VERSION => assemble_config_v2(base, bundle),
        PROJECTION_SCHEMA_VERSION_V3 => assemble_config_v3(base, bundle),
        _ => Err(ProjectionError::Contract(
            "unsupported projection schema version".to_string(),
        )),
    }
}

fn assemble_config_v2(
    base: &LlmRouterConfig,
    bundle: &ProjectionBundle,
) -> Result<LlmRouterConfig, ProjectionError> {
    let mut config = base.clone();
    config.enabled = true;
    config.development_fixtures = false;
    config.providers.clear();
    config.deployments.clear();
    config.aliases.clear();
    let mut prices = Vec::new();
    for resource in bundle.resources.values() {
        match resource.resource_type.as_str() {
            "llm-deployment" => {
                let payload: DeploymentPayload =
                    serde_json::from_value(resource.payload.clone()).map_err(contract_json)?;
                if payload.deployment_id != resource.resource_id {
                    return Err(ProjectionError::Contract(
                        "deployment payload/resource id mismatch".to_string(),
                    ));
                }
                let provider = ProviderConfig {
                    provider_account_id: payload.provider_id.clone(),
                    provider_protocol: payload.provider_protocol,
                    base_url: payload.base_url,
                    secret_ref: payload.credential_ref,
                    endpoint_auth: None,
                    network_profile: Default::default(),
                    headers: payload.headers,
                    quota_group_id: payload.quota_group_id,
                };
                if let Some(existing) = config.providers.get(&payload.provider_id) {
                    let existing = serde_json::to_value(existing).map_err(contract_json)?;
                    let candidate = serde_json::to_value(&provider).map_err(contract_json)?;
                    if existing != candidate {
                        return Err(ProjectionError::Contract(
                            "one provider id has conflicting deployment materialization"
                                .to_string(),
                        ));
                    }
                } else {
                    config
                        .providers
                        .insert(payload.provider_id.clone(), provider);
                }
                let capabilities = projected_capabilities(
                    payload.declared_capabilities,
                    payload.legacy_conformance_result,
                )?;
                config.deployments.insert(
                    payload.deployment_id,
                    DeploymentConfig {
                        provider: payload.provider_id,
                        deployment_revision_id: String::new(),
                        model: payload.model,
                        concurrency: payload.concurrency,
                        runtime_capacity: None,
                        sidecar: None,
                        pricing_basis: Default::default(),
                        prices: BTreeMap::new(),
                        declared_capabilities: Some(capabilities.clone()),
                        embedding_capabilities: capabilities.embedding.clone(),
                        conformance_digest: String::new(),
                        conformance_result: None,
                        text: capabilities
                            .generation
                            .as_ref()
                            .is_some_and(|value| value.content.text),
                        images: capabilities
                            .generation
                            .as_ref()
                            .is_some_and(|value| value.content.images),
                        tools: capabilities
                            .generation
                            .as_ref()
                            .is_some_and(|value| value.content.tools),
                        structured_json: capabilities
                            .generation
                            .as_ref()
                            .is_some_and(|value| value.content.structured_json),
                        streaming: capabilities
                            .generation
                            .as_ref()
                            .is_some_and(|value| value.streaming),
                        pii_placeholder_preservation_percent: 0,
                    },
                );
            }
            "llm-route" => {
                let payload: RoutePayload =
                    serde_json::from_value(resource.payload.clone()).map_err(contract_json)?;
                if payload.alias_name != resource.resource_id {
                    return Err(ProjectionError::Contract(
                        "route payload/resource id mismatch".to_string(),
                    ));
                }
                config.aliases.insert(
                    payload.alias_name,
                    AliasConfig {
                        operations: payload.operations,
                        deployments: payload.deployments,
                        max_attempts: payload.max_attempts,
                        concurrency: payload.concurrency,
                        max_input_tokens: payload.max_input_tokens,
                        max_output_tokens: payload.max_output_tokens,
                        max_cost_micros: payload.max_cost_micros,
                        internal: payload.internal,
                        bound_principal: payload.bound_principal,
                        audit: payload.audit,
                        pii: payload.pii,
                        required_capabilities: payload.required_capabilities,
                        require_expected_embedding_space: payload.require_expected_embedding_space,
                        embedding_workload_lane: payload.embedding_workload_lane,
                    },
                );
            }
            "llm-pricing" => prices.push(
                serde_json::from_value::<PricingPayload>(resource.payload.clone())
                    .map_err(contract_json)?,
            ),
            "llm-policy" => {
                let payload: PolicyPayload =
                    serde_json::from_value(resource.payload.clone()).map_err(contract_json)?;
                if let Some(value) = payload.global_concurrency {
                    require_local_bound("globalConcurrency", value, config.global_concurrency)?;
                }
                if let Some(value) = payload.global_stream_concurrency {
                    require_local_bound(
                        "globalStreamConcurrency",
                        value,
                        config.global_stream_concurrency,
                    )?;
                }
                if let Some(value) = payload.stream_channel_capacity {
                    require_local_bound(
                        "streamChannelCapacity",
                        value,
                        config.stream_channel_capacity,
                    )?;
                }
                if let Some(value) = payload.max_stream_response_bytes {
                    require_local_bound(
                        "maxStreamResponseBytes",
                        value,
                        config.max_stream_response_bytes,
                    )?;
                }
                if let Some(value) = payload.stream_write_timeout_ms {
                    require_local_bound(
                        "streamWriteTimeoutMs",
                        value,
                        config.stream_write_timeout_ms,
                    )?;
                }
                if let Some(value) = payload.max_replay_bytes {
                    require_local_bound("maxReplayBytes", value, config.max_replay_bytes)?;
                }
                if let Some(value) = payload.request_timeout_ms {
                    require_local_bound("requestTimeoutMs", value, config.request_timeout_ms)?;
                }
            }
            other => {
                return Err(ProjectionError::Contract(format!(
                    "unsupported projection resource type `{other}`"
                )));
            }
        }
    }
    for price in prices {
        let deployment = config
            .deployments
            .get_mut(&price.deployment_id)
            .ok_or_else(|| {
                ProjectionError::Contract("pricing references a missing deployment".to_string())
            })?;
        let operation_price = match price.operation {
            Operation::Generate => OperationPrice::Generate(GenerationPrice {
                version: price.price_version,
                input_micros_per_million: price.input_micros_per_million,
                output_micros_per_million: price.output_micros_per_million.ok_or_else(|| {
                    ProjectionError::Contract("generation price requires output rate".to_string())
                })?,
            }),
            Operation::Embed => {
                if price.output_micros_per_million.is_some() {
                    return Err(ProjectionError::Contract(
                        "embedding price must not contain an output rate".to_string(),
                    ));
                }
                OperationPrice::Embed(EmbeddingPrice {
                    version: price.price_version,
                    input_micros_per_million: price.input_micros_per_million,
                })
            }
        };
        if deployment
            .prices
            .insert(price.operation, operation_price)
            .is_some()
        {
            return Err(ProjectionError::Contract(
                "duplicate deployment operation price".to_string(),
            ));
        }
    }
    if config.deployments.is_empty() || config.aliases.is_empty() {
        return Err(ProjectionError::Contract(
            "projection root has no deployable alias".to_string(),
        ));
    }
    Ok(config)
}

fn assemble_config_v3(
    base: &LlmRouterConfig,
    bundle: &ProjectionBundle,
) -> Result<LlmRouterConfig, ProjectionError> {
    let mut config = base.clone();
    config.enabled = true;
    config.development_fixtures = false;
    config.network_zones.clear();
    config.providers.clear();
    config.deployments.clear();
    config.aliases.clear();

    for resource in bundle.resources.values() {
        match resource.resource_type.as_str() {
            "llm-network-zone" => {
                let zone: NetworkZone =
                    serde_json::from_value(resource.payload.clone()).map_err(contract_json)?;
                if zone.id != resource.resource_id
                    || config.network_zones.insert(zone.id.clone(), zone).is_some()
                {
                    return Err(ProjectionError::Contract(
                        "network-zone payload/resource id mismatch or duplicate".to_string(),
                    ));
                }
            }
            "llm-provider-endpoint" => {
                let endpoint: ProviderEndpointPayloadV3 =
                    serde_json::from_value(resource.payload.clone()).map_err(contract_json)?;
                if endpoint.provider_endpoint_id != resource.resource_id {
                    return Err(ProjectionError::Contract(
                        "provider-endpoint payload/resource id mismatch".to_string(),
                    ));
                }
                let provider = ProviderConfig {
                    provider_account_id: endpoint.provider_account_id,
                    provider_protocol: endpoint.provider_protocol,
                    base_url: endpoint.base_url,
                    secret_ref: String::new(),
                    endpoint_auth: Some(endpoint.endpoint_auth),
                    network_profile: endpoint.network_profile,
                    headers: endpoint.headers,
                    quota_group_id: endpoint.quota_group_id,
                };
                if config
                    .providers
                    .insert(endpoint.provider_endpoint_id, provider)
                    .is_some()
                {
                    return Err(ProjectionError::Contract(
                        "duplicate provider endpoint".to_string(),
                    ));
                }
            }
            _ => {}
        }
    }

    let mut prices = Vec::new();
    for resource in bundle.resources.values() {
        match resource.resource_type.as_str() {
            "llm-network-zone" | "llm-provider-endpoint" => {}
            "llm-deployment" => {
                let payload: DeploymentPayloadV3 =
                    serde_json::from_value(resource.payload.clone()).map_err(contract_json)?;
                if payload.deployment_id != resource.resource_id
                    || !config.providers.contains_key(&payload.provider_endpoint_id)
                {
                    return Err(ProjectionError::Contract(
                        "v3 deployment payload/resource/endpoint mismatch".to_string(),
                    ));
                }
                let capabilities = projected_capabilities(
                    payload.declared_capabilities,
                    payload.legacy_conformance_result,
                )?;
                config.deployments.insert(
                    payload.deployment_id,
                    DeploymentConfig {
                        provider: payload.provider_endpoint_id,
                        deployment_revision_id: payload.deployment_revision_id,
                        model: payload.model,
                        concurrency: payload.concurrency,
                        runtime_capacity: Some(payload.runtime_capacity),
                        sidecar: payload.sidecar,
                        pricing_basis: payload.pricing_basis,
                        prices: BTreeMap::new(),
                        declared_capabilities: Some(capabilities.clone()),
                        embedding_capabilities: capabilities.embedding.clone(),
                        conformance_digest: String::new(),
                        conformance_result: None,
                        text: capabilities
                            .generation
                            .as_ref()
                            .is_some_and(|value| value.content.text),
                        images: capabilities
                            .generation
                            .as_ref()
                            .is_some_and(|value| value.content.images),
                        tools: capabilities
                            .generation
                            .as_ref()
                            .is_some_and(|value| value.content.tools),
                        structured_json: capabilities
                            .generation
                            .as_ref()
                            .is_some_and(|value| value.content.structured_json),
                        streaming: capabilities
                            .generation
                            .as_ref()
                            .is_some_and(|value| value.streaming),
                        pii_placeholder_preservation_percent: 0,
                    },
                );
            }
            "llm-route" => {
                let payload: RoutePayload =
                    serde_json::from_value(resource.payload.clone()).map_err(contract_json)?;
                if payload.alias_name != resource.resource_id {
                    return Err(ProjectionError::Contract(
                        "route payload/resource id mismatch".to_string(),
                    ));
                }
                let alias_name = payload.alias_name.clone();
                config.aliases.insert(alias_name, alias_config(payload));
            }
            "llm-pricing" => prices.push(
                serde_json::from_value::<PricingPayload>(resource.payload.clone())
                    .map_err(contract_json)?,
            ),
            "llm-policy" => apply_policy_payload(&mut config, resource)?,
            other => {
                return Err(ProjectionError::Contract(format!(
                    "unsupported v3 projection resource type `{other}`"
                )));
            }
        }
    }
    apply_prices(&mut config, prices)?;
    if config.deployments.is_empty() || config.aliases.is_empty() {
        return Err(ProjectionError::Contract(
            "projection root has no deployable alias".to_string(),
        ));
    }
    Ok(config)
}

fn alias_config(payload: RoutePayload) -> AliasConfig {
    AliasConfig {
        operations: payload.operations,
        deployments: payload.deployments,
        max_attempts: payload.max_attempts,
        concurrency: payload.concurrency,
        max_input_tokens: payload.max_input_tokens,
        max_output_tokens: payload.max_output_tokens,
        max_cost_micros: payload.max_cost_micros,
        internal: payload.internal,
        bound_principal: payload.bound_principal,
        audit: payload.audit,
        pii: payload.pii,
        required_capabilities: payload.required_capabilities,
        require_expected_embedding_space: payload.require_expected_embedding_space,
        embedding_workload_lane: payload.embedding_workload_lane,
    }
}

fn apply_policy_payload(
    config: &mut LlmRouterConfig,
    resource: &ProjectionResource,
) -> Result<(), ProjectionError> {
    let payload: PolicyPayload =
        serde_json::from_value(resource.payload.clone()).map_err(contract_json)?;
    if let Some(value) = payload.global_concurrency {
        require_local_bound("globalConcurrency", value, config.global_concurrency)?;
    }
    if let Some(value) = payload.global_stream_concurrency {
        require_local_bound(
            "globalStreamConcurrency",
            value,
            config.global_stream_concurrency,
        )?;
    }
    if let Some(value) = payload.stream_channel_capacity {
        require_local_bound(
            "streamChannelCapacity",
            value,
            config.stream_channel_capacity,
        )?;
    }
    if let Some(value) = payload.max_stream_response_bytes {
        require_local_bound(
            "maxStreamResponseBytes",
            value,
            config.max_stream_response_bytes,
        )?;
    }
    if let Some(value) = payload.stream_write_timeout_ms {
        require_local_bound(
            "streamWriteTimeoutMs",
            value,
            config.stream_write_timeout_ms,
        )?;
    }
    if let Some(value) = payload.max_replay_bytes {
        require_local_bound("maxReplayBytes", value, config.max_replay_bytes)?;
    }
    if let Some(value) = payload.request_timeout_ms {
        require_local_bound("requestTimeoutMs", value, config.request_timeout_ms)?;
    }
    Ok(())
}

fn apply_prices(
    config: &mut LlmRouterConfig,
    prices: Vec<PricingPayload>,
) -> Result<(), ProjectionError> {
    for price in prices {
        let deployment = config
            .deployments
            .get_mut(&price.deployment_id)
            .ok_or_else(|| {
                ProjectionError::Contract("pricing references a missing deployment".to_string())
            })?;
        let operation_price = match price.operation {
            Operation::Generate => OperationPrice::Generate(GenerationPrice {
                version: price.price_version,
                input_micros_per_million: price.input_micros_per_million,
                output_micros_per_million: price.output_micros_per_million.ok_or_else(|| {
                    ProjectionError::Contract("generation price requires output rate".to_string())
                })?,
            }),
            Operation::Embed => {
                if price.output_micros_per_million.is_some() {
                    return Err(ProjectionError::Contract(
                        "embedding price must not contain an output rate".to_string(),
                    ));
                }
                OperationPrice::Embed(EmbeddingPrice {
                    version: price.price_version,
                    input_micros_per_million: price.input_micros_per_million,
                })
            }
        };
        if deployment
            .prices
            .insert(price.operation, operation_price)
            .is_some()
        {
            return Err(ProjectionError::Contract(
                "duplicate deployment operation price".to_string(),
            ));
        }
    }
    Ok(())
}

fn require_local_bound<T: PartialEq>(
    name: &str,
    published: T,
    configured: T,
) -> Result<(), ProjectionError> {
    if published != configured {
        return Err(ProjectionError::Contract(format!(
            "projection `{name}` conflicts with the replica-local safety bound"
        )));
    }
    Ok(())
}

fn canonical_digest(value: &Value) -> Result<String, ProjectionError> {
    let canonical = canonicalize(value);
    let encoded = serde_json::to_vec(&canonical).map_err(contract_json)?;
    Ok(format!("{:x}", Sha256::digest(encoded)))
}

fn aggregate_resolved_trust_digest(config: &LlmRouterConfig) -> String {
    let trust = config
        .providers
        .iter()
        .map(|(id, provider)| {
            (
                id,
                provider
                    .network_profile
                    .tls
                    .as_ref()
                    .map(|value| value.trust_bundle_sha256.as_str())
                    .unwrap_or("public-roots"),
            )
        })
        .collect::<BTreeMap<_, _>>();
    canonical_digest(&serde_json::to_value(trust).expect("trust material serializes"))
        .expect("trust material canonicalizes")
}

fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(canonicalize).collect()),
        Value::Object(values) => {
            let sorted = values
                .iter()
                .map(|(key, value)| (key.clone(), canonicalize(value)))
                .collect::<BTreeMap<_, _>>();
            Value::Object(sorted.into_iter().collect::<Map<_, _>>())
        }
        other => other.clone(),
    }
}

fn remove_field(value: &mut Value, field: &str) -> Result<(), ProjectionError> {
    value
        .as_object_mut()
        .and_then(|object| object.remove(field))
        .ok_or_else(|| ProjectionError::Contract(format!("projection lacks `{field}`")))?;
    Ok(())
}

fn load_checkpoint(path: &Path) -> Result<ProjectionCheckpoint, ProjectionError> {
    if !path.exists() {
        return Ok(ProjectionCheckpoint::default());
    }
    let bytes = fs::read(path).map_err(transport)?;
    serde_json::from_slice(&bytes).map_err(|error| {
        ProjectionError::Contract(format!("invalid projection checkpoint: {error}"))
    })
}

fn atomic_write_json(path: &Path, value: &impl Serialize) -> Result<(), ProjectionError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(transport)?;
    }
    let bytes = serde_json::to_vec(value).map_err(contract_json)?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&temporary, bytes).map_err(transport)?;
    fs::rename(&temporary, path).map_err(transport)
}

fn validate_path_component(value: &str) -> Result<(), ProjectionError> {
    if value.is_empty()
        || matches!(value, "." | "..")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(ProjectionError::Contract(
            "projection artifact identifier is not path-safe".to_string(),
        ));
    }
    Ok(())
}

fn version_is_newer(required: &str, actual: &str) -> Result<bool, ProjectionError> {
    fn parse(value: &str) -> Result<[u64; 3], ProjectionError> {
        let core = value.split_once('-').map_or(value, |(core, _)| core);
        let parts = core
            .split('.')
            .map(str::parse::<u64>)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| {
                ProjectionError::Contract("invalid semantic gateway version".to_string())
            })?;
        if parts.len() != 3 {
            return Err(ProjectionError::Contract(
                "invalid semantic gateway version".to_string(),
            ));
        }
        Ok([parts[0], parts[1], parts[2]])
    }
    Ok(parse(required)? > parse(actual)?)
}

fn resource_key(resource_type: &str, resource_id: &str) -> String {
    format!("{resource_type}/{resource_id}")
}

fn normalize_digest(value: &str) -> String {
    value
        .strip_prefix("sha256:")
        .unwrap_or(value)
        .to_ascii_lowercase()
}

fn transport(error: impl std::fmt::Display) -> ProjectionError {
    ProjectionError::Transport(error.to_string())
}

fn contract_json(error: impl std::fmt::Display) -> ProjectionError {
    ProjectionError::Contract(error.to_string())
}

fn compile_error(error: LlmGatewayError) -> ProjectionError {
    ProjectionError::Compile(error.to_string())
}

fn unix_timestamp_string() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn default_deployment_concurrency() -> usize {
    32
}
fn default_alias_concurrency() -> usize {
    64
}
fn default_attempts() -> usize {
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credentials::SecretResolver;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Mutex, RwLock};

    #[derive(Clone)]
    struct MemorySource(Arc<Mutex<ProjectionBundle>>);

    impl LlmProjectionSource for MemorySource {
        fn load_latest(&self) -> Result<ProjectionBundle, ProjectionError> {
            Ok(self.0.lock().unwrap().clone())
        }
    }

    #[derive(Default)]
    struct MemoryAcknowledgements(Mutex<Vec<ProjectionAcknowledgement>>);

    impl ProjectionAcknowledgementSink for MemoryAcknowledgements {
        fn acknowledge(
            &self,
            acknowledgement: &ProjectionAcknowledgement,
        ) -> Result<(), ProjectionError> {
            self.0.lock().unwrap().push(acknowledgement.clone());
            Ok(())
        }
    }

    #[derive(Default)]
    struct FailFirstAcknowledgement {
        attempts: AtomicUsize,
        accepted: Mutex<Vec<ProjectionAcknowledgement>>,
    }

    impl ProjectionAcknowledgementSink for FailFirstAcknowledgement {
        fn acknowledge(
            &self,
            acknowledgement: &ProjectionAcknowledgement,
        ) -> Result<(), ProjectionError> {
            if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(ProjectionError::Transport(
                    "temporary acknowledgement delivery failure".to_string(),
                ));
            }
            self.accepted.lock().unwrap().push(acknowledgement.clone());
            Ok(())
        }
    }

    #[derive(Default)]
    struct RotatingResolver(RwLock<BTreeMap<String, String>>);

    impl SecretResolver for RotatingResolver {
        fn resolve(&self, secret_ref: &str) -> Result<String, LlmGatewayError> {
            self.0
                .read()
                .unwrap()
                .get(secret_ref)
                .cloned()
                .ok_or_else(|| LlmGatewayError::Config("credential unavailable".to_string()))
        }
    }

    fn resource(
        resource_type: &str,
        resource_id: &str,
        sequence: u64,
        payload: Value,
    ) -> ProjectionResource {
        let mut resource = ProjectionResource {
            schema_version: PROJECTION_SCHEMA_VERSION.to_string(),
            host_id: "host-a".to_string(),
            environment: "prod".to_string(),
            resource_type: resource_type.to_string(),
            resource_id: resource_id.to_string(),
            resource_version: sequence.to_string(),
            sequence,
            digest: String::new(),
            payload,
        };
        let mut value = serde_json::to_value(&resource).unwrap();
        remove_field(&mut value, "digest").unwrap();
        resource.digest = canonical_digest(&value).unwrap();
        resource
    }

    fn bundle(sequence: u64) -> ProjectionBundle {
        let declared_capabilities: ProviderCapabilities =
            serde_json::from_value(serde_json::json!({
                "operations":["generate"],
                "generation":{"content":{"text":true,"images":false,"tools":false,
                    "parallelTools":false,"structuredJson":false,"reasoningUsage":false},
                    "streaming":true}
            }))
            .unwrap();
        let resources = [
            resource(
                "llm-deployment",
                "openai-primary",
                sequence,
                serde_json::json!({
                    "deploymentId":"openai-primary",
                    "providerId":"openai-account",
                    "providerProtocol":"openai_chat",
                    "baseUrl":"https://provider.example/v1",
                    "credentialRef":"credential://host-a/openai",
                    "quotaGroupId":"openai-capacity",
                    "model":"gpt-governed",
                    "concurrency":8,
                    "declaredCapabilities":declared_capabilities
                }),
            ),
            resource(
                "llm-route",
                "governed-chat",
                sequence,
                serde_json::json!({
                    "aliasName":"governed-chat",
                    "operations":["generate"],
                    "deployments":["openai-primary"],
                    "maxAttempts":1,
                    "concurrency":8,
                    "audit":"required"
                }),
            ),
            resource(
                "llm-pricing",
                "openai-primary-price",
                sequence,
                serde_json::json!({
                    "deploymentId":"openai-primary",
                    "operation":"generate",
                    "priceVersion":1,
                    "inputMicrosPerMillion":1000,
                    "outputMicrosPerMillion":2000
                }),
            ),
            resource("llm-policy", "runtime", sequence, serde_json::json!({})),
        ];
        let resources = resources
            .into_iter()
            .map(|resource| {
                (
                    resource_key(&resource.resource_type, &resource.resource_id),
                    resource,
                )
            })
            .collect::<BTreeMap<_, _>>();
        let references = resources
            .values()
            .map(|resource| ProjectionResourceReference {
                resource_type: resource.resource_type.clone(),
                resource_id: resource.resource_id.clone(),
                resource_version: resource.resource_version.clone(),
                digest: resource.digest.clone(),
                tombstone: false,
            })
            .collect();
        let mut manifest = ProjectionManifest {
            schema_version: PROJECTION_SCHEMA_VERSION.to_string(),
            publication_id: "publication-fixture".to_string(),
            host_id: "host-a".to_string(),
            environment: "prod".to_string(),
            sequence,
            minimum_gateway_version: "0.1.0".to_string(),
            enabled_routing_features: BTreeSet::from(["ordered-routing".to_string()]),
            resources: references,
            root_digest: String::new(),
        };
        let mut value = serde_json::to_value(&manifest).unwrap();
        remove_field(&mut value, "rootDigest").unwrap();
        manifest.root_digest = canonical_digest(&value).unwrap();
        ProjectionBundle {
            manifest,
            resources,
        }
    }

    fn refresh_manifest(bundle: &mut ProjectionBundle) {
        bundle.manifest.resources = bundle
            .resources
            .values()
            .map(|resource| ProjectionResourceReference {
                resource_type: resource.resource_type.clone(),
                resource_id: resource.resource_id.clone(),
                resource_version: resource.resource_version.clone(),
                digest: resource.digest.clone(),
                tombstone: false,
            })
            .collect();
        bundle.manifest.root_digest.clear();
        let mut value = serde_json::to_value(&bundle.manifest).unwrap();
        remove_field(&mut value, "rootDigest").unwrap();
        bundle.manifest.root_digest = canonical_digest(&value).unwrap();
    }

    fn initial_store(
        compiler: &LlmCompiler,
        projection: &ProjectionBundle,
    ) -> Arc<LlmSnapshotStore> {
        let config = assemble_config(&LlmRouterConfig::default(), projection).unwrap();
        let snapshot = compiler.compile(&config, 0, None).unwrap();
        Arc::new(LlmSnapshotStore::new(snapshot, 2))
    }

    #[test]
    fn embedding_pricing_requires_output_rate_to_be_absent_not_null() {
        let missing = serde_json::from_value::<PricingPayload>(serde_json::json!({
            "deploymentId":"embedding",
            "operation":"embed",
            "priceVersion":1,
            "inputMicrosPerMillion":10
        }))
        .unwrap();
        assert_eq!(missing.output_micros_per_million, None);
        assert!(
            serde_json::from_value::<PricingPayload>(serde_json::json!({
                "deploymentId":"embedding",
                "operation":"embed",
                "priceVersion":1,
                "inputMicrosPerMillion":10,
                "outputMicrosPerMillion":null
            }))
            .is_err()
        );
    }

    #[test]
    fn two_replicas_converge_and_rotation_preserves_capacity_identity() {
        let directory = tempfile::tempdir().unwrap();
        let resolver = Arc::new(RotatingResolver::default());
        resolver.0.write().unwrap().insert(
            "credential://host-a/openai".to_string(),
            "secret-one".to_string(),
        );
        let compiler_a = Arc::new(LlmCompiler::new(resolver.clone()));
        let compiler_b = Arc::new(LlmCompiler::new(resolver.clone()));
        let source = Arc::new(MemorySource(Arc::new(Mutex::new(bundle(1)))));
        let acks = Arc::new(MemoryAcknowledgements::default());
        let store_a = initial_store(&compiler_a, &source.load_latest().unwrap());
        let store_b = initial_store(&compiler_b, &source.load_latest().unwrap());
        let mut replica_a = LlmProjectionWorker::new(
            source.clone(),
            acks.clone(),
            compiler_a,
            store_a.clone(),
            directory.path().join("a-state.json"),
            "gateway-a",
            LlmRouterConfig::default(),
        )
        .unwrap();
        let mut replica_b = LlmProjectionWorker::new(
            source,
            acks.clone(),
            compiler_b,
            store_b.clone(),
            directory.path().join("b-state.json"),
            "gateway-b",
            LlmRouterConfig::default(),
        )
        .unwrap();

        assert_eq!(
            replica_a.apply_latest().unwrap(),
            ProjectionApplyOutcome::Published
        );
        assert_eq!(
            replica_b.apply_latest().unwrap(),
            ProjectionApplyOutcome::Published
        );
        assert_eq!(store_a.load().digest, store_b.load().digest);
        assert_eq!(acks.0.lock().unwrap().len(), 2);

        let old = store_a.load();
        let old_deployment = old.deployments["openai-primary"].clone();
        let old_root_digest = old.digest.clone();
        resolver.0.write().unwrap().insert(
            "credential://host-a/openai".to_string(),
            "secret-two".to_string(),
        );
        assert_eq!(
            replica_a.reload_secrets().unwrap(),
            ProjectionApplyOutcome::Published
        );
        let rotated = store_a.load();
        assert_eq!(rotated.digest, old_root_digest);
        assert!(
            !old_deployment
                .provider
                .ptr_eq(&rotated.deployments["openai-primary"].provider)
        );
        assert!(Arc::ptr_eq(
            &old_deployment.account,
            &rotated.deployments["openai-primary"].account
        ));
        assert!(Arc::ptr_eq(
            &old_deployment.permits,
            &rotated.deployments["openai-primary"].permits
        ));
        assert!(Arc::ptr_eq(
            &old_deployment.circuit,
            &rotated.deployments["openai-primary"].circuit
        ));
        assert!(Arc::ptr_eq(
            &old_deployment.readiness,
            &rotated.deployments["openai-primary"].readiness
        ));
        assert_eq!(replica_a.status().credential_generation, 1);
        let acknowledgements = acks.0.lock().unwrap();
        assert_eq!(acknowledgements.len(), 3);
        assert_eq!(acknowledgements.last().unwrap().material_generation, 1);
        assert!(acknowledgements.last().unwrap().verify_digest());
    }

    #[test]
    fn production_projection_accepts_declared_capabilities_without_conformance() {
        let config = assemble_config(&LlmRouterConfig::default(), &bundle(1)).unwrap();
        let resolver = Arc::new(RotatingResolver::default());
        resolver.0.write().unwrap().insert(
            "credential://host-a/openai".to_string(),
            "secret".to_string(),
        );
        LlmCompiler::new(resolver)
            .compile(&config, 1, None)
            .expect("declared capabilities must be sufficient for production routing");
    }

    #[test]
    fn production_projection_preserves_complete_generation_capabilities() {
        let mut candidate = bundle(1);
        let deployment = candidate
            .resources
            .get_mut("llm-deployment/openai-primary")
            .unwrap();
        deployment.payload["providerProtocol"] = serde_json::json!("openai_responses");
        deployment.payload["declaredCapabilities"]["generation"]["content"]["parallelTools"] =
            serde_json::json!(false);
        deployment.payload["declaredCapabilities"]["generation"]["content"]["reasoningUsage"] =
            serde_json::json!(true);

        let config = assemble_config(&LlmRouterConfig::default(), &candidate).unwrap();
        let declared = config.deployments["openai-primary"]
            .declared_capabilities
            .as_ref()
            .unwrap();
        let generation = declared.generation.as_ref().unwrap();
        assert!(!generation.content.parallel_tools);
        assert!(generation.content.reasoning_usage);

        let resolver = Arc::new(RotatingResolver::default());
        resolver.0.write().unwrap().insert(
            "credential://host-a/openai".to_string(),
            "secret".to_string(),
        );
        let snapshot = LlmCompiler::new(resolver)
            .compile(&config, 1, None)
            .unwrap();
        let runtime_generation = snapshot.deployments["openai-primary"]
            .capabilities
            .generation
            .as_ref()
            .unwrap();
        assert!(!runtime_generation.content.parallel_tools);
        assert!(runtime_generation.content.reasoning_usage);
    }

    #[test]
    fn production_projection_compiles_request_scoped_pii_without_endpoint_evidence() {
        let mut candidate = bundle(1);
        candidate
            .resources
            .get_mut("llm-route/governed-chat")
            .unwrap()
            .payload["pii"] = serde_json::json!({"enabled":true,"kinds":["email"]});

        let config = assemble_config(&LlmRouterConfig::default(), &candidate).unwrap();
        let resolver = Arc::new(RotatingResolver::default());
        resolver.0.write().unwrap().insert(
            "credential://host-a/openai".to_string(),
            "secret".to_string(),
        );
        LlmCompiler::new(resolver)
            .compile(&config, 1, None)
            .expect("request-scoped PII must not require Portal endpoint conformance");
    }

    #[test]
    fn legacy_publication_capabilities_bridge_into_the_declared_contract() {
        let mut candidate = bundle(1);
        let deployment = candidate
            .resources
            .get_mut("llm-deployment/openai-primary")
            .unwrap();
        let capabilities = deployment.payload["declaredCapabilities"].take();
        deployment
            .payload
            .as_object_mut()
            .unwrap()
            .remove("declaredCapabilities");
        deployment.payload["conformanceDigest"] = serde_json::json!("sha256:legacy");
        deployment.payload["conformanceResult"] = serde_json::json!({
            "capabilities": capabilities,
            "legacyFieldThatIsNoLongerTrusted": true
        });

        let config = assemble_config(&LlmRouterConfig::default(), &candidate).unwrap();
        assert!(
            config.deployments["openai-primary"]
                .declared_capabilities
                .as_ref()
                .unwrap()
                .supports(Operation::Generate)
        );
        assert!(
            config.deployments["openai-primary"]
                .conformance_result
                .is_none()
        );
    }

    #[test]
    fn v3_nvidia_embeddings_compile_from_declared_catalog_contract() {
        let sequence = 1;
        let embedding_space = serde_json::json!({
            "spaceId":"nvidia-nemotron-3-embed-1b-float-v1",
            "revision":1,
            "dimension":2048,
            "normalization":"l2",
            "distanceMetric":"cosine",
            "documentInputTransformVersion":"document-v1"
        });
        let resources = [
            resource(
                "llm-provider-endpoint",
                "nvidia-free-embedding-demo",
                sequence,
                serde_json::json!({
                    "providerEndpointId":"nvidia-free-embedding-demo",
                    "providerAccountId":"nvidia-free",
                    "providerProtocol":"openai_embeddings",
                    "baseUrl":"https://integrate.api.nvidia.com/v1",
                    "endpointAuth":{"mode":"bearer","credential_ref":"credential://host-a/nvidia"},
                    "networkProfile":{"mode":"public_tls","termination":"native","connection":{}},
                    "quotaGroupId":"nvidia-free"
                }),
            ),
            resource(
                "llm-deployment",
                "nvidia-nemotron-embed",
                sequence,
                serde_json::json!({
                    "deploymentId":"nvidia-nemotron-embed",
                    "deploymentRevisionId":"nvidia-nemotron-embed/r1",
                    "providerEndpointId":"nvidia-free-embedding-demo",
                    "model":"nvidia/nemotron-3-embed-1b",
                    "concurrency":4,
                    "runtimeCapacity":{
                        "physicalRuntimeId":"nvidia-hosted/nemotron-3-embed-1b",
                        "capacityDomainId":"nvidia-free",
                        "maxParallelRequests":4,"maxQueuedRequests":8,
                        "readinessPolicy":"immediate","coldStartTimeoutMs":30000,
                        "streamSetupTimeoutMs":10000,"requestTimeoutMs":30000
                    },
                    "pricingBasis":"external_provider",
                    "declaredCapabilities":{
                        "operations":["embed"],
                        "embedding":{
                            "maxBatchItems":1,"maxInputTokensPerItem":4096,
                            "maxAggregateInputTokens":4096,"supportedDimensions":[2048],
                            "supportedEncodings":["float"],"maxResponseBytes":16777216,
                            "space":embedding_space
                        }
                    }
                }),
            ),
            resource(
                "llm-pricing",
                "nvidia-nemotron-embed-price",
                sequence,
                serde_json::json!({
                    "deploymentId":"nvidia-nemotron-embed","operation":"embed",
                    "priceVersion":1,"inputMicrosPerMillion":0
                }),
            ),
            resource(
                "llm-route",
                "kb-index",
                sequence,
                serde_json::json!({
                    "aliasName":"kb-index","operations":["embed"],
                    "deployments":["nvidia-nemotron-embed"],"internal":true,
                    "boundPrincipal":"light-knowledge","requireExpectedEmbeddingSpace":true,
                    "embeddingWorkloadLane":"kb_index",
                    "requiredCapabilities":{"embeddingSpace":embedding_space}
                }),
            ),
        ]
        .into_iter()
        .map(|item| (resource_key(&item.resource_type, &item.resource_id), item))
        .collect();
        let mut candidate = bundle(sequence);
        candidate.manifest.schema_version = PROJECTION_SCHEMA_VERSION_V3.to_string();
        candidate.resources = resources;

        let config = assemble_config_v3(&LlmRouterConfig::default(), &candidate).unwrap();
        let resolver = Arc::new(RotatingResolver::default());
        resolver.0.write().unwrap().insert(
            "credential://host-a/nvidia".to_string(),
            "demo-key".to_string(),
        );
        let snapshot = LlmCompiler::new(resolver)
            .compile(&config, sequence, None)
            .expect("NVIDIA embeddings must compile without lifecycle or conformance state");
        assert_eq!(
            snapshot.aliases["kb-index"].deployments[0].model,
            "nvidia/nemotron-3-embed-1b"
        );
    }

    #[test]
    fn bad_root_and_gap_retain_last_valid_snapshot_until_resync() {
        let directory = tempfile::tempdir().unwrap();
        let resolver = Arc::new(RotatingResolver::default());
        resolver.0.write().unwrap().insert(
            "credential://host-a/openai".to_string(),
            "secret".to_string(),
        );
        let compiler = Arc::new(LlmCompiler::new(resolver));
        let source = Arc::new(MemorySource(Arc::new(Mutex::new(bundle(1)))));
        let acks = Arc::new(MemoryAcknowledgements::default());
        let store = initial_store(&compiler, &source.load_latest().unwrap());
        let mut worker = LlmProjectionWorker::new(
            source.clone(),
            acks,
            compiler,
            store.clone(),
            directory.path().join("state.json"),
            "gateway-a",
            LlmRouterConfig::default(),
        )
        .unwrap();
        worker.apply_latest().unwrap();
        let valid = store.load();

        let mut invalid = bundle(2);
        invalid.manifest.root_digest = "0".repeat(64);
        *source.0.lock().unwrap() = invalid;
        assert!(worker.apply_latest().is_err());
        assert!(Arc::ptr_eq(&valid, &store.load()));
        assert_eq!(worker.status().rejected_roots, 1);
        assert_eq!(
            worker.status().rejected_by_class[&ProjectionRejectionClass::Contract],
            1
        );
        let rejection = worker
            .last_rejection()
            .expect("rejection must be observable");
        assert_eq!(rejection.rejected_sequence, Some(2));
        assert_eq!(rejection.retained_generation, valid.generation);
        assert_eq!(rejection.retained_digest, valid.digest);

        *source.0.lock().unwrap() = bundle(3);
        assert!(matches!(
            worker.apply_latest(),
            Err(ProjectionError::SequenceGap {
                expected: 2,
                received: 3
            })
        ));
        assert!(Arc::ptr_eq(&valid, &store.load()));
        assert_eq!(
            worker.resync_latest().unwrap(),
            ProjectionApplyOutcome::Published
        );
        assert_eq!(store.load().generation, 3);
    }

    #[test]
    fn applied_root_retries_acknowledgement_without_republishing() {
        let directory = tempfile::tempdir().unwrap();
        let resolver = Arc::new(RotatingResolver::default());
        resolver.0.write().unwrap().insert(
            "credential://host-a/openai".to_string(),
            "secret".to_string(),
        );
        let compiler = Arc::new(LlmCompiler::new(resolver));
        let source = Arc::new(MemorySource(Arc::new(Mutex::new(bundle(1)))));
        let acknowledgements = Arc::new(FailFirstAcknowledgement::default());
        let store = initial_store(&compiler, &source.load_latest().unwrap());
        let mut worker = LlmProjectionWorker::new(
            source,
            acknowledgements.clone(),
            compiler,
            store.clone(),
            directory.path().join("state.json"),
            "gateway-a",
            LlmRouterConfig::default(),
        )
        .unwrap();

        assert!(matches!(
            worker.apply_latest(),
            Err(ProjectionError::Acknowledgement(_))
        ));
        let applied = store.load();
        assert_eq!(worker.status().last_applied_sequence, 1);
        assert_eq!(worker.status().applied_roots, 1);
        assert_eq!(worker.status().rejected_roots, 0);
        assert_eq!(worker.status().acknowledgement_failures, 1);

        assert_eq!(
            worker.apply_latest().unwrap(),
            ProjectionApplyOutcome::Duplicate
        );
        assert!(Arc::ptr_eq(&applied, &store.load()));
        let accepted = acknowledgements.accepted.lock().unwrap();
        assert_eq!(accepted.len(), 1);
        assert_eq!(accepted[0].sequence, 1);
    }

    #[test]
    fn pricing_only_root_reuses_provider_capacity_circuit_and_alias_accounting() {
        let directory = tempfile::tempdir().unwrap();
        let resolver = Arc::new(RotatingResolver::default());
        resolver.0.write().unwrap().insert(
            "credential://host-a/openai".to_string(),
            "secret".to_string(),
        );
        let compiler = Arc::new(LlmCompiler::new(resolver));
        let first = bundle(1);
        let source = Arc::new(MemorySource(Arc::new(Mutex::new(first.clone()))));
        let store = initial_store(&compiler, &first);
        let mut worker = LlmProjectionWorker::new(
            source.clone(),
            Arc::new(MemoryAcknowledgements::default()),
            compiler,
            store.clone(),
            directory.path().join("state.json"),
            "gateway-a",
            LlmRouterConfig::default(),
        )
        .unwrap();
        worker.apply_latest().unwrap();
        let before = store.load();
        let old_deployment = before.deployments["openai-primary"].clone();
        let old_alias = before.aliases["governed-chat"].clone();

        let mut second = bundle(2);
        for key in [
            "llm-deployment/openai-primary",
            "llm-route/governed-chat",
            "llm-policy/runtime",
        ] {
            second
                .resources
                .insert(key.to_string(), first.resources.get(key).unwrap().clone());
        }
        let pricing = second
            .resources
            .get_mut("llm-pricing/openai-primary-price")
            .unwrap();
        pricing.payload["outputMicrosPerMillion"] = serde_json::json!(2_500);
        pricing.digest.clear();
        let mut pricing_value = serde_json::to_value(&*pricing).unwrap();
        remove_field(&mut pricing_value, "digest").unwrap();
        pricing.digest = canonical_digest(&pricing_value).unwrap();
        refresh_manifest(&mut second);
        *source.0.lock().unwrap() = second;
        assert_eq!(
            worker.apply_latest().unwrap(),
            ProjectionApplyOutcome::Published
        );

        let after = store.load();
        let new_deployment = &after.deployments["openai-primary"];
        let new_alias = &after.aliases["governed-chat"];
        assert!(!Arc::ptr_eq(&old_deployment, new_deployment));
        assert_ne!(old_deployment.prices, new_deployment.prices);
        assert!(old_deployment.provider.ptr_eq(&new_deployment.provider));
        assert!(Arc::ptr_eq(
            &old_deployment.account,
            &new_deployment.account
        ));
        assert!(Arc::ptr_eq(
            &old_deployment.permits,
            &new_deployment.permits
        ));
        assert!(Arc::ptr_eq(
            &old_deployment.circuit,
            &new_deployment.circuit
        ));
        assert!(!Arc::ptr_eq(&old_alias, new_alias));
        assert!(Arc::ptr_eq(&old_alias.permits, &new_alias.permits));
        assert!(Arc::ptr_eq(&old_alias.ledger, &new_alias.ledger));
        assert!(Arc::ptr_eq(
            &before.principal_permits,
            &after.principal_permits
        ));
    }

    #[test]
    fn rollback_republishes_prior_resources_at_a_new_monotonic_sequence() {
        let directory = tempfile::tempdir().unwrap();
        let resolver = Arc::new(RotatingResolver::default());
        resolver.0.write().unwrap().insert(
            "credential://host-a/openai".to_string(),
            "secret".to_string(),
        );
        let compiler = Arc::new(LlmCompiler::new(resolver));
        let original = bundle(1);
        let source = Arc::new(MemorySource(Arc::new(Mutex::new(original.clone()))));
        let acknowledgements = Arc::new(MemoryAcknowledgements::default());
        let store = initial_store(&compiler, &original);
        let mut worker = LlmProjectionWorker::new(
            source.clone(),
            acknowledgements.clone(),
            compiler,
            store.clone(),
            directory.path().join("state.json"),
            "gateway-a",
            LlmRouterConfig::default(),
        )
        .unwrap();
        worker.apply_latest().unwrap();
        let original_root = store.load();
        let original_prices = original_root.deployments["openai-primary"].prices.clone();
        let original_circuit = original_root.deployments["openai-primary"].circuit.clone();

        let mut changed = bundle(2);
        let pricing = changed
            .resources
            .get_mut("llm-pricing/openai-primary-price")
            .unwrap();
        pricing.payload["outputMicrosPerMillion"] = serde_json::json!(9_999);
        pricing.digest.clear();
        let mut pricing_value = serde_json::to_value(&*pricing).unwrap();
        remove_field(&mut pricing_value, "digest").unwrap();
        pricing.digest = canonical_digest(&pricing_value).unwrap();
        refresh_manifest(&mut changed);
        *source.0.lock().unwrap() = changed;
        worker.apply_latest().unwrap();
        assert_ne!(
            store.load().deployments["openai-primary"].prices,
            original_prices
        );

        // Rollback is a new publication that references the last-known-good
        // immutable resources. The control-plane sequence never moves back.
        let mut rollback = original;
        rollback.manifest.sequence = 3;
        refresh_manifest(&mut rollback);
        let rollback_digest = rollback.manifest.root_digest.clone();
        *source.0.lock().unwrap() = rollback;
        assert_eq!(
            worker.apply_latest().unwrap(),
            ProjectionApplyOutcome::Published
        );

        let restored = store.load();
        assert_eq!(restored.generation, 3);
        assert_eq!(restored.digest, rollback_digest);
        assert_eq!(
            restored.deployments["openai-primary"].prices,
            original_prices
        );
        assert!(Arc::ptr_eq(
            &restored.deployments["openai-primary"].circuit,
            &original_circuit
        ));
        assert_eq!(worker.status().last_applied_sequence, 3);
        assert_eq!(
            acknowledgements.0.lock().unwrap().last().unwrap().sequence,
            3
        );
    }

    #[test]
    fn phase0_canonical_digest_vectors_remain_compatible() {
        let manifest: Value = serde_json::from_slice(include_bytes!(
            "../../../benchmarks/llm-gateway/manifests/projection-manifest.json"
        ))
        .unwrap();
        let resource: Value = serde_json::from_slice(include_bytes!(
            "../../../benchmarks/llm-gateway/manifests/projection-resource.json"
        ))
        .unwrap();
        let mut manifest_body = manifest.clone();
        let expected_manifest = manifest_body["rootDigest"].as_str().unwrap().to_string();
        remove_field(&mut manifest_body, "rootDigest").unwrap();
        assert_eq!(canonical_digest(&manifest_body).unwrap(), expected_manifest);
        let mut resource_body = resource.clone();
        let expected_resource = resource_body["digest"].as_str().unwrap().to_string();
        remove_field(&mut resource_body, "digest").unwrap();
        assert_eq!(canonical_digest(&resource_body).unwrap(), expected_resource);
    }

    #[test]
    fn config_cache_source_and_per_replica_acknowledgements_use_bounded_files() {
        let directory = tempfile::tempdir().unwrap();
        let projection = bundle(7);
        atomic_write_json(
            &directory.path().join("manifest.json"),
            &projection.manifest,
        )
        .unwrap();
        for resource in projection.resources.values() {
            let path = directory
                .path()
                .join("resources")
                .join(&resource.resource_type)
                .join(&resource.resource_id)
                .join(format!("{}.json", resource.resource_version));
            atomic_write_json(&path, resource).unwrap();
        }
        let loaded = FileProjectionSource::new(directory.path(), 1024 * 1024)
            .load_latest()
            .unwrap();
        validate_bundle(&loaded).unwrap();
        let sink = FileAcknowledgementSink::new(directory.path().join("acks"));
        for instance in ["gateway-a", "gateway-b"] {
            let mut acknowledgement = ProjectionAcknowledgement {
                schema_version: PROJECTION_ACKNOWLEDGEMENT_SCHEMA_VERSION.to_string(),
                gateway_publication_id: "publication-fixture".to_string(),
                host_id: "host-a".to_string(),
                environment: "prod".to_string(),
                inventory_id: "inventory-a".to_string(),
                inventory_generation: 1,
                inventory_digest: "a".repeat(64),
                sequence: 7,
                root_digest: loaded.manifest.root_digest.clone(),
                applied_schema_version: PROJECTION_SCHEMA_VERSION.to_string(),
                applied_at: "1".to_string(),
                gateway_version: GATEWAY_COMPILER_VERSION.to_string(),
                reader_version: "projection-reader-v3".to_string(),
                gateway_instance: instance.to_string(),
                material_generation: 1,
                resolved_trust_digest: "b".repeat(64),
                evidence_key_set_version: "keys-v1".to_string(),
                evidence_key_set_digest: "c".repeat(64),
                state: "applied".to_string(),
                failure_category: None,
                failure_code: None,
                acknowledgement_digest: String::new(),
            };
            acknowledgement.refresh_digest().unwrap();
            sink.acknowledge(&acknowledgement).unwrap();
        }
        let publication = directory.path().join(format!(
            "acks/publication-7-{}",
            loaded.manifest.root_digest
        ));
        assert!(publication.join("gateway-a.json").is_file());
        assert!(publication.join("gateway-b.json").is_file());
    }

    #[test]
    fn projection_path_components_reject_directory_traversal() {
        assert!(validate_path_component("safe.name-1_value").is_ok());
        for unsafe_component in [".", "..", "../resource", "resource/path"] {
            assert!(
                validate_path_component(unsafe_component).is_err(),
                "accepted unsafe component {unsafe_component:?}"
            );
        }
    }

    #[test]
    fn portal_embedding_route_contract_accepts_only_the_six_space_fields() {
        let route = serde_json::json!({
            "aliasName":"kb-query",
            "operations":["embed"],
            "deployments":["deployment-a"],
            "internal":true,
            "boundPrincipal":"kb-query-workload",
            "requireExpectedEmbeddingSpace":true,
            "embeddingWorkloadLane":"kb_query",
            "requiredCapabilities":{"embeddingSpace":{
                "spaceId":"docs", "revision":1, "dimension":768,
                "normalization":"l2", "distanceMetric":"cosine",
                "documentInputTransformVersion":"document-v1"
            }}
        });
        assert!(serde_json::from_value::<RoutePayload>(route.clone()).is_ok());
        let mut poisoned = route;
        poisoned["requiredCapabilities"]["embeddingSpace"]["providerModel"] =
            Value::String("must-not-cross-portal-projection".to_string());
        assert!(serde_json::from_value::<RoutePayload>(poisoned).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn projection_source_rejects_symlinks_that_resolve_outside_its_root() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("config-cache");
        let outside = directory.path().join("outside");
        fs::create_dir_all(root.join("resources/safe")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("1.json"), b"{}").unwrap();
        symlink(&outside, root.join("resources/safe/link")).unwrap();

        let source = FileProjectionSource::new(&root, 1024);
        let result = source.read_json::<Value>(&root.join("resources/safe/link/1.json"));
        assert!(matches!(result, Err(ProjectionError::Contract(_))));
    }
}
