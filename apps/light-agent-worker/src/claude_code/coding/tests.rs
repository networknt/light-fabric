use super::*;
use coding_agent_runtime::{
    CodingImplementationArtifact, CodingReviewInput, CodingRoleExecutionProfile,
};
use std::{collections::BTreeSet, os::unix::fs::PermissionsExt};

#[tokio::test]
async fn canonical_patch_keeps_source_changes_and_excludes_ignored_build_output() {
    let fixture = Fixture::new();
    let (_tx, cancel) = watch::channel(false);
    let workspace = Workspace::new(
        &fixture.bundle,
        &fixture.turn.coding,
        cancel,
        Instant::now() + Duration::from_secs(15),
    )
    .await
    .unwrap();
    // Ignoring an already tracked file must not hide its modification.
    std::fs::write(
        workspace.repository.join(".gitignore"),
        "main.py\n__pycache__/\nbuild/\n",
    )
    .unwrap();
    std::fs::write(workspace.repository.join("main.py"), "value = 42\n").unwrap();
    std::fs::write(workspace.repository.join("new_source.py"), "answer = 42\n").unwrap();
    let mut build = Command::new("/usr/bin/python3");
    build.current_dir(&workspace.repository).args(["-c",
        "import pathlib,py_compile; py_compile.compile('main.py',doraise=True); pathlib.Path('build').mkdir(); pathlib.Path('build/output').write_bytes(b'x'*2097152)"]);
    workspace.output(build, b"").await.unwrap();
    let patch = workspace.diff().await.unwrap();
    let paths = workspace
        .git(&["diff", "--name-only", "HEAD", "--"], b"")
        .await
        .unwrap();
    assert_eq!(
        paths.lines().collect::<Vec<_>>(),
        [".gitignore", "main.py", "new_source.py"]
    );
    assert!(patch.contains("+value = 42") && patch.contains("+answer = 42"));
    let validated = validate_patch(
        &fixture.turn.coding,
        &ProtectedPathPolicy::default_deny(),
        &fixture.turn.coding.base_revision,
        &patch,
        &paths.lines().map(str::to_owned).collect::<Vec<_>>(),
    )
    .unwrap();
    assert_eq!(validated.changed_paths.len(), 3);
}

