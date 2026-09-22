//! The session as a person uses it, driven line by line against a stand-in Gateway.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use futures_util::{FutureExt, SinkExt, StreamExt};
use light_cli::chat::{Options, TokenFn};
use light_cli::config::Secret;
use light_cli::error::exit;
use light_cli::output::Collector;
use light_cli::shell::{Flow, Shell};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio_tungstenite::tungstenite::Message;

use common::{Upgrade, Ws, WsServer, config, start_ws_server, write_server_pki};

fn jwt(claims: Value) -> String {
    let part = |v: &Value| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string());
    format!("{}.{}.sig", part(&json!({"alg": "none"})), part(&claims))
}

fn tokens_for(claims: Value) -> TokenFn {
    let token = jwt(claims);
    Arc::new(move || {
        let token = token.clone();
        async move { Ok(Some(Secret::new(token))) }.boxed()
    })
}

fn signed_in() -> TokenFn {
    tokens_for(json!({"eml": "user@example.test", "uid": "u-1", "role": "user"}))
}

struct Stand {
    _dir: TempDir,
    server: WsServer,
    /// What the agent side received: messages, and a marker when the client closed.
    received: Arc<Mutex<Vec<Value>>>,
    shell: Shell,
    out: Arc<Collector>,
}

fn agent_side(
    n: usize,
    mut ws: Ws,
    log: Arc<Mutex<Vec<Value>>>,
) -> futures_util::future::BoxFuture<'static, ()> {
    async move {
        ws.send(Message::Text(
            json!({"type": "session", "session_id": format!("s-{n}"), "turnTypes": ["chat"]})
                .to_string(),
        ))
        .await
        .unwrap();
        while let Some(Ok(message)) = ws.next().await {
            match message {
                Message::Text(raw) => {
                    let value: Value = serde_json::from_str(&raw).unwrap();
                    let body = value["text"].as_str().unwrap_or_default().to_string();
                    log.lock().unwrap().push(value);
                    if body == "silent" {
                        continue;
                    }
                    if body == "error" {
                        ws.send(Message::Text(
                            json!({"type":"error", "message":"agent failed"}).to_string(),
                        ))
                        .await
                        .unwrap();
                        continue;
                    }
                    let reply = if body == "hostile" {
                        "fine \u{1b}[2J\u{1b}]0;owned\u{7}line one\nline two".to_string()
                    } else {
                        format!("echo: {body}")
                    };
                    ws.send(Message::Text(
                        json!({"type": "text", "text": reply}).to_string(),
                    ))
                    .await
                    .unwrap();
                }
                Message::Close(_) => {
                    log.lock().unwrap().push(json!({"closed": true}));
                    return;
                }
                _ => {}
            }
        }
    }
    .boxed()
}

async fn stand_with(tokens: TokenFn, refuse: fn(&Upgrade, usize) -> Option<u16>) -> Stand {
    let dir = TempDir::new().unwrap();
    let (ca, cert, key) = write_server_pki(dir.path());
    let received: Arc<Mutex<Vec<Value>>> = Arc::default();
    let log = Arc::clone(&received);
    let server = start_ws_server(&cert, &key, refuse, move |n, ws| {
        agent_side(n, ws, Arc::clone(&log))
    })
    .await;
    let config = config(&dir, "a", "dev", &ca, &server.base, "https://localhost:1");
    let out = Arc::new(Collector::default());
    let mut shell = Shell::new(config, out.clone(), false).with_tokens(tokens);
    shell.chat_options = Options {
        init_timeout: Duration::from_secs(5),
        connect_timeout: Duration::from_secs(5),
        max_reconnects: 1,
        reconnect_delay: Duration::from_millis(20),
    };
    shell.reply_quiet = Duration::from_millis(150);
    shell.reply_timeout = Duration::from_secs(5);
    Stand {
        _dir: dir,
        server,
        received,
        shell,
        out,
    }
}

async fn stand() -> Stand {
    stand_with(signed_in(), |_, _| None).await
}

