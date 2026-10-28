use gateway_policy::{HeaderWriter, Request, RequestHead, RequestRead};
use http::{HeaderMap, Method};
use light_pingora::{CorrelationConfig, apply_correlation_request, apply_correlation_response};
use pingora::{http::ResponseHeader, prelude::Session};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
};

struct NeutralRequest {
    headers: HeaderMap,
    method: Method,
}
impl HeaderWriter for NeutralRequest {
    type Error = String;
    fn insert_header(
        &mut self,
        name: impl AsRef<str>,
        value: impl AsRef<str>,
    ) -> Result<(), Self::Error> {
        let name = http::header::HeaderName::from_bytes(name.as_ref().as_bytes())
            .map_err(|e| e.to_string())?;
        let value = http::HeaderValue::from_str(value.as_ref()).map_err(|e| e.to_string())?;
        self.headers.insert(name, value);
        Ok(())
    }
    fn remove_header(&mut self, name: &str) {
        self.headers.remove(name);
    }
}
impl Request for NeutralRequest {
    type Error = String;
    type Writer = Self;
    fn req_header_mut(&mut self) -> &mut Self {
        self
    }
}
impl RequestRead for NeutralRequest {
    fn req_header(&self) -> RequestHead<'_> {
        RequestHead {
            headers: &self.headers,
            method: &self.method,
        }
    }
}

async fn session(wire: &str) -> (Session, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut client = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (server, _) = listener.accept().await.unwrap();
    client.write_all(wire.as_bytes()).await.unwrap();
    let mut session = Session::new_h1(Box::new(pingora::protocols::l4::stream::Stream::from(
        server,
    )));
    assert!(session.as_downstream_mut().read_request().await.unwrap());
    (session, client)
}

#[tokio::test]
async fn correlation_adapter_and_core_preserve_first_value_case_and_disabled_state() {
    for wire in [
        "GET /api HTTP/1.1\r\nHost: localhost\r\nX-CoRrelation-ID: first\r\nx-correlation-id: second\r\nX-Traceability-Id: trace\r\n\r\n",
        "GET /api HTTP/1.1\r\nHost: localhost\r\n\r\n",
    ] {
        for enabled in [true, false] {
            let (mut session, _client) = session(wire).await;
            let mut neutral = NeutralRequest {
                headers: session.req_header().headers.clone(),
                method: session.req_header().method.clone(),
            };
            let config = CorrelationConfig {
                enabled,
                autogen_correlation_id: false,
                ..Default::default()
            };
            let expected =
                gateway_policy::correlation::apply_correlation_request(&mut neutral, &config)
                    .unwrap();
            let actual = apply_correlation_request(&mut session, &config).unwrap();
            assert_eq!(actual, expected);
            assert_eq!(session.req_header().headers, neutral.headers);
            let mut response = ResponseHeader::build(200, None).unwrap();
            apply_correlation_response(&mut response, &actual).unwrap();
            let mut neutral_response = NeutralRequest {
                headers: HeaderMap::new(),
                method: Method::GET,
            };
            gateway_policy::correlation::apply_correlation_response(
                &mut neutral_response,
                &expected,
            )
            .unwrap();
            assert_eq!(response.headers, neutral_response.headers);
        }
    }
}

