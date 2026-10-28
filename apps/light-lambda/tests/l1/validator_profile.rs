//! Explicitly gated real-child qualification. Never run by ordinary checks.
use super::support::*;
#[path = "qualification_cleanup.rs"]
mod cleanup;
use light_lambda_runtime::validator_ipc::lifecycle::{CompletionCause, Snapshot};
use serde_json::json;
use tokio::io::AsyncWriteExt;

/// Prepared expanded semantics only. The original three-request acceptance and
/// its expectations remain unchanged. This test requires separate named scope.
#[tokio::test]
#[ignore = "expanded semantics not covered by three-request execution clearance"]
async fn real_precision_child_semantic_activation_profile() {
    let executable = std::env::var("VALIDATOR_PROFILE_APPLICATION").expect("pinned parent");
    let worker = std::env::var("VALIDATOR_PROFILE_WORKER").expect("pinned worker");
    let evidence_root = std::path::PathBuf::from(
        std::env::var_os("WP10_L1_EVIDENCE_DIR").expect("fresh persistent evidence directory"),
    );
    std::fs::create_dir_all(&evidence_root).unwrap();
    let evidence = evidence_root.join("validator-semantic-activation");
    std::fs::create_dir(&evidence).expect("fresh semantic evidence");
    let cases: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("data/validator-activation-cases.json")).unwrap();
    assert_eq!(cases.len(), 15);
    let accepted: Vec<_> = cases.iter().filter(|c| c["status"] == 200).collect();
    assert_eq!(accepted.len(), 9);
    let invoke = RawServer::start(accepted.iter().map(|_| Reply::ok()).collect()).await;
    let jwks = RawServer::start(vec![Reply::json(json!({"keys":[]}))]).await;
    let dir = config(&invoke, &jwks);
    std::fs::write(
        dir.path().join("openapi.json"),
        include_str!("data/validator-activation-openapi.json"),
    )
    .unwrap();
    std::fs::write(dir.path().join("openapi-handler.yml"), "enabled: true\n").unwrap();
    std::fs::write(
        dir.path().join("openapi-validator.yml"),
        "enabled: true\nskipBodyValidation: false\nhandleNullableField: true\nskipPathPrefixes: []\n",
    )
    .unwrap();
    update_values(dir.path(), "security.enabled", json!(false));
    update_values(dir.path(), "unified-security.enabled", json!(false));
    update_values(
        dir.path(),
        "lambda-proxy.functions",
        json!({
            "/v1/decimal@post":"test-only-business",
            "/v1/integer@post":"test-only-business",
            "/v1/pets/{petId}@get":"test-only-business"
        }),
    );
    let class = |id| {
        serverless_core::compat::COMPATIBILITY
            .iter()
            .find(|c| c.id == id)
            .unwrap()
            .class
    };
    std::fs::write(dir.path().join("handler.yml"),json!({"handlers":[format!("{}@spec",class(serverless_core::HandlerId::Specification)),format!("{}@validator",class(serverless_core::HandlerId::Validator)),format!("{}@proxy",class(serverless_core::HandlerId::Proxy))],"defaultHandlers":["spec","validator","proxy"]}).to_string()).unwrap();
    let events = cases
        .iter()
        .map(|case| {
            let mut e = event(
                case["path"].as_str().unwrap(),
                case["method"].as_str().unwrap(),
                None,
            );
            if let Some(body) = case.get("body") {
                e["body"] = body.clone();
            }
            e["multiValueHeaders"]["Content-Type"] = json!([case
                .get("contentType")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("application/json")]);
            e["multiValueHeaders"]["x-traceability-id"] = json!([case["id"]]);
            if let Some(header) = case.get("requiredHeader") {
                e["multiValueHeaders"]["x-required"] = json!([header]);
            }
            if let Some(limit) = case.get("queryLimit") {
                e["multiValueQueryStringParameters"] = json!({"limit":[limit]});
            }
            Invocation::new(case["id"].as_str().unwrap(), e)
        })
        .collect();
    let runtime = Runtime::start(events, false, false, 202).await;
    let template = command(dir.path(), Some(&runtime));
    let mut cmd = tokio::process::Command::new(executable);
    cmd.env_clear()
        .envs(
            template
                .as_std()
                .get_envs()
                .filter_map(|(k, v)| v.map(|v| (k, v))),
        )
        .current_dir(dir.path())
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let lifecycle_path = dir.path().join("precision-lifecycle.jsonl");
    cmd.arg(dir.path())
        .arg(worker)
        .args(["268435456", "536870912", "1073741824", "16"])
        .arg(&lifecycle_path)
        .stdin(std::process::Stdio::piped());
    let process = ObservedApplication::spawn(cmd);
    let (outcome, finished) = cleanup::run(
        async {
            runtime.wait_posts(cases.len()).await;
            // The owner remains live until cleanup's stdin shutdown, so inspect
            // the actual child, not a replacement helper or parent executable.
            let snapshots: Vec<Snapshot> = std::fs::read_to_string(&lifecycle_path)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            let snapshot = snapshots.last().unwrap();
            assert_eq!(snapshot.children.len(), 1);
            let pid = snapshot.children[0].identity.child_pid;
            let proc = std::path::PathBuf::from(format!("/proc/{pid}"));
            let exe = std::fs::read_link(proc.join("exe")).unwrap();
            let digest = std::process::Command::new("/usr/bin/sha256sum")
                .arg(proc.join("exe"))
                .output()
                .unwrap();
            let digest_text = String::from_utf8(digest.stdout).unwrap();
            let image_digest = digest_text.split_whitespace().next().unwrap();
            let fd_targets: Vec<_> = std::fs::read_dir(proc.join("fd"))
                .unwrap()
                .map(|entry| std::fs::read_link(entry.unwrap().path()).unwrap())
                .collect();
            // Save paths only; addresses and request contents are not needed.
            let maps = std::fs::read_to_string(proc.join("maps")).unwrap();
            let mapped_paths: std::collections::BTreeSet<_> = maps
                .lines()
                .filter_map(|line| line.find('/').map(|start| line[start..].to_owned()))
                .collect();
            std::fs::write(evidence.join("actual-child-image.json"),serde_json::to_vec_pretty(&json!({
                "childPid":pid,"exe":exe,"sha256":image_digest,"digestReadSucceeded":digest.status.success(),
                "fdTargets":fd_targets,"mappedPaths":mapped_paths,
                "mappedLibraryBytesProved":false,"immutableDependencyLoadingProved":false
            })).unwrap()).unwrap();
            assert!(digest.status.success());
            assert_eq!(image_digest,"af8dbe7ab03d0634189299bf26b0fb61c2a18da1aca239d123c6b01ea24aad6b");
            assert!(exe.to_string_lossy().contains("memfd:light-lambda-verified-worker"));
            assert!(fd_targets.iter().all(|p| !p.to_string_lossy().contains("light-lambda-verified-worker")),"sealed image fd must close on exec");
            assert_posts(&runtime, cases.len());
            let posts = runtime.records();
            for (case, post) in cases.iter().zip(&posts) {
                let response = post.json();
                assert_eq!(response["statusCode"], case["status"], "{}", case["id"]);
                if case["status"] != 200 {
                    let body: serde_json::Value =
                        serde_json::from_str(response["body"].as_str().unwrap()).unwrap();
                    assert_eq!(body["code"], case["code"], "{}", case["id"]);
                    assert_eq!(body["message"], case["message"], "{}", case["id"]);
                }
            }
            assert_eq!(invoke.count(), 9);
            let calls = invoke.records();
            for (case, call) in accepted.iter().zip(&calls) {
                let received = call.json();
                assert_eq!(received["path"], case["path"]);
                assert_eq!(received["httpMethod"], case["method"]);
                assert_eq!(received["requestContext"]["extendedRequestId"], case["id"]);
                if let Some(body) = case.get("body") {
                    assert_eq!(&received["body"], body, "exact numeric/body bytes");
                }
            }
            // Nine exact accepted identities imply no dispatch for the six rejects.
            assert_eq!(jwks.count(), 0);
        },
        std::time::Duration::from_secs(40),
        |cause| process.finish(cause, &lifecycle_path),
    )
    .await;
    if let Some(result) = &finished.process {
        result.save(
            "validator-semantic-activation",
            Some(&runtime),
            &invoke,
            &jwks,
        );
    }
    std::fs::write(
        evidence.join("precision-lifecycle.jsonl"),
        std::fs::read(&lifecycle_path).unwrap_or_default(),
    )
    .unwrap();
    std::fs::write(evidence.join("cleanup.json"),serde_json::to_vec_pretty(&json!({"applicationPid":finished.application_pid,"emergencyParentKill":finished.emergency_parent_kill,"cleanupError":finished.error,"precisionChildCleanupDemonstrated":finished.error.is_none(),"completionCause":finished.cause})).unwrap()).unwrap();
    if let Some(error) = finished.error {
        panic!("precision child cleanup incomplete: {error}");
    }
    match outcome {
        cleanup::Outcome::Success(()) => {}
        cleanup::Outcome::AssertionFailure(panic) => std::panic::resume_unwind(panic),
        cleanup::Outcome::Timeout => panic!("semantic qualification timed out after cleanup"),
    }
}
#[tokio::test]
#[ignore = "requires reviewed profile/bundle and platform execution clearance; no scripted IPC"]
async fn real_precision_child_built_application_valid_invalid() {
    let executable =
        std::env::var("VALIDATOR_PROFILE_APPLICATION").expect("built qualification application");
    let worker = std::env::var("VALIDATOR_PROFILE_WORKER").expect("integrity-verified worker");
    let evidence_root = std::path::PathBuf::from(
        std::env::var_os("WP10_L1_EVIDENCE_DIR")
            .expect("persistent qualification evidence directory"),
    );
    std::fs::create_dir_all(&evidence_root).unwrap();
    let evidence = evidence_root.join("validator-real-child-local-qualification");
    std::fs::create_dir(&evidence).expect("fresh qualification evidence directory");
    let invoke = RawServer::start(vec![Reply::ok(), Reply::ok()]).await;
    let jwks = RawServer::start(vec![Reply::json(json!({"keys":[]}))]).await;
    let dir = config(&invoke, &jwks);
    std::fs::write(
        dir.path().join("openapi.yaml"),
        include_str!("../../migrations/native-petstore/openapi.yaml"),
    )
    .unwrap();
    std::fs::write(dir.path().join("openapi-handler.yml"), "enabled: true\n").unwrap();
    std::fs::write(
        dir.path().join("openapi-validator.yml"),
        include_str!("../../migrations/native-petstore/openapi-validator.yml"),
    )
    .unwrap();
    update_values(dir.path(), "security.enabled", json!(false));
    update_values(dir.path(), "unified-security.enabled", json!(false));
    update_values(
        dir.path(),
        "lambda-proxy.functions",
        json!({"/v1/pets@post":"test-only-business"}),
    );
    let class = |id| {
        serverless_core::compat::COMPATIBILITY
            .iter()
            .find(|c| c.id == id)
            .unwrap()
            .class
    };
    std::fs::write(dir.path().join("handler.yml"),json!({"handlers":[format!("{}@spec",class(serverless_core::HandlerId::Specification)),format!("{}@validator",class(serverless_core::HandlerId::Validator)),format!("{}@proxy",class(serverless_core::HandlerId::Proxy))],"defaultHandlers":["spec","validator","proxy"]}).to_string()).unwrap();
    let request = |body: &str| {
        let mut e = event("/v1/pets", "POST", None);
        e["body"] = json!(body);
        e["multiValueHeaders"]["content-type"] = json!(["application/json"]);
        e["multiValueHeaders"]["x-traceability-id"] = json!(["petstore-qualification"]);
        e
    };
    let runtime = Runtime::start(
        vec![
            Invocation::new("valid", request("{\"name\":\"Tucker\"}")),
            Invocation::new("invalid", request("{}")),
            Invocation::new("warm-valid", request("{\"name\":\"Milo\"}")),
        ],
        false,
        false,
        202,
    )
    .await;
    let template = command(dir.path(), Some(&runtime));
    let mut cmd = tokio::process::Command::new(executable);
    cmd.env_clear()
        .envs(
            template
                .as_std()
                .get_envs()
                .filter_map(|(k, v)| v.map(|v| (k, v))),
        )
        .current_dir(dir.path())
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    cmd.arg(dir.path())
        .arg(worker)
        .args(["268435456", "536870912", "1073741824", "16"]);
    let lifecycle_path = dir.path().join("precision-lifecycle.jsonl");
    cmd.arg(&lifecycle_path).stdin(std::process::Stdio::piped());
    let process = ObservedApplication::spawn(cmd);
    let (outcome, finished) = cleanup::run(
        async {
            runtime.wait_posts(3).await;
            assert_posts(&runtime, 3);
            let posts = runtime.records();
            assert_eq!(posts[0].json()["statusCode"], 200);
            assert_eq!(posts[1].json()["statusCode"], 400);
            assert_eq!(posts[2].json()["statusCode"], 200);
            let invalid: serde_json::Value =
                serde_json::from_str(posts[1].json()["body"].as_str().unwrap()).unwrap();
            assert_eq!(
                invalid["code"], "ERR11004",
                "not a schema violation: {invalid}"
            );
            assert_eq!(invalid["message"], "VALIDATOR_SCHEMA");
            assert_eq!(invoke.count(), 2);
            let calls = invoke.records();
            assert_eq!(calls[0].json()["body"], "{\"name\":\"Tucker\"}");
            assert_eq!(calls[1].json()["body"], "{\"name\":\"Milo\"}");
            assert!(calls.iter().all(|call| call.json()["path"] == "/v1/pets"));
            assert_eq!(jwks.count(), 0);
        },
        std::time::Duration::from_secs(40),
        |cause| process.finish(cause, &lifecycle_path),
    )
    .await;
    if let Some(result) = &finished.process {
        result.save(
            "validator-real-child-local-qualification",
            Some(&runtime),
            &invoke,
            &jwks,
        );
    }
    // Preserve even malformed/partial streams and cleanup errors before any
    // original assertion is resumed or timeout is reported.
    std::fs::write(
        evidence.join("precision-lifecycle.jsonl"),
        std::fs::read(&lifecycle_path).unwrap_or_default(),
    )
    .unwrap();
    std::fs::write(evidence.join("cleanup.json"),serde_json::to_vec_pretty(&json!({"applicationPid":finished.application_pid,"emergencyParentKill":finished.emergency_parent_kill,"cleanupError":finished.error,"precisionChildCleanupDemonstrated":finished.error.is_none(),"completionCause":finished.cause})).unwrap()).unwrap();
    if let Some(error) = finished.error {
        panic!("precision child cleanup incomplete: {error}");
    }
    match outcome {
        cleanup::Outcome::Success(()) => {}
        cleanup::Outcome::AssertionFailure(panic) => std::panic::resume_unwind(panic),
        cleanup::Outcome::Timeout => panic!("qualification body timed out after cleanup"),
    }
}

