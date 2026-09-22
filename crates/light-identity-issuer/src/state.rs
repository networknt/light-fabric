//! Durable record of spent bootstrap tokens.
//!
//! A bootstrap token is accepted once. If the record lived only in memory, every
//! issuer restart would hand back a token that had already been used. This is an
//! append-only journal of JSON lines, one per event:
//!
//! ```text
//! {"v":1,"op":"spent","key":"<token jti>","at":"2026-09-20T22:00:00Z"}
//! {"v":1,"op":"reset","key":"<token jti>","at":"2026-09-20T22:05:00Z"}
//! ```
//!
//! * **Durable before acknowledged.** A spend is written and `fsync`ed before the
//!   token is reported as newly spent, so a crash cannot forget an acknowledged
//!   spend.
//! * **Fail closed.** If the write fails, `try_consume` returns an error, the
//!   issuer refuses the request, and the token is not treated as spent. A partial
//!   write is rolled back so it cannot corrupt the next line.
//! * **Torn tail tolerated, corruption not.** A final line with no newline is a
//!   crash mid-append: it is dropped and the file trimmed. Any other malformed
//!   line means the journal cannot be trusted, and opening it fails.
//! * **One writer.** The file is locked exclusively while open, so two issuer
//!   processes cannot both believe they hold the record. The lock is advisory
//!   (`flock`), so keep the journal on a local disk or volume, not NFS.
//!
//! Single instance only. Running several issuers needs a shared store (a database
//! implementing `SpentTokenStore`), not this file.

use std::collections::HashMap;
use std::fs::{DirBuilder, File, OpenOptions, TryLockError};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::{IssuerError, SpentTokenStore};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

const VERSION: u32 = 1;
const MAX_KEY_BYTES: usize = 512;

#[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
#[serde(rename_all = "camelCase")]
enum Op {
    Spent,
    Reset,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    v: u32,
    op: Op,
    key: String,
    at: String,
}

fn storage(context: &str, error: impl std::fmt::Display) -> IssuerError {
    IssuerError::Storage(format!("{context}: {error}"))
}

struct State {
    file: File,
    /// Spent keys and when they were spent.
    spent: HashMap<String, String>,
    /// Length of the journal as last known good, for rolling back a failed write.
    len: u64,
    /// Set if a failed write could not be rolled back; every later call fails.
    poisoned: bool,
}

pub struct FileSpentTokens {
    path: PathBuf,
    state: Mutex<State>,
    #[cfg(test)]
    fail_next_write: std::sync::atomic::AtomicBool,
}

