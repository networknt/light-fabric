//! Fixed, mTLS-only client for the local Workflow issuer profile.
//! Never logs or implements Debug for request/response credential material.
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use oauth_workflow_contract::{IssuerGrant, TokenUse};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{path::Path, time::Duration};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UnattendedProviderConfig {
    pub authorization_url: String,
    pub token_url: String,
    pub enrollment_url: String,
    pub grant_url_prefix: String,
    pub jwks_url: String,
    pub client_id: String,
    pub issuer: String,
    pub audience: String,
    pub client_identity_file: String,
    pub ca_file: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderFailure {
    /// No authorization/credential response can be used. Do not replay rotation.
    Uncertain,
    /// The token POST was never sent; a new caller attempt may reuse its credential.
    NotSent,
    ReauthorizationRequired,
    Configuration,
}
impl std::fmt::Display for ProviderFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unattended provider: {self:?}")
    }
}
impl std::error::Error for ProviderFailure {}

#[derive(Deserialize)]
pub struct RenewedCredential {
    /// Taken from the verified JWT, never calculated from response arrival time.
    #[serde(skip)]
    pub access_expires_at: i64,
    pub access_token: String,
    pub refresh_token: String,
    pub token_type: String,
    pub expires_in: u64,
    pub scope: String,
    pub issuer_grant: IssuerGrant,
}

#[derive(Deserialize)]
pub struct GrantStatus {
    pub active: bool,
    pub grant: IssuerGrant,
}

