// Test-only recorder: acceptance, headers, and bodies are independent facts.
// Every accept is visible before a worker starts reading, including unidentified
// or incomplete traffic. Worker errors/panics remain in the final report.
#[derive(Clone, Debug)]
struct HttpAclHeaderReceipt {
    connection: usize,
    request_id: Option<String>,
    headers: String,
}

#[derive(Clone, Debug)]
struct HttpAclBodyReceipt {
    connection: usize,
    body: Vec<u8>,
}

#[derive(Clone, Debug, Default)]
struct HttpAclObservations {
    accepted: Vec<usize>,
    headers: Vec<HttpAclHeaderReceipt>,
    bodies: Vec<HttpAclBodyReceipt>,
    failures: Vec<String>,
}

struct HttpAclRecorder {
    address: std::net::SocketAddr,
    observations: tokio::sync::watch::Receiver<HttpAclObservations>,
    stop: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
    shutdown_bound: std::time::Duration,
}

fn http_acl_record_worker_result(
    observations: &tokio::sync::watch::Sender<HttpAclObservations>,
    result: Result<Result<(), String>, tokio::task::JoinError>,
) {
    let failure = match result {
        Ok(Ok(())) => return,
        Ok(Err(error)) => error,
        Err(error) => format!("recorder worker panic/cancellation: {error}"),
    };
    observations.send_modify(|report| report.failures.push(failure));
}

fn http_acl_accept_worker(
    socket: tokio::net::TcpStream,
    observations: &tokio::sync::watch::Sender<HttpAclObservations>,
    workers: &mut tokio::task::JoinSet<Result<(), String>>,
    response_body: &str,
    io_bound: std::time::Duration,
) {
    let mut connection = 0;
    observations.send_modify(|report| {
        connection = report.accepted.len();
        report.accepted.push(connection);
    });
    let observations = observations.clone();
    let response_body = response_body.to_owned();
    workers.spawn(async move {
        http_acl_record_connection(socket, connection, &observations, &response_body, io_bound)
            .await
            .map_err(|error| format!("connection {connection}: {error}"))
    });
}

async fn http_acl_record_connection(
    mut socket: tokio::net::TcpStream,
    connection: usize,
    observations: &tokio::sync::watch::Sender<HttpAclObservations>,
    response_body: &str,
    io_bound: std::time::Duration,
) -> Result<(), String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut bytes = Vec::new();
    let header_end = tokio::time::timeout(io_bound, async {
        loop {
            if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                return Ok(end + 4);
            }
            if bytes.len() >= 64 * 1024 {
                return Err("header size limit exceeded".to_owned());
            }
            let mut chunk = [0u8; 8192];
            let count = socket
                .read(&mut chunk)
                .await
                .map_err(|e| format!("header read: {e}"))?;
            if count == 0 {
                return Err(format!(
                    "EOF before complete headers ({} bytes)",
                    bytes.len()
                ));
            }
            bytes.extend_from_slice(&chunk[..count]);
        }
    })
    .await
    .map_err(|_| "timeout before complete headers".to_owned())??;
    let headers = String::from_utf8(bytes[..header_end].to_vec())
        .map_err(|e| format!("invalid header bytes: {e}"))?;
    let request_id = headers.lines().skip(1).find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("x-acl-test-request")
            .then(|| value.trim().to_owned())
    });
    // Publish before parsing framing or attempting to read any remaining body.
    observations.send_modify(|report| {
        report.headers.push(HttpAclHeaderReceipt {
            connection,
            request_id,
            headers: headers.clone(),
        })
    });
    let mut length = None;
    let mut chunked = false;
    for line in headers.lines().skip(1).filter(|line| !line.is_empty()) {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| "malformed header".to_owned())?;
        if name.eq_ignore_ascii_case("transfer-encoding") {
            if chunked || !value.trim().eq_ignore_ascii_case("chunked") {
                return Err("invalid or duplicate Transfer-Encoding".into());
            }
            chunked = true;
        }
        if name.eq_ignore_ascii_case("content-length") {
            let value = value
                .trim()
                .parse::<usize>()
                .map_err(|e| format!("invalid Content-Length: {e}"))?;
            if length.replace(value).is_some() {
                return Err("duplicate Content-Length".into());
            }
        }
    }
    if chunked && length.is_some() {
        return Err("conflicting upstream framing".into());
    }
    if chunked {
        let body = tokio::time::timeout(io_bound, async {
            loop {
                if let Some(body) = http_acl_decode_chunks(&bytes[header_end..])? {
                    return Ok::<_, String>(body);
                }
                if bytes.len() > 2 * 1024 * 1024 {
                    return Err("chunked wire size limit exceeded".into());
                }
                let mut chunk = [0u8; 8192];
                let count = socket.read(&mut chunk).await.map_err(|e| e.to_string())?;
                if count == 0 {
                    return Err("EOF before complete chunked body".into());
                }
                bytes.extend_from_slice(&chunk[..count]);
            }
        })
        .await
        .map_err(|_| "chunked body deadline exceeded".to_owned())??;
        bytes.truncate(header_end);
        bytes.extend_from_slice(&body);
        length = Some(body.len());
    }
    let length = length.unwrap_or(0);
    if length > 1024 * 1024 {
        return Err("body size limit exceeded".into());
    }
    tokio::time::timeout(io_bound, async {
        while bytes.len() - header_end < length {
            let mut chunk = [0u8; 8192];
            let count = socket
                .read(&mut chunk)
                .await
                .map_err(|e| format!("body read: {e}"))?;
            if count == 0 {
                return Err(format!(
                    "EOF before complete body: received {}, expected {length}",
                    bytes.len() - header_end
                ));
            }
            bytes.extend_from_slice(&chunk[..count]);
        }
        Ok::<_, String>(())
    })
    .await
    .map_err(|_| {
        format!(
            "timeout before complete body: received {}, expected {length}",
            bytes.len() - header_end
        )
    })??;
    if bytes.len() - header_end != length {
        return Err("unexpected bytes beyond declared body".into());
    }
    observations.send_modify(|report| {
        report.bodies.push(HttpAclBodyReceipt {
            connection,
            body: bytes[header_end..].to_vec(),
        })
    });
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
        response_body.len()
    );
    tokio::time::timeout(io_bound, async {
        socket
            .write_all(response.as_bytes())
            .await
            .map_err(|e| format!("response write: {e}"))?;
        socket
            .shutdown()
            .await
            .map_err(|e| format!("response shutdown: {e}"))
    })
    .await
    .map_err(|_| "response write/shutdown timeout".to_owned())??;
    Ok(())
}

