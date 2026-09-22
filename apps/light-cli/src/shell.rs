//! The Light CLI session: one long-lived terminal that stays until `/exit`.
//!
//! Every capability is a slash command inside it (`/login`, `/agents`, `/chat`, ...); anything
//! that is not a command is said to the agent you are chatting with. The same [`Shell`] serves
//! the interactive terminal, piped input and `-c` one-shots, so they behave identically and a
//! test can drive it line by line.
//!
//! Output goes through an [`Output`] so a chat reply arriving while you type does not garble
//! your line, and everything an agent or server says is made safe to print first.

use std::sync::Arc;
use std::time::Duration;

use futures_util::FutureExt;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::auth::{self, AuthStatus, LoginOptions};
use crate::chat::{self, Agent, ChatEvent, ChatHandle, Target, TokenFn};
use crate::config::{CliConfig, Secret, Settings, ensure_transport_is_safe};
use crate::error::CliError;
use crate::output::Output;
use crate::remote;
use crate::session;
use crate::text;

/// What the person typed, understood.
#[derive(Debug, PartialEq, Eq)]
pub enum Line {
    Empty,
    /// `/name args`
    Command {
        name: String,
        args: String,
    },
    /// Anything else: said to the agent.
    Say(String),
}

pub fn parse_line(line: &str) -> Line {
    let line = line.trim();
    if line.is_empty() {
        return Line::Empty;
    }
    // A doubled slash sends a message that begins with one: `//etc/hosts` says `/etc/hosts`.
    if let Some(rest) = line.strip_prefix("//") {
        return Line::Say(format!("/{rest}"));
    }
    match line.strip_prefix('/') {
        Some(command) => {
            let (name, args) = command
                .split_once(char::is_whitespace)
                .unwrap_or((command, ""));
            Line::Command {
                name: name.to_ascii_lowercase(),
                args: args.trim().to_string(),
            }
        }
        None => Line::Say(line.to_string()),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Exit,
}

pub const HELP: &str = "\
Commands (anything else you type is said to the agent you are chatting with):
  /help                          this list
  /whoami                        who is signed in, and when the login ends
  /login [--open] [--scope S]    sign in: shows a code to approve in any browser
  /logout [--local]              end the login on the server and delete the local tokens
  /agents                        the agents you can chat with
  /chat [agent]                  start a chat (a name, or part of a service id)
  /new                           start a fresh session with the same agent
  /disconnect                    end the chat
  /tools                         connect to the Gateway and list its tools
  /exit                          leave (also /quit and Ctrl-D)
Start a line with // to say something that begins with a slash.";

pub fn human(seconds: i64) -> String {
    let (d, h, m) = (
        seconds / 86_400,
        seconds % 86_400 / 3_600,
        seconds % 3_600 / 60,
    );
    match (d, h) {
        (0, 0) => format!("{m}m"),
        (0, _) => format!("{h}h {m}m"),
        _ => format!("{d}d {h}h"),
    }
}

pub fn describe_auth(status: &AuthStatus) -> String {
    let who = match (&status.email, &status.user_id) {
        (Some(email), _) => email.clone(),
        (None, Some(id)) => id.clone(),
        _ => "unknown user".to_string(),
    };
    let ends = |status: &AuthStatus| match (&status.login_expires_at, status.login_expires_in) {
        (Some(at), Some(left)) if left > 0 => format!("the login ends {at} (in {})", human(left)),
        (Some(at), _) => format!("the login ended {at}"),
        _ => "the login has no recorded end".to_string(),
    };
    let roles = status
        .roles
        .as_deref()
        .map(|r| format!(" roles={r}"))
        .unwrap_or_default();
    match status.state {
        "signed-out" => "signed out: run /login".to_string(),
        "signed-in" => format!(
            "signed in as {who}{roles}\naccess token valid for {}; {}",
            human(status.access_expires_in.unwrap_or_default()),
            ends(status)
        ),
        "access-expired" => format!(
            "signed in as {who}{roles}\nthe access token has expired and refreshes on the next use; {}",
            ends(status)
        ),
        _ => format!("{who}: {}; run /login", ends(status)),
    }
}

/// The user's access token: `LIGHT_USER_ACCESS_TOKEN` if set (development and tests), else the
/// signed-in session, refreshed if it is about to expire. Never read from arguments.
pub async fn access_token(config: &CliConfig) -> Result<Option<Secret>, CliError> {
    if let Some(token) = std::env::var("LIGHT_USER_ACCESS_TOKEN")
        .ok()
        .as_deref()
        .and_then(Secret::from_authorization)
    {
        return Ok(Some(token));
    }
    auth::user_token(config).await
}

fn default_tokens(config: Arc<CliConfig>) -> TokenFn {
    Arc::new(move || {
        let config = Arc::clone(&config);
        async move { access_token(&config).await }.boxed()
    })
}

/// How a running chat is doing, shared with the task that prints what the agent says.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Progress {
    ready: bool,
    ended: bool,
    failed: bool,
    /// Text or error messages received so far.
    replies: u64,
}

