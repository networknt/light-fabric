use bytes::Bytes;
use reqwest::Version;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};

const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug)]
struct ObservedRequest {
    headers: String,
    body: Vec<u8>,
    expected_body_bytes: Option<usize>,
    complete: bool,
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn free_tcp_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind free TCP port")
        .local_addr()
        .expect("read free TCP port")
        .port()
}

fn header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

fn decode_chunked(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut cursor = 0;
    let mut decoded = Vec::new();
    loop {
        let line_end = bytes[cursor..]
            .windows(2)
            .position(|window| window == b"\r\n")?
            + cursor;
        let size_text = std::str::from_utf8(&bytes[cursor..line_end]).ok()?;
        let size = usize::from_str_radix(size_text.split(';').next()?.trim(), 16).ok()?;
        cursor = line_end + 2;
        if size == 0 {
            if bytes.get(cursor..cursor + 2)? == b"\r\n" {
                return Some(decoded);
            }
            return None;
        }
        let chunk_end = cursor.checked_add(size)?;
        let framing_end = chunk_end.checked_add(2)?;
        if bytes.get(chunk_end..framing_end)? != b"\r\n" {
            return None;
        }
        decoded.extend_from_slice(bytes.get(cursor..chunk_end)?);
        cursor = framing_end;
    }
}

async fn read_upstream_request(socket: &mut TcpStream) -> ObservedRequest {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let wait = if header_end(&request).is_some() {
            Duration::from_millis(750)
        } else {
            Duration::from_secs(10)
        };
        let read = match timeout(wait, socket.read(&mut buffer)).await {
            Ok(result) => result.expect("read upstream request"),
            Err(_) => {
                let headers_end =
                    header_end(&request).expect("request headers before idle timeout");
                let headers = String::from_utf8_lossy(&request[..headers_end]).to_string();
                let expected_body_bytes = headers
                    .to_ascii_lowercase()
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse::<usize>().ok());
                return ObservedRequest {
                    headers,
                    body: request[headers_end + 4..].to_vec(),
                    expected_body_bytes,
                    complete: false,
                };
            }
        };
        assert!(read > 0, "upstream closed before request completed");
        request.extend_from_slice(&buffer[..read]);
        let Some(headers_end) = header_end(&request) else {
            continue;
        };
        let headers = String::from_utf8_lossy(&request[..headers_end]).to_string();
        let lower_headers = headers.to_ascii_lowercase();
        let encoded_body = &request[headers_end + 4..];
        if lower_headers.lines().any(|line| {
            line.strip_prefix("transfer-encoding:")
                .is_some_and(|value| value.trim().eq_ignore_ascii_case("chunked"))
        }) {
            if let Some(body) = decode_chunked(encoded_body) {
                return ObservedRequest {
                    headers,
                    body,
                    expected_body_bytes: None,
                    complete: true,
                };
            }
            continue;
        }
        let content_length = lower_headers
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if encoded_body.len() >= content_length {
            return ObservedRequest {
                headers,
                body: encoded_body[..content_length].to_vec(),
                expected_body_bytes: Some(content_length),
                complete: true,
            };
        }
    }
}

async fn start_counting_upstream() -> (
    std::net::SocketAddr,
    Arc<AtomicUsize>,
    mpsc::UnboundedReceiver<ObservedRequest>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind counting upstream");
    let address = listener.local_addr().expect("counting upstream address");
    let count = Arc::new(AtomicUsize::new(0));
    let (sender, receiver) = mpsc::unbounded_channel();
    let task_count = Arc::clone(&count);
    let task = tokio::spawn(async move {
        while let Ok((mut socket, _peer)) = listener.accept().await {
            task_count.fetch_add(1, Ordering::SeqCst);
            let sender = sender.clone();
            tokio::spawn(async move {
                let request = read_upstream_request(&mut socket).await;
                sender.send(request).expect("record upstream request");
                socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .await
                    .expect("write upstream response");
            });
        }
    });
    (address, count, receiver, task)
}

