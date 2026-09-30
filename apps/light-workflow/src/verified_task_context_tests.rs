//! Owned PostgreSQL and signed-JWT HTTPS transport. No shared stack or provider.
use crate::rule_api::RuleApiState;
use axum::{
    Json, Router,
    body::Bytes,
    extract::{ConnectInfo, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
};
use light_security::{
    SecurityRuntime,
    dual_identity::{AppProfile, Origin, RoutePolicy},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::{collections::BTreeMap, path::Path, sync::Arc};
use uuid::Uuid;

const KEY: &[u8] = b"p02-only-synthetic-signing-key-32-bytes";
const TOOL: Uuid = Uuid::from_u128(0x22222222222242228222222222222222);
async fn wait_for_revocation_lock(pool: &PgPool, statement: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(3),async {
        loop {
            let blocked:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query=$1)")
                .bind(statement).fetch_one(pool).await.unwrap();
            if blocked {break;}
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await.expect("revocation must demonstrably wait on a PostgreSQL lock");
}
fn contract() -> String {
    format!("sha256:{}", "2".repeat(64))
}
fn signed(claims: &Value) -> String {
    let mut h = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
    h.kid = Some("p02".into());
    jsonwebtoken::encode(&h, claims, &jsonwebtoken::EncodingKey::from_secret(KEY)).unwrap()
}
fn user_claims(host: Uuid, user: Uuid, principal: &str) -> Value {
    json!({"iss":"p02","aud":["workflow","https://p02.invalid/native"],"host":host,"user_id":user,"uid":user,"sub":user,"client_id":principal,"token_use":"user","exp":chrono::Utc::now().timestamp()+3600})
}
fn app_claims(host: Uuid, sid: &str) -> Value {
    json!({"iss":"p02","aud":"workflow","host":host,"sid":sid,"client_id":sid,"env":"dev","token_use":"app","exp":chrono::Utc::now().timestamp()+3600})
}

struct Db {
    pool: PgPool,
    admin: PgPool,
    name: String,
}
impl Db {
    async fn new() -> Self {
        let url =
            std::env::var("P02_TEST_DATABASE_URL").expect("explicit P02 disposable DB required");
        let parsed = url::Url::parse(&url).unwrap();
        assert_eq!(parsed.host_str(), Some("127.0.0.1"));
        assert_eq!(parsed.port(), Some(55432));
        assert_eq!(parsed.path(), "/p02_context_gate");
        let admin = PgPool::connect(&url).await.unwrap();
        for role in [
            "operations_workflow_runtime",
            "operations_workflow_migrator",
        ] {
            sqlx::raw_sql(&format!("DO $$ BEGIN IF NOT EXISTS(SELECT FROM pg_roles WHERE rolname='{role}') THEN CREATE ROLE {role}; END IF; END $$")).execute(&admin).await.unwrap();
        }
        let name = format!("p02_context_{}", Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&admin)
            .await
            .unwrap();
        let mut child = parsed;
        child.set_path(&format!("/{name}"));
        let pool = PgPoolOptions::new()
            .max_connections(16)
            .after_connect(|c, _| {
                Box::pin(async move {
                    sqlx::query("SET search_path TO workflow_ops,pg_catalog")
                        .execute(c)
                        .await?;
                    Ok(())
                })
            })
            .connect(child.as_str())
            .await
            .unwrap();
        sqlx::raw_sql("CREATE SCHEMA workflow_ops")
            .execute(&pool)
            .await
            .unwrap();
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../crates/workflow-store/migrations/workflow-postgres");
        let mut files = std::fs::read_dir(dir)
            .unwrap()
            .map(|p| p.unwrap().path())
            .collect::<Vec<_>>();
        files.sort();
        for file in files {
            sqlx::raw_sql(&std::fs::read_to_string(file).unwrap())
                .execute(&pool)
                .await
                .unwrap();
        }
        sqlx::raw_sql("CREATE TABLE p02_test_effect_t(host_id uuid,run_id uuid,task_id uuid,creator jsonb,business_digest text,PRIMARY KEY(host_id,run_id,task_id))").execute(&pool).await.unwrap();
        Self { pool, admin, name }
    }
    async fn close(self) {
        self.pool.close().await;
        sqlx::query(&format!("DROP DATABASE {}", self.name))
            .execute(&self.admin)
            .await
            .unwrap();
        self.admin.close().await;
    }
}
fn certs(dir: &Path) -> (String, String) {
    fn run(dir: &Path, args: &[&str]) {
        let result = std::process::Command::new("rtk")
            .args(["proxy", "openssl"])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "synthetic certificate generation failed"
        );
    }
    run(
        dir,
        &[
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-keyout",
            "ca.key",
            "-out",
            "ca.pem",
            "-subj",
            "/CN=P02 Synthetic CA",
            "-days",
            "1",
        ],
    );
    for (name, usage) in [
        ("server", "serverAuth"),
        ("workflow", "clientAuth"),
        ("gateway", "clientAuth"),
    ] {
        let key = format!("{name}.key");
        let csr = format!("{name}.csr");
        let pem = format!("{name}.pem");
        let ext = format!("{name}.ext");
        std::fs::write(dir.join(&ext),format!("subjectAltName=DNS:localhost,IP:127.0.0.1\nextendedKeyUsage={usage}\nkeyUsage=digitalSignature,keyEncipherment\n")).unwrap();
        run(
            dir,
            &[
                "req",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-keyout",
                &key,
                "-out",
                &csr,
                "-subj",
                "/CN=localhost",
            ],
        );
        run(
            dir,
            &[
                "x509",
                "-req",
                "-in",
                &csr,
                "-CA",
                "ca.pem",
                "-CAkey",
                "ca.key",
                "-CAcreateserial",
                "-out",
                &pem,
                "-days",
                "1",
                "-extfile",
                &ext,
            ],
        );
        let combined = format!(
            "{}{}",
            std::fs::read_to_string(dir.join(&pem)).unwrap(),
            std::fs::read_to_string(dir.join(&key)).unwrap()
        );
        std::fs::write(dir.join(format!("{name}-identity.pem")), combined).unwrap();
    }
    let fingerprint = |name: &str| {
        let pem = std::fs::read(dir.join(format!("{name}.pem"))).unwrap();
        let cert = rustls_pemfile::certs(&mut pem.as_slice())
            .next()
            .unwrap()
            .unwrap();
        hex::encode(Sha256::digest(cert.as_ref()))
    };
    (fingerprint("workflow"), fingerprint("gateway"))
}

#[derive(Clone, Debug)]
struct TestPeer(String);
// Test-only optional-client TLS listener: OAuth requests use Basic auth; MCP
// actions still require a real CA-verified Workflow certificate and app JWT.
struct OptionalTls {
    tcp: tokio::net::TcpListener,
    tls: tokio_rustls::TlsAcceptor,
}
impl axum::serve::Listener for OptionalTls {
    type Io = tokio_rustls::server::TlsStream<tokio::net::TcpStream>;
    type Addr = TestPeer;
    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (tcp, _) = self.tcp.accept().await.unwrap();
            if let Ok(tls) = self.tls.accept(tcp).await {
                let peer = tls
                    .get_ref()
                    .1
                    .peer_certificates()
                    .and_then(|c| c.first())
                    .map(|c| hex::encode(Sha256::digest(c.as_ref())))
                    .unwrap_or_default();
                return (tls, TestPeer(peer));
            }
        }
    }
    fn local_addr(&self) -> std::io::Result<TestPeer> {
        Ok(TestPeer(String::new()))
    }
}
impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, OptionalTls>>
    for TestPeer
{
    fn connect_info(s: axum::serve::IncomingStream<'_, OptionalTls>) -> Self {
        s.remote_addr().clone()
    }
}
async fn optional_tls(dir: &Path) -> (OptionalTls, String) {
    let cert = std::fs::read(dir.join("server.pem")).unwrap();
    let key = std::fs::read(dir.join("server.key")).unwrap();
    let ca = std::fs::read(dir.join("ca.pem")).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    for c in rustls_pemfile::certs(&mut ca.as_slice()) {
        roots.add(c.unwrap()).unwrap();
    }
    let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
        .allow_unauthenticated()
        .build()
        .unwrap();
    let config = rustls::ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            rustls_pemfile::certs(&mut cert.as_slice())
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            rustls_pemfile::private_key(&mut key.as_slice())
                .unwrap()
                .unwrap(),
        )
        .unwrap();
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("https://localhost:{}", tcp.local_addr().unwrap().port());
    (
        OptionalTls {
            tcp,
            tls: tokio_rustls::TlsAcceptor::from(Arc::new(config)),
        },
        url,
    )
}

