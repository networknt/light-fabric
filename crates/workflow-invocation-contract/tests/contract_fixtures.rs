use chrono::{TimeZone, Utc};
use jsonschema::Validator;
use serde::Deserialize;
use serde_json::Value;
use std::{fs, path::PathBuf};
use workflow_invocation_contract::{
    StartInvocationRequest, WorkflowMcpResult, WorkflowToolBindingPublishedEvent,
    canonical_json_bytes, canonical_sha256, parse_strict_json,
};

fn contract_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../contracts/workflow-invocation/v1")
}

#[test]
fn publication_fixture_matches_schema_and_runtime_contract() {
    let root = contract_root();
    let schema: Value =
        serde_json::from_slice(&fs::read(root.join("publication-event.schema.json")).unwrap())
            .unwrap();
    let validator = Validator::new(&schema).unwrap();
    let fixture: Value = serde_json::from_slice(
        &fs::read(root.join("fixtures/valid/publication-event.json")).unwrap(),
    )
    .unwrap();
    assert!(validator.is_valid(&fixture));
    let event: WorkflowToolBindingPublishedEvent = serde_json::from_value(fixture).unwrap();
    event.binding.validate().unwrap();
    for dependency in event.dependencies {
        dependency.validate().unwrap();
        assert_eq!(dependency.host_id, event.binding.host_id);
        assert_eq!(dependency.outer_binding_id, event.binding.binding_id);
    }
}

#[test]
fn qualification_manifest_is_fail_closed_and_numeric() {
    let manifest: Value = serde_json::from_slice(
        &fs::read(contract_root().join("qualification-manifest.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["promotionAllowed"], false);
    assert_eq!(
        manifest["celDecision"]["currentCel014QualifiedForValueEvaluation"],
        false
    );
    for profile in [
        "durableAcceptance",
        "controlledOneTaskResult",
        "controlledMaximumTaskResult",
    ] {
        let threshold = &manifest["latencyThresholdsMs"][profile];
        assert!(threshold["p95"].as_u64().is_some_and(|value| value > 0));
        assert!(threshold["p99"].as_u64().is_some_and(|value| value > 0));
    }
}

#[test]
fn mcp_rendering_and_cel_value_fixtures_are_pinned() {
    let root = contract_root();
    let schema: Value =
        serde_json::from_slice(&fs::read(root.join("mcp-result.schema.json")).unwrap()).unwrap();
    let validator = Validator::new(&schema).unwrap();
    for name in [
        "mcp-result-compact.json",
        "mcp-result-summary.json",
        "mcp-result-error.json",
    ] {
        let fixture: Value =
            serde_json::from_slice(&fs::read(root.join("fixtures/valid").join(name)).unwrap())
                .unwrap();
        assert!(validator.is_valid(&fixture), "{name}");
        serde_json::from_value::<WorkflowMcpResult>(fixture)
            .unwrap()
            .validate()
            .unwrap();
    }

    let cel: Value =
        serde_json::from_slice(&fs::read(root.join("fixtures/cel-value-v1.json")).unwrap())
            .unwrap();
    let names = cel["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|case| case["name"].as_str())
        .collect::<std::collections::HashSet<_>>();
    for required in [
        "large-integer",
        "finite-double",
        "nan",
        "infinity",
        "timestamp",
        "duration",
        "bytes",
        "null",
        "missing",
        "non-string-map-key",
        "opaque",
        "wrong-root-type",
        "checker-error",
        "cost-exhaustion",
    ] {
        assert!(names.contains(required), "missing CEL fixture {required}");
    }
}

#[test]
fn start_fixtures_match_schema_and_runtime_contract() {
    let root = contract_root();
    let schema: Value = serde_json::from_slice(
        &fs::read(root.join("start-request.schema.json")).expect("schema exists"),
    )
    .unwrap();
    let validator = Validator::new(&schema).unwrap();

    let valid: Value = serde_json::from_slice(
        &fs::read(root.join("fixtures/valid/start-sync.json")).expect("valid fixture exists"),
    )
    .unwrap();
    assert!(validator.is_valid(&valid));
    let request: StartInvocationRequest = serde_json::from_value(valid).unwrap();
    assert!(
        request
            .validate(Utc.with_ymd_and_hms(2099, 8, 12, 19, 59, 0).unwrap())
            .is_ok()
    );

    let invalid: Value = serde_json::from_slice(
        &fs::read(root.join("fixtures/invalid/start-unknown-field.json"))
            .expect("invalid fixture exists"),
    )
    .unwrap();
    assert!(!validator.is_valid(&invalid));
    assert!(serde_json::from_value::<StartInvocationRequest>(invalid).is_err());
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CanonicalVectors {
    vectors: Vec<CanonicalVector>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CanonicalVector {
    name: String,
    inputs: Vec<String>,
    canonical: Option<String>,
    digest: Option<String>,
    different_digests: Option<bool>,
    error: Option<String>,
}

#[test]
fn canonical_vectors_are_pinned() {
    let vectors: CanonicalVectors = serde_json::from_slice(
        &fs::read(contract_root().join("fixtures/canonical-input-v1.json")).unwrap(),
    )
    .unwrap();

    for vector in vectors.vectors {
        let parsed = vector
            .inputs
            .iter()
            .map(|input| parse_strict_json(input))
            .collect::<Vec<_>>();
        if vector.error.is_some() {
            assert!(parsed.iter().all(Result::is_err), "{}", vector.name);
            continue;
        }
        let parsed = parsed.into_iter().collect::<Result<Vec<_>, _>>().unwrap();
        if let Some(expected) = vector.canonical {
            assert_eq!(
                String::from_utf8(canonical_json_bytes(&parsed[0]).unwrap()).unwrap(),
                expected,
                "{}",
                vector.name
            );
        }
        if let Some(expected) = vector.digest {
            assert_eq!(
                canonical_sha256(&parsed[0]).unwrap(),
                expected,
                "{}",
                vector.name
            );
        }
        let digests = parsed
            .iter()
            .map(canonical_sha256)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        if vector.different_digests.unwrap_or(false) {
            assert_ne!(digests[0], digests[1], "{}", vector.name);
        } else {
            assert!(
                digests.windows(2).all(|pair| pair[0] == pair[1]),
                "{}",
                vector.name
            );
        }
    }
}
