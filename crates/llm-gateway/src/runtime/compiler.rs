use super::readiness::{DeploymentReadiness, DeploymentReadinessState};
use super::snapshot::{
    AliasPlan, DeploymentRuntime, EmbeddingMemoryBounds, LlmPublishedSnapshot,
    PrincipalPermitStripes, ProviderAccountRuntime,
};
use crate::audit::AuditTransportContext;
use crate::config::{
    EndpointAuth, LlmRouterConfig, NetworkProfileMode, NetworkTermination, PricingBasis,
    ReadinessPolicy,
};
use crate::credentials::{FileTrustBundleResolver, SecretResolver, TrustBundleResolver};
use crate::error::LlmGatewayError;
use crate::pii::validate_pii_promotion;
use crate::provider::{
    CompiledAddressPolicy, HttpEmbeddingProvider, HttpInferenceProvider, ProviderTransportMaterial,
};
use crate::routing::PassiveCircuit;
use crate::usage::{OperationPrice, UsageLedger};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use chrono::Utc;
use model_provider::conformance::{
    CapabilityRequirements, EvidenceKind, FixtureProvenance, TrustedEvidenceKeySet,
};
use model_provider::inference::{
    CompiledProvider, ContentCapabilities, EmbeddingCapabilities, GenerationCapabilities,
    Operation, ProviderCapabilities,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::Semaphore;

// The replay peak includes the retained canonical request, the active
// per-attempt clone, and serialization scratch while dispatch is prepared.
const EMBEDDING_REPLAY_RESIDENT_COPIES: usize = 3;

#[derive(Debug, Default)]
pub struct CompileProbe {
    pub secret_resolutions: AtomicU64,
    pub client_builds: AtomicU64,
}

pub struct LlmCompiler {
    resolver: Arc<dyn SecretResolver>,
    probe: Arc<CompileProbe>,
}

impl LlmCompiler {
    pub fn new(resolver: Arc<dyn SecretResolver>) -> Self {
        Self {
            resolver,
            probe: Arc::new(CompileProbe::default()),
        }
    }

    pub fn with_probe(resolver: Arc<dyn SecretResolver>, probe: Arc<CompileProbe>) -> Self {
        Self { resolver, probe }
    }

    pub fn compile(
        &self,
        config: &LlmRouterConfig,
        generation: u64,
        previous: Option<&LlmPublishedSnapshot>,
    ) -> Result<LlmPublishedSnapshot, LlmGatewayError> {
        validate(config)?;
        if let Some(previous) = previous {
            for (name, alias) in &config.aliases {
                if let Some(old) = previous.aliases.get(name)
                    && old.required_capabilities.embedding_space
                        != alias.required_capabilities.embedding_space
                {
                    return Err(LlmGatewayError::Config(format!(
                        "embedding space for alias `{name}` is immutable; publish a new alias/profile instead"
                    )));
                }
            }
        }
        let embedding_memory = compile_embedding_memory_bounds(config)?;
        let embedding_memory_permits = previous
            .filter(|old| old.embedding_memory == embedding_memory)
            .map(|old| Arc::clone(&old.embedding_memory_permits))
            .unwrap_or_else(|| Arc::new(Semaphore::new(embedding_memory.admission_slots)));
        warn_on_mixed_format_extension_narrowing(config);
        let encoded = serde_json::to_vec(config)
            .map_err(|error| LlmGatewayError::Config(error.to_string()))?;
        let digest = format!("{:x}", Sha256::digest(encoded));
        let timeout = Duration::from_millis(config.request_timeout_ms);
        let mut quota_concurrency = BTreeMap::<String, usize>::new();
        for deployment in config.deployments.values() {
            let provider = &config.providers[&deployment.provider];
            let quota = provider
                .quota_group_id
                .clone()
                .unwrap_or_else(|| deployment.provider.clone());
            let current = quota_concurrency.entry(quota).or_default();
            *current = current.checked_add(deployment.concurrency).ok_or_else(|| {
                LlmGatewayError::Config("provider-account concurrency overflows usize".to_string())
            })?;
        }
        let mut accounts = BTreeMap::<String, Arc<ProviderAccountRuntime>>::new();
        let mut providers = BTreeMap::new();
        for (id, provider) in &config.providers {
            let credential_ref = endpoint_credential_ref(provider)?;
            let secret = if let Some(reference) = credential_ref {
                self.probe
                    .secret_resolutions
                    .fetch_add(1, Ordering::Relaxed);
                Some(self.resolver.resolve(reference)?)
            } else {
                None
            };
            let (transport_material, resolved_trust_digest) =
                provider_transport_material(config, provider)?;
            let network_zone_digest = provider
                .network_profile
                .network_zone_id
                .as_ref()
                .and_then(|zone_id| config.network_zones.get(zone_id))
                .map(canonical_sha256);
            let capabilities = capabilities_for_provider(config, id);
            let material_digest = provider_digest(
                provider,
                secret.as_deref(),
                resolved_trust_digest.as_deref(),
                network_zone_digest.as_deref(),
            );
            let previous_client = previous.and_then(|old| {
                old.deployments.values().find(|deployment| {
                    deployment.provider_endpoint_id == *id
                        && deployment.provider_digest == material_digest
                })
            });
            let reusable_client = previous_client.filter(|deployment| {
                deployment.provider_client_built_at.elapsed()
                    < Duration::from_millis(
                        provider
                            .network_profile
                            .connection
                            .client_refresh_interval_ms,
                    )
            });
            let (client, client_generation, client_built_at) = match reusable_client {
                Some(deployment) => (
                    deployment.provider.clone(),
                    deployment.provider_client_generation,
                    deployment.provider_client_built_at,
                ),
                None => {
                    self.probe.client_builds.fetch_add(1, Ordering::Relaxed);
                    let client = match provider.provider_protocol.operation() {
                        Operation::Generate => CompiledProvider::Generation(Arc::new(
                            HttpInferenceProvider::build_with_material(
                                provider,
                                secret.as_deref(),
                                capabilities.generation.unwrap_or_default(),
                                timeout,
                                transport_material.clone(),
                            )?,
                        )),
                        Operation::Embed => CompiledProvider::Embedding(Arc::new(
                            HttpEmbeddingProvider::build_with_material(
                                provider,
                                secret.as_deref(),
                                capabilities.embedding.unwrap_or_default(),
                                timeout,
                                transport_material,
                            )?,
                        )),
                    };
                    (
                        client,
                        previous_client
                            .map(|deployment| {
                                deployment.provider_client_generation.saturating_add(1)
                            })
                            .unwrap_or(1),
                        std::time::Instant::now(),
                    )
                }
            };
            providers.insert(
                id.clone(),
                (client, material_digest, client_generation, client_built_at),
            );
            let quota = provider
                .quota_group_id
                .clone()
                .unwrap_or_else(|| id.clone());
            let previous_account = previous.and_then(|old| {
                old.deployments
                    .values()
                    .find(|deployment| deployment.account.quota_group_id == quota)
                    .map(|deployment| Arc::clone(&deployment.account))
                    .filter(|account| account.configured_concurrency == quota_concurrency[&quota])
            });
            accounts.entry(quota.clone()).or_insert_with(|| {
                previous_account.unwrap_or_else(|| {
                    Arc::new(ProviderAccountRuntime {
                        provider_account_id: if provider.provider_account_id.is_empty() {
                            id.clone()
                        } else {
                            provider.provider_account_id.clone()
                        },
                        quota_group_id: quota.clone(),
                        configured_concurrency: quota_concurrency[&quota],
                        permits: Arc::new(Semaphore::new(quota_concurrency[&quota])),
                    })
                })
            });
        }
        let mut deployments = BTreeMap::new();
        for (id, deployment) in &config.deployments {
            let required_conformance_provenance = deployment
                .conformance_result
                .as_ref()
                .map(|_| config.production_projection.required_conformance_provenance);
            let provider_config = &config.providers[&deployment.provider];
            let quota = provider_config
                .quota_group_id
                .clone()
                .unwrap_or_else(|| deployment.provider.clone());
            let capabilities = capabilities_for_deployment(deployment);
            let audit_transport =
                audit_transport_context(&deployment.provider, provider_config, deployment);
            let (provider, provider_digest, provider_client_generation, provider_client_built_at) =
                &providers[&deployment.provider];
            let previous_deployment = previous.and_then(|old| old.deployments.get(id));
            let reusable = previous_deployment
                .filter(|old| {
                    old.model == deployment.model
                        && old.configured_concurrency == deployment.concurrency
                        && old.capabilities == capabilities
                        && old.conformance_result == deployment.conformance_result
                        && old.required_conformance_provenance == required_conformance_provenance
                        && old.prices == deployment.prices
                        && old.provider_digest == *provider_digest
                        && old.provider_client_generation == *provider_client_generation
                        && old.audit_transport == audit_transport
                        && old.readiness_policy() == readiness_policy(deployment)
                        && old.cold_start_timeout_ms == cold_start_timeout_ms(deployment)
                        && old.request_timeout_ms == request_timeout_ms(config, deployment)
                        && old.stream_setup_timeout_ms
                            == stream_setup_timeout_ms(config, deployment)
                })
                .cloned();
            let runtime = reusable.unwrap_or_else(|| {
                let retained_state = previous_deployment.filter(|old| {
                    old.model == deployment.model
                        && old.configured_concurrency == deployment.concurrency
                        && old.capabilities == capabilities
                        && old.conformance_result == deployment.conformance_result
                        && old.required_conformance_provenance == required_conformance_provenance
                        && old.account.quota_group_id == quota
                        && old.audit_transport == audit_transport
                        && old.readiness_policy() == readiness_policy(deployment)
                        && old.cold_start_timeout_ms == cold_start_timeout_ms(deployment)
                        && old.request_timeout_ms == request_timeout_ms(config, deployment)
                        && old.stream_setup_timeout_ms
                            == stream_setup_timeout_ms(config, deployment)
                });
                Arc::new(DeploymentRuntime {
                    id: id.clone(),
                    provider_endpoint_id: deployment.provider.clone(),
                    model: deployment.model.clone(),
                    configured_concurrency: deployment.concurrency,
                    provider: provider.clone(),
                    provider_digest: provider_digest.clone(),
                    provider_client_generation: *provider_client_generation,
                    provider_client_built_at: *provider_client_built_at,
                    audit_transport,
                    capabilities,
                    conformance_result: deployment.conformance_result.clone(),
                    required_conformance_provenance,
                    readiness_policy: readiness_policy(deployment),
                    readiness: retained_state
                        .map(|old| Arc::clone(&old.readiness))
                        .unwrap_or_else(|| {
                            Arc::new(DeploymentReadiness::new(initial_readiness(deployment)))
                        }),
                    cold_start_timeout_ms: cold_start_timeout_ms(deployment),
                    request_timeout_ms: request_timeout_ms(config, deployment),
                    stream_setup_timeout_ms: stream_setup_timeout_ms(config, deployment),
                    permits: retained_state
                        .map(|old| Arc::clone(&old.permits))
                        .unwrap_or_else(|| Arc::new(Semaphore::new(deployment.concurrency))),
                    circuit: retained_state
                        .map(|old| Arc::clone(&old.circuit))
                        .unwrap_or_else(|| {
                            Arc::new(PassiveCircuit::new(3, Duration::from_secs(30)))
                        }),
                    account: Arc::clone(&accounts[&quota]),
                    prices: deployment.prices.clone(),
                })
            });
            deployments.insert(id.clone(), runtime);
        }
        let aliases = config
            .aliases
            .iter()
            .map(|(name, alias)| {
                let plans = alias
                    .deployments
                    .iter()
                    .map(|id| Arc::clone(&deployments[id]))
                    .collect::<Vec<_>>();
                let previous_alias = previous.and_then(|old| old.aliases.get(name));
                let same_alias_contract = |old: &AliasPlan| {
                    old.deployments.len() == plans.len()
                        && old
                            .deployments
                            .iter()
                            .zip(&plans)
                            .all(|(old, new)| old.id == new.id)
                        && old.max_attempts == alias.max_attempts
                        && old.operations == alias.operations
                        && old.configured_concurrency == alias.concurrency
                        && old.max_input_tokens == alias.max_input_tokens
                        && old.max_output_tokens == alias.max_output_tokens
                        && old.max_cost_micros == alias.max_cost_micros
                        && old.internal == alias.internal
                        && old.bound_principal == alias.bound_principal
                        && old.audit == alias.audit
                        && old.pii == alias.pii
                        && old.required_capabilities == alias.required_capabilities
                        && old.require_expected_embedding_space
                            == alias.require_expected_embedding_space
                        && old.embedding_workload_lane == alias.embedding_workload_lane
                };
                let retained_state = previous_alias.filter(|old| same_alias_contract(old));
                let reusable = retained_state
                    .filter(|old| {
                        old.deployments
                            .iter()
                            .zip(&plans)
                            .all(|(old, new)| Arc::ptr_eq(old, new))
                    })
                    .cloned();
                (
                    name.clone(),
                    reusable.unwrap_or_else(|| {
                        Arc::new(AliasPlan {
                            public_name: name.clone(),
                            deployments: plans,
                            operations: alias.operations.clone(),
                            max_attempts: alias.max_attempts,
                            configured_concurrency: alias.concurrency,
                            permits: retained_state
                                .map(|old| Arc::clone(&old.permits))
                                .unwrap_or_else(|| Arc::new(Semaphore::new(alias.concurrency))),
                            max_input_tokens: alias.max_input_tokens,
                            max_output_tokens: alias.max_output_tokens,
                            max_cost_micros: alias.max_cost_micros,
                            internal: alias.internal,
                            bound_principal: alias.bound_principal.clone(),
                            audit: alias.audit,
                            pii: alias.pii.clone(),
                            required_capabilities: alias.required_capabilities.clone(),
                            require_expected_embedding_space: alias
                                .require_expected_embedding_space,
                            embedding_workload_lane: alias.embedding_workload_lane,
                            ledger: retained_state
                                .map(|old| Arc::clone(&old.ledger))
                                .unwrap_or_else(|| Arc::new(UsageLedger::default())),
                        })
                    }),
                )
            })
            .collect();
        let principal_permits = previous
            .map(|old| Arc::clone(&old.principal_permits))
            .unwrap_or_else(|| Arc::new(PrincipalPermitStripes::new(64, 16)));
        Ok(LlmPublishedSnapshot {
            generation,
            digest,
            global_concurrency: config.global_concurrency,
            global_stream_concurrency: config.global_stream_concurrency,
            stream_channel_capacity: config.stream_channel_capacity,
            max_stream_response_bytes: config.max_stream_response_bytes,
            stream_write_timeout_ms: config.stream_write_timeout_ms,
            stream_setup_timeout_ms: config.stream_setup_timeout_ms,
            stream_idle_timeout_ms: config.stream_idle_timeout_ms,
            stream_minimum_drain_bytes_per_second: config.stream_minimum_drain_bytes_per_second,
            stream_drain_grace_ms: config.stream_drain_grace_ms,
            max_replay_bytes: config.max_replay_bytes,
            embedding_memory,
            embedding_memory_permits,
            embedding_workload_lane: config.embedding_workload_lane,
            aliases,
            deployments,
            principal_permits,
        })
    }
}

fn readiness_policy(deployment: &crate::config::DeploymentConfig) -> ReadinessPolicy {
    deployment
        .runtime_capacity
        .as_ref()
        .map(|capacity| capacity.readiness_policy)
        .unwrap_or(ReadinessPolicy::Immediate)
}

fn initial_readiness(deployment: &crate::config::DeploymentConfig) -> DeploymentReadinessState {
    match readiness_policy(deployment) {
        ReadinessPolicy::Immediate => DeploymentReadinessState::Ready,
        ReadinessPolicy::WarmBeforeEligible => DeploymentReadinessState::Unqualified,
    }
}

fn cold_start_timeout_ms(deployment: &crate::config::DeploymentConfig) -> u64 {
    deployment
        .runtime_capacity
        .as_ref()
        .map(|capacity| capacity.cold_start_timeout_ms)
        .unwrap_or(30_000)
}

fn request_timeout_ms(
    config: &LlmRouterConfig,
    deployment: &crate::config::DeploymentConfig,
) -> u64 {
    deployment
        .runtime_capacity
        .as_ref()
        .map(|capacity| capacity.request_timeout_ms)
        .unwrap_or(config.request_timeout_ms)
}

fn stream_setup_timeout_ms(
    config: &LlmRouterConfig,
    deployment: &crate::config::DeploymentConfig,
) -> u64 {
    deployment
        .runtime_capacity
        .as_ref()
        .map(|capacity| capacity.stream_setup_timeout_ms)
        .unwrap_or(config.stream_setup_timeout_ms)
}

fn audit_transport_context(
    provider_endpoint_id: &str,
    provider: &crate::config::ProviderConfig,
    deployment: &crate::config::DeploymentConfig,
) -> AuditTransportContext {
    let capacity = deployment.runtime_capacity.as_ref();
    AuditTransportContext {
        network_profile_mode: match provider.network_profile.mode {
            NetworkProfileMode::PublicTls => "public_tls",
            NetworkProfileMode::PrivateTls => "private_tls",
            NetworkProfileMode::PrivatePlaintext => "private_plaintext",
        }
        .to_string(),
        termination: match provider.network_profile.termination {
            NetworkTermination::Native => "native",
            NetworkTermination::LightGatewaySidecar => "light_gateway_sidecar",
        }
        .to_string(),
        provider_endpoint_id: provider_endpoint_id.to_string(),
        profile_digest: canonical_sha256(&provider.network_profile),
        physical_runtime_id: capacity.map(|value| value.physical_runtime_id.clone()),
        capacity_domain_id: capacity.map(|value| value.capacity_domain_id.clone()),
        pricing_basis: match deployment.pricing_basis {
            PricingBasis::ExternalProvider => "external_provider",
            PricingBasis::ZeroMarginal => "zero_marginal",
            PricingBasis::AmortizedInternal => "amortized_internal",
        }
        .to_string(),
        trust_digest_prefix: provider
            .network_profile
            .tls
            .as_ref()
            .map(|value| value.trust_bundle_sha256.chars().take(12).collect()),
    }
}

fn warn_on_mixed_format_extension_narrowing(config: &LlmRouterConfig) {
    if config.openai_extension_allowlist.is_empty() {
        return;
    }
    for (alias_id, alias) in &config.aliases {
        let formats = alias
            .deployments
            .iter()
            .filter_map(|deployment_id| config.deployments.get(deployment_id))
            .filter_map(|deployment| config.providers.get(&deployment.provider))
            .map(|provider| provider.provider_protocol)
            .collect::<BTreeSet<_>>();
        if formats.len() > 1 {
            tracing::warn!(
                alias = %alias_id,
                extensions = ?config.openai_extension_allowlist,
                "mixed-format LLM alias uses strictest-wins parsing; OpenAI extensions will be rejected"
            );
        }
    }
}

fn validate(config: &LlmRouterConfig) -> Result<(), LlmGatewayError> {
    let now = Utc::now();
    let evidence_keys = trusted_evidence_keys(config)?;
    if !config.local_transport_enabled
        && config
            .providers
            .values()
            .any(|provider| provider.network_profile.mode != NetworkProfileMode::PublicTls)
    {
        return Err(LlmGatewayError::Config(
            "local transport not enabled".to_string(),
        ));
    }
    if config.path_prefix != "/v1"
        || config.global_concurrency == 0
        || config.global_stream_concurrency == 0
        || config.stream_channel_capacity == 0
        || config.max_stream_response_bytes == 0
        || config.stream_write_timeout_ms == 0
        || config.stream_setup_timeout_ms == 0
        || config.stream_idle_timeout_ms == 0
        || config.stream_minimum_drain_bytes_per_second == 0
        || config.stream_drain_grace_ms == 0
        || config.max_replay_bytes == 0
        || config.embedding_memory.max_request_body_bytes == 0
        || config.embedding_memory.max_replay_bytes == 0
        || config.embedding_memory.max_memory_bytes == 0
        || config.embedding_memory.ingress_concurrency == 0
        || config.embedding_memory.max_ingress_memory_bytes == 0
        || config.embedding_memory.items_per_permit == 0
        || config.embedding_memory.max_input_bytes_per_item == 0
        || config.embedding_memory.max_total_input_bytes == 0
        || config.embedding_memory.max_total_input_bytes
            > config.embedding_memory.max_request_body_bytes
        || config.embedding_memory.body_read_timeout_ms == 0
        || config.embedding_memory.minimum_receive_bytes_per_second == 0
        || config.embedding_memory.authorization_timeout_ms == 0
        || config.embedding_memory.write_timeout_ms == 0
        || config.embedding_memory.minimum_drain_bytes_per_second == 0
        || config.audit_runtime.max_record_bytes == 0
        || config.audit_runtime.max_segment_bytes == 0
        || config.audit_runtime.max_spool_bytes < config.audit_runtime.max_segment_bytes
        || config.audit_runtime.queue_records == 0
        || config.audit_runtime.batch_records == 0
        || config.audit_runtime.batch_bytes < config.audit_runtime.max_record_bytes
        || config.audit_runtime.commit_delay_ms == 0
        || config.audit_runtime.sink_batch_records == 0
        || config.audit_runtime.sink_batch_bytes < config.audit_runtime.max_record_bytes
        || config.audit_runtime.sink_poll_ms == 0
        || config.audit_runtime.sink_retry_max_ms < config.audit_runtime.sink_poll_ms
    {
        return Err(LlmGatewayError::Config(
            "invalid LLM router bounds or path prefix".to_string(),
        ));
    }
    for (id, provider) in &config.providers {
        let url = url::Url::parse(&provider.base_url).map_err(|error| {
            LlmGatewayError::Config(format!("provider `{id}` URL is invalid: {error}"))
        })?;
        let host = url
            .host_str()
            .ok_or_else(|| LlmGatewayError::Config(format!("provider `{id}` URL has no host")))?;
        let local = host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback());
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(LlmGatewayError::Config(format!(
                "provider `{id}` URL contains forbidden authority, query, or fragment data"
            )));
        }
        let credential_ref = endpoint_credential_ref(provider)?;
        if credential_ref.is_some_and(|reference| {
            !reference.starts_with("env:") && !reference.starts_with("credential://")
        }) && !config.development_fixtures
        {
            return Err(LlmGatewayError::Config(format!(
                "provider `{id}` has an unapproved credential reference"
            )));
        }
        let profile = &provider.network_profile;
        if profile.connection.pool_idle_timeout_ms == 0
            || profile.connection.client_refresh_interval_ms == 0
            || profile.connection.client_refresh_interval_ms
                < profile.connection.pool_idle_timeout_ms
        {
            return Err(LlmGatewayError::Config(format!(
                "provider `{id}` has invalid pool or client refresh bounds"
            )));
        }
        match profile.mode {
            NetworkProfileMode::PublicTls => {
                if !config.development_fixtures && (url.scheme() != "https" || local) {
                    return Err(LlmGatewayError::Config(format!(
                        "provider `{id}` must use HTTPS and a non-loopback host outside development fixtures"
                    )));
                }
                if profile.network_zone_id.is_some() || profile.tls.is_some() {
                    return Err(LlmGatewayError::Config(format!(
                        "public provider `{id}` cannot declare a private zone or trust bundle"
                    )));
                }
            }
            NetworkProfileMode::PrivateTls | NetworkProfileMode::PrivatePlaintext => {
                let zone_id = profile.network_zone_id.as_deref().ok_or_else(|| {
                    LlmGatewayError::Config(format!(
                        "private provider `{id}` requires a network zone"
                    ))
                })?;
                let zone = config.network_zones.get(zone_id).ok_or_else(|| {
                    LlmGatewayError::Config(format!(
                        "private provider `{id}` references an unknown network zone"
                    ))
                })?;
                let port = url.port_or_known_default().ok_or_else(|| {
                    LlmGatewayError::Config(format!("provider `{id}` URL has no effective port"))
                })?;
                if zone.id != zone_id || !zone.ports.contains(&port) {
                    return Err(LlmGatewayError::Config(format!(
                        "private provider `{id}` host zone does not permit its port"
                    )));
                }
                let host_allowed = host.parse::<std::net::IpAddr>().is_ok()
                    || zone
                        .dns_names
                        .iter()
                        .any(|allowed| dns_name_matches(allowed, host));
                if !host_allowed {
                    return Err(LlmGatewayError::Config(format!(
                        "private provider `{id}` host is not allowed by its network zone"
                    )));
                }
                match profile.mode {
                    NetworkProfileMode::PrivateTls => {
                        if url.scheme() != "https" || !zone.allow_private_tls {
                            return Err(LlmGatewayError::Config(format!(
                                "private TLS provider `{id}` has an incompatible scheme or zone"
                            )));
                        }
                        let trust = profile.tls.as_ref().ok_or_else(|| {
                            LlmGatewayError::Config(format!(
                                "private TLS provider `{id}` requires a trust bundle"
                            ))
                        })?;
                        validate_digest(&trust.trust_bundle_sha256, "trust bundle")?;
                    }
                    NetworkProfileMode::PrivatePlaintext => {
                        if url.scheme() != "http"
                            || !zone.allow_private_plaintext
                            || !matches!(provider.endpoint_auth, Some(EndpointAuth::None))
                            || profile.tls.is_some()
                        {
                            return Err(LlmGatewayError::Config(format!(
                                "private plaintext provider `{id}` requires HTTP, an approved zone, no TLS bundle, and endpointAuth none"
                            )));
                        }
                    }
                    NetworkProfileMode::PublicTls => unreachable!(),
                }
            }
        }
    }
    for (name, alias) in &config.aliases {
        alias.pii.validate()?;
        if name.is_empty()
            || name.contains(char::is_whitespace)
            || alias.deployments.is_empty()
            || alias.operations.is_empty()
            || alias.max_attempts == 0
            || alias.max_attempts > alias.deployments.len()
        {
            return Err(LlmGatewayError::Config(format!("invalid alias `{name}`")));
        }
        if alias.operations.contains(&Operation::Embed) && alias.pii.enabled {
            return Err(LlmGatewayError::Config(format!(
                "embedding alias `{name}` cannot use reversible PII tokenization"
            )));
        }
        if alias.internal && alias.bound_principal.as_deref().is_none_or(str::is_empty) {
            return Err(LlmGatewayError::Config(format!(
                "internal alias `{name}` must bind a principal"
            )));
        }
        let expected_embedding_space = alias.required_capabilities.embedding_space.as_ref();
        if alias.operations.contains(&Operation::Embed) {
            let Some(expected) = expected_embedding_space else {
                return Err(LlmGatewayError::Config(format!(
                    "embedding alias `{name}` requires an expected embedding-space contract"
                )));
            };
            validate_embedding_space(expected, &format!("alias `{name}`"))?;
        } else if expected_embedding_space.is_some() || alias.require_expected_embedding_space {
            return Err(LlmGatewayError::Config(format!(
                "non-embedding alias `{name}` cannot declare or require an embedding space"
            )));
        }
        if alias.embedding_workload_lane != crate::config::EmbeddingWorkloadLane::Standard
            && (!alias.operations.contains(&Operation::Embed)
                || !alias.internal
                || !alias.require_expected_embedding_space)
        {
            return Err(LlmGatewayError::Config(format!(
                "Knowledge Base workload alias `{name}` must be internal, embedding-only, and require the expected space"
            )));
        }
        // This is a conservative reload-time cross-check between the raw HTTP
        // body admission bound and the canonical request replay bound.
        // Canonicalization can increase or decrease the serialized size, so
        // the runtime still enforces the exact canonical size before dispatch.
        if alias.max_attempts > 1 && config.max_request_body_bytes > config.max_replay_bytes {
            return Err(LlmGatewayError::Config(format!(
                "multi-attempt alias `{name}` requires raw-body maxRequestBodyBytes <= canonical maxReplayBytes; exact canonical size is rechecked per request"
            )));
        }
        if alias.audit.is_local_durable() && !config.audit_runtime.persistent_volume {
            return Err(LlmGatewayError::Config(format!(
                "local-durable alias `{name}` requires declared persistent audit storage"
            )));
        }
        if config.production_projection.enabled
            && alias.audit != crate::config::AuditMode::Disabled
            && config
                .audit_runtime
                .sink_database_url_env
                .as_deref()
                .is_none_or(str::is_empty)
        {
            return Err(LlmGatewayError::Config(format!(
                "audited production alias `{name}` requires auditRuntime.sinkDatabaseUrlEnv"
            )));
        }
        if alias.audit == crate::config::AuditMode::RemoteDurable {
            return Err(LlmGatewayError::Config(format!(
                "remote-durable alias `{name}` is not implemented"
            )));
        }
        for deployment in &alias.deployments {
            let Some(candidate) = config.deployments.get(deployment) else {
                return Err(LlmGatewayError::Config(format!(
                    "alias `{name}` references missing deployment `{deployment}`"
                )));
            };
            let candidate_operation = config
                .providers
                .get(&candidate.provider)
                .ok_or_else(|| {
                    LlmGatewayError::Config(format!(
                        "alias `{name}` deployment `{deployment}` references a missing provider"
                    ))
                })?
                .provider_protocol
                .operation();
            if !alias.operations.contains(&candidate_operation)
                || !candidate.prices.contains_key(&candidate_operation)
                || !capabilities_for_deployment(candidate).supports(candidate_operation)
            {
                return Err(LlmGatewayError::Config(format!(
                    "alias `{name}` deployment `{deployment}` is not declared, supported, and priced for its provider operation"
                )));
            }
            if candidate_operation == Operation::Embed {
                let capabilities = capabilities_for_deployment(candidate)
                    .embedding
                    .ok_or_else(|| {
                        LlmGatewayError::Config(format!(
                            "embedding deployment `{deployment}` has no embedding capabilities"
                        ))
                    })?;
                let declared = capabilities.space.as_ref().ok_or_else(|| {
                    LlmGatewayError::Config(format!(
                        "embedding deployment `{deployment}` has no embedding-space contract"
                    ))
                })?;
                validate_embedding_space(declared, &format!("deployment `{deployment}`"))?;
                if Some(declared) != expected_embedding_space {
                    return Err(LlmGatewayError::Config(format!(
                        "embedding alias `{name}` mixes incompatible embedding spaces"
                    )));
                }
                let dimensions_match = if alias.require_expected_embedding_space {
                    capabilities.supported_dimensions.len() == 1
                        && capabilities
                            .supported_dimensions
                            .contains(&declared.dimension)
                } else {
                    capabilities
                        .supported_dimensions
                        .contains(&declared.dimension)
                };
                if !dimensions_match {
                    return Err(LlmGatewayError::Config(format!(
                        "embedding deployment `{deployment}` dimensions do not preserve alias `{name}` space"
                    )));
                }
            }
            let requirements = alias_requirements(
                alias,
                candidate_operation,
                candidate
                    .conformance_result
                    .as_ref()
                    .map(|_| config.production_projection.required_conformance_provenance),
            );
            match &candidate.conformance_result {
                Some(result) if !result.satisfies(&requirements, now) => {
                    return Err(LlmGatewayError::Config(format!(
                        "alias `{name}` requirements are not proven by deployment `{deployment}`"
                    )));
                }
                Some(result) => validate_pii_promotion(
                    &alias.pii,
                    &candidate.model,
                    result.pii_preservation.as_ref(),
                    now,
                )?,
                // Request-scoped PII is a gateway transform. Portal projection
                // does not require provider conformance evidence before the
                // operator can publish and validate the live configuration.
                None => {}
            }
        }
        for operation in &alias.operations {
            let covered = alias.deployments.iter().any(|deployment_id| {
                config
                    .deployments
                    .get(deployment_id)
                    .and_then(|deployment| config.providers.get(&deployment.provider))
                    .is_some_and(|provider| provider.provider_protocol.operation() == *operation)
            });
            if !covered {
                return Err(LlmGatewayError::Config(format!(
                    "alias `{name}` has no deployment for declared operation `{operation:?}`"
                )));
            }
        }
    }
    let mut runtime_registrations =
        BTreeMap::<String, (String, crate::config::NetworkProfile, String)>::new();
    for (id, deployment) in &config.deployments {
        if !config.providers.contains_key(&deployment.provider)
            || deployment.concurrency == 0
            || deployment.pii_placeholder_preservation_percent > 100
        {
            return Err(LlmGatewayError::Config(format!(
                "invalid deployment `{id}`"
            )));
        }
        let provider = &config.providers[&deployment.provider];
        if let Some(capacity) = &deployment.runtime_capacity {
            if capacity.physical_runtime_id.trim().is_empty()
                || capacity.capacity_domain_id.trim().is_empty()
                || capacity.max_parallel_requests == 0
                || capacity.max_queued_requests == 0
                || capacity.cold_start_timeout_ms == 0
                || capacity.stream_setup_timeout_ms == 0
                || capacity.request_timeout_ms == 0
                || capacity.stream_setup_timeout_ms > capacity.request_timeout_ms
                || capacity.request_timeout_ms > config.request_timeout_ms
                || capacity.stream_setup_timeout_ms > config.stream_setup_timeout_ms
                || deployment.concurrency > capacity.max_parallel_requests
            {
                return Err(LlmGatewayError::Config(format!(
                    "deployment `{id}` has invalid runtime capacity or exceeds declared parallelism"
                )));
            }
            let registration = (
                deployment.provider.clone(),
                provider.network_profile.clone(),
                capacity.capacity_domain_id.clone(),
            );
            if runtime_registrations
                .insert(capacity.physical_runtime_id.clone(), registration.clone())
                .is_some_and(|existing| existing != registration)
            {
                return Err(LlmGatewayError::Config(format!(
                    "physical runtime `{}` is registered through multiple endpoint or transport boundaries",
                    capacity.physical_runtime_id
                )));
            }
        } else if provider.network_profile.mode != NetworkProfileMode::PublicTls {
            return Err(LlmGatewayError::Config(format!(
                "local deployment `{id}` requires runtime capacity"
            )));
        }
        match deployment.pricing_basis {
            PricingBasis::ZeroMarginal
                if deployment
                    .prices
                    .values()
                    .any(|price| !price_is_zero(price)) =>
            {
                return Err(LlmGatewayError::Config(format!(
                    "deployment `{id}` zero_marginal pricing contains a non-zero rate"
                )));
            }
            PricingBasis::AmortizedInternal if deployment.prices.values().all(price_is_zero) => {
                return Err(LlmGatewayError::Config(format!(
                    "deployment `{id}` amortized_internal pricing requires a non-zero rate"
                )));
            }
            _ => {}
        }
        let provider_operation = provider.provider_protocol.operation();
        if !capabilities_for_deployment(deployment).supports(provider_operation)
            || !deployment.prices.contains_key(&provider_operation)
        {
            return Err(LlmGatewayError::Config(format!(
                "deployment `{id}` provider protocol, capabilities, and pricing operation do not match"
            )));
        }
        for (operation, price) in &deployment.prices {
            let matches = matches!(
                (operation, price),
                (Operation::Generate, OperationPrice::Generate(_))
                    | (Operation::Embed, OperationPrice::Embed(_))
            );
            if !matches {
                return Err(LlmGatewayError::Config(format!(
                    "deployment `{id}` price key does not match its operation variant"
                )));
            }
        }
        if let Some(result) = &deployment.conformance_result
            && (deployment.conformance_digest.len() != 64
                || !deployment
                    .conformance_digest
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
                || !result.verify_digest()
                || result.digest != deployment.conformance_digest
                || result.provider != provider.provider_protocol
                || !result.tested_operations.contains(&provider_operation)
                || result.physical_model != deployment.model
                || !result.is_current_and_passing(now))
        {
            return Err(LlmGatewayError::Config(format!(
                "deployment `{id}` has invalid, mismatched, or expired conformance evidence"
            )));
        }
        if !config.development_fixtures
            && let Some(result) = deployment.conformance_result.as_ref()
        {
            validate_live_evidence(id, provider, deployment, result, evidence_keys.as_ref())?;
        }
    }
    validate_embedding_lane_isolation(config)?;
    Ok(())
}

