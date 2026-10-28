//! Opt-in Router child integration. No app registration or admitted resource
//! profile. Typed resource/loss outcomes deliberately have no HTTP mapping.
pub mod recovery;
use serde::{
    Deserialize, Serialize,
    de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor},
};
use serverless_core::{Invocation, StatusError};
use sha2::{Digest, Sha256};
use std::{
    fmt,
    io::{Read, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    process::Command,
    sync::{Mutex, OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Serialize)]
pub struct Rule {
    pub pattern: String,
    pub replacement: String,
}

/// Every path is trusted application configuration, never request data. Pins
/// are supplied by the reviewed application artifact manifest, not discovered
/// from PATH. Deployment immutability of verified files is a separate gate.
pub struct Artifacts {
    pub java: PathBuf,
    pub classpath: Vec<PathBuf>,
    pub pins: Vec<ArtifactPin>,
}
pub struct ArtifactPin {
    pub path: PathBuf,
    pub sha256: [u8; 32],
    pub bytes: u64,
}
impl Artifacts {
    fn verify(&self) -> Result<(), Failure> {
        if !cfg!(all(target_os = "linux", target_arch = "x86_64"))
            || !self.java.is_absolute()
            || self.classpath.len() != 4
            || self.classpath.iter().filter(|p| p.is_dir()).count() != 1
        {
            return Err(Failure::Artifact);
        }
        if !self.pins.iter().any(|pin| pin.path == self.java) {
            return Err(Failure::Artifact);
        }
        let home = self
            .java
            .parent()
            .and_then(std::path::Path::parent)
            .ok_or(Failure::Artifact)?;
        for relative in ["release", "lib/modules", "lib/server/libjvm.so"] {
            if !self.pins.iter().any(|pin| pin.path == home.join(relative)) {
                return Err(Failure::Artifact);
            }
        }
        for name in [
            "jackson-core-2.22.3.jar",
            "jackson-databind-2.22.3.jar",
            "jackson-annotations-2.22.jar",
        ] {
            if !self
                .classpath
                .iter()
                .any(|p| p.file_name().is_some_and(|f| f == name))
            {
                return Err(Failure::Artifact);
            }
        }
        for path in &self.classpath {
            if path.is_dir()
                && !self
                    .pins
                    .iter()
                    .any(|pin| pin.path == path.join("OwnedRewriteWorker$Rule.class"))
            {
                return Err(Failure::Artifact);
            }
            let actual = if path.is_dir() {
                path.join("OwnedRewriteWorker.class")
            } else {
                path.clone()
            };
            if !actual.is_absolute() || !self.pins.iter().any(|pin| pin.path == actual) {
                return Err(Failure::Artifact);
            }
        }
        for pin in &self.pins {
            let mut file = std::fs::File::open(&pin.path).map_err(|_| Failure::Artifact)?;
            let metadata = file.metadata().map_err(|_| Failure::Artifact)?;
            if !metadata.is_file() || metadata.len() != pin.bytes {
                return Err(Failure::Artifact);
            }
            let mut file =
                Read::by_ref(&mut file).take(pin.bytes.checked_add(1).ok_or(Failure::Artifact)?);
            let mut hash = Sha256::new();
            let mut buffer = [0; 16384];
            loop {
                let n = file.read(&mut buffer).map_err(|_| Failure::Artifact)?;
                if n == 0 {
                    break;
                }
                hash.update(&buffer[..n]);
            }
            if <[u8; 32]>::from(hash.finalize()) != pin.sha256 {
                return Err(Failure::Artifact);
            }
        }
        Ok(())
    }
}

/// All values are explicit local qualification inputs, not production defaults.
#[derive(Clone)]
pub struct QualificationLimits {
    pub frame_bytes: usize,
    pub input_utf16: usize,
    pub output_utf16: usize,
    pub heap_mib: u32,
    pub stack_kib: u32,
    pub address_space_bytes: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    Artifact,
    Init,
    Deadline,
    Cancelled,
    InvalidUtf16,
    InvalidReplacement,
    Protocol,
    Panic,
    Resource,
    InputProfile,
    OutputProfile,
    ChildLost,
    Unclassified,
    NoReady,
}
impl Failure {
    /// No fallback mapping for resource, child loss, input/output profile or
    /// unavailable states. Their production outcomes remain owner-gated.
    pub fn approved_status(self) -> Option<StatusError> {
        match self {
            Self::Deadline | Self::Cancelled => Some(StatusError::timeout()),
            Self::InvalidUtf16 | Self::InvalidReplacement | Self::Protocol | Self::Panic => {
                Some(StatusError::new(
                    500,
                    "ERR14004",
                    "MIDDLEWARE_UNHANDLED_EXCEPTION",
                    "Middleware thread unhandled runtime exception.",
                ))
            }
            _ => None,
        }
    }
}
pub struct Candidate {
    original: String,
    rewritten: String,
}
impl Candidate {
    pub(crate) fn path(&self) -> &str {
        &self.rewritten
    }

    pub fn commit(self, invocation: &mut Invocation) -> Result<(), Failure> {
        check(invocation.deadline, &invocation.cancel)?;
        if invocation.path != self.original {
            return Err(Failure::Protocol);
        }
        invocation.path = self.rewritten;
        Ok(())
    }
}
fn check(deadline: Instant, cancel: &CancellationToken) -> Result<(), Failure> {
    if cancel.is_cancelled() {
        Err(Failure::Cancelled)
    } else if Instant::now() >= deadline {
        Err(Failure::Deadline)
    } else {
        Ok(())
    }
}
fn program_hash(rules: &[Rule]) -> Result<String, Failure> {
    let mut hash = Sha256::new();
    hash.update(b"WP12/rewrite-program/v1\0");
    hash.update(
        u32::try_from(rules.len())
            .map_err(|_| Failure::InputProfile)?
            .to_be_bytes(),
    );
    for rule in rules {
        for text in [&rule.pattern, &rule.replacement] {
            let n = text.encode_utf16().count();
            hash.update(
                u32::try_from(n)
                    .map_err(|_| Failure::InputProfile)?
                    .to_be_bytes(),
            );
            for unit in text.encode_utf16() {
                hash.update(unit.to_be_bytes());
            }
        }
    }
    Ok(format!("{:x}", hash.finalize()))
}
fn encode(value: &impl Serialize, bound: usize) -> Result<Vec<u8>, Failure> {
    struct Count {
        n: usize,
        bound: usize,
    }
    impl Write for Count {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.n = self
                .n
                .checked_add(b.len())
                .ok_or_else(|| std::io::Error::other("frame"))?;
            if self.n > self.bound {
                return Err(std::io::Error::other("frame"));
            }
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count { n: 0, bound };
    serde_json::to_writer(&mut count, value).map_err(|_| Failure::InputProfile)?;
    let mut bytes = Vec::with_capacity(count.n);
    serde_json::to_writer(&mut bytes, value).map_err(|_| Failure::Protocol)?;
    Ok(bytes)
}
async fn write_frame(writer: &mut (impl AsyncWrite + Unpin), bytes: &[u8]) -> Result<(), Failure> {
    let count = u32::try_from(bytes.len()).map_err(|_| Failure::InputProfile)?;
    writer
        .write_all(&count.to_be_bytes())
        .await
        .map_err(|_| Failure::ChildLost)?;
    writer
        .write_all(bytes)
        .await
        .map_err(|_| Failure::ChildLost)?;
    writer.flush().await.map_err(|_| Failure::ChildLost)
}
async fn read_frame(
    reader: &mut (impl AsyncRead + Unpin),
    bound: usize,
) -> Result<Vec<u8>, Failure> {
    let n = reader.read_u32().await.map_err(|_| Failure::ChildLost)? as usize;
    if n > bound {
        return Err(Failure::OutputProfile);
    }
    let mut bytes = vec![0; n];
    reader
        .read_exact(&mut bytes)
        .await
        .map_err(|_| Failure::ChildLost)?;
    Ok(bytes)
}

// Units are checked before every push. The encoded frame and serde scalar
// scratch buffer have a separate frame bound; no unbounded Value tree exists.
struct UnitsSeed<'a>(usize, &'a std::cell::Cell<bool>);
impl<'de> DeserializeSeed<'de> for UnitsSeed<'_> {
    type Value = Vec<u16>;
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<Vec<u16>, D::Error> {
        struct Units<'a>(usize, &'a std::cell::Cell<bool>);
        impl<'de> Visitor<'de> for Units<'_> {
            type Value = Vec<u16>;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("bounded UTF16 units")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Self::Value, A::Error> {
                let mut v = Vec::with_capacity(self.0);
                while let Some(unit) = a.next_element::<u16>()? {
                    if v.len() == self.0 {
                        self.1.set(true);
                        return Err(de::Error::custom("units"));
                    }
                    v.push(unit)
                }
                Ok(v)
            }
        }
        d.deserialize_seq(Units(self.0, self.1))
    }
}
struct Reply {
    id: u64,
    epoch: u64,
    units: Option<Vec<u16>>,
    valid: Option<bool>,
    error: Option<ErrorTag>,
}
#[derive(Deserialize)]
enum ErrorTag {
    IllegalArgumentException,
    IndexOutOfBoundsException,
    IllegalStateException,
    StackOverflowError,
    OutOfMemoryError,
    AssertionError,
    #[serde(other)]
    Unknown,
}
struct ReplySeed<'a>(usize, &'a std::cell::Cell<bool>);
impl<'de> DeserializeSeed<'de> for ReplySeed<'_> {
    type Value = Reply;
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<Reply, D::Error> {
        struct Fields<'a>(usize, &'a std::cell::Cell<bool>);
        impl<'de> Visitor<'de> for Fields<'_> {
            type Value = Reply;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("rewrite reply")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Reply, A::Error> {
                let (mut id, mut epoch, mut units, mut valid, mut error) =
                    (None, None, None, None, None);
                while let Some(key) = a.next_key::<&str>()? {
                    match key {
                        "id" if id.is_none() => id = Some(a.next_value()?),
                        "epoch" if epoch.is_none() => epoch = Some(a.next_value()?),
                        "units" if units.is_none() => {
                            units = Some(a.next_value_seed(UnitsSeed(self.0, self.1))?)
                        }
                        "validUtf16" if valid.is_none() => valid = Some(a.next_value()?),
                        "error" if error.is_none() => error = Some(a.next_value()?),
                        _ => return Err(de::Error::custom("reply fields")),
                    }
                }
                Ok(Reply {
                    id: id.ok_or_else(|| de::Error::custom("id"))?,
                    epoch: epoch.ok_or_else(|| de::Error::custom("epoch"))?,
                    units,
                    valid,
                    error,
                })
            }
        }
        d.deserialize_map(Fields(self.0, self.1))
    }
}
fn decode(bytes: &[u8], output: usize, id: u64, epoch: u64) -> Result<String, Failure> {
    let mut decoder = serde_json::Deserializer::from_slice(bytes);
    let overflow = std::cell::Cell::new(false);
    let reply = ReplySeed(output, &overflow)
        .deserialize(&mut decoder)
        .map_err(|_| {
            if overflow.get() {
                Failure::OutputProfile
            } else {
                Failure::Protocol
            }
        })?;
    decoder.end().map_err(|_| Failure::Protocol)?;
    if reply.id != id || reply.epoch != epoch {
        return Err(Failure::Protocol);
    }
    if let Some(error) = reply.error {
        if reply.units.is_some() || reply.valid.is_some() {
            return Err(Failure::Protocol);
        }
        return Err(match error {
            ErrorTag::IllegalArgumentException | ErrorTag::IndexOutOfBoundsException => {
                Failure::InvalidReplacement
            }
            ErrorTag::IllegalStateException => Failure::OutputProfile,
            ErrorTag::StackOverflowError | ErrorTag::OutOfMemoryError => Failure::Resource,
            ErrorTag::AssertionError => Failure::Panic,
            ErrorTag::Unknown => Failure::Unclassified,
        });
    }
    let units = reply.units.ok_or(Failure::Protocol)?;
    let text = String::from_utf16(&units).map_err(|_| Failure::InvalidUtf16)?;
    if reply.valid != Some(true) {
        return Err(Failure::Protocol);
    }
    Ok(text)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Ready<'a> {
    ready: bool,
    rules: usize,
    #[serde(rename = "compileNanos")]
    _compile_nanos: u64,
    #[serde(rename = "environmentEntries")]
    environment: usize,
    #[serde(borrow, rename = "captureSha256")]
    capture: &'a str,
    #[serde(borrow, rename = "programSha256")]
    program: &'a str,
    epoch: u64,
}
struct Request {
    bytes: Vec<u8>,
    id: u64,
    deadline: Instant,
    cancel: CancellationToken,
    reply: oneshot::Sender<Result<String, Failure>>,
}
/// One actor owns each Child, stdin/stdout and process permit through wait/reap.
/// Dropping an outstanding result receiver kills the child; late results cannot
/// return to another invocation. No blocking task or synchronous request I/O.
pub struct Session {
    commands: mpsc::Sender<Request>,
    serial: AtomicU64,
    epoch: u64,
    limits: QualificationLimits,
    lease: Mutex<()>,
    stopped: CancellationToken,
    program: Arc<[Rule]>,
    #[cfg(test)]
    pid: Arc<std::sync::atomic::AtomicU32>,
}
pub struct Preparation<'a> {
    pub artifacts: &'a Artifacts,
    pub limits: QualificationLimits,
    pub capacity: Arc<Semaphore>,
    pub epoch: u64,
}
impl Session {
    pub async fn prepare(
        preparation: Preparation<'_>,
        rules: Arc<[Rule]>,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<Self, Failure> {
        check(deadline, &cancel)?;
        let Preparation {
            artifacts,
            limits,
            capacity,
            epoch,
        } = preparation;
        if limits.frame_bytes == 0
            || limits.frame_bytes > u32::MAX as usize
            || limits.output_utf16 == 0
            || limits.heap_mib == 0
            || limits.stack_kib == 0
            || limits.address_space_bytes == 0
        {
            return Err(Failure::Init);
        }
        artifacts.verify()?;
        check(deadline, &cancel)?;
        let program = program_hash(&rules)?;
        #[derive(Serialize)]
        struct Init<'a> {
            rules: &'a [Rule],
            epoch: u64,
        }
        let encoded = encode(
            &Init {
                rules: &rules,
                epoch,
            },
            limits.frame_bytes,
        )?;
        let capture = format!("{:x}", Sha256::digest(&encoded));
        let permit = tokio::select! {biased;_=cancel.cancelled()=>return Err(Failure::Cancelled),_=tokio::time::sleep_until(deadline)=>return Err(Failure::Deadline),p=capacity.acquire_owned()=>p.map_err(|_|Failure::NoReady)?};
        let mut command = Command::new(&artifacts.java);
        command.env_clear().args([
            format!("-Xms{}m", limits.heap_mib.min(16)),
            format!("-Xmx{}m", limits.heap_mib),
            format!("-Xss{}k", limits.stack_kib),
            "-XX:ActiveProcessorCount=1".into(),
            "-XX:ReservedCodeCacheSize=32m".into(),
            "-XX:CompressedClassSpaceSize=16m".into(),
            "-XX:MaxMetaspaceSize=128m".into(),
        ]);
        #[cfg(test)]
        command.arg("-Dwp12.qualification=true");
        command
            .arg("-cp")
            .arg(std::env::join_paths(&artifacts.classpath).map_err(|_| Failure::Artifact)?)
            .arg("OwnedRewriteWorker");
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        install_controls(&mut command, limits.address_space_bytes)?;
        let (commands, receiver) = mpsc::channel(1);
        let (ready_tx, ready_rx) = oneshot::channel();
        let stopped = CancellationToken::new();
        let stopped_actor = stopped.clone();
        let actor_limits = limits.clone();
        let rule_count = rules.len();
        #[cfg(test)]
        let pid = Arc::new(std::sync::atomic::AtomicU32::new(0));
        #[cfg(test)]
        let actor_pid = pid.clone();
        tokio::spawn(async move {
            actor(
                command,
                permit,
                receiver,
                ready_tx,
                ActorInit {
                    encoded,
                    capture,
                    program,
                    rules: rule_count,
                    epoch,
                    deadline,
                    cancel,
                    #[cfg(test)]
                    pid: actor_pid,
                },
                actor_limits,
            )
            .await;
            stopped_actor.cancel();
        });
        ready_rx.await.map_err(|_| Failure::ChildLost)??;
        Ok(Self {
            commands,
            serial: AtomicU64::new(1),
            epoch,
            limits,
            lease: Mutex::new(()),
            stopped,
            program: rules,
            #[cfg(test)]
            pid,
        })
    }
    pub async fn rewrite(
        &self,
        path: &str,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<Candidate, Failure> {
        check(deadline, &cancel)?;
        if path.encode_utf16().count() > self.limits.input_utf16 {
            return Err(Failure::InputProfile);
        }
        let _lease = tokio::select! {biased;_=cancel.cancelled()=>return Err(Failure::Cancelled),_=tokio::time::sleep_until(deadline)=>return Err(Failure::Deadline),l=self.lease.lock()=>l};
        check(deadline, &cancel)?;
        let id = self
            .serial
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| Failure::Protocol)?;
        #[derive(Serialize)]
        struct Call<'a> {
            id: u64,
            epoch: u64,
            path: &'a str,
            compare: bool,
        }
        let bytes = encode(
            &Call {
                id,
                epoch: self.epoch,
                path,
                compare: false,
            },
            self.limits.frame_bytes,
        )?;
        let (reply, rx) = oneshot::channel();
        let request = Request {
            bytes,
            id,
            deadline,
            cancel: cancel.clone(),
            reply,
        };
        self.commands
            .send(request)
            .await
            .map_err(|_| Failure::NoReady)?;
        let result = rx.await.map_err(|_| Failure::ChildLost)?;
        check(deadline, &cancel)?;
        Ok(Candidate {
            original: path.to_owned(),
            rewritten: result?,
        })
    }
    pub fn program(&self) -> &Arc<[Rule]> {
        &self.program
    }
    pub async fn shutdown(self) {
        let stopped = self.stopped.clone();
        drop(self);
        stopped.cancelled().await;
    }
}
struct ActorInit {
    encoded: Vec<u8>,
    capture: String,
    program: String,
    rules: usize,
    epoch: u64,
    deadline: Instant,
    cancel: CancellationToken,
    #[cfg(test)]
    pid: Arc<std::sync::atomic::AtomicU32>,
}
async fn actor(
    mut command: Command,
    _permit: OwnedSemaphorePermit,
    mut requests: mpsc::Receiver<Request>,
    mut ready: oneshot::Sender<Result<(), Failure>>,
    init: ActorInit,
    limits: QualificationLimits,
) {
    let Ok(mut child) = command.spawn() else {
        let _ = ready.send(Err(Failure::ChildLost));
        return;
    };
    #[cfg(test)]
    init.pid.store(child.id().unwrap(), Ordering::SeqCst);
    let Some(mut input) = child.stdin.take() else {
        let _ = child.kill().await;
        return;
    };
    let Some(mut output) = child.stdout.take() else {
        let _ = child.kill().await;
        return;
    };
    let startup = async {
        write_frame(&mut input, &init.encoded).await?;
        let bytes = read_frame(&mut output, limits.frame_bytes).await?;
        let value: Ready<'_> = serde_json::from_slice(&bytes).map_err(|_| Failure::Init)?;
        if !value.ready
            || value.rules != init.rules
            || value.environment != 0
            || value.epoch != init.epoch
            || value.capture != init.capture
            || value.program != init.program
        {
            return Err(Failure::Init);
        }
        Ok(())
    };
    let outcome = tokio::select! {biased;_=init.cancel.cancelled()=>Err(Failure::Cancelled),_=tokio::time::sleep_until(init.deadline)=>Err(Failure::Deadline),_=ready.closed()=>Err(Failure::Cancelled),r=startup=>r};
    if outcome.is_err() {
        let _ = child.kill().await;
        let _ = child.wait().await;
        let _ = ready.send(outcome);
        return;
    }
    if ready.send(Ok(())).is_err() {
        let _ = child.kill().await;
        let _ = child.wait().await;
        return;
    }
    while let Some(mut request) = requests.recv().await {
        let work = async {
            write_frame(&mut input, &request.bytes).await?;
            let bytes = read_frame(&mut output, limits.frame_bytes).await?;
            decode(&bytes, limits.output_utf16, request.id, init.epoch)
        };
        let mut result = tokio::select! {biased;_=request.cancel.cancelled()=>Err(Failure::Cancelled),_=tokio::time::sleep_until(request.deadline)=>Err(Failure::Deadline),_=request.reply.closed()=>Err(Failure::Cancelled),r=work=>r};
        if let Err(error) = check(request.deadline, &request.cancel) {
            result = Err(error)
        }
        // A typed selected replacement failure leaves the immutable worker
        // usable. All other failures dispose it before the capacity is returned.
        let dispose = result
            .as_ref()
            .err()
            .is_some_and(|e| *e != Failure::InvalidReplacement && *e != Failure::InvalidUtf16);
        if dispose {
            let _ = child.kill().await;
            let _ = child.wait().await;
            let _ = request.reply.send(result);
            return;
        }
        let _ = request.reply.send(result);
    }
    let _ = child.kill().await;
    let _ = child.wait().await;
}
fn install_controls(command: &mut Command, address_space: u64) -> Result<(), Failure> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        #[repr(C)]
        struct Limit {
            soft: u64,
            hard: u64,
        }
        unsafe extern "C" {
            fn setrlimit(resource: i32, limit: *const Limit) -> i32;
        }
        // No allocation, logging, retrieval or locking inside the fork hook.
        unsafe {
            command.pre_exec(move || {
                for (resource, value) in [(4, 0), (7, 64), (9, address_space)] {
                    if setrlimit(
                        resource,
                        &Limit {
                            soft: value,
                            hard: value,
                        },
                    ) != 0
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        Ok(())
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    {
        let _ = (command, address_space);
        Err(Failure::Artifact)
    }
}

#[cfg(test)]
mod tests;