impl FileSpentTokens {
    /// Open (creating if needed) the journal at `path`, take its lock, and replay
    /// it. Fails if another process holds the lock or the journal is corrupt.
    pub fn open(path: &Path) -> Result<Self, IssuerError> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            let mut builder = DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            builder.mode(0o700);
            builder
                .create(parent)
                .map_err(|e| storage(&format!("could not create {}", parent.display()), e))?;
        }
        let mut options = OpenOptions::new();
        options.read(true).append(true).create(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options
            .open(path)
            .map_err(|e| storage(&format!("could not open {}", path.display()), e))?;

        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(IssuerError::Storage(format!(
                    "{} is in use by another issuer process; stop it first",
                    path.display()
                )));
            }
            Err(TryLockError::Error(e)) => return Err(storage("could not lock the journal", e)),
        }

        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|e| storage(&format!("could not read {}", path.display()), e))?;

        let mut spent: HashMap<String, String> = HashMap::new();
        let mut start = 0usize;
        let mut line_number = 0usize;
        while let Some(offset) = bytes[start..].iter().position(|b| *b == b'\n') {
            line_number += 1;
            let line = &bytes[start..start + offset];
            let corrupt = |why: String| {
                IssuerError::Storage(format!(
                    "{} line {line_number} is corrupt: {why}",
                    path.display()
                ))
            };
            let entry: Entry = serde_json::from_slice(line).map_err(|e| corrupt(e.to_string()))?;
            if entry.v != VERSION {
                return Err(corrupt(format!("unsupported version {}", entry.v)));
            }
            match entry.op {
                Op::Spent => {
                    spent.insert(entry.key, entry.at);
                }
                Op::Reset => {
                    spent.remove(&entry.key);
                }
            }
            start += offset + 1;
        }
        // Bytes after the last newline are a write that never finished.
        if start < bytes.len() {
            file.set_len(start as u64)
                .map_err(|e| storage("could not trim an unfinished final line", e))?;
        }

        Ok(Self {
            path: path.to_path_buf(),
            state: Mutex::new(State {
                file,
                spent,
                len: start as u64,
                poisoned: false,
            }),
            #[cfg(test)]
            fail_next_write: std::sync::atomic::AtomicBool::new(false),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every spent key and when it was spent, oldest first.
    pub fn spent(&self) -> Vec<(String, String)> {
        let state = self.state.lock().expect("spent journal lock");
        let mut all: Vec<(String, String)> = state
            .spent
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        all.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
        all
    }

    fn append(&self, state: &mut State, op: Op, key: &str) -> Result<String, IssuerError> {
        if state.poisoned {
            return Err(IssuerError::Storage(
                "the journal is unusable after an earlier failed write; restart the issuer".into(),
            ));
        }
        let at = OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .map_err(|e| storage("timestamp", e))?;
        let mut line = serde_json::to_vec(&Entry {
            v: VERSION,
            op,
            key: key.to_string(),
            at: at.clone(),
        })
        .map_err(|e| storage("encoding an entry", e))?;
        line.push(b'\n');

        #[cfg(test)]
        if self
            .fail_next_write
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(IssuerError::Storage("injected write failure".into()));
        }

        let written = state
            .file
            .write_all(&line)
            .and_then(|()| state.file.sync_data());
        match written {
            Ok(()) => {
                state.len += line.len() as u64;
                Ok(at)
            }
            Err(error) => {
                // Do not leave half a line behind for the next append to glue onto.
                if state.file.set_len(state.len).is_err() {
                    state.poisoned = true;
                }
                Err(storage(
                    &format!("could not write {}", self.path.display()),
                    error,
                ))
            }
        }
    }

    fn check_key(key: &str) -> Result<(), IssuerError> {
        if key.is_empty() || key.len() > MAX_KEY_BYTES {
            return Err(IssuerError::Storage(
                "a token identity must be 1 to 512 bytes".into(),
            ));
        }
        Ok(())
    }
}

impl SpentTokenStore for FileSpentTokens {
    fn try_consume(&self, key: &str) -> Result<bool, IssuerError> {
        Self::check_key(key)?;
        let mut state = self.state.lock().expect("spent journal lock");
        if state.spent.contains_key(key) {
            return Ok(false);
        }
        let at = self.append(&mut state, Op::Spent, key)?;
        state.spent.insert(key.to_string(), at);
        Ok(true)
    }