fn validate_embedding_lane_isolation(config: &LlmRouterConfig) -> Result<(), LlmGatewayError> {
    use crate::config::EmbeddingWorkloadLane::{KbIndex, KbQuery, Standard};
    let resources = |lane| {
        let mut deployments = BTreeSet::new();
        let mut accounts = BTreeSet::new();
        let mut capacity_domains = BTreeSet::new();
        for alias in config.aliases.values().filter(|alias| {
            alias.embedding_workload_lane == lane && alias.operations.contains(&Operation::Embed)
        }) {
            for deployment_id in &alias.deployments {
                deployments.insert(deployment_id.clone());
                if let Some(deployment) = config.deployments.get(deployment_id)
                    && let Some(provider) = config.providers.get(&deployment.provider)
                {
                    accounts.insert(
                        provider
                            .quota_group_id
                            .clone()
                            .unwrap_or_else(|| deployment.provider.clone()),
                    );
                    if let Some(capacity) = &deployment.runtime_capacity {
                        capacity_domains.insert(capacity.capacity_domain_id.clone());
                    }
                }
            }
        }
        (deployments, accounts, capacity_domains)
    };
    let (query_deployments, query_accounts, query_domains) = resources(KbQuery);
    let (index_deployments, index_accounts, index_domains) = resources(KbIndex);
    let (standard_deployments, standard_accounts, standard_domains) = resources(Standard);
    if !query_deployments.is_disjoint(&index_deployments)
        || !query_accounts.is_disjoint(&index_accounts)
        || !query_deployments.is_disjoint(&standard_deployments)
        || !query_accounts.is_disjoint(&standard_accounts)
        || !index_deployments.is_disjoint(&standard_deployments)
        || !index_accounts.is_disjoint(&standard_accounts)
        || !query_domains.is_disjoint(&index_domains)
        || !query_domains.is_disjoint(&standard_domains)
        || !index_domains.is_disjoint(&standard_domains)
    {
        return Err(LlmGatewayError::Config(
            "standard, KB query, and KB index embedding lanes must not share deployments or provider-account quota groups"
                .to_string(),
        ));
    }
    Ok(())
}

