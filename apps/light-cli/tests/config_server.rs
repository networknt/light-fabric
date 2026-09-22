//! Where the CLI's settings come from: the config server named in `startup.yml`.
//!
//! Which config server the CLI asks decides which instance it belongs to: its OAuth provider, its
//! Gateway and so its sign-in page. The stand-ins record what they receive, so the tests assert on
//! what crossed the wire.

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use light_cli::auth::{self, LoginOptions};
use light_cli::config::Secret;
use light_cli::gateway;
use tempfile::TempDir;

fn fast() -> LoginOptions {
    LoginOptions {
        min_interval: std::time::Duration::ZERO,
        slow_down_step: std::time::Duration::from_millis(50),
        scope: None,
    }
}

/// A stand-in light-oauth that grants at once and records which provider path it was called under.
async fn oauth(cert: &std::path::Path, key: &std::path::Path, name: &'static str) -> TlsServer {
    start_tls_server(cert, key, move |req| {
        if req.path.ends_with("/device_authorization") {
            Reply::json(200, serde_json::json!({
                "device_code": "d", "user_code": "BCDF-GHJK",
                "verification_uri": format!("https://portal.example.test/device?provider={name}"),
                "expires_in": 60, "interval": 0,
            }))
        } else {
            Reply::json(200, serde_json::json!({
                "access_token": "a.b.c", "token_type": "Bearer", "expires_in": 600,
                "refresh_token": format!("refresh-from-{name}"), "refresh_expires_in": 86400, "remember": "N",
            }))
        }
    })
    .await
}

/// A stand-in config server answering `values` (or the status) for every request.
async fn config_server(
    cert: &std::path::Path,
    key: &std::path::Path,
    reply: impl Fn() -> Reply + Send + Sync + 'static,
) -> TlsServer {
    start_tls_server(cert, key, move |_| reply()).await
}

#[tokio::test]
async fn the_config_server_decides_which_provider_and_service_the_cli_uses() {
    let dir = TempDir::new().unwrap();
    let (ca, cert, key) = write_server_pki(dir.path());
    // Two instances, each with its own OAuth service, and cli.yml's own default pointing at neither.
    let tenant_a = oauth(&cert, &key, "tenant-a").await;
    let tenant_b = oauth(&cert, &key, "tenant-b").await;
    for (which, server, provider) in [("a", &tenant_a, "tenant-a"), ("b", &tenant_b, "tenant-b")] {
        let values = format!(
            "cli.oauthUri: {}\ncli.oauthProviderId: {provider}\ncli.oauthClientId: client-1\n",
            server.base
        );
        let cs = config_server(&cert, &key, move || Reply {
            status: 200,
            headers: vec![],
            body: values.clone(),
        })
        .await;
        let cfg = config_with_server(
            &dir,
            which,
            "dev",
            &ca,
            "https://unused",
            "https://localhost:9",
            &cs.base,
            None,
        );

        auth::login(&cfg, &fast(), |_| {})
            .await
            .expect("signs in against what the config server said");

        let paths: Vec<String> = server
            .seen
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.path.clone())
            .collect();
        assert!(
            paths
                .iter()
                .all(|p| p.starts_with(&format!("/oauth2/{provider}/"))),
            "{which}: {paths:?}"
        );
        let session = std::fs::read_to_string(cfg.store_dir.join("user-session.json")).unwrap();
        assert!(
            session.contains(&format!("refresh-from-{provider}")),
            "{which}"
        );
    }
    assert!(
        tenant_a
            .seen
            .lock()
            .unwrap()
            .iter()
            .all(|r| !r.path.contains("tenant-b"))
    );
}

#[tokio::test]
async fn the_request_to_the_config_server_is_the_standard_one() {
    let dir = TempDir::new().unwrap();
    let (ca, cert, key) = write_server_pki(dir.path());
    let cs = config_server(&cert, &key, || Reply {
        status: 404,
        headers: vec![],
        body: "no snapshot".into(),
    })
    .await;
    let cfg = config_with_server(
        &dir,
        "a",
        "dev",
        &ca,
        "https://g",
        "https://o",
        &cs.base,
        Some("read-token"),
    );
    let (_, remote) = light_cli::remote::load_settings(&cfg).await.unwrap();
    assert!(matches!(remote, light_cli::remote::Remote::Unavailable(_)));

    let seen = cs.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, "GET");
    let (path, query) = seen[0].path.split_once('?').unwrap();
    assert_eq!(path, "/config-server/configs");
    let query: std::collections::HashMap<_, _> = url::form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect();
    assert_eq!(query["host"], "dev.lightapi.net");
    assert_eq!(query["serviceId"], "com.networknt.light-cli-1.0.0");
    assert_eq!(query["envTag"], "dev");
    assert_eq!(
        seen[0].headers.get("accept").map(String::as_str),
        Some("application/yaml")
    );
    assert_eq!(
        seen[0].headers.get("authorization").map(String::as_str),
        Some("Bearer read-token")
    );
}

