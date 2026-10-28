//! Digest-bound reconciliation of the preserved 51-fixture gap cohort.
//! This metadata never admits execution, changes expectations or waives a failure.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub fn review(f: &Value) -> Result<Option<Value>> {
    let metadata: Value = serde_json::from_str(include_str!(
        "../../../tests/serverless-conformance/metadata/wp13-gap-work-v1.json"
    ))?;
    let matches = metadata["rows"]
        .as_array()
        .context("gap cohort")?
        .iter()
        .filter(|r| r["id"] == f["id"])
        .collect::<Vec<_>>();
    ensure!(matches.len() <= 1, "duplicate gap classification");
    let Some(row) = matches.first() else {
        return Ok(None);
    };
    ensure!(
        row["fixtureDigest"] == crate::fixture_digest(f)?,
        "stale gap classification for {}",
        f["id"]
    );
    ensure!(
        [
            "adapter-only",
            "router-failed-state",
            "router-rewriting",
            "validator-integration",
            "explicit-deferral"
        ]
        .contains(&row["category"].as_str().unwrap_or("")),
        "unknown work category"
    );
    Ok(Some((*row).clone()))
}

pub(crate) fn annotate(report: &mut Value, inventory: &[(String, Value)]) -> Result<()> {
    let fixtures = inventory
        .iter()
        .map(|(id, f)| (id.as_str(), f))
        .collect::<BTreeMap<_, _>>();
    let mut reconciled = vec![];
    let mut categories = BTreeMap::<String, usize>::new();
    let mut outstanding = BTreeMap::<String, usize>::new();
    let mut service = 0;
    let mut admission = 0;
    let mut component_counts = BTreeMap::<String, usize>::new();
    let mut components = vec![];
    let mut counterparts = 0;
    for row in report["rows"].as_array().context("coverage rows")? {
        let f = fixtures
            .get(row["id"].as_str().context("coverage id")?)
            .context("coverage fixture")?;
        if let Some(mut work) = review(f)? {
            let category = work["category"].as_str().unwrap().to_owned();
            *categories.entry(category.clone()).or_default() += 1;
            if row["runnerSupport"] == "missing" {
                *outstanding.entry(category.clone()).or_default() += 1;
            }
            work["runnerSupport"] = row["runnerSupport"].clone();
            work["selected"] = row["selected"].clone();
            work["status"] = json!(if category == "adapter-only"
                && row["runnerSupport"] == "implemented"
            {
                "completed-java-only-reporting"
            } else if category == "explicit-deferral" {
                "explicitly-excluded-or-deferred"
            } else {
                "production-work-open"
            });
            reconciled.push(work);
        }
        if row["selected"] != true
            || row["handlerProfileGaps"]
                .as_array()
                .is_some_and(|a| !a.is_empty())
        {
            continue;
        }
        if row["rustExecution"] == "skipped"
            && row["coverageKind"] == "single-runner-java-component"
        {
            counterparts += 1;
        }
        if row["rustExecution"] != "passed" {
            continue;
        }
        match row["coverageKind"].as_str() {
            Some("service-execution") => service += 1,
            Some("admission-only") => admission += 1,
            Some("paired-handler-component") => {
                *component_counts
                    .entry("factory-built-handler".into())
                    .or_default() += 1;
                components.push(json!({"id":f["id"],"category":"factory-built-handler"}));
            }
            Some("single-runner-rust-component") => {
                let kind = if crate::logging_component::supported(f) {
                    "logging"
                } else if crate::gate_component::supported(f) {
                    "jwks-gate"
                } else if crate::panic_component::supported(f)
                    || crate::reporting_component::supported(f)
                {
                    "init-failure-reporting"
                } else {
                    anyhow::bail!("unclassified Rust component")
                };
                *component_counts.entry(kind.into()).or_default() += 1;
                components.push(json!({"id":f["id"],"category":kind}));
            }
            _ => anyhow::bail!("unclassified passed Rust execution {}", f["id"]),
        }
    }
    let component_total: usize = component_counts.values().sum();
    let total = service + admission + component_total;
    if report["action"] == "run" {
        ensure!(
            report["expectationPasses"] == total,
            "Rust expectation category totals disagree"
        );
    }
    report["rustExecutionCategories"] = json!({"service":service,"admission":admission,"components":component_counts,"componentTotal":component_total,"expectationTotal":total,"javaOnlyCounterpartSkips":counterparts,"meaning":"Components are separate from service/admission; paired components contribute to differential passes, Rust-only components and counterpart skips use single-runner reporting"});
    report["rustComponentExecutionInventory"] = json!(components);
    report["reconciledGapCohort"] = json!({"sourceCount":51,"present":reconciled.len(),"categories":categories,"remainingAdapterGapsByCategory":outstanding,"rows":reconciled,"meaning":"one primary category per preserved fixture; secondary categories expose overlaps; classification never changes runner support or expectations"});
    Ok(())
}
