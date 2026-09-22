//! `/login`, `/whoami`, `/logout` and the user token, against a stand-in for light-oauth as the Gateway exposes it.
//!
//! The stand-in is an HTTPS server that answers the way the real one does (the
//! error codes and fields are those of `portal-service/apps/light-oauth/src/device.rs`,
//! whose own tests cover the server's rules). It records everything it receives, so the
//! tests assert on what crossed the wire, not on what the CLI believes it sent.

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use common::*;
use light_cli::auth::{self, LoginOptions};
use light_cli::config::CliConfig;
use light_cli::error::{CliError, exit};
use light_cli::session;
use light_cli::store::Store;
use tempfile::TempDir;

const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

fn jwt(email: &str) -> String {
    let b64 = |v: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v);
    let claims = serde_json::json!({"uid": "user-1", "eml": email, "role": "admin user"});
    format!(
        "{}.{}.sig",
        b64(br#"{"alg":"none"}"#),
        b64(claims.to_string().as_bytes())
    )
}

/// How the stand-in should behave. Tests change it while the server runs.
struct Script {
    /// `authorization_pending` this many times before the first real answer.
    pending: usize,
    /// Answer the next poll with `slow_down`, once.
    slow_down_once: bool,
    /// After the pending polls: approve, deny, or keep waiting forever.
    outcome: Outcome,
    /// Answer this many requests to `token` with HTTP 503 first.
    unavailable: usize,
    interval: u64,
    expires_in: u64,
    access_ttl: i64,
    login_seconds: i64,
    /// The refresh token the server currently honours.
    refresh: String,
    generation: usize,
    revoked: bool,
    plain_text_refresh_error: bool,
    /// Take this long to answer a refresh (to make racing callers overlap).
    refresh_delay: Duration,
    /// Take this long to answer a revocation.
    revoke_delay: Duration,
}

#[derive(Clone, Copy, PartialEq)]
enum Outcome {
    Approve,
    Deny,
    Never,
}

impl Script {
    fn new() -> Self {
        Script {
            pending: 0,
            slow_down_once: false,
            outcome: Outcome::Approve,
            unavailable: 0,
            interval: 0,
            expires_in: 60,
            access_ttl: 600,
            login_seconds: 86_400,
            refresh: String::new(),
            generation: 0,
            revoked: false,
            plain_text_refresh_error: false,
            refresh_delay: Duration::ZERO,
            revoke_delay: Duration::ZERO,
        }
    }
}

struct Listener {
    server: TlsServer,
    script: Arc<Mutex<Script>>,
}

impl Listener {
    fn requests(
        &self,
        path_suffix: &str,
    ) -> Vec<(String, std::collections::HashMap<String, String>)> {
        self.server
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.path.ends_with(path_suffix))
            .map(|r| (r.path.clone(), r.form.clone()))
            .collect()
    }

    fn total(&self) -> usize {
        self.server.seen.lock().unwrap().len()
    }
}

fn error(status: u16, code: &str) -> Reply {
    Reply::json(
        status,
        serde_json::json!({"error": code, "error_description": format!("stand-in: {code}")}),
    )
}

fn tokens(script: &mut Script, first: bool) -> Reply {
    script.generation += 1;
    script.refresh = format!("refresh-{}", script.generation);
    let mut body = serde_json::json!({
        "access_token": jwt("a@example.test"),
        "token_type": "Bearer",
        "expires_in": script.access_ttl,
        "refresh_token": script.refresh,
        "remember": "N",
    });
    if first {
        body["refresh_expires_in"] = script.login_seconds.into();
    }
    Reply::json(200, body)
}