async fn wait_for_tcp(address: std::net::SocketAddr) {
    timeout(Duration::from_secs(10), async {
        loop {
            if TcpStream::connect(address).await.is_ok() {
                return;
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("Phase 0 proxy did not start");
}

fn start_proxy(listen: std::net::SocketAddr, upstream: std::net::SocketAddr) -> ChildGuard {
    let child = Command::new(env!("CARGO_BIN_EXE_hmac-phase0-spikes"))
        .arg(listen.to_string())
        .arg(upstream.to_string())
        .arg(MAX_BODY_BYTES.to_string())
        .arg("10000")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start Phase 0 proxy");
    ChildGuard(child)
}

async fn h1_content_length_request(
    address: std::net::SocketAddr,
    path: &str,
    body: &[u8],
    extra_headers: &[(&str, &str)],
) -> Vec<u8> {
    let mut socket = TcpStream::connect(address)
        .await
        .expect("connect HTTP/1.1 client");
    let mut headers = format!(
        "POST {path} HTTP/1.1\r\nHost: {address}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in extra_headers {
        headers.push_str(format!("{name}: {value}\r\n").as_str());
    }
    headers.push_str("\r\n");
    socket
        .write_all(headers.as_bytes())
        .await
        .expect("write HTTP/1.1 headers");
    socket.write_all(body).await.expect("write HTTP/1.1 body");
    let mut response = Vec::new();
    timeout(Duration::from_secs(20), socket.read_to_end(&mut response))
        .await
        .expect("HTTP/1.1 response timeout")
        .expect("read HTTP/1.1 response");
    response
}

async fn h1_chunked_request(
    address: std::net::SocketAddr,
    path: &str,
    chunks: &[&[u8]],
) -> Vec<u8> {
    let mut socket = TcpStream::connect(address)
        .await
        .expect("connect chunked HTTP/1.1 client");
    socket
        .write_all(
            format!(
                "POST {path} HTTP/1.1\r\nHost: {address}\r\nTransfer-Encoding: chunked\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .expect("write chunked request headers");
    for chunk in chunks {
        socket
            .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
            .await
            .expect("write chunk size");
        socket.write_all(chunk).await.expect("write chunk body");
        socket
            .write_all(b"\r\n")
            .await
            .expect("write chunk terminator");
    }
    socket
        .write_all(b"0\r\n\r\n")
        .await
        .expect("finish chunked request");
    let mut response = Vec::new();
    timeout(Duration::from_secs(20), socket.read_to_end(&mut response))
        .await
        .expect("chunked response timeout")
        .expect("read chunked response");
    response
}

fn assert_h1_status(response: &[u8], expected: u16) {
    let status = format!("HTTP/1.1 {expected}");
    assert!(
        response.starts_with(status.as_bytes()),
        "expected {status}, got {}",
        String::from_utf8_lossy(response)
    );
}

async fn next_observed(receiver: &mut mpsc::UnboundedReceiver<ObservedRequest>) -> ObservedRequest {
    timeout(Duration::from_secs(20), receiver.recv())
        .await
        .expect("counting upstream observation timeout")
        .expect("counting upstream stopped")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn phase0_proves_gateway_core_prebuffer_hook() {
    let (upstream, connections, mut observed, upstream_task) = start_counting_upstream().await;
    let proxy = std::net::SocketAddr::from(([127, 0, 0, 1], free_tcp_port()));
    let _proxy_process = start_proxy(proxy, upstream);
    wait_for_tcp(proxy).await;

    let binary_body = Bytes::from_static(b"{\n  \"raw\": \"\xff-not-utf8\"\n}\0");
    let response = h1_content_length_request(
        proxy,
        "/forward",
        &binary_body,
        &[
            ("Content-Type", "application/octet-stream"),
            ("X-GitHub-Event", "push"),
        ],
    )
    .await;
    assert_h1_status(&response, 200);
    let first = next_observed(&mut observed).await;
    assert!(first.complete);
    assert_eq!(first.body, binary_body);
    let first_headers = first.headers.to_ascii_lowercase();
    assert!(first_headers.contains("content-type: application/octet-stream"));
    assert!(first_headers.contains("x-github-event: push"));

    let chunk_a = b"chunk-one\0".as_slice();
    let chunk_b = b"\xffchunk-two".as_slice();
    let response = h1_chunked_request(proxy, "/forward", &[chunk_a, chunk_b]).await;
    assert_h1_status(&response, 200);
    let second = next_observed(&mut observed).await;
    assert!(second.complete);
    assert_eq!(second.body, [chunk_a, chunk_b].concat());

    let before_duplicate = connections.load(Ordering::SeqCst);
    let response = h1_content_length_request(proxy, "/duplicate", b"same-delivery", &[]).await;
    assert_h1_status(&response, 200);
    assert_eq!(
        connections.load(Ordering::SeqCst),
        before_duplicate,
        "HTTP/1.1 duplicate contacted the upstream"
    );

    let h2_client = reqwest::Client::builder()
        .http2_prior_knowledge()
        .build()
        .expect("build h2c client");
    let mut h2_body = vec![b'h'; 128 * 1024];
    h2_body.extend_from_slice(b"\0\xff");
    let h2_body = Bytes::from(h2_body);
    let response = h2_client
        .post(format!("http://{proxy}/forward"))
        .header("content-type", "application/octet-stream")
        .header("x-github-event", "issues")
        .body(h2_body.clone())
        .send()
        .await
        .expect("send h2c request");
    assert_eq!(response.status(), 200);
    assert_eq!(response.version(), Version::HTTP_2);
    let third = next_observed(&mut observed).await;
    assert!(third.complete);
    assert_eq!(third.body, h2_body);
    assert!(
        third
            .headers
            .to_ascii_lowercase()
            .contains("x-github-event: issues")
    );

    let before_h2_duplicate = connections.load(Ordering::SeqCst);
    let duplicate = h2_client
        .post(format!("http://{proxy}/duplicate"))
        .body("same-delivery")
        .send()
        .await
        .expect("send duplicate h2c request");
    assert_eq!(duplicate.status(), 200);
    assert_eq!(duplicate.version(), Version::HTTP_2);
    assert!(
        duplicate
            .bytes()
            .await
            .expect("read duplicate body")
            .is_empty()
    );
    assert_eq!(
        connections.load(Ordering::SeqCst),
        before_h2_duplicate,
        "HTTP/2 duplicate contacted the upstream"
    );

    let exact_limit = vec![b'x'; MAX_BODY_BYTES];
    let response = h1_content_length_request(proxy, "/forward", &exact_limit, &[]).await;
    assert_h1_status(&response, 200);
    let at_limit = next_observed(&mut observed).await;
    assert_eq!(at_limit.expected_body_bytes, Some(MAX_BODY_BYTES));
    assert!(at_limit.complete);
    assert_eq!(at_limit.body, exact_limit);

    let before_oversized = connections.load(Ordering::SeqCst);
    let oversized = vec![b'y'; MAX_BODY_BYTES + 1];
    let split = MAX_BODY_BYTES / 2;
    let response = h1_chunked_request(
        proxy,
        "/forward",
        &[&oversized[..split], &oversized[split..]],
    )
    .await;
    assert_h1_status(&response, 413);
    assert_eq!(
        connections.load(Ordering::SeqCst),
        before_oversized,
        "oversized request contacted the upstream"
    );

    upstream_task.abort();
}
