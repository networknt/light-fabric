//! Neutral regressions for configured Workflow mTLS and guarded rustls transport.
use super::*;
use axum::{Json, Router, extract::State, http::HeaderMap, routing::post};
use bytes::Bytes;
use light_pingora::guarded_http::ActionWriteGuard;
use light_security::{
    SecurityRuntime,
    dual_identity::{AppProfile, Origin, RoutePolicy},
};
use pingora::{
    connectors::{ConnectorOptions, http::Connector},
    http::RequestHeader,
    upstreams::peer::HttpPeer,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use uuid::Uuid;
use workflow_action::{
    guard::{SendGuard, SendState},
    *,
};
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
            "/CN=Generic Transport Synthetic CA",
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

fn decision() -> Decision {
    let digest = request_digest("POST", "https://target/tool", "tool", b"{}");
    Decision {
        binding: Binding {
            host_id: Uuid::now_v7(),
            user_id: Uuid::now_v7(),
            grant_id: Uuid::now_v7(),
            run_id: Uuid::now_v7(),
            action_id: Uuid::now_v7(),
            attempt_id: Uuid::now_v7(),
            calling_app: "workflow".into(),
            request_digest: digest.clone(),
            request_bytes: 2,
            response_byte_limit: 1024,
            cost_unit_limit: 1,
            tool_ref: Uuid::now_v7(),
            target: "https://target/tool".into(),
            contract_digest: digest.clone(),
            policy_digest: digest.clone(),
            disclosure_digest: digest.clone(),
            claims_digest: digest,
            grant_generation: 1,
            run_generation: 1,
            budget_generation: 1,
            action_generation: 1,
            execution_class: ExecutionClass::Standard,
            depth: 0,
            maximum_depth: 4,
            parent_action_id: None,
            deadline: chrono::Utc::now() + chrono::Duration::minutes(5),
        },
        decision_id: Uuid::now_v7(),
        owner: Owner {
            gateway_service: "gateway".into(),
            replica: Uuid::now_v7(),
            boot: Uuid::now_v7(),
            fencing_generation: 1,
        },
        generation: 1,
        lease_ms: 5000,
    }
}

fn armed(expired: bool) -> Arc<SendGuard> {
    let d = decision();
    let now = Instant::now();
    let g = Arc::new(
        SendGuard::new(
            if expired {
                now - Duration::from_secs(6)
            } else {
                now
            },
            d.clone(),
        )
        .unwrap(),
    );
    g.acknowledge(&d, true).unwrap();
    g
}
fn request() -> RequestHeader {
    let mut r = RequestHeader::build("POST", b"/effect", Some(1)).unwrap();
    r.insert_header("Host", "localhost").unwrap();
    r
}
async fn server(dir: &Path, app: Router) -> (String, tokio::task::JoinHandle<()>) {
    let listener = light_axum::mtls::WorkloadListener::bind(
        &light_axum::mtls::Config {
            address: "127.0.0.1:0".into(),
            certificate_file: "server.pem".into(),
            private_key_file: "server.key".into(),
            client_ca_file: "ca.pem".into(),
        },
        dir,
    )
    .await
    .unwrap();
    let url = format!(
        "https://localhost:{}",
        listener.bound_addr().unwrap().port()
    );
    let job = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (url, job)
}

#[tokio::test]
async fn configured_client_mtls() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let dir = tempfile::tempdir().unwrap();
    certs(dir.path());
    let (url, job) = server(
        dir.path(),
        Router::new().route("/mcp", post(|| async { "configured-client-ok" })),
    )
    .await;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://127.0.0.1/unused")
        .unwrap();
    let security =
        Arc::new(SecurityRuntime::with_test_hs256_key("neutral", b"synthetic-only").await);
    let tokens = Arc::new(
        crate::run_token::RunTokenSelector::new(pool.clone(), None, None, security, 0).unwrap(),
    );
    let authority = Arc::new(crate::run_authority::PerRunAuthority::new(
        pool.clone(),
        None,
        tokens.clone(),
    ));
    let mut config = Config {
        gateway_url: format!("{url}/mcp"),
        service_id: "neutral-workflow".into(),
        client_identity_file: "workflow-identity.pem".into(),
        ca_file: "ca.pem".into(),
        scope_token_file: "unused".into(),
        maximum_depth: 4,
        request_byte_limit: 1024,
        response_byte_limit: 1024,
        cost_unit_limit: 1,
    };
    let runtime = Runtime::new(
        pool.clone(),
        None,
        authority.clone(),
        tokens.clone(),
        "Bearer synthetic".into(),
        &config,
        dir.path(),
    )
    .await
    .unwrap();
    assert_eq!(
        runtime
            .client
            .post(&config.gateway_url)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "configured-client-ok"
    );
    config.client_identity_file = "".into();
    let anonymous = Runtime::new(
        pool,
        None,
        authority,
        tokens,
        "Bearer synthetic".into(),
        &config,
        dir.path(),
    )
    .await
    .unwrap();
    assert!(
        anonymous
            .client
            .post(&config.gateway_url)
            .send()
            .await
            .is_err(),
        "server requires a CA-verified client certificate"
    );
    job.abort();
}

