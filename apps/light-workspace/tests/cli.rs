use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Stdio},
};

fn invoke(home: &Path, args: &[&str], request: Option<Value>) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_light-workspace"))
        .args(args)
        .env("HOME", home)
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", home.join("bin").display()),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(request) = request {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(request.to_string().as_bytes())
            .unwrap();
    } else {
        drop(child.stdin.take());
    }
    child.wait_with_output().unwrap()
}
fn call(home: &Path, root: &str, agent: &str, request: Value) -> Value {
    let result = invoke(home, &[root, "call", "portal", agent], Some(request));
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    serde_json::from_slice(&result.stdout).unwrap()
}
fn setup() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    fs::create_dir(temp.path().join("bin")).unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let git = |args: &[&str]| {
        let result = Command::new("git")
            .arg("-C")
            .arg(&source)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    };
    git(&["init", "-b", "develop"]);
    fs::write(source.join("README.md"), "base\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-m", "base"]);
    // The repository identity remains GitHub, but all Git traffic is redirected
    // to this disposable local remote. These tests never contact GitHub.
    fs::write(temp.path().join(".gitconfig"), format!("[user]\n name = Test\n email = test@example.invalid\n[url \"{}\"]\n insteadOf = https://github.com/test/repo.git\n", source.display())).unwrap();
    let registration = json!({"schemaVersion":1,"id":"portal","hostId":"test","agents":["codex","claude"],"operations":["edit","review","commit","push","issue","pull-request"],"repositories":[{"name":"repo","source":"https://github.com/test/repo.git","integrationBranch":"develop","releaseBranch":"master"}]});
    fs::write(temp.path().join("workspace.json"), registration.to_string()).unwrap();
    let out = invoke(
        temp.path(),
        &[
            temp.path().join("store").to_str().unwrap(),
            "register",
            temp.path().join("workspace.json").to_str().unwrap(),
        ],
        None,
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    temp
}

