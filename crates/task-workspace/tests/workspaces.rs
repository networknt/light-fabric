use std::{collections::BTreeSet, fs, path::Path, process::Command};
use task_workspace::{Operation, Repository, TaskState, Workspace, WorkspaceStore};
use tempfile::TempDir;

fn git(path: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Workspace Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Workspace Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}
fn setup() -> (TempDir, WorkspaceStore, Workspace) {
    let temp = tempfile::tempdir().unwrap();
    let mut repositories = Vec::new();
    for name in ["backend", "frontend", "docs"] {
        let path = temp.path().join(name);
        fs::create_dir(&path).unwrap();
        git(&path, &["init", "-b", "develop"]);
        git(&path, &["config", "user.name", "Workspace Test"]);
        git(&path, &["config", "user.email", "test@example.invalid"]);
        fs::write(path.join("README.md"), "base\n").unwrap();
        fs::write(path.join(".gitignore"), "build/\n").unwrap();
        git(&path, &["add", "."]);
        git(&path, &["commit", "-m", "base"]);
        repositories.push(Repository {
            name: name.into(),
            source: path.display().to_string(),
            integration_branch: "develop".into(),
            release_branch: "master".into(),
        });
    }
    let store = WorkspaceStore::open(temp.path().join("managed")).unwrap();
    let workspace = Workspace {
        schema_version: 1,
        id: "portal".into(),
        host_id: "host".into(),
        agents: BTreeSet::from(["codex-personal".into(), "claude-personal".into()]),
        repositories,
        indexers: Default::default(),
        operations: BTreeSet::from([
            Operation::Edit,
            Operation::Review,
            Operation::Execute,
            Operation::Commit,
        ]),
    };
    store.register(&workspace).unwrap();
    (temp, store, workspace)
}
#[test]
fn parallel_tasks_have_all_repositories_and_independent_branches() {
    let (_temp, store, _) = setup();
    let first = store
        .create_task("portal", "issue-384", "codex-personal")
        .unwrap();
    let second = store
        .create_task("portal", "issue-391", "claude-personal")
        .unwrap();
    assert_eq!(first.checkouts.len(), 3);
    assert_eq!(second.checkouts.len(), 3);
    for (one, two) in first.checkouts.iter().zip(&second.checkouts) {
        assert_eq!(one.base_commit, two.base_commit);
        assert_eq!(
            git(&one.path, &["branch", "--show-current"]),
            "agent/issue-384"
        );
        assert_eq!(
            git(&two.path, &["branch", "--show-current"]),
            "agent/issue-391"
        );
        fs::write(one.path.join("README.md"), "task one\n").unwrap();
        assert_eq!(
            fs::read_to_string(two.path.join("README.md")).unwrap(),
            "base\n"
        );
    }
    assert_eq!(
        store
            .create_task("portal", "issue-384", "claude-personal")
            .unwrap(),
        first
    );
}
#[test]
fn review_sees_all_uncommitted_files_and_detects_stale_approval() {
    let (_temp, store, _) = setup();
    let task = store
        .create_task("portal", "change", "codex-personal")
        .unwrap();
    for checkout in &task.checkouts {
        fs::write(checkout.path.join("new.bin"), [0, 1, 255]).unwrap();
        fs::remove_file(checkout.path.join("README.md")).unwrap();
    }
    let frozen = store.freeze("portal", "change", "codex-personal").unwrap();
    let checkpoint = frozen.checkpoint.unwrap();
    assert!(
        checkpoint
            .repositories
            .iter()
            .all(|r| r.files.iter().any(|f| f.path == "new.bin"))
    );
    assert!(
        checkpoint
            .repositories
            .iter()
            .all(|r| !r.files.iter().any(|f| f.path == "README.md"))
    );
    fs::write(task.checkouts[0].path.join("new.bin"), [0, 2, 255]).unwrap();
    assert!(
        store
            .review(
                "portal",
                "change",
                "claude-personal",
                &checkpoint.digest,
                true,
                "reviewed".into()
            )
            .is_err()
    );
    store
        .remediate("portal", "change", "codex-personal")
        .unwrap();
    assert_eq!(
        store
            .status("portal", "change", "claude-personal")
            .unwrap()
            .state,
        TaskState::Ready
    );
}
#[test]
fn grant_is_workspace_wide_and_unknown_agents_and_path_traversal_fail() {
    let (_temp, store, _) = setup();
    assert!(store.create_task("portal", "valid", "unknown").is_err());
    assert!(
        store
            .create_task("portal", "../escape", "codex-personal")
            .is_err()
    );
    assert!(
        store
            .create_task("../portal", "valid", "codex-personal")
            .is_err()
    );
    assert_eq!(
        store
            .create_task("portal", "valid", "claude-personal")
            .unwrap()
            .checkouts
            .len(),
        3
    );
}
#[test]
fn registration_is_idempotent_and_rejects_changed_membership() {
    let (_temp, store, mut workspace) = setup();
    store.register(&workspace).unwrap();
    workspace.repositories.pop();
    assert!(store.register(&workspace).is_err());
}
#[test]
fn checkpoints_survive_store_restart_and_include_staging_state() {
    let (temp, store, _) = setup();
    let task = store
        .create_task("portal", "staging", "codex-personal")
        .unwrap();
    fs::write(task.checkouts[0].path.join("README.md"), "changed\n").unwrap();
    let frozen = store.freeze("portal", "staging", "codex-personal").unwrap();
    let store = WorkspaceStore::open(temp.path().join("managed")).unwrap();
    assert_eq!(
        store
            .status("portal", "staging", "claude-personal")
            .unwrap(),
        frozen
    );
    git(&task.checkouts[0].path, &["add", "README.md"]);
    assert!(
        store
            .review(
                "portal",
                "staging",
                "claude-personal",
                &frozen.checkpoint.unwrap().digest,
                true,
                "ok".into()
            )
            .is_err()
    );
}
#[test]
fn symlink_to_host_file_is_not_followed_by_checkpoint() {
    let (_temp, store, _) = setup();
    let task = store
        .create_task("portal", "symlink", "codex-personal")
        .unwrap();
    std::os::unix::fs::symlink("/etc/passwd", task.checkouts[0].path.join("outside")).unwrap();
    assert!(store.freeze("portal", "symlink", "codex-personal").is_err());
}
#[test]
fn reviewed_commits_cover_multiple_repositories_and_retry_is_idempotent() {
    let (_temp, store, _) = setup();
    let task = store
        .create_task("portal", "publish", "codex-personal")
        .unwrap();
    for checkout in &task.checkouts {
        // Administrative committer identity is intentionally not model-provided.
        git(&checkout.path, &["config", "user.name", "Workspace Test"]);
        git(
            &checkout.path,
            &["config", "user.email", "test@example.invalid"],
        );
        fs::write(checkout.path.join("README.md"), "review me\n").unwrap();
        fs::write(checkout.path.join("added.txt"), "also review me\n").unwrap();
    }
    let frozen = store.freeze("portal", "publish", "codex-personal").unwrap();
    store
        .review(
            "portal",
            "publish",
            "claude-personal",
            &frozen.checkpoint.unwrap().digest,
            true,
            "passed".into(),
        )
        .unwrap();
    let committed = store
        .commit("portal", "publish", "codex-personal", "Reviewed task")
        .unwrap();
    assert_eq!(committed.state, TaskState::Committed);
    assert!(
        committed
            .checkouts
            .iter()
            .all(|r| r.commit.as_ref().unwrap() != &r.base_commit)
    );
    assert_eq!(
        store
            .commit("portal", "publish", "codex-personal", "Reviewed task")
            .unwrap(),
        committed
    );
    assert!(
        store
            .commit("portal", "publish", "codex-personal", "Different task")
            .is_err()
    );
}
#[test]
fn changed_content_after_review_cannot_commit() {
    let (_temp, store, _) = setup();
    let task = store
        .create_task("portal", "tampered", "codex-personal")
        .unwrap();
    let frozen = store
        .freeze("portal", "tampered", "codex-personal")
        .unwrap();
    store
        .review(
            "portal",
            "tampered",
            "claude-personal",
            &frozen.checkpoint.unwrap().digest,
            true,
            "passed".into(),
        )
        .unwrap();
    fs::write(task.checkouts[2].path.join("late.txt"), "unreviewed").unwrap();
    assert!(
        store
            .commit("portal", "tampered", "codex-personal", "Nope")
            .is_err()
    );
}

