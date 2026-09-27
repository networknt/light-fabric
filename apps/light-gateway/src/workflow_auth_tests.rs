#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mcp_entry_requires_user_and_validates_optional_app_scope() {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use hmac::{Hmac, Mac};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    let key = b"workflow-entry-test-signing-key-32-bytes";
    let sign = |claims: &serde_json::Value| {
        let header =
            URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","kid":"workflow-entry","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).unwrap());
        let input = format!("{header}.{payload}");
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(key).unwrap();
        mac.update(input.as_bytes());
        format!(
            "{input}.{}",
            URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
        )
    };
    let jwks = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let jwks_addr = jwks.local_addr().unwrap();
    let jwks_body = json!({"keys":[{"kty":"oct","kid":"workflow-entry","alg":"HS256",
        "k":URL_SAFE_NO_PAD.encode(key)}]})
    .to_string();
    let jwks_task = tokio::spawn(async move {
        loop {
            let (mut socket, _) = jwks.accept().await.unwrap();
            let mut bytes = [0u8; 4096];
            let _ = socket.read(&mut bytes).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{jwks_body}",
                jwks_body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });
    let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = backend.local_addr().unwrap();
    let backend_calls = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&backend_calls);
    let backend_task = tokio::spawn(async move {
        loop {
            let (mut socket, _) = backend.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                let count = socket.read(&mut chunk).await.unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..count]);
                let text = String::from_utf8_lossy(&request);
                if let Some(end) = text.find("\r\n\r\n") {
                    let length = text[..end]
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let end = request
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .unwrap();
            let incoming: serde_json::Value = serde_json::from_slice(&request[end + 4..]).unwrap();
            seen.fetch_add(1, Ordering::SeqCst);
            let body =
                json!({"jsonrpc":"2.0","id":incoming["id"],"result":{"resultType":"complete",
                "isError":false,"content":[{"type":"text","text":"saved"}],
                "structuredContent":{"result":"saved"}}})
                .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        }
    });

    let config_dir = TempDir::new().unwrap();
    let external_dir = TempDir::new().unwrap();
    let port = free_tcp_port();
    let address = format!("127.0.0.1:{port}").parse().unwrap();
    let write =
        |name: &str, value: String| std::fs::write(config_dir.path().join(name), value).unwrap();
    write(
        "server.yml",
        format!(
            "ip: 127.0.0.1\nhttpPort: {port}\nhttpsPort: 8443\nadvertisedAddress: 127.0.0.1\ndynamicPort: false\nstartOnRegistryFailure: true\nenvironment: dev\nenableHttp: true\nenableHttps: false\nenableRegistry: false\nserviceId: gateway-test\nshutdownGracefulPeriod: 100\n"
        ),
    );
    write("handler.yml", "handlers: [unified-security, mcp]\npaths:\n  - path: /mcp\n    method: POST\n    exec: [unified-security, mcp]\ndefaultHandlers: []\n".into());
    write(
        "unified-security.yml",
        "enabled: true\npathPrefixAuths:\n  - prefix: /mcp\n    jwt: true\n".into(),
    );
    write(
        "security.yml",
        "enableVerifyJwt: true\nbootstrapFromKeyService: true\n".into(),
    );
    write(
        "client.yml",
        format!(
            "oauth:\n  token:\n    key:\n      server_url: http://{jwks_addr}\n      uri: /keys\n"
        ),
    );
    write(
        "access-control.yml",
        "enabled: true\ndefaultDeny: true\n".into(),
    );
    write("rule.yml", r#"ruleBodies:
  sync:
    ruleId: sync
    ruleName: User role
    ruleType: req-acc
    common: Y
    conditionLanguage: cel
    conditionSecurityProfile: strict
    expression: "'role' in auditInfo.subject_claims.ClaimsMap && auditInfo.subject_claims.ClaimsMap.role == 'admin'"
endpointRules:
  sync_probe@call:
    permission: {roles: admin}
    req-acc: [sync]
"#.into());
    write(
        light_pingora::MCP_ROUTER_FILE,
        format!(
            r#"enabled: true
path: /mcp
tools:
  - name: sync_probe
    endpointName: sync_probe
    serviceId: test-service
    targetHost: http://{backend_addr}
    path: /mcp
    method: POST
    endpoint: sync_probe@call
    apiType: mcp
    backendMcpProtocol: stateless
    backendCredentialMode: anonymous
    sessionIndependent: true
    toolMetadata:
      runtime:
        allowPrivateTargetHost: true
    inputSchema: {{type: object}}
"#
        ),
    );
    let runtime = LightRuntimeBuilder::new(PingoraTransport::new(GatewayApp::default()))
        .with_config_dir(config_dir.path())
        .with_external_config_dir(external_dir.path())
        .build();
    let running = runtime.start().await.unwrap();
    wait_for_tcp(address).await;

    let now = chrono::Utc::now().timestamp();
    let user = sign(
        &json!({"iss":"workflow-entry","aud":"workflow","exp":now+300,
        "token_use":"user","uid":"user-a","sub":"user-a","host":"host-a","role":"admin"}),
    );
    let viewer = sign(
        &json!({"iss":"workflow-entry","aud":"workflow","exp":now+300,
        "token_use":"user","uid":"user-b","sub":"user-b","host":"host-a","role":"viewer"}),
    );
    let app = sign(
        &json!({"iss":"workflow-entry","aud":"workflow","exp":now+300,
        "token_use":"app","client_id":"cli-a","sub":"cli-a","host":"host-a"}),
    );
    let client = reqwest::Client::new();
    let url = format!("http://{address}/mcp");
    let body = json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
        "params":{"name":"sync_probe","arguments":{},
            "_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28",
                "io.modelcontextprotocol/clientCapabilities":{}}}});
    let call = |authorization: Option<&str>, scope: Option<&str>| {
        let mut request = client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .header("MCP-Protocol-Version", "2026-07-28")
            .header("Mcp-Method", "tools/call")
            .header("Mcp-Name", "sync_probe")
            .json(&body);
        if let Some(token) = authorization {
            request = request.header("Authorization", token);
        }
        if let Some(token) = scope {
            request = request.header("X-Scope-Token", token);
        }
        request
    };
    for scope in [None, Some(format!("Bearer {app}"))] {
        let response = call(Some(&format!("Bearer {user}")), scope.as_deref())
            .send()
            .await
            .unwrap();
        let status = response.status();
        let response_body = response.text().await.unwrap();
        assert_eq!(status, 200, "{response_body}");
        assert!(response_body.contains("saved"), "{response_body}");
    }
    assert_eq!(backend_calls.load(Ordering::SeqCst), 2);
    for (authorization, scope) in [
        (None, Some(format!("Bearer {app}"))),
        (
            Some("Bearer invalid".to_owned()),
            Some(format!("Bearer {app}")),
        ),
        (Some(format!("Bearer {app}")), None),
        (
            Some(format!("Bearer {user}")),
            Some("Bearer invalid".to_owned()),
        ),
        (
            Some(format!("Bearer {user}")),
            Some(format!("Bearer {viewer}")),
        ),
    ] {
        let response = call(authorization.as_deref(), scope.as_deref())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401);
    }
    let response = call(
        Some(&format!("Bearer {viewer}")),
        Some(&format!("Bearer {app}")),
    )
    .send()
    .await
    .unwrap();
    let status = response.status();
    let response_body = response.text().await.unwrap();
    assert!(
        status == 403 || response_body.contains("-32001"),
        "{status}: {response_body}"
    );
    assert_eq!(backend_calls.load(Ordering::SeqCst), 2);
    running.shutdown().await.unwrap();
    backend_task.abort();
    jwks_task.abort();
}
