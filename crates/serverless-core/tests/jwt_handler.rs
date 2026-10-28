mod security_support;
use light_security::{JwtExpiryMode, verify_jwt_token};
use security_support::*;
use serde_json::json;
use serverless_core::{
    handlers::security::{UnifiedHandler, UnifiedSettings},
    unsupported::validate_security,
    *,
};
use std::{
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

#[tokio::test]
async fn real_jwt_matrix_denials_have_no_principal_and_zero_pipeline_dispatch() {
    let (server, key) = server(false).await;
    let jwt = handler(jwks::runtime(&server, config(), 2000), &["read"]);
    let probe = Arc::new(Probe::default());
    let pipe = pipeline(jwt, HandlerId::Jwt, probe.clone());
    let token = signed(&key, claims("user-A"));
    assert_eq!(
        pipe.execute(
            invocation(Some(&format!("Bearer {token}")), "/x"),
            probe.as_ref()
        )
        .await
        .status,
        200
    );
    denied(&pipe, &probe, invocation(None, "/x"), 401, "ERR10002").await;
    for auth in ["bad", "Bearer invalid-synthetic-token"] {
        denied(&pipe, &probe, invocation(Some(auth), "/x"), 401, "ERR10000").await;
    }
    denied(
        &pipe,
        &probe,
        invocation(Some("Basic test-only"), "/x"),
        401,
        "ERR10002",
    )
    .await;
    for (field, value, code) in [
        ("exp", json!(jwks::now() - 120), "ERR10001"),
        ("nbf", json!(jwks::now() + 120), "ERR10000"),
        ("iss", json!("wrong-issuer"), "ERR10000"),
        ("aud", json!("wrong-audience"), "ERR10000"),
        ("scope", json!("write"), "ERR10005"),
    ] {
        let mut c = claims("user-B");
        c[field] = value;
        let token = signed(&key, c);
        denied(
            &pipe,
            &probe,
            invocation(Some(&format!("Bearer {token}")), "/x"),
            if code == "ERR10005" { 403 } else { 401 },
            code,
        )
        .await;
    }
    for token in [
        signed(b"test-only-wrong-signing-key", claims("user-C")),
        jwks::token(Some("unknown-key"), &key, claims("user-C")),
    ] {
        denied(
            &pipe,
            &probe,
            invocation(Some(&format!("Bearer {token}")), "/x"),
            401,
            "ERR10000",
        )
        .await;
    }
    // Incompatible algorithm/key family, with a deliberately corrupted test header.
    use base64::Engine;
    let mut pieces: Vec<_> = token.split('.').map(str::to_owned).collect();
    pieces[0] = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(r#"{"alg":"RS256","kid":"test-key"}"#);
    denied(
        &pipe,
        &probe,
        invocation(Some(&format!("Bearer {}", pieces.join("."))), "/x"),
        401,
        "ERR10000",
    )
    .await;
    assert_eq!(
        server.requests(),
        1,
        "bad signature/unknown kid must retain the shared cooldown"
    );
    assert_eq!(probe.dispatches.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn allowed_skew_absent_nbf_and_scope_string_list_scp_are_supported() {
    let (server, key) = server(false).await;
    let probe = Arc::new(Probe::default());
    let pipe = pipeline(
        handler(jwks::runtime(&server, config(), 2000), &["read", "admin"]),
        HandlerId::Jwt,
        probe.clone(),
    );
    for scope in [json!("read write"), json!(["read", "write"])] {
        let mut c = claims("user-A");
        c["scope"] = scope;
        c["nbf"] = json!(jwks::now() + 30);
        let token = signed(&key, c);
        assert_eq!(
            pipe.execute(
                invocation(Some(&format!("bearer {token}")), "/x"),
                probe.as_ref()
            )
            .await
            .status,
            200
        );
    }
    let mut c = claims("user-B");
    c.as_object_mut().unwrap().remove("scope");
    c["scp"] = json!(["read"]);
    let token = signed(&key, c);
    assert_eq!(
        pipe.execute(
            invocation(Some(&format!("Bearer {token}")), "/x"),
            probe.as_ref()
        )
        .await
        .status,
        200
    );
    let mut c = claims("user-C");
    c.as_object_mut().unwrap().remove("scope");
    let token = signed(&key, c);
    denied(
        &pipe,
        &probe,
        invocation(Some(&format!("Bearer {token}")), "/x"),
        403,
        "ERR10005",
    )
    .await;
}

#[tokio::test]
async fn principal_is_cleared_before_scope_denial_and_skip() {
    let (server, key) = server(false).await;
    let runtime = jwks::runtime(&server, config(), 2000);
    let jwt = handler(runtime.clone(), &["read"]);
    let mut st = ExchangeState::default();
    let token = signed(&key, claims("user-A"));
    let mut inv = invocation(Some(&format!("Bearer {token}")), "/x");
    assert!(matches!(
        jwt.on_request(&mut inv, &mut st).await.unwrap(),
        HandlerOutcome::Continue
    ));
    assert_eq!(st.principal.as_ref().unwrap().principal_id(), "user-A");
    let mut c = claims("user-B");
    c["scope"] = json!("write");
    let token = signed(&key, c);
    inv = invocation(Some(&format!("Bearer {token}")), "/x");
    assert!(matches!(
        jwt.on_request(&mut inv, &mut st).await.unwrap(),
        HandlerOutcome::Reject(_)
    ));
    assert!(st.principal.is_none());
    let mut runtime = runtime;
    runtime.config.skip_path_prefixes = vec!["/public".into()];
    let jwt = handler(runtime, &[]);
    assert!(matches!(
        jwt.on_request(&mut invocation(None, "/public"), &mut st)
            .await
            .unwrap(),
        HandlerOutcome::Continue
    ));
    assert!(st.principal.is_none());
}

#[tokio::test]
async fn cached_nbf_is_rechecked_and_shared_skew_above_60_remains_supported() {
    let (server, key) = server(false).await;
    let runtime = jwks::runtime(&server, config(), 2000);
    let mut c = claims("cached-user");
    c["nbf"] = json!(jwks::now() + 30);
    let token = signed(&key, c);
    let jwt = handler(runtime.clone(), &[]);
    let mut st = ExchangeState::default();
    assert!(matches!(
        jwt.on_request(
            &mut invocation(Some(&format!("Bearer {token}")), "/x"),
            &mut st
        )
        .await
        .unwrap(),
        HandlerOutcome::Continue
    ));
    let mut narrower = runtime.clone();
    narrower.config.jwt.clock_skew_in_seconds = 0;
    let probe = Arc::new(Probe::default());
    let pipe = pipeline(handler(narrower, &[]), HandlerId::Jwt, probe.clone());
    denied(
        &pipe,
        &probe,
        invocation(Some(&format!("Bearer {token}")), "/x"),
        401,
        "ERR10000",
    )
    .await;
    let mut shared = runtime;
    shared.config.jwt.clock_skew_in_seconds = 120;
    let mut c = claims("shared-user");
    c["nbf"] = json!(jwks::now() + 90);
    let token = signed(&key, c);
    assert!(
        verify_jwt_token(&shared, &token, JwtExpiryMode::Enforce)
            .await
            .is_ok()
    );
    assert!(validate_security(&serde_json::to_value(&shared.config).unwrap(), true).is_err());
}

#[tokio::test]
async fn unknown_kid_throttles_then_real_rotation_succeeds() {
    let (server, key) = server(false).await;
    let probe = Arc::new(Probe::default());
    let pipe = pipeline(
        handler(jwks::runtime(&server, config(), 2000), &[]),
        HandlerId::Jwt,
        probe.clone(),
    );
    let token = signed(&key, claims("user-A"));
    assert_eq!(
        pipe.execute(
            invocation(Some(&format!("Bearer {token}")), "/x"),
            probe.as_ref()
        )
        .await
        .status,
        200
    );
    let rotated_key = uuid::Uuid::new_v4().as_bytes().repeat(2);
    server.respond(200, jwks::keys("rotated", &rotated_key));
    let rotated = jwks::token(Some("rotated"), &rotated_key, claims("user-B"));
    for _ in 0..3 {
        denied(
            &pipe,
            &probe,
            invocation(Some(&format!("Bearer {rotated}")), "/x"),
            401,
            "ERR10000",
        )
        .await;
    }
    assert_eq!(server.requests(), 1);
    jwks::cooldown().await;
    assert_eq!(
        pipe.execute(
            invocation(Some(&format!("Bearer {rotated}")), "/x"),
            probe.as_ref()
        )
        .await
        .status,
        200
    );
    assert_eq!(server.requests(), 2);
}

#[tokio::test]
async fn concurrent_real_verification_joins_one_loopback_refresh() {
    let (server, key) = server(true).await;
    let probe = Arc::new(Probe::default());
    let pipe = Arc::new(pipeline(
        handler(jwks::runtime(&server, config(), 2000), &[]),
        HandlerId::Jwt,
        probe.clone(),
    ));
    let token = signed(&key, claims("joined-user"));
    let mut tasks = vec![];
    for _ in 0..20 {
        let pipe = pipe.clone();
        let probe = probe.clone();
        let inv = invocation(Some(&format!("Bearer {token}")), "/x");
        tasks.push(tokio::spawn(async move {
            pipe.execute(inv, probe.as_ref()).await
        }));
    }
    server.request_started().await;
    assert_eq!(server.requests(), 1);
    server.release(1);
    for task in tasks {
        assert_eq!(task.await.unwrap().status, 200);
    }
    assert_eq!(server.requests(), 1);
    assert_eq!(probe.dispatches.load(Ordering::SeqCst), 20);
}

#[tokio::test]
async fn held_real_jwks_exhausts_original_deadline_and_pipeline_does_not_dispatch() {
    let (server, key) = server(true).await;
    let runtime = jwks::runtime(&server, config(), 2000);
    let jwt = handler(runtime.clone(), &[]);
    let probe = Arc::new(Probe::default());
    let pipe = Arc::new(pipeline(jwt.clone(), HandlerId::Jwt, probe.clone()));
    let token = signed(&key, claims("deadline-user"));
    let mut inv = invocation(Some(&format!("Bearer {token}")), "/x");
    inv.deadline = tokio::time::Instant::now() + Duration::from_millis(60);
    let task = {
        let pipe = pipe.clone();
        let probe = probe.clone();
        tokio::spawn(async move { pipe.execute(inv, probe.as_ref()).await })
    };
    server.request_started().await;
    let response = task.await.unwrap();
    assert_eq!(response.status, 504);
    assert!(String::from_utf8_lossy(response.body.as_ref().unwrap()).contains("ERR14017"));
    assert_eq!(probe.dispatches.load(Ordering::SeqCst), 0);
    assert_eq!(probe.final_principals.lock().unwrap().as_slice(), [None]);
    let mut inv = invocation(Some(&format!("Bearer {token}")), "/x");
    inv.deadline = tokio::time::Instant::now();
    let mut st = ExchangeState::default();
    assert!(
        matches!(jwt.on_request(&mut inv,&mut st).await.unwrap(),HandlerOutcome::Reject(e) if e.code=="ERR14017")
    );
    assert!(st.principal.is_none());
    server.release(1);
    assert_eq!(
        pipe.execute(
            invocation(Some(&format!("Bearer {token}")), "/x"),
            probe.as_ref()
        )
        .await
        .status,
        200
    );
    assert_eq!(server.requests(), 1);
}

#[tokio::test]
async fn unified_first_prefix_anonymous_unmatched_disabled_and_scheme_restriction() {
    let (server, key) = server(false).await;
    let jwt = handler(jwks::runtime(&server, config(), 2000), &["read"]);
    let probe = Arc::new(Probe::default());
    let unified=UnifiedHandler::new(UnifiedSettings::from_value(&json!({"anonymousPrefixes":["/public"],"pathPrefixAuths":[{"prefix":"/x","jwt":true},{"prefix":"/x/private","jwt":false},{"prefix":"/open","jwt":false,"basic":false}]})).unwrap(),jwt);
    let pipe = pipeline(Arc::new(unified), HandlerId::Security, probe.clone());
    assert_eq!(
        pipe.execute(invocation(None, "/public/x"), probe.as_ref())
            .await
            .status,
        200
    );
    assert_eq!(
        pipe.execute(invocation(None, "/open/x"), probe.as_ref())
            .await
            .status,
        200
    );
    denied(
        &pipe,
        &probe,
        invocation(None, "/x/private"),
        401,
        "ERR10002",
    )
    .await;
    denied(
        &pipe,
        &probe,
        invocation(Some("Basic test-only"), "/x"),
        401,
        "ERR10000",
    )
    .await;
    denied(&pipe, &probe, invocation(None, "/unknown"), 400, "ERR10078").await;
    let token = signed(&key, claims("user-B"));
    assert_eq!(
        pipe.execute(
            invocation(Some(&format!("Bearer {token}")), "/x"),
            probe.as_ref()
        )
        .await
        .status,
        200
    );
    assert_eq!(
        probe.principals.lock().unwrap().as_slice(),
        [None, None, Some("user-B".into())]
    );
}

#[tokio::test]
async fn configured_provider_service_ids_and_path_selection_reach_shared_verifier() {
    let (server, key) = server(false).await;
    let client = json!({"request":{"timeout":2000,"connectTimeout":2000},"oauth":{"multipleAuthServers":true,"token":{"key":{"serviceIdAuthServers":{
        "provider-a":{"server_url":server.url,"uri":"/jwks","audience":"aud-a","enableHttp2":false},"provider-b":{"server_url":server.url,"uri":"/jwks","audience":"aud-b","enableHttp2":false}}}}},"pathPrefixServices":{"/path-a":"provider-a"}});
    let mut cfg = config();
    cfg.audience.clear();
    let jwt = handler(runtime_with_client(cfg, client), &[]);
    let probe = Arc::new(Probe::default());
    let mut c = claims("provider-user");
    c["aud"] = json!("aud-a");
    let token = signed(&key, c);
    let pipe = pipeline(jwt.clone(), HandlerId::Jwt, probe.clone());
    assert_eq!(
        pipe.execute(
            invocation(Some(&format!("Bearer {token}")), "/path-a"),
            probe.as_ref()
        )
        .await
        .status,
        200
    );
    let mut inv = invocation(Some(&format!("Bearer {token}")), "/other");
    inv.headers
        .insert("service_id", "provider-b".parse().unwrap());
    denied(&pipe, &probe, inv, 401, "ERR10000").await;
    let unified=UnifiedHandler::new(UnifiedSettings::from_value(&json!({"pathPrefixAuths":[{"prefix":"/selected","jwt":true,"jwkServiceIds":["provider-a"]}]})).unwrap(),jwt);
    let pipe = pipeline(Arc::new(unified), HandlerId::Security, probe.clone());
    assert_eq!(
        pipe.execute(
            invocation(Some(&format!("Bearer {token}")), "/selected"),
            probe.as_ref()
        )
        .await
        .status,
        200
    );
}

#[tokio::test]
async fn malformed_bearer_duplicate_headers_and_zero_cooldown_unknown_kid_fail_closed() {
    let (server, key) = server(false).await;
    let mut cfg = config();
    cfg.jwks_refresh_min_interval_secs = 0;
    let probe = Arc::new(Probe::default());
    let pipe = pipeline(
        handler(jwks::runtime(&server, cfg, 2000), &[]),
        HandlerId::Jwt,
        probe.clone(),
    );
    let token = signed(&key, claims("user-A"));
    let mut inv = invocation(Some("Bearer"), "/x");
    inv.headers
        .insert("x-scope-token", format!("Bearer {token}").parse().unwrap());
    denied(&pipe, &probe, inv, 401, "ERR10002").await;
    let mut inv = invocation(Some(&format!("Bearer {token}")), "/x");
    inv.headers
        .append("authorization", format!("Bearer {token}").parse().unwrap());
    denied(&pipe, &probe, inv, 401, "ERR10000").await;
    assert_eq!(server.requests(), 0);
    let token = jwks::token(Some("unknown"), &key, claims("unknown-user"));
    denied(
        &pipe,
        &probe,
        invocation(Some(&format!("Bearer {token}")), "/x"),
        401,
        "ERR10000",
    )
    .await;
    assert_eq!(
        server.requests(),
        1,
        "key-resolution failure must not cause an expiry-diagnostic verification"
    );
}

#[tokio::test]
async fn real_jwks_dependency_failure_maps_to_gateway_denial_with_zero_dispatch() {
    let (server, key) = server(false).await;
    server.respond(503, "test-only-unavailable".into());
    let probe = Arc::new(Probe::default());
    let pipe = pipeline(
        handler(jwks::runtime(&server, config(), 2000), &[]),
        HandlerId::Jwt,
        probe.clone(),
    );
    let token = signed(&key, claims("user-A"));
    denied(
        &pipe,
        &probe,
        invocation(Some(&format!("Bearer {token}")), "/x"),
        401,
        "ERR10000",
    )
    .await;
    assert_eq!(server.requests(), 1);
}

#[tokio::test]
async fn supplied_scope_token_is_verified_and_selected_even_when_primary_scope_matches() {
    let (server, key) = server(false).await;
    let probe = Arc::new(Probe::default());
    let pipe = pipeline(
        handler(jwks::runtime(&server, config(), 2000), &["read"]),
        HandlerId::Jwt,
        probe.clone(),
    );
    let primary = signed(&key, claims("user-A"));
    for (scope_token, status, code) in [
        ("invalid".to_owned(), 401, "ERR10003"),
        (
            {
                let mut c = claims("app");
                c["scope"] = json!("write");
                format!("Bearer {}", signed(&key, c))
            },
            403,
            "ERR10006",
        ),
        (
            {
                let mut c = claims("app");
                c["exp"] = json!(jwks::now() - 120);
                format!("Bearer {}", signed(&key, c))
            },
            401,
            "ERR10004",
        ),
    ] {
        let mut inv = invocation(Some(&format!("Bearer {primary}")), "/x");
        inv.headers
            .insert("x-scope-token", scope_token.parse().unwrap());
        denied(&pipe, &probe, inv, status, code).await;
    }
    let secondary = signed(&key, claims("app"));
    let mut c = claims("user-B");
    c["scope"] = json!("write");
    let primary = signed(&key, c);
    let mut inv = invocation(Some(&format!("Bearer {primary}")), "/x");
    inv.headers.insert(
        "x-scope-token",
        format!("Bearer {secondary}").parse().unwrap(),
    );
    assert_eq!(pipe.execute(inv, probe.as_ref()).await.status, 200);
    assert_eq!(
        probe.principals.lock().unwrap().as_slice(),
        [Some("user-B".into())]
    );
    let mut c = claims("user-C");
    c["scope"] = json!([]);
    c["scp"] = json!("read");
    let token = signed(&key, c);
    assert_eq!(
        pipe.execute(
            invocation(Some(&format!("Bearer {token}")), "/x"),
            probe.as_ref()
        )
        .await
        .status,
        200
    );
}