#[test]
fn tool_edits_are_shared_compare_and_swap_and_frozen_for_every_agent() {
    let (_temp, store, _) = setup();
    store
        .create_task("portal", "shared", "codex-personal")
        .unwrap();
    let original = store
        .read_file(
            "portal",
            "shared",
            "claude-personal",
            "backend",
            "README.md",
        )
        .unwrap();
    let edit = task_workspace::FileEdit {
        repository: "backend".into(),
        path: "README.md".into(),
        content: Some("updated by codex\n".into()),
        expected_digest: Some(original.digest),
    };
    store
        .edit_file("portal", "shared", "codex-personal", edit.clone())
        .unwrap();
    assert!(
        store
            .edit_file("portal", "shared", "claude-personal", edit)
            .is_err()
    );
    let value = store
        .read_file(
            "portal",
            "shared",
            "claude-personal",
            "backend",
            "README.md",
        )
        .unwrap();
    assert_eq!(value.content, "updated by codex\n");
    let frozen = store.freeze("portal", "shared", "codex-personal").unwrap();
    let digest = frozen.checkpoint.unwrap().digest;
    assert!(
        store
            .review(
                "portal",
                "shared",
                "codex-personal",
                &digest,
                true,
                "self review".into()
            )
            .is_err()
    );
    for agent in ["codex-personal", "claude-personal"] {
        assert!(
            store
                .edit_file(
                    "portal",
                    "shared",
                    agent,
                    task_workspace::FileEdit {
                        repository: "frontend".into(),
                        path: "new.txt".into(),
                        content: Some("no".into()),
                        expected_digest: None
                    }
                )
                .is_err()
        );
    }
    store
        .review(
            "portal",
            "shared",
            "claude-personal",
            &digest,
            true,
            "independent review".into(),
        )
        .unwrap();
    assert!(
        store
            .edit_file(
                "portal",
                "shared",
                "codex-personal",
                task_workspace::FileEdit {
                    repository: "frontend".into(),
                    path: "new.txt".into(),
                    content: Some("no".into()),
                    expected_digest: None
                }
            )
            .is_err()
    );
}

