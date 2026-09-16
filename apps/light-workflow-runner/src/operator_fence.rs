//! Explicit local operator recovery. Never invoked by worker input or transport.
//! Only the dedicated Claude user unit may be stopped. Confirmation is recorded
//! separately from the immutable UNKNOWN result, after kernel emptiness proof.
use crate::journal::Journal;
use execution_runner_protocol::{AttemptState, ExecutionId, LeaseContext};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Command,
};

const UNIT: &str = "light-workflow-runner-claude-personal.service";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FenceEvidence {
    pub(crate) lease: LeaseContext,
    pub(crate) unit: String,
    pub(crate) invocation: String,
    pub(crate) parent: String,
    pub(crate) inode: u64,
    pub(crate) boot: String,
    pub(crate) namespace: String,
    pub(crate) terminal_digest: String,
}

impl FenceEvidence {
    pub fn reference(&self) -> Result<String, String> {
        execution_runner_protocol::canonical_sha256(self)
            .map(|hash| format!("operator-unit-fence:{hash}"))
            .map_err(|_| "invalid operator fence evidence".into())
    }
    pub(crate) fn validate(
        &self,
        lease: &LeaseContext,
        terminal: &execution_runner_protocol::TerminalLeaseResult,
    ) -> Result<(), String> {
        self.validate_identity(lease)?;
        if self.terminal_digest
            != execution_runner_protocol::canonical_sha256(terminal)
                .map_err(|_| "invalid terminal digest")?
        {
            return Err("operator fence terminal mismatch".into());
        }
        Ok(())
    }
    pub(crate) fn validate_identity(&self, lease: &LeaseContext) -> Result<(), String> {
        if self.unit != UNIT
            || self.lease != *lease
            || self.invocation.len() != 32
            || !self.invocation.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err("operator fence lease or terminal mismatch".into());
        }
        validate_parent(&self.parent)?;
        if self.boot
            != std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
                .map_err(|_| "boot unavailable")?
                .trim()
            || self.namespace
                != std::fs::read_link("/proc/self/ns/cgroup")
                    .map_err(|_| "namespace unavailable")?
                    .to_string_lossy()
        {
            return Err("operator fence environment mismatch".into());
        }
        Ok(())
    }
}

fn validate_parent(parent: &str) -> Result<(), String> {
    let expected = format!(
        "/user.slice/user-{}.slice/user@{}.service/app.slice/{UNIT}",
        unsafe { libc::geteuid() },
        unsafe { libc::geteuid() }
    );
    if parent != expected {
        return Err("operator fence is not the dedicated owner runner unit".into());
    }
    Ok(())
}

fn show() -> Result<BTreeMap<String, String>, String> {
    let output = Command::new("/usr/bin/systemctl")
        .args([
            "--user",
            "show",
            UNIT,
            "-p",
            "MainPID",
            "-p",
            "InvocationID",
            "-p",
            "ControlGroup",
            "-p",
            "ActiveState",
            "-p",
            "KillMode",
        ])
        .output()
        .map_err(|_| "systemd inspection failed")?;
    if !output.status.success() {
        return Err("systemd inspection rejected".into());
    }
    Ok(String::from_utf8(output.stdout)
        .map_err(|_| "invalid unit response")?
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(a, b)| (a.into(), b.into()))
        .collect())
}

fn value<'a>(map: &'a BTreeMap<String, String>, key: &str) -> Result<&'a str, String> {
    map.get(key)
        .map(String::as_str)
        .ok_or_else(|| "incomplete unit identity".into())
}

