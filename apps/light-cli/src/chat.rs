//! Chat with an agent through `light-gateway`'s `/chat` WebSocket, as the signed-in user.
//!
//! The Gateway treats a WebSocket upgrade as a browser's when it carries an `Origin`, the CSRF
//! cookie or a `csrf.` subprotocol, and as a native client's otherwise. The CLI is a native
//! client: it sends **only** `Authorization: Bearer <user access token>`, so it never needs the
//! browser's CSRF machinery, and the token never appears in the URL.
//!
//! The wire protocol is the one the Portal's chat page speaks (`portal-view` `Chat.tsx`): the
//! server opens with a `session` frame; the client sends `{text, clientMessageId}`; the server
//! answers with `text` frames. When the login's access token expires the server sends
//! `authentication_required` (or closes with code 4401); the CLI fetches a fresh token and
//! reconnects with the same `sessionId`. A turn the server already accepted is **never** sent
//! again: the person is told and decides.
//!
//! Everything the agent says is untrusted and is made safe to print before it leaves this module.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::{AUTHORIZATION, HeaderValue};
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{
    Connector, MaybeTlsStream, WebSocketStream, connect_async_tls_with_config,
};
use url::Url;
use uuid::Uuid;

use crate::config::Secret;
use crate::error::CliError;
use crate::text;

/// The close code the Gateway uses for "authenticate again".
const CLOSE_REAUTHENTICATE: u16 = 4401;
/// The longest message the CLI sends. The agent's own limit is its business; this keeps a
/// pasted file from becoming one giant frame.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;
const MAX_SESSION_ID_CHARS: usize = 256;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Supplies the user's current access token: refreshed if it is about to expire. `Ok(None)` means
/// nobody is signed in.
pub type TokenFn =
    Arc<dyn Fn() -> BoxFuture<'static, Result<Option<Secret>, CliError>> + Send + Sync>;

// ── agents ───────────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    pub service_id: String,
    pub env_tag: String,
}

impl Agent {
    /// The short name a person types: `com.networknt.agent.tech-support-1.0.0` → `tech-support`.
    pub fn name(&self) -> &str {
        let id = self.service_id.as_str();
        let base = id.split_once("agent.").map_or(id, |(_, rest)| rest);
        match base.rfind('-') {
            Some(at) if base[at + 1..].starts_with(|c: char| c.is_ascii_digit()) => &base[..at],
            _ => base,
        }
    }
}

pub fn agents(service_ids: &[String], env_tag: &str) -> Vec<Agent> {
    service_ids
        .iter()
        .map(|service_id| Agent {
            service_id: service_id.clone(),
            env_tag: env_tag.to_string(),
        })
        .collect()
}

