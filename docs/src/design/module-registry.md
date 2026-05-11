# Module Registry

Status: Phase 4 implemented for `light-gateway/gateway`; additional module
reloaders remain planned.

## Purpose

Light Fabric needs a runtime module registry equivalent to the `ModuleRegistry`
feature in `light-4j`.

In `light-4j`, each active component registers its runtime configuration when
the component loads. Older integrations exposed this through the
`/adm/server/info` REST endpoint, but the current control-plane path uses MCP
tools through `portal-registry`. The same registry is also used by the
config-reload operation to decide which modules can reload configuration from
the config server.

Light Fabric already has structured config files and a shared runtime startup
flow, but it does not yet have a central registry that answers these operational
questions:

- which modules are active in this running instance
- which config file each module loaded
- what masked runtime config is currently active
- which modules can be reloaded without restarting the process
- what happened during the last reload attempt

This document proposes a registry in `light-runtime` so every Light Fabric
application can expose the same control-plane behavior.

## Goals

- Register built-in runtime configs such as `startup`, `server`, `client`, and
  `portal-registry`.
- Register application configs such as `gateway`, `deployer`, `ollama`, and
  `mcp-client`.
- Store only masked config snapshots in the registry.
- Expose a Java-compatible server-info payload through the
  `get_service_info` MCP tool.
- Expose a module list through the `get_modules` MCP tool for config reload
  selection.
- Support control-plane reload requests for one module, several modules, or all
  modules through the `reload_modules` MCP tool. Phase 3 reports
  non-reloadable modules as skipped. Phase 4 adds real hot reload for
  `light-gateway/gateway`.
- Keep the feature transport-neutral by routing management requests through
  `portal-registry`, not through framework-specific REST routes.

## Non-Goals

- Do not make every config hot-reloadable in the first phase.
- Do not rebind server ports or TLS listeners unless a transport explicitly
  supports it.
- Do not expose decrypted secrets through diagnostics.
- Do not make Rust type names part of the public control-plane contract.
- Do not add `/adm/...` REST endpoints for Light Fabric.

## Current Light Fabric Runtime Shape

The natural home for this feature is `crates/light-runtime`.

`LightRuntimeBuilder` already owns the startup sequence:

1. load local bootstrap config
2. optionally fetch remote config from config server
3. build `RuntimeConfig`
4. call registered runtime modules
5. bind the transport
6. register the running instance with the controller
7. mark the runtime ready

`RuntimeConfig` already carries the merged `resolved_values`, `config_dir`, and
`external_config_dir`. Application code can use those fields to load resolved
application config without reparsing `values.yml`.

The config registry should build on that runtime boundary instead of creating a
separate app-local registry per product.

## Registry Model

Add a shared registry type in `light-runtime`.

```rust
pub struct ModuleRegistry {
    entries: RwLock<BTreeMap<String, ModuleEntry>>,
    reloaders: RwLock<BTreeMap<String, Arc<dyn ReloadableModule>>>,
}

pub struct ModuleEntry {
    pub module_id: String,
    pub config_name: String,
    pub kind: ModuleKind,
    pub active: bool,
    pub enabled: Option<bool>,
    pub reloadable: bool,
    pub config: serde_json::Value,
    pub masks: Vec<MaskSpec>,
    pub loaded_at: DateTime<Utc>,
    pub last_reload: Option<ReloadStatus>,
}

pub enum ModuleKind {
    Core,
    Framework,
    Application,
    Plugin,
}
```

Use stable module IDs instead of Rust type names. Java uses class names because
they are stable operational identifiers in the JVM. Rust type names are not a
good public API and can change during refactoring.

Example module IDs:

- `light-runtime/startup`
- `light-runtime/server`
- `light-client/client`
- `light-runtime/portal-registry`
- `light-gateway/gateway`
- `light-deployer/deployer`
- `light-agent/ollama`
- `light-agent/mcp-client`

The registry key should be `module_id`. Each entry also carries `config_name`
so the server-info response can preserve the Java-style component map keyed by
config name.

