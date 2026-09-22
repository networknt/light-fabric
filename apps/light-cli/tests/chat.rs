//! `/chat` against a stand-in Gateway: a TLS WebSocket server that records exactly what the CLI
//! sends and plays the parts of an agent, an expiring login and a failing route.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{FutureExt, SinkExt, StreamExt};
use light_cli::chat::{ChatEvent, ChatHandle, Options, Target, TokenFn, agents};
use light_cli::config::Secret;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

use common::{Ws, WsServer, start_ws_server, write_server_pki};

const AGENT: &str = "com.networknt.agent.advisor-1.0.0";

struct Stand {
    _dir: TempDir,
    ca: std::path::PathBuf,
    server: WsServer,
    /// What the agent side received from the CLI, in order.
    received: Arc<Mutex<Vec<Value>>>,
}

type Script = fn(usize, Ws, Arc<Mutex<Vec<Value>>>) -> futures_util::future::BoxFuture<'static, ()>;

async fn stand(
    refuse: impl Fn(usize) -> Option<u16> + Send + Sync + 'static,
    script: Script,
) -> Stand {
    let dir = TempDir::new().unwrap();
    let (ca, cert, key) = write_server_pki(dir.path());
    let received: Arc<Mutex<Vec<Value>>> = Arc::default();
    let log = Arc::clone(&received);
    let server = start_ws_server(
        &cert,
        &key,
        move |_, n| refuse(n),
        move |n, ws| script(n, ws, Arc::clone(&log)),
    )
    .await;
    Stand {
        _dir: dir,
        ca,
        server,
        received,
    }
}

fn session(id: &str) -> Message {
    Message::Text(json!({"type": "session", "session_id": id, "turnTypes": ["chat"], "defaultTurnType": "chat"}).to_string())
}

fn text(body: &str) -> Message {
    Message::Text(json!({"type": "text", "text": body}).to_string())
}

/// The next JSON message the CLI sent, or `None` if it closed or nothing came in `wait`.
async fn client_message(ws: &mut Ws, wait: Duration) -> Option<Value> {
    loop {
        match tokio::time::timeout(wait, ws.next()).await.ok()?? {
            Ok(Message::Text(raw)) => return serde_json::from_str(&raw).ok(),
            Ok(Message::Close(_)) | Err(_) => return None,
            Ok(_) => {}
        }
    }
}

fn tokens(sequence: &'static [&'static str]) -> TokenFn {
    let calls = Arc::new(Mutex::new(0usize));
    Arc::new(move || {
        let mut n = calls.lock().unwrap();
        let token = sequence[(*n).min(sequence.len() - 1)];
        *n += 1;
        async move { Ok(Some(Secret::new(token))) }.boxed()
    })
}

fn quick() -> Options {
    Options {
        init_timeout: Duration::from_secs(5),
        connect_timeout: Duration::from_secs(5),
        max_reconnects: 3,
        reconnect_delay: Duration::from_millis(20),
    }
}

fn target(stand: &Stand) -> Target {
    Target {
        gateway_uri: stand.server.base.clone(),
        agent: agents(&[AGENT.to_string()], "dev").remove(0),
        user_id: "user@example.test".into(),
    }
}

fn start(
    stand: &Stand,
    tokens: TokenFn,
    options: Options,
) -> (ChatHandle, UnboundedReceiver<ChatEvent>) {
    ChatHandle::start(target(stand), tokens, Some(&stand.ca), options).expect("chat starts")
}

async fn next(events: &mut UnboundedReceiver<ChatEvent>) -> ChatEvent {
    tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("an event within 5s")
        .expect("the chat ended without an event")
}

async fn next_matching(
    events: &mut UnboundedReceiver<ChatEvent>,
    what: &str,
    pick: impl Fn(&ChatEvent) -> bool,
) -> ChatEvent {
    for _ in 0..20 {
        let event = next(events).await;
        if pick(&event) {
            return event;
        }
    }
    panic!("never saw {what}");
}

fn echo(
    _n: usize,
    mut ws: Ws,
    log: Arc<Mutex<Vec<Value>>>,
) -> futures_util::future::BoxFuture<'static, ()> {
    async move {
        ws.send(session("s-1")).await.unwrap();
        while let Some(message) = client_message(&mut ws, Duration::from_secs(5)).await {
            let body = message["text"].as_str().unwrap_or_default().to_string();
            log.lock().unwrap().push(message);
            ws.send(text(&format!("echo: {body}"))).await.unwrap();
        }
    }
    .boxed()
}

