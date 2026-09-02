//! Version-2 Host-scoped operational-store registration contract.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use uuid::Uuid;

pub const CONTRACT_VERSION: u64 = 2;
pub const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
pub const SCOPE_KIND: &str = "HOST";
pub const DEFAULT_DATABASE_URL_FILE: &str = "/run/secrets/operational-database-url";

pub const DIGEST_FIELDS: &[&str] = &[
    "bindingId",
    "contractVersion",
    "credentialGeneration",
    "credentialReference",
    "credentialSource",
    "engine",
    "expectedDatabase",
    "hostId",
    "minimumSchemaGeneration",
    "port",
    "serverHost",
    "tlsMode",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Engine {
    Postgresql,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum TlsMode {
    Disable,
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CredentialSource {
    MountedFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum LifecycleState {
    Registered,
    Deactivated,
    Unregistered,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RegistrationRequest {
    pub target_host_id: Uuid,
    pub engine: Engine,
    pub server_host: String,
    pub port: u16,
    pub expected_database: String,
    pub tls_mode: TlsMode,
    pub credential_source: CredentialSource,
    pub credential_reference: String,
    pub minimum_schema_generation: u64,
    pub credential_generation: u64,
    pub aggregate_version: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Registration {
    pub contract_version: u64,
    pub binding_id: Uuid,
    pub binding_digest: String,
    pub host_id: Uuid,
    pub scope_kind: String,
    pub engine: Engine,
    pub server_host: String,
    pub port: u16,
    pub expected_database: String,
    pub tls_mode: TlsMode,
    pub credential_source: CredentialSource,
    pub credential_reference: String,
    pub minimum_schema_generation: u64,
    pub credential_generation: u64,
    pub lifecycle_state: LifecycleState,
    pub aggregate_version: u64,
    pub active: bool,
    pub published: bool,
}

pub fn validate_request(request: &RegistrationRequest, update: bool) -> Vec<String> {
    let mut violations = Vec::new();
    if request.server_host.is_empty()
        || request.server_host.len() > 253
        || !request
            .server_host
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        || !request
            .server_host
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
        || !request
            .server_host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        violations.push("serverHost: invalid value".to_string());
    }
    for (field, value) in [("expectedDatabase", request.expected_database.as_str())] {
        if !postgres_identifier(value) {
            violations.push(format!("{field}: invalid value"));
        }
    }
    if request.credential_reference.is_empty() || request.credential_reference.len() > 512 {
        violations.push(
            "credentialReference: non-blank string of at most 512 characters is required"
                .to_string(),
        );
    } else if !request.credential_reference.starts_with('/') {
        violations.push("credentialReference: mounted file must be an absolute path".to_string());
    }
    let credential_reference = request.credential_reference.to_ascii_lowercase();
    if credential_reference.contains("postgres://")
        || credential_reference.contains("postgresql://")
        || credential_reference.contains("password=")
        || credential_reference.contains("pwd=")
    {
        violations.push("credentialReference: credential material is not allowed".to_string());
    }
    if request.minimum_schema_generation == 0
        || request.minimum_schema_generation > MAX_SAFE_INTEGER
    {
        violations.push("minimumSchemaGeneration: positive integer is required".to_string());
    }
    if request.credential_generation == 0 || request.credential_generation > MAX_SAFE_INTEGER {
        violations.push("credentialGeneration: positive integer is required".to_string());
    }
    if update
        && request
            .aggregate_version
            .is_none_or(|version| version == 0 || version > MAX_SAFE_INTEGER)
    {
        violations.push("aggregateVersion: positive integer is required".to_string());
    }
    violations
}

pub fn binding_digest(registration: &Registration) -> Result<String, serde_json::Error> {
    let value = serde_json::to_value(registration)?;
    let object = value
        .as_object()
        .expect("Registration always serializes as a JSON object");
    let payload: BTreeMap<&str, &Value> = DIGEST_FIELDS
        .iter()
        .map(|field| (*field, &object[*field]))
        .collect();
    let canonical = serde_json::to_vec(&payload)?;
    Ok(format!("sha256:{:x}", Sha256::digest(canonical)))
}

fn postgres_identifier(value: &str) -> bool {
    let mut bytes = value.bytes();
    matches!(bytes.next(), Some(first) if first.is_ascii_lowercase())
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        && value.len() <= 63
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Fixture {
        registration_request: RegistrationRequest,
        registration: Registration,
        compatibility: Compatibility,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Compatibility {
        version1_replay_side_effects: bool,
        automatic_conversion: bool,
        conversion_requires_explicit_command: bool,
    }

    fn fixture() -> Fixture {
        serde_json::from_str(include_str!(
            "../../../contracts/operational-store-registration/v2/fixture.json"
        ))
        .expect("registration fixture must remain valid")
    }

    #[test]
    fn freezes_host_scoped_registration_and_digest() {
        let fixture = fixture();
        assert_eq!(CONTRACT_VERSION, 2);
        assert_eq!(SCOPE_KIND, "HOST");
        assert!(validate_request(&fixture.registration_request, false).is_empty());
        assert_eq!(
            binding_digest(&fixture.registration).unwrap(),
            fixture.registration.binding_digest
        );
    }

    #[test]
    fn requires_update_concurrency_and_side_effect_free_v1_replay() {
        let fixture = fixture();
        assert!(
            validate_request(&fixture.registration_request, true)
                .iter()
                .any(|value| value.starts_with("aggregateVersion:"))
        );
        assert!(!fixture.compatibility.version1_replay_side_effects);
        assert!(!fixture.compatibility.automatic_conversion);
        assert!(fixture.compatibility.conversion_requires_explicit_command);
    }

    #[test]
    fn rejects_environment_and_plaintext_secrets_as_unknown_fields() {
        let source =
            include_str!("../../../contracts/operational-store-registration/v2/fixture.json");
        let mut value: Value = serde_json::from_str(source).unwrap();
        let request = value["registrationRequest"].as_object_mut().unwrap();
        request.insert("environment".to_string(), Value::String("dev".to_string()));
        request.insert("password".to_string(), Value::String("secret".to_string()));
        assert!(
            serde_json::from_value::<RegistrationRequest>(value["registrationRequest"].clone())
                .is_err()
        );
    }

    #[test]
    fn rejects_unimplemented_secret_reference_sources() {
        let source = include_str!(
            "../../../contracts/operational-store-registration/v2/fixture.json"
        );
        let mut value: Value = serde_json::from_str(source).unwrap();
        value["registrationRequest"]["credentialSource"] =
            Value::String("SECRET_REFERENCE".to_string());
        assert!(serde_json::from_value::<RegistrationRequest>(
            value["registrationRequest"].clone()
        )
        .is_err());
    }
}
