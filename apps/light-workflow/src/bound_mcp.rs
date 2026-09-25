//! Trusted Workflow-to-Gateway action producer. No model-provided identity,
//! target override, depth, budget, or authorization header is accepted here.
use crate::credential_broker::CredentialBroker;
use crate::long_authority::LongAuthority;
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
    broker: Option<Arc<CredentialBroker>>,
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
    async fn long_owner_token(
        &self,
        _host: Uuid,
        _process: Uuid,
    ) -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(None)
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
    async fn long_owner_token(
        &self,
        host: Uuid,
        process: Uuid,
    ) -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>> {
        let Some(long) = &self.long else {
            return Ok(None);
        };
        let row = sqlx::query("SELECT i.workflow_instance_id,a.grant_id,a.user_id
            FROM workflow_ops.workflow_invocation_t i
            JOIN workflow_ops.workflow_action_authority_t a
              ON a.host_id=i.host_id AND a.run_id=i.workflow_instance_id
            WHERE i.host_id=$1 AND i.process_id=$2 AND i.state IN ('ACCEPTED','RUNNING','WAITING')
              AND i.cancel_requested_ts IS NULL AND a.active AND a.user_id::text=i.end_user_subject")
            .bind(host).bind(process).fetch_optional(&self.pool).await?.ok_or_else(denied)?;
        let run: Uuid = row.get("workflow_instance_id");
        let user: Uuid = row.get("user_id");
        if long.binding_for(run, host, user).await? != row.get::<Uuid, _>("grant_id") {
            return Err(denied().into());
        }
        Ok(Some(long.token_for(run, host, user).await?))
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
        if let Some(long) = &self.long {
            let run: Uuid = row.get("workflow_instance_id");
            let user: Uuid = row.get("user_id");
            if long.binding_for(run, host, user).await? != row.get::<Uuid, _>("grant_id") {
                return Err(denied().into());
            }
            let _ = long.token_for(run, host, user).await?;
        } else {
            self.broker
                .as_ref()
                .ok_or_else(denied)?
                .lock_run_authority(
                    row.get("workflow_instance_id"),
                    row.get("grant_id"),
                    host,
                    row.get("user_id"),
                )
                .await?;
        }
        Ok(())
    }

    pub async fn new(
        pool: PgPool,
        broker: Arc<CredentialBroker>,
        config: &Config,
        dir: &Path,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let url = url::Url::parse(&config.gateway_url)?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
            || config.service_id.is_empty()
            || config.maximum_depth > 16
            || config.request_byte_limit == 0
            || config.request_byte_limit > 16 * 1024 * 1024
            || config.response_byte_limit == 0
            || config.response_byte_limit > 16 * 1024 * 1024
        {
            return Err(denied().into());
        }
        let identity = tokio::fs::read(dir.join(&config.client_identity_file)).await?;
        let ca = tokio::fs::read(dir.join(&config.ca_file)).await?;
        let scope = tokio::fs::read_to_string(dir.join(&config.scope_token_file))
            .await?
            .trim()
            .to_owned();
        if !scope.starts_with("Bearer ") || scope.bytes().any(|b| b == b'\n' || b == b'\r') {
            return Err(denied().into());
        }
        let mut builder = reqwest::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(120))
            .identity(reqwest::Identity::from_pem(&identity)?);
        for cert in reqwest::Certificate::from_pem_bundle(&ca)? {
            builder = builder.add_root_certificate(cert)
        }
        Ok(Self {
            pool,
            broker: Some(broker),
            long: None,
            client: builder.build()?,
            scope,
            config: config.clone(),
            agent_services: Default::default(),
        })
    }
    pub async fn new_long(
        pool: PgPool,
        long: Arc<LongAuthority>,
        config: &Config,
        dir: &Path,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let url = url::Url::parse(&config.gateway_url)?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || url.origin() != long.gateway_origin()
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
        Ok(Self {
            pool,
            broker: None,
            long: Some(long),
            client: builder.build()?,
            scope: String::new(),
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
        if let Some(long) = &self.long {
            let run: Uuid = row.get("workflow_instance_id");
            let user: Uuid = row.get("user_id");
            if long.binding_for(run, host, user).await? != row.get::<Uuid, _>("grant_id") {
                return Err(denied().into());
            }
            let _ = long.token_for(run, host, user).await?;
        } else {
            self.broker
                .as_ref()
                .ok_or_else(denied)?
                .lock_run_authority(
                    row.get("workflow_instance_id"),
                    row.get("grant_id"),
                    host,
                    row.get("user_id"),
                )
                .await?;
        }
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
        let row=sqlx::query("SELECT i.workflow_instance_id,i.end_user_subject,i.policy_digest,i.response_policy_digest,i.execution_class,i.permit_depth,CASE WHEN i.response_policy_snapshot->'privateExecutionProfile'->>'version'='1' THEN LEAST(a.deadline,COALESCE(p.deadline_ts,a.deadline)) ELSE LEAST(i.deadline_ts,a.deadline) END AS deadline_ts,a.grant_id,a.grant_generation,a.run_generation,a.budget_generation,d.nested_tool_id,d.contract_digest,d.dispatch_target FROM workflow_ops.workflow_invocation_t i JOIN workflow_ops.process_info_t p ON p.host_id=i.host_id AND p.process_id=i.process_id JOIN workflow_ops.workflow_action_authority_t a ON a.host_id=i.host_id AND a.run_id=i.workflow_instance_id JOIN workflow_ops.workflow_tool_dependency_t d ON d.host_id=i.host_id AND d.outer_binding_id=i.binding_id WHERE i.host_id=$1 AND i.process_id=$2 AND d.authorization_tool_name=$3 AND d.active AND d.lifecycle_status<>'revoked' AND i.state IN('ACCEPTED','RUNNING','WAITING') AND i.cancel_requested_ts IS NULL AND (i.deadline_ts>clock_timestamp() OR i.response_policy_snapshot->'privateExecutionProfile'->>'version'='1') AND (p.deadline_ts IS NULL OR p.deadline_ts>clock_timestamp()) AND a.active AND a.deadline>clock_timestamp()")
            .bind(host).bind(process).bind(alias).fetch_optional(&self.pool).await?.ok_or_else(denied)?;
        let run: Uuid = row.get("workflow_instance_id");
        let user = row.get::<String, _>("end_user_subject").parse::<Uuid>()?;
        let target: Value = row.get("dispatch_target");
        if target.get("endpoint").and_then(Value::as_str) != Some(self.config.gateway_url.as_str())
        {
            return Err(denied().into());
        }
        let name = target
            .get("toolName")
            .or_else(|| target.get("targetName"))
            .and_then(Value::as_str)
            .ok_or_else(denied)?;
        params
            .as_object_mut()
            .ok_or_else(denied)?
            .insert("name".into(), Value::String(name.to_owned()));
        let bytes = serde_json::to_vec(
            &json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":params}),
        )?;
        if bytes.len() as u64 > self.config.request_byte_limit {
            return Err(denied().into());
        }
        let access_token = if let Some(long) = &self.long {
            if long.binding_for(run, host, user).await? != row.get::<Uuid, _>("grant_id") {
                return Err(denied().into());
            }
            long.token_for(run, host, user).await?
        } else {
            self.broker
                .as_ref()
                .ok_or_else(denied)?
                .renew_for_run(run, host, user)
                .await?
                .access_token
        };
        // The provider has already verified this exact JWT's signature, purpose,
        // issuer, audience, subject and tenant before exposing the credential.
        use base64::Engine as _;
        let payload = access_token.split('.').nth(1).ok_or_else(denied)?;
        let claims: Value = serde_json::from_slice(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload)?,
        )?;
        let tool: Uuid = row.get("nested_tool_id");
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
            contract_digest: row.get("contract_digest"),
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
            deadline: row.get("deadline_ts"),
        };
        if let Some(stored)=sqlx::query_scalar::<_,Value>("SELECT binding FROM workflow_ops.workflow_action_permit_t WHERE host_id=$1 AND run_id=$2 AND attempt_id=$3")
            .bind(host).bind(run).bind(attempt).fetch_optional(&self.pool).await? {
            let previous:Binding=serde_json::from_value(stored)?;binding.action_id=previous.action_id;
            if binding!=previous {return Err(denied().into())}
        }
        let ledger = Ledger::new(self.pool.clone());
        ledger.install_permit(&binding, 10).await?;
        let mut request = self
            .client
            .post(&self.config.gateway_url)
            .bearer_auth(&access_token);
        if let Some(long) = &self.long {
            request = request.header(
                "x-scope-token",
                format!("Bearer {}", long.workload_token().await?),
            );
        } else {
            request = request.header("x-scope-token", &self.scope);
        }
        let mut response = request
            .header("x-workflow-action", binding.action_id.to_string())
            .header("content-type", "application/json")
            .header("accept", "application/json")
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