async fn start_listener(cert: &std::path::Path, key: &std::path::Path) -> Listener {
    let script = Arc::new(Mutex::new(Script::new()));
    let state = Arc::clone(&script);
    let polls = Arc::new(AtomicUsize::new(0));
    let server = start_tls_server(cert, key, move |req| {
        if req.form.get("client_id").map(String::as_str) != Some("client-1") || req.form.contains_key("client_secret") {
            return error(401, "invalid_client");
        }
        let mut script = state.lock().unwrap();
        match req.path.as_str() {
            "/oauth2/prov/device_authorization" => Reply::json(
                200,
                serde_json::json!({
                    "device_code": "device-secret-1",
                    "user_code": "BCDF-GHJK",
                    "verification_uri": "https://oauth.example.test/oauth2/prov/device",
                    "verification_uri_complete": "https://oauth.example.test/oauth2/prov/device?user_code=BCDFGHJK",
                    "expires_in": script.expires_in,
                    "interval": script.interval,
                }),
            ),
            "/oauth2/prov/token" => {
                if script.unavailable > 0 {
                    script.unavailable -= 1;
                    return error(503, "temporarily_unavailable");
                }
                match req.form.get("grant_type").map(String::as_str) {
                    Some(DEVICE_GRANT) => {
                        if req.form.get("device_code").map(String::as_str) != Some("device-secret-1") {
                            return error(400, "invalid_grant");
                        }
                        if script.slow_down_once {
                            script.slow_down_once = false;
                            return error(400, "slow_down");
                        }
                        if polls.fetch_add(1, Ordering::SeqCst) < script.pending {
                            return error(400, "authorization_pending");
                        }
                        match script.outcome {
                            Outcome::Approve => tokens(&mut script, true),
                            Outcome::Deny => error(400, "access_denied"),
                            Outcome::Never => error(400, "authorization_pending"),
                        }
                    }
                    Some("refresh_token") => {
                        let presented = req.form.get("refresh_token").cloned().unwrap_or_default();
                        if script.revoked || presented != script.refresh {
                            if script.plain_text_refresh_error {
                                return Reply {
                                    status: 400,
                                    headers: Vec::new(),
                                    body: "invalid_grant".into(),
                                };
                            }
                            return error(400, "invalid_grant");
                        }
                        let delay = script.refresh_delay;
                        let reply = tokens(&mut script, false);
                        drop(script);
                        std::thread::sleep(delay);
                        reply
                    }
                    _ => error(400, "unsupported_grant_type"),
                }
            }
            "/oauth2/prov/revoke" => {
                if req.form.get("token").map(String::as_str) == Some(script.refresh.as_str()) {
                    script.revoked = true;
                }
                let delay = script.revoke_delay;
                drop(script);
                std::thread::sleep(delay);
                Reply { status: 200, headers: Vec::new(), body: String::new() }
            }
            _ => error(404, "not_found"),
        }
    })
    .await;
    Listener { server, script }
}

/// A CLI whose `cli.yml` points at a stand-in for light-oauth.
struct Setup {
    dir: TempDir,
    cfg: CliConfig,
    listener: Listener,
    ca: std::path::PathBuf,
}

fn fast() -> LoginOptions {
    LoginOptions {
        scope: None,
        min_interval: Duration::ZERO,
        slow_down_step: Duration::from_millis(300),
    }
}

async fn setup() -> Setup {
    let dir = TempDir::new().unwrap();
    let (ca, cert, key) = write_server_pki(dir.path());
    let listener = start_listener(&cert, &key).await;
    let cfg = config(
        &dir,
        "a",
        "dev",
        &ca,
        "https://localhost",
        &listener.server.base,
    );
    Setup {
        dir,
        cfg,
        listener,
        ca,
    }
}

fn store(cfg: &CliConfig) -> Store {
    Store::new(cfg.store_dir.clone())
}

fn unix_now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

async fn sign_in(s: &Setup) {
    auth::login(&s.cfg, &fast(), |_| {})
        .await
        .expect("signs in");
}

/// Rewrite the stored session, for setting up "about to expire" and similar states.
fn edit_session(s: &Setup, change: impl FnOnce(&mut session::UserSession)) {
    let store = store(&s.cfg);
    let mut current = session::load(&store).unwrap().expect("signed in");
    change(&mut current);
    session::save(&store, &current).unwrap();
}

