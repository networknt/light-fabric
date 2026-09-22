//! Workflow operational-store authority and exact binding validation.

use sqlx::{PgPool, Row};
use std::path::Path;
use uuid::Uuid;

pub const EXPECTED_DATABASE: &str = "operations";
pub const EXPECTED_SCHEMA: &str = "workflow_ops";
pub const EXPECTED_RUNTIME_ROLE: &str = "operations_workflow_runtime";
pub const DEFAULT_DATABASE_URL_FILE: &str = "/run/secrets/operational-database-url";
pub const MIGRATION_ID: &str = "0001_workflow_runtime";
pub const A2A_BINDING_MIGRATION_ID: &str = "0002_governed_a2a_outbound";
pub const CONSUMER_OFFSETS_MIGRATION_ID: &str = "0004_workflow_consumer_offsets";
pub const CATALOG_PROJECTION_MIGRATION_ID: &str = "0005_workflow_catalog_projection";
pub const ENDPOINT_RESOLUTION_MIGRATION_ID: &str = "0006_workflow_endpoint_resolution";
pub const DEVELOPMENT_MIGRATION_ID: &str = "0008_development_workflow";
pub const AGENT_DISPATCH_MIGRATION_ID: &str = "0009_workflow_agent_dispatch";
pub const ACTION_PRIVILEGES_MIGRATION_ID: &str = "0010_workflow_action_runtime_privileges";
pub const HUMAN_TASK_MIGRATION_ID: &str = "0011_workflow_human_task_assignment";
pub const ACTION_TABLES: &[&str] = &[
    "workflow_action_authority_t",
    "workflow_action_permit_t",
    "workflow_action_dispatch_t",
    "workflow_action_audit_t",
    "workflow_gateway_owner_t",
    "workflow_gateway_boot_t",
];
pub const AGENT_DISPATCH_MIGRATION_SQL: &str =
    include_str!("../migrations/workflow-postgres/0009_workflow_agent_dispatch.sql");
pub const DEVELOPMENT_MIGRATION_SQL: &str =
    include_str!("../migrations/workflow-postgres/0008_development_workflow.sql");
pub const HUMAN_TASK_MIGRATION_SQL: &str =
    include_str!("../migrations/workflow-postgres/0011_workflow_human_task_assignment.sql");
pub const DEVELOPMENT_TABLES: &[&str] = &[
    "development_vm_t",
    "development_feature_t",
    "development_stage_t",
    "development_turn_t",
    "development_execution_fence_t",
    "development_transition_t",
];
pub const MIGRATION_SQL: &str =
    include_str!("../migrations/workflow-postgres/0001_workflow_runtime.sql");
pub const A2A_BINDING_MIGRATION_SQL: &str =
    include_str!("../migrations/workflow-postgres/0002_governed_a2a_outbound.sql");
pub const CONSUMER_OFFSETS_MIGRATION_SQL: &str =
    include_str!("../migrations/workflow-postgres/0004_workflow_consumer_offsets.sql");
pub const CATALOG_PROJECTION_MIGRATION_SQL: &str =
    include_str!("../migrations/workflow-postgres/0005_workflow_catalog_projection.sql");
pub const ENDPOINT_RESOLUTION_MIGRATION_SQL: &str =
    include_str!("../migrations/workflow-postgres/0006_workflow_endpoint_resolution.sql");

/// Durable Workflow state. The first three entries are the Phase 0 deferred
/// roots; the remaining rows are runtime-owned state discovered during the
/// Phase 5 authority audit.
pub const AUTHORITY_TABLES: &[&str] = &[
    "consumer_offsets",
    "process_info_t",
    "task_info_t",
    "task_asst_t",
    "workflow_approval_t",
    "workflow_artifact_t",
    "workflow_executor_tenant_turn_t",
    "workflow_fork_branch_t",
    "workflow_fork_join_t",
    "workflow_invocation_audit_outbox_t",
    "workflow_invocation_budget_reservation_t",
    "workflow_invocation_budget_t",
    "workflow_invocation_event_quarantine_t",
    "workflow_invocation_idempotency_t",
    "workflow_invocation_t",
    "workflow_task_effect_t",
    "workflow_tool_access_request_item_t",
    "workflow_tool_access_request_t",
    "workflow_tool_approval_evidence_t",
];

/// Accepted immutable/local projections. These rows are not Portal authoring
/// authority and may only be replaced from an authenticated publication.
pub const PROJECTION_TABLES: &[&str] = &[
    "wf_definition_t",
    "workflow_endpoint_target_t",
    "workflow_execution_policy_t",
    "workflow_tool_binding_t",
    "workflow_tool_dependency_t",
    "workflow_tool_grant_t",
    "workflow_a2a_binding_t",
];

