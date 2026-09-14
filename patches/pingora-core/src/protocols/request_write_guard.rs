//! Optional per-request socket guard, scoped after connection/TLS preparation.
//! The socket retains the scope so error cleanup cannot flush buffered request
//! bytes after cancellation or a pre-initiation deadline failure.
use std::{
    io,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    task::Poll,
};
pub trait RequestWriteGuard: Send + Sync {
    fn first_write(
        &self,
        write: &mut dyn FnMut() -> Poll<io::Result<usize>>,
    ) -> Poll<io::Result<usize>>;
    fn deadline_valid(&self) -> bool;
}
pub struct Scope {
    guard: Arc<dyn RequestWriteGuard>,
    polled: AtomicBool,
    written: AtomicBool,
    blocked: AtomicBool,
}
impl std::fmt::Debug for Scope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RequestWriteScope")
    }
}
impl Scope {
    pub fn new(guard: Arc<dyn RequestWriteGuard>) -> Arc<Self> {
        Arc::new(Self {
            guard,
            polled: AtomicBool::new(false),
            written: AtomicBool::new(false),
            blocked: AtomicBool::new(false),
        })
    }
    pub fn block(&self) {
        self.blocked.store(true, Ordering::Release);
    }
    pub fn wrote(&self) -> bool {
        self.written.load(Ordering::Acquire)
    }
    fn poll(&self, write: &mut dyn FnMut() -> Poll<io::Result<usize>>) -> Poll<io::Result<usize>> {
        if self.blocked.load(Ordering::Acquire) {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "request write fenced",
            )));
        }
        let result = if !self.polled.swap(true, Ordering::AcqRel) {
            self.guard.first_write(write)
        } else if !self.guard.deadline_valid() {
            // TLS may flush control records before application records. Keep
            // enforcing the deadline for every socket write, so a successful
            // control-record write cannot admit later operation bytes. This
            // deliberately bounds the whole request write, not response reads.
            Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "request initiation deadline expired",
            )))
        } else {
            write()
        };
        match &result {
            Poll::Ready(Ok(n)) if *n > 0 => {
                self.written.store(true, Ordering::Release);
            }
            Poll::Ready(Err(_)) => self.block(),
            _ => {}
        }
        result
    }
}
tokio::task_local! {pub static CURRENT:Arc<Scope>;}
pub fn poll_write(
    persisted: &mut Option<Arc<Scope>>,
    write: &mut dyn FnMut() -> Poll<io::Result<usize>>,
) -> Poll<io::Result<usize>> {
    if let Ok(current) = CURRENT.try_with(Arc::clone) {
        if persisted
            .as_ref()
            .is_some_and(|s| s.blocked.load(Ordering::Acquire))
        {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "connection write fenced",
            )));
        }
        *persisted = Some(current);
    }
    match persisted {
        Some(s) => s.poll(write),
        None => write(),
    }
}
/// Cancellation/drop invalidates pending writes, including TLS cleanup writes.
pub struct CancellationFence {
    pub scope: Arc<Scope>,
    pub complete: bool,
}
impl Drop for CancellationFence {
    fn drop(&mut self) {
        if !self.complete {
            self.scope.block();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    struct Guard {
        calls: AtomicUsize,
        valid: AtomicBool,
    }
    impl RequestWriteGuard for Guard {
        fn first_write(
            &self,
            w: &mut dyn FnMut() -> Poll<io::Result<usize>>,
        ) -> Poll<io::Result<usize>> {
            if self.calls.fetch_add(1, Ordering::SeqCst) > 0 || !self.deadline_valid() {
                return Poll::Ready(Err(io::Error::from(io::ErrorKind::TimedOut)));
            }
            w()
        }
        fn deadline_valid(&self) -> bool {
            self.valid.load(Ordering::SeqCst)
        }
    }
    fn guard() -> Arc<Guard> {
        Arc::new(Guard {
            calls: AtomicUsize::new(0),
            valid: AtomicBool::new(true),
        })
    }
    #[tokio::test]
    async fn socket_pending_deadline_never_writes_later_during_cleanup() {
        let g = guard();
        let s = Scope::new(g.clone());
        let mut stored = None;
        CURRENT
            .scope(s.clone(), async {
                assert!(poll_write(&mut stored, &mut || Poll::Pending).is_pending());
                g.valid.store(false, Ordering::SeqCst);
                assert!(matches!(
                    poll_write(&mut stored, &mut || panic!("late socket write")),
                    Poll::Ready(Err(_))
                ));
            })
            .await;
        assert!(matches!(
            poll_write(&mut stored, &mut || panic!("cleanup flushed headers")),
            Poll::Ready(Err(_))
        ));
        assert_eq!(g.calls.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn a_control_record_cannot_allow_operation_bytes_past_deadline() {
        let g = guard();
        let scope = Scope::new(g.clone());
        let mut stored = None;
        CURRENT
            .scope(scope, async {
                assert!(matches!(
                    poll_write(&mut stored, &mut || Poll::Ready(Ok(5))),
                    Poll::Ready(Ok(5))
                ));
                g.valid.store(false, Ordering::SeqCst);
                assert!(matches!(
                    poll_write(&mut stored, &mut || panic!("late operation record")),
                    Poll::Ready(Err(_))
                ));
            })
            .await;
        assert_eq!(g.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancellation_fences_buffered_bytes_and_fresh_guard_cannot_reopen_socket() {
        let g = guard();
        let s = Scope::new(g);
        let mut stored = None;
        let fence = CancellationFence {
            scope: s.clone(),
            complete: false,
        };
        CURRENT
            .scope(s, async {
                let _ = poll_write(&mut stored, &mut || Poll::Pending);
            })
            .await;
        drop(fence);
        CURRENT
            .scope(Scope::new(guard()), async {
                assert!(matches!(
                    poll_write(&mut stored, &mut || panic!("reopened cancelled socket")),
                    Poll::Ready(Err(_))
                ));
            })
            .await;
    }
    #[tokio::test]
    async fn reused_plain_connection_passes_through_real_socket_hook() {
        use crate::{
            connectors::http::Connector, protocols::http::client::HttpSession,
            upstreams::peer::HttpPeer,
        };
        use pingora_http::RequestHeader;
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            for _ in 0..2 {
                let mut b = Vec::new();
                loop {
                    let c = socket.read_u8().await.unwrap();
                    b.push(c);
                    if b.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: keep-alive\r\n\r\n",
                    )
                    .await
                    .unwrap();
            }
        });
        let connector = Connector::new(None);
        let mut peer = HttpPeer::new(addr, false, String::new());
        peer.options.set_http_version(1, 1);
        for expected_reuse in [false, true] {
            let (mut connection, reused) = connector.get_http_session(&peer).await.unwrap();
            assert_eq!(reused, expected_reuse);
            let g = guard();
            let HttpSession::H1(session) = &mut connection else {
                panic!("expected H1")
            };
            let mut req = RequestHeader::build("GET", b"/", Some(1)).unwrap();
            req.insert_header("Host", "localhost").unwrap();
            session
                .write_request_header_guarded(Box::new(req), Some(g.clone()))
                .await
                .unwrap();
            session.read_response().await.unwrap();
            assert_eq!(g.calls.load(Ordering::SeqCst), 1);
            assert_eq!(session.get_status().unwrap().as_u16(), 200);
            session.respect_keepalive();
            connector
                .release_http_session(connection, &peer, Some(std::time::Duration::from_secs(10)))
                .await;
        }
        server.await.unwrap();
    }
}

#[cfg(all(test, feature = "rustls"))]
mod tls_guard_tests {
    use super::*;
    use crate::{
        connectors::http::Connector, protocols::http::client::HttpSession,
        upstreams::peer::HttpPeer,
    };
    use tokio::{io::AsyncReadExt, net::TcpListener};
    struct Expired;
    impl RequestWriteGuard for Expired {
        fn first_write(
            &self,
            _: &mut dyn FnMut() -> Poll<io::Result<usize>>,
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Err(io::Error::from(io::ErrorKind::TimedOut)))
        }
        fn deadline_valid(&self) -> bool {
            false
        }
    }
    #[tokio::test]
    async fn tls_buffered_header_cannot_escape_after_expiry_or_cleanup() {
        pingora_rustls::install_default_crypto_provider();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/keys/server");
        let (certs, key) = pingora_rustls::load_certs_and_key_files(
            root.join("cert.pem").to_str().unwrap(),
            root.join("key.pem").to_str().unwrap(),
        )
        .unwrap()
        .unwrap();
        let tls = pingora_rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .unwrap();
        let acceptor = pingora_rustls::TlsAcceptor::from(Arc::new(tls));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut tls = acceptor.accept(tcp).await.unwrap();
            let mut body = [0u8; 1];
            let read =
                tokio::time::timeout(std::time::Duration::from_secs(2), tls.read(&mut body)).await;
            assert!(
                !matches!(read,Ok(Ok(n)) if n>0),
                "operation bytes escaped the expired guard"
            );
        });
        let connector = Connector::new(None);
        let mut peer = HttpPeer::new(addr, true, "openrusty.org".into());
        // This is a socket-boundary test with an upstream test fixture. Separate mTLS
        // receiver tests exercise chain and workload identity verification.
        peer.options.verify_cert = false;
        peer.options.verify_hostname = false;
        peer.options.set_http_version(1, 1);
        let (mut connection, _) = connector.get_http_session(&peer).await.unwrap();
        let HttpSession::H1(session) = &mut connection else {
            panic!("expected H1")
        };
        let req = pingora_http::RequestHeader::build("POST", b"/", Some(0)).unwrap();
        assert!(session
            .write_request_header_guarded(Box::new(req), Some(Arc::new(Expired)))
            .await
            .is_err());
        session.shutdown().await;
        drop(connection);
        server.await.unwrap();
    }
}
