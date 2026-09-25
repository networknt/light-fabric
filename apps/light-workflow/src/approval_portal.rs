//! Restricted Workflow approval operations through the fixed Gateway origin.
//! The approval lifecycle supplies a short-lived signed actor assertion; this
//! client never accepts an arbitrary URL or operation name from workflow data.
use light_client::config::OAuthWorkflowLongConfig;
use light_client::long_binding::{LongBindingClient, LongClientError};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{path::Path, sync::Arc, time::Duration};
use uuid::Uuid;

const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const HOST: &str = "lightapi.net";
const VERSION: &str = "0.1.0";

#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    pub gateway_url: String,
    pub provider_id: String,
    pub client_id: String,
    pub client_secret_file: String,
    pub ca_file: String,
    pub signing_key_file: String,
}

impl Config {
    pub fn validate(&self) -> Result<(), Error> {
        let gateway = url::Url::parse(&self.gateway_url).map_err(|_| Error::Configuration)?;
        if gateway.scheme() != "https"
            || gateway.host_str().is_none()
            || !gateway.username().is_empty()
            || gateway.password().is_some()
            || gateway.path() != "/"
            || gateway.query().is_some()
            || gateway.fragment().is_some()
            || self.client_id.is_empty()
            || self.client_secret_file.is_empty()
            || self.ca_file.is_empty()
            || self.signing_key_file.is_empty()
            || self.provider_id.is_empty()
            || !self
                .provider_id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        {
            return Err(Error::Configuration);
        }
        Ok(())
    }
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApprovalPortalConfig")
            .field("gateway_url", &self.gateway_url)
            .field("provider_id", &self.provider_id)
            .field("client_id", &self.client_id)
            .field("client_secret_file", &self.client_secret_file)
            .field("ca_file", &self.ca_file)
            .field("signing_key_file", &self.signing_key_file)
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("approval Portal configuration is unavailable or invalid")]
    Configuration,
    #[error("approval Portal request denied")]
    Denied,
    #[error("approval Portal request conflicted")]
    Conflict,
    #[error("approval Portal unavailable")]
    Unavailable,
    #[error("approval Portal response is invalid")]
    InvalidResponse,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadRequest {
    pub host_id: Uuid,
    pub request_id: Uuid,
    pub request_digest: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReadResponse {
    pub request_id: Uuid,
    pub request_digest: String,
    pub request_version: i64,
    pub status: String,
    pub requester_subject: String,
    pub approval_wf_def_id: Uuid,
    pub approval_definition_digest: String,
    pub target_wf_def_id: Uuid,
    pub justification: String,
    pub items: serde_json::Value,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DecisionRequest {
    pub host_id: Uuid,
    pub decision_id: Uuid,
    pub request_id: Uuid,
    pub request_digest: String,
    pub accepted_instance_id: Uuid,
    pub task_id: Uuid,
    pub task_asst_id: Uuid,
    pub decision: String,
    pub comment: Option<String>,
    pub approver_subject: String,
    pub payload_digest: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DecisionResponse {
    pub decision_id: Uuid,
    pub request_id: Uuid,
    pub outcome: String,
    pub request_version: i64,
    pub committed_at: String,
}

pub struct Client {
    http: reqwest::Client,
    credential: Arc<LongBindingClient>,
    gateway: url::Url,
    signing_key: jsonwebtoken::EncodingKey,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActorEvidence {
    pub iss: String,
    pub aud: String,
    pub host_id: Uuid,
    pub request_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_instance_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_id: Option<Uuid>,
    pub operation: String,
    pub sub: String,
    pub actor_evidence_digest: String,
    pub iat: usize,
    pub exp: usize,
    pub jti: Uuid,
}

impl ActorEvidence {
    pub fn new(
        host_id: Uuid,
        request_id: Uuid,
        operation: &str,
        subject: &str,
        digest: &str,
    ) -> Result<Self, Error> {
        if !matches!(
            operation,
            "getWorkflowToolAccessRequestForExecution" | "deliverWorkflowToolAccessDecision"
        ) || subject.is_empty()
            || !digest.starts_with("sha256:")
            || digest.len() != 71
            || !digest.as_bytes()[7..]
                .iter()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(Error::Configuration);
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| Error::Configuration)?
            .as_secs() as usize;
        Ok(Self {
            iss: "light-workflow-approval".into(),
            aud: "portal-workflow-approval".into(),
            host_id,
            request_id,
            accepted_instance_id: None,
            decision_id: None,
            operation: operation.into(),
            sub: subject.into(),
            actor_evidence_digest: digest.into(),
            iat: now,
            exp: now + 300,
            jti: Uuid::now_v7(),
        })
    }
}

impl Client {
    pub async fn open(config: &Config, dir: &Path) -> Result<Self, Error> {
        config.validate()?;
        let secret = tokio::fs::read_to_string(dir.join(&config.client_secret_file))
            .await
            .map_err(|_| Error::Configuration)?;
        let credential = LongBindingClient::from_config(
            &OAuthWorkflowLongConfig {
                gateway_url: config.gateway_url.clone(),
                provider_id: config.provider_id.clone(),
                client_id: config.client_id.clone(),
                client_secret: secret.trim().to_string(),
                ca_file: config.ca_file.clone(),
                ..Default::default()
            },
            dir,
        )
        .await
        .map_err(|_| Error::Configuration)?;
        let gateway = url::Url::parse(&config.gateway_url).map_err(|_| Error::Configuration)?;
        let ca = tokio::fs::read(dir.join(&config.ca_file))
            .await
            .map_err(|_| Error::Configuration)?;
        let signing_key = tokio::fs::read(dir.join(&config.signing_key_file))
            .await
            .map_err(|_| Error::Configuration)?;
        if signing_key.len() < 32 {
            return Err(Error::Configuration);
        }
        let mut builder = reqwest::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(5));
        for cert in reqwest::Certificate::from_pem_bundle(&ca).map_err(|_| Error::Configuration)? {
            builder = builder.add_root_certificate(cert);
        }
        Ok(Self {
            http: builder.build().map_err(|_| Error::Configuration)?,
            credential: Arc::new(credential),
            gateway,
            signing_key: jsonwebtoken::EncodingKey::from_secret(&signing_key),
        })
    }

    pub fn sign_actor(&self, evidence: &ActorEvidence) -> Result<String, Error> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| Error::Configuration)?
            .as_secs() as usize;
        if evidence.iss != "light-workflow-approval"
            || evidence.aud != "portal-workflow-approval"
            || !matches!(
                evidence.operation.as_str(),
                "getWorkflowToolAccessRequestForExecution" | "deliverWorkflowToolAccessDecision"
            )
            || evidence.iat > now + 30
            || evidence.exp <= now
            || evidence.exp <= evidence.iat
            || evidence.exp - evidence.iat > 300
        {
            return Err(Error::Configuration);
        }
        jsonwebtoken::encode(
            &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
            evidence,
            &self.signing_key,
        )
        .map_err(|_| Error::Configuration)
    }

    pub async fn read(
        &self,
        input: &ReadRequest,
        actor: &ActorEvidence,
    ) -> Result<ReadResponse, Error> {
        self.check_actor(
            actor,
            input.host_id,
            input.request_id,
            "getWorkflowToolAccessRequestForExecution",
        )?;
        if actor.accepted_instance_id.is_some() || actor.decision_id.is_some() {
            return Err(Error::Configuration);
        }
        let result: ReadResponse = self
            .call(
                "getWorkflowToolAccessRequestForExecution",
                input,
                actor,
                true,
                None,
            )
            .await?;
        if result.request_id != input.request_id
            || result.request_digest != input.request_digest
            || result.request_version < 0
            || result.status != "REQUESTED"
            || result.requester_subject != actor.sub
            || result.approval_wf_def_id.is_nil()
            || !result.approval_definition_digest.starts_with("sha256:")
            || result.target_wf_def_id.is_nil()
            || result.justification.is_empty()
            || result.items.as_array().is_none_or(Vec::is_empty)
        {
            return Err(Error::InvalidResponse);
        }
        Ok(result)
    }

    pub async fn decide(
        &self,
        input: &DecisionRequest,
        actor: &ActorEvidence,
    ) -> Result<DecisionResponse, Error> {
        self.check_actor(
            actor,
            input.host_id,
            input.request_id,
            "deliverWorkflowToolAccessDecision",
        )?;
        if actor.accepted_instance_id != Some(input.accepted_instance_id)
            || actor.decision_id != Some(input.decision_id)
        {
            return Err(Error::Configuration);
        }
        if !matches!(input.decision.as_str(), "APPROVE" | "REJECT")
            || input.approver_subject != actor.sub
            || !input.payload_digest.starts_with("sha256:")
            || input.payload_digest.len() != 71
        {
            return Err(Error::Configuration);
        }
        let result: DecisionResponse = self
            .call(
                "deliverWorkflowToolAccessDecision",
                input,
                actor,
                false,
                Some(input.decision_id),
            )
            .await?;
        if result.request_id != input.request_id
            || result.decision_id != input.decision_id
            || !matches!(result.outcome.as_str(), "GRANTED" | "REJECTED" | "STALE")
            || result.request_version < 0
            || result.committed_at.is_empty()
        {
            return Err(Error::InvalidResponse);
        }
        Ok(result)
    }

    fn check_actor(
        &self,
        actor: &ActorEvidence,
        host: Uuid,
        request: Uuid,
        action: &str,
    ) -> Result<(), Error> {
        if actor.host_id != host || actor.request_id != request || actor.operation != action {
            return Err(Error::Configuration);
        }
        Ok(())
    }

    async fn call<T: Serialize, R: DeserializeOwned>(
        &self,
        action: &'static str,
        input: &T,
        actor: &ActorEvidence,
        query: bool,
        idempotency: Option<Uuid>,
    ) -> Result<R, Error> {
        let assertion = self.sign_actor(actor)?;
        let app = self
            .credential
            .workload_token()
            .await
            .map_err(map_token_error)?;
        let command = serde_json::json!({"host": HOST, "service": "workflow", "action": action,
            "version": VERSION, "data": input});
        let path = if query {
            "portal/query"
        } else {
            "portal/command"
        };
        let mut endpoint = self.gateway.join(path).map_err(|_| Error::Configuration)?;
        if endpoint.origin() != self.gateway.origin() {
            return Err(Error::Configuration);
        }
        let request = if query {
            endpoint
                .query_pairs_mut()
                .append_pair("cmd", &command.to_string());
            self.http.get(endpoint)
        } else {
            self.http.post(endpoint).json(&command)
        };
        let mut request = request
            .bearer_auth(&app)
            .header("X-Scope-Token", format!("Bearer {app}"))
            .header("X-Workflow-Actor-Evidence", assertion);
        if let Some(id) = idempotency {
            request = request.header("Idempotency-Key", id.to_string());
        }
        let mut response = request.send().await.map_err(|_| Error::Unavailable)?;
        match response.status().as_u16() {
            200 => {}
            401 | 403 => return Err(Error::Denied),
            409 => return Err(Error::Conflict),
            _ => return Err(Error::Unavailable),
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Error::Unavailable)? {
            if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
                return Err(Error::InvalidResponse);
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).map_err(|_| Error::InvalidResponse)
    }
}