fn compile_embedding_memory_bounds(
    config: &LlmRouterConfig,
) -> Result<EmbeddingMemoryBounds, LlmGatewayError> {
    // Tiny strings can expand roughly 16x when represented as Value slots,
    // Vec capacity, and individual heap allocations. Use 20x to also cover
    // allocator bookkeeping and the peak while the Vec grows.
    const PARSED_ADMISSION_WIRE_MULTIPLIER: usize = 20;
    const PARSED_ADMISSION_FIXED_OVERHEAD_BYTES: usize = 64 * 1024;
    let limits = &config.embedding_memory;
    let checked_add = |left: usize, right: usize, label: &str| {
        left.checked_add(right)
            .ok_or_else(|| LlmGatewayError::Config(format!("embedding {label} overflows usize")))
    };
    let checked_mul = |left: usize, right: usize, label: &str| {
        left.checked_mul(right)
            .ok_or_else(|| LlmGatewayError::Config(format!("embedding {label} overflows usize")))
    };
    let parsed_admission_variable_overhead = checked_mul(
        limits.max_request_body_bytes,
        PARSED_ADMISSION_WIRE_MULTIPLIER,
        "parsed admission amplification",
    )?;
    let minimum_ingress_overhead = checked_add(
        parsed_admission_variable_overhead,
        PARSED_ADMISSION_FIXED_OVERHEAD_BYTES,
        "parsed admission overhead",
    )?;
    if limits.ingress_overhead_bytes < minimum_ingress_overhead {
        return Err(LlmGatewayError::Config(format!(
            "embedding ingressOverheadBytes must be at least maxRequestBodyBytes * {PARSED_ADMISSION_WIRE_MULTIPLIER} + {PARSED_ADMISSION_FIXED_OVERHEAD_BYTES} bytes for parsed admission amplification"
        )));
    }

    let max_ingress_resident_bytes = checked_add(
        limits.max_request_body_bytes,
        limits.ingress_overhead_bytes,
        "ingress resident bound",
    )?;
    let aggregate_ingress_bytes = checked_mul(
        max_ingress_resident_bytes,
        limits.ingress_concurrency,
        "ingress aggregate bound",
    )?;
    if aggregate_ingress_bytes > limits.max_ingress_memory_bytes {
        return Err(LlmGatewayError::Config(
            "embedding ingress memory bound exceeds maxEmbeddingIngressMemoryBytes".to_string(),
        ));
    }

    let mut alias_slots = 0_usize;
    let mut max_batch_items = 0_usize;
    let mut max_dimensions = 0_usize;
    let mut provider_response_bytes = 0_usize;
    for (alias_name, alias) in &config.aliases {
        if !alias.operations.contains(&Operation::Embed) {
            continue;
        }
        if alias.embedding_workload_lane != config.embedding_workload_lane {
            continue;
        }
        alias_slots = checked_add(alias_slots, alias.concurrency, "alias admission slots")?;
        if alias.max_attempts > 1 && limits.max_request_body_bytes > limits.max_replay_bytes {
            return Err(LlmGatewayError::Config(format!(
                "multi-attempt embedding alias `{alias_name}` requires embed request bytes <= embed replay bytes"
            )));
        }
        for deployment_id in &alias.deployments {
            let deployment = config.deployments.get(deployment_id).ok_or_else(|| {
                LlmGatewayError::Config(format!(
                    "embedding alias `{alias_name}` references a missing deployment"
                ))
            })?;
            let operation = config
                .providers
                .get(&deployment.provider)
                .ok_or_else(|| {
                    LlmGatewayError::Config(format!(
                        "embedding deployment `{deployment_id}` references a missing provider"
                    ))
                })?
                .provider_protocol
                .operation();
            if operation != Operation::Embed {
                continue;
            }
            let capabilities = capabilities_for_deployment(deployment);
            let embedding = capabilities.embedding.as_ref().ok_or_else(|| {
                LlmGatewayError::Config(format!(
                    "embedding deployment `{deployment_id}` has no embedding limits"
                ))
            })?;
            let batch = usize::try_from(embedding.max_batch_items).map_err(|_| {
                LlmGatewayError::Config("embedding batch limit does not fit usize".to_string())
            })?;
            let dimensions = embedding
                .supported_dimensions
                .iter()
                .next_back()
                .copied()
                .ok_or_else(|| {
                    LlmGatewayError::Config(format!(
                        "embedding deployment `{deployment_id}` has no supported dimension"
                    ))
                })? as usize;
            if batch == 0
                || embedding.max_input_tokens_per_item == 0
                || embedding.max_aggregate_input_tokens == 0
                || embedding.supported_encodings.is_empty()
                || embedding.max_response_bytes == 0
            {
                return Err(LlmGatewayError::Config(format!(
                    "embedding deployment `{deployment_id}` has zero capability bounds"
                )));
            }
            let weight = batch
                .checked_add(limits.items_per_permit - 1)
                .ok_or_else(|| {
                    LlmGatewayError::Config("embedding permit weight overflow".to_string())
                })?
                / limits.items_per_permit;
            if weight > deployment.concurrency {
                return Err(LlmGatewayError::Config(format!(
                    "embedding deployment `{deployment_id}` cannot admit its maximum weighted batch"
                )));
            }
            max_batch_items = max_batch_items.max(batch);
            max_dimensions = max_dimensions.max(dimensions);
            provider_response_bytes = provider_response_bytes.max(embedding.max_response_bytes);
        }
    }
    let admission_slots = config.global_concurrency.min(alias_slots);
    let max_replay_resident_bytes = checked_mul(
        limits.max_replay_bytes,
        EMBEDDING_REPLAY_RESIDENT_COPIES,
        "replay resident bound",
    )?;
    let vector_items = checked_mul(max_batch_items, max_dimensions, "vector element bound")?;
    let vector_values = checked_mul(
        vector_items,
        std::mem::size_of::<f32>(),
        "canonical vector bytes",
    )?;
    let vector_overhead = checked_mul(max_batch_items, 64, "canonical vector overhead")?;
    let max_canonical_vector_bytes =
        checked_add(vector_values, vector_overhead, "canonical vector bound")?;
    let rendered_values = checked_mul(vector_items, 17, "rendered float JSON bytes")?;
    let rendered_overhead = checked_add(
        checked_mul(max_batch_items, 128, "rendered item overhead")?,
        4096,
        "rendered envelope",
    )?;
    let max_rendered_response_bytes = checked_add(
        rendered_values,
        rendered_overhead,
        "rendered response bound",
    )?;
    let mut per_slot = limits.max_request_body_bytes;
    for (value, label) in [
        (max_replay_resident_bytes, "per-slot replay"),
        (max_canonical_vector_bytes, "per-slot vectors"),
        (max_rendered_response_bytes, "per-slot rendered response"),
        (provider_response_bytes, "per-slot provider response"),
    ] {
        per_slot = checked_add(per_slot, value, label)?;
    }
    let aggregate_peak_bytes = checked_mul(per_slot, admission_slots, "aggregate memory bound")?;
    if aggregate_peak_bytes > limits.max_memory_bytes {
        return Err(LlmGatewayError::Config(
            "embedding aggregate memory bound exceeds maxEmbeddingMemoryBytes".to_string(),
        ));
    }
    Ok(EmbeddingMemoryBounds {
        admission_slots,
        per_slot_peak_bytes: per_slot,
        aggregate_peak_bytes,
        max_memory_bytes: limits.max_memory_bytes,
        max_request_body_bytes: limits.max_request_body_bytes,
        max_replay_bytes: limits.max_replay_bytes,
        max_replay_resident_bytes,
        max_canonical_vector_bytes,
        max_rendered_response_bytes,
        overlapping_provider_response_bytes: provider_response_bytes,
        ingress_concurrency: limits.ingress_concurrency,
        max_ingress_resident_bytes,
        aggregate_ingress_bytes,
        max_ingress_memory_bytes: limits.max_ingress_memory_bytes,
        items_per_permit: limits.items_per_permit,
        write_timeout_ms: limits.write_timeout_ms,
        minimum_drain_bytes_per_second: limits.minimum_drain_bytes_per_second,
        max_input_bytes_per_item: limits.max_input_bytes_per_item,
        max_total_input_bytes: limits.max_total_input_bytes,
        body_read_timeout_ms: limits.body_read_timeout_ms,
        minimum_receive_bytes_per_second: limits.minimum_receive_bytes_per_second,
        authorization_timeout_ms: limits.authorization_timeout_ms,
    })
}

