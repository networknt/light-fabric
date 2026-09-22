//! An A2 Agent instance has one immutable origin. Request headers never select
//! credentials or turn an interactive instance into a Workflow job consumer.
use axum::{
    extract::{ConnectInfo, Request, State},
    http::StatusCode,
    middleware::Next,
    response::Response,
};
use light_runtime::{ModuleKind, RuntimeConfig, RuntimeError};
use light_security::{
    SecurityRuntime,
    dual_identity::{self, Origin, RoutePolicy},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Mode {
    Interactive,
    Workflow,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Config {
    pub mode: Mode,
    pub service_id: String,
    pub agent_def_id: Uuid,
    pub incoming: RoutePolicy,
    pub tls: light_axum::mtls::Config,
    pub job_authorization: Option<light_client::workflow_jobs::Config>,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct File {
    authorization: Option<Config>,
}
impl Config {
    pub fn validate(&self, service: &str, agent: Uuid, host: Uuid) -> Result<(), RuntimeError> {
        self.incoming
            .validate()
            .map_err(|_| RuntimeError::Config("invalid Agent origin policy".into()))?;
        if (self.mode == Mode::Workflow) != self.job_authorization.is_some()
            || self.service_id != service
            || self.agent_def_id != agent
            || self.agent_def_id.is_nil()
            || self.incoming.host_id != host
            || self
                .incoming
                .apps
                .values()
                .any(|app| app.origin != Origin::Gateway)
        {
            return Err(RuntimeError::Config(
                "Agent origin must bind the deployed service, definition, host and Gateway peers"
                    .into(),
            ));
        }
        Ok(())
    }
}
pub fn load(runtime: &RuntimeConfig) -> Result<Option<Config>, RuntimeError> {
    let file = match runtime
        .module_registry
        .load_config::<File>(runtime, "workflow-origin.yml")
    {
        Ok(file) => file,
        Err(RuntimeError::MissingConfig(_)) => return Ok(None),
        Err(error) => return Err(error),
    };
    runtime.module_registry.register_loaded_config(
        "light-agent/workflow-origin",
        "workflow-origin",
        ModuleKind::Application,
        &file,
        [],
        true,
        Some(file.authorization.is_some()),
        false,
    )?;
    Ok(file.authorization)
}
#[derive(Clone)]
pub struct Receiver {
    pub config: Config,
    pub security: Arc<SecurityRuntime>,
}
pub async fn enforce(
    State(receiver): State<Receiver>,
    request: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    if request.method() == axum::http::Method::GET && request.uri().path() == "/health" {
        return Ok(next.run(request).await);
    }
    // Workflow jobs arrive through the durable job bridge. No Chat, A2A, static
    // UI or upload route on this instance may admit an interactive user instead.
    if receiver.config.mode == Mode::Workflow {
        return Err(StatusCode::FORBIDDEN);
    }
    let peer = request
        .extensions()
        .get::<ConnectInfo<light_axum::mtls::Peer>>()
        .map(|p| p.0.fingerprint.as_str());
    let identity = dual_identity::authenticate(
        &receiver.security,
        &receiver.config.incoming,
        request.headers(),
        peer,
    )
    .await
    .map_err(|_| StatusCode::FORBIDDEN)?;
    if identity.origin != Origin::Gateway || identity.action_reference.is_some() {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(next.run(request).await)
}

impl Config {
    /// Changing a running definition's origin must not replay pending work from
    /// its previous identity. Use the separately published Agent definition.
    pub async fn check_history(&self, pool: &sqlx::PgPool) -> Result<(), RuntimeError> {
        let conflict: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM agent_turn_t t JOIN agent_session_t s ON s.host_id=t.host_id AND s.session_id=t.session_id WHERE s.host_id=$1 AND s.agent_def_id=$2 AND ((t.origin_kind='workflow') <> $3))")
            .bind(self.incoming.host_id).bind(self.agent_def_id).bind(self.mode==Mode::Workflow)
            .fetch_one(pool).await.map_err(|_|RuntimeError::Config("Agent origin history cannot be verified".into()))?;
        if conflict {
            return Err(RuntimeError::Config("Agent definition has work from another origin; publish a separate definition before enabling A2".into()));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use light_runtime::{
        ModuleRegistry,
        config::{BootstrapConfig, DirectRegistryConfig, ServerConfig, ServiceIdentity},
    };
    use light_security::dual_identity::AppProfile;
    use std::collections::BTreeMap;
    fn config(mode: Mode) -> Config {
        Config {
            mode,
            service_id: "agent-workflow".into(),
            agent_def_id: Uuid::now_v7(),
            incoming: RoutePolicy {
                issuer: "issuer".into(),
                audience: "audience".into(),
                host_id: Uuid::now_v7(),
                apps: BTreeMap::from([(
                    "gateway".into(),
                    AppProfile {
                        origin: Origin::Gateway,
                        peer_sha256: vec!["a".repeat(64)],
                        ca_trust: None,
                    },
                )]),
                legacy_long_lived_app_keys: vec![],
                interactive_user_only: false,
            },
            job_authorization: (mode == Mode::Workflow).then(|| {
                light_client::workflow_jobs::Config {
                    base_url: "https://workflow/".into(),
                    client_identity_file: "identity.pem".into(),
                    ca_file: "ca.pem".into(),
                    scope_token_file: "scope-token".into(),
                }
            }),
            tls: light_axum::mtls::Config {
                address: "127.0.0.1:0".into(),
                certificate_file: "cert.pem".into(),
                private_key_file: "key.pem".into(),
                client_ca_file: "ca.pem".into(),
            },
        }
    }
    #[test]
    fn origin_is_pinned_to_service_definition_host_and_gateway() {
        let mut c = config(Mode::Workflow);
        assert!(
            c.validate(&c.service_id, c.agent_def_id, c.incoming.host_id)
                .is_ok()
        );
        assert!(
            c.validate("interactive-agent", c.agent_def_id, c.incoming.host_id)
                .is_err()
        );
        assert!(
            c.validate(&c.service_id, Uuid::now_v7(), c.incoming.host_id)
                .is_err()
        );
        assert!(
            c.validate(&c.service_id, c.agent_def_id, Uuid::now_v7())
                .is_err()
        );
        c.incoming.apps.get_mut("gateway").unwrap().origin = Origin::Workflow;
        assert!(
            c.validate(&c.service_id, c.agent_def_id, c.incoming.host_id)
                .is_err()
        );
    }
    fn security() -> Arc<SecurityRuntime> {
        let runtime = RuntimeConfig {
            bootstrap: BootstrapConfig::default(),
            server: ServerConfig::default(),
            client: None,
            portal_registry: None,
            direct_registry: DirectRegistryConfig::default(),
            service_identity: ServiceIdentity::default(),
            config_dir: "/nonexistent-a2-test-config".into(),
            external_config_dir: "/nonexistent-a2-test-config".into(),
            resolved_values: Default::default(),
            default_config_dir: None,
            embedded_config: &[],
            module_registry: Arc::new(ModuleRegistry::new()),
            cache_registry: None,
            registry_client: None,
        };
        Arc::new(
            light_security::load_security_runtime(&runtime, true)
                .unwrap()
                .unwrap(),
        )
    }
    #[tokio::test]
    async fn workflow_instance_cannot_be_reclassified_by_interactive_paths_or_headers() {
        use tower::ServiceExt;
        let router = axum::Router::new()
            .fallback(|| async { StatusCode::OK })
            .layer(axum::middleware::from_fn_with_state(
                Receiver {
                    config: config(Mode::Workflow),
                    security: security(),
                },
                enforce,
            ));
        for path in [
            "/chat",
            "/a2a/demo",
            "/knowledge/upload-delegation",
            "/index.html",
            "/diagnostics/tools",
        ] {
            let request = Request::builder()
                .uri(path)
                .header("x-agent-origin", "interactive")
                .header("authorization", "Bearer a-user-token")
                .header("x-scope-token", "Bearer interactive-app-token")
                .body(axum::body::Body::empty())
                .unwrap();
            assert_eq!(
                router.clone().oneshot(request).await.unwrap().status(),
                StatusCode::FORBIDDEN
            );
        }
        let request = Request::builder()
            .uri("/health")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            router.oneshot(request).await.unwrap().status(),
            StatusCode::OK
        );
    }
    #[tokio::test]
    async fn ordinary_listener_and_forwarded_peer_headers_do_not_authenticate() {
        use tower::ServiceExt;
        let router = axum::Router::new()
            .fallback(|| async { StatusCode::OK })
            .layer(axum::middleware::from_fn_with_state(
                Receiver {
                    config: config(Mode::Interactive),
                    security: security(),
                },
                enforce,
            ));
        let request = Request::builder()
            .uri("/chat")
            .header("x-forwarded-client-cert", "a".repeat(64))
            .header("x-client-cert-sha256", "a".repeat(64))
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            router.oneshot(request).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
    }
}

pub struct JobAuthorizer(pub light_client::workflow_jobs::Client);
#[async_trait::async_trait]
impl light_agent::domain::WorkflowJobAuthorizer for JobAuthorizer {
    fn transport_enabled(&self) -> bool {
        true
    }
    async fn pending(
        &self,
        host: Uuid,
    ) -> anyhow::Result<Vec<light_client::workflow_job_transport::Job>> {
        self.0.pending(host).await
    }
    async fn report(
        &self,
        report: &light_client::workflow_job_transport::Report,
    ) -> anyhow::Result<()> {
        self.0.report(report).await
    }
    async fn authorized(&self, host: Uuid, job: Uuid) -> anyhow::Result<bool> {
        self.0.authorized(host, job).await
    }
}