#[tokio::test]
async fn guarded_tls_capability() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let dir = tempfile::tempdir().unwrap();
    certs(dir.path());
    let (url, job) = server(
        dir.path(),
        Router::new().route("/effect", post(|| async { "{}" })),
    )
    .await;
    let connector = Connector::new(Some(ConnectorOptions {
        ca_file: Some(dir.path().join("ca.pem").to_string_lossy().into_owned()),
        cert_key_file: Some((
            dir.path()
                .join("gateway.pem")
                .to_string_lossy()
                .into_owned(),
            dir.path()
                .join("gateway.key")
                .to_string_lossy()
                .into_owned(),
        )),
        ..ConnectorOptions::new(4)
    }));
    let port = url::Url::parse(&url).unwrap().port().unwrap();
    let mut peer = HttpPeer::new(
        (std::net::Ipv4Addr::LOCALHOST, port),
        true,
        "localhost".into(),
    );
    peer.options.set_http_version(1, 1);
    let guard = armed(false);
    let response = light_pingora::guarded_http::execute(
        &connector,
        &peer,
        request(),
        Bytes::from_static(b"{}"),
        guard.clone(),
        1024,
        Duration::from_secs(3),
    )
    .await
    .unwrap();
    assert_eq!(response.header.status.as_u16(), 200);
    assert_eq!(guard.state(), SendState::Started);
    let (unsupported, mut observer) = tokio::io::duplex(1024);
    let mut session = pingora::protocols::http::v1::client::HttpSession::new(Box::new(unsupported));
    let guard = armed(false);
    assert!(
        session
            .write_request_header_guarded(
                Box::new(request()),
                Some(Arc::new(ActionWriteGuard(guard.clone())))
            )
            .await
            .is_err()
    );
    assert!(guard.abort_not_initiated());
    use tokio::io::AsyncReadExt;
    assert!(
        tokio::time::timeout(Duration::from_millis(50), observer.read_u8())
            .await
            .is_err(),
        "unsupported transport wrote request bytes"
    );
    job.abort();
}

#[derive(Clone)]
struct Control {
    mismatch: Arc<std::sync::atomic::AtomicBool>,
}
async fn control(
    State(state): State<Control>,
    axum::extract::Path(method): axum::extract::Path<String>,
    Json(args): Json<Value>,
) -> Json<Value> {
    if method == "register-owner" {
        return Json(
            json!({"gatewayService":args["gatewayService"],"replica":args["replica"],"boot":args["boot"],"fencingGeneration":1}),
        );
    }
    if method == "begin-dispatch" {
        return Json(json!({"decision":args,"newPermission":true}));
    }
    if method == "complete" {
        return Json(json!({}));
    }
    let reference: ActionReference = serde_json::from_value(args["reference"].clone()).unwrap();
    let mut d = decision();
    d.owner = serde_json::from_value(args["owner"].clone()).unwrap();
    d.binding.host_id = reference.host_id;
    d.binding.action_id = reference.action_id;
    d.binding.calling_app = reference.calling_app;
    d.binding.request_digest = reference.request_digest;
    d.binding.tool_ref = reference.tool_ref;
    d.binding.target = reference.target;
    d.binding.contract_digest = reference.contract_digest;
    d.binding.request_bytes = 1024;
    if state.mismatch.load(std::sync::atomic::Ordering::SeqCst) {
        d.binding.action_id = Uuid::new_v4();
    }
    Json(if method == "inspect" {
        serde_json::to_value(d.binding).unwrap()
    } else {
        serde_json::to_value(d).unwrap()
    })
}

