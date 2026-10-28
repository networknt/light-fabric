//! Owned local feasibility only. No production provider/lifecycle integration.
use jsonschema::{Draft, Retrieve, Uri};
use serde_json::{Value, json};
use std::{
    io::{self, BufRead, BufReader, Read, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::Semaphore,
    time::{Instant, timeout_at},
};
use tokio_util::sync::CancellationToken;
#[path = "profile_support/openapi_request.rs"]
mod request_adapter;
const FRAME: usize = 65536; // Experimental IPC value, not a production limit.
// Parent-side feasibility admission, not production OpenAPI/profile values.
const IPC_NODES: usize = 4096;
const IPC_DEPTH: usize = 64;
fn encode_frame(value: &Value) -> io::Result<Vec<u8>> {
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "IPC profile");
    let mut pending = vec![(value, 0usize)];
    let mut nodes = 0usize;
    while let Some((value, depth)) = pending.pop() {
        nodes += 1;
        if nodes > IPC_NODES || depth > IPC_DEPTH {
            return Err(invalid());
        }
        let children = match value {
            Value::Object(v) => v.len(),
            Value::Array(v) => v.len(),
            _ => 0,
        };
        if nodes + pending.len() + children > IPC_NODES {
            return Err(invalid());
        }
        match value {
            Value::Object(v) => pending.extend(v.values().map(|v| (v, depth + 1))),
            Value::Array(v) => pending.extend(v.iter().map(|v| (v, depth + 1))),
            _ => {}
        }
    }
    struct Bounded(Vec<u8>);
    impl Write for Bounded {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            if data.len() > FRAME - self.0.len() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "IPC size"));
            }
            self.0.extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Bounded(Vec::with_capacity(FRAME));
    serde_json::to_writer(&mut writer, value).map_err(io::Error::other)?;
    Ok(writer.0)
}
struct Deny(Arc<AtomicUsize>);
impl Retrieve for Deny {
    fn retrieve(&self, _: &Uri<String>) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err("uncatalogued resource".into())
    }
}
fn read_frame(reader: &mut impl Read) -> io::Result<Value> {
    let mut header = [0; 4];
    reader.read_exact(&mut header)?;
    let len = u32::from_be_bytes(header) as usize;
    if len > FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "IPC size"));
    }
    let mut data = vec![0; len];
    reader.read_exact(&mut data)?;
    serde_json::from_slice(&data).map_err(io::Error::other)
}
fn write_frame(writer: &mut impl Write, value: &Value) -> io::Result<()> {
    let data = encode_frame(value)?;
    write_encoded(writer, &data)
}
fn write_encoded(writer: &mut impl Write, data: &[u8]) -> io::Result<()> {
    writer.write_all(&(data.len() as u32).to_be_bytes())?;
    writer.write_all(data)?;
    writer.flush()
}
fn proc_memory(pid: u32) -> String {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status"))
        .unwrap_or_default()
        .lines()
        .filter(|s| s.starts_with("VmRSS:") || s.starts_with("VmHWM:") || s.starts_with("VmSize:"))
        .collect::<Vec<_>>()
        .join("; ");
    let private = std::fs::read_to_string(format!("/proc/{pid}/smaps_rollup"))
        .unwrap_or_default()
        .lines()
        .filter(|s| {
            s.starts_with("Pss:")
                || s.starts_with("Private_Clean:")
                || s.starts_with("Private_Dirty:")
        })
        .collect::<Vec<_>>()
        .join("; ");
    format!("{status}; {private}")
}
#[cfg(target_os = "linux")]
fn address_space_limit() -> bool {
    #[repr(C)]
    struct Limit {
        soft: u64,
        hard: u64,
    }
    unsafe extern "C" {
        fn setrlimit(resource: i32, limit: *const Limit) -> i32;
    }
    // RLIMIT_AS, Linux; experimental512MiB address space, not an RSS promise.
    unsafe {
        let _ = setrlimit(4, &Limit { soft: 0, hard: 0 }); // No core dumps from owned fault probes.
        setrlimit(
            9,
            &Limit {
                soft: 512 * 1024 * 1024,
                hard: 512 * 1024 * 1024,
            },
        ) == 0
    }
}
#[cfg(not(target_os = "linux"))]
fn address_space_limit() -> bool {
    false
}

