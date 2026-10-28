//! Owned local feasibility only. No production provider/lifecycle integration.
use serde_json::{Value, json};
use std::{
    io::{self, BufRead, BufReader, Read, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::Semaphore,
    time::{Instant, timeout_at},
};
use tokio_util::sync::CancellationToken;
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

#[path = "profile_support/router_rewrite.rs"]
mod rewrite;
#[test]
fn child_worker() {
    if std::env::var_os("WP12_ROUTER_CHILD").is_none() {
        return;
    }
    let limited = address_space_limit();
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    writeln!(output, "WP12_CHILD_V1").unwrap();
    output.flush().unwrap();
    let init = read_frame(&mut input).unwrap();
    let Some(rows) = init["program"].as_array() else {
        write_frame(&mut output, &json!({"ready":false})).unwrap();
        return;
    };
    let rules = rows
        .iter()
        .map(|r| Ok((r[0].as_str().ok_or(())?, r[1].as_str().ok_or(())?)))
        .collect::<Result<Vec<_>, ()>>();
    let program = rules
        .as_ref()
        .ok()
        .and_then(|r| rewrite::Program::try_compile(r).ok());
    let Some(program) = program else {
        write_frame(&mut output, &json!({"ready":false})).unwrap();
        return;
    };
    write_frame(&mut output,&json!({"ready":true,"rules":rows.len(),"memory":proc_memory(std::process::id()),"addressSpaceLimitInstalled":limited})).unwrap();
    while let Ok(request) = read_frame(&mut input) {
        match request["fault"].as_str() {
            Some("die") => std::process::exit(17),
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
        let path = request["path"].as_str().unwrap_or("");
        let limit = request["outputLimit"]
            .as_u64()
            .unwrap_or(32768)
            .min(FRAME as u64) as usize;
        let reply = match program.rewrite_bounded(path, limit) {
            Ok(result) => json!({"valid":true,"result":result,"retrievals":0}),
            Err("output") => json!({"outputExceeded":true}),
            Err(_) => json!({"executionFailure":true}),
        };
        if write_frame(&mut output, &reply).is_err() {
            return;
        }
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
        let init = encode_frame(&json!({"program":schema,"unused":catalogue}));
        // Pre-encode the entire catalogue before spawning or acquiring capacity.
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "child_worker", "--nocapture"])
            .env_clear()
            .env("WP12_ROUTER_CHILD", "1")
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
        let report = match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(Ok(report)) => report,
            Ok(Err(_)) => {
                worker.reap();
                json!({"ready":false,"initOutcome":"protocol"})
            }
            Err(_) => {
                worker.reap();
                json!({"ready":false,"initOutcome":"deadline"})
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
        let mut outcome = tokio::select! {biased;_=cancel.cancelled()=>Err("deadline"),r=tokio::time::timeout_at(deadline,&mut task)=>match r {Err(_)=>Err("deadline"),Ok(Err(_))=>Err("join"),Ok(Ok(Err(_)))=>Err("protocol"),Ok(Ok(Ok(value)))=>if value["outputExceeded"]==true {Err("output")}else if value["exhausted"]==true {Err("resource")}else if value["executionFailure"]==true {Err("engine")}else if value.get("valid").is_some_and(Value::is_boolean)&&value["retrievals"]==0 {Ok(value)}else{Err("protocol")}}};
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

#[test]
fn framing_admits_exact_boundary_and_rejects_before_decode_allocation() {
    let value = Value::String("x".repeat(FRAME - 2));
    let mut frame = Vec::new();
    write_frame(&mut frame, &value).unwrap();
    assert_eq!(read_frame(&mut frame.as_slice()).unwrap(), value);
    assert!(encode_frame(&Value::String("x".repeat(FRAME - 1))).is_err());
    assert!(read_frame(&mut (((FRAME + 1) as u32).to_be_bytes().as_slice())).is_err());
    let program = rewrite::Program::compile(&[("/(x)", "/$1")]);
    assert_eq!(program.rewrite("/x").unwrap(), "/x");
    assert_eq!(program.rewrite_bounded("/x", 2).unwrap(), "/x");
    assert_eq!(program.rewrite_bounded("/x", 1), Err("output"));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn owned_rewrite_child_complete_readiness_faults_cancellation_and_stale_isolation() {
    let (bad, ready) = Worker::start(json!([["/x", "/ok"], ["(", "/bad"]]));
    assert_eq!(ready["ready"], false);
    bad.reap();
    let (worker, ready) =
        Worker::start(json!([["/(x)", "/group-$1"], ["/other", "/other-result"]]));
    assert_eq!(ready["ready"], true, "{ready}");
    assert_eq!(ready["rules"], 2);
    println!("ROUTER_ISOLATION_INIT {}", ready);
    let capacity = Arc::new(Semaphore::new(1));
    let request = |path| json!({"path":path});
    let reply = worker
        .request(
            capacity.clone(),
            request("/x"),
            Instant::now() + Duration::from_secs(1),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(reply["result"], "/group-x");
    assert_eq!(
        worker
            .request(
                capacity.clone(),
                json!({"path":"/x","outputLimit":7}),
                Instant::now() + Duration::from_secs(1),
                CancellationToken::new()
            )
            .await,
        Err("output")
    );
    assert!(worker.dead.load(Ordering::SeqCst));
    assert_eq!(capacity.available_permits(), 1);
    for fault in ["malformed", "oversized-reply", "die", "hang", "late"] {
        let (worker, ready) = Worker::start(json!([["/x", "/old"]]));
        assert_eq!(ready["ready"], true);
        let deadline = Instant::now() + Duration::from_millis(40);
        let outcome = worker
            .request(
                capacity.clone(),
                json!({"path":"/x","fault":fault}),
                deadline,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(
            outcome,
            Err(if ["hang", "late"].contains(&fault) {
                "deadline"
            } else {
                "protocol"
            }),
            "{fault}"
        );
        assert_eq!(capacity.available_permits(), 1);
        assert!(worker.child.lock().unwrap().try_wait().unwrap().is_some());
    }
    let (healthy, ready) = Worker::start(json!([["/x", "/fresh"]]));
    assert_eq!(ready["ready"], true);
    let held = capacity.clone().acquire_owned().await.unwrap();
    assert_eq!(
        healthy
            .request(
                capacity.clone(),
                request("/x"),
                Instant::now() + Duration::from_millis(10),
                CancellationToken::new()
            )
            .await,
        Err("deadline")
    );
    assert!(!healthy.dead.load(Ordering::SeqCst));
    drop(held);
    assert_eq!(
        healthy
            .request(
                capacity.clone(),
                request("/x"),
                Instant::now() + Duration::from_secs(1),
                CancellationToken::new()
            )
            .await
            .unwrap()["result"],
        "/fresh"
    );
    let cancel = CancellationToken::new();
    let signal = cancel.clone();
    let owned = healthy.clone();
    let permits = capacity.clone();
    let task = tokio::spawn(async move {
        owned
            .request(
                permits,
                json!({"path":"/x","fault":"hang"}),
                Instant::now() + Duration::from_secs(1),
                cancel,
            )
            .await
    });
    while capacity.available_permits() != 0 {
        tokio::task::yield_now().await;
    }
    signal.cancel();
    assert_eq!(task.await.unwrap(), Err("deadline"));
    assert_eq!(capacity.available_permits(), 1);
    let (aborted, ready) = Worker::start(json!([["/x", "/never-publish"]]));
    assert_eq!(ready["ready"], true);
    let owned = aborted.clone();
    let permits = capacity.clone();
    let task = tokio::spawn(async move {
        owned
            .request(
                permits,
                json!({"path":"/x","fault":"late"}),
                Instant::now() + Duration::from_secs(1),
                CancellationToken::new(),
            )
            .await
    });
    while capacity.available_permits() != 0 {
        tokio::task::yield_now().await;
    }
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    tokio::time::timeout(Duration::from_secs(1), async {
        while capacity.available_permits() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(aborted.dead.load(Ordering::SeqCst));
    let (engine, ready) = Worker::start(json!([["(a+)+(?=b)", "/never"]]));
    assert_eq!(ready["ready"], true);
    let outcome = engine
        .request(
            capacity,
            json!({"path":format!("{}c","a".repeat(32))}),
            Instant::now() + Duration::from_secs(2),
            CancellationToken::new(),
        )
        .await;
    assert_eq!(outcome, Err("engine"));
}
