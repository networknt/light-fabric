//! Bounded, typed messages for the authenticated Agent pull bridge.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;
use zeroize::Zeroize;

/// Transient poll envelope. The bearer is never part of `Job` and is wiped
/// when the receiving app finishes admission or drops a failed delivery.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JobDelivery {
    pub job: Job,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner_token: Option<String>,
}

impl Drop for JobDelivery {
    fn drop(&mut self) {
        if let Some(token) = &mut self.owner_token {
            token.zeroize();
        }
    }
}

impl From<Job> for JobDelivery {
    fn from(job: Job) -> Self {
        Self {
            job,
            owner_token: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Job {
    pub host_id: Uuid,
    pub job_id: Uuid,
    pub process_id: Uuid,
    pub task_id: Uuid,
    pub agent_def_id: Uuid,
    pub end_user_subject: String,
    pub input: Value,
    pub input_digest: String,
    pub output_schema: Value,
    pub deadline: String,
    pub token_budget: i64,
    pub cost_budget_micros: i64,
    pub depth: i32,
    pub maximum_depth: i32,
    /// Cleanup delivery only; never authorizes a new turn.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cancellation_requested: bool,
}

impl Job {
    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.end_user_subject.is_empty()
                && self.end_user_subject.len() <= 255
                && self.end_user_subject.trim() == self.end_user_subject
                && !self.end_user_subject.chars().any(char::is_control),
            "invalid job owner"
        );
        anyhow::ensure!(
            !self.host_id.is_nil()
                && !self.job_id.is_nil()
                && !self.process_id.is_nil()
                && !self.task_id.is_nil()
                && !self.agent_def_id.is_nil(),
            "invalid job identity"
        );
        anyhow::ensure!(
            self.token_budget > 0
                && self.cost_budget_micros >= 0
                && self.depth >= 0
                && self.depth <= self.maximum_depth,
            "invalid job budget"
        );
        anyhow::ensure!(
            self.input.is_object()
                && self.output_schema.is_object()
                && serde_json::to_vec(self)?.len() <= 256 * 1024,
            "invalid job input bound"
        );
        anyhow::ensure!(
            self.input_digest.len() == 64
                && self
                    .input_digest
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid input digest"
        );
        anyhow::ensure!(
            !self.deadline.is_empty() && self.deadline.len() <= 64,
            "invalid job deadline"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Report {
    pub host_id: Uuid,
    pub job_id: Uuid,
    pub state: String,
    pub output: Option<Value>,
    pub error: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleanup: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Poll {
    pub host_id: Uuid,
}

#[cfg(test)]
mod tests {
    use super::*;
    fn job() -> Job {
        Job {
            host_id: Uuid::new_v4(),
            job_id: Uuid::new_v4(),
            process_id: Uuid::new_v4(),
            task_id: Uuid::new_v4(),
            agent_def_id: Uuid::new_v4(),
            end_user_subject: Uuid::new_v4().to_string(),
            input: serde_json::json!({}),
            input_digest: "a".repeat(64),
            output_schema: serde_json::json!({"type":"object"}),
            deadline: "2026-09-14T12:00:00Z".into(),
            token_budget: 10,
            cost_budget_micros: 0,
            depth: 0,
            maximum_depth: 1,
            cancellation_requested: false,
        }
    }
    #[test]
    fn workflow_job_requires_an_explicit_bounded_owner() {
        let valid = job();
        for owner in ["", " owner", "owner ", "owner\nother"] {
            let mut changed = valid.clone();
            changed.end_user_subject = owner.into();
            assert!(changed.validate().is_err());
        }
        let mut changed = valid.clone();
        changed.end_user_subject = "a".repeat(256);
        assert!(changed.validate().is_err());
        let mut missing = serde_json::to_value(valid).unwrap();
        missing.as_object_mut().unwrap().remove("endUserSubject");
        assert!(serde_json::from_value::<Job>(missing).is_err());
    }

    #[test]
    fn job_transport_bounds_and_identity() {
        let valid = job();
        assert!(valid.validate().is_ok());
        let mut changed = valid.clone();
        changed.host_id = Uuid::nil();
        assert!(changed.validate().is_err());
        let mut changed = valid.clone();
        changed.depth = 2;
        assert!(changed.validate().is_err());
        let mut changed = valid.clone();
        changed.token_budget = 0;
        assert!(changed.validate().is_err());
        let mut changed = valid.clone();
        changed.input_digest = "A".repeat(64);
        assert!(changed.validate().is_err());
        let mut changed = valid.clone();
        changed.input = serde_json::json!({"text":"x".repeat(256*1024)});
        assert!(changed.validate().is_err());
        let mut json = serde_json::to_value(valid).unwrap();
        json["policyOverride"] = serde_json::json!(true);
        assert!(serde_json::from_value::<Job>(json).is_err());
    }

    #[test]
    fn cleanup_delivery_is_explicit_and_legacy_jobs_default_to_execution() {
        let mut value = serde_json::to_value(job()).unwrap();
        assert!(value.get("cancellationRequested").is_none());
        assert!(
            !serde_json::from_value::<Job>(value.clone())
                .unwrap()
                .cancellation_requested
        );
        value["cancellationRequested"] = serde_json::json!(true);
        let cleanup: Job = serde_json::from_value(value.clone()).unwrap();
        assert!(cleanup.cancellation_requested);
        assert!(cleanup.validate().is_ok());
        assert_eq!(serde_json::to_value(cleanup).unwrap(), value);
        value["cancellationRequested"] = serde_json::json!("true");
        assert!(serde_json::from_value::<Job>(value).is_err());
    }
}
