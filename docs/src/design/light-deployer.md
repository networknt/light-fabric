# Light-Deployer Design

`light-deployer` is the cluster-local Kubernetes deployment executor in
Light Fabric.

This document focuses only on the deployer service that lives in
`apps/light-deployer`. The broader Light Portal deployment workflow, approval
flow, deployment history model, controller routing, and portal UI are covered
outside this repository.

## Purpose

`light-deployer` receives a deployment command, fetches Kubernetes templates,
renders them with deployment values, validates the resulting resources, applies
or deletes resources in the target Kubernetes cluster, and returns safe status
details.

It is intentionally narrow. It does not decide whether a user is allowed to
deploy an instance, does not own portal deployment history, and does not create
tenant business workflows. Those decisions belong to Light Portal,
Light Controller, and the workflow engine.

## Service Boundary

`light-deployer` owns:

- local deployment policy enforcement
- template repository fetch
- YAML template rendering
- manifest parsing and resource summary generation
- Kubernetes dry-run, apply, delete, status, and pruning
- safe event and error reporting
- direct local/MicroK8s deployment endpoints

`light-deployer` does not own:

- tenant authorization
- instance metadata
- deployment approval
- deployment history persistence
- config snapshot creation
- long-running human workflow decisions

The deployer should reject commands outside its local policy even if an
upstream service sends them.

## Runtime Model

The service follows the same runtime pattern as `light-agent`.

`main.rs` builds the domain service and starts it through:

```rust
LightRuntimeBuilder::new(AxumTransport::new(app))
```

The HTTP listener is owned by `light-runtime` and `light-axum`, not by
service-specific socket code. Bind address, HTTP/HTTPS ports, service identity,
and registry settings live in runtime config files.

Default config files:

- `config/server.yml`
- `config/deployer.yml`
- `config/portal-registry.yml`

Local `cargo run` resolves config from `apps/light-deployer/config` when run
from the workspace root. The container image runs from `/app` and uses
`/app/config`.

## Public Endpoints

Phase 1 exposes a direct HTTP surface for local and MicroK8s testing:

```text
GET  /health
GET  /ready
POST /mcp
GET  /mcp/tools
GET  /mcp/tools/list
GET  /mcp/tools/{tool}
POST /deployments
POST /mcp/tools/{tool}
GET  /events?request_id=...
```

`POST /mcp` is the MCP JSON-RPC 2.0 endpoint. It supports `tools/list`,
`tools/call`, and a minimal `initialize` response. This is the endpoint that
MCP clients, Light Portal, and AI agents should use.

`/deployments` accepts the canonical deployment request directly.
`/mcp/tools/{tool}` maps tool names onto the same internal service functions as
a REST-style local debugging convenience. The convenience tool-list endpoints
return metadata with `name`, `description`, `inputSchema`, `endpoint`, and
`method`, but they are not the MCP protocol endpoint.

Supported tool names:

- `deployment.render`
- `deployment.dryRun`
- `deployment.diff`
- `deployment.apply`
- `deployment.delete`
- `deployment.status`
- `deployment.rollback`

The direct HTTP mode is useful for development and managed environments. The
same internal command handling should later be reused by controller-mediated
WebSocket/MCP routing.

## Request Model

A deployment request is explicit and auditable.

```json
{
  "requestId": "01964b05-0000-7000-8000-000000000001",
  "hostId": "01964b05-552a-7c4b-9184-6857e7f3dc5f",
  "instanceId": "petstore-dev",
  "environment": "dev",
  "clusterId": "microk8s-local",
  "namespace": "petstore-dev",
  "action": "deploy",
  "values": {
    "name": "petstore",
    "image": {
      "repository": "networknt/openapi-petstore",
      "tag": "latest"
    }
  },
  "template": {
    "repoUrl": "https://github.com/networknt/openapi-petstore.git",
    "ref": "master",
    "path": "k8s"
  },
  "options": {
    "dryRun": false,
    "waitForRollout": true,
    "timeoutSeconds": 300,
    "pruneOverride": false
  }
}
```

The current implementation supports inline `values`. The request model also
contains fields for future values references and immutable snapshot metadata so
it can align with the full portal deployment workflow.