#[test]
fn tool_cannot_write_git_metadata_or_escape_repository() {
    let (_temp, store, _) = setup();
    store
        .create_task("portal", "paths", "codex-personal")
        .unwrap();
    for path in [
        ".git",
        ".git/config",
        "../other",
        "/tmp/other",
        "a/../../other",
        "",
    ] {
        assert!(
            store
                .edit_file(
                    "portal",
                    "paths",
                    "codex-personal",
                    task_workspace::FileEdit {
                        repository: "backend".into(),
                        path: path.into(),
                        content: Some("bad".into()),
                        expected_digest: None
                    }
                )
                .is_err(),
            "{path}"
        );
    }
}

#[test]
fn altered_git_pointer_is_rejected_before_git_runs() {
    let (_temp, store, _) = setup();
    let task = store
        .create_task("portal", "pointer", "codex-personal")
        .unwrap();
    let other = fs::read(task.checkouts[1].path.join(".git")).unwrap();
    fs::write(task.checkouts[0].path.join(".git"), other).unwrap();
    assert!(store.freeze("portal", "pointer", "codex-personal").is_err());
}

#[test]
fn push_without_operation_grant_is_rejected() {
    let (_temp, store, _) = setup();
    store
        .create_task("portal", "push", "codex-personal")
        .unwrap();
    assert!(store.push("portal", "push", "codex-personal").is_err());
}

#[test]
#[ignore = "requires Linux user namespaces; run explicitly on the runner host"]
fn sandbox_enforces_review_read_only_and_hides_host_home() {
    let (temp, store, _) = setup();
    let task = store
        .create_task("portal", "sandbox", "codex-personal")
        .unwrap();
    let secret = temp.path().join("host-secret");
    fs::write(&secret, "host only").unwrap();
    let script = format!(
        "printf changed > backend/README.md; test ! -e '{}'; test ! -w backend/.git",
        secret.display()
    );
    let run = store
        .run(
            "portal",
            "sandbox",
            "codex-personal",
            task_workspace::Access::Implement,
            Path::new("/usr/bin/sh"),
            &["-c".into(), script],
            10,
        )
        .unwrap();
    assert_eq!(run.exit_code, Some(0), "{}", run.stderr);
    assert_eq!(
        fs::read_to_string(task.checkouts[0].path.join("README.md")).unwrap(),
        "changed"
    );
    let frozen = store.freeze("portal", "sandbox", "codex-personal").unwrap();
    let script = "cat backend/README.md; if printf forbidden > backend/README.md; then exit 1; fi; if printf forbidden > frontend/new.txt; then exit 1; fi";
    let review = store
        .run(
            "portal",
            "sandbox",
            "claude-personal",
            task_workspace::Access::Review,
            Path::new("/usr/bin/sh"),
            &["-c".into(), script.into()],
            10,
        )
        .unwrap();
    assert_eq!(review.exit_code, Some(0), "{}", review.stderr);
    assert_eq!(review.stdout, "changed");
    assert_eq!(
        store.files("portal", "sandbox", "claude-personal").unwrap(),
        frozen.checkpoint.unwrap()
    );
    let before = store
        .review(
            "portal",
            "sandbox",
            "claude-personal",
            &store
                .files("portal", "sandbox", "claude-personal")
                .unwrap()
                .digest,
            true,
            "ok".into(),
        )
        .unwrap();
    let timed = store
        .run(
            "portal",
            "sandbox",
            "claude-personal",
            task_workspace::Access::Review,
            Path::new("/usr/bin/sleep"),
            &["10".into()],
            1,
        )
        .unwrap();
    assert!(timed.timed_out);
    let recovered = store
        .recover_after_fencing("portal", "sandbox", "claude-personal")
        .unwrap();
    assert_eq!(recovered.state, TaskState::Approved);
    assert_eq!(recovered.checkpoint, before.checkpoint);
    assert_eq!(recovered.review, before.review);
}

