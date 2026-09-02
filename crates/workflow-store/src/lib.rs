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
pub const MIGRATION_SQL: &str =
    include_str!("../migrations/workflow-postgres/0001_workflow_runtime.sql");
pub const A2A_BINDING_MIGRATION_SQL: &str =
    include_str!("../migrations/workflow-postgres/0002_governed_a2a_outbound.sql");
pub const CONSUMER_OFFSETS_MIGRATION_SQL: &str =
    include_str!("../migrations/workflow-postgres/0004_workflow_consumer_offsets.sql");

/// Durable Workflow state. The first three entries are the Phase 0 deferred
/// roots; the remaining rows are runtime-owned state discovered during the
/// Phase 5 authority audit.
pub const AUTHORITY_TABLES: &[&str] = &[
    "consumer_offsets",
    "process_info_t",
    "task_info_t",
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

pub fn read_database_url(path: &Path, server_host: &str, port: u16, tls_mode: &str,
                         expected_database: &str) -> Result<String, ValidationError> {
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_inventory_and_boundary_are_frozen() {
        assert_eq!(AUTHORITY_TABLES.len(), 18);
        assert_eq!(PROJECTION_TABLES.len(), 7);
        for table in AUTHORITY_TABLES
            .iter()
            .skip(1)
            .chain(PROJECTION_TABLES.iter().take(6))
        {
            assert!(MIGRATION_SQL.contains(&format!("workflow_ops.{table}")));
        }
        assert!(!MIGRATION_SQL.contains("configserver."));
        assert!(!MIGRATION_SQL.contains("REFERENCES public."));
        assert!(A2A_BINDING_MIGRATION_SQL.contains("workflow_ops.workflow_a2a_binding_t"));
        assert!(!A2A_BINDING_MIGRATION_SQL.contains("server"));
        assert!(CONSUMER_OFFSETS_MIGRATION_SQL.contains("workflow_ops.consumer_offsets"));
    }
}
