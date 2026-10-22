use super::*;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde_json::{Value, json};
use std::{collections::HashMap, io::Cursor, sync::Arc};

const TENANT: &str = "11111111-1111-1111-1111-111111111111";
const AUDIENCE: &str = "22222222-2222-2222-2222-222222222222";
const CLIENT: &str = "33333333-3333-3333-3333-333333333333";
const KEY: &[u8] = include_bytes!("../tests/fixtures/msal/signing-key.pem");
const OTHER_KEY: &[u8] = include_bytes!("../tests/fixtures/msal/other-signing-key.pem");

fn policy() -> MsalAuthConfig {
    MsalAuthConfig {
        csrf_active_key_id: "test".into(),
        csrf_keys: CsrfKeys(std::collections::BTreeMap::from([(
            "test".into(),
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
        )])),
        tenant_id: TENANT.into(),
        issuer: format!("https://login.microsoftonline.com/{TENANT}/v2.0"),
        audience: AUDIENCE.into(),
        jwks_uri: "https://login.microsoftonline.com/common/discovery/v2.0/keys".into(),
        required_scopes: vec!["access_as_user".into(), "profile.read".into()],
        allowed_client_ids: vec![CLIENT.into()],
        ..Default::default()
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

pub(super) fn claims() -> Value {
    json!({
        "iss": policy().issuer, "tid": TENANT, "aud": AUDIENCE, "ver": "2.0",
        "azp": CLIENT, "sub": "local-user", "scp": "access_as_user profile.read extra",
        "exp": now() + 3600,
    })
}

pub(super) fn sign(claims: &Value) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("msal-test".into());
    encode(&header, claims, &EncodingKey::from_rsa_pem(KEY).unwrap()).unwrap()
}

// Real HTTPS transport with a test CA trusted only by this runtime's test client.
pub(super) struct JwksServer {
    pub(super) uri: String,
    pub(super) client: reqwest::Client,
    pub(super) requests: Arc<std::sync::atomic::AtomicUsize>,
    pub(super) body: Arc<tokio::sync::RwLock<String>>,
}

pub(super) async fn jwks_server(body: &str) -> JwksServer {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let server_config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![cert.cert.der().clone()],
        rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()).into(),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!(
        "https://localhost:{}/keys",
        listener.local_addr().unwrap().port()
    );
    let client = reqwest::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(5))
        .add_root_certificate(reqwest::Certificate::from_der(cert.cert.der()).unwrap())
        .build()
        .unwrap();
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let count = requests.clone();
    let body = Arc::new(tokio::sync::RwLock::new(body.to_owned()));
    let response_body = body.clone();
    tokio::spawn(async move {
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server_config));
        while let Ok((socket, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            let count = count.clone();
            let response_body = response_body.clone();
            tokio::spawn(async move {
                let Ok(mut socket) = acceptor.accept(socket).await else {
                    return;
                };
                let mut request = [0; 4096];
                if socket.read(&mut request).await.is_ok() {
                    count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let body = response_body.read().await;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                }
            });
        }
    });
    JwksServer {
        uri,
        client,
        requests,
        body,
    }
}

async fn runtime(cache: bool, ignore_expiry: bool) -> MsalAuthRuntime {
    let server = jwks_server(include_str!("../tests/fixtures/msal/signing-key.jwks.json")).await;
    let dir = tempfile::tempdir().unwrap();
    let mut policy = policy();
    policy.jwks_uri = server.uri;
    std::fs::write(
        dir.path().join(MSAL_AUTH_FILE),
        serde_yaml::to_string(&policy).unwrap(),
    )
    .unwrap();
    std::fs::write(dir.path().join(SECURITY_MSAL_FILE), format!(
        "enableJwtCache: {cache}\nignoreJwtExpiry: {ignore_expiry}\njwt:\n  clockSkewInSeconds: 60\n"
    )).unwrap();
    // An invalid shared client must never be loaded by direct MSAL authentication.
    std::fs::write(
        dir.path().join("client.yml"),
        "[invalid shared configuration",
    )
    .unwrap();
    let mut runtime = load_msal_auth_runtime(&runtime_config(&dir), true)
        .unwrap()
        .unwrap();
    runtime.verifier.client = server.client;
    runtime.bootstrap().await.unwrap();
    runtime
}