#[test]
#[ignore = "requires Linux user namespaces; run explicitly on the runner host"]
fn active_writer_blocks_review_and_timeout_requires_fenced_recovery() {
    let (_temp, store, _) = setup();
    store
        .create_task("portal", "lease", "codex-personal")
        .unwrap();
    let background = store.clone();
    let worker = std::thread::spawn(move || {
        background
            .run(
                "portal",
                "lease",
                "codex-personal",
                task_workspace::Access::Implement,
                Path::new("/usr/bin/sh"),
                &["-c".into(), "sleep 10".into()],
                1,
            )
            .unwrap()
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while store
        .status("portal", "lease", "claude-personal")
        .unwrap()
        .state
        != TaskState::Running
    {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(store.freeze("portal", "lease", "claude-personal").is_err());
    assert!(worker.join().unwrap().timed_out);
    assert_eq!(
        store
            .status("portal", "lease", "claude-personal")
            .unwrap()
            .state,
        TaskState::Interrupted
    );
    assert!(store.freeze("portal", "lease", "claude-personal").is_err());
    store
        .recover_after_fencing("portal", "lease", "codex-personal")
        .unwrap();
    assert_eq!(
        store
            .status("portal", "lease", "claude-personal")
            .unwrap()
            .state,
        TaskState::Ready
    );
}

#[test]
fn indexers_use_checkpoint_copies_and_freshness_changes_after_remediation() {
    let (temp, store, mut workspace) = setup();
    workspace.id = "indexed".into();
    let script = temp.path().join("indexer.sh");
    fs::write(
        &script,
        "test -f new.rs || exit 1\ntest ! -f README.md || exit 1\nprintf generated > AGENTS.md\n",
    )
    .unwrap();
    workspace.indexers.insert(
        "gitnexus".into(),
        task_workspace::IndexerCommand {
            executable: "/usr/bin/sh".into(),
            args: vec![script.display().to_string()],
            timeout_seconds: 10,
        },
    );
    store.register(&workspace).unwrap();
    let task = store
        .create_task("indexed", "index", "codex-personal")
        .unwrap();
    for checkout in &task.checkouts {
        fs::remove_file(checkout.path.join("README.md")).unwrap();
        fs::write(checkout.path.join("new.rs"), "fn main() {}\n").unwrap();
    }
    let checkpoint = store
        .freeze("indexed", "index", "codex-personal")
        .unwrap()
        .checkpoint
        .unwrap();
    let receipt = store
        .index("indexed", "index", "claude-personal", "gitnexus")
        .unwrap();
    assert!(receipt.fresh);
    assert_eq!(receipt.repositories.len(), 3);
    assert_eq!(receipt.checkpoint_digest, checkpoint.digest);
    assert!(
        task.checkouts
            .iter()
            .all(|r| !r.path.join("AGENTS.md").exists())
    );
    assert_eq!(
        store.files("indexed", "index", "claude-personal").unwrap(),
        checkpoint
    );
    assert!(
        store
            .index_status("indexed", "index", "claude-personal", "gitnexus")
            .unwrap()
            .fresh
    );
    store
        .remediate("indexed", "index", "codex-personal")
        .unwrap();
    assert!(
        !store
            .index_status("indexed", "index", "claude-personal", "gitnexus")
            .unwrap()
            .fresh
    );
    fs::write(task.checkouts[0].path.join("new.rs"), "fn changed() {}\n").unwrap();
    store.freeze("indexed", "index", "codex-personal").unwrap();
    fs::write(&script, "exit 1\n").unwrap();
    assert!(
        store
            .index("indexed", "index", "claude-personal", "gitnexus")
            .is_err()
    );
    let stale = store
        .index_status("indexed", "index", "claude-personal", "gitnexus")
        .unwrap();
    assert_eq!(stale.root, receipt.root);
    assert!(receipt.root.is_dir());
    let task_root = receipt.root.parent().unwrap();
    let count_roots = || {
        fs::read_dir(task_root)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry.file_type().unwrap().is_dir()
                    && entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with("index-gitnexus-")
            })
            .count()
    };
    assert_eq!(count_roots(), 1);
    fs::write(&script, "exit 0\n").unwrap();
    let replacement = store
        .index("indexed", "index", "claude-personal", "gitnexus")
        .unwrap();
    assert_ne!(replacement.root, receipt.root);
    assert!(!receipt.root.exists());
    assert_eq!(count_roots(), 1);
    fs::write(&script, "exit 1\n").unwrap();
    assert_eq!(
        store
            .index("indexed", "index", "claude-personal", "gitnexus")
            .unwrap()
            .root,
        replacement.root
    );
}

#[test]
fn provisioning_retry_repairs_only_its_missing_worktree() {
    let (temp, store, _) = setup();
    let mut task = store
        .create_task("portal", "provision", "codex-personal")
        .unwrap();
    task.state = TaskState::Provisioning;
    fs::write(
        temp.path().join("managed/portal/tasks/provision/task.json"),
        serde_json::to_vec(&task).unwrap(),
    )
    .unwrap();
    fs::remove_dir_all(&task.checkouts[0].path).unwrap();
    let recovered = store
        .create_task("portal", "provision", "codex-personal")
        .unwrap();
    assert_eq!(recovered.state, TaskState::Ready);
    for checkout in recovered.checkouts {
        assert!(checkout.path.join("README.md").is_file());
    }
}

#[test]
fn committing_retry_recognizes_git_normalized_message() {
    let (temp, store, _) = setup();
    let task = store
        .create_task("portal", "normalize", "codex-personal")
        .unwrap();
    for checkout in &task.checkouts {
        git(&checkout.path, &["config", "user.name", "Workspace Test"]);
        git(
            &checkout.path,
            &["config", "user.email", "test@example.invalid"],
        );
        fs::write(checkout.path.join("README.md"), "changed").unwrap();
    }
    let frozen = store
        .freeze("portal", "normalize", "codex-personal")
        .unwrap();
    let mut approved = store
        .review(
            "portal",
            "normalize",
            "claude-personal",
            &frozen.checkpoint.unwrap().digest,
            true,
            "ok".into(),
        )
        .unwrap();
    let message = "Title  \n\n\nBody\t \n\n";
    git(&task.checkouts[0].path, &["add", "."]);
    git(&task.checkouts[0].path, &["commit", "-m", message]);
    let head = git(&task.checkouts[0].path, &["rev-parse", "HEAD"]);
    approved.state = TaskState::Committing;
    approved.commit_message = Some(message.into());
    fs::write(
        temp.path().join("managed/portal/tasks/normalize/task.json"),
        serde_json::to_vec(&approved).unwrap(),
    )
    .unwrap();
    let committed = store
        .commit("portal", "normalize", "codex-personal", message)
        .unwrap();
    assert_eq!(committed.state, TaskState::Committed);
    assert_eq!(
        committed.checkouts[0].commit.as_deref(),
        Some(head.as_str())
    );
    assert_eq!(
        store
            .commit("portal", "normalize", "codex-personal", message)
            .unwrap(),
        committed
    );
}

#[test]
fn recovery_preserves_unchanged_review_and_invalidates_changed_tree() {
    let (temp, store, _) = setup();
    for changed in [false, true] {
        let id = if changed { "changed" } else { "unchanged" };
        let task = store.create_task("portal", id, "codex-personal").unwrap();
        let frozen = store.freeze("portal", id, "codex-personal").unwrap();
        let mut approved = store
            .review(
                "portal",
                id,
                "claude-personal",
                &frozen.checkpoint.unwrap().digest,
                true,
                "ok".into(),
            )
            .unwrap();
        approved.execution_context = Some(task_workspace::ExecutionContext {
            access: task_workspace::Access::Review,
            previous_state: TaskState::Approved,
        });
        approved.state = TaskState::Interrupted;
        fs::write(
            temp.path()
                .join(format!("managed/portal/tasks/{id}/task.json")),
            serde_json::to_vec(&approved).unwrap(),
        )
        .unwrap();
        if changed {
            fs::write(task.checkouts[0].path.join("README.md"), "changed").unwrap();
        }
        let recovered = store
            .recover_after_fencing("portal", id, "claude-personal")
            .unwrap();
        assert_eq!(
            recovered.state,
            if changed {
                TaskState::Ready
            } else {
                TaskState::Approved
            }
        );
        assert_eq!(recovered.review.is_some(), !changed);
        assert_eq!(recovered.checkpoint.is_some(), !changed);
    }
}

#[test]
fn discovery_reports_invalid_children_and_keeps_valid_repositories() {
    let (temp, _store, _) = setup();
    let dotted = temp.path().join("repo.with.dots");
    fs::create_dir(&dotted).unwrap();
    git(&dotted, &["init", "-b", "develop"]);
    git(
        &temp.path().join("backend"),
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/test/backend.git",
        ],
    );
    let result = task_workspace::discover(temp.path(), "scan", "host", BTreeSet::new()).unwrap();
    assert_eq!(result.workspace.repositories.len(), 1);
    assert_eq!(result.skipped_repositories.len(), 3);
    assert!(
        result
            .skipped_repositories
            .iter()
            .any(|entry| entry.directory == dotted && !entry.reason.is_empty())
    );
}

