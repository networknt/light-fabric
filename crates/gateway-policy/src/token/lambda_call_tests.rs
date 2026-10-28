use super::*;
use std::{
    collections::VecDeque,
    sync::{
        Mutex as StdMutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::{JoinHandle, JoinSet},
    time::{Instant, timeout},
};

#[derive(Clone)]
struct Reply {
    status: u16,
    body: String,
    delay: Duration,
    chunked: bool,
}
impl Reply {
    fn token(token: &str) -> Self {
        Self {
            status: 200,
            body: json!({"access_token":token,"expires_in":3600}).to_string(),
            delay: Duration::ZERO,
            chunked: false,
        }
    }
    fn denial() -> Self {
        Self{status:401,body:json!({"statusCode":401,"code":"ERR10000","message":"INVALID_AUTH_TOKEN","description":"test-only-sensitive-provider-body","severity":"ERROR"}).to_string(),..Self::token("unused")}
    }
}
struct Server {
    url: String,
    calls: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let replies = Arc::new(StdMutex::new(VecDeque::from(replies)));
        let task = tokio::spawn(async move {
            let mut work = JoinSet::new();
            loop {
                tokio::select! {
                    accepted=listener.accept()=>{
                        let Ok((mut stream,_))=accepted else{return};
                        let count=count.clone();let replies=replies.clone();
                        work.spawn(async move{
                            let mut request=Vec::new();let mut buffer=[0u8;4096];
                            let end=loop{
                                let n=stream.read(&mut buffer).await.unwrap_or(0);if n==0{return}
                                request.extend_from_slice(&buffer[..n]);assert!(request.len()<16_384);
                                if let Some(i)=request.windows(4).position(|v|v==b"\r\n\r\n"){break i+4}
                            };
                            let headers=String::from_utf8(request[..end].to_vec()).unwrap();
                            assert!(headers.starts_with("POST /token "));
                            let length=headers.lines().find_map(|line|line.split_once(':').filter(|(name,_)|name.eq_ignore_ascii_case("content-length")).and_then(|(_,v)|v.trim().parse::<usize>().ok())).unwrap_or(0);
                            while request.len()<end+length{let n=stream.read(&mut buffer).await.unwrap_or(0);if n==0{return}request.extend_from_slice(&buffer[..n]);}
                            assert!(&request[end..end+length]==b"grant_type=client_credentials&scope=test");
                            count.fetch_add(1,Ordering::SeqCst);
                            let reply=replies.lock().unwrap().pop_front().expect("unexpected provider call");
                            tokio::time::sleep(reply.delay).await;
                            let body=reply.body.as_bytes();
                            let response=if reply.chunked{
                                format!("HTTP/1.1 {} Reply\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n",reply.status,body.len(),reply.body).into_bytes()
                            }else{
                                let mut response=format!("HTTP/1.1 {} Reply\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",reply.status,body.len()).into_bytes();response.extend_from_slice(body);response
                            };
                            let _=stream.write_all(&response).await;
                        });
                    }
                    _=work.join_next(),if !work.is_empty()=>{}
                }
            }
        });
        Self { url, calls, task }
    }
    async fn wait(&self, n: usize) {
        timeout(Duration::from_secs(2), async {
            while self.calls.load(Ordering::SeqCst) < n {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
    }
}
fn make_runtime(server: &Server, renew: u64) -> TokenRuntime {
    let mut client = ClientTokenConfig::default();
    client.request.timeout = 2000;
    client.request.connect_timeout = 1000;
    client.oauth.token.server_url = Some(server.url.clone());
    client.oauth.token.enable_http2 = false;
    client.oauth.token.token_renew_before_expired = renew;
    client.oauth.token.expired_refresh_retry_delay = 0;
    client.oauth.token.early_refresh_retry_delay = 1000;
    client.oauth.token.client_credentials.client_id = "test-only-client".into();
    client.oauth.token.client_credentials.client_secret = "test-only-secret".into();
    client.oauth.token.client_credentials.uri = "/token".into();
    client.oauth.token.client_credentials.scope = vec!["test".into()];
    TokenRuntime::new(
        TokenHandlerConfig {
            enabled: true,
            applied_path_prefixes: vec!["/".into()],
        },
        SidecarTrafficConfig::default(),
        client,
        DirectRegistryConfig::default(),
        None,
    )
}
async fn lambda(runtime: &TokenRuntime) -> Result<String, HandlerRejection> {
    runtime
        .lambda_client_credentials_token(
            "/x",
            Some("test-service".into()),
            Instant::now() + Duration::from_secs(2),
        )
        .await
}
async fn gateway(runtime: &TokenRuntime) -> Result<String, HandlerRejection> {
    runtime
        .get_client_credentials_token("/x", Some("test-service".into()))
        .await
}
async fn entry(runtime: &TokenRuntime) -> Arc<Mutex<CachedToken>> {
    let options = runtime
        .resolve_request_options("/x", Some("test-service".into()))
        .unwrap();
    runtime
        .cache
        .get_or_insert_empty(TokenCacheKey::new(options.cache_service_id, options.scope))
        .await
}
async fn seed(runtime: &TokenRuntime) {
    let e = entry(runtime).await;
    let mut c = e.lock().await;
    c.token = Some("test-only-old".into());
    c.expires_at_millis = now_millis() + 1000;
}

#[tokio::test]
async fn same_runtime_success_cache_and_mixed_error_policies_remain_separate() {
    let server = Server::start(vec![Reply::token("test-only-token")]).await;
    let runtime = make_runtime(&server, 0);
    assert!(lambda(&runtime).await.unwrap() == "test-only-token");
    assert!(gateway(&runtime).await.unwrap() == "test-only-token");
    assert!(lambda(&runtime).await.unwrap() == "test-only-token");
    assert_eq!(server.calls.load(Ordering::SeqCst), 1);
    let malformed = Reply {
        status: 401,
        body: "test-only-sensitive-invalid-json".into(),
        ..Reply::denial()
    };
    let server = Server::start(vec![
        Reply::denial(),
        Reply::denial(),
        malformed.clone(),
        malformed,
    ])
    .await;
    let runtime = make_runtime(&server, 0);
    for (lambda_policy, expected_status, expected_code) in [
        (false, 502, "ERR10052"),
        (true, 401, "ERR10000"),
        (false, 502, "ERR10052"),
        (true, 500, "ERR14004"),
    ] {
        let err = if lambda_policy {
            lambda(&runtime).await
        } else {
            gateway(&runtime).await
        }
        .unwrap_err();
        assert_eq!(err.status, expected_status);
        assert_eq!(err.code, expected_code);
        if lambda_policy {
            assert!(!err.message.contains("sensitive"));
        }
    }
    assert_eq!(server.calls.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn lambda_redirect_is_structured_redacted_failure_without_destination_request() {
    for cross_origin in [false, true] {
        let destination = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", origin.local_addr().unwrap());
        let location = if cross_origin {
            format!("http://{}/secret", destination.local_addr().unwrap())
        } else {
            format!("{url}/secret")
        };
        let task = tokio::spawn(async move {
            let (mut stream, _) = origin.accept().await.unwrap();
            let mut bytes = [0; 4096];
            assert!(stream.read(&mut bytes).await.unwrap() > 0);
            let response = format!(
                "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            assert!(
                timeout(Duration::from_millis(100), origin.accept())
                    .await
                    .is_err()
            );
        });
        let server = Server {
            url,
            calls: Arc::new(AtomicUsize::new(0)),
            task,
        };
        let runtime = make_runtime(&server, 0);
        let err = lambda(&runtime).await.unwrap_err();
        assert_eq!((err.status, err.code.as_str()), (500, "ERR14004"));
        assert!(!err.message.contains("test-only"));
        assert!(!err.message.contains("secret"));
        assert!(
            timeout(Duration::from_millis(150), destination.accept())
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn approved_valid_json_oauth_error_is_not_canonical_denial() {
    let server = Server::start(vec![Reply {
        status: 401,
        body: r#"{"error":"invalid_client"}"#.into(),
        ..Reply::denial()
    }])
    .await;
    let runtime = make_runtime(&server, 0);
    let err = lambda(&runtime).await.unwrap_err();
    assert_eq!((err.status, err.code.as_str()), (500, "ERR14004"));
    assert_eq!(server.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn opt_in_named_cache_expiry_and_zero_capacity_use_the_shared_cache() {
    for (capacity, expiry) in [(1, Duration::from_millis(20)), (0, Duration::from_secs(60))] {
        let server = Server::start(vec![
            Reply::token("test-only-first"),
            Reply::token("test-only-next"),
        ])
        .await;
        let runtime = make_runtime(&server, 0).with_lambda_cache_limits(capacity, expiry);
        assert_eq!(lambda(&runtime).await.unwrap(), "test-only-first");
        if capacity == 1 {
            assert_eq!(gateway(&runtime).await.unwrap(), "test-only-first");
            assert_eq!(server.calls.load(Ordering::SeqCst), 1);
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        assert_eq!(lambda(&runtime).await.unwrap(), "test-only-next");
        assert_eq!(server.calls.load(Ordering::SeqCst), 2);
    }
}
#[tokio::test]
async fn lambda_bounds_do_not_poison_gateway_cache_or_retry_backoff() {
    for chunked in [false, true] {
        let token = "x".repeat(LAMBDA_TOKEN_RESPONSE_MAX_BYTES + 1);
        let large = Reply {
            chunked,
            ..Reply::token(&token)
        };
        let server = Server::start(vec![large.clone(), large]).await;
        let runtime = make_runtime(&server, 0);
        assert_eq!(lambda(&runtime).await.unwrap_err().code, "ERR14004");
        assert_eq!(entry(&runtime).await.lock().await.expired_retry_timeout, 0);
        assert!(gateway(&runtime).await.unwrap() == token);
        assert_eq!(lambda(&runtime).await.unwrap_err().code, "ERR14004");
        assert!(gateway(&runtime).await.unwrap() == token);
        assert_eq!(server.calls.load(Ordering::SeqCst), 2);
    }
}
#[tokio::test]
async fn deadline_and_abort_clear_only_lambda_renewal_and_discard_late_provider_results() {
    for abort in [false, true] {
        let slow = Reply {
            delay: Duration::from_millis(150),
            ..Reply::token("test-only-late")
        };
        let server = Server::start(vec![slow, Reply::token("test-only-next")]).await;
        let runtime = Arc::new(make_runtime(&server, 0));
        // This case exercises an in-flight request. Hold virtual time until
        // real I/O has started so scheduler load cannot consume its 40 ms first.
        tokio::time::pause();
        let cloned = runtime.clone();
        let task = tokio::spawn(async move {
            cloned
                .lambda_client_credentials_token(
                    "/x",
                    Some("test-service".into()),
                    Instant::now() + Duration::from_millis(if abort { 1000 } else { 40 }),
                )
                .await
        });
        let io_started = std::time::Instant::now();
        while server.calls.load(Ordering::SeqCst) < 1 {
            assert!(
                io_started.elapsed() < Duration::from_secs(2),
                "loopback request starts"
            );
            // Remaining runnable also prevents automatic virtual-time advance.
            tokio::task::yield_now().await;
        }
        if abort {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        } else {
            tokio::time::advance(Duration::from_millis(40)).await;
            assert_eq!(task.await.unwrap().unwrap_err().code, "ERR14017");
        }
        tokio::time::resume();
        let e = entry(&runtime).await;
        {
            let c = e.lock().await;
            assert!(!c.renewing);
            assert_eq!(c.expired_retry_timeout, 0);
            assert!(c.token.is_none());
        }
        assert!(gateway(&runtime).await.unwrap() == "test-only-next");
        tokio::time::sleep(Duration::from_millis(170)).await;
        assert!(lambda(&runtime).await.unwrap() == "test-only-next");
        assert_eq!(server.calls.load(Ordering::SeqCst), 2);
    }
}
#[tokio::test]
async fn lambda_waits_for_shared_background_refresh_without_inheriting_policy() {
    let server = Server::start(vec![Reply {
        delay: Duration::from_millis(30),
        ..Reply::token("test-only-fresh")
    }])
    .await;
    let runtime = make_runtime(&server, 2000);
    seed(&runtime).await;
    assert!(gateway(&runtime).await.unwrap() == "test-only-old");
    server.wait(1).await;
    assert!(lambda(&runtime).await.unwrap() == "test-only-fresh");
    assert!(gateway(&runtime).await.unwrap() == "test-only-fresh");
    assert_eq!(server.calls.load(Ordering::SeqCst), 1);
    let server = Server::start(vec![
        Reply {
            delay: Duration::from_millis(30),
            ..Reply::denial()
        },
        Reply::denial(),
    ])
    .await;
    let runtime = make_runtime(&server, 2000);
    seed(&runtime).await;
    assert!(gateway(&runtime).await.unwrap() == "test-only-old");
    server.wait(1).await;
    assert_eq!(lambda(&runtime).await.unwrap_err().code, "ERR10000");
    assert!(gateway(&runtime).await.unwrap() == "test-only-old");
    assert_eq!(server.calls.load(Ordering::SeqCst), 2);
}