fn http_acl_decode_chunks(wire: &[u8]) -> Result<Option<Vec<u8>>, String> {
    let mut offset = 0;
    let mut body = Vec::new();
    loop {
        let Some(end) = wire[offset..].windows(2).position(|w| w == b"\r\n") else {
            return Ok(None);
        };
        let size = std::str::from_utf8(&wire[offset..offset + end]).map_err(|e| e.to_string())?;
        if size.is_empty() || !size.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("invalid chunk size".into());
        }
        let size = usize::from_str_radix(size, 16).map_err(|e| e.to_string())?;
        if size > 1024 * 1024 - body.len() {
            return Err("chunked body size limit exceeded".into());
        }
        offset += end + 2;
        if wire.len() - offset < size + 2 {
            return Ok(None);
        }
        if &wire[offset + size..offset + size + 2] != b"\r\n" {
            return Err("invalid chunk delimiter".into());
        }
        body.extend_from_slice(&wire[offset..offset + size]);
        offset += size + 2;
        if size == 0 {
            if offset != wire.len() {
                return Err("unexpected bytes after chunk terminator".into());
            }
            return Ok(Some(body));
        }
    }
}

impl HttpAclRecorder {
    async fn start(response_body: &str, io_bound: std::time::Duration) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("recorder bind");
        let address = listener.local_addr().unwrap();
        let (observations, receiver) = tokio::sync::watch::channel(HttpAclObservations::default());
        let (stop, mut stopped) = tokio::sync::oneshot::channel();
        let response_body = response_body.to_owned();
        let task = tokio::spawn(async move {
            let mut workers = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    _ = &mut stopped => break,
                    accepted = listener.accept() => {
                        let (socket, _) = accepted.expect("recorder accept");
                        http_acl_accept_worker(socket, &observations, &mut workers, &response_body, io_bound);
                    }
                    result = workers.join_next(), if !workers.is_empty() => {
                        http_acl_record_worker_result(&observations, result.unwrap());
                    }
                }
            }
            // Caller stops the request producer first. Drain every accept that
            // is already queued; do not cancel workers or use a grace sleep.
            // Tokio readiness can be pending while the kernel queue is nonempty.
            // Producers are quiescent before finish. Drain the nonblocking OS
            // listener until WouldBlock, which is an actual queue observation.
            let listener = listener.into_std().expect("recorder OS listener");
            loop {
                let socket = match listener.accept() {
                    Ok((socket, _)) => socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) => panic!("recorder drain accept: {error}"),
                };
                socket
                    .set_nonblocking(true)
                    .expect("recorder nonblocking accepted socket");
                http_acl_accept_worker(
                    tokio::net::TcpStream::from_std(socket).expect("recorder accepted socket"),
                    &observations,
                    &mut workers,
                    &response_body,
                    io_bound,
                );
            }
            drop(listener);
            while let Some(result) = workers.join_next().await {
                http_acl_record_worker_result(&observations, result);
            }
        });
        Self {
            address,
            observations: receiver,
            stop,
            task,
            // Each worker has separate header, body, and response deadlines.
            shutdown_bound: io_bound.saturating_mul(3) + std::time::Duration::from_secs(2),
        }
    }

    async fn finish(self) -> HttpAclObservations {
        let Self {
            observations,
            stop,
            mut task,
            shutdown_bound,
            ..
        } = self;
        // A closed stop channel does not hide an earlier listener/task panic:
        // always await the JoinHandle and inspect its result.
        let _ = stop.send(());
        match tokio::time::timeout(shutdown_bound, &mut task).await {
            Ok(result) => result.expect("recorder task panic/cancellation"),
            Err(_) => {
                let report = observations.borrow().clone();
                task.abort();
                let result = task.await;
                panic!(
                    "recorder shutdown timed out; cancellation is a test failure: {result:?}; observations before cancellation: {report:?}"
                );
            }
        }
        observations.borrow().clone()
    }
}

fn http_acl_no_denied_dispatch(
    report: &HttpAclObservations,
    allowed: &[&str],
    denied: &[&str],
) -> Result<(), String> {
    for header in &report.headers {
        let id = header
            .request_id
            .as_deref()
            .ok_or_else(|| format!("unidentified headers on connection {}", header.connection))?;
        if denied.contains(&id) {
            return Err(format!("denied request {id} sent upstream headers"));
        }
        if !allowed.contains(&id) {
            return Err(format!("unexpected upstream request {id}"));
        }
    }
    if !report.failures.is_empty() {
        return Err(format!("recorder failed: {:?}", report.failures));
    }
    // Response Connection: close forces one upstream connection per permitted
    // operation in this fixture. Extra/unidentified connections cannot disappear
    // merely because their headers or bodies never completed.
    if report.accepted.len() != allowed.len()
        || report.headers.len() != allowed.len()
        || report.bodies.len() != allowed.len()
    {
        return Err(format!(
            "unexpected upstream stages: accepted {}, headers {}, bodies {}, expected {}",
            report.accepted.len(),
            report.headers.len(),
            report.bodies.len(),
            allowed.len()
        ));
    }
    for id in allowed {
        if report
            .headers
            .iter()
            .filter(|header| header.request_id.as_deref() == Some(id))
            .count()
            != 1
        {
            return Err(format!("missing/duplicate permitted upstream request {id}"));
        }
    }
    for connection in &report.accepted {
        if report
            .headers
            .iter()
            .filter(|h| h.connection == *connection)
            .count()
            != 1
            || report
                .bodies
                .iter()
                .filter(|b| b.connection == *connection)
                .count()
                != 1
        {
            return Err(format!(
                "incomplete/duplicate stages on connection {connection}"
            ));
        }
    }
    Ok(())
}

async fn http_acl_control_socket(
    address: std::net::SocketAddr,
    headers: &[u8],
) -> tokio::net::TcpStream {
    use tokio::io::AsyncWriteExt;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
        socket.write_all(headers).await.unwrap();
        socket
    })
    .await
    .expect("control connect/write timeout")
}

async fn http_acl_control_eof(socket: &mut tokio::net::TcpStream) {
    use tokio::io::AsyncWriteExt;
    tokio::time::timeout(std::time::Duration::from_secs(2), socket.shutdown())
        .await
        .expect("control half-close timeout")
        .unwrap();
}

#[tokio::test]
async fn http_acl_recorder_headers_only_negative_control() {
    let mut recorder = HttpAclRecorder::start("{}", std::time::Duration::from_secs(1)).await;
    let mut socket = http_acl_control_socket(recorder.address, b"POST /negative HTTP/1.1\r\nHost: localhost\r\nX-Acl-Test-Request: denied-control\r\nContent-Length: 2\r\n\r\n").await;
    // Synchronize on header receipt while the connection is still open and its
    // declared body has not arrived. No sleep and no complete-request count.
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        recorder.observations.wait_for(|r| !r.headers.is_empty()),
    )
    .await
    .unwrap()
    .unwrap();
    let partial = recorder.observations.borrow().clone();
    assert_eq!(partial.accepted.len(), 1);
    assert_eq!(partial.headers.len(), 1);
    assert!(partial.bodies.is_empty());
    assert!(partial.failures.is_empty());
    assert_eq!(
        http_acl_no_denied_dispatch(&partial, &[], &["denied-control"]).unwrap_err(),
        "denied request denied-control sent upstream headers"
    );
    http_acl_control_eof(&mut socket).await;
    let final_report = recorder.finish().await;
    assert_eq!(final_report.headers.len(), 1);
    assert!(final_report.bodies.is_empty());
    assert_eq!(final_report.failures.len(), 1);
    assert!(
        final_report.failures[0].contains("EOF before complete body"),
        "{final_report:?}"
    );
    println!(
        "Headers-only negative control: accepted 1, headers 1, bodies 0; denied-header oracle rejected before EOF; incomplete-body failure propagated"
    );
}

