use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fmt};
use thiserror::Error;
use uuid::Uuid;

macro_rules! opaque_uuid {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);
        impl $name {
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }
        }
        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

opaque_uuid!(AgentSessionId);
opaque_uuid!(AgentTurnId);
opaque_uuid!(AgentActionId);
opaque_uuid!(AgentActionAttemptId);
opaque_uuid!(AgentApprovalId);
opaque_uuid!(AgentEventId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SessionState {
    Active,
    Closing,
    Closed,
    Revoked,
    Expired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TurnState {
    Queued,
    Received,
    RunningModel,
    WaitingAction,
    RunningAction,
    WaitingReconciliation,
    WaitingApproval,
    Completed,
    Failed,
    Cancelled,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ActionState {
    Proposed,
    WaitingApproval,
    Ready,
    Dispatched,
    Running,
    ApprovalRequired,
    Succeeded,
    Failed,
    Cancelled,
    Unknown,
    OperatorRequired,
    Accepted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ApprovalState {
    Requested,
    Approved,
    Rejected,
    Expired,
    Revoked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolPlacement {
    Gateway,
    Runner,
    Workflow,
    Fixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultClass {
    Success,
    RecoverableFailure,
    TerminalFailure,
    ApprovalRequired,
    Cancelled,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PolicySnapshot {
    pub snapshot_id: Uuid,
    pub definition_digest: String,
    pub product_profile_digest: String,
    pub model_digest: String,
    pub catalog_digest: String,
    pub memory_digest: String,
    pub execution_digest: String,
    pub channel_digest: String,
    pub data_boundary_digest: String,
    pub tools: BTreeMap<String, ToolBinding>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolBinding {
    pub tool_ref: Uuid,
    pub model_alias: String,
    pub placement: ToolPlacement,
    pub schema_digest: String,
    pub dispatch_binding: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentEvent {
    pub event_id: AgentEventId,
    pub session_id: AgentSessionId,
    pub sequence: i64,
    pub turn_id: Option<AgentTurnId>,
    pub action_attempt_id: Option<AgentActionAttemptId>,
    pub event_type: String,
    pub content: Value,
    pub content_digest: String,
    pub policy_digest: String,
    pub occurred_at: DateTime<Utc>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CatalogError {
    #[error("duplicate model alias `{0}` maps to different tools")]
    AliasCollision(String),
    #[error("returned tool `{0}` was not disclosed")]
    UndisclosedTool(String),
    #[error("tool `{alias}` cannot be rerouted from {expected:?} to {actual:?}")]
    RouteSubstitution {
        alias: String,
        expected: ToolPlacement,
        actual: ToolPlacement,
    },
}

pub fn merge_disclosed_tools(
    sources: impl IntoIterator<Item = ToolBinding>,
) -> Result<BTreeMap<String, ToolBinding>, CatalogError> {
    let mut merged = BTreeMap::new();
    for binding in sources {
        if let Some(previous) = merged.get(&binding.model_alias) {
            if previous != &binding {
                return Err(CatalogError::AliasCollision(binding.model_alias));
            }
        } else {
            merged.insert(binding.model_alias.clone(), binding);
        }
    }
    Ok(merged)
}

pub fn validate_dispatch<'a>(
    catalog: &'a BTreeMap<String, ToolBinding>,
    alias: &str,
    placement: ToolPlacement,
) -> Result<&'a ToolBinding, CatalogError> {
    let binding = catalog
        .get(alias)
        .ok_or_else(|| CatalogError::UndisclosedTool(alias.into()))?;
    if binding.placement != placement {
        return Err(CatalogError::RouteSubstitution {
            alias: alias.into(),
            expected: binding.placement,
            actual: placement,
        });
    }
    Ok(binding)
}

pub fn sha256_digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn binding(alias: &str, placement: ToolPlacement) -> ToolBinding {
        ToolBinding {
            tool_ref: Uuid::now_v7(),
            model_alias: alias.into(),
            placement,
            schema_digest: "sha256:schema".into(),
            dispatch_binding: "target".into(),
        }
    }
    #[test]
    fn placement_catalog_rejects_alias_collision_and_route_substitution() {
        assert!(matches!(
            merge_disclosed_tools([
                binding("read", ToolPlacement::Gateway),
                binding("read", ToolPlacement::Runner)
            ]),
            Err(CatalogError::AliasCollision(_))
        ));
        let catalog = merge_disclosed_tools([binding("read", ToolPlacement::Gateway)]).unwrap();
        assert!(matches!(
            validate_dispatch(&catalog, "read", ToolPlacement::Runner),
            Err(CatalogError::RouteSubstitution { .. })
        ));
    }
}
