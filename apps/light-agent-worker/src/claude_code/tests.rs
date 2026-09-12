use super::*;
use coding_agent_runtime::{CodingRole, CodingRoleExecutionProfile, CodingThreadControl};
use std::{collections::BTreeSet, os::unix::fs::PermissionsExt};

fn policy() -> LaunchPolicy {
    LaunchPolicy {
        permission_source: PermissionSource::ClaudeCli,
        permission_mode: PermissionMode::Inherit,
        default_model: "sonnet".into(),
        models: BTreeMap::from([
            ("sonnet".into(), "claude-sonnet-5".into()),
            ("opus".into(), "claude-opus-test".into()),
        ]),
        tools: vec![],
        allowed_tools: vec![],
    }
}
fn turn() -> ClaudeTurn {
    ClaudeTurn {
        coding: CodingTurnSpec {
            thread: Some(CodingThreadControl {
                runner_id: "runner".into(),
                session_ref: Uuid::new_v4(),
                stage_id: "stage1".into(),
                mode: CodingThreadMode::New,
                expected_checkpoint: None,
                close_after_turn: false,
            }),
            repository_digest: format!("sha256:{:064x}", 1),
            base_revision: "a".repeat(40),
            workspace_root: "/workspace/repo".into(),
            prompt: "hello".into(),
            model_alias: "coding-implementer".into(),
            authentication_profile: CodingAuthenticationProfile::PersonalSubscription,
            role: CodingRole::Implement,
            role_profile: CodingRoleExecutionProfile::pinned(CodingRole::Implement),
            review_input: None,
            remediation: None,
            materialization_manifest_digest: format!("sha256:{:064x}", 2),
            writable_roots: BTreeSet::from(["/workspace/repo".into()]),
            allowed_tools: CodingTurnSpec::supported_tools(CodingRole::Implement),
            maximum_patch_bytes: 4096,
            maximum_changed_files: 10,
        },
        policy: policy(),
        native_model: None,
    }
}
fn fixture() -> (tempfile::TempDir, HostContext, String) {
    let root = tempfile::tempdir().unwrap();
    let executable = root.path().join("claude-fixture");
    std::fs::write(&executable, r#"#!/usr/bin/python3
import sys,json,time,os
args=sys.argv[1:]
if args == ['--version']:
 print('2.1.269 (Claude Code)'); sys.exit(0)
if args == ['auth','status']:
 print(json.dumps({'loggedIn':True,'authMethod':'claude.ai','apiProvider':'firstParty','email':'SECRET'}));sys.exit(0)
prompt=sys.stdin.read()
if prompt == 'orphan':
 import subprocess
 p=subprocess.Popen(['/bin/sleep','30'],stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
 open('descendant.pid','w').write(str(p.pid))
if prompt == 'sleep':
 time.sleep(30)
if prompt == 'flood':
 print('x'*1100000);sys.exit(0)
if prompt == 'stderr':
 sys.stderr.write('SECRET'*12000);sys.exit(0)
sid=args[args.index('--resume')+1] if '--resume' in args else args[args.index('--session-id')+1]
model=args[args.index('--model')+1]
model={'sonnet':'claude-sonnet-5','opus':'claude-opus-test'}[model]
permission='bypassPermissions' if '--dangerously-skip-permissions' in args else 'dontAsk'
print(json.dumps({'type':'system','subtype':'init','session_id':sid,'model':model,'permissionMode':permission}))
if prompt == 'malformed':
 print('{SECRET');sys.exit(0)
if prompt == 'missing':
 sys.exit(0)
if prompt == 'burst':
 for i in range(512):
  print(json.dumps({'type':'stream_event','session_id':sid,'event':{'delta':{'type':'text_delta','text':'hello'}}}))
print(json.dumps({'type':'stream_event','session_id':sid,'event':{'delta':{'type':'text_delta','text':'hello'}}}))
print(json.dumps({'type':'result','subtype':'success','is_error':False,'session_id':sid,'result':'hello','usage':{'input_tokens':10,'secret':'SECRET'},'permission_denials':[]}))
if prompt == 'nonzero':
 sys.exit(1)
"#).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let digest = hex::encode(Sha256::digest(std::fs::read(&executable).unwrap()));
    let home = root.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let host = HostContext {
        executable,
        native_home: home,
        working_directory: root.path().into(),
        thread_scope: format!("sha256:{:064x}", 3),
    };
    (root, host, digest)
}
async fn attempt(host: &HostContext, turn: ClaudeTurn, digest: &str) -> Result<Outcome> {
    let (_tx, rx) = watch::channel(false);
    let (events, _reader) = mpsc::channel(64);
    execute_inner(
        host,
        turn,
        rx,
        Instant::now() + Duration::from_secs(5),
        events,
        digest,
    )
    .await
}
fn accept(outcome: Outcome) -> Value {
    match outcome {
        Outcome::Proposal(p) => {
            assert_eq!(p.result.text, "hello");
            assert!(
                !p.result
                    .usage
                    .as_ref()
                    .unwrap()
                    .to_string()
                    .contains("SECRET")
            );
            p.accept_validated("").unwrap()
        }
        _ => panic!("expected proposal"),
    }
}
fn resume(turn: &mut ClaudeTurn, receipt: &Value) {
    let control = turn.coding.thread.as_mut().unwrap();
    control.mode = CodingThreadMode::Resume;
    control.expected_checkpoint =
        Some(serde_json::from_value(receipt["checkpoint"].clone()).unwrap());
}
#[test]
fn policy_and_launch_are_explicit() {
    let mut p = policy();
    assert_eq!(p.resolve(None).unwrap(), "sonnet");
    assert!(p.resolve(Some("unknown")).is_err());
    let args = arguments(&p, "sonnet", "id", CodingThreadMode::Resume);
    assert!(args.contains(&"--resume".into()));
    assert!(!args.contains(&"--permission-mode".into()));
    assert!(!args.contains(&"--safe-mode".into()));
    assert!(!args.contains(&"--continue".into()));
    p.permission_mode = PermissionMode::BypassPermissions;
    assert!(
        arguments(&p, "sonnet", "id", CodingThreadMode::New)
            .contains(&"--dangerously-skip-permissions".into())
    );
    p.permission_source = PermissionSource::AgentPolicy;
    p.permission_mode = PermissionMode::DontAsk;
    p.tools = vec!["Read".into(), "Bash".into()];
    let args = arguments(&p, "sonnet", "id", CodingThreadMode::New);
    assert!(args.contains(&"--safe-mode".into()));
    assert!(args.contains(&"Read,Bash".into()));
    p.permission_mode = PermissionMode::Inherit;
    assert!(p.resolve(None).is_err());
    let mut json = serde_json::to_value(turn()).unwrap();
    json["permissionOverride"] = json!(true);
    assert!(serde_json::from_value::<ClaudeTurn>(json).is_err());
}
#[test]
fn stream_rejects_unknown_identity_duplicate_and_missing_terminal() {
    let init = json!({"type":"system","subtype":"init","session_id":"s","model":"m"});
    let result = json!({"type":"result","subtype":"success","is_error":false,"session_id":"s","result":"ok"});
    let mut stream = Stream::new("s", "m");
    assert!(stream.feed(b"{SECRET").is_err());
    assert!(stream.feed(br#"{"type":"control_request"}"#).is_err());
    assert!(stream.feed(&serde_json::to_vec(&init).unwrap()).is_ok());
    assert!(stream.feed(&serde_json::to_vec(&init).unwrap()).is_err());
    let mut wrong = result.clone();
    wrong["session_id"] = json!("other");
    assert!(stream.feed(&serde_json::to_vec(&wrong).unwrap()).is_err());
    stream.feed(&serde_json::to_vec(&result).unwrap()).unwrap();
    assert!(stream.feed(&serde_json::to_vec(&result).unwrap()).is_err());
    assert_eq!(stream.finish().unwrap().text, "ok");
    assert!(Stream::new("s", "m").finish().is_err());
}
#[tokio::test]
async fn new_resume_stale_policy_and_close() {
    let (_root, host, digest) = fixture();
    let mut t = turn();
    let receipt = accept(attempt(&host, t.clone(), &digest).await.unwrap());
    assert!(attempt(&host, t.clone(), &digest).await.is_err());
    resume(&mut t, &receipt);
    let mut bad = t.clone();
    bad.policy.permission_mode = PermissionMode::BypassPermissions;
    assert!(attempt(&host, bad, &digest).await.is_err());
    let mut bad = t.clone();
    bad.coding.thread.as_mut().unwrap().expected_checkpoint = Some(Uuid::new_v4());
    assert!(attempt(&host, bad, &digest).await.is_err());
    let receipt = accept(attempt(&host, t.clone(), &digest).await.unwrap());
    resume(&mut t, &receipt);
    t.coding.thread.as_mut().unwrap().mode = CodingThreadMode::Close;
    // Closing does not spawn or authenticate a vendor process.
    std::fs::remove_file(&host.executable).unwrap();
    match attempt(&host, t.clone(), &digest).await.unwrap() {
        Outcome::Closed(r) => assert_eq!(r["state"], "CLOSED"),
        _ => panic!(),
    }
    assert!(attempt(&host, t, &digest).await.is_err());
}
#[tokio::test]
async fn preflight_rejection_preserves_checkpoint_but_unaccepted_result_does_not() {
    let (_root, host, digest) = fixture();
    let mut t = turn();
    let receipt = accept(attempt(&host, t.clone(), &digest).await.unwrap());
    resume(&mut t, &receipt);
    t.native_model = Some("unknown".into());
    assert!(attempt(&host, t.clone(), &digest).await.is_err());
    t.native_model = None;
    let proposal = attempt(&host, t.clone(), &digest).await.unwrap();
    // The session lock prevents simultaneous execution even after CLI exit.
    assert!(attempt(&host, t.clone(), &digest).await.is_err());
    drop(proposal);
    assert!(attempt(&host, t, &digest).await.is_err());
}
#[tokio::test]
async fn malformed_flood_stderr_nonzero_and_missing_fail_without_secret_diagnostics() {
    for prompt in ["malformed", "flood", "stderr", "nonzero", "missing"] {
        let (_root, host, digest) = fixture();
        let mut t = turn();
        t.coding.prompt = prompt.into();
        let error = attempt(&host, t.clone(), &digest)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(!error.contains("SECRET"));
        assert!(attempt(&host, t, &digest).await.is_err());
    }
}
#[tokio::test]
async fn cancellation_deadline_and_event_backpressure() {
    for cancel_now in [true, false] {
        let (_root, host, digest) = fixture();
        let mut t = turn();
        t.coding.prompt = "sleep".into();
        let (tx, rx) = watch::channel(false);
        let (events, _reader) = mpsc::channel(10);
        let deadline = Instant::now() + Duration::from_millis(300);
        let work = execute_inner(&host, t, rx, deadline, events, &digest);
        if cancel_now {
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let _ = tx.send(true);
            });
        }
        let error = work.await.err().unwrap().to_string();
        assert!(error.contains("cancel") || error.contains("deadline"));
    }
    let (_root, host, digest) = fixture();
    let (_tx, rx) = watch::channel(false);
    let (events, _reader) = mpsc::channel(1);
    assert!(
        execute_inner(
            &host,
            turn(),
            rx,
            Instant::now() + Duration::from_secs(3),
            events,
            &digest
        )
        .await
        .err()
        .unwrap()
        .to_string()
        .contains("deadline")
    );
}
#[test]
fn candidate_never_changes_production_capabilities() {
    assert_eq!(crate::capabilities().adapter_id, "codex-app-server-v1");
    assert!(
        !crate::capabilities()
            .actions
            .contains("coding.claude-code-v1")
    );
}

#[tokio::test]
async fn explicit_model_survives_omitted_resume_and_cannot_switch() {
    let (_root, host, digest) = fixture();
    let mut t = turn();
    t.native_model = Some("opus".into());
    let receipt = accept(attempt(&host, t.clone(), &digest).await.unwrap());
    resume(&mut t, &receipt);
    t.native_model = Some("sonnet".into());
    assert!(attempt(&host, t.clone(), &digest).await.is_err());
    t.native_model = None;
    let outcome = attempt(&host, t, &digest).await.unwrap();
    match outcome {
        Outcome::Proposal(p) => assert_eq!(p.result.model, "claude-opus-test"),
        _ => panic!(),
    }
}
#[tokio::test]
async fn close_after_turn_and_scope_fencing() {
    let (_root, mut host, digest) = fixture();
    let mut t = turn();
    let receipt = accept(attempt(&host, t.clone(), &digest).await.unwrap());
    resume(&mut t, &receipt);
    host.thread_scope = format!("sha256:{:064x}", 4);
    assert!(attempt(&host, t.clone(), &digest).await.is_err());
    host.thread_scope = format!("sha256:{:064x}", 3);
    t.coding.thread.as_mut().unwrap().close_after_turn = true;
    assert_eq!(
        accept(attempt(&host, t.clone(), &digest).await.unwrap())["state"],
        "CLOSED"
    );
    assert!(attempt(&host, t, &digest).await.is_err());
}
#[test]
fn permission_denials_and_full_messages_do_not_duplicate_deltas() {
    let mut stream = Stream::new("s", "m");
    stream
        .feed(br#"{"type":"system","subtype":"init","session_id":"s","model":"m"}"#)
        .unwrap();
    let events = stream
        .feed(br#"{"type":"assistant","message":{"content":[{"type":"text","text":"hello"}]}}"#)
        .unwrap();
    assert!(events.is_empty());
    stream
        .feed(br#"{"type":"system","subtype":"permission_denied"}"#)
        .unwrap();
    stream.feed(br#"{"type":"result","subtype":"success","is_error":false,"session_id":"s","result":"hello"}"#).unwrap();
    assert_eq!(stream.finish().unwrap().permission_denials, 1);
}

#[tokio::test]
async fn normal_exit_kills_descendants_that_closed_their_pipes() {
    let (_root, host, digest) = fixture();
    let mut t = turn();
    t.coding.prompt = "orphan".into();
    accept(attempt(&host, t, &digest).await.unwrap());
    let pid = std::fs::read_to_string(host.working_directory.join("descendant.pid")).unwrap();
    for _ in 0..50 {
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid.trim()));
        if stat.as_ref().is_err() || stat.unwrap().split_whitespace().nth(2) == Some("Z") {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("descendant survived process-group cleanup");
}
#[test]
fn pinned_launch_contract_matches_runtime_limits() {
    let contract: Value = serde_json::from_str(LAUNCH_CONTRACT).unwrap();
    assert_eq!(contract["adapterId"], ADAPTER_ID);
    assert_eq!(contract["binarySha256"], BINARY_SHA256);
    assert_eq!(contract["frameBytes"], FRAME);
    assert_eq!(contract["outputBytes"], OUTPUT);
    assert_eq!(contract["stderrBytes"], STDERR);
    assert_eq!(contract["productionQualified"], false);
    assert_eq!(contract["productionActions"], json!([]));
}

#[tokio::test]
async fn explicit_permission_modes_execute_and_commit_in_synthetic_process() {
    for mode in [PermissionMode::DontAsk, PermissionMode::BypassPermissions] {
        let (_root, host, digest) = fixture();
        let mut t = turn();
        t.policy.permission_source = PermissionSource::AgentPolicy;
        t.policy.permission_mode = mode;
        t.policy.tools = vec!["Read".into(), "Bash".into()];
        t.policy.allowed_tools = vec!["Bash(cargo test *)".into()];
        accept(attempt(&host, t, &digest).await.unwrap());
    }
}
#[test]
fn observed_permission_override_must_match() {
    let mut stream = Stream::new("s", "m");
    stream.expected_permission = Some("dontAsk");
    assert!(stream.feed(br#"{"type":"system","subtype":"init","session_id":"s","model":"m","permissionMode":"bypassPermissions"}"#).is_err());
}
#[test]
fn phase0_fixture_is_accepted_by_rust_parser() {
    let mut stream = Stream::new("019a0000-0000-7000-8000-000000000001", "claude-sonnet-5");
    for line in
        include_str!("../../../../contracts/claude-code/v2.1.269/fixtures/new-success.jsonl")
            .lines()
    {
        stream.feed(line.as_bytes()).unwrap();
    }
    assert_eq!(stream.finish().unwrap().text, "synthetic-marker");
}

#[tokio::test]
async fn burst_events_wait_for_slow_consumer_without_aborting_turn() {
    let (_root, host, digest) = fixture();
    let mut request = turn();
    request.coding.prompt = "burst".into();
    let (_cancel, rx) = watch::channel(false);
    let (events, mut reader) = mpsc::channel(64);
    let work = execute_inner(
        &host,
        request,
        rx,
        Instant::now() + Duration::from_secs(10),
        events,
        &digest,
    );
    let drain = async {
        let mut count = 0;
        while reader.recv().await.is_some() {
            count += 1;
            tokio::task::yield_now().await;
        }
        count
    };
    let (result, count) = tokio::join!(work, drain);
    assert!(result.is_ok(), "{:?}", result.err());
    assert_eq!(count, 514);
}