struct ObservedApplication {
    child: tokio::process::Child,
    stdout: tokio::task::JoinHandle<Vec<u8>>,
    stderr: tokio::task::JoinHandle<Vec<u8>>,
    application_pid: u32,
    started: std::time::Instant,
}
struct Finished {
    process: Option<super::support::Result>,
    application_pid: u32,
    cause: CompletionCause,
    emergency_parent_kill: bool,
    error: Option<String>,
}
impl ObservedApplication {
    fn spawn(mut command: tokio::process::Command) -> Self {
        use tokio::io::AsyncReadExt;
        let mut child = command.spawn().unwrap();
        let application_pid = child.id().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        Self {
            child,
            application_pid,
            started: std::time::Instant::now(),
            stdout: tokio::spawn(async move {
                let mut data = vec![];
                let _ = stdout.read_to_end(&mut data).await;
                data
            }),
            stderr: tokio::spawn(async move {
                let mut data = vec![];
                let _ = stderr.read_to_end(&mut data).await;
                data
            }),
        }
    }
    async fn finish(
        mut self,
        cause: CompletionCause,
        lifecycle_path: &std::path::Path,
    ) -> Finished {
        let mut error = None;
        if let Some(mut input) = self.child.stdin.take() {
            let control = format!("{}\n", cause.wire());
            if !matches!(
                tokio::time::timeout(
                    std::time::Duration::from_secs(1),
                    input.write_all(control.as_bytes())
                )
                .await,
                Ok(Ok(()))
            ) {
                error = Some("shutdown control could not be delivered".into());
            }
            // Dropping stdin also notifies controlClosed if the write failed.
        } else {
            error = Some("missing shutdown control".into());
        }
        let status =
            tokio::time::timeout(std::time::Duration::from_secs(10), self.child.wait()).await;
        let mut emergency_parent_kill = false;
        let status = match status {
            Ok(Ok(status)) => Some(status),
            _ => {
                emergency_parent_kill = true;
                error = Some(
                    "application did not finish owner cleanup; parent kill is not child reap proof"
                        .into(),
                );
                let _ = self.child.start_kill();
                tokio::time::timeout(std::time::Duration::from_secs(2), self.child.wait())
                    .await
                    .ok()
                    .and_then(|s| s.ok())
            }
        };
        let process = if let Some(status) = status {
            if !status.success() {
                error.get_or_insert_with(|| {
                    "qualification application exited unsuccessfully".into()
                });
            }
            let out =
                tokio::time::timeout(std::time::Duration::from_secs(2), &mut self.stdout).await;
            let err =
                tokio::time::timeout(std::time::Duration::from_secs(2), &mut self.stderr).await;
            match (out, err) {
                (Ok(Ok(stdout)), Ok(Ok(stderr))) => Some(super::support::Result {
                    status,
                    stdout: String::from_utf8_lossy(&stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&stderr).into_owned(),
                    elapsed: self.started.elapsed(),
                    stopped_by_harness: emergency_parent_kill,
                }),
                _ => {
                    error.get_or_insert_with(|| "application output drain incomplete".into());
                    None
                }
            }
        } else {
            None
        };
        // Parent exit/status is saved separately and cannot satisfy this check.
        if let Err(message) = verify_owner_evidence(lifecycle_path, self.application_pid, cause) {
            error.get_or_insert(message);
        }
        Finished {
            process,
            application_pid: self.application_pid,
            cause,
            emergency_parent_kill,
            error,
        }
    }
}
fn verify_owner_evidence(
    path: &std::path::Path,
    application_pid: u32,
    cause: CompletionCause,
) -> std::result::Result<(), String> {
    let text = std::fs::read_to_string(path).map_err(|_| "missing child-owner lifecycle stream")?;
    let mut last = None;
    for line in text.lines() {
        last = Some(
            serde_json::from_str::<Snapshot>(line)
                .map_err(|_| "invalid child-owner lifecycle stream")?,
        );
    }
    let snapshot = last.ok_or("empty child-owner lifecycle stream")?;
    if snapshot.evidence_origin != "actualChildOwner"
        || snapshot.owner_pid != application_pid
        || snapshot.completion_cause != Some(cause)
        || !snapshot.clean()
        || snapshot.children.len() != 1
    {
        return Err(
            "missing actual child identity, shutdown, exit or owning-process reap witness".into(),
        );
    }
    for child in snapshot.children {
        match std::fs::read_to_string(format!("/proc/{}/stat", child.identity.child_pid)) {
            Ok(stat) => {
                let start = stat
                    .rsplit_once(')')
                    .and_then(|(_, tail)| tail.split_whitespace().nth(19))
                    .and_then(|n| n.parse::<u64>().ok());
                if start.is_none() || start == child.identity.start_time_ticks {
                    return Err("precision child identity still exists after owner wait".into());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("cannot confirm precision child absence after owner wait".into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod evidence_tests {
    use super::*;
    #[test]
    fn parent_exit_or_missing_partial_stream_cannot_prove_child_reaping() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lifecycle.jsonl");
        assert!(verify_owner_evidence(&path, 42, CompletionCause::Success).is_err());
        for text in ["", "{", r#"{"exitCode":0,"applicationPid":42}"#] {
            std::fs::write(&path, text).unwrap();
            assert!(verify_owner_evidence(&path, 42, CompletionCause::Success).is_err());
        }
    }
    #[test]
    fn simulated_or_foreign_owner_records_never_count_as_actual_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lifecycle.jsonl");
        let snapshot = Snapshot {
            evidence_origin: "simulatedNoChild".into(),
            owner_pid: 42,
            active_owners: 0,
            completion_cause: Some(CompletionCause::Success),
            recording_failed: false,
            children: vec![],
        };
        std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
        assert!(verify_owner_evidence(&path, 42, CompletionCause::Success).is_err());
        let mut snapshot = snapshot;
        snapshot.evidence_origin = "actualChildOwner".into();
        std::fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
        assert!(verify_owner_evidence(&path, 43, CompletionCause::Success).is_err());
        assert!(verify_owner_evidence(&path, 42, CompletionCause::Success).is_err());
    }
}
