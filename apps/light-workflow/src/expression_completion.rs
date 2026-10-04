//! W4 snapshots, completion authority and atomic expression preparation; no schema changes.
use super::*;
use expression_runtime::{Failure, failure};
use workflow_expression::{Category, Profile};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProfileDisposition {
    Legacy,
    V2,
    Deferred,
    Corrupt,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunnerReconciliation {
    Completed,
    Deferred,
    Failed,
    Replay,
}

pub(crate) fn require_authority(live: bool) -> Result<(), sqlx::Error> {
    if live {
        Ok(())
    } else {
        Err(sqlx::Error::Protocol("WORKFLOW_STALE_COMPLETION".into()))
    }
}

pub(crate) fn rollback_completion(error: &sqlx::Error) -> bool {
    matches!(error,sqlx::Error::Protocol(code) if matches!(code.as_str(),"WORKFLOW_STALE_COMPLETION"|"WORKFLOW_EXPRESSION_UNAVAILABLE"))
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum FailureRoute {
    Process,
    Branch,
    Compensation,
}
pub(super) fn failure_route(compensation: bool, branch: bool) -> FailureRoute {
    if compensation {
        FailureRoute::Compensation
    } else if branch {
        FailureRoute::Branch
    } else {
        FailureRoute::Process
    }
}

pub(super) fn fork_status(
    expected: i32,
    compete: bool,
    rows: &[(String, String, Option<Value>)],
) -> (usize, usize, bool, bool) {
    let completed = rows
        .iter()
        .filter(|(_, state, _)| state != "RUNNING")
        .count();
    let failed = rows
        .iter()
        .filter(|(_, state, _)| state == "FAILED")
        .count();
    let winner = rows.iter().any(|(_, state, _)| state == "COMPLETED");
    let all = completed == usize::try_from(expected).unwrap_or(usize::MAX);
    (
        completed,
        failed,
        if compete { winner || all } else { all },
        if compete { winner } else { failed == 0 },
    )
}
#[derive(Clone, Debug)]
pub(super) enum CompletionGuard {
    Runner { request: Uuid, attempt: i32 },
    Timer { generation: i64, fence: i64 },
    Agent { job: Uuid },
    Approval { approval: Uuid, execution: Uuid },
    Fork { join: Uuid },
}

pub(crate) fn disposition(
    profile: &str,
    snapshot: Option<&Value>,
    available: bool,
) -> ProfileDisposition {
    let derived = snapshot.map(workflow_expression::resolve_profile);
    match profile {
        "cel-workflow-v1" if derived.as_ref().is_none_or(|p| p == &Ok(Profile::LegacyV1)) => {
            ProfileDisposition::Legacy
        }
        "cel-workflow-v1" => ProfileDisposition::Corrupt,
        "cel-workflow-v2" if derived != Some(Ok(Profile::CelWorkflowV2)) => {
            ProfileDisposition::Corrupt
        }
        "cel-workflow-v2" if !available => ProfileDisposition::Deferred,
        "cel-workflow-v2" => ProfileDisposition::V2,
        _ if snapshot
            .and_then(|s| s.pointer("/document/metadata/lightExpressionProfile"))
            .and_then(Value::as_str)
            == Some(profile) =>
        {
            ProfileDisposition::Deferred
        }
        _ => ProfileDisposition::Corrupt,
    }
}

pub(crate) fn identity(
    profile: &str,
    snapshot: Option<&Value>,
    digest: Option<&str>,
    available: bool,
) -> ProfileDisposition {
    let result = disposition(profile, snapshot, available);
    if profile == "cel-workflow-v2" && result != ProfileDisposition::Corrupt {
        let Some(snapshot) = snapshot else {
            return ProfileDisposition::Corrupt;
        };
        if serde_json::from_value::<WorkflowDefinition>(snapshot.clone()).is_err()
            || digest != canonical_sha256(snapshot).ok().as_deref()
        {
            return ProfileDisposition::Corrupt;
        }
    }
    result
}

pub(super) fn parent_live(
    blocked: Option<&str>,
    deadline: Option<chrono::DateTime<Utc>>,
    authority: Option<chrono::DateTime<Utc>>,
    now: chrono::DateTime<Utc>,
) -> bool {
    blocked.is_none() && deadline.is_none_or(|d| d > now) && authority.is_none_or(|d| d > now)
}
pub(super) fn lease_live(
    owner: Option<Uuid>,
    fence: i64,
    expires: Option<chrono::DateTime<Utc>>,
    claimed: HostTaskLease,
    now: chrono::DateTime<Utc>,
) -> bool {
    owner == Some(claimed.owner)
        && fence == claimed.fencing_token
        && expires.is_some_and(|d| d > now)
}
pub(super) fn completion_status_live(status: &str, marker: Option<&str>, claimed: &str) -> bool {
    status == claimed
        && (matches!(status, "A" | "W")
            || (status == "C" && matches!(marker, Some("W4_FORK_PENDING" | "W4_APPROVAL_PENDING"))))
}

pub(crate) fn snapshot_context<'a>(
    stored: &'a Value,
    task: Uuid,
    profile: &str,
    digest: &str,
) -> Option<&'a Value> {
    (stored.get("format").and_then(Value::as_str) == Some("light-workflow-step-v1")
        && stored.get("taskId").and_then(Value::as_str) == Some(task.to_string().as_str())
        && stored.get("profile").and_then(Value::as_str) == Some(profile)
        && stored.get("definitionDigest").and_then(Value::as_str) == Some(digest))
    .then(|| stored.get("context").filter(|c| c.is_object()))
    .flatten()
}

