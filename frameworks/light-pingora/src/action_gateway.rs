//! A2 Gateway execution context. Verified app/peer identity determines origin;
//! headers cannot reclassify a Workflow caller as an interactive caller.
use bytes::Bytes;
use light_runtime::{ModuleKind, RuntimeConfig, RuntimeError};
use light_security::{
    SecurityRuntime,
    dual_identity::{self, Origin, RoutePolicy},
};
use pingora::{
    connectors::{ConnectorOptions, http::Connector},
    http::RequestHeader,
    upstreams::peer::HttpPeer,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use uuid::Uuid;
use workflow_action::{ActionReference, Binding};
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Target {
    pub url: String,
    pub contract_digest: String,
    pub forward_user: bool,
    #[serde(default)]
    pub receipt: ReceiptPolicy,
}
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ReceiptPolicy {
    #[default]
    TerminalHttp,
    WorkflowAccepted,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    pub gateway_url: String,
    pub policy: RoutePolicy,
    pub incoming_client_ca_file: PathBuf,
    pub control: light_client::workflow_actions::Config,
    pub backend_ca_file: PathBuf,
    pub backend_certificate_file: PathBuf,
    pub backend_key_file: PathBuf,
    pub backend_scope_token_file: PathBuf,
    pub targets: BTreeMap<Uuid, Target>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct File {
    authorization: Option<Config>,
}
pub fn load(c: &RuntimeConfig) -> Result<Option<Config>, RuntimeError> {
    let f = match c
        .module_registry
        .load_config::<File>(c, "workflow-actions.yml")
    {
        Ok(f) => f,
        Err(RuntimeError::MissingConfig(_)) => return Ok(None),
        Err(e) => return Err(e),
    };
    c.module_registry.register_loaded_config(
        "light-pingora/workflow-actions",
        "workflow-actions",
        ModuleKind::Framework,
        &f,
        [],
        true,
        Some(f.authorization.is_some()),
        false,
    )?;
    Ok(f.authorization)
}
pub struct Runtime {
    pub config: Config,
    dir: PathBuf,
    client: tokio::sync::OnceCell<Arc<light_client::workflow_actions::Client>>,
    internal: Connector,
    external: Connector,
    scope: String,
}
impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WorkflowActionRuntime")
    }
}
#[derive(Clone)]
pub struct Context {
    runtime: Arc<Runtime>,
    reference: Uuid,
    caller: String,
    user: String,
    body: Bytes,
}
impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkflowActionContext")
            .field("action", &self.reference)
            .finish()
    }
}
fn denied() -> String {
    "workflow action denied".into()
}
impl Runtime {
    pub fn new(config: Config, dir: &Path) -> Result<Self, RuntimeError> {
        config
            .policy
            .validate()
            .map_err(|_| RuntimeError::Config(denied()))?;
        for target in config.targets.values() {
            let u = url::Url::parse(&target.url).map_err(|_| RuntimeError::Config(denied()))?;
            if u.scheme() != "https"
                || u.host_str().is_none()
                || !u.username().is_empty()
                || u.password().is_some()
                || u.fragment().is_some()
                || !workflow_action::is_digest(&target.contract_digest)
            {
                return Err(RuntimeError::Config(denied()));
            }
        }
        let scope = std::fs::read_to_string(dir.join(&config.backend_scope_token_file))?
            .trim()
            .to_owned();
        if !scope.starts_with("Bearer ") || scope.contains(['\r', '\n']) {
            return Err(RuntimeError::Config(denied()));
        }
        let options = ConnectorOptions {
            ca_file: Some(
                dir.join(&config.backend_ca_file)
                    .to_string_lossy()
                    .into_owned(),
            ),
            cert_key_file: Some((
                dir.join(&config.backend_certificate_file)
                    .to_string_lossy()
                    .into_owned(),
                dir.join(&config.backend_key_file)
                    .to_string_lossy()
                    .into_owned(),
            )),
            debug_ssl_keylog: false,
            ..ConnectorOptions::new(32)
        };
        let internal = std::panic::catch_unwind(|| Connector::new(Some(options)))
            .map_err(|_| RuntimeError::Config("invalid workflow backend TLS".into()))?;
        Ok(Self {
            config,
            dir: dir.to_owned(),
            client: tokio::sync::OnceCell::new(),
            internal,
            external: Connector::new(None),
            scope,
        })
    }
    async fn client(&self) -> Result<Arc<light_client::workflow_actions::Client>, String> {
        self.client
            .get_or_try_init(|| async {
                light_client::workflow_actions::Client::new(&self.config.control, &self.dir)
                    .await
                    .map(Arc::new)
                    .map_err(|_| denied())
            })
            .await
            .cloned()
    }
    pub async fn context(
        self: &Arc<Self>,
        security: &SecurityRuntime,
        headers: &http::HeaderMap,
        peer: dual_identity::TlsPeer<'_>,
        body: Bytes,
    ) -> Result<Option<Context>, String> {
        // A caller with no application credential, on a route that allows it, is judged on its
        // user token alone and is interactive by construction (no action reference).
        let identity = match dual_identity::admit(security, &self.config.policy, headers, peer)
            .await
            .map_err(|_| denied())?
        {
            dual_identity::Admission::Application(identity) => *identity,
            dual_identity::Admission::UserOnly(_) => return Ok(None),
        };
        match identity.action_reference {
            Some(reference) => Ok(Some(Context {
                runtime: self.clone(),
                reference,
                caller: identity.service_id,
                user: format!(
                    "Bearer {}",
                    dual_identity::bearer(headers, "authorization").map_err(|_| denied())?
                ),
                body,
            })),
            None if identity.origin == Origin::Interactive => Ok(None),
            None => Err(denied()),
        }
    }
}
impl Context {
    pub fn action_id(&self) -> Uuid {
        self.reference
    }
    fn reference(&self, tool: Uuid) -> Result<ActionReference, String> {
        let target = self.runtime.config.targets.get(&tool).ok_or_else(denied)?;
        Ok(ActionReference {
            host_id: self.runtime.config.policy.host_id,
            action_id: self.reference,
            calling_app: self.caller.clone(),
            request_digest: workflow_action::request_digest(
                "POST",
                &self.runtime.config.gateway_url,
                &tool.to_string(),
                &self.body,
            ),
            tool_ref: tool,
            target: self.runtime.config.gateway_url.clone(),
            contract_digest: target.contract_digest.clone(),
        })
    }
    pub async fn inspect(&self, tool: Uuid) -> Result<Binding, String> {
        self.runtime
            .client()
            .await?
            .inspect(&self.reference(tool)?, &self.user)
            .await
            .map_err(|_| denied())
    }
    pub async fn execute(
        &self,
        tool: Uuid,
        method: &str,
        url: &str,
        mut headers: http::HeaderMap,
        body: Bytes,
    ) -> Result<crate::guarded_http::Response, String> {
        let target = self.runtime.config.targets.get(&tool).ok_or_else(denied)?;
        if target.url != url {
            return Err(denied());
        }
        let client = self.runtime.client().await?;
        let bounds = client
            .inspect(&self.reference(tool)?, &self.user)
            .await
            .map_err(|_| denied())?;
        if body.len() as u64 > bounds.request_bytes {
            return Err(denied());
        }
        let dispatch = client
            .authorize(&self.reference(tool)?, &self.user)
            .await
            .map_err(|_| denied())?;
        if client.begin(&dispatch, &self.user).await.is_err() {
            // The live guard knows it has not initiated; the ledger alone
            // determines whether a SEND_INTENT exists and accepts the receipt.
            let _ = client.complete(&dispatch, None).await;
            return Err(denied());
        }
        let result = async {
            let parsed = url::Url::parse(url).map_err(|_| denied())?;
            let host = parsed.host_str().ok_or_else(denied)?;
            let addr =
                tokio::net::lookup_host((host, parsed.port_or_known_default().ok_or_else(denied)?))
                    .await
                    .map_err(|_| denied())?
                    .next()
                    .ok_or_else(denied)?;
            let mut peer = HttpPeer::new(addr, true, host.to_owned());
            peer.options.set_http_version(1, 1);
            for name in [
                "authorization",
                "x-scope-token",
                "x-workflow-action",
                "host",
                "connection",
                "transfer-encoding",
                "content-length",
            ] {
                headers.remove(name);
            }
            if target.forward_user {
                headers.insert("authorization", self.user.parse().map_err(|_| denied())?);
                headers.insert(
                    "x-scope-token",
                    self.runtime.scope.parse().map_err(|_| denied())?,
                );
                headers.insert(
                    "x-workflow-action",
                    self.reference.to_string().parse().map_err(|_| denied())?,
                );
            }
            let path = match parsed.query() {
                Some(q) => format!("{}?{q}", parsed.path()),
                None => parsed.path().to_owned(),
            };
            let mut request =
                RequestHeader::build(method, path.as_bytes(), Some(headers.len() + 1))
                    .map_err(|_| denied())?;
            request
                .insert_header("Host", parsed.authority())
                .map_err(|_| denied())?;
            for (name, value) in &headers {
                request
                    .append_header(name.clone(), value.clone())
                    .map_err(|_| denied())?;
            }
            crate::guarded_http::execute(
                if target.forward_user {
                    &self.runtime.internal
                } else {
                    &self.runtime.external
                },
                &peer,
                request,
                body,
                dispatch.guard.clone(),
                dispatch.decision.binding.response_byte_limit as usize,
                Duration::from_secs(120),
            )
            .await
            .map_err(|_| denied())
        }
        .await;
        let known = result.as_ref().ok().and_then(|r| {
            let receipt = match target.receipt {
                ReceiptPolicy::TerminalHttp => response_is_terminal(r.header.status),
                ReceiptPolicy::WorkflowAccepted => workflow_acceptance_receipt(r, tool),
            };
            receipt.then(|| {
                use sha2::{Digest, Sha256};
                let mut hash = Sha256::new();
                hash.update(r.header.status.as_u16().to_be_bytes());
                hash.update(&r.body);
                (
                    r.header.status.is_success(),
                    format!("sha256:{}", hex::encode(hash.finalize())),
                )
            })
        });
        client
            .complete(&dispatch, known)
            .await
            .map_err(|_| denied())?;
        match result {
            Ok(response)
                if match target.receipt {
                    ReceiptPolicy::TerminalHttp => !response_is_terminal(response.header.status),
                    ReceiptPolicy::WorkflowAccepted => {
                        !workflow_acceptance_receipt(&response, tool)
                    }
                } =>
            {
                Err(denied())
            }
            outcome => outcome,
        }
    }
}

