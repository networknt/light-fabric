use super::snapshot::{
    AliasPlan, DeploymentRuntime, LlmPublishedSnapshot, PrincipalPermitStripes,
    ProviderAccountRuntime,
};
use crate::config::LlmRouterConfig;
use crate::credentials::SecretResolver;
use crate::error::LlmGatewayError;
use crate::pii::validate_pii_promotion;
use crate::provider::HttpInferenceProvider;
use crate::routing::PassiveCircuit;
use crate::usage::{Price, UsageLedger};
use chrono::Utc;
use model_provider::conformance::{CapabilityRequirements, FixtureProvenance};
use model_provider::inference::{ContentCapabilities, Operation, ProviderCapabilities};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::Semaphore;

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
        warn_on_mixed_format_extension_narrowing(config);
        let encoded = serde_json::to_vec(config)
            .map_err(|error| LlmGatewayError::Config(error.to_string()))?;
        let digest = format!("{:x}", Sha256::digest(encoded));
        let timeout = Duration::from_millis(config.request_timeout_ms);
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
                .map(|deployment| Arc::clone(&deployment.provider));
            let client = match reusable_client {
                Some(client) => client,
                None => {
                    self.probe.client_builds.fetch_add(1, Ordering::Relaxed);
                    Arc::new(HttpInferenceProvider::build(
                        provider,
                        &secret,
                        capabilities,
                        timeout,
                        config.development_fixtures,
                    )?) as Arc<dyn model_provider::inference::InferenceProvider>
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
            });
            accounts.entry(quota.clone()).or_insert_with(|| {
                previous_account.unwrap_or_else(|| {
                    Arc::new(ProviderAccountRuntime {
                        provider_account_id: id.clone(),
                        quota_group_id: quota,
                    })
                })
            });
        }
        let mut deployments = BTreeMap::new();
        let required_conformance_provenance = (!config.development_fixtures)
            .then_some(config.production_projection.required_conformance_provenance);
        for (id, deployment) in &config.deployments {
            let provider_config = &config.providers[&deployment.provider];
            let quota = provider_config
                .quota_group_id
                .clone()
                .unwrap_or_else(|| deployment.provider.clone());
            let capabilities = capabilities_for_deployment(deployment);
            let price = Price {
                version: price_version(deployment),
                input_micros_per_million: deployment.input_micros_per_million.ok_or_else(|| {
                    LlmGatewayError::Config(format!("deployment `{id}` has unknown input price"))
                })?,
                output_micros_per_million: deployment.output_micros_per_million.ok_or_else(
                    || {
                        LlmGatewayError::Config(format!(
                            "deployment `{id}` has unknown output price"
                        ))
                    },
                )?,
            };
            let (provider, provider_digest) = &providers[&deployment.provider];
            let previous_deployment = previous.and_then(|old| old.deployments.get(id));
            let reusable = previous_deployment
                .filter(|old| {
                    old.model == deployment.model
                        && old.configured_concurrency == deployment.concurrency
                        && old.capabilities == capabilities
                        && old.conformance_result == deployment.conformance_result
                        && old.required_conformance_provenance == required_conformance_provenance
                        && old.price == price
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
                    provider: Arc::clone(provider),
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
                    price,
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
            stream_write_timeout_ms: config.stream_write_timeout_ms,
            stream_setup_timeout_ms: config.stream_setup_timeout_ms,
            stream_idle_timeout_ms: config.stream_idle_timeout_ms,
            stream_minimum_drain_bytes_per_second: config.stream_minimum_drain_bytes_per_second,
            stream_drain_grace_ms: config.stream_drain_grace_ms,
            max_replay_bytes: config.max_replay_bytes,
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
            .map(|provider| provider.format)
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
    let required_provenance = (!config.development_fixtures)
        .then_some(config.production_projection.required_conformance_provenance);
    if config.path_prefix != "/v1"
        || config.global_concurrency == 0
        || config.global_stream_concurrency == 0
        || config.stream_channel_capacity == 0
        || config.stream_write_timeout_ms == 0
        || config.stream_setup_timeout_ms == 0
        || config.stream_idle_timeout_ms == 0
        || config.stream_minimum_drain_bytes_per_second == 0
        || config.stream_drain_grace_ms == 0
        || config.max_replay_bytes == 0
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
            || alias.max_attempts == 0
            || alias.max_attempts > alias.deployments.len()
        {
            return Err(LlmGatewayError::Config(format!("invalid alias `{name}`")));
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
            let requirements = alias_requirements(alias, required_provenance);
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
                None if config.development_fixtures => {
                    if alias.pii.enabled
                        && candidate.pii_placeholder_preservation_percent
                            < alias.pii.minimum_placeholder_preservation_percent
                    {
                        return Err(LlmGatewayError::Config(format!(
                            "alias `{name}` requires PII placeholder preservation not proven by development deployment `{deployment}`"
                        )));
                    }
                }
                None => {
                    return Err(LlmGatewayError::Config(format!(
                        "deployment `{deployment}` has no conformance result"
                    )));
                }
            }
        }
    }
    for (id, deployment) in &config.deployments {
        if !config.providers.contains_key(&deployment.provider)
            || deployment.concurrency == 0
            || deployment.conformance_digest.len() != 64
            || !deployment
                .conformance_digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || deployment.pii_placeholder_preservation_percent > 100
        {
            return Err(LlmGatewayError::Config(format!(
                "invalid deployment `{id}`"
            )));
        }
        if let Some(result) = &deployment.conformance_result {
            let provider = &config.providers[&deployment.provider];
            if !result.verify_digest()
                || result.digest != deployment.conformance_digest
                || result.provider != provider.format
                || result.physical_model != deployment.model
                || !result.is_current_and_passing(now)
            {
                return Err(LlmGatewayError::Config(format!(
                    "deployment `{id}` has invalid, mismatched, or expired conformance evidence"
                )));
            }
        } else if !config.development_fixtures {
            return Err(LlmGatewayError::Config(format!(
                "deployment `{id}` requires complete conformance evidence"
            )));
        }
    }
    Ok(())
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
        result.content.text |= current.content.text;
        result.content.images |= current.content.images;
        result.content.tools |= current.content.tools;
        result.content.parallel_tools |= current.content.parallel_tools;
        result.content.structured_json |= current.content.structured_json;
        result.content.reasoning_usage |= current.content.reasoning_usage;
        result.streaming |= current.streaming;
    }
    result
}

