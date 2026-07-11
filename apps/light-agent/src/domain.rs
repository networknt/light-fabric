use agent_core::{AgentSessionId, AgentTurnId, PolicySnapshot, sha256_digest};
use agent_materializer::MaterializationManifest;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Row, Transaction, postgres::PgListener};
use uuid::Uuid;

use coding_agent_runtime::CodingTurnSpec;
use execution_runner_protocol::{
    CommandExecutionSpec, ExecutionRequirements, HostExposure, IsolationBoundary,
};

#[derive(Clone)]
pub struct AgentRepository {
    pool: PgPool,
}

pub struct SessionSpec {
    pub host_id: Uuid,
    pub session_id: AgentSessionId,
    pub principal_id: String,
    pub user_id: Option<Uuid>,
    pub agent_def_id: Uuid,
    pub bank_id: Option<Uuid>,
    pub policy: PolicySnapshot,
    pub idle_expires_at: DateTime<Utc>,
    pub maximum_expires_at: DateTime<Utc>,
    pub resume_handle_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedTurn {
    pub turn_id: AgentTurnId,
    pub turn_sequence: i64,
    pub duplicate: bool,
    pub policy_digest: String,
    pub data_boundary_digest: String,
}

impl AgentRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn spawn_result_reconciler(&self) -> tokio::task::JoinHandle<()> {
        let repository = self.clone();
        tokio::spawn(async move {
            loop {
                if let Err(error) = repository.listen_and_reconcile().await {
                    tracing::warn!("agent execution-result reconciler disconnected: {error}");
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        })
    }

    async fn listen_and_reconcile(&self) -> Result<()> {
        let mut listener = PgListener::connect_with(&self.pool).await?;
        listener.listen("execution_result_ready_v1").await?;
        self.reconcile_execution_results().await?;
        loop {
            tokio::select! {
                notification = listener.recv() => { notification?; self.reconcile_execution_results().await?; }
                _ = tokio::time::sleep(std::time::Duration::from_secs(15)) => {
                    self.reconcile_execution_results().await?;
                    self.reconcile_expiry_and_cleanup().await?;
                    self.reconcile_projections().await?;
                },
            }
        }
    }

    pub async fn reconcile_execution_results(&self) -> Result<u64> {
        let rows = sqlx::query("SELECT a.host_id,a.action_attempt_id,a.turn_id,a.execution_attempt_id FROM agent_action_attempt_t a JOIN execution_attempt_t e ON e.host_id=a.host_id AND e.execution_id=a.execution_attempt_id WHERE a.origin_accepted_ts IS NULL AND e.terminal_ts IS NOT NULL ORDER BY e.terminal_ts,e.execution_id LIMIT 100")
            .fetch_all(&self.pool).await?;
        let mut accepted = 0;
        for row in rows {
            accepted += self
                .accept_execution_result(
                    row.try_get("host_id")?,
                    row.try_get("action_attempt_id")?,
                    row.try_get("turn_id")?,
                    row.try_get("execution_attempt_id")?,
                )
                .await? as u64;
        }
        let turns = sqlx::query("SELECT t.host_id,t.turn_id,e.execution_id FROM agent_turn_t t JOIN execution_attempt_t e ON e.host_id=t.host_id AND e.agent_turn_id=t.turn_id WHERE t.execution_attempt_id IS NULL AND e.terminal_ts IS NOT NULL AND e.accepted_by_origin_ts IS NULL ORDER BY e.terminal_ts,e.execution_id LIMIT 100")
            .fetch_all(&self.pool).await?;
        for row in turns {
            accepted += self
                .accept_coding_turn_result(
                    row.try_get("host_id")?,
                    row.try_get("turn_id")?,
                    row.try_get("execution_id")?,
                )
                .await? as u64;
        }
        Ok(accepted)
    }

    async fn accept_coding_turn_result(
        &self,
        host_id: Uuid,
        turn_id: Uuid,
        execution_id: Uuid,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let row=sqlx::query("SELECT t.session_id,t.policy_digest,e.state,e.normalized_result,e.normalized_error FROM agent_turn_t t JOIN execution_attempt_t e ON e.host_id=t.host_id AND e.agent_turn_id=t.turn_id WHERE t.host_id=$1 AND t.turn_id=$2 AND e.execution_id=$3 AND e.terminal_ts IS NOT NULL FOR UPDATE OF t,e")
            .bind(host_id).bind(turn_id).bind(execution_id).fetch_optional(&mut *tx).await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(false);
        };
        let session: Uuid = row.try_get("session_id")?;
        let policy: String = row.try_get("policy_digest")?;
        let state: String = row.try_get("state")?;
        let result = json!({"executionId":execution_id,"state":state,"result":row.try_get::<Option<Value>,_>("normalized_result")?,"error":row.try_get::<Option<Value>,_>("normalized_error")?});
        append_event(
            &mut tx,
            host_id,
            session,
            Some(turn_id),
            None,
            "runner",
            "CODING_TURN_RESULT",
            result.clone(),
            &policy,
        )
        .await?;
        sqlx::query("UPDATE agent_turn_t SET execution_attempt_id=$3,state=CASE WHEN $4='SUCCEEDED' THEN 'COMPLETED' WHEN $4='CANCELLED' THEN 'CANCELLED' WHEN $4='UNKNOWN' THEN 'UNKNOWN' ELSE 'FAILED' END,terminal_result=CASE WHEN $4='SUCCEEDED' THEN $5 ELSE terminal_result END,terminal_error=CASE WHEN $4<>'SUCCEEDED' THEN $5 ELSE terminal_error END,terminal_ts=now(),updated_ts=now() WHERE host_id=$1 AND turn_id=$2 AND execution_attempt_id IS NULL")
            .bind(host_id).bind(turn_id).bind(execution_id).bind(&state).bind(&result).execute(&mut *tx).await?;
        sqlx::query("UPDATE execution_attempt_t SET accepted_by_origin_ts=COALESCE(accepted_by_origin_ts,now()),updated_ts=now() WHERE host_id=$1 AND execution_id=$2").bind(host_id).bind(execution_id).execute(&mut *tx).await?;
        sqlx::query("UPDATE agent_session_t SET active_turn_id=NULL,session_version=session_version+1,updated_ts=now() WHERE host_id=$1 AND session_id=$2 AND active_turn_id=$3").bind(host_id).bind(session).bind(turn_id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn schedule_coding_turn(
        &self,
        host_id: Uuid,
        session_id: AgentSessionId,
        turn_id: AgentTurnId,
        instance_id: &str,
        manifest: &MaterializationManifest,
        spec: &CodingTurnSpec,
        compatibility_digest: &str,
    ) -> Result<Uuid> {
        spec.validate()?;
        let manifest_digest = manifest.digest()?;
        if manifest.product_profile != agent_materializer::ProductProfile::Coding
            || spec.materialization_manifest_digest != manifest_digest
        {
            bail!("coding materialization profile or digest mismatch")
        }
        let mut tx = self.pool.begin().await?;
        let row=sqlx::query("SELECT t.policy_snapshot_id,t.policy_digest,t.data_boundary_digest,s.principal_id FROM agent_turn_t t JOIN agent_session_t s ON s.host_id=t.host_id AND s.session_id=t.session_id JOIN agent_policy_snapshot_t p ON p.host_id=t.host_id AND p.policy_snapshot_id=t.policy_snapshot_id AND p.revoked_ts IS NULL WHERE t.host_id=$1 AND t.turn_id=$2 AND t.session_id=$3 AND t.state IN ('RECEIVED','RUNNING_MODEL') FOR UPDATE OF t,s")
            .bind(host_id).bind(turn_id.0).bind(session_id.0).fetch_one(&mut *tx).await?;
        let snapshot: Uuid = row.try_get("policy_snapshot_id")?;
        let policy: String = row.try_get("policy_digest")?;
        let principal: String = row.try_get("principal_id")?;
        let request_id = Uuid::now_v7();
        let requirements = ExecutionRequirements {
            action_kind: "agent.runtime".into(),
            minimum_boundary: IsolationBoundary::MicroVm,
            maximum_host_exposure: HostExposure::None,
            network_enabled: false,
            credential_classes: vec![],
            persistent_workspace: false,
            required_features: vec!["deny-all-egress".into(), "artifacts".into()],
            policy_digest: policy.clone(),
            compatibility_digest: compatibility_digest.into(),
        };
        let command = CommandExecutionSpec {
            schema_version: 1,
            template_id: "coding-agent-worker-v1".into(),
            template_version: 1,
            template_digest: "503c1f8879addd7dec140d9f2e703e6b7230979188bbd6f7c9e4f941e276a717"
                .into(),
            executable: "/usr/local/bin/light-agent-worker".into(),
            arguments: vec![],
            working_directory: spec.workspace_root.clone(),
            environment: Default::default(),
            wall_clock_timeout_ms: 120_000,
            stdout_limit_bytes: 1024 * 1024,
            stderr_limit_bytes: 1024 * 1024,
            network_enabled: false,
            credentials_enabled: false,
            persistent_workspace: false,
        };
        let execution_spec = serde_json::to_value(&command)?;
        sqlx::query("INSERT INTO runner_scheduling_request_t(host_id,request_id,idempotency_key,origin_kind,origin_service_id,origin_instance_id,subject_kind,subject_id,agent_session_id,agent_turn_id,policy_snapshot_id,policy_digest,normalized_requirements,execution_spec,fairness_key,state) VALUES($1,$2,$3,'agent','light-agent',$4,'agent-turn',$5,$6,$5,$7,$8,$9,$10,$11,'PENDING_CAPACITY')")
            .bind(host_id).bind(request_id).bind(format!("coding-turn:{}",turn_id.0)).bind(instance_id).bind(turn_id.0).bind(session_id.0).bind(snapshot).bind(&policy).bind(serde_json::to_value(requirements)?).bind(execution_spec).bind(format!("agent:{principal}")).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO agent_turn_materialization_t(host_id,turn_id,materializer_id,materializer_version,product_profile,manifest,manifest_digest) VALUES($1,$2,$3,$4,'coding',$5,$6)")
            .bind(host_id).bind(turn_id.0).bind(&manifest.materializer_id).bind(manifest.materializer_version as i32).bind(serde_json::to_value(manifest)?).bind(&manifest_digest).execute(&mut *tx).await?;
        for package in &manifest.packages {
            let inserted=sqlx::query("INSERT INTO execution_input_t(host_id,input_id,request_id,kind,artifact_uri,content_digest,size_bytes,media_type,signer_binding,provenance_binding,scanner_binding,revocation_binding,staging_root,mount_target,read_only,executable,trust_bundle_id,package_manifest_digest,mount_options) SELECT $1,$2,$3,'skill-package',p.object_reference,p.content_digest,p.size_bytes,p.media_type,jsonb_build_object('signer',p.signer_reference,'signature',p.signature_reference),jsonb_build_object('reference',p.provenance_reference),jsonb_build_object('scanner',p.scanner_reference,'digest',p.scan_digest),jsonb_build_object('state',p.state,'revokedTs',p.revoked_ts),$4,$5,TRUE,FALSE,p.signer_reference,$6,'[\"ro\",\"nodev\",\"nosuid\",\"noexec\"]'::jsonb FROM skill_package_t p WHERE p.host_id=$1 AND p.package_id=$7 AND p.state='PUBLISHED' AND p.revoked_ts IS NULL AND p.content_digest=$6")
                .bind(host_id).bind(Uuid::now_v7()).bind(request_id).bind(format!("{}/inputs",spec.workspace_root)).bind(&package.mount_target).bind(&package.content_digest).bind(package.package_id).execute(&mut *tx).await?;
            if inserted.rows_affected() != 1 {
                bail!(
                    "skill package {} became unavailable during admission",
                    package.package_id
                );
            }
        }
        sqlx::query("UPDATE agent_turn_t SET scheduling_request_id=$3,materialization_manifest_digest=$4,coding_base_revision=$5,state='WAITING_RECONCILIATION',updated_ts=now() WHERE host_id=$1 AND turn_id=$2")
            .bind(host_id).bind(turn_id.0).bind(request_id).bind(&manifest_digest).bind(&spec.base_revision).execute(&mut *tx).await?;
        append_event(&mut tx,host_id,session_id.0,Some(turn_id.0),None,"agent","CODING_TURN_SCHEDULED",json!({"requestId":request_id,"manifestDigest":manifest_digest,"baseRevision":spec.base_revision}),&policy).await?;
        tx.commit().await?;
        Ok(request_id)
    }

    pub async fn reconcile_expiry_and_cleanup(&self) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE agent_approval_t SET state='EXPIRED',decision_ts=now(),decision_reason='approval deadline expired' WHERE state='REQUESTED' AND expires_ts<=now()")
            .execute(&mut *tx).await?;
        let stale = sqlx::query("UPDATE agent_turn_t SET state='UNKNOWN',terminal_error=jsonb_build_object('message','turn deadline expired during reconciliation'),terminal_ts=now(),updated_ts=now() WHERE state IN ('RECEIVED','RUNNING_MODEL','WAITING_ACTION','RUNNING_ACTION','WAITING_RECONCILIATION') AND deadline_ts<=now() RETURNING host_id,session_id,turn_id")
            .fetch_all(&mut *tx).await?;
        for row in stale {
            sqlx::query("UPDATE agent_session_t SET active_turn_id=NULL,session_version=session_version+1,updated_ts=now() WHERE host_id=$1 AND session_id=$2 AND active_turn_id=$3").bind(row.try_get::<Uuid,_>("host_id")?).bind(row.try_get::<Uuid,_>("session_id")?).bind(row.try_get::<Uuid,_>("turn_id")?).execute(&mut *tx).await?;
        }
        let expired = sqlx::query("UPDATE agent_session_t SET state='EXPIRED',cleanup_state=CASE WHEN execution_session_id IS NULL THEN 'NOT_REQUIRED' ELSE 'CLEANUP_REQUESTED' END,updated_ts=now() WHERE state='ACTIVE' AND LEAST(idle_expires_ts,maximum_expires_ts)<=now() RETURNING host_id,session_id,execution_session_id")
            .fetch_all(&mut *tx).await?;
        for row in expired {
            let host_id: Uuid = row.try_get("host_id")?;
            let session_id: Uuid = row.try_get("session_id")?;
            if let Some(execution_session_id) =
                row.try_get::<Option<Uuid>, _>("execution_session_id")?
            {
                let cleanup_id = Uuid::now_v7();
                sqlx::query("INSERT INTO execution_session_cleanup_request_t(host_id,cleanup_request_id,execution_session_id,origin_kind,origin_service_id,origin_instance_id,origin_session_id,subject_kind,subject_id,idempotency_key,reason,requested_by,cleanup_deadline_ts,state) VALUES($1,$2,$3,'agent','light-agent','session-reconciler',$4,'agent-turn',$4,$5,'session-expired','light-agent',now()+interval '5 minutes','PENDING') ON CONFLICT(host_id,origin_service_id,origin_instance_id,idempotency_key) DO NOTHING")
                    .bind(host_id).bind(cleanup_id).bind(execution_session_id).bind(session_id).bind(format!("session-expired:{session_id}")).execute(&mut *tx).await?;
                sqlx::query("UPDATE agent_session_t SET cleanup_request_id=$3,cleanup_state='CLEANUP_PENDING' WHERE host_id=$1 AND session_id=$2").bind(host_id).bind(session_id).bind(cleanup_id).execute(&mut *tx).await?;
            }
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn reconcile_projections(&self) -> Result<()> {
        let rows = sqlx::query("SELECT s.host_id,s.session_id,h.bank_id FROM agent_session_t s JOIN agent_session_history_t h ON h.host_id=s.host_id AND h.durable_session_id=s.session_id WHERE h.projection_sequence < (SELECT COALESCE(MAX(e.event_sequence),0) FROM agent_session_event_t e WHERE e.host_id=s.host_id AND e.session_id=s.session_id) LIMIT 100")
            .fetch_all(&self.pool).await?;
        for row in rows {
            self.rebuild_history_projection(
                row.try_get("host_id")?,
                AgentSessionId(row.try_get("session_id")?),
                row.try_get("bank_id")?,
            )
            .await?;
        }
        Ok(())
    }

    async fn accept_execution_result(
        &self,
        host_id: Uuid,
        action_attempt_id: Uuid,
        turn_id: Uuid,
        execution_id: Uuid,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT a.origin_accepted_ts,t.session_id,t.policy_digest,e.state,e.normalized_result,e.normalized_error,e.fencing_token FROM agent_action_attempt_t a JOIN agent_turn_t t ON t.host_id=a.host_id AND t.turn_id=a.turn_id JOIN execution_attempt_t e ON e.host_id=a.host_id AND e.execution_id=a.execution_attempt_id WHERE a.host_id=$1 AND a.action_attempt_id=$2 AND a.turn_id=$3 AND e.execution_id=$4 AND e.terminal_ts IS NOT NULL FOR UPDATE OF a,t,e")
            .bind(host_id).bind(action_attempt_id).bind(turn_id).bind(execution_id).fetch_optional(&mut *tx).await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(false);
        };
        if row
            .try_get::<Option<DateTime<Utc>>, _>("origin_accepted_ts")?
            .is_some()
        {
            tx.commit().await?;
            return Ok(false);
        }
        let session_id: Uuid = row.try_get("session_id")?;
        let policy: String = row.try_get("policy_digest")?;
        let state: String = row.try_get("state")?;
        let result = json!({"executionId":execution_id,"state":state,"result":row.try_get::<Option<Value>,_>("normalized_result")?,"error":row.try_get::<Option<Value>,_>("normalized_error")?,"fencingToken":row.try_get::<i64,_>("fencing_token")?});
        append_event(
            &mut tx,
            host_id,
            session_id,
            Some(turn_id),
            Some(action_attempt_id),
            "runner",
            "ACTION_RESULT",
            result.clone(),
            &policy,
        )
        .await?;
        sqlx::query("UPDATE agent_action_attempt_t SET state='ACCEPTED',result=$3,result_digest=$4,origin_accepted_ts=now(),updated_ts=now() WHERE host_id=$1 AND action_attempt_id=$2 AND origin_accepted_ts IS NULL")
            .bind(host_id).bind(action_attempt_id).bind(&result).bind(sha256_digest(&serde_json::to_vec(&result)?)).execute(&mut *tx).await?;
        sqlx::query("UPDATE execution_attempt_t SET accepted_by_origin_ts=COALESCE(accepted_by_origin_ts,now()),updated_ts=now() WHERE host_id=$1 AND execution_id=$2 AND terminal_ts IS NOT NULL")
            .bind(host_id).bind(execution_id).execute(&mut *tx).await?;
        sqlx::query("UPDATE agent_turn_t SET state=CASE WHEN $3 IN ('SUCCEEDED','FAILED','CANCELLED') THEN 'RUNNING_MODEL' ELSE 'UNKNOWN' END,updated_ts=now() WHERE host_id=$1 AND turn_id=$2 AND state IN ('RUNNING_ACTION','WAITING_RECONCILIATION')")
            .bind(host_id).bind(turn_id).bind(state).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn create_or_resume_session(&self, spec: &SessionSpec) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        persist_policy(&mut tx, spec.host_id, spec.agent_def_id, &spec.policy).await?;
        let result = sqlx::query(
            "INSERT INTO agent_session_t
             (host_id,session_id,principal_id,user_id,agent_def_id,agent_definition_version,bank_id,
              policy_snapshot_id,idle_expires_ts,maximum_expires_ts,resume_handle_digest)
             VALUES ($1,$2,$3,$4,$5,1,$6,$7,$8,$9,$10)
             ON CONFLICT (host_id,session_id) DO NOTHING",
        )
        .bind(spec.host_id)
        .bind(spec.session_id.0)
        .bind(&spec.principal_id)
        .bind(spec.user_id)
        .bind(spec.agent_def_id)
        .bind(spec.bank_id)
        .bind(spec.policy.snapshot_id)
        .bind(spec.idle_expires_at)
        .bind(spec.maximum_expires_at)
        .bind(&spec.resume_handle_digest)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() == 0 {
            let row = sqlx::query("SELECT principal_id,agent_def_id,state FROM agent_session_t WHERE host_id=$1 AND session_id=$2 FOR UPDATE")
                .bind(spec.host_id).bind(spec.session_id.0).fetch_one(&mut *tx).await?;
            let principal: String = row.try_get("principal_id")?;
            let definition: Uuid = row.try_get("agent_def_id")?;
            let state: String = row.try_get("state")?;
            if principal != spec.principal_id
                || definition != spec.agent_def_id
                || state != "ACTIVE"
            {
                bail!("durable agent session ownership or state mismatch");
            }
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn admit_user_turn(
        &self,
        host_id: Uuid,
        session_id: AgentSessionId,
        client_message_id: &str,
        text: &str,
        model_provider: &str,
        model_name: &str,
    ) -> Result<AdmittedTurn> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT next_turn_sequence,next_queue_sequence,policy_snapshot_id,(SELECT policy_digest FROM agent_policy_snapshot_t p WHERE p.host_id=s.host_id AND p.policy_snapshot_id=s.policy_snapshot_id) policy_digest,(SELECT data_boundary_digest FROM agent_policy_snapshot_t p WHERE p.host_id=s.host_id AND p.policy_snapshot_id=s.policy_snapshot_id) data_boundary_digest,maximum_expires_ts FROM agent_session_t s WHERE host_id=$1 AND session_id=$2 AND state='ACTIVE' FOR UPDATE")
            .bind(host_id).bind(session_id.0).fetch_optional(&mut *tx).await?.context("active agent session not found")?;
        if let Some(existing) = sqlx::query("SELECT turn_id,turn_sequence,policy_digest,data_boundary_digest FROM agent_turn_t WHERE host_id=$1 AND session_id=$2 AND client_message_id=$3")
            .bind(host_id).bind(session_id.0).bind(client_message_id).fetch_optional(&mut *tx).await? {
            tx.commit().await?;
            return Ok(AdmittedTurn { turn_id: AgentTurnId(existing.try_get("turn_id")?), turn_sequence: existing.try_get("turn_sequence")?, duplicate: true, policy_digest: existing.try_get("policy_digest")?, data_boundary_digest: existing.try_get("data_boundary_digest")? });
        }
        let turn_sequence: i64 = row.try_get("next_turn_sequence")?;
        let queue_sequence: i64 = row.try_get("next_queue_sequence")?;
        let policy_snapshot_id: Uuid = row.try_get("policy_snapshot_id")?;
        let policy_digest: String = row.try_get("policy_digest")?;
        let boundary: String = row.try_get("data_boundary_digest")?;
        let maximum: DateTime<Utc> = row.try_get("maximum_expires_ts")?;
        let deadline = std::cmp::min(Utc::now() + Duration::minutes(2), maximum);
        let turn_id = AgentTurnId::new();
        sqlx::query("INSERT INTO agent_turn_t (host_id,turn_id,session_id,turn_sequence,queue_sequence,origin_kind,client_message_id,idempotency_key,policy_snapshot_id,policy_digest,data_boundary_digest,model_provider,model_name,model_action_budget,token_budget,cost_budget_micros,deadline_ts) VALUES ($1,$2,$3,$4,$5,'user',$6,$6,$7,$8,$9,$10,$11,20,65536,0,$12)")
            .bind(host_id).bind(turn_id.0).bind(session_id.0).bind(turn_sequence).bind(queue_sequence).bind(client_message_id)
            .bind(policy_snapshot_id).bind(&policy_digest).bind(&boundary).bind(model_provider).bind(model_name).bind(deadline).execute(&mut *tx).await?;
        sqlx::query("UPDATE agent_session_t SET next_turn_sequence=next_turn_sequence+1,next_queue_sequence=next_queue_sequence+1,updated_ts=now() WHERE host_id=$1 AND session_id=$2")
            .bind(host_id).bind(session_id.0).execute(&mut *tx).await?;
        append_event(
            &mut tx,
            host_id,
            session_id.0,
            Some(turn_id.0),
            None,
            "user",
            "USER_MESSAGE",
            json!({"text": text}),
            &policy_digest,
        )
        .await?;
        tx.commit().await?;
        Ok(AdmittedTurn {
            turn_id,
            turn_sequence,
            duplicate: false,
            policy_digest,
            data_boundary_digest: boundary,
        })
    }

    pub async fn activate_next_turn(
        &self,
        host_id: Uuid,
        session_id: AgentSessionId,
    ) -> Result<Option<AgentTurnId>> {
        let mut tx = self.pool.begin().await?;
        let locked = sqlx::query("SELECT active_turn_id FROM agent_session_t WHERE host_id=$1 AND session_id=$2 AND state='ACTIVE' FOR UPDATE")
            .bind(host_id).bind(session_id.0).fetch_optional(&mut *tx).await?;
        let Some(row) = locked else {
            tx.commit().await?;
            return Ok(None);
        };
        if row.try_get::<Option<Uuid>, _>("active_turn_id")?.is_some() {
            tx.commit().await?;
            return Ok(None);
        }
        let turn = sqlx::query("SELECT turn_id FROM agent_turn_t WHERE host_id=$1 AND session_id=$2 AND state='QUEUED' ORDER BY queue_sequence FOR UPDATE SKIP LOCKED LIMIT 1")
            .bind(host_id).bind(session_id.0).fetch_optional(&mut *tx).await?;
        let Some(turn) = turn else {
            tx.commit().await?;
            return Ok(None);
        };
        let turn_id: Uuid = turn.try_get("turn_id")?;
        sqlx::query("UPDATE agent_turn_t SET state='RECEIVED',activated_ts=now(),updated_ts=now() WHERE host_id=$1 AND turn_id=$2 AND state='QUEUED'")
            .bind(host_id).bind(turn_id).execute(&mut *tx).await?;
        sqlx::query("UPDATE agent_session_t SET active_turn_id=$3,session_version=session_version+1,updated_ts=now() WHERE host_id=$1 AND session_id=$2 AND active_turn_id IS NULL")
            .bind(host_id).bind(session_id.0).bind(turn_id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(Some(AgentTurnId(turn_id)))
    }

    pub async fn propose_gateway_action(
        &self,
        host_id: Uuid,
        turn_id: AgentTurnId,
        stable_tool_ref: Uuid,
        model_alias: &str,
        arguments: &str,
    ) -> Result<(Uuid, Uuid)> {
        let mut tx = self.pool.begin().await?;
        let policy: String = sqlx::query_scalar("SELECT policy_digest FROM agent_turn_t WHERE host_id=$1 AND turn_id=$2 AND state IN ('RECEIVED','RUNNING_MODEL','WAITING_ACTION','RUNNING_ACTION') FOR UPDATE")
            .bind(host_id).bind(turn_id.0).fetch_one(&mut *tx).await?;
        let logical_action_id = Uuid::now_v7();
        let attempt_id = Uuid::now_v7();
        let tool_ref = stable_tool_ref;
        sqlx::query("INSERT INTO agent_action_attempt_t(host_id,action_attempt_id,turn_id,logical_action_id,attempt_number,stable_tool_ref,model_alias,placement,schema_digest,policy_digest,argument_digest,effect_class,state,gateway_request_id) VALUES($1,$2,$3,$4,1,$5,$6,'gateway',$7,$8,$9,'unknown','DISPATCHED',$10)")
            .bind(host_id).bind(attempt_id).bind(turn_id.0).bind(logical_action_id).bind(tool_ref).bind(model_alias)
            .bind(sha256_digest(model_alias.as_bytes())).bind(&policy).bind(sha256_digest(arguments.as_bytes())).bind(Uuid::now_v7()).execute(&mut *tx).await?;
        sqlx::query("UPDATE agent_turn_t SET state='RUNNING_ACTION',updated_ts=now() WHERE host_id=$1 AND turn_id=$2")
            .bind(host_id).bind(turn_id.0).execute(&mut *tx).await?;
        let session_id = session_id_for_turn(&mut tx, host_id, turn_id.0).await?;
        append_event(
            &mut tx,
            host_id,
            session_id,
            Some(turn_id.0),
            Some(attempt_id),
            "agent",
            "ACTION_DISPATCHED",
            json!({"modelAlias":model_alias,"placement":"gateway"}),
            &policy,
        )
        .await?;
        tx.commit().await?;
        Ok((attempt_id, tool_ref))
    }

    pub async fn accept_gateway_result(
        &self,
        host_id: Uuid,
        turn_id: AgentTurnId,
        action_attempt_id: Uuid,
        succeeded: bool,
        result: Value,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT a.origin_accepted_ts,t.session_id,t.policy_digest FROM agent_action_attempt_t a JOIN agent_turn_t t ON t.host_id=a.host_id AND t.turn_id=a.turn_id WHERE a.host_id=$1 AND a.action_attempt_id=$2 AND a.turn_id=$3 FOR UPDATE OF a,t")
            .bind(host_id).bind(action_attempt_id).bind(turn_id.0).fetch_one(&mut *tx).await?;
        if row
            .try_get::<Option<DateTime<Utc>>, _>("origin_accepted_ts")?
            .is_some()
        {
            tx.commit().await?;
            return Ok(());
        }
        let session_id: Uuid = row.try_get("session_id")?;
        let policy: String = row.try_get("policy_digest")?;
        append_event(
            &mut tx,
            host_id,
            session_id,
            Some(turn_id.0),
            Some(action_attempt_id),
            "gateway",
            "ACTION_RESULT",
            result.clone(),
            &policy,
        )
        .await?;
        sqlx::query("UPDATE agent_action_attempt_t SET state='ACCEPTED',result=$3,result_digest=$4,origin_accepted_ts=now(),updated_ts=now() WHERE host_id=$1 AND action_attempt_id=$2 AND origin_accepted_ts IS NULL")
            .bind(host_id).bind(action_attempt_id).bind(result.clone()).bind(sha256_digest(&serde_json::to_vec(&result)?)).execute(&mut *tx).await?;
        sqlx::query("UPDATE agent_turn_t SET state='RUNNING_MODEL',updated_ts=now(),terminal_error=CASE WHEN $3 THEN terminal_error ELSE $4 END WHERE host_id=$1 AND turn_id=$2")
            .bind(host_id).bind(turn_id.0).bind(succeeded).bind((!succeeded).then_some(result)).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn request_approval(
        &self,
        host_id: Uuid,
        turn_id: AgentTurnId,
        logical_action_id: Uuid,
        input_digest: &str,
        subject_digest: &str,
        expires_at: DateTime<Utc>,
    ) -> Result<Uuid> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT session_id,policy_digest FROM agent_turn_t WHERE host_id=$1 AND turn_id=$2 FOR UPDATE")
            .bind(host_id).bind(turn_id.0).fetch_one(&mut *tx).await?;
        let session_id: Uuid = row.try_get("session_id")?;
        let policy: String = row.try_get("policy_digest")?;
        let approval_id = Uuid::now_v7();
        sqlx::query("INSERT INTO agent_approval_t(host_id,approval_id,turn_id,logical_action_id,subject_digest,input_digest,policy_digest,approver_scope,nonce_digest,expires_ts) VALUES($1,$2,$3,$4,$5,$6,$7,'{}',$8,$9)")
            .bind(host_id).bind(approval_id).bind(turn_id.0).bind(logical_action_id).bind(subject_digest).bind(input_digest).bind(&policy).bind(sha256_digest(Uuid::now_v7().as_bytes())).bind(expires_at).execute(&mut *tx).await?;
        sqlx::query("UPDATE agent_turn_t SET state='WAITING_APPROVAL',updated_ts=now() WHERE host_id=$1 AND turn_id=$2").bind(host_id).bind(turn_id.0).execute(&mut *tx).await?;
        append_event(
            &mut tx,
            host_id,
            session_id,
            Some(turn_id.0),
            None,
            "agent",
            "APPROVAL_REQUESTED",
            json!({"approvalId":approval_id,"logicalActionId":logical_action_id}),
            &policy,
        )
        .await?;
        tx.commit().await?;
        Ok(approval_id)
    }

    pub async fn approve_and_create_fresh_attempt(
        &self,
        host_id: Uuid,
        approval_id: Uuid,
        actor: &str,
        stable_tool_ref: Uuid,
        model_alias: &str,
        placement: &str,
        schema_digest: &str,
        argument_digest: &str,
    ) -> Result<Uuid> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT a.turn_id,a.logical_action_id,a.policy_digest,t.session_id FROM agent_approval_t a JOIN agent_turn_t t ON t.host_id=a.host_id AND t.turn_id=a.turn_id WHERE a.host_id=$1 AND a.approval_id=$2 AND a.state='REQUESTED' AND a.expires_ts>now() FOR UPDATE OF a,t")
            .bind(host_id).bind(approval_id).fetch_optional(&mut *tx).await?.context("approval is unavailable or expired")?;
        let turn_id: Uuid = row.try_get("turn_id")?;
        let logical: Uuid = row.try_get("logical_action_id")?;
        let policy: String = row.try_get("policy_digest")?;
        let session: Uuid = row.try_get("session_id")?;
        let attempt_number: i32 = sqlx::query_scalar("SELECT COALESCE(MAX(attempt_number),0)+1 FROM agent_action_attempt_t WHERE host_id=$1 AND turn_id=$2 AND logical_action_id=$3").bind(host_id).bind(turn_id).bind(logical).fetch_one(&mut *tx).await?;
        let attempt_id = Uuid::now_v7();
        sqlx::query("INSERT INTO agent_action_attempt_t(host_id,action_attempt_id,turn_id,logical_action_id,attempt_number,stable_tool_ref,model_alias,placement,schema_digest,policy_digest,argument_digest,effect_class,state,approval_id) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,'unknown','READY',$12)")
            .bind(host_id).bind(attempt_id).bind(turn_id).bind(logical).bind(attempt_number).bind(stable_tool_ref).bind(model_alias).bind(placement).bind(schema_digest).bind(&policy).bind(argument_digest).bind(approval_id).execute(&mut *tx).await?;
        sqlx::query("UPDATE agent_approval_t SET state='APPROVED',decision_actor=$3,decision_ts=now(),consumed_action_attempt_id=$4 WHERE host_id=$1 AND approval_id=$2 AND state='REQUESTED'").bind(host_id).bind(approval_id).bind(actor).bind(attempt_id).execute(&mut *tx).await?;
        sqlx::query("UPDATE agent_turn_t SET state='WAITING_ACTION',updated_ts=now() WHERE host_id=$1 AND turn_id=$2 AND state='WAITING_APPROVAL'").bind(host_id).bind(turn_id).execute(&mut *tx).await?;
        append_event(&mut tx,host_id,session,Some(turn_id),Some(attempt_id),"approver","APPROVAL_GRANTED",json!({"approvalId":approval_id,"freshAttempt":attempt_id,"attemptNumber":attempt_number}),&policy).await?;
        tx.commit().await?;
        Ok(attempt_id)
    }

    pub async fn complete_turn(
        &self,
        host_id: Uuid,
        session_id: AgentSessionId,
        turn_id: AgentTurnId,
        response: &str,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "SELECT policy_digest FROM agent_turn_t WHERE host_id=$1 AND turn_id=$2 FOR UPDATE",
        )
        .bind(host_id)
        .bind(turn_id.0)
        .fetch_one(&mut *tx)
        .await?;
        let policy: String = row.try_get("policy_digest")?;
        append_event(
            &mut tx,
            host_id,
            session_id.0,
            Some(turn_id.0),
            None,
            "model",
            "MODEL_RESULT",
            json!({"text":response}),
            &policy,
        )
        .await?;
        append_event(
            &mut tx,
            host_id,
            session_id.0,
            Some(turn_id.0),
            None,
            "system",
            "TURN_COMPLETED",
            json!({}),
            &policy,
        )
        .await?;
        sqlx::query("UPDATE agent_turn_t SET state='COMPLETED',terminal_result=$3,terminal_ts=now(),updated_ts=now() WHERE host_id=$1 AND turn_id=$2 AND state NOT IN ('COMPLETED','FAILED','CANCELLED','UNKNOWN')")
            .bind(host_id).bind(turn_id.0).bind(json!({"text":response})).execute(&mut *tx).await?;
        sqlx::query("UPDATE agent_session_t SET active_turn_id=NULL,session_version=session_version+1,updated_ts=now() WHERE host_id=$1 AND session_id=$2 AND active_turn_id=$3")
            .bind(host_id).bind(session_id.0).bind(turn_id.0).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn fail_turn(
        &self,
        host_id: Uuid,
        session_id: AgentSessionId,
        turn_id: AgentTurnId,
        reason: &str,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE agent_turn_t SET state='FAILED',terminal_error=$3,terminal_ts=now(),updated_ts=now() WHERE host_id=$1 AND turn_id=$2 AND state NOT IN ('COMPLETED','FAILED','CANCELLED','UNKNOWN')")
            .bind(host_id).bind(turn_id.0).bind(json!({"message":reason})).execute(&mut *tx).await?;
        sqlx::query("UPDATE agent_session_t SET active_turn_id=NULL,session_version=session_version+1,updated_ts=now() WHERE host_id=$1 AND session_id=$2 AND active_turn_id=$3")
            .bind(host_id).bind(session_id.0).bind(turn_id.0).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn rebuild_history_projection(
        &self,
        host_id: Uuid,
        session_id: AgentSessionId,
        bank_id: Uuid,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        let events = sqlx::query("SELECT event_sequence,event_type,content FROM agent_session_event_t WHERE host_id=$1 AND session_id=$2 AND event_type IN ('USER_MESSAGE','MODEL_RESULT') ORDER BY event_sequence")
            .bind(host_id).bind(session_id.0).fetch_all(&mut *tx).await?;
        let mut messages = Vec::with_capacity(events.len());
        let mut sequence = 0_i64;
        for event in events {
            sequence = event.try_get("event_sequence")?;
            let kind: String = event.try_get("event_type")?;
            let content: Value = event.try_get("content")?;
            messages.push(json!({"role": if kind == "USER_MESSAGE" {"user"} else {"assistant"}, "content": content.get("text").cloned().unwrap_or(Value::Null)}));
        }
        sqlx::query("INSERT INTO agent_session_history_t(host_id,bank_id,session_id,durable_session_id,messages,projection_sequence) VALUES($1,$2,$3,$3,$4,$5) ON CONFLICT(host_id,bank_id,session_id) DO UPDATE SET messages=EXCLUDED.messages,durable_session_id=EXCLUDED.durable_session_id,projection_sequence=EXCLUDED.projection_sequence,aggregate_version=agent_session_history_t.aggregate_version+1,update_ts=now() WHERE agent_session_history_t.projection_sequence < EXCLUDED.projection_sequence")
            .bind(host_id).bind(bank_id).bind(session_id.0).bind(Value::Array(messages)).bind(sequence).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }
}

async fn persist_policy(
    tx: &mut Transaction<'_, Postgres>,
    host_id: Uuid,
    agent_def_id: Uuid,
    policy: &PolicySnapshot,
) -> Result<()> {
    let value = serde_json::to_value(policy)?;
    let digest = sha256_digest(&serde_json::to_vec(&value)?);
    sqlx::query("INSERT INTO agent_policy_snapshot_t(host_id,policy_snapshot_id,agent_def_id,definition_digest,product_profile_digest,model_digest,catalog_digest,memory_digest,execution_digest,channel_digest,data_boundary_digest,resolved_snapshot,policy_digest) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) ON CONFLICT(host_id,policy_snapshot_id) DO NOTHING")
        .bind(host_id).bind(policy.snapshot_id).bind(agent_def_id).bind(&policy.definition_digest).bind(&policy.product_profile_digest).bind(&policy.model_digest).bind(&policy.catalog_digest).bind(&policy.memory_digest).bind(&policy.execution_digest).bind(&policy.channel_digest).bind(&policy.data_boundary_digest).bind(value).bind(digest).execute(&mut **tx).await?;
    Ok(())
}

async fn session_id_for_turn(
    tx: &mut Transaction<'_, Postgres>,
    host_id: Uuid,
    turn_id: Uuid,
) -> Result<Uuid> {
    Ok(
        sqlx::query_scalar("SELECT session_id FROM agent_turn_t WHERE host_id=$1 AND turn_id=$2")
            .bind(host_id)
            .bind(turn_id)
            .fetch_one(&mut **tx)
            .await?,
    )
}

async fn append_event(
    tx: &mut Transaction<'_, Postgres>,
    host_id: Uuid,
    session_id: Uuid,
    turn_id: Option<Uuid>,
    action_attempt_id: Option<Uuid>,
    actor: &str,
    kind: &str,
    content: Value,
    policy_digest: &str,
) -> Result<()> {
    let digest = sha256_digest(&serde_json::to_vec(&content)?);
    sqlx::query("INSERT INTO agent_session_event_t(host_id,event_id,session_id,event_sequence,turn_id,action_attempt_id,actor_class,event_type,content,content_digest,policy_digest) SELECT $1,$2,$3,COALESCE(MAX(event_sequence),0)+1,$4,$5,$6,$7,$8,$9,$10 FROM agent_session_event_t WHERE host_id=$1 AND session_id=$3")
        .bind(host_id).bind(Uuid::now_v7()).bind(session_id).bind(turn_id).bind(action_attempt_id).bind(actor).bind(kind).bind(content).bind(digest).bind(policy_digest).execute(&mut **tx).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[tokio::test]
    async fn durable_admission_is_idempotent_fifo_and_projection_rebuildable() {
        let Ok(url) = std::env::var("LIGHT_AGENT_TEST_DATABASE_URL") else {
            return;
        };
        let pool = PgPool::connect(&url).await.unwrap();
        let host_id = Uuid::now_v7();
        let agent_def_id = Uuid::now_v7();
        let owner = Uuid::now_v7();
        let domain = format!("agent-{}.test", host_id.simple());
        let mut setup = pool.begin().await.unwrap();
        sqlx::query("INSERT INTO org_t(domain,org_name,org_desc,org_owner) VALUES($1,'agent-test','agent-test',$2)").bind(&domain).bind(owner).execute(&mut *setup).await.unwrap();
        sqlx::query(
            "INSERT INTO host_t(host_id,domain,sub_domain,host_owner) VALUES($1,$2,'test',$3)",
        )
        .bind(host_id)
        .bind(&domain)
        .bind(owner)
        .execute(&mut *setup)
        .await
        .unwrap();
        sqlx::query("INSERT INTO api_t(host_id,api_id,api_name,api_status) VALUES($1,'agent','agent','Published')").bind(host_id).execute(&mut *setup).await.unwrap();
        sqlx::query("INSERT INTO api_version_t(host_id,api_version_id,api_id,api_version,api_type,service_id) VALUES($1,$2,'agent','1.0.0','mcp','agent-test')").bind(host_id).bind(agent_def_id).execute(&mut *setup).await.unwrap();
        sqlx::query("INSERT INTO agent_definition_t(host_id,agent_def_id,model_provider,model_name) VALUES($1,$2,'mock','mock')").bind(host_id).bind(agent_def_id).execute(&mut *setup).await.unwrap();
        setup.commit().await.unwrap();
        let repository = AgentRepository::new(pool.clone());
        let session = AgentSessionId::new();
        let digest = |name: &str| sha256_digest(name.as_bytes());
        repository
            .create_or_resume_session(&SessionSpec {
                host_id,
                session_id: session,
                principal_id: Uuid::now_v7().to_string(),
                user_id: None,
                agent_def_id,
                bank_id: None,
                policy: PolicySnapshot {
                    snapshot_id: session.0,
                    definition_digest: digest("definition"),
                    product_profile_digest: digest("profile"),
                    model_digest: digest("model"),
                    catalog_digest: digest("catalog"),
                    memory_digest: digest("memory"),
                    execution_digest: digest("execution"),
                    channel_digest: digest("channel"),
                    data_boundary_digest: digest("boundary"),
                    tools: BTreeMap::new(),
                },
                idle_expires_at: Utc::now() + Duration::hours(1),
                maximum_expires_at: Utc::now() + Duration::hours(2),
                resume_handle_digest: digest(&session.to_string()),
            })
            .await
            .unwrap();
        let first = repository
            .admit_user_turn(host_id, session, "message-1", "hello", "mock", "mock")
            .await
            .unwrap();
        let duplicate = repository
            .admit_user_turn(host_id, session, "message-1", "hello", "mock", "mock")
            .await
            .unwrap();
        let second = repository
            .admit_user_turn(host_id, session, "message-2", "again", "mock", "mock")
            .await
            .unwrap();
        assert_eq!(first.turn_id, duplicate.turn_id);
        assert!(duplicate.duplicate);
        assert!(second.turn_sequence > first.turn_sequence);
        assert_eq!(
            repository
                .activate_next_turn(host_id, session)
                .await
                .unwrap(),
            Some(first.turn_id)
        );
        repository
            .complete_turn(host_id, session, first.turn_id, "world")
            .await
            .unwrap();
        assert_eq!(
            repository
                .activate_next_turn(host_id, session)
                .await
                .unwrap(),
            Some(second.turn_id)
        );
        sqlx::query("DELETE FROM agent_session_t WHERE host_id=$1 AND session_id=$2")
            .bind(host_id)
            .bind(session.0)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "DELETE FROM agent_policy_snapshot_t WHERE host_id=$1 AND policy_snapshot_id=$2",
        )
        .bind(host_id)
        .bind(session.0)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("DELETE FROM org_t WHERE domain=$1")
            .bind(domain)
            .execute(&pool)
            .await
            .unwrap();
    }
}
