//! Isolated real Init/service execution, with actual policy stdout observations.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};
use tokio::io::AsyncReadExt;

// Own and reap the ordinary fixture child before publishing either a result or
// an error. Dropping Command::output on timeout only requests background cleanup.
async fn execute(
    command: &mut tokio::process::Command,
    limit: Duration,
) -> Result<std::process::Output> {
    let mut child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let mut stdout = child.stdout.take().context("worker stdout")?;
    let mut stderr = child.stderr.take().context("worker stderr")?;
    let result = tokio::time::timeout(limit, async {
        let mut out = Vec::new();
        let mut err = Vec::new();
        let (status, _, _) = tokio::try_join!(
            child.wait(),
            stdout.read_to_end(&mut out),
            stderr.read_to_end(&mut err)
        )?;
        Ok::<_, std::io::Error>(std::process::Output {
            status,
            stdout: out,
            stderr: err,
        })
    })
    .await;
    match result {
        Ok(Ok(output)) => Ok(output),
        failure => {
            let _ = child.start_kill();
            child
                .wait()
                .await
                .context("reap emission worker after failure")?;
            match failure {
                Ok(Err(error)) => Err(error.into()),
                Err(error) => Err(error).context("bounded emission worker"),
                Ok(Ok(_)) => unreachable!(),
            }
        }
    }
}
pub(crate) fn needed(f: &Value) -> bool {
    f["configurationProbe"] == "syntheticDecryption"
        || crate::application::cases(f).iter().any(|s| {
            s["expect"].get("emf").is_some()
                || s["expect"].get("configurationDiagnostics").is_some()
        })
}
pub(crate) async fn run(f: &Value, revision: &str) -> Result<Value> {
    ensure!(
        !crate::application::cases(f)
            .iter()
            .any(|s| s["expect"].get("emf").is_some())
            || crate::application::cases(f).len() == 1,
        "EMF attribution currently requires one request"
    );
    let directory = tempfile::tempdir()?;
    let input = directory.path().join("fixture.json");
    let output = directory.path().join("result.json");
    std::fs::write(&input, serde_json::to_vec(f)?)?;
    let current = std::env::current_exe()?;
    let parent = current.parent().context("runner directory")?;
    let target = if parent.file_name().is_some_and(|p| p == "deps") {
        parent.parent().context("target directory")?
    } else {
        parent
    };
    let mut command = tokio::process::Command::new(target.join("serverless-conformance"));
    command
        .arg("__emission-worker")
        .arg(&input)
        .arg(&output)
        .arg(revision)
        .env("LIGHT_LAMBDA_EMISSION_WORKER", "1")
        .kill_on_drop(true);
    if f["configurationProbe"] == "syntheticDecryption" {
        command.env("light_4j_config_password", "test-only-master");
    }
    let execution = execute(&mut command, Duration::from_secs(15)).await?;
    ensure!(
        execution.status.success(),
        "emission worker failed: {}",
        execution.status
    );
    ensure!(
        execution.stdout.len() <= 1_048_576,
        "emission output overflow"
    );
    let stdout = std::str::from_utf8(&execution.stdout)?;
    let lines = stdout.lines().filter(|s| !s.is_empty()).collect::<Vec<_>>();
    let mut result = crate::load(&output)?;
    if crate::application::cases(f)[0]["expect"]
        .get("emf")
        .is_some()
    {
        let window = crate::recipes::window(&result, 0)?
            .context("actual invocation window")?
            .clone();
        result["observations"][0]["emf"] = json!({"rawLines":lines,"invocationStartUnixMs":window["startEpochMs"],"invocationEndUnixMs":window["endEpochMs"]});
    } else {
        ensure!(lines.is_empty(), "unexpected raw policy stdout");
    }
    // Child asserts all non-stdout fields; parent reasserts every full untouched
    // original expectation against actual observations before publication.
    for (index, case) in crate::application::cases(f).iter().enumerate() {
        if let Err(error) = crate::incremental::assert_observation_with_window(
            &case["expect"],
            &result["observations"][index],
            crate::recipes::window(&result, index)?,
        ) {
            result["assertionErrors"]
                .as_array_mut()
                .context("assertion errors")?
                .push(json!(format!("stdout completion: {error:#}")));
            result["execution"] = json!("failed");
        }
    }
    Ok(result)
}
pub(crate) fn command(args: &[String]) -> Result<()> {
    ensure!(
        args.len() == 4 && std::env::var("LIGHT_LAMBDA_EMISSION_WORKER").as_deref() == Ok("1"),
        "private emission worker arguments"
    );
    let f = crate::load(&PathBuf::from(&args[1]))?;
    ensure!(needed(&f), "closed emission fixture");
    // Each child owns one fixture and its async loopback dependencies. A default
    // multi-thread runtime allocates a host-sized pool per concurrent fixture
    // (24 workers on the qualification host), competing with its 50ms deadline.
    // Keep process isolation without multiplying idle scheduler pools.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async {
        let result = crate::application::run(&f, &args[3]).await?;
        // Best-effort sink has no acknowledgement API. A bounded observation
        // window never fabricates a missing record; the parent fails it.
        tokio::time::sleep(Duration::from_millis(50)).await;
        anyhow::Ok(result)
    })?;
    let out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&args[2])?;
    serde_json::to_writer(out, &result)?;
    Ok(())
}
pub(crate) fn assert(expected: &Value, actual: &Value) -> Result<()> {
    ensure!(
        expected["contract"] == "LightLambda-per-request-v1",
        "closed EMF contract"
    );
    let lines = actual["rawLines"].as_array().context("actual EMF lines")?;
    ensure!(
        lines.len() == 1,
        "expected exactly one raw EMF record, got {}",
        lines.len()
    );
    let record: Value = serde_json::from_str(lines[0].as_str().context("raw EMF line")?)?;
    let start = actual["invocationStartUnixMs"]
        .as_u64()
        .context("invocation start")?;
    let end = actual["invocationEndUnixMs"]
        .as_u64()
        .context("invocation end")?;
    let timestamp = record["_aws"]["Timestamp"]
        .as_u64()
        .context("EMF timestamp")?;
    ensure!(
        start <= timestamp && timestamp <= end,
        "EMF timestamp outside invocation"
    );
    ensure!(
        record["Duration"]
            .as_f64()
            .is_some_and(|d| d.is_finite() && d >= 0.0),
        "EMF duration"
    );
    ensure!(
        record.as_object().context("EMF object")?.len() == 6
            && record["_aws"].as_object().context("EMF metadata")?.len() == 2,
        "unexpected EMF fields"
    );
    ensure!(
        record["FunctionName"] == expected["functionName"]
            && record["Route"] == expected["route"]
            && record["Requests"] == expected["requests"]
            && record["Errors"] == expected["errors"],
        "EMF dimensions/counters differ"
    );
    ensure!(
        record["_aws"]["CloudWatchMetrics"]
            == json!([{"Namespace":"LightLambda","Dimensions":[["FunctionName","Route"]],"Metrics":[{"Name":"Requests","Unit":"Count"},{"Name":"Errors","Unit":"Count"},{"Name":"Duration","Unit":"Milliseconds"}]}]),
        "EMF definition differs"
    );
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod cleanup_tests {
    use super::*;

    fn owned_child(pid_file: &std::path::Path, tail: &str) -> tokio::process::Command {
        let mut command = tokio::process::Command::new("sh");
        command
            .args([
                "-c",
                &format!("printf %s \"$$\" > \"$1\"; {tail}"),
                "owned-emission-test",
            ])
            .arg(pid_file)
            .kill_on_drop(true);
        command
    }

    fn assert_reaped(pid_file: &std::path::Path) {
        let pid = std::fs::read_to_string(pid_file).unwrap();
        assert!(
            !std::path::Path::new("/proc").join(pid).exists(),
            "owning process must reap child"
        );
    }

    #[tokio::test]
    async fn success_and_nonzero_exit_are_reaped_before_return() {
        let dir = tempfile::tempdir().unwrap();
        for code in [0, 7] {
            let pid = dir.path().join(format!("pid-{code}"));
            let output = execute(
                &mut owned_child(&pid, &format!("exit {code}")),
                Duration::from_secs(5),
            )
            .await
            .unwrap();
            assert_eq!(output.status.code(), Some(code));
            assert_reaped(&pid);
        }
    }

    #[tokio::test]
    async fn timeout_kills_and_reaps_the_actual_child() {
        let dir = tempfile::tempdir().unwrap();
        let pid = dir.path().join("pid");
        let error = execute(
            &mut owned_child(&pid, "exec sleep 30"),
            Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("bounded emission worker"));
        assert_reaped(&pid);
    }
}