#[tokio::test]
async fn inspected_action_id() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let dir = tempfile::tempdir().unwrap();
    let (workflow_peer, _) = certs(dir.path());
    std::fs::write(dir.path().join("scope"), "Bearer synthetic-scope").unwrap();
    let state = Control {
        mismatch: Default::default(),
    };
    let (control_url, control_job) = server(
        dir.path(),
        Router::new()
            .route("/internal/workflow/actions/{method}", post(control))
            .with_state(state.clone()),
    )
    .await;
    let received = Arc::new(std::sync::Mutex::new(Vec::<HeaderMap>::new()));
    let sink = received.clone();
    let (upstream, upstream_job) = server(
        dir.path(),
        Router::new().route(
            "/effect",
            post(move |headers: HeaderMap| {
                let sink = sink.clone();
                async move {
                    sink.lock().unwrap().push(headers);
                    "{}"
                }
            }),
        ),
    )
    .await;
    let host = Uuid::new_v4();
    let tool = Uuid::new_v4();
    let action = Uuid::new_v4();
    let key = b"neutral-action-synthetic-key-only";
    let security = SecurityRuntime::with_test_hs256_key("neutral", key).await;
    let runtime = Arc::new(
        light_pingora::action_gateway::Runtime::new(
            light_pingora::action_gateway::Config {
                gateway_url: "https://gateway.invalid/mcp".into(),
                policy: RoutePolicy {
                    issuer: "neutral".into(),
                    audience: "workflow".into(),
                    host_id: host,
                    apps: BTreeMap::from([(
                        "workflow-a".into(),
                        AppProfile {
                            origin: Origin::Workflow,
                            peer_sha256: vec![workflow_peer.clone()],
                            ca_trust: None,
                        },
                    )]),
                    legacy_long_lived_app_keys: vec![],
                    interactive_user_only: false,
                },
                incoming_client_ca_file: "ca.pem".into(),
                control: light_client::workflow_actions::Config {
                    base_url: control_url,
                    client_identity_file: "gateway-identity.pem".into(),
                    ca_file: "ca.pem".into(),
                    scope_token_file: "scope".into(),
                    owner: GatewayRegistration {
                        gateway_service: "gateway-a".into(),
                        replica: Uuid::new_v4(),
                    },
                },
                backend_ca_file: "ca.pem".into(),
                backend_certificate_file: "gateway.pem".into(),
                backend_key_file: "gateway.key".into(),
                backend_scope_token_file: "scope".into(),
                targets: BTreeMap::from([(
                    tool,
                    light_pingora::action_gateway::Target {
                        url: format!("{upstream}/effect"),
                        contract_digest: format!("sha256:{}", "a".repeat(64)),
                        forward_user: true,
                        receipt: Default::default(),
                    },
                )]),
            },
            dir.path(),
        )
        .unwrap(),
    );
    let mut jwt_header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
    jwt_header.kid = Some("neutral".into());
    let app = jsonwebtoken::encode(&jwt_header, &json!({"iss":"neutral","aud":"workflow","host":host,"sid":"workflow-a","client_id":"workflow-a","env":"dev","token_use":"app","exp":chrono::Utc::now().timestamp()+3600}), &jsonwebtoken::EncodingKey::from_secret(key)).unwrap();
    let user = Uuid::new_v4();
    let user_token = jsonwebtoken::encode(&jwt_header, &json!({"iss":"neutral","aud":"workflow","host":host,"uid":user,"user_id":user,"sub":user,"client_id":"neutral-ui","token_use":"user","exp":chrono::Utc::now().timestamp()+3600}), &jsonwebtoken::EncodingKey::from_secret(key)).unwrap();
    let user_header = format!("Bearer {user_token}");
    let mut headers = HeaderMap::new();
    headers.insert("authorization", user_header.parse().unwrap());
    headers.insert("x-scope-token", format!("Bearer {app}").parse().unwrap());
    headers.insert("x-workflow-action", action.to_string().parse().unwrap());
    let context = runtime
        .context(
            &security,
            &headers,
            Some(workflow_peer.as_str()).into(),
            Bytes::from_static(b"{}"),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(context.inspect(tool).await.unwrap().action_id, action);
    headers.insert(
        "x-workflow-action",
        Uuid::new_v4().to_string().parse().unwrap(),
    );
    headers.insert("authorization", "Bearer spoofed".parse().unwrap());
    assert_eq!(
        context
            .execute(
                tool,
                "POST",
                &format!("{upstream}/effect"),
                headers.clone(),
                Bytes::from_static(b"{}")
            )
            .await
            .unwrap()
            .header
            .status
            .as_u16(),
        200
    );
    assert_eq!(
        received.lock().unwrap()[0]["x-workflow-action"],
        action.to_string()
    );
    assert_eq!(received.lock().unwrap()[0]["authorization"], user_header);
    state
        .mismatch
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(context.inspect(tool).await.is_err());
    assert!(
        context
            .execute(
                tool,
                "POST",
                &format!("{upstream}/effect"),
                headers,
                Bytes::from_static(b"{}")
            )
            .await
            .is_err()
    );
    assert_eq!(
        received.lock().unwrap().len(),
        1,
        "mismatched inspected action must never reach upstream"
    );
    control_job.abort();
    upstream_job.abort();
}