    fn reset(&self, key: &str) -> Result<(), IssuerError> {
        Self::check_key(key)?;
        let mut state = self.state.lock().expect("spent journal lock");
        if !state.spent.contains_key(key) {
            return Ok(());
        }
        self.append(&mut state, Op::Reset, key)?;
        state.spent.remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn journal(dir: &TempDir) -> PathBuf {
        dir.path().join("state").join("spent-tokens.jsonl")
    }

    #[test]
    fn a_token_is_accepted_once() {
        let dir = TempDir::new().unwrap();
        let store = FileSpentTokens::open(&journal(&dir)).unwrap();
        assert!(store.try_consume("jti-1").unwrap());
        assert!(!store.try_consume("jti-1").unwrap());
        assert!(store.try_consume("jti-2").unwrap());
    }

    #[test]
    fn a_spend_survives_a_restart() {
        let dir = TempDir::new().unwrap();
        {
            let store = FileSpentTokens::open(&journal(&dir)).unwrap();
            assert!(store.try_consume("jti-1").unwrap());
        } // dropped: the process "restarts"
        let reopened = FileSpentTokens::open(&journal(&dir)).unwrap();
        assert!(
            !reopened.try_consume("jti-1").unwrap(),
            "the restart must not hand the token back"
        );
        assert!(reopened.try_consume("jti-2").unwrap());
    }

    #[test]
    fn a_reset_survives_a_restart_and_resetting_an_unknown_key_writes_nothing() {
        let dir = TempDir::new().unwrap();
        let path = journal(&dir);
        {
            let store = FileSpentTokens::open(&path).unwrap();
            store.try_consume("jti-1").unwrap();
            store.reset("jti-1").unwrap();
            let size = std::fs::metadata(&path).unwrap().len();
            store.reset("never-spent").unwrap();
            assert_eq!(
                std::fs::metadata(&path).unwrap().len(),
                size,
                "a no-op reset is not journaled"
            );
        }
        let reopened = FileSpentTokens::open(&path).unwrap();
        assert!(
            reopened.try_consume("jti-1").unwrap(),
            "the reset re-armed the token"
        );
    }

    #[test]
    fn an_unfinished_final_line_is_dropped_and_trimmed() {
        let dir = TempDir::new().unwrap();
        let path = journal(&dir);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let good = "{\"v\":1,\"op\":\"spent\",\"key\":\"jti-1\",\"at\":\"2026-01-01T00:00:00Z\"}\n";
        std::fs::write(&path, format!("{good}{{\"v\":1,\"op\":\"spent\",\"ke")).unwrap();

        let store = FileSpentTokens::open(&path).unwrap();
        assert!(
            !store.try_consume("jti-1").unwrap(),
            "the complete line counts"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            good,
            "the torn tail was trimmed"
        );

        // A later append starts on a clean line and the file reads back.
        assert!(store.try_consume("jti-2").unwrap());
        drop(store);
        let again = FileSpentTokens::open(&path).unwrap();
        assert!(!again.try_consume("jti-2").unwrap());
    }

    #[test]
    fn a_corrupt_complete_line_refuses_to_open_and_leaves_the_file_alone() {
        let dir = TempDir::new().unwrap();
        let path = journal(&dir);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let contents =
            "{\"v\":1,\"op\":\"spent\",\"key\":\"jti-1\",\"at\":\"t\"}\nthis is not json\n";
        std::fs::write(&path, contents).unwrap();

        let error = FileSpentTokens::open(&path).err().expect("must refuse");
        assert!(error.to_string().contains("line 2 is corrupt"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);
    }

    #[test]
    fn an_unknown_version_refuses_to_open() {
        let dir = TempDir::new().unwrap();
        let path = journal(&dir);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            "{\"v\":2,\"op\":\"spent\",\"key\":\"k\",\"at\":\"t\"}\n",
        )
        .unwrap();
        assert!(
            FileSpentTokens::open(&path)
                .err()
                .unwrap()
                .to_string()
                .contains("unsupported version")
        );
    }

    #[test]
    fn a_second_process_cannot_open_a_journal_that_is_in_use() {
        let dir = TempDir::new().unwrap();
        let first = FileSpentTokens::open(&journal(&dir)).unwrap();
        let error = FileSpentTokens::open(&journal(&dir)).err().expect("locked");
        assert!(
            error
                .to_string()
                .contains("in use by another issuer process"),
            "{error}"
        );
        drop(first);
        assert!(
            FileSpentTokens::open(&journal(&dir)).is_ok(),
            "released on drop"
        );
    }

    #[test]
    fn a_failed_write_fails_closed_and_the_token_is_not_treated_as_spent() {
        let dir = TempDir::new().unwrap();
        let path = journal(&dir);
        let store = FileSpentTokens::open(&path).unwrap();
        store.try_consume("jti-0").unwrap();
        let before = std::fs::read(&path).unwrap();

        store
            .fail_next_write
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let error = store.try_consume("jti-1").expect_err("the write failed");
        assert!(matches!(error, IssuerError::Storage(_)), "{error}");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "nothing was half-written"
        );

        // The token was not consumed, so the request can be retried.
        assert!(store.try_consume("jti-1").unwrap());
    }

    #[test]
    fn keys_with_awkward_characters_round_trip() {
        let dir = TempDir::new().unwrap();
        let path = journal(&dir);
        let key = "a\"b\\c\nd é 🙂";
        {
            let store = FileSpentTokens::open(&path).unwrap();
            assert!(store.try_consume(key).unwrap());
        }
        assert!(
            !FileSpentTokens::open(&path)
                .unwrap()
                .try_consume(key)
                .unwrap()
        );
    }

    #[test]
    fn empty_and_oversized_keys_are_refused() {
        let dir = TempDir::new().unwrap();
        let store = FileSpentTokens::open(&journal(&dir)).unwrap();
        assert!(store.try_consume("").is_err());
        assert!(store.try_consume(&"x".repeat(MAX_KEY_BYTES + 1)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn the_journal_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let path = journal(&dir);
        let _store = FileSpentTokens::open(&path).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
    }

    #[test]
    fn spent_lists_keys_oldest_first() {
        let dir = TempDir::new().unwrap();
        let store = FileSpentTokens::open(&journal(&dir)).unwrap();
        store.try_consume("b").unwrap();
        store.try_consume("a").unwrap();
        let keys: Vec<String> = store.spent().into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys.len(), 2);
        assert!(keys.contains(&"a".to_string()) && keys.contains(&"b".to_string()));
    }
}