/// Caller must explicitly select --confirm-fence. The command leaves the unit
/// stopped; deployment/re-enrollment is a separate operator action.
pub fn reset(config_path: &Path, execution: ExecutionId) -> Result<String, String> {
    let config_path =
        std::fs::canonicalize(config_path).map_err(|_| "runner configuration unavailable")?;
    let metadata =
        std::fs::metadata(&config_path).map_err(|_| "configuration metadata unavailable")?;
    use std::os::unix::fs::PermissionsExt;
    if metadata.uid() != unsafe { libc::geteuid() }
        || !metadata.is_file()
        || metadata.permissions().mode() & 0o022 != 0
    {
        return Err("runner configuration ownership invalid".into());
    }
    let config: serde_yaml::Value = serde_yaml::from_slice(
        &std::fs::read(&config_path).map_err(|_| "configuration read failed")?,
    )
    .map_err(|_| "invalid runner configuration")?;
    if config["runnerId"].as_str() != Some("personal-claude-runner")
        || config["maximumConcurrency"].as_u64() != Some(1)
        || config["agentWorker"]["nativeCgroup"].as_bool() != Some(true)
    {
        return Err("operator fence requires the exclusive personal Claude native runner".into());
    }
    let data = PathBuf::from(
        config["dataDirectory"]
            .as_str()
            .ok_or("runner data directory missing")?,
    );
    if !data.is_absolute() {
        return Err("absolute runner data directory required".into());
    }
    let journal = Journal::open(&data.join("execution-journal.sqlite"))?;
    let record = journal
        .find(execution)?
        .ok_or("operator execution not journaled")?;
    let terminal = record
        .terminal_result
        .as_ref()
        .ok_or("operator execution is not terminal")?;
    if terminal.result.state != AttemptState::Unknown
        || record.backend_operation_id.as_deref()
            != Some(format!("agent-worker:{execution}").as_str())
    {
        return Err("operator fence only recovers terminal UNKNOWN native executions".into());
    }
    let before = show()?;
    let parent = value(&before, "ControlGroup")?.to_owned();
    validate_parent(&parent)?;
    if value(&before, "ActiveState")? != "active" || value(&before, "KillMode")? != "control-group"
    {
        return Err("runner must be active with control-group kill mode".into());
    }
    let pid: u32 = value(&before, "MainPID")?
        .parse()
        .map_err(|_| "invalid runner PID")?;
    if pid <= 1 {
        return Err("runner PID unavailable".into());
    }
    let proc = PathBuf::from(format!("/proc/{pid}"));
    let membership =
        std::fs::read_to_string(proc.join("cgroup")).map_err(|_| "runner cgroup unavailable")?;
    if !membership
        .lines()
        .any(|line| line == format!("0::{parent}"))
    {
        return Err("runner unit membership mismatch".into());
    }
    let environment =
        std::fs::read(proc.join("environ")).map_err(|_| "runner environment unavailable")?;
    let configured = environment
        .split(|b| *b == 0)
        .find_map(|entry| entry.strip_prefix(b"LIGHT_WORKFLOW_RUNNER_CONFIG_FILE="))
        .ok_or("runner configuration binding unavailable")?;
    let configured = PathBuf::from(
        std::str::from_utf8(configured).map_err(|_| "invalid runner configuration binding")?,
    );
    if std::fs::canonicalize(configured).map_err(|_| "runner configuration binding missing")?
        != config_path
    {
        return Err("journal does not belong to selected runner service".into());
    }
    let scope = Path::new("/sys/fs/cgroup").join(parent.trim_start_matches('/'));
    let evidence = FenceEvidence {
        lease: record.lease_context.clone(),
        unit: UNIT.into(),
        invocation: value(&before, "InvocationID")?.into(),
        parent,
        inode: std::fs::metadata(&scope)
            .map_err(|_| "runner scope unavailable")?
            .ino(),
        boot: std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .map_err(|_| "boot unavailable")?
            .trim()
            .into(),
        namespace: std::fs::read_link("/proc/self/ns/cgroup")
            .map_err(|_| "namespace unavailable")?
            .to_string_lossy()
            .into(),
        terminal_digest: execution_runner_protocol::canonical_sha256(terminal)
            .map_err(|_| "invalid terminal digest")?,
    };
    evidence.validate(&record.lease_context, terminal)?;
    journal.operator_fence_intent(&evidence)?;
    if show()? != before {
        return Err("runner identity changed before fence".into());
    }
    let stopped = Command::new("/usr/bin/systemctl")
        .args(["--user", "stop", UNIT])
        .status()
        .map_err(|_| "runner stop failed")?;
    if !stopped.success() {
        return Err("runner stop rejected; no cleanup confirmation".into());
    }
    let after = show()?;
    if value(&after, "ActiveState")? != "inactive" || value(&after, "MainPID")? != "0" {
        return Err("runner restarted during fencing".into());
    }
    match std::fs::metadata(&scope) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Ok(meta) if meta.ino() == evidence.inode => {
            let events = std::fs::read_to_string(scope.join("cgroup.events"))
                .map_err(|_| "runner emptiness unavailable")?;
            if events
                .lines()
                .filter(|line| line.starts_with("populated "))
                .collect::<Vec<_>>()
                != vec!["populated 0"]
            {
                return Err("runner scope still populated".into());
            }
        }
        _ => return Err("runner scope replaced during fence".into()),
    }
    evidence.validate(&record.lease_context, terminal)?;
    journal.operator_fence_confirm(&evidence)?;
    evidence.reference()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_shared_foreign_and_traversal_scopes() {
        for parent in [
            "/",
            "/user.slice",
            "/user.slice/user-0.slice/user@0.service/app.slice/other.service",
            "/../light-workflow-runner-claude-personal.service",
        ] {
            assert!(validate_parent(parent).is_err());
        }
        let uid = unsafe { libc::geteuid() };
        assert!(
            validate_parent(&format!(
                "/user.slice/user-{uid}.slice/user@{uid}.service/app.slice/{UNIT}"
            ))
            .is_ok()
        );
    }
}