pub(crate) async fn capture_step(
    tx: &mut Transaction<'_, Postgres>,
    host: Uuid,
    process: Uuid,
    task: Uuid,
) -> Result<bool, sqlx::Error> {
    // Caller has checked identity and availability; NOWAIT prevents an inverted-lock wait.
    let (context,profile,digest):(Value,String,String)=sqlx::query_as("SELECT context_data,expression_profile,definition_digest FROM process_info_t WHERE host_id=$1 AND process_id=$2 FOR UPDATE NOWAIT")
        .bind(host).bind(process).fetch_one(&mut **tx).await?;
    let (stored,marker,fence):(Option<Value>,Option<String>,i64)=sqlx::query_as("SELECT task_input,result_code,lease_fencing_token FROM task_info_t WHERE host_id=$1 AND task_id=$2 FOR UPDATE NOWAIT")
        .bind(host).bind(task).fetch_one(&mut **tx).await?;
    if let Some(stored) = stored.as_ref()
        && stored.get("format").and_then(Value::as_str) == Some("light-workflow-step-v1")
    {
        return Ok(snapshot_context(stored, task, &profile, &digest).is_some());
    }
    if fence > 1 || marker.as_deref().is_some_and(|m| m.starts_with("W4_")) || !context.is_object()
    {
        return Ok(false);
    }
    let snapshot = json!({"format":"light-workflow-step-v1","taskId":task,"profile":profile,"definitionDigest":digest,"context":context});
    let update=sqlx::query("UPDATE task_info_t SET task_input=$3,result_code='W4_STEP_CAPTURED' WHERE host_id=$1 AND task_id=$2 AND status_code='A'")
        .bind(host).bind(task).bind(snapshot).execute(&mut **tx).await?;
    Ok(update.rows_affected() == 1)
}

/// Execution fences needed by cleanup; deliberately contains no executable definition.
pub(super) struct CompletionState<'a> {
    pub task: &'a ActiveTask,
    pub host_lease: Option<HostTaskLease>,
    pub completion_guard: Option<&'a CompletionGuard>,
}
impl<'a> From<&'a ClaimedTask> for CompletionState<'a> {
    fn from(claimed: &'a ClaimedTask) -> Self {
        Self {
            task: &claimed.task,
            host_lease: claimed.host_lease,
            completion_guard: claimed.completion_guard.as_ref(),
        }
    }
}

