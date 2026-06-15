# Deploy Kubernetes

This page describes the recommended Kubernetes deployment model for the Rust
`light-gateway` image from `light-fabric/apps/light-gateway`.

Use this model when `light-gateway` runs as a microgateway in front of backend
MCP servers. The pod starts from local bootstrap config, downloads runtime
config from config-server into `config-cache`, starts Pingora, and registers the
gateway with controller.

## Recommended Model

Deploy the gateway as a normal single-container Kubernetes workload:

- `Deployment` for the gateway pod.
- `Service` for stable in-cluster access.
- `ConfigMap` for bootstrap config and non-secret values.
- `Secret` for bearer tokens and config passwords.
- `emptyDir` or `PersistentVolumeClaim` for `config-cache`.
- Optional `Ingress`, `Gateway API`, `NodePort`, or `LoadBalancer` for external
  client access.

Keep gateway behavior such as MCP route definitions, access-control rules,
backend MCP targets, and runtime TLS files in config-server. The Kubernetes
bootstrap config should only contain enough information for startup, trust, and
registration.

## Image

Build the image from the workspace root:

```sh
./apps/light-gateway/build.sh 2.2.1
```

For local testing without pushing:

```sh
./apps/light-gateway/build.sh 2.2.1 --local
```

Use immutable tags in Kubernetes. Avoid `latest` for customer deployments.

The runtime image uses:

```text
/app/light-gateway
/app/config -> /config
/app/config-cache
```

The process runs as the image user `gateway`. Mount `/config` for bootstrap
config and make `/app/config-cache` writable.

## Runtime Paths

Recommended container layout:

```text
/config/
  startup.yml
  server.yml
  portal-registry.yml
  client.yml
  values.yml
  ca.pem

/app/config-cache/
  values.yml
  downloaded certs and files
```

Use a read-only `ConfigMap` for `/config`. Use a writable volume for
`/app/config-cache`.

For most deployments, use `emptyDir` for `config-cache`. This gives each pod a
fresh cache and avoids accidentally keeping stale config across pod replacement.

Use a `PersistentVolumeClaim` only when the customer explicitly wants the
gateway to restart from the last downloaded config during a config-server
download outage. On each startup, the gateway tries to download the latest
`values.yml` before starting.

## Registration Address

In Kubernetes, do not register the pod IP. Pod IPs are ephemeral.

If controller and callers are inside the same cluster, advertise the Service DNS
name:

```yaml
server.advertisedAddress: ai-microgateway.light-gateway
```

The pattern is:

```text
<service-name>.<namespace>
```

The port is still registered separately from the host/address.

If controller or callers are outside the cluster, advertise the externally
reachable DNS name instead, such as the Ingress or LoadBalancer hostname:

```yaml
server.advertisedAddress: mcp-gateway.customer.example.com
```

For the Rust gateway, this is configured with `server.advertisedAddress`. The
Java gateway template uses `STATUS_HOST_IP`; that is a light-4j-specific hook
and is not the Rust gateway contract.

## Bootstrap Config

Example `values.yml` for an in-cluster controller and config-server:

```yaml
startup.host: customer.example.com
startup.timeout: 3000
startup.connectTimeout: 3000
startup.bootstrapCaCertPath: config/ca.pem

light-config-server-uri: https://config-server.lightapi.svc.cluster.local:8435

server.serviceId: com.customer.mcp-gateway-1.0.0
server.environment: prod
server.ip: 0.0.0.0
server.advertisedAddress: ai-microgateway.light-gateway
server.httpPort: 8080
server.enableHttp: true
server.httpsPort: 8443
server.enableHttps: false
server.enableRegistry: true
server.startOnRegistryFailure: true

portalRegistry.portalUrl: https://controller.lightapi.svc.cluster.local:8438
client.caCertPath: config/ca.pem
client.verifyHostname: true
```

Example `startup.yml`:

```yaml
host: ${startup.host:dev.lightapi.net}
serviceId: ${server.serviceId:com.networknt.light-gateway-1.0.0}
envTag: ${server.environment:dev}
acceptHeader: application/yaml
timeout: ${startup.timeout:3000}
connectTimeout: ${startup.connectTimeout:3000}
configServerUri: ${light-config-server-uri:https://local.localhost}
authorization: ${light_portal_authorization:}
bootstrapCaCertPath: ${startup.bootstrapCaCertPath:config/ca.pem}
```

Example `server.yml`:

```yaml
ip: ${server.ip:0.0.0.0}
advertisedAddress: ${server.advertisedAddress:127.0.0.1}
httpPort: ${server.httpPort:8080}
enableHttp: ${server.enableHttp:true}
httpsPort: ${server.httpsPort:8443}
enableHttps: ${server.enableHttps:false}
tlsCertPath: ${server.tlsCertPath:}
tlsKeyPath: ${server.tlsKeyPath:}
serviceId: ${server.serviceId:com.networknt.light-gateway-1.0.0}
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
  caCertPath: ${client.caCertPath:config/ca.pem}
  verifyHostname: ${client.verifyHostname:true}
```

