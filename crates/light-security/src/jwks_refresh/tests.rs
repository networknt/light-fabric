use super::*;
use crate::{load_security_runtime, verify_jwt_token_for_services_with_deadline};
#[path = "../../tests/support/mod.rs"]
mod support;
use support::*;

fn inactive_recorded(runtime: &SecurityRuntime) {
    let entries = runtime.refresh_gate.entries.lock().unwrap();
    let entry = entries.get(&gate_key(&[])).unwrap();
    assert!(entry.active.is_none());
    assert!(entry.last_attempt.is_some());
}

#[tokio::test]
async fn panic_after_publication_settles_two_joiners_retains_cooldown_and_recovers() {
    let server = Server::new("new", false).await;
    let runtime = runtime(&server, SecurityConfig::default(), 2000);
    let (release, barrier) = tokio::sync::oneshot::channel();
    *runtime.refresh_gate.before_fetch.lock().unwrap() = Some(barrier);
    let token = valid("new");
    let deadline = Instant::now() + Duration::from_secs(1);
    let mut callers = Vec::new();
    for _ in 0..3 {
        let mut f = Box::pin(verify_jwt_token_for_services_with_deadline(
            &runtime,
            &token,
            JwtExpiryMode::Enforce,
            &[],
            Some(deadline),
        ));
        pending(&mut f).await;
        callers.push(f);
    }
    {
        let entries = runtime.refresh_gate.entries.lock().unwrap();
        let entry = entries.get(&gate_key(&[])).unwrap();
        assert!(entry.last_attempt.is_some());
        assert!(entry.active.is_some());
        assert_eq!(entry.starts, 1);
        assert_eq!(entry.joined, 2);
    }
    assert_eq!(server.requests(), 0);
    release.send(()).unwrap();
    for f in callers {
        let error = f.await.unwrap_err();
        assert_eq!((error.status, error.code.as_str()), (502, "ERR10056"));
    }
    assert!(Instant::now() < deadline);
    inactive_recorded(&runtime);
    assert_eq!(server.requests(), 0); // Task start is not an HTTP request.
    assert_eq!(
        verify_jwt_token(&runtime, &token, JwtExpiryMode::Enforce)
            .await
            .unwrap_err()
            .status,
        401
    );
    assert_eq!(server.requests(), 0);
    cooldown().await;
    assert!(
        verify_jwt_token(&runtime, &token, JwtExpiryMode::Enforce)
            .await
            .is_ok()
    );
    inactive_recorded(&runtime);
    assert_eq!(server.requests(), 1);
}

#[tokio::test]
async fn abort_actual_started_fetch_cleans_up_and_preserves_cooldown() {
    let server = Server::new("new", true).await;
    let runtime = runtime(&server, SecurityConfig::default(), 2000);
    let token = valid("new");
    let mut f = Box::pin(verify_jwt_token(&runtime, &token, JwtExpiryMode::Enforce));
    pending(&mut f).await;
    server.request_started().await;
    runtime
        .refresh_gate
        .abort_handle
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .abort();
    assert_eq!(f.await.unwrap_err().status, 502);
    inactive_recorded(&runtime);
    assert_eq!(
        verify_jwt_token(&runtime, &token, JwtExpiryMode::Enforce)
            .await
            .unwrap_err()
            .status,
        401
    );
    assert_eq!(server.requests(), 1);
    cooldown().await;
    server.release(2);
    assert!(
        verify_jwt_token(&runtime, &token, JwtExpiryMode::Enforce)
            .await
            .is_ok()
    );
    inactive_recorded(&runtime);
    assert_eq!(server.requests(), 2);
}

#[tokio::test]
async fn abort_before_first_task_poll_drops_owned_completion() {
    let server = Server::new("new", false).await;
    let runtime = runtime(&server, SecurityConfig::default(), 2000);
    let token = valid("new");
    let mut f = Box::pin(verify_jwt_token(&runtime, &token, JwtExpiryMode::Enforce));
    pending(&mut f).await;
    runtime
        .refresh_gate
        .abort_handle
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .abort();
    assert_eq!(f.await.unwrap_err().status, 502);
    inactive_recorded(&runtime);
    assert_eq!(server.requests(), 0);
}

#[tokio::test]
async fn completion_before_wait_poll_and_closed_channel_preserve_terminal_result() {
    let gate = Arc::new(JwksRefreshGate::default());
    let (sender, mut receiver) = watch::channel(None);
    let key = gate_key(&[]);
    gate.entries.lock().unwrap().insert(
        key.clone(),
        GateEntry {
            last_attempt: Some(Instant::now()),
            active: Some(receiver.clone()),
            ..Default::default()
        },
    );
    drop(Completion {
        gate: gate.clone(),
        key: key.clone(),
        sender,
        result: Some(Ok(())),
    });
    assert_eq!(*receiver.borrow_and_update(), Some(Ok(())));
    assert!(receiver.changed().await.is_err());
    assert_eq!(*receiver.borrow(), Some(Ok(())));
    assert!(gate.entries.lock().unwrap()[&key].active.is_none());
}

