//! Opt-in external-extension coordination. Not an ordinary activation capability.
//! No process launch, worker adoption/restart, payload logging, or implicit fallback.
use crate::qualification_shutdown::{
    CLEANUP_CAP, INIT_TIMEOUT, QualificationShutdown, Refusal, ShutdownState, event_deadline,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    future::Future,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::SystemTime,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::Mutex,
    time::{Instant, timeout_at},
};

pub const MANIFEST: &str = "/opt/validator-shutdown/bridge-manifest.json";
pub const MANIFEST_ENV: &str = "LIGHT_LAMBDA_VALIDATOR_BRIDGE_MANIFEST_SHA256";
pub const DIRECTORY: &str = "/tmp/validator-shutdown-bridge";
pub const MAX_FRAME: usize = 4096;
pub const OWNER_BYTES: u64 = 224 * 1024 * 1024;
pub const EXTENSION_BYTES: u64 = 32 * 1024 * 1024;
pub const ENVELOPE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_IMAGE: u64 = 256 * 1024 * 1024;

/// Only the dedicated external executable may call this, before any threads exist.
///
/// # Safety
/// Changes the process mask; the caller must be the isolated single-thread helper.
pub unsafe fn set_external_process_mask() {
    // SAFETY: the caller guarantees dedicated, pre-thread process initialization.
    unsafe {
        libc::umask(0o177);
    }
}

/// Experimental allocation metadata, not a measured/enforced per-process memory bound.
pub fn validate_policy(policy: &crate::validator_ipc::supervision::Policy) -> Result<(), Refusal> {
    policy.validate().map_err(|_| Refusal::Configuration)?;
    if policy.parent_requested_bytes != ENVELOPE_BYTES
        || policy.worker_requested_bytes != 512 * 1024 * 1024
        || policy.headroom_bytes != 256 * 1024 * 1024
        || policy.platform_bytes != 1024 * 1024 * 1024
        || OWNER_BYTES.checked_add(EXTENSION_BYTES) != Some(policy.parent_requested_bytes)
    {
        return Err(Refusal::Configuration);
    }
    Ok(())
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Image {
    pub bytes: u64,
    pub sha256: String,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub protocol_version: u8,
    pub parent: Image,
    pub extension: Image,
    pub socket_directory: String,
    pub owner_requested_bytes: u64,
    pub extension_requested_bytes: u64,
    pub parent_side_envelope_bytes: u64,
}
impl Manifest {
    pub fn validate(&self) -> Result<(), Refusal> {
        if self.protocol_version != 1
            || self.socket_directory != DIRECTORY
            || self.owner_requested_bytes != OWNER_BYTES
            || self.extension_requested_bytes != EXTENSION_BYTES
            || self.parent_side_envelope_bytes != ENVELOPE_BYTES
            || self
                .owner_requested_bytes
                .checked_add(self.extension_requested_bytes)
                != Some(self.parent_side_envelope_bytes)
        {
            return Err(Refusal::Configuration);
        }
        for image in [&self.parent, &self.extension] {
            if image.bytes == 0 || image.bytes > MAX_IMAGE || !digest_string(&image.sha256) {
                return Err(Refusal::Configuration);
            }
        }
        Ok(())
    }
}
fn digest_string(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn expired(deadline: Instant) -> Result<(), Refusal> {
    if Instant::now() >= deadline {
        Err(Refusal::Transport)
    } else {
        Ok(())
    }
}
fn image_hash(path: &Path, expected: &Image, deadline: Instant) -> Result<(), Refusal> {
    expired(deadline)?;
    let mut file = File::open(path).map_err(|_| Refusal::Configuration)?;
    let before = file.metadata().map_err(|_| Refusal::Configuration)?;
    if !before.is_file() || before.len() != expected.bytes {
        return Err(Refusal::Configuration);
    }
    let mut digest = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 65536];
    loop {
        expired(deadline)?;
        let n = file.read(&mut buffer).map_err(|_| Refusal::Transport)?;
        if n == 0 {
            break;
        }
        total = total.checked_add(n as u64).ok_or(Refusal::Protocol)?;
        if total > expected.bytes {
            return Err(Refusal::Configuration);
        }
        digest.update(&buffer[..n]);
    }
    let after = file.metadata().map_err(|_| Refusal::Configuration)?;
    expired(deadline)?;
    if total != expected.bytes
        || before.dev() != after.dev()
        || before.ino() != after.ino()
        || before.len() != after.len()
        || before.mtime_nsec() != after.mtime_nsec()
        || before.mtime() != after.mtime()
        || format!("{:x}", digest.finalize()) != expected.sha256
    {
        return Err(Refusal::Configuration);
    }
    Ok(())
}
pub fn read_manifest(path: &Path, expected: &str, deadline: Instant) -> Result<Manifest, Refusal> {
    if !digest_string(expected) {
        return Err(Refusal::Configuration);
    }
    for parent in path
        .ancestors()
        .skip(1)
        .filter(|p| !p.as_os_str().is_empty())
    {
        let metadata = fs::symlink_metadata(parent).map_err(|_| Refusal::Configuration)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(Refusal::Configuration);
        }
    }
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| Refusal::Configuration)?;
    let metadata = file.metadata().map_err(|_| Refusal::Configuration)?;
    if !metadata.is_file() || metadata.len() > MAX_FRAME as u64 || metadata.mode() & 0o022 != 0 {
        return Err(Refusal::Configuration);
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_FRAME as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| Refusal::Transport)?;
    expired(deadline)?;
    if bytes.len() > MAX_FRAME || format!("{:x}", Sha256::digest(&bytes)) != expected {
        return Err(Refusal::Configuration);
    }
    let manifest: Manifest = serde_json::from_slice(&bytes).map_err(|_| Refusal::Protocol)?;
    manifest.validate()?;
    Ok(manifest)
}
fn configured(deadline: Instant) -> Result<Manifest, Refusal> {
    let digest = std::env::var(MANIFEST_ENV).map_err(|_| Refusal::Configuration)?;
    read_manifest(Path::new(MANIFEST), &digest, deadline)
}

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub pid: u32,
    pub start_ticks: u64,
}
fn start_ticks(pid: u32) -> Result<u64, Refusal> {
    let bytes = fs::read(format!("/proc/{pid}/stat")).map_err(|_| Refusal::Configuration)?;
    if bytes.len() > MAX_FRAME {
        return Err(Refusal::Protocol);
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| Refusal::Protocol)?;
    text.rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().nth(19))
        .and_then(|s| s.parse().ok())
        .ok_or(Refusal::Protocol)
}
fn self_identity(expected: &Image, deadline: Instant) -> Result<Identity, Refusal> {
    let pid = std::process::id();
    let before = start_ticks(pid)?;
    image_hash(Path::new("/proc/self/exe"), expected, deadline)?;
    if before != start_ticks(pid)? {
        return Err(Refusal::Configuration);
    }
    Ok(Identity {
        pid,
        start_ticks: before,
    })
}
fn peer_identity(
    stream: &UnixStream,
    expected: &Image,
    deadline: Instant,
) -> Result<Identity, Refusal> {
    let credentials = stream.peer_cred().map_err(|_| Refusal::Configuration)?;
    // SAFETY: geteuid has no parameters or memory side effects.
    let uid = unsafe { libc::geteuid() };
    let pid = credential_pid(credentials.uid(), credentials.pid(), uid)?;
    let before = start_ticks(pid)?;
    image_hash(Path::new(&format!("/proc/{pid}/exe")), expected, deadline)?;
    if before != start_ticks(pid)? {
        return Err(Refusal::Configuration);
    }
    Ok(Identity {
        pid,
        start_ticks: before,
    })
}
fn credential_pid(uid: u32, pid: Option<i32>, expected_uid: u32) -> Result<u32, Refusal> {
    if uid != expected_uid {
        return Err(Refusal::Configuration);
    }
    pid.and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n > 0)
        .ok_or(Refusal::Configuration)
}

