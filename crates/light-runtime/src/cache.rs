use async_trait::async_trait;
use moka::future::Cache as MokaCache;
use serde::Serialize;
use serde_json::{Map as JsonMap, Value as JsonValue, json};
use std::collections::BTreeMap;
use std::hash::Hash;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

#[async_trait]
pub trait RuntimeCache: Send + Sync {
    async fn len(&self) -> usize;
    async fn entries_summary(&self) -> JsonValue;
    async fn clear(&self);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClearCacheOutcome {
    pub before_size: usize,
    pub after_size: usize,
}

#[derive(Default)]
pub struct CacheRegistry {
    caches: RwLock<BTreeMap<String, Arc<dyn RuntimeCache>>>,
}

impl CacheRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<C>(&self, name: impl Into<String>, cache: C) -> Option<Arc<dyn RuntimeCache>>
    where
        C: RuntimeCache + 'static,
    {
        let cache: Arc<dyn RuntimeCache> = Arc::new(cache);
        self.register_arc(name, cache)
    }

    pub fn register_arc(
        &self,
        name: impl Into<String>,
        cache: Arc<dyn RuntimeCache>,
    ) -> Option<Arc<dyn RuntimeCache>> {
        self.caches_write().insert(name.into(), cache)
    }

    pub fn unregister(&self, name: &str) -> Option<Arc<dyn RuntimeCache>> {
        self.caches_write().remove(name)
    }

    pub fn names(&self) -> Vec<String> {
        self.caches_read().keys().cloned().collect()
    }

    pub fn cache(&self, name: &str) -> Option<Arc<dyn RuntimeCache>> {
        self.caches_read().get(name).cloned()
    }

    pub async fn len(&self, name: &str) -> Option<usize> {
        let cache = self.cache(name)?;
        Some(cache.len().await)
    }

    pub async fn entries_summary(&self, name: &str) -> Option<JsonValue> {
        let cache = self.cache(name)?;
        Some(cache.entries_summary().await)
    }

    pub async fn clear(&self, name: &str) -> Option<ClearCacheOutcome> {
        let cache = self.cache(name)?;
        let before_size = cache.len().await;
        cache.clear().await;
        let after_size = cache.len().await;
        Some(ClearCacheOutcome {
            before_size,
            after_size,
        })
    }

    fn caches_read(&self) -> RwLockReadGuard<'_, BTreeMap<String, Arc<dyn RuntimeCache>>> {
        self.caches.read().unwrap_or_else(|err| err.into_inner())
    }

    fn caches_write(&self) -> RwLockWriteGuard<'_, BTreeMap<String, Arc<dyn RuntimeCache>>> {
        self.caches.write().unwrap_or_else(|err| err.into_inner())
    }
}

pub struct MokaRuntimeCache<K, V> {
    cache: MokaCache<K, V>,
}

impl<K, V> MokaRuntimeCache<K, V>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    pub fn new(max_capacity: u64) -> Self {
        Self {
            cache: MokaCache::new(max_capacity),
        }
    }

    pub fn from_cache(cache: MokaCache<K, V>) -> Self {
        Self { cache }
    }

    pub fn inner(&self) -> &MokaCache<K, V> {
        &self.cache
    }

    pub async fn insert(&self, key: K, value: V) {
        self.cache.insert(key, value).await;
    }

    pub async fn get(&self, key: &K) -> Option<V> {
        self.cache.get(key).await
    }

    pub async fn invalidate(&self, key: &K) {
        self.cache.invalidate(key).await;
    }
}

#[async_trait]
impl<K, V> RuntimeCache for MokaRuntimeCache<K, V>
where
    K: Eq + Hash + Clone + Serialize + Send + Sync + 'static,
    V: Clone + Serialize + Send + Sync + 'static,
{
    async fn len(&self) -> usize {
        self.cache.run_pending_tasks().await;
        self.cache.entry_count() as usize
    }

    async fn entries_summary(&self) -> JsonValue {
        self.cache.run_pending_tasks().await;
        let mut entries = JsonMap::new();
        for (key, value) in self.cache.iter() {
            entries.insert(
                cache_key_to_string(key.as_ref()),
                cache_value_to_json(value),
            );
        }
        JsonValue::Object(entries)
    }

    async fn clear(&self) {
        self.cache.invalidate_all();
        self.cache.run_pending_tasks().await;
    }
}

fn cache_key_to_string<K>(key: &K) -> String
where
    K: Serialize,
{
    match serde_json::to_value(key) {
        Ok(JsonValue::String(value)) => value,
        Ok(value) => value.to_string(),
        Err(_) => "<unserializable-key>".to_string(),
    }
}

fn cache_value_to_json<V>(value: V) -> JsonValue
where
    V: Serialize,
{
    serde_json::to_value(value)
        .unwrap_or_else(|error| json!({ "serializationError": error.to_string() }))
}
