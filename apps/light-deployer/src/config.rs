use crate::model::DeploymentAction;
use config_loader::ConfigLoader;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeployerConfig {
    #[serde(default = "default_deployer_id")]
    pub deployer_id: String,
    #[serde(default)]
    pub host_id: Option<String>,
    #[serde(default = "default_cluster_id")]
    pub cluster_id: String,
    #[serde(default)]
    pub allowed_namespaces: BTreeSet<String>,
    #[serde(default)]
    pub allowed_repo_hosts: BTreeSet<String>,
    #[serde(default)]
    pub allowed_repo_prefixes: Vec<String>,
    #[serde(default)]
    pub allowed_image_registries: BTreeSet<String>,
    #[serde(default = "default_allowed_actions")]
    pub allowed_actions: BTreeSet<DeploymentAction>,
    #[serde(default = "default_allowed_kinds")]
    pub allowed_kinds: BTreeSet<String>,
    #[serde(default = "default_blocked_kinds")]
    pub blocked_kinds: BTreeSet<String>,
    #[serde(default)]
    pub prune: PruneConfig,
    #[serde(default)]
    pub dev_insecure: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PruneConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_max_delete_percent")]
    pub max_delete_percent: u8,
    #[serde(default = "default_sensitive_kinds")]
    pub sensitive_kinds: BTreeSet<String>,
    #[serde(default = "default_true")]
    pub override_required: bool,
}

impl Default for PruneConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_delete_percent: default_max_delete_percent(),
            sensitive_kinds: default_sensitive_kinds(),
            override_required: true,
        }
    }
}

impl DeployerConfig {
    pub fn load_from_dir(config_dir: impl AsRef<Path>) -> anyhow::Result<Self> {
        let config_dir = config_dir.as_ref();
        let path = std::env::var("LIGHT_DEPLOYER_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|_| config_dir.join("deployer.yml"));
        if path.exists() {
            let password = std::env::var("light_4j_config_password")
                .ok()
                .filter(|value| !value.trim().is_empty());
            let values = load_values(config_dir)?;
            let loader = ConfigLoader::from_values(values, password.as_deref(), None)?;
            let value = loader.load_merged_files([&path])?;
            let mut config: Self = serde_yaml::from_value(value)?;
            config.apply_env_overrides();
            Ok(config)
        } else {
            let mut config = Self::default();
            config.apply_env_overrides();
            Ok(config)
        }
    }

    fn apply_env_overrides(&mut self) {
        if let Ok(deployer_id) = std::env::var("LIGHT_DEPLOYER_ID") {
            self.deployer_id = deployer_id;
        }
        if let Ok(host_id) = std::env::var("LIGHT_DEPLOYER_HOST_ID") {
            self.host_id = (!host_id.trim().is_empty()).then_some(host_id);
        }
        if let Ok(cluster_id) = std::env::var("LIGHT_DEPLOYER_CLUSTER_ID") {
            self.cluster_id = cluster_id;
        }
        if let Ok(dev_insecure) = std::env::var("LIGHT_DEPLOYER_DEV_INSECURE") {
            self.dev_insecure = matches!(
                dev_insecure.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            );
        }
    }
}

fn load_values(config_dir: &Path) -> anyhow::Result<HashMap<String, serde_yaml::Value>> {
    let values_path = config_dir.join("values.yml");
    if !values_path.exists() {
        return Ok(HashMap::new());
    }

    let content = std::fs::read_to_string(values_path)?;
    let values = serde_yaml::from_str(&content)?;
    Ok(values)
}

impl Default for DeployerConfig {
    fn default() -> Self {
        Self {
            deployer_id: default_deployer_id(),
            host_id: None,
            cluster_id: default_cluster_id(),
            allowed_namespaces: BTreeSet::new(),
            allowed_repo_hosts: BTreeSet::new(),
            allowed_repo_prefixes: Vec::new(),
            allowed_image_registries: BTreeSet::new(),
            allowed_actions: default_allowed_actions(),
            allowed_kinds: default_allowed_kinds(),
            blocked_kinds: default_blocked_kinds(),
            prune: PruneConfig::default(),
            dev_insecure: false,
        }
    }
}

fn default_deployer_id() -> String {
    "local-light-deployer".to_string()
}

fn default_cluster_id() -> String {
    "local".to_string()
}

fn default_true() -> bool {
    true
}

fn default_max_delete_percent() -> u8 {
    30
}

fn default_allowed_actions() -> BTreeSet<DeploymentAction> {
    [
        DeploymentAction::Render,
        DeploymentAction::DryRun,
        DeploymentAction::Diff,
        DeploymentAction::Deploy,
        DeploymentAction::Undeploy,
        DeploymentAction::Status,
        DeploymentAction::Rollback,
    ]
    .into_iter()
    .collect()
}

fn default_allowed_kinds() -> BTreeSet<String> {
    ["Deployment", "Service", "Ingress", "ConfigMap", "Secret"]
        .into_iter()
        .map(str::to_string)
        .collect()
}

fn default_blocked_kinds() -> BTreeSet<String> {
    [
        "Namespace",
        "ClusterRole",
        "ClusterRoleBinding",
        "CustomResourceDefinition",
        "MutatingWebhookConfiguration",
        "ValidatingWebhookConfiguration",
    ]
    .into_iter()
    .map(str::to_string)
    .collect()
}

fn default_sensitive_kinds() -> BTreeSet<String> {
    ["PersistentVolumeClaim"]
        .into_iter()
        .map(str::to_string)
        .collect()
}
