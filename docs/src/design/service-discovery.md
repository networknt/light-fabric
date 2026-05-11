# Service Discovery

## Status

Implemented baseline.

`light-runtime`, `portal-registry`, `light-pingora`, and `light-gateway`
already have the main pieces needed for controller-backed service discovery.
This document captures the supported invocation path, the configuration
contract, and the intended hardening direction for gateway, sidecar, BFF, MCP,
WebSocket, and token-handler deployments.

## Purpose

`light-gateway` should be able to discover downstream service instances from
the Light Controller through `portal-registry` instead of relying only on static
host lists in `router.yml`, `proxy.yml`, `mcp-router.yml`, or handler-specific
configuration.

The same mechanism should work with both controller implementations:

- Rust `controller-rs`
- Java `light-controller`

The gateway should use one `portal-registry` connection for registration,
runtime control-plane callbacks, and service discovery lookup. A separate
discovery client connection is not required for a registered runtime.

## Goals

- Reuse the existing `portal-registry` JSON-RPC WebSocket client.
- Keep service discovery available to all `light-pingora` handlers through
  `RuntimeConfig.registry_client`.
- Support controller-backed lookup for:
  - REST/router outbound calls
  - WebSocket routing
  - MCP tool routing
  - OAuth token-server resolution
  - SPA auth token-server resolution
- Keep direct URL configuration as an explicit override when a handler supports
  it.
- Keep static target configuration as a fallback where it already exists.
- Preserve Java-compatible discovery data names such as `serviceId`, `envTag`,
  `protocol`, `address`, and `port`.
- Let `light-portal` and config-server manage product-specific registry and
  handler configuration.
- Work with one `light-gateway` binary and different product config sets.

## Non-Goals

- Do not add a second discovery protocol for `light-gateway`.
- Do not require dynamic Rust plugins, `inventory`, or reflection for discovery.
- Do not make each handler own a separate controller connection.
- Do not require `/ws/discovery` for registered gateway instances.
- Do not remove static fallback configuration from router-style deployments.
- Do not make service discovery hide invalid product configuration. Startup
  validation and runtime errors should remain explicit.

## Controller Protocol

The controller exposes two WebSocket endpoints:

```text
/ws/microservice
/ws/discovery
```

`light-gateway` uses `/ws/microservice`.

The flow is:

```text
light-gateway
  -> connect /ws/microservice
  -> JSON-RPC service/register
  <- registered runtimeInstanceId
  -> JSON-RPC discovery/lookup on the same websocket
  <- DiscoverySnapshot
```

The dedicated `/ws/discovery` endpoint is still useful for non-service clients
that only need discovery. It is not needed by the gateway because both
`controller-rs` and `light-controller` accept discovery JSON-RPC methods on the
registered microservice socket after `service/register` succeeds.

The lookup request uses a `DiscoverySubscription` shape:

```json
{
  "serviceId": "com.networknt.petstore-1.0.0",
  "envTag": "dev",
  "protocol": "https"
}
```

`envTag` and `protocol` are optional. When `protocol` is omitted, the
controller can return all matching protocols and the caller decides which nodes
are usable.

The response is a `DiscoverySnapshot`:

```json
{
  "serviceId": "com.networknt.petstore-1.0.0",
  "envTag": "dev",
  "protocol": "https",
  "nodes": [
    {
      "runtimeInstanceId": "...",
      "serviceId": "com.networknt.petstore-1.0.0",
      "envTag": "dev",
      "environment": "dev",
      "version": "1.0.0",
      "protocol": "https",
      "address": "petstore",
      "port": 8443,
      "tags": {},
      "connectedAt": "...",
      "lastSeenAt": "...",
      "connected": true
    }
  ]
}
```

Only connected nodes with a non-zero port should be used as upstream targets.
Handlers should ignore protocols they cannot proxy.

## Runtime Configuration

Registry participation is controlled by `server.yml`:

```yaml
serviceId: ${server.serviceId:com.networknt.light-gateway-1.0.0}
advertisedAddress: ${server.advertisedAddress:127.0.0.1}
enableRegistry: ${server.enableRegistry:true}
startOnRegistryFailure: ${server.startOnRegistryFailure:true}
environment: ${server.environment:dev}
```