pub struct UnattendedProvider {
    config: UnattendedProviderConfig,
    client: reqwest::Client,
    current_roles_url: url::Url,
    jwks: tokio::sync::Mutex<Option<(std::time::Instant, jsonwebtoken::jwk::JwkSet)>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CurrentWorkflowRoles {
    pub host_id: uuid::Uuid,
    pub user_id: uuid::Uuid,
    pub current_role_ids: Vec<String>,
    pub checked_at: String,
    pub authority: String,
}

impl UnattendedProvider {
    pub async fn new(
        config: UnattendedProviderConfig,
        dir: &Path,
    ) -> Result<Self, ProviderFailure> {
        let token =
            url::Url::parse(&config.token_url).map_err(|_| ProviderFailure::Configuration)?;
        for endpoint in [
            &config.authorization_url,
            &config.token_url,
            &config.enrollment_url,
            &config.grant_url_prefix,
            &config.jwks_url,
        ] {
            let url = url::Url::parse(endpoint).map_err(|_| ProviderFailure::Configuration)?;
            if url.scheme() != "https"
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.fragment().is_some()
                || url.query().is_some()
                || (endpoint != &config.jwks_url
                    && endpoint != &config.authorization_url
                    && url.origin() != token.origin())
            {
                return Err(ProviderFailure::Configuration);
            }
        }
        if config.issuer.is_empty() || config.audience.is_empty() || config.client_id.is_empty() {
            return Err(ProviderFailure::Configuration);
        }
        let current_roles_url = role_authority_url(&config.enrollment_url, &config.client_id)?;
        let identity = tokio::fs::read(dir.join(&config.client_identity_file))
            .await
            .map_err(|_| ProviderFailure::Configuration)?;
        let identity =
            reqwest::Identity::from_pem(&identity).map_err(|_| ProviderFailure::Configuration)?;
        let ca = tokio::fs::read(dir.join(&config.ca_file))
            .await
            .map_err(|_| ProviderFailure::Configuration)?;
        let mut builder = reqwest::Client::builder()
            .https_only(true)
            .tls_built_in_root_certs(false)
            .identity(identity)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10));
        let mut roots = 0;
        for cert in rustls_pemfile::certs(&mut ca.as_slice()) {
            let cert = cert.map_err(|_| ProviderFailure::Configuration)?;
            builder = builder.add_root_certificate(
                reqwest::Certificate::from_der(cert.as_ref())
                    .map_err(|_| ProviderFailure::Configuration)?,
            );
            roots += 1;
        }
        if roots == 0 {
            return Err(ProviderFailure::Configuration);
        }
        Ok(Self {
            config,
            current_roles_url,
            jwks: tokio::sync::Mutex::new(None),
            client: builder
                .build()
                .map_err(|_| ProviderFailure::Configuration)?,
        })
    }

    pub fn client_id(&self) -> &str {
        &self.config.client_id
    }

    /// One mTLS issuer round trip per authority decision. Never reuse token role claims.
    pub async fn current_workflow_roles(
        &self,
        user_authorization: &str,
    ) -> Result<CurrentWorkflowRoles, ProviderFailure> {
        let response = self
            .client
            .post(self.current_roles_url.clone())
            .header(reqwest::header::AUTHORIZATION, user_authorization)
            .header("X-Workflow-Service-Id", "light-workflow")
            .header("X-Workflow-Role-Audience", "portal-workflow-authority")
            .json(&serde_json::json!({}))
            .send()
            .await
            .map_err(|_| ProviderFailure::Uncertain)?;
        if response.status() == reqwest::StatusCode::BAD_REQUEST
            || response.status() == reqwest::StatusCode::UNAUTHORIZED
            || response.status() == reqwest::StatusCode::FORBIDDEN
        {
            return Err(ProviderFailure::ReauthorizationRequired);
        }
        if response.status() != reqwest::StatusCode::OK {
            return Err(ProviderFailure::Uncertain);
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|_| ProviderFailure::Uncertain)?;
        if bytes.len() > 65_536 {
            return Err(ProviderFailure::Uncertain);
        }
        serde_json::from_slice(&bytes).map_err(|_| ProviderFailure::Uncertain)
    }

    pub fn authorization_url(&self, enrollment: uuid::Uuid, state: &str) -> String {
        let mut url =
            url::Url::parse(&self.config.authorization_url).expect("validated provider URL");
        url.query_pairs_mut()
            .append_pair("enrollmentId", &enrollment.to_string())
            .append_pair("state", state);
        url.into()
    }

    pub async fn begin_enrollment(
        &self,
        user_authorization: &str,
        request: &Value,
    ) -> Result<(), ProviderFailure> {
        let response = self
            .client
            .post(&self.config.enrollment_url)
            .header(reqwest::header::AUTHORIZATION, user_authorization)
            .json(request)
            .send()
            .await
            .map_err(|_| ProviderFailure::Uncertain)?;
        checked(response).await?;
        Ok(())
    }

    pub async fn redeem(
        &self,
        code: &str,
        verifier: &str,
        callback: &str,
    ) -> Result<RenewedCredential, ProviderFailure> {
        self.token(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", callback),
            ("client_id", self.client_id()),
        ])
        .await
    }

    /// Obtain a one-time PKCE code over the registered mTLS connection using
    /// existing issuer-recorded user authorization. Never redirect a browser.
    pub async fn acquire_code(
        &self,
        authorization: &str,
        request: &Value,
    ) -> Result<String, ProviderFailure> {
        let response = self
            .client
            .post(format!(
                "{}/acquire",
                self.config.enrollment_url.trim_end_matches('/')
            ))
            .header(reqwest::header::AUTHORIZATION, authorization)
            .json(request)
            .send()
            .await
            .map_err(|_| ProviderFailure::Uncertain)?;
        let body = bounded_body(checked(response).await?).await?;
        let response: Value =
            serde_json::from_slice(&body).map_err(|_| ProviderFailure::Uncertain)?;
        if response.get("enrollmentId") != request.get("enrollmentId") {
            return Err(ProviderFailure::Uncertain);
        }
        response
            .get("authorizationCode")
            .and_then(Value::as_str)
            .filter(|code| {
                !code.is_empty() && code.len() <= 256 && !code.chars().any(char::is_control)
            })
            .map(str::to_owned)
            .ok_or(ProviderFailure::Uncertain)
    }

    pub async fn refresh(&self, refresh: &str) -> Result<RenewedCredential, ProviderFailure> {
        self.token(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("client_id", self.client_id()),
        ])
        .await
    }

    async fn verification_keys(
        &self,
        force: bool,
    ) -> Result<jsonwebtoken::jwk::JwkSet, ProviderFailure> {
        let mut cache = self.jwks.lock().await;
        if !force {
            if let Some((fetched, keys)) = cache.as_ref() {
                if fetched.elapsed() < Duration::from_secs(300) {
                    return Ok(keys.clone());
                }
            }
        }
        let response = self
            .client
            .get(&self.config.jwks_url)
            .send()
            .await
            .map_err(|_| ProviderFailure::Uncertain)?;
        let body = bounded_body(checked(response).await?).await?;
        let keys: jsonwebtoken::jwk::JwkSet =
            serde_json::from_slice(&body).map_err(|_| ProviderFailure::Uncertain)?;
        if keys.keys.is_empty() {
            return Err(ProviderFailure::Uncertain);
        }
        *cache = Some((std::time::Instant::now(), keys.clone()));
        Ok(keys)
    }

    async fn token(&self, form: &[(&str, &str)]) -> Result<RenewedCredential, ProviderFailure> {
        // Resolve verification material before a one-time credential can be consumed.
        let mut keys = self
            .verification_keys(false)
            .await
            .map_err(|_| ProviderFailure::NotSent)?;
        let response = self
            .client
            .post(&self.config.token_url)
            .form(form)
            .send()
            .await
            .map_err(|error| {
                if error.is_connect() {
                    ProviderFailure::NotSent
                } else {
                    ProviderFailure::Uncertain
                }
            })?;
        let response = checked(response).await?;
        let body = bounded_body(response).await?;
        let mut token: RenewedCredential =
            serde_json::from_slice(&body).map_err(|_| ProviderFailure::Uncertain)?;
        if token.token_type != "Bearer"
            || token.refresh_token.is_empty()
            || token.expires_in == 0
            || token.expires_in > 600
            || token.issuer_grant.client_id.to_string() != self.config.client_id
            || token.issuer_grant.status != "ACTIVE"
        {
            return Err(ProviderFailure::Uncertain);
        }
        let header = jsonwebtoken::decode_header(&token.access_token)
            .map_err(|_| ProviderFailure::Uncertain)?;
        if header.alg != jsonwebtoken::Algorithm::RS256 {
            return Err(ProviderFailure::Uncertain);
        }
        let kid = header.kid.as_deref().ok_or(ProviderFailure::Uncertain)?;
        if keys.find(kid).is_none() {
            // One key-rollover retry only. Failure here is after rotation and
            // must remain uncertain; the issuer must publish keys before use.
            keys = self.verification_keys(true).await?;
        }
        let key = header
            .kid
            .as_deref()
            .and_then(|kid| keys.find(kid))
            .ok_or(ProviderFailure::Uncertain)?;
        let key =
            jsonwebtoken::DecodingKey::from_jwk(key).map_err(|_| ProviderFailure::Uncertain)?;
        let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
        validation.validate_nbf = true;
        validation.set_issuer(&[&self.config.issuer]);
        validation.set_audience(&[&self.config.audience]);
        validation.leeway = 0;
        let claims = jsonwebtoken::decode::<Value>(&token.access_token, &key, &validation)
            .map_err(|_| ProviderFailure::Uncertain)?
            .claims;
        let payload = token
            .access_token
            .split('.')
            .nth(1)
            .ok_or(ProviderFailure::Uncertain)?;
        let payload = URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| ProviderFailure::Uncertain)?;
        if oauth_workflow_contract::verified_payload_purpose(&payload)
            .map_err(|_| ProviderFailure::Uncertain)?
            != Some(TokenUse::User)
            || claims.get("uid").and_then(Value::as_str)
                != Some(token.issuer_grant.user_id.to_string().as_str())
            || claims.get("host").and_then(Value::as_str)
                != Some(token.issuer_grant.host_id.to_string().as_str())
            || claims.get("client_id").and_then(Value::as_str) != Some(self.client_id())
        {
            return Err(ProviderFailure::Uncertain);
        }
        let exp = claims
            .get("exp")
            .and_then(Value::as_i64)
            .ok_or(ProviderFailure::Uncertain)?;
        let iat = claims
            .get("iat")
            .and_then(Value::as_i64)
            .ok_or(ProviderFailure::Uncertain)?;
        if !(1..=600).contains(&exp.checked_sub(iat).ok_or(ProviderFailure::Uncertain)?)
            || token.scope != token.issuer_grant.scope
        {
            return Err(ProviderFailure::Uncertain);
        }
        token.access_expires_at = exp;
        Ok(token)
    }

    pub async fn status(&self, grant: &str) -> Result<GrantStatus, ProviderFailure> {
        let grant = uuid::Uuid::parse_str(grant).map_err(|_| ProviderFailure::Configuration)?;
        let response = self
            .client
            .post(format!(
                "{}/{grant}/status",
                self.config.grant_url_prefix.trim_end_matches('/')
            ))
            .json(&serde_json::json!({"clientId":self.client_id()}))
            .send()
            .await
            .map_err(|_| ProviderFailure::Uncertain)?;
        serde_json::from_slice(&bounded_body(checked(response).await?).await?)
            .map_err(|_| ProviderFailure::Uncertain)
    }

    pub async fn revoke(&self, grant: &str) -> Result<(), ProviderFailure> {
        let grant = uuid::Uuid::parse_str(grant).map_err(|_| ProviderFailure::Configuration)?;
        let response = self
            .client
            .post(format!(
                "{}/{grant}/revoke",
                self.config.grant_url_prefix.trim_end_matches('/')
            ))
            .json(&serde_json::json!({"clientId":self.client_id()}))
            .send()
            .await
            .map_err(|_| ProviderFailure::Uncertain)?;
        checked(response).await?;
        Ok(())
    }
}

