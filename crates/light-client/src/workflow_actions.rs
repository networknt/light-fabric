//! Fixed Workflow action control client. Only authenticated internal HTTPS,
//! no model-selected endpoint, redirects, cached approvals or automatic retries.
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use workflow_action::{
    ActionReference, Completion, Decision, DispatchState, Owner,
    guard::{SendGuard, SendState},
};
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    pub base_url: String,
    pub client_identity_file: PathBuf,
    pub ca_file: PathBuf,
    pub scope_token_file: PathBuf,
    pub owner: workflow_action::GatewayRegistration,
}
pub struct Client {
    http: reqwest::Client,
    base: String,
    scope: String,
    owner: Owner,
}
#[derive(Debug)]
pub enum Failure {
    Configuration,
    Unavailable,
    Denied,
    Conflict,
    InvalidResponse,
}
pub struct Dispatch {
    pub decision: Decision,
    pub guard: Arc<SendGuard>,
}
impl Client {
    pub async fn new(c: &Config, dir: &Path) -> Result<Self, Failure> {
        let u = url::Url::parse(&c.base_url).map_err(|_| Failure::Configuration)?;
        if u.scheme() != "https"
            || u.host_str().is_none()
            || !u.username().is_empty()
            || u.password().is_some()
            || !matches!(u.path(), "" | "/")
            || u.query().is_some()
            || u.fragment().is_some()
            || c.owner.replica.is_nil()
            || c.owner.gateway_service.is_empty()
        {
            return Err(Failure::Configuration);
        }
        let identity = tokio::fs::read(dir.join(&c.client_identity_file))
            .await
            .map_err(|_| Failure::Configuration)?;
        let ca = tokio::fs::read(dir.join(&c.ca_file))
            .await
            .map_err(|_| Failure::Configuration)?;
        let scope = tokio::fs::read_to_string(dir.join(&c.scope_token_file))
            .await
            .map_err(|_| Failure::Configuration)?;
        let scope = scope.trim().to_owned();
        if !scope.starts_with("Bearer ") || scope.bytes().any(|b| b == b'\r' || b == b'\n') {
            return Err(Failure::Configuration);
        }
        let mut builder = reqwest::Client::builder()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .timeout(Duration::from_secs(5))
            .identity(reqwest::Identity::from_pem(&identity).map_err(|_| Failure::Configuration)?);
        for cert in
            reqwest::Certificate::from_pem_bundle(&ca).map_err(|_| Failure::Configuration)?
        {
            builder = builder.add_root_certificate(cert)
        }
        let http = builder.build().map_err(|_| Failure::Configuration)?;
        let registration = workflow_action::RegisterOwner {
            gateway_service: c.owner.gateway_service.clone(),
            replica: c.owner.replica,
            boot: uuid::Uuid::now_v7(),
        };
        let mut client = Self {
            http,
            base: c.base_url.trim_end_matches('/').to_string(),
            scope,
            owner: Owner {
                gateway_service: registration.gateway_service.clone(),
                replica: registration.replica,
                boot: registration.boot,
                fencing_generation: 1,
            },
        };
        let bytes = client.call("register-owner", None, &registration).await?;
        let owner: Owner = serde_json::from_slice(&bytes).map_err(|_| Failure::InvalidResponse)?;
        if owner.gateway_service != registration.gateway_service
            || owner.replica != registration.replica
            || owner.boot != registration.boot
            || owner.fencing_generation <= 0
        {
            return Err(Failure::InvalidResponse);
        }
        client.owner = owner;
        Ok(client)
    }
    async fn call<T: Serialize + ?Sized>(
        &self,
        method: &str,
        user: Option<&str>,
        body: &T,
    ) -> Result<Vec<u8>, Failure> {
        let mut request = self
            .http
            .post(format!("{}/internal/workflow/actions/{method}", self.base))
            .header("x-scope-token", &self.scope)
            .json(body);
        if let Some(user) = user {
            request = request.header("authorization", user)
        }
        let mut response = request.send().await.map_err(|_| Failure::Unavailable)?;
        match response.status().as_u16() {
            200 | 204 => {}
            401 | 403 => return Err(Failure::Denied),
            409 => return Err(Failure::Conflict),
            _ => return Err(Failure::Unavailable),
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Failure::Unavailable)? {
            if bytes.len() + chunk.len() > 32768 {
                return Err(Failure::InvalidResponse);
            }
            bytes.extend_from_slice(&chunk)
        }
        Ok(bytes)
    }
    pub async fn authorize(
        &self,
        reference: &ActionReference,
        user: &str,
    ) -> Result<Dispatch, Failure> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Request<'a> {
            reference: &'a ActionReference,
            owner: &'a Owner,
        }
        let started = Instant::now();
        let bytes = self
            .call(
                "authorize",
                Some(user),
                &Request {
                    reference,
                    owner: &self.owner,
                },
            )
            .await?;
        let decision: Decision =
            serde_json::from_slice(&bytes).map_err(|_| Failure::InvalidResponse)?;
        if !reference.matches(&decision.binding) || decision.owner != self.owner {
            return Err(Failure::InvalidResponse);
        }
        let guard = Arc::new(
            SendGuard::new(started, decision.clone()).map_err(|_| Failure::InvalidResponse)?,
        );
        Ok(Dispatch { decision, guard })
    }
    pub async fn begin(&self, dispatch: &Dispatch, user: &str) -> Result<(), Failure> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Ack {
            decision: Decision,
            new_permission: bool,
        }
        let bytes = self
            .call("begin-dispatch", Some(user), &dispatch.decision)
            .await?;
        let ack: Ack = serde_json::from_slice(&bytes).map_err(|_| Failure::InvalidResponse)?;
        dispatch
            .guard
            .acknowledge(&ack.decision, ack.new_permission)
            .map_err(|_| Failure::Conflict)
    }
    /// Derive non-initiation exclusively from this live owner's local guard.
    /// Callers cannot fabricate NOT_INITIATED from an HTTP timeout/absent receipt.
    pub async fn complete(
        &self,
        d: &Dispatch,
        known: Option<(bool, String)>,
    ) -> Result<(), Failure> {
        let (outcome, evidence_digest) = if d.guard.abort_not_initiated() {
            (DispatchState::NotInitiated, None)
        } else if d.guard.state() == SendState::Started {
            match known {
                Some((true, e)) => (DispatchState::Succeeded, Some(e)),
                Some((false, e)) => (DispatchState::Failed, Some(e)),
                None => (DispatchState::Uncertain, None),
            }
        } else {
            return Err(Failure::Conflict);
        };
        self.call(
            "complete",
            None,
            &Completion {
                decision: d.decision.clone(),
                outcome,
                evidence_digest,
            },
        )
        .await?;
        Ok(())
    }
    pub async fn status(&self, d: &Decision, user: &str) -> Result<DispatchState, Failure> {
        serde_json::from_slice(
            &self
                .call(
                    "status",
                    Some(user),
                    &workflow_action::StatusRequest {
                        reference: workflow_action::ActionReference::from_binding(&d.binding),
                        owner: self.owner.clone(),
                    },
                )
                .await?,
        )
        .map_err(|_| Failure::InvalidResponse)
    }
}

impl Client {
    pub async fn inspect(
        &self,
        reference: &ActionReference,
        user: &str,
    ) -> Result<workflow_action::Binding, Failure> {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Request<'a> {
            reference: &'a ActionReference,
            owner: &'a Owner,
        }
        let bytes = self
            .call(
                "inspect",
                Some(user),
                &Request {
                    reference,
                    owner: &self.owner,
                },
            )
            .await?;
        let b: workflow_action::Binding =
            serde_json::from_slice(&bytes).map_err(|_| Failure::InvalidResponse)?;
        if !reference.matches(&b) {
            return Err(Failure::InvalidResponse);
        }
        Ok(b)
    }
}
