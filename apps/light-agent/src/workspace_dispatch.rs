use super::*;
use workspace_execution_protocol::WorkspaceExecutionSpec;

impl AgentRepository {
    /// Authenticated session admission owns principal identity; the browser only
    /// supplies WorkspaceRequest. Enqueue through the normal execution outbox.
    pub async fn schedule_workspace_turn(
        &self,
        host_id: Uuid,
        session_id: AgentSessionId,
        turn_id: AgentTurnId,
        spec: &WorkspaceExecutionSpec,
        runtime: &CodingAdapterRuntime,
    ) -> Result<Uuid> {
        spec.validate()?;
        runtime.contract.validate()?;
        if runtime.enterprise_gateway.is_some() || spec.binding.host_id != host_id.to_string() {
            bail!("workspace execution requires its bound personal runner");
        }
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT t.policy_snapshot_id,t.policy_digest,t.deadline_ts,s.principal_id
            FROM agent_turn_t t JOIN agent_session_t s ON s.host_id=t.host_id AND s.session_id=t.session_id
            JOIN agent_policy_snapshot_t p ON p.host_id=t.host_id AND p.policy_snapshot_id=t.policy_snapshot_id
            WHERE t.host_id=$1 AND t.session_id=$2 AND t.turn_id=$3 AND t.state='RECEIVED'
            AND p.revoked_ts IS NULL AND s.state='ACTIVE' FOR UPDATE OF t,s")
            .bind(host_id).bind(session_id.0).bind(turn_id.0).fetch_one(&mut *tx).await?;
        if row.try_get::<String, _>("principal_id")? != spec.subject {
            bail!("workspace principal differs from session");
        }
        let snapshot: Uuid = row.try_get("policy_snapshot_id")?;
        let policy: String = row.try_get("policy_digest")?;
        let deadline: DateTime<Utc> = row.try_get("deadline_ts")?;
        let timeout = deadline
            .signed_duration_since(Utc::now())
            .num_milliseconds();
        if timeout <= 0 {
            bail!("workspace turn deadline expired");
        }
        let mut features: Vec<_> = runtime.contract.required_features.iter().cloned().collect();
        features.extend([
            "task-workspace-v1".into(),
            "personal-subscription-auth-v1".into(),
            "local-single-user-native-v1".into(),
        ]);
        let requirements = ExecutionRequirements {
            action_kind: runtime.contract.action_kind.clone(),
            minimum_boundary: IsolationBoundary::Process,
            maximum_host_exposure: HostExposure::ExplicitMounts,
            network_enabled: true,
            credential_classes: vec![],
            persistent_workspace: true,
            required_features: features,
            policy_digest: policy.clone(),
            compatibility_digest: runtime.contract.compatibility_digest.clone(),
        };
        let command = AgentWorkerExecutionSpec {
            schema_version: 1,
            template_digest: runtime.contract.template_digest.clone(),
            expected_capability_digest: runtime.contract.capability_digest.clone(),
            session_id,
            turn_id,
            action_attempt_id: AgentActionAttemptId(turn_id.0),
            policy_digest: policy.clone(),
            input: json!({"workspaceSpec":spec,"adapterContract":runtime.contract,"adapterQualification":runtime.qualification,"claudePolicy":runtime.claude_policy}),
            wall_clock_timeout_ms: (timeout as u64).min(300_000),
            maximum_event_bytes: 1024 * 1024,
            maximum_stderr_bytes: 1024 * 1024,
            broker: None,
            enterprise_gateway: None,
        };
        let authority = self
            .authority
            .as_ref()
            .context("workspace execution requires Agent authority")?;
        let request = SchedulingRequestSubmission {
            request_id: turn_id.0,
            idempotency_key: format!("workspace-turn:{}", turn_id.0),
            origin_kind: "agent".into(),
            // Workspace runner scope uses the admitted agent service identity.
            origin_instance_id: spec.agent_id.clone(),
            subject_kind: "agent-turn".into(),
            subject_id: turn_id.0,
            process_id: None,
            task_id: None,
            agent_session_id: Some(session_id.0),
            agent_turn_id: Some(turn_id.0),
            agent_action_id: None,
            policy_snapshot_id: snapshot,
            policy_digest: policy.clone(),
            normalized_requirements: serde_json::to_value(requirements)?,
            execution_spec: serde_json::to_value(command)?,
            resolved_policy: json!({"policyDigest":policy,"workspaceBinding":spec.binding,
                "codingAdapterContractDigest":runtime.contract.digest()?,"adapterId":runtime.contract.adapter_id,
                "adapterVersion":runtime.contract.adapter_version,"capabilityDigest":runtime.contract.capability_digest,
                "templateDigest":runtime.contract.template_digest,"runtimeCompatibilityDigest":runtime.contract.compatibility_digest}),
            definition_digest: authority.definition_digest.clone(),
            fairness_key: format!("agent:{}", spec.subject),
            priority: 0,
            workflow_reference_digest: None,
            origin_reference_digest: format!("sha256:{}", canonical_sha256(spec)?),
            approval_id: None,
            approval_evidence_digest: None,
            pinned_runner_id: Some(spec.binding.runner_id.clone()),
            pinned_backend_id: None,
            edge_binding_id: None,
            edge_binding_compatibility_digest: None,
            edge_binding_revocation_epoch: None,
            inputs: vec![],
        };
        Self::enqueue_execution_request(&mut tx, host_id, &request).await?;
        sqlx::query("UPDATE agent_turn_t SET scheduling_request_id=$3,state='WAITING_RECONCILIATION',updated_ts=now() WHERE host_id=$1 AND turn_id=$2")
            .bind(host_id).bind(turn_id.0).bind(request.request_id).execute(&mut *tx).await?;
        append_event(&mut tx,host_id,session_id.0,Some(turn_id.0),None,"agent","WORKSPACE_TURN_SCHEDULED",
            json!({"requestId":request.request_id,"workspaceId":spec.request.workspace_id,"inputDigest":spec.request.input_digest()?}),&policy).await?;
        tx.commit().await?;
        Ok(request.request_id)
    }
}