fn capabilities_for_provider(config: &LlmRouterConfig, provider: &str) -> ProviderCapabilities {
    let mut result = ProviderCapabilities::default();
    let mut merged_embedding_space: Option<
        Option<model_provider::inference::EmbeddingSpaceContract>,
    > = None;
    for deployment in config
        .deployments
        .values()
        .filter(|deployment| deployment.provider == provider)
    {
        let current = capabilities_for_deployment(deployment);
        result.operations.extend(current.operations);
        if let Some(current) = current.generation {
            let generation = result.generation.get_or_insert_with(Default::default);
            generation.content.text |= current.content.text;
            generation.content.images |= current.content.images;
            generation.content.tools |= current.content.tools;
            generation.content.parallel_tools |= current.content.parallel_tools;
            generation.content.structured_json |= current.content.structured_json;
            generation.content.reasoning_usage |= current.content.reasoning_usage;
            generation.streaming |= current.streaming;
        }
        if let Some(current) = current.embedding {
            let embedding = result
                .embedding
                .get_or_insert_with(EmbeddingCapabilities::default);
            embedding.max_batch_items = embedding.max_batch_items.max(current.max_batch_items);
            embedding.max_input_tokens_per_item = embedding
                .max_input_tokens_per_item
                .max(current.max_input_tokens_per_item);
            embedding.max_aggregate_input_tokens = embedding
                .max_aggregate_input_tokens
                .max(current.max_aggregate_input_tokens);
            embedding
                .supported_dimensions
                .extend(current.supported_dimensions);
            embedding
                .supported_encodings
                .extend(current.supported_encodings);
            embedding.max_response_bytes =
                embedding.max_response_bytes.max(current.max_response_bytes);
            merged_embedding_space = Some(match merged_embedding_space.take() {
                None => current.space,
                Some(existing) if existing == current.space => existing,
                Some(_) => None,
            });
        }
    }
    if let Some(embedding) = result.embedding.as_mut() {
        embedding.space = merged_embedding_space.flatten();
    }
    result
}

