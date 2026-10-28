//! Reviewed metadata only. Fixture bytes, expectations and execution evidence remain original.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub fn review(f: &Value) -> Result<Option<Value>> {
    let mut metadata: Value = serde_json::from_str(include_str!(
        "../../../tests/serverless-conformance/metadata/wp13-declarations-v1.json"
    ))?;
    let closure: Value = serde_json::from_str(include_str!(
        "../../../tests/serverless-conformance/metadata/wp13-declarations-v2.json"
    ))?;
    let families: Value = serde_json::from_str(include_str!(
        "../../../tests/serverless-conformance/metadata/wp13-declarations-v3.json"
    ))?;
    metadata["reviews"]
        .as_array_mut()
        .context("review records")?
        .extend(
            closure["reviews"]
                .as_array()
                .context("closure review records")?
                .iter()
                .cloned(),
        );
    metadata["reviews"]
        .as_array_mut()
        .context("review records")?
        .extend(
            families["reviews"]
                .as_array()
                .context("family review records")?
                .iter()
                .cloned(),
        );
    let bootstrap: Value = serde_json::from_str(include_str!(
        "../../../tests/serverless-conformance/metadata/wp13-declarations-v4.json"
    ))?;
    metadata["reviews"]
        .as_array_mut()
        .context("review records")?
        .extend(
            bootstrap["reviews"]
                .as_array()
                .context("bootstrap review records")?
                .iter()
                .cloned(),
        );
    let transport: Value = serde_json::from_str(include_str!(
        "../../../tests/serverless-conformance/metadata/wp13-declarations-v5.json"
    ))?;
    metadata["reviews"]
        .as_array_mut()
        .context("review records")?
        .extend(
            transport["reviews"]
                .as_array()
                .context("transport review records")?
                .iter()
                .cloned(),
        );
    let admission: Value = serde_json::from_str(include_str!(
        "../../../tests/serverless-conformance/metadata/wp13-declarations-v6.json"
    ))?;
    metadata["reviews"]
        .as_array_mut()
        .context("review records")?
        .extend(
            admission["reviews"]
                .as_array()
                .context("admission reviews")?
                .iter()
                .cloned(),
        );
    let remaining: Value = serde_json::from_str(include_str!(
        "../../../tests/serverless-conformance/metadata/wp13-declarations-v7.json"
    ))?;
    metadata["reviews"]
        .as_array_mut()
        .context("review records")?
        .extend(
            remaining["reviews"]
                .as_array()
                .context("remaining adapter reviews")?
                .iter()
                .cloned(),
        );
    let stdout: Value = serde_json::from_str(include_str!(
        "../../../tests/serverless-conformance/metadata/wp13-declarations-v8.json"
    ))?;
    metadata["reviews"]
        .as_array_mut()
        .context("review records")?
        .extend(
            stdout["reviews"]
                .as_array()
                .context("stdout reviews")?
                .iter()
                .cloned(),
        );
    let reviewed: Value = serde_json::from_str(include_str!(
        "../../../tests/serverless-conformance/metadata/wp13-declarations-v9.json"
    ))?;
    metadata["reviews"]
        .as_array_mut()
        .context("reviews")?
        .extend(
            reviewed["reviews"]
                .as_array()
                .context("reviewed root causes")?
                .iter()
                .cloned(),
        );
    let approved: Value = serde_json::from_str(include_str!(
        "../../../tests/serverless-conformance/metadata/wp13-declarations-v10.json"
    ))?;
    metadata["reviews"]
        .as_array_mut()
        .context("reviews")?
        .extend(
            approved["reviews"]
                .as_array()
                .context("approved dispositions")?
                .iter()
                .cloned(),
        );
    let matches = metadata["reviews"]
        .as_array()
        .context("review records")?
        .iter()
        .filter(|r| r["id"] == f["id"])
        .collect::<Vec<_>>();
    ensure!(matches.len() <= 1, "duplicate declaration review");
    let Some(row) = matches.first() else {
        return Ok(None);
    };
    ensure!(
        row["fixtureDigest"] == crate::fixture_digest(f)?,
        "stale declaration review digest"
    );
    ensure!(
        row["originalDeclared"] == f["differences"],
        "declaration review source changed"
    );
    let declared = f["differences"].as_array().context("source declarations")?;
    let remove = row["remove"].as_array().context("removed declarations")?;
    let mut unique = BTreeSet::new();
    for value in remove {
        let field = value.as_str().context("declaration field")?;
        ensure!(
            unique.insert(field) && declared.contains(value),
            "invalid removed declaration"
        );
        // Re-prove equality from the unchanged per-runner expectations. Recorded
        // execution alone cannot authorize changing a declared behavior contract.
        for case in crate::application::cases(f) {
            let rust = &case["expect"];
            let java = case.get("expectJava").unwrap_or(rust);
            ensure!(
                expected_field(java, field)? == expected_field(rust, field)?,
                "review does not establish equal expectations: {field}"
            );
        }
        if let (Some(rust), Some(java)) = (f.get("expectInit"), f.get("expectJavaInit")) {
            ensure!(
                expected_field(java, field)? == expected_field(rust, field)?,
                "review does not establish equal Init expectations: {field}"
            );
        }
    }
    if let Some(add) = row.get("add") {
        // This closed metadata repair is justified by original typed contracts,
        // never by matching current Rust output or by a permissive difference union.
        ensure!(
            add == &json!(["jwksFetches"]),
            "unreviewed added declaration"
        );
        ensure!(
            !declared.contains(&json!("jwksFetches")),
            "already declared bootstrap field"
        );
        if row.get("bootstrapProof").is_some() {
            validate_scoped_bootstrap(f, row)?;
        } else {
            ensure!(
                row["originalRustInit"] == f["expectInit"]
                    && row["originalJavaInit"] == f["expectJavaInit"],
                "bootstrap contract changed"
            );
            ensure!(
                f["expectInit"]["outcome"] == "initError"
                    && f["expectInit"]["code"] == "InvalidConfiguration"
                    && f["expectInit"]["jwksFetches"] == 0
                    && f["expectJavaInit"]["outcome"] == "initSuccess"
                    && f["expectJavaInit"]["jwksFetches"] == 1,
                "bootstrap difference not established by original expectations"
            );
        }
    }
    Ok(Some((*row).clone()))
}

