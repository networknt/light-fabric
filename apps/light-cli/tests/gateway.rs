//! `light gateway check` against a stand-in Gateway.
//!
//! The Light CLI is an open, downloadable program, so it is a public client on the wire: HTTPS,
//! and the signed-in user's access token in `authorization`, and nothing else. The stand-in
//! records what it received, so the tests assert on what crossed the wire.

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use light_cli::config::{CliConfig, Secret};
use light_cli::error::{CliError, exit};
use light_cli::gateway;
use tempfile::TempDir;

/// What the stand-in does with a call. `Some(status)` refuses every call with it.
type Refusal = Arc<Mutex<Option<u16>>>;

struct Setup {
    dir: TempDir,
    cfg: CliConfig,
    server: TlsServer,
    refuse: Refusal,
    ca: std::path::PathBuf,
}

async fn setup() -> Setup {
    let dir = TempDir::new().unwrap();
    let (ca, cert, key) = write_server_pki(dir.path());
    let refuse: Refusal = Arc::default();
    let switch = Arc::clone(&refuse);
    let server = start_tls_server(&cert, &key, move |req| {
        if let Some(status) = *switch.lock().unwrap() {
            return Reply { status, headers: Vec::new(), body: "ERR: refused".into() };
        }
        let request: serde_json::Value = serde_json::from_slice(&req.body).unwrap_or_default();
        let id = request["id"].clone();
        match request["method"].as_str() {
            Some("server/discover") => Reply::json(200, serde_json::json!({"jsonrpc":"2.0","id":id,"result":{
                "supportedVersions":["2026-07-28"],"capabilities":{"tools":{}},
                "serverInfo":{"name":"stand-in-gateway","version":"0"},"ttlMs":30000,"cacheScope":"private","resultType":"complete"}})),
            Some("tools/list") => Reply::json(
                200,
                serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"tools":[
                    {"name":"workflow_get_feature","inputSchema":{"type":"object"}},{"name":"workflow_mcp_smoke","inputSchema":{"type":"object"}}],"ttlMs":30000,"cacheScope":"private","resultType":"complete"}}),
            ),
            _ => Reply { status: 400, headers: Vec::new(), body: "unexpected method".into() },
        }
    })
    .await;
    let cfg = config(&dir, "a", "dev", &ca, &server.base, "");
    Setup {
        dir,
        cfg,
        server,
        refuse,
        ca,
    }
}

#[tokio::test]
async fn the_gateway_is_called_with_the_user_token_and_nothing_else() {
    let s = setup().await;
    let report = gateway::check(&s.cfg, Some(Secret::new("user-access-token")))
        .await
        .expect("connects");

    assert_eq!(report.protocol_version, "2026-07-28");
    assert!(!report.session);
    assert_eq!(report.server.as_deref(), Some("stand-in-gateway"));
    assert_eq!(
        report.tools,
        vec!["workflow_get_feature", "workflow_mcp_smoke"]
    );

    let seen = s.server.seen.lock().unwrap();
    let paths: Vec<(&str, &str)> = seen
        .iter()
        .map(|r| (r.method.as_str(), r.path.as_str()))
        .collect();
    assert_eq!(paths, [("POST", "/mcp"), ("POST", "/mcp")]);
    let methods: Vec<String> = seen
        .iter()
        .map(|r| {
            serde_json::from_slice::<serde_json::Value>(&r.body).unwrap()["method"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    assert_eq!(
        methods,
        ["server/discover", "tools/list"],
        "the stateless request order"
    );
    for request in seen.iter() {
        assert_eq!(
            request.headers.get("authorization").map(String::as_str),
            Some("Bearer user-access-token")
        );
        assert!(
            !request.headers.contains_key("x-scope-token"),
            "no application token: a downloadable program cannot keep one"
        );
        assert!(!request.headers.contains_key("x-workflow-action"));
        assert!(!request.headers.contains_key("mcp-session-id"));
        assert_eq!(
            request.client_certificates, 0,
            "no client certificate is presented"
        );
    }
}

#[tokio::test]
async fn the_startup_credential_is_never_sent_anywhere() {
    let s = setup().await;
    gateway::check(&s.cfg, Some(Secret::new("user-access-token")))
        .await
        .expect("connects");
    for request in s.server.seen.lock().unwrap().iter() {
        let text = format!("{:?}", request.headers);
        assert!(
            !text.contains("must-never-be-used"),
            "the ignored startup.yml credential leaked: {text}"
        );
    }
}

#[tokio::test]
async fn without_a_user_login_nothing_is_sent_and_the_exit_code_says_sign_in() {
    let s = setup().await;
    let error = gateway::check(&s.cfg, None).await.expect_err("no user");
    assert!(matches!(error, CliError::LoginRequired(_)), "{error}");
    assert_eq!(error.exit_code(), exit::LOGIN_REQUIRED);
    assert!(
        s.server.seen.lock().unwrap().is_empty(),
        "the Gateway was not called"
    );
}

#[tokio::test]
async fn a_refusal_by_the_gateway_is_denied() {
    let s = setup().await;
    for status in [401u16, 403] {
        *s.refuse.lock().unwrap() = Some(status);
        let error = gateway::check(&s.cfg, Some(Secret::new("expired")))
            .await
            .expect_err("refused");
        assert!(matches!(error, CliError::Denied(_)), "{status}: {error}");
        assert_eq!(error.exit_code(), exit::DENIED);
    }
}

#[tokio::test]
async fn a_gateway_failure_is_reported_as_uncertain() {
    let s = setup().await;
    *s.refuse.lock().unwrap() = Some(503);
    let error = gateway::check(&s.cfg, Some(Secret::new("t")))
        .await
        .expect_err("down");
    assert!(matches!(error, CliError::Unreachable(_)), "{error}");
    assert_eq!(error.exit_code(), exit::UNREACHABLE);
}

#[tokio::test]
async fn a_server_the_cli_does_not_trust_fails_the_handshake() {
    let s = setup().await;
    // Trust some other CA only: the stand-in's certificate no longer verifies.
    let other = TempDir::new().unwrap();
    let (other_ca, _, _) = write_server_pki(other.path());
    let cfg = config(&s.dir, "a", "dev", &other_ca, &s.server.base, "");
    let error = gateway::check(&cfg, Some(Secret::new("t")))
        .await
        .expect_err("untrusted");
    assert!(matches!(error, CliError::Unreachable(_)), "{error}");
    assert!(error.to_string().contains("bootstrapCaCertPath"), "{error}");
    assert!(
        s.server.seen.lock().unwrap().is_empty(),
        "no HTTP request got past the TLS layer"
    );
    let _ = &s.ca;
}

#[tokio::test]
async fn a_cleartext_gateway_that_is_not_loopback_is_refused_before_the_token_is_sent() {
    let s = setup().await;
    let cfg = config(&s.dir, "a", "dev", &s.ca, "http://gateway.example.com", "");
    let error = gateway::check(&cfg, Some(Secret::new("user-access-token")))
        .await
        .expect_err("cleartext");
    assert!(
        matches!(error, CliError::Config(_)) && error.to_string().contains("plain HTTP"),
        "{error}"
    );
}

#[tokio::test]
async fn a_missing_gateway_address_is_named() {
    let s = setup().await;
    let cfg = config(&s.dir, "a", "dev", &s.ca, "", "");
    let error = gateway::check(&cfg, Some(Secret::new("t")))
        .await
        .expect_err("no address");
    assert!(error.to_string().contains("cli.gatewayUri"), "{error}");
}
