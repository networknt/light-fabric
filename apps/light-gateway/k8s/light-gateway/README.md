# Rust Light Gateway Kubernetes Template

This folder deploys the Rust `light-gateway` from `light-fabric` as a
single-container microgateway pod.

The gateway uses local bootstrap config from `/config`, downloads runtime config
from config-server into `/app/config-cache`, starts Pingora, and registers with
controller.

The default `name` is `ai-microgateway-rust` so it can be deployed beside the
existing Java `ai-microgateway` in the same `light-gateway` namespace. To
replace the Java deployment, use the Java deployment name and image tag values
intentionally.

## Files

- `configmap.yaml`: bootstrap `values.yml`, `startup.yml`, `server.yml`,
  `portal-registry.yml`, and `client.yml`.
- `secret.yaml`: portal bearer token, optional config password, and bootstrap
  CA certificate.
- `deployment.yaml`: Rust gateway container, projected bootstrap config, and
  writable `config-cache`.
- `service.yaml`: `ClusterIP` Service for in-cluster access.
- `render-request.json`: example light-deployer request.

No `Namespace` manifest is included. Create or reuse the namespace separately.

## Build And Import For MicroK8s

From `/home/steve/workspace/light-fabric`:

```sh
./apps/light-gateway/build.sh rust-local --local
docker save networknt/light-gateway:rust-local | microk8s ctr image import -
```

The template defaults to:

```yaml
image.repository: networknt/light-gateway
image.tag: rust-local
image.pullPolicy: IfNotPresent
```

Use a unique local tag so MicroK8s does not keep running the Java gateway image
from the same repository and tag.

## Required Values

- `lightPortalAuthorization`: bearer token for config-server bootstrap and
  controller registration.
- `bootstrapCaPemBase64`: base64 content of the CA PEM used to trust
  config-server and controller.
- `configServer.uri`: config-server base URL.
- `portalRegistry.portalUrl`: controller URL.
- `startup.host`, `startup.serviceId`, and `startup.envTag`: the tuple used to
  fetch runtime config from config-server.

Generate the CA value:

```sh
base64 -w0 /path/to/ca.pem
```

Do not commit real bearer tokens or customer CA material into this template.

## Registration Address

The Rust gateway registers `server.advertisedAddress`, not `STATUS_HOST_IP`.

For local MicroK8s in-cluster access, use the Service DNS name:

```yaml
server.advertisedAddress: ai-microgateway-rust.light-gateway
```

If you change `name` or `namespace`, update `server.advertisedAddress` to match:

```text
<service-name>.<namespace>
```

If controller or clients are outside the cluster, use the externally reachable
DNS name instead.

## Deploy With Light-Deployer

Start `light-deployer` in real MicroK8s mode with the `light-fabric` repo as the
template base:

```sh
cd /home/steve/workspace/light-fabric/apps/light-deployer
KUBECONFIG=/var/snap/microk8s/current/credentials/client.config \
LIGHT_DEPLOYER_KUBE_MODE=real \
LIGHT_DEPLOYER_TEMPLATE_BASE_DIR=/home/steve/workspace/light-fabric \
./run.sh
```

Render first:

```sh
curl -sS http://127.0.0.1:8437/deployments \
  -H 'content-type: application/json' \
  -d @/home/steve/workspace/light-fabric/apps/light-gateway/k8s/light-gateway/render-request.json \
  | jq .
```

To deploy, change `"action": "render"` to `"action": "deploy"` in the request
or send an equivalent payload with `action` set to `deploy`.

Check status with the returned `requestId`:

```sh
curl -sS http://127.0.0.1:8437/deployments \
  -H 'content-type: application/json' \
  -d '{
    "requestId": "<request-id>",
    "hostId": "01964b05-552a-7c4b-9184-6857e7f3dc5f",
    "instanceId": "ai-microgateway-rust-dev",
    "environment": "dev",
    "clusterId": "local",
    "namespace": "light-gateway",
    "action": "status",
    "template": {
      "repoUrl": "local",
      "ref": "main",
      "path": "apps/light-gateway/k8s/light-gateway"
    }
  }' | jq .
```

For the live event stream:

```sh
curl -N "http://127.0.0.1:8437/events?requestId=<request-id>"
```

## Verify In MicroK8s

```sh
microk8s kubectl -n light-gateway get deploy,svc,pods
microk8s kubectl -n light-gateway logs deploy/ai-microgateway-rust
microk8s kubectl -n light-gateway get events --sort-by=.metadata.creationTimestamp
```

For local browser or curl access to the `ClusterIP` Service:

```sh
microk8s kubectl -n light-gateway port-forward svc/ai-microgateway-rust 8080:8080 8443:8443
```

Health check:

```sh
curl -i http://127.0.0.1:8080/health
```

## Notes

- Keep the namespace creation separate. The deployer may block cluster-scoped
  resources such as `Namespace`.
- Runtime route and policy config should come from config-server, not from this
  bootstrap template.
- The default probes use HTTP on port `8080`. If you disable HTTP and enable
  HTTPS only, change the probes before deploying.
- The template uses `emptyDir` for `/app/config-cache`; cached config disappears
  when the pod is recreated.