struct ActiveChat {
    agent: Agent,
    handle: ChatHandle,
    progress: watch::Receiver<Progress>,
    printer: JoinHandle<()>,
}

/// One line of an agent's words, the name in front and continuation lines indented under it.
fn speak(name: &str, body: &str) -> String {
    let indent = " ".repeat(name.chars().count() + 2);
    let mut lines = body.trim_end().lines();
    let mut out = format!("{name}> {}", lines.next().unwrap_or_default());
    for line in lines {
        out.push('\n');
        if !line.is_empty() {
            out.push_str(&indent);
            out.push_str(line);
        }
    }
    out
}

pub struct Shell {
    config: Arc<CliConfig>,
    out: Arc<dyn Output>,
    interactive: bool,
    tokens: TokenFn,
    settings: Option<Settings>,
    chat: Option<ActiveChat>,
    failure: Option<CliError>,
    pub chat_options: chat::Options,
    /// Non-interactive only: how long to wait for an agent's first reply to a message.
    pub reply_timeout: Duration,
    /// Non-interactive only: how long a reply must be quiet before the next line is read.
    pub reply_quiet: Duration,
}

impl Shell {
    pub fn new(config: CliConfig, out: Arc<dyn Output>, interactive: bool) -> Self {
        let config = Arc::new(config);
        Shell {
            tokens: default_tokens(Arc::clone(&config)),
            config,
            out,
            interactive,
            settings: None,
            chat: None,
            failure: None,
            chat_options: chat::Options::default(),
            reply_timeout: Duration::from_secs(120),
            reply_quiet: Duration::from_millis(750),
        }
    }

    /// Replace where access tokens come from (tests).
    pub fn with_tokens(mut self, tokens: TokenFn) -> Self {
        self.tokens = tokens;
        self
    }

    /// The first command failure, for the exit code of a non-interactive run.
    pub fn take_failure(&mut self) -> Option<CliError> {
        self.failure.take()
    }

    /// The prompt: the agent you are talking to, if any.
    pub fn prompt(&self) -> String {
        match &self.chat {
            Some(chat) if !chat.progress.borrow().ended => format!("{}> ", chat.agent.name()),
            _ => "light> ".to_string(),
        }
    }

    pub fn banner(&self) -> String {
        let signed_in = match auth::status(&self.config) {
            Ok(status) => describe_auth(&status)
                .lines()
                .next()
                .unwrap_or_default()
                .to_string(),
            Err(error) => format!("could not read the login: {error}"),
        };
        format!(
            "Light CLI {} [{}]\n{signed_in}\nType /help for commands, /exit to leave.",
            env!("CARGO_PKG_VERSION"),
            text::sanitize_line(&self.config.env_tag)
        )
    }

