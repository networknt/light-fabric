use serde::{Deserialize, Serialize};

pub const GATEWAY_PROVIDER_ID: &str = "gateway";

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EligibleModelsResponse {
    pub schema_version: String,
    pub agent_def_id: String,
    #[serde(default)]
    pub models: Vec<EligibleModel>,
    #[serde(default)]
    pub resolved_model: Option<String>,
    pub resolution_status: ResolutionStatus,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct EligibleModel {
    pub alias_name: String,
    pub selection_mode: SelectionMode,
    pub selected: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SelectionMode {
    DirectAlias,
    PolicyDefault,
    InternalLegacy,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ResolutionStatus {
    Resolved,
    NoDefault,
    AmbiguousDefault,
    UnapprovedLegacy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GovernedModelSelection {
    pub provider: &'static str,
    pub model_alias: String,
    pub mode: SelectionMode,
}

impl EligibleModelsResponse {
    pub fn resolve(&self) -> Result<GovernedModelSelection, GovernedModelError> {
        if self.schema_version != "1" {
            return Err(GovernedModelError::UnsupportedSchema);
        }
        if self.resolution_status != ResolutionStatus::Resolved {
            return Err(GovernedModelError::Unresolved(self.resolution_status));
        }
        let selected = self
            .models
            .iter()
            .filter(|model| model.selected)
            .collect::<Vec<_>>();
        if selected.len() != 1 {
            return Err(GovernedModelError::InvalidSelectionCardinality);
        }
        let model = selected[0];
        if model.alias_name.trim().is_empty()
            || self.resolved_model.as_deref() != Some(model.alias_name.as_str())
        {
            return Err(GovernedModelError::ResolvedAliasMismatch);
        }
        Ok(GovernedModelSelection {
            provider: GATEWAY_PROVIDER_ID,
            model_alias: model.alias_name.clone(),
            mode: model.selection_mode,
        })
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GovernedModelError {
    #[error("unsupported eligible-model schema")]
    UnsupportedSchema,
    #[error("eligible-model profile is unresolved: {0:?}")]
    Unresolved(ResolutionStatus),
    #[error("eligible-model profile must select exactly one alias")]
    InvalidSelectionCardinality,
    #[error("resolved alias does not match the selected model")]
    ResolvedAliasMismatch,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Fixture {
        production_authority: bool,
        cases: Vec<FixtureCase>,
    }

    #[derive(Deserialize)]
    struct FixtureCase {
        name: String,
        response: EligibleModelsResponse,
    }

    #[test]
    fn shared_portal_contract_drives_governed_alias_selection() {
        let fixture: Fixture = serde_json::from_slice(include_bytes!(
            "../../../benchmarks/llm-gateway/contracts/v1/get-eligible-llm-models-for-agent.fixture.json"
        ))
        .unwrap();
        assert!(!fixture.production_authority);
        let direct = fixture
            .cases
            .iter()
            .find(|case| case.name == "direct-public-alias")
            .unwrap()
            .response
            .resolve()
            .unwrap();
        assert_eq!(direct.provider, GATEWAY_PROVIDER_ID);
        assert_eq!(direct.model_alias, "support-chat");
        assert_eq!(direct.mode, SelectionMode::DirectAlias);
        let policy = fixture
            .cases
            .iter()
            .find(|case| case.name == "policy-only-agent-default")
            .unwrap()
            .response
            .resolve()
            .unwrap();
        assert_eq!(policy.model_alias, "policy-chat");
        let legacy = fixture
            .cases
            .iter()
            .find(|case| case.name == "approved-internal-legacy-alias")
            .unwrap()
            .response
            .resolve()
            .unwrap();
        assert_eq!(legacy.model_alias, "legacy-agent-internal");
        assert_eq!(legacy.mode, SelectionMode::InternalLegacy);
        for name in ["ambiguous-policy-default", "policy-without-default"] {
            assert!(
                fixture
                    .cases
                    .iter()
                    .find(|case| case.name == name)
                    .unwrap()
                    .response
                    .resolve()
                    .is_err()
            );
        }
    }
}
