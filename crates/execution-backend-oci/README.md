# Docker and rootless OCI execution backend

This backend is intended for credential-free, deny-egress, ephemeral execution.
Images must be referenced by digest. The container root filesystem is read-only,
`/workspace` is an isolated bounded tmpfs, Linux capabilities are dropped, and
staged inputs are mounted read-only and `noexec`.

The Docker profile reports a `container` boundary. The rootless Podman profile
reports `user-namespace`. Both report `explicit-mounts` host exposure and must
not satisfy a microVM, remote-sandbox, or dedicated-VM policy.

Docker and Podman do not provide the Cube-style native lease TTL used by this
integration. The runner watchdog owns deadline enforcement. Operations use the
deterministic `light-<execution-id>` name, making prepare and cleanup
idempotent and allowing deployment reconcilers to discover stale resources by
name and the `light.execution` label. Production enablement must include a
host-level orphan sweeper for those resources and separate capacity controls.

Run the live shared conformance gate with a locally available pinned image:

```bash
LIGHT_OCI_CONFORMANCE_IMAGE='repository/image@sha256:...' \
  cargo test -p execution-backend-oci --test docker_conformance -- --ignored
```

