use crate::executor::TaskExecutor;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::PgPool;
use std::sync::Arc;
use thiserror::Error;
use uuid::Uuid;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AgentJobRequest {
    pub job_id: Uuid,
    pub process_id: Uuid,
    pub task_id: Uuid,
    pub agent_def_id: Uuid,
    pub idempotency_key: String,
    pub input: Value,
    pub input_schema_digest: String,
    pub output_schema: Value,
    pub policy_digest: String,
    pub data_boundary_digest: String,
    pub deadline: DateTime<Utc>,
    pub token_budget: u64,
    pub cost_budget_micros: u64,
    pub delegation_depth: u8,
    pub maximum_delegation_depth: u8,
}
impl AgentJobRequest {
    pub fn validate(
        &self,
        parent_deadline: DateTime<Utc>,
        parent_tokens: u64,
        parent_cost: u64,
        parent_boundary: &str,
    ) -> Result<(), AgentJobError> {
        if self.deadline > parent_deadline
            || self.deadline <= Utc::now()
            || self.token_budget > parent_tokens
            || self.cost_budget_micros > parent_cost
            || self.data_boundary_digest != parent_boundary
        {
            return Err(AgentJobError::Widening);
        }
        if self.delegation_depth > self.maximum_delegation_depth {
            return Err(AgentJobError::Cycle);
        }
        Ok(())
    }
}
pub fn validate_public_output(schema: &Value, output: &Value) -> Result<(), AgentJobError> {
    let object = output.as_object().ok_or(AgentJobError::Output)?;
    if let Some(required) = schema.get("required").and_then(Value::as_array) {
        for key in required.iter().filter_map(Value::as_str) {
            if !object.contains_key(key) {
                return Err(AgentJobError::Output);
            }
        }
    }
    Ok(())
}
#[derive(Debug, Error, PartialEq, Eq)]
pub enum AgentJobError {
    #[error("agent job widens inherited authority or budget")]
    Widening,
    #[error("agent workflow delegation depth exceeded")]
    Cycle,
    #[error("agent job public output failed schema validation")]
    Output,
}

pub struct AgentJobReconciler {
    pool: PgPool,
    executor: Arc<TaskExecutor>,
}
impl AgentJobReconciler {
    pub fn new(pool: PgPool, executor: Arc<TaskExecutor>) -> Self {
        Self { pool, executor }
    }
    pub async fn run(&self) -> Result<(), sqlx::Error> {
        loop {
            let jobs:Vec<(Uuid,Uuid)>=sqlx::query_as("SELECT host_id,job_id FROM agent_job_t
                WHERE state IN('SUCCEEDED','FAILED','CANCELLED','UNKNOWN') ORDER BY updated_ts LIMIT 100")
                .fetch_all(&self.pool).await?;
            let mut progressed = false;
            for (host, job) in jobs {
                progressed |= self.executor.reconcile_agent_job(host, job).await?;
            }
            if !progressed {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    #[test]
    fn bridge_narrows_and_detects_cycles() {
        let now = Utc::now();
        let mut r = AgentJobRequest {
            job_id: Uuid::new_v4(),
            process_id: Uuid::new_v4(),
            task_id: Uuid::new_v4(),
            agent_def_id: Uuid::new_v4(),
            idempotency_key: "one".into(),
            input: serde_json::json!({}),
            input_schema_digest: "d".into(),
            output_schema: serde_json::json!({"required":["answer"]}),
            policy_digest: "p".into(),
            data_boundary_digest: "b".into(),
            deadline: now + Duration::minutes(1),
            token_budget: 10,
            cost_budget_micros: 10,
            delegation_depth: 1,
            maximum_delegation_depth: 2,
        };
        assert!(r.validate(now + Duration::minutes(2), 20, 20, "b").is_ok());
        r.delegation_depth = 3;
        assert_eq!(
            r.validate(now + Duration::minutes(2), 20, 20, "b"),
            Err(AgentJobError::Cycle)
        );
        assert!(validate_public_output(&r.output_schema, &serde_json::json!({"answer":1})).is_ok());
    }
}