Controller connection settings come from `portal-registry.yml`:

```yaml
portalUrl: ${portalRegistry.portalUrl:https://localhost:8438}
portalToken: ${light_portal_authorization:}
controllerDiscoveryToken: ${portalRegistry.controllerDiscoveryToken:}
```

Current `light-gateway` discovery uses the microservice registration token from
`LIGHT_PORTAL_AUTHORIZATION` or `portalToken`. The token is sent in the
`service/register` payload. `controllerDiscoveryToken` is reserved for clients
that use the dedicated `/ws/discovery` endpoint and is not part of the current
gateway lookup path.

The runtime converts `portalUrl` to `/ws/microservice`, strips any query string,
and starts the shared `PortalRegistryClient` when registry is enabled. The
client must be connected and registered before discovery lookup can succeed.

## Gateway Invocation Path

Startup path:

```text
config-server/local config
  -> light-runtime loads server.yml, client.yml, portal-registry.yml
  -> RuntimeConfig.service_identity is built from server/bootstrap config
  -> RuntimeConfig.registry_client is created when registry is enabled
  -> runtime startup registers the gateway with controller
  -> light-gateway builds Pingora proxy state from RuntimeConfig
```

Request-time path:

```text
incoming request
  -> handler.yml selects a handler chain
  -> handler resolves direct target, serviceId, or static target
  -> handler calls PortalRegistryClient.lookup_discovery when serviceId discovery is needed
  -> controller returns DiscoverySnapshot
  -> handler converts nodes to Pingora ProxyTarget or base URL
  -> Pingora proxies the request
```

`PortalRegistryClient.lookup_discovery` sends JSON-RPC method
`discovery/lookup` over the registered websocket and waits for a response. If
the websocket is not connected, lookup fails with a registry client connection
error.

## Handler Usage

### Router

The `router` handler supports both direct routing and service discovery.

Resolution order:

1. `service_url` request routing, when configured and present.
2. `service_id` from query/header/path-prefix logic.
3. `direct-registry.directUrls` using `serviceId|envTag`, then `serviceId`.
4. Controller discovery with `serviceId` and optional `envTag`.
5. Legacy static `router.serviceTargets` fallback.

Direct registry is the standard static service map. `router.serviceTargets`
remains a deprecated compatibility fallback for old Rust gateway configs. New
product configs should not maintain both maps.

### WebSocket Router

The `websocket` handler resolves the target service from header, query, or
`pathPrefixService`. It checks `direct-registry.directUrls` first, then passes
`serviceId`, optional `envTag`, and protocol to discovery. Connected `http` and
`https` nodes are converted to upstream WebSocket targets and Pingora handles
the upgrade proxying.

### MCP Router

The `mcp` handler can route tools by direct `targetHost` or discovered
`serviceId`.

Resolution order:

1. Tool `targetHost`.
2. `direct-registry.directUrls` using `serviceId|envTag`, then `serviceId`.
3. Tool `serviceId` through controller discovery.

When a tool uses `serviceId`, portal registry is only required if no direct URL
mapping exists. The tool can also specify `envTag` and `protocol` to constrain
direct URL and discovery lookup.

### Token Handler

The `token` handler can resolve the OAuth token server by direct
`oauth.token.server_url` or by `oauth.token.serviceId`.

Resolution order:

1. Direct token server URL.
2. `direct-registry.directUrls` using token server `serviceId`.
3. Token server `serviceId` through controller discovery.

The selected node prefers `https` and then falls back to `http`. If discovery is
required and portal registry is not enabled, token injection fails explicitly.

### SPA Auth

The stateless SPA auth and MSAL exchange token clients use the same token-server
resolution model as the token handler:

1. Direct token server URL.
2. `direct-registry.directUrls` using token server `serviceId`.
3. Token server `serviceId` through controller discovery.

This keeps BFF deployments independent from fixed OAuth hostnames when the token
service is registered with the controller.

## Direct URLs And Fallbacks

Discovery should not override an explicit direct URL selected by a handler.
Direct URLs are operator intent and should remain authoritative. The standard
shared direct URL map is `direct-registry.directUrls`.

