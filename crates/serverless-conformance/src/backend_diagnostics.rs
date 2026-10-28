//! The existing native closed diagnostic expectation rules.
use anyhow::{Context, Result, ensure};
use serde_json::Value;
pub(crate) fn assert(expected: &Value, actual: &Value) -> Result<()> {
    let records = actual.as_array().context("actual diagnostic records")?;
    match expected["kind"].as_str() {
        Some("rustFunctionErrorSafeMetadata") => {
            ensure!(
                records.len() == 1,
                "exactly one FunctionError diagnostic required"
            );
            let metadata: Value = serde_json::from_str(
                records[0]["message"]
                    .as_str()
                    .context("structured safe diagnostic")?,
            )?;
            ensure!(
                metadata == expected["metadata"],
                "FunctionError metadata differs: actual {metadata}, expected {}",
                expected["metadata"]
            );
        }
        Some("javaFunctionErrorTrace") => {
            let payload = format!(
                "UTF-8 Response: {}",
                expected["payloadUtf8"]
                    .as_str()
                    .context("native payload trace")?
            );
            ensure!(
                records
                    .iter()
                    .filter(|r| r["level"] == "TRACE" && r["message"] == payload)
                    .count()
                    == 1,
                "exact native payload trace required once"
            );
            if let Some(tail) = expected.get("logTailByteArray") {
                let tail = format!(
                    "Response logs: {}",
                    serde_json::to_string(tail)?.replace(',', ", ")
                );
                ensure!(
                    records
                        .iter()
                        .filter(|r| r["level"] == "TRACE" && r["message"] == tail)
                        .count()
                        == 1,
                    "exact native log tail required once"
                );
            }
        }
        _ => anyhow::bail!("unknown diagnostic contract"),
    }
    Ok(())
}