/// Pick an agent by full service id, short name, or an unambiguous part of either.
pub fn find_agent<'a>(agents: &'a [Agent], query: &str) -> Result<&'a Agent, String> {
    let query = query.trim();
    if query.is_empty() {
        return Err("name an agent".into());
    }
    if let Some(agent) = agents.iter().find(|a| a.service_id == query) {
        return Ok(agent);
    }
    if let Some(agent) = agents.iter().find(|a| a.name().eq_ignore_ascii_case(query)) {
        return Ok(agent);
    }
    let lower = query.to_ascii_lowercase();
    let matching: Vec<&Agent> = agents
        .iter()
        .filter(|a| a.service_id.to_ascii_lowercase().contains(&lower))
        .collect();
    match matching.as_slice() {
        [only] => Ok(only),
        [] => Err(format!("no agent matches {query:?}")),
        several => Err(format!(
            "{query:?} matches more than one agent: {}",
            several
                .iter()
                .map(|a| a.name())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

// ── frames ───────────────────────────────────────────────────────────────────────────────────

/// One message from the server, already safe to print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Session {
        session_id: String,
        turn_types: Vec<String>,
    },
    AuthenticationRequired {
        admitted: bool,
        client_message_id: Option<String>,
    },
    Text(String),
    Error(String),
    /// A frame the CLI has no use for (status, coding-only, unknown).
    Ignored,
}

pub fn parse_frame(raw: &str) -> Frame {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        // The server speaks JSON; anything else is shown as the agent's own words, as the Portal does.
        return Frame::Text(text::sanitize(raw));
    };
    let string = |name: &str| value.get(name).and_then(Value::as_str);
    match string("type") {
        Some("session") => {
            let session_id = text::strip_controls(string("session_id").unwrap_or_default())
                .trim()
                .to_string();
            if session_id.is_empty() || session_id.chars().count() > MAX_SESSION_ID_CHARS {
                return Frame::Error("the agent sent an invalid session id".into());
            }
            let turn_types = match value.get("turnTypes").and_then(Value::as_array) {
                Some(types) => types
                    .iter()
                    .filter_map(Value::as_str)
                    .map(text::strip_controls)
                    .collect(),
                // Older agents support the ordinary chat contract only.
                None => vec!["chat".to_string()],
            };
            Frame::Session {
                session_id,
                turn_types,
            }
        }
        Some("authentication_required") => Frame::AuthenticationRequired {
            // Absent means unknown, and unknown is treated as "the server may have taken it".
            admitted: value
                .get("admitted")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            client_message_id: string("clientMessageId").map(str::to_string),
        },
        Some("text") => match string("text") {
            Some(body) => Frame::Text(text::sanitize(body)),
            None => Frame::Error("the agent sent a text message with no text".into()),
        },
        Some("error") => match string("message") {
            Some(message) => Frame::Error(text::sanitize_line(message)),
            None => Frame::Error("the agent reported an error without a message".into()),
        },
        _ => Frame::Ignored,
    }
}

// ── the connection ───────────────────────────────────────────────────────────────────────────

/// Where to connect and as whom.
#[derive(Debug, Clone)]
pub struct Target {
    pub gateway_uri: String,
    pub agent: Agent,
    /// The user's email (or, without one, id): the `userId` the Gateway's chat route expects.
    pub user_id: String,
}

/// The `wss://` address of the chat route for this agent.
pub fn chat_url(target: &Target, session_id: Option<&str>) -> Result<Url, CliError> {
    let mut url = Url::parse(&target.gateway_uri).map_err(|e| {
        CliError::Config(format!(
            "cli.gatewayUri {:?} is not a URL: {e}",
            target.gateway_uri
        ))
    })?;
    let scheme = match url.scheme() {
        "https" => "wss",
        "http" => "ws",
        other => {
            return Err(CliError::Config(format!(
                "cli.gatewayUri must be http(s), not {other}"
            )));
        }
    };
    url.set_scheme(scheme)
        .map_err(|()| CliError::Config("cli.gatewayUri cannot carry a WebSocket".into()))?;
    let base = url.path().trim_end_matches('/').to_string();
    url.set_path(&format!("{base}/chat"));
    url.set_query(None);
    url.set_fragment(None);
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("userId", &target.user_id)
            .append_pair("serviceId", &target.agent.service_id)
            .append_pair("envTag", &target.agent.env_tag)
            // Agent registrations use HTTP; the explicit serviceId bypasses the path defaults.
            .append_pair("protocol", "http");
        if let Some(session_id) = session_id {
            query.append_pair("sessionId", session_id);
        }
    }
    Ok(url)
}

fn upgrade_failure(url: &Url, error: tungstenite::Error) -> CliError {
    let place = format!("{}://{}", url.scheme(), url.host_str().unwrap_or("?"));
    match error {
        tungstenite::Error::Http(response) => {
            let status = response.status();
            match status.as_u16() {
                401 | 403 => CliError::Denied(format!(
                    "the Gateway refused the chat connection (HTTP {status}); sign in again, or ask an administrator to allow /chat for your roles"
                )),
                404 => CliError::Failed(format!(
                    "the Gateway has no chat route or no such agent (HTTP 404) at {place}"
                )),
                500..=599 => CliError::Unreachable(format!(
                    "the Gateway failed the chat connection (HTTP {status})"
                )),
                _ => CliError::Failed(format!(
                    "the Gateway rejected the chat connection (HTTP {status})"
                )),
            }
        }
        other => {
            let detail = other.to_string();
            let lower = detail.to_ascii_lowercase();
            let hint = if lower.contains("certificate")
                || lower.contains("handshake")
                || lower.contains("alert")
            {
                " (a TLS failure: check that this CLI trusts the Gateway's certificate via bootstrapCaCertPath)"
            } else {
                ""
            };
            CliError::Unreachable(format!("{place}: {detail}{hint}"))
        }
    }
}

