//! Single-attempt outbound HTTP/1 transport for Workflow-authorized effects.
//! Connection/TLS preparation happens before the socket guard. No redirect,
//! reconnect, HTTP/2 fallback, or replay exists in this adapter.
use bytes::Bytes;
use pingora::{
    connectors::http::Connector,
    http::{RequestHeader, ResponseHeader},
    protocols::{http::client::HttpSession, request_write_guard::RequestWriteGuard},
    upstreams::peer::HttpPeer,
};
use std::{io, sync::Arc, task::Poll, time::Duration};
use workflow_action::guard::SendGuard;

pub struct ActionWriteGuard(pub Arc<SendGuard>);
impl RequestWriteGuard for ActionWriteGuard {
    fn first_write(
        &self,
        write: &mut dyn FnMut() -> Poll<io::Result<usize>>,
    ) -> Poll<io::Result<usize>> {
        self.0.poll_first_write(write)
    }
    fn deadline_valid(&self) -> bool {
        self.0.deadline_valid()
    }
}
#[derive(Debug, thiserror::Error)]
pub enum Failure {
    #[error("unqualified action transport or request framing")]
    Unsupported,
    #[error("action transport failed; consult the send guard for initiation state")]
    Transport,
    #[error("action response exceeded its limit")]
    ResponseLimit,
}
pub struct Response {
    pub header: Box<ResponseHeader>,
    pub body: Bytes,
}

