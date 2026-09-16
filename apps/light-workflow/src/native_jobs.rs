//! Workflow-owned intent before any native Agent send. The Agent pulls only
//! authenticated, bounded jobs and pins its own current published runtime policy.
use crate::{development_store::*, invocation::AuthenticatedInvocationContext};
use development_workflow_contract::{StageClaimReceipt, TurnCharge, TurnKind};
use serde_json::Value;
use sqlx::{PgPool, Row};
use uuid::Uuid;

#[allow(clippy::too_many_arguments)]
pub async fn enqueue(
    pool: &PgPool,
    host: Uuid,
    process: Uuid,
    task: Uuid,
    name: &str,
    agent: Uuid,
    input: Value,
    schema: Value,
    deadline: chrono::DateTime<chrono::Utc>,
    tokens: u64,
    cost: u64,
    depth: i32,
    max_depth: i32,
) -> Result<Uuid, Box<dyn std::error::Error + Send + Sync>> {
    enqueue_with_artifacts(
        pool, host, process, task, name, agent, input, schema, deadline, tokens, cost, depth,
        max_depth, None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn enqueue_with_artifacts(
    pool: &PgPool,
    host: Uuid,
    process: Uuid,
    task: Uuid,
    name: &str,
    agent: Uuid,
    mut input: Value,
    schema: Value,
    deadline: chrono::DateTime<chrono::Utc>,
    tokens: u64,
    cost: u64,
    depth: i32,
    max_depth: i32,
    artifacts: Option<&crate::artifact_store::DurableArtifactStore>,
) -> Result<Uuid, Box<dyn std::error::Error + Send + Sync>> {
    let mut tx = pool.begin().await?;
    let row=sqlx::query("SELECT i.principal_subject,i.end_user_subject,p.definition_snapshot FROM workflow_invocation_t i JOIN process_info_t p ON p.host_id=i.host_id AND p.process_id=i.process_id WHERE i.host_id=$1 AND i.process_id=$2 AND i.state IN('ACCEPTED','RUNNING','WAITING') AND i.cancel_requested_ts IS NULL AND i.deadline_ts>now() AND i.deadline_ts>=$3 FOR SHARE OF i")
        .bind(host).bind(process).bind(deadline).fetch_one(&mut *tx).await?;
    let definition: Value = row.get("definition_snapshot");
    if input.get("managerSnapshot").is_some() {
        check(
            is_development_definition(&definition),
            "snapshot reads require a development stage claim",
        )?;
        let receipt: Value = sqlx::query_scalar(
            "SELECT receipt FROM development_stage_t WHERE host_id=$1 AND process_id=$2",
        )
        .bind(host)
        .bind(process)
        .fetch_one(&mut *tx)
        .await?;
        let claim: StageClaimReceipt = serde_json::from_value(receipt)?;
        bind_snapshot_stage(&mut input, &claim.stage_execution_id)?;
    }
    if is_development_definition(&definition)
        && input.get("managerSnapshot").is_none()
        && let Some(slot) = definition
            .pointer("/document/metadata/developmentWorkflowTurns")
            .and_then(|v| v.get(name))
        && slot.get("kind").and_then(Value::as_str) == Some("review")
    {
        let (feature, receipt): (String, Value) = sqlx::query_as(
            "SELECT feature_id,receipt FROM development_stage_t WHERE host_id=$1 AND process_id=$2",
        )
        .bind(host)
        .bind(process)
        .fetch_one(&mut *tx)
        .await?;
        let claim: StageClaimReceipt = serde_json::from_value(receipt)?;
        bind_review_request(&mut input, slot, &claim, &feature, name, task, agent)?;
        crate::review_artifacts::prepare(
            &mut tx, artifacts, host, process, &feature, &claim, &mut input,
        )
        .await?;
    }
    // Digest only the fully bound request; replay must preserve server-owned IDs.
    let digest = execution_runner_protocol::canonical_sha256(&input)?;
    let job = light_client::workflow_job_transport::Job {
        host_id: host,
        job_id: task,
        process_id: process,
        task_id: task,
        agent_def_id: agent,
        end_user_subject: row.get("end_user_subject"),
        input: input.clone(),
        input_digest: digest.clone(),
        output_schema: schema.clone(),
        deadline: deadline.to_rfc3339(),
        token_budget: i64::try_from(tokens)?,
        cost_budget_micros: i64::try_from(cost)?,
        depth,
        maximum_depth: max_depth,
        cancellation_requested: false,
    };
    job.validate()?;
    if is_development_definition(&definition) {
        let value: Value = sqlx::query_scalar(
            "SELECT receipt FROM development_stage_t WHERE host_id=$1 AND process_id=$2",
        )
        .bind(host)
        .bind(process)
        .fetch_one(&mut *tx)
        .await?;
        let claim: StageClaimReceipt = serde_json::from_value(value)?;
        if let Some(read) = input.get("managerSnapshot") {
            let read: workspace_execution_protocol::ManagerSnapshotRead =
                serde_json::from_value(read.clone())?;
            read.validate()?;
            check(
                definition
                    .pointer("/document/metadata/developmentWorkflowSnapshotTasks")
                    .and_then(Value::as_array)
                    .is_some_and(|v| v.iter().any(|v| v.as_str() == Some(name))),
                "snapshot task is not pinned in the development definition",
            )?;
            let principal: String = row.get("principal_subject");
            let user: String = row.get("end_user_subject");
            let auth = AuthenticatedInvocationContext {
                host_id: host,
                principal_subject: &principal,
                end_user_subject: &user,
                update_user: "snapshot-dispatch",
                user_authorization: None,
                user_authorization_exp: None,
            };
            let feature = load_feature(&mut tx, &auth, &read.feature_id).await?;
            check_vm_owner(&mut tx, host, &feature).await?;
            check(
                feature.state == development_workflow_contract::FeatureState::Active
                    && feature.active_claim.as_ref() == Some(&claim)
                    && read.stage_id == claim.stage_execution_id,
                "snapshot task has stale feature ownership",
            )?;
        } else {
            let slot = definition
                .pointer("/document/metadata/developmentWorkflowTurns")
                .and_then(|v| v.get(name))
                .ok_or("development native task has no pinned round slot")?;
            let kind: TurnKind =
                serde_json::from_value(slot.get("kind").cloned().ok_or("round kind missing")?)?;
            let charge = TurnCharge {
                logical_turn_id: format!("{}:{name}", claim.stage_execution_id),
                stage_execution_id: claim.stage_execution_id.clone(),
                budget_scope: slot
                    .get("budgetScope")
                    .and_then(Value::as_str)
                    .ok_or("budget scope missing")?
                    .into(),
                kind,
                remediation_round_id: slot
                    .get("remediationRoundId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            };
            let principal: String = row.get("principal_subject");
            let user: String = row.get("end_user_subject");
            let auth = AuthenticatedInvocationContext {
                host_id: host,
                principal_subject: &principal,
                end_user_subject: &user,
                update_user: "workflow-native-dispatch",
                user_authorization: None,
                user_authorization_exp: None,
            };
            // Identical uncertainty means reconcile the durable job; never invent a
            // fresh turn or charge on retry. Agent admission is idempotent on task ID.
            let request_digest =
                workflow_invocation_contract::canonical_sha256(&serde_json::to_value(&job)?)?;
            crate::development_execution::reserve_task_turn(
                &mut tx,
                &auth,
                &claim,
                task,
                &charge,
                &request_digest,
            )
            .await?;
            if charge.kind == TurnKind::Review {
                let binding = review_binding(slot, &input, &claim, task, agent)?;
                check(
                    binding.review_id == charge.logical_turn_id,
                    "review identity differs from reserved turn",
                )?;
                crate::development_handoff::allocate_review(&mut tx, &auth, binding).await?;
            }
        }
    } else if input.get("managerSnapshot").is_some() {
        return Err("snapshot reads require a development stage claim".into());
    }
    sqlx::query("INSERT INTO workflow_agent_job_t(host_id,job_id,workflow_process_id,workflow_task_id,agent_def_id,idempotency_key,input,input_schema_digest,output_schema,deadline_ts,token_budget,cost_budget_micros,delegation_depth,maximum_delegation_depth) VALUES($1,$2,$3,$2,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) ON CONFLICT(host_id,job_id) DO NOTHING")
        .bind(host).bind(task).bind(process).bind(agent).bind(format!("workflow:{process}:{task}"))
        .bind(&input).bind(&digest).bind(&schema).bind(deadline).bind(job.token_budget).bind(job.cost_budget_micros)
        .bind(depth).bind(max_depth).execute(&mut *tx).await?;
    let same:bool=sqlx::query_scalar("SELECT workflow_process_id=$3 AND agent_def_id=$4 AND input=$5 AND output_schema=$6 AND deadline_ts=$7 AND token_budget=$8 AND cost_budget_micros=$9 AND delegation_depth=$10 AND maximum_delegation_depth=$11 FROM workflow_agent_job_t WHERE host_id=$1 AND job_id=$2")
        .bind(host).bind(task).bind(process).bind(agent).bind(input).bind(schema).bind(deadline)
        .bind(job.token_budget).bind(job.cost_budget_micros).bind(depth).bind(max_depth).fetch_one(&mut *tx).await?;
    check(same, "native job replay changed immutable request")?;
    tx.commit().await?;
    Ok(task)
}

fn bind_snapshot_stage(input: &mut Value, stage: &str) -> StoreResult<()> {
    let read = input
        .get_mut("managerSnapshot")
        .and_then(Value::as_object_mut)
        .ok_or(StoreError::Conflict("snapshot request must be an object"))?;
    if let Some(supplied) = read.get("stageId") {
        check(
            supplied.as_str() == Some(stage),
            "snapshot stage differs from accepted claim",
        )?;
    } else {
        read.insert("stageId".into(), Value::String(stage.into()));
    }
    Ok(())
}

/// Bind server-generated identities before hashing the immutable Agent job.
/// Explicit conflicting values are errors, never silently overwritten.
fn bind_review_request(
    input: &mut Value,
    slot: &Value,
    claim: &StageClaimReceipt,
    feature: &str,
    name: &str,
    task: Uuid,
    agent: Uuid,
) -> StoreResult<()> {
    let binding = input
        .get_mut("reviewBinding")
        .and_then(Value::as_object_mut)
        .ok_or(StoreError::Conflict("review binding must be an object"))?;
    for (key, expected) in [
        ("featureRunId", Value::String(feature.into())),
        (
            "stageExecutionId",
            Value::String(claim.stage_execution_id.clone()),
        ),
        (
            "reviewId",
            Value::String(format!("{}:{name}", claim.stage_execution_id)),
        ),
        ("sessionId", Value::String(task.to_string())),
        (
            "reviewer",
            slot.get("reviewer")
                .cloned()
                .ok_or(StoreError::Conflict("pinned reviewer missing"))?,
        ),
    ] {
        if let Some(value) = binding.get(key) {
            check(
                value == &expected,
                "review request conflicts with Workflow identity",
            )?;
        } else {
            binding.insert(key.into(), expected);
        }
    }
    let binding = review_binding(slot, input, claim, task, agent)?;
    check(
        input.pointer("/workspace/intent").and_then(Value::as_str) == Some("review"),
        "development review requires read-only workspace review intent",
    )?;
    let instruction = input
        .pointer_mut("/workspace/instruction")
        .ok_or(StoreError::Conflict("review instruction missing"))?;
    let original = instruction
        .as_str()
        .ok_or(StoreError::Conflict("review instruction must be text"))?;
    let contract = format!(
        "\nWorkflow-owned review binding: {}\nReturn only a JSON ReviewResult object with binding (exactly as above), accepted, evidence (id and digest), existingFindings, and newFindings. Do not wrap it in Markdown or change the binding.",
        serde_json::to_string(&binding)?
    );
    if !original.ends_with(&contract) {
        *instruction = Value::String(format!("{original}{contract}"));
    }
    Ok(())
}

/// The pinned round selects the reviewer and Agent. The workflow supplies the
/// candidate binding; a worker cannot allocate or replace its own review ID.
fn review_binding(
    slot: &Value,
    input: &Value,
    claim: &StageClaimReceipt,
    task: Uuid,
    agent: Uuid,
) -> StoreResult<development_workflow_contract::ReviewBinding> {
    let binding: development_workflow_contract::ReviewBinding = serde_json::from_value(
        input
            .get("reviewBinding")
            .cloned()
            .ok_or(StoreError::Conflict("review binding missing"))?,
    )?;
    let pinned_agent = slot
        .get("agentDefId")
        .and_then(Value::as_str)
        .and_then(|v| Uuid::parse_str(v).ok());
    let reviewer: development_workflow_contract::Reviewer = serde_json::from_value(
        slot.get("reviewer")
            .cloned()
            .ok_or(StoreError::Conflict("pinned reviewer missing"))?,
    )?;
    check(
        pinned_agent == Some(agent)
            && binding.reviewer == reviewer
            && binding.stage_execution_id == claim.stage_execution_id
            && binding.session_id == task.to_string(),
        "review binding differs from pinned reviewer, Agent, stage or session",
    )?;
    Ok(binding)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn review_request_binds_generated_ids_before_digest_and_replays_exactly() {
        let task = Uuid::now_v7();
        let agent = Uuid::now_v7();
        let claim = StageClaimReceipt {
            claim_id: "claim".into(),
            request_digest: "digest".into(),
            stage_execution_id: "stage".into(),
            workflow_instance_id: "run".into(),
            process_id: "process".into(),
            feature_version: 1,
        };
        let slot = json!({"kind":"review","reviewer":"claude","agentDefId":agent});
        let original = json!({"reviewBinding":{"candidate":format!("sha256:{}", "a".repeat(64)),"repositories":["repo"]},
            "workspace":{"intent":"review","instruction":"Review this candidate."}});
        let mut input = original.clone();
        bind_review_request(&mut input, &slot, &claim, "feature", "review", task, agent).unwrap();
        let bound = input.clone();
        bind_review_request(&mut input, &slot, &claim, "feature", "review", task, agent).unwrap();
        assert_eq!(input, bound);
        assert_eq!(input["reviewBinding"]["reviewId"], "stage:review");
        assert_eq!(input["reviewBinding"]["sessionId"], task.to_string());
        for key in [
            "featureRunId",
            "stageExecutionId",
            "reviewId",
            "sessionId",
            "reviewer",
        ] {
            let mut wrong = original.clone();
            wrong["reviewBinding"][key] = json!("wrong");
            assert!(
                bind_review_request(&mut wrong, &slot, &claim, "feature", "review", task, agent)
                    .is_err()
            );
        }
        let mut wrong = original.clone();
        wrong["workspace"]["intent"] = json!("implement");
        assert!(
            bind_review_request(&mut wrong, &slot, &claim, "feature", "review", task, agent)
                .is_err()
        );
        assert!(
            bind_review_request(
                &mut original.clone(),
                &slot,
                &claim,
                "feature",
                "review",
                task,
                Uuid::now_v7()
            )
            .is_err()
        );
    }

    #[test]
    fn snapshot_stage_is_bound_before_digest_and_replays_exactly() {
        let mut input =
            json!({"managerSnapshot":{"featureId":"feature","snapshotId":"snapshot","offset":0}});
        bind_snapshot_stage(&mut input, "accepted-stage").unwrap();
        let digest = execution_runner_protocol::canonical_sha256(&input).unwrap();
        bind_snapshot_stage(&mut input, "accepted-stage").unwrap();
        assert_eq!(
            execution_runner_protocol::canonical_sha256(&input).unwrap(),
            digest
        );
        assert!(bind_snapshot_stage(&mut input, "other-stage").is_err());
        assert!(bind_snapshot_stage(&mut json!({"managerSnapshot":null}), "stage").is_err());
        assert!(
            bind_snapshot_stage(&mut json!({"managerSnapshot":{"stageId":null}}), "stage").is_err()
        );
        assert_eq!(input["managerSnapshot"]["featureId"], "feature");
    }

    #[test]
    fn native_review_requires_pinned_agent_reviewer_stage_and_session() {
        let task = Uuid::now_v7();
        let agent = Uuid::now_v7();
        let claim = StageClaimReceipt {
            claim_id: "claim".into(),
            request_digest: "digest".into(),
            stage_execution_id: "stage".into(),
            workflow_instance_id: "run".into(),
            process_id: "process".into(),
            feature_version: 2,
        };
        let slot = json!({"agentDefId":agent,"reviewer":"claude"});
        let input = json!({"reviewBinding":{
            "featureRunId":"feature","reviewId":"stage:review",
            "stageExecutionId":"stage","reviewer":"claude","sessionId":task,
            "candidate":format!("sha256:{}", "a".repeat(64)),"repositories":["repo"]
        }});
        assert!(review_binding(&slot, &input, &claim, task, agent).is_ok());
        assert!(review_binding(&slot, &input, &claim, task, Uuid::now_v7()).is_err());
        assert!(review_binding(&slot, &input, &claim, Uuid::now_v7(), agent).is_err());
        assert!(
            review_binding(&json!({"reviewer":"claude"}), &input, &claim, task, agent).is_err()
        );
        for (key, replacement) in [
            ("reviewer", json!("codex")),
            ("stageExecutionId", json!("other-stage")),
            ("sessionId", json!(Uuid::now_v7())),
        ] {
            let mut changed = input.clone();
            changed["reviewBinding"][key] = replacement;
            assert!(
                review_binding(&slot, &changed, &claim, task, agent).is_err(),
                "{key}"
            );
        }
        assert!(review_binding(&slot, &json!({}), &claim, task, agent).is_err());
    }
}