#[derive(Clone)]
struct Gateway {
    action: Arc<light_pingora::action_gateway::Runtime>,
    router: Arc<light_pingora::McpRouterRuntime>,
    security: Arc<SecurityRuntime>,
    pause: Arc<std::sync::atomic::AtomicBool>,
    arrived: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
    lose_response: Arc<std::sync::atomic::AtomicBool>,
    dispatches: Arc<std::sync::atomic::AtomicUsize>,
}
async fn gateway_call(
    State(g): State<Gateway>,
    ConnectInfo(peer): ConnectInfo<TestPeer>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    g.dispatches
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let token = headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .unwrap_or("");
    let user = match light_security::token_purpose::verify_with_purpose(
        &g.security,
        token,
        light_security::token_purpose::TokenUse::User,
        &[],
    )
    .await
    {
        Ok(u) => u,
        Err(_) => return StatusCode::UNAUTHORIZED.into_response(),
    };
    assert!(
        user.claims.get("exp").and_then(Value::as_i64).unwrap() > chrono::Utc::now().timestamp(),
        "expired bearer reached the protected Gateway call"
    );
    let action = match g
        .action
        .context(
            &g.security,
            &headers,
            Some(peer.0.as_str()).into(),
            body.clone(),
        )
        .await
    {
        Ok(Some(c)) => c,
        _ => return StatusCode::FORBIDDEN.into_response(),
    };
    assert!(
        action.inspect(TOOL).await.is_ok(),
        "synthetic inspected permit rejected"
    );
    if g.pause.swap(false, std::sync::atomic::Ordering::SeqCst) {
        g.arrived.notify_one();
        g.release.notified().await;
    }
    let request = light_pingora::McpHttpRequest {
        method: "POST".into(),
        path: "/mcp".into(),
        headers: headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap().to_owned()))
            .collect(),
        body: body.to_vec(),
    };
    let context = light_pingora::McpRequestContext {
        action: Some(action),
        auth: Some(user),
        authorization: headers
            .get("authorization")
            .map(|h| h.to_str().unwrap().to_owned()),
        ..Default::default()
    };
    match g.router.handle_request_with_context(request, context).await {
        Ok(Some(r)) => {
            if g.lose_response
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                return StatusCode::BAD_GATEWAY.into_response();
            }
            let mut response = (
                StatusCode::from_u16(r.status).unwrap(),
                r.body.buffered().unwrap_or_default().to_vec(),
            )
                .into_response();
            response
                .headers_mut()
                .insert("content-type", r.content_type.parse().unwrap());
            for (k, v) in r.headers {
                response.headers_mut().insert(
                    axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                    v.parse().unwrap(),
                );
            }
            response
        }
        _ => StatusCode::BAD_GATEWAY.into_response(),
    }
}

