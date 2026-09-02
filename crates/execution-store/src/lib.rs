//! Shared execution-store authority and startup validation.
//!
//! The Controller keeps its Config Server connection for registry state. Its
//! runner subsystem uses a distinct pool validated here before readiness.

use sqlx::{PgPool, Row};
use uuid::Uuid;

pub const EXPECTED_DATABASE: &str = "operations";
pub const EXPECTED_SCHEMA: &str = "execution_ops";
pub const EXPECTED_RUNTIME_ROLE: &str = "operations_execution_runtime";
pub const DEFAULT_DATABASE_URL_FILE: &str = "/run/secrets/execution-database-url";
pub const MIGRATION_ID: &str = "0001_execution_foundations";
pub const MIGRATIONS: &[(&str, &str)] = &[(
    MIGRATION_ID,
    include_str!("../migrations/execution-postgres/0001_execution_foundations.sql"),
)];

pub const TABLES: &[&str] = &[
    "runner_session_t",
    "runner_backend_t",
    "runner_scheduling_request_t",
    "execution_session_t",
    "execution_session_cleanup_request_t",
    "execution_attempt_t",
    "execution_credential_grant_audit_t",
    "execution_fixed_action_t",
    "execution_input_t",
    "execution_provenance_t",
    "execution_runtime_audit_t",
    "execution_runtime_tool_manifest_t",
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
    #[error("execution-store database query failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error(transparent)]
    Runtime(#[from] operational_store::runtime::RuntimeValidationError),
    #[error("execution-store scope validation failed: {0}")]
    Scope(String),
}

/// Fail closed unless this pool is the least-privileged execution pool for the
/// exact active Host/environment binding compiled into the runtime projection.
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
            role_suffix: "execution_runtime",
            minimum_schema_generation: expected.minimum_schema_generation,
        },
    )
    .await?;
    let identity = sqlx::query(
        "SELECT has_schema_privilege(current_user,'execution_ops','CREATE') AS schema_create",
    )
    .fetch_one(pool)
    .await?;
    let schema_create: bool = identity.try_get("schema_create")?;
    if schema_create {
        return Err(ValidationError::Scope(
            "execution runtime role must not have CREATE authority".into(),
        ));
    }

    let missing: Vec<String> = sqlx::query_scalar(
        "SELECT required.table_name FROM unnest($1::text[]) AS required(table_name)
          WHERE to_regclass('execution_ops.' || required.table_name) IS NULL",
    )
    .bind(TABLES)
    .fetch_all(pool)
    .await?;
    if !missing.is_empty() {
        return Err(ValidationError::Scope(format!(
            "execution schema is incomplete; missing {}",
            missing.join(",")
        )));
    }

    let migration_ready: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM operational_meta.operational_schema_migration_t
          WHERE migration_owner='execution-store' AND schema_name='execution_ops'
            AND migration_id=$1)",
    )
    .bind(MIGRATION_ID)
    .fetch_one(pool)
    .await?;
    if !migration_ready {
        return Err(ValidationError::Scope(
            "execution-store migration ledger entry is missing".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frozen_execution_inventory_is_exact() {
        assert_eq!(TABLES.len(), 12);
        assert_eq!(MIGRATIONS.len(), 1);
        let sql = MIGRATIONS[0].1;
        for table in TABLES {
            assert!(sql.contains(&format!("execution_ops.{table}")));
        }
        assert!(!sql.contains("configserver."));
        assert!(!sql.contains("knowledge."));
        assert!(!sql.contains("REFERENCES execution_ops.workflow_"));
        assert!(!sql.contains("REFERENCES execution_ops.agent_"));
    }
}