async fn session(token: &str, cookie: bool, csrf: bool) -> Session {
    let request = if cookie {
        let config = policy();
        let value = CsrfSigner::new(&config.csrf_active_key_id, &config.csrf_keys)
            .unwrap()
            .sign(token)
            .unwrap();
        format!(
            "GET /api HTTP/1.1\r\nHost: localhost\r\nCookie: accessToken={token}; csrf={value}\r\n{}\r\n",
            if csrf {
                format!("X-CSRF-TOKEN: {value}\r\n")
            } else {
                String::new()
            }
        )
    } else {
        format!(
            "POST /auth/ms/login HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\n\r\n"
        )
    };
    let mut session = Session::new_h1(Box::new(Cursor::new(request.into_bytes())));
    assert!(session.as_downstream_mut().read_request().await.unwrap());
    session
}

async fn assert_rejected(runtime: &MsalAuthRuntime, token: &str) {
    for _ in 0..2 {
        for cookie in [false, true] {
            let mut session = session(token, cookie, true).await;
            let error = runtime
                .handle_request(&mut session)
                .await
                .err()
                .expect("must reject");
            assert_eq!(error.status, 401);
            assert_eq!(error.code, "ERR10000");
            assert_eq!(error.message, "Invalid Microsoft access token");
            let cookies: Vec<_> = error
                .headers
                .iter()
                .filter(|(name, _)| name == "set-cookie")
                .collect();
            assert_eq!(cookies.len(), if cookie { 2 } else { 0 });
            for (_, value) in cookies {
                assert!(value.contains("Max-Age=0"));
                assert!(!value.contains(token));
            }
            if cookie {
                assert!(session.req_header().headers.get("authorization").is_none());
            }
        }
    }
}

#[tokio::test]
async fn signed_tokens_require_policy_on_both_paths_regardless_of_generic_cache_setting() {
    for cache in [false, true] {
        let runtime = runtime(cache, false).await;
        let token = sign(&claims());
        for _ in 0..2 {
            let outcome = runtime
                .handle_request(&mut session(&token, false, true).await)
                .await
                .unwrap();
            let SpaSessionOutcome::Respond(response) = outcome else {
                panic!("login response")
            };
            assert_eq!(response.status, 200);
            assert_eq!(
                response
                    .headers
                    .iter()
                    .filter(|(name, _)| name == "set-cookie")
                    .count(),
                2
            );
            let mut request = session(&token, true, true).await;
            let outcome = runtime.handle_request(&mut request).await.unwrap();
            assert!(matches!(
                outcome,
                SpaSessionOutcome::Continue { auth: Some(_), .. }
            ));
            assert_eq!(
                request.req_header().headers.get("authorization").unwrap(),
                &format!("Bearer {token}")
            );
        }
        for (name, value) in [
            ("iss", json!("https://untrusted.example/")),
            ("tid", json!("44444444-4444-4444-4444-444444444444")),
            ("aud", json!("https://graph.microsoft.com")),
            ("aud", json!([AUDIENCE, "another-api"])),
            ("azp", json!("44444444-4444-4444-4444-444444444444")),
            ("ver", json!("1.0")),
            ("ver", json!("3.0")),
            ("scp", json!("access_as_user")),
            ("scp", json!("access_as_user_extra profile.read")),
            ("scp", json!(["access_as_user", "profile.read"])),
            ("tid", json!([TENANT])),
            ("sub", json!("")),
            ("idtyp", json!("app")),
            ("idtyp", json!(true)),
            ("exp", json!(now() - 120)),
            ("nbf", json!(now() + 120)),
            ("nbf", json!("tomorrow")),
        ] {
            let mut invalid = claims();
            invalid[name] = value;
            assert_rejected(&runtime, &sign(&invalid)).await;
        }
        for name in ["iss", "tid", "aud", "azp", "ver", "sub", "scp", "exp"] {
            let mut invalid = claims();
            invalid.as_object_mut().unwrap().remove(name);
            assert_rejected(&runtime, &sign(&invalid)).await;
        }
        let mut id_token = claims();
        id_token.as_object_mut().unwrap().remove("scp");
        id_token["nonce"] = json!("id-token");
        assert_rejected(&runtime, &sign(&id_token)).await;
        id_token["roles"] = json!(["access_as_user", "profile.read"]);
        assert_rejected(&runtime, &sign(&id_token)).await;
    }
}

