//! Fixed, non-credential-disclosing Agent job admission check.
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
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
impl Client {
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
