//! Explicit conformance-only gate hooks; the app harness owns Init and redaction.
use super::*;
use serde_json::{Value, json};
use std::{future::Future, path::Path, pin::Pin};
async fn pending_once<F: Future>(mut f: Pin<&mut F>) {
    std::future::poll_fn(|cx| {
        assert!(
            f.as_mut().poll(cx).is_pending(),
            "expected actual pending verification"
        );
        std::task::Poll::Ready(())
    })
    .await;
}
async fn signal(root: &Path, phase: &str) {
    std::fs::write(root.join("phase"), phase).unwrap();
    let end = Instant::now() + Duration::from_secs(2);
    while !root.join(format!("release-{phase}")).exists() {
        assert!(Instant::now() < end, "owned parent barrier expired");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}
fn state(runtime: &SecurityRuntime, ids: &[String]) -> (bool, Option<Instant>, usize, usize) {
    let entries = runtime.refresh_gate.entries.lock().unwrap();
    let e = entries.get(&gate_key(ids)).unwrap();
    (e.active.is_some(), e.last_attempt, e.starts, e.joined)
}
pub async fn component_probe(config: &RuntimeConfig, data: Value) {
    let root = Path::new(data["control"].as_str().unwrap());
    let runtime = load_security_runtime(config, true).unwrap().unwrap();
    runtime.bootstrap().await.unwrap();
    let f = &data["fixture"];
    let token = data["token"].as_str().unwrap();
    let probe = if f["component"] == "rustJwksGateCancellationS13a" {
        &f["jwksGateCancellationProbe"]
    } else {
        &f["jwksGatePanicProbe"]
    };
    let ids: Vec<String> = serde_json::from_value(probe["serviceIds"].clone()).unwrap();
    let actual = if f["component"] == "rustJwksGateCancellationS13a" {
        let mut verification = Box::pin(verify_jwt_token_for_services_with_deadline(
            &runtime,
            token,
            JwtExpiryMode::Enforce,
            &ids,
            Some(Instant::now() + Duration::from_secs(1)),
        ));
        pending_once(verification.as_mut()).await;
        signal(root, "fetch-waiting").await;
        let before = state(&runtime, &ids);
        runtime
            .refresh_gate
            .abort_handle
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .abort();
        let error = verification.await.unwrap_err();
        let after = state(&runtime, &ids);
        let followup = verify_jwt_token_for_services_with_deadline(
            &runtime,
            token,
            JwtExpiryMode::Enforce,
            &ids,
            Some(Instant::now() + Duration::from_secs(1)),
        )
        .await
        .unwrap_err();
        json!({"observation":{"outcome":"jwksGateCancellation","prerequisiteReady":true,"backendCalls":[],"gate":{"fetchStarted":before.2,"fetchCancelled":usize::from(error.status==502 && before.0 && !after.0),"active":after.0,"attemptRecordedBeforeIo":before.1.is_some(),"cooldownRecorded":before.1==after.1 && after.1.is_some(),"followupUnknownKid":{"outcome":"verificationRejected","code":followup.code}}}})
    } else {
        assert_eq!(f["component"], "rustJwksGatePanicS13c");
        let (release, barrier) = tokio::sync::oneshot::channel();
        *runtime.refresh_gate.before_fetch.lock().unwrap() = Some(barrier);
        let start = Instant::now();
        let deadline = start + Duration::from_secs(1);
        let mut callers = vec![];
        for _ in 0..3 {
            let mut caller = Box::pin(verify_jwt_token_for_services_with_deadline(
                &runtime,
                token,
                JwtExpiryMode::Enforce,
                &ids,
                Some(deadline),
            ));
            pending_once(caller.as_mut()).await;
            callers.push(caller);
        }
        let before = state(&runtime, &ids);
        assert!(before.0 && before.1.is_some());
        assert_eq!((before.2, before.3), (1, 2));
        signal(root, "panic-ready").await;
        release.send(()).unwrap();
        let mut errors = vec![];
        let mut waits = vec![];
        let mut verified = 0;
        for caller in callers {
            let result = caller.await;
            waits.push(start.elapsed().as_millis() as u64);
            if let Err(error) = result {
                errors.push(json!(format!("{}:{}", error.status, error.code)));
            } else {
                verified += 1;
                errors.push(Value::Null);
            }
        }
        let failed = state(&runtime, &ids);
        assert!(!failed.0);
        assert!(Instant::now() < deadline);
        let kid = f["tokens"]["gate-parallel"]["kid"].as_str().unwrap();
        assert!(
            cached_jwk_for_services(&runtime, Some(kid), &ids)
                .await
                .is_none(),
            "failed pre-I/O refresh must not publish a resident key"
        );
        signal(root, "panic-settled").await;
        tokio::time::sleep_until(
            failed.1.unwrap() + Duration::from_secs(runtime.config.jwks_refresh_min_interval_secs),
        )
        .await;
        signal(root, "recovery-ready").await;
        let principal = verify_jwt_token_for_services_with_deadline(
            &runtime,
            token,
            JwtExpiryMode::Enforce,
            &ids,
            Some(Instant::now() + Duration::from_secs(1)),
        )
        .await;
        let recovered = state(&runtime, &ids);
        let cached = cached_principal(&runtime, token).is_some();
        let published = cached_jwk_for_services(&runtime, Some(kid), &ids).await;
        signal(root, "reuse-ready").await;
        let reused = verify_jwt_token_for_services_with_deadline(
            &runtime,
            token,
            JwtExpiryMode::Enforce,
            &ids,
            Some(Instant::now() + Duration::from_secs(1)),
        )
        .await;
        let after_reuse = state(&runtime, &ids);
        let resident = cached_jwk_for_services(&runtime, Some(kid), &ids).await;
        let unchanged = published.is_some()
            && serde_json::to_value(&published).unwrap()
                == serde_json::to_value(&resident).unwrap();
        json!({"observation":{"outcome":"jwksGatePanicRecovery","prerequisiteReady":true,"backendCalls":[],"failure":{"taskStarts":before.2,"joinersAttached":before.3,"waitersTerminated":errors.len(),"verifiedPrincipals":verified,"attemptRecordedBeforeTask":before.1.is_some(),"activePublishedBeforePanic":before.0,"activeAfterCleanup":failed.0,"cooldownRetained":before.1==failed.1 && failed.1.is_some(),"callerBudgetMs":1000,"publicErrorMapping":"unspecified-by-approved-contract"},"recovery":{"afterCooldown":Instant::now().duration_since(failed.1.unwrap())>=Duration::from_secs(runtime.config.jwks_refresh_min_interval_secs),"taskStarts":recovered.2-failed.2,"joinersAttached":recovered.3-failed.3,"verifiedPrincipals":usize::from(principal.is_ok()),"cachePublished":cached,"scopedPrincipalCachePublished":cached,"jwksPublished":published.is_some(),"reuse":{"verifiedPrincipals":usize::from(reused.is_ok()),"taskStarts":after_reuse.2-recovered.2,"joinersAttached":after_reuse.3-recovered.3,"residentKeyUnchanged":unchanged},"activeAfterCompletion":recovered.0}},"gatePanicEvidence":{"barrierEvents":["attempt-recorded-active-published","two-joiners-attached","release-panic","all-waiters-settled","cooldown-elapsed","release-recovery"],"callerWaitsMs":waits,"callerErrors":errors}})
    };
    std::fs::write(
        data["output"].as_str().unwrap(),
        serde_json::to_vec(&actual).unwrap(),
    )
    .unwrap();
}