#[tokio::test]
async fn verified_claims_are_rechecked_against_current_msal_policy() {
    let runtime = runtime(true, false).await;
    let mut wrong = claims();
    wrong["aud"] = json!("another-api");
    let token = sign(&wrong);
    assert_rejected(&runtime, &token).await;
    let token = sign(&claims());
    runtime.verify_microsoft_token(&token).await.unwrap();
    let mut changed = policy();
    changed.required_scopes.push("new.permission".into());
    let changed = MsalAuthRuntime {
        config: changed,
        ..runtime
    };
    assert_rejected(&changed, &token).await;
}

#[tokio::test]
async fn ignore_expiry_cannot_bypass_msal_lifetime_checks_even_with_a_warm_cache() {
    let runtime = runtime(true, true).await;
    for value in [Some(json!(now() - 120)), None] {
        let mut invalid = claims();
        match value {
            Some(value) => invalid["exp"] = value,
            None => {
                invalid.as_object_mut().unwrap().remove("exp");
            }
        }
        let token = sign(&invalid);
        assert_rejected(&runtime, &token).await;
    }
}

#[tokio::test]
async fn configured_clock_skew_is_used_by_real_verifier_and_handler() {
    let runtime = runtime(true, false).await;
    let mut within_skew = claims();
    within_skew["exp"] = json!(now() - 20);
    within_skew["nbf"] = json!(now() + 20);
    let token = sign(&within_skew);
    for _ in 0..2 {
        assert!(runtime.verify_microsoft_token(&token).await.is_ok());
    }
    within_skew["nbf"] = json!(now() + 120);
    assert_rejected(&runtime, &sign(&within_skew)).await;
}

#[test]
fn lifetime_boundaries_use_the_same_allowance_for_exp_and_nbf() {
    let policy = policy();
    let mut token = claims();
    token["exp"] = json!(1000);
    token["nbf"] = json!(1119);
    assert_eq!(policy.validate_claims(&token, 1059, 60), Ok(()));
    assert_eq!(policy.validate_claims(&token, 1060, 60), Err("expired"));
    token["exp"] = json!(2000);
    token["nbf"] = json!(1120);
    assert_eq!(policy.validate_claims(&token, 1059, 60), Err("not_before"));
}

#[test]
fn resolver_failures_remain_server_errors_without_leaking_details() {
    let error = msal_verification_error(HandlerRejection::new(
        502,
        "ERR10056",
        "private resolver URL",
    ));
    assert_eq!(error.status, 502);
    assert_eq!(error.code, "ERR10056");
    assert_eq!(error.message, "Microsoft token verification unavailable");
    assert!(error.headers.is_empty());
}

#[tokio::test]
async fn csrf_failure_does_not_clear_valid_cookies() {
    let runtime = runtime(true, false).await;
    let mut request = session(&sign(&claims()), true, false).await;
    let rejection = runtime.handle_request(&mut request).await.err().unwrap();
    assert_eq!(rejection.code, "ERR10036");
    assert!(
        !rejection
            .headers
            .iter()
            .any(|(name, _)| name == "set-cookie")
    );
}

#[tokio::test]
async fn attacker_chosen_matching_csrf_pair_is_not_a_session_binding() {
    let runtime = runtime(true, false).await;
    let token = sign(&claims());
    let mut request = session(&token, true, true).await;
    request
        .req_header_mut()
        .insert_header("cookie", format!("accessToken={token}; csrf=attacker"))
        .unwrap();
    request
        .req_header_mut()
        .insert_header("X-CSRF-TOKEN", "attacker")
        .unwrap();
    let rejection = runtime
        .handle_request(&mut request)
        .await
        .err()
        .expect("unsigned attacker-chosen cookie/header must be rejected");
    assert_eq!(rejection.code, "ERR10039");
    assert!(request.req_header().headers.get("authorization").is_none());
}

#[tokio::test]
async fn session_bound_csrf_rejects_cross_token_tampering_queries_and_partial_cookies() {
    let runtime = runtime(true, false).await;
    let token = sign(&claims());
    let value = runtime.csrf_signer.sign(&token).unwrap();
    let mut other_claims = claims();
    other_claims["sub"] = json!("other-session");
    let other = sign(&other_claims);
    for (path, access, csrf) in [
        ("/api", other.as_str(), value.as_str()),
        ("/api?csrf=leaked", token.as_str(), value.as_str()),
        ("/api?%63srf=", token.as_str(), value.as_str()),
        ("/api", token.as_str(), "v2.invalid.invalid.invalid"),
    ] {
        let mut session = session(access, true, true).await;
        session.req_header_mut().set_uri(path.parse().unwrap());
        session
            .req_header_mut()
            .insert_header("cookie", format!("accessToken={access}; csrf={csrf}"))
            .unwrap();
        session
            .req_header_mut()
            .insert_header("X-CSRF-TOKEN", csrf)
            .unwrap();
        assert!(runtime.handle_request(&mut session).await.is_err());
        assert!(!session.req_header().headers.contains_key("authorization"));
    }
    let mut partial = session(&token, true, true).await;
    partial
        .req_header_mut()
        .insert_header("cookie", "csrf=attacker")
        .unwrap();
    assert!(runtime.handle_request(&mut partial).await.is_err());
}

