//! Process isolation for the existing closed allow-v1 test-configuration recipe.
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::path::PathBuf;

pub(crate) async fn run(f: &Value, revision: &str) -> Result<Value> {
    crate::test_config_flag::validate(f)?;
    ensure!(
        f["insecureTestConfigPrerequisite"] == "allow-v1",
        "closed test configuration recipe"
    );
    let dir = tempfile::tempdir()?;
    let input = dir.path().join("fixture.json");
    let output = dir.path().join("result.json");
    std::fs::write(&input, serde_json::to_vec(f)?)?;
    let current = std::env::current_exe()?;
    let binary = if current
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n == "serverless-conformance" || n == "serverless-conformance (deleted)")
    {
        // Linux may retain an executing binary after Cargo atomically replaces
        // its path. Resolve the current sibling worker, not a nonexistent
        // "(deleted)" path or the integration-test deps parent.
        current
            .parent()
            .context("runner executable directory")?
            .join("serverless-conformance")
    } else {
        current
            .parent()
            .and_then(|p| p.parent())
            .context("test executable location")?
            .join("serverless-conformance")
    };
    ensure!(
        binary.is_file(),
        "build serverless-conformance binary before isolated fixture execution"
    );
    let status = tokio::process::Command::new(binary)
        .args(["__test-config-worker"])
        .arg(&input)
        .arg(&output)
        .arg(revision)
        .env("LIGHT_LAMBDA_ALLOW_INSECURE_TEST_CONFIG", "1")
        .env("LIGHT_LAMBDA_CONFORMANCE_TEST_WORKER", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .status()
        .await?;
    ensure!(
        status.success(),
        "isolated test-config worker failed: {status}"
    );
    crate::load(&output)
}
pub(crate) fn command(args: &[String]) -> Result<()> {
    ensure!(args.len() == 4, "private test-config worker arguments");
    ensure!(
        std::env::var("LIGHT_LAMBDA_CONFORMANCE_TEST_WORKER").as_deref() == Ok("1")
            && std::env::var("LIGHT_LAMBDA_ALLOW_INSECURE_TEST_CONFIG").as_deref() == Ok("1"),
        "worker requires scoped process flags"
    );
    let f = crate::load(&PathBuf::from(&args[1]))?;
    crate::test_config_flag::validate(&f)?;
    ensure!(
        f["insecureTestConfigPrerequisite"] == "allow-v1",
        "worker cannot activate an undeclared flag"
    );
    let result = tokio::runtime::Runtime::new()?.block_on(crate::application::run(&f, &args[3]))?;
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&args[2])?;
    serde_json::to_writer(&mut out, &result)?;
    Ok(())
}
