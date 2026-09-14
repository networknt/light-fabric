//! Trusted Workflow-to-Gateway action producer. No model-provided identity,
//! target override, depth, budget, or authorization header is accepted here.
use crate::credential_broker::CredentialBroker;
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
    broker: Arc<CredentialBroker>,
    client: reqwest::Client,
    scope: String,
    config: Config,
    agent_services: std::collections::BTreeMap<String, Uuid>,
}
fn denied() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::PermissionDenied,
        "workflow MCP action denied",
    )
}
impl Runtime {
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
            broker,
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
        let row = sqlx::query("SELECT i.workflow_instance_id,i.deadline_ts,i.permit_depth,a.grant_id,a.user_id FROM workflow_ops.workflow_invocation_t i JOIN workflow_ops.workflow_action_authority_t a ON a.host_id=i.host_id AND a.run_id=i.workflow_instance_id WHERE i.host_id=$1 AND i.process_id=$2 AND i.state IN('ACCEPTED','RUNNING','WAITING') AND i.cancel_requested_ts IS NULL AND i.deadline_ts>clock_timestamp() AND a.active AND a.deadline>clock_timestamp() AND a.user_id::text=i.end_user_subject")
            .bind(host).bind(process).fetch_optional(&self.pool).await?.ok_or_else(denied)?;
        let depth: i32 = row.get("permit_depth");
        if depth < 0 || depth > i32::from(self.config.maximum_depth) {
            return Err(denied().into());
        }
        let _grant = self
            .broker
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
        let row=sqlx::query("SELECT i.workflow_instance_id,i.end_user_subject,i.policy_digest,i.response_policy_digest,i.execution_class,i.permit_depth,i.deadline_ts,a.grant_id,a.grant_generation,a.run_generation,a.budget_generation,d.nested_tool_id,d.contract_digest,d.dispatch_target FROM workflow_ops.workflow_invocation_t i JOIN workflow_ops.workflow_action_authority_t a ON a.host_id=i.host_id AND a.run_id=i.workflow_instance_id JOIN workflow_ops.workflow_tool_dependency_t d ON d.host_id=i.host_id AND d.outer_binding_id=i.binding_id WHERE i.host_id=$1 AND i.process_id=$2 AND d.authorization_tool_name=$3 AND d.active AND d.lifecycle_status<>'revoked'")
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
        let credential = self.broker.renew_for_run(run, host, user).await?;
        // The provider has already verified this exact JWT's signature, purpose,
        // issuer, audience, subject and tenant before exposing the credential.
        use base64::Engine as _;
        let payload = credential
            .access_token
            .split('.')
            .nth(1)
            .ok_or_else(denied)?;
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
        let mut response = self
            .client
            .post(&self.config.gateway_url)
            .header(
                "authorization",
                format!("Bearer {}", credential.access_token),
            )
            .header("x-scope-token", &self.scope)
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