#[tokio::test]
async fn enforced_msal_logout_verifies_mac_binding_even_for_expired_sessions() {
    let runtime = runtime(true, false).await;
    let mut expired = claims();
    expired["exp"] = json!(now() - 3600);
    let expired = sign(&expired);
    let value = runtime.csrf_signer.sign(&expired).unwrap();
    for (token, csrf, accepted) in [
        (expired.as_str(), value.as_str(), true),
        (expired.as_str(), "attacker", false),
        ("modified-token", value.as_str(), false),
    ] {
        let raw = format!(
            "POST /auth/ms/logout HTTP/1.1\r\nHost: localhost\r\nCookie: accessToken={token}; csrf={csrf}\r\nX-CSRF-TOKEN: {csrf}\r\n\r\n"
        );
        let mut session = Session::new_h1(Box::new(Cursor::new(raw.into_bytes())));
        assert!(session.as_downstream_mut().read_request().await.unwrap());
        let result = runtime.handle_request(&mut session).await;
        assert_eq!(result.is_ok(), accepted);
        if let Ok(SpaSessionOutcome::Respond(response)) = result {
            assert_eq!(response.status, 204);
        }
    }
    let mut api = session(&expired, true, true).await;
    assert_eq!(
        runtime.handle_request(&mut api).await.err().unwrap().code,
        "ERR10000"
    );
    for cookie in [
        "",
        "Cookie: csrf=attacker\r\n",
        "Cookie: refreshToken=opaque\r\n",
    ] {
        let raw = format!("POST /auth/ms/logout HTTP/1.1\r\nHost: localhost\r\n{cookie}\r\n");
        let mut session = Session::new_h1(Box::new(Cursor::new(raw.into_bytes())));
        assert!(session.as_downstream_mut().read_request().await.unwrap());
        assert_eq!(
            runtime.handle_request(&mut session).await.is_ok(),
            cookie.is_empty()
        );
    }
}

#[tokio::test]
async fn msal_websocket_uses_the_same_binding_and_origin_policy() {
    let mut runtime = runtime(true, false).await;
    runtime.config.websocket_origins = vec!["https://portal.example".into()];
    let token = sign(&claims());
    let csrf = runtime.csrf_signer.sign(&token).unwrap();
    for (origin, value, accepted) in [
        ("https://portal.example", csrf.as_str(), true),
        ("null", csrf.as_str(), false),
        ("https://attacker.example", csrf.as_str(), false),
        ("https://portal.example", "attacker", false),
    ] {
        let raw = format!(
            "GET /chat HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nOrigin: {origin}\r\nCookie: accessToken={token}; csrf={value}\r\nSec-WebSocket-Protocol: light-spa-v1, csrf.{value}\r\n\r\n"
        );
        let mut session = Session::new_h1(Box::new(Cursor::new(raw.into_bytes())));
        assert!(session.as_downstream_mut().read_request().await.unwrap());
        assert_eq!(runtime.handle_request(&mut session).await.is_ok(), accepted);
        assert_eq!(
            session.req_header().headers.contains_key("authorization"),
            accepted
        );
    }
}

#[test]
fn csrf_signing_configuration_is_required_and_diagnostics_are_redacted() {
    let dir = tempfile::tempdir().unwrap();
    let config = runtime_config(&dir);
    config.module_registry.set_mask_config_properties(false);
    let valid = policy();
    let secret = valid.csrf_keys.0["test"].clone();
    assert!(!format!("{valid:?}").contains(&secret));
    std::fs::write(
        dir.path().join(MSAL_AUTH_FILE),
        serde_yaml::to_string(&valid).unwrap(),
    )
    .unwrap();
    load_msal_auth_runtime(&config, true).unwrap();
    assert!(!format!("{:?}", config.module_registry.component_configs()).contains(&secret));
    let metadata = config.module_registry.component_configs();
    for field in ["csrfActiveKeyId", "csrfKeys"] {
        let mut value = serde_json::to_value(&valid).unwrap();
        value.as_object_mut().unwrap().remove(field);
        std::fs::write(
            dir.path().join(MSAL_AUTH_FILE),
            serde_yaml::to_string(&value).unwrap(),
        )
        .unwrap();
        let error = load_msal_auth_runtime(&config, true).err().unwrap();
        assert!(!error.to_string().contains(&secret));
        assert_eq!(metadata, config.module_registry.component_configs());
    }
}