impl TaskExecutor {
    async fn w4_parent(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        claimed_task: Uuid,
        host: Uuid,
        process: Uuid,
    ) -> Result<crate::durable_timer::Parent, sqlx::Error> {
        let mut parent = crate::durable_timer::try_lock_parent(tx, host, process).await?;
        // Cancellation starts compensation; it must not veto the explicitly
        // authorized compensation tasks themselves. All other authority fences remain.
        if parent.blocked == Some("WORKFLOW_TIMER_CANCELLED") {
            let compensating:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM task_info_t t JOIN process_info_t p ON p.host_id=t.host_id AND p.process_id=t.process_id JOIN workflow_invocation_t i ON i.host_id=p.host_id AND i.process_id=p.process_id WHERE t.host_id=$1 AND t.task_id=$2 AND t.process_id=$3 AND t.is_compensation AND p.active AND p.status_code IN ('A','W') AND i.state='COMPENSATING')")
                .bind(host).bind(claimed_task).bind(process).fetch_one(&mut **tx).await?;
            if compensating {
                parent.blocked = None;
            }
        }
        crate::durable_timer::try_check_authority(tx, host, &mut parent).await?;
        Ok(parent)
    }
    /// Also used before parsing results or promoting artifacts. None means ready.
    pub(crate) async fn w4_runner_gate(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        attempt: &TerminalAttempt,
    ) -> Result<Option<RunnerReconciliation>, sqlx::Error> {
        match self
            .w4_guard(tx, attempt.host_id, attempt.process_id)
            .await?
        {
            ProfileDisposition::Legacy => return Ok(None),
            ProfileDisposition::Deferred if attempt.state == "SUCCEEDED" => {
                return Ok(Some(RunnerReconciliation::Deferred));
            }
            ProfileDisposition::Deferred => {}
            ProfileDisposition::Corrupt => {
                self.w4_reject(
                    tx,
                    attempt.host_id,
                    attempt.process_id,
                    attempt.task_id,
                    "/definition_snapshot/expression_profile",
                )
                .await?;
                return Ok(Some(RunnerReconciliation::Failed));
            }
            ProfileDisposition::V2 => {}
        }
        let parent = self
            .w4_parent(tx, attempt.task_id, attempt.host_id, attempt.process_id)
            .await?;
        let row=sqlx::query("SELECT status_code::text,result_code,active,deadline_ts,scheduling_request_id,accepted_attempt,task_input,clock_timestamp() AS now FROM task_info_t WHERE host_id=$1 AND task_id=$2 FOR UPDATE NOWAIT")
            .bind(attempt.host_id).bind(attempt.task_id).fetch_one(&mut **tx).await?;
        let now: chrono::DateTime<Utc> = row.try_get("now")?;
        let marker: Option<String> = row.try_get("result_code")?;
        if matches!(marker.as_deref(), Some("W4_STEP_DONE" | "W4_STEP_FAILED")) {
            return Ok(Some(RunnerReconciliation::Replay));
        }
        if parent.blocked.is_some()
            || parent.deadline.is_some_and(|d| d <= now)
            || parent.authority_deadline.is_some_and(|d| d <= now)
            || !row.try_get::<bool, _>("active")?
            || row
                .try_get::<Option<chrono::DateTime<Utc>>, _>("deadline_ts")?
                .is_some_and(|d| d <= now)
        {
            return Ok(Some(RunnerReconciliation::Deferred));
        }
        let fixed:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workflow_approval_t WHERE host_id=$1 AND task_id=$2 AND consuming_execution_id=$3 AND state='CONSUMED')")
            .bind(attempt.host_id).bind(attempt.task_id).bind(attempt.execution_id).fetch_one(&mut **tx).await?;
        if !fixed && marker.as_deref() == Some("W4_APPROVAL_PENDING") {
            return Ok(Some(RunnerReconciliation::Replay));
        }
        if !fixed
            && (row.try_get::<Option<Uuid>, _>("scheduling_request_id")?
                != Some(attempt.request_id)
                || row.try_get::<String, _>("status_code")? != "A"
                || row.try_get::<Option<i32>, _>("accepted_attempt")?.is_some())
        {
            return Ok(Some(RunnerReconciliation::Deferred));
        }
        if attempt.state != "SUCCEEDED" {
            return Ok(None);
        }
        let digest: String = sqlx::query_scalar(
            "SELECT definition_digest FROM process_info_t WHERE host_id=$1 AND process_id=$2",
        )
        .bind(attempt.host_id)
        .bind(attempt.process_id)
        .fetch_one(&mut **tx)
        .await?;
        let stored: Option<Value> = row.try_get("task_input")?;
        if stored
            .as_ref()
            .and_then(|v| snapshot_context(v, attempt.task_id, "cel-workflow-v2", &digest))
            .is_none()
        {
            self.w4_reject(
                tx,
                attempt.host_id,
                attempt.process_id,
                attempt.task_id,
                "/task_input/stepSnapshot",
            )
            .await?;
            return Ok(Some(RunnerReconciliation::Failed));
        }
        Ok(None)
    }
    pub(crate) async fn w4_guard(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        host: Uuid,
        process: Uuid,
    ) -> Result<ProfileDisposition, sqlx::Error> {
        crate::profile_support::read(tx, host, process, self.supported_profiles()).await
    }
    pub(crate) async fn w4_reject(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        host: Uuid,
        process: Uuid,
        task: Uuid,
        field: &str,
    ) -> Result<(), sqlx::Error> {
        reject_step(tx, host, process, task, field).await
    }
    pub(super) async fn w4_failure(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        task: &ActiveTask,
        output: &Value,
    ) -> Result<(), sqlx::Error> {
        record_failure(tx, task, output).await
    }
    pub(super) async fn w4_context(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        claimed: &mut ClaimedTask,
    ) -> Result<bool, sqlx::Error> {
        if claimed.expression_profile != "cel-workflow-v2" {
            return Ok(true);
        }
        let (stored,digest):(Option<Value>,String)=sqlx::query_as("SELECT t.task_input,p.definition_digest FROM task_info_t t JOIN process_info_t p ON p.host_id=t.host_id AND p.process_id=t.process_id WHERE t.host_id=$1 AND t.task_id=$2")
            .bind(claimed.task.host_id).bind(claimed.task.task_id).fetch_one(&mut **tx).await?;
        if let Some(context) = stored.as_ref().and_then(|s| {
            snapshot_context(
                s,
                claimed.task.task_id,
                &claimed.expression_profile,
                &digest,
            )
        }) {
            claimed.context_data = context.clone();
            return Ok(true);
        }
        self.w4_reject(
            tx,
            claimed.task.host_id,
            claimed.task.process_id,
            claimed.task.task_id,
            "/task_input/stepSnapshot",
        )
        .await?;
        Ok(false)
    }
    pub(super) async fn w4_authority(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        claimed: &ClaimedTask,
    ) -> Result<bool, sqlx::Error> {
        self.w6_state_authority(tx, &CompletionState::from(claimed), true)
            .await
    }
    pub(super) async fn w6_state_authority(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        claimed: &CompletionState<'_>,
        require_support: bool,
    ) -> Result<bool, sqlx::Error> {
        let parent = self
            .w4_parent(
                tx,
                claimed.task.task_id,
                claimed.task.host_id,
                claimed.task.process_id,
            )
            .await?;
        let row=sqlx::query("SELECT process_id,task_type,wf_instance_id,wf_task_id,execution_placement,status_code::text,result_code,active,lease_owner,lease_fencing_token,lease_expires_ts,accepted_attempt,scheduling_request_id,deadline_ts,clock_timestamp() AS now FROM task_info_t WHERE host_id=$1 AND task_id=$2 FOR UPDATE NOWAIT")
            .bind(claimed.task.host_id).bind(claimed.task.task_id).fetch_one(&mut **tx).await?;
        let now: chrono::DateTime<Utc> = row.try_get("now")?;
        let status: String = row.try_get("status_code")?;
        let marker: Option<String> = row.try_get("result_code")?;
        // Answer submission persists C plus the answer in result_code. Only
        // its current host claim may finish that ask; answer text itself never
        // grants authority and W4 terminal/control markers cannot be answers.
        let answered_ask = status == "C"
            && claimed.task.status_code == "C"
            && claimed.task.task_type == "ask"
            && row.try_get::<String, _>("task_type")? == "ask"
            && row.try_get::<Uuid, _>("process_id")? == claimed.task.process_id
            && row.try_get::<String, _>("wf_instance_id")? == claimed.task.wf_instance_id
            && row.try_get::<String, _>("wf_task_id")? == claimed.task.wf_task_id
            && row.try_get::<String, _>("execution_placement")? == "host"
            && claimed.host_lease.is_some()
            && claimed.completion_guard.is_none()
            && marker == claimed.task.result_code
            && marker.as_deref().is_none_or(|m| !m.starts_with("W4_"));
        if !parent_live(
            parent.blocked,
            parent.deadline,
            parent.authority_deadline,
            now,
        ) || !row.try_get::<bool, _>("active")?
            || row
                .try_get::<Option<chrono::DateTime<Utc>>, _>("deadline_ts")?
                .is_some_and(|d| d <= now)
            || !((completion_status_live(&status, marker.as_deref(), &claimed.task.status_code)
                && !(status == "C" && claimed.task.task_type == "ask"))
                || answered_ask)
        {
            return Ok(false);
        }
        if let Some(lease) = claimed.host_lease
            && !lease_live(
                row.try_get("lease_owner")?,
                row.try_get("lease_fencing_token")?,
                row.try_get("lease_expires_ts")?,
                lease,
                now,
            )
        {
            return Ok(false);
        }
        let valid=match &claimed.completion_guard {
            Some(CompletionGuard::Runner{request,attempt})=> row.try_get::<Option<Uuid>,_>("scheduling_request_id")?==Some(*request) && row.try_get::<Option<i32>,_>("accepted_attempt")?==Some(*attempt),
            Some(CompletionGuard::Timer{generation,fence})=> {
                let live:bool=sqlx::query_scalar("SELECT generation=$3 AND task_fence=$4 AND state='FIRED' AND (effective_deadline IS NULL OR effective_deadline>clock_timestamp()) FROM workflow_task_timer_t WHERE host_id=$1 AND task_id=$2")
                    .bind(claimed.task.host_id).bind(claimed.task.task_id).bind(generation).bind(fence).fetch_one(&mut **tx).await?;
                live && row.try_get::<i64,_>("lease_fencing_token")?==*fence
            }
            Some(CompletionGuard::Agent{job})=>sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workflow_agent_job_t WHERE host_id=$1 AND job_id=$3 AND workflow_task_id=$2 AND state IN ('SUCCEEDED','FAILED','CANCELLED','UNKNOWN'))")
                .bind(claimed.task.host_id).bind(claimed.task.task_id).bind(job).fetch_one(&mut **tx).await?,
            Some(CompletionGuard::Approval{approval,execution})=> marker.as_deref()==Some("W4_APPROVAL_PENDING") && sqlx::query_scalar::<_,bool>("SELECT EXISTS(SELECT 1 FROM workflow_approval_t WHERE host_id=$1 AND approval_id=$3 AND task_id=$2 AND state='CONSUMED' AND consuming_execution_id=$4)")
                .bind(claimed.task.host_id).bind(claimed.task.task_id).bind(approval).bind(execution).fetch_one(&mut **tx).await?,
            Some(CompletionGuard::Fork{join})=> marker.as_deref()==Some("W4_FORK_PENDING") && sqlx::query_scalar::<_,bool>("SELECT EXISTS(SELECT 1 FROM workflow_fork_join_t WHERE host_id=$1 AND join_id=$3 AND fork_task_id=$2 AND state='COMPLETED')")
                .bind(claimed.task.host_id).bind(claimed.task.task_id).bind(join).fetch_one(&mut **tx).await?,
            None=>claimed.host_lease.is_some(),
        };
        Ok(valid
            && (!require_support
                || self
                    .w4_guard(tx, claimed.task.host_id, claimed.task.process_id)
                    .await?
                    == ProfileDisposition::V2))
    }
    async fn w4_prepared(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        claimed: &ClaimedTask,
        output: &Value,
        next: Option<String>,
        terminal_allowed: bool,
    ) -> Result<(Value, Option<Value>), Failure> {
        let evaluated = self.w4_exports(claimed, output).await?;
        #[cfg(test)]
        if self.w4_expire_after_evaluation {
            sqlx::query("UPDATE task_info_t SET deadline_ts=clock_timestamp() WHERE host_id=$1 AND task_id=$2")
                .bind(claimed.task.host_id).bind(claimed.task.task_id).execute(&mut **tx).await
                .map_err(|_|Failure{error:workflow_expression::WorkerError::Unavailable,field:"/completion".into()})?;
        }
        let current: Value = sqlx::query_scalar(
            "SELECT context_data FROM process_info_t WHERE host_id=$1 AND process_id=$2",
        )
        .bind(claimed.task.host_id)
        .bind(claimed.task.process_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(|_| Failure {
            error: workflow_expression::WorkerError::Unavailable,
            field: "/export/as".into(),
        })?;
        let mut merged = current
            .as_object()
            .cloned()
            .ok_or_else(|| failure(Category::ResultType, "/context"))?;
        if let Some(keys) = self
            .find_raw_task_definition(&claimed.raw_definition, &claimed.task.wf_task_id)
            .and_then(|t| t.get("export"))
            .and_then(|e| e.get("as"))
            .and_then(YamlValue::as_mapping)
        {
            for key in keys.keys() {
                let key = key
                    .as_str()
                    .ok_or_else(|| failure(Category::Invalid, "/export/as"))?;
                merged.insert(key.into(), evaluated[key].clone());
            }
        }
        let context = Value::Object(merged);
        workflow_expression::validate_output(&context).map_err(|e| Failure {
            error: e.into(),
            field: "/export/as".into(),
        })?;
        if !terminal_allowed {
            return Ok((context, None));
        }
        let task = self
            .find_task_definition(&claimed.definition, &claimed.task.wf_task_id)
            .ok_or_else(|| failure(Category::Invalid, "/task"))?;
        let next = self.resolve_next_task_name(
            &claimed.definition,
            &claimed.raw_definition,
            &claimed.task.wf_task_id,
            task,
            next,
        );
        if let Some(name) = next {
            if self
                .find_task_definition(&claimed.definition, &name)
                .is_none()
            {
                return Err(failure(Category::Invalid, "/then"));
            }
            return Ok((context, None));
        }
        let public = if let Some(template) = claimed
            .definition
            .output
            .as_ref()
            .and_then(|o| o.as_.as_ref())
        {
            expression_runtime::batch(
                self.expression_engine.as_ref(),
                &context,
                &claimed.input_data,
                None,
                None,
                vec![workflow_expression::StepField {
                    path: "/output/as".into(),
                    position: workflow_expression::Position::WorkflowOutput,
                    template: template.clone(),
                    value_from: None,
                }],
                false,
            )
            .await?
            .remove(0)
        } else if output.is_object() {
            output.clone()
        } else {
            json!({"value":output})
        };
        workflow_expression::validate_output(&public).map_err(|e| Failure {
            error: e.into(),
            field: "/output/as".into(),
        })?;
        let schema:Option<Value>=sqlx::query_scalar("SELECT response_policy_snapshot->'publicOutputSchema' FROM workflow_invocation_t WHERE host_id=$1 AND process_id=$2")
            .bind(claimed.task.host_id).bind(claimed.task.process_id).fetch_optional(&mut **tx).await.map_err(|_|Failure{error:workflow_expression::WorkerError::Unavailable,field:"/output/as".into()})?.flatten();
        if schema
            .filter(|s| !s.is_null())
            .is_some_and(|s| !jsonschema::Validator::new(&s).is_ok_and(|v| v.is_valid(&public)))
        {
            return Err(failure(Category::ResultType, "/output/as"));
        }
        let cap:Option<i64>=sqlx::query_scalar("SELECT b.result_byte_limit FROM workflow_invocation_budget_t b JOIN workflow_invocation_t i ON i.host_id=b.host_id AND i.workflow_instance_id=b.workflow_instance_id WHERE i.host_id=$1 AND i.process_id=$2")
            .bind(claimed.task.host_id).bind(claimed.task.process_id).fetch_optional(&mut **tx).await.map_err(|_|Failure{error:workflow_expression::WorkerError::Unavailable,field:"/output/as".into()})?;
        if cap.is_some_and(|cap| {
            serde_json::to_vec(&public).map_or(true, |v| v.len() as u128 > cap.max(0) as u128)
        }) {
            return Err(failure(Category::Limit, "/output/as"));
        }
        Ok((context, Some(public)))
    }
    pub(super) async fn w4_transition(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        claimed: &ClaimedTask,
        output: Value,
        next: Option<String>,
    ) -> Result<(), sqlx::Error> {
        if !crate::profile_support::success(
            tx,
            claimed.task.host_id,
            claimed.task.process_id,
            claimed.task.task_id,
            self.supported_profiles(),
        )
        .await?
        {
            return Ok(());
        }
        require_authority(self.w4_authority(tx, claimed).await?)?;
        let (context, public) = match self
            .w4_prepared(tx, claimed, &output, next.clone(), true)
            .await
        {
            Ok(prepared) => prepared,
            Err(error) => {
                if !matches!(error.error, workflow_expression::WorkerError::Expression(_)) {
                    return Err(sqlx::Error::Protocol(
                        "WORKFLOW_EXPRESSION_UNAVAILABLE".into(),
                    ));
                }
                require_authority(self.w4_authority(tx, claimed).await?)?;
                return self
                    .w4_failed_task(tx, claimed, &error.result(claimed).task_output)
                    .await;
            }
        };
        require_authority(self.w4_authority(tx, claimed).await?)?;
        let changed=sqlx::query("UPDATE task_info_t SET status_code='C',task_output=$3,completed_ts=clock_timestamp(),locked='N',lease_owner=NULL,lease_expires_ts=NULL,result_code='W4_STEP_DONE' WHERE host_id=$1 AND task_id=$2 AND status_code=$4 AND result_code IS DISTINCT FROM 'W4_STEP_DONE'")
            .bind(claimed.task.host_id).bind(claimed.task.task_id).bind(&output).bind(&claimed.task.status_code).execute(&mut **tx).await?;
        if changed.rows_affected() != 1 {
            return Err(sqlx::Error::Protocol("WORKFLOW_STALE_COMPLETION".into()));
        }
        let advanced=sqlx::query("UPDATE process_info_t SET status_code='A',custom_status_code=NULL WHERE host_id=$1 AND process_id=$2 AND status_code IN ('A','W') AND active")
            .bind(claimed.task.host_id).bind(claimed.task.process_id).execute(&mut **tx).await?;
        if advanced.rows_affected() != 1 {
            return Err(sqlx::Error::Protocol("WORKFLOW_STALE_COMPLETION".into()));
        }
        self.advance_transition(
            tx,
            &claimed.task,
            (&claimed.definition, &claimed.raw_definition),
            context,
            next,
        )
        .await?;
        self.sync_invocation_state(tx, claimed, &output, public.as_ref())
            .await
    }
    async fn w4_failed_task(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        claimed: &ClaimedTask,
        output: &Value,
    ) -> Result<(), sqlx::Error> {
        require_authority(
            self.w6_state_authority(tx, &CompletionState::from(claimed), false)
                .await?,
        )?;
        let compensation = self.is_compensation_task(tx, claimed).await?;
        let branch = self.is_fork_branch(tx, claimed).await?;
        let route = failure_route(compensation, branch);
        if route == FailureRoute::Process {
            return self.w4_failure(tx, &claimed.task, output).await;
        }
        // Expression failures are terminal for this task, but a competing fork
        // may still succeed. Never evaluate exports or mutate context on failure.
        let updated=sqlx::query("UPDATE task_info_t SET status_code='F',result_code='W4_STEP_FAILED',task_output=$3,locked='N',lease_owner=NULL,lease_expires_ts=NULL,completed_ts=clock_timestamp() WHERE host_id=$1 AND task_id=$2 AND status_code=$4 AND result_code IS DISTINCT FROM 'W4_STEP_DONE' AND result_code IS DISTINCT FROM 'W4_STEP_FAILED'")
            .bind(claimed.task.host_id).bind(claimed.task.task_id).bind(output).bind(&claimed.task.status_code).execute(&mut **tx).await?;
        require_authority(updated.rows_affected() == 1)?;
        if route == FailureRoute::Compensation {
            return self
                .reconcile_compensation_task(tx, claimed, false, output)
                .await;
        }
        Box::pin(self.reconcile_fork_branch(tx, claimed, false, output.clone())).await?;
        self.sync_invocation_state(tx, claimed, output, None).await
    }
    pub(super) async fn finish_task_v2(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        claimed: &ClaimedTask,
        result: TaskExecutionResult,
    ) -> Result<(), sqlx::Error> {
        if result.status_code == "F" {
            return self.w4_failed_task(tx, claimed, &result.task_output).await;
        }
        if !crate::profile_support::success(
            tx,
            claimed.task.host_id,
            claimed.task.process_id,
            claimed.task.task_id,
            self.supported_profiles(),
        )
        .await?
        {
            return Ok(());
        }
        require_authority(self.w4_authority(tx, claimed).await?)?;
        if result.status_code == "W" {
            if let Some(TaskDefinition::Wait(wait)) =
                self.find_task_definition(&claimed.definition, &claimed.task.wf_task_id)
            {
                let lease = claimed
                    .host_lease
                    .ok_or_else(|| sqlx::Error::Protocol("WORKFLOW_TIMER_LEASE_REQUIRED".into()))?;
                return crate::durable_timer::arm(
                    tx,
                    claimed.task.host_id,
                    claimed.task.process_id,
                    claimed.task.task_id,
                    lease.owner,
                    lease.fencing_token,
                    crate::durable_timer::duration_seconds(&wait.wait)
                        .map_err(|_| sqlx::Error::Protocol("WORKFLOW_TIMER_INVALID".into()))?,
                )
                .await;
            }
            if let Some(TaskDefinition::Ask(_)) =
                self.find_task_definition(&claimed.definition, &claimed.task.wf_task_id)
            {
                let ask: AskDefinition = serde_json::from_value(result.task_output["ask"].clone())
                    .map_err(|_| sqlx::Error::Protocol("EXPRESSION_RESULT_TYPE".into()))?;
                self.write_ask_assignments(tx, claimed, &ask, true).await?;
            } else if !matches!(self.find_task_definition(&claimed.definition,&claimed.task.wf_task_id),
                Some(TaskDefinition::Call(CallTaskDefinition::Agent(agent))) if agent.with.mode == workflow_core::models::task::AgentCallMode::Service)
            {
                return Err(sqlx::Error::Protocol(
                    "WORKFLOW_EXPRESSION_FIELDS_PENDING".into(),
                ));
            }
            require_authority(self.w4_authority(tx, claimed).await?)?;
            let changed = sqlx::query("UPDATE task_info_t SET status_code='W',task_output=$3,locked='N',lease_owner=NULL,lease_expires_ts=NULL WHERE host_id=$1 AND task_id=$2 AND status_code=$4")
                .bind(claimed.task.host_id).bind(claimed.task.task_id).bind(&result.task_output).bind(&claimed.task.status_code)
                .execute(&mut **tx).await?;
            require_authority(changed.rows_affected() == 1)?;
            self.sync_invocation_state(tx, claimed, &result.task_output, None)
                .await?;
            return Ok(());
        }
        let branch = self.is_fork_branch(tx, claimed).await?;
        let fork = matches!(
            self.find_task_definition(&claimed.definition, &claimed.task.wf_task_id),
            Some(TaskDefinition::Fork(_))
        );
        if !branch && !fork && !self.is_compensation_task(tx, claimed).await? {
            return self
                .w4_transition(tx, claimed, result.task_output, result.next_task)
                .await;
        }
        let compensation = self.is_compensation_task(tx, claimed).await?;
        let evaluated = if branch || compensation {
            match self
                .w4_prepared(tx, claimed, &result.task_output, None, false)
                .await
            {
                Ok((context, _)) => Some(context),
                Err(e) => {
                    if !matches!(e.error, workflow_expression::WorkerError::Expression(_)) {
                        return Err(sqlx::Error::Protocol(
                            "WORKFLOW_EXPRESSION_UNAVAILABLE".into(),
                        ));
                    }
                    require_authority(self.w4_authority(tx, claimed).await?)?;
                    return self
                        .w4_failed_task(tx, claimed, &e.result(claimed).task_output)
                        .await;
                }
            }
        } else {
            None
        };
        require_authority(self.w4_authority(tx, claimed).await?)?;
        let changed=sqlx::query("UPDATE task_info_t SET status_code='C',task_output=$3,result_code=$4,locked='N',lease_owner=NULL,lease_expires_ts=NULL,completed_ts=clock_timestamp() WHERE host_id=$1 AND task_id=$2 AND status_code=$5 AND result_code IS DISTINCT FROM 'W4_STEP_DONE'")
            .bind(claimed.task.host_id).bind(claimed.task.task_id).bind(&result.task_output).bind(if fork {"W4_FORK_PENDING"} else {"W4_STEP_DONE"}).bind(&claimed.task.status_code).execute(&mut **tx).await?;
        if changed.rows_affected() != 1 {
            return Err(sqlx::Error::Protocol("WORKFLOW_STALE_COMPLETION".into()));
        }
        if let Some(context) = evaluated {
            let merged=sqlx::query(
                "UPDATE process_info_t SET context_data=$3 WHERE host_id=$1 AND process_id=$2 AND active AND status_code IN ('A','W')",
            )
            .bind(claimed.task.host_id)
            .bind(claimed.task.process_id)
            .bind(context)
            .execute(&mut **tx)
            .await?;
            if merged.rows_affected() != 1 {
                return Err(sqlx::Error::Protocol("WORKFLOW_STALE_COMPLETION".into()));
            }
        }
        if compensation {
            return self
                .reconcile_compensation_task(tx, claimed, true, &result.task_output)
                .await;
        }
        if branch {
            self.reconcile_fork_branch(tx, claimed, true, result.task_output)
                .await?;
        } else {
            self.start_fork(tx, claimed).await?;
        }
        Ok(())
    }
}