#[test]
fn provisioning_fetches_integration_branch_created_after_initial_clone() {
    let (temp, store, mut workspace) = setup();
    workspace.id = "new-branch".into();
    workspace.repositories.truncate(1);
    workspace.repositories[0].integration_branch = "integration".into();
    store.register(&workspace).unwrap();
    let error = store
        .create_task("new-branch", "retry", "codex-personal")
        .unwrap_err();
    assert!(error.to_string().contains("backend"));
    assert!(error.to_string().contains("integration"));
    assert!(
        temp.path()
            .join("managed/new-branch/repositories/backend")
            .is_dir()
    );
    git(
        Path::new(&workspace.repositories[0].source),
        &["branch", "integration", "develop"],
    );
    let task = store
        .create_task("new-branch", "retry", "codex-personal")
        .unwrap();
    assert_eq!(task.state, TaskState::Ready);
    assert_eq!(task.checkouts.len(), 1);
}

#[test]
fn durable_workspace_jobs_are_scoped_idempotent_and_reauthorize_before_provisioning() {
    use workspace_execution_protocol::*;
    let (temp, store, workspace) = setup();
    let context = AdmissionContext {
        host_id: "host".into(),
        environment: "dev".into(),
        subject: "steve".into(),
        agent_id: "codex-personal".into(),
        runner_id: "runner".into(),
        coding_turn_authorized: true,
    };
    let policy = WorkspaceAccessPolicy {
        schema_version: VERSION,
        workspace_id: "portal".into(),
        host_id: "host".into(),
        environment: "dev".into(),
        runner_id: "runner".into(),
        membership_revision: task_workspace::membership_revision(&workspace).unwrap(),
        authorization_revision: 1,
        subjects: BTreeSet::from(["steve".into()]),
        agents: workspace.agents.clone(),
        intents: BTreeSet::from([WorkspaceIntent::Inspect, WorkspaceIntent::Implement]),
    };
    let request = WorkspaceRequest {
        schema_version: VERSION,
        request_id: "request-1".into(),
        workspace_id: "portal".into(),
        expected_membership_revision: policy.membership_revision.clone(),
        task: TaskSelection::New {
            description: "Understand config".into(),
        },
        intent: WorkspaceIntent::Inspect,
        expected_checkpoint_digest: None,
        instruction: "Explain this repository".into(),

        thread: None,
        native_model: None,
    };
    let job = store
        .admit_job(&request, &context, &policy, &policy.intents)
        .unwrap();
    assert_eq!(job.state, task_workspace::WorkspaceJobState::Queued);
    assert!(
        !temp
            .path()
            .join("managed/portal/tasks")
            .join(&job.task_id)
            .exists()
    );
    assert_eq!(
        store
            .admit_job(&request, &context, &policy, &policy.intents)
            .unwrap(),
        job
    );
    let mut changed = request.clone();
    changed.instruction = "Changed".into();
    assert!(
        store
            .admit_job(&changed, &context, &policy, &policy.intents)
            .is_err()
    );
    let mut revoked = policy.clone();
    revoked.subjects.clear();
    revoked.authorization_revision += 1;
    assert!(
        store
            .provision_job("portal", &job.job_id, &context, &revoked, &policy.intents)
            .is_err()
    );
    assert!(
        !temp
            .path()
            .join("managed/portal/tasks")
            .join(&job.task_id)
            .exists()
    );
    let restarted = WorkspaceStore::open(temp.path().join("managed")).unwrap();
    let ready = restarted
        .provision_job("portal", &job.job_id, &context, &policy, &policy.intents)
        .unwrap();
    assert_eq!(ready.state, task_workspace::WorkspaceJobState::Ready);
    assert_eq!(ready.task_id, job.task_id);
    // A committed model result is reusable, but a crashed model turn must never
    // be replayed merely because the caller retries its request.
    let execution = restarted.claim_job_execution(&ready).unwrap();
    assert!(restarted.claim_job_execution(&ready).is_err());
    match execution {
        task_workspace::JobExecution::Active(mut active) => {
            active.mark_started().unwrap();
            active.finish(serde_json::json!({"result":"done"})).unwrap()
        }
        _ => panic!("first claim must be active"),
    }
    match restarted.claim_job_execution(&ready).unwrap() {
        task_workspace::JobExecution::Cached(value) => assert_eq!(value["result"], "done"),
        _ => panic!("completed claim must be cached"),
    }
    let mut uncertain_request = request.clone();
    uncertain_request.request_id = "uncertain-request".into();
    uncertain_request.task = TaskSelection::Existing {
        task_id: ready.task_id.clone(),
    };
    let uncertain = restarted
        .admit_job(&uncertain_request, &context, &policy, &policy.intents)
        .unwrap();
    let uncertain = restarted
        .provision_job(
            "portal",
            &uncertain.job_id,
            &context,
            &policy,
            &policy.intents,
        )
        .unwrap();
    // A setup/spawn failure releases only the job lock; the admitted job remains retryable.
    let setup_attempt = restarted.claim_job_execution(&uncertain).unwrap();
    assert!(
        Command::new(temp.path().join("missing-bwrap"))
            .spawn()
            .is_err()
    );
    drop(setup_attempt);
    let execution = restarted.claim_job_execution(&uncertain).unwrap();
    let task_before = restarted
        .status("portal", &ready.task_id, "codex-personal")
        .unwrap();
    assert!(
        restarted
            .begin_tool_session(
                "portal",
                &ready.task_id,
                "codex-personal",
                true,
                Some("invalid-checkpoint")
            )
            .is_err()
    );
    drop(execution);
    assert_eq!(
        restarted
            .status("portal", &ready.task_id, "codex-personal")
            .unwrap(),
        task_before
    );
    match restarted.claim_job_execution(&uncertain).unwrap() {
        task_workspace::JobExecution::Active(mut active) => {
            active.mark_started().unwrap();
            // Crash after the uncertainty boundary must still prevent duplicate execution.
        }
        _ => panic!("setup failure must remain retryable"),
    }
    assert!(restarted.claim_job_execution(&uncertain).is_err());
    assert_eq!(
        restarted
            .status("portal", &job.task_id, "codex-personal")
            .unwrap()
            .checkouts
            .len(),
        3
    );
    assert_eq!(
        restarted
            .provision_job("portal", &job.job_id, &context, &policy, &policy.intents)
            .unwrap(),
        ready
    );
    assert!(
        restarted
            .provision_job("portal", &job.job_id, &context, &revoked, &policy.intents)
            .is_err()
    );
}

