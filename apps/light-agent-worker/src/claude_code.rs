//! Feature-gated Claude adapter. No production dispatch/capability registration.
//! The trusted worker must validate workspace/artifacts before committing a proposal.
pub mod coding;
mod native_namespace;
pub(crate) mod runtime;
pub(crate) mod workspace;

use super::coding_session::CodingSession;
use anyhow::{Context, Result, bail, ensure};
use coding_agent_runtime::{CodingAuthenticationProfile, CodingThreadMode, CodingTurnSpec};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, Command},
    sync::{mpsc, watch},
    time::{Duration, Instant},
};
use uuid::Uuid;

pub const ADAPTER_ID: &str = "claude-code-v1";
pub const VERSION: &str = "2.1.269 (Claude Code)";
pub const BINARY_SHA256: &str = "25e44883f54419569a3d739f38cbbdaebe83b09895da0f343e1b003710a4775b";
pub const LAUNCH_CONTRACT: &str =
    include_str!("../../../contracts/claude-code/v2.1.269/phase1-launch.json");
const FRAME: usize = 1024 * 1024;
const OUTPUT: usize = 8 * FRAME;
const STDERR: usize = 64 * 1024;

pub use coding_agent_runtime::claude::{LaunchPolicy, PermissionMode, PermissionSource};
fn identifier(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b))
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaudeTurn {
    pub coding: CodingTurnSpec,
    pub policy: LaunchPolicy,
    pub native_model: Option<String>,
}

/// Runner-owned paths/identity, never deserialized from prompt or task input.
pub struct HostContext {
    pub executable: PathBuf,
    pub native_home: PathBuf,
    pub working_directory: PathBuf,
    pub thread_scope: String,
}
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Event {
    Initialized {
        native_session: String,
        model: String,
    },
    TextDelta {
        text: String,
    },
    ToolObserved {
        id: String,
        name: String,
    },
    PermissionDenied,
}
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeResult {
    pub text: String,
    pub structured_output: Option<Value>,
    pub model: String,
    pub usage: Option<Value>,
    pub permission_denials: usize,
}
/// Dropping this proposal leaves IN_FLIGHT. Only trusted artifact acceptance may commit it.
pub struct Proposal {
    pub result: NativeResult,
    session: CodingSession,
    native_session: String,
    close_after_turn: bool,
}
impl Proposal {
    /// Phase 2 calls this only after canonical patch/review validation, not on model prose.
    pub fn accept_validated(mut self, canonical_patch: &str) -> Result<Value> {
        ensure!(
            self.result.permission_denials == 0,
            "Claude permission denial requires explicit workflow resolution"
        );
        ensure!(
            canonical_patch.len() <= coding_agent_runtime::MAX_INLINE_PATCH_BYTES as usize,
            "patch too large"
        );
        self.session
            .finish(&self.native_session, canonical_patch, self.close_after_turn)
    }
}
pub enum Outcome {
    Proposal(Proposal),
    Closed(Value),
}

