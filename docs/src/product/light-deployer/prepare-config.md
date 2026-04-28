# Prepare Config

`light-deployer` uses two kinds of configuration:

- runtime config loaded by `light-runtime`
- deployment request data sent through MCP `tools/call` at `POST /mcp`

## Runtime Config Files

Default config lives in:

```text
apps/light-deployer/config
```

Files:

- `server.yml`: HTTP/HTTPS bind settings and service identity
- `deployer.yml`: local deployer policy
- `portal-registry.yml`: future portal/controller registry settings

When running from the workspace root, the deployer automatically uses:

```text
apps/light-deployer/config
```

When running inside the Docker image, it uses:

```text
/app/config
```

Override the config directory with:

```sh
LIGHT_DEPLOYER_CONFIG_DIR=/path/to/config
```

## Server Config

The default server config listens on HTTP port `7088`:

```yaml
ip: ${server.ip:0.0.0.0}
httpPort: ${server.httpPort:7088}
enableHttp: ${server.enableHttp:true}
enableHttps: ${server.enableHttps:false}
serviceId: ${server.serviceId:com.networknt.light-deployer-0.1.0}
enableRegistry: ${server.enableRegistry:false}
```

To change the port without editing the file, provide values through the normal
runtime values mechanism, or use a copied config directory for local testing.

## Deployer Policy

The default policy is permissive enough for local testing:

```yaml
deployerId: ${deployer.deployerId:local-light-deployer}
clusterId: ${deployer.clusterId:local}
allowedNamespaces: []
allowedRepoHosts: []
allowedRepoPrefixes: []
allowedImageRegistries: []
devInsecure: ${deployer.devInsecure:false}
```

Empty allow lists mean the policy does not restrict that dimension. For
production, configure explicit values.

Example tighter policy:

```yaml
deployerId: petstore-microk8s
clusterId: microk8s-local
allowedNamespaces:
  - petstore-dev
allowedRepoHosts:
  - github.com
allowedRepoPrefixes:
  - https://github.com/networknt/
allowedImageRegistries:
  - networknt
devInsecure: false
prune:
  enabled: true
  maxDeletePercent: 30
  sensitiveKinds:
    - PersistentVolumeClaim
  overrideRequired: true
```

## Git Access

Public repositories do not need credentials.

For private HTTPS repositories, set:

```sh
LIGHT_DEPLOYER_GIT_TOKEN=...
```

Defaults:

- GitHub username: `x-access-token`
- Bitbucket Cloud username: `x-token-auth`

For Bitbucket app passwords or other Git servers:

```sh
LIGHT_DEPLOYER_GIT_USERNAME=my-user
LIGHT_DEPLOYER_GIT_TOKEN=my-token-or-app-password
```

Only HTTPS token auth is supported in Phase 1. SSH auth is deferred.

## Template Repository Requirements

The target application repository should contain a `k8s/` directory with YAML
templates. The deployer reads all `.yaml` and `.yml` files under the requested
template path.

Example template reference:

```json
{
  "template": {
    "repoUrl": "https://github.com/networknt/openapi-petstore.git",
    "ref": "master",
    "path": "k8s"
  }
}
```

For local testing without Git clone, set:

```sh
LIGHT_DEPLOYER_TEMPLATE_BASE_DIR=/home/steve/workspace/openapi-petstore
```

Then use:

```json
{
  "template": {
    "repoUrl": "local",
    "ref": "master",
    "path": "k8s"
  }
}
```

## Request Values

The request `values` object supplies placeholder values for templates.

Example for `openapi-petstore`:

```json
{
  "name": "openapi-petstore",
  "image": {
    "repository": "networknt/openapi-petstore",
    "tag": "latest",
    "pullPolicy": "IfNotPresent"
  },
  "service": {
    "name": "openapi-petstore",
    "type": "ClusterIP"
  },
  "resources": {
    "requests": {
      "memory": "64Mi",
      "cpu": "250m"
    },
    "limits": {
      "memory": "256Mi",
      "cpu": "500m"
    }
  }
}
```

The current renderer replaces placeholders inside YAML string scalar values.
Avoid placeholders in Kubernetes fields that must be numeric unless the
template keeps those fields as fixed numbers.