#[test]
fn workspace_membership_revision_excludes_grants_but_legacy_digest_does_not_change() {
    let (_temp, store, workspace) = setup();
    let task = store
        .create_task("portal", "legacy", "codex-personal")
        .unwrap();
    let revision = task_workspace::membership_revision(&workspace).unwrap();
    let mut changed = workspace.clone();
    changed.agents.insert("another".into());
    changed.operations.clear();
    assert_eq!(
        task_workspace::membership_revision(&changed).unwrap(),
        revision
    );
    changed.repositories.reverse();
    assert_eq!(
        task_workspace::membership_revision(&changed).unwrap(),
        revision
    );
    changed.repositories[0].integration_branch = "next".into();
    assert_ne!(
        task_workspace::membership_revision(&changed).unwrap(),
        revision
    );
    assert_eq!(
        store
            .status("portal", "legacy", "codex-personal")
            .unwrap()
            .membership_digest,
        task.membership_digest
    );
}

#[test]
fn inspection_keeps_task_state_and_blocks_writers_until_guard_is_released() {
    let (_temp, store, _) = setup();
    let task = store
        .create_task("portal", "inspect", "codex-personal")
        .unwrap();
    let inspection = store
        .begin_inspection("portal", "inspect", "claude-personal", None)
        .unwrap();
    assert_eq!(
        inspection
            .read_file("backend", "README.md")
            .unwrap()
            .content,
        "base\n"
    );
    assert!(
        inspection
            .read_file("backend", "../frontend/README.md")
            .is_err()
    );
    assert!(inspection.read_file("backend", ".git").is_err());
    assert!(
        store
            .edit_file(
                "portal",
                "inspect",
                "codex-personal",
                task_workspace::FileEdit {
                    repository: "backend".into(),
                    path: "README.md".into(),
                    content: Some("changed".into()),
                    expected_digest: Some(
                        inspection.read_file("backend", "README.md").unwrap().digest
                    )
                }
            )
            .is_err()
    );
    assert_eq!(
        store.status("portal", "inspect", "codex-personal").unwrap(),
        task
    );
    drop(inspection);
    let frozen = store.freeze("portal", "inspect", "codex-personal").unwrap();
    let approved = store
        .review(
            "portal",
            "inspect",
            "claude-personal",
            &frozen.checkpoint.unwrap().digest,
            true,
            "reviewed".into(),
        )
        .unwrap();
    let inspection = store
        .begin_inspection(
            "portal",
            "inspect",
            "claude-personal",
            approved.checkpoint.as_ref().map(|c| c.digest.as_str()),
        )
        .unwrap();
    assert_eq!(
        inspection.checkpoint(),
        approved.checkpoint.as_ref().unwrap()
    );
    drop(inspection);
    assert_eq!(
        store.status("portal", "inspect", "codex-personal").unwrap(),
        approved
    );
}

