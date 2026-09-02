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
# Host Registration

The Portal records a version-2, Host-scoped registration for a database that
already exists. It does not create a database, run a provider, rotate database
credentials, or poll a provisioning queue. A deployment supplies the database
URL through a restricted local file; Config Server receives only non-secret
connection metadata and the file reference.

Bundle `2.0.0` migrates the operational database metadata row from the retired
Host/environment provider profile to `scope_kind=HOST`,
`deployment_profile=CUSTOMER_MANAGED`, and `binding_version>=2`. Environment
remains runtime-instance routing metadata, not part of database ownership.