## Registered Config Loading

Add a small registered-loader API around the existing `ConfigLoader` behavior.

```rust
let gateway_config: GatewayConfig = context
    .config()
    .load_registered(
        "gateway",
        "light-gateway/gateway",
        [MaskSpec::key("password")],
    )?;
```

The helper should:

1. merge the base file from `config_dir`
2. overlay the external file from `external_config_dir`
3. resolve variables from `RuntimeConfig.resolved_values`
4. deserialize the typed config
5. serialize the resolved config to `serde_json::Value`
6. apply masks to the serialized copy
7. store only the masked copy in `ModuleRegistry`
8. return the typed config to the caller

This keeps the app code simple and prevents accidental registry entries that
contain raw secrets.

Phase 2 added this shared registered-loader path in `ModuleRegistry` and
attached the registry to `RuntimeConfig` so apps that load after runtime
bootstrap can register resolved config through the same runtime-owned registry.
Apps that load before runtime startup can create the registry first, register
their application configs, and pass that registry into `LightRuntimeBuilder`.
For modules that must validate typed config before changing the registry
snapshot, the same loader is also available as `load_config(...)` followed by
`register_loaded_config(...)` after validation succeeds.

## Masking

Masking must happen at registration time. The registry should not store raw
config and then mask it later.

Support two mask forms:

```rust
pub enum MaskSpec {
    Key(String),
    Path(String),
}
```

`MaskSpec::Key("password")` masks every matching key recursively, matching the
current `light-4j` behavior.

`MaskSpec::Path("oauth.clientSecret")` masks a precise path for configs where a
generic key would be too broad.

Suggested default masks:

- `authorization`
- `password`
- `secret`
- `clientSecret`
- `apiKey`
- `token`
- `portalToken`
- `controllerDiscoveryToken`
- `privateKey`
- `tlsKeyPath`
- `bootstrapKeyPath`

Add a runtime flag such as `server.maskConfigProperties` or
`admin.maskConfigProperties`, defaulting to `true`, for parity with the Java
`server.maskConfigProperties` behavior. Even if this flag is disabled, the
control-plane documentation should treat unmasked output as a local debugging
mode only.

## Server Info MCP Response

The `get_service_info` MCP tool response should preserve the same logical shape
that portal-view already understands from Java instances.

```json
{
  "deployment": {
    "apiVersion": "0.1.0",
    "frameworkVersion": "0.1.0"
  },
  "environment": {
    "host": {
      "ip": "127.0.0.1",
      "hostname": "light-gateway-0"
    },
    "runtime": {},
    "system": {}
  },
  "security": {},
  "component": {
    "server": {},
    "gateway": {}
  },
  "plugin": {},
  "plugins": [],
  "modules": []
}
```

`component` should remain keyed by `config_name` for compatibility.

`modules` should provide richer Rust metadata:

```json
[
  {
    "moduleId": "light-gateway/gateway",
    "configName": "gateway",
    "kind": "application",
    "active": true,
    "enabled": true,
    "reloadable": true,
    "loadedAt": "2026-05-07T14:30:00Z",
    "lastReload": {
      "status": "success",
      "message": "reloaded from config server",
      "completedAt": "2026-05-07T14:45:00Z"
    }
  }
]
```

## MCP Access

Expose the registry only through MCP tools served by the runtime's
`portal-registry` connection.

MCP tools:

```text
get_service_info
get_modules
reload_modules
```

These are invoked through standard MCP JSON-RPC calls:

```json
{
  "jsonrpc": "2.0",
  "id": "info-1",
  "method": "tools/call",
  "params": {
    "name": "get_service_info",
    "arguments": {}
  }
}
```

The controller remains the management channel. `portal-registry` receives the
MCP request from the controller, dispatches it to the local runtime registry,
and returns the result through the same websocket session. Light Fabric should
not expose a parallel REST admin surface for this feature.

For compatibility with the existing Java and portal-view workflow,
`get_modules` returns a string list of module IDs:

```json
{
  "modules": [
    "light-runtime/server",
    "light-gateway/gateway"
  ]
}
```

The richer module metadata remains available in the `modules` field of
`get_service_info`.

## Reload Request

The `reload_modules` tool should accept omitted arguments, `ALL`, or explicit
module IDs.

```json
{
  "modules": [
    "light-gateway/gateway",
    "light-runtime/portal-registry"
  ]
}
```

An omitted `modules` value, an empty array, or `["ALL"]` targets all registered
modules. Registered modules without concrete reload implementations are
reported as skipped instead of being marked as reloaded.

The response should be explicit about what happened:

```json
{
  "modules": ["light-gateway/gateway"],
  "reloaded": ["light-gateway/gateway"],
  "skipped": [
    {
      "moduleId": "light-runtime/server",
      "reason": "requiresRestart"
    }
  ],
  "failed": [
    {
      "moduleId": "light-agent/ollama",
      "message": "missing ollama.yml"
    }
  ]
}
```

`modules` is a Java-compatible alias for the successfully reloaded module IDs
and is the field portal-view reads today. `reloaded`, `skipped`, and `failed`
carry the more explicit Rust result details.

## Reload Implementation

Phase 4 adds a reload trait for modules that can safely swap runtime config.

```rust
#[async_trait]
pub trait ReloadableModule: Send + Sync {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError>;
}
```

`ReloadContext` includes:

- a refreshed `RuntimeConfig`
- updated `resolved_values`
- the existing `config_dir`
- the existing `external_config_dir`
- the shared `ModuleRegistry`

Reload flow:

1. Re-fetch `values.yml`, certs, and files from the config server into
   `external_config_dir`.
2. Rebuild the merged `resolved_values`.
3. Resolve requested module IDs.
4. For each reloadable module, call its `reload` implementation.
5. Each module validates the new typed config before swapping it into live
   state.
6. Update the registry entry and `last_reload` status.
7. Return a detailed reload result.

Use `ConfigManager<T>` or another `ArcSwap`-backed holder for modules that need
hot reload. This avoids locking the request path while still allowing atomic
config replacement.

Phase 4 implements this with `ConfigManager<T>` in `light-runtime`. It stores an
`Arc<T>` behind a short-lived `RwLock`, so request handlers clone the current
config quickly and reloaders replace the entire typed config only after the new
config has loaded and validated.

## Reloadability Rules

Classify configs by reload safety.

Reloadable candidates:

- `light-gateway/gateway`
- `light-deployer/deployer`
- `light-agent/ollama`
- `light-agent/mcp-client`
- route, policy, provider, or rule configs that are already read through
  swappable state

Requires restart by default:

- bind IP
- HTTP/HTTPS port
- protocol enablement
- TLS certificate path used by the listener
- runtime config directory
- config-server bootstrap identity
- controller registration identity

Some `server.yml` fields can still be reloadable later, such as
`shutdownGracefulPeriod`, but listener-affecting fields should stay
`requiresRestart` until each transport supports safe rebinding.

## Framework Integration

The registry should not require each framework to expose admin routes.

`light-runtime` should attach an MCP-capable `RegistryHandler` to the
`portal-registry` client. When the controller invokes `tools/list` or
`tools/call`, the handler can advertise and execute the local management tools
without involving `light-axum` or `light-pingora` request routing.

This keeps `light-axum` and `light-pingora` focused on application traffic. It
also avoids adding service ports, Kubernetes routes, or Pingora request filters
only for control-plane operations.

## Application Integration

`light-gateway` is integrated first because it already loads `gateway.yml` from
`RuntimeConfig.resolved_values`, `config_dir`, and `external_config_dir`. It
loads the resolved typed config, validates upstreams, and then stores the
masked registry snapshot. In Phase 4, `light-gateway/gateway` also registers a
`ReloadableModule` that reloads and validates `gateway.yml`, updates the masked
registry snapshot, and swaps the live `GatewayConfig` through `ConfigManager`.