#[tokio::test]
async fn a_chat_sends_only_the_users_bearer_token_and_exchanges_messages() {
    let stand = stand(|_| None, echo).await;
    let (chat, mut events) = start(&stand, tokens(&["tok-1"]), quick());

    assert_eq!(
        next(&mut events).await,
        ChatEvent::Ready {
            session_id: "s-1".into(),
            resumed: false
        }
    );
    chat.send("hello").unwrap();
    assert_eq!(
        next(&mut events).await,
        ChatEvent::Text("echo: hello".into())
    );

    let upgrades = stand.server.upgrades.lock().unwrap().clone();
    assert_eq!(upgrades.len(), 1);
    let up = &upgrades[0];
    assert_eq!(
        up.headers.get("authorization").map(String::as_str),
        Some("Bearer tok-1")
    );
    assert!(up.uri.starts_with("/chat?"), "{}", up.uri);
    assert_eq!(up.query("userId").as_deref(), Some("user@example.test"));
    assert_eq!(up.query("serviceId").as_deref(), Some(AGENT));
    assert_eq!(up.query("envTag").as_deref(), Some("dev"));
    assert_eq!(up.query("protocol").as_deref(), Some("http"));
    assert_eq!(
        up.query("sessionId"),
        None,
        "a first connection has no session to resume"
    );
    // A native client: none of what makes the Gateway treat the upgrade as a browser's, and the
    // token is not in the address.
    for browser_only in ["origin", "cookie", "x-csrf-token"] {
        assert!(
            !up.headers.contains_key(browser_only),
            "{browser_only} was sent"
        );
    }
    assert!(
        !up.headers
            .get("sec-websocket-protocol")
            .is_some_and(|p| p.contains("csrf")),
        "a csrf subprotocol was offered"
    );
    assert!(!up.uri.contains("tok-1"));

    let received = stand.received.lock().unwrap().clone();
    assert_eq!(received.len(), 1);
    assert_eq!(received[0]["text"], "hello");
    assert!(
        received[0]["clientMessageId"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );
}

#[tokio::test]
async fn what_the_agent_says_is_made_safe_before_it_reaches_the_terminal() {
    fn hostile(
        _n: usize,
        mut ws: Ws,
        _log: Arc<Mutex<Vec<Value>>>,
    ) -> futures_util::future::BoxFuture<'static, ()> {
        async move {
            ws.send(session("s-1")).await.unwrap();
            ws.send(Message::Text(
                json!({"type": "text", "text": "hi \u{1b}]0;owned\u{7}\u{1b}[2Jthere\r\u{202e}gnp"}).to_string(),
            ))
            .await
            .unwrap();
            ws.send(Message::Text(json!({"type": "error", "message": "bad\u{1b}[31m\nred"}).to_string())).await.unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        .boxed()
    }
    let stand = stand(|_| None, hostile).await;
    let (_chat, mut events) = start(&stand, tokens(&["t"]), quick());
    next_matching(&mut events, "ready", |e| {
        matches!(e, ChatEvent::Ready { .. })
    })
    .await;
    let ChatEvent::Text(body) = next(&mut events).await else {
        panic!("not text")
    };
    assert_eq!(body, "hi ]0;owned[2Jtheregnp");
    let ChatEvent::Error(message) = next(&mut events).await else {
        panic!("not an error")
    };
    assert_eq!(message, "bad[31m red");
}

#[tokio::test]
async fn messages_typed_before_the_agent_is_ready_wait_for_it() {
    fn slow(
        _n: usize,
        mut ws: Ws,
        log: Arc<Mutex<Vec<Value>>>,
    ) -> futures_util::future::BoxFuture<'static, ()> {
        async move {
            // Nothing the client sends may arrive before the session frame has been sent.
            assert!(
                client_message(&mut ws, Duration::from_millis(300))
                    .await
                    .is_none(),
                "sent before ready"
            );
            ws.send(session("s-1")).await.unwrap();
            while let Some(message) = client_message(&mut ws, Duration::from_secs(5)).await {
                log.lock().unwrap().push(message);
            }
        }
        .boxed()
    }
    let stand = stand(|_| None, slow).await;
    let (chat, mut events) = start(&stand, tokens(&["t"]), quick());
    chat.send("one").unwrap();
    chat.send("two").unwrap();
    next_matching(&mut events, "ready", |e| {
        matches!(e, ChatEvent::Ready { .. })
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let texts: Vec<String> = stand
        .received
        .lock()
        .unwrap()
        .iter()
        .map(|m| m["text"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        texts,
        ["one", "two"],
        "queued messages go out once, in order"
    );
}

fn expires_after_first_message(admitted: bool) -> Script {
    // Function pointers cannot capture, so the two variants are two functions.
    fn refusing(
        n: usize,
        ws: Ws,
        log: Arc<Mutex<Vec<Value>>>,
    ) -> futures_util::future::BoxFuture<'static, ()> {
        expiring(n, ws, log, false)
    }
    fn unsure(
        n: usize,
        ws: Ws,
        log: Arc<Mutex<Vec<Value>>>,
    ) -> futures_util::future::BoxFuture<'static, ()> {
        expiring(n, ws, log, true)
    }
    if admitted { unsure } else { refusing }
}

fn expiring(
    n: usize,
    mut ws: Ws,
    log: Arc<Mutex<Vec<Value>>>,
    admitted: bool,
) -> futures_util::future::BoxFuture<'static, ()> {
    async move {
        ws.send(session("s-1")).await.unwrap();
        if n == 0 {
            let message = client_message(&mut ws, Duration::from_secs(5)).await.expect("a message");
            let id = message["clientMessageId"].as_str().unwrap().to_string();
            log.lock().unwrap().push(message);
            ws.send(Message::Text(
                json!({"type": "authentication_required", "admitted": admitted, "clientMessageId": id}).to_string(),
            ))
            .await
            .unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
        } else {
            while let Some(message) = client_message(&mut ws, Duration::from_secs(5)).await {
                let body = message["text"].as_str().unwrap_or_default().to_string();
                log.lock().unwrap().push(message);
                ws.send(text(&format!("echo: {body}"))).await.unwrap();
            }
        }
    }
    .boxed()
}

