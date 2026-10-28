//! Owner-approved Rust expectations, distinct from unchanged native fixtures.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub fn review(f: &Value) -> Result<Option<Value>> {
    let mut matched = None;
    for (metadata, source) in [
        (
            include_str!(
                "../../../tests/serverless-conformance/metadata/wp13-rust-expectations-v1.json"
            ),
            include_str!(
                "../../../tests/serverless-conformance/metadata/wp13-rust-expectations-proposal-v1.json"
            ),
        ),
        (
            include_str!(
                "../../../tests/serverless-conformance/metadata/wp13-rust-expectations-v2.json"
            ),
            include_str!(
                "../../../tests/serverless-conformance/metadata/wp13-rust-expectations-proposal-v2.json"
            ),
        ),
        (
            include_str!(
                "../../../tests/serverless-conformance/metadata/wp13-rust-expectations-v3.json"
            ),
            include_str!(
                "../../../tests/serverless-conformance/metadata/wp13-rust-expectations-proposal-v3.json"
            ),
        ),
        (
            include_str!(
                "../../../tests/serverless-conformance/metadata/wp13-rust-expectations-v4.json"
            ),
            include_str!(
                "../../../tests/serverless-conformance/metadata/wp13-rust-expectations-proposal-v4.json"
            ),
        ),
        (
            include_str!(
                "../../../tests/serverless-conformance/metadata/wp13-rust-expectations-v5.json"
            ),
            include_str!(
                "../../../tests/serverless-conformance/metadata/wp13-rust-expectations-proposal-v5.json"
            ),
        ),
    ] {
        if let Some(row) = review_version(f, metadata, source)? {
            ensure!(matched.is_none(), "duplicate versioned Rust amendment");
            matched = Some(row);
        }
    }
    Ok(matched)
}

fn review_version(f: &Value, metadata: &str, source: &str) -> Result<Option<Value>> {
    let metadata: Value = serde_json::from_str(metadata)?;
    ensure!(
        metadata["sourcePackageSha256"] == hex::encode(Sha256::digest(source.as_bytes())),
        "approved package hash changed"
    );
    let proposed: Value = serde_json::from_str(source)?;
    let matches = metadata["proposals"]
        .as_array()
        .context("amendments")?
        .iter()
        .filter(|r| r["id"] == f["id"])
        .collect::<Vec<_>>();
    ensure!(matches.len() <= 1, "duplicate Rust amendment");
    let Some(row) = matches.first() else {
        return Ok(None);
    };
    let approved = proposed["proposals"]
        .as_array()
        .context("approved proposals")?
        .iter()
        .find(|r| r["id"] == row["id"])
        .context("unapproved amendment")?;
    for key in [
        "originalFixtureDigest",
        "originalExpected",
        "proposedVersionedRustExpectation",
    ] {
        ensure!(
            row[key] == approved[key],
            "approved amendment changed: {key}"
        );
    }
    for key in ["originalRustInit", "proposedRustInit"] {
        ensure!(
            row.get(key) == approved.get(key),
            "approved Init amendment changed: {key}"
        );
    }
    if row.get("proposedRustInit").is_some() {
        ensure!(
            row["originalRustInit"] == f["expectInit"],
            "Init amendment source changed"
        );
    }
    ensure!(
        row["originalFixtureDigest"] == crate::fixture_digest(f)?,
        "stale Rust amendment digest for {}: expected {}, actual {}",
        f["id"],
        row["originalFixtureDigest"],
        crate::fixture_digest(f)?
    );
    let original = json!(
        crate::application::cases(f)
            .iter()
            .map(|c| c["expect"].clone())
            .collect::<Vec<_>>()
    );
    ensure!(
        original == row["originalExpected"],
        "Rust amendment source expectations changed"
    );
    ensure!(
        row["approvedExpectationDifferenceFields"] == expected_difference_fields(f, row)?,
        "amendment difference fields not established by approved expectations"
    );
    Ok(Some((*row).clone()))
}