#[tokio::test]
async fn signature_failures_are_rejected_and_v1_client_claim_is_supported_explicitly() {
    let runtime = runtime(false, false).await;
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("msal-test".into());
    let wrong_signature = encode(
        &header,
        &claims(),
        &EncodingKey::from_rsa_pem(OTHER_KEY).unwrap(),
    )
    .unwrap();
    assert_rejected(&runtime, &wrong_signature).await;
    assert_rejected(&runtime, "not.a.jwt").await;
    let mut config = policy();
    config.issuer = format!("https://sts.windows.net/{TENANT}/");
    let runtime = MsalAuthRuntime {
        config: config.clone(),
        ..runtime
    };
    let mut token = claims();
    token["iss"] = json!(config.issuer);
    token["ver"] = json!("1.0");
    token["appid"] = json!(CLIENT);
    token.as_object_mut().unwrap().remove("azp");
    runtime.verify_microsoft_token(&sign(&token)).await.unwrap();
    token.as_object_mut().unwrap().remove("appid");
    token["azp"] = json!(CLIENT);
    assert_rejected(&runtime, &sign(&token)).await;
}

fn runtime_config(dir: &tempfile::TempDir) -> RuntimeConfig {
    RuntimeConfig {
        bootstrap: Default::default(),
        server: Default::default(),
        client: None,
        portal_registry: None,
        direct_registry: Default::default(),
        service_identity: Default::default(),
        config_dir: dir.path().into(),
        external_config_dir: dir.path().join("external"),
        resolved_values: HashMap::new(),
        default_config_dir: None,
        embedded_config: &[],
        module_registry: Arc::new(light_runtime::ModuleRegistry::new()),
        cache_registry: None,
        registry_client: None,
    }
}

#[test]
fn shipped_template_loads_values_and_supports_list_substitution_formats() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(MSAL_AUTH_FILE),
        include_str!("../../../apps/light-gateway/config/msal-auth.yml"),
    )
    .unwrap();
    for list in [
        json!(["access_as_user"]),
        json!("access_as_user"),
        json!("[\"access_as_user\"]"),
        json!("access_as_user,profile.read"),
    ] {
        let mut config = runtime_config(&dir);
        for (name, value) in [
            ("csrfActiveKeyId", json!("test")),
            ("csrfKeys", json!(policy().csrf_keys)),
            ("tenantId", json!(TENANT)),
            ("issuer", json!(policy().issuer)),
            ("audience", json!(AUDIENCE)),
            ("jwksUri", json!(policy().jwks_uri)),
            ("requiredScopes", list),
            ("allowedClientIds", json!(CLIENT)),
        ] {
            config.resolved_values.insert(
                format!("msal-auth.{name}"),
                serde_yaml::to_value(value).unwrap(),
            );
        }
        let loaded = load_msal_auth_config(&config).unwrap().unwrap();
        loaded.validate().unwrap();
        assert_eq!(loaded.tenant_id, TENANT);
        assert_eq!(loaded.allowed_client_ids, vec![CLIENT]);
        assert_eq!(loaded.required_scopes[0], "access_as_user");
    }
    let config = runtime_config(&dir);
    assert!(load_msal_auth_runtime(&config, true).is_err());
    assert!(load_msal_auth_runtime(&config, false).unwrap().is_none());
}

