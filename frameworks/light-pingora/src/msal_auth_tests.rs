use super::*;
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use serde_json::{Value, json};
use std::{collections::HashMap, io::Cursor, sync::Arc};

const TENANT: &str = "11111111-1111-1111-1111-111111111111";
const AUDIENCE: &str = "22222222-2222-2222-2222-222222222222";
const CLIENT: &str = "33333333-3333-3333-3333-333333333333";
const KEY: &[u8] = b"local-msal-regression-signing-key-not-a-production-secret";

fn policy() -> MsalAuthConfig {
    MsalAuthConfig {
        tenant_id: TENANT.into(),
        issuer: format!("https://login.microsoftonline.com/{TENANT}/v2.0"),
        audience: AUDIENCE.into(),
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

fn claims() -> Value {
    json!({
        "iss": policy().issuer, "tid": TENANT, "aud": AUDIENCE, "ver": "2.0",
        "azp": CLIENT, "sub": "local-user", "scp": "access_as_user profile.read extra",
        "exp": now() + 3600,
    })
}

fn sign(claims: &Value) -> String {
    let mut header = Header::new(Algorithm::HS256);
    header.kid = Some("msal-test".into());
    encode(&header, claims, &EncodingKey::from_secret(KEY)).unwrap()
}

async fn runtime(cache: bool, ignore_expiry: bool) -> MsalAuthRuntime {
    use base64::Engine;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let keys = json!({"keys": [{
        "kty": "oct", "kid": "msal-test", "alg": "HS256",
        "k": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(KEY)
    }]})
    .to_string();
    // Serve the real key-fetch/refresh path, including refresh after a bad signature.
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let mut request = [0; 4096];
            if socket.read(&mut request).await.is_ok() {
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{keys}",
                    keys.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        }
    });
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(MSAL_AUTH_FILE),
        serde_yaml::to_string(&policy()).unwrap(),
    )
    .unwrap();
    std::fs::write(dir.path().join(SECURITY_MSAL_FILE), format!(
        "enableJwtCache: {cache}\nignoreJwtExpiry: {ignore_expiry}\nbootstrapFromKeyService: true\njwt:\n  clockSkewInSeconds: 60\n"
    )).unwrap();
    let mut config = runtime_config(&dir);
    config.client = Some(Default::default());
    let client = config.client.as_mut().unwrap();
    client.oauth.token.key.server_url = Some(format!("http://{address}"));
    client.oauth.token.key.uri = "/keys".into();
    let runtime = load_msal_auth_runtime(&config, true).unwrap().unwrap();
    runtime.bootstrap().await.unwrap();
    runtime
}

async fn session(token: &str, cookie: bool, csrf: bool) -> Session {
    let request = if cookie {
        format!(
            "GET /api HTTP/1.1\r\nHost: localhost\r\nCookie: accessToken={token}; csrf=local-csrf\r\n{}\r\n",
            if csrf {
                "X-CSRF-TOKEN: local-csrf\r\n"
            } else {
                ""
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
async fn signed_tokens_require_policy_on_login_and_cookie_paths_with_and_without_cache() {
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
async fn cached_claims_are_rechecked_against_current_msal_policy() {
    let runtime = runtime(true, false).await;
    let mut wrong = claims();
    wrong["aud"] = json!("another-api");
    let token = sign(&wrong);
    // Deliberately warm the generic cache: it has no issuer/audience policy.
    verify_jwt_token(&runtime.msal_security, &token, JwtExpiryMode::Enforce)
        .await
        .unwrap();
    assert_rejected(&runtime, &token).await;
    let token = sign(&claims());
    runtime.verify_microsoft_token(&token).await.unwrap();
    let mut changed = policy();
    changed.required_scopes.push("new.permission".into());
    let changed = MsalAuthRuntime::new(changed, runtime.msal_security.clone()).unwrap();
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
        verify_jwt_token(&runtime.msal_security, &token, JwtExpiryMode::Enforce)
            .await
            .unwrap();
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
async fn signature_failures_are_rejected_and_v1_client_claim_is_supported_explicitly() {
    let runtime = runtime(false, false).await;
    let mut header = Header::new(Algorithm::HS256);
    header.kid = Some("msal-test".into());
    let wrong_signature = encode(
        &header,
        &claims(),
        &EncodingKey::from_secret(b"wrong-secret"),
    )
    .unwrap();
    assert_rejected(&runtime, &wrong_signature).await;
    assert_rejected(&runtime, "not.a.jwt").await;
    let mut config = policy();
    config.issuer = format!("https://sts.windows.net/{TENANT}/");
    let runtime = MsalAuthRuntime::new(config.clone(), runtime.msal_security).unwrap();
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
            ("tenantId", json!(TENANT)),
            ("issuer", json!(policy().issuer)),
            ("audience", json!(AUDIENCE)),
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
