pub mod support;
use gateway_policy::token::{
    ClientTokenConfig, SidecarTrafficConfig, TokenHandlerConfig, TokenRuntime,
};
use light_runtime::DirectRegistryConfig;
use serverless_core::{handlers::token::TokenHandler, plan::*, *};
use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use support::*;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

fn runtime(url: String) -> Arc<TokenRuntime> {
    let mut client = ClientTokenConfig::default();
    client.oauth.token.server_url = Some(url);
    client.oauth.token.enable_http2 = false;
    client.oauth.token.token_renew_before_expired = 0;
    client.oauth.token.expired_refresh_retry_delay = 0;
    client.oauth.token.client_credentials.client_id = "test-only-client".into();
    client.oauth.token.client_credentials.client_secret = "test-only-secret".into();
    client.oauth.token.client_credentials.uri = "/token".into();
    Arc::new(TokenRuntime::new(
        TokenHandlerConfig::default(),
        SidecarTrafficConfig::default(),
        client,
        DirectRegistryConfig::default(),
        None,
    ))
}
fn pipeline(runtime: Arc<TokenRuntime>, counts: &Arc<Counts>) -> Pipeline {
    let mut r = registry();
    r.register(
        HandlerId::Token,
        Arc::new(
            TokenHandler::from_yaml(
                serde_yaml::from_str("enabled: true\nappliedPathPrefixes: ['/x']").unwrap(),
                runtime,
                "test-only".into(),
            )
            .unwrap(),
        ),
    );
    install(
        &mut r,
        HandlerId::ResponseHeader,
        Binding::Response(Arc::new(Handler::new(
            HandlerId::ResponseHeader,
            Action::Continue,
            counts,
        ))),
    );
    Pipeline::new(
        compile(
            config(
                &[
                    (HandlerId::Token, "token"),
                    (HandlerId::Proxy, "proxy"),
                    (HandlerId::ResponseHeader, "response"),
                ],
                &["token", "proxy", "response"],
            ),
            &r,
        ),
        vec![counts.clone()],
    )
}
struct ObserveBackend(Arc<Counts>, bool);
#[async_trait::async_trait]
impl Backend for ObserveBackend {
    async fn dispatch(
        &self,
        inv: &Invocation,
        _: &ExchangeState,
        _: &Target,
    ) -> Result<GatewayResponse, DispatchError> {
        self.0.backend.fetch_add(1, Ordering::SeqCst);
        if self.1 {
            assert_eq!(inv.headers["authorization"], "Bearer original");
            assert_eq!(inv.headers["x-scope-token"], "Bearer test-only-token");
        } else {
            assert_eq!(inv.headers["authorization"], "Bearer test-only-token");
            assert!(!inv.headers.contains_key("x-scope-token"));
        }
        Ok(response(None))
    }
}
#[tokio::test]
async fn real_pipeline_token_success_cache_secondary_header_and_raw_prefix() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut bytes = [0; 4096];
        assert!(stream.read(&mut bytes).await.unwrap() > 0);
        let body = r#"{"access_token":"test-only-token","expires_in":3600}"#;
        stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
    });
    let c = Arc::new(Counts::default());
    let p = pipeline(runtime(url), &c);
    for secondary in [false, true] {
        let mut inv = invocation();
        inv.path = "/xy".into();
        inv.headers.insert("service_id", "fixed".parse().unwrap());
        if secondary {
            inv.headers
                .insert("authorization", "Bearer original".parse().unwrap());
        }
        assert_eq!(
            p.execute(inv, &ObserveBackend(c.clone(), secondary))
                .await
                .status,
            200
        );
    }
    assert_eq!(n(&c.backend), 2);
    assert_eq!(n(&c.response_entries), 2);
    assert_eq!(n(&c.finalized), 2);
    server.await.unwrap();
}
#[tokio::test]
async fn real_pipeline_provider_denials_redirect_and_timeout_never_dispatch() {
    for (status, body, delay, expected, code) in [
        (
            401,
            r#"{"statusCode":401,"code":"ERR10000","message":"INVALID_AUTH_TOKEN","severity":"ERROR"}"#,
            0,
            401,
            "ERR10000",
        ),
        (401, r#"{"error":"invalid_client"}"#, 0, 500, "ERR14004"),
        (302, "", 0, 500, "ERR14004"),
        (
            200,
            r#"{"access_token":"late-secret","expires_in":3600}"#,
            100,
            504,
            "ERR14017",
        ),
    ] {
        let destination = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let location = format!("http://{}/secret", destination.local_addr().unwrap());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let connection = tokio::time::timeout(Duration::from_secs(1), listener.accept()).await;
            let Ok(connection) = connection else {
                assert!(delay > 0, "non-timeout provider must receive a connection");
                return;
            };
            let (mut stream, _) = connection.unwrap();
            let mut bytes = [0; 4096];
            let received = stream.read(&mut bytes).await.unwrap();
            if received == 0 {
                // The request deadline can cancel after connect but before HTTP
                // bytes are sent. That is a valid outcome only for this timeout.
                assert!(delay > 0, "non-timeout provider must receive request bytes");
                return;
            }
            tokio::time::sleep(Duration::from_millis(delay)).await;
            let _=stream.write_all(format!("HTTP/1.1 {status} Reply\r\nLocation: {location}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await;
        });
        let c = Arc::new(Counts::default());
        let p = pipeline(runtime(url), &c);
        let mut inv = invocation();
        inv.headers.insert("service_id", "fixed".parse().unwrap());
        if delay > 0 {
            inv.deadline = tokio::time::Instant::now() + Duration::from_millis(20);
        }
        let response = p.execute(inv, &FakeBackend::new(&c)).await;
        assert_eq!(response.status.as_u16(), expected);
        let text = String::from_utf8(response.body.unwrap().to_vec()).unwrap();
        assert!(text.contains(code));
        assert!(!text.contains("secret"));
        assert!(!response.headers.contains_key("authorization"));
        assert!(!response.headers.contains_key("x-scope-token"));
        assert_eq!(n(&c.backend), 0);
        assert_eq!(n(&c.response_entries), 1);
        assert_eq!(n(&c.finalized), 1);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), destination.accept())
                .await
                .is_err()
        );
        server.await.unwrap();
    }
}