    /// Run one line. Failures are printed, remembered for the exit code, and never end the session.
    pub async fn handle(&mut self, line: &str) -> Flow {
        let parsed = parse_line(line);
        let interrupted = tokio::signal::ctrl_c();
        let result = tokio::select! {
            result = self.dispatch(parsed) => result,
            _ = interrupted => {
                self.abandon_unready_chat().await;
                Err(CliError::Failed("cancelled".into()))
            }
        };
        match result {
            Ok(flow) => flow,
            Err(error) => {
                self.out.line(&format!(
                    "error: {}",
                    text::sanitize_line(&error.to_string())
                ));
                self.failure.get_or_insert(error);
                Flow::Continue
            }
        }
    }

    /// Leave cleanly: end the chat if there is one.
    pub async fn shutdown(&mut self) {
        self.end_chat().await;
    }

    async fn dispatch(&mut self, line: Line) -> Result<Flow, CliError> {
        match line {
            Line::Empty => {}
            Line::Say(message) => self.say(&message).await?,
            Line::Command { name, args } => match name.as_str() {
                "help" | "?" => self.out.line(HELP),
                "exit" | "quit" => {
                    self.end_chat().await;
                    return Ok(Flow::Exit);
                }
                "whoami" | "status" => {
                    let status = auth::status(&self.config)?;
                    self.out.line(&describe_auth(&status));
                    // A script asking "am I signed in?" gets its answer in the exit code (6); at the
                    // terminal the report above is the whole answer.
                    if !self.interactive && matches!(status.state, "signed-out" | "login-ended") {
                        self.failure
                            .get_or_insert(CliError::LoginRequired("no valid login".into()));
                    }
                }
                "login" => self.login(&args).await?,
                "logout" => self.logout(&args).await?,
                "agents" => self.list_agents().await?,
                "chat" => self.start_chat(&args).await?,
                "new" => self.new_session().await?,
                "disconnect" => self.disconnect().await,
                "tools" => self.tools().await?,
                other => {
                    return Err(CliError::Failed(format!(
                        "unknown command /{}; /help lists the commands",
                        text::strip_controls(other)
                    )));
                }
            },
        }
        Ok(Flow::Continue)
    }

    async fn settings(&mut self) -> Result<Settings, CliError> {
        if let Some(settings) = &self.settings {
            return Ok(settings.clone());
        }
        let (settings, from) = remote::load_settings(&self.config).await?;
        self.out.line(&format!(
            "settings: {}",
            remote::origin_of(&from, "cli.gatewayUri")
        ));
        self.settings = Some(settings.clone());
        Ok(settings)
    }

    // ── sign in and out ───────────────────────────────────────────────────────────────────────

    async fn login(&mut self, args: &str) -> Result<(), CliError> {
        let (mut open, mut scope) = (false, None);
        let mut words = args.split_whitespace();
        while let Some(word) = words.next() {
            match word {
                "--open" => open = true,
                "--scope" => {
                    scope = Some(
                        words
                            .next()
                            .ok_or_else(|| {
                                CliError::Failed("usage: /login [--open] [--scope S]".into())
                            })?
                            .to_string(),
                    );
                }
                _ => {
                    return Err(CliError::Failed(
                        "usage: /login [--open] [--scope S]".into(),
                    ));
                }
            }
        }
        let options = LoginOptions {
            scope,
            ..LoginOptions::default()
        };
        let out = Arc::clone(&self.out);
        let status = auth::login(&self.config, &options, move |code| {
            let complete = code.verification_uri_complete.as_deref().map(auth::printable);
            let uri = auth::printable(&code.verification_uri);
            out.line(&format!(
                "To sign in, open {} (or go to {uri} and enter the code)\ncode: {}\n\
                 Only approve this if you are connecting from this CLI right now: the page shows the same code.\n\
                 The code expires in {}. Waiting for approval... (Ctrl-C cancels)",
                complete.as_deref().unwrap_or(&uri),
                auth::printable(&code.user_code),
                human(i64::try_from(code.expires_in).unwrap_or(i64::MAX)),
            ));
            let target = code.verification_uri_complete.as_deref().unwrap_or(&code.verification_uri);
            if auth::should_open_browser(open, std::env::var_os("SSH_CONNECTION").is_some())
                && !auth::open_browser(target)
            {
                out.line("(could not open a browser; open the address above yourself)");
            }
        })
        .await?;
        self.out.line(&describe_auth(&status));
        Ok(())
    }