fn validate_embedding_space(
    contract: &model_provider::inference::EmbeddingSpaceContract,
    owner: &str,
) -> Result<(), LlmGatewayError> {
    if contract.space_id.trim().is_empty()
        || contract.space_id.len() > 255
        || contract.revision == 0
        || contract.dimension == 0
        || contract.document_input_transform_version.trim().is_empty()
        || contract.document_input_transform_version.len() > 255
    {
        return Err(LlmGatewayError::Config(format!(
            "{owner} has an invalid embedding-space contract"
        )));
    }
    Ok(())
}

fn capabilities_for_deployment(config: &crate::config::DeploymentConfig) -> ProviderCapabilities {
    if let Some(capabilities) = &config.declared_capabilities {
        return capabilities.clone();
    }
    if let Some(result) = &config.conformance_result {
        return result.capabilities.clone();
    }
    if let Some(embedding) = &config.embedding_capabilities {
        return ProviderCapabilities {
            operations: BTreeSet::from([Operation::Embed]),
            generation: None,
            embedding: Some(embedding.clone()),
        };
    }
    ProviderCapabilities {
        operations: BTreeSet::from([Operation::Generate]),
        generation: Some(GenerationCapabilities {
            content: ContentCapabilities {
                text: config.text,
                images: config.images,
                tools: config.tools,
                parallel_tools: config.tools,
                structured_json: config.structured_json,
                reasoning_usage: false,
            },
            streaming: config.streaming,
        }),
        embedding: None,
    }
}