struct Fixture {
    _root: tempfile::TempDir,
    host: HostContext,
    bundle: PathBuf,
    manifest: MaterializationManifest,
    turn: ClaudeTurn,
    digest: String,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        std::fs::create_dir(&source).unwrap();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("/usr/bin/git")
                .args(args)
                .current_dir(&source)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8(out.stdout).unwrap()
        };
        git(&["init", "-q"]);
        git(&["config", "user.name", "test"]);
        git(&["config", "user.email", "test@example.com"]);
        std::fs::write(source.join("main.py"), "value = 0\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "base"]);
        let base = git(&["rev-parse", "HEAD"]).trim().to_owned();
        let bundle = root.path().join("input.bundle");
        git(&["bundle", "create", bundle.to_str().unwrap(), "HEAD"]);
        let home = root.path().join("native");
        std::fs::create_dir(&home).unwrap();
        std::fs::write(home.join(".credentials.json"), "{}").unwrap();
        let executable = root.path().join("claude");
        std::fs::write(&executable, r#"#!/usr/bin/python3
import sys,os,json,re,pathlib
args=sys.argv[1:]
if args == ['--version']:
 print('2.1.269 (Claude Code)');sys.exit()
if args == ['auth','status']:
 print(json.dumps(dict(loggedIn=True,authMethod='claude.ai',apiProvider='firstParty')));sys.exit()
prompt=sys.stdin.read()
for hidden_path in re.findall(r'HIDDEN_PATH=(\S+)',prompt):
 assert not pathlib.Path(hidden_path).exists(), 'host data leaked into native namespace'
sid=args[args.index('--resume')+1] if '--resume' in args else args[args.index('--session-id')+1]
history=pathlib.Path(os.environ['CLAUDE_CONFIG_DIR']) / (sid+'.json')
prior=json.loads(history.read_text()) if '--resume' in args else []
if '--resume' in args: assert prior
repo=pathlib.Path.cwd()
text='completed'
if 'WAIT_NAMESPACE' in prompt:
 import subprocess,time
 target=str(pathlib.Path(os.environ['CLAUDE_CONFIG_DIR'])/'late-child')
 subprocess.Popen([sys.executable,'-c','import time,pathlib; time.sleep(2); pathlib.Path('+repr(target)+').write_text("escaped")'])
 print(json.dumps(dict(type='system',subtype='init',session_id=sid,model='claude-sonnet-5',permissionMode='bypassPermissions')),flush=True)
 time.sleep(30)
if 'Review the exact candidate at ' in prompt:
 repo=pathlib.Path(re.search(r'Review the exact candidate at (.*?)\. This is',prompt).group(1))
 assert (repo/'main.py').exists()
 assert all('Review the exact candidate' in p for p in prior), 'implementer history leaked'
 if 'VERIFY_READ_ONLY' in prompt:
  try:
   (repo/'main.py').write_text('MUTATED')
   raise AssertionError('reviewer write succeeded')
  except OSError as e: assert e.errno in (13,30)
 rid=json.loads(re.search(r'reviewId:(".*?"),',prompt).group(1))
 digest=json.loads(re.search(r'artifactDigest:(".*?"),',prompt).group(1))
 text=json.dumps(dict(schemaVersion=1,reviewId=rid,artifactDigest=digest,verdict='approved',findings=[],validationGaps=[]))
 if 'WRONG_REVIEW' in prompt: text=text.replace(digest,'sha256:'+'0'*64)
 if 'EXPECT_TWO' in prompt: assert (repo/'main.py').read_text() == 'value = 2\n'
else:
 if 'FIRST_EDIT' in prompt:
  assert not prior
  (repo/'main.py').write_text('value = 1\n')
 if 'SECOND_EDIT' in prompt:
  assert prior and 'FIRST_EDIT' in prior[0]
  assert (repo/'main.py').read_text() == 'value = 1\n'
  (repo/'main.py').write_text('value = 2\n')
 if 'PROTECTED_EDIT' in prompt:
  (repo/'.github').mkdir(exist_ok=True)
  (repo/'.github'/'workflows').mkdir(exist_ok=True)
  (repo/'.github'/'workflows'/'ci.yml').write_text('unsafe\n')
 if 'METADATA_WRITE' in prompt:
  metadata=pathlib.Path((repo/'.git').read_text().strip().split(': ',1)[1])
  try:
   (metadata/'HEAD').write_text('bad')
   raise AssertionError('metadata write succeeded')
  except OSError as e: assert e.errno in (13,30)
if 'METADATA_WRITE' in prompt:
 for checkpoint in pathlib.Path(os.environ['CLAUDE_CONFIG_DIR']).parent.glob('.light-claude-checkpoints-*/light-worker-threads/*.json'):
  try:
   checkpoint.write_text('corrupt')
   raise AssertionError('private checkpoint write succeeded')
  except OSError as e: assert e.errno in (13,30)
history.write_text(json.dumps(prior+[prompt]))
print(json.dumps(dict(type='system',subtype='init',session_id=sid,model='claude-sonnet-5',permissionMode='bypassPermissions')))
result=dict(type='result',subtype='success',is_error=False,session_id=sid,result=text,permission_denials=[],usage={'input_tokens':1})
if '--json-schema' in args:
 result['structured_output']=json.loads(text)
print(json.dumps(result))
"#).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let digest = hex::encode(Sha256::digest(std::fs::read(&executable).unwrap()));
        let manifest = MaterializationManifest {
            schema_version: 1,
            materializer_id: "coding".into(),
            materializer_version: 1,
            product_profile: agent_materializer::ProductProfile::Coding,
            runtime_compatibility: ADAPTER_ID.into(),
            packages: vec![],
            effective_instructions: vec![],
            allowed_tools: BTreeSet::new(),
            writable_roots: BTreeSet::from(["/workspace/repository".into()]),
        };
        let coding: CodingTurnSpec = serde_json::from_value(json!({
            "thread":{"runnerId":"personal","sessionRef":Uuid::new_v4(),"stageId":"review-stage","mode":"new","closeAfterTurn":false},
            "repositoryDigest":agent_core::sha256_digest(&std::fs::read(&bundle).unwrap()), "baseRevision":base,
            "workspaceRoot":"/workspace/repository", "prompt":"FIRST_EDIT METADATA_WRITE",
            "modelAlias":"coding-implementer","authenticationProfile":"personal-subscription",
            "role":"implement","roleProfile":CodingRoleExecutionProfile::pinned(CodingRole::Implement),
            "materializationManifestDigest":manifest.digest().unwrap(), "writableRoots":manifest.writable_roots,
            "allowedTools":CodingTurnSpec::supported_tools(CodingRole::Implement),"maximumPatchBytes":4096,"maximumChangedFiles":10
        })).unwrap();
        let turn = ClaudeTurn {
            coding,
            policy: LaunchPolicy {
                permission_source: PermissionSource::ClaudeCli,
                permission_mode: PermissionMode::BypassPermissions,
                default_model: "sonnet".into(),
                models: BTreeMap::from([("sonnet".into(), "claude-sonnet-5".into())]),
                tools: vec![],
                allowed_tools: vec![],
            },
            native_model: Some("sonnet".into()),
        };
        let host = HostContext {
            executable,
            native_home: home,
            working_directory: root.path().into(),
            thread_scope: format!("sha256:{}", "1".repeat(64)),
        };
        Self {
            _root: root,
            host,
            bundle,
            manifest,
            turn,
            digest,
        }
    }
    async fn run(&self, turn: ClaudeTurn) -> Result<CodingOutput> {
        let (_tx, rx) = watch::channel(false);
        let (events, _read) = mpsc::channel(64);
        let mut manifest = self.manifest.clone();
        manifest.writable_roots = turn.coding.writable_roots.clone();
        execute_coding_inner(
            &self.host,
            &self.bundle,
            &manifest,
            turn,
            rx,
            Instant::now() + Duration::from_secs(15),
            events,
            &self.digest,
        )
        .await
    }
    fn resume(&self, mut turn: ClaudeTurn, output: &CodingOutput) -> ClaudeTurn {
        let thread = turn.coding.thread.as_mut().unwrap();
        thread.mode = CodingThreadMode::Resume;
        thread.expected_checkpoint =
            Some(serde_json::from_value(output.coding_thread["checkpoint"].clone()).unwrap());
        turn.native_model = None;
        turn
    }
    fn review(&self, output: &CodingOutput) -> ClaudeTurn {
        let mut turn = self.turn.clone();
        turn.coding.role = CodingRole::Review;
        turn.coding.role_profile = CodingRoleExecutionProfile::pinned(CodingRole::Review);
        turn.coding.model_alias = "coding-reviewer".into();
        turn.coding.allowed_tools = CodingTurnSpec::supported_tools(CodingRole::Review);
        turn.coding.thread.as_mut().unwrap().session_ref = Uuid::new_v4();
        let native_key=agent_runtime_protocol::canonical_digest(&json!({"home":self.host.native_home,"scope":self.host.thread_scope,
            "session":self.turn.coding.thread.as_ref().unwrap().session_ref,"role":CodingRole::Implement})).unwrap();
        let implementer_state = self.host.native_home.parent().unwrap().join(format!(
            ".light-claude-native-{}",
            native_key.trim_start_matches("sha256:")
        ));

        turn.coding.prompt = format!(
            "VERIFY_READ_ONLY HIDDEN_PATH={} HIDDEN_PATH={}",
            self._root.path().join("source/main.py").display(),
            implementer_state.display()
        );
        turn.coding.writable_roots = BTreeSet::from(["/workspace/review-scratch".into()]);
        let mut manifest = self.manifest.clone();
        manifest.writable_roots = turn.coding.writable_roots.clone();
        turn.coding.materialization_manifest_digest = manifest.digest().unwrap();
        let artifact = output.patch.as_ref().unwrap();
        turn.coding.review_input = Some(Box::new(CodingReviewInput {
            review_id: "review-1".into(),
            repository: "example/repo".into(),
            requirements: "change value".into(),
            requirements_digest: patch_digest("change value"),
            candidate_patch: artifact["patch"].as_str().unwrap().into(),
            implementation: CodingImplementationArtifact {
                schema_version: 1,
                adapter_contract_digest: format!("sha256:{}", "2".repeat(64)),
                repository_digest: turn.coding.repository_digest.clone(),
                base_revision: turn.coding.base_revision.clone(),
                patch_digest: artifact["patchDigest"].as_str().unwrap().into(),
                changed_paths: serde_json::from_value(artifact["changedPaths"].clone()).unwrap(),
                validation_evidence: vec![],
                resolved_finding_ids: BTreeSet::new(),
            },
            prior_review: None,
        }));
        turn
    }
}

