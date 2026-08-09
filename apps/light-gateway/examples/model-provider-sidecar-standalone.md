# Standalone Model Provider Sidecar

Generate the profile rather than editing `handler.yml` by hand:

```bash
cargo run -p light-gateway --bin model-provider-sidecar-profile -- \
  apps/light-gateway/k8s/model-provider-sidecar/profile-request.json ./generated-sidecar
```

Run the model runtime under a dedicated operating-system identity and bind it
only to `127.0.0.1`. Run Light Gateway under a different identity, install the
complete generated configuration bundle, mount the declared JWT trust CA and
TLS serving key paths, and publish only the sidecar listener. Do not use
`hostNetwork`, a wildcard runtime bind, a host
port, or a second process with access to the sidecar runtime credential.

The generated non-empty default chain terminates locally with
`sidecar-deny`. A binary predating the terminal `sidecar-deny` and
`sidecar-identity` handlers rejects the profile at boot because unknown handler
IDs fail configuration loading.
