//! Durable identity evidence, not permission to kill a PID or release a fence.
//! Process groups alone cannot prove cleanup of descendants that call setsid.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeProcessIdentity {
    pub pid: u32,
    pub start_ticks: u64,
    pub boot_id: String,
    pub pid_namespace: String,
    pub cgroup_membership: String,
    #[cfg(target_os = "linux")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub containment: Option<crate::native_containment::NativeContainment>,
}

impl NativeProcessIdentity {
    #[cfg(target_os = "linux")]
    pub fn capture(pid: u32) -> Result<Self, String> {
        if pid <= 1 {
            return Err("invalid native worker pid".into());
        }
        let root = std::path::PathBuf::from(format!("/proc/{pid}"));
        let first = std::fs::read_to_string(root.join("stat"))
            .map_err(|_| "native process stat unavailable")?;
        let start_ticks = start_ticks(&first)?;
        let boot_id = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .map_err(|_| "boot identity unavailable")?
            .trim()
            .to_owned();
        let pid_namespace = std::fs::read_link(root.join("ns/pid"))
            .map_err(|_| "pid namespace unavailable")?
            .to_string_lossy()
            .into_owned();
        let cgroup_membership = std::fs::read_to_string(root.join("cgroup"))
            .map_err(|_| "native cgroup identity unavailable")?;
        let last = std::fs::read_to_string(root.join("stat"))
            .map_err(|_| "native process disappeared during capture")?;
        if Self::ticks(&last)? != start_ticks {
            return Err("native process identity changed during capture".into());
        }
        Ok(Self {
            pid,
            start_ticks,
            boot_id,
            pid_namespace,
            cgroup_membership,
            containment: None,
        })
    }

    fn ticks(stat: &str) -> Result<u64, String> {
        start_ticks(stat)
    }
}

fn start_ticks(stat: &str) -> Result<u64, String> {
    // comm may contain spaces and ')'; fields after the final ')' begin at #3.
    stat.rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().nth(19))
        .and_then(|s| s.parse().ok())
        .filter(|ticks| *ticks > 0)
        .ok_or_else(|| "invalid native process start identity".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stat_parser_handles_parentheses_and_rejects_missing_identity() {
        let fields = (3..=21).map(|_| "0").collect::<Vec<_>>().join(" ");
        assert_eq!(
            start_ticks(&format!("42 (a tricky ) process) {fields} 12345 0")).unwrap(),
            12345
        );
        assert!(start_ticks("42 (missing) S 0").is_err());
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn captures_current_process_without_command_line_or_environment() {
        let value = NativeProcessIdentity::capture(std::process::id()).unwrap();
        assert_eq!(value.pid, std::process::id());
        assert!(!value.boot_id.is_empty());
        assert!(value.pid_namespace.starts_with("pid:["));
        assert!(!value.cgroup_membership.is_empty());
    }
}
