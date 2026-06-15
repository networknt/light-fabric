# Deploy Kubernetes

This page describes the recommended Kubernetes deployment model for the Rust
`light-agent` image from `light-fabric/apps/light-agent`.

Use this model when an agent service runs in a cluster and exposes the chat
UI/WebSocket endpoint through a Kubernetes Service, Ingress, or Gateway API. The
agent serves the local chat UI, connects to an LLM provider, calls MCP tools
through `light-gateway`, stores conversation memory in Postgres, and registers
with controller.

## Recommended Model

Deploy the agent as a normal single-container Kubernetes workload:

- `Deployment` for the agent pod.
- `Service` for stable in-cluster access.
- `ConfigMap` for bootstrap config and non-secret values.
- `Secret` for bearer tokens, config passwords, host id, and database URL.
- `emptyDir` or `PersistentVolumeClaim` for `config-cache`.
- `ConfigMap` or custom image layer for `public/` chat UI assets.
- Optional `Ingress`, `Gateway API`, `NodePort`, or `LoadBalancer` for external
  browser access.

Keep runtime policy and shared platform configuration in config-server. The
Kubernetes bootstrap config should only contain enough information for startup,
trust, model/provider selection, `light-gateway` access, database access, and
controller registration.

## Image

Build the image from the workspace root:

```sh
./apps/light-agent/build.sh 2.2.1
```

For local testing without pushing:

```sh
./apps/light-agent/build.sh 2.2.1 --local
```

Use immutable tags in Kubernetes. Avoid `latest` for customer deployments.

The current runtime image uses:

```text
/app/light-agent
/app/config -> /config
```

The process runs as the image user `agent`. Mount `/config` for bootstrap
config and make `/app/config-cache` writable.

The current Dockerfile does not copy `apps/light-agent/public/` into the runtime
image. For Kubernetes, either mount the `public/` files from a ConfigMap or
build a custom image that includes them under `/app/public`.

## Runtime Paths

Recommended container layout:

```text
/config/
  startup.yml
  server.yml
  portal-registry.yml
  client.yml
  mcp-client.yml
  ollama.yml
  values.yml
  ca.pem

/app/config-cache/
  values.yml
  downloaded certs and files

/app/public/
  index.html
```

Use a read-only projected volume for `/config`. Use a writable volume for
`/app/config-cache`.

For most deployments, use `emptyDir` for `config-cache`. This gives each pod a
fresh cache and avoids accidentally keeping stale config across pod replacement.

Use a `PersistentVolumeClaim` only when the customer explicitly wants the agent
to restart from the last downloaded config during a config-server outage. A
persistent cache improves outage tolerance but can also preserve stale runtime
state.

## Runtime Dependencies

The pod must be able to reach:

- Controller, through `portalRegistry.portalUrl`.
- Config-server, through `startup.configServerUri`.
- `light-gateway`, through `mcp-client.gatewayUrl` and `mcp-client.path`.
- The model provider, currently Ollama by default.
- Postgres, through `DATABASE_URL`.

The Postgres database must contain the Hindsight memory tables used by
`light-agent`, including:

- `agent_memory_bank_t`
- `agent_memory_unit_t`
- `agent_session_history_t`

`LIGHT_AGENT_HOST_ID` must be a valid host UUID for the target tenant/host. The
agent stores memory and session history under this host id.

## Agent Roles

The same image can run different logical agents. Use a different service id,
deployment name, Service name, and port for each concurrently running role.

Common service ids are:

```text
com.networknt.agent.account-1.0.0
com.networknt.agent.advisor-1.0.0
com.networknt.agent.tech-support-1.0.0
```

For a single account agent, a conventional Kubernetes name is
`light-agent-account`. For multiple agents in the same namespace, use names such
as:

```text
light-agent-account
light-agent-advisor
light-agent-tech-support
```

Each role needs a unique Service name. If they share one namespace and expose
through one Ingress host, route each role by host or path.

## Registration Address

In Kubernetes, do not register the pod IP. Pod IPs are ephemeral.

If controller and callers are inside the same cluster, advertise the Service DNS
name:

```yaml
server.advertisedAddress: light-agent-account.light-agent
```

The pattern is:

```text
<service-name>.<namespace>
```

The port is still registered separately from the host/address.

