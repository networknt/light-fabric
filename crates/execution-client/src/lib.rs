//! Authenticated, bounded client for the Controller-owned execution service.

use std::path::{Path, PathBuf};
use std::time::Duration;

use execution_runner_protocol::{
    CleanupRequestSubmission, ExecutionResultView, SchedulingRequestSubmission,
};
use reqwest::StatusCode;
use serde::Deserialize;
use thiserror::Error;
use url::Url;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("invalid execution service URL: {0}")]
    Url(#[from] url::ParseError),
    #[error("execution service transport failed: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("execution credential is unavailable or unsafe: {0}")]
    Credential(String),
    #[error("execution service rejected the request ({status}): {code}")]
    Rejected { status: StatusCode, code: String },
    #[error("execution service returned an invalid response: {0}")]
    InvalidResponse(#[from] serde_json::Error),
}

#[derive(Clone)]
pub struct ExecutionClient {
    endpoint: Url,
    credential: CredentialSource,
    client: reqwest::Client,
}

#[derive(Clone)]
enum CredentialSource {
    File(PathBuf),
    Token(String),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RequestAccepted {
    request_id: Uuid,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CleanupAccepted {
    cleanup_request_id: Uuid,
}

impl ExecutionClient {
    pub fn new(
        endpoint: &str,
        token_file: impl Into<PathBuf>,
        timeout: Duration,
        root_certificate_pem: Option<&[u8]>,
    ) -> Result<Self, ClientError> {
        let endpoint = Url::parse(endpoint)?;
        if endpoint.scheme() != "https" {
            return Err(ClientError::Credential(
                "execution service requires HTTPS".to_string(),
            ));
        }
        let mut builder = reqwest::Client::builder().timeout(timeout);
        if let Some(pem) = root_certificate_pem {
            builder = builder.add_root_certificate(reqwest::Certificate::from_pem(pem)?);
        }
        Ok(Self {
            endpoint,
            credential: CredentialSource::File(token_file.into()),
            client: builder.build()?,
        })
    }

    pub fn new_with_bearer_token(
        endpoint: &str,
        bearer_token: &str,
        timeout: Duration,
        root_certificate_pem: Option<&[u8]>,
    ) -> Result<Self, ClientError> {
        validate_token(bearer_token)?;
        let endpoint = Url::parse(endpoint)?;
        if endpoint.scheme() != "https" {
            return Err(ClientError::Credential(
                "execution service requires HTTPS".to_string(),
            ));
        }
        let mut builder = reqwest::Client::builder().timeout(timeout);
        if let Some(pem) = root_certificate_pem {
            builder = builder.add_root_certificate(reqwest::Certificate::from_pem(pem)?);
        }
        Ok(Self {
            endpoint,
            credential: CredentialSource::Token(bearer_token.to_string()),
            client: builder.build()?,
        })
    }

    pub async fn submit_request(
        &self,
        request: &SchedulingRequestSubmission,
    ) -> Result<Uuid, ClientError> {
        let response = self
            .client
            .post(self.endpoint.join("internal/execution/requests")?)
            .bearer_auth(self.token()?)
            .json(request)
            .send()
            .await?;
        let body = successful_body(response).await?;
        Ok(serde_json::from_slice::<RequestAccepted>(&body)?.request_id)
    }

    pub async fn pending_results(
        &self,
        limit: u16,
    ) -> Result<Vec<ExecutionResultView>, ClientError> {
        let response = self
            .client
            .get(self.endpoint.join("internal/execution/results")?)
            .query(&[("limit", limit.clamp(1, 1000))])
            .bearer_auth(self.token()?)
            .send()
            .await?;
        let body = successful_body(response).await?;
        Ok(serde_json::from_slice(&body)?)
    }

    pub async fn result(&self, execution_id: Uuid) -> Result<ExecutionResultView, ClientError> {
        let response = self
            .client
            .get(
                self.endpoint
                    .join(&format!("internal/execution/results/{execution_id}"))?,
            )
            .bearer_auth(self.token()?)
            .send()
            .await?;
        let body = successful_body(response).await?;
        Ok(serde_json::from_slice(&body)?)
    }

    pub async fn acknowledge_result(
        &self,
        execution_id: Uuid,
        fencing_token: i64,
    ) -> Result<(), ClientError> {
        let response = self
            .client
            .post(
                self.endpoint
                    .join(&format!("internal/execution/results/{execution_id}/ack"))?,
            )
            .bearer_auth(self.token()?)
            .json(&serde_json::json!({"fencingToken": fencing_token}))
            .send()
            .await?;
        successful_body(response).await?;
        Ok(())
    }

    pub async fn submit_cleanup_request(
        &self,
        request: &CleanupRequestSubmission,
    ) -> Result<Uuid, ClientError> {
        let response = self
            .client
            .post(self.endpoint.join("internal/execution/cleanup-requests")?)
            .bearer_auth(self.token()?)
            .json(request)
            .send()
            .await?;
        let body = successful_body(response).await?;
        Ok(serde_json::from_slice::<CleanupAccepted>(&body)?.cleanup_request_id)
    }

    fn token(&self) -> Result<String, ClientError> {
        match &self.credential {
            CredentialSource::File(path) => read_token(path),
            CredentialSource::Token(token) => Ok(token.clone()),
        }
    }
}

async fn successful_body(response: reqwest::Response) -> Result<Vec<u8>, ClientError> {
    let status = response.status();
    let body = response.bytes().await?.to_vec();
    if status.is_success() {
        return Ok(body);
    }
    let code = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|value| {
            value
                .get("message")
                .or_else(|| value.get("code"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| "EXECUTION_SERVICE_ERROR".to_string());
    Err(ClientError::Rejected { status, code })
}

fn read_token(path: &Path) -> Result<String, ClientError> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| ClientError::Credential(format!("{}: {error}", path.display())))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > 16 * 1024 {
        return Err(ClientError::Credential(format!(
            "{} must be a bounded regular non-symlink file",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o037 != 0 {
            return Err(ClientError::Credential(format!(
                "{} permissions expose the execution credential",
                path.display()
            )));
        }
    }
    let token = std::fs::read_to_string(path)
        .map_err(|error| ClientError::Credential(format!("{}: {error}", path.display())))?;
    let token = token.trim();
    validate_token(token)?;
    Ok(token.to_string())
}

fn validate_token(token: &str) -> Result<(), ClientError> {
    if token.len() < 16 || token.contains(char::is_whitespace) {
        return Err(ClientError::Credential(
            "execution credential has an invalid shape".to_string(),
        ));
    }
    Ok(())
}