/// The agent side sees the close a moment after the CLI sends it.
async fn closed(received: &Arc<Mutex<Vec<Value>>>) -> bool {
    for _ in 0..100 {
        if received.lock().unwrap().last() == Some(&json!({"closed": true})) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

fn said(received: &Arc<Mutex<Vec<Value>>>) -> Vec<String> {
    received
        .lock()
        .unwrap()
        .iter()
        .filter_map(|m| m["text"].as_str().map(str::to_string))
        .collect()
}

#[tokio::test]
async fn a_chat_from_start_to_exit() {
    let mut s = stand().await;
    assert_eq!(s.shell.prompt(), "light> ");
    assert_eq!(s.shell.handle("/chat advisor").await, Flow::Continue);
    assert_eq!(
        s.shell.prompt(),
        "advisor> ",
        "the prompt shows who you are talking to"
    );
    assert_eq!(s.shell.handle("hello there").await, Flow::Continue);
    assert_eq!(s.shell.handle("/exit").await, Flow::Exit);

    let text = s.out.text();
    assert!(text.contains("connecting to advisor"), "{text}");
    assert!(
        text.contains("connected to advisor (session s-0)"),
        "{text}"
    );
    assert!(text.contains("advisor> echo: hello there"), "{text}");
    assert!(text.contains("chat with advisor ended"), "{text}");
    assert_eq!(said(&s.received), ["hello there"]);
    assert!(closed(&s.received).await, "/exit closes the chat");

    let upgrades = s.server.upgrades.lock().unwrap().clone();
    assert_eq!(upgrades.len(), 1);
    assert_eq!(
        upgrades[0].query("serviceId").as_deref(),
        Some("com.networknt.agent.advisor-1.0.0")
    );
    assert_eq!(
        upgrades[0].query("userId").as_deref(),
        Some("user@example.test")
    );
    assert!(
        upgrades[0].headers["authorization"].starts_with("Bearer eyJ"),
        "{:?}",
        upgrades[0].headers
    );
    assert!(s.shell.take_failure().is_none());
}

#[tokio::test]
async fn a_script_fails_when_the_agent_errors_or_does_not_reply() {
    let mut error = stand().await;
    error.shell.handle("/chat advisor").await;
    error.shell.handle("error").await;
    assert!(matches!(error.shell.take_failure(), Some(e) if e.exit_code() == exit::FAILED));
    error.shell.shutdown().await;

    let mut timeout = stand().await;
    timeout.shell.reply_timeout = Duration::from_millis(50);
    timeout.shell.handle("/chat advisor").await;
    timeout.shell.handle("silent").await;
    assert!(matches!(timeout.shell.take_failure(), Some(e) if e.exit_code() == exit::FAILED));
    timeout.shell.shutdown().await;
}

#[tokio::test]
async fn agents_are_listed_and_chosen_by_name() {
    let mut s = stand().await;
    s.shell.handle("/agents").await;
    let text = s.out.text();
    assert!(
        text.contains("advisor") && text.contains("com.networknt.agent.advisor-1.0.0"),
        "{text}"
    );
    assert!(text.contains("tech-support"), "{text}");

    s.shell.handle("/chat support").await;
    assert_eq!(s.shell.prompt(), "tech-support> ");
    s.shell.handle("/agents").await;
    assert!(
        s.out.text().contains("* tech-support"),
        "the active agent is marked: {}",
        s.out.text()
    );
    s.shell.shutdown().await;
    assert_eq!(
        s.server.upgrades.lock().unwrap()[0]
            .query("serviceId")
            .as_deref(),
        Some("com.networknt.agent.tech-support-1.0.0")
    );
}

#[tokio::test]
async fn a_bad_agent_or_no_agent_is_an_error_and_the_session_carries_on() {
    let mut s = stand().await;
    assert_eq!(s.shell.handle("/chat nobody").await, Flow::Continue);
    assert!(
        s.out.text().contains("no agent matches"),
        "{}",
        s.out.text()
    );
    s.shell.handle("/chat").await;
    assert!(
        s.out.text().contains("name an agent"),
        "two agents are configured, so one must be named: {}",
        s.out.text()
    );
    s.shell.handle("hello").await;
    assert!(s.out.text().contains("not in a chat"), "{}", s.out.text());
    s.shell.handle("/disconnect").await;
    assert!(s.out.text().contains("not in a chat"));
    s.shell.handle("/new").await;
    assert!(
        s.out.text().contains("no chat to renew"),
        "{}",
        s.out.text()
    );
    assert!(
        s.server.upgrades.lock().unwrap().is_empty(),
        "nothing connected"
    );
    assert!(matches!(s.shell.take_failure(), Some(e) if e.exit_code() == exit::FAILED));
}

#[tokio::test]
async fn chatting_needs_a_login_and_says_how_to_get_one() {
    let none: TokenFn = Arc::new(|| async { Ok(None) }.boxed());
    let mut s = stand_with(none, |_, _| None).await;
    s.shell.handle("/chat advisor").await;
    assert!(
        s.out.text().contains("sign in first: /login"),
        "{}",
        s.out.text()
    );
    assert!(s.server.upgrades.lock().unwrap().is_empty());
    assert!(matches!(s.shell.take_failure(), Some(e) if e.exit_code() == exit::LOGIN_REQUIRED));
}

#[tokio::test]
async fn a_token_that_names_no_user_is_refused_before_connecting() {
    let mut s = stand_with(tokens_for(json!({"role": "user"})), |_, _| None).await;
    s.shell.handle("/chat advisor").await;
    assert!(s.out.text().contains("names no user"), "{}", s.out.text());
    assert!(s.server.upgrades.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_user_id_falls_back_to_the_uid_when_there_is_no_email() {
    let mut s = stand_with(tokens_for(json!({"uid": "u-77"})), |_, _| None).await;
    s.shell.handle("/chat advisor").await;
    s.shell.shutdown().await;
    assert_eq!(
        s.server.upgrades.lock().unwrap()[0]
            .query("userId")
            .as_deref(),
        Some("u-77")
    );
}

#[tokio::test]
async fn a_double_slash_says_a_message_that_starts_with_a_slash_and_commands_are_never_sent() {
    let mut s = stand().await;
    s.shell.handle("/chat advisor").await;
    s.shell.handle("//etc/hosts").await;
    s.shell.handle("/whoami").await;
    s.shell.handle("/nonsense").await;
    s.shell.shutdown().await;
    assert_eq!(
        said(&s.received),
        ["/etc/hosts"],
        "only the chat message reached the agent"
    );
}

#[tokio::test]
async fn the_agents_words_are_sanitised_and_laid_out_under_its_name() {
    let mut s = stand().await;
    s.shell.handle("/chat advisor").await;
    s.shell.handle("hostile").await;
    s.shell.shutdown().await;
    let text = s.out.text();
    assert!(
        !text.contains('\u{1b}') && !text.contains('\u{7}'),
        "{text:?}"
    );
    assert!(
        text.contains("advisor> fine [2J]0;ownedline one\n         line two"),
        "{text:?}"
    );
}

#[tokio::test]
async fn new_starts_a_fresh_session_with_the_same_agent_and_disconnect_ends_it() {
    let mut s = stand().await;
    s.shell.handle("/chat advisor").await;
    s.shell.handle("/new").await;
    assert_eq!(s.shell.prompt(), "advisor> ");
    let upgrades = s.server.upgrades.lock().unwrap().clone();
    assert_eq!(upgrades.len(), 2);
    assert_eq!(
        upgrades[1].query("sessionId"),
        None,
        "a new session does not resume the old one"
    );
    assert!(s.out.text().contains("session s-1"), "{}", s.out.text());

    s.shell.handle("/disconnect").await;
    assert_eq!(s.shell.prompt(), "light> ");
    s.shell.handle("still there?").await;
    assert!(s.out.text().contains("not in a chat"));
}

#[tokio::test]
async fn logging_out_ends_the_chat_first() {
    let mut s = stand().await;
    s.shell.handle("/chat advisor").await;
    s.shell.handle("/logout --local").await;
    assert_eq!(s.shell.prompt(), "light> ");
    assert!(closed(&s.received).await, "the chat was closed");
    let text = s.out.text();
    assert!(
        text.contains("chat with advisor ended") && text.contains("not signed in"),
        "{text}"
    );
    s.shell.handle("/logout now").await;
    assert!(s.out.text().contains("usage: /logout [--local]"));
}

#[tokio::test]
async fn a_gateway_that_refuses_the_chat_is_reported_and_nothing_stays_open() {
    let mut s = stand_with(signed_in(), |_, _| Some(403)).await;
    s.shell.handle("/chat advisor").await;
    let text = s.out.text();
    assert!(text.contains("refused the chat connection"), "{text}");
    assert_eq!(s.shell.prompt(), "light> ");
    s.shell.handle("hello").await;
    assert!(s.out.text().contains("not in a chat"));
}

#[tokio::test]
async fn help_and_unknown_commands() {
    let mut s = stand().await;
    s.shell.handle("/help").await;
    for command in [
        "/login",
        "/logout",
        "/agents",
        "/chat",
        "/new",
        "/disconnect",
        "/tools",
        "/exit",
    ] {
        assert!(s.out.text().contains(command), "{command} is not in /help");
    }
    s.shell.handle("/frobnicate now").await;
    assert!(
        s.out.text().contains("unknown command /frobnicate"),
        "{}",
        s.out.text()
    );
    assert_eq!(s.shell.handle("/quit").await, Flow::Exit);
    assert_eq!(s.shell.handle("").await, Flow::Continue);
}

#[tokio::test]
async fn a_message_over_the_limit_is_refused_and_the_chat_survives() {
    let mut s = stand().await;
    s.shell.handle("/chat advisor").await;
    s.shell
        .handle(&"x".repeat(light_cli::chat::MAX_MESSAGE_BYTES + 1))
        .await;
    assert!(
        s.out.text().contains("send less at once"),
        "{}",
        s.out.text()
    );
    s.shell.handle("small").await;
    s.shell.shutdown().await;
    assert_eq!(said(&s.received), ["small"]);
}

#[tokio::test]
async fn a_cleartext_gateway_is_refused_before_any_token_is_produced() {
    let dir = TempDir::new().unwrap();
    let (ca, _cert, _key) = write_server_pki(dir.path());
    let config = config(
        &dir,
        "a",
        "dev",
        &ca,
        "http://gateway.example.test",
        "https://localhost:1",
    );
    let asked = Arc::new(Mutex::new(0usize));
    let counter = Arc::clone(&asked);
    let tokens: TokenFn = Arc::new(move || {
        *counter.lock().unwrap() += 1;
        async { Ok(Some(Secret::new(jwt(json!({"eml": "user@example.test"}))))) }.boxed()
    });
    let out = Arc::new(Collector::default());
    let mut shell = Shell::new(config, out.clone(), false).with_tokens(tokens);
    shell.handle("/chat advisor").await;
    assert!(
        out.text().contains("cleartext") || out.text().contains("https"),
        "{}",
        out.text()
    );
    assert_eq!(*asked.lock().unwrap(), 0, "the token was not even fetched");
    assert_eq!(shell.prompt(), "light> ");
}
