use regex::Regex;
use serde::Deserialize;
use serde_yaml::{Mapping, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const CONTRACT_DIR: &str = "config-contract";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Inventory {
    version: u16,
    service: String,
    static_environment: Vec<EnvironmentEntry>,
    yaml_properties: Vec<YamlEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EnvironmentEntry {
    key: String,
    current_source: String,
    final_authority: String,
    value_type: String,
    default: Option<Value>,
    validation: String,
    sensitivity: String,
    reload_class: String,
    consumers: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct YamlEntry {
    file: String,
    path: String,
    final_authority: String,
    reload_class: String,
    sensitivity: String,
}

#[derive(Debug, Deserialize)]
struct ResolverManifest {
    version: u16,
    resolvers: Vec<Resolver>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Resolver {
    id: String,
    source: String,
    #[serde(default)]
    patterns: Vec<String>,
    #[serde(default)]
    accepted_references: Vec<String>,
    #[serde(default)]
    rejected_managed_references: Vec<String>,
    value_authority: String,
    source_anchor: String,
    validation: String,
}

#[derive(Debug, Deserialize)]
struct CharacterizationManifest {
    version: u16,
    cases: Vec<CharacterizationCase>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CharacterizationCase {
    id: String,
    layer: String,
    evidence: String,
    postgres_required: bool,
}

#[derive(Debug, Deserialize)]
struct RemoteFixture {
    files: BTreeMap<String, String>,
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn contract_path(name: &str) -> PathBuf {
    manifest_dir().join(CONTRACT_DIR).join(name)
}

fn read_yaml<T: for<'de> Deserialize<'de>>(path: &Path) -> T {
    let input = fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
    serde_yaml::from_str(&input)
        .unwrap_or_else(|error| panic!("failed to parse {}: {error}", path.display()))
}

fn production_sources() -> Vec<(String, String)> {
    let source_dir = manifest_dir().join("src");
    let mut files = fs::read_dir(&source_dir)
        .expect("workflow source directory")
        .map(|entry| entry.expect("workflow source entry").path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "rs"))
        .collect::<Vec<_>>();
    files.sort();
    files
        .into_iter()
        .map(|path| {
            let name = path
                .file_name()
                .expect("source file name")
                .to_string_lossy()
                .into_owned();
            let source = fs::read_to_string(&path)
                .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
            (name, source)
        })
        .collect()
}

fn static_environment_reads() -> BTreeSet<String> {
    let call = Regex::new(
        r#"(?:env::var|required_environment|required_list_environment|env_bool|with_legacy_ansi_env)\s*\(\s*\"([A-Z][A-Z0-9_]+)\""#,
    )
    .unwrap();
    let constant =
        Regex::new(r#"const\s+[A-Z][A-Z0-9_]*\s*:\s*&str\s*=\s*\"([A-Z][A-Z0-9_]+)\""#).unwrap();
    let provider =
        Regex::new(r#"provider\s*\(\s*\"([A-Z][A-Z0-9_]+)\"\s*,\s*\"([A-Z][A-Z0-9_]+)\""#).unwrap();
    let quoted_environment = Regex::new(r#"\"([A-Z][A-Z0-9_]+_(?:API_KEY))\""#).unwrap();

    let mut found = BTreeSet::new();
    for (_, source) in production_sources() {
        for captures in call.captures_iter(&source) {
            found.insert(captures[1].to_string());
        }
        for captures in constant.captures_iter(&source) {
            found.insert(captures[1].to_string());
        }
        for captures in provider.captures_iter(&source) {
            found.insert(captures[1].to_string());
            found.insert(captures[2].to_string());
        }
        for captures in quoted_environment.captures_iter(&source) {
            found.insert(captures[1].to_string());
        }
    }
    found
}

fn yaml_leaf_paths(value: &Value, prefix: &str, output: &mut BTreeSet<String>) {
    match value {
        Value::Mapping(mapping) => {
            for (key, child) in mapping {
                let key = key.as_str().expect("configuration keys must be strings");
                let path = if prefix.is_empty() {
                    key.to_string()
                } else {
                    format!("{prefix}.{key}")
                };
                yaml_leaf_paths(child, &path, output);
            }
        }
        _ => {
            output.insert(prefix.to_string());
        }
    }
}

fn effective_remote_fixture() -> Value {
    let fixture: RemoteFixture = read_yaml(&contract_path("fixtures/effective-remote-values.yml"));
    let mut effective = Mapping::new();
    for (file, document) in fixture.files {
        let key = file
            .strip_suffix(".yml")
            .expect("remote fixture file suffix");
        let value: Value = serde_yaml::from_str(&document)
            .unwrap_or_else(|error| panic!("failed to parse remote {file}: {error}"));
        effective.insert(Value::String(key.to_string()), value);
    }
    Value::Mapping(effective)
}

#[test]
fn phase0_inventory_covers_static_environment_reads_and_yaml_leaves() {
    let inventory: Inventory = read_yaml(&contract_path("configuration-inventory.yml"));
    assert_eq!(inventory.version, 1);
    assert_eq!(inventory.service, "com.networknt.workflow-1.0.0");

    let mut declared_environment = BTreeSet::new();
    for entry in &inventory.static_environment {
        assert!(
            declared_environment.insert(entry.key.clone()),
            "duplicate {}",
            entry.key
        );
        assert!(!entry.current_source.trim().is_empty());
        assert!(!entry.final_authority.trim().is_empty());
        assert!(!entry.value_type.trim().is_empty());
        assert!(!entry.validation.trim().is_empty());
        assert!(["public", "internal", "secret"].contains(&entry.sensitivity.as_str()));
        assert!(["reloadable", "restartRequired"].contains(&entry.reload_class.as_str()));
        assert!(!entry.consumers.is_empty());
        let _ = &entry.default;
    }
    let actual_environment = static_environment_reads();
    assert!(
        actual_environment.is_subset(&declared_environment),
        "undeclared environment reads: {:?}",
        actual_environment
            .difference(&declared_environment)
            .collect::<Vec<_>>()
    );

    let declared_yaml = inventory
        .yaml_properties
        .iter()
        .map(|entry| {
            assert!(!entry.final_authority.trim().is_empty());
            assert!(["reloadable", "restartRequired"].contains(&entry.reload_class.as_str()));
            assert!(["public", "internal", "secret"].contains(&entry.sensitivity.as_str()));
            format!("{}:{}", entry.file, entry.path)
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(declared_yaml.len(), inventory.yaml_properties.len());

    let mut actual_yaml = BTreeSet::new();
    for file in ["server.yml", "security.yml", "client.yml", "workflow.yml"] {
        let value: Value = read_yaml(&manifest_dir().join("config").join(file));
        let mut paths = BTreeSet::new();
        yaml_leaf_paths(&value, "", &mut paths);
        actual_yaml.extend(paths.into_iter().map(|path| format!("{file}:{path}")));
    }
    assert_eq!(declared_yaml, actual_yaml);
}

#[test]
fn phase0_dynamic_resolvers_are_explicit_and_secret_safe() {
    let manifest: ResolverManifest = read_yaml(&contract_path("dynamic-resolvers.yml"));
    assert_eq!(manifest.version, 1);
    let mut ids = BTreeSet::new();
    for resolver in &manifest.resolvers {
        assert!(ids.insert(resolver.id.clone()), "duplicate {}", resolver.id);
        assert!(!resolver.source.trim().is_empty());
        assert!(
            ["secretEnvironment", "configServer", "providerEnvironment"]
                .contains(&resolver.value_authority.as_str())
        );
        assert!(!resolver.source_anchor.trim().is_empty());
        assert!(!resolver.validation.trim().is_empty());
        assert!(!resolver.patterns.is_empty() || !resolver.accepted_references.is_empty());
    }
    assert!(ids.contains("agent-api-key-reference"));
    assert!(ids.contains("agent-provider-default-api-key"));
    assert!(ids.contains("agent-provider-base-url"));
    assert!(ids.contains("object-store-provider-environment"));
    let api_key = manifest
        .resolvers
        .iter()
        .find(|resolver| resolver.id == "agent-api-key-reference")
        .unwrap();
    assert_eq!(api_key.rejected_managed_references, ["literal:VALUE"]);
    assert_eq!(api_key.value_authority, "secretEnvironment");
    assert_eq!(
        manifest
            .resolvers
            .iter()
            .find(|resolver| resolver.id == "agent-provider-base-url")
            .unwrap()
            .value_authority,
        "configServer"
    );

    let sources = production_sources()
        .into_iter()
        .map(|(_, source)| source)
        .collect::<String>();
    for marker in [
        "env::var(env_name)",
        "agent_provider_base_urls",
        "AmazonS3Builder::from_env()",
        "strip_prefix(\"literal:\")",
    ] {
        assert!(
            sources.contains(marker),
            "dynamic resolver source marker missing: {marker}"
        );
    }
}

#[test]
fn phase0_local_and_remote_fixtures_resolve_to_one_effective_config() {
    let local: Value = read_yaml(&contract_path("fixtures/effective-local.yml"));
    let remote = effective_remote_fixture();
    assert_eq!(local, remote);

    let rendered = serde_json::to_string(&local).unwrap().to_ascii_lowercase();
    for forbidden in [
        "authorization",
        "apikey",
        "password",
        "privatekey",
        "databaseurl",
    ] {
        assert!(
            !rendered.contains(forbidden),
            "fixture contains secret-shaped field {forbidden}"
        );
    }
}

#[test]
fn phase0_identity_registration_and_observability_contracts_are_pinned() {
    let identity: Value = read_yaml(&contract_path("identity-registration.yml"));
    assert_eq!(identity["version"].as_u64(), Some(1));
    assert_eq!(
        identity["correlation"]["equalityRequired"].as_bool(),
        Some(false)
    );
    assert_eq!(
        identity["registration"]["metadata"]["tagUpdateSemantics"].as_str(),
        Some("replaceCompleteMap")
    );
    assert_eq!(
        identity["registration"]["metadata"]["durableProjectionRequired"].as_bool(),
        Some(true)
    );
    assert_eq!(
        identity["drain"]["metadataTagIsInformational"].as_bool(),
        Some(true)
    );

    let observability: Value = read_yaml(&contract_path("observability.yml"));
    let metrics = observability["metrics"].as_sequence().unwrap();
    let metric_names = metrics
        .iter()
        .map(|metric| metric["name"].as_str().unwrap())
        .collect::<BTreeSet<_>>();
    assert_eq!(metric_names.len(), metrics.len());
    assert!(
        metric_names
            .iter()
            .all(|name| name.starts_with("light_workflow_"))
    );
    let events = observability["events"].as_sequence().unwrap();
    assert!(events.iter().all(|event| {
        event["name"]
            .as_str()
            .is_some_and(|name| name.starts_with("workflow."))
    }));
}

#[test]
fn phase0_characterization_manifest_points_to_executable_evidence() {
    let manifest: CharacterizationManifest = read_yaml(&contract_path("characterization.yml"));
    assert_eq!(manifest.version, 1);
    let source_and_tests = production_sources()
        .into_iter()
        .map(|(_, source)| source)
        .chain(
            ["tests/postgres_runner_integration.rs"]
                .into_iter()
                .map(|path| fs::read_to_string(manifest_dir().join(path)).unwrap()),
        )
        .collect::<String>();
    let mut ids = BTreeSet::new();
    let mut layers = BTreeSet::new();
    for case in manifest.cases {
        assert!(ids.insert(case.id.clone()), "duplicate {}", case.id);
        layers.insert(case.layer);
        if !case.evidence.starts_with("phase0_source_") {
            assert!(
                source_and_tests.contains(&format!("fn {}", case.evidence)),
                "missing executable evidence {}",
                case.evidence
            );
        }
        if case.postgres_required {
            assert!(
                case.evidence.contains("restart")
                    || case.evidence.contains("fenc")
                    || case.evidence.contains("origin")
            );
        }
    }
    assert_eq!(
        layers,
        BTreeSet::from([
            "authorization".to_string(),
            "lifecycle".to_string(),
            "readiness".to_string(),
            "runnerRecovery".to_string(),
            "startup".to_string(),
        ])
    );
}

#[test]
fn phase0_source_characterizes_current_manual_lifecycle() {
    let main = fs::read_to_string(manifest_dir().join("src/main.rs")).unwrap();
    let service_runtime =
        fs::read_to_string(manifest_dir().join("src/service_runtime.rs")).unwrap();
    for marker in [
        "AxumTransport::new(app)",
        ".with_prepared_config(config_activation.runtime_config)",
        ".run_until_shutdown(watcher)",
        "light-workflow-event-consumer",
        "light-workflow-task-executor",
        "light-workflow-result-reconciler",
    ] {
        assert!(
            main.contains(marker),
            "current lifecycle marker missing: {marker}"
        );
    }
    assert!(service_runtime.contains("async fn quiesce"));
    assert!(service_runtime.contains("self.cancellation.cancel();"));
    assert!(!main.contains("axum::serve"));
    assert!(!main.contains("timeout_at(deadline, &mut tasks)"));
    assert!(main.contains("legacy_event_source_available"));
    assert!(main.contains("workflow.legacy_event_consumer.disabled"));
    assert!(main.contains("direct invocation admission remains active"));
}

#[test]
fn phase0_source_characterizes_current_readiness_surface() {
    let rule_api = fs::read_to_string(manifest_dir().join("src/rule_api.rs")).unwrap();
    for route in [
        ".route(\"/rule/test\"",
        ".route(\"/v1/workflow-invocations\"",
    ] {
        assert!(rule_api.contains(route));
    }
    for route in [".route(\"/health\"", ".route(\"/ready\""] {
        assert!(rule_api.contains(route), "readiness route missing: {route}");
    }
    assert!(rule_api.contains(".route(\"/metrics\""));
    assert!(rule_api.contains("StatusCode::SERVICE_UNAVAILABLE"));
}
