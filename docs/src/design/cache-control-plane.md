# Cache Control Plane

Status: Proposed

## Purpose

Light Fabric should expose the same cache operations through the portal control
plane that Java services expose through `light-4j` and `portal-registry`.

Today, portal-view can list caches and inspect cache entries for a running
service instance. The next required operation is clearing a cache so cached data
can be reloaded from its source of truth after operational data changes. A
common case is clearing the reference-data cache in `portal-service` after
reference tables are changed from `light-portal`.

The feature should be generic. It should not be a `portal-service` only endpoint.
Any Java or Rust service that registers with the controller and has named local
caches should be manageable through the same MCP tool contract.

## Current Shape

The Java implementation already has most of the control-plane pieces:

- `light-4j/cache-manager` defines the generic `CacheManager` API.
- `light-4j/caffeine-cache` provides the Caffeine-backed implementation.
- `light-4j/portal-registry` exposes MCP tools such as `list_caches` and
  `get_cache_entries`.
- `controller-rs` and the Java controller forward instance-specific MCP tool
  calls by `runtimeInstanceId`.
- `portal-view` calls the controller MCP websocket and passes
  `runtimeInstanceId` for cache exploration.

The main semantic gap is that `CacheManager.removeCache(name)` removes the cache
from the manager in the Caffeine implementation. For a control-plane clear
operation, the desired behavior is different: invalidate all entries while
keeping the configured cache alive so the next application read repopulates it.

## Goals

- Add a generic whole-cache clear operation.
- Keep the control-plane contract compatible between Java services and Light
  Fabric services.
- Expose cache operations through `portal-registry` and controller MCP routing,
  not through service-specific REST endpoints.
- Let portal-view clear a selected cache from the existing Cache Explorer page.
- Use the same feature for `portal-service` reference data caching.
- Preserve existing cache inspection behavior.

## Non-Goals

- Do not remove or unregister a configured cache when clearing entries.
- Do not require every service to use the same cache backend.
- Do not expose raw secrets or unsafe object internals through cache inspection.
- Do not build event-driven cross-service cache invalidation in the first phase.
- Do not confuse runtime data caches with the `config-cache` directory used for
  remote configuration files.

## MCP Tool Contract

Add a new generic tool:

```json
{
  "name": "clear_cache",
  "description": "Clear all entries from a named cache on a live runtime instance.",
  "inputSchema": {
    "type": "object",
    "required": ["runtimeInstanceId", "name"],
    "properties": {
      "runtimeInstanceId": { "type": "string", "format": "uuid" },
      "name": { "type": "string" }
    }
  }
}
```

The controller accepts `runtimeInstanceId`, removes it from the forwarded
arguments, and sends this to the target runtime:

```json
{
  "name": "clear_cache",
  "arguments": {
    "name": "reference-data"
  }
}
```

Recommended success response:

```json
{
  "supported": true,
  "status": "success",
  "name": "reference-data",
  "beforeSize": 42,
  "afterSize": 0
}
```

Recommended unsupported response:

```json
{
  "supported": false,
  "status": "unsupported",
  "name": "reference-data",
  "message": "Cache support is not available on this service."
}
```

Key-level invalidation can be added later as a separate
`invalidate_cache_entry` tool with `{ "name": "...", "key": "..." }`.
Whole-cache clear should be implemented first because it solves the reference
data reload case without introducing cache-key UX and serialization questions.

## Java Compatibility Work

In `light-4j`, add an explicit clear operation to the generic cache API:

```java
void clear(String cacheName);
```

The Caffeine implementation should call `cache.invalidateAll()` and keep the
cache in the manager. It may call `cache.cleanUp()` before returning size data.
`removeCache(name)` should keep its existing unregister/remove semantics.

`portal-registry` should advertise `clear_cache` in `tools/list` and handle it
in `tools/call` by using `CacheManager.getInstance()`. The handler should
return `supported: false` when cache classes or a cache manager are not
available, matching the current `list_caches` and `get_cache_entries` behavior.