#[tokio::test]
async fn poisoned_map_mutex_recovers_for_refresh_and_cleanup() {
    let server = Server::new("new", false).await;
    let runtime = runtime(&server, SecurityConfig::default(), 2000);
    let gate = runtime.refresh_gate.clone();
    let _ = std::thread::spawn(move || {
        let _guard = gate.entries.lock().unwrap();
        panic!("test-only lock poisoning");
    })
    .join();
    assert!(
        verify_jwt_token(&runtime, &valid("new"), JwtExpiryMode::Enforce)
            .await
            .is_ok()
    );
    let entries = runtime
        .refresh_gate
        .entries
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    assert!(entries[&gate_key(&[])].active.is_none());
    assert_eq!(server.requests(), 1);
}

#[tokio::test]
async fn controlled_cached_nbf_mutation_cannot_bypass_validation() {
    let server = Server::new("known", false).await;
    let runtime = runtime(&server, SecurityConfig::default(), 2000);
    let token = valid("known");
    assert!(
        verify_jwt_token(&runtime, &token, JwtExpiryMode::Enforce)
            .await
            .is_ok()
    );
    runtime
        .cache
        .lock()
        .unwrap()
        .get_mut(&token)
        .unwrap()
        .principal
        .claims["nbf"] = serde_json::json!(now() + 120);
    assert!(cached_principal(&runtime, &token).is_none());
    // The authentic signed token has no nbf. Re-verification restores its actual claims.
    let verified = verify_jwt_token(&runtime, &token, JwtExpiryMode::Enforce)
        .await
        .unwrap();
    assert!(verified.claims.get("nbf").is_none());
    assert_eq!(server.requests(), 1);
}

#[test]
fn gate_identity_is_a_set_without_separator_collisions() {
    assert_eq!(
        gate_key(&[" b ".into(), "a".into(), "b".into()]),
        gate_key(&["a".into(), "b".into()])
    );
    assert_ne!(
        gate_key(&["a,b".into()]),
        gate_key(&["a".into(), "b".into()])
    );
    assert_eq!(gate_key(&[" ".into()]), gate_key(&[]));
    // Lookup order is intentionally not normalized to set order.
    assert_eq!(
        normalized_service_ids(&[" b ".into(), "a".into()]),
        vec!["b", "a"]
    );
}

#[test]
fn runtime_shutdown_drops_detached_attempt_without_wedging_gate() {
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let runtime = executor.block_on(async {
        let server = Server::new("new", true).await;
        let runtime = runtime(&server, SecurityConfig::default(), 2000);
        let token = valid("new");
        let mut f = Box::pin(verify_jwt_token(&runtime, &token, JwtExpiryMode::Enforce));
        pending(&mut f).await;
        server.request_started().await;
        drop(f);
        assert!(
            runtime.refresh_gate.entries.lock().unwrap()[&gate_key(&[])]
                .active
                .is_some()
        );
        runtime
    });
    drop(executor);
    inactive_recorded(&runtime);
}

#[tokio::test]
async fn failed_bootstrap_is_ungated_but_records_attempt() {
    let server = Server::new("known", false).await;
    server.respond(503, "test-only failure".into());
    let runtime = runtime(
        &server,
        SecurityConfig {
            bootstrap_from_key_service: true,
            ..Default::default()
        },
        2000,
    );
    assert_eq!(runtime.bootstrap().await.unwrap_err().status, 502);
    inactive_recorded(&runtime);
    assert_eq!(
        verify_jwt_token(&runtime, &valid("unknown"), JwtExpiryMode::Enforce)
            .await
            .unwrap_err()
            .status,
        401
    );
    assert_eq!(server.requests(), 1);
    server.respond(200, keys("known", SECRET));
    runtime.bootstrap().await.unwrap();
    assert_eq!(server.requests(), 2);
}

#[tokio::test]
async fn timeout_completion_clears_active_and_allows_post_cooldown_recovery() {
    let server = Server::new("new", true).await;
    let runtime = runtime(&server, SecurityConfig::default(), 50);
    let token = valid("new");
    let mut f = Box::pin(verify_jwt_token(&runtime, &token, JwtExpiryMode::Enforce));
    pending(&mut f).await;
    server.request_started().await;
    let mut completion = runtime.refresh_gate.entries.lock().unwrap()[&gate_key(&[])]
        .active
        .clone()
        .unwrap();
    assert_eq!(f.await.unwrap_err().status, 502);
    // The caller and task timers can fire together. Observe actual task cleanup.
    let _ = tokio::time::timeout(Duration::from_secs(1), completion.changed())
        .await
        .unwrap();
    assert!(completion.borrow().as_ref().unwrap().is_err());
    inactive_recorded(&runtime);
    assert_eq!(server.requests(), 1);
    assert_eq!(
        verify_jwt_token(&runtime, &token, JwtExpiryMode::Enforce)
            .await
            .unwrap_err()
            .status,
        401
    );
    cooldown().await;
    server.release(2);
    assert!(
        verify_jwt_token(&runtime, &token, JwtExpiryMode::Enforce)
            .await
            .is_ok()
    );
    inactive_recorded(&runtime);
    assert_eq!(server.requests(), 2);
}
