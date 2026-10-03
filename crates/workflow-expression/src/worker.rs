//! Program owners never cross these message boundaries. Control traffic is separate
//! from bounded work; cancellation observes safe boundaries, not parser internals.
use crate::{ExpressionError, Kind, Limits, Position, Profile, compiler, facade::Bindings};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tokio::sync::oneshot;

#[derive(Clone, Copy, Debug)]
pub struct WorkerConfig {
    pub workers: usize,
    pub stack_bytes: usize,
    pub queue_per_worker: usize,
    pub leases_per_worker: usize,
    pub cache_entries: usize,
    pub cache_bytes: usize,
}
impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            workers: 2,
            stack_bytes: crate::COMPILATION_STACK_BYTES,
            queue_per_worker: 32,
            leases_per_worker: 256,
            cache_entries: 128,
            cache_bytes: 16 * 1024 * 1024,
        }
    }
}
impl WorkerConfig {
    pub fn stack_reservation(self) -> Option<usize> {
        self.stack_bytes.checked_mul(self.workers)
    }
    pub fn partitions(self) -> Vec<(usize, usize)> {
        (0..self.workers)
            .map(|i| {
                (
                    self.cache_entries / self.workers
                        + usize::from(i < self.cache_entries % self.workers),
                    self.cache_bytes / self.workers
                        + usize::from(i < self.cache_bytes % self.workers),
                )
            })
            .collect()
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkerError {
    Expression(ExpressionError),
    Configuration,
    Unavailable,
    Full,
    InvalidHandle,
    IncompleteCleanup,
    BlockingInAsync,
}
impl From<ExpressionError> for WorkerError {
    fn from(value: ExpressionError) -> Self {
        Self::Expression(value)
    }
}
impl WorkerError {
    pub(crate) fn at(self, span: usize, offset: usize) -> Self {
        match self {
            Self::Expression(error) => Self::Expression(error.at(span, offset)),
            other => other,
        }
    }
}
impl std::fmt::Display for WorkerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Expression(error) => error.fmt(f),
            other => write!(f, "workflow expression worker: {other:?}"),
        }
    }
}
impl std::error::Error for WorkerError {}
// All simultaneously alive pools reserve against one process-wide ceiling. The
// last worker's reservation reference outlives disposal, including incomplete drain.
static BUDGET: Mutex<(usize, usize)> = Mutex::new((0, 0));
struct Reservation {
    entries: usize,
    bytes: usize,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        let mut budget = BUDGET.lock().unwrap_or_else(|e| e.into_inner());
        budget.0 -= self.entries;
        budget.1 -= self.bytes;
    }
}
static GENERATION: AtomicU64 = AtomicU64::new(1);
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Id {
    worker: usize,
    generation: u64,
    slot: u64,
}
struct Lease {
    id: Id,
    route: Weak<Route>,
}
impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(route) = self.route.upgrade() {
            // Reliable ID-only release, independent of the saturated work queue.
            let _ = route.control.send(Control::Release(self.id));
        }
    }
}
/// Opaque process-local lease. Private IDs have a monotonically increasing slot
/// nonce as well as an engine generation; no slot reuse can alias an expression.
#[derive(Clone)]
pub struct Handle {
    lease: Arc<Lease>,
}
struct Route {
    work: Mutex<Option<mpsc::SyncSender<Job>>>,
    control: mpsc::Sender<Control>,
    generation: u64,
    worker: usize,
}
enum Control {
    Release(Id),
    Shutdown,
}
enum Op {
    Step(crate::StepRequest),
    Compile {
        profile: Profile,
        source: String,
        position: Position,
        limits: Limits,
    },
    Evaluate {
        id: Id,
        bindings: Bindings,
        kind: Kind,
    },
    Stats,
    #[cfg(test)]
    Barrier {
        entered: mpsc::Sender<()>,
        resume: mpsc::Receiver<()>,
    },
}
enum Reply {
    Step(Result<Vec<Value>, crate::StepError>),
    Handle(Handle),
    Value(Value),
    Stats(usize, usize),
}
struct Job {
    op: Op,
    cancelled: Arc<AtomicBool>,
    reply: oneshot::Sender<Result<Reply, WorkerError>>,
}
struct Running {
    join: Option<JoinHandle<()>>,
    ack: mpsc::Receiver<()>,
}
struct Inner {
    routes: Vec<Arc<Route>>,
    running: Mutex<Vec<Running>>,
    config: WorkerConfig,
    compilations: AtomicU64,
}
#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}
struct Pending {
    reply: Option<oneshot::Receiver<Result<Reply, WorkerError>>>,
    cancelled: Arc<AtomicBool>,
}
impl Drop for Pending {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}
impl Pending {
    fn wait(mut self) -> Result<Reply, WorkerError> {
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(WorkerError::BlockingInAsync);
        }
        self.reply
            .take()
            .unwrap()
            .blocking_recv()
            .map_err(|_| WorkerError::Unavailable)?
    }
    async fn receive(mut self) -> Result<Reply, WorkerError> {
        self.reply
            .take()
            .unwrap()
            .await
            .map_err(|_| WorkerError::Unavailable)?
    }
}
pub struct Compilation {
    pending: Pending,
}
impl Compilation {
    pub async fn receive(self) -> Result<Handle, WorkerError> {
        match self.pending.receive().await? {
            Reply::Handle(h) => Ok(h),
            _ => Err(WorkerError::Unavailable),
        }
    }
    pub fn wait(self) -> Result<Handle, WorkerError> {
        match self.pending.wait()? {
            Reply::Handle(h) => Ok(h),
            _ => Err(WorkerError::Unavailable),
        }
    }
}
impl Engine {
    /// W4 batches have one owning worker; multiworker session routing is not implemented.
    pub async fn step(&self, request: crate::StepRequest) -> Result<Vec<Value>, crate::StepError> {
        let unavailable = |error| crate::StepError {
            path: String::new(),
            error,
        };
        if self.config().workers != 1 {
            return Err(unavailable(WorkerError::Configuration));
        }
        let pending = self.submit(0, Op::Step(request)).map_err(unavailable)?;
        match pending.receive().await.map_err(unavailable)? {
            Reply::Step(result) => result,
            _ => Err(unavailable(WorkerError::Unavailable)),
        }
    }
    pub fn new(config: WorkerConfig) -> Result<Self, WorkerError> {
        Self::start(config, None)
    }
    fn start(config: WorkerConfig, observer: Option<Observer>) -> Result<Self, WorkerError> {
        if config.workers == 0
            || config.workers > 128
            || config.stack_bytes <= 2 * 1024 * 1024
            || config.stack_reservation().is_none()
            || config.queue_per_worker == 0
            || config.queue_per_worker > 256
            || config.leases_per_worker == 0
            || config.leases_per_worker > 4096
            || config.cache_entries > 128
            || config.cache_bytes > 16 * 1024 * 1024
        {
            return Err(WorkerError::Configuration);
        }
        let reservation = {
            let mut budget = BUDGET.lock().unwrap_or_else(|e| e.into_inner());
            if budget.0 + config.cache_entries > 128
                || budget.1 + config.cache_bytes > 16 * 1024 * 1024
            {
                return Err(WorkerError::Full);
            }
            budget.0 += config.cache_entries;
            budget.1 += config.cache_bytes;
            Arc::new(Reservation {
                entries: config.cache_entries,
                bytes: config.cache_bytes,
            })
        };
        let generation = GENERATION
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| v.checked_add(1))
            .map_err(|_| WorkerError::Unavailable)?;
        let mut routes = Vec::<Arc<Route>>::new();
        let mut running = Vec::<Running>::new();
        for (worker, (entries, bytes)) in config.partitions().into_iter().enumerate() {
            let (tx, rx) = mpsc::sync_channel(config.queue_per_worker);
            let (control, controls) = mpsc::channel();
            let (ack_tx, ack) = mpsc::channel();
            let route = Arc::new(Route {
                work: Mutex::new(Some(tx)),
                control,
                generation,
                worker,
            });
            let weak = Arc::downgrade(&route);
            let reservation = reservation.clone();
            let observer = observer.clone();
            let join = match thread::Builder::new()
                .name(format!("workflow-expression-{generation}-{worker}"))
                .stack_size(config.stack_bytes)
                .spawn(move || {
                    let _reservation = reservation;
                    OBSERVER.with(|o| *o.borrow_mut() = observer);
                    run(
                        weak,
                        rx,
                        controls,
                        ack_tx,
                        entries,
                        bytes,
                        config.leases_per_worker,
                    );
                    OBSERVER.with(|o| *o.borrow_mut() = None);
                }) {
                Ok(join) => join,
                Err(_) => {
                    for r in &routes {
                        stop(r);
                    }
                    for r in &mut running {
                        let r: &mut Running = r;
                        if r.ack.recv_timeout(Duration::from_secs(5)).is_ok()
                            && let Some(join) = r.join.take()
                        {
                            let _ = join.join();
                        }
                    }
                    return Err(WorkerError::Unavailable);
                }
            };
            routes.push(route);
            running.push(Running {
                join: Some(join),
                ack,
            });
        }
        tracing::info!(
            stack_bytes = config.stack_bytes,
            workers = config.workers,
            aggregate_stack_bytes = config.stack_reservation().unwrap(),
            cache_entries = config.cache_entries,
            cache_bytes = config.cache_bytes,
            queue_per_worker = config.queue_per_worker,
            "workflow expression workers started"
        );
        Ok(Self {
            inner: Arc::new(Inner {
                routes,
                running: Mutex::new(running),
                config,
                compilations: AtomicU64::new(0),
            }),
        })
    }
    pub fn config(&self) -> WorkerConfig {
        self.inner.config
    }
    pub fn compilation_requests(&self) -> u64 {
        self.inner.compilations.load(Ordering::Relaxed)
    }
    fn partition(&self, source: &str) -> usize {
        let hash = Sha256::digest(source.as_bytes());
        // V2 is the only compiled profile. Position/location never enters this key.
        u64::from_be_bytes(hash[..8].try_into().unwrap()) as usize % self.inner.routes.len()
    }
    fn submit(&self, worker: usize, op: Op) -> Result<Pending, WorkerError> {
        let compilation = matches!(&op, Op::Compile { .. });
        let route = &self.inner.routes[worker];
        let gate = route.work.lock().unwrap_or_else(|e| e.into_inner());
        let tx = gate.as_ref().ok_or(WorkerError::Unavailable)?;
        let (reply, receiver) = oneshot::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        tx.try_send(Job {
            op,
            cancelled: cancelled.clone(),
            reply,
        })
        .map_err(|error| match error {
            mpsc::TrySendError::Full(_) => WorkerError::Full,
            mpsc::TrySendError::Disconnected(_) => WorkerError::Unavailable,
        })?;
        if compilation {
            self.inner.compilations.fetch_add(1, Ordering::Relaxed);
        }
        Ok(Pending {
            reply: Some(receiver),
            cancelled,
        })
    }
    pub fn enqueue(
        &self,
        profile: Profile,
        source: String,
        position: Position,
        limits: Limits,
    ) -> Result<Compilation, WorkerError> {
        let worker = self.partition(&source);
        Ok(Compilation {
            pending: self.submit(
                worker,
                Op::Compile {
                    profile,
                    source,
                    position,
                    limits,
                },
            )?,
        })
    }
    pub fn compile(
        &self,
        source: &str,
        position: Position,
        limits: Limits,
    ) -> Result<Handle, WorkerError> {
        self.enqueue(Profile::CelWorkflowV2, source.to_owned(), position, limits)?
            .wait()
    }
    pub fn evaluate(
        &self,
        handle: &Handle,
        bindings: &Bindings,
        kind: Kind,
    ) -> Result<Value, WorkerError> {
        let id = handle.lease.id;
        let route = self
            .inner
            .routes
            .get(id.worker)
            .ok_or(WorkerError::InvalidHandle)?;
        if id.generation != route.generation
            || !Weak::ptr_eq(&handle.lease.route, &Arc::downgrade(route))
        {
            return Err(WorkerError::InvalidHandle);
        }
        match self
            .submit(
                id.worker,
                Op::Evaluate {
                    id,
                    bindings: bindings.clone(),
                    kind,
                },
            )?
            .wait()?
        {
            Reply::Value(value) => Ok(value),
            _ => Err(WorkerError::Unavailable),
        }
    }
    pub fn cache_stats(&self) -> Result<(usize, usize), WorkerError> {
        let mut total = (0, 0);
        for worker in 0..self.inner.routes.len() {
            if let Reply::Stats(entries, bytes) = self.submit(worker, Op::Stats)?.wait()? {
                total.0 += entries;
                total.1 += bytes;
            }
        }
        Ok(total)
    }
    /// Bounded acknowledgement wait. A busy CEL call cannot be forcibly interrupted.
    /// A failed drain retains worker ownership and its cache reservation.
    pub fn shutdown(&self, timeout: Duration) -> Result<(), WorkerError> {
        self.inner.shutdown(timeout)
    }
}
pub(crate) fn evaluate_handle(
    handle: &Handle,
    bindings: &Bindings,
    kind: Kind,
) -> Result<Value, WorkerError> {
    let route = handle
        .lease
        .route
        .upgrade()
        .ok_or(WorkerError::InvalidHandle)?;
    let (reply, receiver) = oneshot::channel();
    let cancelled = Arc::new(AtomicBool::new(false));
    {
        let gate = route.work.lock().unwrap_or_else(|e| e.into_inner());
        gate.as_ref()
            .ok_or(WorkerError::Unavailable)?
            .try_send(Job {
                op: Op::Evaluate {
                    id: handle.lease.id,
                    bindings: bindings.clone(),
                    kind,
                },
                cancelled: cancelled.clone(),
                reply,
            })
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => WorkerError::Full,
                _ => WorkerError::Unavailable,
            })?;
    }
    match (Pending {
        reply: Some(receiver),
        cancelled,
    })
    .wait()?
    {
        Reply::Value(value) => Ok(value),
        _ => Err(WorkerError::Unavailable),
    }
}
fn stop(route: &Route) {
    let mut gate = route.work.lock().unwrap_or_else(|e| e.into_inner());
    if gate.take().is_some() {
        let _ = route.control.send(Control::Shutdown);
    }
}
impl Inner {
    fn shutdown(&self, timeout: Duration) -> Result<(), WorkerError> {
        for route in &self.routes {
            stop(route);
        }
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(WorkerError::Configuration)?;
        let mut running = self.running.lock().unwrap_or_else(|e| e.into_inner());
        for worker in running.iter_mut() {
            if worker.join.is_none() {
                continue;
            }
            worker
                .ack
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .map_err(|_| WorkerError::IncompleteCleanup)?;
            if worker.join.take().unwrap().join().is_err() {
                return Err(WorkerError::IncompleteCleanup);
            }
        }
        Ok(())
    }
}
impl Drop for Inner {
    fn drop(&mut self) {
        if self.shutdown(Duration::from_secs(5)).is_err() {
            eprintln!("workflow expression worker shutdown: incomplete cleanup");
        }
    }
}
type Observer = Arc<Mutex<Vec<(&'static str, thread::ThreadId)>>>;
thread_local! { static OBSERVER: std::cell::RefCell<Option<Observer>> = const { std::cell::RefCell::new(None) }; }
pub(crate) fn witness(event: &'static str) {
    OBSERVER.with(|o| {
        if let Some(observer) = &*o.borrow() {
            observer
                .lock()
                .unwrap()
                .push((event, thread::current().id()));
        }
    });
}
fn run(
    route: Weak<Route>,
    work: mpsc::Receiver<Job>,
    controls: mpsc::Receiver<Control>,
    ack: mpsc::Sender<()>,
    entries: usize,
    bytes: usize,
    leases: usize,
) {
    // These !Send Rc program owners are created here, never in the coordinator.
    let mut cache = compiler::CompileCache::with_budget(entries, bytes);
    let mut registry = HashMap::<Id, compiler::Compiled>::new();
    let mut nonce = 0u64;
    let mut shutting_down = false;
    loop {
        while let Ok(control) = controls.try_recv() {
            match control {
                Control::Release(id) => {
                    registry.remove(&id);
                }
                Control::Shutdown => shutting_down = true,
            }
        }
        cache.collect();
        let job = match work.recv_timeout(Duration::from_millis(10)) {
            Ok(job) => job,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        // A release can arrive while recv_timeout waits for work. Observe control
        // again before servicing that work, including cache statistics/barriers.
        while let Ok(control) = controls.try_recv() {
            match control {
                Control::Release(id) => {
                    registry.remove(&id);
                }
                Control::Shutdown => shutting_down = true,
            }
        }
        cache.collect();
        if shutting_down || job.cancelled.load(Ordering::Acquire) || job.reply.is_closed() {
            let _ = job.reply.send(Err(WorkerError::Unavailable));
            continue;
        }
        let result = (|| -> Result<Reply, WorkerError> {
            match job.op {
                Op::Step(request) => Ok(Reply::Step(crate::step::run(
                    request,
                    &mut cache,
                    &job.cancelled,
                ))),
                Op::Compile {
                    profile,
                    source,
                    position,
                    limits,
                } => {
                    if registry.len() >= leases {
                        return Err(WorkerError::Full);
                    }
                    let compiled = cache.compile_with_limits(profile, &source, position, limits)?;
                    if job.cancelled.load(Ordering::Acquire) || job.reply.is_closed() {
                        drop(compiled);
                        cache.collect();
                        return Err(WorkerError::Unavailable);
                    }
                    nonce = nonce.checked_add(1).ok_or(WorkerError::Unavailable)?;
                    let owner = route.upgrade().ok_or(WorkerError::Unavailable)?;
                    let id = Id {
                        worker: owner.worker,
                        generation: owner.generation,
                        slot: nonce,
                    };
                    registry.insert(id, compiled);
                    Ok(Reply::Handle(Handle {
                        lease: Arc::new(Lease {
                            id,
                            route: Arc::downgrade(&owner),
                        }),
                    }))
                }
                Op::Evaluate { id, bindings, kind } => {
                    let compiled = registry.get(&id).ok_or(WorkerError::InvalidHandle)?;
                    let bindings = crate::json::Bindings::with_limits(
                        &bindings.context,
                        &bindings.input,
                        bindings.output.as_ref(),
                        bindings.value.as_ref(),
                        bindings.limits,
                    )?;
                    Ok(Reply::Value(crate::json::evaluate(
                        compiled, &bindings, kind,
                    )?))
                }
                Op::Stats => Ok(Reply::Stats(cache.len(), cache.retained_bytes())),
                #[cfg(test)]
                Op::Barrier { entered, resume } => {
                    let _ = entered.send(());
                    let _ = resume.recv();
                    Ok(Reply::Value(Value::Null))
                }
            }
        })();
        let stopping = route
            .upgrade()
            .is_none_or(|r| r.work.lock().unwrap_or_else(|e| e.into_inner()).is_none());
        if job.cancelled.load(Ordering::Acquire) || stopping {
            drop(result);
        } else {
            let _ = job.reply.send(result);
        } // failed send drops metadata only; reliable release follows
    }
    // Dropping queued jobs only drops source/plain data/oneshot senders. Registry and
    // cache final disposal is on this worker, before acknowledgement and reservation release.
    drop(registry);
    drop(cache);
    witness("shutdown");
    let _ = ack.send(());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Category;
    use serde_json::json;
    static SERIAL: Mutex<()> = Mutex::new(());
    fn config(entries: usize) -> WorkerConfig {
        WorkerConfig {
            workers: 1,
            queue_per_worker: 2,
            leases_per_worker: 8,
            cache_entries: entries,
            cache_bytes: if entries == 0 { 0 } else { 64 * 1024 },
            ..WorkerConfig::default()
        }
    }
    fn fixture(entries: usize) -> (Engine, Observer) {
        let observer = Arc::new(Mutex::new(Vec::new()));
        (
            Engine::start(config(entries), Some(observer.clone())).unwrap(),
            observer,
        )
    }
    fn finish(engine: &Engine, observer: &Observer) {
        engine.shutdown(Duration::from_secs(2)).unwrap();
        let events = observer.lock().unwrap();
        let current = thread::current().id();
        assert!(events.iter().all(|(_, id)| *id != current));
        assert!(events.iter().all(|(_, id)| *id == events[0].1));
        assert_eq!(
            events.iter().filter(|(e, _)| *e == "create").count(),
            events.iter().filter(|(e, _)| *e == "drop").count()
        );
        assert_eq!(events.last().unwrap().0, "shutdown");
    }
    fn blocked(engine: &Engine) -> (Pending, mpsc::Sender<()>) {
        let (tx, rx) = mpsc::channel();
        let (entered, ready) = mpsc::channel();
        let pending = engine
            .submit(
                0,
                Op::Barrier {
                    entered,
                    resume: rx,
                },
            )
            .unwrap();
        ready.recv().unwrap();
        (pending, tx)
    }
    #[test]
    fn success_evaluation_release_and_shutdown_are_worker_owned() {
        let _serial = SERIAL.lock().unwrap();
        let (engine, observer) = fixture(0);
        let handle = engine
            .compile("context.n + 1", Position::Set, Limits::default())
            .unwrap();
        let b = Bindings::new(&json!({"n":2}), &Value::Null, None, None).unwrap();
        assert_eq!(engine.evaluate(&handle, &b, Kind::Json).unwrap(), json!(3));
        drop(handle);
        engine.cache_stats().unwrap();
        finish(&engine, &observer);
    }
    #[test]
    fn rejection_and_position_hit_checks_cleanup_on_owner() {
        let _serial = SERIAL.lock().unwrap();
        let (engine, observer) = fixture(2);
        assert!(
            matches!(engine.compile("unknown_root",Position::Set,Limits::default()),Err(WorkerError::Expression(e)) if e.category==Category::Invalid)
        );
        let h = engine
            .compile("output", Position::Export, Limits::default())
            .unwrap();
        drop(h);
        let before = engine.cache_stats().unwrap();
        assert!(
            matches!(engine.compile("output",Position::Set,Limits::default()),Err(WorkerError::Expression(e)) if e.category==Category::Invalid)
        );
        assert_eq!(engine.cache_stats().unwrap(), before);
        finish(&engine, &observer);
    }
    #[test]
    fn pinned_eviction_keeps_charge_until_final_release() {
        let _serial = SERIAL.lock().unwrap();
        let (engine, observer) = fixture(1);
        let first = engine
            .compile("1", Position::Set, Limits::default())
            .unwrap();
        let charge = engine.cache_stats().unwrap();
        let second = engine
            .compile("2", Position::Set, Limits::default())
            .unwrap();
        assert_eq!(engine.cache_stats().unwrap(), charge);
        let b = Bindings::new(&json!({}), &Value::Null, None, None).unwrap();
        assert_eq!(engine.evaluate(&first, &b, Kind::Json).unwrap(), json!(1));
        drop(first);
        assert_eq!(engine.cache_stats().unwrap(), (0, 0));
        drop(second);
        finish(&engine, &observer);
    }
    #[test]
    fn stale_slot_generation_and_duplicate_release_cannot_alias() {
        let _serial = SERIAL.lock().unwrap();
        let (engine, observer) = fixture(0);
        let first = engine
            .compile("1", Position::Set, Limits::default())
            .unwrap();
        let id = first.lease.id;
        let route = first.lease.route.clone();
        drop(first);
        engine.cache_stats().unwrap();
        let second = engine
            .compile("2", Position::Set, Limits::default())
            .unwrap();
        assert_ne!(id, second.lease.id);
        let stale = Handle {
            lease: Arc::new(Lease { id, route }),
        };
        let b = Bindings::new(&json!({}), &Value::Null, None, None).unwrap();
        assert_eq!(
            engine.evaluate(&stale, &b, Kind::Json),
            Err(WorkerError::InvalidHandle)
        );
        engine.inner.routes[0]
            .control
            .send(Control::Release(id))
            .unwrap();
        drop(stale);
        assert_eq!(engine.evaluate(&second, &b, Kind::Json).unwrap(), json!(2));
        let wrong = Handle {
            lease: Arc::new(Lease {
                id: Id {
                    generation: id.generation + 1,
                    ..id
                },
                route: second.lease.route.clone(),
            }),
        };
        assert_eq!(
            engine.evaluate(&wrong, &b, Kind::Json),
            Err(WorkerError::InvalidHandle)
        );
        drop(wrong);
        drop(second);
        finish(&engine, &observer);
    }
    #[test]
    fn abandoned_completed_reply_releases_lease_without_program_transfer() {
        let _serial = SERIAL.lock().unwrap();
        let (engine, observer) = fixture(0);
        let pending = engine
            .enqueue(
                Profile::CelWorkflowV2,
                "1".into(),
                Position::Set,
                Limits::default(),
            )
            .unwrap();
        engine.cache_stats().unwrap(); // compile reply now contains an ID-only lease
        drop(pending);
        engine.cache_stats().unwrap();
        finish(&engine, &observer);
    }
    #[test]
    fn cancelled_queued_job_full_queue_and_cleanup_channel() {
        let _serial = SERIAL.lock().unwrap();
        let observer = Arc::new(Mutex::new(Vec::new()));
        let engine = Engine::start(
            WorkerConfig {
                queue_per_worker: 1,
                ..config(0)
            },
            Some(observer.clone()),
        )
        .unwrap();
        let handle = engine
            .compile("1", Position::Set, Limits::default())
            .unwrap();
        let (pending, tx) = blocked(&engine);
        let queued = engine
            .enqueue(
                Profile::CelWorkflowV2,
                "2".into(),
                Position::Set,
                Limits::default(),
            )
            .unwrap();
        assert!(matches!(
            engine.enqueue(
                Profile::CelWorkflowV2,
                "3".into(),
                Position::Set,
                Limits::default()
            ),
            Err(WorkerError::Full)
        ));
        drop(handle); // cannot be lost despite saturated work queue
        queued.pending.cancelled.store(true, Ordering::Release);
        tx.send(()).unwrap();
        pending.wait().unwrap();
        assert!(matches!(queued.wait(), Err(WorkerError::Unavailable)));
        engine.cache_stats().unwrap();
        assert_eq!(
            observer
                .lock()
                .unwrap()
                .iter()
                .filter(|(e, _)| *e == "create")
                .count(),
            1
        );
        finish(&engine, &observer);
    }
    #[test]
    fn shutdown_incomplete_does_not_ack_or_release_reservation() {
        let _serial = SERIAL.lock().unwrap();
        let (engine, observer) = fixture(1);
        let handle = engine
            .compile("1", Position::Set, Limits::default())
            .unwrap();
        let (pending, tx) = blocked(&engine);
        assert_eq!(
            engine.shutdown(Duration::ZERO),
            Err(WorkerError::IncompleteCleanup)
        );
        assert!(matches!(
            engine.enqueue(
                Profile::CelWorkflowV2,
                "2".into(),
                Position::Set,
                Limits::default()
            ),
            Err(WorkerError::Unavailable)
        ));
        assert!(
            observer
                .lock()
                .unwrap()
                .iter()
                .all(|(e, _)| *e != "shutdown")
        );
        drop(handle);
        drop(pending);
        tx.send(()).unwrap();
        finish(&engine, &observer);
    }
    #[test]
    fn aggregate_partitions_and_zero_quota_worker() {
        let _serial = SERIAL.lock().unwrap();
        let config = WorkerConfig {
            workers: 3,
            cache_entries: 2,
            cache_bytes: 6,
            ..WorkerConfig::default()
        };
        let partitions = config.partitions();
        assert_eq!(partitions.iter().map(|q| q.0).sum::<usize>(), 2);
        assert_eq!(partitions.iter().map(|q| q.1).sum::<usize>(), 6);
        assert_eq!(partitions[2].0, 0);
        let engine = Engine::new(config).unwrap();
        for s in ["1", "2", "3", "4"] {
            drop(engine.compile(s, Position::Set, Limits::default()).unwrap());
        }
        assert_eq!(engine.cache_stats().unwrap(), (0, 0));
        assert!(matches!(
            Engine::new(WorkerConfig::default()),
            Err(WorkerError::Full)
        ));
        engine.shutdown(Duration::from_secs(2)).unwrap();
        drop(engine);
        let engine = Engine::new(WorkerConfig::default()).unwrap();
        assert_eq!(engine.config().stack_reservation(), Some(16 * 1024 * 1024));
        engine.shutdown(Duration::from_secs(2)).unwrap();
    }
    #[test]
    fn step_selected_assertion_bindings_convert_once_on_owner() {
        let _serial = SERIAL.lock().unwrap();
        let (engine, observer) = fixture(16);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let request = crate::StepRequest {
            cancel_after_span: None,
            bindings: Bindings::new(&json!({"rows":[3,4]}), &json!({"immutable":9}), None, None)
                .unwrap(),
            first_true: false,
            fields: vec![
                crate::StepField {
                    path: "/assert/value".into(),
                    position: Position::AssertValue,
                    template: json!("${context.rows}"),
                    value_from: None,
                },
                crate::StepField {
                    path: "/assert/json/0".into(),
                    position: Position::AssertJsonPredicate,
                    template: json!("${value == 3}"),
                    value_from: Some((0, "$[0]".into())),
                },
                crate::StepField {
                    path: "/assert/json/1".into(),
                    position: Position::AssertJsonPredicate,
                    template: json!("${value == 4 && workflow.input.immutable == 9}"),
                    value_from: Some((0, "$[1]".into())),
                },
            ],
        };
        assert_eq!(
            runtime.block_on(engine.step(request)).unwrap(),
            vec![json!([3, 4]), json!(true), json!(true)]
        );
        let conversions = observer
            .lock()
            .unwrap()
            .iter()
            .filter(|(event, _)| *event == "step-conversion")
            .count();
        assert_eq!(conversions, 1);
        finish(&engine, &observer);
    }

    #[test]
    fn step_cancellation_after_recursive_span_discards_result_and_keeps_cleanup_owned() {
        let _serial = SERIAL.lock().unwrap();
        let (engine, observer) = fixture(16);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let request = crate::StepRequest {
            cancel_after_span: Some(1),
            bindings: Bindings::new(&json!({}), &json!({}), None, None).unwrap(),
            first_true: false,
            fields: vec![crate::StepField {
                path: "/set".into(),
                position: Position::Set,
                template: json!({"nested":["${'first'} ${'second'}"]}),
                value_from: None,
            }],
        };
        let error = runtime.block_on(engine.step(request)).unwrap_err();
        assert_eq!(error.error, WorkerError::Unavailable);
        let executions = observer
            .lock()
            .unwrap()
            .iter()
            .filter(|(event, _)| *event == "execute")
            .count();
        assert_eq!(executions, 1);
        finish(&engine, &observer);
    }
}
