use anyhow::{Result, ensure};
use serde_json::Value;

pub(crate) fn validate(f: &Value) -> Result<()> {
    if let Some(mode) = f.get("insecureTestConfigPrerequisite") {
        ensure!(mode == "allow-v1", "unknown test-config flag prerequisite");
        ensure!(
            f["ledger"]
                .as_array()
                .is_some_and(|rows| rows.iter().any(|r| r == "S-14" || r == "S-17")),
            "closed test flag requires S-14/S-17"
        );
        ensure!(
            f["config"]["files"]["security"]["ignoreJwtExpiry"] == true
                || f["config"]["files"]["security"]["enableMockJwt"] == true
                || f["config"]["files"]["security"]["logJwtToken"] == true,
            "test-config flag must exercise its approved exception"
        );
    }
    Ok(())
}