#[tokio::test]
async fn http_acl_recorder_reports_incomplete_headers_and_body_timeout() {
    let recorder = HttpAclRecorder::start("{}", std::time::Duration::from_millis(200)).await;
    let mut partial_header =
        http_acl_control_socket(recorder.address, b"GET /partial HTTP/1.1\r\n").await;
    http_acl_control_eof(&mut partial_header).await;
    let missing_body = http_acl_control_socket(recorder.address, b"POST /timeout HTTP/1.1\r\nX-Acl-Test-Request: timeout-control\r\nContent-Length: 2\r\n\r\n").await;
    let report = recorder.finish().await;
    assert_eq!(report.accepted.len(), 2);
    assert_eq!(report.headers.len(), 1);
    assert!(report.bodies.is_empty());
    assert_eq!(report.failures.len(), 2);
    assert!(
        report
            .failures
            .iter()
            .any(|e| e.contains("EOF before complete headers")),
        "{report:?}"
    );
    assert!(
        report
            .failures
            .iter()
            .any(|e| e.contains("timeout before complete body")),
        "{report:?}"
    );
    assert!(http_acl_no_denied_dispatch(&report, &[], &[]).is_err());
    drop(missing_body);
}

#[tokio::test]
async fn http_acl_recorder_propagates_worker_panics_and_rejects_unidentified_connections() {
    let (sender, receiver) = tokio::sync::watch::channel(HttpAclObservations::default());
    let mut workers = tokio::task::JoinSet::new();
    workers.spawn(async {
        panic!("synthetic recorder panic");
        #[allow(unreachable_code)]
        Ok::<(), String>(())
    });
    let result = tokio::time::timeout(std::time::Duration::from_secs(2), workers.join_next())
        .await
        .unwrap()
        .unwrap();
    http_acl_record_worker_result(&sender, result);
    let report = receiver.borrow().clone();
    assert_eq!(report.failures.len(), 1);
    assert!(report.failures[0].contains("recorder worker panic/cancellation"));
    assert!(http_acl_no_denied_dispatch(&report, &[], &[]).is_err());
    let unidentified = HttpAclObservations {
        accepted: vec![0],
        ..Default::default()
    };
    assert!(
        http_acl_no_denied_dispatch(&unidentified, &[], &[])
            .unwrap_err()
            .contains("unexpected upstream stages")
    );
}