pub async fn execute(
    host: &HostContext,
    turn: ClaudeTurn,
    cancel: watch::Receiver<bool>,
    deadline: Instant,
    events: mpsc::Sender<Event>,
) -> Result<Outcome> {
    execute_inner(host, turn, cancel, deadline, events, BINARY_SHA256).await
}
async fn execute_inner(
    host: &HostContext,
    turn: ClaudeTurn,
    cancel: watch::Receiver<bool>,
    deadline: Instant,
    events: mpsc::Sender<Event>,
    binary_digest: &str,
) -> Result<Outcome> {
    execute_prepared(host, turn, cancel, deadline, events, binary_digest, None).await
}
async fn execute_prepared(
    host: &HostContext,
    turn: ClaudeTurn,
    cancel: watch::Receiver<bool>,
    deadline: Instant,
    events: mpsc::Sender<Event>,
    binary_digest: &str,
    workspace: Option<&coding::Workspace>,
) -> Result<Outcome> {
    turn.coding.validate()?;
    ensure!(
        turn.coding.authentication_profile == CodingAuthenticationProfile::PersonalSubscription,
        "Claude candidate requires personal authentication"
    );
    ensure!(
        host.executable.is_absolute()
            && host.native_home.is_absolute()
            && host.working_directory.is_absolute(),
        "runner paths must be absolute"
    );
    let control = turn
        .coding
        .thread
        .as_ref()
        .context("Claude requires explicit workflow thread control")?;
    turn.policy.resolve(turn.native_model.as_deref())?;
    let contract = agent_runtime_protocol::canonical_digest(
        &json!({"adapter":ADAPTER_ID,"version":VERSION,
        "binary":binary_digest,"codingIntegration":workspace.map(|_| coding_agent_runtime::claude::schema_digest()),"launchContract":agent_core::sha256_digest(LAUNCH_CONTRACT.as_bytes()),"policy":turn.policy}),
    )?;
    let checkpoint_home = workspace
        .map(|w| w.checkpoint_home.as_path())
        .unwrap_or(&host.native_home);
    let mut session =
        CodingSession::open(checkpoint_home, &host.thread_scope, &turn.coding, &contract)?;
    ensure!(
        !*cancel.borrow() && Instant::now() < deadline,
        "Claude attempt cancelled or expired"
    );
    if control.mode == CodingThreadMode::Close {
        return Ok(Outcome::Closed(session.mark_closed()?));
    }
    let model = if control.mode == CodingThreadMode::Resume {
        let previous = session
            .adapter_state()
            .and_then(|v| v["nativeModel"].as_str())
            .context("Claude checkpoint model missing")?;
        ensure!(
            turn.native_model
                .as_deref()
                .is_none_or(|model| model == previous),
            "Claude session model changed"
        );
        turn.policy.resolve(Some(previous))?.to_owned()
    } else {
        turn.policy
            .resolve(turn.native_model.as_deref())?
            .to_owned()
    };
    session.set_adapter_state(json!({"nativeModel": model}));
    let native_session = match control.mode {
        CodingThreadMode::New => Uuid::new_v4().to_string(),
        CodingThreadMode::Resume => session
            .thread_id()
            .context("native session missing")?
            .to_string(),
        CodingThreadMode::Close => unreachable!(),
    };
    Uuid::parse_str(&native_session).context("invalid native session identity")?;
    let environment = environment(&host.native_home)?;
    tokio::time::timeout_at(deadline, async {
        let mut file = tokio::fs::File::open(&host.executable).await?;
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 65536];
        loop {
            let n = file.read(&mut buffer).await?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
        }
        ensure!(
            hex::encode(hash.finalize()) == binary_digest,
            "Claude binary digest mismatch"
        );
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("Claude preflight deadline")??;
    let version = capture(host, &environment, &["--version"], cancel.clone(), deadline).await?;
    ensure!(version.trim() == VERSION, "Claude binary version mismatch");
    let auth = capture(
        host,
        &environment,
        &["auth", "status"],
        cancel.clone(),
        deadline,
    )
    .await?;
    let auth: Value = serde_json::from_str(&auth)
        .map_err(|_| anyhow::anyhow!("invalid Claude authentication status"))?;
    ensure!(
        auth["loggedIn"] == true
            && auth["authMethod"] == "claude.ai"
            && auth["apiProvider"] == "firstParty",
        "native subscription authentication required"
    );
    let mut command = command(host, &environment);
    command.args(arguments(
        &turn.policy,
        &model,
        &native_session,
        control.mode,
    ));
    let mut stream = Stream::new(&native_session, &turn.policy.models[&model]);
    stream.expected_permission = match turn.policy.permission_mode {
        PermissionMode::Inherit => None,
        PermissionMode::DontAsk => Some("dontAsk"),
        PermissionMode::BypassPermissions => Some("bypassPermissions"),
    };
    let prompt = if let Some(workspace) = workspace {
        let prompt = workspace.prepare(&session, &turn.coding).await?;
        workspace.confine(
            &mut command,
            host,
            &turn.coding,
            turn.policy.permission_source,
        )?;
        prompt
    } else {
        turn.coding.prompt.clone()
    };
    // All preflight failures leave READY untouched. Anything after this may have advanced history.
    session.begin()?;
    let status = supervise(
        command,
        prompt.as_bytes(),
        cancel,
        deadline,
        Some(events),
        |line| {
            let normalized = stream.feed(line)?;
            for event in &normalized {
                ensure!(
                    serde_json::to_vec(&event)?.len() <= FRAME - 4096,
                    "Claude normalized event too large"
                );
            }
            Ok(normalized)
        },
    )
    .await?;
    ensure!(status, "Claude process failed");
    let result = stream.finish()?;
    Ok(Outcome::Proposal(Proposal {
        result,
        session,
        native_session,
        close_after_turn: control.close_after_turn,
    }))
}
fn arguments(
    policy: &LaunchPolicy,
    model: &str,
    session: &str,
    mode: CodingThreadMode,
) -> Vec<String> {
    let mut args: Vec<String> = [
        "-p",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--permission-prompts",
        "none",
        "--model",
        model,
        if mode == CodingThreadMode::Resume {
            "--resume"
        } else {
            "--session-id"
        },
        session,
    ]
    .map(String::from)
    .into();
    if policy.permission_source == PermissionSource::AgentPolicy {
        args.extend([
            "--safe-mode".into(),
            "--tools".into(),
            policy.tools.join(","),
        ]);
        if !policy.allowed_tools.is_empty() {
            args.push("--allowedTools".into());
            args.extend(policy.allowed_tools.clone());
        }
    }
    match policy.permission_mode {
        PermissionMode::Inherit => (),
        PermissionMode::DontAsk => args.extend(["--permission-mode".into(), "dontAsk".into()]),
        PermissionMode::BypassPermissions => args.push("--dangerously-skip-permissions".into()),
    }
    args
}
fn environment(home: &Path) -> Result<BTreeMap<OsString, OsString>> {
    for key in [
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_BASE_URL",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
        "CLAUDE_CODE_SKIP_PROMPT_HISTORY",
        "CLAUDE_CODE_SIMPLE",
        "CLAUDE_CODE_SAFE_MODE",
    ] {
        ensure!(
            std::env::var_os(key).is_none_or(|v| v.is_empty()),
            "conflicting native Claude environment"
        );
    }
    let mut result = BTreeMap::new();
    for key in [
        "HOME",
        "PATH",
        "LANG",
        "LC_ALL",
        "TERM",
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "XDG_CACHE_HOME",
        "XDG_RUNTIME_DIR",
        "DBUS_SESSION_BUS_ADDRESS",
        "SSL_CERT_FILE",
        "SSL_CERT_DIR",
    ] {
        if let Some(value) = std::env::var_os(key) {
            result.insert(key.into(), value);
        }
    }
    result.insert("CLAUDE_CONFIG_DIR".into(), home.as_os_str().to_owned());
    Ok(result)
}
fn command(host: &HostContext, env: &BTreeMap<OsString, OsString>) -> Command {
    let mut cmd = Command::new(&host.executable);
    cmd.current_dir(&host.working_directory)
        .env_clear()
        .envs(env);
    cmd
}
async fn capture(
    host: &HostContext,
    env: &BTreeMap<OsString, OsString>,
    args: &[&str],
    cancel: watch::Receiver<bool>,
    deadline: Instant,
) -> Result<String> {
    let mut cmd = command(host, env);
    cmd.args(args);
    let mut bytes = Vec::new();
    let ok = supervise(cmd, b"", cancel, deadline, None, |line| {
        ensure!(
            bytes.len() + line.len() <= STDERR,
            "Claude preflight output limit"
        );
        bytes.extend(line);
        Ok(Vec::new())
    })
    .await?;
    ensure!(ok, "Claude preflight process failed");
    String::from_utf8(bytes).map_err(|_| anyhow::anyhow!("invalid Claude preflight encoding"))
}
struct Process {
    child: Child,
    group: i32,
}
impl Drop for Process {
    fn drop(&mut self) {
        // Child owns this freshly-created process group. No externally supplied PID.
        unsafe {
            libc::kill(-self.group, libc::SIGKILL);
        }
    }
}
async fn supervise<F>(
    mut command: Command,
    prompt: &[u8],
    mut cancel: watch::Receiver<bool>,
    deadline: Instant,
    events: Option<mpsc::Sender<Event>>,
    mut frame: F,
) -> Result<bool>
where
    F: FnMut(&[u8]) -> Result<Vec<Event>>,
{
    ensure!(
        !*cancel.borrow() && Instant::now() < deadline,
        "Claude attempt cancelled or expired"
    );
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let child = command.spawn().context("start Claude process")?;
    let group = child.id().context("Claude pid missing")? as i32;
    let mut process = Process { child, group };
    let mut stdin = process.child.stdin.take().context("Claude stdin missing")?;
    let stdout = process
        .child
        .stdout
        .take()
        .context("Claude stdout missing")?;
    let stderr = process
        .child
        .stderr
        .take()
        .context("Claude stderr missing")?;
    let result = {
        let work = async {
            let read_out = async {
                let mut reader = BufReader::new(stdout);
                let mut total = 0;
                loop {
                    let mut line = Vec::new();
                    let n = (&mut reader)
                        .take((FRAME + 1) as u64)
                        .read_until(b'\n', &mut line)
                        .await?;
                    if n == 0 {
                        break;
                    }
                    total += n;
                    ensure!(
                        n <= FRAME && total <= OUTPUT,
                        "Claude output limit exceeded"
                    );
                    ensure!(line.last() == Some(&b'\n'), "truncated Claude frame");
                    let normalized = frame(&line)?;
                    if let Some(events) = &events {
                        for event in normalized {
                            events
                                .send(event)
                                .await
                                .context("Claude event consumer disconnected")?;
                        }
                    }
                }
                Ok::<_, anyhow::Error>(())
            };
            let read_err = async {
                let n = tokio::io::copy(
                    &mut stderr.take((STDERR + 1) as u64),
                    &mut tokio::io::sink(),
                )
                .await?;
                ensure!(n <= STDERR as u64, "Claude stderr limit exceeded");
                Ok::<_, anyhow::Error>(())
            };
            let write = async {
                stdin.write_all(prompt).await?;
                stdin.shutdown().await?;
                drop(stdin);
                Ok::<_, anyhow::Error>(())
            };
            let wait = async { Ok::<_, anyhow::Error>(process.child.wait().await?.success()) };
            let (_, _, _, status) = tokio::try_join!(read_out, read_err, write, wait)?;
            Ok::<_, anyhow::Error>(status)
        };
        tokio::select! {
            biased;
            _ = cancel.changed() => Err(anyhow::anyhow!("Claude attempt cancelled")),
            _ = tokio::time::sleep_until(deadline) => Err(anyhow::anyhow!("Claude deadline exceeded")),
            result = work => result,
        }
    };
    unsafe {
        libc::kill(-process.group, libc::SIGKILL);
    }
    let _ = tokio::time::timeout(Duration::from_secs(2), process.child.wait()).await;
    result
}
struct Stream {
    expected_permission: Option<&'static str>,
    session: String,
    expected_model: String,
    initialized: bool,
    result: Option<NativeResult>,
    denied: usize,
}
impl Stream {
    fn new(session: &str, model: &str) -> Self {
        Self {
            session: session.into(),
            expected_model: model.into(),
            expected_permission: None,
            initialized: false,
            result: None,
            denied: 0,
        }
    }
    fn feed(&mut self, frame: &[u8]) -> Result<Vec<Event>> {
        ensure!(
            frame.len() <= FRAME && self.result.is_none(),
            "oversized or post-terminal Claude frame"
        );
        let v: Value = serde_json::from_slice(frame)
            .map_err(|_| anyhow::anyhow!("invalid Claude JSON frame"))?;
        ensure!(v.is_object(), "invalid Claude event envelope");
        if let Some(session) = v.get("session_id") {
            ensure!(session == &self.session, "Claude session mismatch");
        }
        let mut events = Vec::new();
        match v["type"].as_str().context("Claude event type missing")? {
            "system" if v["subtype"] == "init" => {
                ensure!(
                    !self.initialized
                        && v["session_id"] == self.session
                        && v["model"] == self.expected_model,
                    "Claude initialization mismatch"
                );
                ensure!(
                    self.expected_permission
                        .is_none_or(|mode| v["permissionMode"] == mode),
                    "Claude permission mode differs from admitted override"
                );
                self.initialized = true;
                events.push(Event::Initialized {
                    native_session: self.session.clone(),
                    model: self.expected_model.clone(),
                });
            }
            "system" => match v["subtype"].as_str() {
                Some("permission_denied") => {
                    self.denied += 1;
                    events.push(Event::PermissionDenied);
                }
                Some(
                    "hook_started" | "hook_progress" | "hook_response" | "status" | "api_retry"
                    | "compact_boundary" | "thinking_tokens",
                ) => (),
                Some(kind) if identifier(kind) => bail!("unsupported Claude system event: {kind}"),
                _ => bail!("unsupported Claude system event"),
            },
            "stream_event" => {
                ensure!(self.initialized, "Claude stream before initialization");
                if v["event"]["delta"]["type"] == "text_delta" {
                    let text = v["event"]["delta"]["text"]
                        .as_str()
                        .context("invalid Claude text delta")?;
                    events.push(Event::TextDelta { text: text.into() });
                }
            }
            "assistant" | "user" => {
                ensure!(self.initialized, "Claude message before initialization");
                if let Some(content) = v["message"]["content"].as_array() {
                    for item in content {
                        if item["type"] == "tool_use" {
                            let id = item["id"].as_str().context("tool id missing")?;
                            let name = item["name"].as_str().context("tool name missing")?;
                            ensure!(identifier(id) && identifier(name), "invalid tool identity");
                            events.push(Event::ToolObserved {
                                id: id.into(),
                                name: name.into(),
                            });
                        }
                    }
                }
            }
            "rate_limit_event" => (),
            "result" => {
                ensure!(
                    self.initialized
                        && v["session_id"] == self.session
                        && v["subtype"] == "success"
                        && v["is_error"] == false,
                    "Claude terminal failure"
                );
                let structured_output = v
                    .get("structured_output")
                    .filter(|v| v.is_object())
                    .cloned();
                let text = match v.get("result").and_then(Value::as_str) {
                    Some(text) => text.to_owned(),
                    None if structured_output.is_some() => String::new(),
                    None => bail!("Claude result text missing"),
                };
                let denials = match v.get("permission_denials") {
                    Some(Value::Array(a)) => a.len(),
                    None => 0,
                    _ => bail!("invalid permission denials"),
                };
                let usage = v.get("usage").filter(|u| u.is_object()).map(|u| {
                    let mut safe = serde_json::Map::new();
                    for key in [
                        "input_tokens",
                        "output_tokens",
                        "cache_read_input_tokens",
                        "cache_creation_input_tokens",
                    ] {
                        if let Some(n) = u[key].as_u64() {
                            safe.insert(key.into(), json!(n));
                        }
                    }
                    Value::Object(safe)
                });
                self.result = Some(NativeResult {
                    text,
                    structured_output,
                    model: self.expected_model.clone(),
                    usage,
                    permission_denials: denials.max(self.denied),
                });
            }
            _ => bail!("unsupported Claude lifecycle event"),
        }
        Ok(events)
    }
    fn finish(self) -> Result<NativeResult> {
        self.result.context("Claude exited without terminal result")
    }
}
#[cfg(test)]
mod tests;