#[tokio::test]
async fn an_expired_login_reconnects_with_a_fresh_token_and_the_same_session_and_does_not_resend() {
    let stand = stand(|_| None, expires_after_first_message(false)).await;
    let (chat, mut events) = start(&stand, tokens(&["tok-old", "tok-new"]), quick());
    next_matching(&mut events, "ready", |e| {
        matches!(e, ChatEvent::Ready { .. })
    })
    .await;
    chat.send("first").unwrap();

    let ChatEvent::Notice(notice) = next_matching(
        &mut events,
        "a notice",
        |e| matches!(e, ChatEvent::Notice(n) if n.contains("not sent")),
    )
    .await
    else {
        unreachable!()
    };
    assert!(notice.contains("send it again"), "{notice}");
    assert_eq!(
        next_matching(&mut events, "ready again", |e| matches!(
            e,
            ChatEvent::Ready { .. }
        ))
        .await,
        ChatEvent::Ready {
            session_id: "s-1".into(),
            resumed: true
        }
    );
    chat.send("second").unwrap();
    assert_eq!(
        next_matching(&mut events, "the echo", |e| matches!(e, ChatEvent::Text(_))).await,
        ChatEvent::Text("echo: second".into())
    );

    let upgrades = stand.server.upgrades.lock().unwrap().clone();
    assert_eq!(upgrades.len(), 2);
    assert_eq!(
        upgrades[0].headers.get("authorization").map(String::as_str),
        Some("Bearer tok-old")
    );
    assert_eq!(
        upgrades[1].headers.get("authorization").map(String::as_str),
        Some("Bearer tok-new")
    );
    assert_eq!(upgrades[0].query("sessionId"), None);
    assert_eq!(upgrades[1].query("sessionId").as_deref(), Some("s-1"));
    let texts: Vec<String> = stand
        .received
        .lock()
        .unwrap()
        .iter()
        .map(|m| m["text"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        texts,
        ["first", "second"],
        "the refused message was not sent a second time"
    );
}

#[tokio::test]
async fn a_message_the_server_may_have_taken_is_never_resent_and_the_person_is_told() {
    let stand = stand(|_| None, expires_after_first_message(true)).await;
    let (chat, mut events) = start(&stand, tokens(&["a", "b"]), quick());
    next_matching(&mut events, "ready", |e| {
        matches!(e, ChatEvent::Ready { .. })
    })
    .await;
    chat.send("first").unwrap();
    let ChatEvent::Notice(notice) = next_matching(
        &mut events,
        "a notice",
        |e| matches!(e, ChatEvent::Notice(n) if n.contains("may have been received")),
    )
    .await
    else {
        unreachable!()
    };
    assert!(notice.contains("will not be sent again"), "{notice}");
    next_matching(&mut events, "ready again", |e| {
        matches!(e, ChatEvent::Ready { resumed: true, .. })
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        stand.received.lock().unwrap().len(),
        1,
        "only the original send"
    );
}

#[tokio::test]
async fn close_code_4401_also_means_sign_in_again() {
    fn close_4401(
        n: usize,
        mut ws: Ws,
        _log: Arc<Mutex<Vec<Value>>>,
    ) -> futures_util::future::BoxFuture<'static, ()> {
        async move {
            ws.send(session("s-9")).await.unwrap();
            if n == 0 {
                let _ = ws
                    .send(Message::Close(Some(CloseFrame {
                        code: CloseCode::from(4401),
                        reason: "authenticate".into(),
                    })))
                    .await;
                tokio::time::sleep(Duration::from_millis(200)).await;
            } else {
                let _ = client_message(&mut ws, Duration::from_secs(2)).await;
            }
        }
        .boxed()
    }
    let stand = stand(|_| None, close_4401).await;
    let (_chat, mut events) = start(&stand, tokens(&["one", "two"]), quick());
    next_matching(&mut events, "ready", |e| {
        matches!(e, ChatEvent::Ready { resumed: false, .. })
    })
    .await;
    // Told it is a renewed sign-in, not a dropped connection.
    next_matching(
        &mut events,
        "a renewal notice",
        |e| matches!(e, ChatEvent::Notice(n) if n.contains("sign-in was renewed")),
    )
    .await;
    next_matching(&mut events, "ready again", |e| {
        matches!(e, ChatEvent::Ready { resumed: true, .. })
    })
    .await;
    let upgrades = stand.server.upgrades.lock().unwrap().clone();
    assert_eq!(
        upgrades[1].headers.get("authorization").map(String::as_str),
        Some("Bearer two")
    );
    assert_eq!(upgrades[1].query("sessionId").as_deref(), Some("s-9"));
}

#[tokio::test]
async fn a_dropped_connection_is_retried_a_bounded_number_of_times() {
    fn drop_after_hello(
        _n: usize,
        mut ws: Ws,
        _log: Arc<Mutex<Vec<Value>>>,
    ) -> futures_util::future::BoxFuture<'static, ()> {
        async move {
            ws.send(session("s-1")).await.unwrap();
            // Vanish without a close frame.
        }
        .boxed()
    }
    let stand = stand(|_| None, drop_after_hello).await;
    let options = Options {
        max_reconnects: 2,
        ..quick()
    };
    let (_chat, mut events) = start(&stand, tokens(&["t"]), options);
    next_matching(&mut events, "ready", |e| {
        matches!(e, ChatEvent::Ready { .. })
    })
    .await;
    let ChatEvent::Error(error) = next_matching(&mut events, "bounded reconnect error", |e| {
        matches!(e, ChatEvent::Error(_))
    })
    .await
    else {
        unreachable!()
    };
    assert!(
        error.contains("could not keep the chat connected"),
        "{error}"
    );
    assert_eq!(
        next_matching(&mut events, "closed", |e| matches!(e, ChatEvent::Closed)).await,
        ChatEvent::Closed
    );
    assert_eq!(
        stand.server.upgrades.lock().unwrap().len(),
        3,
        "the initial connection plus exactly two retries"
    );
}

