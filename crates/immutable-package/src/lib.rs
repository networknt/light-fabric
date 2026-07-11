use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PackageTrust {
    pub package_digest: String,
    pub maximum_bytes: u64,
    pub maximum_entries: usize,
    pub signer_verified: bool,
    pub scanner_approved: bool,
    pub executable_paths: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MaterializedPackage {
    pub package_digest: String,
    pub entries: BTreeMap<String, String>,
    pub total_bytes: u64,
}

pub fn verify_and_extract_tar(
    archive_path: &Path,
    target: &Path,
    trust: &PackageTrust,
) -> Result<MaterializedPackage, PackageError> {
    if !trust.signer_verified || !trust.scanner_approved {
        return Err(PackageError::Trust);
    }
    let mut source = File::open(archive_path)?;
    let mut package_hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let n = source.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        package_hash.update(&buffer[..n]);
    }
    let actual = format!("sha256:{}", hex::encode(package_hash.finalize()));
    if actual != trust.package_digest {
        return Err(PackageError::Digest);
    }
    fs::create_dir(target)?;
    let mut archive = tar::Archive::new(File::open(archive_path)?);
    let mut seen = BTreeSet::new();
    let mut entries = BTreeMap::new();
    let mut total = 0u64;
    for item in archive.entries()? {
        let mut item = item?;
        let kind = item.header().entry_type();
        if !(kind.is_file() || kind.is_dir()) {
            return Err(PackageError::Entry);
        }
        let path = item.path()?.into_owned();
        let normalized = normalize(&path)?;
        let folded = normalized.to_ascii_lowercase();
        if !seen.insert(folded) {
            return Err(PackageError::Collision);
        }
        if seen.len() > trust.maximum_entries {
            return Err(PackageError::Limit);
        }
        if kind.is_dir() {
            fs::create_dir(target.join(&normalized))?;
            continue;
        }
        let declared = item.size();
        total = total.checked_add(declared).ok_or(PackageError::Limit)?;
        if total > trust.maximum_bytes {
            return Err(PackageError::Limit);
        }
        let destination = target.join(&normalized);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?
        }
        let mut output = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&destination)?;
        let mut hash = Sha256::new();
        let mut written = 0u64;
        loop {
            let n = item.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            written += n as u64;
            if written > declared {
                return Err(PackageError::Limit);
            }
            hash.update(&buffer[..n]);
            output.write_all(&buffer[..n])?
        }
        if written != declared {
            return Err(PackageError::Limit);
        }
        output.sync_all()?;
        entries.insert(
            normalized,
            format!("sha256:{}", hex::encode(hash.finalize())),
        );
    }
    make_read_only(target, &trust.executable_paths)?;
    Ok(MaterializedPackage {
        package_digest: actual,
        entries,
        total_bytes: total,
    })
}

fn normalize(path: &Path) -> Result<String, PackageError> {
    if path.is_absolute() {
        return Err(PackageError::Path);
    }
    let mut parts = Vec::new();
    for c in path.components() {
        match c {
            Component::Normal(v) => parts.push(v.to_str().ok_or(PackageError::Path)?),
            _ => return Err(PackageError::Path),
        }
    }
    if parts.is_empty() {
        return Err(PackageError::Path);
    }
    Ok(parts.join("/"))
}

fn make_read_only(root: &Path, executables: &BTreeSet<String>) -> Result<(), PackageError> {
    for entry in walk(root)? {
        let metadata = fs::symlink_metadata(&entry)?;
        if metadata.file_type().is_symlink() {
            return Err(PackageError::Entry);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let relative = entry
                .strip_prefix(root)
                .map_err(|_| PackageError::Path)?
                .to_string_lossy()
                .replace('\\', "/");
            let mode = if metadata.is_dir() {
                0o555
            } else if executables.contains(&relative) {
                0o555
            } else {
                0o444
            };
            fs::set_permissions(&entry, fs::Permissions::from_mode(mode))?;
        }
    }
    Ok(())
}
fn walk(root: &Path) -> Result<Vec<PathBuf>, PackageError> {
    let mut pending = vec![root.to_path_buf()];
    let mut result = Vec::new();
    while let Some(path) = pending.pop() {
        for entry in fs::read_dir(&path)? {
            let child = entry?.path();
            if child.is_dir() {
                pending.push(child.clone())
            }
            result.push(child)
        }
    }
    result.push(root.to_path_buf());
    result.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    Ok(result)
}

#[derive(Debug, Error)]
pub enum PackageError {
    #[error("package trust approval is missing")]
    Trust,
    #[error("package digest mismatch")]
    Digest,
    #[error("archive path is unsafe")]
    Path,
    #[error("archive contains links or unsupported entries")]
    Entry,
    #[error("archive contains case-colliding or duplicate paths")]
    Collision,
    #[error("archive exceeds declared limits")]
    Limit,
    #[error("package IO failed: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use tar::{Builder, EntryType, Header};

    fn archive(entries: &[(&str, EntryType, &[u8])]) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        let mut builder = Builder::new(file.reopen().unwrap());
        for (path, kind, contents) in entries {
            let mut header = Header::new_gnu();
            header.set_entry_type(*kind);
            header.set_size(contents.len() as u64);
            header.set_mode(0o777);
            header.set_cksum();
            builder.append_data(&mut header, path, *contents).unwrap();
        }
        builder.finish().unwrap();
        file
    }

    fn trust(path: &Path) -> PackageTrust {
        PackageTrust {
            package_digest: format!(
                "sha256:{}",
                hex::encode(Sha256::digest(fs::read(path).unwrap()))
            ),
            maximum_bytes: 1024,
            maximum_entries: 10,
            signer_verified: true,
            scanner_approved: true,
            executable_paths: BTreeSet::new(),
        }
    }

    #[test]
    fn extracts_verified_package_read_only() {
        let file = archive(&[("SKILL.md", EntryType::Regular, b"safe")]);
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("materialized");
        let result = verify_and_extract_tar(file.path(), &target, &trust(file.path())).unwrap();
        assert_eq!(result.total_bytes, 4);
        assert!(result.entries.contains_key("SKILL.md"));
        assert!(
            fs::metadata(target.join("SKILL.md"))
                .unwrap()
                .permissions()
                .readonly()
        );
    }

    #[test]
    fn rejects_links_and_case_collisions() {
        let link = archive(&[("escape", EntryType::Symlink, b"target")]);
        let root = tempfile::tempdir().unwrap();
        assert!(matches!(
            verify_and_extract_tar(link.path(), &root.path().join("link"), &trust(link.path())),
            Err(PackageError::Entry)
        ));

        let collision = archive(&[
            ("Skill.md", EntryType::Regular, b"one"),
            ("skill.md", EntryType::Regular, b"two"),
        ]);
        assert!(matches!(
            verify_and_extract_tar(
                collision.path(),
                &root.path().join("collision"),
                &trust(collision.path())
            ),
            Err(PackageError::Collision)
        ));
    }
}