The controller catalogs need the same tool so portal-view can call it through
the normal controller websocket:

- `controller-rs` tool catalog and command serialization
- Java `light-controller` tool catalog and routed-call handling, if it remains a
  supported control-plane runtime

## Light Fabric Runtime Design

Light Fabric should provide a small cache abstraction at the runtime layer so
applications do not each define a different operational surface.

A practical shape is:

```rust
#[async_trait::async_trait]
pub trait RuntimeCache: Send + Sync {
    async fn len(&self) -> usize;
    async fn entries_summary(&self) -> serde_json::Value;
    async fn clear(&self);
}

#[derive(Default)]
pub struct CacheRegistry {
    caches: RwLock<BTreeMap<String, Arc<dyn RuntimeCache>>>,
}
```

The registry should support:

- register named cache
- list cache names
- get summarized entries
- clear a named cache

`moka` is the preferred default backend for async Rust services because it maps
well to the Caffeine use case. Applications should still be free to register
custom cache wrappers as long as they implement the runtime trait.

`RuntimeMcpHandler` in `light-runtime` should expose the same tools as Java:

- `list_caches`
- `get_cache_entries`
- `clear_cache`

If a runtime has no cache registry, these tools should return `supported:
false` rather than failing the request.

## Portal Service Reference Data Cache

`portal-service` can use the generic Light Fabric cache for `/r/data`.

Suggested cache names:

- `reference-data`
- `reference-data-relation`

Suggested keys:

- `host:{hostId|global}:lang:{lang}:table:{name}`
- `host:{hostId|global}:lang:{lang}:table:{name}:rela:{rela}:from:{from}`

The request flow becomes:

1. `/r/data` receives a reference-data request.
2. `ReferenceService` builds a stable cache key from host, language, table,
   relation, and source value.
3. On cache hit, return cached reference data.
4. On cache miss, query Postgres, cache the result, and return it.
5. When reference data changes in `light-portal`, an operator clears
   `reference-data` or `reference-data-relation` for the target
   `portal-service` runtime instance from portal-view.
6. The next `/r/data` call reloads from Postgres.

This keeps the first implementation manual and deterministic. A later phase can
subscribe to reference-table change events and clear matching caches
automatically.

## Portal View UX

The existing Cache Explorer page should stay the main UI.

Add a clear action for the selected cache:

- show the selected cache name
- require confirmation before clearing
- disable the button while the request is running
- call `clear_cache` with `{ runtimeInstanceId, name }`
- show success or error status
- refetch cache entries after a successful clear

The UI should not require users to know whether the target service is Java or
Rust. Unsupported runtimes should show the returned unsupported message.

## Implementation Phases

### Phase 1: Java clear support

- Add `CacheManager.clear(cacheName)`.
- Implement it in `caffeine-cache`.
- Add `clear_cache` to `portal-registry` MCP tools.
- Add targeted tests for clearing while preserving the configured cache.

### Phase 2: Controller and portal-view

- Add `clear_cache` to controller tool catalogs and command routing.
- Add the Cache Explorer clear button and confirmation.
- Verify the existing `runtimeInstanceId` forwarding path is reused.

### Phase 3: Light Fabric generic cache

- Add a runtime cache registry and trait.
- Add `moka` backed cache support.
- Expose `list_caches`, `get_cache_entries`, and `clear_cache` from
  `RuntimeMcpHandler`.
- Add focused `light-runtime` tests for supported and unsupported cache cases.

### Phase 4: Portal service reference data

- Register `reference-data` and `reference-data-relation` caches.
- Cache `/r/data` query results.
- Clear the cache from portal-view and verify the next request reloads from
  Postgres.

## Verification

Recommended targeted checks:

```bash
mvn -q -pl cache-manager,caffeine-cache,portal-registry test
cargo test -p light-runtime
cargo check --workspace
yarn build
```

Use the Maven command in `light-4j`, the Cargo commands in `light-fabric` and
`portal-service` as appropriate, and the frontend build in `portal-view`.
