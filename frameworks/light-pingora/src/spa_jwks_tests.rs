use super::*;
use crate::msal_auth::policy_tests::{claims, jwks_server, sign};
use jsonwebtoken::{EncodingKey, Header};
use std::sync::atomic::{AtomicU64, Ordering};

struct ManualClock {
    base: Instant,
    seconds: AtomicU64,
}
impl ManualClock {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            base: Instant::now(),
            seconds: AtomicU64::new(0),
        })
    }
    fn advance(&self, seconds: u64) {
        self.seconds.fetch_add(seconds, Ordering::SeqCst);
    }
}
impl Clock for ManualClock {
    fn now(&self) -> Instant {
        self.base + Duration::from_secs(self.seconds.load(Ordering::SeqCst))
    }
}

const KEYS: &str = include_str!("../tests/fixtures/msal/signing-key.jwks.json");

fn unknown(kid: &str) -> String {
    let mut header = Header::new(Algorithm::RS256);
    header.kid = Some(kid.into());
    jsonwebtoken::encode(
        &header,
        &claims(),
        &EncodingKey::from_rsa_pem(include_bytes!("../tests/fixtures/msal/signing-key.pem"))
            .unwrap(),
    )
    .unwrap()
}

#[test]
fn response_freshness_has_a_thirty_second_floor_and_five_minute_ceiling() {
    for (directive, expected) in [
        ("no-cache", 30),
        ("no-store", 30),
        ("max-age=0", 30),
        ("max-age=1", 30),
        ("max-age=29", 30),
        ("max-age=45", 45),
        ("max-age=900", 300),
        ("no-cache, max-age=900", 30),
    ] {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("cache-control", directive.parse().unwrap());
        assert_eq!(freshness(&headers).as_secs(), expected, "{directive}");
    }
}

#[tokio::test]
async fn cache_directives_refresh_at_the_bounded_deadline_with_a_controllable_clock() {
    for (directive, seconds) in [
        ("no-cache", 30),
        ("no-store", 30),
        ("max-age=0", 30),
        ("max-age=1", 30),
        ("max-age=45", 45),
    ] {
        let server = jwks_server(KEYS).await;
        *server.cache_control.write().await = directive.into();
        let mut source = JwksSource::new(&server.uri).unwrap();
        source.client = server.client;
        let clock = ManualClock::new();
        source.clock = clock.clone();
        source.bootstrap().await.unwrap();
        clock.advance(seconds - 1);
        source.verify(&sign(&claims()), 0).await.unwrap();
        assert_eq!(server.requests.load(Ordering::SeqCst), 1);
        clock.advance(1);
        source.verify(&sign(&claims()), 0).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if state_lock(&source.state).generation == 2 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(server.requests.load(Ordering::SeqCst), 2);
        assert_eq!(
            state_lock(&source.state).fresh_until.unwrap(),
            clock.now() + Duration::from_secs(seconds)
        );
    }
}