    async fn logout(&mut self, args: &str) -> Result<(), CliError> {
        let local = match args.trim() {
            "" => false,
            "--local" => true,
            _ => return Err(CliError::Failed("usage: /logout [--local]".into())),
        };
        // A chat is the signed-in user's: it ends with the login.
        self.end_chat().await;
        let outcome = auth::logout(&self.config, local).await?;
        self.out.line(if !outcome.was_signed_in {
            "not signed in"
        } else if outcome.revoked_on_server {
            "signed out: the login was ended on the server and the local tokens were deleted"
        } else {
            "local tokens deleted; the login stays valid on the server until it expires"
        });
        Ok(())
    }

    async fn tools(&mut self) -> Result<(), CliError> {
        let token = (self.tokens)().await?;
        let report = crate::gateway::check(&self.config, token).await?;
        self.out.line(&format!(
            "connected to {}\nMCP {} session established{}\n{} tools: {}",
            text::sanitize_line(&report.endpoint),
            text::sanitize_line(&report.protocol_version),
            report
                .server
                .as_deref()
                .map(|s| format!(" with {}", text::sanitize_line(s)))
                .unwrap_or_default(),
            report.tool_count,
            report
                .tools
                .iter()
                .map(|t| text::sanitize_line(t))
                .collect::<Vec<_>>()
                .join(", "),
        ));
        Ok(())
    }

    // ── agent chat ────────────────────────────────────────────────────────────────────────────

    async fn roster(&mut self) -> Result<Vec<Agent>, CliError> {
        let settings = self.settings().await?;
        if settings.agent_service_ids.is_empty() {
            return Err(CliError::Config(
                "no agents are configured: set cli.agentServiceIds (comma-separated service ids) in cli.yml or the config server".into(),
            ));
        }
        Ok(chat::agents(
            &settings.agent_service_ids,
            &self.config.env_tag,
        ))
    }

    async fn list_agents(&mut self) -> Result<(), CliError> {
        let roster = self.roster().await?;
        let active = self
            .chat
            .as_ref()
            .filter(|c| !c.progress.borrow().ended)
            .map(|c| c.agent.service_id.clone());
        let width = roster
            .iter()
            .map(|a| a.name().chars().count())
            .max()
            .unwrap_or(0);
        let mut lines = vec![format!(
            "agents in {}:",
            text::sanitize_line(&self.config.env_tag)
        )];
        for agent in &roster {
            let mark = if active.as_deref() == Some(agent.service_id.as_str()) {
                '*'
            } else {
                ' '
            };
            lines.push(format!(
                "{mark} {:<width$}  {}",
                agent.name(),
                agent.service_id
            ));
        }
        lines.push("/chat <name> starts a chat".to_string());
        self.out.line(&lines.join("\n"));
        Ok(())
    }

    async fn start_chat(&mut self, query: &str) -> Result<(), CliError> {
        let roster = self.roster().await?;
        let agent = if query.is_empty() {
            match roster.as_slice() {
                [only] => only.clone(),
                _ => return Err(CliError::Failed("name an agent: /agents lists them".into())),
            }
        } else {
            chat::find_agent(&roster, query)
                .map_err(CliError::Failed)?
                .clone()
        };
        self.connect(agent).await
    }

