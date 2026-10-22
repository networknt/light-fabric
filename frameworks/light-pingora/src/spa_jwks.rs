//! Snapshot-owned RS256 signing source. No generic key routing or principal cache.
use crate::HandlerRejection;
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use light_runtime::RuntimeError;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;

const MAX_BODY: usize = 1024 * 1024;
const REPRIEVE: Duration = Duration::from_secs(30);
const COOLDOWN: Duration = Duration::from_secs(5);
const STALE: Duration = Duration::from_secs(3600);
const MAX_NEGATIVE: usize = 256;

pub(crate) fn invalid() -> HandlerRejection {
    HandlerRejection::new(
        401,
        "ERR10000",
        "Token signature or trust policy is invalid",
    )
}
pub(crate) fn unavailable() -> HandlerRejection {
    HandlerRejection::new(
        503,
        "ERR10056",
        "Token signing source temporarily unavailable",
    )
    .with_header("retry-after", "1")
    .with_header("cache-control", "no-store")
}

pub(crate) trait Clock: Send + Sync {
    fn now(&self) -> Instant;
}
struct SystemClock;
impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

#[derive(Default)]
struct KeyState {
    keys: BTreeMap<String, DecodingKey>,
    duplicates: BTreeSet<String>,
    fresh_until: Option<Instant>,
    confirmed_at: Option<Instant>,
    attempted_at: Option<Instant>,
    failed: bool,
    generation: u64,
    negative: BTreeMap<String, Instant>,
}

#[derive(Clone)]
pub(crate) struct JwksSource {
    uri: String,
    pub(crate) client: reqwest::Client,
    state: Arc<Mutex<KeyState>>,
    gate: Arc<Semaphore>,
    waiters: Arc<Semaphore>,
    clock: Arc<dyn Clock>,
}

fn state_lock(state: &Mutex<KeyState>) -> std::sync::MutexGuard<'_, KeyState> {
    state.lock().unwrap_or_else(|error| error.into_inner())
}

pub(crate) fn validate_endpoint(uri: &str) -> Result<(), RuntimeError> {
    let valid = url::Url::parse(uri).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
            && uri.trim() == uri
    });
    if !valid {
        return Err(RuntimeError::Unsupported(
            "SPA JWKS endpoint must be explicit HTTPS without credentials or fragment".into(),
        ));
    }
    Ok(())
}

pub(crate) fn client_with_roots(paths: &[String]) -> Result<reqwest::Client, RuntimeError> {
    let mut builder = reqwest::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(5));
    // add_root_certificate extends the default public trust store; never replace it.
    for path in paths {
        let pem = std::fs::read(path)
            .map_err(|_| RuntimeError::Unsupported("Cannot read SPA JWKS CA certificate".into()))?;
        let certificates = reqwest::Certificate::from_pem_bundle(&pem)
            .map_err(|_| RuntimeError::Unsupported("Invalid SPA JWKS CA certificate".into()))?;
        if certificates.is_empty() {
            return Err(RuntimeError::Unsupported(
                "Empty SPA JWKS CA certificate bundle".into(),
            ));
        }
        for cert in certificates {
            builder = builder.add_root_certificate(cert);
        }
    }
    builder
        .build()
        .map_err(|_| RuntimeError::Unsupported("Cannot create SPA JWKS client".into()))
}

impl JwksSource {
    #[cfg(test)]
    pub(crate) fn seed_test_keys(&self, jwks: &str) {
        let set: JwkSet = serde_json::from_str(jwks).unwrap();
        let mut state = state_lock(&self.state);
        state.keys = set
            .keys
            .iter()
            .map(|key| {
                (
                    key.common.key_id.clone().unwrap(),
                    DecodingKey::from_jwk(key).unwrap(),
                )
            })
            .collect();
        state.fresh_until = Some(self.clock.now() + Duration::from_secs(300));
        state.confirmed_at = Some(self.clock.now());
        state.attempted_at = Some(self.clock.now());
        state.generation += 1;
    }

    #[cfg(test)]
    pub(crate) fn allow_test_refresh(&self) {
        state_lock(&self.state).attempted_at = None;
    }

    #[cfg(test)]
    pub(crate) fn new(uri: &str) -> Result<Self, RuntimeError> {
        Self::with_roots(uri, &[])
    }
    pub(crate) fn with_roots(uri: &str, roots: &[String]) -> Result<Self, RuntimeError> {
        validate_endpoint(uri)?;
        Ok(Self {
            uri: uri.into(),
            client: client_with_roots(roots)?,
            state: Arc::new(Mutex::new(KeyState::default())),
            gate: Arc::new(Semaphore::new(1)),
            waiters: Arc::new(Semaphore::new(32)),
            clock: Arc::new(SystemClock),
        })
    }

