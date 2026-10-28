//! Closed approved response-cap and selected-audit-string contracts.
use anyhow::{Result, ensure};
use serde_json::Value;

pub fn validate(f: &Value) -> Result<()> {
    if let Some(cap) = f.pointer("/config/values/light-lambda.maxResponseBytes") {
        let cap = cap
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("response cap must be a nonnegative integer"))?;
        if cap < 4096 {
            ensure!(
                f["expect"]["outcome"] == "initError"
                    && f["expect"]["code"] == "InvalidConfiguration",
                "response cap below 4096 must fail Init"
            );
        }
    }
    let steps = f
        .get("steps")
        .and_then(Value::as_array)
        .map_or_else(|| vec![f], |steps| steps.iter().collect());
    for step in steps {
        if f["javaOnly"] == true {
            continue;
        }
        for record in step["expect"]["auditRecords"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if let Some(data) = record["data"].as_object() {
                for (name, expected) in data {
                    if let Some(value) = expected.as_str() {
                        ensure!(
                            value.len() <= 1024,
                            "selected Rust audit string exceeds approved UTF-8 bound"
                        );
                    }
                    if let Some(source) = step["rawEvent"]["headers"][name].as_str() {
                        let bounded = if source.len() > 1024 {
                            "<truncated>"
                        } else {
                            source
                        };
                        ensure!(
                            expected == bounded,
                            "selected audit header must obey exact approved byte-bound rule"
                        );
                        if let Some(java) = step["expectJava"]["auditRecords"].as_array() {
                            ensure!(
                                java.iter().any(|record| record["data"][name] == source),
                                "Java selected audit source must remain unmodified"
                            );
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn minimum_cap_requires_an_init_error() {
        let mut f = json!({"config":{"values":{"light-lambda.maxResponseBytes":4095}},"expect":{"outcome":"response"}});
        assert!(validate(&f).is_err());
        f["expect"] = json!({"outcome":"initError","code":"InvalidConfiguration"});
        validate(&f).unwrap();
        f["config"]["values"]["light-lambda.maxResponseBytes"] = json!(4096);
        f["expect"] = json!({"outcome":"initSuccess"});
        validate(&f).unwrap();
    }

    #[test]
    fn utf8_bound_rejects_masks_and_preserves_java_source() {
        for (source, expected) in [
            ("a".repeat(1024), "a".repeat(1024)),
            ("a".repeat(1025), "<truncated>".into()),
            ("é".repeat(512), "é".repeat(512)),
            ("é".repeat(513), "<truncated>".into()),
        ] {
            let mut f = json!({"rawEvent":{"headers":{"x-test":source}},"expect":{"auditRecords":[{"data":{"x-test":expected}}]},"expectJava":{"auditRecords":[{"data":{"x-test":source}}]}});
            validate(&f).unwrap();
            f["expect"]["auditRecords"][0]["data"]["x-test"] = json!("wrong");
            assert!(validate(&f).is_err());
            f["expect"]["auditRecords"][0]["data"]["x-test"] = json!(expected);
            f["expectJava"]["auditRecords"][0]["data"]["x-test"] = json!("<truncated>");
            assert!(validate(&f).is_err());
        }
    }
}