/// Bounded private evidence of observed bridge stages, never a claim of process exit/reap.
struct Observation {
    file: File,
    records: usize,
    actor: &'static str,
    own: Identity,
}
impl Observation {
    fn create(path: &Path, actor: &'static str, own: Identity) -> Result<Self, Refusal> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .map_err(|_| Refusal::Transport)?;
        Ok(Self {
            file,
            records: 0,
            actor,
            own,
        })
    }
    fn record(
        &mut self,
        stage: &'static str,
        peer: Option<Identity>,
        platform: bool,
        deadline: Instant,
    ) -> Result<(), Refusal> {
        use std::io::Write;
        expired(deadline)?;
        if self.records >= 4 {
            return Err(Refusal::Protocol);
        }
        let value = serde_json::json!({"evidenceOrigin":"actualShutdownBridge", "actor":self.actor,
            "stage":stage,"pid":self.own.pid,"startTimeTicks":self.own.start_ticks,
            "peerPid":peer.map(|p|p.pid),"peerStartTimeTicks":peer.map(|p|p.start_ticks),
            "platformShutdownObserved":platform,"processExitProved":false});
        let mut bytes = serde_json::to_vec(&value).map_err(|_| Refusal::Protocol)?;
        if bytes.len() > MAX_FRAME {
            return Err(Refusal::Protocol);
        }
        bytes.push(b'\n');
        self.file
            .write_all(&bytes)
            .map_err(|_| Refusal::Transport)?;
        self.file.flush().map_err(|_| Refusal::Transport)?;
        self.records += 1;
        expired(deadline)
    }
}