struct Fixture {
    lifecycle: crate::long_lifecycle::Reconciler,
    db: Db,
    dir: tempfile::TempDir,
    host: Uuid,
    user: Uuid,
    wf: Uuid,
    grant: Uuid,
    native: String,
    client: reqwest::Client,
    producer: Arc<crate::bound_mcp::Runtime>,
    gateway: Gateway,
    jobs: Vec<tokio::task::JoinHandle<()>>,
    original: String,
    scope: String,
    sink_gate: Arc<crate::verified_task_context::TestGate>,
    exchanges: Arc<std::sync::atomic::AtomicUsize>,
    exchange_status: Arc<std::sync::atomic::AtomicU16>,
    exchanged_token: Arc<std::sync::Mutex<String>>,
    original_exp: i64,
}
impl Fixture {
    async fn new() -> Self {
        Self::with_token_lifetime(3600, 7200).await
    }
    async fn with_token_lifetime(lifetime: i64, margin: i64) -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let db = Db::new().await;
        let dir = tempfile::tempdir().unwrap();
        let (workflow_peer, gateway_peer) = certs(dir.path());
        let host = Uuid::new_v4();
        let user = Uuid::new_v4();
        let wf = Uuid::new_v4();
        let grant = Uuid::new_v4();
        let security = Arc::new(SecurityRuntime::with_test_hs256_key("p02", KEY).await);
        let mut original_claims = user_claims(host, user, "portal-ui");
        let original_exp = chrono::Utc::now().timestamp() + lifetime;
        original_claims["exp"] = json!(original_exp);
        let original = signed(&original_claims);
        let exchanged = signed(&user_claims(host, user, "workflow-client"));
        let scope = signed(&app_claims(host, "gateway-a"));
        let workload = signed(&app_claims(host, "workflow-a"));
        std::fs::write(dir.path().join("scope"), format!("Bearer {scope}")).unwrap();
        std::fs::write(dir.path().join("keyring.json"),r#"{"activeKeyId":"p02","keys":{"p02":"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="}}"#).unwrap();
        let (gateway_listener, gateway_url) = optional_tls(dir.path()).await;
        // Issuer fixture is reached over owned HTTPS, returns signed synthetic
        // tokens and no real credentials. It is not live issuer qualification.
        let long = Arc::new(
            crate::long_authority::LongAuthority::open(
                &light_client::config::OAuthWorkflowLongConfig {
                    gateway_url: gateway_url.clone(),
                    provider_id: "p02".into(),
                    client_id: "workflow-client".into(),
                    client_secret: "synthetic-only".into(),
                    database_url_file: String::new(),
                    keyring_file: "keyring.json".into(),
                    ca_file: "ca.pem".into(),
                },
                dir.path(),
                db.pool.clone(),
                Some(Path::new("keyring.json")),
                &[],
            )
            .await
            .unwrap()
            .unwrap(),
        );
        let mut state = RuleApiState::for_verified_task_test(
            SecurityRuntime::with_test_hs256_key("p02", KEY).await,
            host,
            db.pool.clone(),
            long.clone(),
        );
        let sink_gate = Arc::new(crate::verified_task_context::TestGate::default());
        state.test_receiver_gate = Some(sink_gate.clone());
        let native_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let native = format!(
            "https://127.0.0.1:{}",
            native_listener.local_addr().unwrap().port()
        );
        native_listener.set_nonblocking(true).unwrap();
        let tls = axum_server::tls_rustls::RustlsConfig::from_pem_file(
            dir.path().join("server.pem"),
            dir.path().join("server.key"),
        )
        .await
        .unwrap();
        let native_job = tokio::spawn(async move {
            axum_server::from_tcp_rustls(native_listener, tls)
                .serve(
                    crate::mcp_api::routes()
                        .with_state(state)
                        .into_make_service(),
                )
                .await
                .unwrap();
        });
        let tokens = Arc::new(
            crate::run_token::RunTokenSelector::new(
                db.pool.clone(),
                None,
                Some(long.clone()),
                security.clone(),
                margin,
            )
            .unwrap(),
        );
        let authority = Arc::new(crate::run_authority::PerRunAuthority::new(
            db.pool.clone(),
            Some(long.clone()),
            tokens.clone(),
        ));
        let control_policy = RoutePolicy {
            issuer: "p02".into(),
            audience: "workflow".into(),
            host_id: host,
            apps: BTreeMap::from([(
                "gateway-a".into(),
                AppProfile {
                    origin: Origin::Gateway,
                    peer_sha256: vec![gateway_peer.clone()],
                    ca_trust: None,
                },
            )]),
            legacy_long_lived_app_keys: vec![],
            interactive_user_only: false,
        };
        let registration = workflow_action::GatewayRegistration {
            gateway_service: "gateway-a".into(),
            replica: Uuid::new_v4(),
        };
        let control_listener = light_axum::mtls::WorkloadListener::bind(
            &light_axum::mtls::Config {
                address: "127.0.0.1:0".into(),
                certificate_file: "server.pem".into(),
                private_key_file: "server.key".into(),
                client_ca_file: "ca.pem".into(),
            },
            dir.path(),
        )
        .await
        .unwrap();
        let control_url = format!(
            "https://localhost:{}",
            control_listener.bound_addr().unwrap().port()
        );
        let api = crate::action_api::router(crate::action_api::ActionApi {
            ledger: workflow_action::ledger::Ledger::new(db.pool.clone()),
            authority: authority.clone(),
            security: security.clone(),
            policy: control_policy,
            owners: BTreeMap::from([(gateway_peer, registration.clone())]),
            receivers: BTreeMap::new(),
        })
        .unwrap();
        let control_job = tokio::spawn(async move {
            axum::serve(
                control_listener,
                api.into_make_service_with_connect_info::<light_axum::mtls::Peer>(),
            )
            .await
            .unwrap();
        });
        let policy = RoutePolicy {
            issuer: "p02".into(),
            audience: "workflow".into(),
            host_id: host,
            apps: BTreeMap::from([(
                "workflow-a".into(),
                AppProfile {
                    origin: Origin::Workflow,
                    peer_sha256: vec![workflow_peer],
                    ca_trust: None,
                },
            )]),
            legacy_long_lived_app_keys: vec![],
            interactive_user_only: false,
        };
        let action = Arc::new(
            light_pingora::action_gateway::Runtime::new(
                light_pingora::action_gateway::Config {
                    gateway_url: format!("{gateway_url}/mcp"),
                    policy,
                    incoming_client_ca_file: "ca.pem".into(),
                    control: light_client::workflow_actions::Config {
                        base_url: control_url,
                        client_identity_file: "gateway-identity.pem".into(),
                        ca_file: "ca.pem".into(),
                        scope_token_file: "scope".into(),
                        owner: registration,
                    },
                    backend_ca_file: "ca.pem".into(),
                    backend_certificate_file: "gateway.pem".into(),
                    backend_key_file: "gateway.key".into(),
                    backend_scope_token_file: "scope".into(),
                    targets: BTreeMap::from([(
                        TOOL,
                        light_pingora::action_gateway::Target {
                            url: format!("{native}/mcp"),
                            contract_digest: contract(),
                            forward_user: true,
                            receipt: Default::default(),
                        },
                    )]),
                },
                dir.path(),
            )
            .unwrap(),
        );
        let config:light_pingora::McpRouterConfig=serde_json::from_value(json!({"tools":[{"name":"p02_capture_sink","apiType":"mcp","protocol":"https","path":"/mcp","targetHost":native,"backendMcpProtocol":"stateless","sessionIndependent":true,"backendCredentialMode":"caller","backendResource":"https://p02.invalid/native","inputSchema":{"type":"object","required":["issueUrl"],"additionalProperties":false,"properties":{"issueUrl":{"type":"string"}}},"toolMetadata":{"stableToolRef":TOOL,"contractDigest":contract(),"runtime":{"allowPrivateTargetHost":true}}}]})).unwrap();
        let gateway = Gateway {
            action,
            router: Arc::new(light_pingora::McpRouterRuntime::new(config).unwrap()),
            security: security.clone(),
            pause: Default::default(),
            arrived: Default::default(),
            release: Default::default(),
            lose_response: Default::default(),
            dispatches: Default::default(),
        };
        let token_workload = workload.clone();
        let exchanged_token = Arc::new(std::sync::Mutex::new(exchanged));
        let exchanges = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let exchange_status = Arc::new(std::sync::atomic::AtomicU16::new(200));
        let token_exchanged = exchanged_token.clone();
        let issuer_exchanges = exchanges.clone();
        let issuer_status = exchange_status.clone();
        let app=Router::new().route("/mcp",post(gateway_call)).with_state(gateway.clone())
            .route("/oauth2/p02/token",post(move |body:String|{
                let w=token_workload.clone(); let u=token_exchanged.clone(); let count=issuer_exchanges.clone(); let status=issuer_status.clone();
                async move {
                    let form: BTreeMap<String,String>=url::form_urlencoded::parse(body.as_bytes()).into_owned().collect();
                    let exchange=form.get("grant_type").map(String::as_str)!=Some("client_credentials");
                    if exchange { count.fetch_add(1,std::sync::atomic::Ordering::SeqCst); }
                    let code=if exchange {status.load(std::sync::atomic::Ordering::SeqCst)} else {200};
                    if code!=200 { return (StatusCode::from_u16(code).unwrap(),Json(json!({"error":"invalid_grant"}))).into_response(); }
                    let token=if exchange {u.lock().unwrap().clone()} else {w};
                    Json(json!({"access_token":token,"token_type":"Bearer","expires_in":3600,"scope":"portal.r portal.w"})).into_response()
                }
            }))
            .route("/oauth2/p02/long/v1/bindings",post(move |Json(args):Json<Value>|async move{let work=args["workId"].clone();Json(json!({"bindingId":Uuid::new_v4(),"workId":work,"workflowInstanceId":work,"hostId":host,"ownerUserId":user,"state":"PENDING","version":1}))}))
            .route("/oauth2/p02/workflow/bindings/{binding}/activate",post(|axum::extract::Path(binding):axum::extract::Path<Uuid>|async move{Json(json!({"bindingId":binding,"state":"ACTIVE","version":2}))}));
        let gateway_job = tokio::spawn(async move {
            axum::serve(
                gateway_listener,
                app.into_make_service_with_connect_info::<TestPeer>(),
            )
            .await
            .unwrap();
        });
        let ca = reqwest::Certificate::from_pem(&std::fs::read(dir.path().join("ca.pem")).unwrap())
            .unwrap();
        let client = reqwest::Client::builder()
            .add_root_certificate(ca)
            .build()
            .unwrap();
        let producer = Arc::new(
            crate::bound_mcp::Runtime::new(
                db.pool.clone(),
                Some(long.clone()),
                authority,
                tokens,
                format!("Bearer {workload}"),
                &crate::bound_mcp::Config {
                    gateway_url: format!("{gateway_url}/mcp"),
                    service_id: "workflow-a".into(),
                    client_identity_file: "workflow-identity.pem".into(),
                    ca_file: "ca.pem".into(),
                    scope_token_file: "unused".into(),
                    maximum_depth: 8,
                    request_byte_limit: 65536,
                    response_byte_limit: 65536,
                    cost_unit_limit: 1,
                },
                dir.path(),
            )
            .await
            .unwrap(),
        );
        let definition = json!({"document":{"dsl":"1.0.3","namespace":"p02","name":"native-proof","version":"1.0.0","metadata":{"developmentInputProfile":"capture-v1"}},"evaluate":{"language":"cel"},"use":{"mcpSessions":{"gateway":{"server":{"endpoint":{"uri":format!("{gateway_url}/mcp")},"transport":"http"}}}},"do":[{"capture":{"call":"mcp","metadata":{"workflowTool":{"toolId":TOOL,"toolVersion":"1.0.0","lightapiDigest":contract(),"contractDigest":contract(),"environment":"dev"}},"with":{"session":"gateway","tool":"p02_capture_sink","arguments":{"issueUrl":"${ .issueUrl }"}}}}]});
        sqlx::query("INSERT INTO wf_definition_t(host_id,wf_def_id,namespace,name,version,definition,lifecycle_status,source_revision) VALUES($1,$2,'p02','native-proof','1.0.0',$3,'PUBLISHED',1)").bind(host).bind(wf).bind(definition.to_string()).execute(&db.pool).await.unwrap();
        crate::publication_api::sync_grants_verified(&db.pool,&json!({"hostId":host,"wfDefId":wf,"sourceRevision":1,"actor":"synthetic-publisher","grants":[{"grantId":grant,"toolId":TOOL,"toolVersion":"1.0.0","lightapiDigest":contract(),"allowedEnvironments":["dev"]}]})).await.unwrap();
        Self {
            lifecycle: crate::long_lifecycle::Reconciler::new(db.pool.clone(), long),
            db,
            dir,
            host,
            user,
            wf,
            grant,
            native,
            client,
            producer,
            gateway,
            jobs: vec![native_job, control_job, gateway_job],
            original,
            scope,
            sink_gate,
            exchanges,
            exchange_status,
            exchanged_token,
            original_exp,
        }
    }
    async fn rpc(
        &self,
        name: &str,
        args: Value,
        user_token: &str,
        scope_token: &str,
        action: Option<Uuid>,
    ) -> Value {
        let mut req = self
            .client
            .post(format!("{}/mcp", self.native))
            .header("authorization", format!("Bearer {user_token}"))
            .header("x-scope-token", format!("Bearer {scope_token}"))
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2026-07-28")
            .header("mcp-method", "tools/call")
            .header("mcp-name", name);
        if let Some(action) = action {
            req = req.header("x-workflow-action", action.to_string());
        }
        req.json(&json!({"jsonrpc":"2.0","id":Uuid::new_v4(),"method":"tools/call","params":{"name":name,"arguments":args,"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}})).send().await.unwrap().json().await.unwrap()
    }
    async fn start(&self) -> (Uuid, Uuid, Uuid) {
        let result=self.rpc("workflow_start",json!({"workflowDefinitionId":self.wf,"input":{"issueUrl":"https://github.invalid/o/r/issues/1"},"idempotencyKey":Uuid::new_v4().to_string()}),&self.original,&self.scope,None).await;
        assert_eq!(
            result["result"]["isError"], false,
            "native start failed: {result}"
        );
        let row:(Uuid,Uuid,Option<Uuid>)=sqlx::query_as("SELECT workflow_instance_id,process_id,binding_id FROM workflow_invocation_t ORDER BY accepted_ts DESC LIMIT 1").fetch_one(&self.db.pool).await.unwrap();
        assert_eq!(row.2, None);
        let outer: i64 = sqlx::query_scalar("SELECT count(*) FROM workflow_tool_binding_t")
            .fetch_one(&self.db.pool)
            .await
            .unwrap();
        assert_eq!(outer, 0);
        assert_eq!(self.lifecycle.reconcile_once().await.unwrap(), 1);
        let task: Uuid =
            sqlx::query_scalar("SELECT task_id FROM workflow_claim_host_task_v1($1,30000)")
                .bind(Uuid::new_v4())
                .fetch_one(&self.db.pool)
                .await
                .unwrap();
        (row.0, row.1, task)
    }
    async fn count(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM p02_test_effect_t")
            .fetch_one(&self.db.pool)
            .await
            .unwrap()
    }
    async fn close(self) {
        for job in self.jobs {
            job.abort();
            let _ = job.await;
        }
        drop(self.producer);
        drop(self.gateway);
        drop(self.client);
        drop(self.dir);
        self.db.close().await;
    }
}

async fn wait_past_original_expiry(f: &Fixture) {
    while chrono::Utc::now().timestamp() <= f.original_exp {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

#[tokio::test]
#[ignore = "requires explicit owned P02 PostgreSQL"]
async fn demand_original_to_exchanged_lost_response_recovery() {
    let f = Fixture::with_token_lifetime(4, 0).await;
    let (_, process, task) = f.start().await;
    f.gateway
        .lose_response
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let args = json!({"arguments":{"issueUrl":"https://github.invalid/o/r/issues/1"}});
    assert!(
        f.producer
            .call(f.host, process, task, "p02_capture_sink", args.clone())
            .await
            .is_err()
    );
    assert_eq!(f.count().await, 1);
    assert_eq!(f.exchanges.load(std::sync::atomic::Ordering::SeqCst), 0);
    let before: Value = sqlx::query_scalar("SELECT binding FROM workflow_action_permit_t")
        .fetch_one(&f.db.pool)
        .await
        .unwrap();
    wait_past_original_expiry(&f).await;
    assert_eq!(f.exchanges.load(std::sync::atomic::Ordering::SeqCst), 0);
    let recovered = f
        .producer
        .call(f.host, process, task, "p02_capture_sink", args.clone())
        .await
        .expect("exchanged credential must recover completed original-token operation");
    assert_eq!(recovered["isError"], false);
    assert_eq!(
        recovered["structuredContent"]["structuredContent"]["creator"]["principal"],
        "portal-ui"
    );
    assert_eq!(f.exchanges.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(f.count().await, 1);
    assert_eq!(
        f.gateway
            .dispatches
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    let after: Value = sqlx::query_scalar("SELECT binding FROM workflow_action_permit_t")
        .fetch_one(&f.db.pool)
        .await
        .unwrap();
    assert_eq!(before, after, "original permit must remain immutable");
    assert!(
        f.producer
            .call(
                f.host,
                process,
                task,
                "p02_capture_sink",
                json!({"arguments":{"issueUrl":"altered"}})
            )
            .await
            .is_err()
    );
    for claims in [
        user_claims(f.host, Uuid::new_v4(), "workflow-client"),
        json!({"token_use":"app"}),
        json!({"exp":1}),
    ] {
        let mut current = user_claims(f.host, f.user, "workflow-client");
        for (k, v) in claims.as_object().unwrap() {
            current[k] = v.clone();
        }
        *f.exchanged_token.lock().unwrap() = signed(&current);
        assert!(
            f.producer
                .call(f.host, process, task, "p02_capture_sink", args.clone())
                .await
                .is_err()
        );
        assert_eq!(f.count().await, 1);
        assert_eq!(
            f.gateway
                .dispatches
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }
    f.close().await;
}

#[tokio::test]
#[ignore = "requires explicit owned P02 PostgreSQL"]
async fn demand_original_and_near_expiry_call_selection() {
    for (lifetime, margin, expected) in [(3600, 60, 0), (30, 60, 1), (2, 0, 1)] {
        let f = Fixture::with_token_lifetime(lifetime, margin).await;
        let (_, process, task) = f.start().await;
        assert_eq!(f.exchanges.load(std::sync::atomic::Ordering::SeqCst), 0);
        if lifetime == 2 {
            wait_past_original_expiry(&f).await;
        }
        assert_eq!(f.exchanges.load(std::sync::atomic::Ordering::SeqCst), 0);
        let result = f
            .producer
            .call(
                f.host,
                process,
                task,
                "p02_capture_sink",
                json!({"arguments":{"issueUrl":"https://github.invalid/o/r/issues/1"}}),
            )
            .await
            .unwrap();
        assert_eq!(result["isError"], false);
        assert_eq!(
            f.exchanges.load(std::sync::atomic::Ordering::SeqCst),
            expected
        );
        assert_eq!(f.count().await, 1);
        f.close().await;
    }
}

#[tokio::test]
#[ignore = "requires explicit owned P02 PostgreSQL"]
async fn demand_exchange_denial_fails_executor_and_transient_defers() {
    // Synthetic issuer statuses follow the existing LongBindingClient contract:
    // definitive 4xx denial vs bounded retryable 429/5xx handling.
    for (reason, status) in [
        ("locked-user", 403),
        ("revoked-registration", 400),
        ("transient", 503),
        ("rate-limit", 429),
    ] {
        let f = Fixture::with_token_lifetime(30, 60).await;
        let (run, _, task) = f.start().await;
        f.exchange_status
            .store(status, std::sync::atomic::Ordering::SeqCst);
        sqlx::query("UPDATE task_info_t SET locked='N',lease_owner=NULL,lease_expires_ts=NULL WHERE task_id=$1").bind(task).execute(&f.db.pool).await.unwrap();
        let executor = crate::executor::TaskExecutor::new(f.db.pool.clone());
        assert!(executor.bound_mcp.set(f.producer.clone()).is_ok());
        assert!(
            executor
                .verified_context_test_tick(Uuid::new_v4())
                .await
                .unwrap()
        );
        let state: String = sqlx::query_scalar(
            "SELECT state FROM workflow_invocation_t WHERE workflow_instance_id=$1",
        )
        .bind(run)
        .fetch_one(&f.db.pool)
        .await
        .unwrap();
        if status < 429 {
            assert_eq!(state, "FAILED", "{reason}");
        } else {
            assert_ne!(state, "FAILED", "transient is not authorization denial");
        }
        assert_eq!(f.count().await, 0);
        assert_eq!(
            f.gateway
                .dispatches
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(f.exchanges.load(std::sync::atomic::Ordering::SeqCst), 1);
        f.close().await;
    }
}

#[tokio::test]
#[ignore = "requires explicit owned P02 PostgreSQL"]
async fn demand_wait_and_local_work_cross_expiry_without_exchange() {
    let f = Fixture::with_token_lifetime(3, 0).await;
    let text: String =
        sqlx::query_scalar("SELECT definition FROM wf_definition_t WHERE wf_def_id=$1")
            .bind(f.wf)
            .fetch_one(&f.db.pool)
            .await
            .unwrap();
    let mut definition: Value = serde_json::from_str(&text).unwrap();
    let call = definition["do"][0].clone();
    definition["do"] =
        json!([{"wait":{"wait":"PT4S"}},{"local":{"set":{"localComplete":true}}},call]);
    sqlx::query("UPDATE wf_definition_t SET definition=$1 WHERE wf_def_id=$2")
        .bind(definition.to_string())
        .bind(f.wf)
        .execute(&f.db.pool)
        .await
        .unwrap();
    let (run, _, task) = f.start().await;
    sqlx::query(
        "UPDATE task_info_t SET locked='N',lease_owner=NULL,lease_expires_ts=NULL WHERE task_id=$1",
    )
    .bind(task)
    .execute(&f.db.pool)
    .await
    .unwrap();
    let executor = crate::executor::TaskExecutor::new(f.db.pool.clone());
    assert!(executor.bound_mcp.set(f.producer.clone()).is_ok());
    assert!(
        executor
            .verified_context_test_tick(Uuid::new_v4())
            .await
            .unwrap()
    );
    wait_past_original_expiry(&f).await;
    assert_eq!(f.exchanges.load(std::sync::atomic::Ordering::SeqCst), 0);
    while chrono::Utc::now().timestamp() <= f.original_exp + 2 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        executor
            .verified_context_test_tick(Uuid::new_v4())
            .await
            .unwrap()
    );
    assert_eq!(
        f.exchanges.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "timer wake and local set must not renew"
    );
    let state: String =
        sqlx::query_scalar("SELECT state FROM workflow_invocation_t WHERE workflow_instance_id=$1")
            .bind(run)
            .fetch_one(&f.db.pool)
            .await
            .unwrap();
    assert_ne!(state, "FAILED");
    assert_eq!(f.count().await, 0);
    assert!(
        executor
            .verified_context_test_tick(Uuid::new_v4())
            .await
            .unwrap()
    );
    assert_eq!(f.exchanges.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(f.count().await, 1);
    f.close().await;
}

#[tokio::test]
#[ignore = "requires explicit owned P02 PostgreSQL"]
async fn native_business_digest_ignores_jsonrpc_id_and_effect_deadline_is_live() {
    let f = Fixture::new().await;
    let (_, process, task) = f.start().await;
    f.sink_gate
        .pause
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let producer = f.producer.clone();
    let host = f.host;
    let call = tokio::spawn(async move {
        producer
            .call(
                host,
                process,
                task,
                "p02_capture_sink",
                json!({"arguments":{"issueUrl":"https://github.invalid/o/r/issues/1"}}),
            )
            .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        f.sink_gate.arrived.notified(),
    )
    .await
    .unwrap();
    let action: Uuid = sqlx::query_scalar(
        "SELECT action_id FROM workflow_verified_task_context_t WHERE task_id=$1",
    )
    .bind(task)
    .fetch_one(&f.db.pool)
    .await
    .unwrap();
    let token = signed(&user_claims(f.host, f.user, "workflow-client"));
    for _ in 0..2 {
        let reply = f
            .rpc(
                "p02_capture_sink",
                json!({"issueUrl":"https://github.invalid/o/r/issues/1"}),
                &token,
                &f.scope,
                Some(action),
            )
            .await;
        assert_eq!(reply["result"]["isError"], false);
        assert_eq!(f.count().await, 1);
    }
    f.sink_gate.release.notify_one();
    assert!(call.await.unwrap().is_ok());
    assert_eq!(f.count().await, 1);
    f.close().await;

    let f = Fixture::new().await;
    let (_, process, task) = f.start().await;
    sqlx::query("UPDATE task_info_t SET lease_expires_ts=clock_timestamp()+interval '2 seconds' WHERE task_id=$1").bind(task).execute(&f.db.pool).await.unwrap();
    f.sink_gate
        .pause_after
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let producer = f.producer.clone();
    let host = f.host;
    let call = tokio::spawn(async move {
        producer
            .call(
                host,
                process,
                task,
                "p02_capture_sink",
                json!({"arguments":{"issueUrl":"https://github.invalid/o/r/issues/1"}}),
            )
            .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        f.sink_gate.arrived.notified(),
    )
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    f.sink_gate.release.notify_one();
    let _ = call.await.unwrap();
    assert_eq!(f.count().await, 0);
    f.close().await;
}

#[tokio::test]
#[ignore = "requires explicit owned P02 PostgreSQL"]
async fn native_executor_dispatch_reaches_authenticated_sink() {
    let f = Fixture::new().await;
    let (_, _, task) = f.start().await;
    sqlx::query(
        "UPDATE task_info_t SET locked='N',lease_owner=NULL,lease_expires_ts=NULL WHERE task_id=$1",
    )
    .bind(task)
    .execute(&f.db.pool)
    .await
    .unwrap();
    let executor = crate::executor::TaskExecutor::new(f.db.pool.clone());
    assert!(executor.bound_mcp.set(f.producer.clone()).is_ok());
    assert!(
        executor
            .verified_context_test_tick(Uuid::new_v4())
            .await
            .unwrap()
    );
    assert_eq!(f.count().await, 1);
    let creator: Value = sqlx::query_scalar("SELECT creator FROM p02_test_effect_t")
        .fetch_one(&f.db.pool)
        .await
        .unwrap();
    assert_eq!(creator["principal"], "portal-ui");
    f.close().await;
}

#[tokio::test]
#[ignore = "requires explicit owned P02 PostgreSQL"]
async fn strict_tool_binding_context_and_revocation_effect_races() {
    // Synthetic accepted binding/dependency rows isolate the strict branch.
    // Tool publication/admission itself remains covered by Invoke handler gates.
    for source in ["workflow_tool_binding_t", "workflow_tool_dependency_t"] {
        for ordering in ["success", "revocation-first", "effect-first"] {
            let f = Fixture::new().await;
            let (run, process, task) = f.start().await;
            let binding = Uuid::new_v4();
            let outer_tool = Uuid::new_v4();
            let definition: String =
                sqlx::query_scalar("SELECT definition FROM wf_definition_t WHERE wf_def_id=$1")
                    .bind(f.wf)
                    .fetch_one(&f.db.pool)
                    .await
                    .unwrap();
            let parsed: Value = serde_json::from_str(&definition).unwrap();
            let endpoint = parsed
                .pointer("/use/mcpSessions/gateway/server/endpoint/uri")
                .unwrap()
                .as_str()
                .unwrap();
            sqlx::query("INSERT INTO workflow_tool_binding_t(host_id,binding_id,tool_id,wf_def_id,workflow_version,definition_digest,schema_digest,policy_digest,response_policy_digest,invocation_mode,sync_wait_ms,total_deadline_ms,execution_class,result_text_mode,idempotency_policy,delegation_policy,runtime_bounds,revision_status,binding_digest,approval_digest,source_binding_id,requested_by,requested_ts) SELECT host_id,$2,$3,wf_def_id,'1.0.0',definition_digest,$4,policy_digest,response_policy_digest,'async',1000,3600000,'interactive','compact-json','{}','{}','{}','approved',$4,$4,$2,$5,clock_timestamp() FROM workflow_invocation_t WHERE workflow_instance_id=$1")
                .bind(run).bind(binding).bind(outer_tool).bind(contract()).bind(f.user.to_string()).execute(&f.db.pool).await.unwrap();
            sqlx::query("INSERT INTO workflow_tool_dependency_t(host_id,outer_binding_id,nested_tool_id,nested_tool_version,contract_digest,compatibility_policy,authorization_tool_name,authorization_endpoint_key,authorization_policy_digest,lifecycle_status,dispatch_target) VALUES($1,$2,$3,'1.0.0',$4,'exact','p02_capture_sink','synthetic-gateway',$4,'active',$5)")
                .bind(f.host).bind(binding).bind(TOOL).bind(contract()).bind(json!({"endpoint":endpoint,"toolName":"p02_capture_sink"})).execute(&f.db.pool).await.unwrap();
            sqlx::query(
                "UPDATE workflow_invocation_t SET binding_id=$2 WHERE workflow_instance_id=$1",
            )
            .bind(run)
            .bind(binding)
            .execute(&f.db.pool)
            .await
            .unwrap();
            if ordering == "revocation-first" {
                f.sink_gate
                    .pause
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
            if ordering == "effect-first" {
                f.sink_gate
                    .pause_after
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
            let producer = f.producer.clone();
            let host = f.host;
            let call = tokio::spawn(async move {
                producer
                    .call(
                        host,
                        process,
                        task,
                        "p02_capture_sink",
                        json!({"arguments":{"issueUrl":"https://github.invalid/o/r/issues/1"}}),
                    )
                    .await
            });
            let sql = if source == "workflow_tool_binding_t" {
                "UPDATE workflow_tool_binding_t SET active=false,revision_status='revoked'"
            } else {
                "UPDATE workflow_tool_dependency_t SET active=false,lifecycle_status='revoked'"
            };
            if ordering != "success" {
                tokio::time::timeout(
                    std::time::Duration::from_secs(20),
                    f.sink_gate.arrived.notified(),
                )
                .await
                .unwrap();
                if ordering == "revocation-first" {
                    sqlx::query(sql).execute(&f.db.pool).await.unwrap();
                } else {
                    let pool = f.db.pool.clone();
                    let mut revoke =
                        tokio::spawn(async move { sqlx::query(sql).execute(&pool).await.unwrap() });
                    wait_for_revocation_lock(&f.db.pool, sql).await;
                    assert!(
                        tokio::time::timeout(std::time::Duration::from_millis(100), &mut revoke)
                            .await
                            .is_err()
                    );
                    f.sink_gate.release.notify_one();
                    assert!(call.await.unwrap().is_ok());
                    revoke.await.unwrap();
                    assert_eq!(f.count().await, 1);
                    f.close().await;
                    continue;
                }
                f.sink_gate.release.notify_one();
            }
            let result = call.await.unwrap();
            if ordering == "success" {
                assert!(result.is_ok());
                assert_eq!(f.count().await, 1);
                let context: Value = sqlx::query_scalar(
                    "SELECT context FROM workflow_verified_task_context_t WHERE task_id=$1",
                )
                .bind(task)
                .fetch_one(&f.db.pool)
                .await
                .unwrap();
                assert_eq!(context["authority"]["source"], "toolBinding");
            } else {
                assert_eq!(f.count().await, 0);
            }
            f.close().await;
        }
    }
}

#[tokio::test]
#[ignore = "requires explicit owned P02 PostgreSQL"]
async fn signed_refresh_replays_start_and_preserves_original_creator() {
    let f = Fixture::new().await;
    let args = json!({"workflowDefinitionId":f.wf,"input":{"issueUrl":"https://github.invalid/o/r/issues/1"},"idempotencyKey":Uuid::new_v4().to_string()});
    let original = f
        .rpc("workflow_start", args.clone(), &f.original, &f.scope, None)
        .await;
    assert_eq!(original["result"]["isError"], false);
    let before: Value = sqlx::query_scalar("SELECT creator FROM workflow_verified_invocation_t")
        .fetch_one(&f.db.pool)
        .await
        .unwrap();
    let mut claims = user_claims(f.host, f.user, "portal-ui");
    claims["exp"] = json!(chrono::Utc::now().timestamp() + 7200);
    claims["iat"] = json!(chrono::Utc::now().timestamp());
    claims["jti"] = json!(Uuid::new_v4());
    let replay = f
        .rpc(
            "workflow_start",
            args.clone(),
            &signed(&claims),
            &f.scope,
            None,
        )
        .await;
    assert_eq!(
        replay["result"]["isError"], false,
        "refreshed signed token must replay accepted start"
    );
    let after: Value = sqlx::query_scalar("SELECT creator FROM workflow_verified_invocation_t")
        .fetch_one(&f.db.pool)
        .await
        .unwrap();
    assert_eq!(before, after);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM workflow_invocation_t")
        .fetch_one(&f.db.pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    claims["exp"] = json!(1);
    let expired = f
        .rpc("workflow_start", args, &signed(&claims), &f.scope, None)
        .await;
    assert!(expired["error"].is_object() || expired["result"]["isError"] == true);
    f.close().await;
}

#[tokio::test]
#[ignore = "requires explicit owned P02 PostgreSQL"]
async fn lost_gateway_response_recovers_success_without_redispatch() {
    let f = Fixture::new().await;
    let (_, process, task) = f.start().await;
    f.gateway
        .lose_response
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let args = json!({"arguments":{"issueUrl":"https://github.invalid/o/r/issues/1"}});
    assert!(
        f.producer
            .call(f.host, process, task, "p02_capture_sink", args.clone())
            .await
            .is_err()
    );
    assert_eq!(f.count().await, 1);
    let recovered = f
        .producer
        .call(f.host, process, task, "p02_capture_sink", args)
        .await
        .expect("reply-loss recovery must succeed");
    assert_eq!(recovered["isError"], false);
    assert_eq!(f.count().await, 1);
    assert_eq!(
        f.gateway
            .dispatches
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    f.close().await;
}

#[tokio::test]
#[ignore = "requires explicit owned P02 PostgreSQL"]
async fn native_null_binding_signed_transport_creator_and_retry() {
    let f = Fixture::new().await;
    let (run, process, task) = f.start().await;
    let args = json!({"name":"p02_capture_sink","arguments":{"issueUrl":"https://github.invalid/o/r/issues/1"}});
    let reply = f
        .producer
        .call(f.host, process, task, "p02_capture_sink", args.clone())
        .await
        .expect("real producer/Gateway/native call must succeed");
    assert_eq!(f.count().await, 1);
    assert!(reply.to_string().contains("portal-ui"));
    assert!(!reply.to_string().contains("workflow-client"));
    let creator: Value =
        sqlx::query_scalar("SELECT creator FROM p02_test_effect_t WHERE run_id=$1")
            .bind(run)
            .fetch_one(&f.db.pool)
            .await
            .unwrap();
    assert_eq!(creator["userId"], json!(f.user));
    let before: Value =
        sqlx::query_scalar("SELECT context FROM workflow_verified_task_context_t WHERE task_id=$1")
            .bind(task)
            .fetch_one(&f.db.pool)
            .await
            .unwrap();
    let retried = f
        .producer
        .call(f.host, process, task, "p02_capture_sink", args)
        .await
        .expect("completed operation retry must recover successfully");
    assert_eq!(retried["isError"], false);
    for field in ["resultType", "structuredContent", "content"] {
        assert_eq!(
            retried[field], reply[field],
            "retry must recover the same operation result"
        );
    }
    assert_eq!(f.count().await, 1);
    let after: Value =
        sqlx::query_scalar("SELECT context FROM workflow_verified_task_context_t WHERE task_id=$1")
            .bind(task)
            .fetch_one(&f.db.pool)
            .await
            .unwrap();
    assert_eq!(before, after);
    let permits: i64 =
        sqlx::query_scalar("SELECT count(*) FROM workflow_action_permit_t WHERE attempt_id=$1")
            .bind(task)
            .fetch_one(&f.db.pool)
            .await
            .unwrap();
    assert_eq!(permits, 1);
    f.close().await;
}

#[tokio::test]
#[ignore = "requires explicit owned P02 PostgreSQL"]
async fn native_effect_transaction_holds_tool_grant_fence() {
    for table in ["workflow_tool_grant_t"] {
        let f = Fixture::new().await;
        let (_, process, task) = f.start().await;
        f.sink_gate
            .pause_after
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let producer = f.producer.clone();
        let host = f.host;
        let call = tokio::spawn(async move {
            producer
                .call(
                    host,
                    process,
                    task,
                    "p02_capture_sink",
                    json!({"arguments":{"issueUrl":"https://github.invalid/o/r/issues/1"}}),
                )
                .await
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            f.sink_gate.arrived.notified(),
        )
        .await
        .unwrap();
        let pool = f.db.pool.clone();
        let sql = format!("UPDATE {table} SET active=false");
        let expected_sql = sql.clone();
        let mut revoke =
            tokio::spawn(async move { sqlx::query(&sql).execute(&pool).await.unwrap() });
        wait_for_revocation_lock(&f.db.pool, &expected_sql).await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut revoke)
                .await
                .is_err(),
            "revocation escaped effect transaction fence"
        );
        f.sink_gate.release.notify_one();
        assert!(call.await.unwrap().is_ok());
        revoke.await.unwrap();
        assert_eq!(f.count().await, 1);
        f.close().await;
    }
}

#[tokio::test]
#[ignore = "requires explicit owned P02 PostgreSQL"]
async fn native_transport_revocation_wins_effect_fence() {
    let f = Fixture::new().await;
    let (_, process, task) = f.start().await;
    f.gateway
        .pause
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let producer = f.producer.clone();
    let host = f.host;
    let call = tokio::spawn(async move {
        producer
            .call(
                host,
                process,
                task,
                "p02_capture_sink",
                json!({"arguments":{"issueUrl":"https://github.invalid/o/r/issues/1"}}),
            )
            .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        f.gateway.arrived.notified(),
    )
    .await
    .unwrap();
    sqlx::query("UPDATE workflow_tool_grant_t SET active=false WHERE grant_id=$1")
        .bind(f.grant)
        .execute(&f.db.pool)
        .await
        .unwrap();
    f.gateway.release.notify_one();
    assert!(call.await.unwrap().is_err());
    assert_eq!(f.count().await, 0);
    f.close().await;
}

#[tokio::test]
#[ignore = "requires explicit owned P02 PostgreSQL"]
async fn native_receiver_signed_transport_denial_matrix() {
    let f = Fixture::new().await;
    let (_, process, task) = f.start().await;
    f.sink_gate
        .pause
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let producer = f.producer.clone();
    let host = f.host;
    let business = json!({"issueUrl":"https://github.invalid/o/r/issues/1"});
    let args = business.clone();
    let call = tokio::spawn(async move {
        producer
            .call(
                host,
                process,
                task,
                "p02_capture_sink",
                json!({"arguments":args}),
            )
            .await
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        f.sink_gate.arrived.notified(),
    )
    .await
    .unwrap();
    let action: Uuid = sqlx::query_scalar(
        "SELECT action_id FROM workflow_verified_task_context_t WHERE task_id=$1",
    )
    .bind(task)
    .fetch_one(&f.db.pool)
    .await
    .unwrap();
    let exchanged = signed(&user_claims(f.host, f.user, "workflow-client"));
    let mut users = vec![("original-claims", f.original.clone())];
    for (label, change) in [
        ("wrong-host", json!({"host":Uuid::new_v4()})),
        ("noncreator", json!({"user_id":Uuid::new_v4()})),
        ("app-spoof", json!({"token_use":"app"})),
        ("client-spoof", json!({"grant_type":"client_credentials"})),
        ("expired", json!({"exp":1})),
        ("malformed-purpose", json!({"token_use":["user"]})),
        ("unknown-purpose", json!({"token_use":"service"})),
    ] {
        let mut c = user_claims(f.host, f.user, "workflow-client");
        for (key, value) in change.as_object().unwrap() {
            c[key] = value.clone();
        }
        users.push((label, signed(&c)));
    }
    for field in ["token_use", "user_id"] {
        let mut c = user_claims(f.host, f.user, "workflow-client");
        c.as_object_mut().unwrap().remove(field);
        users.push((field, signed(&c)));
    }
    for (label, token) in users {
        let reply = f
            .rpc(
                "p02_capture_sink",
                business.clone(),
                &token,
                &f.scope,
                Some(action),
            )
            .await;
        assert!(
            reply["error"].is_object() || reply["result"]["isError"] == true,
            "denial case {label} accepted"
        );
        assert_eq!(f.count().await, 0, "denial case {label} produced effect");
    }
    for (label, reference, args) in [
        ("missing-action", None, business.clone()),
        ("forged-action", Some(Uuid::new_v4()), business.clone()),
        (
            "changed-arguments",
            Some(action),
            json!({"issueUrl":"https://github.invalid/altered"}),
        ),
    ] {
        let reply = f
            .rpc("p02_capture_sink", args, &exchanged, &f.scope, reference)
            .await;
        assert!(
            reply["error"].is_object() || reply["result"]["isError"] == true,
            "denial case {label} accepted"
        );
        assert_eq!(f.count().await, 0);
    }
    let mut expired_scope = app_claims(f.host, "gateway-a");
    expired_scope["exp"] = json!(1);
    let reply = f
        .rpc(
            "p02_capture_sink",
            business.clone(),
            &exchanged,
            &signed(&expired_scope),
            Some(action),
        )
        .await;
    assert!(reply["error"].is_object() || reply["result"]["isError"] == true);
    assert_eq!(f.count().await, 0);
    sqlx::query("UPDATE workflow_tool_grant_t SET active=false WHERE grant_id=$1")
        .bind(f.grant)
        .execute(&f.db.pool)
        .await
        .unwrap();
    f.sink_gate.release.notify_one();
    assert!(call.await.unwrap().is_err());
    assert_eq!(f.count().await, 0);
    f.close().await;
}

#[tokio::test]
#[ignore = "requires explicit owned P02 PostgreSQL"]
async fn native_receiver_current_authority_and_task_fences() {
    for (label, sql) in [
        (
            "lease",
            "UPDATE task_info_t SET lease_fencing_token=lease_fencing_token+1",
        ),
        (
            "expired-lease",
            "UPDATE task_info_t SET lease_expires_ts=clock_timestamp()-interval '1 second'",
        ),
        (
            "wrong-run",
            "UPDATE task_info_t SET wf_instance_id='00000000-0000-4000-8000-000000000001'",
        ),
        (
            "wrong-task",
            "UPDATE task_info_t SET wf_task_id='forged-task'",
        ),
        (
            "wrong-process",
            "UPDATE workflow_verified_task_context_t SET context=jsonb_set(context,'{processId}',to_jsonb(gen_random_uuid()::text))",
        ),
        (
            "wrong-tool",
            "UPDATE process_info_t SET definition_snapshot=jsonb_set(definition_snapshot,'{do,0,capture,metadata,workflowTool,toolId}',to_jsonb('00000000-0000-4000-8000-000000000001'::text))",
        ),
        (
            "wrong-alias",
            "UPDATE process_info_t SET definition_snapshot=jsonb_set(definition_snapshot,'{do,0,capture,with,tool}',to_jsonb('forged-alias'::text))",
        ),
        (
            "altered-creator",
            "UPDATE workflow_verified_invocation_t SET creator=jsonb_set(creator,'{principal}',to_jsonb('forged-creator'::text))",
        ),
        (
            "cancelled",
            "UPDATE workflow_invocation_t SET cancel_requested_ts=clock_timestamp()",
        ),
        (
            "run-generation",
            "UPDATE workflow_action_authority_t SET run_generation=run_generation+1",
        ),
        (
            "budget-generation",
            "UPDATE workflow_action_authority_t SET budget_generation=budget_generation+1",
        ),
        (
            "authority-revoked",
            "UPDATE workflow_action_authority_t SET active=false",
        ),
        (
            "deadline",
            "UPDATE workflow_action_authority_t SET deadline=clock_timestamp()-interval '1 second'",
        ),
        ("missing-grant", "DELETE FROM workflow_tool_grant_t"),
        (
            "grant-generation",
            "UPDATE workflow_definition_grant_sync_t SET source_revision=source_revision+1",
        ),
        (
            "contract",
            "UPDATE process_info_t SET definition_snapshot=jsonb_set(definition_snapshot,'{do,0,capture,metadata,workflowTool,contractDigest}',to_jsonb('sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff'::text))",
        ),
    ] {
        let f = Fixture::new().await;
        let (_, process, task) = f.start().await;
        f.sink_gate
            .pause
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let producer = f.producer.clone();
        let host = f.host;
        let call = tokio::spawn(async move {
            producer
                .call(
                    host,
                    process,
                    task,
                    "p02_capture_sink",
                    json!({"arguments":{"issueUrl":"https://github.invalid/o/r/issues/1"}}),
                )
                .await
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(20),
            f.sink_gate.arrived.notified(),
        )
        .await
        .unwrap();
        sqlx::query(sql).execute(&f.db.pool).await.unwrap();
        f.sink_gate.release.notify_one();
        let _ = call.await.unwrap();
        assert_eq!(f.count().await, 0, "fence case {label} produced effect");
        f.close().await;
    }
}

#[tokio::test]
async fn strict_signed_provenance_rejects_fallback_app_and_malformed_purpose() {
    let security = SecurityRuntime::with_test_hs256_key("p02", KEY).await;
    let host = Uuid::new_v4();
    let user = Uuid::new_v4();
    let valid = user_claims(host, user, "portal-ui");
    assert!(
        crate::verified_caller::verify_user(&security, &signed(&valid), host)
            .await
            .is_ok()
    );
    let mut cases = Vec::new();
    for purpose in [json!("app"), json!(null), json!("invalid"), json!(["user"])] {
        let mut v = valid.clone();
        v["token_use"] = purpose;
        cases.push(v);
    }
    let mut missing = valid.clone();
    missing.as_object_mut().unwrap().remove("token_use");
    cases.push(missing);
    let mut fallback = valid.clone();
    fallback.as_object_mut().unwrap().remove("user_id");
    cases.push(fallback);
    let mut conflict = valid.clone();
    conflict["userId"] = json!(Uuid::new_v4());
    cases.push(conflict);
    let mut expired = valid.clone();
    expired["exp"] = json!(1);
    cases.push(expired);
    let mut client = valid.clone();
    client["grant_type"] = json!("client_credentials");
    cases.push(client);
    for claims in cases {
        assert!(
            crate::verified_caller::verify_user(&security, &signed(&claims), host)
                .await
                .is_err()
        );
    }
    assert!(
        crate::verified_caller::verify_user(&security, &signed(&valid), Uuid::new_v4())
            .await
            .is_err()
    );
    use base64::Engine as _;
    let encode = |s: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s);
    let payload = serde_json::to_string(&valid).unwrap();
    let duplicate = payload.replacen("{", "{\"token_use\":\"app\",", 1);
    let message = format!(
        "{}.{}",
        encode(br#"{"alg":"HS256","kid":"p02","typ":"JWT"}"#),
        encode(duplicate.as_bytes())
    );
    let signature = jsonwebtoken::crypto::sign(
        message.as_bytes(),
        &jsonwebtoken::EncodingKey::from_secret(KEY),
        jsonwebtoken::Algorithm::HS256,
    )
    .unwrap();
    assert!(
        crate::verified_caller::verify_user(&security, &format!("{message}.{signature}"), host)
            .await
            .is_err()
    );
}