    async fn new_session(&mut self) -> Result<(), CliError> {
        let agent =
            self.chat.as_ref().map(|c| c.agent.clone()).ok_or_else(|| {
                CliError::Failed("no chat to renew: /chat <agent> starts one".into())
            })?;
        self.connect(agent).await
    }

    async fn connect(&mut self, agent: Agent) -> Result<(), CliError> {
        let settings = self.settings().await?;
        let gateway_uri = settings.gateway_uri.ok_or_else(|| {
            CliError::Config("no Gateway URL: set cli.gatewayUri in cli.yml, the config server, or CLI_GATEWAYURI".into())
        })?;
        ensure_transport_is_safe(&gateway_uri)?;
        // Ask for the token first, so a missing or ended login is reported here, plainly.
        let token = (self.tokens)()
            .await?
            .ok_or_else(|| CliError::LoginRequired("sign in first: /login".into()))?;
        let claims = session::unverified_claims(token.expose());
        let user_id = claims.email.or(claims.user_id).ok_or_else(|| {
            CliError::Failed("the access token names no user; sign in again with /login".into())
        })?;

        self.end_chat().await;
        let name = agent.name().to_string();
        let (handle, mut events) = ChatHandle::start(
            Target {
                gateway_uri,
                agent: agent.clone(),
                user_id,
            },
            Arc::clone(&self.tokens),
            self.config.ca_bundle.as_deref(),
            self.chat_options.clone(),
        )?;
        let (progress_tx, progress) = watch::channel(Progress::default());
        let out = Arc::clone(&self.out);
        let printer = tokio::spawn({
            let name = name.clone();
            async move {
                while let Some(event) = events.recv().await {
                    match event {
                        ChatEvent::Ready {
                            session_id,
                            resumed,
                        } => {
                            out.line(&if resumed {
                                format!("reconnected to {name}")
                            } else {
                                format!(
                                    "connected to {name} (session {}); type to chat, /disconnect to leave",
                                    text::strip_controls(&session_id)
                                )
                            });
                            progress_tx.send_modify(|p| p.ready = true);
                        }
                        ChatEvent::Text(body) => {
                            out.line(&speak(&name, &body));
                            progress_tx.send_modify(|p| p.replies += 1);
                        }
                        ChatEvent::Notice(notice) => out.line(&format!("· {notice}")),
                        ChatEvent::Error(error) => {
                            out.line(&format!("error: {error}"));
                            progress_tx.send_modify(|p| {
                                p.failed = true;
                                p.replies += 1;
                            });
                        }
                        ChatEvent::Closed => {
                            out.line(&format!("chat with {name} ended"));
                            break;
                        }
                    }
                }
                progress_tx.send_modify(|p| p.ended = true);
            }
        });
        self.chat = Some(ActiveChat {
            agent,
            handle,
            progress,
            printer,
        });
        self.out.line(&format!("connecting to {name}..."));

        let wait = self.chat_options.init_timeout
            + self.chat_options.connect_timeout
            + Duration::from_secs(5);
        let mut progress = self
            .chat
            .as_ref()
            .map(|c| c.progress.clone())
            .expect("just set");
        let outcome = tokio::time::timeout(wait, progress.wait_for(|p| p.ready || p.ended)).await;
        match outcome {
            Ok(Ok(state)) if state.ready => Ok(()),
            _ => {
                self.end_chat().await;
                Err(CliError::Failed("the chat did not start".into()))
            }
        }
    }