    pub(crate) async fn bootstrap(&self) -> Result<(), HandlerRejection> {
        if state_lock(&self.state)
            .fresh_until
            .is_some_and(|until| self.clock.now() < until)
        {
            return Ok(());
        }
        self.refresh(None).await
    }

    async fn fetch(
        &self,
    ) -> Result<(BTreeMap<String, DecodingKey>, BTreeSet<String>, Duration), HandlerRejection> {
        let mut response = self
            .client
            .get(&self.uri)
            .send()
            .await
            .map_err(|_| unavailable())?
            .error_for_status()
            .map_err(|_| unavailable())?;
        let ttl = freshness(response.headers());
        if response
            .content_length()
            .is_some_and(|n| n > MAX_BODY as u64)
        {
            return Err(unavailable());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| unavailable())? {
            if chunk.len() > MAX_BODY.saturating_sub(bytes.len()) {
                return Err(unavailable());
            }
            bytes.extend_from_slice(&chunk);
        }
        let set: JwkSet = serde_json::from_slice(&bytes).map_err(|_| unavailable())?;
        if set.keys.len() > 512 {
            return Err(unavailable());
        }
        let mut keys = BTreeMap::new();
        let mut duplicates = BTreeSet::new();
        for jwk in set.keys {
            if jwk
                .common
                .key_algorithm
                .is_some_and(|alg| alg != jsonwebtoken::jwk::KeyAlgorithm::RS256)
                || jwk
                    .common
                    .public_key_use
                    .as_ref()
                    .is_some_and(|usage| *usage != jsonwebtoken::jwk::PublicKeyUse::Signature)
                || jwk
                    .common
                    .key_operations
                    .as_ref()
                    .is_some_and(|ops| !ops.contains(&jsonwebtoken::jwk::KeyOperations::Verify))
                || !matches!(
                    jwk.algorithm,
                    jsonwebtoken::jwk::AlgorithmParameters::RSA(_)
                )
            {
                continue;
            }
            let Some(id) = jwk
                .common
                .key_id
                .as_ref()
                .filter(|id| !id.is_empty() && id.len() <= 128)
            else {
                continue;
            };
            let key = DecodingKey::from_jwk(&jwk).map_err(|_| unavailable())?;
            if keys.insert(id.clone(), key).is_some() {
                duplicates.insert(id.clone());
            }
        }
        for id in &duplicates {
            keys.remove(id);
        }
        Ok((keys, duplicates, ttl))
    }

    fn negative(state: &mut KeyState, kid: &str, now: Instant) {
        state.negative.retain(|_, until| *until > now);
        if state.negative.len() >= MAX_NEGATIVE
            && let Some(id) = state
                .negative
                .iter()
                .min_by_key(|(_, until)| **until)
                .map(|(id, _)| id.clone())
        {
            state.negative.remove(&id);
        }
        // Waiting callers may use the same new snapshot as evidence, but cannot
        // extend its negative lifetime beyond thirty seconds after confirmation.
        let confirmed = state.confirmed_at.unwrap_or(now);
        state.negative.insert(kid.into(), confirmed + REPRIEVE);
    }

    fn cooling(state: &KeyState, now: Instant) -> bool {
        state.attempted_at.is_some_and(|last| {
            now.saturating_duration_since(last) < if state.failed { COOLDOWN } else { REPRIEVE }
        })
    }