Static fallback is handler-specific:

- `direct-registry.directUrls` is checked before controller discovery.
- `router.serviceTargets` is deprecated and only remains as a legacy router
  fallback.
- MCP, token, SPA auth, JWK, and WebSocket service-id routing can use
  `direct-registry.directUrls` without per-handler duplicate maps.

This keeps failure behavior predictable. Product configs that require dynamic
discovery should fail requests loudly when the controller connection is down
instead of silently choosing an unrelated target.

## Load Balancing

The controller returns a list of matching nodes. The handler is responsible for
choosing one.

Current behavior is intentionally simple:

- drop disconnected nodes
- drop nodes with port `0`
- drop unsupported protocols
- prefer `https` for token-server resolution
- round-robin or index-based selection where the handler already has an index

Future hardening can add weighted selection, zone preference, health score,
least-connections, or sticky routing. Those policies should live in the handler
or a shared target-selection helper, not in the controller protocol.

## Failure Semantics

Startup behavior is controlled by `startOnRegistryFailure`:

- `true`: the gateway can start if initial controller registration times out;
  the registry client keeps retrying in the background.
- `false`: initial controller registration timeout fails startup.

Request-time behavior depends on handler fallback:

- with direct URL: discovery is bypassed
- with usable static fallback: handler may continue
- with discovery-only config: return an explicit gateway error

The runtime should continue reconnecting the registry websocket. Once the
client is registered again, new discovery lookups can succeed without restarting
the gateway.

## Security

The gateway registers through `/ws/microservice` with the portal registry token.
The controller validates the registration token and then allows discovery RPCs
on that registered socket.

Security expectations:

- Use TLS for controller connections outside local development.
- Keep hostname verification enabled outside local development.
- Prefer environment-provided token values over static config files.
- Mask `portalToken` and `controllerDiscoveryToken` in module-registry output.
- Do not pass registry tokens to downstream services.
- Do not trust discovery data from an untrusted controller.

Discovery returns transport endpoints. Authentication, authorization, rate
limit, CORS, header mutation, token injection, and access-control decisions
remain normal handler-chain responsibilities.

## Config Server Model

In production, `light-portal` owns product configuration and config-server
delivers resolved files at startup.

A product that needs controller-backed discovery should include:

- `server.yml` with `enableRegistry: true`
- `portal-registry.yml` with `portalUrl` and a valid portal token source
- `direct-registry.yml` or `values.yml` entries under
  `direct-registry.directUrls` for transition services that are not registered
  in the controller yet
- handler-specific config that uses `serviceId` instead of direct host URLs
- `handler.yml` chains that include the relevant handler IDs

For local Docker Compose, the Rust gateway must not keep the default
`https://localhost:8438` controller URL because `localhost` is the gateway
container. Use `portalRegistry.portalUrl: https://controller:8438`, pass
`LIGHT_PORTAL_AUTHORIZATION`, and keep static transition mappings in
`direct-registry.directUrls`.

The same binary can therefore run as:

- gateway
- sidecar
- proxy server
- proxy client
- balancer
- BFF

The product identity comes from config, not from a separate executable.

## Compatibility Notes

The current Rust and Java controllers are compatible with the gateway discovery
path because both support:

- `/ws/microservice`
- `service/register`
- discovery lookup on the registered microservice socket
- `serviceId`, `envTag`, and `protocol` filters
- `DiscoverySnapshot.nodes`
- connected-node metadata with `address`, `port`, and `protocol`

The gateway does not currently depend on `/ws/discovery`, although that endpoint
can remain available for external discovery clients.

## Future Work

- Add optional discovery subscriptions for handlers that benefit from a local
  in-memory discovery cache.
- Add shared target-selection policies for weighted, sticky, or zone-aware
  routing.
- Expose discovery health through the module registry or an admin endpoint.
- Add an integration test that starts a controller, registers a backend, starts
  light-gateway, and verifies an end-to-end proxied request through discovery.
- Decide whether `controllerDiscoveryToken` should be used by any standalone
  discovery-only client in light-fabric.
- Document operational examples for gateway, sidecar, WebSocket, MCP, token
  handler, and BFF product profiles.