#[tokio::test]
async fn reprieve_is_uncertainty_then_a_fresh_miss_is_stably_terminal_and_bounded() {
    let server = jwks_server(KEYS).await;
    let mut source = JwksSource::new(&server.uri).unwrap();
    source.client = server.client;
    let clock = ManualClock::new();
    source.clock = clock.clone();
    source.bootstrap().await.unwrap();
    let token = unknown("new-key");
    for _ in 0..5 {
        assert_eq!(source.verify(&token, 0).await.unwrap_err().status, 503);
    }
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
    clock.advance(30);
    for _ in 0..5 {
        assert_eq!(source.verify(&token, 0).await.unwrap_err().status, 401);
    }
    assert_eq!(server.requests.load(Ordering::SeqCst), 2);
    // Bounded negative evidence, never an unbounded map of attacker-controlled kids.
    let mut state = state_lock(&source.state);
    let deadline = state.negative["new-key"];
    clock.advance(1);
    JwksSource::negative(&mut state, "new-key", clock.now());
    assert_eq!(
        state.negative["new-key"], deadline,
        "same evidence cannot extend a negative result"
    );
    for i in 0..1000 {
        JwksSource::negative(&mut state, &format!("id-{i}"), clock.now());
    }
    assert_eq!(state.negative.len(), MAX_NEGATIVE);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_unknown_lookup_does_not_block_known_keys_and_waiters_are_bounded() {
    let server = jwks_server(KEYS).await;
    let mut source = JwksSource::new(&server.uri).unwrap();
    source.client = server.client;
    let clock = ManualClock::new();
    source.clock = clock.clone();
    source.bootstrap().await.unwrap();
    clock.advance(300);
    server.delay_ms.store(750, Ordering::SeqCst);
    let source = Arc::new(source);
    let mut tasks = Vec::new();
    for _ in 0..80 {
        let source = source.clone();
        tasks.push(tokio::spawn(async move {
            source
                .verify(&unknown("unknown"), 0)
                .await
                .err()
                .unwrap()
                .status
        }));
    }
    while server.requests.load(Ordering::SeqCst) < 2 {
        tokio::task::yield_now().await;
    }
    tokio::time::timeout(
        Duration::from_millis(100),
        source.verify(&sign(&claims()), 0),
    )
    .await
    .unwrap()
    .unwrap();
    let mut statuses = Vec::new();
    for task in tasks {
        statuses.push(task.await.unwrap());
    }
    assert!(statuses.iter().all(|status| matches!(status, 401 | 503)));
    assert!(statuses.contains(&503));
    assert_eq!(server.requests.load(Ordering::SeqCst), 2);
    assert_eq!(source.waiters.available_permits(), 32);
}

#[tokio::test]
async fn stale_key_outage_retention_is_bounded_and_success_discards_superseded_keys() {
    let server = jwks_server(KEYS).await;
    *server.cache_control.write().await = "max-age=0".into();
    let mut source = JwksSource::new(&server.uri).unwrap();
    source.client = server.client;
    let clock = ManualClock::new();
    source.clock = clock.clone();
    source.bootstrap().await.unwrap();
    *server.body.write().await = "malformed JWKS".into();
    clock.advance(30);
    source.verify(&sign(&claims()), 0).await.unwrap();
    // Wait for the background attempt, not a fake clock sleep.
    loop {
        if state_lock(&source.state).failed {
            break;
        }
        tokio::task::yield_now().await;
    }
    for _ in 0..5 {
        source.verify(&sign(&claims()), 0).await.unwrap();
    }
    assert_eq!(server.requests.load(Ordering::SeqCst), 2);
    clock.advance(3600);
    assert_eq!(
        source.verify(&sign(&claims()), 0).await.unwrap_err().status,
        503
    );
    *server.body.write().await = "{\"keys\":[]}".into();
    clock.advance(5);
    assert_eq!(
        source.verify(&sign(&claims()), 0).await.unwrap_err().status,
        401
    );
    assert!(state_lock(&source.state).keys.is_empty());
}

#[tokio::test]
async fn duplicate_eligible_keys_are_terminal_and_ineligible_keys_cannot_conflict() {
    let mut keys: serde_json::Value = serde_json::from_str(KEYS).unwrap();
    let entry = keys["keys"][0].clone();
    keys["keys"].as_array_mut().unwrap().push(entry.clone());
    let server = jwks_server(&keys.to_string()).await;
    let mut source = JwksSource::new(&server.uri).unwrap();
    source.client = server.client;
    let clock = ManualClock::new();
    source.clock = clock.clone();
    source.bootstrap().await.unwrap();
    for _ in 0..5 {
        assert_eq!(
            source.verify(&sign(&claims()), 0).await.unwrap_err().status,
            401
        );
    }
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
    keys["keys"][1]["use"] = serde_json::json!("enc");
    *server.body.write().await = keys.to_string();
    clock.advance(300);
    assert_eq!(
        source.verify(&sign(&claims()), 0).await.unwrap_err().status,
        401
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if state_lock(&source.state).generation == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    source.verify(&sign(&claims()), 0).await.unwrap();
}

#[tokio::test]
async fn source_specific_ca_extension_and_replacement_have_independent_state() {
    let first = jwks_server(KEYS).await;
    let second = jwks_server(KEYS).await;
    let dir = tempfile::tempdir().unwrap();
    let ca = dir.path().join("endpoint-ca.pem");
    std::fs::write(&ca, first.ca_pem).unwrap();
    let source = JwksSource::with_roots(&first.uri, &[ca.to_string_lossy().into_owned()]).unwrap();
    source.verify(&sign(&claims()), 0).await.unwrap();
    let foreign =
        JwksSource::with_roots(&second.uri, &[ca.to_string_lossy().into_owned()]).unwrap();
    assert_eq!(
        foreign
            .verify(&sign(&claims()), 0)
            .await
            .unwrap_err()
            .status,
        503
    );
    let replacement =
        JwksSource::with_roots(&first.uri, &[ca.to_string_lossy().into_owned()]).unwrap();
    assert!(state_lock(&replacement.state).keys.is_empty());
    replacement.verify(&sign(&claims()), 0).await.unwrap();
    assert_eq!(first.requests.load(Ordering::SeqCst), 2);
}

#[tokio::test]
#[ignore = "opt-in public TLS network qualification; no authentication or deployment"]
async fn additional_ca_preserves_default_public_roots() {
    let local = jwks_server(KEYS).await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("local-ca.pem");
    std::fs::write(&path, local.ca_pem).unwrap();
    let client = client_with_roots(&[path.to_string_lossy().into_owned()]).unwrap();
    client
        .get(&local.uri)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    client
        .get("https://login.microsoftonline.com/common/discovery/v2.0/keys")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
}

#[tokio::test]
async fn cancelled_lookup_keeps_its_refresh_single_flight_owned_until_completion() {
    let server = jwks_server(KEYS).await;
    server.delay_ms.store(50, Ordering::SeqCst);
    let mut source = JwksSource::new(&server.uri).unwrap();
    source.client = server.client;
    let source = Arc::new(source);
    let task = tokio::spawn({
        let source = source.clone();
        async move { source.verify(&sign(&claims()), 0).await }
    });
    while server.requests.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    task.abort();
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if !state_lock(&source.state).keys.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    source.verify(&sign(&claims()), 0).await.unwrap();
    assert_eq!(server.requests.load(Ordering::SeqCst), 1);
}
