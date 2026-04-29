# light-deployer

`light-deployer` is the cluster-local deployment executor for Light Portal.

This initial Phase 1 slice provides:

- health/readiness endpoints
- direct MCP-style tool endpoints under `/mcp/tools/{tool}`
- deployment request model
- AST-based YAML placeholder rendering
- secret and sealed-secret redaction
- redacted diff scaffolding
- `gix` template repository fetch for remote Git templates
- `kube-rs` server-side apply, dry-run, delete, status, and prune support
- pruning calculation and blast-radius policy
- SSE deployment events under `/events?request_id=...`

The app follows the same runtime pattern as `light-agent`: `light-runtime`
owns startup, server binding, config loading, and optional portal registry
registration through `light-axum`.

## Deploy To MicroK8s

Build the image from the repository root:

```sh
./apps/light-deployer/build.sh latest
```

Import the local Docker image into MicroK8s containerd:

```sh
docker save networknt/light-deployer:latest | microk8s ctr image import -
```

If MicroK8s requires elevated permissions:

```sh
docker save networknt/light-deployer:latest | sudo microk8s ctr image import -
```

Apply the manifests:

```sh
microk8s kubectl apply -f apps/light-deployer/k8s/namespace.yaml
microk8s kubectl apply -f apps/light-deployer/k8s/rbac.yaml
microk8s kubectl apply -f apps/light-deployer/k8s/deployment.yaml
microk8s kubectl apply -f apps/light-deployer/k8s/service.yaml
```

Check status and logs:

```sh
microk8s kubectl -n light-deployer get pods
microk8s kubectl -n light-deployer logs deploy/light-deployer
```

Port-forward for local testing:

```sh
microk8s kubectl -n light-deployer port-forward svc/light-deployer 7088:7088
```

Test health:

```sh
curl http://127.0.0.1:7088/health
```

## Run Locally

```sh
LIGHT_DEPLOYER_TEMPLATE_BASE_DIR=/path/to/sample-repo \
LIGHT_DEPLOYER_DEV_INSECURE=true \
cargo run -p light-deployer
```

By default, local `cargo run` loads config from `apps/light-deployer/config`.
The container image runs from `/app` and loads `/app/config`. Override the
config directory with `LIGHT_DEPLOYER_CONFIG_DIR` when needed. The HTTP port is
defined in `config/server.yml` and can be overridden with `server.httpPort`.

For the included sample:

```sh
LIGHT_DEPLOYER_TEMPLATE_BASE_DIR=apps/light-deployer/examples/petstore \
LIGHT_DEPLOYER_KUBE_MODE=noop \
cargo run -p light-deployer
```

Without `LIGHT_DEPLOYER_TEMPLATE_BASE_DIR`, the deployer clones
`template.repoUrl` with `gix` into a temporary work directory, checks out
`template.ref`, and reads YAML files from `template.path`.

For private HTTPS Git repositories, set a repository-scoped token:

```sh
LIGHT_DEPLOYER_GIT_TOKEN=... cargo run -p light-deployer
```

GitHub defaults to the `x-access-token` username. Bitbucket Cloud defaults to
`x-token-auth`. For Bitbucket app passwords or other Git servers, set
`LIGHT_DEPLOYER_GIT_USERNAME` explicitly.

Phase 1 intentionally supports HTTPS token auth only. SSH auth is deferred
because it requires secure private key and `known_hosts` management.

## Example Tool Call

```sh
curl -s http://localhost:7088/mcp/tools/deployment.render \
  -H 'content-type: application/json' \
  -d '{
    "hostId": "host",
    "instanceId": "petstore-dev",
    "environment": "dev",
    "clusterId": "local",
    "namespace": "petstore-dev",
    "action": "render",
    "values": { "name": "petstore", "image": { "repository": "petstore", "tag": "latest" } },
    "template": { "repoUrl": "local", "ref": "main", "path": "k8s" }
  }'
```

The same request is available at
`apps/light-deployer/examples/petstore/render-request.json`.

## Real Kubernetes Mode

Inside a Kubernetes cluster, `light-deployer` automatically uses the in-cluster
ServiceAccount through `kube-rs`.

Outside a cluster, force real Kubernetes mode with:

```sh
LIGHT_DEPLOYER_KUBE_MODE=real cargo run -p light-deployer
```

For local render-only testing, use:

```sh
LIGHT_DEPLOYER_KUBE_MODE=noop cargo run -p light-deployer
```

The sample Kubernetes manifests deploy into the `light-deployer` namespace so
they work with the namespace-scoped example RBAC.
