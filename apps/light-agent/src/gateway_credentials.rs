//! Renewable workload credentials are shared; user credentials belong to one turn.
use agent_runtime_protocol::gateway_delegation::DualTokenPolicy;
use light_security::{AuthPrincipal, JwtExpiryMode, SecurityRuntime, verify_jwt_token};
use model_provider::gateway_authorization::{
    GatewayAuthorization, GatewayCredentialError, GatewayRequestCredentials,
};
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Clone)]
struct Token {
    value: String,
    expires_at: i64,
}
#[derive(Default)]
struct Cache {
    token: Option<Token>,
    retry_at: i64,
}

pub struct WorkloadCredentials {
    policy: DualTokenPolicy,
    client: reqwest::Client,
    security: Arc<SecurityRuntime>,
    host: String,
    environment: String,
    cache: Mutex<Cache>,
}

pub fn check_claims(
    claims: &serde_json::Value,
    issuer: &str,
    audience: &str,
    now: i64,
) -> anyhow::Result<i64> {
    let exp = claims.get("exp").and_then(|v| v.as_i64()).unwrap_or(0);
    let aud = claims.get("aud").is_some_and(|v| {
        v.as_str() == Some(audience)
            || v.as_array()
                .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(audience)))
    });
    anyhow::ensure!(
        claims.get("iss").and_then(|v| v.as_str()) == Some(issuer)
            && aud
            && exp > now
            && !claims
                .get("nbf")
                .is_some_and(|v| v.as_i64().is_none_or(|n| n > now))
            && !claims
                .get("iat")
                .is_some_and(|v| v.as_i64().is_none_or(|n| n > now)),
        "credential profile invalid"
    );
    Ok(exp)
}