If controller or callers are outside the cluster, advertise the externally
reachable DNS name instead, such as the Ingress or LoadBalancer hostname:

```yaml
server.advertisedAddress: account-agent.customer.example.com
```

## Bootstrap Config

Example `values.yml` for an in-cluster controller, config-server, gateway,
Ollama, and Postgres:

```yaml
startup.host: customer.example.com
startup.timeout: 3000
startup.connectTimeout: 3000
startup.bootstrapCaCertPath: config/ca.pem
startup.externalConfigDir: /app/config-cache

light-config-server-uri: https://config-server.lightapi.svc.cluster.local:8435

server.serviceId: com.networknt.agent.account-1.0.0
server.environment: prod
server.ip: 0.0.0.0
server.advertisedAddress: light-agent-account.light-agent
server.httpPort: 8083
server.enableHttp: true
server.httpsPort: 8443
server.enableHttps: false
server.enableRegistry: true
server.startOnRegistryFailure: true

portalRegistry.portalUrl: https://controller.lightapi.svc.cluster.local:8438

client.verifyHostname: true

mcp-client.gatewayUrl: https://ai-microgateway.light-gateway:8443
mcp-client.path: /mcp
mcp-client.timeoutMs: 5000

ollama.ollamaUrl: http://ollama.ai.svc.cluster.local:11434
ollama.model: llama3.1:8b
```

Example `startup.yml`:

```yaml
host: ${startup.host:dev.lightapi.net}
serviceId: ${server.serviceId:com.networknt.agent.account-1.0.0}
envTag: ${server.environment:dev}
acceptHeader: application/yaml
timeout: ${startup.timeout:3000}
connectTimeout: ${startup.connectTimeout:3000}
configServerUri: ${light-config-server-uri:https://local.localhost}
authorization: ${light_portal_authorization:}
bootstrapCaCertPath: ${startup.bootstrapCaCertPath:config/ca.pem}
externalConfigDir: ${startup.externalConfigDir:/app/config-cache}
```

Example `server.yml`:

```yaml
ip: ${server.ip:0.0.0.0}
advertisedAddress: ${server.advertisedAddress:127.0.0.1}
httpPort: ${server.httpPort:8083}
enableHttp: ${server.enableHttp:true}
httpsPort: ${server.httpsPort:8443}
enableHttps: ${server.enableHttps:false}
serviceId: ${server.serviceId:com.networknt.agent.account-1.0.0}
enableRegistry: ${server.enableRegistry:true}
startOnRegistryFailure: ${server.startOnRegistryFailure:true}
dynamicPort: ${server.dynamicPort:false}
environment: ${server.environment:dev}
shutdownGracefulPeriod: ${server.shutdownGracefulPeriod:2000}
```

Example `portal-registry.yml`:

```yaml
portalUrl: ${portalRegistry.portalUrl:https://localhost:8438}
portalToken: ${light_portal_authorization:}
controllerDiscoveryToken: ${portalRegistry.controllerDiscoveryToken:}
```

Example `client.yml`:

```yaml
tls:
  verifyHostname: ${client.verifyHostname:true}
```

Example `mcp-client.yml`:

```yaml
gatewayUrl: ${mcp-client.gatewayUrl:https://ai-microgateway.light-gateway:8443}
path: ${mcp-client.path:/mcp}
timeoutMs: ${mcp-client.timeoutMs:5000}
```

Example `ollama.yml`:

```yaml
ollamaUrl: ${ollama.ollamaUrl:http://ollama.ai.svc.cluster.local:11434}
model: ${ollama.model:llama3.1:8b}
```

For the current `light-agent` implementation, keep `ollama.yml` and
`mcp-client.yml` in the local bootstrap config. They are read during process
initialization before the runtime completes remote config bootstrap.

Use the customer CA in `ca.pem`. Do not disable hostname verification in
production to work around certificate SAN problems.

## Secrets

Store the portal bearer token, optional config password, host id, and database
URL in a Kubernetes `Secret`.

Example:

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: light-agent-account-secret
  namespace: light-agent
type: Opaque
stringData:
  LIGHT_PORTAL_AUTHORIZATION: "Bearer <token>"
  light_4j_config_password: "<config-password-if-needed>"
  LIGHT_AGENT_HOST_ID: "<host-uuid>"
  DATABASE_URL: "postgres://agent_user:<password>@postgres.lightapi.svc.cluster.local:5432/configserver"
