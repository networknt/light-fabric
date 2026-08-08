use super::fixtures::{
    ConformanceCapability, CorpusFixture, CorpusManifest, FixtureKind, FixtureReference,
    ProviderProfile,
};
use crate::inference::capabilities::{
    ContentCapabilities, GenerationCapabilities, ProviderCapabilities,
};
use crate::inference::compatibility::OpenAiCompatibilityProfile;
use crate::inference::error::InferenceError;
use crate::inference::provider::{Operation, ProviderProtocol};
use crate::inference::request::InferenceRequest;
use crate::inference::stream::StreamDecoder;
use crate::providers::{anthropic, openai};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConformanceState {
    Pass,
    Fail,
    Quarantined,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaseResult {
    pub id: String,
    pub passed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityEvidence {
    pub fixture_ids: BTreeSet<String>,
    pub provenances: BTreeSet<super::fixtures::FixtureProvenance>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConformanceResult {
    pub schema_version: String,
    pub provider: ProviderProtocol,
    pub provider_codec_version: String,
    pub physical_model: String,
    pub api_version: String,
    pub tested_operations: BTreeSet<Operation>,
    pub capabilities: ProviderCapabilities,
    pub capability_evidence: BTreeMap<ConformanceCapability, CapabilityEvidence>,
    pub state: ConformanceState,
    pub corpus_version: String,
    pub corpus_digest: String,
    pub tested_at: DateTime<Utc>,
    pub valid_until: DateTime<Utc>,
    pub refresh_before_seconds: u64,
    pub pii_preservation: Option<Value>,
    pub cases: Vec<CaseResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quarantine_reason: Option<String>,
    pub digest: String,
}

impl ConformanceResult {
    pub fn is_current_and_passing(&self, now: DateTime<Utc>) -> bool {
        self.state == ConformanceState::Pass && now < self.valid_until && self.verify_digest()
    }

    pub fn quarantine(&self, reason: impl Into<String>) -> Self {
        let mut result = self.clone();
        result.state = ConformanceState::Quarantined;
        result.quarantine_reason = Some(reason.into());
        result.digest.clear();
        result.digest = digest_serializable(&result);
        result
    }

    pub fn verify_digest(&self) -> bool {
        let mut unsigned = self.clone();
        let expected = unsigned.digest.clone();
        unsigned.digest.clear();
        digest_serializable(&unsigned) == expected
    }

    /// Recomputes the canonical integrity digest after an authorized producer
    /// has populated or updated the result. This is integrity binding, not an
    /// authenticity mechanism; authenticity remains the responsibility of the
    /// publication channel.
    pub fn refresh_digest(&mut self) {
        self.digest.clear();
        self.digest = digest_serializable(self);
    }

    /// Returns true only when this exact, digested result is current, passing,
    /// and proves every capability required by the caller. Keeping validity
    /// and capability checks together prevents production routing from
    /// accidentally treating an expired or quarantined result as a capability
    /// declaration.
    pub fn satisfies(&self, requirements: &CapabilityRequirements, now: DateTime<Utc>) -> bool {
        self.is_current_and_passing(now) && requirements.satisfied_by(self)
    }

    pub fn capability_has_provenance(
        &self,
        capability: ConformanceCapability,
        provenance: super::fixtures::FixtureProvenance,
    ) -> bool {
        self.capability_evidence
            .get(&capability)
            .is_some_and(|evidence| evidence.provenances.contains(&provenance))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentEligibility {
    pub deployment_id: String,
    pub result: ConformanceResult,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityRequirements {
    pub operation: Operation,
    pub images: bool,
    pub tools: bool,
    pub parallel_tools: bool,
    pub structured_json: bool,
    pub reasoning: bool,
    pub streaming: bool,
    pub required_provenance: Option<super::fixtures::FixtureProvenance>,
}

impl CapabilityRequirements {
    pub fn generation() -> Self {
        Self {
            operation: Operation::Generate,
            images: false,
            tools: false,
            parallel_tools: false,
            structured_json: false,
            reasoning: false,
            streaming: false,
            required_provenance: None,
        }
    }

    pub fn embedding() -> Self {
        let mut requirements = Self::generation();
        requirements.operation = Operation::Embed;
        requirements
    }

    fn satisfied_by(&self, result: &ConformanceResult) -> bool {
        let capabilities = &result.capabilities;
        let generation = capabilities.generation.as_ref();
        capabilities.supports(self.operation)
            && (!self.images || generation.is_some_and(|value| value.content.images))
            && (!self.tools || generation.is_some_and(|value| value.content.tools))
            && (!self.parallel_tools
                || generation.is_some_and(|value| value.content.parallel_tools))
            && (!self.structured_json
                || generation.is_some_and(|value| value.content.structured_json))
            && (!self.reasoning
                || (result.provider == ProviderProtocol::OpenAiResponses
                    && generation.is_some_and(|value| value.content.reasoning_usage)))
            && (!self.streaming || generation.is_some_and(|value| value.streaming))
            && self.required_provenance.is_none_or(|provenance| {
                self.required_attestations()
                    .into_iter()
                    .all(|capability| result.capability_has_provenance(capability, provenance))
            })
    }

    fn required_attestations(&self) -> BTreeSet<ConformanceCapability> {
        let mut required = BTreeSet::new();
        match self.operation {
            Operation::Generate => {
                required.insert(ConformanceCapability::Generate);
            }
            Operation::Embed => {
                required.insert(ConformanceCapability::Embed);
            }
        }
        if self.images {
            required.insert(ConformanceCapability::Images);
        }
        if self.tools {
            required.insert(ConformanceCapability::Tools);
        }
        if self.parallel_tools {
            required.insert(ConformanceCapability::ParallelTools);
        }
        if self.structured_json {
            required.insert(ConformanceCapability::StructuredJson);
        }
        if self.reasoning {
            required.insert(ConformanceCapability::ReasoningUsage);
        }
        if self.streaming {
            required.insert(ConformanceCapability::Streaming);
        }
        required
    }
}

pub fn eligible_deployment_ids(
    deployments: &[DeploymentEligibility],
    requirements: &CapabilityRequirements,
    now: DateTime<Utc>,
) -> Vec<String> {
    deployments
        .iter()
        .filter(|deployment| deployment.result.satisfies(requirements, now))
        .map(|deployment| deployment.deployment_id.clone())
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeploymentDelta {
    pub schema_version: String,
    pub sequence: u64,
    pub deployment_id: String,
    pub previous_root_digest: String,
    pub conformance_digest: String,
    pub eligible: bool,
    pub root_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicationAcknowledgement {
    pub gateway_id: String,
    pub gateway_version: String,
    pub sequence: u64,
    pub root_digest: String,
    pub applied_at: DateTime<Utc>,
}

impl DeploymentDelta {
    pub fn new(
        sequence: u64,
        deployment_id: impl Into<String>,
        previous_root_digest: impl Into<String>,
        result: &ConformanceResult,
        now: DateTime<Utc>,
    ) -> Self {
        let mut delta = Self {
            schema_version: "1".to_string(),
            sequence,
            deployment_id: deployment_id.into(),
            previous_root_digest: previous_root_digest.into(),
            conformance_digest: result.digest.clone(),
            eligible: result.is_current_and_passing(now),
            root_digest: String::new(),
        };
        delta.root_digest = digest_serializable(&delta);
        delta
    }

    pub fn acknowledge(
        &self,
        gateway_id: impl Into<String>,
        gateway_version: impl Into<String>,
        applied_at: DateTime<Utc>,
    ) -> PublicationAcknowledgement {
        PublicationAcknowledgement {
            gateway_id: gateway_id.into(),
            gateway_version: gateway_version.into(),
            sequence: self.sequence,
            root_digest: self.root_digest.clone(),
            applied_at,
        }
    }
}

pub struct ConformanceRunner {
    compatibility: OpenAiCompatibilityProfile,
}

#[derive(Debug)]
struct LoadedFixture {
    reference: FixtureReference,
    fixture: CorpusFixture,
}

impl Default for ConformanceRunner {
    fn default() -> Self {
        let mut compatibility = OpenAiCompatibilityProfile::default();
        compatibility
            .extension_allowlist
            .insert("metadata".to_string());
        Self { compatibility }
    }
}

impl ConformanceRunner {
    pub fn run(
        &self,
        corpus_dir: &Path,
        tested_at: DateTime<Utc>,
    ) -> Result<Vec<ConformanceResult>, Box<dyn std::error::Error>> {
        let manifest: CorpusManifest =
            serde_json::from_slice(&fs::read(corpus_dir.join("manifest.json"))?)?;
        if manifest.schema_version != "1" {
            return Err("unsupported conformance manifest schema".into());
        }
        let fixtures = load_fixtures(corpus_dir, &manifest)?;
        manifest
            .profiles
            .iter()
            .map(|profile| self.run_profile(&manifest, profile, &fixtures, tested_at))
            .collect()
    }

    fn run_profile(
        &self,
        manifest: &CorpusManifest,
        profile: &ProviderProfile,
        fixtures: &[LoadedFixture],
        tested_at: DateTime<Utc>,
    ) -> Result<ConformanceResult, Box<dyn std::error::Error>> {
        let cases = fixtures
            .iter()
            .filter(|loaded| loaded.fixture.provider == profile.provider)
            .map(|loaded| match self.run_fixture(&loaded.fixture) {
                Ok(()) => CaseResult {
                    id: loaded.fixture.id.clone(),
                    passed: true,
                    detail: None,
                },
                Err(error) => CaseResult {
                    id: loaded.fixture.id.clone(),
                    passed: false,
                    detail: Some(error.to_string()),
                },
            })
            .collect::<Vec<_>>();
        let state = if !cases.is_empty() && cases.iter().all(|case| case.passed) {
            ConformanceState::Pass
        } else {
            ConformanceState::Fail
        };
        let capability_evidence =
            capability_evidence_from_passing_fixtures(profile.provider, fixtures, &cases);
        let capabilities = capabilities_from_evidence(profile, &capability_evidence);
        let operations = capabilities.operations.clone();
        let mut result = ConformanceResult {
            schema_version: "1".to_string(),
            provider: profile.provider,
            provider_codec_version: codec_version(profile.provider).to_string(),
            physical_model: profile.physical_model.clone(),
            api_version: profile.api_version.clone(),
            tested_operations: operations,
            capabilities,
            capability_evidence,
            state,
            corpus_version: manifest.corpus_version.clone(),
            corpus_digest: digest_serializable(manifest),
            tested_at,
            valid_until: tested_at + Duration::seconds(manifest.valid_for_seconds),
            refresh_before_seconds: manifest.refresh_before_seconds,
            pii_preservation: None,
            cases,
            quarantine_reason: None,
            digest: String::new(),
        };
        result.digest = digest_serializable(&result);
        Ok(result)
    }

    fn run_fixture(&self, fixture: &CorpusFixture) -> Result<(), Box<dyn std::error::Error>> {
        let actual = match fixture.kind {
            FixtureKind::Request => match fixture.provider {
                ProviderProtocol::OpenAiChat => {
                    let request: InferenceRequest = serde_json::from_value(fixture.input.clone())?;
                    openai::OpenAiCodec.encode_request(&request, false)?
                }
                ProviderProtocol::AnthropicMessages => {
                    let request: InferenceRequest = serde_json::from_value(fixture.input.clone())?;
                    anthropic::AnthropicCodec.encode_request(&request, false)?
                }
                ProviderProtocol::OpenAiResponses => {
                    let request: InferenceRequest = serde_json::from_value(fixture.input.clone())?;
                    openai::OpenAiResponsesCodec.encode_request(&request, false)?
                }
                ProviderProtocol::OpenAiEmbeddings => {
                    let request: crate::inference::EmbeddingRequest =
                        serde_json::from_value(fixture.input.clone())?;
                    openai::OpenAiEmbeddingsCodec.encode_request(&request)
                }
            },
            FixtureKind::Response => {
                let decoded: Result<Value, InferenceError> = match fixture.provider {
                    ProviderProtocol::OpenAiChat => openai::OpenAiCodec
                        .decode_response(&fixture.input)
                        .and_then(|value| {
                            serde_json::to_value(value).map_err(|error| {
                                InferenceError::provider_protocol(Some(502), error.to_string())
                            })
                        }),
                    ProviderProtocol::AnthropicMessages => anthropic::AnthropicCodec
                        .decode_response(&fixture.input)
                        .and_then(|value| {
                            serde_json::to_value(value).map_err(|error| {
                                InferenceError::provider_protocol(Some(502), error.to_string())
                            })
                        }),
                    ProviderProtocol::OpenAiResponses => openai::OpenAiResponsesCodec
                        .decode_response(&fixture.input)
                        .and_then(|value| {
                            serde_json::to_value(value).map_err(|error| {
                                InferenceError::provider_protocol(Some(502), error.to_string())
                            })
                        }),
                    ProviderProtocol::OpenAiEmbeddings => (|| {
                        let response = fixture.input.get("response").ok_or_else(|| {
                            InferenceError::invalid_request(
                                "embedding response fixture has no response",
                            )
                        })?;
                        let expected_count = fixture
                            .input
                            .get("expectedCount")
                            .and_then(Value::as_u64)
                            .and_then(|value| usize::try_from(value).ok())
                            .ok_or_else(|| {
                                InferenceError::invalid_request(
                                    "embedding response fixture has no expectedCount",
                                )
                            })?;
                        let expected_dimensions = fixture
                            .input
                            .get("expectedDimensions")
                            .and_then(Value::as_u64)
                            .and_then(|value| u32::try_from(value).ok());
                        openai::OpenAiEmbeddingsCodec
                            .decode_response(response, expected_count, expected_dimensions)
                            .and_then(|value| {
                                serde_json::to_value(value).map_err(|error| {
                                    InferenceError::provider_protocol(Some(502), error.to_string())
                                })
                            })
                    })(),
                };
                match decoded {
                    Ok(value) => value,
                    Err(error) if fixture.expected.get("errorCategory").is_some() => {
                        json!({"errorCategory":serde_json::to_value(error.category)?})
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            FixtureKind::Stream => match self.run_stream_fixture(fixture) {
                Ok(events) => events,
                Err(error) if fixture.expected.get("errorCategory").is_some() => {
                    json!({"errorCategory":serde_json::to_value(error.category)?})
                }
                Err(error) => return Err(error.into()),
            },
            FixtureKind::Error => {
                let status = fixture.input["status"]
                    .as_u64()
                    .ok_or("error fixture has no status")? as u16;
                let retry_after = fixture.input["retryAfter"].as_str();
                let body = serde_json::to_vec(&fixture.input["body"])?;
                let error = match fixture.provider {
                    ProviderProtocol::OpenAiChat => {
                        openai::OpenAiCodec.decode_error(status, retry_after, &body)
                    }
                    ProviderProtocol::AnthropicMessages => {
                        anthropic::AnthropicCodec.decode_error(status, retry_after, &body)
                    }
                    ProviderProtocol::OpenAiResponses => {
                        openai::OpenAiResponsesCodec.decode_error(status, retry_after, &body)
                    }
                    ProviderProtocol::OpenAiEmbeddings => {
                        openai::OpenAiCodec.decode_error(status, retry_after, &body)
                    }
                };
                serde_json::to_value(error)?
            }
            FixtureKind::Compatibility => {
                let provider_request = fixture.input["request"].clone();
                let expected_error = fixture.expected.get("errorCategory");
                let parsed = self
                    .compatibility
                    .parse_request(&serde_json::to_vec(&provider_request)?, fixture.provider);
                if expected_error.is_some() {
                    let error = parsed.expect_err("fixture expected compatibility error");
                    json!({"errorCategory":serde_json::to_value(error.category)?})
                } else {
                    serde_json::to_value(parsed?)?
                }
            }
        };
        if actual != fixture.expected {
            return Err(format!(
                "fixture {} mismatch\nexpected: {}\nactual: {}",
                fixture.id, fixture.expected, actual
            )
            .into());
        }
        Ok(())
    }

    fn run_stream_fixture(&self, fixture: &CorpusFixture) -> Result<Value, InferenceError> {
        let chunks = fixture.input["chunks"]
            .as_array()
            .ok_or_else(|| InferenceError::invalid_request("stream fixture has no chunks"))?;
        let mut events = Vec::new();
        match fixture.provider {
            ProviderProtocol::OpenAiChat => {
                let mut decoder = openai::OpenAiStreamDecoder::default();
                for chunk in chunks {
                    let chunk = chunk.as_str().ok_or_else(|| {
                        InferenceError::invalid_request("stream chunk is not text")
                    })?;
                    events.extend(decoder.push(chunk.as_bytes())?);
                }
                events.extend(decoder.finish()?);
            }
            ProviderProtocol::AnthropicMessages => {
                let mut decoder = anthropic::AnthropicStreamDecoder::default();
                for chunk in chunks {
                    let chunk = chunk.as_str().ok_or_else(|| {
                        InferenceError::invalid_request("stream chunk is not text")
                    })?;
                    events.extend(decoder.push(chunk.as_bytes())?);
                }
                events.extend(decoder.finish()?);
            }
            ProviderProtocol::OpenAiResponses => {
                let mut decoder = openai::OpenAiResponsesStreamDecoder::default();
                for chunk in chunks {
                    let chunk = chunk.as_str().ok_or_else(|| {
                        InferenceError::invalid_request("stream chunk is not text")
                    })?;
                    events.extend(decoder.push(chunk.as_bytes())?);
                }
                events.extend(decoder.finish()?);
            }
            ProviderProtocol::OpenAiEmbeddings => {
                return Err(InferenceError::unsupported(
                    "embedding protocol does not stream",
                ));
            }
        }
        serde_json::to_value(events).map_err(|error| InferenceError::protocol(error.to_string()))
    }
}

fn load_fixtures(
    corpus_dir: &Path,
    manifest: &CorpusManifest,
) -> Result<Vec<LoadedFixture>, Box<dyn std::error::Error>> {
    let mut fixtures = Vec::with_capacity(manifest.fixtures.len());
    for reference in &manifest.fixtures {
        let relative = Path::new(&reference.path);
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| matches!(component, Component::ParentDir | Component::RootDir))
        {
            return Err(format!("fixture path escapes corpus: {}", reference.path).into());
        }
        let bytes = fs::read(corpus_dir.join(relative))?;
        if sha256_hex(&bytes) != reference.sha256 {
            return Err(format!("fixture digest mismatch: {}", reference.path).into());
        }
        let fixture: CorpusFixture = serde_json::from_slice(&bytes)?;
        if fixture.schema_version != "1" {
            return Err(format!("unsupported fixture schema: {}", fixture.id).into());
        }
        for capability in &reference.covers {
            if !fixture_demonstrates(&fixture, *capability) {
                return Err(format!(
                    "fixture {} does not demonstrate claimed capability {capability:?}",
                    fixture.id
                )
                .into());
            }
        }
        fixtures.push(LoadedFixture {
            reference: reference.clone(),
            fixture,
        });
    }
    Ok(fixtures)
}

fn fixture_demonstrates(fixture: &CorpusFixture, capability: ConformanceCapability) -> bool {
    match capability {
        ConformanceCapability::Generate => fixture.kind == FixtureKind::Request,
        ConformanceCapability::Embed => {
            fixture.provider == ProviderProtocol::OpenAiEmbeddings
                && matches!(fixture.kind, FixtureKind::Request | FixtureKind::Response)
        }
        ConformanceCapability::Text => {
            contains_type(&fixture.input, &["text", "text_delta"])
                || contains_type(&fixture.expected, &["text", "text_delta"])
        }
        ConformanceCapability::Images => {
            contains_type(&fixture.input, &["image", "image_url"])
                || contains_type(&fixture.expected, &["image", "image_url"])
        }
        ConformanceCapability::Tools => {
            contains_non_empty_array_field(&fixture.input, "tools")
                || contains_type(
                    &fixture.input,
                    &[
                        "tool_call",
                        "tool_use",
                        "tool_result",
                        "tool_call_delta",
                        "function_call",
                        "function_call_output",
                    ],
                )
                || contains_type(
                    &fixture.expected,
                    &[
                        "tool_call",
                        "tool_use",
                        "tool_result",
                        "tool_call_delta",
                        "function_call",
                        "function_call_output",
                    ],
                )
        }
        ConformanceCapability::ParallelTools => {
            demonstrates_parallel_tools(&fixture.input)
                || demonstrates_parallel_tools(&fixture.expected)
        }
        ConformanceCapability::StructuredJson => {
            contains_type(&fixture.input, &["json_object", "json_schema"])
                || contains_type(&fixture.expected, &["json_object", "json_schema"])
        }
        ConformanceCapability::ReasoningUsage => {
            contains_numeric_field(&fixture.expected, "reasoningTokens")
        }
        ConformanceCapability::Streaming => {
            fixture.kind == FixtureKind::Stream && fixture.expected.is_array()
        }
    }
}

fn contains_type(value: &Value, accepted: &[&str]) -> bool {
    match value {
        Value::Array(items) => items.iter().any(|item| contains_type(item, accepted)),
        Value::Object(object) => {
            object
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| accepted.contains(&kind))
                || object.values().any(|value| contains_type(value, accepted))
        }
        _ => false,
    }
}

fn contains_non_empty_array_field(value: &Value, field: &str) -> bool {
    match value {
        Value::Array(items) => items
            .iter()
            .any(|item| contains_non_empty_array_field(item, field)),
        Value::Object(object) => {
            object
                .get(field)
                .and_then(Value::as_array)
                .is_some_and(|items| !items.is_empty())
                || object
                    .values()
                    .any(|value| contains_non_empty_array_field(value, field))
        }
        _ => false,
    }
}

fn contains_numeric_field(value: &Value, field: &str) -> bool {
    match value {
        Value::Array(items) => items.iter().any(|item| contains_numeric_field(item, field)),
        Value::Object(object) => {
            object.get(field).is_some_and(Value::is_number)
                || object
                    .values()
                    .any(|value| contains_numeric_field(value, field))
        }
        _ => false,
    }
}

fn demonstrates_parallel_tools(value: &Value) -> bool {
    fn collect(value: &Value, complete_calls: &mut usize, delta_indices: &mut BTreeSet<u64>) {
        match value {
            Value::Array(items) => {
                for item in items {
                    collect(item, complete_calls, delta_indices);
                }
            }
            Value::Object(object) => {
                match object.get("type").and_then(Value::as_str) {
                    Some("tool_call") | Some("tool_use") | Some("function_call") => {
                        *complete_calls += 1
                    }
                    Some("tool_call_delta") => {
                        if let Some(index) = object
                            .get("delta")
                            .and_then(|delta| delta.get("index"))
                            .and_then(Value::as_u64)
                        {
                            delta_indices.insert(index);
                        }
                    }
                    _ => {}
                }
                for nested in object.values() {
                    collect(nested, complete_calls, delta_indices);
                }
            }
            _ => {}
        }
    }

    let mut complete_calls = 0;
    let mut delta_indices = BTreeSet::new();
    collect(value, &mut complete_calls, &mut delta_indices);
    complete_calls >= 2 || delta_indices.len() >= 2
}

fn capability_evidence_from_passing_fixtures(
    provider: ProviderProtocol,
    fixtures: &[LoadedFixture],
    cases: &[CaseResult],
) -> BTreeMap<ConformanceCapability, CapabilityEvidence> {
    let passed = cases
        .iter()
        .filter(|case| case.passed)
        .map(|case| case.id.as_str())
        .collect::<BTreeSet<_>>();
    let mut evidence = BTreeMap::new();
    for loaded in fixtures.iter().filter(|loaded| {
        loaded.fixture.provider == provider && passed.contains(loaded.fixture.id.as_str())
    }) {
        for capability in &loaded.reference.covers {
            let entry = evidence
                .entry(*capability)
                .or_insert_with(|| CapabilityEvidence {
                    fixture_ids: BTreeSet::new(),
                    provenances: BTreeSet::new(),
                });
            entry.fixture_ids.insert(loaded.fixture.id.clone());
            entry.provenances.insert(loaded.reference.provenance);
        }
    }
    evidence
}

fn capabilities_from_evidence(
    profile: &ProviderProfile,
    evidence: &BTreeMap<ConformanceCapability, CapabilityEvidence>,
) -> ProviderCapabilities {
    let covered = evidence.keys().copied().collect::<BTreeSet<_>>();
    let mut operations = BTreeSet::new();
    if covered.contains(&ConformanceCapability::Generate) {
        operations.insert(Operation::Generate);
    }
    if covered.contains(&ConformanceCapability::Embed) {
        operations.insert(Operation::Embed);
    }
    ProviderCapabilities {
        operations,
        generation: covered.contains(&ConformanceCapability::Generate).then(|| {
            GenerationCapabilities {
                content: ContentCapabilities {
                    text: covered.contains(&ConformanceCapability::Text),
                    images: covered.contains(&ConformanceCapability::Images),
                    tools: covered.contains(&ConformanceCapability::Tools),
                    parallel_tools: covered.contains(&ConformanceCapability::ParallelTools),
                    structured_json: covered.contains(&ConformanceCapability::StructuredJson),
                    reasoning_usage: covered.contains(&ConformanceCapability::ReasoningUsage),
                },
                streaming: covered.contains(&ConformanceCapability::Streaming),
            }
        }),
        embedding: covered
            .contains(&ConformanceCapability::Embed)
            .then(|| profile.embedding_capabilities.clone())
            .flatten(),
    }
}

fn codec_version(provider: ProviderProtocol) -> &'static str {
    match provider {
        ProviderProtocol::OpenAiChat => openai::CODEC_VERSION,
        ProviderProtocol::AnthropicMessages => anthropic::CODEC_VERSION,
        ProviderProtocol::OpenAiResponses => openai::RESPONSES_CODEC_VERSION,
        ProviderProtocol::OpenAiEmbeddings => "openai-embeddings-v1",
    }
}

fn digest_serializable<T: Serialize>(value: &T) -> String {
    let value = serde_json::to_value(value).expect("conformance result serializes");
    let canonical = canonicalize(&value);
    sha256_hex(&serde_json::to_vec(&canonical).expect("canonical JSON serializes"))
}

fn canonicalize(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(canonicalize).collect()),
        Value::Object(object) => {
            let mut output = Map::new();
            let mut keys = object.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            for key in keys {
                output.insert(key.clone(), canonicalize(&object[key]));
            }
            Value::Object(output)
        }
        _ => value.clone(),
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corpus_manifest(corpus: &Path) -> CorpusManifest {
        serde_json::from_slice(&fs::read(corpus.join("manifest.json")).unwrap()).unwrap()
    }

    #[test]
    fn quarantine_and_expiry_remove_deployment_from_eligibility() {
        let now = DateTime::parse_from_rfc3339("2026-07-19T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("conformance/v1");
        let results = ConformanceRunner::default().run(&corpus, now).unwrap();
        assert_eq!(results.len(), 4);
        for result in &results {
            assert_eq!(result.state, ConformanceState::Pass, "{:#?}", result.cases);
            assert!(result.verify_digest());
        }
        let result = results[0].clone();
        assert_eq!(result.state, ConformanceState::Pass, "{:#?}", result.cases);
        let active = DeploymentEligibility {
            deployment_id: "openai-primary".to_string(),
            result: result.clone(),
        };
        assert_eq!(
            eligible_deployment_ids(&[active], &CapabilityRequirements::generation(), now),
            vec!["openai-primary"]
        );
        let quarantined = DeploymentEligibility {
            deployment_id: "openai-primary".to_string(),
            result: result.quarantine("codec drift"),
        };
        assert!(
            eligible_deployment_ids(&[quarantined], &CapabilityRequirements::generation(), now)
                .is_empty()
        );
        let expired = DeploymentEligibility {
            deployment_id: "openai-primary".to_string(),
            result,
        };
        assert!(
            eligible_deployment_ids(
                &[expired],
                &CapabilityRequirements::generation(),
                now + Duration::days(8)
            )
            .is_empty()
        );
    }

    #[test]
    fn quarantine_delta_is_sequenced_and_changes_root_digest() {
        let now = DateTime::parse_from_rfc3339("2026-07-19T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("conformance/v1");
        let result = ConformanceRunner::default()
            .run(&corpus, now)
            .unwrap()
            .remove(0);
        let pass_delta = DeploymentDelta::new(9, "openai-primary", "root-8", &result, now);
        let quarantined = result.quarantine("response codec drift");
        let quarantine_delta = DeploymentDelta::new(
            10,
            "openai-primary",
            &pass_delta.root_digest,
            &quarantined,
            now,
        );
        assert!(pass_delta.eligible);
        assert!(!quarantine_delta.eligible);
        assert_eq!(quarantine_delta.sequence, pass_delta.sequence + 1);
        assert_ne!(quarantine_delta.root_digest, pass_delta.root_digest);
        let acknowledgements = [
            quarantine_delta.acknowledge("gateway-a", "0.1.0", now),
            quarantine_delta.acknowledge("gateway-b", "0.1.0", now),
        ];
        assert!(acknowledgements.iter().all(|ack| {
            ack.sequence == quarantine_delta.sequence
                && ack.root_digest == quarantine_delta.root_digest
        }));
    }

    #[test]
    fn missing_required_capability_fails_closed() {
        let now = DateTime::parse_from_rfc3339("2026-07-19T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("conformance/v1");
        let anthropic = ConformanceRunner::default()
            .run(&corpus, now)
            .unwrap()
            .into_iter()
            .find(|result| result.provider == ProviderProtocol::AnthropicMessages)
            .unwrap();
        let deployment = DeploymentEligibility {
            deployment_id: "anthropic-fallback".to_string(),
            result: anthropic,
        };
        let requirements = CapabilityRequirements {
            structured_json: true,
            ..CapabilityRequirements::generation()
        };
        assert!(eligible_deployment_ids(&[deployment], &requirements, now).is_empty());
    }

    #[test]
    fn eligibility_enforces_provenance_for_every_required_capability() {
        let now = DateTime::parse_from_rfc3339("2026-07-19T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("conformance/v1");
        let mut result = ConformanceRunner::default()
            .run(&corpus, now)
            .unwrap()
            .into_iter()
            .find(|result| result.provider == ProviderProtocol::OpenAiChat)
            .unwrap();
        let captured = super::super::FixtureProvenance::CapturedSanitized;
        result
            .capability_evidence
            .get_mut(&ConformanceCapability::Generate)
            .unwrap()
            .provenances
            .insert(captured);
        result.digest.clear();
        result.digest = digest_serializable(&result);
        let requirements = CapabilityRequirements {
            images: true,
            required_provenance: Some(captured),
            ..CapabilityRequirements::generation()
        };
        let deployment = DeploymentEligibility {
            deployment_id: "openai-vision".to_string(),
            result: result.clone(),
        };
        assert!(eligible_deployment_ids(&[deployment], &requirements, now).is_empty());

        result
            .capability_evidence
            .get_mut(&ConformanceCapability::Images)
            .unwrap()
            .provenances
            .insert(captured);
        result.digest.clear();
        result.digest = digest_serializable(&result);
        let deployment = DeploymentEligibility {
            deployment_id: "openai-vision".to_string(),
            result,
        };
        assert_eq!(
            eligible_deployment_ids(&[deployment], &requirements, now),
            vec!["openai-vision"]
        );
    }

    #[test]
    fn capabilities_require_a_passing_covering_fixture() {
        let now = DateTime::parse_from_rfc3339("2026-07-19T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("conformance/v1");
        let mut manifest = corpus_manifest(&corpus);
        let image_fixture = manifest
            .fixtures
            .iter_mut()
            .find(|reference| reference.path == "openai/request-multimodal-tools.json")
            .unwrap();
        assert!(image_fixture.covers.remove(&ConformanceCapability::Images));
        let fixtures = load_fixtures(&corpus, &manifest).unwrap();
        let profile = manifest
            .profiles
            .iter()
            .find(|profile| profile.provider == ProviderProtocol::OpenAiChat)
            .unwrap();
        let result = ConformanceRunner::default()
            .run_profile(&manifest, profile, &fixtures, now)
            .unwrap();
        assert_eq!(result.state, ConformanceState::Pass);
        assert!(
            !result
                .capabilities
                .generation
                .as_ref()
                .unwrap()
                .content
                .images
        );
        assert!(
            result
                .capabilities
                .generation
                .as_ref()
                .unwrap()
                .content
                .tools
        );
        let tool_evidence = &result.capability_evidence[&ConformanceCapability::Tools];
        assert!(
            tool_evidence
                .provenances
                .contains(&super::super::FixtureProvenance::SyntheticSpecDerived)
        );
        assert!(!result.capability_has_provenance(
            ConformanceCapability::Tools,
            super::super::FixtureProvenance::CapturedSanitized
        ));
    }

    #[test]
    fn dishonest_capability_coverage_is_rejected() {
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("conformance/v1");
        let mut manifest = corpus_manifest(&corpus);
        manifest.fixtures[0]
            .covers
            .insert(ConformanceCapability::Images);
        let error = load_fixtures(&corpus, &manifest).unwrap_err();
        assert!(error.to_string().contains("does not demonstrate"));
    }

    #[test]
    fn fixture_paths_cannot_escape_the_corpus() {
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("conformance/v1");
        let mut manifest = corpus_manifest(&corpus);
        manifest.fixtures[0].path = "../outside.json".to_string();
        let error = load_fixtures(&corpus, &manifest).unwrap_err();
        assert!(error.to_string().contains("escapes corpus"));
    }

    #[test]
    fn fixture_digest_mismatch_is_rejected() {
        let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("conformance/v1");
        let mut manifest = corpus_manifest(&corpus);
        manifest.fixtures[0].sha256 = "0".repeat(64);
        let error = load_fixtures(&corpus, &manifest).unwrap_err();
        assert!(error.to_string().contains("digest mismatch"));
    }
}