async fn connect(
    url: &Url,
    token: &Secret,
    tls: Arc<rustls::ClientConfig>,
    timeout: Duration,
) -> Result<Socket, CliError> {
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|e| CliError::Config(format!("not a valid chat address: {e}")))?;
    let mut bearer = HeaderValue::from_str(&format!("Bearer {}", token.expose()))
        .map_err(|_| CliError::Failed("the access token cannot be sent as a header".into()))?;
    bearer.set_sensitive(true);
    request.headers_mut().insert(AUTHORIZATION, bearer);
    let connecting =
        connect_async_tls_with_config(request, None, false, Some(Connector::Rustls(tls)));
    match tokio::time::timeout(timeout, connecting).await {
        Err(_) => Err(CliError::Unreachable(format!(
            "the Gateway did not answer within {}s",
            timeout.as_secs()
        ))),
        Ok(Err(error)) => Err(upgrade_failure(url, error)),
        Ok(Ok((socket, _))) => Ok(socket),
    }
}

// ── the session ──────────────────────────────────────────────────────────────────────────────

/// What a chat tells the terminal. Text is already safe to print.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatEvent {
    /// The agent is ready for messages. `resumed` is true when this continues an earlier session
    /// after a reconnect.
    Ready { session_id: String, resumed: bool },
    /// What the agent said.
    Text(String),
    /// Something the person should know that is not the agent speaking.
    Notice(String),
    /// A failure. The chat may continue or may be followed by `Closed`.
    Error(String),
    /// The chat has ended; nothing more will arrive.
    Closed,
}

#[derive(Debug, Clone)]
pub struct Options {
    /// How long to wait for the agent's `session` frame after connecting.
    pub init_timeout: Duration,
    pub connect_timeout: Duration,
    /// Reconnects (after an expired login or a dropped connection) before giving up.
    pub max_reconnects: u32,
    pub reconnect_delay: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            init_timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(10),
            max_reconnects: 3,
            reconnect_delay: Duration::from_secs(1),
        }
    }
}

enum Command {
    Send(String),
    Close,
}

/// A running chat. Dropping it ends the chat.
pub struct ChatHandle {
    commands: mpsc::UnboundedSender<Command>,
    task: JoinHandle<()>,
}

impl ChatHandle {
    /// Start connecting. Events arrive on the returned receiver, beginning with `Ready` (or an
    /// `Error` then `Closed`).
    pub fn start(
        target: Target,
        tokens: TokenFn,
        ca_bundle: Option<&std::path::Path>,
        options: Options,
    ) -> Result<(ChatHandle, mpsc::UnboundedReceiver<ChatEvent>), CliError> {
        let tls = crate::http::websocket_tls(ca_bundle)?;
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (events, event_rx) = mpsc::unbounded_channel();
        let driver = Driver {
            target,
            tokens,
            options,
            tls,
            commands: command_rx,
            events,
            session_id: None,
            pending: VecDeque::new(),
            in_flight: None,
        };
        let task = tokio::spawn(driver.run());
        Ok((ChatHandle { commands, task }, event_rx))
    }

    /// Queue a message. It is sent as soon as the agent is ready, and not before.
    pub fn send(&self, message: &str) -> Result<(), CliError> {
        if message.len() > MAX_MESSAGE_BYTES {
            return Err(CliError::Failed(format!(
                "that message is over {} KiB; send less at once",
                MAX_MESSAGE_BYTES / 1024
            )));
        }
        self.commands
            .send(Command::Send(message.to_string()))
            .map_err(|_| CliError::Failed("the chat has ended; start one with /chat".into()))
    }

