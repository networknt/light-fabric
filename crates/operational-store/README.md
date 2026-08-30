# Operational Store

This crate owns the Phase 1 PostgreSQL metadata contract for a Host/environment
operational database. Service-owned tables remain in their runtime crates:
`execution-store` joined bundle `1.1.0`, and `agent-store` joins bundle `1.2.0`
with the frozen Agent and embedded-memory cutover set. Bundle `1.3.0` adds
Workflow and native/external A2A authority. Bundle `1.4.0` adds the bounded
Gateway evidence spool plus tenant audit and artifact-metadata stores.

The canonical migration source is `migrations/metadata-postgres`. Deployment
bundles copy these files without editing them and verify both the bundle and
per-migration SHA-256 digests before application. The scripts under
`deployment` create/adopt the database, validate its scope binding, generate a
permission-restricted local URL file, and support the explicit empty-database
development fallback.

Phase 6 uses separate runtime roles and URL files for `gateway_ops`,
`audit_ops`, and `artifact_ops`. Artifact bytes never enter PostgreSQL; the
artifact store persists immutable digest, owner/relationship, object reference,
scan, retention, hold, and tombstone evidence. Gateway traffic is retained in
PostgreSQL only while awaiting delivery to an approved external sink. Reset
scripts are development-only and require an exact `OPERATIONAL_RESET_CONFIRM`
value for the schema being cleared.
# Managed Development Provisioning

Phase 7 keeps Host identity creation independent from operational-store
provisioning. The `operational-store-provisioner.sh` worker claims the Portal
control-plane job projection with a lease and reconciles a dedicated
PostgreSQL 17 container for each additional Host/environment binding. Provider
resources are discovered by immutable binding/Host/environment labels before
creation, so a retry cannot create a second logical store.

The development profile intentionally supports only `DEV_DEDICATED`.
`DEV_POOLED` remains inactive until the complete cross-Host query, cache,
outbox, artifact, export, and erasure suite has passed.

Required worker inputs are secret-file paths for the Portal control database
URL and service token, the Portal command endpoint, an existing Docker network,
the canonical migration bundle, and a permission-restricted provisioning
secret root. For example:

```bash
PORTAL_CONTROL_DATABASE_URL_FILE=/run/secrets/portal-control-database-url \
PORTAL_COMMAND_TOKEN_FILE=/run/secrets/operational-provisioner-token \
PORTAL_COMMAND_URL=https://light-gateway:8443/portal/command \
OPERATIONAL_DOCKER_NETWORK=portal-dev_default \
OPERATIONAL_BUNDLE_ROOT="$PWD/release/bundle" \
OPERATIONAL_PROVISIONING_SECRET_ROOT=/var/lib/lightapi/operational-bindings \
./deployment/operational-store-provisioner.sh
```

The worker never puts a database URL or password in an event, projection, or
Config Server property. Portal stores a logical `secretRef`; deployment-owned
files under the binding secret root are mounted into the selected runtime.
Credential rotation replaces service credentials without changing the
binding. Deactivation stops the dedicated container but preserves its data.
Decommission also preserves the volume by default; physical destruction is a
separate retention-approved operation.