#[test]
fn child_worker() {
    if std::env::var_os("WP12_ISOLATED_CHILD").is_none() {
        return;
    }
    let limited = address_space_limit();
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    writeln!(output, "WP12_CHILD_V1").unwrap();
    output.flush().unwrap();
    let init = read_frame(&mut input).unwrap();
    if init["schema"]["x-prototype-init-hang"] == true {
        loop {
            std::thread::park_timeout(Duration::from_secs(1));
        }
    }
    if init["schema"]["x-prototype-init-die"] == true {
        std::process::exit(18);
    }
    if init["schema"]["x-prototype-init-memory"] == true {
        let denied = Vec::<u8>::new()
            .try_reserve_exact(1024 * 1024 * 1024)
            .is_err();
        write_frame(
            &mut output,
            &json!({"ready":false,"resourceExhausted":denied,"addressSpaceLimitInstalled":limited}),
        )
        .unwrap();
        return;
    }
    let retrievals = Arc::new(AtomicUsize::new(0));
    let start = std::time::Instant::now();
    let adapter = if let Some(document) = init["schema"].get("x-prototype-openapi") {
        match request_adapter::Prepared::compile(document, &init["schema"]["x-prototype-settings"])
        {
            Ok(prepared) => Some(prepared),
            Err(_) => {
                write_frame(&mut output, &json!({"ready":false,"adapterRejected":true})).unwrap();
                return;
            }
        }
    } else {
        None
    };
    // Every captured operation root is compiled before ready; an unselected
    // invalid root rejects the complete prototype catalogue, not a later call.
    let mut catalogue = Vec::new();
    if let Some(roots) = init["catalogue"].as_array() {
        for root in roots {
            match jsonschema::options()
                .with_draft(Draft::Draft202012)
                .with_retriever(Deny(retrievals.clone()))
                .build(root)
            {
                Ok(v) => catalogue.push(v),
                Err(_) => {
                    write_frame(&mut output,&json!({"ready":false,"catalogueRejected":true,"retrievals":retrievals.load(Ordering::SeqCst)})).unwrap();
                    return;
                }
            }
        }
    }
    let compiler = jsonschema::options()
        .with_draft(Draft::Draft202012)
        .with_retriever(Deny(retrievals.clone()))
        .build(&init["schema"]);
    let validator = match compiler {
        Ok(v) => v,
        Err(_) => {
            write_frame(
                &mut output,
                &json!({"ready":false,"retrievals":retrievals.load(Ordering::SeqCst)}),
            )
            .unwrap();
            return;
        }
    };
    write_frame(&mut output,&json!({"ready":true,"compiledRoots":adapter.as_ref().map_or(catalogue.len()+1,|a|a.schema_count()),"compileMicros":start.elapsed().as_micros(),"addressSpaceLimitInstalled":limited,"memory":proc_memory(std::process::id()),"retrievals":retrievals.load(Ordering::SeqCst)})).unwrap();
    while let Ok(request) = read_frame(&mut input) {
        match request["fault"].as_str() {
            Some("die") => std::process::exit(17),
            Some("memory") => {
                let denied = Vec::<u8>::new()
                    .try_reserve_exact(1024 * 1024 * 1024)
                    .is_err();
                write_frame(&mut output, &json!({"exhausted":denied,"retrievals":0})).unwrap();
                continue;
            }
            Some("hang") => loop {
                std::thread::park_timeout(Duration::from_secs(1));
            },
            Some("late") => std::thread::sleep(Duration::from_millis(200)),
            Some("malformed") => {
                output.write_all(&1u32.to_be_bytes()).unwrap();
                output.write_all(b"!").unwrap();
                output.flush().unwrap();
                continue;
            }
            Some("oversized-reply") => {
                output
                    .write_all(&((FRAME + 1) as u32).to_be_bytes())
                    .unwrap();
                output.flush().unwrap();
                continue;
            }
            _ => {}
        }
        if let Some(adapter) = &adapter {
            let reply = match adapter.validate(&request["event"]) {
                Ok(Some(error)) => {
                    json!({"valid":false,"status":error.status_code,"body":String::from_utf8(error.to_bytes().to_vec()).unwrap(),"retrievals":0})
                }
                Ok(None) => json!({"valid":true,"status":200,"retrievals":0}),
                Err(reason) => json!({"unqualified":reason}),
            };
            write_frame(&mut output, &reply).unwrap();
            continue;
        }
        let selected = request["operation"].as_u64().unwrap_or(0) as usize;
        let selected_validator = if selected == 0 {
            &validator
        } else if let Some(v) = catalogue.get(selected - 1) {
            v
        } else {
            write_frame(&mut output, &json!({"invalidOperation":true})).unwrap();
            continue;
        };
        let error = selected_validator.validate(&request["request"]).err();
        let execution_failure = error.as_ref().is_some_and(|error| {
            matches!(
                error.kind(),
                jsonschema::error::ValidationErrorKind::BacktrackLimitExceeded { .. }
                    | jsonschema::error::ValidationErrorKind::RegexEngineFailure { .. }
            )
        });
        let valid = error.is_none();
        write_frame(
            &mut output,
            &json!({"valid":valid,"executionFailure":execution_failure,"retrievals":retrievals.load(Ordering::SeqCst)}),
        )
        .unwrap();
    }
}
struct Worker {
    child: Mutex<Child>,
    io: Mutex<(ChildStdin, BufReader<ChildStdout>)>,
    dead: AtomicBool,
}
struct KillOnDrop {
    worker: Arc<Worker>,
    armed: bool,
}
struct RestartBudget(AtomicUsize);
impl RestartBudget {
    fn start(&self, captured: &Value) -> Option<(Arc<Worker>, Value)> {
        self.0
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .ok()?;
        Some(Worker::start(captured.clone()))
    }
}
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.worker.reap();
        }
    }
}
impl Worker {
    fn start(schema: Value) -> (Arc<Self>, Value) {
        Self::start_catalogue(schema, vec![])
    }
    fn start_catalogue(schema: Value, catalogue: Vec<Value>) -> (Arc<Self>, Value) {
        Self::start_cancellable(schema, catalogue, CancellationToken::new())
    }
    fn start_cancellable(
        schema: Value,
        catalogue: Vec<Value>,
        cancel: CancellationToken,
    ) -> (Arc<Self>, Value) {
        let init = encode_frame(&json!({"schema":schema,"catalogue":catalogue}));
        // Pre-encode the entire catalogue before spawning or acquiring capacity.
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "child_worker", "--nocapture"])
            .env_clear()
            .env("WP12_ISOLATED_CHILD", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        let worker = Arc::new(Self {
            child: Mutex::new(child),
            io: Mutex::new((input, output)),
            dead: AtomicBool::new(false),
        });
        let (tx, rx) = std::sync::mpsc::channel();
        let owned = worker.clone();
        let init_task = std::thread::spawn(move || {
            let result = (|| -> io::Result<Value> {
                let mut io = owned.io.lock().unwrap();
                let mut seen = 0;
                loop {
                    let mut line = Vec::new();
                    let n = io.1.by_ref().take(512).read_until(b'\n', &mut line)?;
                    seen += n;
                    if n == 0 || seen > 512 {
                        return Err(std::io::Error::other("handshake"));
                    }
                    if String::from_utf8_lossy(&line).trim() == "WP12_CHILD_V1" {
                        break;
                    }
                }
                write_encoded(&mut io.0, &init?)?;
                read_frame(&mut io.1)
            })();
            let _ = tx.send(result);
        });
        let until = std::time::Instant::now() + Duration::from_secs(2);
        let report = loop {
            if cancel.is_cancelled() || std::time::Instant::now() >= until {
                worker.reap();
                break json!({"ready":false,"initOutcome":if cancel.is_cancelled(){"cancelled"}else{"deadline"}});
            }
            match rx.recv_timeout(Duration::from_millis(5)) {
                Ok(Ok(report)) => break report,
                Ok(Err(_)) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    worker.reap();
                    break json!({"ready":false,"initOutcome":"protocol"});
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }
        };
        init_task.join().unwrap();
        (worker, report)
    }
    fn reap(&self) {
        self.dead.store(true, Ordering::SeqCst);
        let mut child = self.child.lock().unwrap();
        let _ = child.kill();
        let _ = child.wait();
    }
    async fn request(
        self: &Arc<Self>,
        capacity: Arc<Semaphore>,
        request: Value,
        deadline: Instant,
        cancel: CancellationToken,
    ) -> Result<Value, &'static str> {
        // Count the complete request tree, then serialize into one fixed-cap
        // buffer before taking a worker. The existing Value/ingress is separate.
        let encoded = encode_frame(&request).map_err(|_| "input")?;
        let permit = tokio::select! {biased;_=cancel.cancelled()=>return Err("deadline"),r=timeout_at(deadline,capacity.acquire_owned())=>r.map_err(|_|"deadline")?.map_err(|_|"capacity")?};
        if self.dead.load(Ordering::SeqCst) {
            return Err("dead");
        }
        let permit = Arc::new(permit);
        let io_permit = permit.clone();
        let mut cleanup = KillOnDrop {
            worker: self.clone(),
            armed: true,
        };
        let worker = self.clone();
        let mut task = tokio::task::spawn_blocking(move || {
            let _permit = io_permit;
            let mut io = worker.io.lock().unwrap();
            write_encoded(&mut io.0, &encoded)?;
            read_frame(&mut io.1)
        });
        let mut outcome = tokio::select! {biased;_=cancel.cancelled()=>Err("deadline"),r=tokio::time::timeout_at(deadline,&mut task)=>match r {Err(_)=>Err("deadline"),Ok(Err(_))=>Err("join"),Ok(Ok(Err(_)))=>Err("protocol"),Ok(Ok(Ok(value)))=>if value["exhausted"]==true {Err("resource")}else if value["executionFailure"]==true {Err("engine")}else if value.get("valid").is_some_and(Value::is_boolean)&&value["retrievals"]==0 {Ok(value)}else{Err("protocol")}}};
        if cancel.is_cancelled() || Instant::now() >= deadline {
            outcome = Err("deadline");
        }
        if outcome.is_err() {
            let worker = self.clone();
            tokio::task::spawn_blocking(move || worker.reap())
                .await
                .map_err(|_| "reap")?;
            // Join the blocked reader after kill/reap; only then release permit.
            if !task.is_finished() {
                let _ = task.await;
            }
        }
        cleanup.armed = false;
        drop(permit);
        outcome
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let child = self.child.get_mut().unwrap();
        let _ = child.kill();
        let _ = child.wait();
    }
}
// Owned qualification controller only; no runtime provider or lifecycle hooks.
struct RecoveryState {
    epoch: u64,
    owner: Option<u64>,
    attempts: usize,
    next: Instant,
    ready: Option<Arc<Worker>>,
    starts: usize,
    last: Instant,
}
struct RecoveryController {
    captured: Arc<(Value, Vec<Value>)>,
    state: Mutex<RecoveryState>,
    compiler: Arc<Semaphore>,
}
type PublishGate = Arc<(Mutex<bool>, std::sync::Condvar)>;
impl RecoveryController {
    fn new(captured: Arc<(Value, Vec<Value>)>, now: Instant) -> Arc<Self> {
        Arc::new(Self {
            captured,
            state: Mutex::new(RecoveryState {
                epoch: 0,
                owner: None,
                attempts: 0,
                next: now,
                ready: None,
                starts: 0,
                last: now,
            }),
            compiler: Arc::new(Semaphore::new(1)),
        })
    }
    fn begin(&self, now: Instant) -> Option<u64> {
        let mut state = self.state.lock().unwrap();
        let now = now.max(state.last);
        state.last = now;
        if state.owner.is_some()
            || state
                .ready
                .as_ref()
                .is_some_and(|w| !w.dead.load(Ordering::SeqCst))
        {
            return None;
        }
        state.ready = None;
        if state.attempts == 2 {
            if now < state.next {
                return None;
            }
            state.attempts = 0;
        }
        state.attempts += 1;
        if state.attempts == 2 {
            state.next = now + Duration::from_secs(30);
        }
        state.epoch += 1;
        let ticket = state.epoch;
        state.owner = Some(ticket);
        state.starts += 1;
        Some(ticket)
    }
    fn revoke(&self, ticket: u64) {
        let mut state = self.state.lock().unwrap();
        if state.owner == Some(ticket) {
            state.epoch += 1;
        }
    }
    fn finish(&self, ticket: u64, worker: Option<Arc<Worker>>, cancelled: bool) -> bool {
        let mut state = self.state.lock().unwrap();
        let publish =
            state.owner == Some(ticket) && state.epoch == ticket && !cancelled && worker.is_some();
        if publish {
            state.ready = worker;
        } else if let Some(worker) = worker {
            worker.reap();
        }
        // Cleanup, including kill/reap above, precedes new compiler admission.
        if state.owner == Some(ticket) {
            state.owner = None;
        }
        publish
    }
    async fn recover(
        self: &Arc<Self>,
        cancel: CancellationToken,
        gate: Option<PublishGate>,
    ) -> bool {
        struct CallerDrop {
            controller: Arc<RecoveryController>,
            cancel: CancellationToken,
            ticket: u64,
            armed: bool,
        }
        impl Drop for CallerDrop {
            fn drop(&mut self) {
                if self.armed {
                    self.cancel.cancel();
                    self.controller.revoke(self.ticket);
                }
            }
        }
        let Some(ticket) = self.begin(Instant::now()) else {
            return false;
        };
        let permit = self
            .compiler
            .clone()
            .try_acquire_owned()
            .expect("one compiler owner");
        let mut caller = CallerDrop {
            controller: self.clone(),
            cancel: cancel.clone(),
            ticket,
            armed: true,
        };
        let controller = self.clone();
        let signal = cancel.clone();
        let result = tokio::task::spawn_blocking(move || {
            struct OwnerDrop {
                controller: Arc<RecoveryController>,
                ticket: u64,
                armed: bool,
            }
            impl Drop for OwnerDrop {
                fn drop(&mut self) {
                    if self.armed {
                        self.controller.finish(self.ticket, None, true);
                    }
                }
            }
            let mut owner = OwnerDrop {
                controller: controller.clone(),
                ticket,
                armed: true,
            };
            let (worker, ready) = Worker::start_cancellable(
                controller.captured.0.clone(),
                controller.captured.1.clone(),
                signal.clone(),
            );
            if let Some(gate) = gate {
                let (lock, cv) = &*gate;
                let mut open = lock.lock().unwrap();
                while !*open && !signal.is_cancelled() {
                    open = cv.wait_timeout(open, Duration::from_millis(5)).unwrap().0;
                }
            }
            let accepted = controller.finish(
                ticket,
                if ready["ready"] == true {
                    Some(worker)
                } else {
                    worker.reap();
                    None
                },
                signal.is_cancelled(),
            );
            owner.armed = false;
            drop(permit);
            accepted
        })
        .await
        .unwrap_or(false);
        caller.armed = false;
        result
    }
}
#[test]
fn recovery_rate_claim_holds_for_adversarial_starts_and_concurrent_owners() {
    let start = Instant::now();
    let controller = RecoveryController::new(Arc::new((json!({}), vec![])), start);
    let mut admitted = 0;
    for elapsed in [
        0, 0, 0, 1, 29, 29, 30, 30, 30, 31, 59, 60, 60, 60, 61, 89, 90,
    ] {
        if let Some(ticket) = controller.begin(start + Duration::from_secs(elapsed)) {
            admitted += 1;
            controller.finish(ticket, None, false);
        }
        assert!(admitted <= 2 * (1 + elapsed / 30));
    }
    let controller = RecoveryController::new(Arc::new((json!({}), vec![])), start);
    std::thread::scope(|scope| {
        for _ in 0..64 {
            let controller = controller.clone();
            scope.spawn(move || {
                if let Some(ticket) = controller.begin(start) {
                    controller.finish(ticket, None, false);
                }
            });
        }
    });
    assert_eq!(controller.state.lock().unwrap().starts, 2);
    assert!(controller.begin(start + Duration::from_secs(29)).is_none());
    assert!(controller.begin(start + Duration::from_secs(30)).is_some());
    // A caller can capture time before waiting for the mutex. Stale observations
    // must not shorten the cooldown when that caller finally becomes owner.
    let controller = RecoveryController::new(Arc::new((json!({}), vec![])), start);
    let mut effective = 0;
    for elapsed in [0, 30, 29, 60, 30, 60, 61, 0, 90, 60, 90, 91, 1, 120] {
        effective = effective.max(elapsed);
        if let Some(ticket) = controller.begin(start + Duration::from_secs(elapsed)) {
            controller.finish(ticket, None, false);
        }
        assert!(controller.state.lock().unwrap().starts <= 2 * (1 + effective as usize / 30));
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn recovery_complete_readiness_generation_cancel_and_request_compile_overlap() {
    let capture = Arc::new((schema(), vec![json!({"type":"number","multipleOf":0.25})]));
    let controller = RecoveryController::new(capture.clone(), Instant::now());
    let (survivor, ready) = Worker::start_catalogue(capture.0.clone(), capture.1.clone());
    assert_eq!(ready["ready"], true);
    let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let owned = controller.clone();
    let paused = gate.clone();
    let compile =
        tokio::spawn(async move { owned.recover(CancellationToken::new(), Some(paused)).await });
    while controller.compiler.available_permits() != 0 {
        tokio::task::yield_now().await;
    }
    assert!(!controller.recover(CancellationToken::new(), None).await);
    assert_eq!(
        survivor
            .request(
                Arc::new(Semaphore::new(1)),
                json!({"request":complete_request()}),
                Instant::now() + Duration::from_secs(1),
                CancellationToken::new()
            )
            .await
            .unwrap()["valid"],
        true
    );
    let ticket = controller.state.lock().unwrap().owner.unwrap();
    controller.revoke(ticket);
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    assert!(!compile.await.unwrap());
    assert!(controller.state.lock().unwrap().ready.is_none());
    assert_eq!(controller.compiler.available_permits(), 1);
    assert!(controller.recover(CancellationToken::new(), None).await);
    let worker = controller.state.lock().unwrap().ready.clone().unwrap();
    assert_eq!(
        worker
            .request(
                Arc::new(Semaphore::new(1)),
                json!({"operation":1,"request":1.25}),
                Instant::now() + Duration::from_secs(1),
                CancellationToken::new()
            )
            .await
            .unwrap()["valid"],
        true
    );
    println!(
        "RECOVERY_OVERLAP {}",
        json!({"capturedRoots":2,"parent":proc_memory(std::process::id()),"survivor":proc_memory(survivor.child.lock().unwrap().id()),"replacement":proc_memory(worker.child.lock().unwrap().id()),"starts":controller.state.lock().unwrap().starts})
    );
    worker.reap();
    survivor.reap();
    let hanging = RecoveryController::new(
        Arc::new((json!({"x-prototype-init-hang":true}), vec![])),
        Instant::now(),
    );
    let cancel = CancellationToken::new();
    let signal = cancel.clone();
    let owned = hanging.clone();
    let task = tokio::spawn(async move { owned.recover(cancel, None).await });
    while hanging.compiler.available_permits() != 0 {
        tokio::task::yield_now().await;
    }
    signal.cancel();
    assert!(!task.await.unwrap());
    assert_eq!(hanging.compiler.available_permits(), 1);
    assert!(hanging.state.lock().unwrap().owner.is_none());
    let owned = hanging.clone();
    let task = tokio::spawn(async move { owned.recover(CancellationToken::new(), None).await });
    while hanging.compiler.available_permits() != 0 {
        tokio::task::yield_now().await;
    }
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(1), async {
        while hanging.compiler.available_permits() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(hanging.state.lock().unwrap().owner.is_none());
}
fn schema() -> Value {
    json!({"type":"object","required":["path","query","headers","body"],"properties":{
        "path":{"type":"object","required":["id"],"properties":{"id":{"type":"integer"}}},
        "query":{"type":"object","required":["q"],"properties":{"q":{"type":"number"}}},
        "headers":{"type":"object","required":["x-required"],"properties":{"x-required":{"type":"string","pattern":"^a(?=b)"}}},
        "body":{"$ref":"#/$defs/Node"}},"$defs":{"Node":{"type":"object","properties":{"amount":{"type":"number","multipleOf":0.25},"child":{"$ref":"#/$defs/Node"},"value":{"anyOf":[{"type":"integer"},{"type":"string"}]}}}}})
}
fn complete_request() -> Value {
    json!({"path":{"id":7},"query":{"q":1.25},"headers":{"x-required":"ab"},"body":{"amount":1.25,"child":{"value":"text"}}})
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_openapi_adapter_compiles_before_child_readiness_and_returns_exact_violation_bytes() {
    let rows: Vec<Value> =
        serde_json::from_str(include_str!("data/wp12-request-java-reproduction.json")).unwrap();
    let amendments: Value = serde_json::from_str(include_str!(
        "../../../tests/serverless-conformance/amendments/wp12-validator-dialects-v1.json"
    ))
    .unwrap();
    let capacity = Arc::new(Semaphore::new(1));
    let mut exact = 0;
    let mut changed = 0;
    for row in &rows {
        let input = &row["input"];
        let id = input["id"].as_str().unwrap();
        let (worker, ready) = Worker::start(
            json!({"x-prototype-openapi":input["files"]["openapi"],"x-prototype-settings":input["files"]["openapi-validator"]}),
        );
        assert_eq!(ready["ready"], true, "{id} {ready}");
        assert!(ready["compiledRoots"].as_u64().unwrap() > 0);
        let reply = worker
            .request(
                capacity.clone(),
                json!({"event":input["event"]}),
                Instant::now() + Duration::from_secs(2),
                CancellationToken::new(),
            )
            .await
            .unwrap_or_else(|error| panic!("{id}: {error}"));
        if let Some(amendment) = amendments["cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["observationId"] == id)
        {
            assert_eq!(reply["valid"], amendment["approvedRustSchemaValid"], "{id}");
            changed += 1;
        } else {
            assert_eq!(reply["status"], row["java"]["status"], "{id}");
            if reply["valid"] == false {
                assert_eq!(reply["body"], row["java"]["body"], "{id}");
            }
            exact += 1;
        }
        worker.reap();
    }
    println!(
        "OPENAPI_CHILD_QUALIFICATION {}",
        json!({"requestCases":rows.len(),"baselineStatusAndErrorBytes":exact,"approvedAmendmentCases":changed,"productionAdmission":false})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn complete_catalogue_and_parent_ipc_accounting() {
    let roots=(0..32).map(|i|json!({"$id":format!("urn:wp12:operation:{i}"),"type":"object","required":["body"],"properties":{"body":{"type":"number","multipleOf":0.25}}})).collect::<Vec<_>>();
    let encoded = encode_frame(&json!({"schema":schema(),"catalogue":roots})).unwrap();
    assert!(encoded.len() < FRAME);
    assert_eq!(encoded.capacity(), FRAME);
    let (worker, ready) = Worker::start_catalogue(schema(), roots);
    assert_eq!(ready["ready"], true, "{ready}");
    assert_eq!(ready["compiledRoots"], 33);
    let capacity = Arc::new(Semaphore::new(1));
    for operation in 1..=32 {
        let reply = worker
            .request(
                capacity.clone(),
                json!({"operation":operation,"request":{"body":1.25}}),
                Instant::now() + Duration::from_secs(2),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(reply["valid"], true);
    }
    let reply = worker
        .request(
            capacity.clone(),
            json!({"operation":32,"request":{"body":1.26}}),
            Instant::now() + Duration::from_secs(2),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(reply["valid"], false);
    worker.reap();
    let (bad, report) = Worker::start_catalogue(schema(), vec![json!({"type":"not-a-type"})]);
    assert_eq!(report["ready"], false);
    assert_eq!(report["catalogueRejected"], true);
    bad.reap();
    // Whole tree and encoding admission happen before capacity acquisition.
    assert!(encode_frame(&json!({"body":"x".repeat(FRAME*2)})).is_err());
    assert!(encode_frame(&json!([vec![Value::Null; IPC_NODES]])).is_err());
    let mut deep = Value::Null;
    for _ in 0..=IPC_DEPTH {
        deep = json!([deep]);
    }
    assert!(encode_frame(&deep).is_err());
    println!(
        "ISOLATION_CATALOGUE {}",
        json!({"roots":33,"encodedCatalogueBytes":encoded.len(),"fixedEncodeCapacity":encoded.capacity(),"ready":ready})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owned_bounded_recovery_recompiles_only_the_same_catalogue() {
    let captured = Arc::new(schema());
    let (first, ready) = Worker::start((*captured).clone());
    assert_eq!(ready["ready"], true, "{ready}");
    let capacity = Arc::new(Semaphore::new(1));
    let mut current = first;
    let mut observations = Vec::new();
    let restarts = Arc::new(RestartBudget(AtomicUsize::new(2)));
    // Experimental two replacement attempts per prototype Init, not a proposed
    // production guarantee. No new source reads or mutable schema cache.
    for attempt in 1..=2 {
        assert_eq!(
            current
                .request(
                    capacity.clone(),
                    json!({"fault":"die"}),
                    Instant::now() + Duration::from_secs(1),
                    CancellationToken::new()
                )
                .await,
            Err("protocol")
        );
        let held = capacity.clone().acquire_owned().await.unwrap();
        assert!(capacity.clone().try_acquire_owned().is_err());
        let immutable = captured.clone();
        let restarts = restarts.clone();
        let started = std::time::Instant::now();
        let (replacement, ready) =
            tokio::task::spawn_blocking(move || restarts.start(&immutable).unwrap())
                .await
                .unwrap();
        assert_eq!(ready["ready"], true, "{ready}");
        observations.push(json!({"attempt":attempt,"recoveryWallMicros":started.elapsed().as_micros(),"ready":ready}));
        drop(held);
        assert_eq!(
            replacement
                .request(
                    capacity.clone(),
                    json!({"request":complete_request()}),
                    Instant::now() + Duration::from_secs(1),
                    CancellationToken::new()
                )
                .await
                .unwrap()["valid"],
            true
        );
        current = replacement;
    }
    current.reap();
    assert!(restarts.start(&captured).is_none());
    // No third replacement; no unreserved ERR14033 or production mapping.
    println!(
        "ISOLATION_RECOVERY {}",
        json!({"experimentalAttemptCap":2,"newInitOnlyCompilationExceptionRequired":true,"replacements":observations})
    );
}
#[test]
fn ipc_exact_boundary_rejects_oversized_or_malformed_before_read_allocation() {
    let value = Value::String("x".repeat(FRAME - 2));
    let mut frame = Vec::new();
    write_frame(&mut frame, &value).unwrap();
    assert_eq!(frame.len(), FRAME + 4);
    assert_eq!(read_frame(&mut frame.as_slice()).unwrap(), value);
    let mut output = Vec::new();
    assert!(write_frame(&mut output, &Value::String("x".repeat(FRAME - 1))).is_err());
    assert!(output.is_empty());
    assert!(read_frame(&mut ((FRAME + 1) as u32).to_be_bytes().as_slice()).is_err());
    assert!(read_frame(&mut [0, 0, 0, 1, b'!'].as_slice()).is_err());
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn isolated_complete_request_faults_saturation_and_memory_feasibility() {
    let start = std::time::Instant::now();
    let (worker, report) = Worker::start(schema());
    assert_eq!(report["ready"], true);
    assert_eq!(report["retrievals"], 0);
    let (duplicate, duplicate_report) = Worker::start(schema());
    assert_eq!(duplicate_report["ready"], true);
    println!(
        "ISOLATION_INIT {}",
        json!({"wallMicros":start.elapsed().as_micros(),"first":report,"second":duplicate_report,"parentMemory":proc_memory(std::process::id()),"ipcBytes":FRAME,"productionAdmission":false})
    );
    let capacity = Arc::new(Semaphore::new(1));
    let call = |request| json!({"request":request});
    let deadline = || Instant::now() + Duration::from_secs(2);
    assert_eq!(
        worker
            .request(
                capacity.clone(),
                call(complete_request()),
                deadline(),
                CancellationToken::new()
            )
            .await
            .unwrap()["valid"],
        true
    );
    for field in ["path", "query", "headers", "body"] {
        let mut input = complete_request();
        input[field] = match field {
            "path" => json!({"id":"bad"}),
            "query" => json!({"q":"bad"}),
            "headers" => json!({"x-required":"xx"}),
            _ => json!({"amount":1.1}),
        };
        assert_eq!(
            worker
                .request(
                    capacity.clone(),
                    call(input),
                    deadline(),
                    CancellationToken::new()
                )
                .await
                .unwrap()["valid"],
            false,
            "{field}"
        );
    }
    assert_eq!(
        worker
            .request(
                capacity.clone(),
                json!({"request":"x".repeat(FRAME)}),
                deadline(),
                CancellationToken::new()
            )
            .await,
        Err("input")
    );
    // A blocked first request retains the only permit. A second expires waiting;
    // cancellation kills/reaps the first before its permit is released.
    let cancel = CancellationToken::new();
    let w = worker.clone();
    let c = capacity.clone();
    let token = cancel.clone();
    let task = tokio::spawn(async move {
        w.request(
            c,
            json!({"fault":"hang"}),
            Instant::now() + Duration::from_secs(5),
            token,
        )
        .await
    });
    while capacity.available_permits() != 0 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        duplicate
            .request(
                capacity.clone(),
                call(complete_request()),
                Instant::now() + Duration::from_millis(20),
                CancellationToken::new()
            )
            .await,
        Err("deadline")
    );
    cancel.cancel();
    assert_eq!(task.await.unwrap(), Err("deadline"));
    assert_eq!(capacity.available_permits(), 1);
    assert!(worker.child.lock().unwrap().try_wait().unwrap().is_some());
    assert_eq!(
        worker
            .request(
                capacity.clone(),
                call(complete_request()),
                deadline(),
                CancellationToken::new()
            )
            .await,
        Err("dead")
    );
    assert_eq!(
        duplicate
            .request(
                capacity.clone(),
                call(complete_request()),
                deadline(),
                CancellationToken::new()
            )
            .await
            .unwrap()["valid"],
        true
    );
    for fault in [
        "die",
        "malformed",
        "oversized-reply",
        "hang",
        "memory",
        "late",
    ] {
        let (w, ready) = Worker::start(schema());
        assert_eq!(ready["ready"], true);
        assert!(
            w.request(
                capacity.clone(),
                json!({"fault":fault}),
                Instant::now() + Duration::from_millis(100),
                CancellationToken::new()
            )
            .await
            .is_err()
        );
        assert!(w.child.lock().unwrap().try_wait().unwrap().is_some());
        assert_eq!(capacity.available_permits(), 1);
    }
    let (uncatalogued, denied) =
        Worker::start(json!({"$ref":"https://uncatalogued.invalid/schema"}));
    assert_eq!(denied["ready"], false);
    assert_eq!(denied["retrievals"], 1);
    uncatalogued.reap();
    let files = tempfile::tempdir().unwrap();
    let file = files.path().join("uncatalogued.json");
    std::fs::write(&file, "{\"type\":\"object\"}").unwrap();
    let (uncatalogued, denied) = Worker::start(json!({"$ref":format!("file://{}",file.display())}));
    assert_eq!(denied["ready"], false);
    assert_eq!(denied["retrievals"], 1);
    uncatalogued.reap();
    let (aborted, ready) = Worker::start(schema());
    assert_eq!(ready["ready"], true);
    let worker = aborted.clone();
    let cap = capacity.clone();
    let task = tokio::spawn(async move {
        worker
            .request(
                cap,
                json!({"fault":"hang"}),
                Instant::now() + Duration::from_secs(5),
                CancellationToken::new(),
            )
            .await
    });
    while capacity.available_permits() != 0 {
        tokio::task::yield_now().await;
    }
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    while capacity.available_permits() != 1 {
        tokio::task::yield_now().await;
    }
    assert!(aborted.child.lock().unwrap().try_wait().unwrap().is_some());
    for (key, expected) in [
        ("x-prototype-init-hang", "deadline"),
        ("x-prototype-init-die", "protocol"),
    ] {
        let (worker, report) = Worker::start(json!({key:true}));
        assert_eq!(report["ready"], false);
        assert_eq!(report["initOutcome"], expected);
        assert!(worker.child.lock().unwrap().try_wait().unwrap().is_some());
    }
    let (worker, report) = Worker::start(json!({"x-prototype-init-memory":true}));
    assert_eq!(report["ready"], false);
    assert_eq!(report["resourceExhausted"], true);
    assert_eq!(report["addressSpaceLimitInstalled"], true);
    worker.reap();
    for (name, schema, input) in [
        (
            "backtracking",
            json!({"type":"object","properties":{"body":{"type":"string","pattern":"^(a+)+(?=b)$"}}}),
            json!({"path":{},"query":{},"headers":{},"body":format!("{}c","a".repeat(64))}),
        ),
        (
            "unproductive-recursion",
            json!({"$ref":"#"}),
            complete_request(),
        ),
        (
            "large-compiled-pattern",
            json!({"type":"object","properties":{"body":{"type":"string","pattern":"a{1000000000}"}}}),
            complete_request(),
        ),
    ] {
        let start = std::time::Instant::now();
        let (worker, ready) = Worker::start(schema);
        let outcome = if ready["ready"] == true {
            format!(
                "{:?}",
                worker
                    .request(
                        capacity.clone(),
                        call(input),
                        Instant::now() + Duration::from_millis(100),
                        CancellationToken::new()
                    )
                    .await
            )
        } else {
            "InitRejected".into()
        };
        println!(
            "ISOLATION_ADVERSARIAL {}",
            json!({"case":name,"init":ready,"outcome":outcome,"elapsedMicros":start.elapsed().as_micros()})
        );
        worker.reap();
    }
    // Captured internal resources resolved at Init without retrieval. No child
    // replacement, production SDK lifecycle or deliberate environment retirement.
}
