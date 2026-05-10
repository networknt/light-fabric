# WebSocket Router

## Status

Phases 1, 2, and 3 are implemented. Phase 1 added configuration parsing,
Java-compatible `pathPrefixService` normalization, route resolution, and
upstream URI cleanup in `light-pingora`. Phase 2 wired the `websocket` handler
into `light-gateway` with WebSocket upgrade detection, discovery-based upstream
selection, request context storage, and upstream header/query cleanup. Phase 3
added a real gateway-to-backend WebSocket integration test for text, binary,
close, subprotocol, and header behavior.

## Purpose

The Java `light-websocket-4j` `websocket-router` module routes WebSocket
traffic through a gateway or sidecar. A client connects to the gateway, the
router resolves the downstream service from headers, query parameters, or path
prefix configuration, and the gateway connects to the target WebSocket service.

In light-fabric this should be a `light-pingora` traffic handler activated by
`light-gateway` through `handler.yml`. The same `light-gateway` binary can link
the WebSocket router implementation, while each product decides whether it runs
by including the `websocket` handler and `websocket-router.yml` configuration
from config-server.

The Rust implementation should preserve the Java routing semantics and most of
the Java configuration shape, but it should not copy Java's `enabled` flag or
frame-bridging architecture. Pingora already supports HTTP/1 upgrade proxying,
so the first implementation should resolve the target and let Pingora tunnel
the upgraded connection.

## Goals

- Add a Java-compatible WebSocket router to `frameworks/light-pingora`.
- Activate the router with the existing `websocket` handler id in
  `apps/light-gateway`.
- Keep the Java `websocket-router` routing configuration recognizable:
  `defaultProtocol`, `defaultEnvTag`, and `pathPrefixService`.
- Allow `websocket-router.pathPrefixService` to be injected by config-server at
  startup the same way other handler-specific config is injected.
- Resolve downstream services from header, query parameter, or longest path
  prefix.
- Reuse the existing light-gateway discovery and upstream selection model.
- Preserve WebSocket handshake headers and pass normal agent/browser headers
  through to the downstream service.
- Register the router configuration with the module registry and support the
  same reload model as other light-pingora handler configs.
- Keep the design suitable for gateway, sidecar, and BFF deployments.

## Non-Goals

- Do not implement a separate WebSocket server framework in light-fabric.
- Do not terminate and re-create WebSocket frames in the first phase.
- Do not multiplex multiple client WebSocket sessions over one downstream
  connection.
- Do not support HTTP/2 extended CONNECT for WebSocket in the first phase.
- Do not use Rust dynamic plugins or `inventory` for WebSocket route
  registration.
- Do not create a separate gateway binary for WebSocket routing.
- Do not use `enabled` in `websocket-router.yml`. The handler is active when
  `handler.yml` includes `websocket` in the matched execution chain.

## Resolved Decisions

- Activation is controlled only by `handler.yml`. If a matched chain includes
  `websocket`, the router is enabled for that request.
- `websocket-router.yml` should not contain `enabled`.
- WebSocket-specific controls should cover both request/upgrade rate and active
  upgraded connection count.
- The first implementation should use Pingora HTTP/1 upgrade passthrough, not a
  frame-aware WebSocket bridge.
- Invalid `websocket-router.yml` configuration should fail startup. Invalid
  reloads should be rejected while the last valid runtime state keeps serving
  existing traffic.

## Java Behavior To Map

Java configuration includes `enabled`, but the Rust target config removes it:

```yaml
# Light websocket router configuration
defaultProtocol: ${websocket-router.defaultProtocol:http}
defaultEnvTag: ${websocket-router.defaultEnvTag:}
pathPrefixService: ${websocket-router.pathPrefixService:}
```

The Java `enabled` field is intentionally not carried forward. In Rust, the
handler chain is the activation contract. Removing `websocket` from a path or
default chain disables WebSocket routing for that path.

`pathPrefixService` accepts three forms:

```yaml
pathPrefixService:
  /chat:
    serviceId: com.networknt.llmchat-1.0.0
    protocol: http
    envTag: dev
```

```yaml
pathPrefixService:
  /chat: com.networknt.llmchat-1.0.0
```

```yaml
pathPrefixService: {"/chat":{"serviceId":"com.networknt.llmchat-1.0.0","protocol":"http","envTag":"dev"}}
```

The Java handler resolves the downstream service in this order:

1. Header: first non-blank value from `Service-Id`, `service_id`, or
   `serviceId`.
2. Query parameter: first non-blank value from `service_id` or `serviceId`.
3. Path prefix: `pathPrefixService` match against the request path.

If a target is found, query parameters can override the target protocol and
environment tag:

- `protocol`
- `env_tag`
- `envTag`

The Java handler removes router-only query parameters before connecting to the
downstream service:

- `protocol`
- `service_id`
- `serviceId`
- `env_tag`
- `envTag`

The Java implementation accepts client WebSocket subprotocols, opens a new JDK
WebSocket client connection to the downstream service, forwards `Authorization`,
forwards the selected subprotocols, and then bridges text and binary frames in
both directions.

## Rust Architecture

Add the WebSocket router to `light-pingora` because it is a Pingora gateway
traffic handler.

Proposed module:

```text
frameworks/light-pingora/src/websocket.rs
```

Primary types:

```rust
pub struct WebSocketRouterConfig {
    pub default_protocol: String,
    pub default_env_tag: Option<String>,
    pub path_prefix_service: BTreeMap<String, WebSocketServiceTarget>,
}

pub struct WebSocketServiceTarget {
    pub service_id: String,
    pub protocol: String,
    pub env_tag: Option<String>,
}

pub struct WebSocketRouteDecision {
    pub service_id: String,
    pub protocol: String,
    pub env_tag: Option<String>,
    pub upstream_path_and_query: String,
}
```

The serde layer should accept Java field names through aliases:

- `defaultProtocol`
- `defaultEnvTag`
- `pathPrefixService`
- `serviceId`
- `envTag`

Use `websocket-router.yml` as the preferred Rust file name. Accept
`websocket-router.yaml` as a compatibility fallback.

### Config Normalization

Normalize `pathPrefixService` at load time:

```text
raw config
  -> validate defaultProtocol/defaultEnvTag
  -> parse pathPrefixService YAML map, JSON string map, or legacy key/value string
  -> apply defaults to entries missing protocol or envTag
  -> sort prefixes by length for longest-prefix matching
  -> build Arc<WebSocketRouterState>
```

An invalid entry should fail config loading instead of being ignored silently.
This is stricter than Java and is safer for remote config delivered by
config-server.

### Handler Registration

`apps/light-gateway` already reserves the `websocket` handler id as a traffic
handler. The implementation should attach that id to the WebSocket router
runtime:

```yaml
handlers:
  - correlation
  - metrics
  - jwt
  - limit
  - websocket

paths:
  - path: /chat
    method: GET
    exec:
      - correlation
      - metrics
      - jwt
      - limit
      - websocket
```

The router should only run for chains that include `websocket`. This lets a BFF
serve static SPA assets, REST APIs, MCP, JSON-RPC, and WebSocket endpoints from
the same gateway binary with path-specific handler chains.

## Request Flow

The target flow should be:

```text
client request
  -> handler.yml path/chain match
  -> cross-cutting request handlers
  -> websocket handler
       -> verify WebSocket upgrade
       -> resolve service target
       -> strip router-only query parameters
       -> store WebSocketRouteDecision in request context
  -> Pingora upstream_peer selects discovered target
  -> Pingora upstream_request_filter preserves WebSocket handshake headers
  -> Pingora proxies the HTTP/1 upgraded stream
  -> response/metrics handlers observe completion
```

The router should not read the request body and should not buffer WebSocket
messages. Once the request is upgraded, Pingora owns the tunnel.

## Upgrade Detection

The handler should require the normal WebSocket handshake:

- method `GET`
- `Connection` contains `upgrade`
- `Upgrade` equals `websocket`
- `Sec-WebSocket-Key` exists
- HTTP version is compatible with HTTP/1 upgrade

If the `websocket` handler is selected by `handler.yml` but the request is not
a WebSocket upgrade, return `426 Upgrade Required`.

HTTP/2 extended CONNECT can be considered later, but should not block the first
implementation.

## Target Resolution

Target resolution should match Java precedence:

```text
1. service id header
2. service id query parameter
3. pathPrefixService longest-prefix match
```

Header names:

```text
Service-Id
service_id
serviceId
```

Query names:

```text
service_id
serviceId
protocol
env_tag
envTag
```

For path-prefix matches, use the request path without the query string. When
multiple prefixes match, choose the longest prefix.

The resolved `protocol` should be `http` or `https`. Conceptually this maps to
`ws` or `wss`, but Pingora should still connect to the upstream as HTTP or
HTTPS and then perform the WebSocket upgrade.

## Header And Query Policy

Because the Rust implementation should use Pingora upgrade passthrough, it
should preserve the original handshake headers:

- `Upgrade`
- `Connection`
- `Sec-WebSocket-Key`
- `Sec-WebSocket-Version`
- `Sec-WebSocket-Protocol`
- `Sec-WebSocket-Extensions`
- `Authorization`
- cookies
- normal agent/browser headers

