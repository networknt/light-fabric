//! Request-scoped gateway authority. Never serialize or Debug bearer credentials.
use async_trait::async_trait;

#[derive(Debug, thiserror::Error)]
pub enum GatewayCredentialError {
    #[error("authentication_required")]
    AuthenticationRequired,
    #[error("workload_credential_unavailable")]
    WorkloadUnavailable,
}

pub struct GatewayRequestCredentials {
    pub user_token: String,
    pub workload_token: String,
    pub user_expires_at: i64,
    pub workload_expires_at: i64,
}
impl GatewayRequestCredentials {
    pub fn headers(&self) -> anyhow::Result<reqwest::header::HeaderMap> {
        let now = chrono::Utc::now().timestamp();
        if now >= self.user_expires_at {
            return Err(GatewayCredentialError::AuthenticationRequired.into());
        }
        if now >= self.workload_expires_at {
            return Err(GatewayCredentialError::WorkloadUnavailable.into());
        }
        let mut headers = reqwest::header::HeaderMap::new();
        for (name, token) in [
            ("authorization", &self.user_token),
            ("x-scope-token", &self.workload_token),
        ] {
            anyhow::ensure!(
                !token.is_empty() && !token.bytes().any(|c| c.is_ascii_whitespace() || c == b','),
                "invalid gateway credential"
            );
            let mut value = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| anyhow::anyhow!("invalid gateway credential"))?;
            value.set_sensitive(true);
            headers.insert(name, value);
        }
        Ok(headers)
    }
}

#[async_trait]
pub trait GatewayAuthorization: Send + Sync {
    async fn credentials(&self) -> anyhow::Result<GatewayRequestCredentials>;
}
