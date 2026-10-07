// Included in the existing isolated Gateway test module. Test-generated JWTs
// and a local JWKS endpoint; no owner signing key, service token or GitHub call.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires disposable G03 PostgreSQL fixture"]
async fn dispatch_observer_real_signed_roles_six_probe_matrix() {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use hmac::{Hmac, Mac};
    let key = uuid::Uuid::now_v7().to_string() + &uuid::Uuid::now_v7().to_string();
    let sign = |role: &str| {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","kid":"g03-local","typ":"JWT"}"#);
        let claims = json!({"iss":"g03-local","sub":"local-caller","user_id":"33333333-3333-7333-8333-333333333333","role":role,"exp":Utc::now().timestamp()+120});
        let input = format!(
            "{header}.{}",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap())
        );
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(key.as_bytes()).unwrap();
        mac.update(input.as_bytes());
        format!(
            "{input}.{}",
            URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
        )
    };
    let jwks = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let jwks_address = jwks.local_addr().unwrap();
    let body = json!({"keys":[{"kty":"oct","kid":"g03-local","alg":"HS256","k":URL_SAFE_NO_PAD.encode(key.as_bytes())}]}).to_string();
    let jwks_task = tokio::spawn(async move {
        loop {
            let (mut socket, _) = jwks.accept().await.unwrap();
            let _ = read_complete_http_request(&mut socket).await;
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
        }
    });
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_address = upstream.local_addr().unwrap();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = calls.clone();
    let upstream_task = tokio::spawn(async move {
        loop {
            let (mut socket, _) = upstream.accept().await.unwrap();
            let _ = read_complete_http_request(&mut socket).await;
            observed.fetch_add(1, Ordering::SeqCst);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                .await
                .unwrap();
        }
    });
    let dir = TempDir::new().unwrap();
    let external = TempDir::new().unwrap();
    let port = free_tcp_port();
    let gateway = format!("127.0.0.1:{port}").parse().unwrap();
    let pki = phase3_test_pki();
    write_dispatch_test_config(
        &dir,
        port,
        &format!("http://{upstream_address}"),
        false,
        &pki,
        "correlation, security, access-control, proxy",
        "g03-signed-matrix",
    );
    std::fs::write(dir.path().join("client.yml"),format!("oauth:\n  token:\n    key:\n      server_url: http://{jwks_address}\n      uri: /keys\n")).unwrap();
    std::fs::write(
        dir.path().join("rule.yml"),
        r#"
ruleBodies:
  g03-role:
    common: Y
    ruleId: g03-role
    ruleName: G03 local role
    ruleType: req-acc
    actions:
      - actionClassName: com.networknt.rule.RoleBasedAccessControlAction
endpointRules:
  /github/repos/{owner}/{repo}/issues/{issue_number}@get:
    permission: {roles: admin}
    req-acc: [g03-role]
  /github/repos/{owner}/{repo}/issues/{issue_number}/comments@get:
    permission: {roles: admin}
    req-acc: [g03-role]
"#,
    )
    .unwrap();
    let running = LightRuntimeBuilder::new(PingoraTransport::new(GatewayApp::default()))
        .with_config_dir(dir.path())
        .with_external_config_dir(external.path())
        .build()
        .start()
        .await
        .unwrap();
    wait_for_tcp(gateway).await;
    let db_port = std::env::var("POLICY_ADMIN_TEST_PORT").unwrap();
    let pool = sqlx::PgPool::connect(&format!(
        "postgres://postgres@127.0.0.1:{db_port}/operations?sslmode=disable"
    ))
    .await
    .unwrap();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();
    let mut audits = std::collections::HashSet::new();
    let mut records = 0;
    for (role, status) in [(Some("admin"), 200), (Some("user"), 403), (None, 401)] {
        for suffix in ["", "/comments"] {
            let correlation = uuid::Uuid::now_v7().to_string();
            let mut request = client
                .get(format!(
                    "http://{gateway}/github/repos/a/b/issues/25{suffix}"
                ))
                .header("x-correlation-id", &correlation);
            if let Some(role) = role {
                request = request.bearer_auth(sign(role));
            }
            let response = request.send().await.unwrap();
            assert_eq!(response.status().as_u16(), status);
            let _ = response.bytes().await.unwrap();
            let rows = dispatch_test_receipts(&pool, &correlation).await;
            records += rows.len();
            let terminal = &rows.last().unwrap()["dispatch_observation"];
            assert_eq!(terminal["observationComplete"], true);
            assert_eq!(
                terminal["upstreamAttemptCount"],
                if status == 200 { 1 } else { 0 }
            );
            assert_eq!(
                terminal["upstreamHandoffCount"],
                if status == 200 { 1 } else { 0 }
            );
            assert!(audits.insert(terminal["requestAuditId"].as_str().unwrap().to_string()));
        }
    }
    assert_eq!(audits.len(), 6);
    assert_eq!(records, 16);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    running.shutdown().await.unwrap();
    upstream_task.abort();
    jwks_task.abort();
}
