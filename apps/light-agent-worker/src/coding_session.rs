//! Private native thread checkpoints. A lost/uncertain attempt is never replayed.
//!
//! Opening a checkpoint locks and loads it but changes nothing on disk. The record is
//! only marked IN_FLIGHT by `begin`, immediately before the first native Codex request
//! that can touch the durable thread, so every preflight rejection above that point
//! leaves the workflow's conversation resumable.
use anyhow::{Context, Result, bail};
use coding_agent_runtime::{CodingThreadMode, CodingTurnSpec};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::SystemTime,
};
use uuid::Uuid;

/// A CLOSED checkpoint is retained only long enough to reject a late resume of the
/// same sessionRef. Past that it is dead weight in the owner's Codex home.
const CLOSED_RETENTION: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 60 * 60);

fn expired(path: &Path, cutoff: SystemTime) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| {
        meta.file_type().is_file() && meta.modified().is_ok_and(|when| when < cutoff)
    })
}

fn recorded_state(path: &Path) -> Option<Value> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

/// The guard explicitly unlocks on every exit path, including rejected opens and
/// pruning, even while a fork-inherited description remains open.
struct ThreadLock(File);
impl Drop for ThreadLock {
    fn drop(&mut self) {
        // Unlock even on rejected opens and pruning: fork-inherited descriptions
        // may outlive this descriptor until the child execs.
        let _ = self.0.unlock();
    }
}

fn lock_thread(lock_path: &Path) -> Result<ThreadLock> {
    if lock_path.is_symlink() {
        bail!("coding thread lock cannot be a symlink");
    }
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(lock_path)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(5))
            }
            Err(error) => return Err(error).context("coding thread is busy"),
        }
    }
    Ok(ThreadLock(lock))
}

/// Best-effort reclamation of long-closed checkpoints.
///
/// Every removal happens under the same per-thread lock that `open` takes, and the
/// CLOSED/age observation is repeated under that lock, so a pass can never delete a
/// record that another pass or a live session has replaced since it was listed.
///
/// The lock file itself is deliberately never unlinked. `flock` ownership belongs to the
/// inode, so removing a lock file would let one process keep a lock on the unlinked inode
/// while another locks a freshly created one and both believe they own the thread. An
/// empty lock file is a far cheaper residue than a corrupted mutual exclusion.
fn prune_closed(root: &Path) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    let Some(cutoff) = SystemTime::now().checked_sub(CLOSED_RETENTION) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json")
            || !expired(&path, cutoff)
        {
            continue;
        }
        // A live session owns the thread whenever this lock is held; skip it untouched.
        let Ok(_lock) = lock_thread(&path.with_extension("lock")) else {
            continue;
        };
        // The listing observation above is stale by construction. Re-decide under the
        // lock, against whatever the record is now.
        if expired(&path, cutoff)
            && recorded_state(&path).is_some_and(|state| state["state"] == json!("CLOSED"))
        {
            let _ = fs::remove_file(&path);
        }
    }
}

