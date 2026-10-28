use light_security::{
    JwtExpiryMode, SecurityConfig, SecurityRuntime, load_security_runtime,
    verify_jwt_token_for_services, verify_jwt_token_for_services_with_deadline,
};
use serde_json::json;
use std::time::Duration;
use tokio::time::Instant;
mod support;
use support::*;
const OTHER: &[u8] = b"test-only-rotated-hmac-secret-32bytes";

async fn verify(
    runtime: &SecurityRuntime,
    token: &str,
) -> Result<light_security::AuthPrincipal, light_security::HandlerRejection> {
    verify_jwt_token_for_services(runtime, token, JwtExpiryMode::Enforce, &[]).await
}

#[tokio::test]
async fn hundred_unknown_kids_cooldown_and_new_attempt() {
    let server = Server::new("known", false).await;
    let runtime = runtime(&server, SecurityConfig::default(), 2000);
    for i in 0..100 {
        let error = verify(&runtime, &valid(&format!("unknown-{i}")))
            .await
            .unwrap_err();
        assert_eq!(
            (error.status, error.message.as_str()),
            (401, "unknown key id")
        );
    }
    assert_eq!(server.requests(), 1);
    cooldown().await;
    assert_eq!(
        verify(&runtime, &valid("unknown-again"))
            .await
            .unwrap_err()
            .status,
        401
    );
    assert_eq!(server.requests(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fifty_rotation_joiners_share_one_successful_fetch() {
    let server = Server::new("rotated", true).await;
    let runtime = runtime(&server, SecurityConfig::default(), 2000);
    let token = valid("rotated");
    let mut callers = Vec::new();
    for _ in 0..50 {
        let mut caller = Box::pin(verify(&runtime, &token));
        pending(&mut caller).await;
        callers.push(caller);
    }
    server.request_started().await;
    assert_eq!(server.requests(), 1);
    server.release(1);
    for caller in callers {
        assert_eq!(
            caller.await.unwrap().user_id.as_deref(),
            Some("test-only-user")
        );
    }
    assert_eq!(server.requests(), 1);
}

#[tokio::test]
async fn known_kid_validation_errors_do_not_refresh() {
    let server = Server::new("known", false).await;
    let config = SecurityConfig {
        bootstrap_from_key_service: true,
        jwks_refresh_min_interval_secs: 0,
        issuer: "issuer".into(),
        audience: vec!["audience".into()],
        ..Default::default()
    };
    let runtime = runtime(&server, config, 2000);
    runtime.bootstrap().await.unwrap();
    for claims in [
        json!({"exp":now()-120,"iss":"issuer","aud":"audience"}),
        json!({"exp":now()+600,"nbf":now()+120,"iss":"issuer","aud":"audience"}),
        json!({"exp":now()+600,"iss":"wrong","aud":"audience"}),
        json!({"exp":now()+600,"iss":"issuer","aud":"wrong"}),
    ] {
        assert_eq!(
            verify(&runtime, &token(Some("known"), SECRET, claims))
                .await
                .unwrap_err()
                .status,
            401
        );
        assert_eq!(server.requests(), 1);
    }
    assert!(
        verify(
            &runtime,
            &token(
                Some("known"),
                SECRET,
                json!({"exp":now()+600,"iss":"issuer","aud":"audience"})
            )
        )
        .await
        .is_ok()
    );
}

#[tokio::test]
async fn invalid_signature_same_kid_rotates_once_and_no_nested_refresh() {
    let server = Server::new("known", false).await;
    let mut runtime = runtime(
        &server,
        SecurityConfig {
            bootstrap_from_key_service: true,
            ..Default::default()
        },
        2000,
    );
    runtime.bootstrap().await.unwrap();
    cooldown().await;
    server.respond(200, keys("known", OTHER));
    let rotated = token(Some("known"), OTHER, json!({"exp":now()+600}));
    assert!(verify(&runtime, &rotated).await.is_ok());
    assert_eq!(server.requests(), 2);
    for _ in 0..5 {
        assert_eq!(
            verify(&runtime, &valid("known")).await.unwrap_err().status,
            401
        );
    }
    assert_eq!(server.requests(), 2);
    runtime.config.jwks_refresh_min_interval_secs = 0;
    // Unknown key fetch supplies the named key with the wrong secret: no second fetch.
    server.respond(200, keys("new", OTHER));
    assert_eq!(
        verify(&runtime, &valid("new")).await.unwrap_err().status,
        401
    );
    assert_eq!(server.requests(), 3);
}

#[tokio::test]
async fn failed_fetch_starts_cooldown_and_recovers() {
    let server = Server::new("known", false).await;
    server.respond(503, "test-only unavailable".into());
    let runtime = runtime(&server, SecurityConfig::default(), 2000);
    assert_eq!(
        verify(&runtime, &valid("known")).await.unwrap_err().status,
        502
    );
    assert_eq!(
        verify(&runtime, &valid("known")).await.unwrap_err().status,
        401
    );
    assert_eq!(server.requests(), 1);
    server.respond(200, keys("known", SECRET));
    cooldown().await;
    assert!(verify(&runtime, &valid("known")).await.is_ok());
    assert_eq!(server.requests(), 2);
}

#[tokio::test]
async fn interval_zero_sequential_misses_fetch_but_active_callers_join() {
    let server = Server::new("known", true).await;
    let runtime = runtime(
        &server,
        SecurityConfig {
            jwks_refresh_min_interval_secs: 0,
            ..Default::default()
        },
        2000,
    );
    let token = valid("unknown");
    let mut callers = Vec::new();
    for _ in 0..10 {
        let mut f = Box::pin(verify(&runtime, &token));
        pending(&mut f).await;
        callers.push(f);
    }
    server.request_started().await;
    server.release(20);
    for f in callers {
        assert_eq!(f.await.unwrap_err().status, 401);
    }
    assert_eq!(server.requests(), 1);
    for _ in 0..5 {
        assert_eq!(verify(&runtime, &token).await.unwrap_err().status, 401);
    }
    assert_eq!(server.requests(), 6);
}

#[tokio::test]
async fn dropped_starter_and_twenty_dropped_callers_leave_ten_joiners_usable() {
    let server = Server::new("new", true).await;
    let runtime = runtime(&server, SecurityConfig::default(), 2000);
    let token = valid("new");
    let mut starter = Box::pin(verify(&runtime, &token));
    pending(&mut starter).await;
    server.request_started().await;
    let mut joiners = Vec::new();
    for _ in 0..10 {
        let mut f = Box::pin(verify(&runtime, &token));
        pending(&mut f).await;
        joiners.push(f);
    }
    drop(starter); // Drop the whole verification future, not just a refresh helper.
    for _ in 0..20 {
        let mut f = Box::pin(verify(&runtime, &token));
        pending(&mut f).await;
        drop(f);
    }
    assert_eq!(server.requests(), 1);
    server.release(1);
    for f in joiners {
        assert!(f.await.is_ok());
    }
    assert_eq!(
        verify(&runtime, &valid("missing"))
            .await
            .unwrap_err()
            .status,
        401
    );
    assert_eq!(server.requests(), 1);
    cooldown().await;
    server.respond(200, keys("recovered", SECRET));
    server.release(1);
    assert!(verify(&runtime, &valid("recovered")).await.is_ok());
    assert_eq!(server.requests(), 2);
}

#[tokio::test]
async fn caller_and_joiner_deadlines_do_not_cancel_later_success() {
    let server = Server::new("new", true).await;
    let runtime = runtime(&server, SecurityConfig::default(), 2000);
    let token = valid("new");
    let start = Instant::now();
    let mut starter = Box::pin(verify_jwt_token_for_services_with_deadline(
        &runtime,
        &token,
        JwtExpiryMode::Enforce,
        &[],
        Some(start + Duration::from_millis(50)),
    ));
    pending(&mut starter).await;
    server.request_started().await;
    let mut short = Box::pin(verify_jwt_token_for_services_with_deadline(
        &runtime,
        &token,
        JwtExpiryMode::Enforce,
        &[],
        Some(start + Duration::from_millis(50)),
    ));
    pending(&mut short).await;
    let mut long = Box::pin(verify_jwt_token_for_services_with_deadline(
        &runtime,
        &token,
        JwtExpiryMode::Enforce,
        &[],
        Some(start + Duration::from_secs(1)),
    ));
    pending(&mut long).await;
    // Network start is established. Virtual time controls deadlines, not JWT wall time.
    tokio::time::pause();
    tokio::time::advance(Duration::from_millis(51)).await;
    for error in [starter.await.unwrap_err(), short.await.unwrap_err()] {
        assert_eq!((error.status, error.code.as_str()), (504, "ERR14017"));
    }
    assert!(Instant::now() - start < Duration::from_millis(150));
    tokio::time::resume();
    server.release(1);
    assert!(long.await.is_ok());
    assert_eq!(server.requests(), 1);
    assert!(verify(&runtime, &token).await.is_ok());
    let expired = verify_jwt_token_for_services_with_deadline(
        &runtime,
        &valid("missing"),
        JwtExpiryMode::Enforce,
        &[],
        Some(Instant::now()),
    )
    .await
    .unwrap_err();
    assert_eq!(expired.status, 504);
    assert_eq!(server.requests(), 1);
}

#[tokio::test]
async fn shared_fetch_timeout_is_finite_without_a_caller_deadline() {
    let server = Server::new("new", true).await;
    let runtime = runtime(&server, SecurityConfig::default(), 50);
    let token = valid("new");
    let mut f = Box::pin(verify(&runtime, &token));
    pending(&mut f).await;
    server.request_started().await;
    assert_eq!(f.await.unwrap_err().status, 502);
    // Permit completion cleanup to run; a second waiter observes that same attempt if active.
    let error = verify(&runtime, &token).await.unwrap_err();
    assert!([401, 502].contains(&error.status));
    assert_eq!(server.requests(), 1);
}

#[tokio::test]
async fn nbf_modes_optional_claim_skew_and_cache_hits() {
    let server = Server::new("known", false).await;
    let mut runtime = runtime(
        &server,
        SecurityConfig {
            bootstrap_from_key_service: true,
            jwks_refresh_min_interval_secs: 0,
            ..Default::default()
        },
        2000,
    );
    runtime.bootstrap().await.unwrap();
    for mode in [JwtExpiryMode::Enforce, JwtExpiryMode::Ignore] {
        for offset in [None, Some(30), Some(120)] {
            let mut claims = json!({"exp":now()+600});
            if let Some(offset) = offset {
                claims["nbf"] = json!(now() + offset);
            }
            let token = token(Some("known"), SECRET, claims);
            for _ in 0..2 {
                let result = verify_jwt_token_for_services(&runtime, &token, mode, &[]).await;
                assert_eq!(result.is_ok(), offset != Some(120));
            }
        }
    }
    let cached = token(
        Some("known"),
        SECRET,
        json!({"exp":now()+600,"nbf":now()+30}),
    );
    assert!(verify(&runtime, &cached).await.is_ok());
    runtime.config.jwt.clock_skew_in_seconds = 0;
    assert_eq!(verify(&runtime, &cached).await.unwrap_err().status, 401);
    runtime.config.ignore_jwt_expiry = true;
    assert_eq!(verify(&runtime, &cached).await.unwrap_err().status, 401);
    let expired = token(Some("known"), SECRET, json!({"exp":now()-120}));
    assert!(verify(&runtime, &expired).await.is_ok());
    assert!(verify(&runtime, &expired).await.is_ok());
    runtime.config.ignore_jwt_expiry = false;
    assert_eq!(verify(&runtime, &expired).await.unwrap_err().status, 401);
    assert!(
        verify_jwt_token_for_services(&runtime, &expired, JwtExpiryMode::Ignore, &[])
            .await
            .is_ok()
    );
    runtime.config.jwt.clock_skew_in_seconds = 180; // No Lambda-specific <=60 restriction here.
    assert!(
        verify(
            &runtime,
            &token(
                Some("known"),
                SECRET,
                json!({"exp":now()+600,"nbf":now()+120})
            )
        )
        .await
        .is_ok()
    );
    assert_eq!(server.requests(), 1);
}

#[tokio::test]
async fn bootstrap_records_attempt_unknown_kid_rejects_but_absent_kid_fallback_remains() {
    let server = Server::new("known", false).await;
    let runtime = runtime(
        &server,
        SecurityConfig {
            bootstrap_from_key_service: true,
            ..Default::default()
        },
        2000,
    );
    runtime.bootstrap().await.unwrap();
    assert_eq!(
        verify(&runtime, &valid("unknown"))
            .await
            .unwrap_err()
            .status,
        401
    );
    assert!(
        verify(&runtime, &token(None, SECRET, json!({"exp":now()+600})))
            .await
            .is_ok()
    );
    assert_eq!(server.requests(), 1);
    runtime.bootstrap().await.unwrap();
    assert_eq!(server.requests(), 2); // Bootstrap is ungated.
}

#[tokio::test]
async fn normalized_service_set_shares_cooldown_and_algorithm_error_does_not_refresh() {
    let server = Server::new("known", false).await;
    let runtime = runtime(
        &server,
        SecurityConfig {
            jwks_refresh_min_interval_secs: 30,
            ..Default::default()
        },
        2000,
    );
    for ids in [
        vec!["b".into(), "a".into()],
        vec![" a ".into(), "b".into(), "a".into(), "".into()],
    ] {
        assert_eq!(
            verify_jwt_token_for_services(
                &runtime,
                &valid("unknown"),
                JwtExpiryMode::Enforce,
                &ids
            )
            .await
            .unwrap_err()
            .status,
            401
        );
    }
    assert_eq!(server.requests(), 2); // One attempt, two configured provider resolutions.
    let server2 = Server::new("rsa", false).await;
    server2.respond(
        200,
        json!({"keys":[{"kty":"RSA","kid":"rsa","n":"u1WabeaJBoGl","e":"AQAB"}]}).to_string(),
    );
    let runtime2 = support::runtime(
        &server2,
        SecurityConfig {
            bootstrap_from_key_service: true,
            jwks_refresh_min_interval_secs: 0,
            ..Default::default()
        },
        2000,
    );
    runtime2.bootstrap().await.unwrap();
    assert_eq!(
        verify(&runtime2, &valid("rsa")).await.unwrap_err().status,
        401
    );
    assert_eq!(server2.requests(), 1);
}

#[test]
fn refresh_configuration_defaults_and_zero_deserialize() {
    assert_eq!(SecurityConfig::default().jwks_refresh_min_interval_secs, 30);
    assert_eq!(
        serde_json::from_value::<SecurityConfig>(json!({}))
            .unwrap()
            .jwks_refresh_min_interval_secs,
        30
    );
    assert_eq!(
        serde_json::from_value::<SecurityConfig>(json!({"jwksRefreshMinIntervalSecs":0}))
            .unwrap()
            .jwks_refresh_min_interval_secs,
        0
    );
}

#[tokio::test]
async fn expiry_diagnostic_reverifies_resident_signature_issuer_audience_without_refresh() {
    let server = Server::new("known", false).await;
    let runtime = runtime(
        &server,
        SecurityConfig {
            bootstrap_from_key_service: true,
            jwks_refresh_min_interval_secs: 0,
            issuer: "issuer".into(),
            audience: vec!["audience".into()],
            ..Default::default()
        },
        2000,
    );
    runtime.bootstrap().await.unwrap();
    let expired_claims =
        json!({"sub":"test-only-user", "exp":now()-120, "iss":"issuer", "aud":"audience"});
    let expired = token(Some("known"), SECRET, expired_claims.clone());
    assert_eq!(verify(&runtime, &expired).await.unwrap_err().status, 401);
    let diagnostic = light_security::verify_jwt_token_for_services_without_refresh(
        &runtime,
        &expired,
        JwtExpiryMode::Ignore,
        &[],
        None,
    )
    .await
    .unwrap();
    assert_eq!(diagnostic.claims["exp"], expired_claims["exp"]);
    assert_eq!(server.requests(), 1);
    // Diagnostic success must never seed the normal principal cache.
    assert_eq!(verify(&runtime, &expired).await.unwrap_err().status, 401);
    for candidate in [
        token(Some("known"), OTHER, expired_claims.clone()),
        token(Some("absent"), SECRET, expired_claims.clone()),
        token(
            Some("known"),
            SECRET,
            json!({"exp":now()-120,"iss":"wrong","aud":"audience"}),
        ),
        token(
            Some("known"),
            SECRET,
            json!({"exp":now()-120,"iss":"issuer","aud":"wrong"}),
        ),
        token(
            Some("known"),
            SECRET,
            json!({"exp":now()-120,"nbf":now()+120,"iss":"issuer","aud":"audience"}),
        ),
        "not-a-signed-token".to_owned(),
    ] {
        assert_eq!(
            light_security::verify_jwt_token_for_services_without_refresh(
                &runtime,
                &candidate,
                JwtExpiryMode::Ignore,
                &[],
                None
            )
            .await
            .unwrap_err()
            .status,
            401
        );
        assert_eq!(server.requests(), 1);
    }
    // The regular invalid-signature path still performs its one refresh at interval0.
    let forged = token(
        Some("known"),
        OTHER,
        json!({"exp":now()+600,"iss":"issuer","aud":"audience"}),
    );
    assert_eq!(verify(&runtime, &forged).await.unwrap_err().status, 401);
    assert_eq!(server.requests(), 2);
    assert_eq!(
        light_security::verify_jwt_token_for_services_without_refresh(
            &runtime,
            &forged,
            JwtExpiryMode::Ignore,
            &[],
            None
        )
        .await
        .unwrap_err()
        .status,
        401
    );
    assert_eq!(server.requests(), 2);
}