fn expected_field(e: &Value, field: &str) -> Result<Value> {
    if field == "responses" {
        if e.get("responses").is_none() {
            return Ok(Value::Null);
        }
        let mut responses = e["responses"]
            .as_array()
            .context("parallel response expectations")?
            .clone();
        for response in &mut responses {
            let mut headers = BTreeMap::<String, Vec<Value>>::new();
            for pair in response["headers"].as_array().context("response headers")? {
                headers
                    .entry(
                        pair[0]
                            .as_str()
                            .context("header name")?
                            .to_ascii_lowercase(),
                    )
                    .or_default()
                    .push(pair[1].clone());
            }
            response["headers"] = json!(headers);
        }
        return Ok(json!(responses));
    }
    if let Some(name) = field.strip_prefix("header:") {
        // Init errors are a distinct, non-ALB observation: no response envelope
        // or HTTP headers exists. Missing headers on a response remain invalid.
        if ["initError", "initSuccess", "exception", "notExecuted"]
            .contains(&e["outcome"].as_str().unwrap_or(""))
        {
            ensure!(
                e.get("headers").is_none(),
                "Init expectation cannot carry response headers"
            );
            return Ok(Value::Null);
        }
        let mut headers = BTreeMap::<String, Vec<Value>>::new();
        for pair in e["headers"].as_array().context("expected headers")? {
            headers
                .entry(
                    pair[0]
                        .as_str()
                        .context("header name")?
                        .to_ascii_lowercase(),
                )
                .or_default()
                .push(pair[1].clone());
        }
        return Ok(json!(headers.get(name)));
    }
    ensure!(
        [
            "outcome",
            "status",
            "body",
            "bodyBase64",
            "backendCalls",
            "jwksFetches",
            "httpCalls",
            "auditRecords",
            "cacheActions"
        ]
        .contains(&field),
        "unreviewed metadata field"
    );
    Ok(e.get(field).cloned().unwrap_or(Value::Null))
}

