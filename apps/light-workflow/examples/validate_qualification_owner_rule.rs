//! Exercise the exact qualification CEL rule; never changes configuration.
use light_rule::{ActionRegistry, RuleEngine};
use serde_json::{Value, json};
use std::sync::Arc;

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let path = std::env::args().nth(1).ok_or("rule path required")?;
    let rule: Value = serde_json::from_slice(&std::fs::read(path)?)?;
    let engine = RuleEngine::new(Arc::new(ActionRegistry::new()));
    let host = rule["hostId"].as_str().ok_or("host required")?;
    let owner = rule["author"].as_str().ok_or("owner required")?;
    for (label, claims, expected) in [
        ("owner", json!({"uid":owner,"host":host}), true),
        (
            "other user",
            json!({"uid":uuid::Uuid::now_v7(),"host":host}),
            false,
        ),
        (
            "other Host",
            json!({"uid":owner,"host":uuid::Uuid::now_v7()}),
            false,
        ),
        ("no user", json!({"host":host}), false),
        ("no Host", json!({"uid":owner}), false),
        ("no claims", json!({}), false),
    ] {
        let actual = engine.evaluate_cel_predicate(
            rule["ruleId"].as_str().ok_or("rule ID required")?,
            rule["expression"].as_str().ok_or("expression required")?,
            Some("strict"),
            "req-acc",
            &json!({"auditInfo":{"subject_claims":{"ClaimsMap":claims}}}),
        )?;
        if actual != expected {
            return Err(format!("owner-rule gate failed: {label}").into());
        }
    }
    println!(
        "Owner rule passed six CEL cases: owner allowed; other user, other Host and missing claims denied."
    );
    Ok(())
}
