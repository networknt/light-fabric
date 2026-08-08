use super::snapshot::{
    AliasPlan, DeploymentRuntime, EmbeddingMemoryBounds, LlmPublishedSnapshot,
    PrincipalPermitStripes, ProviderAccountRuntime,
};
use crate::config::LlmRouterConfig;
use crate::credentials::SecretResolver;
use crate::error::LlmGatewayError;
use crate::pii::validate_pii_promotion;
use crate::provider::{HttpEmbeddingProvider, HttpInferenceProvider};
use crate::routing::PassiveCircuit;
use crate::usage::{OperationPrice, UsageLedger};
use chrono::Utc;
use model_provider::conformance::{CapabilityRequirements, FixtureProvenance};
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
            self.probe
                .secret_resolutions
                .fetch_add(1, Ordering::Relaxed);
            let secret = self.resolver.resolve(&provider.secret_ref)?;
            let capabilities = capabilities_for_provider(config, id);
            let material_digest = provider_digest(provider, &secret);
            let reusable_client = previous
                .and_then(|old| {
                    old.deployments.values().find(|deployment| {
                        deployment.account.provider_account_id == *id
                            && deployment.provider_digest == material_digest
                    })
                })
                .map(|deployment| deployment.provider.clone());
            let client = match reusable_client {
                Some(client) => client,
                None => {
                    self.probe.client_builds.fetch_add(1, Ordering::Relaxed);
                    match provider.provider_protocol.operation() {
                        Operation::Generate => {
                            CompiledProvider::Generation(Arc::new(HttpInferenceProvider::build(
                                provider,
                                &secret,
                                capabilities.generation.unwrap_or_default(),
                                timeout,
                                config.development_fixtures,
                            )?))
                        }
                        Operation::Embed => {
                            CompiledProvider::Embedding(Arc::new(HttpEmbeddingProvider::build(
                                provider,
                                &secret,
                                capabilities.embedding.unwrap_or_default(),
                                timeout,
                                config.development_fixtures,
                            )?))
                        }
                    }
                }
            };
            providers.insert(id.clone(), (client, material_digest));
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
                        provider_account_id: id.clone(),
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
            let (provider, provider_digest) = &providers[&deployment.provider];
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
                })
                .cloned();
            let runtime = reusable.unwrap_or_else(|| {
                let retained_state = previous_deployment.filter(|old| {
                    old.model == deployment.model
                        && old.configured_concurrency == deployment.concurrency
                        && old.capabilities == capabilities
                        && old.conformance_result == deployment.conformance_result
                        && old.required_conformance_provenance == required_conformance_provenance
                        && old.provider_digest == *provider_digest
                        && old.account.quota_group_id == quota
                });
                Arc::new(DeploymentRuntime {
                    id: id.clone(),
                    model: deployment.model.clone(),
                    configured_concurrency: deployment.concurrency,
                    provider: provider.clone(),
                    provider_digest: provider_digest.clone(),
                    capabilities,
                    conformance_result: deployment.conformance_result.clone(),
                    required_conformance_provenance,
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
            aliases,
            deployments,
            principal_permits,
        })
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
        let production_reference = provider.secret_ref.starts_with("env:")
            || provider.secret_ref.starts_with("credential://");
        if !config.development_fixtures
            && (url.scheme() != "https" || local || !production_reference)
        {
            return Err(LlmGatewayError::Config(format!(
                "provider `{id}` must use HTTPS, a non-loopback host, and an approved credential reference outside development fixtures"
            )));
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
                None => {
                    if alias.pii.enabled
                        && candidate.pii_placeholder_preservation_percent
                            < alias.pii.minimum_placeholder_preservation_percent
                    {
                        return Err(LlmGatewayError::Config(format!(
                            "alias `{name}` requires PII placeholder preservation not proven by development deployment `{deployment}`"
                        )));
                    }
                }
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
    }
    Ok(())
}

fn compile_embedding_memory_bounds(
    config: &LlmRouterConfig,
) -> Result<EmbeddingMemoryBounds, LlmGatewayError> {
    let limits = &config.embedding_memory;
    let checked_add = |left: usize, right: usize, label: &str| {
        left.checked_add(right)
            .ok_or_else(|| LlmGatewayError::Config(format!("embedding {label} overflows usize")))
    };
    let checked_mul = |left: usize, right: usize, label: &str| {
        left.checked_mul(right)
            .ok_or_else(|| LlmGatewayError::Config(format!("embedding {label} overflows usize")))
    };

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
        }
    }
    result
}

fn capabilities_for_deployment(config: &crate::config::DeploymentConfig) -> ProviderCapabilities {
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

fn provider_digest(config: &crate::config::ProviderConfig, secret: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(serde_json::to_vec(config).unwrap_or_default());
    digest.update([0]);
    digest.update(secret.as_bytes());
    format!("{:x}", digest.finalize())
}
