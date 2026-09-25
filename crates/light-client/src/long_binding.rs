//! Gateway-only issuer transport for LONG work bindings. The calling app owns
//! its encrypted source-token store and durable work lifecycle.
use crate::config::{OAuthTokenConfig, OAuthWorkflowLongConfig};
use serde::{Deserialize, de::DeserializeOwned};
use std::{path::Path, sync::Arc, time::Duration};
use uuid::Uuid;
use zeroize::Zeroizing;

const SCOPE: &str = "portal.r portal.w";
const TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:access_token";
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum LongClientError {
    #[error("LONG issuer configuration or response rejected")]
    Evidence,
    #[error("LONG issuer temporarily unavailable")]
    Retryable,
    #[error("LONG issuer denied the binding or client")]
    Denied,
    #[error("LONG credential store unavailable")]
    Store,
}

/// Loaded only for an on-demand exchange, then wiped when it leaves scope.
/// Store implementations must encrypt this bearer at rest and fence terminal
/// work before returning it.
pub struct StoredLongBinding {
    pub binding_id: Uuid,
    pub work_id: Uuid,
    pub host_id: Uuid,
    pub owner_user_id: Uuid,
    pub source_token: Zeroizing<String>,
}

#[async_trait::async_trait]
pub trait LongBindingStore: Send + Sync {
    async fn active(
        &self,
        work: Uuid,
        host: Uuid,
        owner: Uuid,
    ) -> Result<Option<StoredLongBinding>, LongClientError>;
}

/// Generic on-demand broker. It owns no persisted bearer or lifecycle state;
/// each app supplies a restricted encrypted store for its own work items.
pub struct LongCredentialBroker<S: LongBindingStore> {
    client: Arc<LongBindingClient>,
    store: Arc<S>,
}

impl<S: LongBindingStore> LongCredentialBroker<S> {
    pub fn new(client: Arc<LongBindingClient>, store: Arc<S>) -> Self {
        Self { client, store }
    }

