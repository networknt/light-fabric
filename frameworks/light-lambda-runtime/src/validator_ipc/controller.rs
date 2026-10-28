//! Preserved v6 IPC controller, staged only; no application admission or HTTP mapping.
//! Paused experiment tests are deliberately outside this module.
#[path = "budget.rs"]
pub(crate) mod budget;
use crate::validator_init_diagnostic::{InitDiagnostic, Stage};
pub use budget::{Budget, MetadataReservation, Reservation, Retained};
use budget::{Credit, Response};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst},
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
    time::{Duration, Instant},
};
const FRAME: usize = 4 << 20;
const ROOTS: usize = 65536;
#[derive(Debug, PartialEq, Eq)]
pub enum Failure {
    Frame,
    ParentBudget,
    RequestedQuota,
    AllocatorRefused,
    EngineResource,
    EngineInternal,
    AdapterInvariant,
    Protocol,
    Deadline,
    Cancelled,
    Death,
    NoReady,
    Rate,
    Stale,
}
#[derive(Default)]
pub struct Stats {
    _ownership: Option<Arc<Credit>>,
    pub(super) lifecycle: Mutex<Option<Arc<super::lifecycle::Lifecycle>>>,
    // Managed controllers own at most two legacy processes or one supervised.
    // A retained Process forms an intentional ownership cycle on uncertain wait.
    quarantine: Mutex<[Option<ProcessResources>; 2]>,
    pub last_child_pid: AtomicUsize,
    pub live: AtomicUsize,
    pub reaped: AtomicUsize,
    pub peak: AtomicUsize,
    pub request_frames: AtomicUsize,
    pub max_frame: AtomicUsize,
}
#[derive(Default)]
struct Cancellation {
    flag: AtomicBool,
    notify: Notify,
}
struct CancelOnDrop(Arc<Cancellation>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.flag.store(true, SeqCst);
        self.0.notify.notify_one();
    }
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Ready {
    ready: bool,
    request_schema_count: usize,
    metadata_json: String,
    source_sha256: String,
    raw_bytes: usize,
    resource_bytes: usize,
    resource_nodes: usize,
    catalogue_bytes: usize,
    engine_cache_entries: usize,
    compiler_reservation_bytes: usize,
    aggregate_compiler_input_bytes: usize,
    #[serde(deserialize_with = "roots")]
    pub roots: Vec<String>,
    compile_micros: u64,
}
fn roots<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    struct Visitor;
    impl<'de> serde::de::Visitor<'de> for Visitor {
        type Value = Vec<String>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("bounded string roots")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut a: A,
        ) -> Result<Self::Value, A::Error> {
            let mut roots = Vec::new();
            let mut bytes = 0usize;
            loop {
                if roots.len() == ROOTS {
                    if a.next_element::<serde::de::IgnoredAny>()?.is_some() {
                        return Err(serde::de::Error::custom("root count"));
                    }
                    break;
                }
                let Some(root) = a.next_element::<String>()? else {
                    break;
                };
                bytes = bytes
                    .checked_add(root.len())
                    .ok_or_else(|| serde::de::Error::custom("root bytes"))?;
                if bytes > FRAME {
                    return Err(serde::de::Error::custom("root bytes"));
                }
                roots.push(root);
            }
            Ok(roots)
        }
    }
    d.deserialize_seq(Visitor)
}
impl Response for Ready {
    fn parse_reservation(n: usize) -> Result<usize, Failure> {
        budget::parse_reservation::<Self>(n, ROOTS)
    }
}
impl Ready {
    pub fn metadata_json(&self) -> &str {
        &self.metadata_json
    }
    fn matches(&self, source: &str) -> bool {
        self.ready
            && self.raw_bytes == source.len()
            && self.source_sha256 == format!("{:x}", Sha256::digest(source.as_bytes()))
            && self.metadata_json.len() <= FRAME
            && self.resource_bytes <= FRAME
            && self.resource_nodes <= 65536
            && self.catalogue_bytes <= 16 << 20
            && self.compiler_reservation_bytes <= 16 << 20
            && self.aggregate_compiler_input_bytes <= 16 << 20
            && self.request_schema_count <= self.roots.len()
            && self.engine_cache_entries >= self.roots.len()
            && self.compile_micros <= 15_000_000
    }
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Violation {
    pub code: String,
    pub message: String,
    pub description: String,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    pub status: u16,
    pub body: String,
    pub violation: Option<Violation>,
}
impl Response for Reply {
    fn parse_reservation(n: usize) -> Result<usize, Failure> {
        budget::parse_reservation::<Self>(n, 0)
    }
    fn parse(bytes: &[u8]) -> Result<Self, Failure> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        enum Fault {
            EngineResource,
            EngineInternal,
            AdapterInvariant,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            status: Option<u16>,
            body: Option<String>,
            violation: Option<Violation>,
            failure: Option<Fault>,
        }
        let wire: Wire = serde_json::from_slice(bytes).map_err(|_| Failure::Protocol)?;
        if let Some(fault) = wire.failure {
            if wire.status.is_some() || wire.body.is_some() || wire.violation.is_some() {
                return Err(Failure::Protocol);
            }
            return Err(match fault {
                Fault::EngineResource => Failure::EngineResource,
                Fault::EngineInternal => Failure::EngineInternal,
                Fault::AdapterInvariant => Failure::AdapterInvariant,
            });
        }
        Ok(Self {
            status: wire.status.ok_or(Failure::Protocol)?,
            body: wire.body.ok_or(Failure::Protocol)?,
            violation: wire.violation,
        })
    }
}
impl Reply {
    pub fn decode(bytes: &[u8]) -> Result<Self, Failure> {
        let reply = <Self as Response>::parse(bytes)?;
        if reply.valid() {
            Ok(reply)
        } else {
            Err(Failure::Protocol)
        }
    }
    fn valid(&self) -> bool {
        if self.status == 200 {
            return self.body == "ok" && self.violation.is_none();
        }
        if self.status != 400 {
            return false;
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields, rename_all = "camelCase")]
        struct Envelope {
            status_code: u16,
            code: String,
            message: String,
            description: String,
            severity: String,
        }
        let (Some(v), Ok(body)) = (
            &self.violation,
            serde_json::from_str::<Envelope>(&self.body),
        ) else {
            return false;
        };
        body.status_code == 400
            && body.severity == "ERROR"
            && body.code == v.code
            && body.message == v.message
            && body.description == v.description
    }
}
struct Counter(usize);
impl std::io::Write for Counter {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_add(b.len())
            .ok_or_else(|| std::io::Error::other("frame"))?;
        if self.0 > FRAME {
            return Err(std::io::Error::other("frame"));
        }
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn encode(value: &impl Serialize, budget: &Arc<Budget>) -> Result<Retained<Vec<u8>>, Failure> {
    let mut count = Counter(0);
    serde_json::to_writer(&mut count, value).map_err(|_| Failure::Frame)?;
    let credit = budget.reserve(count.0)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(count.0)
        .map_err(|_| Failure::Frame)?;
    serde_json::to_writer(&mut bytes, value).map_err(|_| Failure::Frame)?;
    Ok(Retained {
        value: bytes,
        _credit: credit,
    })
}
async fn shutdown_signal(drain: Option<&Arc<super::ownership::Drain>>) {
    if let Some(drain) = drain {
        drain.closed().await;
    } else {
        std::future::pending::<()>().await;
    }
}
struct Supervised {
    authority: Arc<super::supervision::Authority>,
    owner: Mutex<Option<super::supervision::Owner>>,
    drain: Arc<super::ownership::Drain>,
    _credit: Credit,
}
/// Non-Clone admission capability. Only internal Work retention duplicates its Arc.
struct LeasedRequest {
    request: super::supervision::Request,
    _credit: Credit,
}
pub struct Admission {
    request: Option<Arc<LeasedRequest>>,
}
/// A hold cannot authorize another request.
pub struct AdmissionHold {
    _request: Option<Arc<LeasedRequest>>,
}
impl Admission {
    pub fn legacy() -> Self {
        Self { request: None }
    }
    pub fn is_legacy(&self) -> bool {
        self.request.is_none()
    }
    pub fn hold(&self) -> AdmissionHold {
        AdmissionHold {
            _request: self.request.clone(),
        }
    }
    fn reserve(
        authority: Option<&Arc<super::supervision::Authority>>,
        budget: &Arc<Budget>,
    ) -> Result<Self, Failure> {
        let request = if let Some(authority) = authority {
            // Acquire capacity before reserving/allocating the lease. Refusal
            // drops this stack token and cannot retain uncounted heap work.
            let request = authority.reserve_request().map_err(|_| Failure::NoReady)?;
            let credit = budget
                .reserve(std::mem::size_of::<LeasedRequest>() + 2 * std::mem::size_of::<usize>())?;
            Some(Arc::new(LeasedRequest {
                request,
                _credit: credit,
            }))
        } else {
            None
        };
        Ok(Self { request })
    }
    fn matches(&self, authority: Option<&Arc<super::supervision::Authority>>) -> bool {
        match (&self.request, authority) {
            (None, None) => true,
            (Some(request), Some(authority)) => request.request.belongs_to(authority),
            _ => false,
        }
    }
}
fn finalize_wait<T>(
    resources: T,
    stats: &Stats,
    owner: Option<super::supervision::Owner>,
    waited: &std::io::Result<std::process::ExitStatus>,
) -> Result<(), T> {
    if let Some(owner) = owner {
        let _ = owner.waited(waited);
    }
    if waited.is_err() {
        return Err(resources);
    }
    stats.live.fetch_sub(1, SeqCst);
    stats.reaped.fetch_add(1, SeqCst);
    drop(resources);
    Ok(())
}
fn retain_uncertain<T>(quarantine: &Mutex<[Option<T>; 2]>, resources: T) {
    let mut held = quarantine.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(slot) = held.iter_mut().find(|slot| slot.is_none()) {
        *slot = Some(resources);
    } else {
        // Defensive retention for public legacy callers supplying capacity >2.
        // Managed/supervised limits cannot reach this branch. Never refund an
        // uncertain Child. This is not a bound for arbitrary legacy start calls.
        std::mem::forget(resources);
    }
}
type Process = super::owned_guard::Owner<ProcessResources>;
struct ProcessResources {
    slots: Arc<Semaphore>,
    supervised_owner: Option<super::supervision::Owner>,
    _owner_observation: Option<super::lifecycle::OwnerObservation>,
    child: Child,
    input: Option<ChildStdin>,
    output: Option<ChildStdout>,
    _drain_work: Option<super::ownership::Work>,
    _permit: Option<OwnedSemaphorePermit>,
    stats: Arc<Stats>,
    memory: Memory,
    _control_credit: Arc<Credit>,
    resource_signal: std::os::unix::net::UnixStream,
    observation: Option<super::lifecycle::ChildObservation>,
}
impl super::owned_guard::Abandon for ProcessResources {
    fn abandon(mut self) {
        self.slots.close();
        // Consume the live token now; keeping it in quarantine without dropping
        // would leave its authority Ready. The real resources remain retained.
        drop(self.supervised_owner.take());
        if let Some(supervised) = &self.memory.supervised {
            supervised.drain.uncertain();
        }
        let killed = self.child.start_kill();
        if let Some(observation) = &self.observation {
            observation.abandoned(&killed);
        }
        let stats = self.stats.clone();
        retain_uncertain(&stats.quarantine, self);
    }
}
impl Process {
    async fn exchange<T: Response>(
        &mut self,
        value: &impl Serialize,
    ) -> Result<Retained<T>, Failure> {
        let bytes = encode(value, &self.memory.budget)?;
        self.stats.max_frame.fetch_max(bytes.len(), SeqCst);
        self.input
            .as_mut()
            .ok_or(Failure::AdapterInvariant)?
            .write_all(&(bytes.len() as u32).to_be_bytes())
            .await
            .map_err(|_| Failure::Death)?;
        self.input
            .as_mut()
            .ok_or(Failure::AdapterInvariant)?
            .write_all(&bytes)
            .await
            .map_err(|_| Failure::Death)?;
        self.input
            .as_mut()
            .ok_or(Failure::AdapterInvariant)?
            .flush()
            .await
            .map_err(|_| Failure::Death)?;
        drop(bytes);
        let n = self
            .output
            .as_mut()
            .ok_or(Failure::AdapterInvariant)?
            .read_u32()
            .await
            .map_err(|_| Failure::Death)? as usize;
        if n > FRAME {
            return Err(Failure::Frame);
        }
        let credit = self.memory.budget.reserve(T::parse_reservation(n)?)?;
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(n).map_err(|_| Failure::Frame)?;
        bytes.resize(n, 0);
        self.output
            .as_mut()
            .ok_or(Failure::AdapterInvariant)?
            .read_exact(&mut bytes)
            .await
            .map_err(|_| Failure::Death)?;
        self.stats.max_frame.fetch_max(n, SeqCst);
        let value = T::parse(&bytes)?;
        drop(bytes);
        if let Some(failure) = self.resource_fault() {
            return Err(failure);
        }
        Ok(Retained {
            value,
            _credit: credit,
        })
    }
    fn classify_loss(&mut self, failure: Failure) -> Failure {
        if failure != Failure::Death {
            return failure;
        }
        self.resource_fault().unwrap_or(failure)
    }
    fn resource_fault(&mut self) -> Option<Failure> {
        let mut marker = [0u8; 1];
        match std::io::Read::read(&mut self.resource_signal, &mut marker) {
            Ok(1) => match marker[0] {
                b'Q' => Some(Failure::RequestedQuota),
                b'A' => Some(Failure::AllocatorRefused),
                _ => Some(Failure::Protocol),
            },
            _ => None,
        }
    }
    async fn stop(mut self, trigger: super::lifecycle::ShutdownTrigger) {
        if let Some(observation) = &self.observation {
            observation.shutdown(trigger);
        }
        let killed = self.child.start_kill();
        if let Some(observation) = &self.observation {
            observation.kill_attempted(&killed);
        }
        let waited = self.child.wait().await;
        if let Some(observation) = &self.observation {
            observation.waited(&waited);
        }
        let mut resources = self.into_inner();
        let stats = resources.stats.clone();
        let owner = resources.supervised_owner.take();
        if let Err(resources) = finalize_wait(resources, &stats, owner, &waited) {
            super::owned_guard::Abandon::abandon(resources);
        }
    }
}
struct Work {
    event: serde_json::Value,
    _event_credit: Arc<Credit>,
    _admission: AdmissionHold,
    deadline: Instant,
    cancel: Arc<Cancellation>,
    reply: oneshot::Sender<Result<Retained<Reply>, Failure>>,
}
#[derive(Clone)]
pub struct Actor {
    drain: Option<Arc<super::ownership::Drain>>,
    authority: Option<Arc<super::supervision::Authority>>,
    _control: Arc<Credit>,
    tx: mpsc::Sender<Work>,
    pub ready: Arc<Retained<Ready>>,
    budget: Arc<Budget>,
}
impl Actor {
    pub async fn request(
        &self,
        event: serde_json::Value,
        deadline: Instant,
    ) -> Result<Retained<Reply>, Failure> {
        if Instant::now() >= deadline {
            return Err(Failure::Deadline);
        }
        if self.drain.as_ref().is_some_and(|d| d.is_closed()) {
            return Err(Failure::NoReady);
        }
        let admission = Admission::reserve(self.authority.as_ref(), &self.budget)?;
        let credit = budget::retain_event(&self.budget, &event).map_err(|e| {
            if Instant::now() >= deadline {
                Failure::Deadline
            } else {
                e
            }
        })?;
        self.request_retained(event, credit, deadline, admission)
            .await
    }
    async fn request_retained(
        &self,
        event: serde_json::Value,
        credit: Credit,
        deadline: Instant,
        admission: Admission,
    ) -> Result<Retained<Reply>, Failure> {
        if self.drain.as_ref().is_some_and(|d| d.is_closed()) {
            return Err(Failure::NoReady);
        }
        if !admission.matches(self.authority.as_ref()) {
            return Err(Failure::AdapterInvariant);
        }
        // Shared by the caller and queued/processing work. Closing the actor or
        // dropping a caller cannot refund a live channel/event allocation.
        let credit = Arc::new(credit);
        let cancel = Arc::new(Cancellation::default());
        let guard = CancelOnDrop(cancel.clone());
        let permit = tokio::time::timeout_at(deadline, self.tx.reserve())
            .await
            .map_err(|_| Failure::Deadline)?
            .map_err(|_| Failure::NoReady)?;
        let (tx, rx) = oneshot::channel();
        permit.send(Work {
            event,
            _event_credit: credit.clone(),
            _admission: admission.hold(),
            deadline,
            cancel,
            reply: tx,
        });
        let result = tokio::time::timeout_at(deadline, rx)
            .await
            .map_err(|_| Failure::Deadline)?
            .map_err(|_| Failure::Death)?;
        if Instant::now() >= deadline {
            return Err(Failure::Deadline);
        }
        drop(guard);
        result
    }
}
/// Immutable captured source format; recovery reuses it with the original bytes.
#[derive(Clone, Copy)]
pub enum SourceFormat {
    Json,
    Yaml,
}
impl SourceFormat {
    fn wire(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Yaml => "yaml",
        }
    }
}
#[derive(Clone)]
struct Memory {
    supervised: Option<Arc<Supervised>>,
    executable: Option<Arc<super::sealed_executable::Image>>,
    owner: Option<Arc<Credit>>,
    settings: Arc<super::Settings>,
    budget: Arc<Budget>,
    source_credit: Option<Arc<Credit>>,
    allocator_limit: usize,
    address_space_limit: u64,
    cpu_seconds: u64,
}
impl Memory {
    fn new(source: &str, budget: Arc<Budget>) -> Self {
        let source_credit = budget.reserve(source.len()).ok().map(Arc::new);
        Self {
            supervised: None,
            executable: None,
            owner: None,
            settings: Arc::default(),
            budget,
            source_credit,
            allocator_limit: 256 << 20,
            address_space_limit: 512 << 20,
            cpu_seconds: 8,
        }
    }
}
/// The sealed path never calls the new protection. The closure is a private
/// test seam, not an executable/profile or configuration injection point.
pub(super) fn readonly_pre_exec(
    required: bool,
    apply: impl FnOnce() -> std::io::Result<()>,
) -> std::io::Result<()> {
    if required {
        apply()?;
    }
    Ok(())
}
pub async fn start(
    path: Arc<PathBuf>,
    source: Arc<str>,
    slots: Arc<Semaphore>,
    stats: Arc<Stats>,
    deadline: Instant,
) -> Result<Actor, Failure> {
    start_owned(
        path,
        source.clone(),
        slots,
        stats,
        deadline,
        None,
        (
            SourceFormat::Json,
            Memory::new(&source, Budget::new(256 << 20)),
        ),
    )
    .await
}
async fn start_owned(
    path: Arc<PathBuf>,
    source: Arc<str>,
    slots: Arc<Semaphore>,
    stats: Arc<Stats>,
    deadline: Instant,
    rate: Option<Arc<Mutex<Rate>>>,
    controls: (SourceFormat, Memory),
) -> Result<Actor, Failure> {
    let (format, memory) = controls;
    if source.len() > 1 << 20 {
        return Err(Failure::Frame);
    }
    if memory.source_credit.is_none() && memory.owner.is_none() {
        return Err(Failure::ParentBudget);
    }
    // Process/IPC channel bookkeeping plus a disclosed pinned-Tokio allowance.
    // Actor clones retain this after process exit until the channel is gone.
    let control_credit = Arc::new(
        memory
            .budget
            .reserve(16384 + std::mem::size_of::<Process>() + std::mem::size_of::<Actor>())?,
    );
    let cancel = Arc::new(Cancellation::default());
    let guard = CancelOnDrop(cancel.clone());
    let (ready_tx, ready_rx) = oneshot::channel();
    let task_budget = memory.budget.clone();
    let owner_observation = stats
        .lifecycle
        .lock()
        .unwrap()
        .as_ref()
        .map(|lifecycle| lifecycle.owner_started());
    let drain = memory.supervised.as_ref().map(|s| s.drain.clone());
    let owner_work = drain.as_ref().map(|d| d.owner()).transpose()?;
    let task = async move {
        let mut owner_observation = owner_observation;
        let mut owner_work = owner_work;
        let permit = tokio::select! {biased;_=tokio::time::sleep_until(deadline)=>return,_=cancel.notify.notified()=>return,_=shutdown_signal(drain.as_ref())=>return,p=slots.clone().acquire_owned()=>match p{Ok(p)=>p,Err(_)=>return}};
        if cancel.flag.load(SeqCst) {
            return;
        }
        // Admission is charged after capacity wait, immediately before the
        // synchronous spawn attempt. It is never refunded on cancel/failure.
        let attempt = if let Some(rate) = &rate {
            if !rate.lock().unwrap().begin(Instant::now()) {
                let _ = ready_tx.send(Err(Failure::Rate));
                return;
            }
            Some(RateAttempt(rate.clone()))
        } else {
            None
        };
        let (resource_signal, child_signal) = match std::os::unix::net::UnixStream::pair() {
            Ok(pair) => pair,
            Err(_) => {
                let _ = ready_tx.send(Err(Failure::Death));
                return;
            }
        };
        if resource_signal.set_nonblocking(true).is_err() {
            let _ = ready_tx.send(Err(Failure::Death));
            return;
        }
        let signal_fd = std::os::fd::AsRawFd::as_raw_fd(&child_signal);
        // A pinned profile owns the sealed image through Memory in this task
        // and Process, including cancellation and actual owner wait/reap.
        let mut command = if let Some(image) = &memory.executable {
            match image.command() {
                Ok(command) => command,
                Err(failure) => {
                    let _ = ready_tx.send(Err(failure));
                    return;
                }
            }
        } else {
            Command::new(path.as_ref())
        };
        let readonly = memory
            .executable
            .as_ref()
            .is_some_and(|image| image.requires_no_new_privileges());
        // Owned Linux qualification values only; no Lambda support inferred.
        unsafe {
            command.pre_exec(move || {
                readonly_pre_exec(readonly, || {
                    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
                    if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
                    return Err(std::io::Error::from(std::io::ErrorKind::Unsupported));
                    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
                    Ok(())
                })?;
                if libc::fcntl(signal_fd, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                for (resource, limit) in [
                    (libc::RLIMIT_AS, memory.address_space_limit),
                    (libc::RLIMIT_NOFILE, 64),
                    (libc::RLIMIT_CORE, 0),
                    (libc::RLIMIT_CPU, memory.cpu_seconds),
                ] {
                    let value = libc::rlimit {
                        rlim_cur: limit,
                        rlim_max: limit,
                    };
                    if libc::setrlimit(resource, &value) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        // A spawn attempt consumes the single fail-stop generation. A failed
        // spawn drops the started token, closes admission and never invents a reap.
        let supervised_owner = if let Some(supervised) = &memory.supervised {
            let owner = supervised.owner.lock().ok().and_then(|mut o| o.take());
            let Some(mut owner) = owner else {
                if let Some(image) = &memory.executable {
                    image.note_no_ready(Stage::ControllerOwnerMissing);
                }
                let _ = ready_tx.send(Err(Failure::NoReady));
                return;
            };
            if let Err(refusal) = owner.started() {
                if let Some(image) = &memory.executable {
                    image.note_refusal(Stage::ControllerOwnerStart, refusal);
                }
                let _ = ready_tx.send(Err(Failure::NoReady));
                return;
            }
            Some(owner)
        } else {
            None
        };
        command
            .env_clear()
            .env(
                "WP12_QUALIFICATION_ALLOCATOR_LIMIT",
                memory.allocator_limit.to_string(),
            )
            .env("WP12_RESOURCE_SIGNAL_FD", signal_fd.to_string())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        if cancel.flag.load(SeqCst) {
            let _ = ready_tx.send(Err(Failure::Cancelled));
            return;
        }
        let spawned = if let Some(drain) = &drain {
            // Serialize spawn admission with completed close. This synchronous
            // spawn can still block: no hard IO/wall-clock bound is claimed.
            drain
                .publish(deadline, || command.spawn())
                .and_then(|r| r.map_err(|_| Failure::Death))
        } else {
            command.spawn().map_err(|_| Failure::Death)
        };
        let child = match spawned {
            Ok(child) => child,
            Err(failure) => {
                let _ = ready_tx.send(Err(failure));
                return;
            }
        };
        // Establish the resource-bearing guard immediately on obtaining Child,
        // before rate/lifecycle callbacks or extracting optional pipe handles.
        let mut process = Process::new(ProcessResources {
            slots,
            supervised_owner,
            _owner_observation: owner_observation.take(),
            child,
            input: None,
            output: None,
            _drain_work: owner_work.take(),
            _permit: Some(permit),
            stats: stats.clone(),
            memory: memory.clone(),
            resource_signal,
            _control_credit: control_credit.clone(),
            observation: None,
        });
        let live = stats.live.fetch_add(1, SeqCst) + 1;
        stats.peak.fetch_max(live, SeqCst);
        drop(attempt);
        drop(child_signal);
        let pid = process.child.id().unwrap();
        stats.last_child_pid.store(pid as usize, SeqCst);
        process.observation = stats
            .lifecycle
            .lock()
            .unwrap()
            .as_ref()
            .map(|l| l.spawned(pid));
        let input = process.child.stdin.take();
        process.input = input;
        let output = process.child.stdout.take();
        process.output = output;
        #[derive(Serialize)]
        struct Init<'a> {
            source: &'a str,
            format: &'static str,
            settings: &'a super::Settings,
            protocol: &'static str,
        }
        let init = Init {
            source: source.as_ref(),
            format: format.wire(),
            settings: &memory.settings,
            protocol: "wp12-owned-request-v2",
        };
        let result: Result<Retained<Ready>, Failure> = tokio::select! {biased;_=tokio::time::sleep_until(deadline)=>Err(Failure::Deadline),_=cancel.notify.notified()=>Err(Failure::Cancelled),_=shutdown_signal(drain.as_ref())=>Err(Failure::Cancelled),r=process.exchange(&init)=>r};
        let ready = match result {
            Ok(r) if r.matches(&source) => Arc::new(r),
            Ok(_) => {
                let _ = ready_tx.send(Err(Failure::Protocol));
                process
                    .stop(super::lifecycle::ShutdownTrigger::InitRejected)
                    .await;
                return;
            }
            Err(e) => {
                let e = process.classify_loss(e);
                let _ = ready_tx.send(Err(e));
                process
                    .stop(super::lifecycle::ShutdownTrigger::InitFailed)
                    .await;
                return;
            }
        };
        if let Some(owner) = &mut process.supervised_owner
            && let Err(refusal) = owner.ready()
        {
            if let Some(image) = &memory.executable {
                image.note_refusal(Stage::ControllerOwnerReady, refusal);
            }
            let _ = ready_tx.send(Err(Failure::NoReady));
            process
                .stop(super::lifecycle::ShutdownTrigger::InitRejected)
                .await;
            return;
        }
        let (tx, mut rx) = mpsc::channel::<Work>(1);
        if ready_tx
            .send(Ok(Actor {
                drain: drain.clone(),
                authority: memory.supervised.as_ref().map(|s| s.authority.clone()),
                _control: control_credit.clone(),
                tx,
                ready,
                budget: memory.budget.clone(),
            }))
            .is_err()
        {
            process
                .stop(super::lifecycle::ShutdownTrigger::ReadinessReceiverDropped)
                .await;
            return;
        }
        // Readiness ownership has transferred. Its cancellation guard may now
        // fire normally; each request has its own cancellation signal.
        let mut shutdown_trigger = super::lifecycle::ShutdownTrigger::ActorSendersDropped;
        loop {
            let work = tokio::select! {biased;
                _=shutdown_signal(drain.as_ref())=>{
                    shutdown_trigger=super::lifecycle::ShutdownTrigger::ExplicitShutdown;break;
                },
                work=rx.recv()=>match work {Some(work)=>work,None=>break},
            };
            if work.cancel.flag.load(SeqCst) {
                let _ = work.reply.send(Err(Failure::Cancelled));
                continue;
            }
            if Instant::now() >= work.deadline {
                let _ = work.reply.send(Err(Failure::Deadline));
                continue;
            }
            process.stats.request_frames.fetch_add(1, SeqCst);
            #[derive(Serialize)]
            struct Request<'a> {
                event: &'a serde_json::Value,
            }
            let request = Request { event: &work.event };
            let mut result: Result<Retained<Reply>, Failure> = tokio::select! {biased;_=tokio::time::sleep_until(work.deadline)=>Err(Failure::Deadline),_=work.cancel.notify.notified()=>Err(Failure::Cancelled),_=shutdown_signal(drain.as_ref())=>Err(Failure::Cancelled),r=process.exchange(&request)=>r};
            result = result.map_err(|failure| process.classify_loss(failure));
            if result.as_ref().is_ok_and(|reply| !reply.valid()) {
                result = Err(Failure::Protocol)
            }
            if Instant::now() >= work.deadline {
                result = Err(Failure::Deadline)
            } else if work.cancel.flag.load(SeqCst) {
                result = Err(Failure::Cancelled)
            }
            let fatal = result.is_err();
            let _ = work.reply.send(result);
            if fatal {
                shutdown_trigger = if drain.as_ref().is_some_and(|d| d.is_closed()) {
                    super::lifecycle::ShutdownTrigger::ExplicitShutdown
                } else {
                    super::lifecycle::ShutdownTrigger::RequestFailed
                };
                break;
            }
        }
        process.stop(shutdown_trigger).await;
    };
    // Charge the actual compiled future before tokio allocates its task box.
    let task_credit = task_budget.reserve(
        std::mem::size_of_val(&task)
            .checked_add(4096)
            .ok_or(Failure::ParentBudget)?,
    )?;
    tokio::spawn(async move {
        let _task_credit = task_credit;
        task.await;
    });
    let result = tokio::time::timeout_at(deadline, ready_rx)
        .await
        .map_err(|_| Failure::Deadline)?
        .map_err(|_| Failure::Death)?;
    if Instant::now() >= deadline {
        return Err(Failure::Deadline);
    }
    drop(guard);
    result
}
pub struct Rate {
    starts: VecDeque<Instant>,
    last: Instant,
    pending: usize,
}
struct RateAttempt(Arc<Mutex<Rate>>);
impl Drop for RateAttempt {
    fn drop(&mut self) {
        self.0.lock().unwrap().finish(Instant::now());
    }
}
impl Rate {
    pub fn new(now: Instant) -> Self {
        Self {
            starts: VecDeque::new(),
            last: now,
            pending: 0,
        }
    }
    pub fn reserve(&mut self, now: Instant) -> bool {
        let now = now.max(self.last);
        self.last = now;
        while self
            .starts
            .front()
            .is_some_and(|t| *t + Duration::from_secs(30) <= now)
        {
            self.starts.pop_front();
        }
        if self.starts.len() + self.pending >= 2 {
            return false;
        }
        self.starts.push_back(now);
        true
    }
    fn begin(&mut self, now: Instant) -> bool {
        // Reuse monotonic pruning, then convert this admission to a pending
        // charge. An OS/scheduler delay cannot make that charge expire.
        if !self.reserve(now) {
            return false;
        }
        self.starts.pop_back();
        self.pending += 1;
        true
    }
    fn finish(&mut self, now: Instant) {
        let now = now.max(self.last);
        self.last = now;
        self.pending -= 1;
        self.starts.push_back(now);
    }
}
pub struct Controller {
    source: Arc<str>,
    format: SourceFormat,
    path: Arc<PathBuf>,
    slots: Arc<Semaphore>,
    compiler: tokio::sync::Mutex<()>,
    admitted_init: std::sync::atomic::AtomicBool,
    ready: tokio::sync::RwLock<Option<Actor>>,
    epoch: AtomicUsize,
    rate: Arc<Mutex<Rate>>,
    pub stats: Arc<Stats>,
    memory: Memory,
}
// Existing controls plus the optional trusted, per-Init finite recorder.
pub(super) type OwnedPreparationControls = (
    Arc<super::supervision::Authority>,
    Arc<super::ownership::Drain>,
    Option<Arc<crate::init::PreparationContext>>,
    Option<Arc<InitDiagnostic>>,
);
impl Controller {
    /// Fallible application constructor. Reserve ownership before source/path
    /// copies, Arcs, synchronization primitives or the fixed recovery deque.
    pub fn prepare(
        path: &std::path::Path,
        source: &[u8],
        format: SourceFormat,
        settings: Arc<super::Settings>,
        profile: ResourceProfile,
    ) -> Result<Self, Failure> {
        profile.validate()?;
        if source.len() > 1 << 20 {
            return Err(Failure::Frame);
        }
        Self::prepare_with_budget(
            path,
            source,
            format,
            settings,
            profile,
            Budget::new(profile.parent_bytes),
        )
    }
    fn prepare_with_budget(
        path: &std::path::Path,
        source: &[u8],
        format: SourceFormat,
        settings: Arc<super::Settings>,
        profile: ResourceProfile,
        budget: Arc<Budget>,
    ) -> Result<Self, Failure> {
        profile.validate()?;
        if source.len() > 1 << 20 {
            return Err(Failure::Frame);
        }
        let fixed = std::mem::size_of::<Self>()
            + std::mem::size_of::<Stats>()
            + 4096
            + 4 * std::mem::size_of::<Instant>();
        let size = fixed
            .checked_add(path.as_os_str().len())
            // The application retains its captured raw Arc as well as this
            // controller's UTF-8 copy. Cover both resident source owners.
            .and_then(|n| n.checked_add(source.len().checked_mul(2)?))
            .and_then(|n| n.checked_add(super::accounting::settings_bound(&settings).ok()?))
            .ok_or(Failure::ParentBudget)?;
        let owner = Arc::new(budget.reserve(size)?);
        let source: Arc<str> = std::str::from_utf8(source)
            .map_err(|_| Failure::Frame)?
            .into();
        let mut rate = Rate::new(Instant::now());
        rate.starts = VecDeque::with_capacity(2);
        Ok(Self {
            source,
            format,
            path: Arc::new(path.to_owned()),
            slots: Arc::new(Semaphore::new(2)),
            compiler: tokio::sync::Mutex::new(()),
            admitted_init: AtomicBool::new(false),
            ready: tokio::sync::RwLock::new(None),
            epoch: AtomicUsize::new(0),
            rate: Arc::new(Mutex::new(rate)),
            stats: Arc::new(Stats {
                _ownership: Some(owner.clone()),
                ..Stats::default()
            }),
            memory: Memory {
                supervised: None,
                executable: None,
                owner: Some(owner),
                settings,
                budget,
                source_credit: None,
                allocator_limit: profile.worker_bytes,
                address_space_limit: profile.address_space_bytes,
                cpu_seconds: profile.cpu_seconds,
            },
        })
    }
    /// Opt-in trusted pin. Image capture has no spawn/readiness side effects.
    /// Existing path-based constructors and default activation are unchanged.
    pub fn prepare_pinned(
        pin: &crate::bundle_snapshot::Pin,
        source: &[u8],
        format: SourceFormat,
        settings: Arc<super::Settings>,
        profile: ResourceProfile,
    ) -> Result<Self, Failure> {
        // No original pathname is retained as an execution fallback. An empty
        // legacy name cannot resolve to a different executable if misused.
        let mut controller =
            Self::prepare(std::path::Path::new(""), source, format, settings, profile)?;
        let image = super::sealed_executable::Image::capture(pin, &controller.memory.budget)?;
        controller.memory.executable = Some(image);
        Ok(controller)
    }
    /// Opt-in static admission only; no readiness, spawn or default activation.
    /// Generic/path constructors retain their existing behavior.
    pub fn prepare_static_pinned(
        pin: &crate::bundle_snapshot::Pin,
        source: &[u8],
        format: SourceFormat,
        settings: Arc<super::Settings>,
        profile: ResourceProfile,
    ) -> Result<Self, Failure> {
        let mut controller =
            Self::prepare(std::path::Path::new(""), source, format, settings, profile)?;
        let image =
            super::sealed_executable::Image::capture_static(pin, &controller.memory.budget)?;
        controller.memory.executable = Some(image);
        Ok(controller)
    }
    /// Synchronous trusted qualification API remains explicit. Engine composition
    /// uses the bounded asynchronous capture path below, never a path fallback.
    pub fn prepare_supervised_static_pinned(
        pin: &crate::bundle_snapshot::Pin,
        source: &[u8],
        format: SourceFormat,
        settings: Arc<super::Settings>,
        policy: super::supervision::Policy,
    ) -> Result<Self, Failure> {
        Self::prepare_supervised_in(
            pin,
            source,
            format,
            settings,
            policy,
            (
                super::supervision::Authority::process(),
                super::ownership::Drain::process(),
            ),
        )
    }
    #[cfg(test)]
    fn prepare_supervised_with_authority(
        pin: &crate::bundle_snapshot::Pin,
        source: &[u8],
        format: SourceFormat,
        settings: Arc<super::Settings>,
        policy: super::supervision::Policy,
        authority: Arc<super::supervision::Authority>,
    ) -> Result<Self, Failure> {
        Self::prepare_supervised_in(
            pin,
            source,
            format,
            settings,
            policy,
            (authority, super::ownership::Drain::new()),
        )
    }
    fn profile(policy: super::supervision::Policy) -> Result<ResourceProfile, Failure> {
        policy.validate().map_err(|_| Failure::AdapterInvariant)?;
        Ok(ResourceProfile {
            parent_bytes: usize::try_from(policy.parent_requested_bytes)
                .map_err(|_| Failure::AdapterInvariant)?,
            worker_bytes: usize::try_from(policy.worker_requested_bytes)
                .map_err(|_| Failure::AdapterInvariant)?,
            address_space_bytes: policy.worker_address_space_bytes,
            cpu_seconds: policy.lifetime_cpu_seconds,
        })
    }
    fn attach_supervised(
        mut controller: Self,
        authority: Arc<super::supervision::Authority>,
        owner: super::supervision::Owner,
        drain: Arc<super::ownership::Drain>,
    ) -> Result<Self, Failure> {
        controller.slots = Arc::new(Semaphore::new(1));
        let credit = controller
            .memory
            .budget
            .reserve(std::mem::size_of::<Supervised>() + 2 * std::mem::size_of::<usize>())?;
        controller.memory.supervised = Some(Arc::new(Supervised {
            authority,
            owner: Mutex::new(Some(owner)),
            drain,
            _credit: credit,
        }));
        Ok(controller)
    }
    fn prepare_supervised_in(
        pin: &crate::bundle_snapshot::Pin,
        source: &[u8],
        format: SourceFormat,
        settings: Arc<super::Settings>,
        policy: super::supervision::Policy,
        controls: (
            Arc<super::supervision::Authority>,
            Arc<super::ownership::Drain>,
        ),
    ) -> Result<Self, Failure> {
        let profile = Self::profile(policy)?;
        let (authority, drain) = controls;
        if drain.is_closed() {
            return Err(Failure::NoReady);
        }
        let owner = authority.reserve_owner().map_err(|_| Failure::NoReady)?;
        let controller = Self::prepare_static_pinned(pin, source, format, settings, profile)?;
        Self::attach_supervised(controller, authority, owner, drain)
    }
    #[cfg(test)]
    pub(super) async fn prepare_supervised_async(
        pin: &crate::bundle_snapshot::Pin,
        source: Arc<[u8]>,
        format: SourceFormat,
        settings: Arc<super::Settings>,
        policy: super::supervision::Policy,
        deadline: Instant,
        controls: (
            Arc<super::supervision::Authority>,
            Arc<super::ownership::Drain>,
        ),
    ) -> Result<(Self, super::ownership::Initialization), Failure> {
        Self::prepare_supervised_async_observed(
            pin,
            source,
            format,
            settings,
            policy,
            deadline,
            (controls.0, controls.1, None, None),
        )
        .await
    }
    pub(super) async fn prepare_supervised_async_observed(
        pin: &crate::bundle_snapshot::Pin,
        source: Arc<[u8]>,
        format: SourceFormat,
        settings: Arc<super::Settings>,
        policy: super::supervision::Policy,
        deadline: Instant,
        controls: OwnedPreparationControls,
    ) -> Result<(Self, super::ownership::Initialization), Failure> {
        Self::prepare_supervised_async_selected(
            pin, source, format, settings, policy, deadline, controls, None,
        )
        .await
    }
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn prepare_supervised_async_selected(
        pin: &crate::bundle_snapshot::Pin,
        source: Arc<[u8]>,
        format: SourceFormat,
        settings: Arc<super::Settings>,
        policy: super::supervision::Policy,
        deadline: Instant,
        controls: OwnedPreparationControls,
        readonly: Option<super::ReadonlyProfile>,
    ) -> Result<(Self, super::ownership::Initialization), Failure> {
        if let Some(profile) = readonly {
            profile.validate_pin(pin)?;
        }
        let profile = Self::profile(policy)?;
        let (authority, drain, context, diagnostic) = controls;
        if source.len() > 1 << 20 {
            return Err(Failure::Frame);
        }
        if Instant::now() >= deadline {
            return Err(Failure::Deadline);
        }
        if drain.is_closed() {
            if let Some(d) = &diagnostic {
                d.note(Stage::ControllerDrain, Some(&Failure::NoReady));
            }
            return Err(Failure::NoReady);
        }
        // Busy competing initialization does not close the incumbent epoch.
        let owner = authority.reserve_owner().map_err(|refusal| {
            if let Some(d) = &diagnostic {
                d.note_refusal(Stage::ControllerOwnerReserve, refusal);
            }
            Failure::NoReady
        })?;
        let ticket = drain.capture().map_err(|failure| {
            if matches!(failure, Failure::NoReady)
                && let Some(d) = &diagnostic
            {
                d.note(Stage::ControllerCapture, Some(&failure));
            }
            failure
        })?;
        if let Some(context) = &context {
            context.register_drain(drain.clone()).map_err(|_| {
                if Instant::now() >= context.deadline() {
                    Failure::Deadline
                } else if context.check().is_err() {
                    Failure::Cancelled
                } else {
                    Failure::AdapterInvariant
                }
            })?;
        }
        let budget = Budget::new(profile.parent_bytes);
        let bytes = source
            .len()
            .checked_add(pin.path.as_os_str().len())
            .and_then(|n| n.checked_add(super::accounting::settings_bound(&settings).ok()?))
            .and_then(|n| n.checked_add(std::mem::size_of::<crate::bundle_snapshot::Pin>() + 4096))
            .ok_or(Failure::ParentBudget)?;
        let input_credit = budget.reserve(bytes)?;
        let pin = crate::bundle_snapshot::Pin {
            path: pin.path.clone(),
            bytes: pin.bytes,
            sha256: pin.sha256,
        };
        let epoch = drain.clone();
        let capture_diagnostic = diagnostic.clone();
        let controller = super::ownership::capture(ticket, deadline, move || {
            let _credit = input_credit;
            let mut controller = Self::prepare_with_budget(
                std::path::Path::new(""),
                &source,
                format,
                settings,
                profile,
                budget,
            )?;
            let image = match readonly {
                Some(profile) => super::sealed_executable::readonly::capture(
                    &pin,
                    profile,
                    &controller.memory.budget,
                    capture_diagnostic,
                )?,
                None => super::sealed_executable::Image::capture_static_observed(
                    &pin,
                    &controller.memory.budget,
                    capture_diagnostic,
                )?,
            };
            controller.memory.executable = Some(image);
            Self::attach_supervised(controller, authority, owner, epoch)
        })
        .await
        .map_err(|failure| {
            if matches!(failure, Failure::NoReady)
                && let Some(d) = &diagnostic
            {
                d.note(Stage::ControllerCapture, Some(&failure));
            }
            failure
        })?;
        Ok((controller, super::ownership::Initialization::new(drain)))
    }
    pub(super) fn initialization_scope(&self) -> Option<super::ownership::Initialization> {
        self.memory
            .supervised
            .as_ref()
            .map(|s| super::ownership::Initialization::new(s.drain.clone()))
    }
    pub(super) fn close_supervised(&self) {
        if let Some(s) = &self.memory.supervised {
            self.admitted_init.store(false, SeqCst);
            self.epoch.fetch_add(1, SeqCst);
            self.slots.close();
            s.drain.close();
        }
    }
    pub async fn shutdown(&self, deadline: Instant) -> Result<super::ownership::Snapshot, Failure> {
        let supervised = self
            .memory
            .supervised
            .as_ref()
            .ok_or(Failure::AdapterInvariant)?;
        self.close_supervised();
        let mut ready = tokio::time::timeout_at(deadline, self.ready.write())
            .await
            .map_err(|_| Failure::Deadline)?;
        ready.take();
        drop(ready);
        supervised.drain.shutdown(deadline).await
    }
    pub fn admit_request(&self) -> Result<Admission, Failure> {
        if !self.admitted_init.load(SeqCst)
            || self
                .memory
                .supervised
                .as_ref()
                .is_some_and(|s| s.drain.is_closed())
        {
            return Err(Failure::NoReady);
        }
        Admission::reserve(
            self.memory.supervised.as_ref().map(|s| &s.authority),
            &self.memory.budget,
        )
    }
    pub fn budget(&self) -> Arc<Budget> {
        self.memory.budget.clone()
    }
    /// Same immutable settings are retained in Memory across every generation.
    pub fn new_with_settings(
        path: PathBuf,
        source: Arc<str>,
        format: SourceFormat,
        settings: super::Settings,
    ) -> Self {
        let mut controller = Self::new_with_format(path, source, format);
        controller.memory.settings = Arc::new(settings);
        controller
    }
    pub fn new(path: PathBuf, source: Arc<str>) -> Self {
        Self::new_with_format(path, source, SourceFormat::Json)
    }
    pub fn new_with_format(path: PathBuf, source: Arc<str>, format: SourceFormat) -> Self {
        Self::new_with_budget(path, source, format, Budget::new(256 << 20))
    }
    pub fn new_with_budget(
        path: PathBuf,
        source: Arc<str>,
        format: SourceFormat,
        budget: Arc<Budget>,
    ) -> Self {
        let memory = Memory::new(&source, budget);
        Self {
            source,
            format,
            path: Arc::new(path),
            slots: Arc::new(Semaphore::new(2)),
            compiler: tokio::sync::Mutex::new(()),
            admitted_init: std::sync::atomic::AtomicBool::new(false),
            ready: tokio::sync::RwLock::new(None),
            epoch: AtomicUsize::new(0),
            rate: Arc::new(Mutex::new(Rate::new(Instant::now()))),
            stats: Arc::new(Stats::default()),
            memory,
        }
    }
    pub async fn recover(&self, deadline: Instant) -> Result<(), Failure> {
        let _owner = tokio::time::timeout_at(deadline, self.compiler.lock())
            .await
            .map_err(|_| Failure::Deadline)?;
        self.compile_and_publish(deadline).await?;
        self.admitted_init.store(true, SeqCst);
        Ok(())
    }
    async fn compile_and_publish(&self, deadline: Instant) -> Result<(), Failure> {
        if self
            .memory
            .supervised
            .as_ref()
            .is_some_and(|s| s.authority.phase() != super::supervision::Phase::Reserved)
        {
            if let Some(image) = &self.memory.executable {
                image.note_no_ready(Stage::ControllerPhase);
            }
            return Err(Failure::NoReady);
        }
        let epoch = self.epoch.load(SeqCst);
        let actor = start_owned(
            self.path.clone(),
            self.source.clone(),
            self.slots.clone(),
            self.stats.clone(),
            deadline,
            Some(self.rate.clone()),
            (self.format, self.memory.clone()),
        )
        .await?;
        let mut ready = tokio::time::timeout_at(deadline, self.ready.write())
            .await
            .map_err(|_| Failure::Deadline)?;
        if Instant::now() >= deadline {
            return Err(Failure::Deadline);
        }
        if self.epoch.load(SeqCst) != epoch {
            return Err(Failure::Stale);
        }
        if let Some(supervised) = &self.memory.supervised {
            supervised
                .drain
                .publish(deadline, || *ready = Some(actor))?;
        } else {
            *ready = Some(actor);
        }
        Ok(())
    }
    /// Experimental request-driven availability: one complete-catalogue compiler,
    /// no retry of a request that has already reached a validation child.
    pub async fn request(
        &self,
        event: serde_json::Value,
        deadline: Instant,
    ) -> Result<Retained<Reply>, Failure> {
        if Instant::now() >= deadline {
            return Err(Failure::Deadline);
        }
        if !self.admitted_init.load(SeqCst) {
            return Err(Failure::NoReady);
        }
        let admission = self.admit_request()?;
        self.request_admitted(event, deadline, admission).await
    }
    pub async fn request_admitted(
        &self,
        event: serde_json::Value,
        deadline: Instant,
        admission: Admission,
    ) -> Result<Retained<Reply>, Failure> {
        if Instant::now() >= deadline {
            return Err(Failure::Deadline);
        }
        if !self.admitted_init.load(SeqCst) {
            return Err(Failure::NoReady);
        }
        if !admission.matches(self.memory.supervised.as_ref().map(|s| &s.authority)) {
            return Err(Failure::AdapterInvariant);
        }
        let credit = budget::retain_event(&self.memory.budget, &event).map_err(|e| {
            if Instant::now() >= deadline {
                Failure::Deadline
            } else {
                e
            }
        })?;
        let actor = self.available_actor(deadline).await?;
        let result = actor
            .request_retained(event, credit, deadline, admission)
            .await;
        if result.is_err() {
            let mut ready = tokio::time::timeout_at(deadline, self.ready.write())
                .await
                .map_err(|_| Failure::Deadline)?;
            // A late failure must not invalidate a newer generation.
            if ready
                .as_ref()
                .is_some_and(|current| current.tx.same_channel(&actor.tx))
            {
                self.epoch.fetch_add(1, SeqCst);
                ready.take();
            }
        }
        if Instant::now() >= deadline {
            return Err(Failure::Deadline);
        }
        result
    }
    async fn available_actor(&self, deadline: Instant) -> Result<Actor, Failure> {
        if Instant::now() >= deadline {
            return Err(Failure::Deadline);
        }
        if !self.admitted_init.load(SeqCst) {
            return Err(Failure::NoReady);
        }
        if let Some(actor) = self
            .ready
            .read()
            .await
            .as_ref()
            .filter(|actor| !actor.tx.is_closed())
            .cloned()
        {
            if Instant::now() >= deadline {
                return Err(Failure::Deadline);
            }
            return Ok(actor);
        }
        if self.memory.supervised.is_some() {
            return Err(Failure::NoReady);
        }
        let _owner = tokio::time::timeout_at(deadline, self.compiler.lock())
            .await
            .map_err(|_| Failure::Deadline)?;
        // Double-check after ownership: simultaneous callers do not compile
        // replacement catalogues for a generation already restored by a peer.
        if let Some(actor) = self
            .ready
            .read()
            .await
            .as_ref()
            .filter(|actor| !actor.tx.is_closed())
            .cloned()
        {
            if Instant::now() >= deadline {
                return Err(Failure::Deadline);
            }
            return Ok(actor);
        }
        self.compile_and_publish(deadline).await?;
        self.actor().await
    }
    pub async fn actor(&self) -> Result<Actor, Failure> {
        self.ready.read().await.clone().ok_or_else(|| {
            if let Some(image) = &self.memory.executable {
                image.note_no_ready(Stage::ControllerActorMissing);
            }
            Failure::NoReady
        })
    }
    pub async fn invalidate(&self) {
        let mut ready = self.ready.write().await;
        self.epoch.fetch_add(1, SeqCst);
        ready.take();
    }
}

/// Candidate values are explicit, never an application/default activation grant.
#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ResourceProfile {
    pub parent_bytes: usize,
    pub worker_bytes: usize,
    pub address_space_bytes: u64,
    pub cpu_seconds: u64,
}
impl Default for ResourceProfile {
    fn default() -> Self {
        Self {
            parent_bytes: 256 << 20,
            worker_bytes: 256 << 20,
            address_space_bytes: 512 << 20,
            cpu_seconds: 8,
        }
    }
}
impl ResourceProfile {
    pub fn validate(&self) -> Result<(), Failure> {
        if self.parent_bytes == 0
            || self.worker_bytes == 0
            || self.address_space_bytes < self.worker_bytes as u64
            || self.cpu_seconds == 0
        {
            Err(Failure::Frame)
        } else {
            Ok(())
        }
    }
}

#[cfg(all(test, target_os = "linux", target_arch = "x86_64"))]
mod sealed_image_tests {
    use super::*;
    #[test]
    fn pinned_image_is_shared_by_owner_memory_without_spawning() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("worker");
        let mut b = vec![0; 64];
        b[..7].copy_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1]);
        b[16] = 3;
        b[18] = 62;
        b[20] = 1;
        std::fs::write(&path, &b).unwrap();
        let pin = crate::bundle_snapshot::Pin {
            path,
            bytes: b.len(),
            sha256: Sha256::digest(&b).into(),
        };
        let controller = Controller::prepare_pinned(
            &pin,
            b"{}",
            SourceFormat::Json,
            Arc::default(),
            ResourceProfile {
                parent_bytes: 1 << 20,
                worker_bytes: 1 << 20,
                address_space_bytes: 1 << 20,
                cpu_seconds: 1,
            },
        )
        .unwrap();
        assert!(controller.path.as_os_str().is_empty());
        assert_eq!(controller.stats.live.load(SeqCst), 0);
        let budget = controller.budget();
        let memory = controller.memory.clone();
        let image = Arc::downgrade(memory.executable.as_ref().unwrap());
        drop(controller);
        assert!(image.upgrade().is_some());
        assert!(budget.used() > 0);
        drop(memory);
        assert!(image.upgrade().is_none());
        assert_eq!(budget.used(), 0);
    }
    #[test]
    fn legacy_prepare_keeps_its_path_profile_without_capture_or_spawn() {
        let controller = Controller::prepare(
            std::path::Path::new("/not-opened-by-legacy-prepare"),
            b"{}",
            SourceFormat::Json,
            Arc::default(),
            ResourceProfile {
                parent_bytes: 1 << 20,
                worker_bytes: 1 << 20,
                address_space_bytes: 1 << 20,
                cpu_seconds: 1,
            },
        )
        .unwrap();
        assert!(controller.memory.executable.is_none());
        assert_eq!(
            controller.path.as_ref(),
            std::path::Path::new("/not-opened-by-legacy-prepare")
        );
        assert_eq!(controller.stats.live.load(SeqCst), 0);
    }
}