The router should strip only router-control query parameters from the upstream
URI:

- `protocol`
- `service_id`
- `serviceId`
- `env_tag`
- `envTag`

The service-id routing headers should be removed before the upstream request by
default:

- `Service-Id`
- `service_id`
- `serviceId`

This keeps gateway routing controls separate from backend application headers.
If a backend later needs these headers, add an explicit config option rather
than leaking them by default.

## Discovery And Upstream Selection

The WebSocket router should reuse the same discovery/runtime model as
`router.yml` and the existing Pingora proxy flow.

Resolved target:

```text
protocol + serviceId + envTag
```

Discovery returns an upstream HTTP or HTTPS endpoint. `upstream_peer` creates
the Pingora peer:

- `http`: non-TLS upstream
- `https`: TLS upstream with normal SNI/hostname handling

For the first implementation, require HTTP/1.1 to the backend for WebSocket
upgrade. HTTP/2 WebSocket tunneling can be a later feature.

## Error Handling

Errors should be returned before the connection is upgraded:

| Condition | Response |
| --- | --- |
| Handler selected but request is not WebSocket upgrade | `426 Upgrade Required` |
| No service id and no path-prefix match | `403 Forbidden` |
| Invalid protocol override | `400 Bad Request` |
| Discovery has no usable endpoint | `502 Bad Gateway` |
| Upstream connect/upgrade failure | `502 Bad Gateway` |

Returning HTTP errors before upgrade is clearer than Java's close-frame behavior
because the Rust implementation does not accept the WebSocket until the target
is known.

## Module Registry And Reload

Register the loaded configuration with the module registry:

```text
module id: light-pingora/websocket-router
config name: websocket-router
config file: websocket-router.yml or websocket-router.yaml
```

On reload:

1. Load and validate the new config.
2. Build a new immutable route state.
3. Atomically swap the state.
4. Let in-flight upgraded connections continue with the old decision.

Existing WebSocket tunnels should not be interrupted by a config reload unless
the gateway process is restarted.

## Observability

The handler should integrate with existing correlation and metrics handlers:

- include correlation id in pre-upgrade logs
- record target resolution result
- record route source: `header`, `query`, or `pathPrefixService`
- count upgrade attempts, successful upgrades, rejected upgrades, and upstream
  connection failures
- optionally record tunnel duration once Pingora exposes completion

Do not log full query strings by default because they may contain application
data.

## Test Plan

Parser and resolver tests:

- YAML object `pathPrefixService`
- string service id entries
- JSON string map entries
- legacy key/value string entries
- default protocol and env tag application
- invalid entries fail load
- header beats query and path prefix
- query beats path prefix
- longest prefix wins
- query protocol/envTag override
- router query params are stripped

Gateway tests:

- non-upgrade request to a WebSocket chain returns `426`
- missing target returns `403`
- unknown discovery target returns `502`
- upgrade request preserves `Sec-WebSocket-Protocol`
- `Authorization` and normal browser/agent headers pass through
- service-id routing headers are stripped before upstream

Integration tests:

- connect through light-gateway to a local WebSocket echo backend
- text message round trip
- binary message round trip
- close frame behavior
- subprotocol negotiation
- TLS upstream smoke test when a local test certificate is available

## Implementation Phases

### Phase 1: Config And Resolver

Status: implemented.

- Add `frameworks/light-pingora/src/websocket.rs`.
- Parse `websocket-router.yml` and `websocket-router.yaml`.
- Normalize all Java-compatible `pathPrefixService` forms.
- Implement target resolution and upstream URI cleanup.
- Add unit tests.

### Phase 2: Gateway Handler Wiring

Status: implemented.

- Connect the existing `websocket` handler id to the router runtime.
- Detect WebSocket upgrade requests in the Pingora request flow.
- Store `WebSocketRouteDecision` in the request context.
- Select the discovered upstream in `upstream_peer`.
- Strip router query params and service-id headers in
  `upstream_request_filter`.

### Phase 3: WebSocket Integration Tests

Status: implemented.

- Add a local test WebSocket echo service.
- Verify text, binary, close, subprotocol, and header behavior through
  light-gateway.
- Verify HTTP and HTTPS upstream paths if practical in CI.

### Phase 4: Production Controls

- Add optional idle timeout and max connection duration.
- Add WebSocket-specific limit controls for both upgrade/request rate and
  active upgraded connection count.
- Add explicit config for preserving routing headers if a backend requires
  them.
- Add access-control integration once the same access-control model is shared
  across REST, JSON-RPC, MCP, and WebSocket routes.

## Open Questions

None.