fn alias_requirements(
    alias: &crate::config::AliasConfig,
    operation: Operation,
    required_provenance: Option<FixtureProvenance>,
) -> CapabilityRequirements {
    CapabilityRequirements {
        operation,
        images: alias.required_capabilities.images,
        tools: alias.required_capabilities.tools,
        parallel_tools: alias.required_capabilities.parallel_tools,
        structured_json: alias.required_capabilities.structured_json,
        reasoning: false,
        streaming: alias.required_capabilities.streaming,
        required_provenance,
    }
}

fn provider_digest(
    config: &crate::config::ProviderConfig,
    secret: Option<&str>,
    resolved_trust_digest: Option<&str>,
    network_zone_digest: Option<&str>,
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"provider-client-v2\0");
    digest.update(serde_json::to_vec(config).unwrap_or_default());
    digest.update([0]);
    if let Some(secret) = secret {
        digest.update(Sha256::digest(secret.as_bytes()));
    } else {
        digest.update(b"<none>");
    }
    digest.update([0]);
    digest.update(resolved_trust_digest.unwrap_or("<public-roots>").as_bytes());
    digest.update([0]);
    digest.update(network_zone_digest.unwrap_or("<public-zone>").as_bytes());
    format!("{:x}", digest.finalize())
}

fn provider_transport_material(
    config: &LlmRouterConfig,
    provider: &crate::config::ProviderConfig,
) -> Result<(ProviderTransportMaterial, Option<String>), LlmGatewayError> {
    let address_policy = match provider.network_profile.mode {
        NetworkProfileMode::PublicTls if config.development_fixtures => {
            CompiledAddressPolicy::development()
        }
        NetworkProfileMode::PublicTls => CompiledAddressPolicy::public_tls(),
        NetworkProfileMode::PrivateTls | NetworkProfileMode::PrivatePlaintext => {
            let zone_id = provider
                .network_profile
                .network_zone_id
                .as_ref()
                .ok_or_else(|| {
                    LlmGatewayError::Config("private provider has no zone".to_string())
                })?;
            let zone = config.network_zones.get(zone_id).ok_or_else(|| {
                LlmGatewayError::Config("private provider zone is unavailable".to_string())
            })?;
            let networks = zone
                .cidrs
                .iter()
                .map(|cidr| {
                    cidr.parse::<ipnet::IpNet>().map_err(|_| {
                        LlmGatewayError::Config(format!(
                            "network zone `{zone_id}` contains invalid CIDR `{cidr}`"
                        ))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            CompiledAddressPolicy::private(networks)?
        }
    };
    let (trust_bundle_pem, resolved_trust_digest) =
        if let Some(expected) = &provider.network_profile.tls {
            let resolver = FileTrustBundleResolver::new(
                config.production_projection.trust_bundle_files.clone(),
                1024 * 1024,
            );
            let resolved = resolver.resolve(&expected.trust_bundle_ref)?;
            if !resolved
                .sha256
                .eq_ignore_ascii_case(&expected.trust_bundle_sha256)
            {
                return Err(LlmGatewayError::Config(
                    "resolved trust bundle digest does not match projection".to_string(),
                ));
            }
            (Some(resolved.pem), Some(resolved.sha256))
        } else {
            (None, None)
        };
    Ok((
        ProviderTransportMaterial {
            address_policy,
            trust_bundle_pem,
        },
        resolved_trust_digest,
    ))
}

fn endpoint_credential_ref(
    provider: &crate::config::ProviderConfig,
) -> Result<Option<&str>, LlmGatewayError> {
    match &provider.endpoint_auth {
        Some(EndpointAuth::None) => Ok(None),
        Some(EndpointAuth::Bearer { credential_ref })
        | Some(EndpointAuth::ApiKey { credential_ref, .. }) => {
            if credential_ref.trim().is_empty() {
                Err(LlmGatewayError::Config(
                    "credential-bearing endpoint auth requires a non-empty reference".to_string(),
                ))
            } else {
                Ok(Some(credential_ref))
            }
        }
        None if provider.secret_ref.trim().is_empty() => Err(LlmGatewayError::Config(
            "legacy provider requires a non-empty secretRef".to_string(),
        )),
        None => Ok(Some(&provider.secret_ref)),
    }
}

fn validate_digest(value: &str, name: &str) -> Result<(), LlmGatewayError> {
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(LlmGatewayError::Config(format!(
            "{name} digest must be 64 hexadecimal characters"
        )))
    }
}

fn dns_name_matches(pattern: &str, host: &str) -> bool {
    let pattern = pattern.trim_end_matches('.').to_ascii_lowercase();
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if pattern == host {
        return true;
    }
    pattern.strip_prefix("*.").is_some_and(|suffix| {
        host.strip_suffix(&format!(".{suffix}"))
            .is_some_and(|label| !label.is_empty() && !label.contains('.'))
    })
}

#[cfg(test)]
mod dns_name_tests {
    use super::dns_name_matches;

    #[test]
    fn wildcard_matches_exactly_one_dns_label() {
        assert!(dns_name_matches("*.internal.example", "a.internal.example"));
        assert!(dns_name_matches(
            "*.INTERNAL.EXAMPLE.",
            "A.internal.example."
        ));
        assert!(!dns_name_matches("*.internal.example", "internal.example"));
        assert!(!dns_name_matches(
            "*.internal.example",
            "a.b.internal.example"
        ));
    }
}

fn price_is_zero(price: &OperationPrice) -> bool {
    match price {
        OperationPrice::Generate(price) => {
            price.input_micros_per_million == 0 && price.output_micros_per_million == 0
        }
        OperationPrice::Embed(price) => price.input_micros_per_million == 0,
    }
}

fn trusted_evidence_keys(
    config: &LlmRouterConfig,
) -> Result<Option<TrustedEvidenceKeySet>, LlmGatewayError> {
    if config.production_projection.evidence_public_keys.is_empty() {
        return Ok(None);
    }
    let keys = config
        .production_projection
        .evidence_public_keys
        .iter()
        .map(|(key_id, encoded)| {
            STANDARD
                .decode(encoded)
                .map(|key| (key_id.clone(), key))
                .map_err(|_| {
                    LlmGatewayError::Config(format!(
                        "evidence public key `{key_id}` is not valid base64"
                    ))
                })
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    let trust = TrustedEvidenceKeySet::new(
        config
            .production_projection
            .evidence_key_set_version
            .clone(),
        keys,
    )
    .map_err(|error| LlmGatewayError::Config(error.to_string()))?;
    let published = &config.production_projection.evidence_key_set_digest;
    if !published.is_empty() && !published.eq_ignore_ascii_case(trust.digest()) {
        return Err(LlmGatewayError::Config(
            "evidence key-set digest does not match protected keys".to_string(),
        ));
    }
    Ok(Some(trust))
}

fn validate_live_evidence(
    id: &str,
    provider: &crate::config::ProviderConfig,
    deployment: &crate::config::DeploymentConfig,
    result: &model_provider::conformance::ConformanceResult,
    evidence_keys: Option<&TrustedEvidenceKeySet>,
) -> Result<(), LlmGatewayError> {
    if result.schema_version != model_provider::conformance::CONFORMANCE_RESULT_SCHEMA_VERSION
        || result.evidence_kind != EvidenceKind::LiveEndpoint
    {
        return Err(LlmGatewayError::Config(format!(
            "deployment `{id}` requires ConformanceResult v2 live_endpoint evidence"
        )));
    }
    let trust = evidence_keys.ok_or_else(|| {
        LlmGatewayError::Config("protected evidence trust store is not configured".to_string())
    })?;
    result
        .verify_signature(trust)
        .map_err(|error| LlmGatewayError::Config(format!("live evidence signature: {error}")))?;
    let live = result.live_evidence.as_ref().ok_or_else(|| {
        LlmGatewayError::Config(format!("deployment `{id}` live evidence has no binding"))
    })?;
    let endpoint_digest = canonical_sha256(&serde_json::json!({
        "providerProtocol": provider.provider_protocol,
        "baseUrl": provider.base_url.trim_end_matches('/'),
        "endpointAuth": provider.endpoint_auth,
        "headerNames": provider.headers.keys().collect::<Vec<_>>(),
    }));
    let profile_digest = canonical_sha256(&provider.network_profile);
    let capacity_digest = canonical_sha256(&deployment.runtime_capacity);
    if live.deployment_revision_id != deployment.deployment_revision_id
        || live.provider_endpoint_sha256 != endpoint_digest
        || live.network_profile_sha256 != profile_digest
        || live.capacity_declaration_sha256 != capacity_digest
        || deployment
            .runtime_capacity
            .as_ref()
            .is_some_and(|capacity| live.physical_runtime_id != capacity.physical_runtime_id)
    {
        return Err(LlmGatewayError::Config(format!(
            "deployment `{id}` live evidence does not match endpoint, profile, revision, runtime, or capacity"
        )));
    }
    match provider.network_profile.termination {
        crate::config::NetworkTermination::Native => {
            if live.sidecar.is_some() {
                return Err(LlmGatewayError::Config(format!(
                    "native deployment `{id}` carries unexpected sidecar evidence"
                )));
            }
        }
        crate::config::NetworkTermination::LightGatewaySidecar => {
            let expected = deployment.sidecar.as_ref().ok_or_else(|| {
                LlmGatewayError::Config(format!(
                    "sidecar deployment `{id}` has no expected sidecar identity"
                ))
            })?;
            let sidecar = live.sidecar.as_ref().ok_or_else(|| {
                LlmGatewayError::Config(format!(
                    "sidecar deployment `{id}` has no signed sidecar evidence"
                ))
            })?;
            let vantage = live.runner_vantage.as_ref().ok_or_else(|| {
                LlmGatewayError::Config(format!(
                    "sidecar deployment `{id}` has no external runner vantage"
                ))
            })?;
            if sidecar.profile_version != expected.profile_version
                || sidecar.config_sha256 != expected.config_sha256
                || sidecar.raw_port_reachable
                || !is_digest(&sidecar.isolation_evidence_sha256)
                || vantage.source_network_namespace_id == vantage.target_network_namespace_id
                || !is_digest(&vantage.raw_probe_target_sha256)
            {
                return Err(LlmGatewayError::Config(format!(
                    "sidecar deployment `{id}` has mismatched identity or invalid isolation evidence"
                )));
            }
        }
    }
    Ok(())
}

fn canonical_sha256(value: &impl serde::Serialize) -> String {
    model_provider::conformance::sha256_hex(
        &model_provider::conformance::canonical_json_bytes(value)
            .expect("compiled LLM contract serializes"),
    )
}

fn is_digest(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
