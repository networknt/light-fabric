//! Published personal Codex policy. Workflow inputs never supply these permissions.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionSource {
    AgentPolicy,
    CodexCli,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    Managed,
    Inherit,
    TrustedPersonalUnattended,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Interaction {
    Unattended,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PersonalPolicy {
    pub schema_version: u16,
    pub permission_source: PermissionSource,
    pub permission_mode: PermissionMode,
    pub interaction: Interaction,
    /// Exact native catalog model identifiers; aliases and automatic upgrades are rejected.
    pub allowed_models: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
}
impl PersonalPolicy {
    pub fn validate(&self, requested: Option<&str>) -> Result<()> {
        ensure!(
            self.schema_version == 1,
            "unsupported Codex personal policy version"
        );
        ensure!(
            matches!(
                (self.permission_source, self.permission_mode),
                (PermissionSource::AgentPolicy, PermissionMode::Managed)
                    | (
                        PermissionSource::CodexCli,
                        PermissionMode::Inherit | PermissionMode::TrustedPersonalUnattended
                    )
            ),
            "conflicting Codex permission source and mode"
        );
        ensure!(
            !self.allowed_models.is_empty()
                && self.allowed_models.len() <= 32
                && self.allowed_models.iter().all(|s| identifier(s)),
            "invalid Codex allowed models"
        );
        for model in [self.default_model.as_deref(), requested]
            .into_iter()
            .flatten()
        {
            ensure!(
                self.allowed_models.contains(model),
                "Codex native model is not admitted"
            );
        }
        Ok(())
    }
    pub fn native_permissions(&self) -> bool {
        self.permission_source == PermissionSource::CodexCli
    }
}
pub fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_mode_and_model_are_fail_closed() {
        let mut p: PersonalPolicy = serde_json::from_value(serde_json::json!({
            "schemaVersion":1,"permissionSource":"codex-cli","permissionMode":"inherit",
            "interaction":"unattended","allowedModels":["test-model"]}))
        .unwrap();
        p.validate(None).unwrap();
        assert!(p.validate(Some("other")).is_err());
        p.permission_mode = PermissionMode::TrustedPersonalUnattended;
        p.validate(Some("test-model")).unwrap();
        p.permission_source = PermissionSource::AgentPolicy;
        assert!(p.validate(None).is_err());
        p.permission_mode = PermissionMode::Managed;
        p.validate(None).unwrap();
        p.schema_version = 2;
        assert!(p.validate(None).is_err());
    }
}