data:
  ca.pem: <base64-ca-pem>
```

`LIGHT_PORTAL_AUTHORIZATION` is used for config-server bootstrap and controller
registration. It is not the end-user chat token. If downstream MCP tools require
caller identity, the browser or BFF should send the user's `Authorization`
header to the agent WebSocket endpoint so the agent can forward it to
`light-gateway`.

Do not store real bearer tokens, database passwords, or customer CA material in
Git, ConfigMaps, Helm values committed to the repo, or rendered deployment
examples.

## Example Manifests

Example `ConfigMap`:

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: light-agent-account-config
  namespace: light-agent
  labels:
    app.kubernetes.io/name: light-agent-account
    app.kubernetes.io/component: agent
data:
  values.yml: |
    startup.host: customer.example.com
    startup.timeout: 3000
    startup.connectTimeout: 3000
    startup.bootstrapCaCertPath: config/ca.pem
    startup.externalConfigDir: /app/config-cache
    light-config-server-uri: https://config-server.lightapi.svc.cluster.local:8435
    server.serviceId: com.networknt.agent.account-1.0.0
    server.environment: prod
    server.ip: 0.0.0.0
    server.advertisedAddress: light-agent-account.light-agent
    server.httpPort: 8083
    server.enableHttp: true
    server.httpsPort: 8443
    server.enableHttps: false
    server.enableRegistry: true
    server.startOnRegistryFailure: true
    portalRegistry.portalUrl: https://controller.lightapi.svc.cluster.local:8438
    client.verifyHostname: true
    mcp-client.gatewayUrl: https://ai-microgateway.light-gateway:8443
    mcp-client.path: /mcp
    mcp-client.timeoutMs: 5000
    ollama.ollamaUrl: http://ollama.ai.svc.cluster.local:11434
    ollama.model: llama3.1:8b
  startup.yml: |
    host: ${startup.host:dev.lightapi.net}
    serviceId: ${server.serviceId:com.networknt.agent.account-1.0.0}
    envTag: ${server.environment:dev}
    acceptHeader: application/yaml
    timeout: ${startup.timeout:3000}
    connectTimeout: ${startup.connectTimeout:3000}
    configServerUri: ${light-config-server-uri:https://local.localhost}
    authorization: ${light_portal_authorization:}
    bootstrapCaCertPath: ${startup.bootstrapCaCertPath:config/ca.pem}
    externalConfigDir: ${startup.externalConfigDir:/app/config-cache}
  server.yml: |
    ip: ${server.ip:0.0.0.0}
    advertisedAddress: ${server.advertisedAddress:127.0.0.1}
    httpPort: ${server.httpPort:8083}
    enableHttp: ${server.enableHttp:true}
    httpsPort: ${server.httpsPort:8443}
    enableHttps: ${server.enableHttps:false}
    serviceId: ${server.serviceId:com.networknt.agent.account-1.0.0}
    enableRegistry: ${server.enableRegistry:true}
    startOnRegistryFailure: ${server.startOnRegistryFailure:true}
    dynamicPort: ${server.dynamicPort:false}
    environment: ${server.environment:dev}
    shutdownGracefulPeriod: ${server.shutdownGracefulPeriod:2000}
  portal-registry.yml: |
    portalUrl: ${portalRegistry.portalUrl:https://localhost:8438}
    portalToken: ${light_portal_authorization:}
    controllerDiscoveryToken: ${portalRegistry.controllerDiscoveryToken:}
  client.yml: |
    tls:
      verifyHostname: ${client.verifyHostname:true}
  mcp-client.yml: |
    gatewayUrl: ${mcp-client.gatewayUrl:https://ai-microgateway.light-gateway:8443}
    path: ${mcp-client.path:/mcp}
    timeoutMs: ${mcp-client.timeoutMs:5000}
  ollama.yml: |
    ollamaUrl: ${ollama.ollamaUrl:http://ollama.ai.svc.cluster.local:11434}
    model: ${ollama.model:llama3.1:8b}
```

Create the `public/` ConfigMap from the repo asset:

```sh
kubectl -n light-agent create configmap light-agent-account-public \
  --from-file=index.html=apps/light-agent/public/index.html \
  --dry-run=client -o yaml
```

Example `Deployment`:

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: light-agent-account
  namespace: light-agent
  labels:
    app.kubernetes.io/name: light-agent-account
    app.kubernetes.io/component: agent
    app.kubernetes.io/part-of: lightapi
spec:
  replicas: 1
  selector:
    matchLabels:
      app.kubernetes.io/name: light-agent-account
  template:
    metadata:
      labels:
        app.kubernetes.io/name: light-agent-account
        app.kubernetes.io/component: agent
        app.kubernetes.io/part-of: lightapi
    spec:
      securityContext:
        fsGroup: 999
        fsGroupChangePolicy: OnRootMismatch
      containers:
        - name: light-agent
          image: networknt/light-agent:2.2.1
          imagePullPolicy: IfNotPresent
          env:
            - name: LIGHT_PORTAL_AUTHORIZATION
              valueFrom:
                secretKeyRef:
                  name: light-agent-account-secret
                  key: LIGHT_PORTAL_AUTHORIZATION
            - name: light_4j_config_password
              valueFrom:
                secretKeyRef:
                  name: light-agent-account-secret
                  key: light_4j_config_password
                  optional: true
            - name: LIGHT_AGENT_HOST_ID
              valueFrom:
                secretKeyRef:
                  name: light-agent-account-secret
                  key: LIGHT_AGENT_HOST_ID
            - name: DATABASE_URL
              valueFrom:
                secretKeyRef:
                  name: light-agent-account-secret
                  key: DATABASE_URL
            - name: RUST_LOG
              value: info
            - name: AGENT_LOG_ANSI
              value: "false"
          ports:
            - name: http
              containerPort: 8083
              protocol: TCP
            - name: https
              containerPort: 8443
              protocol: TCP
          readinessProbe:
            httpGet:
              path: /health
              port: http
            initialDelaySeconds: 5
            periodSeconds: 10
          livenessProbe:
            httpGet:
              path: /health
              port: http
            initialDelaySeconds: 30
            periodSeconds: 30
          resources:
            requests:
              cpu: 100m
              memory: 256Mi
            limits:
              cpu: 1000m
              memory: 768Mi
          volumeMounts:
            - name: bootstrap-config
              mountPath: /config
              readOnly: true
            - name: config-cache
              mountPath: /app/config-cache
            - name: public
              mountPath: /app/public
              readOnly: true
      volumes:
        - name: bootstrap-config
          projected:
            sources:
              - configMap:
                  name: light-agent-account-config
              - secret:
                  name: light-agent-account-secret
                  items:
                    - key: ca.pem
                      path: ca.pem
        - name: config-cache
          emptyDir: {}
        - name: public
          configMap:
            name: light-agent-account-public
```

Example `Service`:

```yaml
apiVersion: v1
kind: Service
metadata:
  name: light-agent-account
  namespace: light-agent
  labels:
    app.kubernetes.io/name: light-agent-account
    app.kubernetes.io/component: agent
spec:
  type: ClusterIP
  selector:
    app.kubernetes.io/name: light-agent-account
  ports:
    - name: http
      port: 8083
      targetPort: http
      protocol: TCP
    - name: https
      port: 8443
      targetPort: https
      protocol: TCP