/// Requested-memory planning charge retained for the connection lifetime. Not RSS enforcement.
pub struct EnvelopeCharge {
    owner: u64,
    extension: u64,
}
impl EnvelopeCharge {
    pub fn admit(manifest: &Manifest, available: u64) -> Result<Self, Refusal> {
        manifest.validate()?;
        let remaining = available
            .checked_sub(manifest.owner_requested_bytes)
            .and_then(|n| n.checked_sub(manifest.extension_requested_bytes))
            .ok_or(Refusal::Configuration)?;
        if available != manifest.parent_side_envelope_bytes || remaining != 0 {
            return Err(Refusal::Configuration);
        }
        Ok(Self {
            owner: manifest.owner_requested_bytes,
            extension: manifest.extension_requested_bytes,
        })
    }
    pub fn requested(&self) -> (u64, u64) {
        (self.owner, self.extension)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum Frame {
    Hello { version: u8, identity: Identity },
    Ready { version: u8, identity: Identity },
    Shutdown { deadline_ms: u64, reason: String },
    Done { success: bool },
    Ack { success: bool },
}
impl Frame {
    pub fn decode(bytes: &[u8]) -> Result<Self, Refusal> {
        if bytes.is_empty() || bytes.len() > MAX_FRAME {
            return Err(Refusal::Protocol);
        }
        serde_json::from_slice(bytes).map_err(|_| Refusal::Protocol)
    }
}
/// Persistent progress makes cancellation preserve any partially received frame.
#[derive(Default)]
struct Reader {
    header: [u8; 4],
    header_len: usize,
    body: Vec<u8>,
    body_len: usize,
}
impl Reader {
    fn partial(&self) -> bool {
        self.header_len != 0
    }
    async fn read(&mut self, stream: &mut UnixStream) -> Result<Frame, Refusal> {
        while self.header_len < 4 {
            let n = stream
                .read(&mut self.header[self.header_len..])
                .await
                .map_err(|_| Refusal::Transport)?;
            if n == 0 {
                return Err(Refusal::Transport);
            }
            self.header_len += n;
        }
        let length = u32::from_be_bytes(self.header) as usize;
        if length == 0 || length > MAX_FRAME {
            return Err(Refusal::Protocol);
        }
        if self.body.is_empty() {
            self.body.resize(length, 0);
        }
        while self.body_len < length {
            let n = stream
                .read(&mut self.body[self.body_len..])
                .await
                .map_err(|_| Refusal::Transport)?;
            if n == 0 {
                return Err(Refusal::Transport);
            }
            self.body_len += n;
        }
        let frame = Frame::decode(&self.body)?;
        *self = Self::default();
        Ok(frame)
    }
}
async fn send(stream: &mut UnixStream, frame: &Frame) -> Result<(), Refusal> {
    let bytes = serde_json::to_vec(frame).map_err(|_| Refusal::Protocol)?;
    if bytes.is_empty() || bytes.len() > MAX_FRAME {
        return Err(Refusal::Protocol);
    }
    stream
        .write_all(&(bytes.len() as u32).to_be_bytes())
        .await
        .map_err(|_| Refusal::Transport)?;
    stream
        .write_all(&bytes)
        .await
        .map_err(|_| Refusal::Transport)
}
fn reject_queued(stream: &UnixStream, allow_eof: bool) -> Result<(), Refusal> {
    let mut byte = [0_u8; 1];
    match stream.try_read(&mut byte) {
        Ok(0) if allow_eof => Ok(()),
        Ok(0) => Err(Refusal::Transport),
        Ok(_) => Err(Refusal::Protocol),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(()),
        Err(_) => Err(Refusal::Transport),
    }
}
fn notice(deadline_ms: u64, reason: String) -> Result<Instant, Refusal> {
    let bytes = serde_json::to_vec(&serde_json::json!({"eventType":"SHUTDOWN", "deadlineMs":deadline_ms,"shutdownReason":reason})).map_err(|_| Refusal::Protocol)?;
    event_deadline(&bytes, SystemTime::now(), Instant::now())
}

struct Session {
    stream: UnixStream,
    reader: Reader,
    notice: bool,
}
pub struct Parent {
    session: Mutex<Session>,
    state: ShutdownState,
    platform_deadline: std::sync::Mutex<Option<Instant>>,
    _allocation: EnvelopeCharge,
    observation: std::sync::Mutex<Observation>,
    peer: Identity,
}
impl Parent {
    pub async fn connect(manifest: &Manifest, deadline: Instant) -> Result<Arc<Self>, Refusal> {
        manifest.validate()?;
        let allocation = EnvelopeCharge::admit(manifest, ENVELOPE_BYTES)?;
        let own = self_identity(&manifest.parent, deadline)?;
        let path = Path::new(DIRECTORY).join("control.sock");
        let mut stream = loop {
            expired(deadline)?;
            match fs::symlink_metadata(DIRECTORY) {
                Ok(metadata) => {
                    // SAFETY: geteuid reads the current effective UID only.
                    if !metadata.is_dir()
                        || metadata.file_type().is_symlink()
                        || metadata.mode() & 0o777 != 0o700
                        || metadata.uid() != unsafe { libc::geteuid() }
                    {
                        return Err(Refusal::Configuration);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(_) => return Err(Refusal::Configuration),
            }
            if let Ok(metadata) = fs::symlink_metadata(&path) {
                use std::os::unix::fs::FileTypeExt;
                if !metadata.file_type().is_socket() || metadata.mode() & 0o777 != 0o600 {
                    return Err(Refusal::Configuration);
                }
            }
            match UnixStream::connect(&path).await {
                Ok(stream) => break stream,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    tokio::time::sleep_until(
                        deadline.min(Instant::now() + std::time::Duration::from_millis(20)),
                    )
                    .await;
                }
                Err(_) => return Err(Refusal::Transport),
            }
        };
        let peer = peer_identity(&stream, &manifest.extension, deadline)?;
        let mut reader = Reader::default();
        timeout_at(deadline, async {
            send(
                &mut stream,
                &Frame::Hello {
                    version: 1,
                    identity: own,
                },
            )
            .await?;
            match reader.read(&mut stream).await? {
                Frame::Ready {
                    version: 1,
                    identity,
                } if identity == peer => Ok(()),
                _ => Err(Refusal::Protocol),
            }
        })
        .await
        .map_err(|_| Refusal::Transport)??;
        expired(deadline)?;
        let mut observation = Observation::create(
            Path::new("/tmp/validator-shutdown-parent.jsonl"),
            "parent",
            own,
        )?;
        observation.record("ready", Some(peer), false, deadline)?;
        Ok(Arc::new(Self {
            session: Mutex::new(Session {
                stream,
                reader,
                notice: false,
            }),
            state: ShutdownState::default(),
            platform_deadline: std::sync::Mutex::new(None),
            _allocation: allocation,
            observation: std::sync::Mutex::new(observation),
            peer,
        }))
    }
    async fn receive(&self) -> Result<Option<Instant>, Refusal> {
        let mut session = self.session.lock().await;
        let Session {
            stream,
            reader,
            notice: seen,
        } = &mut *session;
        match reader.read(stream).await? {
            Frame::Shutdown {
                deadline_ms,
                reason,
            } if !*seen => {
                *seen = true;
                let end = notice(deadline_ms, reason)?;
                *self
                    .platform_deadline
                    .lock()
                    .map_err(|_| Refusal::Protocol)? = Some(end);
                Ok(Some(end))
            }
            _ => Err(Refusal::Protocol),
        }
    }
    pub async fn wait<S: Future<Output = Result<(), Refusal>>>(&self, signal: S) {
        let result = tokio::select! { biased;
            result=self.receive()=>result,
            result=signal=>result.map(|()| None),
        };
        self.state.record(result);
    }
    pub fn deadline(&self, stopped_at: Instant) -> Instant {
        let cap = self.state.cleanup_deadline(stopped_at);
        match self.platform_deadline.lock() {
            Ok(value) => value.map_or(cap, |end| cap.min(end)),
            Err(_) => stopped_at,
        }
    }
    /// Called after real owner cleanup. No successful DONE after partial/protocol/late failure.
    pub async fn finish(&self, cleaned: bool, stopped_at: Instant) -> Result<(), Refusal> {
        let deadline = self.deadline(stopped_at);
        let mut session = timeout_at(deadline, self.session.lock())
            .await
            .map_err(|_| Refusal::Transport)?;
        let mut success = cleaned
            && self.state.failure().is_none()
            && !session.reader.partial()
            && Instant::now() < self.deadline(stopped_at);
        let Session {
            stream,
            reader,
            notice: seen,
        } = &mut *session;
        timeout_at(
            self.deadline(stopped_at),
            send(stream, &Frame::Done { success }),
        )
        .await
        .map_err(|_| Refusal::Transport)??;
        loop {
            // Recompute the absolute bound after a late SHUTDOWN, never the stop.
            let frame = timeout_at(self.deadline(stopped_at), reader.read(stream))
                .await
                .map_err(|_| Refusal::Transport)??;
            match frame {
                Frame::Shutdown {
                    deadline_ms,
                    reason,
                } if !*seen => {
                    *seen = true;
                    let end = notice(deadline_ms, reason)?;
                    *self
                        .platform_deadline
                        .lock()
                        .map_err(|_| Refusal::Protocol)? = Some(end);
                    if Instant::now() >= end {
                        success = false;
                    }
                }
                Frame::Ack { success: accepted } => {
                    if success && accepted && Instant::now() < self.deadline(stopped_at) {
                        reject_queued(stream, true)?;
                        self.observation
                            .lock()
                            .map_err(|_| Refusal::Protocol)?
                            .record(
                                "ackReceived",
                                Some(self.peer),
                                *seen,
                                self.deadline(stopped_at),
                            )?;
                        expired(self.deadline(stopped_at))?;
                        return Ok(());
                    }
                    return Err(Refusal::Protocol);
                }
                _ => return Err(Refusal::Protocol),
            }
        }
    }
    /// Transfer an already authenticated connection into sole-reader ownership.
    pub fn into_profile_channel(self: Arc<Self>) -> Result<ProfileChannel, Refusal> {
        Arc::try_unwrap(self)
            .map(|parent| ProfileChannel { parent })
            .map_err(|_| Refusal::Configuration)
    }
}

/// Existing qualified experimental bridge, not a production policy selector.
/// No Clone/public constructor; created only from a unique authenticated Parent.
pub struct ProfileChannel {
    parent: Parent,
}
impl ProfileChannel {
    /// Exclusive borrow prevents competing notice and DONE/ACK readers.
    pub async fn notice(&mut self) -> Result<Instant, Refusal> {
        let result = self.parent.receive().await;
        self.parent.state.record(result);
        result?.ok_or(Refusal::Protocol)
    }
    pub async fn finish(&mut self, cleaned: bool, stopped_at: Instant) -> Result<(), Refusal> {
        self.parent.finish(cleaned, stopped_at).await
    }
}

struct SocketOwner {
    directory: PathBuf,
    directory_id: (u64, u64),
    socket_id: Option<(u64, u64)>,
}
impl SocketOwner {
    fn create() -> Result<Self, Refusal> {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .mode(0o700)
            .create(DIRECTORY)
            .map_err(|_| Refusal::Configuration)?;
        let meta = fs::symlink_metadata(DIRECTORY).map_err(|_| Refusal::Configuration)?;
        let owner = Self {
            directory: DIRECTORY.into(),
            directory_id: (meta.dev(), meta.ino()),
            socket_id: None,
        };
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(DIRECTORY)
            .map_err(|_| Refusal::Configuration)?;
        let opened = directory.metadata().map_err(|_| Refusal::Configuration)?;
        if (meta.dev(), meta.ino()) != (opened.dev(), opened.ino()) {
            return Err(Refusal::Configuration);
        }
        directory
            .set_permissions(fs::Permissions::from_mode(0o700))
            .map_err(|_| Refusal::Configuration)?;
        Ok(owner)
    }
    fn cleanup(&mut self) -> Result<(), Refusal> {
        let directory = fs::symlink_metadata(&self.directory).map_err(|_| Refusal::Transport)?;
        if (directory.dev(), directory.ino()) != self.directory_id {
            return Err(Refusal::Configuration);
        }
        let path = self.directory.join("control.sock");
        let socket = fs::symlink_metadata(&path).map_err(|_| Refusal::Transport)?;
        if Some((socket.dev(), socket.ino())) != self.socket_id {
            return Err(Refusal::Configuration);
        }
        fs::remove_file(path).map_err(|_| Refusal::Transport)?;
        fs::remove_dir(&self.directory).map_err(|_| Refusal::Transport)?;
        self.socket_id = None;
        Ok(())
    }
    fn bind(&mut self) -> Result<UnixListener, Refusal> {
        let path = self.directory.join("control.sock");
        let listener = UnixListener::bind(&path).map_err(|_| Refusal::Configuration)?;
        let meta = fs::symlink_metadata(path).map_err(|_| Refusal::Configuration)?;
        self.socket_id = Some((meta.dev(), meta.ino()));
        use std::os::unix::fs::FileTypeExt;
        if !meta.file_type().is_socket() || meta.mode() & 0o777 != 0o600 {
            return Err(Refusal::Configuration);
        }
        Ok(listener)
    }
}
impl Drop for SocketOwner {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.directory)
            .ok()
            .is_some_and(|m| (m.dev(), m.ino()) == self.directory_id)
        {
            let path = self.directory.join("control.sock");
            if fs::symlink_metadata(&path)
                .ok()
                .is_some_and(|m| Some((m.dev(), m.ino())) == self.socket_id)
            {
                let _ = fs::remove_file(path);
            }
            let _ = fs::remove_dir(&self.directory);
        }
    }
}

/// External helper driver. It never launches, signals or waits for a worker.
pub async fn extension(authority: &str) -> Result<(), Refusal> {
    let deadline = Instant::now() + INIT_TIMEOUT;
    let manifest = configured(deadline)?;
    let own = self_identity(&manifest.extension, deadline)?;
    let mut observation = Observation::create(
        Path::new("/tmp/validator-shutdown-extension.jsonl"),
        "extension",
        own,
    )?;
    let mut owner = SocketOwner::create()?;
    let listener = owner.bind()?;
    let api =
        QualificationShutdown::register_external(authority, deadline, ShutdownState::default())
            .await?;
    observation.record("registered", None, false, deadline)?;
    let (mut stream, _) = timeout_at(deadline, listener.accept())
        .await
        .map_err(|_| Refusal::Transport)?
        .map_err(|_| Refusal::Transport)?;
    drop(listener); // No second admission, reconnect, or accepted queue after the sole peer.
    let peer = peer_identity(&stream, &manifest.parent, deadline)?;
    let mut reader = Reader::default();
    timeout_at(deadline, async {
        match reader.read(&mut stream).await? {
            Frame::Hello {
                version: 1,
                identity,
            } if identity == peer => (),
            _ => return Err(Refusal::Protocol),
        }
        send(
            &mut stream,
            &Frame::Ready {
                version: 1,
                identity: own,
            },
        )
        .await
    })
    .await
    .map_err(|_| Refusal::Transport)??;
    expired(deadline)?;
    observation.record("ready", Some(peer), false, deadline)?;
    enum Next {
        Platform(Result<Vec<u8>, Refusal>),
        Parent(Result<Frame, Refusal>),
    }
    let next = tokio::select! { biased;
        event=api.external_event()=>Next::Platform(event),
        frame=reader.read(&mut stream)=>Next::Parent(frame),
    };
    let (done, hard_deadline) = match next {
        Next::Parent(frame) => (frame?, None),
        Next::Platform(event) => {
            let event: serde_json::Value =
                serde_json::from_slice(&event?).map_err(|_| Refusal::Protocol)?;
            let epoch = event
                .get("deadlineMs")
                .and_then(|v| v.as_u64())
                .ok_or(Refusal::Protocol)?;
            let reason = event
                .get("shutdownReason")
                .and_then(|v| v.as_str())
                .ok_or(Refusal::Protocol)?
                .to_owned();
            let end = notice(epoch, reason.clone())?;
            expired(end)?;
            observation.record("shutdownReceived", Some(peer), true, end)?;
            timeout_at(
                end,
                send(
                    &mut stream,
                    &Frame::Shutdown {
                        deadline_ms: epoch,
                        reason,
                    },
                ),
            )
            .await
            .map_err(|_| Refusal::Transport)??;
            (
                timeout_at(end, reader.read(&mut stream))
                    .await
                    .map_err(|_| Refusal::Transport)??,
                Some(end),
            )
        }
    };
    reject_queued(&stream, false)?;
    let success = matches!(done, Frame::Done { success: true })
        && hard_deadline.is_none_or(|end| Instant::now() < end);
    let end = hard_deadline.unwrap_or_else(|| Instant::now() + CLEANUP_CAP);
    acknowledge_after_cleanup(&mut stream, success, end, || {
        owner.cleanup()?;
        observation.record("ackPrepared", Some(peer), hard_deadline.is_some(), end)
    })
    .await
}

async fn acknowledge_after_cleanup<C>(
    stream: &mut UnixStream,
    success: bool,
    deadline: Instant,
    cleanup: C,
) -> Result<(), Refusal>
where
    C: FnOnce() -> Result<(), Refusal>,
{
    expired(deadline)?;
    if success {
        cleanup()?;
    }
    timeout_at(deadline, send(stream, &Frame::Ack { success }))
        .await
        .map_err(|_| Refusal::Transport)??;
    expired(deadline)?;
    if success {
        Ok(())
    } else {
        Err(Refusal::Protocol)
    }
}

/// Same owning runtime/cleanup path; only the notification admission changes.
pub async fn run_external<P, S, C, F>(
    policy: crate::validator_ipc::supervision::Policy,
    signals: Result<S, Refusal>,
    prepare: P,
    guard: Option<crate::service::RequestGuard>,
    cleanup: C,
) -> crate::owned_runtime::Outcome
where
    P: FnOnce() -> Result<crate::init::InitInput, crate::init_error::InitError>,
    S: Future<Output = Result<(), Refusal>>,
    C: FnOnce(crate::owned_runtime::ExitKind, Instant) -> F,
    F: Future<Output = Result<(), crate::validator_ipc::Failure>>,
{
    let deadline = Instant::now() + INIT_TIMEOUT;
    let mut signal = signals.map(Box::pin);
    let ready = match (&mut signal, validate_policy(&policy), configured(deadline)) {
        (Ok(signal), Ok(()), Ok(manifest)) => tokio::select! { biased;
            result=timeout_at(deadline, Parent::connect(&manifest, deadline))=>result.unwrap_or(Err(Refusal::Transport)),
            _=signal.as_mut()=>Err(Refusal::Signal),
        },
        _ => Err(Refusal::Configuration),
    };
    let admission_failure = ShutdownState::default();
    if let Err(error) = &ready {
        admission_failure.record(Err(*error));
    }
    let bridge = ready.ok();
    let admitted = bridge.is_some();
    let waiting = bridge.clone();
    crate::owned_runtime::run_initialized_owned_with_guard(
        move || {
            if !admitted {
                return Err(crate::init_error::InitError::InvalidConfiguration(
                    "validator: qualification startup unavailable",
                ));
            }
            prepare()
        },
        guard,
        async move {
            if let (Some(bridge), Ok(signal)) = (waiting, signal) {
                bridge.wait(signal).await;
            } else {
                std::future::pending::<()>().await;
            }
        },
        move |kind, stopped_at| async move {
            let deadline = bridge
                .as_ref()
                .map_or(stopped_at + CLEANUP_CAP, |b| b.deadline(stopped_at));
            let cleaned = timeout_at(deadline, cleanup(kind, deadline))
                .await
                .map_err(|_| crate::validator_ipc::Failure::Deadline)?;
            let on_time = Instant::now() < deadline;
            if let Some(bridge) = bridge {
                let ack = bridge.finish(cleaned.is_ok() && on_time, stopped_at).await;
                cleaned?;
                if !on_time {
                    return Err(crate::validator_ipc::Failure::Deadline);
                }
                ack.map_err(|_| crate::validator_ipc::Failure::NoReady)?;
            } else {
                cleaned?;
                return Err(crate::validator_ipc::Failure::NoReady);
            }
            Ok(())
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    fn manifest() -> Manifest {
        Manifest {
            protocol_version: 1,
            parent: Image {
                bytes: 4,
                sha256: "a".repeat(64),
            },
            extension: Image {
                bytes: 4,
                sha256: "b".repeat(64),
            },
            socket_directory: DIRECTORY.into(),
            owner_requested_bytes: OWNER_BYTES,
            extension_requested_bytes: EXTENSION_BYTES,
            parent_side_envelope_bytes: ENVELOPE_BYTES,
        }
    }
    fn parent(stream: UnixStream, directory: &Path) -> Arc<Parent> {
        Arc::new(Parent {
            session: Mutex::new(Session {
                stream,
                reader: Reader::default(),
                notice: false,
            }),
            state: ShutdownState::default(),
            platform_deadline: std::sync::Mutex::new(None),
            _allocation: EnvelopeCharge::admit(&manifest(), ENVELOPE_BYTES).unwrap(),
            observation: std::sync::Mutex::new(
                Observation::create(
                    &directory.join("parent.jsonl"),
                    "unit",
                    Identity {
                        pid: 1,
                        start_ticks: 2,
                    },
                )
                .unwrap(),
            ),
            peer: Identity {
                pid: 3,
                start_ticks: 4,
            },
        })
    }
    #[test]
    fn kernel_credential_shape_rejects_wrong_uid_absent_and_nonpositive_pid() {
        assert_eq!(credential_pid(7, Some(3), 7).unwrap(), 3);
        for (uid, pid) in [(8, Some(3)), (7, None), (7, Some(0)), (7, Some(-1))] {
            assert_eq!(
                credential_pid(uid, pid, 7).err(),
                Some(Refusal::Configuration)
            );
        }
    }
    #[test]
    fn image_hash_is_streamed_exact_and_late_admission_never_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image");
        fs::write(&path, b"data").unwrap();
        let good = Image {
            bytes: 4,
            sha256: format!("{:x}", Sha256::digest(b"data")),
        };
        assert!(image_hash(&path, &good, Instant::now() + Duration::from_secs(1)).is_ok());
        assert!(image_hash(&path, &good, Instant::now()).is_err());
        assert!(
            image_hash(
                &path,
                &Image {
                    bytes: 3,
                    sha256: good.sha256.clone()
                },
                Instant::now() + Duration::from_secs(1)
            )
            .is_err()
        );
        assert!(
            image_hash(
                &path,
                &Image {
                    bytes: 4,
                    sha256: "f".repeat(64)
                },
                Instant::now() + Duration::from_secs(1)
            )
            .is_err()
        );
    }
    #[tokio::test]
    async fn actual_in_test_peer_image_mismatch_is_rejected_without_a_process_launch() {
        let (a, _b) = UnixStream::pair().unwrap();
        assert_eq!(
            peer_identity(
                &a,
                &manifest().parent,
                Instant::now() + Duration::from_secs(1)
            )
            .err(),
            Some(Refusal::Configuration)
        );
    }
    #[tokio::test]
    async fn cancellation_keeps_partial_frame_progress_and_resumes_without_replay() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        let mut reader = Reader::default();
        let body = serde_json::to_vec(&Frame::Done { success: false }).unwrap();
        let header = (body.len() as u32).to_be_bytes();
        b.write_all(&header[..2]).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(2), reader.read(&mut a))
                .await
                .is_err()
        );
        assert_eq!(reader.header_len, 2);
        b.write_all(&header[2..]).await.unwrap();
        b.write_all(&body).await.unwrap();
        assert!(matches!(
            reader.read(&mut a).await.unwrap(),
            Frame::Done { success: false }
        ));
        assert!(!reader.partial());
    }
    #[tokio::test]
    async fn oversized_header_allocates_no_body_and_eof_is_failure() {
        let (mut a, mut b) = UnixStream::pair().unwrap();
        let mut reader = Reader::default();
        b.write_all(&((MAX_FRAME + 1) as u32).to_be_bytes())
            .await
            .unwrap();
        assert_eq!(reader.read(&mut a).await.err(), Some(Refusal::Protocol));
        assert!(reader.body.is_empty());
        let (mut a, b) = UnixStream::pair().unwrap();
        drop(b);
        assert_eq!(
            Reader::default().read(&mut a).await.err(),
            Some(Refusal::Transport)
        );
    }
    #[tokio::test]
    async fn role_order_and_duplicate_shutdown_refuse() {
        let dir = tempfile::tempdir().unwrap();
        let (a, mut b) = UnixStream::pair().unwrap();
        let parent = parent(a, dir.path());
        send(
            &mut b,
            &Frame::Ready {
                version: 1,
                identity: Identity {
                    pid: 3,
                    start_ticks: 4,
                },
            },
        )
        .await
        .unwrap();
        assert_eq!(parent.receive().await.err(), Some(Refusal::Protocol));
        let epoch = (SystemTime::now() + Duration::from_secs(2))
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        send(
            &mut b,
            &Frame::Shutdown {
                deadline_ms: epoch,
                reason: "SPINDOWN".into(),
            },
        )
        .await
        .unwrap();
        assert!(parent.receive().await.is_ok());
        send(
            &mut b,
            &Frame::Shutdown {
                deadline_ms: epoch,
                reason: "SPINDOWN".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(parent.receive().await.err(), Some(Refusal::Protocol));
    }
    #[tokio::test]
    async fn successful_cleanup_still_waits_for_actual_ack() {
        let dir = tempfile::tempdir().unwrap();
        let (a, mut b) = UnixStream::pair().unwrap();
        let p = parent(a, dir.path());
        let (sent, received) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let peer = tokio::spawn(async move {
            let frame = Reader::default().read(&mut b).await.unwrap();
            assert!(matches!(frame, Frame::Done { success: true }));
            sent.send(()).unwrap();
            released.await.unwrap();
            send(&mut b, &Frame::Ack { success: true }).await.unwrap();
        });
        let finishing = tokio::spawn(async move { p.finish(true, Instant::now()).await });
        received.await.unwrap();
        assert!(!finishing.is_finished());
        release.send(()).unwrap();
        assert!(finishing.await.unwrap().is_ok());
        peer.await.unwrap();
    }
    #[tokio::test]
    async fn failed_cleanup_never_sends_success_done() {
        let dir = tempfile::tempdir().unwrap();
        let (a, mut b) = UnixStream::pair().unwrap();
        let p = parent(a, dir.path());
        let peer = tokio::spawn(async move {
            assert!(matches!(
                Reader::default().read(&mut b).await.unwrap(),
                Frame::Done { success: false }
            ));
            send(&mut b, &Frame::Ack { success: false }).await.unwrap();
        });
        assert!(p.finish(false, Instant::now()).await.is_err());
        peer.await.unwrap();
    }
    #[tokio::test]
    async fn expired_and_partial_cleanup_cannot_be_acknowledged_successfully() {
        let dir = tempfile::tempdir().unwrap();
        let (a, _b) = UnixStream::pair().unwrap();
        let p = parent(a, dir.path());
        assert!(
            p.finish(true, Instant::now() - Duration::from_secs(1))
                .await
                .is_err()
        );
        let dir2 = tempfile::tempdir().unwrap();
        let (a, mut b) = UnixStream::pair().unwrap();
        let p = parent(a, dir2.path());
        p.session.lock().await.reader.header_len = 1;
        let peer = tokio::spawn(async move {
            assert!(matches!(
                Reader::default().read(&mut b).await.unwrap(),
                Frame::Done { success: false }
            ));
            send(&mut b, &Frame::Ack { success: true }).await.unwrap();
        });
        assert!(p.finish(true, Instant::now()).await.is_err());
        peer.await.unwrap();
    }
    #[test]
    fn first_failure_and_deadline_cap_remain_existing_policy() {
        let state = ShutdownState::default();
        state.record(Err(Refusal::Transport));
        state.record(Err(Refusal::Protocol));
        assert_eq!(state.failure(), Some(Refusal::Transport));
        let stop = Instant::now();
        assert_eq!(state.cleanup_deadline(stop), stop + CLEANUP_CAP);
    }
    #[test]
    fn observations_are_bounded_private_and_never_claim_exit_or_include_span_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("observation");
        let span = tracing::info_span!("hostile", token = "SYNTHETIC_SURROUNDING_SECRET");
        let _entered = span.enter();
        let mut observation = Observation::create(
            &path,
            "unit",
            Identity {
                pid: 1,
                start_ticks: 2,
            },
        )
        .unwrap();
        for _ in 0..4 {
            observation
                .record(
                    "ready",
                    None,
                    false,
                    Instant::now() + Duration::from_secs(1),
                )
                .unwrap();
        }
        assert!(
            observation
                .record(
                    "ready",
                    None,
                    false,
                    Instant::now() + Duration::from_secs(1)
                )
                .is_err()
        );
        let text = fs::read_to_string(path).unwrap();
        assert!(!text.contains("SYNTHETIC_SURROUNDING_SECRET"));
        assert!(!text.contains("token"));
        assert_eq!(text.lines().count(), 4);
        for line in text.lines() {
            let value: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(value["processExitProved"], false);
        }
    }
    #[tokio::test]
    async fn helper_cleanup_must_precede_successful_ack_and_failure_writes_no_ack() {
        use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
        let cleaned = Arc::new(AtomicBool::new(false));
        let marker = cleaned.clone();
        let (mut helper, mut parent) = UnixStream::pair().unwrap();
        let peer = tokio::spawn(async move {
            let frame = Reader::default().read(&mut parent).await.unwrap();
            assert!(marker.load(SeqCst));
            assert!(matches!(frame, Frame::Ack { success: true }));
        });
        let marker = cleaned.clone();
        assert!(
            acknowledge_after_cleanup(&mut helper, true, Instant::now() + CLEANUP_CAP, move || {
                marker.store(true, SeqCst);
                Ok(())
            })
            .await
            .is_ok()
        );
        peer.await.unwrap();
        let (mut helper, parent) = UnixStream::pair().unwrap();
        assert_eq!(
            acknowledge_after_cleanup(&mut helper, true, Instant::now() + CLEANUP_CAP, || Err(
                Refusal::Transport
            ))
            .await
            .err(),
            Some(Refusal::Transport)
        );
        assert!(
            matches!(parent.try_read(&mut [0_u8;1]),Err(error) if error.kind()==std::io::ErrorKind::WouldBlock)
        );
    }
    #[tokio::test]
    async fn queued_duplicate_frame_is_not_accepted_as_success() {
        let (a, mut b) = UnixStream::pair().unwrap();
        send(&mut b, &Frame::Done { success: true }).await.unwrap();
        a.readable().await.unwrap();
        assert_eq!(reject_queued(&a, false).err(), Some(Refusal::Protocol));
    }
    #[tokio::test]
    async fn actual_in_test_peer_and_self_image_are_admitted_without_spawning_a_process() {
        let mut file = File::open("/proc/self/exe").unwrap();
        let bytes = file.metadata().unwrap().len();
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 65536];
        loop {
            let n = file.read(&mut buffer).unwrap();
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
        }
        let image = Image {
            bytes,
            sha256: format!("{:x}", hasher.finalize()),
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        let (a, _b) = UnixStream::pair().unwrap();
        let peer = peer_identity(&a, &image, deadline).unwrap();
        let own = self_identity(&image, deadline).unwrap();
        assert!(peer == own);
        assert_eq!(peer.pid, std::process::id());
        assert!(peer.start_ticks > 0);
    }
    #[tokio::test]
    async fn cleanup_removes_owned_inodes_but_not_a_replaced_socket() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("owned");
        fs::create_dir(&directory).unwrap();
        let meta = fs::symlink_metadata(&directory).unwrap();
        let path = directory.join("control.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let socket = fs::symlink_metadata(&path).unwrap();
        let mut owner = SocketOwner {
            directory: directory.clone(),
            directory_id: (meta.dev(), meta.ino()),
            socket_id: Some((socket.dev(), socket.ino())),
        };
        owner.cleanup().unwrap();
        assert!(!directory.exists());
        drop(listener);
        let directory = root.path().join("substituted");
        fs::create_dir(&directory).unwrap();
        let meta = fs::symlink_metadata(&directory).unwrap();
        let path = directory.join("control.sock");
        let old = UnixListener::bind(&path).unwrap();
        let socket = fs::symlink_metadata(&path).unwrap();
        let mut owner = SocketOwner {
            directory: directory.clone(),
            directory_id: (meta.dev(), meta.ino()),
            socket_id: Some((socket.dev(), socket.ino())),
        };
        fs::rename(&path, directory.join("saved.sock")).unwrap();
        let replacement = UnixListener::bind(&path).unwrap();
        assert_eq!(owner.cleanup().err(), Some(Refusal::Configuration));
        drop(owner);
        assert!(path.exists());
        assert!(directory.join("saved.sock").exists());
        drop(old);
        drop(replacement);
    }
    #[tokio::test]
    async fn channel_requires_unique_parent_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let (a, _b) = UnixStream::pair().unwrap();
        let p = parent(a, dir.path());
        let competing = p.clone();
        assert_eq!(p.into_profile_channel().err(), Some(Refusal::Configuration));
        assert!(competing.into_profile_channel().is_ok());
    }
    #[tokio::test]
    async fn channel_notice_refusal_prevents_successful_done() {
        let dir = tempfile::tempdir().unwrap();
        let (a, mut b) = UnixStream::pair().unwrap();
        let mut channel = parent(a, dir.path()).into_profile_channel().unwrap();
        send(&mut b, &Frame::Done { success: true }).await.unwrap();
        assert_eq!(channel.notice().await.err(), Some(Refusal::Protocol));
        let peer = tokio::spawn(async move {
            assert!(matches!(
                Reader::default().read(&mut b).await.unwrap(),
                Frame::Done { success: false }
            ));
            send(&mut b, &Frame::Ack { success: true }).await.unwrap();
        });
        assert_eq!(
            channel.finish(true, Instant::now()).await.err(),
            Some(Refusal::Protocol)
        );
        peer.await.unwrap();
    }
    #[tokio::test(start_paused = true)]
    async fn late_expired_shutdown_shortens_pending_ack_wait() {
        let dir = tempfile::tempdir().unwrap();
        let (a, mut b) = UnixStream::pair().unwrap();
        // Prime actual socket readiness before a virtual timer exists. Otherwise
        // Tokio may advance the original cap while OS readiness is still pending,
        // letting finish return before the test's observation spin can see a frame.
        send(
            &mut b,
            &Frame::Shutdown {
                deadline_ms: 0,
                reason: "synthetic".into(),
            },
        )
        .await
        .unwrap();
        a.readable().await.unwrap();
        a.writable().await.unwrap();
        let p = parent(a, dir.path());
        // finish writes DONE before consuming the queued SHUTDOWN. No ACK is
        // provided. The same 1ms bound distinguishes the expired event deadline
        // from the unchanged 350ms initial cap, without peer tasks or spin loops.
        let result =
            tokio::time::timeout(Duration::from_millis(1), p.finish(true, Instant::now())).await;
        assert_eq!(result.unwrap().err(), Some(Refusal::Transport));
        assert!(p.platform_deadline.lock().unwrap().is_some());
        assert!(matches!(
            Reader::default().read(&mut b).await.unwrap(),
            Frame::Done { success: true }
        ));
    }
    #[tokio::test(start_paused = true)]
    async fn expired_deadline_also_bounds_session_lock_acquisition() {
        let dir = tempfile::tempdir().unwrap();
        let (a, _b) = UnixStream::pair().unwrap();
        let p = parent(a, dir.path());
        let _held = p.session.lock().await;
        *p.platform_deadline.lock().unwrap() = Some(Instant::now());
        let result =
            tokio::time::timeout(Duration::from_millis(1), p.finish(true, Instant::now())).await;
        assert_eq!(result.unwrap().err(), Some(Refusal::Transport));
    }
    #[tokio::test]
    async fn exclusive_channel_delivers_notice_then_done_ack_without_second_reader() {
        let dir = tempfile::tempdir().unwrap();
        let (a, mut b) = UnixStream::pair().unwrap();
        let mut channel = parent(a, dir.path()).into_profile_channel().unwrap();
        let epoch: u64 = (SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
            + 5000)
            .try_into()
            .unwrap();
        let peer = tokio::spawn(async move {
            send(
                &mut b,
                &Frame::Shutdown {
                    deadline_ms: epoch,
                    reason: "synthetic".into(),
                },
            )
            .await
            .unwrap();
            assert!(matches!(
                Reader::default().read(&mut b).await.unwrap(),
                Frame::Done { success: true }
            ));
            send(&mut b, &Frame::Ack { success: true }).await.unwrap();
        });
        let deadline = channel.notice().await.unwrap();
        assert!(deadline > Instant::now());
        assert!(channel.finish(true, Instant::now()).await.is_ok());
        peer.await.unwrap();
    }
}