#[test]
fn runner_tool_session_binds_task_and_holds_writer_lease_until_finished() {
    use task_workspace::FileEdit;
    let (_temp, store, _) = setup();
    store
        .create_task("portal", "chat-task", "codex-personal")
        .unwrap();
    let mut session = store
        .begin_tool_session("portal", "chat-task", "codex-personal", true, None)
        .unwrap();
    assert_eq!(
        store
            .status("portal", "chat-task", "codex-personal")
            .unwrap()
            .state,
        TaskState::Running
    );
    assert!(
        store
            .begin_tool_session("portal", "chat-task", "claude-personal", true, None)
            .is_err()
    );
    assert!(session.read_file("other", "README.md").is_err());
    assert!(session.read_file("backend", "../README.md").is_err());
    let original = session.read_file("backend", "README.md").unwrap();
    session
        .edit_file(FileEdit {
            repository: "backend".into(),
            path: "README.md".into(),
            content: Some("implemented\n".into()),
            expected_digest: Some(original.digest.clone()),
        })
        .unwrap();
    assert!(
        session
            .edit_file(FileEdit {
                repository: "backend".into(),
                path: "README.md".into(),
                content: Some("stale\n".into()),
                expected_digest: Some(original.digest)
            })
            .is_err()
    );
    assert_eq!(
        session.read_file("backend", "README.md").unwrap().content,
        "implemented\n"
    );
    let checkpoint = session.finish().unwrap();
    assert_eq!(
        store
            .files("portal", "chat-task", "claude-personal")
            .unwrap(),
        checkpoint
    );
    assert_eq!(
        store
            .status("portal", "chat-task", "codex-personal")
            .unwrap()
            .state,
        TaskState::Ready
    );
    let session = store
        .begin_tool_session("portal", "chat-task", "codex-personal", true, None)
        .unwrap();
    drop(session);
    assert_eq!(
        store
            .status("portal", "chat-task", "codex-personal")
            .unwrap()
            .state,
        TaskState::Interrupted
    );
}

#[test]
fn inspection_tool_session_preserves_approval_and_rejects_edits() {
    let (_temp, store, _) = setup();
    store
        .create_task("portal", "inspect", "codex-personal")
        .unwrap();
    let frozen = store.freeze("portal", "inspect", "codex-personal").unwrap();
    let digest = frozen.checkpoint.unwrap().digest;
    let approved = store
        .review(
            "portal",
            "inspect",
            "claude-personal",
            &digest,
            true,
            "Fine".into(),
        )
        .unwrap();
    let mut session = store
        .begin_tool_session("portal", "inspect", "codex-personal", false, Some(&digest))
        .unwrap();
    assert!(
        session
            .edit_file(task_workspace::FileEdit {
                repository: "backend".into(),
                path: "new.txt".into(),
                content: Some("no".into()),
                expected_digest: None
            })
            .is_err()
    );
    session.finish().unwrap();
    assert_eq!(
        store.status("portal", "inspect", "codex-personal").unwrap(),
        approved
    );
}

#[test]
fn model_tool_arguments_cannot_override_scope_or_implicitly_delete() {
    use serde_json::json;
    let (_temp, store, _) = setup();
    store
        .create_task("portal", "scoped", "codex-personal")
        .unwrap();
    let mut session = store
        .begin_tool_session("portal", "scoped", "codex-personal", true, None)
        .unwrap();
    assert_eq!(
        session
            .call_tool(json!({"operation":"repositories"}))
            .unwrap()["repositories"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert!(
        session
            .call_tool(
                json!({"operation":"read","repository":"backend","path":"README.md","task":"other"})
            )
            .is_err()
    );
    assert!(
        session
            .call_tool(json!({"operation":"execute","command":"cat /etc/passwd"}))
            .is_err()
    );
    let read = session
        .call_tool(json!({"operation":"read","repository":"backend","path":"README.md"}))
        .unwrap();
    assert!(session.call_tool(json!({"operation":"edit","repository":"backend","path":"README.md","expectedDigest":read["digest"]})).is_err());
    session.call_tool(json!({"operation":"edit","repository":"backend","path":"new.txt","content":"created","expectedDigest":null})).unwrap();
    session.finish().unwrap();
    assert_eq!(
        store
            .read_file("portal", "scoped", "claude-personal", "backend", "new.txt")
            .unwrap()
            .content,
        "created"
    );
}

#[test]
fn uninitialized_submodule_is_pinned_but_file_tools_cannot_enter_it() {
    let (_temp, store, _workspace) = setup();
    let task = store
        .create_task("portal", "submodule", "codex-personal")
        .unwrap();
    let checkout = &task.checkouts[0];
    let head = git(&checkout.path, &["rev-parse", "HEAD"]);
    git(
        &checkout.path,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{},vendor/theme", head.trim()),
        ],
    );
    std::fs::create_dir_all(checkout.path.join("vendor/theme")).unwrap();
    let inspection = store
        .begin_inspection("portal", "submodule", "codex-personal", None)
        .unwrap();
    let before = inspection.checkpoint().digest.clone();
    assert!(
        !inspection.checkpoint().repositories[0]
            .files
            .iter()
            .any(|f| f.path.starts_with("vendor/theme"))
    );
    drop(inspection);
    let edit = task_workspace::FileEdit {
        repository: checkout.repository.clone(),
        path: "vendor/theme/escape.txt".into(),
        content: Some("blocked".into()),
        expected_digest: None,
    };
    assert!(
        store
            .edit_file("portal", "submodule", "codex-personal", edit.clone())
            .is_err()
    );
    let mut session = store
        .begin_tool_session("portal", "submodule", "codex-personal", true, None)
        .unwrap();
    assert!(session.edit_file(edit).is_err());
    session.finish().unwrap();
    assert!(!checkout.path.join("vendor/theme/escape.txt").exists());
    git(
        &checkout.path,
        &["update-index", "--force-remove", "vendor/theme"],
    );
    let inspection = store
        .begin_inspection("portal", "submodule", "codex-personal", None)
        .unwrap();
    assert_ne!(before, inspection.checkpoint().digest);
    drop(inspection);
    git(
        &checkout.path,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{},vendor/theme", head.trim()),
        ],
    );
    std::fs::write(
        checkout.path.join("vendor/theme/outside.txt"),
        "not checkpointed",
    )
    .unwrap();
    assert!(
        store
            .begin_inspection("portal", "submodule", "codex-personal", None)
            .is_err()
    );
}

