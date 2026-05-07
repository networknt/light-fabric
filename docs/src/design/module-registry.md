# Module Registry

Status: Phase 1 implemented; application registration and hot reload remain
planned.

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
- Support reloading one module, several modules, or all reloadable modules
  through the `reload_modules` MCP tool.
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
- `light-runtime/client`
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

An empty `modules` array or `["ALL"]` means all reloadable modules.

The response should be explicit about what happened:

```json
{
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

## Reload Implementation

Add a reload trait for modules that can safely swap runtime config.

```rust
#[async_trait]
pub trait ReloadableModule: Send + Sync {
    async fn reload(&self, ctx: ReloadContext) -> Result<ReloadOutcome, RuntimeError>;
}
```

`ReloadContext` should include:

- current `RuntimeConfig`
- updated `resolved_values`
- `config_dir`
- `external_config_dir`
- registered-loader helper
- optional config-server fetch result

Reload flow:

1. Re-fetch `values.yml`, certs, and files from config server into
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

`light-gateway` should be the first application integration because it already
loads `gateway.yml` from `RuntimeConfig.resolved_values`,
`config_dir`, and `external_config_dir`.

`light-deployer` currently loads `deployer.yml` before the runtime is started.
To register and reload it cleanly, move the deployer config load behind the
runtime context or introduce a pre-runtime registration path that is later
attached to `RuntimeConfig`.

`light-agent` also loads application configs before runtime startup. It should
eventually use the registered-loader path for `ollama.yml` and
`mcp-client.yml`. The existing manual `PortalRegistryClient` setup must stay in
mind so the registry feature does not reintroduce duplicate controller
registration.

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

- Convert `light-gateway/gateway` to registered config loading.
- Convert `light-deployer/deployer`.
- Convert `light-agent/ollama` and `light-agent/mcp-client`.
- Add docs showing module IDs and reloadability.

### Phase 3: Controller Operations

- Add MCP `tools/list` and `tools/call` support for `reload_modules`.
- Align portal-view calls so Java and Rust instances can be managed with the
  same control-plane workflow.

### Phase 4: Hot Reload

- Add `ReloadableModule`.
- Add atomic config holders for reloadable app configs.
- Implement reload for `light-gateway/gateway`.
- Add reload result tracking in the registry.
- Add integration tests with a temp config server or mocked remote bootstrap.

## Open Questions

- Should module IDs be centrally reserved in `light-runtime`, or should each
  application own its ID namespace?
- Should the Java-compatible `component` map include only active modules, while
  `modules` includes inactive-but-known modules?
- Should MCP tool execution be enabled whenever `portal-registry` is enabled,
  or guarded by a separate admin-tools flag?
- Should `reload_modules` use module IDs only, or accept legacy Java-style class
  names for control-plane compatibility during migration?
- Should `server.maskConfigProperties=false` be allowed in production builds, or
  should Rust always mask known dangerous keys?

## Recommended First Cut

Implement registry and masked server info first, without hot reload.

That delivers immediate control-plane value and gives every Light Fabric
instance a trustworthy runtime inventory. Once portal-view can display Rust
modules next to Java modules, add reload one module at a time, starting with
`light-gateway/gateway` because its current config loading path is already close
to the target design.
