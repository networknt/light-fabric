# Kubernetes Gateway API Design

## Status

Proposal.

This page captures how the current `light-gateway` work can be reused for
Kubernetes Gateway API without turning the microgateway product into a
catch-all Kubernetes control plane. The recommended direction is a separate
`light-k8s-gateway` product built on `light-pingora` for north/south ingress,
with a later sidecar or mesh product for transparent east/west traffic.

## Context

The current Kubernetes deployment model runs `light-gateway` as a normal
`Deployment` with a `ClusterIP` `Service`. Runtime behavior comes from local
bootstrap config, config-server downloaded files in `config-cache`, and the
Pingora data plane built by `light-pingora`.

The current gateway already has useful data-plane pieces:

- HTTP and HTTPS proxying through Pingora.
- Static upstreams from `proxy.yml`.
- Service-aware routing from `router.yml`.
- Direct registry, controller-backed discovery, and static service targets.
- Handler chains for security, header mutation, CORS, rate limits, token
  handling, MCP, WebSocket, static resources, and config reload.
- Live config managers and reloaders for route and handler modules.

Gateway API adds a Kubernetes-native control plane. For ingress, users create
`GatewayClass`, `Gateway`, and route resources such as `HTTPRoute`. For service
mesh, the GAMMA model attaches route resources directly to Kubernetes
`Service` objects instead of using `Gateway` and `GatewayClass`.

## Product Boundary

Keep the product line split by operational role:

- `light-pingora` is the shared data-plane framework.
- `light-gateway` remains the microgateway, sidecar, BFF, API, agent, MCP, and
  LLM gateway product configured through Light runtime, config-server,
  controller-rs, and local config.
- `light-k8s-gateway` is the proposed Kubernetes Gateway API product for
  north/south ingress. It should reuse `light-pingora` and lift reusable
  `light-gateway` modules where appropriate, but it should own Kubernetes
  watches, Gateway API status, RBAC, listener translation, TLS Secret handling,
  and EndpointSlice routing.
- `light-k8s-gateway-controller` and `light-k8s-gateway-proxy` should be
  separate deployments from the first implementation. The controller owns
  Kubernetes RBAC and status writes. The proxy owns untrusted client traffic
  and should not need Kubernetes API permissions.
- A future `light-mesh` or `light-sidecar` product should own transparent
  east/west Service Mesh behavior if we pursue GAMMA conformance. It should
  share the Gateway API route compiler and `light-pingora` data-plane modules,
  but its deployment model is sidecar or node-local interception, not ingress.

This avoids giving ordinary microgateway deployments broad Kubernetes RBAC and
keeps config-server/controller-rs routing separate from portable Gateway API
routing intent.

## Goals

- Let operators install `light-k8s-gateway` as a Gateway API implementation
  with a controller name such as `networknt.com/light-k8s-gateway`.
- Support north/south ingress with `GatewayClass`, `Gateway`, `HTTPRoute`,
  Kubernetes `Service`, `EndpointSlice`, `Secret`, and `ReferenceGrant`.
- Separate Kubernetes reconciliation from request proxying so control-plane RBAC
  is never granted to the public traffic data plane.
- Provide a migration path from NGINX or Traefik by running side by side with a
  distinct GatewayClass, then moving routes class by class or host by host.
- Reuse the existing Pingora proxy, handler chain, service discovery, metrics,
  and config reload model instead of creating a separate proxy stack.
- Use Gateway API policy attachment for Light-specific Kubernetes policy CRDs
  instead of annotations or out-of-band route policy.
- Support east/west traffic using Gateway API mesh semantics where
  `HTTPRoute.parentRefs` can point at a `Service`.
- Keep Light-specific policies available without forcing them into portable
  Gateway API fields. Gateway API should configure routing; Light config and
  future policy CRDs should configure Light-specific behavior.
- Build toward Gateway API conformance tests for both Gateway and Mesh
  feature sets.

## Non-Goals

- Do not remove existing config-server, direct registry, portal registry, or
  static route support.
- Do not require every `light-gateway` deployment to watch Kubernetes. Gateway
  API support should be disabled unless explicitly configured.