#[tokio::test]
async fn reconnecting_gives_up_after_the_budget_and_says_so() {
    fn hello_once(
        n: usize,
        mut ws: Ws,
        _log: Arc<Mutex<Vec<Value>>>,
    ) -> futures_util::future::BoxFuture<'static, ()> {
        async move {
            if n == 0 {
                ws.send(session("s-1")).await.unwrap();
            }
            // Later connections are accepted and dropped without a hello.
        }
        .boxed()
    }
    let stand = stand(|_| None, hello_once).await;
    let options = Options {
        max_reconnects: 2,
        ..quick()
    };
    let (_chat, mut events) = start(&stand, tokens(&["t"]), options);
    next_matching(&mut events, "ready", |e| {
        matches!(e, ChatEvent::Ready { .. })
    })
    .await;
    let ChatEvent::Error(error) = next_matching(&mut events, "an error", |e| {
        matches!(e, ChatEvent::Error(_))
    })
    .await
    else {
        unreachable!()
    };
    assert!(
        error.contains("could not keep the chat connected"),
        "{error}"
    );
    assert_eq!(
        next_matching(&mut events, "closed", |e| matches!(e, ChatEvent::Closed)).await,
        ChatEvent::Closed
    );
    // The first connection plus at most two retries.
    assert!(stand.server.upgrades.lock().unwrap().len() <= 3);
}

