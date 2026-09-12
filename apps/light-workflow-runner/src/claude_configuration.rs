use crate::worker_process::WorkerProcessConfig;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
impl WorkerProcessConfig {
    pub(crate) fn validate_claude_configuration(&self) -> Result<(), String> {
        match (&self.claude_home, &self.claude_executable) {
            (None, None) => return Ok(()),
            (Some(home), Some(executable)) => {
                if self.codex_home.is_some()
                    || self.codex_executable.is_some()
                    || self.broker.is_some()
                    || self.sandbox_launcher.is_some()
                {
                    return Err(
                        "Claude requires a separate personal coding worker configuration".into(),
                    );
                }
                if !home.is_absolute() || !executable.is_absolute() {
                    return Err("Claude paths must be absolute".into());
                }
                let h = std::fs::symlink_metadata(home).map_err(|e| e.to_string())?;
                let x = std::fs::symlink_metadata(executable).map_err(|e| e.to_string())?;
                if !h.is_dir()
                    || h.file_type().is_symlink()
                    || h.permissions().mode() & 0o002 != 0
                    || h.uid() != unsafe { libc::geteuid() }
                    || !x.is_file()
                    || x.file_type().is_symlink()
                    || x.permissions().mode() & 0o111 == 0
                {
                    return Err("Claude home must be owner-owned and not world-writable and executable must be a regular executable file".into());
                }
                if agent_runtime_protocol::canonical_digest(
                    &coding_agent_runtime::claude::capabilities(),
                )
                .map_err(|e| e.to_string())?
                    != self.capability_digest
                {
                    return Err(
                        "Claude runner requires the exact Claude worker capability digest".into(),
                    );
                }
            }
            _ => return Err("claudeHome and claudeExecutable must be configured together".into()),
        }
        Ok(())
    }
}
