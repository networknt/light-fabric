# light-a2a deployment profiles

Both profiles run the same `light-a2a` binary and load only an activated Config
Server snapshot identified by `host`, `serviceId`, and `envTag`.

- `sidecar` shares one network namespace with one external business agent. The
  backend listens only on loopback and receives the mounted backend-context key.
- `shared` runs the integration service independently for approved profiles.
  A local backend still needs process or pod colocation; the profile never
  relaxes the fixed-loopback backend contract into a private-network protocol.

The Compose labels are Controller registration metadata for deployment
inventory. Runtime registration continues through `portal-registry.yml`; no
backend URL or key material is registered with the Controller.

Phase 7 health checks use `/_a2a/ready`. It remains unavailable when the
immutable projection has expired or when an enabled push-delivery worker has
not completed a database poll within its maximum lease window. Liveness at
`/health` intentionally does not claim that policy or delivery authority is
ready.

## Image build

From the light-fabric workspace root, run
`./apps/light-a2a/build.sh VERSION --local` to build without publishing. The root
`./build.sh VERSION` includes this image in the default release. The Dockerfile
builds the locked Rust workspace with the canonical backend contract and runs
as UID/GID 999, matching the protected local operational-store secret mounts.
It includes curl for readiness and writable configuration-cache/artifact paths.

Select `LIGHT_A2A_IMAGE=networknt/light-a2a:VERSION` in the deployment image env
file. Building an image does not activate a workload: publish an external A2A
binding and activate the resulting Config Server snapshot before deploying.
