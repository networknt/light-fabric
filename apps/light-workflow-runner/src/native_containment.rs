//! Per-execution cgroup-v2 containment. No PID-based kill or broad unit kill.
//! Only a recorded direct child of this runner's cgroup is a valid target.
use execution_runner_protocol::ExecutionId;
use serde::{Deserialize, Serialize};
use std::{
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeContainment {
    pub execution_id: ExecutionId,
    pub parent: String,
    pub inode: u64,
    pub boot_id: String,
    pub cgroup_namespace: String,
}

fn environment() -> Result<(String, String, String), String> {
    let membership = std::fs::read_to_string("/proc/self/cgroup")
        .map_err(|_| "cgroup membership unavailable")?;
    let parent = membership
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .ok_or("unified cgroup required")?
        .to_owned();
    validate_parent(&parent)?;
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map_err(|_| "boot identity unavailable")?
        .trim()
        .to_owned();
    let ns = std::fs::read_link("/proc/self/ns/cgroup")
        .map_err(|_| "cgroup namespace unavailable")?
        .to_string_lossy()
        .into_owned();
    Ok((parent, boot, ns))
}

fn validate_parent(parent: &str) -> Result<(), String> {
    if !parent.starts_with('/')
        || parent == "/"
        || parent
            .split('/')
            .skip(1)
            .any(|p| p.is_empty() || p == "." || p == "..")
    {
        return Err("invalid delegated runner cgroup".into());
    }
    Ok(())
}

impl NativeContainment {
    pub fn create(execution_id: ExecutionId) -> Result<Self, String> {
        let (parent, boot_id, cgroup_namespace) = environment()?;
        let path = Path::new("/sys/fs/cgroup")
            .join(parent.trim_start_matches('/'))
            .join(format!("execution-{execution_id}"));
        std::fs::create_dir(&path).map_err(|_| "create delegated execution cgroup failed")?;
        let inode = std::fs::metadata(&path)
            .map_err(|_| "execution cgroup metadata unavailable")?
            .ino();
        let result = Self {
            execution_id,
            parent,
            inode,
            boot_id,
            cgroup_namespace,
        };
        // A normal filesystem or cgroup-v1 directory cannot provide this proof.
        std::fs::read_to_string(path.join("cgroup.events"))
            .map_err(|_| "cgroup-v2 events unavailable")?;
        Ok(result)
    }

    fn checked_path(&self, execution: ExecutionId) -> Result<PathBuf, String> {
        let (parent, boot, namespace) = environment()?;
        if execution != self.execution_id
            || self.parent != parent
            || self.boot_id != boot
            || self.cgroup_namespace != namespace
        {
            return Err("native containment identity mismatch".into());
        }
        validate_parent(&self.parent)?;
        Ok(Path::new("/sys/fs/cgroup")
            .join(self.parent.trim_start_matches('/'))
            .join(format!("execution-{execution}")))
    }

    pub fn attach(&self, pid: u32) -> Result<(), String> {
        if pid <= 1 || pid == std::process::id() {
            return Err("invalid worker pid for containment".into());
        }
        let path = self.checked_path(self.execution_id)?;
        if std::fs::metadata(&path)
            .map_err(|_| "execution cgroup disappeared")?
            .ino()
            != self.inode
        {
            return Err("execution cgroup was replaced".into());
        }
        std::fs::write(path.join("cgroup.procs"), pid.to_string())
            .map_err(|_| "attach worker to execution cgroup failed".to_owned())
    }

    pub async fn cleanup(&self, execution: ExecutionId) -> Result<(), String> {
        let path = self.checked_path(execution)?;
        match std::fs::metadata(&path) {
            // The kernel only permits removal of an unpopulated cgroup. Check
            // identity first, so another boot/namespace is never absence proof.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) => return Err("execution cgroup inspection failed".into()),
            Ok(metadata) if metadata.ino() != self.inode => {
                return Err("execution cgroup was replaced".into());
            }
            Ok(_) => {}
        }
        #[cfg(feature = "qualification-hooks")]
        qualification_pause(execution).await?;
        std::fs::write(path.join("cgroup.kill"), "1")
            .map_err(|_| "execution cgroup kill failed")?;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let events = std::fs::read_to_string(path.join("cgroup.events"))
                .map_err(|_| "execution cleanup evidence unavailable")?;
            if unpopulated(&events)? {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err("execution cgroup remains populated".into());
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
}

fn unpopulated(events: &str) -> Result<bool, String> {
    let values = events
        .lines()
        .filter_map(|line| line.strip_prefix("populated "))
        .collect::<Vec<_>>();
    match values.as_slice() {
        ["0"] => Ok(true),
        ["1"] => Ok(false),
        _ => Err("invalid cgroup population evidence".into()),
    }
}

#[cfg(feature = "qualification-hooks")]
async fn qualification_pause(execution: ExecutionId) -> Result<(), String> {
    let Some(directory) = std::env::var_os("LIGHT_RUNNER_CLEANUP_QUALIFICATION_DIR") else {
        return Ok(());
    };
    let directory = PathBuf::from(directory);
    let metadata =
        std::fs::symlink_metadata(&directory).map_err(|_| "qualification directory unavailable")?;
    if !directory.is_absolute()
        || !metadata.is_dir()
        || metadata.mode() & 0o077 != 0
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        return Err("qualification directory must be private and owned by runner".into());
    }
    let entered = directory.join(format!("entered-{execution}"));
    match std::fs::rename(directory.join("armed"), &entered) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err("qualification barrier could not be claimed".into()),
        Ok(()) => {}
    }
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
    while !directory.join(format!("release-{execution}")).exists() {
        if tokio::time::Instant::now() >= deadline {
            return Err("qualification cleanup pause expired".into());
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn population_requires_one_explicit_kernel_field() {
        assert!(unpopulated("populated 0\nfrozen 0\n").unwrap());
        assert!(!unpopulated("populated 1\n").unwrap());
        for bad in ["", "populated 2", "populated 0\npopulated 1", "frozen 0"] {
            assert!(unpopulated(bad).is_err());
        }
    }
    #[test]
    fn rejects_broad_and_traversing_parent_paths() {
        for bad in ["/", "relative", "/a/../b", "/a//b", "/a/./b"] {
            assert!(validate_parent(bad).is_err());
        }
        assert!(validate_parent("/user.slice/runner.service").is_ok());
    }
    #[test]
    fn stale_identity_cannot_select_a_cleanup_target() {
        let (parent, boot_id, cgroup_namespace) = environment().unwrap();
        let id = ExecutionId::new();
        let mut value = NativeContainment {
            execution_id: id,
            parent,
            inode: 1,
            boot_id,
            cgroup_namespace,
        };
        assert!(value.checked_path(ExecutionId::new()).is_err());
        value.boot_id = "old-boot".into();
        assert!(value.checked_path(id).is_err());
    }
}
