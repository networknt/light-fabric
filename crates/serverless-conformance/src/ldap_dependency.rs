//! Owned synthetic LDAP bind-denial wire prerequisite; no authentication logic.
use anyhow::{Result, ensure};
use serde_json::Value;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};
pub(crate) struct Ldap {
    pub url: String,
    binds: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}
impl Drop for Ldap {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Ldap {
    pub fn binds(&self) -> usize {
        self.binds.load(Ordering::SeqCst)
    }
    pub async fn start(declaration: &Value) -> Result<Self> {
        ensure!(
            declaration["bindResult"] == "invalidCredentials",
            "closed bind response"
        );
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("ldap://{}", listener.local_addr()?);
        let binds = Arc::new(AtomicUsize::new(0));
        let count = binds.clone();
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut header = [0; 2];
                if stream.read_exact(&mut header).await.is_err()
                    || header[0] != 0x30
                    || header[1] >= 128
                {
                    continue;
                }
                let mut message = vec![0; header[1] as usize];
                if stream.read_exact(&mut message).await.is_err() {
                    continue;
                }
                if message.len() >= 4 && message[0] == 2 && message[1] == 1 && message[3] == 0x60 {
                    count.fetch_add(1, Ordering::SeqCst);
                    let reply = [
                        0x30, 0x0c, 0x02, 0x01, message[2], 0x61, 0x07, 0x0a, 0x01, 0x31, 0x04,
                        0x00, 0x04, 0x00,
                    ];
                    let _ = stream.write_all(&reply).await;
                }
            }
        });
        Ok(Self { url, binds, task })
    }
}
pub(crate) fn substitute(value: &mut Value, url: &str) {
    match value {
        Value::String(s) => *s = s.replace("${dependency:ldap}", url),
        Value::Array(a) => {
            for v in a {
                substitute(v, url)
            }
        }
        Value::Object(o) => {
            for v in o.values_mut() {
                substitute(v, url)
            }
        }
        _ => {}
    }
}