// ---------------------------------------------------------------------------
// Login
// ---------------------------------------------------------------------------

#[tokio::test]
async fn login_requests_a_code_shows_it_then_polls_and_stores_the_session() {
    let s = setup().await;
    s.listener.script.lock().unwrap().pending = 2;
    let shown = Arc::new(Mutex::new(None));
    let seen = Arc::clone(&shown);
    let before = unix_now();

    let status = auth::login(&s.cfg, &fast(), move |code| {
        *seen.lock().unwrap() = Some((
            code.user_code.clone(),
            code.verification_uri_complete.clone(),
        ));
    })
    .await
    .expect("signs in");

    let (user_code, complete) = shown
        .lock()
        .unwrap()
        .clone()
        .expect("the code was announced");
    assert_eq!(user_code, "BCDF-GHJK");
    assert!(complete.unwrap().contains("user_code=BCDFGHJK"));
    assert_eq!(status.state, "signed-in");
    assert_eq!(status.email.as_deref(), Some("a@example.test"));
    assert_eq!(status.roles.as_deref(), Some("admin user"));

    let stored = session::load(&store(&s.cfg))
        .unwrap()
        .expect("session saved");
    assert_eq!(stored.refresh_token, "refresh-1");
    assert_eq!(stored.oauth_uri, s.listener.server.base);
    let login_end = stored
        .login_expires_at
        .expect("the server said when the login ends");
    assert!(
        (before + 86_399..=unix_now() + 86_400).contains(&login_end),
        "{login_end}"
    );
    assert!(stored.access_expires_at >= before + 599);

    let polls = s.listener.requests("/token");
    assert_eq!(polls.len(), 3, "two pending answers, then the approval");
    for (_, form) in &polls {
        assert_eq!(form["grant_type"], DEVICE_GRANT);
        assert_eq!(form["device_code"], "device-secret-1");
        assert_eq!(form["client_id"], "client-1");
    }
}

#[tokio::test]
async fn every_request_is_a_public_clients_no_secret_no_certificate_no_application_token() {
    let s = setup().await;
    sign_in(&s).await;
    let seen = s.listener.server.seen.lock().unwrap();
    assert!(!seen.is_empty());
    for request in seen.iter() {
        assert_eq!(
            request.client_certificates, 0,
            "no client certificate on {}",
            request.path
        );
        assert!(
            !request.form.contains_key("client_secret"),
            "a public client sends no secret"
        );
        assert!(
            !request.headers.contains_key("authorization"),
            "no bearer either"
        );
        assert!(
            !request.headers.contains_key("x-scope-token"),
            "no application token"
        );
        let text = format!("{:?}", request.headers);
        assert!(
            !text.contains("must-never-be-used"),
            "the ignored startup.yml credential leaked"
        );
    }
}

#[tokio::test]
async fn the_session_file_is_private_and_the_device_code_is_never_stored() {
    use std::os::unix::fs::PermissionsExt;
    let s = setup().await;
    sign_in(&s).await;
    let path = session::path(s.cfg.store_dir.as_path());
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    for file in walk(&s.cfg.store_dir) {
        if let Ok(text) = std::fs::read_to_string(&file) {
            assert!(
                !text.contains("device-secret-1"),
                "device code found in {}",
                file.display()
            );
        }
    }
}

