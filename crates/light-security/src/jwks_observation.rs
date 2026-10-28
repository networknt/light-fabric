//! Fixed-domain counters around client-send/parse work, never provider/token values.
use std::sync::atomic::{AtomicU64, Ordering};
static NEXT: AtomicU64 = AtomicU64::new(1);
pub(super) struct Fetch {
    id: u64,
    completed: bool,
}
impl Fetch {
    pub(super) fn begin() -> Self {
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        tracing::info!(target:"light_security::jwks_fetch", event="started", fetch_id=id);
        Self {
            id,
            completed: false,
        }
    }
    pub(super) fn completed(&mut self) {
        self.completed = true;
        tracing::info!(target:"light_security::jwks_fetch", event="completed", fetch_id=self.id);
    }
}
impl Drop for Fetch {
    fn drop(&mut self) {
        if !self.completed {
            tracing::info!(target:"light_security::jwks_fetch", event="interrupted_or_failed", fetch_id=self.id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Fetch;
    use std::{
        collections::BTreeMap,
        sync::{Arc, Mutex},
    };
    use tracing::{
        Subscriber,
        field::{Field, Visit},
        span::{Attributes, Id, Record},
    };
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<(u64, String)>>>);
    impl Subscriber for Capture {
        fn enabled(&self, m: &tracing::Metadata<'_>) -> bool {
            m.target() == "light_security::jwks_fetch"
        }
        fn new_span(&self, _: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }
        fn record(&self, _: &Id, _: &Record<'_>) {}
        fn record_follows_from(&self, _: &Id, _: &Id) {}
        fn enter(&self, _: &Id) {}
        fn exit(&self, _: &Id) {}
        fn event(&self, e: &tracing::Event<'_>) {
            #[derive(Default)]
            struct Values {
                id: Option<u64>,
                kind: Option<String>,
                unexpected: bool,
            }
            impl Visit for Values {
                fn record_u64(&mut self, f: &Field, v: u64) {
                    if f.name() == "fetch_id" {
                        self.id = Some(v)
                    } else {
                        self.unexpected = true
                    }
                }
                fn record_str(&mut self, f: &Field, v: &str) {
                    if f.name() == "event" {
                        self.kind = Some(v.into())
                    } else {
                        self.unexpected = true
                    }
                }
                fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {
                    self.unexpected = true;
                }
            }
            let mut v = Values::default();
            e.record(&mut v);
            assert!(!v.unexpected);
            self.0
                .lock()
                .unwrap()
                .push((v.id.unwrap(), v.kind.unwrap()));
        }
    }
    fn verify(c: &Capture, completed: usize, incomplete: usize) {
        let mut ids = BTreeMap::<u64, Vec<String>>::new();
        for (id, kind) in c.0.lock().unwrap().iter() {
            ids.entry(*id).or_default().push(kind.clone());
        }
        assert_eq!(ids.len(), completed + incomplete);
        assert_eq!(
            ids.values()
                .filter(|v| v.as_slice() == ["started", "completed"])
                .count(),
            completed
        );
        assert_eq!(
            ids.values()
                .filter(|v| v.as_slice() == ["started", "interrupted_or_failed"])
                .count(),
            incomplete
        );
    }
    #[test]
    fn normal_error_unwind_and_pending_future_drop_each_emit_one_terminal() {
        use std::{
            future::Future,
            task::{Context, Waker},
        };
        let c = Capture::default();
        tracing::subscriber::with_default(c.clone(), || {
            {
                let mut f = Fetch::begin();
                f.completed();
            }
            {
                let _f = Fetch::begin();
            }
            assert!(
                std::panic::catch_unwind(|| {
                    let _f = Fetch::begin();
                    panic!("synthetic observer unwind");
                })
                .is_err()
            );
            let mut pending = Box::pin(async {
                let _f = Fetch::begin();
                std::future::pending::<()>().await;
            });
            assert!(
                pending
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            drop(pending);
        });
        verify(&c, 1, 3);
    }
    #[test]
    fn concurrent_operations_have_distinct_ids_and_balanced_events() {
        let c = Capture::default();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let c = c.clone();
                std::thread::spawn(move || {
                    tracing::subscriber::with_default(c, || {
                        let mut f = Fetch::begin();
                        f.completed();
                    })
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        verify(&c, 8, 0);
    }
    #[tokio::test]
    async fn actual_client_send_success_http_and_parse_failures_keep_mapping_and_counts() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tracing::instrument::WithSubscriber;
        for (status, body, ok) in [
            (
                200,
                r#"{"keys":[{"kty":"oct","kid":"PRIVATE_KID","k":"YWJjZA","alg":"HS256"}]}"#,
                true,
            ),
            (503, "PRIVATE_HTTP_ERROR", false),
            (200, "PRIVATE_MALFORMED_JSON", false),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut b = [0u8; 4096];
                let n = stream.read(&mut b).await.unwrap();
                assert!(n > 0);
                let response = format!(
                    "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            });
            let mut client = crate::ClientTokenConfig::default();
            client.oauth.token.key.server_url = Some(endpoint);
            client.oauth.token.key.uri = "/PRIVATE_KEYS_PATH".into();
            let source = crate::JwkSource {
                request: client.request.clone(),
                tls: client.tls.clone(),
                client,
                direct_registry: Default::default(),
                registry_client: None,
            };
            let provider = crate::key_providers_for_services(&source, &[])
                .unwrap()
                .remove(0);
            let c = Capture::default();
            let result = crate::fetch_jwks_for_provider(&source, &provider)
                .with_subscriber(c.clone())
                .await;
            server.await.unwrap();
            if ok {
                assert_eq!(result.unwrap().len(), 1);
            } else {
                let e = result.unwrap_err();
                assert_eq!(e.status, 502);
                assert_eq!(e.code, "ERR10056");
            }
            verify(&c, usize::from(ok), usize::from(!ok));
        }
        // A refused connection starts one client-send operation but reaches
        // zero server requests; this is deliberately not called a wire count.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let mut client = crate::ClientTokenConfig::default();
        client.oauth.token.key.server_url = Some(endpoint);
        client.oauth.token.key.uri = "/keys".into();
        let source = crate::JwkSource {
            request: client.request.clone(),
            tls: client.tls.clone(),
            client,
            direct_registry: Default::default(),
            registry_client: None,
        };
        let provider = crate::key_providers_for_services(&source, &[])
            .unwrap()
            .remove(0);
        let c = Capture::default();
        let e = crate::fetch_jwks_for_provider(&source, &provider)
            .with_subscriber(c.clone())
            .await
            .unwrap_err();
        assert_eq!(e.status, 502);
        assert_eq!(e.code, "ERR10056");
        verify(&c, 0, 1);
    }
}