#[tokio::test]
async fn multi_turn_edit_and_independent_review_refresh_close() {
    let fixture = Fixture::new();
    let first = fixture.run(fixture.turn.clone()).await.unwrap();
    assert!(
        first.patch.as_ref().unwrap()["patch"]
            .as_str()
            .unwrap()
            .contains("+value = 1")
    );
    assert_eq!(
        first.authentication.as_ref().unwrap().credential_source,
        CodingCredentialSource::NativeClaudeStore
    );
    let review_turn = fixture.review(&first);
    let review = fixture.run(review_turn.clone()).await.unwrap();
    assert!(review.coding_review.is_some());
    let mut follow = fixture.resume(fixture.turn.clone(), &first);
    follow.coding.prompt = "SECOND_EDIT".into();
    let second = fixture.run(follow.clone()).await.unwrap();
    assert!(
        second.patch.as_ref().unwrap()["patch"]
            .as_str()
            .unwrap()
            .contains("+value = 2")
    );
    let mut refresh = fixture.resume(review_turn, &review);
    refresh.coding.review_input = fixture.review(&second).coding.review_input;
    refresh.coding.prompt = "VERIFY_READ_ONLY EXPECT_TWO".into();
    refresh.coding.thread.as_mut().unwrap().close_after_turn = true;
    let reviewed = fixture.run(refresh.clone()).await.unwrap();
    assert_eq!(reviewed.coding_thread["state"], "CLOSED");
    assert!(
        fixture
            .run(fixture.resume(refresh, &reviewed))
            .await
            .is_err()
    );
    let mut close = fixture.resume(follow, &second);
    close.coding.thread.as_mut().unwrap().mode = CodingThreadMode::Close;
    let closed = fixture.run(close).await.unwrap();
    assert_eq!(closed.coding_thread["state"], "CLOSED");
    assert!(closed.authentication.is_none());
}

