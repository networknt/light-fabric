//! Non-executing, integrity-checked bundle capture. A snapshot owns bytes, not
//! mutable path references. This does not admit an executable or spawn a child.
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, io::Read, path::PathBuf, sync::Arc};

pub struct Pin {
    pub path: PathBuf,
    pub bytes: usize,
    pub sha256: [u8; 32],
}
/// Explicit caller-supplied qualification limits, never production defaults.
pub struct Limits {
    pub files: usize,
    pub per_file: usize,
    pub aggregate: usize,
}
#[derive(Debug, PartialEq, Eq)]
pub enum Failure {
    Profile,
    Path,
    Io,
    Integrity,
    Allocation,
}
pub struct Snapshot {
    files: BTreeMap<PathBuf, Arc<[u8]>>,
    bytes: usize,
    identity: [u8; 32],
}
impl Snapshot {
    pub fn capture(pins: &[Pin], limits: Limits) -> Result<Self, Failure> {
        if pins.is_empty() || pins.len() > limits.files {
            return Err(Failure::Profile);
        }
        let mut total = 0usize;
        let mut paths = std::collections::BTreeSet::new();
        // Entire manifest admitted before any file open or content allocation.
        for pin in pins {
            if !pin.path.is_absolute()
                || pin
                    .path
                    .components()
                    .any(|p| matches!(p, std::path::Component::ParentDir))
                || !paths.insert(&pin.path)
            {
                return Err(Failure::Path);
            }
            total = total.checked_add(pin.bytes).ok_or(Failure::Profile)?;
            if pin.bytes > limits.per_file || total > limits.aggregate {
                return Err(Failure::Profile);
            }
        }
        let mut files: BTreeMap<PathBuf, Arc<[u8]>> = BTreeMap::new();
        for pin in pins {
            // Reject manifest symlinks. Hashes bind the actual opened bytes;
            // filesystem immutability and immutable execution remain separate.
            let metadata = std::fs::symlink_metadata(&pin.path).map_err(|_| Failure::Io)?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(Failure::Path);
            }
            let mut file = std::fs::File::open(&pin.path).map_err(|_| Failure::Io)?;
            if file.metadata().map_err(|_| Failure::Io)?.len() != pin.bytes as u64 {
                return Err(Failure::Integrity);
            }
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(pin.bytes)
                .map_err(|_| Failure::Allocation)?;
            bytes.resize(pin.bytes, 0);
            file.read_exact(&mut bytes)
                .map_err(|_| Failure::Integrity)?;
            let mut extra = [0u8; 1];
            if file.read(&mut extra).map_err(|_| Failure::Io)? != 0
                || <[u8; 32]>::from(Sha256::digest(&bytes)) != pin.sha256
            {
                return Err(Failure::Integrity);
            }
            files.insert(pin.path.clone(), Arc::from(bytes));
        }
        let mut identity = Sha256::new();
        identity.update(b"light-lambda/bundle-snapshot/v1\0");
        for (path, bytes) in &files {
            use std::os::unix::ffi::OsStrExt;
            let path = path.as_os_str().as_bytes();
            identity.update((path.len() as u64).to_be_bytes());
            identity.update(path);
            identity.update((bytes.len() as u64).to_be_bytes());
            identity.update(Sha256::digest(bytes));
        }
        Ok(Self {
            files,
            bytes: total,
            identity: identity.finalize().into(),
        })
    }
    pub fn get(&self, path: &std::path::Path) -> Option<&[u8]> {
        self.files.get(path).map(AsRef::as_ref)
    }
    /// Captured payload bytes only, not an allocator/RSS bound. Manifest paths,
    /// map nodes, Arc storage and Vec-to-Arc overlap require separate accounting.
    pub fn payload_bytes(&self) -> usize {
        self.bytes
    }
    pub fn identity(&self) -> [u8; 32] {
        self.identity
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pin(path: PathBuf, bytes: &[u8]) -> Pin {
        Pin {
            path,
            bytes: bytes.len(),
            sha256: Sha256::digest(bytes).into(),
        }
    }
    fn limits() -> Limits {
        Limits {
            files: 2,
            per_file: 4,
            aggregate: 8,
        }
    }
    #[test]
    fn exact_snapshot_survives_source_replacement_and_removal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("worker");
        std::fs::write(&path, b"old!").unwrap();
        let snapshot = Snapshot::capture(&[pin(path.clone(), b"old!")], limits()).unwrap();
        std::fs::write(&path, b"new!").unwrap();
        assert!(matches!(
            Snapshot::capture(&[pin(path.clone(), b"old!")], limits()),
            Err(Failure::Integrity)
        ));
        std::fs::remove_file(&path).unwrap();
        assert_eq!(snapshot.get(&path), Some(b"old!".as_slice()));
        assert_eq!(snapshot.payload_bytes(), 4);
    }
    #[test]
    fn profile_rejects_before_io_and_checks_exact_bounds() {
        let absent = PathBuf::from("/not-opened-by-profile-test");
        assert!(matches!(
            Snapshot::capture(&[pin(absent, b"12345")], limits()),
            Err(Failure::Profile)
        ));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a");
        std::fs::write(&path, b"1234").unwrap();
        let p = pin(path.clone(), b"1234");
        assert!(Snapshot::capture(&[p], limits()).is_ok());
        assert!(matches!(
            Snapshot::capture(&[pin(path.clone(), b"123")], limits()),
            Err(Failure::Integrity)
        ));
        assert!(matches!(
            Snapshot::capture(&[pin(path.clone(), b"1234"), pin(path, b"1234")], limits()),
            Err(Failure::Path)
        ));
    }
    #[test]
    fn identity_is_order_independent_and_symlinks_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::write(&a, b"aaaa").unwrap();
        std::fs::write(&b, b"bbbb").unwrap();
        let one = Snapshot::capture(
            &[pin(a.clone(), b"aaaa"), pin(b.clone(), b"bbbb")],
            limits(),
        )
        .unwrap();
        let two = Snapshot::capture(&[pin(b, b"bbbb"), pin(a.clone(), b"aaaa")], limits()).unwrap();
        assert_eq!(one.identity(), two.identity());
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&a, &link).unwrap();
        assert!(matches!(
            Snapshot::capture(&[pin(link, b"aaaa")], limits()),
            Err(Failure::Path)
        ));
    }
}
