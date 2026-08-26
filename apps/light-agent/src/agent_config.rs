use agent_core::{PolicySnapshot, sha256_digest};
use agent_runtime_protocol::canonical_digest;
use chrono::{DateTime, Utc};
use knowledge_core::RetrievalFilters;
use serde::de::{DeserializeOwned, Error as DeError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub const AGENT_CONFIG_FILE: &str = "agent.yml";
pub const AGENT_CONFIG_MODULE_ID: &str = "light-agent/agent";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentConfig {
    pub runtime_policy: RuntimePolicyEnvelope,
    pub agent_policy: AgentPolicy,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuntimePolicyEnvelope {
    pub publication_id: Uuid,
    pub release_version: u64,
    pub policy_snapshot_id: Uuid,
    pub policy_version: u64,
    pub policy_digest: String,
    pub content_digest: String,
    pub audience: String,
    pub host_id: Uuid,
    pub environment: String,
    pub service_id: String,
    pub instance_id: Uuid,
    pub source_event_sequence: i64,
    pub schema_version: u32,
    pub created_at: DateTime<Utc>,
    pub valid_from: DateTime<Utc>,
    pub refresh_after: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub revocation_epoch: u64,
    pub compatibility_generation: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentPolicy {
    pub agent_def_id: Uuid,
    pub definition_version: i64,
    pub prompt: AgentPromptPolicy,
    pub model: AgentModelPolicy,
    #[serde(default)]
    pub skills: Vec<AgentSkillPolicy>,
    pub policy_snapshot: PolicySnapshot,
    pub execution: AgentExecutionPolicy,
    pub catalog: AgentCatalogPolicy,
    pub memory: AgentMemoryPolicy,
    pub knowledge: AgentKnowledgePolicy,
    pub channel: Value,
    pub data_boundary: Value,
    pub gateway_delegation: Value,
    pub session: AgentSessionPolicy,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentPromptPolicy {
    pub system: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentSkillPolicy {
    pub skill_id: Uuid,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub content_markdown: String,
    pub version: String,
    pub aggregate_version: i64,
    pub digest: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentModelPolicy {
    pub provider: String,
    pub alias: String,
    pub temperature: f64,
    pub maximum_tokens: u64,
    pub gateway: LlmGatewayClientPolicy,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LlmGatewayClientPolicy {
    pub name: String,
    pub base_url: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentExecutionPolicy {
    pub maximum_turn_seconds: u64,
    pub maximum_model_calls: usize,
    pub maximum_action_calls: usize,
    pub maximum_user_message_bytes: usize,
    pub maximum_tool_argument_bytes: usize,
    pub maximum_tool_output_bytes: usize,
    pub maximum_gateway_response_bytes: usize,
    pub maximum_response_bytes: usize,
    pub maximum_output_depth: usize,
    pub maximum_output_items: usize,
    pub maximum_turn_tokens: u64,
    #[serde(default)]
    pub quota_policies: Vec<Value>,
    #[serde(default)]
    pub model_rates: Vec<Value>,
    #[serde(default)]
    pub service_pools: Vec<Value>,
    #[serde(default)]
    pub approval_rules: Vec<Value>,
    #[serde(default, deserialize_with = "deserialize_optional_non_empty_map")]
    pub coding_profile: Option<CodingProfilePolicy>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodingProfilePolicy {
    pub product_profile_digest: String,
    pub repository_uri_prefix: String,
    pub compatibility_digest: String,
    pub template_digest: String,
    pub binary_digest: String,
    pub provider: String,
    pub model: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentCatalogPolicy {
    pub cache_ttl_seconds: u64,
    pub stale_on_error_seconds: u64,
    #[serde(default)]
    pub effective_catalog: Option<Value>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentMemoryPolicy {
    pub write_mode: String,
    #[serde(default)]
    pub portal_command_url: Option<String>,
    pub allow_direct_pg: bool,
    #[serde(default)]
    pub personal_profile_digest: Option<String>,
    #[serde(default)]
    pub rules: Value,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentKnowledgePolicy {
    #[serde(default)]
    pub endpoint: Option<String>,
    pub allow_private_plaintext: bool,
    #[serde(default)]
    pub bindings: Vec<Value>,
    pub retrieval: AgentKnowledgeRetrievalPolicy,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentKnowledgeRetrievalPolicy {
    pub top_k: usize,
    pub token_budget: usize,
    #[serde(default, deserialize_with = "deserialize_optional_non_empty_map")]
    pub filters: Option<RetrievalFilters>,
}

fn deserialize_optional_non_empty_map<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: DeserializeOwned,
{
    let value = Value::deserialize(deserializer)?;
    let value = match value {
        Value::Null => return Ok(None),
        Value::String(value) => {
            let value = value.trim();
            if value.is_empty() {
                return Ok(None);
            }
            serde_yaml::from_str::<Value>(value).map_err(D::Error::custom)?
        }
        value => value,
    };

    match &value {
        Value::Null => Ok(None),
        Value::Object(entries) if entries.is_empty() => Ok(None),
        Value::Object(_) => serde_json::from_value(value)
            .map(Some)
            .map_err(D::Error::custom),
        _ => Err(D::Error::custom(
            "expected null, an empty value, or a JSON/YAML map",
        )),
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentSessionPolicy {
    pub idle_seconds: u64,
    pub maximum_seconds: u64,
    pub maximum_active_sessions: u64,
    pub maximum_queued_turns: u64,
}

impl AgentConfig {
    pub fn validate(
        &self,
        service_id: &str,
        environment: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<(), String> {
        let envelope = &self.runtime_policy;
        let policy = &self.agent_policy;
        if envelope.schema_version != 1 {
            return Err(format!(
                "unsupported Agent policy schema version {}",
                envelope.schema_version
            ));
        }
        if envelope.audience != "agent" {
            return Err("runtimePolicy.audience must be agent".to_string());
        }
        if envelope.service_id != service_id {
            return Err("runtimePolicy.serviceId does not match the running service".to_string());
        }
        if let Some(environment) = environment {
            if !environment.is_empty() && envelope.environment != environment {
                return Err(
                    "runtimePolicy.environment does not match the running service".to_string(),
                );
            }
        }
        if envelope.policy_snapshot_id != policy.policy_snapshot.snapshot_id {
            return Err(
                "runtimePolicy.policySnapshotId does not match agentPolicy.policySnapshot"
                    .to_string(),
            );
        }
        if now < envelope.valid_from {
            return Err("Agent policy is not valid yet".to_string());
        }
        if now >= envelope.expires_at {
            return Err("Agent policy has expired".to_string());
        }
        if envelope.created_at > envelope.valid_from
            || envelope.valid_from >= envelope.refresh_after
            || envelope.refresh_after >= envelope.expires_at
        {
            return Err("Agent policy validity window is invalid".to_string());
        }
        if policy.definition_version <= 0 {
            return Err("agentPolicy.definitionVersion must be positive".to_string());
        }
        if policy.prompt.system.trim().is_empty() {
            return Err("agentPolicy.prompt.system is required".to_string());
        }
        for skill in &policy.skills {
            if skill.name.trim().is_empty()
                || skill.content_markdown.trim().is_empty()
                || skill.version.trim().is_empty()
                || skill.aggregate_version <= 0
                || !skill.digest.starts_with("sha256:")
            {
                return Err("agentPolicy.skills contains an invalid skill projection".to_string());
            }
        }
        if policy.model.provider != "gateway" {
            return Err("agentPolicy.model.provider must be gateway".to_string());
        }
        if policy.model.alias.trim().is_empty() {
            return Err("agentPolicy.model.alias is required".to_string());
        }
        if policy.model.gateway.base_url.trim().is_empty() {
            return Err("agentPolicy.model.gateway.baseUrl is required".to_string());
        }
        if !policy.model.temperature.is_finite() || !(0.0..=2.0).contains(&policy.model.temperature)
        {
            return Err("agentPolicy.model.temperature must be between 0 and 2".to_string());
        }
        if policy.model.maximum_tokens == 0 || policy.model.maximum_tokens > u64::from(u32::MAX) {
            return Err(
                "agentPolicy.model.maximumTokens must be between 1 and 4294967295".to_string(),
            );
        }
        if policy.model.maximum_tokens > policy.execution.maximum_turn_tokens {
            return Err(
                "agentPolicy.model.maximumTokens cannot exceed execution.maximumTurnTokens"
                    .to_string(),
            );
        }
        if policy.session.idle_seconds == 0
            || policy.session.maximum_seconds == 0
            || policy.session.idle_seconds > policy.session.maximum_seconds
            || policy.session.maximum_active_sessions == 0
            || policy.session.maximum_queued_turns == 0
            || policy.session.idle_seconds > i64::MAX as u64
            || policy.session.maximum_seconds > i64::MAX as u64
        {
            return Err("agentPolicy.session limits are invalid".to_string());
        }
        if policy.knowledge.retrieval.top_k == 0
            || policy.knowledge.retrieval.top_k > 100
            || policy.knowledge.retrieval.token_budget == 0
        {
            return Err("agentPolicy.knowledge.retrieval limits are invalid".to_string());
        }
        validate_digest(
            "runtimePolicy.policyDigest",
            &envelope.policy_digest,
            &persisted_policy_digest(&policy.policy_snapshot)
                .map_err(|error| format!("failed to digest Agent policy: {error}"))?,
        )?;
        validate_digest(
            "runtimePolicy.contentDigest",
            &envelope.content_digest,
            &canonical_digest(policy)
                .map_err(|error| format!("failed to digest Agent projection: {error}"))?,
        )?;
        Ok(())
    }

    pub fn compiled_system_prompt(&self) -> String {
        let mut prompt = self.agent_policy.prompt.system.trim().to_string();
        for skill in &self.agent_policy.skills {
            prompt.push_str("\n\n## Skill: ");
            prompt.push_str(skill.name.trim());
            if let Some(description) = skill.description.as_deref() {
                let description = description.trim();
                if !description.is_empty() {
                    prompt.push_str("\n");
                    prompt.push_str(description);
                }
            }
            prompt.push_str("\n\n");
            prompt.push_str(skill.content_markdown.trim());
        }
        prompt
    }
}

fn persisted_policy_digest(policy: &PolicySnapshot) -> Result<String, serde_json::Error> {
    // Keep the deployed Portal/Agent digest contract until a coordinated
    // canonical-digest migration and database backfill are available.
    Ok(sha256_digest(&serde_json::to_vec(policy)?))
}

fn validate_digest(name: &str, configured: &str, actual: &str) -> Result<(), String> {
    if configured != actual {
        return Err(format!("{name} failed digest verification"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::sha256_digest;
    use chrono::Duration;
    use std::collections::BTreeMap;

    fn digest(value: &str) -> String {
        sha256_digest(value.as_bytes())
    }

    fn config(now: DateTime<Utc>) -> AgentConfig {
        let snapshot_id = Uuid::now_v7();
        let policy_snapshot = PolicySnapshot {
            snapshot_id,
            definition_digest: digest("definition"),
            product_profile_digest: digest("profile"),
            model_digest: digest("model"),
            catalog_digest: digest("catalog"),
            memory_digest: digest("memory"),
            execution_digest: digest("execution"),
            channel_digest: digest("channel"),
            data_boundary_digest: digest("boundary"),
            tools: BTreeMap::new(),
        };
        let agent_policy = AgentPolicy {
            agent_def_id: Uuid::now_v7(),
            definition_version: 3,
            prompt: AgentPromptPolicy {
                system: "help safely".into(),
            },
            model: AgentModelPolicy {
                provider: "gateway".into(),
                alias: "support-chat".into(),
                temperature: 0.3,
                maximum_tokens: 4096,
                gateway: LlmGatewayClientPolicy {
                    name: "llm-gateway".into(),
                    base_url: "https://llm-gateway:8443/v1".into(),
                },
            },
            skills: vec![AgentSkillPolicy {
                skill_id: Uuid::now_v7(),
                name: "Support".into(),
                description: Some("Answer product questions".into()),
                content_markdown: "Use only verified product facts.".into(),
                version: "1.0.0".into(),
                aggregate_version: 1,
                digest: digest("support-skill"),
            }],
            policy_snapshot,
            execution: AgentExecutionPolicy {
                maximum_turn_seconds: 120,
                maximum_model_calls: 10,
                maximum_action_calls: 20,
                maximum_user_message_bytes: 65_536,
                maximum_tool_argument_bytes: 65_536,
                maximum_tool_output_bytes: 65_536,
                maximum_gateway_response_bytes: 1_048_576,
                maximum_response_bytes: 65_536,
                maximum_output_depth: 16,
                maximum_output_items: 1024,
                maximum_turn_tokens: 65_536,
                quota_policies: vec![],
                model_rates: vec![],
                service_pools: vec![],
                approval_rules: vec![],
                coding_profile: None,
            },
            catalog: AgentCatalogPolicy {
                cache_ttl_seconds: 60,
                stale_on_error_seconds: 300,
                effective_catalog: None,
            },
            memory: AgentMemoryPolicy {
                write_mode: "portal-command".into(),
                portal_command_url: None,
                allow_direct_pg: false,
                personal_profile_digest: None,
                rules: serde_json::json!({}),
            },
            knowledge: AgentKnowledgePolicy {
                endpoint: None,
                allow_private_plaintext: false,
                bindings: vec![],
                retrieval: AgentKnowledgeRetrievalPolicy {
                    top_k: 5,
                    token_budget: 2_000,
                    filters: None,
                },
            },
            channel: serde_json::json!({}),
            data_boundary: serde_json::json!({}),
            gateway_delegation: serde_json::json!({}),
            session: AgentSessionPolicy {
                idle_seconds: 3600,
                maximum_seconds: 86_400,
                maximum_active_sessions: 10,
                maximum_queued_turns: 100,
            },
        };
        let policy_digest = persisted_policy_digest(&agent_policy.policy_snapshot).unwrap();
        let content_digest = canonical_digest(&agent_policy).unwrap();
        AgentConfig {
            runtime_policy: RuntimePolicyEnvelope {
                publication_id: Uuid::now_v7(),
                release_version: 4,
                policy_snapshot_id: snapshot_id,
                policy_version: 3,
                policy_digest,
                content_digest,
                audience: "agent".into(),
                host_id: Uuid::now_v7(),
                environment: "dev".into(),
                service_id: "com.networknt.agent.support-1.0.0".into(),
                instance_id: Uuid::now_v7(),
                source_event_sequence: 42,
                schema_version: 1,
                created_at: now - Duration::minutes(1),
                valid_from: now - Duration::seconds(30),
                refresh_after: now + Duration::minutes(5),
                expires_at: now + Duration::minutes(15),
                revocation_epoch: 1,
                compatibility_generation: 1,
            },
            agent_policy,
        }
    }

    #[test]
    fn validates_bound_immutable_agent_projection() {
        let now = Utc::now();
        let config = config(now);
        assert!(
            config
                .validate("com.networknt.agent.support-1.0.0", Some("dev"), now)
                .is_ok()
        );
        assert_eq!(
            config.compiled_system_prompt(),
            "help safely\n\n## Skill: Support\nAnswer product questions\n\nUse only verified product facts."
        );
    }

    #[test]
    fn rejects_direct_provider_and_tampered_content() {
        let now = Utc::now();
        let mut direct = config(now);
        direct.agent_policy.model.provider = "openai".into();
        assert!(
            direct
                .validate("com.networknt.agent.support-1.0.0", Some("dev"), now)
                .unwrap_err()
                .contains("must be gateway")
        );

        let mut tampered = config(now);
        tampered.agent_policy.model.alias = "unpublished-alias".into();
        assert!(
            tampered
                .validate("com.networknt.agent.support-1.0.0", Some("dev"), now)
                .unwrap_err()
                .contains("contentDigest")
        );
    }

    #[test]
    fn rejects_unbounded_session_model_and_retrieval_limits() {
        let now = Utc::now();

        let mut session = config(now);
        session.agent_policy.session.maximum_seconds = i64::MAX as u64 + 1;
        assert!(
            session
                .validate("com.networknt.agent.support-1.0.0", Some("dev"), now)
                .unwrap_err()
                .contains("session limits")
        );

        let mut model = config(now);
        model.agent_policy.model.maximum_tokens = u64::from(u32::MAX) + 1;
        assert!(
            model
                .validate("com.networknt.agent.support-1.0.0", Some("dev"), now)
                .unwrap_err()
                .contains("maximumTokens")
        );

        let mut retrieval = config(now);
        retrieval.agent_policy.knowledge.retrieval.top_k = 0;
        assert!(
            retrieval
                .validate("com.networknt.agent.support-1.0.0", Some("dev"), now)
                .unwrap_err()
                .contains("retrieval limits")
        );
    }

    #[test]
    fn optional_structured_policy_maps_accept_disabled_and_populated_forms() {
        #[derive(Deserialize)]
        struct CodingProfileValue {
            #[serde(default, deserialize_with = "deserialize_optional_non_empty_map")]
            value: Option<CodingProfilePolicy>,
        }

        #[derive(Deserialize, Serialize)]
        struct CatalogValue {
            #[serde(default)]
            value: Option<Value>,
        }

        #[derive(Deserialize)]
        struct FiltersValue {
            #[serde(default, deserialize_with = "deserialize_optional_non_empty_map")]
            value: Option<RetrievalFilters>,
        }

        for yaml in ["{}", "value: null", "value: ''", "value: {}"] {
            let parsed: CodingProfileValue = serde_yaml::from_str(yaml).unwrap();
            assert!(parsed.value.is_none(), "{yaml}");
        }

        let coding: CodingProfileValue = serde_yaml::from_str(
            r#"
value:
  productProfileDigest: sha256:profile
  repositoryUriPrefix: file:///var/lib/light-agent/repositories/
  compatibilityDigest: sha256:compatibility
  templateDigest: sha256:template
  binaryDigest: sha256:binary
  provider: gateway
  model: coding-model
"#,
        )
        .unwrap();
        let coding = coding.value.unwrap();
        assert_eq!(coding.provider, "gateway");
        assert_eq!(coding.model, "coding-model");

        let missing_catalog: CatalogValue = serde_yaml::from_str("{}").unwrap();
        assert!(missing_catalog.value.is_none());
        let catalog: CatalogValue = serde_yaml::from_str("value: {}").unwrap();
        assert_eq!(catalog.value, Some(serde_json::json!({})));
        assert_eq!(
            serde_json::to_value(catalog).unwrap(),
            serde_json::json!({"value": {}})
        );

        let filters: FiltersValue =
            serde_yaml::from_str("value: {languages: [en], sourceIds: []}").unwrap();
        let filters = filters.value.unwrap();
        assert_eq!(filters.languages, ["en"]);
        assert!(filters.source_ids.is_empty());

        let quoted_filters: FiltersValue =
            serde_yaml::from_str("value: '{languages: [en], sourceIds: []}'").unwrap();
        assert_eq!(quoted_filters.value.unwrap().languages, ["en"]);

        let quoted_null: FiltersValue = serde_yaml::from_str("value: 'null'").unwrap();
        assert!(quoted_null.value.is_none());

        assert!(serde_yaml::from_str::<CodingProfileValue>("value: []").is_err());
        assert!(serde_yaml::from_str::<CodingProfileValue>("value: enabled").is_err());
        assert!(serde_yaml::from_str::<CodingProfileValue>("value: {provider: gateway}").is_err());
    }
}
