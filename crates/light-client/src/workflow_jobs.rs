//! Fixed, non-credential-disclosing Agent job admission check.
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use uuid::Uuid;
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
pub struct Check {
    pub host_id: Uuid,
    pub job_id: Uuid,
}
pub struct Client {
    http: reqwest::Client,
    endpoint: String,
    scope: String,
}

/// App-token-only Agent bridge through Gateway. The target Workflow client ID
/// selects one backend; this Agent uses its own LONG client for its app token.
pub struct GatewayClient {
    issuer: Arc<crate::long_binding::LongBindingClient>,
    target_workflow_client_id: Uuid,
}

impl GatewayClient {
    pub fn new(
        issuer: Arc<crate::long_binding::LongBindingClient>,
        target_workflow_client_id: Uuid,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !target_workflow_client_id.is_nil(),
            "invalid Workflow target"
        );
        Ok(Self {
            issuer,
            target_workflow_client_id,
        })
    }

    fn request(&self, action: &str) -> anyhow::Result<reqwest::RequestBuilder> {
        Ok(self.issuer.gateway_post(&format!(
            "internal/workflow/{}/jobs/{action}",
            self.target_workflow_client_id,
        ))?)
    }

    pub async fn pending(
        &self,
        host_id: Uuid,
    ) -> anyhow::Result<Vec<crate::workflow_job_transport::JobDelivery>> {
        let app = self.issuer.workload_token().await?;
        let response = self
            .request("poll")?
            .header("X-Scope-Token", format!("Bearer {app}"))
            .json(&crate::workflow_job_transport::Poll { host_id })
            .send()
            .await?;
        anyhow::ensure!(
            response.status() == reqwest::StatusCode::OK,
            "Workflow job polling unavailable"
        );
        let mut response = response;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            anyhow::ensure!(
                bytes.len().saturating_add(chunk.len()) <= 1024 * 1024,
                "Workflow job poll exceeds bound"
            );
            bytes.extend_from_slice(&chunk);
        }
        let deliveries: Vec<crate::workflow_job_transport::JobDelivery> =
            serde_json::from_slice(&bytes)?;
        anyhow::ensure!(
            deliveries.len() <= 4,
            "Workflow job poll exceeds batch bound"
        );
        for delivery in &deliveries {
            delivery.job.validate()?;
            anyhow::ensure!(
                delivery.job.host_id == host_id,
                "Workflow job Host mismatch"
            );
            anyhow::ensure!(
                delivery.job.cancellation_requested
                    || delivery
                        .owner_token
                        .as_deref()
                        .is_some_and(|v| !v.is_empty() && v.len() <= 16_384),
                "Workflow job owner handoff unavailable"
            );
        }
        Ok(deliveries)
    }

    pub async fn authorized(&self, host_id: Uuid, job_id: Uuid) -> anyhow::Result<bool> {
        let app = self.issuer.workload_token().await?;
        let response = self
            .request("authorize")?
            .header("X-Scope-Token", format!("Bearer {app}"))
            .json(&Check { host_id, job_id })
            .send()
            .await?;
        match response.status() {
            reqwest::StatusCode::NO_CONTENT => Ok(true),
            reqwest::StatusCode::FORBIDDEN | reqwest::StatusCode::UNAUTHORIZED => Ok(false),
            _ => Err(anyhow::anyhow!("Workflow job authorization unavailable")),
        }
    }

    pub async fn report(
        &self,
        report: &crate::workflow_job_transport::Report,
    ) -> anyhow::Result<()> {
        let app = self.issuer.workload_token().await?;
        let response = self
            .request("report")?
            .header("X-Scope-Token", format!("Bearer {app}"))
            .json(report)
            .send()
            .await?;
        anyhow::ensure!(
            response.status() == reqwest::StatusCode::NO_CONTENT,
            "Workflow job report unavailable"
        );
        Ok(())
    }
}
impl Client {
    pub async fn pending(
        &self,
        host_id: Uuid,
    ) -> anyhow::Result<Vec<crate::workflow_job_transport::Job>> {
        let response = self
            .http
            .post(format!(
                "{}poll",
                self.endpoint.trim_end_matches("authorize")
            ))
            .header("x-scope-token", &self.scope)
            .json(&crate::workflow_job_transport::Poll { host_id })
            .send()
            .await?;
        anyhow::ensure!(
            response.status() == reqwest::StatusCode::OK,
            "Workflow job polling unavailable"
        );
        let bytes = response.bytes().await?;
        anyhow::ensure!(
            bytes.len() <= 1024 * 1024,
            "Workflow job poll exceeds bound"
        );
        let jobs: Vec<crate::workflow_job_transport::Job> = serde_json::from_slice(&bytes)?;
        anyhow::ensure!(jobs.len() <= 4, "Workflow job poll exceeds batch bound");
        for job in &jobs {
            job.validate()?;
            anyhow::ensure!(job.host_id == host_id, "job Host mismatch");
        }
        Ok(jobs)
    }

    pub async fn report(
        &self,
        report: &crate::workflow_job_transport::Report,
    ) -> anyhow::Result<()> {
        let response = self
            .http
            .post(format!(
                "{}report",
                self.endpoint.trim_end_matches("authorize")
            ))
            .header("x-scope-token", &self.scope)
            .json(report)
            .send()
            .await?;
        anyhow::ensure!(
            response.status() == reqwest::StatusCode::NO_CONTENT,
            "Workflow job report unavailable"
        );
        Ok(())
    }
    pub async fn new(config: &Config, dir: &Path) -> anyhow::Result<Self> {
        let url = url::Url::parse(&config.base_url)?;
        anyhow::ensure!(
            url.scheme() == "https"
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
                && url.path() == "/",
            "invalid Workflow job endpoint"
        );
        let identity = tokio::fs::read(dir.join(&config.client_identity_file)).await?;
        let ca = tokio::fs::read(dir.join(&config.ca_file)).await?;
        let scope = tokio::fs::read_to_string(dir.join(&config.scope_token_file))
            .await?
            .trim()
            .to_owned();
        anyhow::ensure!(
            scope.strip_prefix("Bearer ").is_some_and(
                |s| !s.is_empty() && !s.bytes().any(|b| b.is_ascii_whitespace() || b == b',')
            ),
            "invalid Workflow job credential"
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
            endpoint: url.join("internal/workflow/jobs/authorize")?.to_string(),
            scope,
        })
    }
    pub async fn authorized(&self, host_id: Uuid, job_id: Uuid) -> anyhow::Result<bool> {
        anyhow::ensure!(
            !host_id.is_nil() && !job_id.is_nil(),
            "invalid Workflow job reference"
        );
        let response = self
            .http
            .post(&self.endpoint)
            .header("x-scope-token", &self.scope)
            .json(&Check { host_id, job_id })
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("Workflow job authorization unavailable"))?;
        match response.status() {
            reqwest::StatusCode::NO_CONTENT => Ok(true),
            reqwest::StatusCode::FORBIDDEN | reqwest::StatusCode::UNAUTHORIZED => Ok(false),
            _ => Err(anyhow::anyhow!("Workflow job authorization unavailable")),
        }
    }
}
