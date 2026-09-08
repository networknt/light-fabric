// Live Pingora + signed JWTs + PostgreSQL. Run only against a disposable audit DB.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires LLM_AUDIT_TEST_DATABASE_URL and audit migrations"]
async fn dual_token_live_gateway_and_postgres_audit() {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use hmac::{Hmac, Mac};
    use sha2::{Digest, Sha256};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use uuid::Uuid;
    let database =
        std::env::var("LLM_AUDIT_TEST_DATABASE_URL").expect("dedicated audit DB required");
    let pool = sqlx::PgPool::connect(&database).await.unwrap();
    let key = b"phase2-test-signing-key-at-least-32-bytes";
    let sign = |claims: &serde_json::Value| {
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","kid":"phase2","typ":"JWT"}"#);
        let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).unwrap());
        let input = format!("{header}.{body}");
        let mut mac = Hmac::<Sha256>::new_from_slice(key).unwrap();
        mac.update(input.as_bytes());
        format!(
            "{input}.{}",
            URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
        )
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    let provider = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider_addr = provider.local_addr().unwrap();
    let provider_task = tokio::spawn(async move {
        loop {
            let (mut socket, _) = provider.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                let n = socket.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                bytes.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&bytes);
                if let Some(end) = text.find("\r\n\r\n") {
                    let len = text[..end]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if bytes.len() >= end + 4 + len {
                        break;
                    }
                }
            }
            let text = String::from_utf8_lossy(&bytes);
            assert!(!text.to_ascii_lowercase().contains("x-scope-token"));
            assert!(
                text.to_ascii_lowercase()
                    .contains("authorization: bearer provider-only")
            );
            seen.fetch_add(1, Ordering::SeqCst);
            let body = r#"{"id":"test","object":"chat.completion","created":1,"model":"mock","choices":[{"index":0,"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1,"total_tokens":4}}"#;
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
        }
    });
    let jwks = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let jwks_addr = jwks.local_addr().unwrap();
    let jwks_body=json!({"keys":[{"kty":"oct","kid":"phase2","alg":"HS256","k":URL_SAFE_NO_PAD.encode(key)}]}).to_string();
    let jwks_task = tokio::spawn(async move {
        loop {
            let (mut s, _) = jwks.accept().await.unwrap();
            let mut b = [0u8; 4096];
            let _ = s.read(&mut b).await;
            s.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",jwks_body.len(),jwks_body).as_bytes()).await.unwrap();
        }
    });
    let dir = TempDir::new().unwrap();
    let external = TempDir::new().unwrap();
    let wal = TempDir::new().unwrap();
    let port = free_tcp_port();
    let address = format!("127.0.0.1:{port}").parse().unwrap();
    let write = |name: &str, value: String| std::fs::write(dir.path().join(name), value).unwrap();
    write(
        "server.yml",
        format!(
            "ip: 127.0.0.1\nhttpPort: {port}\nhttpsPort: 8443\nadvertisedAddress: 127.0.0.1\ndynamicPort: false\nstartOnRegistryFailure: true\nenvironment: dev\nenableHttp: true\nenableHttps: false\nenableRegistry: false\nserviceId: phase2-gateway\nshutdownGracefulPeriod: 100\n"
        ),
    );
    write("handler.yml","handlers: [correlation, unified-security, limit, access-control, llm]\npaths:\n  - path: /v1/chat/completions\n    method: POST\n    exec: [correlation, unified-security, limit, access-control, llm]\n  - path: /v1/responses\n    method: POST\n    exec: [correlation, unified-security, limit, access-control, llm]\ndefaultHandlers: []\n".into());
    write(
        "unified-security.yml",
        "enabled: true\npathPrefixAuths:\n  - prefix: /v1\n    jwt: true\n".into(),
    );
    write(
        "security.yml",
        "enableVerifyJwt: true\nignoreJwtExpiry: true\nbootstrapFromKeyService: true\n".into(),
    ); // strict profile must still reject expired tokens
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
    write(
        "rule.yml",
        r#"
endpointRules:
  /v1/chat/completions@post:
    permission: {roles: admin}
    req-acc: [portal]
  /v1/responses@post:
    permission: {roles: admin}
    req-acc: [portal]
ruleBodies:
  portal:
    ruleId: portal
    ruleName: Portal RBAC
    ruleType: req-acc
    common: Y
    conditionLanguage: cel
    conditionSecurityProfile: strict
    expression: 'true'
    actions:
      - actionClassName: com.networknt.rule.RoleBasedAccessControlAction
"#
        .into(),
    );
    let host = Uuid::now_v7().to_string();
    let agent = Uuid::now_v7().to_string();
    let client_id = Uuid::now_v7().to_string();
    let user_id = Uuid::now_v7().to_string();
    let config = json!({"enabled":true,"developmentFixtures":true,
        "auditRuntime":{"directory":wal.path(),"gatewayInstance":"phase2-test","hostId":host,"persistentVolume":true,"terminalCommitBeforeResponse":true,"sinkDatabaseUrlEnv":"LLM_AUDIT_TEST_DATABASE_URL","sinkPollMs":10},
        "agentDelegation":{"endpoints":{"/v1/chat/completions@post":true,"/v1/responses@post":false},"userIssuer":"phase2-issuer","userAudience":"user-gateway","bindings":[
            {"clientId":client_id,"agentDefId":agent,"hostId":host,"environment":"test","issuer":"phase2-issuer","audience":"agent-gateway","scopes":["llm.invoke"],"routeAlias":"agent-private","policyDigest":format!("sha256:{}","a".repeat(64)),"registrationVersion":1,"expiresAt":1}]},
        "providers":{"mock":{"providerProtocol":"openai_chat","materialGeneration":1,"baseUrl":format!("http://{provider_addr}/v1"),"endpointAuth":{"mode":"bearer","credential_ref":"env:LIGHT_PHASE2_PROVIDER_KEY"}}},
        "deployments":{"mock":{"provider":"mock","model":"mock","concurrency":2,"prices":{"generate":{"operation":"generate","version":1,"inputMicrosPerMillion":1,"outputMicrosPerMillion":1}},"conformanceDigest":"a".repeat(64)}},
        "aliases":{"agent-private":{"operations":["generate"],"deployments":["mock"],"internal":true,"boundPrincipal":agent,"audit":"local_durable","maxInputTokens":1000,"maxOutputTokens":100}}
    });
    write(LLM_ROUTER_FILE, serde_yaml::to_string(&config).unwrap());
    unsafe {
        std::env::set_var("LIGHT_PHASE2_PROVIDER_KEY", "provider-only");
    }
    let runtime = LightRuntimeBuilder::new(PingoraTransport::new(GatewayApp::default()))
        .with_config_dir(dir.path())
        .with_external_config_dir(external.path())
        .build();
    let running = runtime.start().await.unwrap();
    wait_for_tcp(address).await;
    unsafe {
        std::env::remove_var("LIGHT_PHASE2_PROVIDER_KEY");
    }
    let now = chrono::Utc::now().timestamp();
    let user = json!({"iss":"phase2-issuer","aud":"user-gateway","exp":now+300,"iat":now,"uid":user_id,"sub":user_id,"client_id":"portal-ui","host":host,"role":"admin"});
    let workload = json!({"iss":"phase2-issuer","aud":"agent-gateway","exp":now+300,"iat":now,"client_id":client_id,"sub":client_id,"host":host,"env":"test","scp":["llm.invoke"],"routeAlias":"agent-private"});
    let http = reqwest::Client::new();
    let request_body =
        json!({"model":"agent-private","messages":[{"role":"user","content":"hello"}]});
    for (label, u, w, status) in [
        ("valid", user.clone(), workload.clone(), 200),
        (
            "user-role",
            {
                let mut v = user.clone();
                v["role"] = "viewer".into();
                v
            },
            workload.clone(),
            403,
        ),
        (
            "user-audience",
            {
                let mut v = user.clone();
                v["aud"] = "portal-only".into();
                v
            },
            workload.clone(),
            401,
        ),
        (
            "user-expiry",
            {
                let mut v = user.clone();
                v["exp"] = (now - 1).into();
                v
            },
            workload.clone(),
            401,
        ),
        (
            "workload-expiry",
            user.clone(),
            {
                let mut v = workload.clone();
                v["exp"] = (now - 1).into();
                v
            },
            401,
        ),
        (
            "workload-audience",
            user.clone(),
            {
                let mut v = workload.clone();
                v["aud"] = "other".into();
                v
            },
            401,
        ),
        (
            "cross-host",
            user.clone(),
            {
                let mut v = workload.clone();
                v["host"] = Uuid::now_v7().to_string().into();
                v
            },
            403,
        ),
        (
            "scope",
            user.clone(),
            {
                let mut v = workload.clone();
                v["scp"] = json!([]);
                v
            },
            403,
        ),
        (
            "route",
            user.clone(),
            {
                let mut v = workload.clone();
                v["routeAlias"] = "another".into();
                v
            },
            403,
        ),
        (
            "unknown-client",
            user.clone(),
            {
                let mut v = workload.clone();
                v["client_id"] = Uuid::now_v7().to_string().into();
                v
            },
            403,
        ),
    ] {
        let response = http
            .post(format!("http://{address}/v1/chat/completions"))
            .bearer_auth(sign(&u))
            .header("X-Scope-Token", format!("Bearer {}", sign(&w)))
            .json(&request_body)
            .send()
            .await
            .unwrap();
        let got = response.status().as_u16();
        let body = response.text().await.unwrap();
        assert_eq!(got, status, "{label}: {body}");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "denied request dispatched: {label}"
        );
    }
    for token in [
        None,
        Some(sign(&workload)),
        Some("Bearer lad1.invalid".into()),
        Some("Bearer invalid".into()),
    ] {
        let mut req = http
            .post(format!("http://{address}/v1/chat/completions"))
            .bearer_auth(sign(&user))
            .json(&request_body);
        if let Some(token) = token {
            req = req.header("X-Scope-Token", token);
        }
        assert_eq!(req.send().await.unwrap().status().as_u16(), 401);
    }
    let duplicate = http
        .post(format!("http://{address}/v1/chat/completions"))
        .bearer_auth(sign(&user))
        .header("X-Scope-Token", format!("Bearer {}", sign(&workload)))
        .header("X-Scope-Token", format!("Bearer {}", sign(&workload)))
        .json(&request_body)
        .send()
        .await
        .unwrap();
    assert_eq!(duplicate.status().as_u16(), 401);
    // Optional profile must conceal both an internal alias and an unknown alias equally.
    for model in ["agent-private", "not-found"] {
        let response = http
            .post(format!("http://{address}/v1/responses"))
            .bearer_auth(sign(&user))
            .json(&json!({"model":model,"input":"hello"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 404);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    tokio::time::timeout(Duration::from_secs(10),async {
        loop {
            let count:i64=sqlx::query_scalar("SELECT count(*) FROM llm_audit_event_t WHERE host_id=$1 AND event_kind='request_finished' AND authorization_context IS NOT NULL")
                .bind(&host).fetch_one(&pool).await.unwrap();
            if count>=17 {break;} tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }).await.expect("audit evidence persisted");
    let success:String=sqlx::query_scalar("SELECT authorization_context::text FROM llm_audit_event_t WHERE host_id=$1 AND event_kind='request_finished' AND status='complete' LIMIT 1").bind(&host).fetch_one(&pool).await.unwrap();
    let audit: serde_json::Value = serde_json::from_str(&success).unwrap();
    assert_eq!(audit["userId"], user_id);
    assert_eq!(audit["workloadClientId"], client_id);
    assert_eq!(audit["agentDefId"], agent);
    assert_eq!(audit["userAccessDecision"], "allowed");
    assert_eq!(audit["agentAssignmentDecision"], "allowed");
    assert!(!success.contains(&sign(&user)));
    assert!(!success.contains(&sign(&workload)));
    let expected = format!("{:x}", Sha256::digest(agent.as_bytes()));
    let principal:String=sqlx::query_scalar("SELECT principal_digest FROM llm_audit_event_t WHERE host_id=$1 AND status='complete' LIMIT 1").bind(&host).fetch_one(&pool).await.unwrap();
    assert_eq!(principal, expected);
    running.shutdown().await.unwrap();
    provider_task.abort();
    jwks_task.abort();
    pool.close().await;
}
