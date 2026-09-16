use light_workflow::snapshot_transfer::{ChunkResult, assemble};
use std::{collections::BTreeSet, fs, process::Command};
use task_workspace::{Operation, Repository, SNAPSHOT_CHUNK_BYTES, Workspace, WorkspaceStore};
use workspace_execution_protocol::ManagerSnapshotRead;

#[test]
fn chunks_resume_out_of_order_and_reject_corruption_without_runner_refs() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    fs::create_dir(&repo).unwrap();
    for args in [
        vec!["init", "-b", "main"],
        vec!["config", "user.name", "Gate"],
        vec!["config", "user.email", "gate@example.invalid"],
    ] {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    fs::write(
        repo.join("payload.bin"),
        (0..300_000).map(|i| (i % 251) as u8).collect::<Vec<_>>(),
    )
    .unwrap();
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["add", "."])
            .output()
            .unwrap()
            .status
            .success()
    );
    assert!(
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["commit", "-m", "fixture"])
            .output()
            .unwrap()
            .status
            .success()
    );
    let root = temp.path().join("manager");
    let store = WorkspaceStore::open(&root).unwrap();
    store
        .register(&Workspace {
            schema_version: 1,
            id: "workspace".into(),
            host_id: "host".into(),
            agents: BTreeSet::from(["agent".into()]),
            repositories: vec![Repository {
                name: "repo".into(),
                source: repo.display().to_string(),
                integration_branch: "main".into(),
                release_branch: "main".into(),
            }],
            indexers: Default::default(),
            operations: BTreeSet::from([Operation::Edit, Operation::Review]),
        })
        .unwrap();
    store.create_task("workspace", "task", "agent").unwrap();
    let cp = store.files("workspace", "task", "agent").unwrap();
    let receipt = store
        .capture_snapshot(
            "workspace",
            "task",
            "agent",
            "feature",
            "stage",
            "snapshot",
            &cp.digest,
        )
        .unwrap();
    let mut chunks = Vec::new();
    for offset in (0..receipt.package_bytes).step_by(SNAPSHOT_CHUNK_BYTES) {
        chunks.push(ChunkResult {
            workspace_id: "workspace".into(),
            task_id: "task".into(),
            receipt: receipt.clone(),
            manager_snapshot: ManagerSnapshotRead {
                feature_id: "feature".into(),
                stage_id: "stage".into(),
                snapshot_id: "snapshot".into(),
                checkpoint_digest: cp.digest.clone(),
                package_digest: Some(receipt.package_digest.clone()),
                offset,
            },
            chunk: store
                .snapshot_chunk(
                    "workspace",
                    "task",
                    "agent",
                    "snapshot",
                    &receipt.package_digest,
                    offset,
                )
                .unwrap(),
        });
    }
    assert!(chunks.len() > 2);
    assert!(assemble(&chunks[..1]).unwrap().is_none());
    let expected = assemble(&chunks).unwrap().unwrap();
    chunks.reverse();
    chunks.push(chunks[0].clone());
    // No runner-local cache, tree refs, or worktree is needed to verify recovery.
    fs::remove_dir_all(&root).unwrap();
    assert_eq!(assemble(&chunks).unwrap().unwrap(), expected);
    let mut bad = chunks.clone();
    bad[0].chunk.bytes[0] ^= 1;
    assert!(assemble(&bad).is_err());
    let mut bad = chunks.clone();
    bad[0].manager_snapshot.feature_id = "other".into();
    assert!(assemble(&bad).is_err());
    let mut bad = chunks.clone();
    bad[0].chunk.total_bytes = u64::MAX;
    assert!(assemble(&bad).is_err());
}