pub(crate) async fn reject_step(
    tx: &mut Transaction<'_, Postgres>,
    host: Uuid,
    process: Uuid,
    task: Uuid,
    field: &str,
) -> Result<(), sqlx::Error> {
    let task=sqlx::query_as::<_,ActiveTask>("SELECT host_id,task_id,task_type,process_id,wf_instance_id,wf_task_id,status_code,result_code FROM task_info_t WHERE host_id=$1 AND task_id=$2")
            .bind(host).bind(task).fetch_one(&mut **tx).await?;
    let definition: Uuid = sqlx::query_scalar(
        "SELECT wf_def_id FROM process_info_t WHERE host_id=$1 AND process_id=$2",
    )
    .bind(host)
    .bind(process)
    .fetch_one(&mut **tx)
    .await?;
    let output = json!({"code":"EVALUATOR_PROFILE_UNSUPPORTED","retryable":false,"details":{"reason":"snapshot/profile mismatch","category":"EVALUATOR_PROFILE_UNSUPPORTED","phase":"runtime","definitionId":definition,"taskId":task.task_id,"field":field,"spanIndex":null,"offset":null}});
    record_failure_mode(tx, &task, &output, true).await
}
pub(crate) async fn record_failure(
    tx: &mut Transaction<'_, Postgres>,
    task: &ActiveTask,
    output: &Value,
) -> Result<(), sqlx::Error> {
    record_failure_mode(tx, task, output, false).await
}

