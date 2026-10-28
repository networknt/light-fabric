use super::{SecurityConfig, SecurityRuntime};
use base64::Engine;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use light_runtime::{ModuleRegistry, RuntimeConfig};
use serde_json::{Value, json};
use std::future::Future;
use std::pin::Pin;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::task::Poll;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

// Synthetic signing material; never used outside loopback component tests.
pub const SECRET: &[u8] = b"test-only-jwks-refresh-hmac-secret-32";

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

pub fn token(kid: Option<&str>, secret: &[u8], claims: Value) -> String {
    let mut header = Header::new(Algorithm::HS256);
    header.kid = kid.map(str::to_owned);
    encode(&header, &claims, &EncodingKey::from_secret(secret)).unwrap()
}

pub fn valid(kid: &str) -> String {
    token(
        Some(kid),
        SECRET,
        json!({"sub":"test-only-user", "exp":now()+3600}),
    )
}

pub fn keys(kid: &str, secret: &[u8]) -> String {
    json!({"keys":[{"kty":"oct","alg":"HS256","kid":kid,
        "k":base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(secret)}]})
    .to_string()
}

pub struct Server {
    pub url: String,
    pub count: Arc<AtomicUsize>,
    response: Arc<Mutex<(u16, String)>>,
    started: Arc<Semaphore>,
    permits: Arc<Semaphore>,
    task: tokio::task::JoinHandle<()>,
}

impl Server {
    pub async fn new(kid: &str, held: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let count = Arc::new(AtomicUsize::new(0));
        let response = Arc::new(Mutex::new((200, keys(kid, SECRET))));
        let started = Arc::new(Semaphore::new(0));
        let permits = Arc::new(Semaphore::new(if held { 0 } else { 10000 }));
        let (c, r, s, p) = (
            count.clone(),
            response.clone(),
            started.clone(),
            permits.clone(),
        );
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let (c, r, s, p) = (c.clone(), r.clone(), s.clone(), p.clone());
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buf = [0; 1024];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        let n = stream.read(&mut buf).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        request.extend_from_slice(&buf[..n]);
                    }
                    c.fetch_add(1, Ordering::SeqCst);
                    s.add_permits(1);
                    let Ok(permit) = p.acquire().await else {
                        return;
                    };
                    permit.forget();
                    let (status, body) = r.lock().unwrap().clone();
                    let response = format!(
                        "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        Self {
            url,
            count,
            response,
            started,
            permits,
            task,
        }
    }
    pub fn requests(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }
    pub fn respond(&self, status: u16, body: String) {
        *self.response.lock().unwrap() = (status, body);
    }
    pub fn release(&self, count: usize) {
        self.permits.add_permits(count);
    }
    pub async fn request_started(&self) {
        tokio::time::timeout(Duration::from_secs(5), self.started.acquire())
            .await
            .expect("loopback request starts")
            .unwrap()
            .forget();
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
        self.permits.close();
    }
}

pub fn runtime(server: &Server, config: SecurityConfig, timeout_ms: u64) -> SecurityRuntime {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("security.yml"),
        serde_yaml::to_string(&config).unwrap(),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("client.yml"),
        serde_json::to_string(&json!({
            "request":{"timeout":timeout_ms,"connectTimeout":timeout_ms},
            "oauth":{"token":{"key":{"server_url":server.url,"uri":"/jwks","enableHttp2":false}}}
        }))
        .unwrap(),
    )
    .unwrap();
    let runtime_config = RuntimeConfig {
        bootstrap: Default::default(),
        server: Default::default(),
        client: None,
        portal_registry: None,
        direct_registry: Default::default(),
        service_identity: Default::default(),
        config_dir: dir.path().into(),
        external_config_dir: dir.path().into(),
        resolved_values: Default::default(),
        default_config_dir: None,
        embedded_config: &[],
        module_registry: Arc::new(ModuleRegistry::new()),
        cache_registry: None,
        registry_client: None,
    };
    super::load_security_runtime(&runtime_config, true)
        .unwrap()
        .unwrap()
}

pub async fn pending<F: Future>(future: &mut Pin<Box<F>>) {
    std::future::poll_fn(|cx| {
        assert!(
            future.as_mut().poll(cx).is_pending(),
            "verification must be waiting"
        );
        Poll::Ready(())
    })
    .await;
}

pub async fn cooldown() {
    // Only advance Tokio time once the HTTP exchange has completed. JWT claims
    // continue to use wall time; no virtual JWT clock is claimed.
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(31)).await;
    tokio::time::resume();
}