#[tokio::test]
async fn a_failed_resume_upgrade_is_retried_within_the_budget() {
    fn resume(
        n: usize,
        mut ws: Ws,
        _log: Arc<Mutex<Vec<Value>>>,
    ) -> futures_util::future::BoxFuture<'static, ()> {
        async move {
            ws.send(session("s-1")).await.unwrap();
            if n > 0 {
                while ws.next().await.is_some() {}
            }
        }
        .boxed()
    }

    let stand = stand(|n| (n == 1).then_some(503), resume).await;
    let options = Options {
        max_reconnects: 2,
        ..quick()
    };
    let (_chat, mut events) = start(&stand, tokens(&["t"]), options);
    next_matching(&mut events, "initial session", |e| {
        matches!(e, ChatEvent::Ready { resumed: false, .. })
    })
    .await;
    next_matching(&mut events, "session after failed upgrade", |e| {
        matches!(e, ChatEvent::Ready { resumed: true, .. })
    })
    .await;
    assert_eq!(stand.server.upgrades.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn a_successful_exchange_resets_the_consecutive_reconnect_budget() {
    fn recover(
        n: usize,
        mut ws: Ws,
        _log: Arc<Mutex<Vec<Value>>>,
    ) -> futures_util::future::BoxFuture<'static, ()> {
        async move {
            ws.send(session("s-1")).await.unwrap();
            match n {
                0 => {}
                1 => {
                    let message = client_message(&mut ws, Duration::from_secs(5))
                        .await
                        .expect("message after first reconnect");
                    ws.send(text(message["text"].as_str().unwrap()))
                        .await
                        .unwrap();
                }
                _ => while ws.next().await.is_some() {},
            }
        }
        .boxed()
    }

    let stand = stand(|_| None, recover).await;
    let options = Options {
        max_reconnects: 1,
        ..quick()
    };
    let (chat, mut events) = start(&stand, tokens(&["t"]), options);
    next_matching(&mut events, "initial session", |e| {
        matches!(e, ChatEvent::Ready { resumed: false, .. })
    })
    .await;
    next_matching(&mut events, "first resumed session", |e| {
        matches!(e, ChatEvent::Ready { resumed: true, .. })
    })
    .await;
    chat.send("healthy").unwrap();
    next_matching(
        &mut events,
        "successful reply",
        |e| matches!(e, ChatEvent::Text(body) if body == "healthy"),
    )
    .await;
    next_matching(&mut events, "second resumed session", |e| {
        matches!(e, ChatEvent::Ready { resumed: true, .. })
    })
    .await;
    assert_eq!(stand.server.upgrades.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn a_refused_upgrade_is_reported_without_the_token() {
    for (status, needle) in [
        (401u16, "refused the chat connection"),
        (403, "refused the chat connection"),
        (404, "no chat route"),
        (502, "failed the chat connection"),
    ] {
        let stand = stand(move |_| Some(status), echo).await;
        let (_chat, mut events) = start(&stand, tokens(&["super-secret-token"]), quick());
        let ChatEvent::Error(error) = next(&mut events).await else {
            panic!("expected an error for {status}")
        };
        assert!(error.contains(needle), "{status}: {error}");
        assert!(!error.contains("super-secret-token"), "{error}");
        assert_eq!(next(&mut events).await, ChatEvent::Closed);
        assert_eq!(
            stand.server.upgrades.lock().unwrap().len(),
            1,
            "a refusal is not retried"
        );
    }
}

#[tokio::test]
async fn no_login_means_no_connection() {
    let stand = stand(|_| None, echo).await;
    let none: TokenFn = Arc::new(|| async { Ok(None) }.boxed());
    let (_chat, mut events) = start(&stand, none, quick());
    let ChatEvent::Error(error) = next(&mut events).await else {
        panic!("expected an error")
    };
    assert!(error.contains("/login"), "{error}");
    assert_eq!(next(&mut events).await, ChatEvent::Closed);
    assert!(stand.server.upgrades.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_agent_that_never_starts_a_session_times_out() {
    fn silent(
        _n: usize,
        mut ws: Ws,
        _log: Arc<Mutex<Vec<Value>>>,
    ) -> futures_util::future::BoxFuture<'static, ()> {
        async move {
            let _ = client_message(&mut ws, Duration::from_secs(3)).await;
        }
        .boxed()
    }
    let stand = stand(|_| None, silent).await;
    let options = Options {
        init_timeout: Duration::from_millis(200),
        ..quick()
    };
    let (_chat, mut events) = start(&stand, tokens(&["t"]), options);
    let ChatEvent::Error(error) = next(&mut events).await else {
        panic!("expected an error")
    };
    assert!(error.contains("did not start a session"), "{error}");
    assert_eq!(next(&mut events).await, ChatEvent::Closed);
}

#[tokio::test]
async fn an_agent_that_does_not_take_chat_is_refused() {
    fn coding_only(
        _n: usize,
        mut ws: Ws,
        _log: Arc<Mutex<Vec<Value>>>,
    ) -> futures_util::future::BoxFuture<'static, ()> {
        async move {
            ws.send(Message::Text(
                json!({"type": "session", "session_id": "s", "turnTypes": ["coding"]}).to_string(),
            ))
            .await
            .unwrap();
            let _ = client_message(&mut ws, Duration::from_secs(2)).await;
        }
        .boxed()
    }
    let stand = stand(|_| None, coding_only).await;
    let (_chat, mut events) = start(&stand, tokens(&["t"]), quick());
    let ChatEvent::Error(error) = next(&mut events).await else {
        panic!("expected an error")
    };
    assert!(error.contains("does not take chat"), "{error}");
    assert_eq!(next(&mut events).await, ChatEvent::Closed);
}

#[tokio::test]
async fn ending_the_chat_closes_the_connection() {
    fn watch_close(
        _n: usize,
        mut ws: Ws,
        log: Arc<Mutex<Vec<Value>>>,
    ) -> futures_util::future::BoxFuture<'static, ()> {
        async move {
            ws.send(session("s-1")).await.unwrap();
            loop {
                match tokio::time::timeout(Duration::from_secs(3), ws.next()).await {
                    Ok(Some(Ok(Message::Close(_)))) => {
                        log.lock().unwrap().push(json!({"closed": true}));
                        return;
                    }
                    Ok(Some(Ok(_))) => {}
                    _ => return,
                }
            }
        }
        .boxed()
    }
    let stand = stand(|_| None, watch_close).await;
    let (chat, mut events) = start(&stand, tokens(&["t"]), quick());
    next_matching(&mut events, "ready", |e| {
        matches!(e, ChatEvent::Ready { .. })
    })
    .await;
    chat.close().await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        stand.received.lock().unwrap().as_slice(),
        [json!({"closed": true})]
    );
    assert_eq!(
        next_matching(&mut events, "closed", |e| matches!(e, ChatEvent::Closed)).await,
        ChatEvent::Closed
    );
}

#[tokio::test]
async fn a_gateway_the_cli_does_not_trust_is_a_tls_error_with_a_hint() {
    let stand = stand(|_| None, echo).await;
    // No CA bundle: the stand-in's certificate is signed by a CA this client has never heard of.
    let (_chat, mut events) =
        ChatHandle::start(target(&stand), tokens(&["t"]), None, quick()).unwrap();
    let ChatEvent::Error(error) = next(&mut events).await else {
        panic!("expected an error")
    };
    assert!(error.contains("TLS failure"), "{error}");
    assert!(
        stand.server.upgrades.lock().unwrap().is_empty(),
        "no upgrade should reach the server"
    );
}