fn map_token_error(error: LongClientError) -> Error {
    match error {
        LongClientError::Denied => Error::Denied,
        LongClientError::Retryable => Error::Unavailable,
        _ => Error::Configuration,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_rejects_unknown_fields_and_redacts_no_secret_value() {
        assert!(serde_json::from_value::<Config>(serde_json::json!({
            "gatewayUrl":"https://gateway.example/", "providerId":"portal",
            "clientId":"workflow", "clientSecretFile":"secret", "caFile":"ca", "signingKeyFile":"key",
            "allowHttp":true
        })).is_err());
        let config: Config = serde_json::from_value(serde_json::json!({
            "gatewayUrl":"https://gateway.example/", "providerId":"portal",
            "clientId":"workflow", "clientSecretFile":"secret", "caFile":"ca", "signingKeyFile":"key"
        })).unwrap();
        assert!(!format!("{config:?}").contains("client_secret:"));
        assert!(config.validate().is_ok());
        let mut invalid = config;
        invalid.gateway_url = "http://gateway.example/".into();
        assert!(invalid.validate().is_err());
        invalid.gateway_url = "https://gateway.example/".into();
        invalid.provider_id = "../other".into();
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn actor_assertion_is_bounded_to_one_operation() {
        let evidence = ActorEvidence::new(
            Uuid::now_v7(),
            Uuid::now_v7(),
            "getWorkflowToolAccessRequestForExecution",
            "requester",
            &format!("sha256:{}", "a".repeat(64)),
        )
        .unwrap();
        assert_eq!(evidence.exp - evidence.iat, 300);
        assert!(
            ActorEvidence::new(
                evidence.host_id,
                evidence.request_id,
                "arbitraryOperation",
                "requester",
                &evidence.actor_evidence_digest,
            )
            .is_err()
        );
    }

    #[test]
    fn request_read_requires_pinned_approval_definition_identity() {
        let response = serde_json::json!({
            "requestId": Uuid::now_v7(),
            "requestDigest": format!("sha256:{}", "a".repeat(64)),
            "requestVersion": 1,
            "status": "REQUESTED",
            "requesterSubject": "requester",
            "approvalWfDefId": Uuid::now_v7(),
            "approvalDefinitionDigest": format!("sha256:{}", "b".repeat(64)),
            "targetWfDefId": Uuid::now_v7(),
            "justification": "Required lookup",
            "items": [{"toolId": Uuid::now_v7()}]
        });
        assert!(serde_json::from_value::<ReadResponse>(response.clone()).is_ok());
        let mut missing = response;
        missing.as_object_mut().unwrap().remove("approvalWfDefId");
        assert!(serde_json::from_value::<ReadResponse>(missing).is_err());
    }
}