    async fn say(&mut self, message: &str) -> Result<(), CliError> {
        let Some(chat) = self.chat.as_ref().filter(|c| !c.progress.borrow().ended) else {
            self.chat = None;
            return Err(CliError::Failed(
                "not in a chat: /agents lists agents, /chat <agent> starts one".into(),
            ));
        };
        let mut progress = chat.progress.clone();
        let before = progress.borrow().replies;
        chat.handle.send(message)?;
        if self.interactive {
            return Ok(());
        }
        // A script cannot watch the terminal: wait for the reply so the next line, or /exit,
        // does not cut it off.
        let (first_failed, first_ended) = {
            let first = tokio::time::timeout(
                self.reply_timeout,
                progress.wait_for(|p| p.replies > before || p.ended),
            )
            .await
            .map_err(|_| CliError::Failed("timed out waiting for the agent's reply".into()))?
            .map_err(|_| CliError::Failed("the chat ended before the agent replied".into()))?;
            (first.failed, first.ended)
        };
        if first_failed {
            return Err(CliError::Failed(
                "the agent or chat connection reported an error".into(),
            ));
        }
        if first_ended {
            return Err(CliError::Failed(
                "the chat ended before the agent replied".into(),
            ));
        }
        loop {
            let seen = progress.borrow().replies;
            let more = tokio::time::timeout(
                self.reply_quiet,
                progress.wait_for(|p| p.replies > seen || p.ended),
            )
            .await
            .ok()
            .and_then(Result::ok);
            match more {
                Some(state) if state.failed => {
                    return Err(CliError::Failed(
                        "the agent or chat connection reported an error".into(),
                    ));
                }
                Some(state) if !state.ended => {}
                _ => return Ok(()),
            }
        }
    }

    async fn disconnect(&mut self) {
        if self.chat.is_some() {
            self.end_chat().await;
        } else {
            self.out.line("not in a chat");
        }
    }

    async fn end_chat(&mut self) {
        if let Some(chat) = self.chat.take() {
            let progress = chat.progress.clone();
            chat.handle.close().await;
            // The printer prints the last lines and "ended" once the driver has closed.
            let _ = tokio::time::timeout(Duration::from_secs(1), chat.printer).await;
            if !self.interactive && progress.borrow().failed {
                self.failure.get_or_insert_with(|| {
                    CliError::Failed("the agent or chat connection reported an error".into())
                });
            }
        }
    }

    /// Ctrl-C while a chat is still connecting: drop it rather than leave a half-started one.
    async fn abandon_unready_chat(&mut self) {
        if self
            .chat
            .as_ref()
            .is_some_and(|c| !c.progress.borrow().ready)
        {
            self.end_chat().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(name: &str, args: &str) -> Line {
        Line::Command {
            name: name.into(),
            args: args.into(),
        }
    }

    #[test]
    fn a_line_is_a_command_a_message_or_nothing() {
        assert_eq!(parse_line(""), Line::Empty);
        assert_eq!(parse_line("   \t "), Line::Empty);
        assert_eq!(parse_line("/exit"), command("exit", ""));
        assert_eq!(
            parse_line("  /Chat   Advisor  "),
            command("chat", "Advisor")
        );
        assert_eq!(
            parse_line("/login --open --scope a b"),
            command("login", "--open --scope a b")
        );
        assert_eq!(parse_line("hello there"), Line::Say("hello there".into()));
        assert_eq!(parse_line("what is 1/2?"), Line::Say("what is 1/2?".into()));
    }

    #[test]
    fn a_doubled_slash_says_something_that_starts_with_a_slash() {
        assert_eq!(parse_line("//etc/hosts"), Line::Say("/etc/hosts".into()));
        assert_eq!(parse_line("//exit"), Line::Say("/exit".into()));
        assert_eq!(parse_line("/"), command("", ""));
    }

    #[test]
    fn an_agents_words_are_indented_under_its_name() {
        assert_eq!(speak("advisor", "one"), "advisor> one");
        assert_eq!(
            speak("advisor", "one\ntwo\n\nfour\n\n"),
            "advisor> one\n         two\n\n         four"
        );
        assert_eq!(speak("a", ""), "a> ");
    }

    #[test]
    fn durations_are_shown_in_the_largest_useful_units() {
        assert_eq!(human(59), "0m");
        assert_eq!(human(3_600 + 120), "1h 2m");
        assert_eq!(human(2 * 86_400 + 3 * 3_600 + 59), "2d 3h");
    }
}