impl WorkloadCredentials {
    pub fn new(
        policy: DualTokenPolicy,
        client: reqwest::Client,
        security: Arc<SecurityRuntime>,
        host: String,
        environment: String,
    ) -> Self {
        Self {
            policy,
            client,
            security,
            host,
            environment,
            cache: Mutex::new(Cache::default()),
        }
    }
    async fn acquire(&self) -> anyhow::Result<Token> {
        // Reopen the mounted file on every grant to support atomic secret rotation.
        let secret = tokio::fs::read_to_string(&self.policy.client_secret_file)
            .await
            .map_err(|_| GatewayCredentialError::WorkloadUnavailable)?;
        anyhow::ensure!(
            !secret.trim().is_empty() && secret.len() <= 16384,
            "workload credential unavailable"
        );
        let response = self
            .client
            .post(&self.policy.token_endpoint)
            .basic_auth(self.policy.client_id.to_string(), Some(secret.trim()))
            .form(&[
                ("grant_type", "client_credentials"),
                ("scope", &self.policy.scopes.join(" ")),
            ])
            .send()
            .await
            .map_err(|_| GatewayCredentialError::WorkloadUnavailable)?;
        anyhow::ensure!(
            response.status().is_success(),
            "workload credential unavailable"
        );
        // Bound issuer responses; never propagate response bodies into errors/logs.
        let mut response = response;
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| GatewayCredentialError::WorkloadUnavailable)?
        {
            anyhow::ensure!(
                bytes.len() + chunk.len() <= 65536,
                "workload credential unavailable"
            );
            bytes.extend_from_slice(&chunk);
        }
        let body: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|_| GatewayCredentialError::WorkloadUnavailable)?;
        anyhow::ensure!(
            body["token_type"]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case("bearer")),
            "workload credential unavailable"
        );
        let value = body["access_token"]
            .as_str()
            .ok_or(GatewayCredentialError::WorkloadUnavailable)?;
        let principal = verify_jwt_token(&self.security, value, JwtExpiryMode::Enforce)
            .await
            .map_err(|_| GatewayCredentialError::WorkloadUnavailable)?;
        let expires_at = self.validate_workload(&principal, chrono::Utc::now().timestamp())?;
        Ok(Token {
            value: value.to_owned(),
            expires_at,
        })
    }
    fn validate_workload(&self, principal: &AuthPrincipal, now: i64) -> anyhow::Result<i64> {
        let p = &self.policy;
        let exp = check_claims(
            &principal.claims,
            &p.workload_issuer,
            &p.workload_audience,
            now,
        )?;
        let claims = &principal.claims;
        let environment = claims
            .get("env")
            .or_else(|| claims.get("environment"))
            .and_then(|v| v.as_str());
        anyhow::ensure!(
            principal.client_id.as_deref() == Some(p.client_id.to_string().as_str())
                && principal.host.as_deref() == Some(self.host.as_str())
                && environment == Some(self.environment.as_str()),
            "workload identity invalid"
        );
        anyhow::ensure!(
            !claims
                .get("routeAlias")
                .is_some_and(|v| v.as_str() != Some(p.route_alias.as_str())),
            "workload alias invalid"
        );
        let scopes: Vec<&str> = match claims.get("scp") {
            Some(serde_json::Value::Array(values)) => {
                values.iter().filter_map(|v| v.as_str()).collect()
            }
            _ => claims
                .get("scope")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .split_whitespace()
                .collect(),
        };
        anyhow::ensure!(
            p.scopes.iter().all(|s| scopes.contains(&s.as_str())),
            "workload scope invalid"
        );
        anyhow::ensure!(
            exp > now + i64::from(p.refresh_before_seconds),
            "workload lifetime too short"
        );
        Ok(exp)
    }
    async fn token(&self) -> anyhow::Result<Token> {
        // Serialize refresh, including failed grants, with bounded retry backoff.
        let mut cache = self.cache.lock().await;
        let now = chrono::Utc::now().timestamp();
        if let Some(token) = &cache.token {
            if token.expires_at > now + i64::from(self.policy.refresh_before_seconds) {
                return Ok(token.clone());
            }
        }
        if now >= cache.retry_at {
            match self.acquire().await {
                Ok(token) => {
                    cache.token = Some(token);
                    cache.retry_at = 0;
                }
                Err(_) => {
                    let now = chrono::Utc::now().timestamp();
                    cache.retry_at = cache
                        .token
                        .as_ref()
                        .filter(|t| t.expires_at > now)
                        .map_or(now + 2, |t| (now + 2).min(t.expires_at));
                }
            }
        }
        cache
            .token
            .as_ref()
            .filter(|t| t.expires_at > chrono::Utc::now().timestamp())
            .cloned()
            .ok_or_else(|| GatewayCredentialError::WorkloadUnavailable.into())
    }
    pub fn for_turn(
        self: &Arc<Self>,
        authorization: &str,
        claims: &serde_json::Value,
    ) -> anyhow::Result<Arc<dyn GatewayAuthorization>> {
        let expires_at = check_claims(
            claims,
            &self.policy.user_issuer,
            &self.policy.user_audience,
            chrono::Utc::now().timestamp(),
        )
        .map_err(|_| GatewayCredentialError::AuthenticationRequired)?;
        let token = authorization
            .strip_prefix("Bearer ")
            .ok_or(GatewayCredentialError::AuthenticationRequired)?;
        Ok(Arc::new(TurnAuthority {
            user: Token {
                value: token.into(),
                expires_at,
            },
            workload: self.clone(),
        }))
    }
}
struct TurnAuthority {
    user: Token,
    workload: Arc<WorkloadCredentials>,
}
#[async_trait::async_trait]
impl GatewayAuthorization for TurnAuthority {
    async fn credentials(&self) -> anyhow::Result<GatewayRequestCredentials> {
        if self.user.expires_at <= chrono::Utc::now().timestamp() {
            return Err(GatewayCredentialError::AuthenticationRequired.into());
        }
        let workload = self.workload.token().await?;
        Ok(GatewayRequestCredentials {
            user_token: self.user.value.clone(),
            workload_token: workload.value,
            user_expires_at: self.user.expires_at,
            workload_expires_at: workload.expires_at,
        })
    }
}

#[cfg(test)]
mod tests {
    include!("gateway_credentials_tests.rs");
}