When invoking a specific `/mcp/tools/{tool}` endpoint, callers do not need to
send `action`. The deployer derives the action from the tool name. The generic
`/deployments` endpoint still expects an explicit action in the request body.

For the MCP endpoint, callers use JSON-RPC:

```json
{
  "jsonrpc": "2.0",
  "id": "tools-list-1",
  "method": "tools/list",
  "params": {}
}
```

Tool invocation uses `tools/call`:

```json
{
  "jsonrpc": "2.0",
  "id": "render-1",
  "method": "tools/call",
  "params": {
    "name": "deployment.render",
    "arguments": {
      "hostId": "local-host",
      "instanceId": "petstore-dev",
      "environment": "dev",
      "clusterId": "local",
      "namespace": "light-deployer",
      "values": {},
      "template": {
        "repoUrl": "local",
        "ref": "main",
        "path": "k8s"
      }
    }
  }
}
```

`tools/call` derives the deployment action from `params.name`; callers should
not provide an `action` field in `arguments`.

## Actions

`render`
: Fetch templates, render manifests, add namespaces and management labels, and
return resource summaries plus a manifest hash.

`dryRun`
: Render manifests and validate them against Kubernetes using server-side
dry-run.

`diff`
: Render manifests, fetch current managed resources, calculate additions,
modifications, and pruned resources, and return a redacted diff summary.

`deploy`
: Accept the request, run the deployment in the background, apply manifests,
prune removed managed resources, and stream events.

`undeploy`
: Delete resources associated with the deployment.

`status`
: Return current managed resource status.

`rollback`
: Reserved for redeploying a previous immutable portal snapshot. Native
Kubernetes rollout undo is not the target rollback model because it does not
restore ConfigMaps, Secrets, or values snapshots.

## Template Fetching

Templates are loaded through the `TemplateSource` trait.

The current source supports two modes:

- local template root through `LIGHT_DEPLOYER_TEMPLATE_BASE_DIR`
- remote HTTPS Git clone through `gix`

For remote repositories, the deployment request provides:

```json
{
  "template": {
    "repoUrl": "https://github.com/networknt/openapi-petstore.git",
    "ref": "master",
    "path": "k8s"
  }
}
```

Private HTTPS Git access is controlled by environment variables:

- `LIGHT_DEPLOYER_GIT_TOKEN`: token or app password
- `LIGHT_DEPLOYER_GIT_USERNAME`: optional username override

Defaults:

- GitHub uses `x-access-token`
- Bitbucket Cloud uses `x-token-auth`

SSH authentication is intentionally deferred because it requires private key
handling and strict `known_hosts` validation.

## Template Format

The built-in renderer uses simple placeholders:

```yaml
image: ${image.repository}:${image.tag:latest}
```

Supported behavior:

- nested paths such as `image.repository`
- default values after `:`
- render failure when a required value is missing
- placeholder replacement only inside YAML string scalar values

The renderer parses YAML into `serde_yaml::Value`, traverses the AST, replaces
placeholders, and serializes or applies structured YAML values afterward. This
avoids the most common raw string replacement bugs around quoting,
indentation, certificates, and multi-line values.

Because placeholders currently produce strings, templates should avoid
placeholders in numeric-only Kubernetes fields unless Kubernetes accepts a
string value there. For example, `containerPort` should be fixed or rendered by
a future typed placeholder extension.

## Resource Metadata

After rendering, the deployer ensures every resource has the target namespace
and adds management labels:

- `app.kubernetes.io/managed-by=light-deployer`
- `lightapi.net/host-id`
- `lightapi.net/instance-id`
- `lightapi.net/request-id`

These labels are used for status lookup and pruning.

## Kubernetes Execution

Kubernetes execution is behind the `KubeExecutor` trait.

Current implementations:

- `KubeRsExecutor`: real Kubernetes API execution through `kube-rs`
- `NoopKubeExecutor`: local render/test mode

Execution mode:

- `LIGHT_DEPLOYER_KUBE_MODE=real`: force real Kubernetes mode
- `LIGHT_DEPLOYER_KUBE_MODE=noop`: force no-op mode
- default: real mode when `KUBERNETES_SERVICE_HOST` is present, otherwise no-op

The production path uses `kube-rs`, not `kubectl`.