// An HTTP acknowledgement is not an execution receipt. Redirects are never
// followed and asynchronous acceptance retains the uncertain reservation until
// qualified target evidence reconciles it.
fn response_is_terminal(status: http::StatusCode) -> bool {
    !status.is_informational() && !status.is_redirection() && status != http::StatusCode::ACCEPTED
}

fn workflow_acceptance_receipt(response: &crate::guarded_http::Response, tool: Uuid) -> bool {
    if response.header.status != http::StatusCode::ACCEPTED {
        return false;
    }
    serde_json::from_slice::<workflow_invocation_contract::InvocationStatus>(&response.body)
        .ok()
        .is_some_and(|status| {
            status.contract_version == workflow_invocation_contract::CONTRACT_VERSION
                && !status.workflow_instance_id.is_nil()
                && status.stable_tool_ref == tool
                && status.state_version > 0
        })
}

#[cfg(test)]
mod tests {
    use super::{response_is_terminal, workflow_acceptance_receipt};
    use bytes::Bytes;
    use http::StatusCode;
    use pingora::http::ResponseHeader;
    use uuid::Uuid;

    #[test]
    fn acknowledgements_and_redirects_are_not_completion_evidence() {
        for status in [100, 101, 202, 301, 302, 307, 308] {
            assert!(!response_is_terminal(StatusCode::from_u16(status).unwrap()));
        }
        for status in [200, 201, 204, 400, 401, 403, 404, 409, 500] {
            assert!(response_is_terminal(StatusCode::from_u16(status).unwrap()));
        }
    }

