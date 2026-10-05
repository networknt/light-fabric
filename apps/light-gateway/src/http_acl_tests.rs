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
    for line in headers.lines().skip(1).filter(|line| !line.is_empty()) {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| "malformed header".to_owned())?;
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err("fixture requires Content-Length framing, not Transfer-Encoding".into());
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

impl HttpAclRecorder {
    async fn start(response_body: &str, io_bound: std::time::Duration) -> Self {
        use futures_util::FutureExt;
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
            while let Some(accepted) = listener.accept().now_or_never() {
                let (socket, _) = accepted.expect("recorder drain accept");
                http_acl_accept_worker(
                    socket,
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
        r#"{"id":725,"secret":"redacted"}"#,
        std::time::Duration::from_secs(2),
    )
    .await;
    let backend_addr = backend.address;
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
        format!("hosts: http://{backend_addr}/rewritten\n"),
    );
    write(
        "access-control.yml",
        "enabled: true\ndefaultDeny: true\n".into(),
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
  /exact@get:
    permission: {roles: admin}
    req-acc: [role]
"#.into());
    let runtime = LightRuntimeBuilder::new(PingoraTransport::new(GatewayApp::default()))
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
        "token_use":"user","uid":"synthetic-user","sub":"synthetic-user","role":role}),
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
    let allowed = ["allowed-0", "allowed-1", "allowed-2", "allowed-3"];
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
    // Quiesce the sole producer, then drain accepts and await all recorder
    // workers. No late headers or unjoined worker failures can evade the oracle.
    tokio::time::timeout(std::time::Duration::from_secs(10), running.shutdown())
        .await
        .unwrap()
        .unwrap();
    let report = backend.finish().await;
    let jwks_report = jwks.finish().await;
    assert!(jwks_report.failures.is_empty(), "{jwks_report:?}");
    assert!(!jwks_report.accepted.is_empty());
    assert_eq!(jwks_report.accepted.len(), jwks_report.headers.len());
    assert_eq!(jwks_report.accepted.len(), jwks_report.bodies.len());
    let denied_ids = denied.iter().map(String::as_str).collect::<Vec<_>>();
    assert_eq!(denied_ids.len(), 11);
    http_acl_no_denied_dispatch(&report, &allowed, &denied_ids)
        .unwrap_or_else(|error| panic!("{error}; {report:?}"));
    let paths = [
        format!("GET /rewritten{issue}?page=2 HTTP/1.1"),
        format!("GET /rewritten{comments} HTTP/1.1"),
        "GET /rewritten/exact HTTP/1.1".into(),
        format!("POST /rewritten{issue}/labels?source=test HTTP/1.1"),
    ];
    for (index, id) in allowed.iter().enumerate() {
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
            if index == 3 {
                b"{}".as_slice()
            } else {
                b"".as_slice()
            }
        );
    }
    println!(
        "Gateway recorder: 15 correlated requests; 4 accepts, 4 header receipts, 4 body receipts; 11 denied IDs absent; 0 recorder failures"
    );
}