#[test]
fn stdio_binds_identity_and_returns_tool_errors_without_breaking_rpc() {
    let temp = setup();
    let root = temp.path().join("store");
    let mut child = Command::new(env!("CARGO_BIN_EXE_light-workspace"))
        .args([root.to_str().unwrap(), "serve", "portal", "not-granted"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let messages = [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"task_workspace","arguments":{"operation":"create","task":"one"}}}),
    ];
    let mut input = child.stdin.take().unwrap();
    for message in messages {
        writeln!(input, "{message}").unwrap();
    }
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let responses: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(responses.len(), 3);
    assert_eq!(responses[0]["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(responses[1]["result"]["tools"][0]["name"], "task_workspace");
    assert_eq!(responses[2]["result"]["isError"], true);
    assert!(
        responses[2]["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("grant")
    );
}

#[test]
fn review_push_and_pr_are_idempotent_and_pr_targets_develop() {
    let temp = setup();
    let home = temp.path();
    let root_path = home.join("store");
    let root = root_path.to_str().unwrap();
    let mock = home.join("bin/gh");
    fs::write(
        &mock,
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$HOME/gh-calls"
if [ "$2" = view ]; then cat "$HOME/pr-view.json"; exit 0; fi
cat > "$HOME/gh-body"
printf 'https://github.com/test/repo/pull/7\n'
"#,
    )
    .unwrap();
    fs::set_permissions(&mock, fs::Permissions::from_mode(0o700)).unwrap();
    call(
        home,
        root,
        "codex",
        json!({"operation":"create","task":"task-1"}),
    );
    call(
        home,
        root,
        "codex",
        json!({"operation":"edit","task":"task-1","edit":{"repository":"repo","path":"new.txt","content":"reviewed\n","expectedDigest":null}}),
    );
    let frozen = call(
        home,
        root,
        "codex",
        json!({"operation":"freeze","task":"task-1"}),
    );
    call(
        home,
        root,
        "claude",
        json!({"operation":"review","task":"task-1","checkpoint":frozen["checkpoint"]["digest"],"approved":true,"findings":"passed"}),
    );
    let committed = call(
        home,
        root,
        "codex",
        json!({"operation":"commit","task":"task-1","message":"Reviewed change"}),
    );
    fs::write(
        home.join("pr-view.json"),
        json!({"headRefOid":committed["checkouts"][0]["commit"],"baseRefName":"develop"})
            .to_string(),
    )
    .unwrap();
    let push = call(
        home,
        root,
        "codex",
        json!({"operation":"push","task":"task-1"}),
    );
    assert_eq!(push[0]["state"], "succeeded");
    assert_eq!(
        call(
            home,
            root,
            "codex",
            json!({"operation":"push","task":"task-1"})
        ),
        push
    );
    let request = json!({"operation":"github","task":"task-1","repository":"repo","action":"pull-request","title":"Task 1","body":"Reviewed across agents.\n\nCloses #1"});
    let result = call(home, root, "codex", request.clone());
    assert_eq!(result["resource"], "https://github.com/test/repo/pull/7");
    assert_eq!(call(home, root, "codex", request), result);
    let calls = fs::read_to_string(home.join("gh-calls")).unwrap();
    assert_eq!(calls.lines().count(), 2);
    assert!(calls.contains("--base develop"));
    assert!(
        fs::read_to_string(home.join("gh-body"))
            .unwrap()
            .contains("\n\nCloses #1\n\n<!-- light-workspace")
    );
}

#[test]
fn uncertain_github_create_is_not_repeated() {
    let temp = setup();
    let home = temp.path();
    let root_path = home.join("store");
    let root = root_path.to_str().unwrap();
    let mock = home.join("bin/gh");
    fs::write(&mock, "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$HOME/gh-calls\"\nif [ \"$2\" = create ]; then cat >/dev/null; exit 1; fi\nprintf '[]\\n'\n").unwrap();
    fs::set_permissions(&mock, fs::Permissions::from_mode(0o700)).unwrap();
    call(
        home,
        root,
        "codex",
        json!({"operation":"create","task":"issue"}),
    );
    let request = json!({"operation":"github","task":"issue","repository":"repo","action":"issue","title":"Issue","body":"Description"});
    for _ in 0..2 {
        assert!(
            !invoke(
                home,
                &[root, "call", "portal", "codex"],
                Some(request.clone())
            )
            .status
            .success()
        );
    }
    let calls = fs::read_to_string(home.join("gh-calls")).unwrap();
    assert_eq!(
        calls
            .lines()
            .filter(|line| line.starts_with("issue create"))
            .count(),
        1
    );
    assert_eq!(
        calls
            .lines()
            .filter(|line| line.starts_with("api repos/test/repo/issues?"))
            .count(),
        1
    );
}

#[test]
fn oversized_and_invalid_frames_do_not_end_session() {
    let temp = setup();
    let root = temp.path().join("store");
    let mut child = Command::new(env!("CARGO_BIN_EXE_light-workspace"))
        .args([root.to_str().unwrap(), "serve", "portal", "codex"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    input.write_all(&vec![b'x'; 2 * 1024 * 1024 + 10]).unwrap();
    input.write_all(b"\n\xff\n").unwrap();
    writeln!(input, "{}", json!({"jsonrpc":"2.0","id":9,"method":"ping"})).unwrap();
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let responses: Vec<Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(responses.len(), 3);
    assert_eq!(responses[0]["error"]["code"], -32600);
    assert_eq!(responses[1]["error"]["code"], -32700);
    assert_eq!(responses[2]["id"], 9);
    assert!(responses[2].get("result").is_some());
}

#[test]
fn operator_recovery_checks_fenced_generation() {
    let temp = setup();
    let home = temp.path();
    let root_path = home.join("store");
    let root = root_path.to_str().unwrap();
    let mut task = call(
        home,
        root,
        "codex",
        json!({"operation":"create","task":"recover"}),
    );
    task["state"] = json!("interrupted");
    task["generation"] = json!(7);
    fs::write(
        root_path.join("portal/tasks/recover/task.json"),
        task.to_string(),
    )
    .unwrap();
    assert!(
        !invoke(
            home,
            &[
                root,
                "recover",
                "portal",
                "recover",
                "codex",
                "--fenced-generation",
                "6"
            ],
            None
        )
        .status
        .success()
    );
    let recovered = invoke(
        home,
        &[
            root,
            "recover",
            "portal",
            "recover",
            "codex",
            "--fenced-generation",
            "7",
        ],
        None,
    );
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    let recovered: Value = serde_json::from_slice(&recovered.stdout).unwrap();
    assert_eq!(recovered["state"], "ready");
    assert_eq!(recovered["generation"], 8);
}

#[test]
fn uncertain_issue_reconciles_exact_marker_on_later_page() {
    let temp = setup();
    let home = temp.path();
    let root_path = home.join("store");
    let root = root_path.to_str().unwrap();
    let mock = home.join("bin/gh");
    fs::write(
        &mock,
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$HOME/gh-calls"
if [ "$2" = create ]; then cat > "$HOME/gh-body"; exit 1; fi
case "$2" in
  *page=1) cat "$HOME/page1.json" ;;
  *page=2) cat "$HOME/page2.json" ;;
  *) exit 2 ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&mock, fs::Permissions::from_mode(0o700)).unwrap();
    call(
        home,
        root,
        "codex",
        json!({"operation":"create","task":"issue-page"}),
    );
    let request = json!({"operation":"github","task":"issue-page","repository":"repo","action":"issue","title":"Issue","body":"Description"});
    assert!(
        !invoke(
            home,
            &[root, "call", "portal", "codex"],
            Some(request.clone())
        )
        .status
        .success()
    );
    let body = fs::read_to_string(home.join("gh-body")).unwrap();
    let mut page1 =
        vec![json!({"html_url":"https://github.com/test/repo/issues/1","body":"unrelated"}); 99];
    page1.push(
        json!({"html_url":"https://github.com/test/repo/pull/2","body":body,"pull_request":{}}),
    );
    fs::write(home.join("page1.json"), serde_json::to_vec(&page1).unwrap()).unwrap();
    fs::write(
        home.join("page2.json"),
        json!([{"html_url":"https://github.com/test/repo/issues/7","body":body}]).to_string(),
    )
    .unwrap();
    let result = call(home, root, "codex", request.clone());
    assert_eq!(result["state"], "succeeded");
    assert_eq!(result["resource"], "https://github.com/test/repo/issues/7");
    assert_eq!(call(home, root, "codex", request), result);
    let calls = fs::read_to_string(home.join("gh-calls")).unwrap();
    assert_eq!(calls.lines().count(), 3);
    assert!(!calls.contains("--search"));
}