    async fn refresh(&self, requested: Option<&str>) -> Result<(), HandlerRejection> {
        let generation = state_lock(&self.state).generation;
        let _waiter = self
            .waiters
            .clone()
            .try_acquire_owned()
            .map_err(|_| unavailable())?;
        let permit = tokio::time::timeout(
            Duration::from_millis(500),
            self.gate.clone().acquire_owned(),
        )
        .await
        .map_err(|_| unavailable())?
        .map_err(|_| unavailable())?;
        {
            let mut state = state_lock(&self.state);
            let now = self.clock.now();
            if state.generation != generation {
                if let Some(kid) = requested
                    && !state.keys.contains_key(kid)
                {
                    Self::negative(&mut state, kid, now);
                    return Err(invalid());
                }
                return Ok(());
            }
            if Self::cooling(&state, now) {
                return Err(unavailable());
            }
            state.attempted_at = Some(now);
        }
        // All network work is owned by a task, so cancellation of a waiter cannot
        // leave a permanently pending refresh or abandon its permit/state update.
        let source = self.clone();
        let requested = requested.map(str::to_owned);
        let (send, receive) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _permit = permit;
            let result = source.fetch_and_install(requested.as_deref()).await;
            let _ = send.send(result);
        });
        receive.await.map_err(|_| unavailable())?
    }

    async fn fetch_and_install(&self, requested: Option<&str>) -> Result<(), HandlerRejection> {
        let result = self.fetch().await;
        let now = self.clock.now();
        let mut state = state_lock(&self.state);
        match result {
            Ok((keys, duplicates, ttl)) => {
                state.keys = keys;
                state.duplicates = duplicates;
                state.negative.clear();
                state.fresh_until = Some(now + ttl);
                state.confirmed_at = Some(now);
                state.failed = false;
                state.generation = state.generation.wrapping_add(1);
                if let Some(kid) = requested
                    && !state.keys.contains_key(kid)
                {
                    Self::negative(&mut state, kid, now);
                    return Err(invalid());
                }
                Ok(())
            }
            Err(error) => {
                state.failed = true;
                Err(error)
            }
        }
    }

    fn background_refresh(&self) {
        let Ok(permit) = self.gate.clone().try_acquire_owned() else {
            return;
        };
        {
            let mut state = state_lock(&self.state);
            let now = self.clock.now();
            if Self::cooling(&state, now) {
                return;
            }
            state.attempted_at = Some(now);
        }
        let source = self.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let _ = source.fetch_and_install(None).await;
        });
    }

    async fn key(&self, kid: &str) -> Result<DecodingKey, HandlerRejection> {
        let cached = {
            let state = state_lock(&self.state);
            let now = self.clock.now();
            if state.duplicates.contains(kid) {
                let stale = state.fresh_until.is_none_or(|until| now >= until);
                drop(state);
                if stale {
                    self.background_refresh();
                }
                return Err(invalid());
            }
            if state.negative.get(kid).is_some_and(|until| *until > now) {
                return Err(invalid());
            }
            state
                .keys
                .get(kid)
                .filter(|_| state.fresh_until.is_some_and(|until| now < until + STALE))
                .map(|key| {
                    (
                        key.clone(),
                        state.fresh_until.is_some_and(|until| now >= until),
                    )
                })
        };
        if let Some((key, stale)) = cached {
            if stale {
                self.background_refresh();
            }
            return Ok(key);
        }
        self.refresh(Some(kid)).await?;
        state_lock(&self.state)
            .keys
            .get(kid)
            .cloned()
            .ok_or_else(invalid)
    }

    pub(crate) async fn verify(
        &self,
        token: &str,
        skew: u64,
    ) -> Result<serde_json::Value, HandlerRejection> {
        self.verify_expiry(token, skew, false).await
    }
    pub(crate) async fn verify_expiry(
        &self,
        token: &str,
        skew: u64,
        allow_expired: bool,
    ) -> Result<serde_json::Value, HandlerRejection> {
        if token.is_empty() || token.len() > 65536 {
            return Err(invalid());
        }
        let header = decode_header(token).map_err(|_| invalid())?;
        if header.alg != Algorithm::RS256 {
            return Err(invalid());
        }
        let kid = header
            .kid
            .filter(|kid| !kid.is_empty() && kid.len() <= 128)
            .ok_or_else(invalid)?;
        let key = self.key(&kid).await?;
        let mut validation = Validation::new(Algorithm::RS256);
        validation.leeway = skew;
        validation.validate_aud = false;
        validation.validate_nbf = true;
        validation.validate_exp = !allow_expired;
        match decode::<serde_json::Value>(token, &key, &validation) {
            Ok(data) => Ok(data.claims),
            Err(error)
                if matches!(
                    error.kind(),
                    jsonwebtoken::errors::ErrorKind::InvalidSignature
                ) =>
            {
                // A known-key signature failure is terminal unless a permitted
                // fresh lookup proves a same-kid rotation. It never rotates behind 503.
                if self.refresh(Some(&kid)).await.is_ok() {
                    let key = state_lock(&self.state)
                        .keys
                        .get(&kid)
                        .cloned()
                        .ok_or_else(invalid)?;
                    return decode::<serde_json::Value>(token, &key, &validation)
                        .map(|data| data.claims)
                        .map_err(|_| invalid());
                }
                Err(invalid())
            }
            Err(_) => Err(invalid()),
        }
    }
}

fn freshness(headers: &reqwest::header::HeaderMap) -> Duration {
    let mut ttl = 300;
    for value in headers.get_all("cache-control") {
        if let Ok(value) = value.to_str() {
            for directive in value.split(',').map(str::trim) {
                if directive.eq_ignore_ascii_case("no-cache")
                    || directive.eq_ignore_ascii_case("no-store")
                {
                    ttl = 0;
                }
                if let Some((name, value)) = directive.split_once('=')
                    && name.trim().eq_ignore_ascii_case("max-age")
                    && let Ok(seconds) = value.trim().trim_matches('"').parse::<u64>()
                {
                    ttl = ttl.min(seconds);
                }
            }
        }
    }
    let age = headers
        .get("age")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    Duration::from_secs(ttl.saturating_sub(age).clamp(30, 300))
}

#[cfg(test)]
#[path = "spa_jwks_tests.rs"]
mod tests;