    pub async fn for_gateway(
        &self,
        work: Uuid,
        host: Uuid,
        owner: Uuid,
    ) -> Result<GatewayLongCredentials, LongClientError> {
        let row = self
            .store
            .active(work, host, owner)
            .await?
            .ok_or(LongClientError::Denied)?;
        if row.work_id != work || row.host_id != host || row.owner_user_id != owner {
            return Err(LongClientError::Evidence);
        }
        self.client
            .credentials_for_work(row.binding_id, &row.source_token)
            .await
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BindingResponse {
    pub binding_id: Uuid,
    pub workflow_instance_id: Uuid,
    pub work_id: Option<Uuid>,
    pub owner_user_id: Uuid,
    pub host_id: Uuid,
    pub state: String,
    pub version: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StateResponse {
    pub binding_id: Uuid,
    pub state: String,
    pub version: i64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    token_type: String,
    expires_in: u64,
    scope: Option<String>,
}

#[derive(Clone)]
pub struct LongBindingClient {
    http: reqwest::Client,
    gateway: url::Url,
    provider_id: String,
    client_id: String,
    client_secret: String,
    scope: String,
}

/// Short-lived Gateway credentials for one outbound request. Deliberately has
/// no `Debug` or serialization implementation so bearer values are not logged.
pub struct GatewayLongCredentials {
    authorization: reqwest::header::HeaderValue,
    scope_token: reqwest::header::HeaderValue,
}

impl GatewayLongCredentials {
    pub fn apply(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request
            .header(reqwest::header::AUTHORIZATION, self.authorization.clone())
            .header("X-Scope-Token", self.scope_token.clone())
    }

    fn new(owner: &str, app: &str) -> Result<Self, LongClientError> {
        let value = |token: &str| {
            if token.is_empty() || token.contains(['\r', '\n']) {
                return Err(LongClientError::Evidence);
            }
            reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
                .map_err(|_| LongClientError::Evidence)
        };
        Ok(Self {
            authorization: value(owner)?,
            scope_token: value(app)?,
        })
    }
}

impl LongBindingClient {
    /// Agent/A2A apps may reuse the existing token-exchange client credentials
    /// in client.yml. Dedicated LONG credentials take precedence for legacy
    /// Workflow deployments. A static subjectToken is never accepted here.
    pub async fn from_token_config(
        token: &OAuthTokenConfig,
        dir: &Path,
    ) -> Result<Self, LongClientError> {
        if token.token_exchange.subject_token.is_some() {
            return Err(LongClientError::Evidence);
        }
        let mut config = token.workflow_long.clone();
        if config.client_id.is_empty() && config.client_secret.is_empty() {
            config.client_id = token.token_exchange.client_id.clone();
            config.client_secret = token.token_exchange.client_secret.clone();
        }
        Self::from_config(&config, dir)
            .await?
            .with_scope(&token.token_exchange.scope)
    }

    /// Exchanges this app's binding just before a Gateway call and obtains a
    /// separate app token. The caller loads the encrypted source from its own
    /// store; no token is retained by this client after the request.
    pub async fn credentials_for_work(
        &self,
        binding: Uuid,
        source: &str,
    ) -> Result<GatewayLongCredentials, LongClientError> {
        let owner = self.exchange_work(binding, source).await?;
        let app = self.workload_token().await?;
        GatewayLongCredentials::new(&owner, &app)
    }

    /// The `long` and legacy `workflow_long` client.yml sections share this
    /// connection shape. The source bearer is passed only to each call.
    pub async fn from_config(
        config: &OAuthWorkflowLongConfig,
        dir: &Path,
    ) -> Result<Self, LongClientError> {
        Self::from_workflow_config(config, dir).await
    }

    pub async fn from_workflow_config(
        config: &OAuthWorkflowLongConfig,
        dir: &Path,
    ) -> Result<Self, LongClientError> {
        if config.client_id.is_empty()
            || config.client_secret.is_empty()
            || config.provider_id.is_empty()
        {
            return Err(LongClientError::Evidence);
        }
        let gateway =
            url::Url::parse(&config.gateway_url).map_err(|_| LongClientError::Evidence)?;
        if gateway.scheme() != "https"
            || gateway.host_str().is_none()
            || !gateway.username().is_empty()
            || gateway.password().is_some()
            || gateway.query().is_some()
            || gateway.fragment().is_some()
            || gateway.path() != "/"
        {
            return Err(LongClientError::Evidence);
        }
        let mut builder = reqwest::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(15));
        if !config.ca_file.is_empty() {
            let ca = tokio::fs::read(dir.join(&config.ca_file))
                .await
                .map_err(|_| LongClientError::Evidence)?;
            for cert in
                reqwest::Certificate::from_pem_bundle(&ca).map_err(|_| LongClientError::Evidence)?
            {
                builder = builder.add_root_certificate(cert);
            }
        }
        Ok(Self {
            http: builder.build().map_err(|_| LongClientError::Evidence)?,
            gateway,
            provider_id: config.provider_id.clone(),
            client_id: config.client_id.clone(),
            client_secret: config.client_secret.clone(),
            scope: SCOPE.to_string(),
        })
    }

    pub fn with_scope(mut self, scopes: &[String]) -> Result<Self, LongClientError> {
        if !scopes.is_empty() {
            if scopes
                .iter()
                .any(|scope| scope.is_empty() || scope.chars().any(char::is_whitespace))
            {
                return Err(LongClientError::Evidence);
            }
            self.scope = scopes.join(" ");
        }
        Ok(self)
    }

    pub fn gateway_origin(&self) -> url::Origin {
        self.gateway.origin()
    }
    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    pub fn gateway_post(&self, path: &str) -> Result<reqwest::RequestBuilder, LongClientError> {
        if path.starts_with('/') || path.contains("..") || path.contains(['?', '#']) {
            return Err(LongClientError::Evidence);
        }
        let url = self
            .gateway
            .join(path)
            .map_err(|_| LongClientError::Evidence)?;
        if url.origin() != self.gateway.origin() {
            return Err(LongClientError::Evidence);
        }
        Ok(self.http.post(url))
    }

    fn endpoint(&self, suffix: &str) -> Result<url::Url, LongClientError> {
        self.gateway
            .join(&format!("oauth2/{}/{}", self.provider_id, suffix))
            .map_err(|_| LongClientError::Evidence)
    }

    fn request(&self, url: url::Url) -> reqwest::RequestBuilder {
        self.http
            .post(url)
            .basic_auth(&self.client_id, Some(&self.client_secret))
    }

    async fn response(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, LongClientError> {
        let response = request
            .send()
            .await
            .map_err(|_| LongClientError::Retryable)?;
        if response.status().is_server_error()
            || matches!(
                response.status(),
                reqwest::StatusCode::REQUEST_TIMEOUT | reqwest::StatusCode::TOO_MANY_REQUESTS
            )
        {
            return Err(LongClientError::Retryable);
        }
        if !response.status().is_success() {
            return Err(LongClientError::Denied);
        }
        Ok(response)
    }

    async fn response_json<T: DeserializeOwned>(
        response: reqwest::Response,
    ) -> Result<T, LongClientError> {
        if response
            .content_length()
            .is_some_and(|len| len > MAX_RESPONSE_BYTES as u64)
        {
            return Err(LongClientError::Evidence);
        }
        let mut body = Vec::new();
        let mut response = response;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| LongClientError::Retryable)?
        {
            if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err(LongClientError::Evidence);
            }
            body.extend_from_slice(&chunk);
        }
        Self::parse_response_body(&body)
    }

    fn parse_response_body<T: DeserializeOwned>(body: &[u8]) -> Result<T, LongClientError> {
        if body.len() > MAX_RESPONSE_BYTES {
            return Err(LongClientError::Evidence);
        }
        serde_json::from_slice(body).map_err(|_| LongClientError::Retryable)
    }

    pub async fn workload_token(&self) -> Result<String, LongClientError> {
        let response = self
            .response(
                self.request(self.endpoint("token")?)
                    .form(&[("grant_type", "client_credentials"), ("scope", &self.scope)]),
            )
            .await?;
        let token: TokenResponse = Self::response_json(response).await?;
        if token.token_type != "Bearer" || token.expires_in == 0 || token.access_token.is_empty() {
            return Err(LongClientError::Evidence);
        }
        Ok(token.access_token)
    }

    pub async fn register_workflow(
        &self,
        run: Uuid,
        host: Uuid,
        source: &str,
        key: &str,
    ) -> Result<BindingResponse, LongClientError> {
        if source.is_empty() || source.len() > 16_384 || key.len() < 32 || key.len() > 256 {
            return Err(LongClientError::Evidence);
        }
        let response = self
            .response(self.request(self.endpoint("workflow/bindings")?).json(
                &serde_json::json!({"workflowInstanceId":run,"hostId":host,
                "subjectToken":source,"subjectTokenType":TOKEN_TYPE,"scope":self.scope,
                "registrationKey":key}),
            ))
            .await?;
        Self::response_json(response).await
    }

    pub async fn register_work(
        &self,
        work: Uuid,
        host: Uuid,
        source: &str,
        key: &str,
    ) -> Result<BindingResponse, LongClientError> {
        if work.is_nil()
            || host.is_nil()
            || source.is_empty()
            || source.len() > 16_384
            || key.len() < 32
            || key.len() > 256
        {
            return Err(LongClientError::Evidence);
        }
        let response = self
            .response(self.request(self.endpoint("long/v1/bindings")?).json(
                &serde_json::json!({"workId":work,"hostId":host,
                "subjectToken":source,"subjectTokenType":TOKEN_TYPE,"scope":self.scope,
                "registrationKey":key}),
            ))
            .await?;
        let binding: BindingResponse = Self::response_json(response).await?;
        if binding.work_id != Some(work)
            || binding.host_id != host
            || !matches!(binding.state.as_str(), "PENDING" | "ACTIVE")
        {
            return Err(LongClientError::Evidence);
        }
        Ok(binding)
    }

    pub async fn activate_workflow(
        &self,
        binding: Uuid,
        run: Uuid,
        version: i64,
        acceptance_digest: &str,
    ) -> Result<StateResponse, LongClientError> {
        let response = self
            .response(
                self.request(self.endpoint(&format!("workflow/bindings/{binding}/activate"))?)
                    .json(
                        &serde_json::json!({"workflowInstanceId":run,"registrationVersion":version,
                "acceptanceReceiptDigest":acceptance_digest}),
                    ),
            )
            .await?;
        Self::response_json(response).await
    }

    pub async fn activate_work(
        &self,
        binding: Uuid,
        work: Uuid,
        version: i64,
        acceptance_digest: &str,
    ) -> Result<StateResponse, LongClientError> {
        let response = self
            .response(
                self.request(self.endpoint(&format!("long/v1/bindings/{binding}/activate"))?)
                    .json(
                        &serde_json::json!({"workId":work,"registrationVersion":version,
            "acceptanceReceiptDigest":acceptance_digest}),
                    ),
            )
            .await?;
        Self::response_json(response).await
    }

    pub async fn exchange(&self, binding: Uuid, source: &str) -> Result<String, LongClientError> {
        self.exchange_with_field("workflow_binding_id", binding, source)
            .await
    }

    pub async fn exchange_work(
        &self,
        binding: Uuid,
        source: &str,
    ) -> Result<String, LongClientError> {
        self.exchange_with_field("long_binding_id", binding, source)
            .await
    }

    async fn exchange_with_field(
        &self,
        field: &str,
        binding: Uuid,
        source: &str,
    ) -> Result<String, LongClientError> {
        let response = self
            .response(self.request(self.endpoint("token")?).form(&[
                (
                    "grant_type",
                    "urn:ietf:params:oauth:grant-type:token-exchange",
                ),
                ("subject_token", source),
                ("subject_token_type", TOKEN_TYPE),
                ("requested_token_type", TOKEN_TYPE),
                (field, &binding.to_string()),
                ("scope", &self.scope),
            ]))
            .await?;
        let token: TokenResponse = Self::response_json(response).await?;
        if token.token_type != "Bearer"
            || token.scope.as_deref() != Some(self.scope.as_str())
            || token.expires_in == 0
            || token.access_token.is_empty()
        {
            return Err(LongClientError::Evidence);
        }
        Ok(token.access_token)
    }

    pub async fn close_workflow(
        &self,
        binding: Uuid,
        run: Uuid,
        close_id: Uuid,
        terminal_state: &str,
        terminal_version: i64,
    ) -> Result<StateResponse, LongClientError> {
        let response = self
            .response(
                self.request(self.endpoint(&format!("workflow/bindings/{binding}/close"))?)
                    .json(
                        &serde_json::json!({"closeId":close_id,"workflowInstanceId":run,
                "terminalState":terminal_state,"terminalVersion":terminal_version}),
                    ),
            )
            .await?;
        Self::response_json(response).await
    }

    pub async fn close_work(
        &self,
        binding: Uuid,
        work: Uuid,
        close_id: Uuid,
        terminal_state: &str,
        terminal_version: i64,
    ) -> Result<StateResponse, LongClientError> {
        let response = self
            .response(
                self.request(self.endpoint(&format!("long/v1/bindings/{binding}/close"))?)
                    .json(&serde_json::json!({"closeId":close_id,"workId":work,
            "terminalState":terminal_state,"terminalVersion":terminal_version})),
            )
            .await?;
        Self::response_json(response).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeStore(Option<StoredLongBinding>);

    #[async_trait::async_trait]
    impl LongBindingStore for FakeStore {
        async fn active(
            &self,
            _: Uuid,
            _: Uuid,
            _: Uuid,
        ) -> Result<Option<StoredLongBinding>, LongClientError> {
            Ok(self.0.as_ref().map(|row| StoredLongBinding {
                binding_id: row.binding_id,
                work_id: row.work_id,
                host_id: row.host_id,
                owner_user_id: row.owner_user_id,
                source_token: row.source_token.clone(),
            }))
        }
    }

    #[tokio::test]
    async fn rejects_direct_issuer_and_missing_client_credentials() {
        let mut config = OAuthWorkflowLongConfig::default();
        config.gateway_url = "http://issuer.internal/".into();
        config.provider_id = "provider".into();
        config.client_id = "client".into();
        config.client_secret = "secret".into();
        assert!(
            LongBindingClient::from_workflow_config(&config, Path::new("/tmp"))
                .await
                .is_err()
        );
        config.gateway_url = "https://gateway.example/".into();
        config.client_secret.clear();
        assert!(
            LongBindingClient::from_workflow_config(&config, Path::new("/tmp"))
                .await
                .is_err()
        );
    }

    #[test]
    fn rejects_oversized_issuer_response() {
        let oversized = vec![b' '; MAX_RESPONSE_BYTES + 1];
        assert!(matches!(
            LongBindingClient::parse_response_body::<TokenResponse>(&oversized),
            Err(LongClientError::Evidence)
        ));
    }

    #[test]
    fn generic_long_section_reuses_client_configuration() {
        let config: crate::config::OAuthTokenConfig = serde_json::from_value(
            serde_json::json!({"long":{"gateway_url":"https://gateway.example/",
                "provider_id":"portal","client_id":"agent","client_secret":"top-secret-value"}}),
        )
        .unwrap();
        assert_eq!(config.workflow_long.client_id, "agent");
        assert!(!format!("{config:?}").contains("top-secret-value"));
    }

    #[tokio::test]
    async fn long_client_uses_existing_token_exchange_credentials_without_static_source() {
        let config: crate::config::OAuthTokenConfig = serde_json::from_value(serde_json::json!({
            "long": {"gateway_url":"https://gateway.example/", "provider_id":"portal"},
            "token_exchange": {"client_id":"agent", "client_secret":"secret", "scope":["workflow.run", "portal.r"]}
        }))
        .unwrap();
        let client = LongBindingClient::from_token_config(&config, Path::new("/tmp"))
            .await
            .unwrap();
        assert_eq!(client.client_id(), "agent");
        assert_eq!(client.scope, "workflow.run portal.r");

        let mut static_source = config;
        static_source.token_exchange.subject_token = Some("stale-source".into());
        assert!(matches!(
            LongBindingClient::from_token_config(&static_source, Path::new("/tmp")).await,
            Err(LongClientError::Evidence)
        ));
    }

    #[test]
    fn outbound_gateway_credentials_pair_owner_and_app_tokens() {
        let credentials = GatewayLongCredentials::new("owner-token", "app-token").unwrap();
        let request = credentials
            .apply(reqwest::Client::new().get("https://gateway.example/mcp"))
            .build()
            .unwrap();
        assert_eq!(
            request.headers()[reqwest::header::AUTHORIZATION],
            "Bearer owner-token"
        );
        assert_eq!(request.headers()["X-Scope-Token"], "Bearer app-token");
        assert!(GatewayLongCredentials::new("owner\nother", "app").is_err());
    }

    #[tokio::test]
    async fn broker_refuses_missing_or_mismatched_work_before_exchange() {
        let mut config = OAuthWorkflowLongConfig::default();
        config.gateway_url = "https://gateway.example/".into();
        config.provider_id = "portal".into();
        config.client_id = "agent".into();
        config.client_secret = "secret".into();
        let client = Arc::new(
            LongBindingClient::from_config(&config, Path::new("/tmp"))
                .await
                .unwrap(),
        );
        let work = Uuid::new_v4();
        let host = Uuid::new_v4();
        let owner = Uuid::new_v4();
        let absent = LongCredentialBroker::new(client.clone(), Arc::new(FakeStore(None)));
        assert!(matches!(
            absent.for_gateway(work, host, owner).await,
            Err(LongClientError::Denied)
        ));
        let mismatch = LongCredentialBroker::new(
            client,
            Arc::new(FakeStore(Some(StoredLongBinding {
                binding_id: Uuid::new_v4(),
                work_id: Uuid::new_v4(),
                host_id: host,
                owner_user_id: owner,
                source_token: Zeroizing::new("source".into()),
            }))),
        );
        assert!(matches!(
            mismatch.for_gateway(work, host, owner).await,
            Err(LongClientError::Evidence)
        ));
    }
}