    /// End the chat politely, waiting briefly for the close to go out.
    pub async fn close(mut self) {
        let _ = self.commands.send(Command::Close);
        if tokio::time::timeout(Duration::from_secs(2), &mut self.task)
            .await
            .is_err()
        {
            self.task.abort();
        }
    }
}

impl Drop for ChatHandle {
    fn drop(&mut self) {
        // Without this an abandoned chat would sit connected until the server timed it out.
        self.task.abort();
    }
}

struct Driver {
    target: Target,
    tokens: TokenFn,
    options: Options,
    tls: Arc<rustls::ClientConfig>,
    commands: mpsc::UnboundedReceiver<Command>,
    events: mpsc::UnboundedSender<ChatEvent>,
    session_id: Option<String>,
    /// Messages typed before the agent was ready.
    pending: VecDeque<String>,
    /// The id of the last message sent on the current connection, to match a refusal to it.
    in_flight: Option<String>,
}

/// Why one connection ended.
enum Outcome {
    /// The person ended the chat.
    Closed,
    /// The login's access token expired: get a fresh one and reconnect.
    Reauthenticate,
    /// The connection dropped. `ready` is whether the agent had said hello.
    Dropped {
        detail: String,
        ready: bool,
        /// A reply/error frame proves this connection carried a complete application exchange.
        stable: bool,
    },
    /// Nothing more can be done.
    Fatal,
}

impl Driver {
    fn emit(&self, event: ChatEvent) {
        let _ = self.events.send(event);
    }

    async fn run(mut self) {
        let mut reconnects = 0u32;
        loop {
            let outcome = match self.connect_once().await {
                Ok(socket) => self.pump(socket).await,
                Err(error) if self.session_id.is_some() => Outcome::Dropped {
                    detail: text::sanitize_line(&error.to_string()),
                    ready: false,
                    stable: false,
                },
                Err(error) => {
                    self.emit(ChatEvent::Error(text::sanitize_line(&error.to_string())));
                    Outcome::Fatal
                }
            };
            match outcome {
                Outcome::Closed | Outcome::Fatal => break,
                Outcome::Reauthenticate => {
                    self.emit(ChatEvent::Notice(
                        "your sign-in was renewed; reconnecting".into(),
                    ));
                }
                Outcome::Dropped {
                    detail,
                    ready,
                    stable,
                } => {
                    if !ready && self.session_id.is_none() {
                        self.emit(ChatEvent::Error(format!(
                            "the connection closed before the agent started a session ({detail}); check the agent and Gateway logs"
                        )));
                        break;
                    }
                    if stable {
                        // Bound consecutive failures, not the lifetime number of successful
                        // reconnects in a long-running chat.
                        reconnects = 0;
                    }
                    if reconnects >= self.options.max_reconnects {
                        self.emit(ChatEvent::Error(
                            "could not keep the chat connected; use /chat to start again".into(),
                        ));
                        break;
                    }
                    reconnects += 1;
                    self.emit(ChatEvent::Notice(format!(
                        "the connection dropped ({detail}); reconnecting"
                    )));
                    tokio::time::sleep(self.options.reconnect_delay).await;
                    continue;
                }
            }
            if reconnects >= self.options.max_reconnects {
                self.emit(ChatEvent::Error(
                    "could not keep the chat connected; use /chat to start again".into(),
                ));
                break;
            }
            reconnects += 1;
        }
        self.emit(ChatEvent::Closed);
    }

    async fn connect_once(&mut self) -> Result<Socket, CliError> {
        let token = (self.tokens)().await?.ok_or_else(|| {
            CliError::LoginRequired("no user login for the chat; run /login".into())
        })?;
        let url = chat_url(&self.target, self.session_id.as_deref())?;
        connect(&url, &token, self.tls.clone(), self.options.connect_timeout).await
    }

