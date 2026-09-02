//! Canonical Phase 1 contract for the host/environment operational database.
//!
//! Runtime stores are added by their owning crates in later phases. This crate
//! owns only the shared metadata schema, empty service-schema boundaries, and
//! the release assets used by deployment bootstrap jobs.

pub mod integrity;
pub mod registration;
pub mod runtime;

/// Logical database identity required by every operational runtime.
pub const EXPECTED_DATABASE: &str = "operations";

/// Shared in-container location for an audience-specific operational DSN.
pub const DEFAULT_DATABASE_URL_FILE: &str = "/run/secrets/operational-database-url";

/// Phase 1 schema contract generation.
pub const CONTRACT_GENERATION: u64 = 1;

/// Ordered service-owned schema names created empty in Phase 1.
pub const OPERATIONAL_SCHEMAS: &[&str] = &[
    "operational_meta",
    "execution_ops",
    "agent_ops",
    "a2a_ops",
    "workflow_ops",
    "gateway_ops",
    "audit_ops",
    "artifact_ops",
];

/// Canonical ordered migrations embedded for packaging and contract tests.
pub const MIGRATIONS: &[(&str, &str)] = &[
    (
        "0001_operational_roles_and_ledger",
        include_str!("../migrations/metadata-postgres/0001_operational_roles_and_ledger.sql"),
    ),
    (
        "0002_operational_store_binding",
        include_str!("../migrations/metadata-postgres/0002_operational_store_binding.sql"),
    ),
    (
        "0003_empty_service_schemas",
        include_str!("../migrations/metadata-postgres/0003_empty_service_schemas.sql"),
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_one_contract_is_stable() {
        assert_eq!(EXPECTED_DATABASE, "operations");
        assert_eq!(CONTRACT_GENERATION, 1);
        assert_eq!(OPERATIONAL_SCHEMAS.len(), 8);
        assert_eq!(MIGRATIONS.len(), 3);
    }

    #[test]
    fn migrations_do_not_target_control_plane_databases() {
        for (_, sql) in MIGRATIONS {
            assert!(!sql.contains("configserver."));
            assert!(!sql.contains("knowledge."));
            assert!(!sql.contains("CREATE DATABASE"));
        }
    }
}
