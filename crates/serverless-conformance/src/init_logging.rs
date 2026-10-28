use anyhow::{Result, ensure};
use serde_json::{Value, json};

pub(crate) fn validate(f: &Value) -> Result<()> {
    if f.get("initLoggingProbe").is_none() && f["component"] != "rustInitLoggingCfg01" {
        return Ok(());
    }
    ensure!(
        f["component"] == "rustInitLoggingCfg01" && f["ledger"] == json!(["CFG-01"]),
        "Init logging is a closed CFG-01 component"
    );
    ensure!(
        f["rustOnly"] == true && f["javaOnly"] == false,
        "Init logging has no meaningful Java counterpart"
    );
    ensure!(
        f["config"] == json!({"values":{},"files":{}}),
        "logging component cannot introduce production configuration"
    );
    let levels = match f["initLoggingProbe"]["mode"].as_str() {
        Some("default-info-v1") => vec!["INFO", "WARN", "ERROR"],
        Some("configured-debug-v1") => vec!["DEBUG", "INFO", "WARN", "ERROR"],
        Some("configured-error-v1") => vec!["ERROR"],
        _ => anyhow::bail!("closed logging mode"),
    };
    let records: Vec<Value> = levels.into_iter().map(|level| json!({"level":level,"target":"light_lambda::init","fields":{"message":format!("test-only-{}",level.to_lowercase())}})).collect();
    ensure!(
        f["expect"]
            == json!({"outcome":"initLogging","stdoutContract":{"format":"json-lines-v1","recordKeys":["timestamp","level","target","fields"],"timestamp":"rfc3339-process-window-v1","records":records,"forbiddenSubstrings":["logback","/opt/logback.xml","test-only-secret"]},"stderr":"","fileWrites":[]}),
        "exact logging shape/filter/order and bounded timestamp contract"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Value {
        json!({"component":"rustInitLoggingCfg01","ledger":["CFG-01"],"rustOnly":true,"javaOnly":false,"config":{"values":{},"files":{}},"initLoggingProbe":{"mode":"configured-error-v1"},"expect":{"outcome":"initLogging","stdoutContract":{"format":"json-lines-v1","recordKeys":["timestamp","level","target","fields"],"timestamp":"rfc3339-process-window-v1","records":[{"level":"ERROR","target":"light_lambda::init","fields":{"message":"test-only-error"}}],"forbiddenSubstrings":["logback","/opt/logback.xml","test-only-secret"]},"stderr":"","fileWrites":[]}})
    }
    #[test]
    fn closed_logging_positive() {
        validate(&fixture()).unwrap();
    }
    #[test]
    fn rejects_filter_shape_and_scope_changes() {
        for pointer in [
            "/expect/stdoutContract/records/0/level",
            "/expect/stdoutContract/timestamp",
            "/expect/stderr",
            "/component",
            "/initLoggingProbe/mode",
        ] {
            let mut f = fixture();
            *f.pointer_mut(pointer).unwrap() = json!("invalid");
            assert!(validate(&f).is_err(), "{pointer}");
        }
        let mut f = fixture();
        f["rustOnly"] = json!(false);
        assert!(validate(&f).is_err());
        let mut f = fixture();
        f["config"]["files"]["logback"] = json!({});
        assert!(validate(&f).is_err());
    }
}