pub(crate) struct CodingSession {
    _lock: ThreadLock,
    path: PathBuf,
    state: Value,
}
impl CodingSession {
    /// Locks and loads the checkpoint, validating that the caller is entitled to it.
    /// Nothing is written: a failure after this point but before `begin` leaves the
    /// stored record exactly as it was.
    pub fn open(
        home: &Path,
        scope: &str,
        spec: &CodingTurnSpec,
        contract_digest: &str,
    ) -> Result<Self> {
        let control = spec.thread.as_ref().context("missing thread control")?;
        // The repository base and all authority stay fixed within a role's stage.
        let mut binding = json!({"scope":scope,"runnerId":control.runner_id,"stageId":control.stage_id,"role":spec.role,
            "roleProfile":spec.role_profile,"model":spec.model_alias,"authentication":spec.authentication_profile,
            "contract":contract_digest,"repositoryDigest":spec.repository_digest,"baseRevision":spec.base_revision,
            "workspaceRoot":spec.workspace_root,"writableRoots":spec.writable_roots,"allowedTools":spec.allowed_tools,
            "manifest":spec.materialization_manifest_digest});
        if let Some(policy) = &spec.codex_policy {
            binding["codexPolicy"] = serde_json::to_value(policy)?;
        }
        Self::open_bound(
            home,
            scope,
            spec.thread.as_ref().context("missing thread control")?,
            binding,
        )
    }
    pub(crate) fn open_bound(
        home: &Path,
        scope: &str,
        control: &coding_agent_runtime::CodingThreadControl,
        binding: Value,
    ) -> Result<Self> {
        control.validate()?;
        if !scope.starts_with("sha256:") || scope.len() != 71 {
            bail!("missing trusted coding thread scope");
        }
        let root = home.join("light-worker-threads");
        match fs::DirBuilder::new().mode(0o700).create(&root) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
        let meta = fs::symlink_metadata(&root)?;
        if !meta.is_dir() || meta.permissions().mode() & 0o077 != 0 {
            bail!("coding thread directory must be private and not a symlink");
        }
        prune_closed(&root);
        let key = agent_core::sha256_digest(format!("{scope}:{}", control.session_ref).as_bytes());
        let stem = key.strip_prefix("sha256:").unwrap_or(&key);
        let lock = lock_thread(&root.join(format!("{stem}.lock")))?;
        let path = root.join(format!("{stem}.json"));
        if path.is_symlink() {
            bail!("coding thread checkpoint cannot be a symlink");
        }
        let state = match control.mode {
            CodingThreadMode::New => {
                if path.exists() {
                    bail!("coding thread already exists; use resume or a new sessionRef");
                }
                json!({"binding":binding,"sessionRef":control.session_ref,"state":"UNCLAIMED","threadId":null,"patch":"","checkpoint":null})
            }
            CodingThreadMode::Resume | CodingThreadMode::Close => {
                let state: Value =
                    serde_json::from_slice(&fs::read(&path).context(
                        "coding thread unavailable; workflow must authorize a new thread",
                    )?)?;
                if state["binding"] != binding {
                    bail!("coding thread scope, stage, role, repository, or policy changed");
                }
                if state["state"] != "READY" {
                    bail!(
                        "coding thread is closed or uncertain; workflow must authorize a new thread"
                    );
                }
                if state["checkpoint"] != json!(control.expected_checkpoint) {
                    bail!("stale coding thread checkpoint");
                }
                if state["threadId"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .is_none()
                {
                    bail!("coding thread has no durable native thread");
                }
                state
            }
        };
        Ok(Self {
            _lock: lock,
            path,
            state,
        })
    }
    /// Durably claims the thread for one native attempt. Call this immediately before
    /// the first Codex request that can create or advance the durable thread: from here
    /// on an interrupted attempt is indistinguishable from a lost one and the checkpoint
    /// must never be silently resumed.
    pub fn begin(&mut self) -> Result<()> {
        self.state["state"] = json!("IN_FLIGHT");
        self.save()
    }
    /// Candidate adapters can bind native metadata before begin; no disk write occurs here.
    pub(crate) fn adapter_state(&self) -> Option<&Value> {
        self.state.get("adapterState")
    }
    pub(crate) fn set_adapter_state(&mut self, value: Value) {
        self.state["adapterState"] = value;
    }

    pub fn thread_id(&self) -> Option<&str> {
        self.state["threadId"].as_str()
    }
    pub fn patch(&self) -> &str {
        self.state["patch"].as_str().unwrap_or("")
    }
    /// Commits the turn's result and mints the checkpoint that authorizes the next
    /// resume. Call this as soon as the turn's work is known-good, before any optional
    /// bookkeeping: once it returns, the result survives a kill.
    pub fn finish(&mut self, thread_id: &str, patch: &str, closed: bool) -> Result<Value> {
        self.state["threadId"] = json!(thread_id);
        self.state["patch"] = json!(patch);
        self.state["checkpoint"] = json!(Uuid::now_v7());
        self.state["state"] = json!(if closed { "CLOSED" } else { "READY" });
        self.save()?;
        Ok(self.receipt())
    }
    /// Records that an already-committed thread was archived. The checkpoint is
    /// deliberately unchanged: the workflow was handed it by `finish`, and closing is a
    /// state transition on that same checkpoint, not a new one.
    pub fn mark_closed(&mut self) -> Result<Value> {
        self.state["state"] = json!("CLOSED");
        self.save()?;
        Ok(self.receipt())
    }
    fn receipt(&self) -> Value {
        json!({"sessionRef":self.state["sessionRef"],"checkpoint":self.state["checkpoint"],"state":self.state["state"]})
    }
    fn save(&self) -> Result<()> {
        let mut file = tempfile::NamedTempFile::new_in(self.path.parent().unwrap())?;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(&serde_json::to_vec(&self.state)?)?;
        file.as_file().sync_all()?;
        file.persist(&self.path)?;
        File::open(self.path.parent().unwrap())?.sync_all()?;
        Ok(())
    }
}

#[cfg(test)]
mod lock_lifetime_tests {
    use super::*;

    #[test]
    fn rejected_open_guard_releases_inherited_description() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.lock");
        let guard = lock_thread(&path).unwrap();
        let inherited = guard.0.try_clone().unwrap();
        // Same guard is used before constructing CodingSession and by prune_closed.
        drop(guard);
        let next = lock_thread(&path).unwrap();
        drop(inherited);
        assert!(lock_thread(&path).is_err());
        drop(next);
    }

    #[test]
    fn acquisition_tolerates_short_inherited_lock_window() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.lock");
        let owner = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        owner.try_lock().unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            owner.unlock().unwrap();
        });
        let next = lock_thread(&path).unwrap();
        release.join().unwrap();
        drop(next);
    }

    #[test]
    fn dropping_owner_releases_lock_even_with_inherited_description() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("session.lock");
        let file = lock_thread(&path).unwrap();
        let inherited_description = file.0.try_clone().unwrap();
        let owner = CodingSession {
            _lock: file,
            path: root.path().join("session.json"),
            state: json!({}),
        };
        assert!(lock_thread(&path).is_err());
        drop(owner);
        let next = lock_thread(&path).unwrap();
        drop(inherited_description);
        // Closing the inherited old description cannot release the next owner's lock.
        assert!(lock_thread(&path).is_err());
        drop(next);
    }
}
