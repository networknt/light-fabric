use async_trait::async_trait;
use light_runtime::{RuntimeCache, RuntimeError};
use serde_json::{Value as JsonValue, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::time::timeout;
use uuid::Uuid;

pub const HMAC_REPLAY_CACHE_PREFIX: &str = "hmac-replay:";

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct WebhookReplayKey {
    digest: String,
}

impl WebhookReplayKey {
    pub fn new(profile: &str, selector: &str, delivery_id: &str) -> Result<Self, ReplayAdminError> {
        for (name, value) in [
            ("profile", profile),
            ("selector", selector),
            ("deliveryId", delivery_id),
        ] {
            if value.trim().is_empty() {
                return Err(ReplayAdminError::Invalid(format!(
                    "{name} must be a non-empty string"
                )));
            }
        }
        let mut digest = Sha256::new();
        for value in [profile, selector, delivery_id] {
            let bytes = value.as_bytes();
            let length = u32::try_from(bytes.len()).map_err(|_| {
                ReplayAdminError::Invalid("replay identity component is too large".to_string())
            })?;
            digest.update(length.to_be_bytes());
            digest.update(bytes);
        }
        Ok(Self {
            digest: hex::encode(digest.finalize()),
        })
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }
}

impl fmt::Debug for WebhookReplayKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebhookReplayKey")
            .field("digest", &format_args!("{}...", &self.digest[..12]))
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ReplayReservation {
    key: WebhookReplayKey,
    owner_token: String,
}

impl ReplayReservation {
    fn create(key: WebhookReplayKey) -> Self {
        Self {
            key,
            owner_token: Uuid::new_v4().to_string(),
        }
    }

    pub fn key(&self) -> &WebhookReplayKey {
        &self.key
    }
}

impl fmt::Debug for ReplayReservation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReplayReservation")
            .field("key", &self.key)
            .field("owner_token", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReserveOutcome {
    Reserved(ReplayReservation),
    Duplicate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayStoreScope {
    Local,
    Distributed,
}

impl ReplayStoreScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Distributed => "distributed",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ReplayStoreError {
    #[error("replay store capacity is exhausted")]
    Capacity,
    #[error("replay store is unavailable")]
    Unavailable,
}

#[derive(Debug, thiserror::Error)]
pub enum ReplayAdminError {
    #[error("invalid replay removal request: {0}")]
    Invalid(String),
    #[error("unknown HMAC profile")]
    UnknownProfile,
    #[error("replay protection is not enabled for the HMAC profile")]
    ReplayDisabled,
    #[error(transparent)]
    Store(#[from] ReplayStoreError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayRemovalOutcome {
    pub removed: bool,
    pub scope: ReplayStoreScope,
}

#[async_trait]
pub trait WebhookReplayStore: Send + Sync {
    async fn reserve(
        &self,
        key: &WebhookReplayKey,
        retention: Duration,
    ) -> Result<ReserveOutcome, ReplayStoreError>;

    async fn release(&self, reservation: &ReplayReservation) -> Result<(), ReplayStoreError>;

    async fn force_remove(&self, key: &WebhookReplayKey) -> Result<bool, ReplayStoreError>;

    /// Returns the current entry count when the provider is an in-process store.
    /// Distributed providers deliberately return `None`: their global cardinality
    /// is neither cheap nor useful to sample on the request path.
    async fn local_entries(&self) -> Option<usize> {
        None
    }

    fn scope(&self) -> ReplayStoreScope;
}

#[derive(Clone)]
struct LocalEntry {
    owner_token: String,
    expires_at_millis: u64,
}

pub struct LocalWebhookReplayStore {
    max_entries: usize,
    entries: Mutex<HashMap<WebhookReplayKey, LocalEntry>>,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl LocalWebhookReplayStore {
    pub fn new(max_entries: usize) -> Self {
        let started = Instant::now();
        Self::with_clock(
            max_entries,
            Arc::new(move || started.elapsed().as_millis().try_into().unwrap_or(u64::MAX)),
        )
    }

    fn with_clock(max_entries: usize, clock: Arc<dyn Fn() -> u64 + Send + Sync>) -> Self {
        Self {
            max_entries,
            entries: Mutex::new(HashMap::new()),
            clock,
        }
    }

    fn entries(&self) -> MutexGuard<'_, HashMap<WebhookReplayKey, LocalEntry>> {
        self.entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    fn purge_expired(&self, entries: &mut HashMap<WebhookReplayKey, LocalEntry>, now: u64) {
        entries.retain(|_, entry| entry.expires_at_millis > now);
    }
}

#[async_trait]
impl WebhookReplayStore for LocalWebhookReplayStore {
    async fn reserve(
        &self,
        key: &WebhookReplayKey,
        retention: Duration,
    ) -> Result<ReserveOutcome, ReplayStoreError> {
        let retention = u64::try_from(retention.as_millis())
            .ok()
            .filter(|value| *value > 0)
            .ok_or(ReplayStoreError::Unavailable)?;
        let now = (self.clock)();
        let mut entries = self.entries();
        if let Some(entry) = entries.get(key) {
            if entry.expires_at_millis > now {
                return Ok(ReserveOutcome::Duplicate);
            }
            entries.remove(key);
        }
        if entries.len() >= self.max_entries {
            self.purge_expired(&mut entries, now);
            if entries.len() >= self.max_entries {
                return Err(ReplayStoreError::Capacity);
            }
        }
        let reservation = ReplayReservation::create(key.clone());
        entries.insert(
            key.clone(),
            LocalEntry {
                owner_token: reservation.owner_token.clone(),
                expires_at_millis: now.saturating_add(retention),
            },
        );
        Ok(ReserveOutcome::Reserved(reservation))
    }

    async fn release(&self, reservation: &ReplayReservation) -> Result<(), ReplayStoreError> {
        let mut entries = self.entries();
        if entries
            .get(reservation.key())
            .is_some_and(|entry| entry.owner_token == reservation.owner_token)
        {
            entries.remove(reservation.key());
        }
        Ok(())
    }

    async fn force_remove(&self, key: &WebhookReplayKey) -> Result<bool, ReplayStoreError> {
        Ok(self.entries().remove(key).is_some())
    }

    async fn local_entries(&self) -> Option<usize> {
        let now = (self.clock)();
        let mut entries = self.entries();
        self.purge_expired(&mut entries, now);
        Some(entries.len())
    }

    fn scope(&self) -> ReplayStoreScope {
        ReplayStoreScope::Local
    }
}

#[async_trait]
impl RuntimeCache for LocalWebhookReplayStore {
    async fn len(&self) -> usize {
        let now = (self.clock)();
        let mut entries = self.entries();
        self.purge_expired(&mut entries, now);
        entries.len()
    }

    async fn entries_summary(&self) -> JsonValue {
        json!({
            "scope": "local",
            "entries": self.len().await,
            "capacity": self.max_entries,
            "clearSupported": false
        })
    }

    fn clear_supported(&self) -> bool {
        false
    }

    async fn clear(&self) {}
}

pub(crate) struct RedisWebhookReplayStore {
    client: redis::Client,
    connection: tokio::sync::OnceCell<redis::aio::ConnectionManager>,
    key_prefix: String,
    connect_timeout: Duration,
    operation_timeout: Duration,
}

impl RedisWebhookReplayStore {
    pub(crate) fn new(
        url: &str,
        key_prefix: String,
        connect_timeout: Duration,
        operation_timeout: Duration,
    ) -> Result<Self, RuntimeError> {
        let parsed = url::Url::parse(url)
            .map_err(|_| RuntimeError::Config("invalid Redis replay-store URL".to_string()))?;
        if !matches!(parsed.scheme(), "redis" | "rediss") {
            return Err(RuntimeError::Config(
                "Redis replay-store URL must use redis or rediss".to_string(),
            ));
        }
        let client = redis::Client::open(url)
            .map_err(|_| RuntimeError::Config("invalid Redis replay-store URL".to_string()))?;
        Ok(Self {
            client,
            connection: tokio::sync::OnceCell::new(),
            key_prefix,
            connect_timeout,
            operation_timeout,
        })
    }

    fn storage_key(&self, key: &WebhookReplayKey) -> String {
        format!("{}{}", self.key_prefix, key.digest())
    }

    async fn connection(&self) -> Result<redis::aio::ConnectionManager, ReplayStoreError> {
        let connection = timeout(
            self.connect_timeout,
            self.connection
                .get_or_try_init(|| async { self.client.get_connection_manager().await }),
        )
        .await
        .map_err(|_| ReplayStoreError::Unavailable)?
        .map_err(|_| ReplayStoreError::Unavailable)?;
        Ok(connection.clone())
    }
}

#[async_trait]
impl WebhookReplayStore for RedisWebhookReplayStore {
    async fn reserve(
        &self,
        key: &WebhookReplayKey,
        retention: Duration,
    ) -> Result<ReserveOutcome, ReplayStoreError> {
        let retention = u64::try_from(retention.as_millis())
            .ok()
            .filter(|value| *value > 0)
            .ok_or(ReplayStoreError::Unavailable)?;
        let reservation = ReplayReservation::create(key.clone());
        let mut connection = self.connection().await?;
        let response = timeout(
            self.operation_timeout,
            redis::cmd("SET")
                .arg(self.storage_key(key))
                .arg(&reservation.owner_token)
                .arg("NX")
                .arg("PX")
                .arg(retention)
                .query_async::<Option<String>>(&mut connection),
        )
        .await
        .map_err(|_| ReplayStoreError::Unavailable)?
        .map_err(|_| ReplayStoreError::Unavailable)?;
        if response.as_deref() == Some("OK") {
            Ok(ReserveOutcome::Reserved(reservation))
        } else {
            Ok(ReserveOutcome::Duplicate)
        }
    }

    async fn release(&self, reservation: &ReplayReservation) -> Result<(), ReplayStoreError> {
        let mut connection = self.connection().await?;
        timeout(
            self.operation_timeout,
            redis::cmd("EVAL")
                .arg("if redis.call('get', KEYS[1]) == ARGV[1] then return redis.call('del', KEYS[1]) else return 0 end")
                .arg(1)
                .arg(self.storage_key(reservation.key()))
                .arg(&reservation.owner_token)
                .query_async::<i64>(&mut connection),
        )
        .await
        .map_err(|_| ReplayStoreError::Unavailable)?
        .map_err(|_| ReplayStoreError::Unavailable)?;
        Ok(())
    }

    async fn force_remove(&self, key: &WebhookReplayKey) -> Result<bool, ReplayStoreError> {
        let mut connection = self.connection().await?;
        let removed = timeout(
            self.operation_timeout,
            redis::cmd("DEL")
                .arg(self.storage_key(key))
                .query_async::<u64>(&mut connection),
        )
        .await
        .map_err(|_| ReplayStoreError::Unavailable)?
        .map_err(|_| ReplayStoreError::Unavailable)?;
        Ok(removed > 0)
    }

    fn scope(&self) -> ReplayStoreScope {
        ReplayStoreScope::Distributed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, TcpStream};

    fn key(delivery: &str) -> WebhookReplayKey {
        WebhookReplayKey::new("github", "shared", delivery).unwrap()
    }

    #[test]
    fn replay_key_matches_java_length_prefixed_contract() {
        assert_eq!(
            key("delivery-1").digest(),
            "292d360acd5de602b9e8a99f73d63a1df6ac20b606382a173289c2ea8b5a3501"
        );
        let unicode = WebhookReplayKey::new("github", "客户|一", "delivery\0id").unwrap();
        assert_eq!(
            unicode.digest(),
            "f348265a48b1272a91a25427da11a99e4beaee119bdd3583c62a4462546195ae"
        );
        assert!(WebhookReplayKey::new("", "shared", "delivery").is_err());
    }

    #[test]
    fn shared_java_rust_replay_key_conformance_fixture() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../fixtures/hmac-webhook-conformance-v1.json"
        ))
        .expect("parse shared HMAC fixture");
        for vector in fixture["replayKeyVectors"]
            .as_array()
            .expect("replay vectors")
        {
            let key = WebhookReplayKey::new(
                vector["profile"].as_str().expect("fixture profile"),
                vector["selector"].as_str().expect("fixture selector"),
                vector["deliveryId"].as_str().expect("fixture delivery ID"),
            )
            .expect("fixture replay key");
            assert_eq!(
                key.digest(),
                vector["digest"].as_str().expect("fixture digest"),
                "fixture `{}`",
                vector["name"]
            );
        }
    }

    #[tokio::test]
    async fn local_store_is_atomic_capacity_safe_and_owner_checked() {
        let clock = Arc::new(AtomicU64::new(100));
        let clock_fn = {
            let clock = Arc::clone(&clock);
            Arc::new(move || clock.load(Ordering::SeqCst)) as Arc<dyn Fn() -> u64 + Send + Sync>
        };
        let store = Arc::new(LocalWebhookReplayStore::with_clock(1, clock_fn));
        let first_key = key("first");
        let attempts = (0..100)
            .map(|_| {
                let store = Arc::clone(&store);
                let key = first_key.clone();
                tokio::spawn(async move { store.reserve(&key, Duration::from_millis(10)).await })
            })
            .collect::<Vec<_>>();
        let mut reserved = Vec::new();
        for attempt in attempts {
            if let ReserveOutcome::Reserved(reservation) = attempt.await.unwrap().unwrap() {
                reserved.push(reservation);
            }
        }
        assert_eq!(reserved.len(), 1);
        assert!(matches!(
            store.reserve(&key("second"), Duration::from_secs(1)).await,
            Err(ReplayStoreError::Capacity)
        ));
        assert!(store.force_remove(&first_key).await.unwrap());
        let newer = match store
            .reserve(&first_key, Duration::from_secs(1))
            .await
            .unwrap()
        {
            ReserveOutcome::Reserved(value) => value,
            ReserveOutcome::Duplicate => panic!("expected reservation"),
        };
        store.release(&reserved[0]).await.unwrap();
        assert_eq!(
            store
                .reserve(&first_key, Duration::from_secs(1))
                .await
                .unwrap(),
            ReserveOutcome::Duplicate
        );
        store.release(&newer).await.unwrap();
        clock.store(111, Ordering::SeqCst);
        assert!(matches!(
            store.reserve(&key("second"), Duration::from_secs(1)).await,
            Ok(ReserveOutcome::Reserved(_))
        ));
    }

    #[tokio::test]
    async fn local_cache_summary_is_redacted_and_bulk_clear_is_disabled() {
        let store = LocalWebhookReplayStore::new(4);
        let sensitive = key("sensitive-delivery");
        store
            .reserve(&sensitive, Duration::from_secs(60))
            .await
            .unwrap();
        let summary = store.entries_summary().await;
        assert_eq!(summary["entries"], 1);
        assert_eq!(summary["clearSupported"], false);
        assert!(!summary.to_string().contains("sensitive-delivery"));
        assert!(!store.clear_supported());
    }

    #[tokio::test]
    async fn local_store_purges_globally_only_on_capacity_pressure_or_observation() {
        let clock = Arc::new(AtomicU64::new(100));
        let clock_fn = {
            let clock = Arc::clone(&clock);
            Arc::new(move || clock.load(Ordering::SeqCst)) as Arc<dyn Fn() -> u64 + Send + Sync>
        };
        let store = LocalWebhookReplayStore::with_clock(3, clock_fn);
        store
            .reserve(&key("expired"), Duration::from_millis(10))
            .await
            .unwrap();
        clock.store(111, Ordering::SeqCst);
        store
            .reserve(&key("current"), Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(
            store.entries().len(),
            2,
            "reserve below capacity must not scan and purge unrelated entries"
        );
        assert_eq!(store.local_entries().await, Some(1));
    }

    #[tokio::test]
    async fn redis_store_uses_nx_px_owner_release_and_force_delete() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let observed = Arc::new(Mutex::new(Vec::<Vec<String>>::new()));
        let observed_server = Arc::clone(&observed);
        let server = tokio::spawn(async move {
            let values = Arc::new(Mutex::new(HashMap::<String, String>::new()));
            let (socket, _) = listener.accept().await.unwrap();
            serve_fake_redis(socket, observed_server, values).await;
        });
        let store = RedisWebhookReplayStore::new(
            &format!("redis://{address}"),
            "light:hmac-replay:".to_string(),
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .unwrap();
        let key = key("redis-sensitive-delivery");
        let first = match store.reserve(&key, Duration::from_secs(60)).await.unwrap() {
            ReserveOutcome::Reserved(value) => value,
            ReserveOutcome::Duplicate => panic!("first reservation must win"),
        };
        assert_eq!(
            store.reserve(&key, Duration::from_secs(60)).await.unwrap(),
            ReserveOutcome::Duplicate
        );
        assert!(store.force_remove(&key).await.unwrap());
        let newer = match store.reserve(&key, Duration::from_secs(60)).await.unwrap() {
            ReserveOutcome::Reserved(value) => value,
            ReserveOutcome::Duplicate => panic!("reservation after force removal must win"),
        };
        store.release(&first).await.unwrap();
        assert_eq!(
            store.reserve(&key, Duration::from_secs(60)).await.unwrap(),
            ReserveOutcome::Duplicate
        );
        store.release(&newer).await.unwrap();
        assert!(!store.force_remove(&key).await.unwrap());
        drop(store);
        server.await.unwrap();

        let commands = observed.lock().unwrap();
        let text = format!("{commands:?}");
        assert!(commands.iter().any(|command| {
            command.first().is_some_and(|value| value == "SET")
                && command.iter().any(|value| value == "NX")
                && command.iter().any(|value| value == "PX")
        }));
        assert!(
            commands
                .iter()
                .any(|command| command.first().is_some_and(|value| value == "EVAL"))
        );
        assert!(
            commands
                .iter()
                .any(|command| command.first().is_some_and(|value| value == "DEL"))
        );
        assert!(!text.contains("redis-sensitive-delivery"));
        assert!(text.contains(&format!("light:hmac-replay:{}", key.digest())));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires HMAC_PHASE4_REDIS_URL"]
    async fn redis_store_is_atomic_across_independent_provider_connections() {
        let url = std::env::var("HMAC_PHASE4_REDIS_URL")
            .expect("HMAC_PHASE4_REDIS_URL must identify the qualification Redis instance");
        let prefix = format!("light:hmac-phase4:{}:", Uuid::new_v4());
        let first = Arc::new(
            RedisWebhookReplayStore::new(
                &url,
                prefix.clone(),
                Duration::from_secs(2),
                Duration::from_secs(2),
            )
            .expect("build first gateway replay provider"),
        );
        let second = Arc::new(
            RedisWebhookReplayStore::new(
                &url,
                prefix,
                Duration::from_secs(2),
                Duration::from_secs(2),
            )
            .expect("build second gateway replay provider"),
        );
        let replay_key = key("concurrent-gateway-delivery");
        let attempts = (0..128)
            .map(|index| {
                let store: Arc<dyn WebhookReplayStore> = if index % 2 == 0 {
                    first.clone()
                } else {
                    second.clone()
                };
                let replay_key = replay_key.clone();
                tokio::spawn(
                    async move { store.reserve(&replay_key, Duration::from_secs(60)).await },
                )
            })
            .collect::<Vec<_>>();
        let mut winner = None;
        for attempt in attempts {
            match attempt
                .await
                .expect("reservation task")
                .expect("Redis reserve")
            {
                ReserveOutcome::Reserved(reservation) => {
                    assert!(
                        winner.replace(reservation).is_none(),
                        "only one reserve may win"
                    )
                }
                ReserveOutcome::Duplicate => {}
            }
        }
        let winner = winner.expect("one gateway instance must reserve the delivery");
        assert_eq!(
            second
                .reserve(&replay_key, Duration::from_secs(60))
                .await
                .expect("cross-instance duplicate check"),
            ReserveOutcome::Duplicate
        );

        first.release(&winner).await.expect("owner release");
        let newer = match second
            .reserve(&replay_key, Duration::from_secs(60))
            .await
            .expect("reserve after release")
        {
            ReserveOutcome::Reserved(reservation) => reservation,
            ReserveOutcome::Duplicate => panic!("released delivery must be reservable"),
        };
        first
            .release(&winner)
            .await
            .expect("stale owner release must be harmless");
        assert_eq!(
            first
                .reserve(&replay_key, Duration::from_secs(60))
                .await
                .expect("newer owner protection"),
            ReserveOutcome::Duplicate
        );
        second.release(&newer).await.expect("qualification cleanup");
    }

    async fn serve_fake_redis(
        socket: TcpStream,
        observed: Arc<Mutex<Vec<Vec<String>>>>,
        values: Arc<Mutex<HashMap<String, String>>>,
    ) {
        let mut socket = BufReader::new(socket);
        loop {
            let mut line = String::new();
            if socket.read_line(&mut line).await.unwrap() == 0 {
                return;
            }
            let Some(count) = line
                .strip_prefix('*')
                .and_then(|value| value.trim().parse::<usize>().ok())
            else {
                return;
            };
            let mut command = Vec::with_capacity(count);
            for _ in 0..count {
                line.clear();
                socket.read_line(&mut line).await.unwrap();
                let length = line[1..].trim().parse::<usize>().unwrap();
                let mut bytes = vec![0_u8; length + 2];
                socket.read_exact(&mut bytes).await.unwrap();
                command.push(String::from_utf8(bytes[..length].to_vec()).unwrap());
            }
            command[0] = command[0].to_ascii_uppercase();
            observed.lock().unwrap().push(command.clone());
            let response = match command[0].as_str() {
                "CLIENT" => "+OK\r\n".to_string(),
                "SET" => {
                    let key = command[1].clone();
                    let mut values = values.lock().unwrap();
                    if let std::collections::hash_map::Entry::Vacant(entry) = values.entry(key) {
                        entry.insert(command[2].clone());
                        "$2\r\nOK\r\n".to_string()
                    } else {
                        "$-1\r\n".to_string()
                    }
                }
                "EVAL" => {
                    let key = &command[3];
                    let owner = &command[4];
                    let mut values = values.lock().unwrap();
                    if values.get(key) == Some(owner) {
                        values.remove(key);
                        ":1\r\n".to_string()
                    } else {
                        ":0\r\n".to_string()
                    }
                }
                "DEL" => {
                    let mut values = values.lock().unwrap();
                    if values.remove(&command[1]).is_some() {
                        ":1\r\n".to_string()
                    } else {
                        ":0\r\n".to_string()
                    }
                }
                _ => "+OK\r\n".to_string(),
            };
            socket
                .get_mut()
                .write_all(response.as_bytes())
                .await
                .unwrap();
        }
    }
}