    async fn pump(&mut self, socket: Socket) -> Outcome {
        let (mut sink, mut stream) = socket.split();
        let mut ready = false;
        let mut stable = false;
        self.in_flight = None;
        let init_deadline = tokio::time::sleep(self.options.init_timeout);
        tokio::pin!(init_deadline);

        loop {
            tokio::select! {
                () = &mut init_deadline, if !ready => {
                    self.emit(ChatEvent::Error(format!(
                        "the agent did not start a session within {} seconds; check that it is running",
                        self.options.init_timeout.as_secs()
                    )));
                    return Outcome::Fatal;
                }
                command = self.commands.recv() => match command {
                    None | Some(Command::Close) => {
                        let _ = sink.send(Message::Close(None)).await;
                        return Outcome::Closed;
                    }
                    Some(Command::Send(message)) if ready => {
                        if let Err(detail) = self.send_message(&mut sink, &message).await {
                            self.pending.push_front(message);
                            return Outcome::Dropped { detail, ready, stable };
                        }
                    }
                    Some(Command::Send(message)) => self.pending.push_back(message),
                },
                incoming = stream.next() => {
                    let Some(incoming) = incoming else {
                        return Outcome::Dropped { detail: "closed by the server".into(), ready, stable };
                    };
                    let message = match incoming {
                        Ok(message) => message,
                        Err(error) => return Outcome::Dropped { detail: error.to_string(), ready, stable },
                    };
                    match message {
                        Message::Text(raw) => match parse_frame(&raw) {
                            Frame::Session { session_id, turn_types } => {
                                if !turn_types.iter().any(|t| t == "chat") {
                                    self.emit(ChatEvent::Error("this agent does not take chat messages".into()));
                                    let _ = sink.send(Message::Close(None)).await;
                                    return Outcome::Fatal;
                                }
                                let resumed = self.session_id.as_deref() == Some(session_id.as_str());
                                self.session_id = Some(session_id.clone());
                                ready = true;
                                self.emit(ChatEvent::Ready { session_id, resumed });
                                while let Some(message) = self.pending.pop_front() {
                                    if let Err(detail) = self.send_message(&mut sink, &message).await {
                                        self.pending.push_front(message);
                                        return Outcome::Dropped { detail, ready, stable };
                                    }
                                }
                            }
                            Frame::AuthenticationRequired { admitted, client_message_id } => {
                                if let Some(sent) = self.in_flight.take() {
                                    if !admitted && client_message_id.as_deref() == Some(sent.as_str()) {
                                        self.emit(ChatEvent::Notice(
                                            "your last message was not sent; send it again once the chat has reconnected".into(),
                                        ));
                                    } else {
                                        self.emit(ChatEvent::Notice(
                                            "your last message may have been received; it will not be sent again automatically".into(),
                                        ));
                                    }
                                }
                                return Outcome::Reauthenticate;
                            }
                            Frame::Text(body) => {
                                stable = true;
                                self.emit(ChatEvent::Text(body));
                            }
                            Frame::Error(message) => {
                                stable = true;
                                self.emit(ChatEvent::Error(message));
                            }
                            Frame::Ignored => {}
                        },
                        Message::Close(frame) => {
                            let code = frame.as_ref().map_or(1005, |f| u16::from(f.code));
                            if code == CLOSE_REAUTHENTICATE {
                                return Outcome::Reauthenticate;
                            }
                            return Outcome::Dropped { detail: format!("closed with code {code}"), ready, stable };
                        }
                        Message::Ping(payload) => {
                            let _ = sink.send(Message::Pong(payload)).await;
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    async fn send_message<S>(&mut self, sink: &mut S, message: &str) -> Result<(), String>
    where
        S: futures_util::Sink<Message, Error = tungstenite::Error> + Unpin,
    {
        let id = Uuid::new_v4().to_string();
        let frame = json!({"text": message, "clientMessageId": id}).to_string();
        sink.send(Message::Text(frame))
            .await
            .map_err(|e| e.to_string())?;
        self.in_flight = Some(id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(id: &str) -> Agent {
        Agent {
            service_id: id.into(),
            env_tag: "dev".into(),
        }
    }

    fn roster() -> Vec<Agent> {
        agents(
            &[
                "com.networknt.agent.advisor-1.0.0".into(),
                "com.networknt.agent.tech-support-1.0.0".into(),
                "com.networknt.agent.tech-billing-2.1.0".into(),
            ],
            "dev",
        )
    }

    #[test]
    fn a_short_name_drops_the_prefix_and_the_version() {
        assert_eq!(agent("com.networknt.agent.advisor-1.0.0").name(), "advisor");
        assert_eq!(
            agent("com.networknt.agent.tech-support-1.0.0").name(),
            "tech-support"
        );
        assert_eq!(agent("plain-service").name(), "plain-service");
        assert_eq!(agent("plain-service-2").name(), "plain-service");
    }

    #[test]
    fn an_agent_is_found_by_id_name_or_unique_part() {
        let roster = roster();
        assert_eq!(
            find_agent(&roster, "com.networknt.agent.advisor-1.0.0")
                .unwrap()
                .name(),
            "advisor"
        );
        assert_eq!(find_agent(&roster, "Advisor").unwrap().name(), "advisor");
        assert_eq!(
            find_agent(&roster, "support").unwrap().name(),
            "tech-support"
        );
        let ambiguous = find_agent(&roster, "tech").unwrap_err();
        assert!(
            ambiguous.contains("tech-support") && ambiguous.contains("tech-billing"),
            "{ambiguous}"
        );
        assert!(
            find_agent(&roster, "nobody")
                .unwrap_err()
                .contains("no agent matches")
        );
        assert!(find_agent(&roster, "  ").is_err());
    }

    #[test]
    fn a_session_frame_carries_the_id_and_turn_types() {
        assert_eq!(
            parse_frame(
                r#"{"type":"session","session_id":"s-1","turnTypes":["chat","coding"],"defaultTurnType":"chat"}"#
            ),
            Frame::Session {
                session_id: "s-1".into(),
                turn_types: vec!["chat".into(), "coding".into()]
            }
        );
        // An older agent sends no turn types and takes chat.
        assert_eq!(
            parse_frame(r#"{"type":"session","session_id":"s-2"}"#),
            Frame::Session {
                session_id: "s-2".into(),
                turn_types: vec!["chat".into()]
            }
        );
        for bad in [
            r#"{"type":"session"}"#,
            r#"{"type":"session","session_id":"   "}"#,
            r#"{"type":"session","session_id":42}"#,
        ] {
            assert!(matches!(parse_frame(bad), Frame::Error(_)), "{bad}");
        }
        let long = format!(
            r#"{{"type":"session","session_id":"{}"}}"#,
            "a".repeat(MAX_SESSION_ID_CHARS + 1)
        );
        assert!(matches!(parse_frame(&long), Frame::Error(_)));
    }

    #[test]
    fn authentication_required_says_whether_the_message_was_admitted() {
        assert_eq!(
            parse_frame(
                r#"{"type":"authentication_required","admitted":false,"clientMessageId":"m1"}"#
            ),
            Frame::AuthenticationRequired {
                admitted: false,
                client_message_id: Some("m1".into())
            }
        );
        // No verdict is treated as "may have been taken".
        assert_eq!(
            parse_frame(r#"{"type":"authentication_required"}"#),
            Frame::AuthenticationRequired {
                admitted: true,
                client_message_id: None
            }
        );
    }

    #[test]
    fn agent_text_is_stripped_of_terminal_control_sequences() {
        let Frame::Text(body) = parse_frame(
            "{\"type\":\"text\",\"text\":\"hi \\u001b]0;pwned\\u0007\\u001b[2Jthere\\r\\nnext\"}",
        ) else {
            panic!("not text");
        };
        assert!(
            !body.contains('\u{1b}') && !body.contains('\u{7}') && !body.contains('\r'),
            "{body:?}"
        );
        assert!(
            body.contains("hi ") && body.contains("there") && body.contains("\nnext"),
            "{body:?}"
        );

        let Frame::Error(message) =
            parse_frame("{\"type\":\"error\",\"message\":\"bad\\u001b[31m\\nred\"}")
        else {
            panic!("not an error");
        };
        assert_eq!(message, "bad[31m red");

        let Frame::Text(raw) = parse_frame("not json \u{1b}[2J") else {
            panic!("not text")
        };
        assert_eq!(raw, "not json [2J");
    }

    #[test]
    fn other_frames_are_ignored_and_malformed_ones_do_not_panic() {
        for frame in [
            r#"{"type":"workspaceCatalog","workspaces":[]}"#,
            r#"{"type":"authentication_context","expiresAt":1}"#,
            r#"{"type":"turnAccepted","turnId":"t","clientMessageId":"m"}"#,
            r#"{"type":"turn_status","turns":[]}"#,
            r#"{"type":"executionResult","turnId":"t","state":"COMPLETED"}"#,
            r#"{"type":"something-new"}"#,
            r#"{"no":"type"}"#,
            "[]",
            "null",
        ] {
            // `[]` and `null` are valid JSON with no `type`; the rest are frames the CLI skips.
            assert_eq!(parse_frame(frame), Frame::Ignored, "{frame}");
        }
        assert!(matches!(parse_frame(r#"{"type":"text"}"#), Frame::Error(_)));
        assert!(matches!(
            parse_frame(r#"{"type":"error"}"#),
            Frame::Error(_)
        ));
    }

    fn target(gateway: &str) -> Target {
        Target {
            gateway_uri: gateway.into(),
            agent: agent("com.networknt.agent.advisor-1.0.0"),
            user_id: "a b@example.test".into(),
        }
    }

    #[test]
    fn the_chat_address_is_a_websocket_url_with_the_agent_in_the_query_and_no_token() {
        let url = chat_url(&target("https://localhost"), None).unwrap();
        assert_eq!(url.scheme(), "wss");
        assert_eq!(url.path(), "/chat");
        let pairs: Vec<(String, String)> = url
            .query_pairs()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("userId".to_string(), "a b@example.test".to_string()),
                (
                    "serviceId".into(),
                    "com.networknt.agent.advisor-1.0.0".into()
                ),
                ("envTag".into(), "dev".into()),
                ("protocol".into(), "http".into()),
            ]
        );
        assert!(!url.as_str().to_ascii_lowercase().contains("token"));

        let resumed = chat_url(&target("https://localhost/"), Some("s&1")).unwrap();
        assert_eq!(resumed.path(), "/chat");
        assert!(
            resumed
                .query_pairs()
                .any(|(k, v)| k == "sessionId" && v == "s&1")
        );
        assert_eq!(
            resumed
                .query_pairs()
                .filter(|(k, _)| k == "sessionId")
                .count(),
            1,
            "{resumed}"
        );
    }

    #[test]
    fn the_chat_address_keeps_a_base_path_and_refuses_other_schemes() {
        assert_eq!(
            chat_url(&target("https://gw.example.test/portal"), None)
                .unwrap()
                .path(),
            "/portal/chat"
        );
        assert_eq!(
            chat_url(&target("http://localhost:8080"), None)
                .unwrap()
                .scheme(),
            "ws"
        );
        assert!(chat_url(&target("ftp://gw"), None).is_err());
        assert!(chat_url(&target("not a url"), None).is_err());
    }

    #[tokio::test]
    async fn an_over_long_message_is_refused_before_it_is_queued() {
        let (commands, mut queue) = mpsc::unbounded_channel();
        let handle = ChatHandle {
            commands,
            task: tokio::spawn(async {}),
        };
        assert!(handle.send(&"x".repeat(MAX_MESSAGE_BYTES + 1)).is_err());
        assert!(queue.try_recv().is_err());
        assert!(handle.send("ok").is_ok());
        assert!(matches!(queue.try_recv(), Ok(Command::Send(m)) if m == "ok"));
    }
}
