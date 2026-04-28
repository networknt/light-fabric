# Run Standalone

Standalone mode is the fastest way to test `light-deployer` before using a
real Kubernetes cluster.

Use `noop` mode first. It validates config, HTTP endpoints, template loading,
rendering, resource summaries, and response shape without mutating Kubernetes.

Run all commands from:

```sh
cd /home/steve/workspace/light-fabric
```

## Start With Built-In Sample

Start the deployer with the sample template directory:

```sh
LIGHT_DEPLOYER_TEMPLATE_BASE_DIR=apps/light-deployer/examples/petstore \
LIGHT_DEPLOYER_KUBE_MODE=noop \
cargo run -p light-deployer
```

The service listens on:

```text
http://127.0.0.1:7088
```

Check health from another terminal:

```sh
curl -fsSL http://127.0.0.1:7088/health
```

Expected output:

```text
ok
```

## List Tools With MCP JSON-RPC

The MCP endpoint is JSON-RPC 2.0 over HTTP at:

```text
POST /mcp
```

List all deployment tools:

```sh
curl -fsSL http://127.0.0.1:7088/mcp \
  -H 'content-type: application/json' \
  -d '{
    "jsonrpc": "2.0",
    "id": "tools-list-1",
    "method": "tools/list",
    "params": {}
  }'
```

Call a tool through MCP:

```sh
curl -fsSL http://127.0.0.1:7088/mcp \
  -H 'content-type: application/json' \
  -d '{
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
        "values": {
          "name": "petstore",
          "image": {
            "repository": "nginx",
            "tag": "1.27"
          },
          "containerPort": 80
        },
        "template": {
          "repoUrl": "local",
          "ref": "main",
          "path": "k8s"
        }
      }
    }
  }'
```

For local debugging, the deployer also exposes REST-style convenience
endpoints:

```sh
curl -fsSL http://127.0.0.1:7088/mcp/tools/list
curl -fsSL http://127.0.0.1:7088/mcp/tools
curl -fsSL http://127.0.0.1:7088/mcp/tools/deployment.render
```

Use `POST /mcp` for MCP clients and AI agents.

## Render The Built-In Sample

```sh
curl -fsSL http://127.0.0.1:7088/mcp \
  -H 'content-type: application/json' \
  -d '{
    "jsonrpc": "2.0",
    "id": "render-sample-1",
    "method": "tools/call",
    "params": {
      "name": "deployment.render",
      "arguments": {
        "hostId": "local-host",
        "instanceId": "petstore-dev",
        "environment": "dev",
        "clusterId": "microk8s-local",
        "namespace": "light-deployer",
        "values": {
          "name": "petstore",
          "replicas": 1,
          "image": {
            "repository": "nginx",
            "tag": "1.27"
          },
          "containerPort": 80,
          "service": {
            "port": 80
          }
        },
        "template": {
          "repoUrl": "local",
          "ref": "main",
          "path": "k8s"
        }
      }
    }
  }'
```

Expected response shape:

```json
{
  "jsonrpc": "2.0",
  "result": {
    "isError": false,
    "structuredContent": {
      "action": "render",
      "status": "rendered",
      "deployerId": "local-light-deployer",
      "clusterId": "local",
      "resources": [
        {
          "kind": "Deployment",
          "name": "petstore"
        },
        {
          "kind": "Service",
          "name": "petstore"
        }
      ]
    }
  }
}
```

The exact `requestId` and `manifestHash` will differ.

## Render openapi-petstore Locally

If `/home/steve/workspace/openapi-petstore` is available and has a `k8s/`
folder, run:

```sh
LIGHT_DEPLOYER_TEMPLATE_BASE_DIR=/home/steve/workspace/openapi-petstore \
LIGHT_DEPLOYER_KUBE_MODE=noop \
cargo run -p light-deployer
```

Render request:

