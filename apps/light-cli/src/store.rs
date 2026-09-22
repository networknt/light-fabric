//! Where the CLI keeps its state for one environment: `~/.light/<envTag>/`, mode `0700`.
//!
//! What lives there is the user's login (`user-session.json`, see [`crate::session`]) and a lock
//! file that serialises concurrent invocations, so two parallel runs cannot race on a refresh.

use std::fs::{DirBuilder, File, OpenOptions};
use std::path::{Path, PathBuf};

use crate::error::CliError;

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};

/// Held while this process may change the store. Dropping it releases the lock.
pub struct StoreLock(#[allow(dead_code)] File);

pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn create_private_dir(path: &Path) -> Result<(), CliError> {
        let mut builder = DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        builder.mode(0o700);
        builder.create(path)?;
        Ok(())
    }

    /// Take the exclusive lock, blocking until it is free.
    pub fn lock(&self) -> Result<StoreLock, CliError> {
        Self::create_private_dir(&self.root)?;
        let mut options = OpenOptions::new();
        options.create(true).truncate(false).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let file = options.open(self.root.join("lock"))?;
        file.lock()?;
        Ok(StoreLock(file))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[cfg(unix)]
    #[test]
    fn the_store_and_its_lock_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let store = Store::new(dir.path().join("dev"));
        let _held = store.lock().unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(store.root()), 0o700);
        assert_eq!(mode(&store.root().join("lock")), 0o600);
    }

    #[test]
    fn the_lock_is_exclusive_between_holders() {
        let dir = tempdir().unwrap();
        let store = Store::new(dir.path().join("dev"));
        let held = store.lock().unwrap();

        let contender = OpenOptions::new()
            .write(true)
            .open(store.root().join("lock"))
            .unwrap();
        assert!(matches!(
            contender.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        drop(held);
        assert!(contender.try_lock().is_ok());
    }
}