async fn record_failure_mode(
    tx: &mut Transaction<'_, Postgres>,
    task: &ActiveTask,
    output: &Value,
    allow_pending_completion: bool,
) -> Result<(), sqlx::Error> {
    let parent = crate::durable_timer::try_lock_parent(tx, task.host_id, task.process_id).await?;
    let now: chrono::DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut **tx)
        .await?;
    // A legacy direct terminal seam can set process C before invocation sync.
    // Reject corruption there, but never reopen a committed terminal invocation.
    let pending_completion: bool = if allow_pending_completion && parent.blocked.is_some() {
        sqlx::query_scalar("SELECT p.active AND p.status_code='C' AND NOT EXISTS(SELECT 1 FROM workflow_invocation_t i WHERE i.host_id=p.host_id AND i.process_id=p.process_id AND (i.cancel_requested_ts IS NOT NULL OR i.state NOT IN ('ACCEPTED','RUNNING','WAITING'))) FROM process_info_t p WHERE p.host_id=$1 AND p.process_id=$2")
            .bind(task.host_id).bind(task.process_id).fetch_one(&mut **tx).await?
    } else {
        false
    };
    if (parent.blocked.is_some() && !pending_completion)
        || parent.deadline.is_some_and(|d| d <= now)
    {
        return Err(sqlx::Error::Protocol("WORKFLOW_STALE_COMPLETION".into()));
    }
    let changed=sqlx::query("UPDATE task_info_t SET status_code='F',task_output=$3,result_code='W4_STEP_FAILED',locked='N',lease_owner=NULL,lease_expires_ts=NULL,completed_ts=clock_timestamp() WHERE host_id=$1 AND task_id=$2 AND status_code IN ('A','W','C') AND result_code IS DISTINCT FROM 'W4_STEP_DONE' AND result_code IS DISTINCT FROM 'W4_STEP_FAILED'")
            .bind(task.host_id).bind(task.task_id).bind(output).execute(&mut **tx).await?;
    if changed.rows_affected() != 1 {
        return Err(sqlx::Error::Protocol("WORKFLOW_STALE_COMPLETION".into()));
    }
    let changed=sqlx::query("UPDATE process_info_t SET status_code='F',error_info=$3,completed_ts=clock_timestamp() WHERE host_id=$1 AND process_id=$2 AND (status_code IN ('A','W') OR ($4 AND status_code='C'))")
            .bind(task.host_id).bind(task.process_id).bind(output.to_string()).bind(allow_pending_completion).execute(&mut **tx).await?;
    if changed.rows_affected() != 1 {
        return Err(sqlx::Error::Protocol("WORKFLOW_STALE_COMPLETION".into()));
    }
    let invocation=sqlx::query("UPDATE workflow_invocation_t SET state='FAILED',normalized_error=$3,terminal_ts=clock_timestamp(),user_authorization=NULL,user_authorization_exp=NULL,updated_ts=clock_timestamp(),state_version=state_version+1 WHERE host_id=$1 AND process_id=$2 AND state NOT IN ('COMPLETED','FAILED','CANCELLED')")
            .bind(task.host_id).bind(task.process_id).bind(output).execute(&mut **tx).await?;
    if invocation.rows_affected() == 0 {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM workflow_invocation_t WHERE host_id=$1 AND process_id=$2)",
        )
        .bind(task.host_id)
        .bind(task.process_id)
        .fetch_one(&mut **tx)
        .await?;
        if exists {
            return Err(sqlx::Error::Protocol("WORKFLOW_STALE_COMPLETION".into()));
        }
    }
    Ok(())
}
