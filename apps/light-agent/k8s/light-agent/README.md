# Light Agent Kubernetes Template

This folder deploys the Rust `light-agent` from `light-fabric` as an account
agent pod for local MicroK8s.

The agent uses local bootstrap config from `/config`, downloads runtime config
from config-server into `/app/config-cache`, connects to Postgres for Hindsight
memory, calls MCP tools through `light-gateway`, serves the chat UI from
`/app/public`, and registers with controller.

The default `name` is `light-agent-account` and the default namespace is
`light-agent`.

## Files

- `configmap.yaml`: bootstrap `startup.yml`. The image carries the default
  config templates under `/app/config-defaults`.
- `secret.yaml`: portal bearer token, config password, host id, database URL,
  and bootstrap CA certificate.
- `public-configmap.yaml`: static chat UI mounted at `/app/public`.
- `deployment.yaml`: Rust agent container, bootstrap config, public UI, and
  writable `config-cache`.
- `service.yaml`: `ClusterIP` Service for in-cluster access.
- `render-request.json`: example light-deployer request.

No `Namespace` manifest is included. Create or reuse the namespace separately.

## Build And Import For MicroK8s

From `/home/steve/workspace/light-fabric`:

```sh
./apps/light-agent/build.sh agent-local --local
docker save networknt/light-agent:agent-local | microk8s ctr image import -
```

The template defaults to:

```yaml
image.repository: networknt/light-agent
image.tag: agent-local
image.pullPolicy: IfNotPresent
```

Use a unique local tag each time you need to force MicroK8s to use a rebuilt
image, or import the rebuilt image before redeploying.

## Required Values

- `lightPortalAuthorization`: bearer token for config-server bootstrap and
  controller registration.
- `bootstrapCaPemBase64`: base64 content of the CA PEM used to trust
  config-server, controller, and the gateway.
- `lightAgentHostId`: UUID used by Hindsight memory tables.
- `database.url`: Postgres connection string for the configserver database.
- `configServer.uri`: config-server base URL.
- `startup.host`, `startup.serviceId`, and `startup.envTag`: the tuple used to
  fetch runtime config from config-server.

The config-server `values.yml` for the selected startup tuple should provide
the runtime overrides for `server.*`, `portalRegistry.*`, `client.*`,
`mcp-client.*`, `model-provider.*`, and the selected provider namespace such as
`ollama.*`, `bedrock.*`, `codex.*`, `openai.*`, or `anthropic.*`.

Supported `model-provider.provider` values are `ollama`, `openai`,
`azure-openai`, `anthropic`, `bedrock`, `codex`, `compatible`, `gemini`,
`glm`, `openrouter`, `telnyx`, `copilot`, `claude-code`, `gemini-cli`, and
`kilo-cli`.

Generate the CA value:

```sh
base64 -w0 /path/to/ca.pem
```

Do not commit real bearer tokens, database passwords, or customer CA material
into this template.

## Local Defaults

The example request assumes:

- config-server is reachable from the cluster at `https://192.168.5.85:8435`.
- controller is reachable as `https://controller:8438`.
- Java `light-gateway` is already deployed as
  `https://ai-microgateway.light-gateway:8443`.
- Ollama is reachable from the cluster at `http://192.168.5.85:11434`.
- Postgres is reachable from the cluster at `192.168.5.85:5432`.

Update `render-request.json` for bootstrap-only values such as
`configServer.uri`. Put controller, gateway, Ollama, and server registration
overrides in config-server `values.yml` for the selected startup tuple. If
using the Rust gateway template beside Java, set `mcp-client.gatewayUrl` there
to the Rust Service, for example
`https://ai-microgateway-rust.light-gateway:8443`.

## Registration Address

The agent registers `server.advertisedAddress` from config-server values.

For local MicroK8s in-cluster access, use the Service DNS name:

```yaml
server.advertisedAddress: light-agent-account.light-agent
```

If you change `name` or `namespace`, update `server.advertisedAddress` in
config-server values to match:

```text
<service-name>.<namespace>
```

If controller or clients are outside the cluster, use the externally reachable
DNS name instead.

## Deploy With Light-Deployer

Create the namespace if needed:

```sh
microk8s kubectl create namespace light-agent
```

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
  -d @/home/steve/workspace/light-fabric/apps/light-agent/k8s/light-agent/render-request.json \
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
    "instanceId": "light-agent-account-dev",
    "environment": "dev",
    "clusterId": "local",
    "namespace": "light-agent",
    "action": "status",
    "template": {
      "repoUrl": "local",
      "ref": "main",
      "path": "apps/light-agent/k8s/light-agent"
    }
  }' | jq .
```

For the live event stream:

```sh
curl -N "http://127.0.0.1:8437/events?requestId=<request-id>"
```

## Verify In MicroK8s

```sh
microk8s kubectl -n light-agent get deploy,svc,pods
microk8s kubectl -n light-agent logs deploy/light-agent-account
microk8s kubectl -n light-agent get events --sort-by=.metadata.creationTimestamp
```

For local browser or curl access to the `ClusterIP` Service:

```sh
microk8s kubectl -n light-agent port-forward svc/light-agent-account 8083:8083
```

Health check:

```sh
curl -i http://127.0.0.1:8083/health
```

Open the chat UI at:

```text
http://127.0.0.1:8083/
```

## Notes

- Keep namespace creation separate. The deployer may block cluster-scoped
  resources such as `Namespace`.
- The agent layers config files in this order: bundled `/app/config-defaults`,
  mounted `/config`, and writable `/app/config-cache`.
- The current light-agent image does not include `public/`, so this template
  mounts `public-configmap.yaml` at `/app/public`.
- The default probes use HTTP on port `8083`. If you disable HTTP and enable
  HTTPS only, change the probes before deploying.
- The template uses `emptyDir` for `/app/config-cache`; cached config disappears
  when the pod is recreated.
