use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::OpenOptions,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};
use workspace_execution_protocol::{WorkspaceAccessPolicy, WorkspaceExecutionSpec};

/// Private host configuration, never a browser or worker tool argument.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunnerWorkspaceConfig {
    pub store: PathBuf,
    pub bindings: Vec<WorkspaceAccessPolicy>,
}
impl RunnerWorkspaceConfig {
    pub fn load(path: &Path) -> Result<Self> {
        ensure!(path.is_absolute(), "workspace config path must be absolute");
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        let metadata = file.metadata()?;
        ensure!(
            metadata.is_file()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.permissions().mode() & 0o077 == 0
                && metadata.len() <= 1024 * 1024,
            "workspace config must be a bounded owner-only file"
        );
        let mut bytes = Vec::new();
        file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 1024 * 1024, "workspace config exceeds limit");
        let config: Self = serde_json::from_slice(&bytes)?;
        ensure!(
            config.store.is_absolute(),
            "workspace store must be absolute"
        );
        let mut ids = std::collections::BTreeSet::new();
        for binding in &config.bindings {
            binding.validate()?;
            ensure!(
                ids.insert(&binding.workspace_id),
                "duplicate workspace binding"
            );
        }
        Ok(config)
    }
    pub fn authorize(&self, spec: &WorkspaceExecutionSpec) -> Result<()> {
        spec.validate()?;
        ensure!(
            self.bindings.iter().any(|binding| binding == &spec.binding),
            "workspace binding is absent, revoked, or differs from the host binding"
        );
        Ok(())
    }
}