```

## External Access

For local testing with a `ClusterIP` Service:

```sh
kubectl -n light-agent port-forward svc/light-agent-account 8083:8083
```

Health check:

```sh
curl -i http://127.0.0.1:8083/health
```

If exposing through Ingress, make sure WebSocket upgrade is supported and idle
timeouts are long enough for chat sessions.

Example NGINX Ingress annotations:

```yaml
nginx.ingress.kubernetes.io/proxy-read-timeout: "3600"
nginx.ingress.kubernetes.io/proxy-send-timeout: "3600"
nginx.ingress.kubernetes.io/backend-protocol: "HTTP"
```

If downstream MCP tools require caller identity, put the agent behind a BFF or
authenticated reverse proxy that forwards the user's `Authorization` header to
the WebSocket request. A browser-created WebSocket from the embedded static UI
does not directly set arbitrary authorization headers.

## Deploy Through Light-Deployer

The repo template lives at:

```text
apps/light-agent/k8s/light-agent
```

Use the same template rules as `light-gateway`.

When `light-deployer` runs outside the cluster and has
`LIGHT_DEPLOYER_TEMPLATE_BASE_DIR` set, `repoUrl: "local"` can point to local
templates.

When `light-deployer` runs inside Kubernetes, use a real Git URL:

```json
{
  "template": {
    "repoUrl": "https://github.com/networknt/light-fabric.git",
    "ref": "main",
    "path": "apps/light-agent/k8s/light-agent"
  }
}
```

Do not use `repoUrl: "local"` for an in-cluster deployer unless the template
repo is mounted into the deployer container and
`LIGHT_DEPLOYER_TEMPLATE_BASE_DIR` points to it.

Keep `Namespace` out of templates rendered by `light-deployer` if the deployer
policy blocks cluster-scoped resources. Create the namespace separately:

```sh
kubectl create namespace light-agent
```

## Config-Server Requirements

Before deploying the agent pod, config-server should already have config for
the tuple used by startup:

```text
host = startup.host
serviceId = server.serviceId
envTag = server.environment
```

At minimum, config-server should return runtime config for:

- `values.yml`
- `server.yml` when listener or registration settings are centrally managed.
- `portal-registry.yml` when controller URLs or registry settings are centrally
  managed.
- `client.yml` when TLS verification behavior is centrally managed.

For the current `light-agent`, keep `mcp-client.yml` and `ollama.yml` in the
local bootstrap ConfigMap even if other runtime config comes from config-server.
They are loaded before remote bootstrap completes.

## Startup Flow

Expected runtime flow:

```text
Kubernetes starts pod
  -> /app/light-agent
  -> read local /config/values.yml, ollama.yml, and mcp-client.yml
  -> connect to Postgres with DATABASE_URL
  -> build the MCP client for light-gateway
  -> call config-server with LIGHT_PORTAL_AUTHORIZATION
  -> write downloaded runtime config into /app/config-cache
  -> start the Axum HTTP/WebSocket server
  -> register the agent with controller using portalRegistry.portalUrl
  -> serve the chat UI from /app/public
  -> forward tool discovery and tool calls to light-gateway
```

When `startup.yml` configures config-server, the runtime tries to download the
latest `values.yml` before starting. If that download fails for any reason, the
runtime continues startup with the available local and cached config, including
`/app/config-cache/values.yml` when present.

## Upgrade And Rollback

Use Kubernetes rolling updates with immutable image tags:

```sh
kubectl -n light-agent set image deploy/light-agent-account \
  light-agent=networknt/light-agent:2.2.2
kubectl -n light-agent rollout status deploy/light-agent-account
```

Rollback:

```sh
kubectl -n light-agent rollout undo deploy/light-agent-account
```

For production, prefer changing only one variable at a time: either image tag
or config-server runtime config, not both in the same rollout.

## Validation Checklist

After deployment:

- `kubectl -n light-agent rollout status deploy/light-agent-account` succeeds.
- Pods are ready and restart count is stable.
- Logs show successful Postgres connection.
- Logs show successful config-server bootstrap.
- Logs show successful controller registration.
- Controller shows the agent registered with the expected service id,
  environment, host, and port.
- `curl http://127.0.0.1:8083/health` succeeds through port-forward or Ingress.
- The chat UI loads.
- The chat WebSocket connects to `/chat`.
- MCP `tools/list` reaches `light-gateway`.
- MCP `tools/call` reaches the backend MCP server through `light-gateway`.
- A pod restart still starts cleanly with the selected cache policy.

## Security Checklist

- Keep bearer tokens, config passwords, database passwords, and host ids in
  Kubernetes `Secret`, not `ConfigMap`.
- Use customer CA trust and keep `client.verifyHostname: true` in production.
- Use immutable image tags and image pull credentials from Kubernetes secrets
  when the registry is private.
- Run as the non-root image user.
- Make `/config` read-only.
- Make only `/app/config-cache` writable.
- Restrict ingress traffic to required agent ports.
- Restrict egress traffic to config-server, controller, `light-gateway`,
  Ollama, and Postgres.
- Rotate `LIGHT_PORTAL_AUTHORIZATION` through the customer secret process.