#[derive(Debug, Clone)]
pub struct ExpectedBinding<'a> {
    pub binding_id: Uuid,
    pub binding_digest: &'a str,
    pub host_id: Uuid,
    pub environment: &'a str,
    pub server_host: &'a str,
    pub port: u16,
    pub tls_mode: &'a str,
    pub expected_database: &'a str,
    pub minimum_schema_generation: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum ValidationError {
    #[error("workflow-store database query failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Runtime(#[from] operational_store::runtime::RuntimeValidationError),
    #[error("workflow-store scope validation failed: {0}")]
    Scope(String),
}

pub fn read_database_url(
    path: &Path,
    server_host: &str,
    port: u16,
    tls_mode: &str,
    expected_database: &str,
) -> Result<String, ValidationError> {
    Ok(operational_store::runtime::read_database_url(
        path,
        server_host,
        port,
        tls_mode,
        expected_database,
        "workflow_runtime",
    )?)
}

pub async fn validate(
    pool: &PgPool,
    expected: &ExpectedBinding<'_>,
) -> Result<(), ValidationError> {
    operational_store::runtime::validate_binding(
        pool,
        &operational_store::runtime::ExpectedBinding {
            binding_id: expected.binding_id,
            binding_digest: expected.binding_digest,
            host_id: expected.host_id,
            environment: expected.environment,
            server_host: expected.server_host,
            port: expected.port,
            tls_mode: expected.tls_mode,
            expected_database: expected.expected_database,
            role_suffix: "workflow_runtime",
            minimum_schema_generation: expected.minimum_schema_generation,
        },
    )
    .await?;
    let identity = sqlx::query(
        "SELECT has_schema_privilege(current_user,'workflow_ops','CREATE') AS schema_create",
    )
    .fetch_one(pool)
    .await?;
    let schema_create: bool = identity.try_get("schema_create")?;
    if schema_create {
        return Err(ValidationError::Scope(
            "Workflow runtime role must not have CREATE authority".into(),
        ));
    }

    let required = AUTHORITY_TABLES
        .iter()
        .chain(PROJECTION_TABLES)
        .chain(DEVELOPMENT_TABLES)
        .chain(ACTION_TABLES)
        .copied()
        .collect::<Vec<_>>();
    let missing: Vec<String> = sqlx::query_scalar(
        "SELECT required.table_name FROM unnest($1::text[]) AS required(table_name)
          WHERE to_regclass('workflow_ops.' || required.table_name) IS NULL",
    )
    .bind(&required)
    .fetch_all(pool)
    .await?;
    if !missing.is_empty() {
        return Err(ValidationError::Scope(format!(
            "Workflow schema is incomplete; missing {}",
            missing.join(",")
        )));
    }
    let migration_ready: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM operational_meta.operational_schema_migration_t
          WHERE migration_owner='workflow-store' AND schema_name='workflow_ops' AND migration_id=$1)",
    )
    .bind(MIGRATION_ID)
    .fetch_one(pool)
    .await?;
    if !migration_ready {
        return Err(ValidationError::Scope(
            "workflow-store migration ledger entry is missing".into(),
        ));
    }
    let a2a_binding_ready: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM operational_meta.operational_schema_migration_t
          WHERE migration_owner='workflow-store' AND schema_name='workflow_ops' AND migration_id=$1)",
    )
    .bind(A2A_BINDING_MIGRATION_ID)
    .fetch_one(pool)
    .await?;
    if !a2a_binding_ready {
        return Err(ValidationError::Scope(
            "Workflow governed A2A binding migration ledger entry is missing".into(),
        ));
    }
    let consumer_offsets_ready: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM operational_meta.operational_schema_migration_t
          WHERE migration_owner='workflow-store' AND schema_name='workflow_ops' AND migration_id=$1)",
    )
    .bind(CONSUMER_OFFSETS_MIGRATION_ID)
    .fetch_one(pool)
    .await?;
    if !consumer_offsets_ready {
        return Err(ValidationError::Scope(
            "Workflow consumer-offset migration ledger entry is missing".into(),
        ));
    }
    let catalog_projection_ready: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM operational_meta.operational_schema_migration_t
          WHERE migration_owner='workflow-store' AND schema_name='workflow_ops' AND migration_id=$1)",
    )
    .bind(CATALOG_PROJECTION_MIGRATION_ID)
    .fetch_one(pool)
    .await?;
    if !catalog_projection_ready {
        return Err(ValidationError::Scope(
            "Workflow catalog-projection migration ledger entry is missing".into(),
        ));
    }
    let endpoint_resolution_ready: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM operational_meta.operational_schema_migration_t
          WHERE migration_owner='workflow-store' AND schema_name='workflow_ops' AND migration_id=$1)",
    )
    .bind(ENDPOINT_RESOLUTION_MIGRATION_ID)
    .fetch_one(pool)
    .await?;
    if !endpoint_resolution_ready {
        return Err(ValidationError::Scope(
            "Workflow endpoint-resolution migration ledger entry is missing".into(),
        ));
    }
    let development_ready: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM operational_meta.operational_schema_migration_t
          WHERE migration_owner='workflow-store' AND schema_name='workflow_ops' AND migration_id=$1)",
    ).bind(DEVELOPMENT_MIGRATION_ID).fetch_one(pool).await?;
    if !development_ready {
        return Err(ValidationError::Scope(
            "Workflow development migration ledger entry is missing".into(),
        ));
    }
    let dispatch_ready: bool = sqlx::query_scalar(
        "SELECT to_regclass('workflow_ops.workflow_agent_job_t') IS NOT NULL AND EXISTS(SELECT 1 FROM operational_meta.operational_schema_migration_t WHERE migration_owner='workflow-store' AND schema_name='workflow_ops' AND migration_id=$1)",
    ).bind(AGENT_DISPATCH_MIGRATION_ID).fetch_one(pool).await?;
    if !dispatch_ready {
        return Err(ValidationError::Scope(
            "Workflow Agent dispatch migration is missing".into(),
        ));
    }
    let action_ready: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM operational_meta.operational_schema_migration_t WHERE migration_owner='workflow-store' AND schema_name='workflow_ops' AND migration_id=$1)",
    ).bind(ACTION_PRIVILEGES_MIGRATION_ID).fetch_one(pool).await?;
    if !action_ready {
        return Err(ValidationError::Scope(
            "Workflow action runtime privilege migration is missing".into(),
        ));
    }
    let human_task_ready: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM operational_meta.operational_schema_migration_t WHERE migration_owner='workflow-store' AND schema_name='workflow_ops' AND migration_id=$1)",
    ).bind(HUMAN_TASK_MIGRATION_ID).fetch_one(pool).await?;
    if !human_task_ready {
        return Err(ValidationError::Scope(
            "Workflow human-task assignment migration is missing".into(),
        ));
    }
    let denied: Vec<String> = sqlx::query_scalar(
        "SELECT t FROM unnest($1::text[]) AS t WHERE NOT (
            has_table_privilege(current_user,'workflow_ops.' || t,'SELECT') AND
            has_table_privilege(current_user,'workflow_ops.' || t,'INSERT') AND
            has_table_privilege(current_user,'workflow_ops.' || t,'UPDATE'))",
    )
    .bind(ACTION_TABLES)
    .fetch_all(pool)
    .await?;
    if !denied.is_empty() {
        return Err(ValidationError::Scope(format!(
            "Workflow action runtime privileges missing: {}",
            denied.join(",")
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_inventory_and_boundary_are_frozen() {
        assert_eq!(AUTHORITY_TABLES.len(), 19);
        assert_eq!(PROJECTION_TABLES.len(), 7);
        for table in AUTHORITY_TABLES
            .iter()
            .skip(1)
            .filter(|table| **table != "task_asst_t")
            .chain(PROJECTION_TABLES.iter().take(6))
        {
            assert!(MIGRATION_SQL.contains(&format!("workflow_ops.{table}")));
        }
        assert!(!MIGRATION_SQL.contains("configserver."));
        assert!(!MIGRATION_SQL.contains("REFERENCES public."));
        assert!(A2A_BINDING_MIGRATION_SQL.contains("workflow_ops.workflow_a2a_binding_t"));
        assert!(!A2A_BINDING_MIGRATION_SQL.contains("server"));
        assert!(CONSUMER_OFFSETS_MIGRATION_SQL.contains("workflow_ops.consumer_offsets"));
        assert!(CATALOG_PROJECTION_MIGRATION_SQL.contains("tool_name"));
        assert!(ENDPOINT_RESOLUTION_MIGRATION_SQL.contains("resolution_document"));
        for table in DEVELOPMENT_TABLES {
            assert!(
                DEVELOPMENT_MIGRATION_SQL.contains(&format!("CREATE TABLE workflow_ops.{table}"))
            );
        }
        assert!(!DEVELOPMENT_MIGRATION_SQL.contains("configserver."));
        assert!(HUMAN_TASK_MIGRATION_SQL.contains("workflow_ops.task_asst_t"));
        assert!(!HUMAN_TASK_MIGRATION_SQL.contains("configserver."));
    }
}