fn role_authority_url(enrollment_url: &str, client_id: &str) -> Result<url::Url, ProviderFailure> {
    if !enrollment_url.ends_with("/workflow/enrollments") {
        return Err(ProviderFailure::Configuration);
    }
    let mut url = url::Url::parse(enrollment_url).map_err(|_| ProviderFailure::Configuration)?;
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| ProviderFailure::Configuration)?;
        segments.pop();
        segments
            .push("clients")
            .push(client_id)
            .push("current-roles");
    }
    Ok(url)
}

#[cfg(test)]
mod role_authority_tests {
    use super::*;

    #[test]
    fn role_lookup_stays_on_the_configured_private_issuer_origin() {
        let url = role_authority_url(
            "https://issuer.internal:7443/oauth2/lightapi/workflow/enrollments",
            "client-id",
        )
        .unwrap();
        assert_eq!(
            url.as_str(),
            "https://issuer.internal:7443/oauth2/lightapi/workflow/clients/client-id/current-roles"
        );
        assert!(
            role_authority_url("https://issuer.internal/oauth2/lightapi/token", "client-id")
                .is_err()
        );
    }
}

async fn bounded_body(mut response: reqwest::Response) -> Result<Vec<u8>, ProviderFailure> {
    const LIMIT: usize = 65536;
    if response
        .content_length()
        .is_some_and(|size| size > LIMIT as u64)
    {
        return Err(ProviderFailure::Uncertain);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| ProviderFailure::Uncertain)?
    {
        if chunk.len() > LIMIT - body.len() {
            return Err(ProviderFailure::Uncertain);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

async fn checked(response: reqwest::Response) -> Result<reqwest::Response, ProviderFailure> {
    if response.status().is_success() {
        Ok(response)
    } else if response.status().is_client_error() {
        Err(ProviderFailure::ReauthorizationRequired)
    } else {
        Err(ProviderFailure::Uncertain)
    }
}