#[tokio::test]
async fn rejected_artifacts_do_not_commit_a_checkpoint() {
    let fixture = Fixture::new();
    let mut turn = fixture.turn.clone();
    turn.coding.prompt = "FIRST_EDIT PROTECTED_EDIT".into();
    assert!(fixture.run(turn.clone()).await.is_err());
    assert!(fixture.run(turn).await.is_err());
    let mut valid = fixture.turn.clone();
    valid.coding.thread.as_mut().unwrap().session_ref = Uuid::new_v4();
    let artifact = fixture.run(valid).await.unwrap();
    let mut review = fixture.review(&artifact);
    review.coding.prompt = "WRONG_REVIEW".into();
    assert!(fixture.run(review.clone()).await.is_err());
    assert!(fixture.run(review).await.is_err());
}

#[tokio::test]
async fn profile_owner_and_model_confusion_fail_without_consuming_checkpoint() {
    let mut fixture = Fixture::new();
    let first = fixture.run(fixture.turn.clone()).await.unwrap();
    let valid = fixture.resume(fixture.turn.clone(), &first);
    let mut wrong = valid.clone();
    wrong.native_model = Some("unadmitted".into());
    assert!(fixture.run(wrong).await.is_err());
    let mut wrong = valid.clone();
    wrong.policy.permission_mode = PermissionMode::DontAsk;
    assert!(fixture.run(wrong).await.is_err());
    let mut wrong = valid.clone();
    wrong.coding.authentication_profile = CodingAuthenticationProfile::EnterpriseApi;
    assert!(fixture.run(wrong).await.is_err());
    fixture.host.thread_scope = format!("sha256:{}", "9".repeat(64));
    assert!(fixture.run(valid.clone()).await.is_err());
    fixture.host.thread_scope = format!("sha256:{}", "1".repeat(64));
    let mut good = valid;
    good.coding.prompt = "SECOND_EDIT".into();
    fixture.run(good).await.unwrap();
}