#[tokio::test]
async fn slow_down_makes_the_next_poll_wait_longer() {
    let s = setup().await;
    {
        let mut script = s.listener.script.lock().unwrap();
        script.slow_down_once = true;
    }
    let started = Instant::now();
    sign_in(&s).await;
    // Interval 0, then slow_down adds the step (300 ms) before the next poll.
    assert!(
        started.elapsed() >= Duration::from_millis(300),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(s.listener.requests("/token").len(), 2);
}

#[tokio::test]
async fn a_client_light_oauth_does_not_accept_is_told_what_it_must_be() {
    let s = setup().await;
    // The stand-in refuses any client other than "client-1"; ask as another.
    set_cli_yml(&s.dir, "a", "https://localhost", &s.listener.server.base);
    let conf = s.dir.path().join("conf-a/cli.yml");
    let text = std::fs::read_to_string(&conf)
        .unwrap()
        .replace("client-1", "some-other-client");
    std::fs::write(&conf, text).unwrap();
    let error = auth::login(&s.cfg, &fast(), |_| {})
        .await
        .expect_err("refused");
    assert!(matches!(error, CliError::Denied(_)), "{error}");
    let text = error.to_string();
    for needed in [
        "some-other-client",
        "Client Profile \"cli\"",
        "\"public\" or \"trusted\"",
        "cli.oauthClientId",
    ] {
        assert!(text.contains(needed), "{needed}: {text}");
    }
    assert!(
        !text.to_lowercase().contains("certificate"),
        "the old certificate advice is gone: {text}"
    );
    assert!(session::load(&store(&s.cfg)).unwrap().is_none());
}

#[tokio::test]
async fn a_refusal_in_front_of_light_oauth_points_at_the_gateway_route() {
    let dir = TempDir::new().unwrap();
    let (ca, cert, key) = write_server_pki(dir.path());
    let front = start_tls_server(&cert, &key, |_| Reply {
        status: 401,
        headers: vec![],
        body: "Missing authorization header".into(),
    })
    .await;
    let cfg = config(&dir, "a", "dev", &ca, "https://localhost", &front.base);
    let error = auth::login(&cfg, &fast(), |_| {})
        .await
        .expect_err("refused");
    assert!(matches!(error, CliError::Denied(_)), "{error}");
    let text = error.to_string();
    assert!(
        text.contains("Gateway route") && text.contains("/oauth2/prov/device_authorization"),
        "{text}"
    );
    assert!(
        !text.contains("Client Type"),
        "it is not the client check that refused: {text}"
    );
}

#[tokio::test]
async fn oauth_form_posts_are_never_followed_across_redirects() {
    let dir = TempDir::new().unwrap();
    let (ca, cert, key) = write_server_pki(dir.path());
    let front = start_tls_server(&cert, &key, |_| Reply {
        status: 307,
        headers: vec![("location".into(), "https://example.invalid/stolen".into())],
        body: String::new(),
    })
    .await;
    let cfg = config(&dir, "a", "dev", &ca, "https://localhost", &front.base);

    auth::login(&cfg, &fast(), |_| {})
        .await
        .expect_err("a redirect is an OAuth refusal, not a navigation");
    assert_eq!(front.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_denied_sign_in_is_denied_and_stores_nothing() {
    let s = setup().await;
    s.listener.script.lock().unwrap().outcome = Outcome::Deny;
    let error = auth::login(&s.cfg, &fast(), |_| {})
        .await
        .expect_err("denied");
    assert!(matches!(error, CliError::Denied(_)), "{error}");
    assert_eq!(error.exit_code(), exit::DENIED);
    assert!(session::load(&store(&s.cfg)).unwrap().is_none());
}

#[tokio::test]
async fn a_code_that_is_never_approved_expires_and_stores_nothing() {
    let s = setup().await;
    {
        let mut script = s.listener.script.lock().unwrap();
        script.outcome = Outcome::Never;
        script.expires_in = 1;
    }
    let options = LoginOptions {
        min_interval: Duration::from_millis(100),
        ..fast()
    };
    let started = Instant::now();
    let error = auth::login(&s.cfg, &options, |_| {})
        .await
        .expect_err("expires");
    assert!(error.to_string().contains("expired"), "{error}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "gave up at the server's deadline, not later"
    );
    assert!(session::load(&store(&s.cfg)).unwrap().is_none());
}

#[tokio::test]
async fn a_brief_outage_while_waiting_does_not_lose_the_approval() {
    let s = setup().await;
    s.listener.script.lock().unwrap().unavailable = 2;
    sign_in(&s).await;
    assert_eq!(
        session::load(&store(&s.cfg))
            .unwrap()
            .unwrap()
            .refresh_token,
        "refresh-1"
    );
}

#[tokio::test]
async fn a_lasting_outage_is_reported_as_unreachable() {
    let s = setup().await;
    s.listener.script.lock().unwrap().unavailable = 100;
    let error = auth::login(&s.cfg, &fast(), |_| {})
        .await
        .expect_err("gives up");
    assert!(matches!(error, CliError::Unreachable(_)), "{error}");
    assert_eq!(error.exit_code(), exit::UNREACHABLE);
    assert!(session::load(&store(&s.cfg)).unwrap().is_none());
}

#[tokio::test]
async fn signing_in_again_replaces_the_previous_session() {
    let s = setup().await;
    sign_in(&s).await;
    sign_in(&s).await;
    assert_eq!(
        session::load(&store(&s.cfg))
            .unwrap()
            .unwrap()
            .refresh_token,
        "refresh-2"
    );
}

#[tokio::test]
async fn a_server_the_cli_does_not_trust_fails_the_handshake_and_says_how_to_fix_it() {
    let s = setup().await;
    let other = TempDir::new().unwrap();
    let (other_ca, _, _) = write_server_pki(other.path());
    let cfg = config(
        &s.dir,
        "a",
        "dev",
        &other_ca,
        "https://localhost",
        &s.listener.server.base,
    );
    let error = auth::login(&cfg, &fast(), |_| {})
        .await
        .expect_err("handshake must fail");
    assert!(matches!(error, CliError::Unreachable(_)), "{error}");
    assert!(error.to_string().contains("bootstrapCaCertPath"), "{error}");
    assert_eq!(
        s.listener.total(),
        0,
        "no HTTP request got past the TLS layer"
    );
    let _ = &s.ca;
}

#[tokio::test]
async fn the_device_listener_must_be_https() {
    let s = setup().await;
    set_cli_yml(&s.dir, "a", "https://localhost", "http://localhost:7444");
    let error = auth::login(&s.cfg, &fast(), |_| {})
        .await
        .expect_err("plain http refused");
    assert!(
        matches!(error, CliError::Config(_)) && error.to_string().contains("https"),
        "{error}"
    );
    assert_eq!(s.listener.total(), 0);
}

#[tokio::test]
async fn missing_sign_in_settings_are_named() {
    let s = setup().await;
    set_cli_yml(&s.dir, "a", "https://localhost", "");
    let error = auth::login(&s.cfg, &fast(), |_| {})
        .await
        .expect_err("not configured");
    assert!(error.to_string().contains("cli.oauth"), "{error}");
}

// ---------------------------------------------------------------------------
// Using the login: refresh on use
// ---------------------------------------------------------------------------

#[tokio::test]
async fn nobody_signed_in_means_no_token_and_no_network() {
    let s = setup().await;
    assert!(auth::user_token(&s.cfg).await.unwrap().is_none());
    assert_eq!(s.listener.total(), 0);
}

#[tokio::test]
async fn a_fresh_access_token_is_used_as_it_is() {
    let s = setup().await;
    sign_in(&s).await;
    let before = s.listener.total();
    let token = auth::user_token(&s.cfg).await.unwrap().expect("signed in");
    assert_eq!(
        token.expose(),
        session::load(&store(&s.cfg)).unwrap().unwrap().access_token
    );
    assert_eq!(
        s.listener.total(),
        before,
        "no refresh while the token has time left"
    );
}

#[tokio::test]
async fn a_token_about_to_expire_is_refreshed_and_the_rotated_token_is_saved() {
    let s = setup().await;
    sign_in(&s).await;
    // A login end unlike anything a refresh could compute, so extending it would show.
    let end_before = Some(unix_now() + 5_000);
    edit_session(&s, |session| {
        session.access_expires_at = unix_now() + 30;
        session.login_expires_at = end_before;
    });

    let token = auth::user_token(&s.cfg).await.unwrap().expect("refreshed");

    let refreshes: Vec<_> = s
        .listener
        .requests("/token")
        .into_iter()
        .filter(|(_, f)| f["grant_type"] == "refresh_token")
        .collect();
    assert_eq!(refreshes.len(), 1);
    assert_eq!(
        refreshes[0].1["refresh_token"], "refresh-1",
        "the current refresh token was presented"
    );
    let stored = session::load(&store(&s.cfg)).unwrap().unwrap();
    assert_eq!(
        stored.refresh_token, "refresh-2",
        "the rotated refresh token replaced it on disk"
    );
    assert_eq!(stored.access_token, token.expose());
    assert!(stored.access_expires_at > unix_now() + 500);
    assert_eq!(
        stored.login_expires_at, end_before,
        "refreshing does not extend the login"
    );
}

// Several tasks block on the store's file lock, so the runtime needs threads to spare.
#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn parallel_invocations_refresh_once_and_all_get_a_working_token() {
    let s = setup().await;
    sign_in(&s).await;
    edit_session(&s, |session| session.access_expires_at = unix_now() + 30);
    // Slow the refresh so the callers genuinely overlap.
    s.listener.script.lock().unwrap().refresh_delay = Duration::from_millis(300);

    let cfg = Arc::new(s.cfg);
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..4 {
        let cfg = Arc::clone(&cfg);
        tasks.spawn(async move { auth::user_token(&cfg).await });
    }
    while let Some(result) = tasks.join_next().await {
        assert!(result.unwrap().unwrap().is_some());
    }
    let refreshes = s
        .listener
        .requests("/token")
        .into_iter()
        .filter(|(_, f)| f["grant_type"] == "refresh_token")
        .count();
    assert_eq!(
        refreshes, 1,
        "the others waited for the lock and saw the fresh token"
    );
}

#[tokio::test]
async fn a_dead_refresh_token_ends_the_local_login_and_says_sign_in_again() {
    let s = setup().await;
    sign_in(&s).await;
    edit_session(&s, |session| session.access_expires_at = unix_now() - 5);
    s.listener.script.lock().unwrap().revoked = true;

    let error = auth::user_token(&s.cfg).await.expect_err("login ended");
    assert!(matches!(error, CliError::LoginRequired(_)), "{error}");
    assert_eq!(error.exit_code(), exit::LOGIN_REQUIRED);
    assert!(
        session::load(&store(&s.cfg)).unwrap().is_none(),
        "the dead session is removed"
    );
    assert!(
        auth::user_token(&s.cfg).await.unwrap().is_none(),
        "and asking again does not call the server"
    );
}

#[tokio::test]
async fn a_plain_text_invalid_grant_also_ends_the_local_login() {
    let s = setup().await;
    sign_in(&s).await;
    edit_session(&s, |session| session.access_expires_at = unix_now() - 5);
    {
        let mut script = s.listener.script.lock().unwrap();
        script.revoked = true;
        script.plain_text_refresh_error = true;
    }

    let error = auth::user_token(&s.cfg).await.expect_err("login ended");
    assert!(matches!(error, CliError::LoginRequired(_)), "{error}");
    assert!(session::load(&store(&s.cfg)).unwrap().is_none());
}

#[tokio::test]
async fn an_outage_during_refresh_keeps_the_session() {
    let s = setup().await;
    sign_in(&s).await;
    edit_session(&s, |session| session.access_expires_at = unix_now() - 5);
    s.listener.script.lock().unwrap().unavailable = 5;

    let error = auth::user_token(&s.cfg).await.expect_err("server down");
    assert!(matches!(error, CliError::Unreachable(_)), "{error}");
    assert_eq!(
        session::load(&store(&s.cfg))
            .unwrap()
            .expect("still signed in")
            .refresh_token,
        "refresh-1",
        "a network error must not sign the user out"
    );
}

#[tokio::test]
async fn a_login_past_its_end_is_refused_locally_without_contacting_the_server() {
    let s = setup().await;
    sign_in(&s).await;
    edit_session(&s, |session| {
        session.access_expires_at = unix_now() - 5;
        session.login_expires_at = Some(unix_now() - 1);
    });
    let before = s.listener.total();
    let error = auth::user_token(&s.cfg).await.expect_err("ended");
    assert!(matches!(error, CliError::LoginRequired(_)), "{error}");
    assert_eq!(s.listener.total(), before);
    assert_eq!(auth::status(&s.cfg).unwrap().state, "login-ended");
}

#[tokio::test]
async fn refresh_goes_to_the_endpoint_the_login_came_from_even_if_cli_yml_changes() {
    let s = setup().await;
    sign_in(&s).await;
    edit_session(&s, |session| session.access_expires_at = unix_now() - 5);
    set_cli_yml(&s.dir, "a", "https://localhost", "https://localhost:1");
    assert!(
        auth::user_token(&s.cfg).await.unwrap().is_some(),
        "refreshed at the recorded endpoint"
    );
}

// ---------------------------------------------------------------------------
// Status and logout
// ---------------------------------------------------------------------------

#[tokio::test]
async fn status_reports_who_is_signed_in_without_contacting_anyone() {
    let s = setup().await;
    assert_eq!(auth::status(&s.cfg).unwrap().state, "signed-out");
    sign_in(&s).await;
    let before = s.listener.total();
    let status = auth::status(&s.cfg).unwrap();
    assert_eq!(
        (status.state, status.email.as_deref()),
        ("signed-in", Some("a@example.test"))
    );
    assert!(status.login_expires_in.unwrap() > 86_000);
    edit_session(&s, |session| session.access_expires_at = unix_now() - 5);
    assert_eq!(auth::status(&s.cfg).unwrap().state, "access-expired");
    assert_eq!(s.listener.total(), before);
}

#[tokio::test]
async fn logout_revokes_on_the_server_then_deletes_the_local_tokens() {
    let s = setup().await;
    sign_in(&s).await;
    let outcome = auth::logout(&s.cfg, false).await.expect("signs out");
    assert!(outcome.was_signed_in && outcome.revoked_on_server);

    let revocations = s.listener.requests("/revoke");
    assert_eq!(revocations.len(), 1);
    assert_eq!(revocations[0].1["token"], "refresh-1");
    assert_eq!(revocations[0].1["client_id"], "client-1");
    assert!(
        s.listener.script.lock().unwrap().revoked,
        "the server now refuses that refresh token"
    );
    assert!(session::load(&store(&s.cfg)).unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_login_saved_while_logout_is_revoking_survives_it() {
    let s = setup().await;
    sign_in(&s).await;
    let mut newer = session::load(&store(&s.cfg)).unwrap().expect("signed in");
    newer.refresh_token = "refresh-of-the-newer-login".into();
    s.listener.script.lock().unwrap().revoke_delay = Duration::from_millis(600);

    // Another invocation finishes a login while the revocation is in flight. It waits its turn for
    // the store, so its login lands after the logout and is not deleted by it.
    let store_dir = s.cfg.store_dir.clone();
    let rival = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        let store = Store::new(store_dir);
        let _lock = store.lock().unwrap();
        session::save(&store, &newer).unwrap();
    });
    let outcome = auth::logout(&s.cfg, false).await.expect("signs out");
    rival.join().unwrap();

    assert!(outcome.revoked_on_server);
    let kept = session::load(&store(&s.cfg))
        .unwrap()
        .expect("the newer login is still there");
    assert_eq!(kept.refresh_token, "refresh-of-the-newer-login");
    assert_eq!(s.listener.requests("/revoke").len(), 1);
    assert_eq!(
        s.listener.requests("/revoke")[0].1["token"],
        "refresh-1",
        "only the login that was read was revoked"
    );
}

#[tokio::test]
async fn logout_when_the_server_cannot_be_reached_keeps_the_tokens_unless_told_to_go_local() {
    let s = setup().await;
    sign_in(&s).await;
    set_cli_yml(&s.dir, "a", "https://localhost", "https://localhost:1");
    edit_session(&s, |session| {
        session.oauth_uri = "https://localhost:1".into()
    });

    let error = auth::logout(&s.cfg, false)
        .await
        .expect_err("cannot revoke");
    assert!(matches!(error, CliError::Unreachable(_)), "{error}");
    assert!(
        session::load(&store(&s.cfg)).unwrap().is_some(),
        "nothing was deleted"
    );

    let outcome = auth::logout(&s.cfg, true).await.expect("local logout");
    assert!(outcome.was_signed_in && !outcome.revoked_on_server);
    assert!(session::load(&store(&s.cfg)).unwrap().is_none());
}

#[tokio::test]
async fn logout_when_signed_out_is_a_no_op() {
    let s = setup().await;
    let outcome = auth::logout(&s.cfg, false).await.unwrap();
    assert!(!outcome.was_signed_in);
    assert_eq!(s.listener.total(), 0);
}

// ---------------------------------------------------------------------------
// The binary: what a user and a script see
// ---------------------------------------------------------------------------

fn light(s: &Setup) -> std::process::Command {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_light"));
    command
        .arg("--startup")
        .arg(s.dir.path().join("conf-a/startup.yml"))
        .arg("--home")
        .arg(s.dir.path().join("a"))
        .env_remove("LIGHT_USER_ACCESS_TOKEN");
    command
}

/// Run each line as `-c LINE`.
fn run_light(s: &Setup, lines: &[&str]) -> std::process::Output {
    let mut command = light(s);
    for line in lines {
        command.arg("-c").arg(line);
    }
    command.output().expect("runs the light binary")
}

/// Pipe `input` in as if typed.
fn run_light_piped(s: &Setup, input: &str) -> std::process::Output {
    use std::io::Write;
    let mut child = light(s)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("runs the light binary");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_binary_signs_in_reports_status_and_signs_out_with_stable_exit_codes() {
    let s = setup().await;

    let out = run_light(&s, &["/whoami"]);
    assert_eq!(
        out.status.code(),
        Some(exit::LOGIN_REQUIRED as i32),
        "signed out"
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("signed out"));

    let out = run_light(&s, &["/login"]);
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert!(
        stdout.contains("BCDF-GHJK") && stdout.contains("user_code=BCDFGHJK"),
        "the code is shown: {stdout}"
    );
    assert!(
        !stderr.contains("device-secret-1") && !stdout.contains("device-secret-1"),
        "the device code is never printed"
    );
    assert!(stdout.contains("signed in as a@example.test"), "{stdout}");
    assert!(
        !stdout.contains("refresh-1") && !stderr.contains("refresh-1"),
        "no token is printed"
    );

    let out = run_light(&s, &["/whoami"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0));
    assert!(stdout.contains("signed in as a@example.test"), "{stdout}");
    assert!(!stdout.contains("eyJ"), "the report has no token");

    let out = run_light(&s, &["/logout"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        run_light(&s, &["/whoami"]).status.code(),
        Some(exit::LOGIN_REQUIRED as i32)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_terminal_stays_open_across_commands_until_exit() {
    let s = setup().await;
    let out = run_light_piped(&s, "/whoami\n\n/help\n/whoami\n/exit\n/whoami\n");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        out.status.code(),
        Some(exit::LOGIN_REQUIRED as i32),
        "signed out: {stdout}"
    );
    // Two reports before /exit; the third line is never run.
    assert_eq!(stdout.matches("signed out").count(), 2, "{stdout}");
    assert!(stdout.contains("/chat [agent]"), "/help ran: {stdout}");

    let out = run_light_piped(&s, "/nonsense\n/whoami\n");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("unknown command /nonsense"), "{stdout}");
    assert!(
        stdout.contains("signed out"),
        "a failed command does not end the session: {stdout}"
    );
    assert_eq!(
        out.status.code(),
        Some(exit::FAILED as i32),
        "the first failure decides the exit code"
    );

    // End of input ends the session too.
    assert_eq!(run_light_piped(&s, "").status.code(), Some(0));
}