/// `peer` must come from the approved destination registry, never from model
/// input. It owns the TLS identity/trust and resolved destination. This routine
/// intentionally has no token selection or destination-policy fallback.
pub async fn execute(
    connector: &Connector,
    peer: &HttpPeer,
    mut request: RequestHeader,
    body: Bytes,
    guard: Arc<SendGuard>,
    response_limit: usize,
    timeout: Duration,
) -> Result<Response, Failure> {
    if response_limit == 0
        || timeout.is_zero()
        || request.headers.contains_key("transfer-encoding")
        || request.headers.contains_key("upgrade")
        || request.headers.contains_key("expect")
    {
        return Err(Failure::Unsupported);
    }
    // The adapter, not an untrusted caller, owns framing of the exact bytes.
    request.remove_header("content-length");
    request
        .insert_header("Content-Length", body.len().to_string())
        .map_err(|_| Failure::Unsupported)?;
    // One outer timeout drops the connection on any uncertain result. There is
    // deliberately no retry loop, including for a stale pooled connection.
    tokio::time::timeout(timeout, async {
        let (mut connection, _reused) = connector
            .get_http_session(peer)
            .await
            .map_err(|_| Failure::Transport)?;
        let HttpSession::H1(session) = &mut connection else {
            return Err(Failure::Unsupported);
        };
        session
            .write_request_header_guarded(
                Box::new(request),
                Some(Arc::new(ActionWriteGuard(guard))),
            )
            .await
            .map_err(|_| Failure::Transport)?;
        if !body.is_empty() {
            session
                .write_body(&body)
                .await
                .map_err(|_| Failure::Transport)?;
        }
        session
            .finish_body()
            .await
            .map_err(|_| Failure::Transport)?;
        // Cap informational responses as well as the final body.
        let mut final_header = false;
        for _ in 0..8 {
            session
                .read_response()
                .await
                .map_err(|_| Failure::Transport)?;
            if !session
                .get_status()
                .ok_or(Failure::Transport)?
                .is_informational()
            {
                final_header = true;
                break;
            }
        }
        if !final_header {
            return Err(Failure::ResponseLimit);
        }
        let header = Box::new(session.resp_header().ok_or(Failure::Transport)?.clone());
        let mut bytes = Vec::new();
        while let Some(chunk) = session
            .read_body_bytes()
            .await
            .map_err(|_| Failure::Transport)?
        {
            if chunk.len() > response_limit.saturating_sub(bytes.len()) {
                return Err(Failure::ResponseLimit);
            }
            bytes.extend_from_slice(&chunk);
        }
        session.respect_keepalive();
        connector
            .release_http_session(connection, peer, Some(Duration::from_secs(10)))
            .await;
        Ok(Response {
            header,
            body: Bytes::from(bytes),
        })
    })
    .await
    .map_err(|_| Failure::Transport)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    use uuid::Uuid;
    use workflow_action::guard::SendState;
    use workflow_action::*;
    fn decision() -> Decision {
        let digest = request_digest("POST", "https://target/tool", "tool", b"{}");
        Decision {
            binding: Binding {
                host_id: Uuid::now_v7(),
                user_id: Uuid::now_v7(),
                grant_id: Uuid::now_v7(),
                run_id: Uuid::now_v7(),
                action_id: Uuid::now_v7(),
                attempt_id: Uuid::now_v7(),
                calling_app: "workflow".into(),
                request_digest: digest.clone(),
                request_bytes: 2,
                response_byte_limit: 1024,
                cost_unit_limit: 1,
                tool_ref: Uuid::now_v7(),
                target: "https://target/tool".into(),
                contract_digest: digest.clone(),
                policy_digest: digest.clone(),
                disclosure_digest: digest.clone(),
                claims_digest: digest,
                grant_generation: 1,
                run_generation: 1,
                budget_generation: 1,
                action_generation: 1,
                execution_class: ExecutionClass::Standard,
                depth: 0,
                maximum_depth: 4,
                parent_action_id: None,
                deadline: chrono::Utc::now() + chrono::Duration::minutes(5),
            },
            decision_id: Uuid::now_v7(),
            owner: Owner {
                gateway_service: "gateway".into(),
                replica: Uuid::now_v7(),
                boot: Uuid::now_v7(),
                fencing_generation: 1,
            },
            generation: 1,
            lease_ms: 5000,
        }
    }

    fn armed(expired: bool) -> Arc<SendGuard> {
        let d = decision();
        let now = Instant::now();
        let g = Arc::new(
            SendGuard::new(
                if expired {
                    now - Duration::from_secs(6)
                } else {
                    now
                },
                d.clone(),
            )
            .unwrap(),
        );
        g.acknowledge(&d, true).unwrap();
        g
    }
    fn request() -> RequestHeader {
        let mut r = RequestHeader::build("POST", b"/effect", Some(1)).unwrap();
        r.insert_header("Host", "localhost").unwrap();
        r
    }
    #[tokio::test]
    async fn adapter_reuses_connection_with_a_new_guard_for_each_operation() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            for _ in 0..2 {
                let mut h = Vec::new();
                while !h.ends_with(b"\r\n\r\n") {
                    h.push(socket.read_u8().await.unwrap())
                }
                let mut body = [0; 2];
                socket.read_exact(&mut body).await.unwrap();
                assert_eq!(&body, b"{}");
                socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\n{}",
                    )
                    .await
                    .unwrap();
            }
        });
        let connector = Connector::new(None);
        let mut peer = HttpPeer::new(addr, false, String::new());
        peer.options.set_http_version(1, 1);
        for _ in 0..2 {
            let guard = armed(false);
            let r = execute(
                &connector,
                &peer,
                request(),
                Bytes::from_static(b"{}"),
                guard.clone(),
                1024,
                Duration::from_secs(2),
            )
            .await
            .unwrap();
            assert_eq!(r.header.status.as_u16(), 200);
            assert_eq!(r.body, Bytes::from_static(b"{}"));
            assert_eq!(guard.state(), SendState::Started);
        }
        server.await.unwrap();
    }
    #[tokio::test]
    async fn adapter_expired_during_preparation_never_writes_operation() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut b = [0];
            assert_eq!(s.read(&mut b).await.unwrap(), 0);
        });
        let connector = Connector::new(None);
        let mut peer = HttpPeer::new(addr, false, String::new());
        peer.options.set_http_version(1, 1);
        let guard = armed(true);
        assert!(
            execute(
                &connector,
                &peer,
                request(),
                Bytes::from_static(b"{}"),
                guard.clone(),
                1024,
                Duration::from_secs(2)
            )
            .await
            .is_err()
        );
        assert!(guard.abort_not_initiated());
        server.await.unwrap();
    }
    #[tokio::test]
    async fn adapter_does_not_retry_a_replayable_body_after_partial_send() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut b = [0; 32];
            s.read_exact(&mut b).await.unwrap();
            drop(s);
            assert!(
                tokio::time::timeout(Duration::from_millis(200), listener.accept())
                    .await
                    .is_err(),
                "hidden reconnect after partial operation"
            );
        });
        let connector = Connector::new(None);
        let mut peer = HttpPeer::new(addr, false, String::new());
        peer.options.set_http_version(1, 1);
        let guard = armed(false);
        assert!(
            execute(
                &connector,
                &peer,
                request(),
                Bytes::from(vec![b'x'; 1024 * 1024]),
                guard.clone(),
                1024,
                Duration::from_secs(2)
            )
            .await
            .is_err()
        );
        assert_eq!(guard.state(), SendState::Started);
        assert!(!guard.abort_not_initiated());
        server.await.unwrap();
    }
}