pub fn compare(f: &Value, java: &Value, rust: &Value) -> Result<()> {
    let Some(row) = review(f)? else {
        return crate::incremental::compare(f, java, rust);
    };
    let mut effective = f.clone();
    if row.get("bootstrapProof").is_some() {
        crate::incremental::assert_observation(
            &row["bootstrapProof"]["javaInitialization"],
            &java["initialization"],
        )?;
        crate::incremental::assert_observation(&f["expectRustInit"], &rust["initialization"])?;
    }
    let removed = row["remove"].as_array().context("removed declarations")?;
    effective["differences"] = json!(
        f["differences"]
            .as_array()
            .context("declarations")?
            .iter()
            .filter(|v| !removed.contains(v))
            .collect::<Vec<_>>()
    );
    if let Some(add) = row["add"].as_array() {
        effective["differences"]
            .as_array_mut()
            .context("effective declarations")?
            .extend(add.iter().cloned());
    }
    // The exact original Java/Rust expectations and all observed differences
    // still go through the same strict comparator. Only metadata differs.
    crate::incremental::compare(&effective, java, rust)
}

// Existing admission/bootstrap contracts, not an observation-driven waiver.
// The original fixture omits an explicit Java Init assertion; this sealed
// proof adds that check without modifying its original expectations.
fn validate_scoped_bootstrap(f: &Value, row: &Value) -> Result<()> {
    ensure!(
        f["id"] == "S-17-controlled-cached-nbf-mutation-v1",
        "unreviewed implicit Init contract"
    );
    ensure!(
        f.get("expectInit").is_none() && f.get("expectJavaInit").is_none(),
        "implicit bootstrap contract changed"
    );
    ensure!(
        row["originalRustInit"] == f["expectRustInit"]
            && f["expectRustInit"]
                == json!({"outcome":"initError","code":"InvalidConfiguration","backendCalls":[],"jwksFetches":0}),
        "Rust admission contract changed"
    );
    ensure!(
        f["config"]["files"]["security"]["bootstrapFromKeyService"] == true
            && f["config"]["files"]["client"]["oauth"]["multipleAuthServers"] == false
            && f["config"]["files"]["client"]["oauth"]["token"]["key"]["server_url"]
                == "${dependency:jwks}"
            && f["config"]["files"]["client"]["oauth"]["token"]["key"]["uri"] == "/jwks"
            && f["dependencies"]["jwks"]["publishedKeys"] == json!(["k1"]),
        "review requires the approved single-bootstrap prerequisite"
    );
    ensure!(
        row["bootstrapProof"]["javaInitialization"]
            == json!({"outcome":"initSuccess","backendCalls":[],"jwksFetches":1}),
        "unreviewed Java bootstrap contract"
    );
    Ok(())
}

#[cfg(test)]
mod root_cause_tests {
    use super::*;
    #[test]
    fn response_expectation_equality_uses_header_case_rules_but_preserves_values() {
        let a = json!({"responses":[{"status":401,"body":"denied","headers":[["Content-Type","application/json"]]}]});
        let mut b = a.clone();
        b["responses"][0]["headers"][0][0] = json!("content-type");
        assert_eq!(
            expected_field(&a, "responses").unwrap(),
            expected_field(&b, "responses").unwrap()
        );
        b["responses"][0]["headers"][0][1] = json!("text/plain");
        assert_ne!(
            expected_field(&a, "responses").unwrap(),
            expected_field(&b, "responses").unwrap()
        );
        b = a.clone();
        b["responses"][0]["status"] = json!(403);
        assert_ne!(
            expected_field(&a, "responses").unwrap(),
            expected_field(&b, "responses").unwrap()
        );
    }
}