`light-deployer` loads `deployer.yml` before the runtime is started, so it
creates a `ModuleRegistry` before loading its config, registers the final
env-overridden deployer config, and passes the same registry to
`LightRuntimeBuilder`.

`light-agent` also loads application configs before runtime startup. It now
registers `ollama.yml` and `mcp-client.yml` in the pre-runtime registry and
passes that registry into `LightRuntimeBuilder`. The existing manual
`PortalRegistryClient` setup is unchanged so the registry feature does not
reintroduce duplicate controller registration.

## Current Registered Modules

Phase 4 registers these modules:

| Module ID | Config name | Kind | Reloadable |
| --- | --- | --- | --- |
| `light-runtime/startup` | `startup` | core | no |
| `light-runtime/server` | `server` | core | no |
| `light-client/client` | `client` | core | no |
| `light-runtime/portal-registry` | `portal-registry` | core | no |
| `light-gateway/gateway` | `gateway` | application | yes |
| `light-deployer/deployer` | `deployer` | application | no |
| `light-agent/ollama` | `ollama` | application | no |
| `light-agent/mcp-client` | `mcp-client` | application | no |

The application modules are visible in `get_service_info` once their owning
application loads them. `get_modules` returns the corresponding module ID
strings for portal-view selection. `light-gateway/gateway` can reload without a
restart. Other application modules keep `reloadable=false` until their runtime
state is moved behind swappable holders.

## Rollout Plan

### Phase 1: Registry and Masked Info

- Implemented: `ModuleRegistry`, `ModuleEntry`, and mask utilities in
  `light-runtime`.
- Implemented: built-in runtime config registration.
- Implemented: tests proving raw secrets are not stored in registry entries.
- Implemented: Java-compatible server-info response assembly.
- Implemented: module-list response.
- Implemented: a `portal-registry` MCP handler that exposes
  `get_service_info` and `get_modules`.

### Phase 2: Application Registration

- Implemented: convert `light-gateway/gateway` to registered config loading.
- Implemented: convert `light-deployer/deployer`.
- Implemented: convert `light-agent/ollama` and `light-agent/mcp-client`.
- Implemented: add docs showing module IDs and reloadability.

### Phase 3: Controller Operations

- Implemented: add MCP `tools/list` and `tools/call` support for
  `reload_modules`.
- Implemented: align portal-view calls so Java and Rust instances can be
  managed with the same control-plane workflow.
- Implemented: return Java-compatible `modules` string lists while preserving
  detailed `reloaded`, `skipped`, and `failed` reload result fields.

### Phase 4: Hot Reload

- Implemented: add `ReloadableModule`, `ReloadContext`, and `ReloadOutcome`.
- Implemented: add `ConfigManager<T>` for swappable typed configs.
- Implemented: implement reload for `light-gateway/gateway`.
- Implemented: add reload result tracking in the registry.
- Implemented: add tests for registry reload results, gateway live config
  swapping, and config-server-backed reload context refresh.

## Open Questions

- Should module IDs be centrally reserved in `light-runtime`, or should each
  application own its ID namespace?
- Should the Java-compatible `component` map include only active modules, while
  `modules` includes inactive-but-known modules?
- Should MCP tool execution be enabled whenever `portal-registry` is enabled,
  or guarded by a separate admin-tools flag?
- Should `server.maskConfigProperties=false` be allowed in production builds, or
  should Rust always mask known dangerous keys?

## Implementation Sequence

Phase 1 implemented registry and masked server info first, without hot reload.

Phase 2 added application registration, so portal-view can display Rust
application modules next to Java modules once it calls the MCP tools through
`portal-registry`.

Phase 3 added the controller-facing `reload_modules` tool and Java-compatible
module ID lists.

Phase 4 added the first real hot reload implementation for
`light-gateway/gateway`. The next implementation step is to move additional
application configs, such as `light-deployer/deployer`,
`light-agent/ollama`, and `light-agent/mcp-client`, behind swappable runtime
state before marking them reloadable.
