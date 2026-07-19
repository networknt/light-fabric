use super::snapshot::{
    AliasPlan, DeploymentRuntime, LlmPublishedSnapshot, PrincipalPermitStripes,
    ProviderAccountRuntime,
};
use crate::config::LlmRouterConfig;
use crate::credentials::SecretResolver;
use crate::error::LlmGatewayError;
use crate::provider::HttpInferenceProvider;
use crate::routing::PassiveCircuit;
use crate::usage::{Price, UsageLedger};
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
            let reusable = previous
                .and_then(|old| old.deployments.get(id))
                .filter(|old| {
                    old.model == deployment.model
                        && old.capabilities == capabilities
                        && old.price == price
                        && old.provider_digest == *provider_digest
                })
                .cloned();
            let runtime = reusable.unwrap_or_else(|| {
                Arc::new(DeploymentRuntime {
                    id: id.clone(),
                    model: deployment.model.clone(),
                    provider: Arc::clone(provider),
                    provider_digest: provider_digest.clone(),
                    capabilities,
                    permits: Arc::new(Semaphore::new(deployment.concurrency)),
                    circuit: Arc::new(PassiveCircuit::new(3, Duration::from_secs(30))),
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
                    .collect();
                (
                    name.clone(),
                    Arc::new(AliasPlan {
                        public_name: name.clone(),
                        deployments: plans,
                        max_attempts: alias.max_attempts,
                        permits: Arc::new(Semaphore::new(alias.concurrency)),
                        max_input_tokens: alias.max_input_tokens,
                        max_output_tokens: alias.max_output_tokens,
                        max_cost_micros: alias.max_cost_micros,
                        internal: alias.internal,
                        bound_principal: alias.bound_principal.clone(),
                        audit: alias.audit,
                        ledger: Arc::new(UsageLedger::default()),
                    }),
                )
            })
            .collect();
        Ok(LlmPublishedSnapshot {
            generation,
            digest,
            global_concurrency: config.global_concurrency,
            max_replay_bytes: config.max_replay_bytes,
            aliases,
            deployments,
            principal_permits: PrincipalPermitStripes::new(64, 16),
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
    if config.path_prefix != "/v1" || config.global_concurrency == 0 || config.max_replay_bytes == 0
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
        if !config.development_fixtures
            && (url.scheme() != "https" || local || !provider.secret_ref.starts_with("env:"))
        {
            return Err(LlmGatewayError::Config(format!(
                "provider `{id}` must use HTTPS, a non-loopback host, and env:<NAME> credentials outside development fixtures"
            )));
        }
    }
    for (name, alias) in &config.aliases {
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
        for deployment in &alias.deployments {
            if !config.deployments.contains_key(deployment) {
                return Err(LlmGatewayError::Config(format!(
                    "alias `{name}` references missing deployment `{deployment}`"
                )));
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
        {
            return Err(LlmGatewayError::Config(format!(
                "invalid deployment `{id}`"
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
        result.content.structured_json |= current.content.structured_json;
    }
    result
}

fn capabilities_for_deployment(config: &crate::config::DeploymentConfig) -> ProviderCapabilities {
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
        streaming: false,
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