#[test]
fn existing_task_reopens_offline_without_refreshing_pinned_revisions() {
    let (temp, store, workspace) = setup();
    let task = store
        .create_task("portal", "offline", "codex-personal")
        .unwrap();
    for repo in &workspace.repositories {
        fs::rename(
            &repo.source,
            temp.path().join(format!("offline-{}", repo.name)),
        )
        .unwrap();
    }
    assert_eq!(
        store
            .create_task("portal", "offline", "codex-personal")
            .unwrap(),
        task
    );
    // New tasks must still fetch current integration branches and report unavailable origins.
    assert!(
        store
            .create_task("portal", "new-offline", "codex-personal")
            .is_err()
    );
    // Interrupted provisioning also uses its persisted base, without a remote fetch.
    let metadata = temp.path().join("managed/portal/tasks/offline/task.json");
    let mut provisioning = task.clone();
    provisioning.state = TaskState::Provisioning;
    fs::write(metadata, serde_json::to_vec(&provisioning).unwrap()).unwrap();
    assert_eq!(
        store
            .create_task("portal", "offline", "codex-personal")
            .unwrap(),
        task
    );
}

#[test]
fn finished_tool_checkpoint_is_admissible_by_a_different_reviewer() {
    use workspace_execution_protocol::*;
    let (_temp, store, workspace) = setup();
    store
        .create_task("portal", "handoff", "codex-personal")
        .unwrap();
    let mut writer = store
        .begin_tool_session("portal", "handoff", "codex-personal", true, None)
        .unwrap();
    let original = writer.read_file("backend", "README.md").unwrap();
    writer
        .edit_file(task_workspace::FileEdit {
            repository: "backend".into(),
            path: "README.md".into(),
            content: Some("handoff\n".into()),
            expected_digest: Some(original.digest),
        })
        .unwrap();
    let checkpoint = writer.finish().unwrap();
    let context = AdmissionContext {
        host_id: "host".into(),
        environment: "dev".into(),
        subject: "owner".into(),
        agent_id: "claude-personal".into(),
        runner_id: "claude".into(),
        coding_turn_authorized: true,
    };
    let binding = WorkspaceAccessPolicy {
        schema_version: 1,
        workspace_id: "portal".into(),
        host_id: "host".into(),
        environment: "dev".into(),
        runner_id: "claude".into(),
        membership_revision: task_workspace::membership_revision(&workspace).unwrap(),
        authorization_revision: 1,
        subjects: BTreeSet::from(["owner".into()]),
        agents: workspace.agents.clone(),
        intents: BTreeSet::from([WorkspaceIntent::Review]),
    };
    let request = WorkspaceRequest {
        schema_version: 1,
        request_id: "review".into(),
        workspace_id: "portal".into(),
        expected_membership_revision: binding.membership_revision.clone(),
        task: TaskSelection::Existing {
            task_id: "handoff".into(),
        },
        intent: WorkspaceIntent::Review,
        expected_checkpoint_digest: Some(checkpoint.digest.clone()),
        instruction: "review".into(),
        thread: None,
        native_model: None,
    };
    let job = store
        .admit_job(&request, &context, &binding, &standalone_intents())
        .unwrap();
    store
        .provision_job(
            "portal",
            &job.job_id,
            &context,
            &binding,
            &standalone_intents(),
        )
        .unwrap();
    let mut reviewer = store
        .begin_tool_session(
            "portal",
            "handoff",
            "claude-personal",
            false,
            Some(&checkpoint.digest),
        )
        .unwrap();
    assert!(
        reviewer
            .edit_file(task_workspace::FileEdit {
                repository: "backend".into(),
                path: "README.md".into(),
                content: Some("bad".into()),
                expected_digest: None
            })
            .is_err()
    );
    assert_eq!(reviewer.finish().unwrap().digest, checkpoint.digest);
}
