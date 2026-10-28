//! Closed private-hook components: real app Init, real security loader/verifier,
//! owned JWKS wire I/O and existing cfg(test) gate hooks. No gate reimplementation.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{process::Command, time::Instant};
pub(crate) fn supported(f: &Value) -> bool {
    matches!(
        f["component"].as_str(),
        Some("rustJwksGateCancellationS13a" | "rustJwksGatePanicS13c")
    ) && f["rustOnly"] == true
}
pub(crate) fn validate(f: &Value) -> Result<()> {
    crate::jwks_gate_cancellation::validate(f)?;
    if f.get("expectJava").is_some() && f["component"] == "rustJwksGatePanicS13c" {
        let mut original = f.clone();
        let expected = original
            .as_object_mut()
            .unwrap()
            .remove("expectJava")
            .unwrap();
        original["expect"] = expected;
        // effective() materializes a null steps slot for single-case contracts.
        if original["steps"].is_null() {
            original.as_object_mut().unwrap().remove("steps");
        }
        ensure!(
            crate::amendments::effective(&original)? == *f,
            "unapproved closed gate amendment"
        );
        crate::closed_final_faults::validate(&original)
    } else {
        crate::closed_final_faults::validate(f)
    }
}
pub(crate) async fn worker_binary() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("LIGHT_LAMBDA_SECURITY_COMPONENT_BIN") {
        return Ok(path.into());
    }
    let current = std::env::current_exe()?;
    let parent = current.parent().context("runner directory")?;
    let target = if parent.file_name().is_some_and(|n| n == "deps") {
        parent.to_path_buf()
    } else {
        parent.join("deps")
    };
    let mut paths = std::fs::read_dir(target)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("jwks_gate_component-"))
                && p.extension().is_none()
        })
        .collect::<Vec<_>>();
    paths.sort_by_key(|p| std::cmp::Reverse(p.metadata().and_then(|m| m.modified()).ok()));
    for path in paths {
        let output = Command::new(&path)
            .args(["--list", "--format", "terse"])
            .output()
            .await?;
        if String::from_utf8_lossy(&output.stdout).contains("component_probe:") {
            return Ok(path);
        }
    }
    anyhow::bail!(
        "build gate worker: CARGO_INCREMENTAL=0 cargo test --locked --offline -p light-lambda --test jwks_gate_component --no-run"
    )
}
pub(crate) async fn run(
    f: &Value,
    revision: &str,
    dir: &Path,
    tokens: &crate::tokens::Tokens,
    invoke: &crate::application::Dependency,
) -> Result<Value> {
    validate(f)?;
    let start = Instant::now();
    let control = tempfile::tempdir()?;
    let output = control.path().join("result.json");
    let input = control.path().join("input.json");
    let mut token = json!("${token:gate-parallel}");
    tokens.resolve(&mut token);
    std::fs::write(
        &input,
        serde_json::to_vec(
            &json!({"fixture":f,"config":dir,"control":control.path(),"output":output,"token":token}),
        )?,
    )?;
    let mut child = Command::new(worker_binary().await?)
        .args(["--exact", "--ignored", "component_probe", "--nocapture"])
        .env("LIGHT_LAMBDA_GATE_INPUT", &input)
        .env("LIGHT_LAMBDA_ALLOW_INSECURE_TEST_ENDPOINTS", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let initial = tokens.calls().len();
    let mut failure_calls = 0;
    let mut recovery_before = initial;
    let mut cancellation_before = initial;
    let mut reuse_before = initial;
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait()? {
            ensure!(status.success(), "closed gate worker failed: {status}");
            break;
        }
        ensure!(
            Instant::now() < end,
            "closed gate worker exceeded bounded lifetime"
        );
        let phase = std::fs::read_to_string(control.path().join("phase")).unwrap_or_default();
        let marker = control.path().join(format!("release-{phase}"));
        if !marker.exists() {
            match phase.as_str() {
                "fetch-waiting" if tokens.calls().len() > initial => {
                    cancellation_before = tokens.calls().len();
                    std::fs::write(marker, "")?;
                }
                "panic-ready" => {
                    ensure!(
                        tokens.calls().len() == initial,
                        "panic hook must precede I/O"
                    );
                    std::fs::write(marker, "")?;
                }
                "panic-settled" => {
                    failure_calls = tokens.calls().len() - initial;
                    std::fs::write(marker, "")?;
                }
                "reuse-ready" => {
                    reuse_before = tokens.calls().len();
                    std::fs::write(marker, "")?;
                }
                "recovery-ready" => {
                    recovery_before = tokens.calls().len();
                    std::fs::write(marker, "")?;
                }
                _ => {}
            }
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let mut witness = crate::load(&output)?;
    let observed = &mut witness["observation"];
    observed["backendCalls"] = json!(invoke.calls());
    observed["jwksFetches"] = json!(tokens.calls().len() - initial);
    observed["jwksCalls"] = json!(tokens.calls()[initial..]);
    if f["component"] == "rustJwksGateCancellationS13a" {
        observed["gate"]["followupUnknownKid"]["jwksFetches"] =
            json!(tokens.calls().len() - cancellation_before);
    } else {
        observed["failure"]["networkRequests"] = json!(failure_calls);
        observed["recovery"]["networkRequests"] = json!(reuse_before - recovery_before);
        observed["recovery"]["reuse"]["networkRequests"] =
            json!(tokens.calls().len() - reuse_before);
        witness["gatePanicEvidence"]["networkRequests"] = json!(tokens.calls()[initial..]);
    }
    let observed = witness["observation"].clone();
    let errors = match crate::incremental::assert_observation(&f["expect"], &observed) {
        Ok(()) => vec![],
        Err(e) => vec![format!("{e:#}")],
    };
    let mut result = json!({"schema":"light-lambda-conformance/result/v1","id":f["id"],"runner":"rust","runnerRevision":revision,"fixtureDigest":crate::fixture_digest(f)?,"execution":if errors.is_empty(){"passed"}else{"failed"},"durationMs":start.elapsed().as_millis() as u64,"observations":[observed],"assertionErrors":errors});
    if let Some(evidence) = witness.get("gatePanicEvidence") {
        result["gatePanicEvidence"] = evidence.clone();
    }
    Ok(result)
}