#[tokio::test]
async fn correlation_generation_is_java_shaped_and_does_not_replace_existing_values() {
    let (mut session, _client) = session("GET / HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
    let state = apply_correlation_request(&mut session, &CorrelationConfig::default()).unwrap();
    let id = state.correlation_id.unwrap();
    assert_eq!(id.len(), 22);
    assert!(
        id.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    );
    assert_eq!(session.req_header().headers["x-correlation-id"], id);
    let second = apply_correlation_request(&mut session, &CorrelationConfig::default()).unwrap();
    assert_eq!(second.correlation_id.as_deref(), Some(id.as_str()));
}

#[tokio::test]
async fn header_global_prefix_order_and_partial_failure_match_real_adapter() {
    use light_pingora::{HeaderConfig, apply_header_request, apply_header_response};
    for path in ["/api/item", "/other", "/apix"] {
        for invalid in [false, true] {
            let mut config: HeaderConfig = serde_yaml::from_str(
                r#"
enabled: true
request:
  remove: x-old
  update: {x-stage: global}
response:
  remove: x-old
  update: {x-stage: global}
pathPrefixHeader:
  /api:
    request:
      remove: x-stage
      update: {x-stage: prefix}
    response:
      remove: x-stage
      update: {x-stage: prefix}
"#,
            )
            .unwrap();
            if invalid {
                config
                    .request
                    .update
                    .insert("bad header".into(), "value".into());
                config
                    .response
                    .update
                    .insert("bad header".into(), "value".into());
            }
            let (mut session, _client) =
                session("GET / HTTP/1.1\r\nHost: localhost\r\nx-old: one\r\nx-old: two\r\n\r\n")
                    .await;
            let mut neutral = NeutralRequest {
                headers: session.req_header().headers.clone(),
                method: Method::GET,
            };
            let expected =
                gateway_policy::header::apply_header_request(&mut neutral, &config, path);
            let actual = apply_header_request(&mut session, &config, path);
            assert_eq!(actual.is_err(), expected.is_err());
            assert_eq!(session.req_header().headers, neutral.headers);
            assert!(!neutral.headers.contains_key("x-old"));
            if !invalid {
                assert_eq!(
                    neutral.headers["x-stage"],
                    if path.starts_with("/api") {
                        "prefix"
                    } else {
                        "global"
                    }
                );
            }
            let mut response = ResponseHeader::build(200, None).unwrap();
            response.append_header("x-old", "one").unwrap();
            response.append_header("x-old", "two").unwrap();
            let mut neutral = NeutralRequest {
                headers: response.headers.clone(),
                method: Method::GET,
            };
            let expected =
                gateway_policy::header::apply_header_response(&mut neutral, &config, path);
            let actual = apply_header_response(&mut response, &config, path);
            assert_eq!(actual.is_err(), expected.is_err());
            assert_eq!(response.headers, neutral.headers);
        }
    }
    let default = HeaderConfig::default();
    let decoded: HeaderConfig = serde_yaml::from_str("{}").unwrap();
    assert!(!default.enabled && !decoded.enabled);
    assert!(default.request.remove.is_empty() && default.path_prefix_header.is_empty());
}

#[tokio::test]
async fn cors_preflight_denial_host_fallback_and_response_headers_match() {
    use light_pingora::{
        CorsConfig, CorsRequestOutcome, apply_cors_response, evaluate_cors_request,
    };
    let config: CorsConfig = serde_yaml::from_str(
        r#"
allowedOrigins: [https://allowed.example]
allowedMethods: [GET, POST]
pathPrefixAllowed:
  /private:
    allowedOrigins: [https://private.example]
    allowedMethods: [PUT]
"#,
    )
    .unwrap();
    let cases = [
        (
            "OPTIONS",
            "/api",
            "Origin: https://allowed.example\r\nAccess-Control-Request-Headers: X-Custom\r\n",
            Some(200),
        ),
        (
            "GET",
            "/api",
            "Origin: https://denied.example\r\n",
            Some(403),
        ),
        ("GET", "/api", "Origin: https://localhost:443\r\n", None),
        (
            "GET",
            "/private/item",
            "Origin: https://allowed.example\r\n",
            Some(403),
        ),
        (
            "OPTIONS",
            "/private/item",
            "Origin: https://private.example\r\n",
            Some(200),
        ),
        (
            "OPTIONS",
            "/api",
            "Access-Control-Request-Method: POST\r\n",
            Some(200),
        ),
        (
            "GET",
            "/api",
            "oRiGin: https://allowed.example\r\nOrigin: https://denied.example\r\n",
            None,
        ),
        ("GET", "/api", "", None),
    ];
    for (method, path, headers, expected_status) in cases {
        let wire = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\n{headers}\r\n");
        let (session, _client) = session(&wire).await;
        let neutral = NeutralRequest {
            headers: session.req_header().headers.clone(),
            method: session.req_header().method.clone(),
        };
        let expected =
            gateway_policy::cors::evaluate_cors_request(&neutral, &config, path, "https", 443);
        let actual = evaluate_cors_request(&session, &config, path, "https", 443);
        assert_eq!(actual, expected);
        let status = match &actual {
            CorsRequestOutcome::Respond { status, .. } => Some(*status),
            _ => None,
        };
        assert_eq!(status, expected_status);
        let headers = match actual {
            CorsRequestOutcome::Continue(Some(headers))
            | CorsRequestOutcome::Respond { headers, .. } => Some(headers),
            _ => None,
        };
        if let Some(headers) = headers {
            let mut response = ResponseHeader::build(200, None).unwrap();
            let mut neutral = NeutralRequest {
                headers: HeaderMap::new(),
                method: Method::GET,
            };
            apply_cors_response(&mut response, &headers).unwrap();
            gateway_policy::cors::apply_cors_response(&mut neutral, &headers).unwrap();
            assert_eq!(response.headers, neutral.headers);
            assert_eq!(response.headers["access-control-allow-credentials"], "true");
            assert_eq!(response.headers["access-control-max-age"], "3600");
        }
    }
}

#[tokio::test]
async fn api_key_optional_required_repeated_headers_and_denials_match() {
    use light_pingora::{ApiKeyConfig, verify_api_key, verify_required_api_key};
    let config: ApiKeyConfig = serde_yaml::from_str(
        r#"
pathPrefixAuths:
  - {pathPrefix: /api, headerName: X-Key, apiKey: secret}
  - {pathPrefix: /api/admin, headerName: X-Key, apiKey: admin}
"#,
    )
    .unwrap();
    for (path, headers, optional_ok, required_ok) in [
        ("/api", "X-Key: secret\r\n", true, true),
        ("/api", "x-key: secret\r\nX-Key: wrong\r\n", true, true),
        ("/api", "X-Key: wrong\r\n", false, false),
        ("/api", "", false, false),
        ("/other", "", true, false),
        ("/api/admin", "X-Key: secret\r\n", false, false),
    ] {
        let (session, _client) = session(&format!(
            "GET {path} HTTP/1.1\r\nHost: localhost\r\n{headers}\r\n"
        ))
        .await;
        let neutral = NeutralRequest {
            headers: session.req_header().headers.clone(),
            method: Method::GET,
        };
        let actual = verify_api_key(&session, &config, path);
        let expected = gateway_policy::apikey::verify_api_key(&neutral, &config, path);
        assert_eq!(actual.is_ok(), optional_ok);
        assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
        let actual = verify_required_api_key(&session, &config, path);
        let expected = gateway_policy::apikey::verify_required_api_key(&neutral, &config, path);
        assert_eq!(actual.is_ok(), required_ok);
        assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
        if let Err(error) = actual {
            assert_eq!(error.status, 401);
            assert_eq!(error.code, "ERR10075");
        }
    }
}

#[tokio::test]
async fn basic_auth_challenges_anonymous_bearer_and_path_denial_match() {
    use light_pingora::{BasicAuthConfig, verify_basic_auth};
    let config: BasicAuthConfig = serde_yaml::from_str(
        r#"
enabled: true
allowAnonymous: true
allowBearerToken: true
users:
  alice: {password: secret, paths: [/api]}
  anonymous: {paths: [/public]}
  bearer: {paths: [/public]}
"#,
    )
    .unwrap();
    for (path, authorization, status) in [
        ("/api", Some("Basic YWxpY2U6c2VjcmV0"), None),
        ("/private", Some("Basic YWxpY2U6c2VjcmV0"), Some(403)),
        ("/api", Some("Basic YWxpY2U6d3Jvbmc="), Some(401)),
        ("/api", Some("Basic !!!"), Some(401)),
        ("/api", Some("Digest unknown"), Some(401)),
        ("/public", Some("bEaReR caller"), None),
        ("/api", Some("Bearer caller"), Some(401)),
        ("/public", None, None),
        ("/api", None, Some(401)),
    ] {
        let headers = authorization
            .map(|value| format!("Authorization: {value}\r\n"))
            .unwrap_or_default();
        let (session, _client) = session(&format!(
            "GET {path} HTTP/1.1\r\nHost: localhost\r\n{headers}\r\n"
        ))
        .await;
        let neutral = NeutralRequest {
            headers: session.req_header().headers.clone(),
            method: Method::GET,
        };
        let actual = verify_basic_auth(&session, &config, path);
        let expected = gateway_policy::basic_auth::verify_basic_auth(&neutral, &config, path);
        assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
        assert_eq!(actual.as_ref().err().map(|e| e.status), status);
        if let Err(error) = actual {
            if error.status == 401 {
                assert_eq!(error.code, "ERR10005");
                assert_eq!(
                    error.headers,
                    [(
                        "www-authenticate".into(),
                        "Basic realm=\"light-gateway\"".into()
                    )]
                );
            }
        }
    }
}

#[tokio::test]
async fn rate_limit_warm_state_and_verified_identity_match_without_network_dependencies() {
    use light_pingora::{AuthPrincipal, LimitConfig, RateLimitRuntime, check_rate_limit};
    for key in ["server", "address", "client", "user"] {
        let config: LimitConfig = serde_yaml::from_str(&format!(
            "enabled: true\nheadersAlwaysSet: true\nrateLimit: 2/h\nkey: {key}\n"
        ))
        .unwrap();
        let adapter_runtime = RateLimitRuntime::new(config.clone());
        let core_runtime = RateLimitRuntime::new(config);
        let (session, _client) = session("GET /api HTTP/1.1\r\nHost: localhost\r\nX-Forwarded-For: 192.0.2.10, 192.0.2.11\r\n\r\n").await;
        let neutral = NeutralRequest {
            headers: session.req_header().headers.clone(),
            method: Method::GET,
        };
        let principal = AuthPrincipal {
            client_id: Some("client".into()),
            user_id: Some("user".into()),
            ..Default::default()
        };
        for request in 0..3 {
            let actual = check_rate_limit(&session, &adapter_runtime, Some(&principal), "/api");
            let expected = gateway_policy::rate_limit::check_rate_limit(
                &neutral,
                &core_runtime,
                Some(&principal),
                "/api",
                None,
            );
            assert_eq!(actual.is_ok(), request < 2);
            assert_eq!(actual.is_ok(), expected.is_ok());
            match (actual, expected) {
                (Ok(Some(a)), Ok(Some(b))) => {
                    assert_eq!(a.limit, b.limit);
                    assert_eq!(a.remaining, b.remaining);
                    assert!(a.reset.abs_diff(b.reset) <= 1);
                    assert_eq!(a.remaining, 1 - request);
                }
                (Err(a), Err(b)) => {
                    assert_eq!(a.status, b.status);
                    assert_eq!(a.code, "ERR10077");
                    assert_eq!(a.code, b.code);
                    assert_eq!(a.message, b.message);
                    assert_eq!(a.headers[0..2], b.headers[0..2]);
                }
                pair => panic!("unexpected parity result {pair:?}"),
            }
        }
        if key == "client" || key == "user" {
            let actual = check_rate_limit(&session, &adapter_runtime, None, "/api").unwrap_err();
            let expected = gateway_policy::rate_limit::check_rate_limit(
                &neutral,
                &core_runtime,
                None,
                "/api",
                None,
            )
            .unwrap_err();
            assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
            assert_eq!(actual.status, 401);
        }
    }
}

#[tokio::test]
async fn new_limit_resolvers_are_separate_opt_ins_and_headers_never_authenticate() {
    use light_pingora::{AuthPrincipal, LimitConfig, RateLimitRuntime, check_rate_limit};
    for key in ["akamai", "header-client-id"] {
        let config: LimitConfig = serde_yaml::from_str(&format!(
            "enabled: true\nheadersAlwaysSet: true\nrateLimit: 1/h\nkey: {key}\n"
        ))
        .unwrap();
        let adapter = RateLimitRuntime::new(config.clone());
        let core = RateLimitRuntime::new(config);
        let (session, _client) = session("GET / HTTP/1.1\r\nHost: localhost\r\ntRuE-cLiEnT-iP: 192.0.2.10\r\nTrue-Client-IP: 192.0.2.11\r\nClient-Id: header-client\r\n\r\n").await;
        let neutral = NeutralRequest {
            headers: session.req_header().headers.clone(),
            method: Method::GET,
        };
        let principal = AuthPrincipal {
            user_id: Some("verified-user".into()),
            client_id: Some("verified-client".into()),
            ..Default::default()
        };
        if key == "header-client-id" {
            let error = check_rate_limit(&session, &adapter, None, "/").unwrap_err();
            assert_eq!(error.status, 401);
            assert_eq!(error.code, "ERR10002");
            let user_only = AuthPrincipal {
                user_id: Some("verified-user".into()),
                ..Default::default()
            };
            assert_eq!(
                check_rate_limit(&session, &adapter, Some(&user_only), "/")
                    .unwrap_err()
                    .status,
                401
            );
        }
        for request in 0..2 {
            let actual = check_rate_limit(&session, &adapter, Some(&principal), "/");
            let expected = gateway_policy::rate_limit::check_rate_limit(
                &neutral,
                &core,
                Some(&principal),
                "/",
                None,
            );
            assert_eq!(actual.is_ok(), request == 0);
            assert_eq!(actual.is_ok(), expected.is_ok());
        }
    }
    assert!(matches!(
        LimitConfig::default().key,
        light_pingora::LimitKey::Server
    ));
}

#[tokio::test]
async fn shared_jwt_verification_optional_scope_and_claim_mutations_match() {
    use light_pingora::{SecurityRuntime, verify_jwt_request};
    use serde_json::json;
    let test_key = [
        uuid::Uuid::new_v4().as_bytes().as_slice(),
        uuid::Uuid::new_v4().as_bytes().as_slice(),
    ]
    .concat();
    let mut runtime = SecurityRuntime::with_test_hs256_key("wp11-parity", &test_key).await;
    runtime
        .config
        .pass_through_claims
        .insert("sub".into(), "X-Subject".into());
    let sign = |purpose: &str| {
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
        header.kid = Some("wp11-parity".into());
        jsonwebtoken::encode(&header, &json!({"iss":"wp11-parity","aud":"workflow","exp":4102444800u64,"token_use":purpose,"sub":"verified-test-user"}), &jsonwebtoken::EncodingKey::from_secret(&test_key)).unwrap()
    };
    let user = sign("user");
    let app = sign("app");
    for (authorization, scope, ok) in [
        (Some(format!("Bearer {user}")), None, true),
        (
            Some(format!("Bearer {user}")),
            Some(format!("Bearer {app}")),
            true,
        ),
        (
            Some(format!("Bearer {user}")),
            Some(format!("Bearer {user}")),
            false,
        ),
        (
            Some(format!("Bearer {user}")),
            Some("Bearer invalid".into()),
            false,
        ),
        (None, None, false),
        (Some("Basic ignored".into()), None, false),
    ] {
        let mut headers = String::new();
        if let Some(authorization) = authorization {
            headers.push_str(&format!("Authorization: {authorization}\r\n"));
        }
        if let Some(scope) = scope {
            headers.push_str(&format!("X-Scope-Token: {scope}\r\n"));
        }
        let (mut session, _client) = session(&format!(
            "GET /api HTTP/1.1\r\nHost: localhost\r\n{headers}\r\n"
        ))
        .await;
        let mut neutral = NeutralRequest {
            headers: session.req_header().headers.clone(),
            method: Method::GET,
        };
        let actual = verify_jwt_request(&mut session, &runtime, "/api").await;
        let expected =
            gateway_policy::security::verify_jwt_request(&mut neutral, &runtime, "/api").await;
        assert_eq!(actual.is_ok(), ok);
        assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
        assert_eq!(session.req_header().headers, neutral.headers);
        assert_eq!(neutral.headers.contains_key("x-subject"), ok);
    }
}

#[tokio::test]
async fn unified_authentication_denial_anonymous_and_factor_order_match() {
    use light_pingora::{
        ApiKeyConfig, BasicAuthConfig, UnifiedSecurityConfig, verify_unified_security,
    };
    let config: UnifiedSecurityConfig = serde_yaml::from_str(
        r#"
anonymousPrefixes: [/public]
pathPrefixAuths:
  - {prefix: /basic, basic: true}
  - {prefix: /key, apikey: true}
  - prefix: /composed
    authentication:
      allOf: [{type: apiKey}, {type: hmac, profile: webhook}]
"#,
    )
    .unwrap();
    let basic: BasicAuthConfig =
        serde_yaml::from_str("enabled: true\nusers: [{username: alice, password: secret}]\n")
            .unwrap();
    let key: ApiKeyConfig = serde_yaml::from_str(
        "pathPrefixAuths: [{pathPrefix: /, headerName: X-Key, apiKey: secret}]\n",
    )
    .unwrap();
    for (path, headers, expected_status, expected_profile) in [
        ("/public", "", None, None),
        ("/unmatched", "", Some(403), None),
        (
            "/basic",
            "Authorization: Basic YWxpY2U6c2VjcmV0\r\n",
            None,
            None,
        ),
        (
            "/basic",
            "Authorization: Bearer unsupported\r\n",
            Some(401),
            None,
        ),
        ("/key", "", Some(401), None),
        ("/composed", "X-Key: secret\r\n", None, Some("webhook")),
        ("/composed", "X-Key: wrong\r\n", Some(401), None),
    ] {
        let (mut session, _client) = session(&format!(
            "GET {path} HTTP/1.1\r\nHost: localhost\r\n{headers}\r\n"
        ))
        .await;
        let mut neutral = NeutralRequest {
            headers: session.req_header().headers.clone(),
            method: Method::GET,
        };
        let actual = verify_unified_security(
            &mut session,
            &config,
            Some(&basic),
            Some(&key),
            None,
            path,
            "GET",
        )
        .await;
        let expected = gateway_policy::unified_security::verify_unified_security(
            &mut neutral,
            &config,
            Some(&basic),
            Some(&key),
            None,
            path,
            "GET",
        )
        .await;
        assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
        assert_eq!(actual.as_ref().err().map(|e| e.status), expected_status);
        assert_eq!(
            actual.as_ref().ok().and_then(|v| v.hmac_profile.as_deref()),
            expected_profile
        );
        assert_eq!(session.req_header().headers, neutral.headers);
    }
}
#[tokio::test]
async fn token_adapter_and_core_preserve_headers_cache_and_dependency_errors() {
    use light_client::{
        ClientConfig, ClientOauthConfig, OAuthClientCredentialsConfig, OAuthTokenConfig,
    };
    use light_pingora::{SidecarTrafficConfig, TokenHandlerConfig, TokenRuntime};
    use light_runtime::DirectRegistryConfig;
    use tokio::io::AsyncReadExt;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = [0u8; 4096];
        socket.read(&mut buffer).await.unwrap();
        let body = r#"{"access_token":"fixture-token","expires_in":300}"#;
        socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .as_bytes(),
            )
            .await
            .unwrap();
    });
    let runtime = TokenRuntime::new(
        TokenHandlerConfig {
            enabled: true,
            applied_path_prefixes: vec!["/api".into()],
        },
        SidecarTrafficConfig::default(),
        ClientConfig {
            oauth: ClientOauthConfig {
                token: OAuthTokenConfig {
                    server_url: Some(format!("http://{address}")),
                    client_credentials: OAuthClientCredentialsConfig {
                        client_id: "fixture".into(),
                        client_secret: "fixture".into(),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        },
        DirectRegistryConfig::default(),
        None,
    );
    for (wire, path) in [
        (
            "GET /api HTTP/1.1\r\nHost: localhost\r\nSeRvice_Id: backend\r\n\r\n",
            "/api",
        ),
        (
            "GET /api HTTP/1.1\r\nHost: localhost\r\nservice_id: backend\r\nAuthorization: first\r\nAuthorization: second\r\n\r\n",
            "/api",
        ),
        ("GET /api HTTP/1.1\r\nHost: localhost\r\n\r\n", "/api"),
        (
            "GET /apix HTTP/1.1\r\nHost: localhost\r\nservice_id: backend\r\n\r\n",
            "/apix",
        ),
    ] {
        let (mut transport, _client) = session(wire).await;
        let mut neutral = NeutralRequest {
            headers: transport.req_header().headers.clone(),
            method: Method::GET,
        };
        let core =
            gateway_policy::token::apply_token_request(&mut neutral, &runtime, path, true).await;
        let adapter = light_pingora::apply_token_request(&mut transport, &runtime, path).await;
        assert_eq!(core, adapter);
        assert_eq!(neutral.headers, transport.req_header().headers);
        if path == "/api" && neutral.headers.contains_key("service_id") {
            let target = if wire.contains("Authorization:") {
                "x-scope-token"
            } else {
                "authorization"
            };
            assert_eq!(neutral.headers[target], "Bearer fixture-token");
        }
    }
    server.await.unwrap();
    assert_eq!(
        light_runtime::RuntimeCache::len(runtime.cache().as_ref()).await,
        1
    );
    // Only test-owned loopback is used; a closed listener deterministically denies the provider connection.
    let unavailable = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let unavailable_address = unavailable.local_addr().unwrap();
    drop(unavailable);
    let mut config = runtime.client_config().clone();
    config.oauth.token.server_url = Some(format!("http://{unavailable_address}"));
    let failed_runtime = TokenRuntime::new(
        runtime.handler_config().clone(),
        runtime.sidecar_config().clone(),
        config,
        DirectRegistryConfig::default(),
        None,
    );
    let failed_adapter_runtime = TokenRuntime::new(
        failed_runtime.handler_config().clone(),
        failed_runtime.sidecar_config().clone(),
        failed_runtime.client_config().clone(),
        DirectRegistryConfig::default(),
        None,
    );
    let (mut transport, _client) =
        session("GET /api HTTP/1.1\r\nHost: localhost\r\nservice_id: backend\r\n\r\n").await;
    let mut neutral = NeutralRequest {
        headers: transport.req_header().headers.clone(),
        method: Method::GET,
    };
    let before = neutral.headers.clone();
    let core =
        gateway_policy::token::apply_token_request(&mut neutral, &failed_runtime, "/api", true)
            .await
            .unwrap_err();
    let adapter =
        light_pingora::apply_token_request(&mut transport, &failed_adapter_runtime, "/api")
            .await
            .unwrap_err();
    assert_eq!(core, adapter);
    assert_eq!((core.status, core.code.as_str()), (502, "ERR10052"));
    let retry_core =
        gateway_policy::token::apply_token_request(&mut neutral, &failed_runtime, "/api", true)
            .await
            .unwrap_err();
    let retry_adapter =
        light_pingora::apply_token_request(&mut transport, &failed_adapter_runtime, "/api")
            .await
            .unwrap_err();
    assert_eq!(retry_core, retry_adapter);
    assert_eq!(
        (retry_core.status, retry_core.code.as_str()),
        (408, "ERR10009")
    );
    assert_eq!(neutral.headers, before);
    assert_eq!(transport.req_header().headers, before);
}
