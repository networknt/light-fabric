//! Trusted Workflow-to-Gateway action producer. No model-provided identity,
//! target override, depth, budget, or authorization header is accepted here.
use crate::long_authority::LongAuthority;
use crate::run_authority::{PerRunAuthority, RunAuthority};
use crate::run_token::RunTokenSelector;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use uuid::Uuid;
use workflow_action::{Binding, ExecutionClass, ledger::Ledger};
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    pub gateway_url: String,
    pub service_id: String,
    pub client_identity_file: PathBuf,
    pub ca_file: PathBuf,
    pub scope_token_file: PathBuf,
    pub maximum_depth: u16,
    pub request_byte_limit: u64,
    pub response_byte_limit: u64,
    pub cost_unit_limit: u64,
}
pub struct Runtime {
    pool: PgPool,
    authority: Arc<PerRunAuthority>,
    tokens: Arc<RunTokenSelector>,
    long: Option<Arc<LongAuthority>>,
    client: reqwest::Client,
    scope: String,
    config: Config,
    agent_services: std::collections::BTreeMap<String, Uuid>,
}

/// The trusted Workflow-to-Gateway dispatch boundary. Keeping this interface
/// injectable lets component tests exercise private task dispatch without
/// installing a Gateway or issuer credential in the shared local stack.
#[async_trait::async_trait]
pub trait Dispatch: Send + Sync {
    fn long_gateway_origin(&self) -> Option<String> {
        None
    }
    async fn long_workload_token(
        &self,
    ) -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(None)
    }
    async fn authorize_private_run(
        &self,
        host: Uuid,
        process: Uuid,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>;
    async fn authorize_agent(
        &self,
        host: Uuid,
        process: Uuid,
        agent: Uuid,
    ) -> Result<(chrono::DateTime<chrono::Utc>, i32, i32), Box<dyn std::error::Error + Send + Sync>>;
    async fn call(
        &self,
        host: Uuid,
        process: Uuid,
        attempt: Uuid,
        alias: &str,
        params: Value,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>>;
}

#[async_trait::async_trait]
impl Dispatch for Runtime {
    fn long_gateway_origin(&self) -> Option<String> {
        self.long.as_ref().map(|_| self.config.gateway_url.clone())
    }
    async fn long_workload_token(
        &self,
    ) -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>> {
        match &self.long {
            Some(long) => Ok(Some(long.workload_token().await?)),
            None => Ok(None),
        }
    }
    async fn authorize_private_run(
        &self,
        host: Uuid,
        process: Uuid,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Runtime::authorize_private_run(self, host, process).await
    }

    async fn authorize_agent(
        &self,
        host: Uuid,
        process: Uuid,
        agent: Uuid,
    ) -> Result<(chrono::DateTime<chrono::Utc>, i32, i32), Box<dyn std::error::Error + Send + Sync>>
    {
        Runtime::authorize_agent(self, host, process, agent).await
    }

    async fn call(
        &self,
        host: Uuid,
        process: Uuid,
        attempt: Uuid,
        alias: &str,
        params: Value,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        Runtime::call(self, host, process, attempt, alias, params).await
    }
}
fn denied() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "workflow MCP action denied",
    )
}
impl Runtime {
    pub async fn authorize_private_run(
        &self,
        host: Uuid,
        process: Uuid,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let row = sqlx::query(
            "SELECT i.workflow_instance_id,a.grant_id,a.user_id
               FROM workflow_ops.workflow_invocation_t i
               JOIN workflow_ops.process_info_t p
                 ON p.host_id=i.host_id AND p.process_id=i.process_id
               JOIN workflow_ops.workflow_action_authority_t a
                 ON a.host_id=i.host_id AND a.run_id=i.workflow_instance_id
              WHERE i.host_id=$1 AND i.process_id=$2
                AND i.response_policy_snapshot->>'acceptedAdmissionProfile'='portal_execution'
                AND (i.deadline_ts>clock_timestamp()
                     OR i.response_policy_snapshot->'privateExecutionProfile'->>'version'='1')
                AND (p.deadline_ts IS NULL OR p.deadline_ts>clock_timestamp())
                AND i.state IN('ACCEPTED','RUNNING','WAITING')
                AND i.cancel_requested_ts IS NULL
                AND a.active AND a.deadline>clock_timestamp()
                AND a.user_id::text=i.end_user_subject",
        )
        .bind(host)
        .bind(process)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(denied)?;
        let verified: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workflow_ops.workflow_verified_invocation_t WHERE host_id=$1 AND run_id=$2)")
            .bind(host).bind(row.get::<Uuid,_>("workflow_instance_id")).fetch_one(&self.pool).await?;
        if verified {
            return Ok(());
        }
        self.authority
            .lock_run_authority(
                row.get("workflow_instance_id"),
                row.get("grant_id"),
                host,
                row.get("user_id"),
            )
            .await?;
        Ok(())
    }

    pub async fn new(
        pool: PgPool,
        long: Option<Arc<LongAuthority>>,
        authority: Arc<PerRunAuthority>,
        tokens: Arc<RunTokenSelector>,
        scope: String,
        config: &Config,
        dir: &Path,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let url = url::Url::parse(&config.gateway_url)?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || long
                .as_ref()
                .is_some_and(|long| url.origin() != long.gateway_origin())
            || url.query().is_some()
            || url.fragment().is_some()
            || config.service_id.is_empty()
            || config.maximum_depth > 16
            || config.request_byte_limit == 0
            || config.response_byte_limit == 0
        {
            return Err(denied().into());
        }
        let mut builder = reqwest::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(120));
        if !config.ca_file.as_os_str().is_empty() {
            let ca = tokio::fs::read(dir.join(&config.ca_file)).await?;
            for cert in reqwest::Certificate::from_pem_bundle(&ca)? {
                builder = builder.add_root_certificate(cert);
            }
        }
        if !config.client_identity_file.as_os_str().is_empty() {
            builder = builder.identity(reqwest::Identity::from_pem(
                &tokio::fs::read(dir.join(&config.client_identity_file)).await?,
            )?);
        }
        Ok(Self {
            pool,
            authority,
            tokens,
            long,
            client: builder.build()?,
            scope,
            config: config.clone(),
            agent_services: Default::default(),
        })
    }
    pub fn with_agent_services(
        mut self,
        services: std::collections::BTreeMap<String, Uuid>,
    ) -> Self {
        self.agent_services = services;
        self
    }
    pub async fn authorize_agent(
        &self,
        host: Uuid,
        process: Uuid,
        agent: Uuid,
    ) -> Result<(chrono::DateTime<chrono::Utc>, i32, i32), Box<dyn std::error::Error + Send + Sync>>
    {
        if !self.agent_services.values().any(|def| *def == agent) {
            return Err(denied().into());
        }
        let row = sqlx::query("SELECT i.workflow_instance_id,CASE WHEN i.response_policy_snapshot->'privateExecutionProfile'->>'version'='1' THEN LEAST(a.deadline,COALESCE(p.deadline_ts,a.deadline)) ELSE LEAST(i.deadline_ts,a.deadline) END AS deadline_ts,i.permit_depth,a.grant_id,a.user_id FROM workflow_ops.workflow_invocation_t i JOIN workflow_ops.process_info_t p ON p.host_id=i.host_id AND p.process_id=i.process_id JOIN workflow_ops.workflow_action_authority_t a ON a.host_id=i.host_id AND a.run_id=i.workflow_instance_id WHERE i.host_id=$1 AND i.process_id=$2 AND i.state IN('ACCEPTED','RUNNING','WAITING') AND i.cancel_requested_ts IS NULL AND (i.deadline_ts>clock_timestamp() OR i.response_policy_snapshot->'privateExecutionProfile'->>'version'='1') AND (p.deadline_ts IS NULL OR p.deadline_ts>clock_timestamp()) AND a.active AND a.deadline>clock_timestamp() AND a.user_id::text=i.end_user_subject")
            .bind(host).bind(process).fetch_optional(&self.pool).await?.ok_or_else(denied)?;
        let depth: i32 = row.get("permit_depth");
        if depth < 0 || depth > i32::from(self.config.maximum_depth) {
            return Err(denied().into());
        }
        self.authority
            .lock_run_authority(
                row.get("workflow_instance_id"),
                row.get("grant_id"),
                host,
                row.get("user_id"),
            )
            .await?;
        Ok((
            row.get("deadline_ts"),
            depth,
            i32::from(self.config.maximum_depth),
        ))
    }
    pub async fn call(
        &self,
        host: Uuid,
        process: Uuid,
        attempt: Uuid,
        alias: &str,
        mut params: Value,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        let row=sqlx::query("SELECT i.workflow_instance_id,i.binding_id,EXISTS(SELECT 1 FROM workflow_ops.workflow_verified_invocation_t v WHERE v.host_id=i.host_id AND v.run_id=i.workflow_instance_id) AS strict,i.end_user_subject,i.policy_digest,i.response_policy_digest,i.execution_class,i.permit_depth,CASE WHEN i.response_policy_snapshot->'privateExecutionProfile'->>'version'='1' THEN LEAST(a.deadline,COALESCE(p.deadline_ts,a.deadline)) ELSE LEAST(i.deadline_ts,a.deadline) END AS deadline_ts,a.credential_kind,a.grant_id,a.grant_generation,a.run_generation,a.budget_generation,d.nested_tool_id,d.contract_digest,d.dispatch_target FROM workflow_ops.workflow_invocation_t i JOIN workflow_ops.process_info_t p ON p.host_id=i.host_id AND p.process_id=i.process_id JOIN workflow_ops.workflow_action_authority_t a ON a.host_id=i.host_id AND a.run_id=i.workflow_instance_id LEFT JOIN workflow_ops.workflow_tool_dependency_t d ON d.host_id=i.host_id AND d.outer_binding_id=i.binding_id AND d.authorization_tool_name=$3 AND d.lifecycle_status<>'revoked' WHERE i.host_id=$1 AND i.process_id=$2 AND (i.binding_id IS NULL OR d.nested_tool_id IS NOT NULL) AND i.state IN('ACCEPTED','RUNNING','WAITING') AND i.cancel_requested_ts IS NULL AND (i.deadline_ts>clock_timestamp() OR i.response_policy_snapshot->'privateExecutionProfile'->>'version'='1') AND (p.deadline_ts IS NULL OR p.deadline_ts>clock_timestamp()) AND a.active AND a.deadline>clock_timestamp()")
            .bind(host).bind(process).bind(alias).fetch_optional(&self.pool).await?.ok_or_else(denied)?;
        let run: Uuid = row.get("workflow_instance_id");
        let user = row.get::<String, _>("end_user_subject").parse::<Uuid>()?;
        let strict = row.get::<bool, _>("strict");
        let context = if strict {
            let mut tx = self.pool.begin().await?;
            let context = crate::verified_task_context::project(
                &mut tx,
                host,
                process,
                attempt,
                alias,
                params.get("arguments").ok_or_else(denied)?,
            )
            .await?;
            tx.commit().await?;
            Some(context)
        } else {
            None
        };
        let native = row.get::<Option<Uuid>, _>("binding_id").is_none();
        let name = if native {
            // A native start has no caller-side Tool binding. The accepted
            // definition selects a tool; Gateway owns its backend routing.
            if context.is_none() {
                return Err(denied().into());
            }
            alias.to_owned()
        } else {
            let target: Value = row.get("dispatch_target");
            if target.get("endpoint").and_then(Value::as_str)
                != Some(self.config.gateway_url.as_str())
            {
                return Err(denied().into());
            }
            target
                .get("toolName")
                .or_else(|| target.get("targetName"))
                .and_then(Value::as_str)
                .ok_or_else(denied)?
                .to_owned()
        };
        params
            .as_object_mut()
            .ok_or_else(denied)?
            .insert("name".into(), Value::String(name));
        if strict {
            params.as_object_mut().ok_or_else(denied)?.insert(
                "_meta".into(),
                json!({
                    "io.modelcontextprotocol/protocolVersion":"2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities":{}
                }),
            );
        }
        let bytes = serde_json::to_vec(
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":params}),
        )?;
        if bytes.len() as u64 > self.config.request_byte_limit {
            return Err(denied().into());
        }
        if !strict {
            self.authority
                .lock_run_authority(run, row.get("grant_id"), host, user)
                .await?;
        }
        let selected = self
            .tokens
            .select_run_token_with_source(run, host, user, chrono::Utc::now())
            .await?;
        let access_token = selected.token;
        if let Some(context) = context.as_ref() {
            let verified = crate::verified_caller::verify_user(
                self.tokens.security_runtime(),
                &access_token,
                host,
            )
            .await?;
            if verified.user_id != context.creator.user_id
                || verified.issuer != context.creator.issuer
            {
                return Err(denied().into());
            }
        }
        // The selector verified this exact JWT before exposing the credential.
        use base64::Engine as _;
        let payload = access_token.split('.').nth(1).ok_or_else(denied)?;
        let claims: Value = serde_json::from_slice(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload)?,
        )?;
        let tool: Uuid = context
            .as_ref()
            .map(|c| c.tool_ref)
            .unwrap_or_else(|| row.get("nested_tool_id"));
        let mut binding = Binding {
            host_id: host,
            user_id: user,
            grant_id: row.get("grant_id"),
            run_id: run,
            action_id: Uuid::now_v7(),
            attempt_id: attempt,
            calling_app: self.config.service_id.clone(),
            request_digest: workflow_action::request_digest(
                "POST",
                &self.config.gateway_url,
                &tool.to_string(),
                &bytes,
            ),
            request_bytes: self.config.request_byte_limit,
            response_byte_limit: self.config.response_byte_limit,
            cost_unit_limit: self.config.cost_unit_limit,
            tool_ref: tool,
            target: self.config.gateway_url.clone(),
            contract_digest: context
                .as_ref()
                .map(|c| c.contract_digest.clone())
                .unwrap_or_else(|| row.get("contract_digest")),
            policy_digest: row.get("policy_digest"),
            disclosure_digest: row.get("response_policy_digest"),
            claims_digest: workflow_invocation_contract::canonical_sha256(
                &workflow_invocation_contract::stable_subject_claims(&claims),
            )?,
            grant_generation: row.get("grant_generation"),
            run_generation: row.get("run_generation"),
            budget_generation: row.get("budget_generation"),
            action_generation: 1,
            execution_class: serde_json::from_value::<ExecutionClass>(Value::String(
                row.get("execution_class"),
            ))?,
            depth: u16::try_from(row.get::<i32, _>("permit_depth"))?,
            maximum_depth: self.config.maximum_depth,
            parent_action_id: None,
            deadline: context
                .as_ref()
                .map(|c| c.effective_deadline)
                .unwrap_or_else(|| row.get("deadline_ts")),
        };
        let mut credential_transition = false;
        if let Some(stored)=sqlx::query_scalar::<_,Value>("SELECT binding FROM workflow_ops.workflow_action_permit_t WHERE host_id=$1 AND run_id=$2 AND attempt_id=$3")
            .bind(host).bind(run).bind(attempt).fetch_optional(&self.pool).await? {
            let previous:Binding=serde_json::from_value(stored)?;binding.action_id=previous.action_id;
            if binding != previous {
                // Credential changes are accepted only for completed-result recovery
                // after verified LONG exchange. All other permit fields stay exact.
                if context.is_none() || selected.source != crate::run_token::RunTokenSource::LongExchange {
                    return Err(denied().into());
                }
                binding.claims_digest = previous.claims_digest.clone();
                if binding != previous { return Err(denied().into()); }
                credential_transition = true;
            }
        }
        let ledger = Ledger::new(self.pool.clone());
        if let Some(context) = context.as_ref() {
            let mut tx = self.pool.begin().await?;
            crate::verified_task_context::install(&mut tx, &ledger, &binding, context).await?;
            let user_exp = claims
                .get("exp")
                .and_then(Value::as_i64)
                .ok_or_else(denied)?;
            let recovered:Option<Value>=sqlx::query_scalar("SELECT r.result FROM workflow_ops.workflow_verified_task_result_t r JOIN workflow_ops.task_info_t t ON t.host_id=r.host_id AND t.task_id=$3 WHERE r.host_id=$1 AND r.action_id=$2 AND clock_timestamp()<$4 AND t.lease_expires_ts>clock_timestamp() AND clock_timestamp()<to_timestamp($5)")
                .bind(host).bind(binding.action_id).bind(attempt).bind(binding.deadline).bind(user_exp as f64).fetch_optional(&mut *tx).await?;
            tx.commit().await?;
            if let Some(result) = recovered {
                return Ok(result);
            }
            if credential_transition {
                return Err(denied().into());
            }
        } else {
            ledger.install_permit(&binding, 10).await?;
        }
        let mut request = self
            .client
            .post(&self.config.gateway_url)
            .bearer_auth(&access_token);
        if row.get::<&str, _>("credential_kind") == "long" {
            let long = self.long.as_ref().ok_or_else(denied)?;
            request = request.header(
                "x-scope-token",
                format!("Bearer {}", long.workload_token().await?),
            );
        } else {
            request = request.header("x-scope-token", &self.scope);
        }
        if strict {
            request = request
                .header("mcp-protocol-version", "2026-07-28")
                .header("mcp-method", "tools/call")
                .header(
                    "mcp-name",
                    params
                        .get("name")
                        .and_then(Value::as_str)
                        .ok_or_else(denied)?,
                );
        }
        let mut response = request
            .header("x-workflow-action", binding.action_id.to_string())
            .header("content-type", "application/json")
            .header(
                "accept",
                if strict {
                    "application/json, text/event-stream"
                } else {
                    "application/json"
                },
            )
            .body(bytes)
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(denied().into());
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if chunk.len() > self.config.response_byte_limit as usize - body.len() {
                return Err(denied().into());
            }
            body.extend_from_slice(&chunk)
        }
        let value: Value = serde_json::from_slice(&body)?;
        if value.get("error").is_some() {
            return Err(denied().into());
        }
        value.get("result").cloned().ok_or_else(|| denied().into())
    }
}

#[cfg(test)]
#[path = "generic_transport_tests.rs"]
mod generic_transport_tests;