// This is contract projection, not an observation-based waiver. Re-prove every
// amendment's difference fields from the preserved Java and exact approved Rust
// expectations before comparison. Runtime observations cannot alter this set.
fn expected_difference_fields(f: &Value, row: &Value) -> Result<Value> {
    let replacements = row["proposedVersionedRustExpectation"]
        .as_array()
        .cloned()
        .unwrap_or_else(|| vec![row["proposedVersionedRustExpectation"].clone()]);
    let cases = crate::application::cases(f);
    ensure!(cases.len() == replacements.len(), "amendment case count");
    let mut fields = BTreeSet::new();
    for (case, rust) in cases.iter().zip(&replacements) {
        let java = case.get("expectJava").unwrap_or(&case["expect"]);
        for key in [
            "outcome",
            "status",
            "body",
            "bodyBase64",
            "jwksFetches",
            "backendCalls",
            "httpCalls",
            "backendDiagnostics",
            "completionProbeResult",
        ] {
            let empty = json!([]);
            let default = if key.ends_with("Calls") {
                &empty
            } else {
                &Value::Null
            };
            if java.get(key).unwrap_or(default) != rust.get(key).unwrap_or(default) {
                fields.insert(key.to_owned());
            }
        }
        let headers = |e: &Value| -> Result<BTreeMap<String, Vec<Value>>> {
            let mut map = BTreeMap::<String, Vec<Value>>::new();
            for pair in e["headers"].as_array().into_iter().flatten() {
                map.entry(
                    pair[0]
                        .as_str()
                        .context("expectation header")?
                        .to_ascii_lowercase(),
                )
                .or_default()
                .push(pair[1].clone());
            }
            Ok(map)
        };
        let jh = headers(java)?;
        let rh = headers(rust)?;
        for key in jh.keys().chain(rh.keys()) {
            if jh.get(key) != rh.get(key) {
                fields.insert(format!("header:{key}"));
            }
        }
    }
    if let Some(rust) = row.get("proposedRustInit") {
        let java = f
            .get("expectJavaInit")
            .or_else(|| f.get("expectInit"))
            .context("original Java Init contract")?;
        for key in ["outcome", "backendCalls", "jwksFetches"] {
            if java.get(key) != rust.get(key) {
                fields.insert(key.to_owned());
            }
        }
    }
    Ok(json!(fields))
}

pub fn effective(f: &Value) -> Result<Value> {
    let Some(row) = review(f)? else {
        return Ok(f.clone());
    };
    let replacement = &row["proposedVersionedRustExpectation"];
    let mut effective = f.clone();
    if let Some(initialization) = row.get("proposedRustInit") {
        effective["expectJavaInit"] = f.get("expectJavaInit").unwrap_or(&f["expectInit"]).clone();
        effective["expectInit"] = initialization.clone();
    }
    if replacement.is_object() {
        ensure!(
            crate::application::cases(f).len() == 1,
            "Init amendment must have one source case"
        );
        effective["expect"] = replacement.clone();
    }
    if let Some(steps) = effective["steps"].as_array_mut() {
        let replacements = replacement
            .as_array()
            .cloned()
            .unwrap_or_else(|| vec![replacement.clone()]);
        ensure!(
            steps.len() == replacements.len(),
            "warm amendment count changed"
        );
        for (step, expected) in steps.iter_mut().zip(replacements) {
            step["expectJava"] = step.get("expectJava").unwrap_or(&step["expect"]).clone();
            step["expect"] = expected.clone();
        }
    } else {
        effective["expectJava"] = f.get("expectJava").unwrap_or(&f["expect"]).clone();
        effective["expect"] = if replacement.is_array() {
            ensure!(
                replacement.as_array().unwrap().len() == 1,
                "single amendment count"
            );
            replacement[0].clone()
        } else {
            replacement.clone()
        };
    }
    Ok(effective)
}

pub async fn run(f: &Value, revision: &str) -> Result<Value> {
    let effective = effective(f)?;
    let mut result = crate::application::run(&effective, revision).await?;
    // Result-v1 stays unchanged; sidecar coverage carries amendment provenance.
    result["fixtureDigest"] = json!(crate::fixture_digest(f)?);
    Ok(result)
}

pub fn compare(f: &Value, java: &Value, rust: &Value, original_declarations: bool) -> Result<()> {
    let mut effective = effective(f)?;
    if !original_declarations && let Some(review) = crate::declarations::review(f)? {
        effective["differences"] = json!(
            f["differences"]
                .as_array()
                .context("declarations")?
                .iter()
                .filter(|v| !review["remove"].as_array().unwrap().contains(v))
                .collect::<Vec<_>>()
        );
    }
    if !original_declarations
        && let Some(declaration) = crate::declarations::review(f)?
        && let Some(add) = declaration["add"].as_array()
    {
        effective["differences"]
            .as_array_mut()
            .context("effective declarations")?
            .extend(add.iter().cloned());
    }
    if !original_declarations && let Some(row) = review(f)? {
        effective["differences"] = row["approvedExpectationDifferenceFields"].clone();
    }
    // The active contract is an explicit, versioned projection of the approved
    // expectation pair. Original declarations remain in the fixture and report.
    crate::incremental::compare(&effective, java, rust)
}

pub fn coverage(result: &Value) -> &'static str {
    let observations = result["observations"].as_array();
    if observations.is_some_and(|a| {
        !a.is_empty()
            && a.iter().all(|o| {
                o["outcome"] == "initError"
                    || o["outcome"] == "initSuccess"
                    || (o["outcome"] == "notExecuted"
                        && result["initialization"]["outcome"] == "initError")
                    || (o["backendCalls"].as_array().is_none_or(Vec::is_empty)
                        && o["httpCalls"].as_array().is_none_or(Vec::is_empty)
                        && o["body"].as_str().is_some_and(|b| {
                            serde_json::from_str::<Value>(b).is_ok_and(|v| {
                                matches!(v["code"].as_str(), Some("ERR14010" | "ERR14011"))
                            })
                        }))
            })
    }) {
        "admission-only"
    } else {
        "service-execution"
    }
}