#[cfg(all(test, target_os = "linux", target_arch = "x86_64"))]
mod supervision_integration_tests {
    use super::super::supervision::{Authority, Phase, Policy};
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    struct Witness {
        drops: Arc<AtomicUsize>,
        _credit: Credit,
        _permit: OwnedSemaphorePermit,
        _fd: std::fs::File,
    }
    impl Drop for Witness {
        fn drop(&mut self) {
            self.drops.fetch_add(1, SeqCst);
        }
    }
    fn witness() -> (Witness, Arc<AtomicUsize>, Arc<Budget>, Arc<Semaphore>) {
        let drops = Arc::new(AtomicUsize::new(0));
        let budget = Budget::new(100);
        let slots = Arc::new(Semaphore::new(1));
        (
            Witness {
                drops: drops.clone(),
                _credit: budget.reserve(20).unwrap(),
                _permit: slots.clone().try_acquire_owned().unwrap(),
                _fd: tempfile::tempfile().unwrap(),
            },
            drops,
            budget,
            slots,
        )
    }
    fn ready_authority() -> (Arc<Authority>, super::super::supervision::Owner) {
        let a = Authority::isolated();
        let mut o = a.reserve_owner().unwrap();
        o.started().unwrap();
        o.ready().unwrap();
        (a, o)
    }
    fn fixture_pin(dir: &std::path::Path) -> crate::bundle_snapshot::Pin {
        // Synthetic static ELF envelope only. Never execute these fixture bytes.
        let mut b = vec![0; 512];
        b[..7].copy_from_slice(&[127, b'E', b'L', b'F', 2, 1, 1]);
        b[16..18].copy_from_slice(&2u16.to_le_bytes());
        b[18..20].copy_from_slice(&62u16.to_le_bytes());
        b[20..24].copy_from_slice(&1u32.to_le_bytes());
        b[24..32].copy_from_slice(&0x400100u64.to_le_bytes());
        b[32..40].copy_from_slice(&64u64.to_le_bytes());
        b[52..54].copy_from_slice(&64u16.to_le_bytes());
        b[54..56].copy_from_slice(&56u16.to_le_bytes());
        b[56..58].copy_from_slice(&2u16.to_le_bytes());
        b[64..68].copy_from_slice(&1u32.to_le_bytes());
        b[68..72].copy_from_slice(&5u32.to_le_bytes());
        for (at, value) in [(80, 0x400000u64), (96, 512), (104, 512), (112, 4096)] {
            b[at..at + 8].copy_from_slice(&value.to_le_bytes());
        }
        b[120..124].copy_from_slice(&0x6474e551u32.to_le_bytes());
        b[124..128].copy_from_slice(&6u32.to_le_bytes());
        let path = dir.join("static-envelope-never-executed");
        std::fs::write(&path, &b).unwrap();
        crate::bundle_snapshot::Pin {
            path,
            bytes: b.len(),
            sha256: Sha256::digest(&b).into(),
        }
    }
    fn policy() -> Policy {
        Policy {
            platform_bytes: 3 << 20,
            parent_requested_bytes: 1 << 20,
            worker_requested_bytes: 1 << 20,
            headroom_bytes: 1 << 20,
            worker_address_space_bytes: 1 << 20,
            lifetime_cpu_seconds: 7,
        }
    }
    #[test]
    fn successful_wait_alone_releases_resources_and_counts_reap() {
        let (resources, drops, budget, slots) = witness();
        let stats = Stats::default();
        stats.live.store(1, SeqCst);
        let result = finalize_wait(
            resources,
            &stats,
            None,
            &Ok(std::process::ExitStatus::from_raw(9)),
        );
        assert!(result.is_ok());
        assert_eq!(drops.load(SeqCst), 1);
        assert_eq!(budget.used(), 0);
        assert_eq!(slots.available_permits(), 1);
        assert_eq!(stats.live.load(SeqCst), 0);
        assert_eq!(stats.reaped.load(SeqCst), 1);
    }
    #[test]
    fn failed_wait_retains_resources_and_does_not_count_reap() {
        let (resources, drops, budget, slots) = witness();
        let stats = Stats::default();
        stats.live.store(1, SeqCst);
        let (a, o) = ready_authority();
        let resources = finalize_wait(
            resources,
            &stats,
            Some(o),
            &Err(std::io::Error::other("synthetic-private")),
        )
        .err()
        .unwrap();
        let held = Mutex::new([None, None]);
        retain_uncertain(&held, resources);
        assert_eq!(a.phase(), Phase::Quarantined);
        assert!(a.reserve_owner().is_err());
        assert_eq!(drops.load(SeqCst), 0);
        assert_eq!(budget.used(), 20);
        assert_eq!(slots.available_permits(), 0);
        assert_eq!(stats.live.load(SeqCst), 1);
        assert_eq!(stats.reaped.load(SeqCst), 0);
        // Only fake resources are freed on test teardown. Real Process quarantine
        // retains its Arc<Stats> cycle until enclosing process reset.
    }
    #[test]
    fn successful_wait_closes_generation_without_granting_restart() {
        let (a, o) = ready_authority();
        let stats = Stats::default();
        stats.live.store(1, SeqCst);
        assert!(
            finalize_wait(
                (),
                &stats,
                Some(o),
                &Ok(std::process::ExitStatus::from_raw(256))
            )
            .is_ok()
        );
        assert_eq!(a.phase(), Phase::Reaped);
        assert!(a.reserve_owner().is_err());
    }
    #[test]
    fn poisoned_quarantine_still_retains_owned_resources() {
        let held = Arc::new(Mutex::new([None, None]));
        let h = held.clone();
        assert!(
            std::thread::spawn(move || {
                let _g = h.lock().unwrap();
                panic!("synthetic poison");
            })
            .join()
            .is_err()
        );
        let (r, drops, budget, _) = witness();
        retain_uncertain(&held, r);
        assert_eq!(drops.load(SeqCst), 0);
        assert_eq!(budget.used(), 20);
    }
    #[test]
    fn request_token_cannot_cross_authorities_or_legacy_path() {
        let (a, _o) = ready_authority();
        let (b, _p) = ready_authority();
        let admission = Admission::reserve(Some(&a), &Budget::new(10000)).unwrap();
        assert!(admission.matches(Some(&a)));
        assert!(!admission.matches(Some(&b)));
        assert!(!admission.matches(None));
        assert!(!Admission::legacy().matches(Some(&a)));
        assert!(Admission::legacy().matches(None));
    }
    #[tokio::test]
    async fn queued_work_holds_permit_after_caller_is_gone() {
        let (a, _o) = ready_authority();
        let admission = Admission::reserve(Some(&a), &Budget::new(10000)).unwrap();
        let other = Admission::reserve(Some(&a), &Budget::new(10000)).unwrap();
        let budget = Budget::new(1000);
        let (tx, _rx) = oneshot::channel();
        let work = Work {
            event: serde_json::json!({}),
            _event_credit: Arc::new(budget.reserve(10).unwrap()),
            _admission: admission.hold(),
            deadline: Instant::now(),
            cancel: Arc::default(),
            reply: tx,
        };
        drop(admission);
        assert!(Admission::reserve(Some(&a), &Budget::new(10000)).is_err());
        drop(work);
        assert!(Admission::reserve(Some(&a), &Budget::new(10000)).is_ok());
        drop(other);
    }
    #[test]
    fn handler_hold_cannot_authorize_or_refund_another_request() {
        let (a, _o) = ready_authority();
        let admission = Admission::reserve(Some(&a), &Budget::new(10000)).unwrap();
        let held = admission.hold();
        let _other = Admission::reserve(Some(&a), &Budget::new(10000)).unwrap();
        drop(admission);
        assert!(Admission::reserve(Some(&a), &Budget::new(10000)).is_err());
        drop(held);
        assert!(Admission::reserve(Some(&a), &Budget::new(10000)).is_ok());
    }
    #[test]
    fn supervised_constructor_captures_static_image_and_reserves_single_owner_without_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let pin = fixture_pin(dir.path());
        let a = Authority::isolated();
        let c = Controller::prepare_supervised_with_authority(
            &pin,
            b"{}",
            SourceFormat::Json,
            Arc::default(),
            policy(),
            a.clone(),
        )
        .unwrap();
        assert_eq!(a.phase(), Phase::Reserved);
        assert_eq!(c.slots.available_permits(), 1);
        assert_eq!(c.stats.live.load(SeqCst), 0);
        assert_eq!(c.memory.cpu_seconds, 7);
        assert!(c.memory.executable.is_some());
        assert!(c.path.as_os_str().is_empty());
        assert!(c.admit_request().is_err());
        assert!(matches!(
            Controller::prepare_supervised_with_authority(
                &pin,
                b"{}",
                SourceFormat::Json,
                Arc::default(),
                policy(),
                a.clone()
            ),
            Err(Failure::NoReady)
        ));
        let memory = c.memory.clone();
        drop(c);
        assert_eq!(a.phase(), Phase::Reserved);
        drop(memory);
        assert_eq!(a.phase(), Phase::Idle);
    }
    #[test]
    fn failed_static_capture_refunds_unstarted_owner() {
        let dir = tempfile::tempdir().unwrap();
        let mut pin = fixture_pin(dir.path());
        pin.sha256 = [0; 32];
        let a = Authority::isolated();
        assert!(
            Controller::prepare_supervised_with_authority(
                &pin,
                b"{}",
                SourceFormat::Json,
                Arc::default(),
                policy(),
                a.clone()
            )
            .is_err()
        );
        assert_eq!(a.phase(), Phase::Idle);
        assert!(a.reserve_owner().is_ok());
    }
    #[test]
    fn invalid_explicit_policy_fails_before_capture_or_owner_reservation() {
        let a = Authority::isolated();
        let pin = crate::bundle_snapshot::Pin {
            path: "/not-opened".into(),
            bytes: 64,
            sha256: [0; 32],
        };
        let mut p = policy();
        p.lifetime_cpu_seconds = 0;
        assert!(matches!(
            Controller::prepare_supervised_with_authority(
                &pin,
                b"{}",
                SourceFormat::Json,
                Arc::default(),
                p,
                a.clone()
            ),
            Err(Failure::AdapterInvariant)
        ));
        assert_eq!(a.phase(), Phase::Idle);
    }

    fn actor(a: Arc<Authority>) -> (Actor, mpsc::Receiver<Work>) {
        let budget = Budget::new(10000);
        let (tx, rx) = mpsc::channel(1);
        let ready = Ready {
            ready: true,
            request_schema_count: 0,
            metadata_json: "{}".into(),
            source_sha256: String::new(),
            raw_bytes: 0,
            resource_bytes: 0,
            resource_nodes: 0,
            catalogue_bytes: 0,
            engine_cache_entries: 0,
            compiler_reservation_bytes: 0,
            aggregate_compiler_input_bytes: 0,
            roots: vec![],
            compile_micros: 0,
        };
        (
            Actor {
                drain: None,
                authority: Some(a),
                _control: Arc::new(budget.reserve(10).unwrap()),
                tx,
                ready: Arc::new(Retained {
                    value: ready,
                    _credit: budget.reserve(10).unwrap(),
                }),
                budget,
            },
            rx,
        )
    }
    #[tokio::test]
    async fn direct_actor_capacity_rejection_allocates_no_event_and_sends_no_work() {
        let (a, _o) = ready_authority();
        let _one = Admission::reserve(Some(&a), &Budget::new(10000)).unwrap();
        let _two = Admission::reserve(Some(&a), &Budget::new(10000)).unwrap();
        let (actor, mut rx) = actor(a);
        let before = actor.budget.used();
        assert!(matches!(
            actor
                .request(
                    serde_json::json!({}),
                    Instant::now() + Duration::from_secs(1)
                )
                .await,
            Err(Failure::NoReady)
        ));
        assert_eq!(actor.budget.used(), before);
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }
    #[tokio::test]
    async fn admitted_actor_path_never_double_charges_capacity() {
        let (a, _o) = ready_authority();
        let admission = Admission::reserve(Some(&a), &Budget::new(10000)).unwrap();
        let _other = Admission::reserve(Some(&a), &Budget::new(10000)).unwrap();
        let (actor, mut rx) = actor(a.clone());
        let task = tokio::spawn(async move {
            let credit = actor.budget.reserve(10).unwrap();
            actor
                .request_retained(
                    serde_json::json!({}),
                    credit,
                    Instant::now() + Duration::from_secs(5),
                    admission,
                )
                .await
        });
        let work = rx.recv().await.unwrap();
        assert!(Admission::reserve(Some(&a), &Budget::new(10000)).is_err());
        let reply = Reply::decode(br#"{"status":200,"body":"ok"}"#).unwrap();
        let reply_budget = Budget::new(10000);
        let sent = Retained {
            value: reply,
            _credit: reply_budget.reserve(10).unwrap(),
        };
        assert!(work.reply.send(Ok(sent)).is_ok());
        assert!(task.await.unwrap().is_ok());
        // Remaining Work fields keep capacity after its reply sender was moved.
        assert!(Admission::reserve(Some(&a), &Budget::new(10000)).is_err());
        drop(work._admission);
        assert!(Admission::reserve(Some(&a), &Budget::new(10000)).is_ok());
    }
    #[test]
    fn lease_budget_refusal_refunds_authority_capacity() {
        let (a, _o) = ready_authority();
        assert!(matches!(
            Admission::reserve(Some(&a), &Budget::new(1)),
            Err(Failure::ParentBudget)
        ));
        let _first = Admission::reserve(Some(&a), &Budget::new(10000)).unwrap();
        let _second = Admission::reserve(Some(&a), &Budget::new(10000)).unwrap();
        assert!(Admission::reserve(Some(&a), &Budget::new(10000)).is_err());
    }
    #[tokio::test]
    async fn reaped_supervised_controller_cannot_recover_or_reset_lifetime_cpu() {
        let dir = tempfile::tempdir().unwrap();
        let pin = fixture_pin(dir.path());
        let a = Authority::isolated();
        let c = Controller::prepare_supervised_with_authority(
            &pin,
            b"{}",
            SourceFormat::Json,
            Arc::default(),
            policy(),
            a.clone(),
        )
        .unwrap();
        let mut owner = c
            .memory
            .supervised
            .as_ref()
            .unwrap()
            .owner
            .lock()
            .unwrap()
            .take()
            .unwrap();
        owner.started().unwrap();
        owner.ready().unwrap();
        owner
            .waited(&Ok(std::process::ExitStatus::from_raw(0)))
            .unwrap();
        assert!(matches!(
            c.recover(Instant::now() + Duration::from_secs(1)).await,
            Err(Failure::NoReady)
        ));
        assert_eq!(a.phase(), Phase::Reaped);
        assert_eq!(c.stats.last_child_pid.load(SeqCst), 0);
        assert_eq!(c.stats.live.load(SeqCst), 0);
        assert_eq!(c.memory.cpu_seconds, 7);
    }
    #[test]
    fn quarantine_retains_real_file_and_lifecycle_owner_witness_without_child() {
        use std::os::fd::AsRawFd;
        let dir = tempfile::tempdir().unwrap();
        let lifecycle =
            super::super::lifecycle::Lifecycle::create(&dir.path().join("synthetic-owner.jsonl"))
                .unwrap();
        let owner_observation = lifecycle.owner_started();
        let (resources, drops, budget, _) = witness();
        let fd = resources._fd.as_raw_fd();
        let stats = Stats::default();
        stats.live.store(1, SeqCst);
        let held = Mutex::new([None, None]);
        let resources = (resources, owner_observation);
        let resources = finalize_wait(
            resources,
            &stats,
            None,
            &Err(std::io::Error::other("synthetic")),
        )
        .err()
        .unwrap();
        retain_uncertain(&held, resources);
        assert!(std::fs::read_link(format!("/proc/self/fd/{fd}")).is_ok());
        assert_eq!(lifecycle.snapshot().active_owners, 1);
        assert_eq!(drops.load(SeqCst), 0);
        assert_eq!(budget.used(), 20);
        assert_eq!(stats.reaped.load(SeqCst), 0);
    }

    #[tokio::test]
    async fn async_capture_never_spawns_and_reuses_controller_budget() {
        let dir = tempfile::tempdir().unwrap();
        let pin = fixture_pin(dir.path());
        let a = Authority::isolated();
        let drain = super::super::ownership::Drain::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        let (controller, scope) = Controller::prepare_supervised_async(
            &pin,
            Arc::from(&b"{}"[..]),
            SourceFormat::Json,
            Arc::default(),
            policy(),
            deadline,
            (a.clone(), drain.clone()),
        )
        .await
        .unwrap();
        assert_eq!(controller.stats.live.load(SeqCst), 0);
        assert_eq!(controller.stats.last_child_pid.load(SeqCst), 0);
        assert_eq!(drain.snapshot().captures, 0);
        assert_eq!(a.phase(), Phase::Reserved);
        assert!(Arc::ptr_eq(&controller.memory.budget, &controller.budget()));
        assert!(controller.memory.executable.is_some());
        scope.publish(deadline, || ()).unwrap();
        assert!(!drain.is_closed());
        let result = controller.shutdown(deadline).await.unwrap();
        assert_eq!(result.owners, 0);
        // Quiescent work is not release of the still-owned captured image.
        assert!(controller.budget().used() > 0);
        assert_eq!(a.phase(), Phase::Reserved);
        drop(controller);
        assert_eq!(a.phase(), Phase::Idle);
    }
    #[tokio::test]
    async fn busy_preparation_does_not_close_incumbent_epoch() {
        let (a, _owner) = ready_authority();
        let drain = super::super::ownership::Drain::new();
        let pin = crate::bundle_snapshot::Pin {
            path: "/never-opened-busy".into(),
            bytes: 64,
            sha256: [0; 32],
        };
        let result = Controller::prepare_supervised_async(
            &pin,
            Arc::from(&b"{}"[..]),
            SourceFormat::Json,
            Arc::default(),
            policy(),
            Instant::now() + Duration::from_secs(5),
            (a.clone(), drain.clone()),
        )
        .await;
        assert!(matches!(result, Err(Failure::NoReady)));
        assert!(!drain.is_closed());
        assert_eq!(drain.snapshot().captures, 0);
        assert_eq!(a.phase(), Phase::Ready);
    }
    #[tokio::test]
    async fn expired_preparation_opens_nothing_and_reserves_no_owner() {
        let a = Authority::isolated();
        let drain = super::super::ownership::Drain::new();
        let pin = crate::bundle_snapshot::Pin {
            path: "/never-opened-expired".into(),
            bytes: 64,
            sha256: [0; 32],
        };
        assert!(matches!(
            Controller::prepare_supervised_async(
                &pin,
                Arc::from(&b"{}"[..]),
                SourceFormat::Json,
                Arc::default(),
                policy(),
                Instant::now(),
                (a.clone(), drain.clone())
            )
            .await,
            Err(Failure::Deadline)
        ));
        assert_eq!(a.phase(), Phase::Idle);
        assert!(!drain.is_closed());
        assert_eq!(drain.snapshot().captures, 0);
    }
}

#[cfg(test)]
#[path = "controller_diagnostic_tests.rs"]
mod init_diagnostic_tests;
