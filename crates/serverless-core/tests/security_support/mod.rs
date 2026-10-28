#![allow(dead_code)] // Shared test utilities used by separate integration-test binaries.
use async_trait::async_trait;
use http::{HeaderMap, Method, StatusCode};
use light_security::{SecurityConfig, SecurityRuntime, load_security_runtime};
use serde_json::{Value, json};
use serverless_core::{
    compat::{COMPATIBILITY, Compatibility},
    handlers::jwt::JwtHandler,
    plan::{Binding, HandlerFactory, Registry, TargetRoutes},
    unsupported::validate_security,
    *,
};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

// Reuse the approved WP6 loopback server and deterministic start/release synchronization.
#[path = "../../../light-security/tests/support/mod.rs"]
pub mod jwks;

pub fn config() -> SecurityConfig {
    SecurityConfig {
        issuer: "test-issuer".into(),
        audience: vec!["test-audience".into()],
        ..Default::default()
    }
}
pub async fn server(held: bool) -> (jwks::Server, Vec<u8>) {
    let server = jwks::Server::new("test-key", held).await;
    let key = [
        uuid::Uuid::new_v4().as_bytes().as_slice(),
        uuid::Uuid::new_v4().as_bytes().as_slice(),
    ]
    .concat();
    server.respond(200, jwks::keys("test-key", &key));
    (server, key)
}
pub fn claims(user: &str) -> Value {
    json!({"sub":user,"user_id":user,"client_id":"test-client","iss":"test-issuer","aud":"test-audience","exp":jwks::now()+3600,"scope":"read write","secret":"SECRET-MARKER-WP9a"})
}
pub fn signed(key: &[u8], claims: Value) -> String {
    jwks::token(Some("test-key"), key, claims)
}
pub fn handler(runtime: SecurityRuntime, scopes: &[&str]) -> Arc<JwtHandler> {
    let settings =
        validate_security(&serde_json::to_value(&runtime.config).unwrap(), false).unwrap();
    Arc::new(
        JwtHandler::new(
            settings,
            Some(Arc::new(runtime)),
            scopes.iter().map(|s| (*s).to_owned()).collect(),
        )
        .unwrap(),
    )
}
pub fn invocation(auth: Option<&str>, path: &str) -> Invocation {
    let mut headers = HeaderMap::new();
    if let Some(auth) = auth {
        headers.insert("authorization", auth.parse().unwrap());
    }
    Invocation {
        method: Method::GET,
        raw_path: path.into(),
        path: path.into(),
        raw_query: None,
        query: MultiMap::default(),
        headers,
        body: None,
        body_was_base64: false,
        source: SourceMetadata::default(),
        request_id: "test-request".into(),
        deadline: Instant::now() + Duration::from_secs(2),
        response_deadline: Instant::now() + Duration::from_secs(3),
        cancel: CancellationToken::new(),
        path_params: BTreeMap::new(),
        route: None,
        original: Value::Null,
    }
}
pub struct One(pub Binding);
impl HandlerFactory for One {
    fn build(&self, _: &Compatibility, _: &str) -> Result<Binding, InitError> {
        Ok(self.0.clone())
    }
}
#[derive(Default)]
pub struct Probe {
    pub dispatches: AtomicUsize,
    pub principals: Mutex<Vec<Option<String>>>,
    pub final_principals: Mutex<Vec<Option<String>>>,
}
#[async_trait]
impl Backend for Probe {
    async fn dispatch(
        &self,
        _: &Invocation,
        st: &ExchangeState,
        _: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        self.dispatches.fetch_add(1, Ordering::SeqCst);
        self.principals
            .lock()
            .unwrap()
            .push(st.principal.as_ref().map(|p| p.principal_id().to_owned()));
        Ok(GatewayResponse {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: None,
            body_is_binary: false,
        })
    }
}
impl Finalizer for Probe {
    fn finalize(&self, _: &Invocation, st: &ExchangeState, _: &GatewayResponse) {
        self.final_principals
            .lock()
            .unwrap()
            .push(st.principal.as_ref().map(|p| p.principal_id().to_owned()));
    }
}
pub fn pipeline(factory: Arc<dyn HandlerFactory>, id: HandlerId, probe: Arc<Probe>) -> Pipeline {
    let mut registry = Registry::default();
    registry.register(id, factory);
    registry.register(
        HandlerId::Proxy,
        Arc::new(One(Binding::Dispatch(
            TargetRoutes::compile([("/*@get".into(), Target::Function("test-business".into()))])
                .unwrap(),
        ))),
    );
    let class = |id| COMPATIBILITY.iter().find(|c| c.id == id).unwrap().class;
    let plan = ExecutionPlan::compile(json!({"enabled":true,"handlers":[format!("{}@auth",class(id)),format!("{}@proxy",class(HandlerId::Proxy))],"defaultHandlers":["auth","proxy"]}),json!({}),&registry).unwrap();
    Pipeline::new(Arc::new(plan), vec![probe])
}
pub async fn denied(pipe: &Pipeline, probe: &Probe, inv: Invocation, status: u16, code: &str) {
    let before = probe.dispatches.load(Ordering::SeqCst);
    let response = pipe.execute(inv, probe).await;
    assert_eq!(response.status.as_u16(), status);
    assert!(
        !response.headers.contains_key("www-authenticate"),
        "JWT-only Java denials carry no Basic challenge"
    );
    let body: Value = serde_json::from_slice(response.body.as_ref().unwrap()).unwrap();
    assert_eq!(body["code"], code);
    assert_eq!(
        probe.dispatches.load(Ordering::SeqCst),
        before,
        "denial must execute zero backend calls"
    );
    assert_eq!(probe.final_principals.lock().unwrap().last(), Some(&None));
}
pub fn runtime_with_client(cfg: SecurityConfig, client: Value) -> SecurityRuntime {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("security.yml"),
        serde_yaml::to_string(&cfg).unwrap(),
    )
    .unwrap();
    std::fs::write(dir.path().join("client.yml"), client.to_string()).unwrap();
    let config = light_runtime::RuntimeConfig {
        bootstrap: Default::default(),
        server: Default::default(),
        client: None,
        portal_registry: None,
        direct_registry: Default::default(),
        service_identity: Default::default(),
        config_dir: dir.path().into(),
        external_config_dir: dir.path().into(),
        resolved_values: Default::default(),
        default_config_dir: None,
        embedded_config: &[],
        module_registry: Arc::new(light_runtime::ModuleRegistry::new()),
        cache_registry: None,
        registry_client: None,
    };
    load_security_runtime(&config, true).unwrap().unwrap()
}
