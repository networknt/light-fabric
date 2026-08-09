# Model-provider sidecar profile

Generate the immutable profile with:

```bash
cargo run -p light-gateway --bin model-provider-sidecar-profile -- \
  apps/light-gateway/k8s/model-provider-sidecar/profile-request.json ./generated-sidecar
```

Install the generated files as the `ollama-embedding-sidecar-profile`
ConfigMap and record their manifest digests on the Provider Deployment before
live qualification.

The model process must bind loopback. Containers in a Pod share one network
namespace, so NetworkPolicy cannot mediate the sidecar-to-runtime hop. The
boundary depends on `hostNetwork: false`, the runtime's `127.0.0.1` bind, no raw
runtime Service or host port, and exactly the two reviewed containers shown in
the Deployment. The sidecar operations/metrics listener is intentionally
deferred; health and identity are exact paths on the protected data listener.
