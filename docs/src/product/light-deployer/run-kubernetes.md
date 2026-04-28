# Run Kubernetes

This page runs `light-deployer` inside MicroK8s and uses the in-cluster
ServiceAccount with `kube-rs`.

## Prerequisites

MicroK8s should be running and `microk8s kubectl` should work:

```sh
microk8s status --wait-ready
microk8s kubectl get nodes
```

Build the image first:

```sh
cd /home/steve/workspace/light-fabric
./apps/light-deployer/build.sh latest
```

## Import Image Into MicroK8s

```sh
docker save networknt/light-deployer:latest | microk8s ctr image import -
```

If your MicroK8s install requires elevated permissions:

```sh
docker save networknt/light-deployer:latest | sudo microk8s ctr image import -
```

Verify the image is available:

```sh
microk8s ctr images ls | grep light-deployer
```

## Install Deployer

Apply the included manifests:

```sh
microk8s kubectl apply -f apps/light-deployer/k8s/namespace.yaml
microk8s kubectl apply -f apps/light-deployer/k8s/rbac.yaml
microk8s kubectl apply -f apps/light-deployer/k8s/deployment.yaml
microk8s kubectl apply -f apps/light-deployer/k8s/service.yaml
```

Wait for the pod:

```sh
microk8s kubectl -n light-deployer rollout status deploy/light-deployer
microk8s kubectl -n light-deployer get pods
```

Check logs:

```sh
microk8s kubectl -n light-deployer logs deploy/light-deployer
```

The deployment sets:

```text
LIGHT_DEPLOYER_KUBE_MODE=real
```

So the service uses real Kubernetes API calls from inside the cluster.

## Port Forward

```sh
microk8s kubectl -n light-deployer port-forward svc/light-deployer 7088:7088
```

In another terminal:

```sh
curl -fsSL http://127.0.0.1:7088/health
```

Expected:

```text
ok
```

## List Tools

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

The response contains the deployer's tool names, descriptions, input schemas,
and invocation metadata. Light Portal can use this JSON-RPC response to
populate MCP tools for the API details view.

## Render In Kubernetes

Rendering does not mutate the cluster:

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

## Dry Run In Kubernetes

Dry-run renders the manifest and asks the Kubernetes API to validate it without
persisting resources:

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

Expected status:

```json
{
  "jsonrpc": "2.0",
  "result": {
    "isError": false,
    "structuredContent": {
      "status": "validated"
    }
  }
}
```

## Deploy Sample

The sample request deploys into the `light-deployer` namespace so it matches
the included namespace-scoped RBAC.

```sh
curl -fsSL http://127.0.0.1:7088/mcp \
  -H 'content-type: application/json' \
  -d '{
    "jsonrpc": "2.0",
    "id": "apply-sample-1",
    "method": "tools/call",
    "params": {
      "name": "deployment.apply",
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

The response should return quickly with an accepted/applying-style status. The
operation continues in the deployer.

Watch Kubernetes resources:

```sh
microk8s kubectl -n light-deployer get deploy,svc,pods
```

## Stream Events

Use the `requestId` from the deployment response:

```sh
curl -N "http://127.0.0.1:7088/events?request_id=<requestId>"
```

The event stream reports deployment progress and failures for that request.

## Check Status

```sh
curl -fsSL http://127.0.0.1:7088/mcp \
  -H 'content-type: application/json' \
  -d '{
    "jsonrpc": "2.0",
    "id": "status-sample-1",
    "method": "tools/call",
    "params": {
      "name": "deployment.status",
      "arguments": {
        "hostId": "local-host",
        "instanceId": "petstore-dev",
        "environment": "dev",
        "clusterId": "microk8s-local",
        "namespace": "light-deployer",
        "template": {
          "repoUrl": "local",
          "ref": "main",
          "path": "k8s"
        }
      }
    }
  }'
```

## Undeploy Sample

```sh
curl -fsSL http://127.0.0.1:7088/mcp \
  -H 'content-type: application/json' \
  -d '{
    "jsonrpc": "2.0",
    "id": "delete-sample-1",
    "method": "tools/call",
    "params": {
      "name": "deployment.delete",
      "arguments": {
        "hostId": "local-host",
        "instanceId": "petstore-dev",
        "environment": "dev",
        "clusterId": "microk8s-local",
        "namespace": "light-deployer",
        "template": {
          "repoUrl": "local",
          "ref": "main",
          "path": "k8s"
        }
      }
    }
  }'
```

Then verify resources:

```sh
microk8s kubectl -n light-deployer get deploy,svc,pods
```

## Deploy openapi-petstore From Git

After the `openapi-petstore` repository has a `k8s/` folder committed, use a
request like this:

```sh
curl -fsSL http://127.0.0.1:7088/mcp \
  -H 'content-type: application/json' \
  -d '{
    "jsonrpc": "2.0",
    "id": "apply-openapi-petstore-1",
    "method": "tools/call",
    "params": {
      "name": "deployment.apply",
      "arguments": {
        "hostId": "local-host",
        "instanceId": "openapi-petstore-dev",
        "environment": "dev",
        "clusterId": "microk8s-local",
        "namespace": "light-deployer",
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

For private Git access, set `LIGHT_DEPLOYER_GIT_TOKEN` on the deployer pod.
In Kubernetes this should be injected from a Secret, not written directly into
the deployment manifest.

## Update The Deployer Image

After rebuilding locally:

```sh
./apps/light-deployer/build.sh latest
docker save networknt/light-deployer:latest | microk8s ctr image import -
microk8s kubectl -n light-deployer rollout restart deploy/light-deployer
microk8s kubectl -n light-deployer rollout status deploy/light-deployer
```

## Remove The Deployer

```sh
microk8s kubectl delete -f apps/light-deployer/k8s/service.yaml
microk8s kubectl delete -f apps/light-deployer/k8s/deployment.yaml
microk8s kubectl delete -f apps/light-deployer/k8s/rbac.yaml
microk8s kubectl delete -f apps/light-deployer/k8s/namespace.yaml
```