Kubernetes operations should use:

- in-cluster ServiceAccount auth when running as a pod
- server-side dry-run for validation
- server-side apply with field manager `light-deployer`
- structured status and error handling

## Pruning

The deployer is declarative. If a previously managed resource is no longer
rendered from the template, it should be considered for pruning.

Pruning is calculated by comparing:

- current resources in the namespace with `lightapi.net/instance-id`
- resources rendered from the new template

The policy layer enforces blast-radius protection:

- maximum delete percentage
- sensitive kinds requiring override
- explicit `pruneOverride` in deployment options

This prevents stale resources while still protecting against accidental
large-scale deletion.

## Policy

The local `deployer.yml` policy constrains what a deployer is allowed to do.

Policy dimensions:

- allowed namespaces
- allowed repository hosts
- allowed repository URL prefixes
- allowed image registries
- allowed actions
- allowed Kubernetes kinds
- blocked Kubernetes kinds
- prune settings
- development insecure mode

Version 1 allows application-level resource kinds by default:

- `Deployment`
- `Service`
- `Ingress`
- `ConfigMap`
- `Secret`

Cluster-scoped and control-plane resources are blocked by default:

- `Namespace`
- `ClusterRole`
- `ClusterRoleBinding`
- `CustomResourceDefinition`
- admission webhooks

## Security

The deployer can mutate a Kubernetes cluster, so its default posture must be
conservative.

Required practices:

- run in Kubernetes with a dedicated ServiceAccount
- prefer namespace-scoped `Role` and `RoleBinding`
- restrict allowed namespaces and resource kinds
- restrict template repository hosts or prefixes in production
- restrict image registries in production
- never log raw rendered Secret manifests
- never log raw Kubernetes patch/apply payloads containing Secret data
- return redacted summaries and diffs

Secret values in rendered manifests are redacted before being included in
responses or diffs. Kubernetes Secret values are base64 encoded, not encrypted,
so they must be treated as plaintext for logging purposes.

## Response Model

Responses include enough detail for callers to understand what happened without
exposing secrets.

Important fields:

- `requestId`
- `action`
- `status`
- `deployerId`
- `clusterId`
- `namespace`
- `manifestHash`
- `templateCommitSha`
- `resources`
- `diff`
- `events`
- `error`

Resource summaries contain kind, namespace, name, apiVersion, and action. Full
rendered manifests should not be returned or persisted by default.

## Event Model

Long-running operations return quickly and continue in the background.

Clients can subscribe to:

```text
GET /events?request_id=...
```

Events contain:

- request ID
- timestamp
- status
- message
- optional resource identity

The event stream is currently direct SSE. Controller-mediated mode can forward
the same event shape later.

## Installation

The app includes Kubernetes install manifests under `apps/light-deployer/k8s`:

- namespace
- RBAC
- deployment
- service

The deployment runs the container with `LIGHT_DEPLOYER_KUBE_MODE=real`. The
image contains `/app/config`, and `server.yml` defaults the HTTP port to 7088.

For MicroK8s testing:

```sh
./apps/light-deployer/build.sh latest
docker save networknt/light-deployer:latest | microk8s ctr image import -
microk8s kubectl apply -f apps/light-deployer/k8s/namespace.yaml
microk8s kubectl apply -f apps/light-deployer/k8s/rbac.yaml
microk8s kubectl apply -f apps/light-deployer/k8s/deployment.yaml
microk8s kubectl apply -f apps/light-deployer/k8s/service.yaml
```

## Current Limitations

- Direct HTTP/MCP-style mode is implemented first; controller-mediated
  WebSocket routing is a later integration step.
- Inline values are implemented; config-server `valuesRef` fetching is still a
  future integration point.
- Rollback is represented in the model but needs portal snapshot integration.
- Helm and Kustomize are not implemented yet.
- Typed placeholders are not implemented yet.
- Rollout watch depth is intentionally basic in the first phase.

## Design Direction

Keep `light-deployer` small and cluster-local.

The deployer should execute precise deployment commands, enforce local safety
policy, and report structured results. It should not grow into a portal,
workflow engine, or deployment database. That separation keeps the service easy
to install inside customer clusters and reduces the security blast radius.
