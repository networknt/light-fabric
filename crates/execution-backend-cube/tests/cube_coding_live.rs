use chrono::{Duration, Utc};
use coding_agent_runtime::{CodingFixtureRequest, CodingTurnSpec};
use execution_backend::{ExecutionBackend, StagedInput};
use execution_backend_cube::{
    CubeBackendConfig, CubeExecutionBackend, CubeHttpClient, CubeHttpClientConfig,
};
use execution_runner_protocol::{
    AuthenticatedOrigin, CommandExecutionSpec, ExecuteLease, ExecutionId, ExecutionSubject,
    LeaseContext, LeaseId, OriginKind, SchedulingRequestId,
};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    process::Command,
    sync::Arc,
    time::Duration as StdDuration,
};
use url::Url;
use uuid::Uuid;

#[tokio::test]
#[ignore = "requires a live Cube cluster and a template containing light-coding-agent-fixture"]
async fn immutable_repository_returns_canonical_patch_through_live_cube() {
    let api_url =
        Url::parse(&std::env::var("LIGHT_CUBE_TEST_API_URL").expect("LIGHT_CUBE_TEST_API_URL"))
            .unwrap();
    let sandbox_url = std::env::var("LIGHT_CUBE_TEST_SANDBOX_URL")
        .ok()
        .map(|value| Url::parse(&value).unwrap());
    let api_key = fs::read_to_string(
        std::env::var("LIGHT_CUBE_TEST_API_KEY_FILE").expect("LIGHT_CUBE_TEST_API_KEY_FILE"),
    )
    .unwrap();
    let template_id =
        std::env::var("LIGHT_CUBE_TEST_TEMPLATE_ID").expect("LIGHT_CUBE_TEST_TEMPLATE_ID");
    let tls_ca_pem = std::env::var("LIGHT_CUBE_TEST_TLS_CA_FILE")
        .ok()
        .map(|path| fs::read(path).unwrap());
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    fs::create_dir(&source).unwrap();
    git(None, ["init", source.to_str().unwrap()]);
    fs::write(source.join("fixture.txt"), "before\n").unwrap();
    git(Some(&source), ["add", "fixture.txt"]);
    git(
        Some(&source),
        [
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-m",
            "base",
        ],
    );
    let revision = git_output(Some(&source), ["rev-parse", "HEAD"]);
    let bundle = root.path().join("repository.bundle");
    git(
        Some(&source),
        ["bundle", "create", bundle.to_str().unwrap(), "--all"],
    );
    let bundle_bytes = fs::read(&bundle).unwrap();
    let repository_digest = format!("sha256:{:x}", Sha256::digest(&bundle_bytes));
    let spec = CodingTurnSpec {
        repository_digest: repository_digest.clone(),
        base_revision: revision.clone(),
        workspace_root: "/workspace/repo".into(),
        prompt: "replace fixture text".into(),
        model_alias: "deterministic-fixture".into(),
        materialization_manifest_digest: format!("sha256:{}", "1".repeat(64)),
        writable_roots: BTreeSet::from(["/workspace/repo".into()]),
        allowed_tools: BTreeSet::from(["fs.read".into(), "fs.write".into()]),
        maximum_patch_bytes: 4096,
        maximum_changed_files: 1,
    };
    let request = CodingFixtureRequest {
        spec: spec.clone(),
        target_path: "fixture.txt".into(),
        expected_text: "before".into(),
        replacement_text: "after".into(),
    };
    let compatibility = "sha256:live-cube-coding-fixture-v1";
    let template_digest = "sha256:503c1f8879addd7dec140d9f2e703e6b7230979188bbd6f7c9e4f941e276a717";
    let command = CommandExecutionSpec {
        schema_version: 1,
        template_id: "cube-coding-fixture-v1".into(),
        template_version: 1,
        template_digest: template_digest.into(),
        executable: "/usr/local/bin/light-coding-agent-fixture".into(),
        arguments: vec![
            "--repository".into(),
            "/inputs/repository.bundle".into(),
            "--request-base64".into(),
            request.encode_argument().unwrap(),
        ],
        working_directory: "/workspace".into(),
        environment: BTreeMap::new(),
        wall_clock_timeout_ms: 120_000,
        stdout_limit_bytes: 1024 * 1024,
        stderr_limit_bytes: 1024 * 1024,
        network_enabled: false,
        credentials_enabled: false,
        persistent_workspace: false,
    };
    let lease = ExecuteLease {
        lease: LeaseContext {
            scheduling_request_id: SchedulingRequestId::new(),
            execution_id: ExecutionId::new(),
            origin: AuthenticatedOrigin {
                kind: OriginKind::Agent,
                service_id: "light-agent".into(),
                instance_id: "cube-live-test".into(),
                host_id: Uuid::nil(),
            },
            subject: ExecutionSubject::AgentTurn {
                subject_id: Uuid::new_v4(),
                session_id: Uuid::new_v4(),
                turn_id: Uuid::new_v4(),
            },
            attempt: 1,
            lease_id: LeaseId::new(),
            fencing_token: 1,
            policy_digest: "sha256:live-policy".into(),
            compatibility_digest: compatibility.into(),
            deadline: Utc::now() + Duration::minutes(3),
        },
        backend_id: "cube".into(),
        execution_profile: serde_json::json!({}),
        command: serde_json::to_value(&command).unwrap(),
        inputs: Vec::new(),
        definition_digest: "sha256:live-fixture".into(),
        command_template_digest: template_digest.into(),
    };
    let staged = StagedInput {
        input_id: Uuid::new_v4(),
        source_digest: repository_digest,
        local_path: bundle,
        mount_target: "/inputs/repository.bundle".into(),
        media_type: "application/x-git-bundle".into(),
        size: bundle_bytes.len() as u64,
        read_only: true,
        executable: false,
        mount_options: vec![
            "ro".into(),
            "nodev".into(),
            "nosuid".into(),
            "noexec".into(),
        ],
    };
    let client = CubeHttpClient::new(CubeHttpClientConfig {
        api_url,
        sandbox_url,
        api_key: api_key.trim().into(),
        request_timeout: StdDuration::from_secs(30),
        maximum_response_bytes: 4 * 1024 * 1024,
        allow_insecure_http: std::env::var("LIGHT_CUBE_TEST_ALLOW_HTTP").as_deref() == Ok("true"),
        tls_ca_pem,
    })
    .unwrap();
    let backend = CubeExecutionBackend::new(
        Arc::new(client),
        CubeBackendConfig {
            template_id,
            compatibility_digest: compatibility.into(),
            owner_runner: "cube-live-test".into(),
            available_slots: 1,
            maximum_native_ttl_seconds: 300,
            discovery_page_limit: 200,
        },
    );
    let prepared = backend.prepare(&lease, &[staged]).await.unwrap();
    let (_cancel, cancellation) = tokio::sync::watch::channel(false);
    let result = backend.execute(&prepared, &lease, cancellation).await;
    let cleanup = backend.cleanup(&prepared.backend_operation_id).await;
    let result = result.unwrap();
    cleanup.unwrap();
    let patch = result.structured_output.expect("trusted canonical patch");
    assert_eq!(patch["baseRevision"], revision);
    assert_eq!(patch["changedPaths"], serde_json::json!(["fixture.txt"]));
    assert!(patch["patch"].as_str().unwrap().contains("+after"));
}

fn git<const N: usize>(workspace: Option<&std::path::Path>, arguments: [&str; N]) {
    let output = command(workspace, arguments).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_output<const N: usize>(workspace: Option<&std::path::Path>, arguments: [&str; N]) -> String {
    let output = command(workspace, arguments).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

fn command<const N: usize>(workspace: Option<&std::path::Path>, arguments: [&str; N]) -> Command {
    let mut command = Command::new("git");
    if let Some(workspace) = workspace {
        command.arg("-C").arg(workspace);
    }
    command.args(arguments);
    command
}
