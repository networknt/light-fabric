//! Fixed A2 receiver authorization client. The result contains only immutable
//! action metadata; user and application credentials are never returned.
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use uuid::Uuid;
use workflow_action::Binding;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    pub base_url: String,
    pub client_identity_file: PathBuf,
    pub ca_file: PathBuf,
    pub scope_token_file: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub host_id: Uuid,
    pub action_id: Uuid,
}

pub struct Client {
    http: reqwest::Client,
    endpoint: String,
    scope: String,
}

impl Client {
    pub async fn new(config: &Config, dir: &Path) -> anyhow::Result<Self> {
        let base = url::Url::parse(&config.base_url)?;
        anyhow::ensure!(
            base.scheme() == "https"
                && base.host_str().is_some()
                && base.username().is_empty()
                && base.password().is_none()
                && base.query().is_none()
                && base.fragment().is_none()
                && base.path() == "/",
            "invalid Workflow receiver endpoint"
        );
        let identity = tokio::fs::read(dir.join(&config.client_identity_file)).await?;
        let ca = tokio::fs::read(dir.join(&config.ca_file)).await?;
        let scope = tokio::fs::read_to_string(dir.join(&config.scope_token_file))
            .await?
            .trim()
            .to_owned();
        anyhow::ensure!(
            scope.strip_prefix("Bearer ").is_some_and(|token| {
                !token.is_empty()
                    && !token
                        .bytes()
                        .any(|byte| byte.is_ascii_whitespace() || byte == b',')
            }),
            "invalid Workflow receiver credential"
        );
        let mut builder = reqwest::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(5))
            .identity(reqwest::Identity::from_pem(&identity)?);
        for certificate in reqwest::Certificate::from_pem_bundle(&ca)? {
            builder = builder.add_root_certificate(certificate);
        }
        Ok(Self {
            http: builder.build()?,
            endpoint: base
                .join("internal/workflow/actions/receiver-authorize")?
                .to_string(),
            scope,
        })
    }

    pub async fn authorize(
        &self,
        host_id: Uuid,
        action_id: Uuid,
        user_authorization: &str,
    ) -> anyhow::Result<Binding> {
        anyhow::ensure!(
            !host_id.is_nil()
                && !action_id.is_nil()
                && user_authorization.starts_with("Bearer ")
                && !user_authorization.contains(['\r', '\n', ',']),
            "invalid Workflow receiver request"
        );
        let response = self
            .http
            .post(&self.endpoint)
            .header("authorization", user_authorization)
            .header("x-scope-token", &self.scope)
            .header("x-workflow-action", action_id.to_string())
            .json(&Request { host_id, action_id })
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("Workflow receiver authorization unavailable"))?;
        anyhow::ensure!(response.status().is_success(), "Workflow receiver denied");
        let bytes = response.bytes().await?;
        anyhow::ensure!(
            bytes.len() <= 32 * 1024,
            "Workflow receiver response too large"
        );
        let binding: Binding = serde_json::from_slice(&bytes)?;
        anyhow::ensure!(
            binding.host_id == host_id && binding.action_id == action_id,
            "Workflow receiver binding mismatch"
        );
        Ok(binding)
    }

    /// Submit only receiver-owned terminal evidence. Workflow accepts it only
    /// for an already uncertain generation and an approved tool registration.
    pub async fn reconcile(&self, receipt: &workflow_action::Reconciliation) -> anyhow::Result<()> {
        anyhow::ensure!(
            !receipt.host_id.is_nil()
                && !receipt.action_id.is_nil()
                && receipt.generation > 0
                && matches!(
                    receipt.outcome,
                    workflow_action::DispatchState::Succeeded
                        | workflow_action::DispatchState::Failed
                )
                && workflow_action::is_digest(&receipt.evidence_digest),
            "invalid Workflow reconciliation receipt"
        );
        let endpoint = url::Url::parse(&self.endpoint)?
            .join("complete")?
            .to_string();
        let response = self
            .http
            .post(endpoint)
            .header("x-scope-token", &self.scope)
            .json(receipt)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("Workflow reconciliation unavailable"))?;
        anyhow::ensure!(
            response.status().is_success(),
            "Workflow reconciliation denied"
        );
        Ok(())
    }
}