#[tokio::test]
async fn http_acl_recorder_finish_propagates_task_panic() {
    let (_sender, observations) = tokio::sync::watch::channel(HttpAclObservations::default());
    let (stop, stopped) = tokio::sync::oneshot::channel();
    drop(stopped);
    let recorder = HttpAclRecorder {
        address: "127.0.0.1:0".parse().unwrap(),
        observations,
        stop,
        task: tokio::spawn(async { panic!("synthetic listener task panic") }),
        shutdown_bound: std::time::Duration::from_secs(2),
    };
    // The parent checks that finish itself fails on a task panic, including a
    // closed stop channel. It cannot turn cancellation into a successful report.
    let parent = tokio::spawn(async move { recorder.finish().await });
    let error = tokio::time::timeout(std::time::Duration::from_secs(7), parent)
        .await
        .unwrap()
        .unwrap_err();
    assert!(error.is_panic(), "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wildcard_http_acl_uses_registered_policies_before_rewrites() {
    http_acl_jwt_fixture(false, false, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_acl_review_tokenize_acl_detokenize_cached_composition() {
    http_acl_jwt_fixture(true, false, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_acl_review_tokenize_without_acl_framing() {
    http_acl_jwt_fixture(true, true, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_acl_review_tokenize_without_acl_h2_framing() {
    http_acl_jwt_fixture(true, true, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_acl_review_tokenize_with_acl_h2_framing() {
    http_acl_jwt_fixture(true, false, true).await;
}

struct HttpAclCachedPiiApp {
    standalone: bool,
    pii: std::sync::Mutex<Option<light_pingora::PiiTokenizationRuntime>>,
    budgets: Arc<std::sync::Mutex<Option<(Arc<AtomicUsize>, Arc<AtomicUsize>)>>>,
}

type HttpAclH2Receipt = (String, http::HeaderMap, Vec<u8>);

async fn http_acl_h2_body_backend(
    pki: &Phase3TestPki,
) -> (
    std::net::SocketAddr,
    tokio::sync::oneshot::Sender<()>,
    tokio::task::JoinHandle<Vec<HttpAclH2Receipt>>,
) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(pki.private_key_der.clone()));
    let mut tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![pki.certificate.clone()], key)
        .unwrap();
    tls.alpn_protocols = vec![b"h2".to_vec()];
    let acceptor = TlsAcceptor::from(Arc::new(tls));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stop, mut stopped) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let mut workers = Vec::new();
        let mut accepted = 0;
        let complete_headers = Arc::new(AtomicUsize::new(0));
        let complete_bodies = Arc::new(AtomicUsize::new(0));
        loop {
            tokio::select! {
                _ = &mut stopped => break,
                socket = listener.accept() => {
                    let (socket, _) = socket.unwrap();
                    accepted += 1;
                    let acceptor = acceptor.clone();
                    let complete_headers = Arc::clone(&complete_headers);
                    let complete_bodies = Arc::clone(&complete_bodies);
                    workers.push(tokio::spawn(async move {
                        tokio::time::timeout(std::time::Duration::from_secs(10), async move {
                            let tls = acceptor.accept(socket).await.unwrap();
                            assert_eq!(tls.get_ref().1.alpn_protocol(), Some(b"h2".as_slice()));
                            let mut connection = h2::server::handshake(tls).await.unwrap();
                            let mut streams = Vec::new();
                            while let Some(request) = connection.accept().await {
                                let (request, mut respond) = request.unwrap();
                                complete_headers.fetch_add(1, Ordering::AcqRel);
                                assert_eq!(request.version(), http::Version::HTTP_2);
                                let id = request.headers().get("x-acl-test-request").unwrap().to_str().unwrap().to_owned();
                                let headers = request.headers().clone();
                                let complete_bodies = Arc::clone(&complete_bodies);
                                streams.push(tokio::spawn(async move {
                                    let mut incoming = request.into_body();
                                    let mut body = Vec::new();
                                    while let Some(chunk) = incoming.data().await {
                                        let chunk = chunk.unwrap();
                                        assert!(body.len() + chunk.len() <= 1024 * 1024);
                                        body.extend_from_slice(&chunk);
                                        incoming.flow_control().release_capacity(chunk.len()).unwrap();
                                    }
                                    if let Some(length) = headers.get("content-length") {
                                        assert_eq!(length.to_str().unwrap().parse::<usize>().unwrap(), body.len());
                                    }
                                    complete_bodies.fetch_add(1, Ordering::AcqRel);
                                    let response = http::Response::builder().status(200).header("content-type", "application/json").body(()).unwrap();
                                    let mut output = respond.send_response(response, false).unwrap();
                                    output.send_data(Bytes::from_static(b"{}"), true).unwrap();
                                    (id, headers, body)
                                }));
                            }
                            let mut receipts = Vec::new();
                            for stream in streams { receipts.push(stream.await.unwrap()); }
                            receipts
                        }).await.expect("H2 recorder connection/body deadline")
                    }));
                }
            }
        }
        // Producer has shut down. Check the OS queue, rather than interpreting
        // an unready async accept as an empty queue or waiting a grace sleep.
        let listener = listener.into_std().unwrap();
        match listener.accept() {
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Ok(_) => panic!("unexpected queued H2 connection after producer shutdown"),
            Err(error) => panic!("H2 recorder drain: {error}"),
        }
        let mut receipts = Vec::new();
        for worker in workers {
            receipts.extend(worker.await.unwrap());
        }
        assert!(accepted > 0);
        assert_eq!(complete_headers.load(Ordering::Acquire), receipts.len());
        assert_eq!(complete_bodies.load(Ordering::Acquire), receipts.len());
        assert!(accepted <= complete_headers.load(Ordering::Acquire));
        let ids = receipts
            .iter()
            .map(|r| &r.0)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(ids.len(), receipts.len(), "duplicate H2 dispatch");
        println!(
            "H2 recorder: {accepted} accepted connections; {} complete headers and bodies",
            receipts.len()
        );
        receipts
    });
    (address, stop, task)
}
impl PingoraApp for HttpAclCachedPiiApp {
    type Proxy = GatewayProxy;
    fn proxy(
        &self,
        config: &RuntimeConfig,
        lifecycle: &LifecycleRegistrar,
        admission: &AdmissionGate,
    ) -> Result<GatewayProxy, RuntimeError> {
        let proxy = GatewayApp::default().proxy(config, lifecycle, admission)?;
        *self.budgets.lock().unwrap() = Some((
            Arc::clone(&proxy.acl_body_bytes),
            Arc::clone(&proxy.hmac_body_bytes),
        ));
        if let Some(pii) = self.pii.lock().unwrap().take() {
            let mut handlers = proxy.active_handlers.load().config().clone();
            handlers
                .handlers
                .extend(["tokenize".into(), "detokenize".into()]);
            for chain in handlers.chains.values_mut() {
                let position = chain
                    .exec
                    .iter()
                    .position(|id| id == "access-control")
                    .unwrap();
                chain.exec.insert(position, "tokenize".into());
                chain.exec.insert(position + 2, "detokenize".into());
            }
            let position = handlers
                .default_handlers
                .iter()
                .position(|id| id == "access-control")
                .unwrap();
            handlers
                .default_handlers
                .insert(position, "tokenize".into());
            handlers
                .default_handlers
                .insert(position + 2, "detokenize".into());
            if self.standalone {
                for chain in handlers.chains.values_mut() {
                    chain.exec.retain(|id| id != "access-control");
                }
                handlers
                    .default_handlers
                    .retain(|id| id != "access-control");
                let route: light_pingora::HandlerPath = serde_yaml::from_str("path: /strict/private\nmethod: GET\nexec: [security, header, access-control, proxy]\n").unwrap();
                handlers.paths.insert(0, route);
            }
            let active = gateway_handler_registry().build_active_handlers(config, handlers)?;
            validate_hmac_effective_chains(&active, None, None)?;
            let mut execution = (*proxy.security_execution.load()).clone();
            execution.active_handlers = Arc::new(active.clone());
            proxy.security_execution.store(execution);
            proxy.active_handlers.store(active);
            proxy.pii_tokenization.store(Some(pii));
        }
        Ok(proxy)
    }
}

async fn http_acl_jwt_fixture(pii: bool, standalone: bool, h2: bool) {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use hmac::{Hmac, Mac};
    let key = b"workflow-entry-test-signing-key-32-bytes";
    let sign = |claims: &serde_json::Value| {
        let header =
            URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","kid":"workflow-entry","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).unwrap());
        let input = format!("{header}.{payload}");
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(key).unwrap();
        mac.update(input.as_bytes());
        format!(
            "{input}.{}",
            URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
        )
    };
    let jwks_body = json!({"keys":[{"kty":"oct","kid":"workflow-entry","alg":"HS256",
        "k":URL_SAFE_NO_PAD.encode(key)}]})
    .to_string();
    let jwks = HttpAclRecorder::start(&jwks_body, std::time::Duration::from_secs(2)).await;
    let jwks_addr = jwks.address;
    let backend = HttpAclRecorder::start(
        if pii {
            r#"{"id":725,"email":"00000000-0000-0000-0000-000000000725","secret":"redacted"}"#
        } else {
            r#"{"id":725,"secret":"redacted"}"#
        },
        std::time::Duration::from_secs(2),
    )
    .await;
    let pki = phase3_test_pki();
    let h2_backend = if h2 {
        Some(http_acl_h2_body_backend(&pki).await)
    } else {
        None
    };
    let backend_addr = h2_backend.as_ref().map_or(backend.address, |b| b.0);
    let config_dir = TempDir::new().unwrap();
    let external_dir = TempDir::new().unwrap();
    let port = free_tcp_port();
    let address = format!("127.0.0.1:{port}").parse().unwrap();
    let write =
        |name: &str, value: String| std::fs::write(config_dir.path().join(name), value).unwrap();
    write(
        "server.yml",
        format!(
            "ip: 127.0.0.1\nhttpPort: {port}\nhttpsPort: 8443\nadvertisedAddress: 127.0.0.1\ndynamicPort: false\nstartOnRegistryFailure: true\nenvironment: dev\nenableHttp: true\nenableHttps: false\nenableRegistry: false\nserviceId: gateway-test\nshutdownGracefulPeriod: 100\n"
        ),
    );
    write("handler.yml", "handlers: [security, header, access-control, proxy]\nchains:\n  shared: [security, header, access-control, proxy]\npaths:\n  - path: /github/repos/*\n    method: GET\n    exec: [shared]\n  - path: /exact\n    method: GET\n    exec: [shared]\ndefaultHandlers: [security, header, access-control, proxy]\n".into());
    write(
        "security.yml",
        "enableVerifyJwt: true\nbootstrapFromKeyService: true\n".into(),
    );
    write(
        "client.yml",
        format!(
            "oauth:\n  token:\n    key:\n      server_url: http://{jwks_addr}\n      uri: /keys\n"
        ),
    );
    write("header.yml", "enabled: true\nrequest:\n  update: {x-upstream-test: rewritten}\n  remove: [authorization]\n".into());
    write(
        "proxy.yml",
        format!(
            "hosts: {}://{backend_addr}/rewritten\nhttp2Enabled: {h2}\n",
            if h2 { "https" } else { "http" }
        ),
    );
    if h2 {
        let ca = config_dir.path().join("fixture-ca.pem");
        std::fs::write(&ca, &pki.ca_pem).unwrap();
        let client_path = config_dir.path().join("client.yml");
        let mut contents = std::fs::read_to_string(&client_path).unwrap();
        contents.push_str(&format!(
            "tls:\n  verifyHostname: true\n  caCertPath: {}\n",
            ca.display()
        ));
        std::fs::write(client_path, contents).unwrap();
    }
    write(
        "access-control.yml",
        "enabled: true\ndefaultDeny: true\nbodyReadTimeoutMillis: 200\nmaxBufferedBodyBytes: 1024\n".into(),
    );
    write("rule.yml", r#"ruleBodies:
  role:
    ruleId: role
    ruleName: Verified role
    ruleType: req-acc
    common: Y
    conditionLanguage: cel
    conditionSecurityProfile: strict
    expression: "'role' in auditInfo.subject_claims.ClaimsMap && auditInfo.subject_claims.ClaimsMap.role == permission.roles"
  columns:
    ruleId: columns
    ruleName: Remove secret
    ruleType: res-fil
    common: Y
    actions:
      - actionClassName: com.networknt.rule.ResponseColumnFilterAction
endpointRules:
  /github/repos/{owner}/{repo}/issues/{issue_number}@get:
    permission:
      roles: admin
      col: {role: {admin: '["id"]'}}
    req-acc: [role]
    res-fil: [columns]
  /github/repos/{owner}/{repo}/issues/{issue_number}/comments@get:
    permission: {roles: commenter}
    req-acc: [role]
  /github/repos/{owner}/{repo}/issues/{issue_number}/labels@post:
    permission: {roles: admin}
    req-acc: [role]
  /github/repos/{owner}/{repo}/issues/{issue_number}/labels@delete:
    permission: {roles: admin}
    req-acc: [role]
  /exact@get:
    permission: {roles: admin}
    req-acc: [role]
  /strict/private@get:
    permission: {roles: admin}
    req-acc: [role]
  /github/repos@get:
    permission: {roles: viewer}
    req-acc: [role]
  /github/repos/{owner}/{repo}/labels/{label}@get:
    permission: {roles: admin}
    req-acc: [role]
  /github/repos/{owner}/{repo}/contents/{directory}/{file}@get:
    permission: {roles: admin}
    req-acc: [role]
"#.into());
    let cached = if pii {
        let config: light_pingora::PiiTokenizationConfig = serde_yaml::from_str(
            r#"
maxBodySize: 256
crypto:
  valueEncryptionKey: AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=
  valueHashKey: AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=
rules:
  - pathPrefix: /github
    methods: [POST, DELETE, GET]
    request: [{path: '$.email', scheme: UUID}]
    response: [{path: '$.email', scheme: UUID}]
"#,
        )
        .unwrap();
        Some(
            light_pingora::PiiTokenizationRuntime::cached_test_fixture(
                config,
                uuid::Uuid::nil(),
                &[
                    (
                        light_pingora::TokenScheme::Uuid,
                        "fixture@example.com",
                        "00000000-0000-0000-0000-000000000725",
                    ),
                    (
                        light_pingora::TokenScheme::Uuid,
                        "oversize@example.com",
                        &"x".repeat(300),
                    ),
                ],
            )
            .await
            .unwrap(),
        )
    } else {
        None
    };
    if pii {
        let rule_path = config_dir.path().join("rule.yml");
        let rule = std::fs::read_to_string(&rule_path).unwrap().replace("auditInfo.subject_claims.ClaimsMap.role == permission.roles", "auditInfo.subject_claims.ClaimsMap.role == permission.roles && (!('email' in toolArguments) || toolArguments.email == '00000000-0000-0000-0000-000000000725')");
        std::fs::write(rule_path, rule).unwrap();
    }
    let budgets = Arc::new(std::sync::Mutex::new(None));
    let runtime = LightRuntimeBuilder::new(PingoraTransport::new(HttpAclCachedPiiApp {
        standalone,
        pii: std::sync::Mutex::new(cached),
        budgets: Arc::clone(&budgets),
    }))
    .with_config_dir(config_dir.path())
    .with_external_config_dir(external_dir.path())
    .build();
    let running = tokio::time::timeout(std::time::Duration::from_secs(10), runtime.start())
        .await
        .unwrap()
        .unwrap();
    wait_for_tcp(address).await;
    let now = chrono::Utc::now().timestamp();
    let token = |role| {
        sign(
            &json!({"iss":"workflow-entry","aud":"workflow","exp":now+300,
        "token_use":"user","uid":"synthetic-user","sub":"synthetic-user","role":role,"host_id":"00000000-0000-0000-0000-000000000000"}),
        )
    };
    let admin = token("admin");
    let commenter = token("commenter");
    let viewer = token("viewer");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();
    let issue = "/github/repos/lightapi/light-portal/issues/725";
    let comments = format!("{issue}/comments");
    let call = |method: http::Method, path: &str, token: Option<&str>, id: &str| {
        let mut request = client
            .request(method, format!("http://{address}{path}"))
            .header("x-acl-test-request", id);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        request
    };
    if standalone || h2 {
        let response = call(
            http::Method::POST,
            &format!("{issue}/labels"),
            Some(&admin),
            "standalone-tokenized",
        )
        .body(r#"{ "email": "fixture@example.com" }"#)
        .send()
        .await
        .unwrap();
        assert_eq!(response.status(), 200);
        let response = call(
            http::Method::POST,
            &format!("{issue}/labels"),
            Some(&admin),
            "standalone-empty",
        )
        .body("")
        .send()
        .await
        .unwrap();
        assert_eq!(response.status(), 200);
        if !standalone {
            for (id, body) in [
                ("h2-denied-body", r#"{"email":"fixture@example.com"}"#),
                ("h2-denied-empty", ""),
            ] {
                assert_eq!(
                    call(
                        http::Method::POST,
                        &format!("{issue}/labels"),
                        Some(&viewer),
                        id
                    )
                    .body(body)
                    .send()
                    .await
                    .unwrap()
                    .status(),
                    403
                );
            }
        }
        let raw_paths = [
            ("raw-pchars", "/v1/items:batchGet@x+,;=!$&'()*"),
            ("raw-session", "/x/;jsessionid=fixture"),
            ("raw-trailing", "/directory/"),
            ("raw-repeated", "/x//y"),
            ("raw-percent", "/x/a%25b"),
            ("raw-slash", "/github/repos/o/r/branches/feature%2Ftopic"),
            ("raw-unreserved", "/x/%69tems"),
            ("raw-reserved", "/x/a%3Ab"),
        ];
        if !h2 {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            for (id, path) in raw_paths {
                let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
                socket.write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {admin}\r\nX-Acl-Test-Request: {id}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
                let mut response = Vec::new();
                tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    socket.read_to_end(&mut response),
                )
                .await
                .unwrap()
                .unwrap();
                assert!(response.starts_with(b"HTTP/1.1 200"), "{id}");
            }
            for (id, path) in [
                ("escape-unreserved", "/%73trict/private"),
                ("escape-slash", "/strict%2Fprivate"),
                ("escape-double", "/strict%252Fprivate"),
                ("escape-repeated", "/strict//private"),
                ("escape-dot", "/strict/a/../private"),
                ("escape-parameter", "/strict/private;x=1"),
                ("escape-parent-parameter", "/strict;x=1/private"),
                ("escape-encoded-parameter", "/strict/private%3Bx=1"),
                ("escape-double-parameter", "/strict/private%253Bx=1"),
            ] {
                let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
                socket.write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {admin}\r\nX-Acl-Test-Request: {id}\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
                let mut response = Vec::new();
                tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    socket.read_to_end(&mut response),
                )
                .await
                .unwrap()
                .unwrap();
                assert!(response.starts_with(b"HTTP/1.1 400"), "{id}");
            }
        }
        running.shutdown().await.unwrap();
        let report = backend.finish().await;
        let jwks_report = jwks.finish().await;
        assert!(jwks_report.failures.is_empty(), "{jwks_report:?}");
        if let Some((_, stop, task)) = h2_backend {
            stop.send(()).unwrap();
            let receipts = task.await.unwrap();
            assert_eq!(receipts.len(), 2);
            for (id, headers, body) in receipts {
                assert!(!headers.contains_key("transfer-encoding"));
                match id.as_str() {
                    "standalone-tokenized" => assert_eq!(
                        serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
                        json!({"email":"00000000-0000-0000-0000-000000000725"})
                    ),
                    "standalone-empty" => assert!(body.is_empty()),
                    _ => panic!("unexpected H2 stream identity"),
                }
            }
            assert!(report.accepted.is_empty());
            return;
        }
        let mut expected = vec!["standalone-tokenized", "standalone-empty"];
        expected.extend(raw_paths.iter().map(|p| p.0));
        http_acl_no_denied_dispatch(
            &report,
            &expected,
            &[
                "escape-unreserved",
                "escape-slash",
                "escape-double",
                "escape-repeated",
                "escape-dot",
                "escape-parameter",
                "escape-parent-parameter",
                "escape-encoded-parameter",
                "escape-double-parameter",
            ],
        )
        .unwrap();
        for (id, path) in raw_paths {
            let receipt = report
                .headers
                .iter()
                .find(|h| h.request_id.as_deref() == Some(id))
                .unwrap();
            assert_eq!(
                receipt.headers.lines().next().unwrap(),
                format!("GET /rewritten{path} HTTP/1.1")
            );
        }
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&report.bodies[0].body).unwrap(),
            json!({"email":"00000000-0000-0000-0000-000000000725"})
        );
        let empty = report
            .headers
            .iter()
            .find(|h| h.request_id.as_deref() == Some("standalone-empty"))
            .unwrap();
        assert!(
            report
                .bodies
                .iter()
                .find(|b| b.connection == empty.connection)
                .unwrap()
                .body
                .is_empty()
        );
        return;
    }
    let mut allowed = vec![
        "allowed-0",
        "allowed-1",
        "allowed-2",
        "allowed-3",
        "allowed-empty-delete",
        "allowed-continue",
    ];
    let mut denied = Vec::new();
    for (index, (path, credential, filtered)) in [
        (format!("{issue}?page=2"), &admin, true),
        (comments.clone(), &commenter, false),
        ("/exact".into(), &admin, false),
    ]
    .into_iter()
    .enumerate()
    {
        let response = call(http::Method::GET, &path, Some(credential), allowed[index])
            .send()
            .await
            .unwrap();
        let status = response.status();
        let body = response.text().await.unwrap();
        assert_eq!(status, 200, "{body}");
        assert_eq!(body.contains("secret"), !filtered, "{body}");
    }
    let response = call(
        http::Method::POST,
        &format!("{issue}/labels?source=test"),
        Some(&admin),
        allowed[3],
    )
    .body("{}")
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    for (index, (path, credential)) in [
        (issue, &viewer),
        (comments.as_str(), &admin),
        (issue, &commenter),
        ("/github/repos/lightapi/light-portal/unknown", &admin),
    ]
    .into_iter()
    .enumerate()
    {
        let id = format!("denied-get-{index}");
        let response = call(http::Method::GET, path, Some(credential), &id)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403, "{}", response.text().await.unwrap());
        denied.push(id);
    }
    for method in [
        http::Method::POST,
        http::Method::PUT,
        http::Method::PATCH,
        http::Method::DELETE,
    ] {
        let id = format!("denied-method-{method}");
        let response = call(method, issue, Some(&admin), &id)
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403, "{}", response.text().await.unwrap());
        denied.push(id);
    }
    let response = call(
        http::Method::POST,
        &format!("{issue}/labels"),
        Some(&viewer),
        "denied-post-role",
    )
    .body("{}")
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), 403, "{}", response.text().await.unwrap());
    denied.push("denied-post-role".to_owned());
    for (index, credential) in [None, Some("invalid")].into_iter().enumerate() {
        let id = format!("denied-security-{index}");
        let response = call(http::Method::GET, issue, credential, &id)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401, "{}", response.text().await.unwrap());
        denied.push(id);
    }
    let response = call(
        http::Method::DELETE,
        &format!("{issue}/labels"),
        Some(&admin),
        allowed[4],
    )
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    // A real client sends only headers, waits for 100, then sends the body.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
    socket.write_all(format!("POST {issue}/labels HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {admin}\r\nX-Acl-Test-Request: {}\r\nContent-Length: 2\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n", allowed[5]).as_bytes()).await.unwrap();
    let mut interim = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !interim.ends_with(b"\r\n\r\n") {
            interim.push(socket.read_u8().await.unwrap());
        }
    })
    .await
    .unwrap();
    assert!(String::from_utf8_lossy(&interim).starts_with("HTTP/1.1 100"));
    socket.write_all(b"{}").await.unwrap();
    let mut result = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        socket.read_to_end(&mut result),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        String::from_utf8_lossy(&result).starts_with("HTTP/1.1 200"),
        "{}",
        String::from_utf8_lossy(&result)
    );
    for (id, extra, expected) in [
        (
            "denied-declared-size",
            "Content-Length: 10485761\r\nExpect: 100-continue\r\n",
            413,
        ),
        (
            "denied-budget",
            "Content-Length: 1025\r\nExpect: 100-continue\r\n",
            503,
        ),
        ("denied-timeout", "Content-Length: 2\r\n", 408),
    ] {
        let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
        socket.write_all(format!("POST {issue}/labels HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {admin}\r\nX-Acl-Test-Request: {id}\r\nConnection: close\r\n{extra}\r\n").as_bytes()).await.unwrap();
        let mut result = Vec::new();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            socket.read_to_end(&mut result),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            String::from_utf8_lossy(&result).starts_with(&format!("HTTP/1.1 {expected}")),
            "{id}: {}",
            String::from_utf8_lossy(&result)
        );
        denied.push(id.to_owned());
    }
    // A disconnected body reader releases its capture reservation and never
    // starts upstream. Wait for EOF from Gateway after half-closing the producer.
    let mut failed = tokio::net::TcpStream::connect(address).await.unwrap();
    failed.write_all(format!("POST {issue}/labels HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {admin}\r\nX-Acl-Test-Request: denied-read-failure\r\nContent-Length: 2\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
    failed.shutdown().await.unwrap();
    let mut failure_response = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        failed.read_to_end(&mut failure_response),
    )
    .await
    .unwrap()
    .unwrap();
    denied.push("denied-read-failure".into());
    // Capture timeout must release its budget for a subsequent full-size request.
    let response = call(
        http::Method::POST,
        &format!("{issue}/labels"),
        Some(&viewer),
        "denied-after-timeout",
    )
    .body("{}")
    .send()
    .await
    .unwrap();
    assert_eq!(response.status(), 403);
    denied.push("denied-after-timeout".into());
    if pii {
        let response = call(
            http::Method::POST,
            &format!("{issue}/labels"),
            Some(&admin),
            "allowed-tokenized",
        )
        .header("accept-encoding", "gzip")
        .body(r#"{ "email": "fixture@example.com" }"#)
        .send()
        .await
        .unwrap();
        assert_eq!(response.status(), 200);
        let response: serde_json::Value = response.json().await.unwrap();
        assert_eq!(response["email"], "fixture@example.com");
        allowed.push("allowed-tokenized");
        let response = call(
            http::Method::POST,
            &format!("{issue}/labels"),
            Some(&admin),
            "denied-transformed-size",
        )
        .body(r#"{"email":"oversize@example.com"}"#)
        .send()
        .await
        .unwrap();
        assert_eq!(response.status(), 413);
        denied.push("denied-transformed-size".into());
    }
    for (id, path) in [
        (
            "label-space",
            "/github/repos/lightapi/light-portal/labels/help%20wanted",
        ),
        (
            "contents-unicode",
            "/github/repos/lightapi/light-portal/contents/dir%20name/caf%c3%a9.txt",
        ),
        (
            "issue-trailing",
            "/github/repos/lightapi/light-portal/issues/725/",
        ),
        (
            "label-pchars",
            "/github/repos/lightapi/light-portal/labels/a:b@c+,=!$&'()*",
        ),
    ] {
        assert_eq!(
            call(http::Method::GET, path, Some(&admin), id)
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        allowed.push(id);
    }
    for (id, path, status) in [
        (
            "alias-encoded",
            "/github/repos/lightapi/light-portal/%69ssues/725",
            403,
        ),
        (
            "alias-trailing",
            "/github/repos/lightapi/light-portal/issues/725/",
            403,
        ),
        (
            "branch-slash",
            "/github/repos/lightapi/light-portal/branches/feature%2Ftopic",
            400,
        ),
        (
            "issue-parameter",
            "/github/repos/lightapi/light-portal/issues/725;x=1",
            400,
        ),
        (
            "issue-parent-parameter",
            "/github/repos/lightapi/light-portal/issues;x=1/725",
            400,
        ),
        (
            "issue-encoded-parameter",
            "/github/repos/lightapi/light-portal/issues/725%3Bx=1",
            400,
        ),
    ] {
        assert_eq!(
            call(http::Method::GET, path, Some(&viewer), id)
                .send()
                .await
                .unwrap()
                .status(),
            status
        );
        denied.push(id.into());
    }
    // Quiesce the sole producer, then drain accepts and await all recorder
    // workers. No late headers or unjoined worker failures can evade the oracle.
    tokio::time::timeout(std::time::Duration::from_secs(10), running.shutdown())
        .await
        .unwrap()
        .unwrap();
    let (acl_budget, hmac_budget) = budgets.lock().unwrap().clone().unwrap();
    assert_eq!(acl_budget.load(Ordering::Acquire), 0);
    assert_eq!(hmac_budget.load(Ordering::Acquire), 0);
    let report = backend.finish().await;
    let jwks_report = jwks.finish().await;
    assert!(jwks_report.failures.is_empty(), "{jwks_report:?}");
    assert!(!jwks_report.accepted.is_empty());
    assert_eq!(jwks_report.accepted.len(), jwks_report.headers.len());
    assert_eq!(jwks_report.accepted.len(), jwks_report.bodies.len());
    let denied_ids = denied.iter().map(String::as_str).collect::<Vec<_>>();
    assert_eq!(denied_ids.len(), if pii { 23 } else { 22 });
    http_acl_no_denied_dispatch(&report, &allowed, &denied_ids)
        .unwrap_or_else(|error| panic!("{error}; {report:?}"));
    for (id, path) in [
        (
            "label-space",
            "/github/repos/lightapi/light-portal/labels/help%20wanted",
        ),
        (
            "contents-unicode",
            "/github/repos/lightapi/light-portal/contents/dir%20name/caf%c3%a9.txt",
        ),
        (
            "issue-trailing",
            "/github/repos/lightapi/light-portal/issues/725/",
        ),
        (
            "label-pchars",
            "/github/repos/lightapi/light-portal/labels/a:b@c+,=!$&'()*",
        ),
    ] {
        let header = report
            .headers
            .iter()
            .find(|h| h.request_id.as_deref() == Some(id))
            .unwrap();
        assert_eq!(
            header.headers.lines().next().unwrap(),
            format!("GET /rewritten{path} HTTP/1.1")
        );
    }
    let paths = [
        format!("GET /rewritten{issue}?page=2 HTTP/1.1"),
        format!("GET /rewritten{comments} HTTP/1.1"),
        "GET /rewritten/exact HTTP/1.1".into(),
        format!("POST /rewritten{issue}/labels?source=test HTTP/1.1"),
        format!("DELETE /rewritten{issue}/labels HTTP/1.1"),
        format!("POST /rewritten{issue}/labels HTTP/1.1"),
    ];
    for (index, id) in allowed.iter().take(6).enumerate() {
        let header = report
            .headers
            .iter()
            .find(|h| h.request_id.as_deref() == Some(id))
            .unwrap();
        assert_eq!(header.headers.lines().next().unwrap(), paths[index]);
        let headers = header.headers.to_ascii_lowercase();
        assert!(
            headers.contains("x-upstream-test: rewritten\r\n"),
            "{headers}"
        );
        assert!(!headers.contains("\r\nauthorization:"), "{headers}");
        let body = report
            .bodies
            .iter()
            .find(|b| b.connection == header.connection)
            .unwrap();
        assert_eq!(
            body.body.as_slice(),
            if index == 3 || index == 5 {
                b"{}".as_slice()
            } else {
                b"".as_slice()
            }
        );
    }
    if pii {
        let tokenized = report
            .headers
            .iter()
            .find(|header| header.request_id.as_deref() == Some("allowed-tokenized"))
            .unwrap();
        assert!(
            !tokenized
                .headers
                .to_ascii_lowercase()
                .contains("accept-encoding:")
        );
        let body = report
            .bodies
            .iter()
            .find(|body| body.connection == tokenized.connection)
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body.body).unwrap(),
            json!({"email":"00000000-0000-0000-0000-000000000725"})
        );
    }
    println!(
        "Gateway recorder: {} correlated requests; {} accepts, {} header receipts, {} body receipts; {} denied IDs absent; 0 recorder failures",
        allowed.len() + denied_ids.len(),
        report.accepted.len(),
        report.headers.len(),
        report.bodies.len(),
        denied_ids.len()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn http_acl_recorder_drains_connection_queued_immediately_before_finish() {
    // Blocking OS connect/write queues a full connection without yielding to
    // the async accept task. Stop the producer before issuing recorder finish.
    use std::io::Write;
    let recorder = HttpAclRecorder::start("{}", std::time::Duration::from_secs(1)).await;
    let mut producer = std::net::TcpStream::connect(recorder.address).unwrap();
    producer
        .write_all(b"GET /queued HTTP/1.1\r\nHost: localhost\r\nX-Acl-Test-Request: queued\r\n\r\n")
        .unwrap();
    producer.shutdown(std::net::Shutdown::Write).unwrap();
    let report = recorder.finish().await;
    http_acl_no_denied_dispatch(&report, &["queued"], &[]).unwrap();
    assert_eq!(report.accepted.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_acl_review_standalone_hmac_original_body_and_denial() {
    http_acl_hmac_fixture(false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn http_acl_review_unified_hmac_original_body_and_denial() {
    http_acl_hmac_fixture(true).await;
}

async fn http_acl_hmac_fixture(unified: bool) {
    use hmac::{Hmac, Mac};
    let backend = HttpAclRecorder::start("{}", std::time::Duration::from_secs(2)).await;
    let config_dir = TempDir::new().unwrap();
    let external_dir = TempDir::new().unwrap();
    let port = free_tcp_port();
    let address = format!("127.0.0.1:{port}").parse().unwrap();
    let write =
        |name: &str, value: String| std::fs::write(config_dir.path().join(name), value).unwrap();
    write(
        "server.yml",
        format!(
            "ip: 127.0.0.1\nhttpPort: {port}\nhttpsPort: 8443\nadvertisedAddress: 127.0.0.1\ndynamicPort: false\nstartOnRegistryFailure: true\nenvironment: dev\nenableHttp: true\nenableHttps: false\nenableRegistry: false\nserviceId: acl-hmac-test\nshutdownGracefulPeriod: 100\n"
        ),
    );
    let entry = if unified { "unified-security" } else { "hmac" };
    write(
        "handler.yml",
        format!(
            "handlers: [{entry}, access-control, router]\npaths:\n  - path: /hooks/*\n    method: POST\n    exec: [{entry}, access-control, router]\ndefaultHandlers: []\n"
        ),
    );
    write("router.yml", "hostWhitelist: ['127\\.0\\.0\\.1']\n".into());
    write(
        "access-control.yml",
        "enabled: true\ndefaultDeny: true\n".into(),
    );
    let route = if unified {
        ""
    } else {
        "pathPrefixAuths:\n  - prefix: /hooks\n    methods: [POST]\n    profile: fixture\n"
    };
    // Existing test convention uses PATH as an available non-credential fixture
    // input. Neither it nor signatures are emitted in assertions/artifacts.
    write(
        "hmac.yml",
        format!(
            "enabled: true\n{route}profiles:\n  fixture:\n    maxBodyBytes: 1024\n    secrets:\n      defaultEnvNames: [PATH]\n"
        ),
    );
    if unified {
        write("unified-security.yml", "enabled: true\npathPrefixAuths:\n  - prefix: /hooks\n    methods: [POST]\n    authentication:\n      allOf:\n        - type: hmac\n          profile: fixture\n".into());
    }
    write("rule.yml", "ruleBodies:\n  allow:\n    ruleId: allow\n    ruleName: Allow fixture\n    ruleType: req-acc\n    common: Y\n    conditionLanguage: cel\n    conditionSecurityProfile: strict\n    expression: 'true'\nendpointRules:\n  /hooks/allow@post:\n    req-acc: [allow]\n".into());
    let runtime = LightRuntimeBuilder::new(PingoraTransport::new(GatewayApp::default()))
        .with_config_dir(config_dir.path())
        .with_external_config_dir(external_dir.path())
        .build();
    let running = tokio::time::timeout(std::time::Duration::from_secs(10), runtime.start())
        .await
        .unwrap()
        .unwrap();
    wait_for_tcp(address).await;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .unwrap();
    let original = b"{ \"id\": 725 }";
    let mut mac =
        Hmac::<sha2::Sha256>::new_from_slice(std::env::var("PATH").unwrap().as_bytes()).unwrap();
    mac.update(original);
    let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
    for (path, id, signed, status) in [
        ("/hooks/allow", "hmac-allowed", true, 200),
        ("/hooks/denied", "hmac-acl-denied", true, 403),
        ("/hooks/allow", "hmac-invalid", false, 401),
    ] {
        let response = client
            .post(format!("http://{address}{path}"))
            .header("service_url", format!("http://{}", backend.address))
            .header("x-acl-test-request", id)
            .header(
                "x-hub-signature-256",
                if signed {
                    signature.as_str()
                } else {
                    "sha256=0000000000000000000000000000000000000000000000000000000000000000"
                },
            )
            .body(original.to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), status, "{id} rejected incorrectly");
    }
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
    let oversized = "x".repeat(1025);
    socket.write_all(format!("POST /hooks/allow HTTP/1.1\r\nHost: localhost\r\nX-Acl-Test-Request: hmac-streamed-oversize\r\nX-Hub-Signature-256: {signature}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{oversized}\r\n0\r\n\r\n", oversized.len()).as_bytes()).await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        socket.read_to_end(&mut response),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(String::from_utf8_lossy(&response).starts_with("HTTP/1.1 413"));
    // Missing signature is a header-only rejection: do not send the body or
    // accept an interim 100 response as the result.
    let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
    socket.write_all(b"POST /hooks/allow HTTP/1.1\r\nHost: localhost\r\nX-Acl-Test-Request: hmac-missing-signature\r\nContent-Length: 2\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n").await.unwrap();
    let mut response = Vec::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        socket.read_to_end(&mut response),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(String::from_utf8_lossy(&response).starts_with("HTTP/1.1 401"));
    running.shutdown().await.unwrap();
    let report = backend.finish().await;
    http_acl_no_denied_dispatch(
        &report,
        &["hmac-allowed"],
        &[
            "hmac-acl-denied",
            "hmac-invalid",
            "hmac-streamed-oversize",
            "hmac-missing-signature",
        ],
    )
    .unwrap();
    assert_eq!(report.bodies[0].body, original);
}