#[tokio::test]
async fn an_unavailable_config_server_is_never_fatal_the_defaults_apply() {
    let dir = TempDir::new().unwrap();
    let (ca, cert, key) = write_server_pki(dir.path());
    let server = oauth(&cert, &key, "default").await;
    for (name, reply) in [
        ("not found", (404u16, "no snapshot")),
        ("server error", (503, "down")),
        ("not yaml mapping", (200, "- just\n- a list\n")),
    ] {
        let body = reply.1.to_string();
        let cs = config_server(&cert, &key, move || Reply {
            status: reply.0,
            headers: vec![],
            body: body.clone(),
        })
        .await;
        // cli.yml's default for the provider is what the stand-in above answers under.
        let cfg = config_with_server(
            &dir,
            "a",
            "dev",
            &ca,
            "https://g",
            &server.base,
            &cs.base,
            None,
        );
        set_cli_yml(&dir, "a", "https://g", &server.base);
        auth::login(&cfg, &fast(), |_| {})
            .await
            .unwrap_or_else(|e| panic!("{name}: {e}"));
    }
    // And one that is simply not there.
    let cfg = config_with_server(
        &dir,
        "b",
        "dev",
        &ca,
        "https://g",
        &server.base,
        "https://localhost:1",
        None,
    );
    auth::login(&cfg, &fast(), |_| {})
        .await
        .expect("a closed config server does not stop sign-in");
}

#[tokio::test]
async fn the_config_token_goes_to_the_config_server_and_nowhere_else() {
    let dir = TempDir::new().unwrap();
    let (ca, cert, key) = write_server_pki(dir.path());
    let oauth_server = oauth(&cert, &key, "p").await;
    let gateway_seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let log = Arc::clone(&gateway_seen);
    let gateway_server = start_tls_server(&cert, &key, move |req| {
        log.lock().unwrap().push(format!("{:?}", req.headers));
        Reply {
            status: 401,
            headers: vec![],
            body: "no".into(),
        }
    })
    .await;
    let values = format!(
        "cli.oauthUri: {}\ncli.oauthProviderId: p\ncli.oauthClientId: client-1\ncli.gatewayUri: {}\n",
        oauth_server.base, gateway_server.base
    );
    let cs = config_server(&cert, &key, move || Reply {
        status: 200,
        headers: vec![],
        body: values.clone(),
    })
    .await;
    let cfg = config_with_server(
        &dir,
        "a",
        "dev",
        &ca,
        "https://unused",
        "https://unused",
        &cs.base,
        Some("config-read-only-token"),
    );

    auth::login(&cfg, &fast(), |_| {}).await.expect("signs in");
    let user = auth::user_token(&cfg).await.unwrap().expect("signed in");
    let _ = gateway::check(&cfg, Some(user)).await;

    assert!(
        cs.seen
            .lock()
            .unwrap()
            .iter()
            .all(|r| r.headers.get("authorization").map(String::as_str)
                == Some("Bearer config-read-only-token"))
    );
    for request in oauth_server.seen.lock().unwrap().iter() {
        assert!(
            !format!("{:?}{:?}", request.headers, request.form).contains("config-read-only-token"),
            "leaked to light-oauth"
        );
    }
    let sent = gateway_seen.lock().unwrap();
    assert!(!sent.is_empty(), "the Gateway was called");
    assert!(
        sent.iter().all(|h| !h.contains("config-read-only-token")),
        "leaked to the Gateway: {sent:?}"
    );
    assert!(
        sent.iter().all(|h| h.contains("Bearer a.b.c")),
        "the Gateway got the user's token instead"
    );
}

#[tokio::test]
async fn a_cleartext_config_server_that_is_not_loopback_is_refused_before_the_token_is_sent() {
    let dir = TempDir::new().unwrap();
    let (ca, _, _) = write_server_pki(dir.path());
    let cfg = config_with_server(
        &dir,
        "a",
        "dev",
        &ca,
        "https://g",
        "https://o",
        "http://config.example.com",
        Some("tok"),
    );
    match light_cli::remote::fetch_values(&cfg).await {
        light_cli::remote::Remote::Unavailable(reason) => {
            assert!(reason.contains("plain HTTP"), "{reason}")
        }
        light_cli::remote::Remote::Found(_) => panic!("must not have been contacted"),
    }
}

#[tokio::test]
async fn without_a_config_server_address_the_defaults_apply_and_nothing_is_asked() {
    let dir = TempDir::new().unwrap();
    let (ca, _, _) = write_server_pki(dir.path());
    let mut cfg = config(&dir, "a", "dev", &ca, "https://g", "https://o");
    cfg.config_server_uri = None;
    assert!(matches!(
        light_cli::remote::fetch_values(&cfg).await,
        light_cli::remote::Remote::Unavailable(_)
    ));
    let _ = Secret::new("x");
}