Use the customer CA in `ca.pem`. Do not disable hostname verification in
production to work around certificate SAN problems.

## Secrets

Store the portal bearer token and optional config password in a Kubernetes
`Secret`.

Example:

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: light-gateway-secret
  namespace: light-gateway
type: Opaque
stringData:
  LIGHT_PORTAL_AUTHORIZATION: "Bearer <token>"
  light_4j_config_password: "<config-password-if-needed>"
```

`LIGHT_PORTAL_AUTHORIZATION` is used for config-server bootstrap. It is also
used by portal registry startup when `portal-registry.yml` resolves
`portalToken` from `light_portal_authorization`.

Do not store real bearer tokens in Git, ConfigMaps, Helm values committed to the
repo, or rendered deployment examples.

## Example Manifests

Create the namespace separately:

```sh
kubectl create namespace light-gateway
```

If deploying through `light-deployer`, keep `Namespace` out of the rendered
bundle because deployer policy may block cluster-scoped resources.

Example bootstrap `ConfigMap`:

```yaml
apiVersion: v1
kind: ConfigMap
metadata:
  name: light-gateway-bootstrap
  namespace: light-gateway
data:
  values.yml: |
    startup.host: customer.example.com
    startup.timeout: 3000
    startup.connectTimeout: 3000
    startup.bootstrapCaCertPath: config/ca.pem
    light-config-server-uri: https://config-server.lightapi.svc.cluster.local:8435
    server.serviceId: com.customer.mcp-gateway-1.0.0
    server.environment: prod
    server.ip: 0.0.0.0
    server.advertisedAddress: ai-microgateway.light-gateway
    server.httpPort: 8080
    server.enableHttp: true
    server.httpsPort: 8443
    server.enableHttps: false
    server.enableRegistry: true
    server.startOnRegistryFailure: true
    portalRegistry.portalUrl: https://controller.lightapi.svc.cluster.local:8438
    client.caCertPath: config/ca.pem
    client.verifyHostname: true
  startup.yml: |
    host: ${startup.host:dev.lightapi.net}
    serviceId: ${server.serviceId:com.networknt.light-gateway-1.0.0}
    envTag: ${server.environment:dev}
    acceptHeader: application/yaml
    timeout: ${startup.timeout:3000}
    connectTimeout: ${startup.connectTimeout:3000}
    configServerUri: ${light-config-server-uri:https://local.localhost}
    authorization: ${light_portal_authorization:}
    bootstrapCaCertPath: ${startup.bootstrapCaCertPath:config/ca.pem}
  server.yml: |
    ip: ${server.ip:0.0.0.0}
    advertisedAddress: ${server.advertisedAddress:127.0.0.1}
    httpPort: ${server.httpPort:8080}
    enableHttp: ${server.enableHttp:true}
    httpsPort: ${server.httpsPort:8443}
    enableHttps: ${server.enableHttps:false}
    tlsCertPath: ${server.tlsCertPath:}
    tlsKeyPath: ${server.tlsKeyPath:}
    serviceId: ${server.serviceId:com.networknt.light-gateway-1.0.0}
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
      caCertPath: ${client.caCertPath:config/ca.pem}
      verifyHostname: ${client.verifyHostname:true}
  ca.pem: |
    -----BEGIN CERTIFICATE-----
    <customer-ca-certificate>
    -----END CERTIFICATE-----
```

Example `Deployment`:

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: ai-microgateway
  namespace: light-gateway
  labels:
    app: ai-microgateway
spec:
  replicas: 2
  selector:
    matchLabels:
      app: ai-microgateway
  template:
    metadata:
      labels:
        app: ai-microgateway
    spec:
      securityContext:
        fsGroup: 999
        fsGroupChangePolicy: OnRootMismatch
      containers:
        - name: light-gateway
          image: networknt/light-gateway:2.2.1
          imagePullPolicy: IfNotPresent
          env:
            - name: LIGHT_PORTAL_AUTHORIZATION
              valueFrom:
                secretKeyRef:
                  name: light-gateway-secret
                  key: LIGHT_PORTAL_AUTHORIZATION
            - name: light_4j_config_password
              valueFrom:
                secretKeyRef:
                  name: light-gateway-secret
                  key: light_4j_config_password
                  optional: true
            - name: RUST_LOG
              value: info
          ports:
            - name: http
              containerPort: 8080
            - name: https
              containerPort: 8443
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
              memory: 128Mi
            limits:
              cpu: "1"
              memory: 512Mi
          volumeMounts:
            - name: bootstrap-config
              mountPath: /config
              readOnly: true
            - name: config-cache
              mountPath: /app/config-cache
      volumes:
        - name: bootstrap-config
          configMap:
            name: light-gateway-bootstrap
        - name: config-cache
          emptyDir: {}
```

The example uses `fsGroup: 999`, which matches the default gateway group in the
current image. Adjust it if the image user or group changes.

If HTTP is disabled and only HTTPS is enabled, change the probes to an HTTPS
probe or a TCP probe.

Example `Service`:

```yaml
apiVersion: v1
kind: Service
metadata:
  name: ai-microgateway
  namespace: light-gateway
spec:
  type: ClusterIP
  selector:
    app: ai-microgateway
  ports:
    - name: http
      port: 8080
      targetPort: http
    - name: https
      port: 8443
      targetPort: https
```

For external access, add an Ingress, Gateway API route, `NodePort`, or
`LoadBalancer` according to the customer cluster standard. If external clients
or controller use that external path, set `server.advertisedAddress` to the same
externally reachable DNS name.

## Apply With Kubectl

Apply manifests in this order:

```sh
kubectl apply -f namespace.yml
kubectl apply -f secret.yml
kubectl apply -f configmap.yml
kubectl apply -f deployment.yml
kubectl apply -f service.yml
```

Check rollout:

```sh
kubectl -n light-gateway rollout status deploy/ai-microgateway
kubectl -n light-gateway get pods -l app=ai-microgateway
kubectl -n light-gateway logs deploy/ai-microgateway
```

For local testing with a `ClusterIP` Service:

```sh
kubectl -n light-gateway port-forward svc/ai-microgateway 8080:8080 8443:8443
```

## Deploy Through Light-Deployer

When `light-deployer` runs outside the cluster and has
`LIGHT_DEPLOYER_TEMPLATE_BASE_DIR` set, `repoUrl: "local"` can point to local
templates.

When `light-deployer` runs inside Kubernetes, use a real Git URL:

```json
{
  "template": {
    "repoUrl": "https://github.com/networknt/light-fabric.git",
    "ref": "main",
    "path": "apps/light-gateway/k8s/light-gateway"
  }
}
```

Do not use `repoUrl: "local"` for an in-cluster deployer unless the template
repo is mounted into the deployer container and
`LIGHT_DEPLOYER_TEMPLATE_BASE_DIR` points to it.

The in-cluster deployer checks out `repoUrl` at `ref` and reads manifests from
`template.path`.

Keep `Namespace` out of templates rendered by `light-deployer` if the deployer
policy blocks cluster-scoped resources. Create the namespace separately:

```sh
kubectl create namespace light-gateway
```

## Config-Server Requirements

Before deploying the gateway pod, config-server should already have config for
the tuple used by startup:

```text
host = startup.host
serviceId = server.serviceId
envTag = server.environment
```

At minimum, config-server should return runtime config for:

- `handler.yml`
- `mcp-router.yml`
- `access-control.yml` and `rule.yml` when MCP authorization is enabled.
- `security.yml`, `unified-security.yml`, or other active auth config.
- `websocket-router.yml` when WebSocket MCP/BFF routing is enabled.
- Any downstream client, token, or registry config required by the selected
  handlers.

The pod bootstrap files should stay small and stable. Normal route, policy, and
backend changes should go through config-server and controller reload flows.

## Startup Flow

Expected runtime flow:

```text
Kubernetes starts pod
  -> /app/light-gateway
  -> read /app/config -> /config bootstrap files
  -> call config-server with LIGHT_PORTAL_AUTHORIZATION
  -> write downloaded config and files into /app/config-cache
  -> start Pingora with resolved runtime config
  -> register gateway to controller using portalRegistry.portalUrl
  -> advertise server.advertisedAddress and configured port
  -> route protected MCP traffic to backend MCP servers
```

When `startup.yml` configures config-server, the runtime tries to download the
latest `values.yml` before starting. If that download fails for any reason, the
runtime continues startup with the available local and cached config, including
`/app/config-cache/values.yml` when present.

## Upgrade And Rollback

Use Kubernetes rolling updates with immutable image tags:

```sh
kubectl -n light-gateway set image deploy/ai-microgateway \
  light-gateway=networknt/light-gateway:2.2.2
kubectl -n light-gateway rollout status deploy/ai-microgateway
```

Rollback:

```sh
kubectl -n light-gateway rollout undo deploy/ai-microgateway
```

For production, prefer changing only one variable at a time: either image tag
or config-server runtime config, not both in the same rollout.

## Validation Checklist

After deployment:

- `kubectl -n light-gateway rollout status deploy/ai-microgateway` succeeds.
- Pods are ready and restart count is stable.
- Logs show successful config-server bootstrap.
- Logs show successful controller registration.
- Controller shows the gateway registered with the expected service id,
  environment, host, and port.
- `server.advertisedAddress` is reachable from the controller.
- The Service responds on `/health`.
- MCP `tools/list` reaches the gateway.
- MCP `tools/call` reaches the backend MCP server.
- A pod restart still starts cleanly with the selected cache policy.

## Security Checklist

- Keep bearer tokens in Kubernetes `Secret`, not `ConfigMap`.
- Use customer CA trust and keep `client.verifyHostname: true` in production.
- Use immutable image tags and image pull credentials from Kubernetes secrets
  when the registry is private.
- Run as the non-root image user.
- Make `/config` read-only.
- Make only `/app/config-cache` writable.
- Restrict ingress traffic to required gateway ports.
- Restrict egress traffic to config-server, controller, token/key services, and
  backend MCP servers.
- Rotate `LIGHT_PORTAL_AUTHORIZATION` through the customer secret process.
