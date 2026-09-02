//! Fail-closed runtime consumption of a Host-scoped registration.

use sqlx::{PgPool, Row};
use std::path::Path;
use uuid::Uuid;

pub const CONTRACT_VERSION: u64 = 2;
pub const DATABASE_URL_FILE: &str = "/run/secrets/operational-database-url";

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
    pub role_suffix: &'a str,
    pub minimum_schema_generation: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeValidationError {
    #[error("operational-store database query failed: {0}")]
    Database(#[from] sqlx::Error),
    #[error("operational-store runtime contract failed: {0}")]
    Contract(String),
}

pub fn postgres_identifier(value: &str) -> bool {
    let mut bytes = value.bytes();
    matches!(bytes.next(), Some(first) if first.is_ascii_lowercase())
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        && value.len() <= 63
}

pub fn runtime_role(database: &str, role_suffix: &str) -> Result<String, RuntimeValidationError> {
    if !postgres_identifier(database) || !postgres_identifier(role_suffix) {
        return Err(RuntimeValidationError::Contract(
            "database identity and runtime role suffix must be PostgreSQL identifiers".into(),
        ));
    }
    let role = format!("{database}_{role_suffix}");
    if role.len() > 63 {
        return Err(RuntimeValidationError::Contract(
            "derived runtime role exceeds the PostgreSQL identifier limit".into(),
        ));
    }
    Ok(role)
}

pub fn read_database_url(
    path: &Path,
    expected_server_host: &str,
    expected_port: u16,
    expected_tls_mode: &str,
    expected_database: &str,
    role_suffix: &str,
) -> Result<String, RuntimeValidationError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        RuntimeValidationError::Contract(format!("cannot inspect database URL file: {error}"))
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(RuntimeValidationError::Contract(
            "database URL path must be a regular non-symlink file".into(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o037 != 0 {
            return Err(RuntimeValidationError::Contract(
                "database URL file permissions are too broad".into(),
            ));
        }
    }
    let value = std::fs::read_to_string(path).map_err(|error| {
        RuntimeValidationError::Contract(format!("cannot read database URL file: {error}"))
    })?;
    let value = value.trim_end_matches(['\r', '\n']);
    let role = runtime_role(expected_database, role_suffix)?;
    let prefix = format!("postgres://{role}:");
    let database_path = format!("/{expected_database}");
    if value.is_empty()
        || value.len() > 2048
        || value.contains(['\r', '\n'])
        || !value.starts_with(&prefix)
        || !value[prefix.len()..].contains('@')
    {
        return Err(RuntimeValidationError::Contract(format!(
            "database URL does not match role {role} and database {expected_database}"
        )));
    }
    let after_user = value[prefix.len()..]
        .split_once('@')
        .map(|(_, value)| value)
        .ok_or_else(|| RuntimeValidationError::Contract("database URL authority is missing".into()))?;
    let (authority, path_and_query) = after_user
        .split_once('/')
        .ok_or_else(|| RuntimeValidationError::Contract("database URL path is missing".into()))?;
    let (actual_host, actual_port) = authority
        .rsplit_once(':')
        .ok_or_else(|| RuntimeValidationError::Contract("database URL port is missing".into()))?;
    let actual_port = actual_port.parse::<u16>().map_err(|_| {
        RuntimeValidationError::Contract("database URL port is invalid".into())
    })?;
    let (actual_database, query) = path_and_query
        .split_once('?')
        .map_or((path_and_query, ""), |(database, query)| (database, query));
    let ssl_mode = query.split('&').find_map(|pair| pair.strip_prefix("sslmode="));
    let tls_matches = match expected_tls_mode {
        "DISABLE" => ssl_mode.is_none() || ssl_mode == Some("disable"),
        "PREFER" => ssl_mode == Some("prefer"),
        "REQUIRE" => ssl_mode == Some("require"),
        "VERIFY_CA" => ssl_mode == Some("verify-ca"),
        "VERIFY_FULL" => ssl_mode == Some("verify-full"),
        _ => false,
    };
    if actual_host != expected_server_host
        || actual_port != expected_port
        || actual_database != expected_database
        || !tls_matches
        || !value.contains(&database_path)
    {
        return Err(RuntimeValidationError::Contract(format!(
            "database URL does not match registered endpoint {expected_server_host}:{expected_port}, TLS mode {expected_tls_mode}, and database {expected_database}"
        )));
    }
    Ok(value.to_string())
}

pub async fn validate_binding(
    pool: &PgPool,
    expected: &ExpectedBinding<'_>,
) -> Result<(), RuntimeValidationError> {
    if expected.binding_id.is_nil()
        || expected.host_id.is_nil()
        || expected.environment.trim().is_empty()
        || expected.server_host.trim().is_empty()
        || expected.port == 0
        || !matches!(expected.tls_mode, "DISABLE" | "PREFER" | "REQUIRE" | "VERIFY_CA" | "VERIFY_FULL")
        || expected.minimum_schema_generation < 1
        || expected.binding_digest.len() != 71
        || !expected.binding_digest.starts_with("sha256:")
    {
        return Err(RuntimeValidationError::Contract(
            "incomplete operational-store runtime projection".into(),
        ));
    }
    let role = runtime_role(expected.expected_database, expected.role_suffix)?;
    let identity = sqlx::query(
        "SELECT current_database() AS database_name,current_user AS role_name,
                has_database_privilege(current_user,current_database(),'CREATE') AS database_create",
    )
    .fetch_one(pool)
    .await?;
    if identity.try_get::<String, _>("database_name")? != expected.expected_database
        || identity.try_get::<String, _>("role_name")? != role
        || identity.try_get::<bool, _>("database_create")?
    {
        return Err(RuntimeValidationError::Contract(format!(
            "expected database {} and role {role} without CREATE DATABASE authority",
            expected.expected_database
        )));
    }
    let binding = sqlx::query(
        "SELECT binding_id,binding_version,binding_digest,scope_kind,scope_id,host_id,
                environment,database_identity,deployment_profile,schema_contract_generation
           FROM operational_meta.operational_store_binding_t WHERE active",
    )
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| RuntimeValidationError::Contract("active binding is missing".into()))?;
    if binding.try_get::<Uuid, _>("binding_id")? != expected.binding_id
        || binding.try_get::<i64, _>("binding_version")? < CONTRACT_VERSION as i64
        || binding.try_get::<String, _>("binding_digest")? != expected.binding_digest
        || binding.try_get::<String, _>("scope_kind")? != "HOST"
        || binding.try_get::<Uuid, _>("scope_id")? != expected.host_id
        || binding.try_get::<Uuid, _>("host_id")? != expected.host_id
        || binding.try_get::<Option<String>, _>("environment")?.is_some()
        || binding.try_get::<String, _>("database_identity")? != expected.expected_database
        || binding.try_get::<String, _>("deployment_profile")? != "CUSTOMER_MANAGED"
        || binding.try_get::<i64, _>("schema_contract_generation")?
            < expected.minimum_schema_generation
    {
        return Err(RuntimeValidationError::Contract(
            "active binding does not match the exact Host runtime audience".into(),
        ));
    }
    let identity = sqlx::query(
        "SELECT scope_root_id,database_identity
           FROM operational_meta.operational_database_identity_t WHERE singleton",
    )
    .fetch_one(pool)
    .await?;
    if identity.try_get::<Uuid, _>("scope_root_id")? != expected.host_id
        || identity.try_get::<String, _>("database_identity")? != expected.expected_database
    {
        return Err(RuntimeValidationError::Contract(
            "immutable operational database identity does not match the Host audience".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn three_host_url_audiences_reject_every_swap() {
        let root = std::env::temp_dir().join(format!("operational-store-p5-{}", Uuid::now_v7()));
        fs::create_dir_all(&root).unwrap();
        let audiences = [
            ("dev.lightapi.net", "operations"),
            ("dev.networknt.com", "operations_networknt"),
            ("dev.taiji.io", "operations_taiji"),
        ];
        let mut paths = Vec::new();
        for (host, database) in audiences {
            let path = root.join(host);
            fs::write(
                &path,
                format!("postgres://{database}_agent_runtime:secret@postgres:5432/{database}"),
            )
            .unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            }
            assert!(read_database_url(&path, "postgres", 5432, "DISABLE", database, "agent_runtime").is_ok());
            assert!(read_database_url(&path, "customer-db", 5432, "DISABLE", database, "agent_runtime").is_err());
            assert!(read_database_url(&path, "postgres", 5433, "DISABLE", database, "agent_runtime").is_err());
            assert!(read_database_url(&path, "postgres", 5432, "VERIFY_FULL", database, "agent_runtime").is_err());
            paths.push(path);
        }
        for (expected_index, (_, expected_database)) in audiences.iter().enumerate() {
            for (actual_index, path) in paths.iter().enumerate() {
                if expected_index != actual_index {
                    assert!(read_database_url(path, "postgres", 5432, "DISABLE", expected_database, "agent_runtime").is_err());
                }
            }
        }
        fs::remove_dir_all(root).unwrap();
    }
}
