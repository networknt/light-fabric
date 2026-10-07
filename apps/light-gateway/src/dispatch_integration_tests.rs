// Included in the existing test module to reuse its isolated TLS/runtime harness.
fn write_dispatch_test_config(
    dir: &TempDir,
    port: u16,
    upstream: &str,
    h2: bool,
    pki: &Phase3TestPki,
    chain: &str,
    instance: &str,
) {
    write_phase3_gateway_config(dir, port, upstream, false, h2, false, pki);
    std::fs::write(dir.path().join("handler.yml"),format!("handlers: [correlation, security, access-control, proxy]\npaths:\n  - path: /github/repos/*\n    method: GET\n    exec: [{chain}]\ndefaultHandlers: []\n")).unwrap();
    std::fs::write(dir.path().join("correlation.yml"), "enabled: true\n").unwrap();
    std::fs::write(dir.path().join("security.yml"),"enableVerifyJwt: true\nenableVerifyScope: false\nignoreJwtExpiry: false\nbootstrapFromKeyService: false\n").unwrap();
    // The role-denial lifecycle exercises the real ACL handler with no matching
    // anonymous permission. Valid JWT/registered-role controls remain the live matrix.
    std::fs::write(
        dir.path().join("access-control.yml"),
        "enabled: true\ndefaultDeny: true\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("rule.yml"),
        "endpointRules: {}\nruleBodies: {}\n",
    )
    .unwrap();
    let db_port = std::env::var("POLICY_ADMIN_TEST_PORT").unwrap();
    let url_file = dir.path().join("fixture-database-url");
    std::fs::write(&url_file,format!("postgres://operations_gateway_runtime:localfixture@127.0.0.1:{db_port}/operations?sslmode=disable&options=-csearch_path%3Dgateway_ops%2Coperational_meta")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&url_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let evidence = format!(
        "enabled: true\ndispatchObservationEnabled: true\ncontractVersion: 2\ndatabaseUrlFile: {}\nbindingId: 11111111-1111-7111-8111-111111111111\nbindingDigest: sha256:{}\nhostId: 22222222-2222-7222-8222-222222222222\nenvironment: test\nserverHost: 127.0.0.1\nport: {db_port}\ntlsMode: DISABLE\nserviceOwner: light-gateway\nschema: gateway_ops\nexpectedDatabase: operations\nminimumSchemaGeneration: 2\ncredentialGeneration: 1\ngatewayInstance: {instance}\nmaximumPendingRecords: 8192\nmaximumPendingBytes: 67108864\nsinkEndpoint: http://127.0.0.1:9/collector\nsinkBearerTokenFile: ''\npublisherBatchRecords: 128\npublisherPollMs: 250\npublisherRetryMs: 1000\npublisherLeaseSeconds: 30\ndeliveredRetentionSeconds: 3600\n",
        url_file.display(),
        "b".repeat(64)
    );
    std::fs::write(dir.path().join("gateway-evidence.yml"), evidence).unwrap();
}

async fn dispatch_test_receipts(pool: &sqlx::PgPool, correlation: &str) -> Vec<serde_json::Value> {
    let digest = sha256_digest(correlation);
    timeout(TokioDuration::from_secs(5),async {
        loop {
            let value:Option<sqlx::types::Json<serde_json::Value>>=sqlx::query_scalar("SELECT json_agg(t ORDER BY (dispatch_observation->>'dispatchSequence')::bigint) FROM (SELECT event_type,status_code,dispatch_observation FROM gateway_ops.gateway_evidence_spool_t WHERE correlation_digest=$1 AND dispatch_observation IS NOT NULL) t").bind(&digest).fetch_one(pool).await.unwrap();
            if let Some(value)=value {
                let rows=value.0.as_array().unwrap();
                if rows.iter().any(|r|r["dispatch_observation"]["dispatchPhase"]=="terminal") {return rows.clone();}
            }
            sleep(TokioDuration::from_millis(10)).await;
        }
    }).await.expect("complete dispatch receipt")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the disposable G03 observer/policy PostgreSQL fixture"]
async fn dispatch_observer_real_h1_h2_reuse_denial_connect_failure_and_writer_failure() {
    let db_port = std::env::var("POLICY_ADMIN_TEST_PORT").unwrap();
    let pool = sqlx::PgPool::connect(&format!(
        "postgres://postgres@127.0.0.1:{db_port}/operations?sslmode=disable"
    ))
    .await
    .unwrap();
    let pki = phase3_test_pki();
    let mut measured = Vec::new();
    for h2 in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let sent = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let conn = connections.clone();
        let sends = sent.clone();
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut tls = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![pki.certificate.clone()],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(pki.private_key_der.clone())),
            )
            .unwrap();
        tls.alpn_protocols = vec![b"h2".to_vec()];
        let acceptor = TlsAcceptor::from(Arc::new(tls));
        let upstream = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                conn.fetch_add(1, Ordering::SeqCst);
                let sends = sends.clone();
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    if h2 {
                        let tls = acceptor.accept(socket).await.unwrap();
                        let mut connection = h2::server::handshake(tls).await.unwrap();
                        while let Some(Ok((_request, mut respond))) = connection.accept().await {
                            if sends.fetch_add(1, Ordering::SeqCst) == 2 {
                                respond.send_reset(h2::Reason::REFUSED_STREAM);
                                continue;
                            }
                            let mut stream = respond
                                .send_response(
                                    http::Response::builder()
                                        .status(200)
                                        .header("content-length", "2")
                                        .body(())
                                        .unwrap(),
                                    false,
                                )
                                .unwrap();
                            stream.send_data(Bytes::from_static(b"{}"), true).unwrap();
                        }
                    } else {
                        loop {
                            let mut request = Vec::new();
                            let mut b = [0u8; 1];
                            while !request.ends_with(b"\r\n\r\n") {
                                if socket.read(&mut b).await.unwrap_or(0) == 0 {
                                    return;
                                }
                                request.push(b[0]);
                            }
                            if sends.fetch_add(1, Ordering::SeqCst) == 2 {
                                return;
                            } // fail the reused connection once
                            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\n{}").await.unwrap();
                        }
                    }
                });
            }
        });
        let dir = TempDir::new().unwrap();
        let external = TempDir::new().unwrap();
        let port = free_tcp_port();
        let gateway = format!("127.0.0.1:{port}").parse().unwrap();
        write_dispatch_test_config(
            &dir,
            port,
            &format!("{}://{address}", if h2 { "https" } else { "http" }),
            h2,
            &pki,
            "correlation, proxy",
            if h2 { "g03-h2" } else { "g03-h1" },
        );
        let running = LightRuntimeBuilder::new(PingoraTransport::new(GatewayApp::default()))
            .with_config_dir(dir.path())
            .with_external_config_dir(external.path())
            .build()
            .start()
            .await
            .unwrap();
        wait_for_tcp(gateway).await;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        for n in 0..2 {
            let correlation = uuid::Uuid::now_v7().to_string();
            let start = Instant::now();
            let response = client
                .get(format!(
                    "http://{gateway}/github/repos/a/b/issues/25{}",
                    if n == 1 { "/comments" } else { "" }
                ))
                .header("x-correlation-id", &correlation)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            response.bytes().await.unwrap();
            let rows = dispatch_test_receipts(&pool, &correlation).await;
            assert_eq!(rows.len(), 4);
            let terminal = &rows[3]["dispatch_observation"];
            assert_eq!(terminal["upstreamAttemptCount"], 1);
            assert_eq!(terminal["upstreamHandoffCount"], 1);
            assert_eq!(terminal["observationComplete"], true);
            measured.push(start.elapsed().as_micros());
        }
        assert_eq!(sent.load(Ordering::SeqCst), 2);
        assert_eq!(
            connections.load(Ordering::SeqCst),
            1,
            "both requests must reuse the actual upstream connection"
        );
        let correlation = uuid::Uuid::now_v7().to_string();
        let response = client
            .get(format!("http://{gateway}/github/repos/a/b/issues/25"))
            .header("x-correlation-id", &correlation)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        response.bytes().await.unwrap();
        let rows = dispatch_test_receipts(&pool, &correlation).await;
        let terminal = &rows.last().unwrap()["dispatch_observation"];
        assert_eq!(
            terminal["upstreamAttemptCount"], 2,
            "both actual retry connection acquisitions must be counted"
        );
        assert_eq!(terminal["upstreamHandoffCount"], 2);
        assert_eq!(rows.len(), 6);
        assert_eq!(sent.load(Ordering::SeqCst), 4);
        assert_eq!(terminal["observationComplete"], true);
        running.shutdown().await.unwrap();
        upstream.abort();
    }
    // Actual security and ACL early returns, failed connection, writer failure,
    // and quota overflow. All use disposable/local endpoints only.
    for (chain, instance, expected, break_writer) in [
        (
            "correlation, security, access-control, proxy",
            "g03-auth-denial",
            401,
            false,
        ),
        (
            "correlation, access-control, proxy",
            "g03-acl-denial",
            403,
            false,
        ),
        ("correlation, proxy", "g03-connect-failure", 502, false),
        ("correlation, proxy", "g03-writer-failure", 502, true),
    ] {
        let dir = TempDir::new().unwrap();
        let external = TempDir::new().unwrap();
        let port = free_tcp_port();
        let gateway = format!("127.0.0.1:{port}").parse().unwrap();
        write_dispatch_test_config(
            &dir,
            port,
            "http://127.0.0.1:9",
            false,
            &pki,
            chain,
            instance,
        );
        let running = LightRuntimeBuilder::new(PingoraTransport::new(GatewayApp::default()))
            .with_config_dir(dir.path())
            .with_external_config_dir(external.path())
            .build()
            .start()
            .await
            .unwrap();
        wait_for_tcp(gateway).await;
        if break_writer {
            sqlx::query("REVOKE INSERT ON gateway_ops.gateway_evidence_spool_t FROM operations_gateway_runtime").execute(&pool).await.unwrap();
        }
        let correlation = uuid::Uuid::now_v7().to_string();
        let response = reqwest::Client::new()
            .get(format!("http://{gateway}/github/repos/a/b/issues/25"))
            .header("x-correlation-id", &correlation)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), expected);
        response.bytes().await.unwrap();
        if break_writer {
            let count:i64=sqlx::query_scalar("SELECT count(*) FROM gateway_ops.gateway_evidence_spool_t WHERE correlation_digest=$1").bind(sha256_digest(&correlation)).fetch_one(&pool).await.unwrap();
            assert_eq!(count, 0);
            sqlx::query("GRANT INSERT ON gateway_ops.gateway_evidence_spool_t TO operations_gateway_runtime").execute(&pool).await.unwrap();
        } else {
            let rows = dispatch_test_receipts(&pool, &correlation).await;
            let terminal = &rows.last().unwrap()["dispatch_observation"];
            assert_eq!(terminal["observationComplete"], true);
            if expected == 401 || expected == 403 {
                assert_eq!(rows.len(), 2);
                assert_eq!(terminal["upstreamAttemptCount"], 0);
                assert_eq!(terminal["upstreamHandoffCount"], 0);
            } else {
                assert!(terminal["upstreamAttemptCount"].as_u64().unwrap() >= 1);
                assert_eq!(terminal["upstreamHandoffCount"], 0);
            }
        }
        running.shutdown().await.unwrap();
    }
    for before_handoff in [true, false] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (arrived_tx, arrived_rx) = oneshot::channel();
        let (respond_tx, respond_rx) = oneshot::channel();
        let upstream = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_complete_http_request(&mut socket).await;
            let _ = arrived_tx.send(());
            let _ = respond_rx.await;
            let _ = socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 4194304\r\nConnection: close\r\n\r\n",
                )
                .await;
            let _ = socket.write_all(&vec![b'x'; 4194304]).await;
        });
        let dir = TempDir::new().unwrap();
        let external = TempDir::new().unwrap();
        let port = free_tcp_port();
        let gateway = format!("127.0.0.1:{port}").parse().unwrap();
        write_dispatch_test_config(
            &dir,
            port,
            &format!("http://{address}"),
            false,
            &pki,
            "correlation, proxy",
            if before_handoff {
                "g03-disconnect-before"
            } else {
                "g03-disconnect-after"
            },
        );
        let running = LightRuntimeBuilder::new(PingoraTransport::new(GatewayApp::default()))
            .with_config_dir(dir.path())
            .with_external_config_dir(external.path())
            .build()
            .start()
            .await
            .unwrap();
        wait_for_tcp(gateway).await;
        // Hold start-event durability to establish the before-handoff disconnect
        // boundary deterministically. No existing/deployed store is involved.
        let mut lock = pool.begin().await.unwrap();
        if before_handoff {
            sqlx::query("SELECT host_id FROM gateway_ops.gateway_evidence_quota_t WHERE host_id='22222222-2222-7222-8222-222222222222' FOR UPDATE").fetch_one(&mut *lock).await.unwrap();
        }
        let correlation = uuid::Uuid::now_v7().to_string();
        let mut client = TcpStream::connect(gateway).await.unwrap();
        client.write_all(format!("GET /github/repos/a/b/issues/25 HTTP/1.1\r\nHost: localhost\r\nX-Correlation-Id: {correlation}\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
        if before_handoff {
            sleep(TokioDuration::from_millis(50)).await;
            drop(client);
            lock.commit().await.unwrap();
            timeout(TokioDuration::from_secs(5), arrived_rx)
                .await
                .unwrap()
                .unwrap();
        } else {
            lock.rollback().await.unwrap();
            timeout(TokioDuration::from_secs(5), arrived_rx)
                .await
                .unwrap()
                .unwrap();
            drop(client);
        }
        let _ = respond_tx.send(());
        let rows = dispatch_test_receipts(&pool, &correlation).await;
        let terminal = &rows.last().unwrap()["dispatch_observation"];
        assert_eq!(terminal["observationComplete"], true);
        assert_eq!(terminal["upstreamAttemptCount"], 1);
        assert_eq!(terminal["upstreamHandoffCount"], 1);
        // A downstream disconnect does not imply non-dispatch. These actual
        // sends must remain visible even when the caller never receives output.
        running.shutdown().await.unwrap();
        upstream.abort();
    }
    if let Ok(path) = std::env::var("G03_DURABILITY_MEASUREMENT_FILE") {
        std::fs::write(path,serde_json::to_vec_pretty(&json!({"localAllowedRequestAndDurableReceiptMicros":measured,"includesProxyAndPolling":true,"productionLatencyClaim":false})).unwrap()).unwrap();
    }
}