    #[test]
    fn only_a_durable_workflow_acceptance_body_qualifies_202() {
        let id = Uuid::now_v7();
        let tool = Uuid::now_v7();
        let response = |body: Bytes| crate::guarded_http::Response {
            header: Box::new(ResponseHeader::build(StatusCode::ACCEPTED, None).unwrap()),
            body,
        };
        let accepted = serde_json::json!({
            "contractVersion":workflow_invocation_contract::CONTRACT_VERSION,
            "workflowInstanceId":id,"stableToolRef":tool,
            "definitionDigest":format!("sha256:{}","a".repeat(64)),
            "state":"ACCEPTED","stateVersion":1,
            "acceptedTs":"2026-01-01T00:00:00Z","updatedTs":"2026-01-01T00:00:00Z",
            "deadlineTs":"2026-01-01T00:01:00Z","retryable":false,"effectState":"none"
        });
        assert!(workflow_acceptance_receipt(
            &response(Bytes::from(accepted.to_string())),
            tool
        ));
        for body in [
            Bytes::from_static(b"{}"),
            Bytes::from_static(b"not-json"),
            Bytes::from(serde_json::json!({"workflowInstanceId":Uuid::nil()}).to_string()),
        ] {
            assert!(!workflow_acceptance_receipt(&response(body), tool));
        }
        assert!(!workflow_acceptance_receipt(
            &response(Bytes::from(accepted.to_string())),
            Uuid::now_v7()
        ));
    }
}
