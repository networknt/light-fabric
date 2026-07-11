use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};

use execution_backend::StagedInput;
use execution_runner_protocol::{ExecuteLease, ExecutionInput};
use sha2::{Digest, Sha256};

#[derive(Clone)]
pub struct InputStager {
    root: PathBuf,
    maximum_bytes: u64,
}

impl InputStager {
    pub fn new(root: PathBuf, maximum_bytes: u64) -> Result<Self, String> {
        fs::create_dir_all(&root)
            .map_err(|error| format!("create staging root {}: {error}", root.display()))?;
        set_owner_only_directory(&root)?;
        Ok(Self {
            root,
            maximum_bytes,
        })
    }

    pub fn stage(&self, lease: &ExecuteLease) -> Result<Vec<StagedInput>, String> {
        let execution_root = self.root.join(lease.lease.execution_id.to_string());
        if execution_root.exists() {
            fs::remove_dir_all(&execution_root)
                .map_err(|error| format!("reset stale staging directory: {error}"))?;
        }
        fs::create_dir(&execution_root)
            .map_err(|error| format!("create execution staging directory: {error}"))?;
        set_owner_only_directory(&execution_root)?;
        let mut total = 0_u64;
        let mut staged = Vec::with_capacity(lease.inputs.len());
        for input in &lease.inputs {
            total = total
                .checked_add(input.size)
                .ok_or_else(|| "staged input size overflow".to_string())?;
            if total > self.maximum_bytes {
                return Err(format!(
                    "staged inputs exceed {} byte limit",
                    self.maximum_bytes
                ));
            }
            staged.push(stage_one(&execution_root, input)?);
        }
        Ok(staged)
    }

    pub fn cleanup(
        &self,
        execution_id: execution_runner_protocol::ExecutionId,
    ) -> Result<(), String> {
        let path = self.root.join(execution_id.to_string());
        if path.exists() {
            fs::remove_dir_all(path).map_err(|error| format!("remove staged inputs: {error}"))?;
        }
        Ok(())
    }
}

fn stage_one(root: &Path, input: &ExecutionInput) -> Result<StagedInput, String> {
    if !input.read_only {
        return Err("execution inputs must be read-only".to_string());
    }
    validate_mount_target(&input.mount_target)?;
    if input.kind.eq_ignore_ascii_case("archive") {
        return Err("archive inputs require a safe-extraction implementation".to_string());
    }
    let uri = url::Url::parse(&input.artifact_uri)
        .map_err(|error| format!("invalid input artifact URI: {error}"))?;
    if uri.scheme() != "file" || uri.host_str().is_some_and(|host| !host.is_empty()) {
        return Err(
            "MVP input staging accepts only runner-local file:// URIs; no store credential is projected"
                .to_string(),
        );
    }
    let source_path = uri
        .to_file_path()
        .map_err(|_| "input file URI is not a local absolute path".to_string())?;
    let source = open_no_follow(&source_path)?;
    let metadata = source
        .metadata()
        .map_err(|error| format!("read input metadata: {error}"))?;
    if !metadata.is_file() || metadata.len() != input.size {
        return Err("input is not a regular file or its declared size differs".to_string());
    }
    let target = root.join(input.input_id.to_string());
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&target)
        .map_err(|error| format!("create staged input: {error}"))?;
    set_owner_only_file(&target)?;
    let mut source = source;
    let mut digest = Sha256::new();
    let mut copied = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = source
            .read(&mut buffer)
            .map_err(|error| format!("read input: {error}"))?;
        if count == 0 {
            break;
        }
        copied = copied
            .checked_add(count as u64)
            .ok_or_else(|| "input size overflow".to_string())?;
        if copied > input.size {
            return Err("input grew while it was being staged".to_string());
        }
        digest.update(&buffer[..count]);
        output
            .write_all(&buffer[..count])
            .map_err(|error| format!("write staged input: {error}"))?;
    }
    output
        .sync_all()
        .map_err(|error| format!("fsync staged input: {error}"))?;
    let actual_digest = format!("sha256:{}", hex::encode(digest.finalize()));
    if copied != input.size || actual_digest != input.digest {
        let _ = fs::remove_file(&target);
        return Err("staged input size or SHA-256 digest verification failed".to_string());
    }
    Ok(StagedInput {
        input_id: input.input_id,
        source_digest: input.digest.clone(),
        local_path: target,
        mount_target: input.mount_target.clone(),
        media_type: input.media_type.clone(),
        size: copied,
        read_only: true,
        executable: input.executable,
        mount_options: if input.executable {
            vec!["ro".into(), "nodev".into(), "nosuid".into()]
        } else {
            vec![
                "ro".into(),
                "nodev".into(),
                "nosuid".into(),
                "noexec".into(),
            ]
        },
    })
}

fn validate_mount_target(target: &str) -> Result<(), String> {
    let path = Path::new(target);
    if !path.is_absolute()
        || !target.starts_with("/inputs/")
        || path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(
            "input mount target must be an absolute normalized path under /inputs".to_string(),
        );
    }
    Ok(())
}

fn open_no_follow(path: &Path) -> Result<File, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .map_err(|error| {
                format!(
                    "open input {} without symlink following: {error}",
                    path.display()
                )
            })
    }
    #[cfg(not(unix))]
    {
        let metadata = fs::symlink_metadata(path)
            .map_err(|error| format!("inspect input {}: {error}", path.display()))?;
        if metadata.file_type().is_symlink() {
            return Err("input symlinks are forbidden".to_string());
        }
        File::open(path).map_err(|error| format!("open input {}: {error}", path.display()))
    }
}

#[cfg(unix)]
fn set_owner_only_directory(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("set directory permissions: {error}"))
}

#[cfg(not(unix))]
fn set_owner_only_directory(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(unix)]
fn set_owner_only_file(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("set file permissions: {error}"))
}

#[cfg(not(unix))]
fn set_owner_only_file(_path: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Sha256;
    use uuid::Uuid;

    #[test]
    fn digest_mismatch_prevents_staging() {
        let root = std::env::temp_dir().join(format!("runner-staging-{}", Uuid::new_v4()));
        let source = root.with_extension("input");
        fs::write(&source, b"hello").unwrap();
        let input = ExecutionInput {
            input_id: Uuid::new_v4(),
            kind: "file".into(),
            artifact_uri: url::Url::from_file_path(&source).unwrap().to_string(),
            digest: format!("sha256:{}", hex::encode(Sha256::digest(b"wrong"))),
            size: 5,
            media_type: "text/plain".into(),
            mount_target: "/inputs/source.txt".into(),
            read_only: true,
            executable: false,
        };
        fs::create_dir_all(&root).unwrap();
        assert!(stage_one(&root, &input).is_err());
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_file(&source);
    }
}