fn capabilities_for_deployment(config: &crate::config::DeploymentConfig) -> ProviderCapabilities {
    if let Some(result) = &config.conformance_result {
        return result.capabilities.clone();
    }
    ProviderCapabilities {
        operations: BTreeSet::from([Operation::ChatCompletions]),
        content: ContentCapabilities {
            text: config.text,
            images: config.images,
            tools: config.tools,
            parallel_tools: config.tools,
            structured_json: config.structured_json,
            reasoning_usage: false,
        },
        streaming: config.streaming,
    }
}

fn alias_requirements(
    alias: &crate::config::AliasConfig,
    required_provenance: Option<FixtureProvenance>,
) -> CapabilityRequirements {
    CapabilityRequirements {
        operation: Operation::ChatCompletions,
        images: alias.required_capabilities.images,
        tools: alias.required_capabilities.tools,
        parallel_tools: alias.required_capabilities.parallel_tools,
        structured_json: alias.required_capabilities.structured_json,
        streaming: alias.required_capabilities.streaming,
        required_provenance,
    }
}

fn price_version(config: &crate::config::DeploymentConfig) -> u64 {
    let encoded = serde_json::to_vec(config).unwrap_or_default();
    let digest = Sha256::digest(encoded);
    u64::from_be_bytes(digest[..8].try_into().expect("SHA-256 prefix"))
}

fn provider_digest(config: &crate::config::ProviderConfig, secret: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(serde_json::to_vec(config).unwrap_or_default());
    digest.update([0]);
    digest.update(secret.as_bytes());
    format!("{:x}", digest.finalize())
}