#[tokio::test]
async fn bundle_and_manifest_mismatch_fail_before_session_creation() {
    let mut fixture = Fixture::new();
    let mut turn = fixture.turn.clone();
    turn.coding.repository_digest = format!("sha256:{}", "9".repeat(64));
    assert!(fixture.run(turn).await.is_err());
    fixture
        .manifest
        .effective_instructions
        .push("changed".into());
    assert!(fixture.run(fixture.turn.clone()).await.is_err());
    assert!(
        !fixture
            .host
            .native_home
            .join("light-worker-threads")
            .exists()
    );
}

#[tokio::test]
async fn remediation_digest_must_match_accepted_implementation() {
    let fixture = Fixture::new();
    let first = fixture.run(fixture.turn.clone()).await.unwrap();
    let mut follow = fixture.resume(fixture.turn.clone(), &first);
    follow.coding.prompt = "SECOND_EDIT".into();
    follow.coding.remediation = Some(Box::new(serde_json::from_value(json!({
        "priorReview": { "schemaVersion":1,"reviewId":"review-1","artifactDigest":format!("sha256:{}", "0".repeat(64)),
            "verdict":"changes-required","findings":[{"findingId":"f1","severity":"high","repository":"example/repo",
                "location":"main.py","summary":"change value","evidence":"value is one","requiredResolution":"set two"}],"validationGaps":[] }
    })).unwrap()));
    assert!(
        fixture
            .run(follow.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("another checkpoint")
    );
    follow
        .coding
        .remediation
        .as_mut()
        .unwrap()
        .prior_review
        .artifact_digest = first.patch.as_ref().unwrap()["patchDigest"]
        .as_str()
        .unwrap()
        .into();
    let second = fixture.run(follow).await.unwrap();
    assert!(
        second.patch.unwrap()["patch"]
            .as_str()
            .unwrap()
            .contains("+value = 2")
    );
}

#[tokio::test]
async fn explicit_close_needs_neither_native_binary_nor_repository_bundle() {
    let mut fixture = Fixture::new();
    let first = fixture.run(fixture.turn.clone()).await.unwrap();
    let mut close = fixture.resume(fixture.turn.clone(), &first);
    close.coding.thread.as_mut().unwrap().mode = CodingThreadMode::Close;
    fixture.host.executable = fixture._root.path().join("missing-cli");
    fixture.bundle = fixture._root.path().join("missing-bundle");
    let closed = fixture.run(close).await.unwrap();
    assert_eq!(closed.coding_thread["state"], "CLOSED");
    assert!(closed.authentication.is_none());
}

#[test]
fn native_claude_authentication_is_never_api_billing_evidence() {
    let mut evidence = CodingAuthenticationEvidence {
        profile: CodingAuthenticationProfile::PersonalSubscription,
        credential_source: CodingCredentialSource::NativeClaudeStore,
        credential_generation: None,
        authoritative_usage: false,
    };
    evidence.validate().unwrap();
    evidence.authoritative_usage = true;
    assert!(evidence.validate().is_err());
    evidence.authoritative_usage = false;
    evidence.profile = CodingAuthenticationProfile::EnterpriseApi;
    assert!(evidence.validate().is_err());
}

#[test]
fn observed_thinking_advisory_cannot_bypass_session_or_terminal_checks() {
    let mut stream = Stream::new("session", "model");
    assert!(
        stream
            .feed(
                b"{\"type\":\"system\",\"subtype\":\"thinking_tokens\",\"session_id\":\"session\"}"
            )
            .unwrap()
            .is_empty()
    );
    assert!(
        stream
            .feed(b"{\"type\":\"system\",\"subtype\":\"thinking_tokens\",\"session_id\":\"other\"}")
            .is_err()
    );
    assert!(stream.finish().is_err());
}

#[tokio::test]
async fn namespace_mounts_only_the_current_workspace_and_native_state() {
    let fixture = Fixture::new();
    let (_tx, rx) = watch::channel(false);
    let workspace = Workspace::new(
        &fixture.bundle,
        &fixture.turn.coding,
        rx,
        Instant::now() + Duration::from_secs(10),
    )
    .await
    .unwrap();
    let host = HostContext {
        executable: std::fs::canonicalize("/usr/bin/python3").unwrap(),
        native_home: fixture.host.native_home.clone(),
        working_directory: workspace.repository.clone(),
        thread_scope: fixture.host.thread_scope.clone(),
    };
    let mut command = Command::new(&host.executable);
    command
        .current_dir(&workspace.repository)
        .arg("-c")
        .arg("import os; assert os.path.isfile('/etc/resolv.conf'); print('namespace-ready')");
    workspace
        .confine(
            &mut command,
            &host,
            &fixture.turn.coding,
            PermissionSource::AgentPolicy,
        )
        .unwrap();
    let output = command.output().await.unwrap();
    assert!(
        output.status.success(),
        "synthetic namespace diagnostic: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "namespace-ready"
    );
}

#[tokio::test]
async fn cancellation_kills_native_namespace_descendants_before_they_can_write() {
    let fixture = Fixture::new();
    let mut turn = fixture.turn.clone();
    turn.coding.prompt = "WAIT_NAMESPACE".into();
    let key = agent_runtime_protocol::canonical_digest(
        &json!({"home":fixture.host.native_home,"scope":fixture.host.thread_scope,
        "session":turn.coding.thread.as_ref().unwrap().session_ref,"role":turn.coding.role}),
    )
    .unwrap();
    let marker = fixture
        .host
        .native_home
        .parent()
        .unwrap()
        .join(format!(
            ".light-claude-native-{}",
            key.trim_start_matches("sha256:")
        ))
        .join("config/late-child");
    let (cancel_tx, cancel) = watch::channel(false);
    let (events, mut rx) = mpsc::channel(64);
    let work = execute_coding_inner(
        &fixture.host,
        &fixture.bundle,
        &fixture.manifest,
        turn,
        cancel,
        Instant::now() + Duration::from_secs(10),
        events,
        &fixture.digest,
    );
    tokio::pin!(work);
    tokio::select! {
        Some(Event::Initialized{..})=rx.recv()=>{cancel_tx.send(true).unwrap();},
        result=&mut work=>panic!("native namespace did not start: {}",result.is_ok()),
    }
    assert!(work.await.is_err());
    tokio::time::sleep(Duration::from_millis(2200)).await;
    assert!(
        !marker.exists(),
        "cancelled native child escaped the namespace lifetime"
    );
}
