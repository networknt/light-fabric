//! Only the two canonical GitHub GET shapes are observed; never export path/query/content.
use gateway_operational_store::{DISPATCH_CONTRACT_VERSION, DispatchObservation, DispatchPhase};
use uuid::Uuid;

pub fn in_scope(method: &str, path: &str) -> bool {
    if method != "GET" {
        return false;
    }
    let parts: Vec<_> = path.split('/').collect();
    matches!(parts.len(), 7 | 8)
        && parts[0].is_empty()
        && parts[1] == "github"
        && parts[2] == "repos"
        && parts[5] == "issues"
        && parts[3..5].iter().all(|p| {
            !p.is_empty()
                && !matches!(*p, "." | "..")
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        })
        && !parts[6].is_empty()
        && parts[6].bytes().all(|b| b.is_ascii_digit())
        && (parts.len() == 7 || parts[7] == "comments")
}

pub struct DispatchState {
    pub audit_id: Uuid,
    pub correlation_digest: String,
    pub deployment_config_digest: String,
    pub attempts: u32,
    pub handoffs: u32,
    pub writes_complete: bool,
    terminal: bool,
}
impl DispatchState {
    pub fn new(correlation_digest: String, deployment_config_digest: String) -> Self {
        Self {
            audit_id: Uuid::now_v7(),
            correlation_digest,
            deployment_config_digest,
            attempts: 0,
            handoffs: 0,
            writes_complete: true,
            terminal: false,
        }
    }
    pub fn event(&mut self, phase: DispatchPhase, error: bool) -> Option<DispatchObservation> {
        if self.terminal {
            self.writes_complete = false;
            return None;
        }
        match phase {
            DispatchPhase::Attempt => self.attempts = self.attempts.checked_add(1)?,
            DispatchPhase::Handoff => {
                if self.handoffs >= self.attempts {
                    self.writes_complete = false;
                    return None;
                }
                self.handoffs = self.handoffs.checked_add(1)?;
            }
            DispatchPhase::Terminal => self.terminal = true,
            DispatchPhase::Started => {}
        }
        Some(DispatchObservation {
            request_audit_id: self.audit_id,
            dispatch_phase: phase,
            dispatch_sequence: self
                .attempts
                .checked_add(self.handoffs)?
                .checked_add(u32::from(self.terminal))?,
            upstream_attempt_count: self.attempts,
            upstream_handoff_count: self.handoffs,
            observation_complete: self.terminal && self.writes_complete,
            deployment_config_digest: self.deployment_config_digest.clone(),
            observer_contract_version: DISPATCH_CONTRACT_VERSION,
            completion_phase: if self.terminal {
                if error { "error" } else { "response" }
            } else {
                "in_progress"
            }
            .into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scope_excludes_other_endpoints_methods_and_aliases() {
        for suffix in ["", "/comments"] {
            assert!(in_scope(
                "GET",
                &format!("/github/repos/a/b/issues/25{suffix}")
            ));
        }
        for path in [
            "/github/repos/a/b/issues/25/labels",
            "/github/repos/a/b/issues/25/",
            "/github/repos/a/b/issues/%32%35",
            "/github/repos/a/../issues/25",
            "/github/repos/a/b/issues/25/comments/x",
        ] {
            assert!(!in_scope("GET", path));
        }
        assert!(!in_scope("POST", "/github/repos/a/b/issues/25"));
    }
    #[test]
    fn retry_connection_failure_and_failed_writer_cannot_hide_attempts() {
        let mut s = DispatchState::new(
            "sha256:".to_string() + &"a".repeat(64),
            "sha256:".to_string() + &"b".repeat(64),
        );
        assert_eq!(
            s.event(DispatchPhase::Started, false)
                .unwrap()
                .dispatch_sequence,
            0
        );
        s.event(DispatchPhase::Attempt, false).unwrap(); // failed connect, no handoff
        s.writes_complete = false;
        s.event(DispatchPhase::Attempt, false).unwrap(); // retry/reused connection
        s.event(DispatchPhase::Handoff, false).unwrap();
        let t = s.event(DispatchPhase::Terminal, true).unwrap();
        assert_eq!(
            (
                t.upstream_attempt_count,
                t.upstream_handoff_count,
                t.dispatch_sequence
            ),
            (2, 1, 4)
        );
        assert!(!t.observation_complete);
        assert!(s.event(DispatchPhase::Terminal, false).is_none());
    }
    #[test]
    fn denied_lifecycle_is_explicit_complete_zero() {
        let mut s = DispatchState::new(String::new(), String::new());
        s.event(DispatchPhase::Started, false).unwrap();
        let t = s.event(DispatchPhase::Terminal, false).unwrap();
        assert!(t.observation_complete);
        assert_eq!(
            (
                t.upstream_attempt_count,
                t.upstream_handoff_count,
                t.dispatch_sequence
            ),
            (0, 0, 1)
        );
    }
}