- Do not run the Kubernetes controller reconciler inside public data-plane pods
  with broad Kubernetes RBAC.
- Do not claim immediate support for every Gateway API route type. Start with
  `HTTPRoute`; add `GRPCRoute`, `TLSRoute`, `TCPRoute`, and `UDPRoute` in later
  milestones.
- Do not make transparent east/west interception a hidden side effect of the
  ingress deployment. Mesh mode needs an explicit data-plane deployment model.
- Do not treat a non-transparent egress gateway as fully GAMMA-compliant mesh
  support.

## Target API Versions

The north/south MVP targets the Gateway API `v1`
[Standard Channel](https://gateway-api.sigs.k8s.io/concepts/versioning/)
resources:

- `GatewayClass`
- `Gateway`
- `HTTPRoute`
- `ReferenceGrant`

Experimental or later milestones must be labeled explicitly in docs, manifests,
and conformance reports. This includes GAMMA mesh behavior and route kinds such
as `GRPCRoute`, `TLSRoute`, `TCPRoute`, and `UDPRoute` when those features rely
on non-Standard channels in the installed Gateway API version.

## North/South Ingress Model

For ingress replacement, `light-k8s-gateway` should run as two cooperating
pieces:

- `light-k8s-gateway-controller`: watches Kubernetes resources, validates
  attachment and policy, updates status, performs leader election, and produces
  a compiled routing snapshot.
- `light-k8s-gateway-proxy`: consumes signed or mTLS-protected snapshots and
  serves client traffic through Pingora. It has no Kubernetes watch or status
  permissions and can scale independently with an HPA.

The split is mandatory from day 1. It prevents a proxy vulnerability in the
public data plane from becoming a Kubernetes control-plane compromise. The
controller can run as an HA deployment with Kubernetes `Lease` leader election;
only the leader reconciles resources and writes status. Non-leader controller
replicas stay warm and can take over quickly.

Snapshot delivery can start as a lightweight internal gRPC stream and evolve
toward an xDS-like API if we need richer incremental updates. The proxy should
apply the received `GatewayApiSnapshot` through the same kind of `ConfigManager`
swap used by the current Pingora modules.

Typical installation:

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: GatewayClass
metadata:
  name: light-k8s-gateway
spec:
  controllerName: networknt.com/light-k8s-gateway
```

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata:
  name: public
  namespace: gateway-system
spec:
  gatewayClassName: light-k8s-gateway
  listeners:
    - name: http
      protocol: HTTP
      port: 80
      allowedRoutes:
        namespaces:
          from: All
    - name: https
      protocol: HTTPS
      port: 443
      hostname: api.example.com
      tls:
        mode: Terminate
        certificateRefs:
          - kind: Secret
            name: api-example-com
```

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata:
  name: petstore
  namespace: apps
spec:
  parentRefs:
    - name: public
      namespace: gateway-system
      sectionName: https
  hostnames:
    - api.example.com
  rules:
    - matches:
        - path:
            type: PathPrefix
            value: /pets
      backendRefs:
        - name: petstore
          port: 8080
```

The controller resolves this into a runtime route table:

```text
Gateway listener
  -> accepted HTTPRoutes
  -> host/path/header/method/query matches
  -> filters supported by light-k8s-gateway
  -> backend Service
  -> EndpointSlice addresses
  -> Pingora ProxyTarget set
```

The existing `proxy.yml` and `router.yml` paths remain useful for legacy and
non-Kubernetes deployments. Kubernetes Gateway API routes should not depend on
`service_id` headers or `pathPrefixService.yml`; they should route from the
compiled Gateway API table directly to Kubernetes endpoints.

## Required Ingress Patches

Add a Kubernetes Gateway API module:

```yaml
k8sGatewayApi:
  enabled: ${k8sGatewayApi.enabled:false}
  mode: ${k8sGatewayApi.mode:ingress}
  controllerName: ${k8sGatewayApi.controllerName:networknt.com/light-k8s-gateway}
  gatewayClassName: ${k8sGatewayApi.gatewayClassName:light-k8s-gateway}
  watchNamespaces: ${k8sGatewayApi.watchNamespaces:[]}
  statusAddress: ${k8sGatewayApi.statusAddress:}
```

Implementation changes:

- Create `apps/light-k8s-gateway-controller` and
  `apps/light-k8s-gateway-proxy`.
- Add Gateway API and Kubernetes clients, likely behind a Cargo feature such
  as `k8s-gateway-api`, using `kube`, `kube-runtime`, `k8s-openapi`, and
  generated Gateway API resource types.
- Watch `GatewayClass`, `Gateway`, `HTTPRoute`, `ReferenceGrant`, `Service`,
  `EndpointSlice`, `Secret`, and `Namespace`.
- Compile watched objects into a deterministic `GatewayApiSnapshot`.
- Push the compiled snapshot to proxy pods over an authenticated internal
  channel.
- Store the received snapshot in a proxy-side `ConfigManager`, similar to the
  current proxy and router reload model.
- Add a `light-pingora` Gateway API route-table module that can select a
  backend before falling back to existing proxy/router behavior.
- Update Kubernetes status conditions for `GatewayClass`, `Gateway`, listeners,
  and routes. Status must clearly report unsupported route types, listener
  conflicts, missing TLS secrets, rejected cross-namespace references, empty
  backends, and unsupported filters.
- Add Kubernetes `Lease` leader election so only one controller replica writes
  status and publishes snapshots.
- Add controller RBAC for read watches, Secret reads where allowed, Lease
  writes, and status updates. Secret read permissions should be namespace-scoped
  where possible.
- Give proxy pods no Kubernetes RBAC by default.
- Add install manifests for separate controller and proxy `ServiceAccount`,
  `ClusterRole`, `ClusterRoleBinding`, `Deployment`, `Service`, and a sample
  `GatewayClass`.

The transport also needs a listener model. Today `PingoraTransport` binds the
single `server.httpPort` and single `server.httpsPort` from `server.yml`. That
is enough for the first `80`/`443` ingress path, but full Gateway API support
needs multiple listeners with independent protocol, port, hostname, and TLS
settings.

Suggested runtime patch:

```yaml
server:
  listeners:
    - name: http
      protocol: HTTP
      ip: 0.0.0.0
      port: 80
    - name: https-api
      protocol: HTTPS
      ip: 0.0.0.0
      port: 443
      hostname: api.example.com
      tlsCertPath: /var/run/light-k8s-gateway/tls/api/tls.crt
      tlsKeyPath: /var/run/light-k8s-gateway/tls/api/tls.key
```

Keep `server.httpPort`, `server.enableHttp`, `server.httpsPort`, and
`server.enableHttps` as backward-compatible shorthand.

## HTTPRoute Support Plan

Start with the common ingress subset:

- `GatewayClass` acceptance for `networknt.com/light-k8s-gateway`.
- `Gateway` listeners for `HTTP` and terminated `HTTPS`.
- `HTTPRoute` attachment by `parentRefs`, `sectionName`, listener hostname,
  listener namespace policy, and route hostname.
- `HTTPRoute` matches for path prefix, exact path, method, headers, and query
  parameters.
- `backendRefs` to Kubernetes `Service` backends, including weights.
- `ReferenceGrant` for cross-namespace backend references.
- Endpoint resolution from `EndpointSlice`, with Service DNS as a fallback only
  when endpoint watching is unavailable.
- TLS Secret loading for terminated HTTPS.
- Request header modification and URL rewrite where existing Pingora handlers
  already provide equivalent behavior.

Later milestones:

- Request redirect, response header modification, request mirroring, retries,
  and timeouts.
- `GRPCRoute` over HTTP/2.
- `TLSRoute` for SNI routing and passthrough.
- `TCPRoute` and `UDPRoute` for L4 ingress if Pingora transport support is
  added.
- Backend TLS policy and mTLS to upstream services.

## Light Policy Attachment

Kubernetes-native deployments should use the Gateway API
[Policy Attachment pattern from GEP-713](https://gateway-api.sigs.k8s.io/geps/gep-713/)
for Light-specific behavior. Do not use annotations for core behavior, and do
not require config-server-owned route policy for the Kubernetes Gateway API
path.

Add Light policy CRDs with `targetRefs` that point at Gateway API resources:

```yaml
apiVersion: gateway.lightapi.net/v1alpha1
kind: LightAuthPolicy
metadata:
  name: petstore-auth
  namespace: apps
spec:
  targetRefs:
    - group: gateway.networking.k8s.io
      kind: HTTPRoute
      name: petstore
  jwt:
    issuer: https://issuer.example.com
    audience:
      - petstore
```

```yaml
apiVersion: gateway.lightapi.net/v1alpha1
kind: LightRateLimitPolicy
metadata:
  name: petstore-ratelimit
  namespace: apps
spec:
  targetRefs:
    - group: gateway.networking.k8s.io
      kind: HTTPRoute
      name: petstore
  limits:
    - name: default
      requests: 1000
      window: 60s
```

The controller should resolve effective policy for supported target kinds such
as `Gateway`, listener section, `HTTPRoute`, route rule, and eventually
`Service` for mesh. Policy status should report `Accepted`, `Programmed`, and
conflict conditions so resource owners can tell whether a policy is active.

Config-server remains valid for non-Kubernetes `light-gateway` deployments and
for migration bridges. For `light-k8s-gateway`, Kubernetes resources should be
the source of routing and policy intent.

## TLS Secret Handling

TLS Secret material must not be written to persistent disk or normal
`config-cache`.

Preferred handling:

- The controller reads referenced TLS `Secret` objects, validates references
  and `ReferenceGrant` requirements, and distributes certificate material to
  proxies through the authenticated snapshot channel.
- Proxies hold certificate material in memory and update Pingora TLS state
  without persisting private keys.
- If Pingora integration requires file paths for an early milestone, write
  temporary files only to an `emptyDir` mounted with `medium: Memory`, under a
  path such as `/var/run/light-k8s-gateway/tls`.

Never copy TLS private keys into config-server, `config-cache`, persistent
volumes, image layers, or logs.

## Endpoint Abstraction

`light-pingora` should not need to know whether endpoints came from Kubernetes,
`direct-registry.yml`, controller-rs discovery, or a static config file. Add a
shared endpoint abstraction such as:

```text
UpstreamCluster
  name
  protocol
  tls settings
  load-balancing policy
  EndpointSet
    endpoint address
    port
    health/ready state
    metadata
```

`light-k8s-gateway-controller` translates `Service` and `EndpointSlice` objects
into this shape. Existing Light discovery paths can translate direct registry
and portal-registry results into the same shape. The Pingora route-table module
then selects an `UpstreamCluster` without carrying Kubernetes-specific logic.

## East/West Mesh Model

Gateway API mesh support uses a different binding model. Routes attach directly
to `Service` resources:

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata:
  name: petstore-policy
  namespace: apps
spec:
  parentRefs:
    - group: core
      kind: Service
      name: petstore
      port: 8080
  rules:
    - matches:
        - path:
            type: PathPrefix
            value: /v1
      backendRefs:
        - name: petstore-v2
          port: 8080
          weight: 10
        - name: petstore-v1
          port: 8080
          weight: 90
```

The runtime semantics are:

- If no route attaches to a Service, default mesh behavior forwards to the
  Service backend.
- If routes attach and the request matches at least one route, the selected
  route backendRefs determine the destination.
- If routes attach and no route matches, reject the request.
- Same-namespace routes are producer routes and affect all clients.
- Different-namespace routes are consumer routes and affect clients in the
  route namespace.

The current `light-gateway` can proxy service-to-service calls explicitly, but
it does not transparently intercept traffic to Kubernetes Service frontends.
That means a real mesh implementation needs a data-plane attachment model, not
only a route compiler.

Recommended mesh milestones:

- Mesh M0: compile Service-attached `HTTPRoute` resources and expose the
  effective route table through logs, module registry, and status. This proves
  the control-plane model without traffic interception.
- Mesh M1: support an explicit in-cluster egress gateway mode. Workloads call
  `light-gateway` directly or through a configured HTTP proxy. This is useful
  operationally, but not advertised as transparent GAMMA conformance.
- Mesh M2: add sidecar mode. Inject a lightweight `light-gateway` sidecar, or
  preferably a smaller `light-sidecar` or `light-mesh` binary using the same
  `light-pingora` route-table module. Redirect outbound HTTP traffic to the
  sidecar, identify the original Service destination, apply Service-attached
  routes, then proxy to selected endpoints.
- Mesh M3: add node-local or ambient mode. Use a DaemonSet plus CNI or eBPF
  redirection to intercept Service traffic without per-pod sidecars. This has a
  larger operational surface and should follow sidecar validation.

Sidecar mode is the shortest path because the current `light-gateway` already
has sidecar concepts such as `sidecar.egressIngressIndicator`, token handling
for outbound calls, and service discovery. The production packaging should
still be a dedicated sidecar or mesh product if the target is transparent
east/west traffic. The missing pieces are transparent redirect, original
destination detection, and a Service-oriented route table.

## Mesh Data-Plane Requirements

To proxy east/west traffic with GAMMA semantics, add:

- A mesh route compiler that watches `HTTPRoute`, `Service`, `EndpointSlice`,
  `ReferenceGrant`, and namespaces.
- A Service frontend index keyed by namespace, Service name, port, DNS name,
  ClusterIP, and possibly original destination socket address.
- Producer and consumer route merge logic that follows Gateway API mesh rules.
- Request matching and rejection behavior for Services with attached routes.
- Backend endpoint selection from the selected route's backendRefs.
- A sidecar or node-local interception mechanism that can recover the original
  destination Service before the request is proxied.
- Policy hooks for Light security, token, and observability handlers.
- Mesh conformance test wiring with `--supported-features=Mesh`.

Do not map GAMMA Service routes to `Gateway` listeners. In mesh mode, the
Service is the parent object, and `GatewayClass`/`Gateway` are intentionally not
part of the route binding.

## Coexistence With Existing Light Runtime

Keep these layers distinct:

- Gateway API resources express portable Kubernetes routing intent.
- `light-pingora` route tables execute the selected routing intent.
- `handler.yml` and Light module config apply Light-specific behavior.
- `light-gateway` continues to serve the current microgateway, sidecar, BFF,
  API, agent, MCP, and LLM provider use cases.
- `light-k8s-gateway` owns Kubernetes Gateway API ingress behavior.
- `portal-registry` and `direct-registry.yml` remain available for non-Kubernetes
  targets and existing Light service discovery.
- Config-server remains the source for non-Kubernetes `light-gateway` policy
  and migration bridges. Kubernetes-native `light-k8s-gateway` routing and
  policy intent should come from Gateway API resources and Light policy CRDs.

For ingress, Kubernetes `Service` and `EndpointSlice` should be the primary
backend source. For non-Kubernetes or hybrid targets, add an explicit
implementation-specific backend policy instead of overloading portable
`backendRefs`.

## Status And Conformance

Gateway API users rely on status. The controller must update:

- `GatewayClass.status.conditions`.
- `Gateway.status.addresses`, listener conditions, and supported features.
- `HTTPRoute.status.parents` for every parentRef.
- Light policy CRD status, including `Accepted`, `Programmed`, and conflict
  conditions.

Only the active leader should update Kubernetes status. Controller replicas use
Kubernetes `Lease` leader election to avoid API-server write races and status
flapping.

Minimum conformance gates:

```sh
go test ./conformance -run TestConformance -args \
  --gateway-class=light-k8s-gateway \
  --supported-features=Gateway,HTTPRoute
```

Mesh conformance gate:

```sh
go test ./conformance -run TestConformance -args \
  --supported-features=Mesh
```

When ingress and mesh are both enabled:

```sh
go test ./conformance -run TestConformance -args \
  --gateway-class=light-k8s-gateway \
  --supported-features=Mesh,Gateway,HTTPRoute
```

## Observability And Telemetry

`light-k8s-gateway` must be operable as a primary ingress controller. Provide
Prometheus metrics, OpenTelemetry traces, and structured logs from day 1.

Proxy metrics:

- Request count tagged by Gateway, listener, route namespace, `HTTPRoute`,
  backend Service, status code, and status class.
- Request duration and upstream duration histograms.
- Active connections and in-flight requests.
- Upstream connection errors, retries, timeouts, and circuit-breaker opens.
- Snapshot version, snapshot age, and snapshot apply errors.

Controller metrics:

- Reconcile count, duration, and error count by resource kind.
- Kubernetes watch reconnect count and API-server request errors.
- Status update count and conflict count.
- Leader-election state.
- Snapshot generation count, size, and publish errors.

Tracing:

- Propagate W3C `traceparent` and existing Light correlation IDs.
- Create ingress spans tagged with Gateway API resource identity:
  `gateway.namespace`, `gateway.name`, `listener.name`, `route.namespace`,
  `route.name`, `route.rule`, `backend.service.namespace`, and
  `backend.service.name`.
- Record upstream selection, retries, and policy decisions as span events
  without logging tokens, private keys, or sensitive headers.

## Migration From NGINX Or Traefik

Recommended customer migration:

1. Install `light-k8s-gateway` with a new `GatewayClass` named
   `light-k8s-gateway`.
2. Keep NGINX or Traefik running for existing `Ingress` or Gateway API classes.
3. Create equivalent `Gateway` and `HTTPRoute` resources for one host.
4. Validate status, route behavior, TLS, logs, metrics, and backend health.
5. Move DNS or load balancer traffic for that host to `light-k8s-gateway`.
6. Repeat host by host.
7. Remove the old ingress controller only after route parity and operational
   dashboards are in place.

An optional Ingress-to-HTTPRoute converter can help customers migrate, but it
should be a tool, not part of the runtime request path.

## Open Questions

- What is the first supported east/west deployment model: current
  `light-gateway` as explicit egress gateway, a dedicated sidecar, or ambient?
- How much of the current `server.yml` listener contract should remain in
  `light-runtime` versus moving Gateway API listener binding into
  `light-pingora`?
- Should the controller-to-proxy snapshot protocol stay as a small internal
  gRPC API, or should it adopt an xDS-compatible model early?
- Which Light policy CRDs are required for the MVP: auth, rate limit, header
  policy, request size, token, or a generic handler-chain policy?
- What is the exact `UpstreamCluster` health model shared by Kubernetes
  EndpointSlice, controller-rs discovery, and direct registry sources?

## Suggested Implementation Order

1. Create `apps/light-k8s-gateway-controller` and
   `apps/light-k8s-gateway-proxy` with separate ServiceAccounts and RBAC.
2. Add controller leader election with Kubernetes `Lease` objects.
3. Define `GatewayApiSnapshot`, `UpstreamCluster`, `EndpointSet`, and the
   authenticated controller-to-proxy snapshot stream.
4. Implement proxy-side snapshot loading through `ConfigManager`.
5. Implement `GatewayClass`, `Gateway`, `HTTPRoute`, `ReferenceGrant`,
   `Service`, `EndpointSlice`, `Secret`, and `Namespace` watches.
6. Implement attachment validation, policy validation, status updates, and
   snapshot publishing.
7. Add a `light-pingora` Gateway API route table and route HTTP traffic to
   Kubernetes Service endpoints.
8. Add memory-only TLS Secret handling and terminated HTTPS listener support
   for the common `80`/`443` ingress case.
9. Add initial Light policy CRDs using Gateway API policy attachment.
10. Add Prometheus metrics, OpenTelemetry tracing, and structured logs for the
    controller and proxy.
11. Run HTTPRoute Gateway conformance and close gaps.
12. Add multi-listener runtime support.
13. Add mesh route compilation for Service-attached `HTTPRoute` resources.
14. Add explicit egress gateway mode for early east/west use.
15. Add sidecar interception and run mesh conformance.
16. Evaluate ambient/node-local mode after sidecar behavior is proven.