```sh
curl -fsSL http://127.0.0.1:7088/mcp \
  -H 'content-type: application/json' \
  -d '{
    "jsonrpc": "2.0",
    "id": "render-openapi-petstore-1",
    "method": "tools/call",
    "params": {
      "name": "deployment.render",
      "arguments": {
        "hostId": "local-host",
        "instanceId": "openapi-petstore-dev",
        "environment": "dev",
        "clusterId": "microk8s-local",
        "namespace": "petstore-dev",
        "values": {
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
        },
        "template": {
          "repoUrl": "local",
          "ref": "master",
          "path": "k8s"
        }
      }
    }
  }'
```

Expected resources:

- `Deployment/openapi-petstore`
- `Service/openapi-petstore`

## Test Git Fetch

Stop the local-template run and restart without `LIGHT_DEPLOYER_TEMPLATE_BASE_DIR`:

```sh
LIGHT_DEPLOYER_KUBE_MODE=noop \
cargo run -p light-deployer
```

Render from GitHub:

```sh
curl -fsSL http://127.0.0.1:7088/mcp \
  -H 'content-type: application/json' \
  -d '{
    "jsonrpc": "2.0",
    "id": "render-git-1",
    "method": "tools/call",
    "params": {
      "name": "deployment.render",
      "arguments": {
        "hostId": "local-host",
        "instanceId": "openapi-petstore-dev",
        "environment": "dev",
        "clusterId": "microk8s-local",
        "namespace": "petstore-dev",
        "values": {
          "name": "openapi-petstore",
          "image": {
            "repository": "networknt/openapi-petstore",
            "tag": "latest"
          }
        },
        "template": {
          "repoUrl": "https://github.com/networknt/openapi-petstore.git",
          "ref": "master",
          "path": "k8s"
        }
      }
    }
  }'
```

For a private repository:

```sh
LIGHT_DEPLOYER_GIT_TOKEN=... \
LIGHT_DEPLOYER_KUBE_MODE=noop \
cargo run -p light-deployer
```

For Bitbucket app-password style auth:

```sh
LIGHT_DEPLOYER_GIT_USERNAME=my-user \
LIGHT_DEPLOYER_GIT_TOKEN=my-app-password \
LIGHT_DEPLOYER_KUBE_MODE=noop \
cargo run -p light-deployer
```

## Dry Run And Diff In Noop Mode

Noop mode can also exercise the request path for these tools:

```sh
curl -fsSL http://127.0.0.1:7088/mcp \
  -H 'content-type: application/json' \
  -d '{
    "jsonrpc": "2.0",
    "id": "dry-run-sample-1",
    "method": "tools/call",
    "params": {
      "name": "deployment.dryRun",
      "arguments": {
        "hostId": "local-host",
        "instanceId": "petstore-dev",
        "environment": "dev",
        "clusterId": "microk8s-local",
        "namespace": "light-deployer",
        "values": {
          "name": "petstore",
          "replicas": 1,
          "image": {
            "repository": "nginx",
            "tag": "1.27"
          },
          "containerPort": 80,
          "service": {
            "port": 80
          }
        },
        "template": {
          "repoUrl": "local",
          "ref": "main",
          "path": "k8s"
        }
      }
    }
  }'
```

```sh
curl -fsSL http://127.0.0.1:7088/mcp \
  -H 'content-type: application/json' \
  -d '{
    "jsonrpc": "2.0",
    "id": "diff-sample-1",
    "method": "tools/call",
    "params": {
      "name": "deployment.diff",
      "arguments": {
        "hostId": "local-host",
        "instanceId": "petstore-dev",
        "environment": "dev",
        "clusterId": "microk8s-local",
        "namespace": "light-deployer",
        "values": {
          "name": "petstore",
          "replicas": 1,
          "image": {
            "repository": "nginx",
            "tag": "1.27"
          },
          "containerPort": 80,
          "service": {
            "port": 80
          }
        },
        "template": {
          "repoUrl": "local",
          "ref": "main",
          "path": "k8s"
        }
      }
    }
  }'
```

These calls do not validate against Kubernetes unless real mode is enabled.

## Stop The Service

Press `Ctrl-C` in the terminal running `cargo run`.