#[test]
fn disabled_and_invalid_policy_configs_fail_at_load_time_without_request_rebuilds() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(MSAL_AUTH_FILE), "enabled: false\n").unwrap();
    assert!(
        load_msal_auth_runtime(&runtime_config(&dir), true)
            .unwrap()
            .is_none()
    );
    for field in [
        "tenantId",
        "issuer",
        "audience",
        "jwksUri",
        "requiredScopes",
        "allowedClientIds",
    ] {
        let mut value = serde_json::to_value(policy()).unwrap();
        value.as_object_mut().unwrap().remove(field);
        let config: MsalAuthConfig = serde_json::from_value(value).unwrap();
        assert!(config.validate().is_err(), "{field}");
    }
    for issuer in [
        "https://login.microsoftonline.com/common/v2.0".to_owned(),
        format!("http://login.microsoftonline.com/{TENANT}/v2.0"),
        format!("{}?x=1", policy().issuer),
    ] {
        let mut config = policy();
        config.issuer = issuer;
        assert!(config.validate().is_err());
    }
    let mut value = serde_json::to_value(policy()).unwrap();
    value["requiredScopes"] = json!(["scope", 42]);
    assert!(serde_json::from_value::<MsalAuthConfig>(value).is_err());
}

#[tokio::test]
async fn msal_signing_trust_is_isolated_for_same_kid_in_both_provider_orders() {
    let other = jwks_server(include_str!(
        "../tests/fixtures/msal/other-signing-key.jwks.json"
    ))
    .await;
    let mut other_config = policy();
    other_config.jwks_uri = other.uri;
    let mut other_runtime = MsalAuthRuntime::new(other_config, SecurityConfig::default()).unwrap();
    other_runtime.verifier.client = other.client;
    let trusted = runtime(true, false).await;
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some("msal-test".into());
    let other_token = encode(
        &header,
        &claims(),
        &EncodingKey::from_rsa_pem(OTHER_KEY).unwrap(),
    )
    .unwrap();
    let trusted_token = sign(&claims());
    for other_first in [true, false] {
        if other_first {
            other_runtime
                .verify_microsoft_token(&other_token)
                .await
                .unwrap();
        }
        trusted
            .verify_microsoft_token(&trusted_token)
            .await
            .unwrap();
        assert_rejected(&trusted, &other_token).await;
        other_runtime
            .verify_microsoft_token(&other_token)
            .await
            .unwrap();
        assert_rejected(&other_runtime, &trusted_token).await;
    }
    // Neither a warm shared principal cache nor a permitted generic HMAC key
    // may establish MSAL signing trust.
    let secret = b"local-generic-cache-key";
    let generic = crate::security::SecurityRuntime::with_test_hs256_key("msal-test", secret).await;
    let mut generic = generic;
    generic.config.enable_jwt_cache = true;
    let mut header = Header::new(Algorithm::HS256);
    header.kid = Some("msal-test".into());
    let token = encode(&header, &claims(), &EncodingKey::from_secret(secret)).unwrap();
    for _ in 0..2 {
        crate::security::verify_jwt_token(
            &generic,
            &token,
            crate::security::JwtExpiryMode::Enforce,
        )
        .await
        .unwrap();
        assert_rejected(&trusted, &token).await;
    }
    // Another RSA signing algorithm is also rejected even with the trusted key.
    let mut header = Header::new(Algorithm::RS512);
    header.kid = Some("msal-test".into());
    assert_rejected(
        &trusted,
        &encode(&header, &claims(), &EncodingKey::from_rsa_pem(KEY).unwrap()).unwrap(),
    )
    .await;
}

#[test]
fn jwks_endpoint_must_be_explicit_https_without_credentials_or_fragment() {
    for uri in [
        "",
        "http://localhost/keys",
        "https://user:pass@example.com/keys",
        "https://example.com/keys#fragment",
        " https://example.com/keys",
    ] {
        let mut config = policy();
        config.jwks_uri = uri.into();
        assert!(config.validate().is_err(), "{uri}");
    }
}

#[tokio::test]
async fn untrusted_tls_endpoint_fails_closed_without_clearing_session_cookies() {
    let server = jwks_server(include_str!("../tests/fixtures/msal/signing-key.jwks.json")).await;
    let mut config = policy();
    config.jwks_uri = server.uri;
    // Keep the production client: the fixture CA is intentionally untrusted.
    let runtime = MsalAuthRuntime::new(config, SecurityConfig::default()).unwrap();
    let token = sign(&claims());
    let error = runtime
        .handle_request(&mut session(&token, true, true).await)
        .await
        .err()
        .unwrap();
    assert_eq!(error.status, 502);
    assert_eq!(error.message, "Microsoft token verification unavailable");
    assert!(!error.headers.iter().any(|(name, _)| name == "set-cookie"));
    assert_eq!(server.requests.load(std::sync::atomic::Ordering::SeqCst), 0);
}
