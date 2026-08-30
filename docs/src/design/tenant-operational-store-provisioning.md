# Tenant Operational Store Provisioning

## Status

Proposed.

This design defines how Light Portal and standalone Light-Fabric deployments
create, bind, validate, rotate, and retire the operational stores used by
gateways, agents, A2A adapters, workflows, audit publishers, and artifact
metadata.

It complements:

- [Control Plane And Operational Data](control-plane-operational-data.md);
- [Database Design](database-design.md);
- [Hindsight Memory](hindsight-memory.md);
- [Light-Agent Execution](light-agent-execution.md);
- [Light-Workflow Runner](light-workflow-runner.md); and
- [A2A Gateway](../product/light-gateway/a2a-gateway.md).

## Decision

Creating a Host and provisioning its operational storage are related user
operations but separate backend state machines.

- The existing `createHost` command continues to create the Host identity
  through the Portal event-sourced control plane.
- An operational-store binding is a separate, versioned control-plane
  aggregate. It identifies a host, environment, deployment profile, service
  schemas, secret reference, expected database identity, and lifecycle state.
- Database and schema creation is performed asynchronously by a privileged,
  retryable provisioning worker. The Host command handler never runs database
  DDL and never waits for infrastructure creation.
- Host Admin may present Host creation and storage selection as one wizard.
  Completion of the Host command means that the Host identity exists; it does
  not falsely report that an operational store is ready.
- Portal-managed deployments can create storage automatically. In a
  customer-managed deployment, the customer can create the database first and
  provide a secret-backed binding for Portal to validate and activate.
- A Portal-managed operational schema is never created in the Config Server
  database. Config Server stores control-plane projections and publishes the
  accepted binding, not operational rows or database credentials.
- Only a binding in `READY` state is eligible for publication to runtime
  instances.
- Host deactivation stops new work but does not delete the operational store.
  Destruction is a separate, authorized, retention-aware workflow.

The recommended deployment defaults are:

| Deployment | Default storage profile | Who provisions it? |
| --- | --- | --- |
| Light Portal cloud | `MANAGED_POOLED` operational cluster with service-owned schemas and tenant-scoped rows | Portal provisioning service |
| Enterprise Portal | `MANAGED_DEDICATED` database per Host and environment | Portal provisioning service or an enterprise infrastructure provider |
| Enterprise bring-your-own database | `CUSTOMER_MANAGED` database per Host and environment | Customer; Portal validates and binds it |
| Standalone Light-Fabric | Local or customer-managed operational database | Deployment scripts or operator |

`MANAGED_DEDICATED` is the preferred enterprise isolation boundary. Pooled
storage is an explicit cloud or high-density deployment profile; it is not an
excuse to weaken tenant predicates, database roles, retention, or audit
evidence.

## Current State

The current Host flow creates control-plane identity only:

1. Portal View's `HostAdmin` page collects Host fields;
2. `host-command` handles `createHost`;
3. the handler assigns or accepts the Host identifier and emits the Host
   creation event; and
4. Host projections become available to the rest of Portal.

There is no operational-store provisioning contract in that command. Adding
database DDL directly to it would combine two incompatible failure models:

| Host identity creation | Infrastructure provisioning |
| --- | --- |
| Event-sourced command | External side effect |
| Atomic event append | Multi-step and provider-dependent |
| Fast and deterministic | Slow, retryable, and sometimes manual |
| Replayed to rebuild projections | Must be idempotently reconciled, not replayed as raw DDL |

PostgreSQL `CREATE DATABASE` also requires suitable privilege and cannot run
inside a transaction block. These constraints reinforce the architectural
boundary; they are not the primary reason for it. See the PostgreSQL
[`CREATE DATABASE`](https://www.postgresql.org/docs/current/sql-createdatabase.html)
documentation.

## Goals

- Preserve the existing Host command and event contract.
- Give Host Admin one understandable workflow for Host identity and storage.
- Support managed, customer-managed, cloud-pooled, and standalone operation.
- Keep infrastructure credentials out of Host records, Portal events, Config
  Server snapshots, and downloaded `values.yml` files.
- Bind every ordinary operational store to an explicit Host and environment.
- Give each service an owned schema and least-privilege runtime role.
- Make provisioning idempotent, observable, retryable, and safe to resume.
- Publish only validated bindings to the exact runtime audience that needs
  them.
- Support credential rotation, schema migration, backup, restore, export,
  residency, retention, and decommissioning.
- Avoid a database per logical agent and avoid gateway ownership of agent or
  workflow state.

## Non-Goals

- Do not turn Portal View into a database administration client.
- Do not grant database-creation privilege to `light-agent`, `light-a2a`,
  `light-gateway`, or `light-workflow`.
- Do not store raw connection URLs, passwords, client certificates, encryption
  keys, or cloud-provider credentials in the control-plane event stream.
- Do not create one schema per logical agent or per agent publication.
- Do not require Portal to be available for normal runtime database writes.
- Do not automatically destroy data when a Host is disabled or deleted from a
  normal administration view.
- Do not place organization-shared Knowledge operational data in the ordinary
  Host store. Knowledge retains its separately governed storage profile.

## Terminology

### Host

A Host is the Portal tenant and runtime-isolation identity used by existing
authorization, API binding, configuration, and instance administration flows.
This document uses “Host” for that concrete platform concept even when a
business deployment calls it a tenant.

### Environment

An environment is the explicit `envTag` used to select and publish runtime
configuration, such as `dev`, `qa`, or `prod`. A storage binding is keyed by
Host and environment. The environment must not be inferred from a Host
subdomain or database name.

### Operational Store Profile

A profile is reusable control-plane policy describing how storage is supplied:

- managed or customer-managed;
- dedicated or pooled;
- provider and region/residency;
- database engine and supported versions;
- backup, recovery, encryption, and availability class;
- default service schemas and migration policy;
- secret-provider integration; and
- artifact and audit sink profiles.

The profile is intended state. It does not contain a runtime password.

### Operational Store Binding

A binding is the accepted association between one operational scope and one
concrete store. It carries stable identifiers, expected database identity,
schema assignments, secret references, compatibility versions, digests, and
lifecycle state.

### Operational Scope Root

Every ordinary store contains an immutable operational-scope record with at
least:

- `binding_id`;
- `host_id`;
- `environment`;
- `scope_kind`;
- `scope_id`;
- database identity;
- binding digest;
- schema-contract generation; and
- creation and activation evidence.

Runtimes validate this record at startup before accepting traffic. A valid URL
that points to another Host's database must fail readiness.

### Provisioning Worker

The worker is a privileged infrastructure component that reconciles requested
bindings. It may use PostgreSQL administration APIs, a cloud database provider,
Kubernetes operators, Terraform-compatible automation, or an enterprise
provisioning adapter. Runtime services never inherit its privileges.

## Deployment Profiles

### Managed Dedicated

The preferred enterprise topology is one database per Host and environment.
Within it, service-owned schemas may include:

```text
operational_meta
agent_ops
memory_ops             # may initially remain in agent_ops
a2a_ops
workflow_ops
execution_ops
gateway_ops
audit_ops
artifact_ops
```

The database boundary provides independent backup, restore, residency,
capacity, and decommission operations. Schemas and roles retain service
ownership inside that database. Two services do not gain cross-schema write
access merely because they are colocated.

A deployment may put several dedicated databases on one PostgreSQL cluster.
“Database per Host” does not imply “server process per Host.”

### Managed Pooled

Portal cloud may place many Hosts in a shared operational database. The pooled
profile uses service schemas and Host-scoped rows, not a schema for every Host:

```text
agent_ops.agent_session_t(host_id, ...)
a2a_ops.a2a_task_t(host_id, ...)
workflow_ops.execution_t(host_id, ...)
```

Every tenant-owned key, unique constraint, index, query, outbox event, cache
key, artifact prefix, backup/export operation, and audit record includes the
Host boundary. Database roles are service-specific. PostgreSQL Row-Level
Security is defense in depth, with tenant context set from authenticated
runtime state rather than caller-supplied JSON.

Table owners normally bypass Row-Level Security unless it is forced. Runtime
roles therefore must not own pooled tables, and `FORCE ROW LEVEL SECURITY`
should be enabled where the selected PostgreSQL profile supports it. See the
PostgreSQL [Row Security Policies](https://www.postgresql.org/docs/current/ddl-rowsecurity.html)
documentation.

Pooled storage reduces provisioning and migration overhead but increases the
impact of a missing tenant predicate and limits per-tenant restore. It is a
deliberate service-provider profile with dedicated isolation tests, not the
default enterprise shortcut.

### Customer Managed

The customer creates an empty or pre-migrated operational database and grants
the documented bootstrap and runtime roles. Host Admin collects a connection
secret reference plus non-secret binding metadata. Portal then:

1. resolves the secret through an approved secret-provider integration;
2. connects from a controlled validator, not the user's browser;
3. verifies TLS, network policy, engine/version, database identity, and
   privileges;
4. installs or validates the operational-scope root;
5. applies allowed migrations if authorized;
6. validates backup and service schemas according to profile policy; and
7. activates the binding only after every required gate passes.

The customer may choose validation-only mode. In that mode Portal reports the
required migration or grant without changing the database.

### Standalone

The same binding and scope-root contracts are usable without Light Portal.
Open-source deployment scripts or an operator create the database, install
schemas, generate a local binding document, and mount the connection URL as a
secret file. The runtime validates the same identity and schema versions.

A Portal customer may later import the non-secret binding metadata and secret
reference. No operational data migration is implied merely by adopting Portal.

## Why Schema Per Host Is Not The Default

Automatically creating one Host schema in the Config Server database is
rejected. Creating one Host schema in a separate operational database is
possible but is not the general default either.

PostgreSQL schemas are namespaces, not rigid tenant boundaries. Any role with
write access to a schema on the `search_path` can influence unqualified object
resolution; the PostgreSQL
[Schemas](https://www.postgresql.org/docs/current/ddl-schemas.html)
documentation calls out that trust implication. Thousands of tenant schemas
also complicate migration fan-out, catalogs, connection pooling, monitoring,
backup/restore, retention, and per-service ownership.

The supported defaults are therefore:

- dedicated database per Host and environment for enterprise isolation; or
- shared operational database with service schemas and tenant-scoped rows for
  Portal cloud density.

A schema-per-Host profile may be added for a constrained enterprise deployment
only if its migration, role, backup, restore, search-path, and scale contracts
are explicit. It must never reuse the Config Server database.

## Architecture

```text
                          LIGHT PORTAL

 Host Admin
    |
    +-- createHost -------------------> Host aggregate and projections
    |
    +-- requestOperationalStoreBinding
             |
             v
       Store-binding aggregate -----> provisioning outbox
             |                                |
             |                                v
             |                       Provisioning worker
             |                         |      |      |
             |                         |      |      +--> audit evidence
             |                         |      +---------> secret provider
             |                         +----------------> DB / object store
             |                                |
             |                         Ready or Failed event
             |                                |
             +--------------------------------+
             |
             +--> Host Admin status and diagnostics
             |
             +--> publication compiler --> Config Server binding projection
                                                  |
                                                  v
                                    gateway / agent / A2A / workflow
                                                  |
                                         deployment secret file
                                                  |
                                                  v
                                      Host operational database

 Standalone: provisioning scripts produce the same non-secret binding and
 secret-file contract without the Portal components.
```

The command side records requested intent and observed lifecycle changes. The
worker owns infrastructure reconciliation. Runtime services own operational
content after activation.

## Component Responsibilities

| Component | Responsibilities | Must not do |
| --- | --- | --- |
| Host Admin UI | Create Host identity, select a storage profile, request/observe provisioning, retry authorized failures, rotate, and request decommission | Accept or display raw passwords; execute DDL; imply that Host creation means store readiness |
| Host command service | Preserve Host identity lifecycle | Call a database provider or write an operational store |
| Store-binding command service | Validate intent, authorization, state transitions, version, and optimistic concurrency; emit events/outbox | Carry credential material in events; perform long-running provisioning inline |
| Provisioning worker | Reconcile infrastructure, secrets, roles, scope root, migrations, validation, and lifecycle evidence | Serve runtime traffic; use runtime roles as admin roles |
| Publication compiler | Publish only `READY` non-secret bindings to the correct Host, service, environment, and instance audiences | Publish a URL or password; publish a failed or stale binding |
| Config Server | Distribute immutable binding policy and references | Own operational rows or credential material |
| Secret provider or deployment | Materialize runtime credentials at the configured file path | Make secrets readable through ordinary Portal queries |
| Runtime service | Validate binding/scope identity and schema compatibility; write only its owned operational schema | Create databases; write another service's schema; query Portal authoring tables on the request path |

## Control-Plane Model

### Store Profile

An illustrative authored profile is:

```yaml
operationalStoreProfile:
  profileId: enterprise-postgres-dedicated-v1
  mode: MANAGED_DEDICATED
  provider: POSTGRESQL
  regionPolicy: ca-central
  residencyPolicyId: canada-primary
  engineVersions:
    min: "17"
    max: "18"
  highAvailability: true
  encryptionProfileId: enterprise-db-kms-v1
  backupProfileId: enterprise-pitr-35d-v1
  secretProviderId: enterprise-vault-v1
  migrationProfileId: fabric-operational-v1
  serviceSchemas:
    - agent_ops
    - a2a_ops
    - workflow_ops
    - execution_ops
    - gateway_ops
    - audit_ops
    - artifact_ops
```

For a customer-managed profile, provider-specific provisioning fields are
replaced by validation requirements and a secret-provider allowlist.

### Host And Environment Binding

An illustrative binding projection is:

```yaml
operationalStoreBinding:
  contractVersion: 1
  bindingId: 019c...
  bindingVersion: 4
  bindingDigest: sha256:...
  hostId: 019b...
  environment: prod
  scopeKind: HOST_ENVIRONMENT
  scopeId: 019d...
  profileId: enterprise-postgres-dedicated-v1
  deploymentProfile: MANAGED_DEDICATED
  state: READY
  expectedDatabase: lightapi_ops_019b_prod
  secretRef: operational-store/019b/prod/runtime
  objectStoreProfileId: tenant-artifact-ca-v1
  auditSinkProfileId: tenant-audit-ca-v1
  serviceSchemas:
    agent: agent_ops
    a2a: a2a_ops
    workflow: workflow_ops
    execution: execution_ops
    gateway: gateway_ops
    audit: audit_ops
    artifact: artifact_ops
  schemaContractGeneration: 1
  minimumSchemaVersions:
    agent: 1
    a2a: 1
    workflow: 1
  validFrom: 2026-08-29T00:00:00Z
  revocationEpoch: 0
```

`secretRef` is a logical reference interpreted only by the approved deployment
and secret integration. It is not a connection URL. A Config Server audience
projection may translate it to an expected local `databaseUrlFile`; that file
is populated outside Config Server.

### Aggregate And Event Boundary

The Host aggregate is not expanded with provider job state. A separate binding
aggregate supports events such as:

```text
OperationalStoreBindingRequested
OperationalStoreProvisioningStarted
OperationalStoreInfrastructureAllocated
OperationalStoreMigrationApplied
OperationalStoreValidationFailed
OperationalStoreBindingReady
OperationalStoreCredentialRotationRequested
OperationalStoreCredentialRotated
OperationalStoreDecommissionRequested
OperationalStoreRetentionHoldApplied
OperationalStoreDecommissioned
```

Events contain stable identifiers, desired or observed state, version/digest,
provider operation references, redacted error codes, and audit context. They
never contain credentials.

Each external action is associated with an idempotency key derived from the
binding, desired generation, operation kind, and provider. Replaying the Portal
event stream rebuilds lifecycle projections but does not blindly re-execute
every historical infrastructure action. The reconciler compares desired and
observed generations.

## Provisioning Lifecycle

### 1. Create The Host Identity

An authorized Host administrator submits the existing Host form. The Host
command commits independently. The resulting Host can exist while its store is
`NOT_REQUESTED`, `REQUESTED`, or `FAILED`.

### 2. Request A Binding

The administrator selects:

- environment;
- deployment mode and allowed store profile;
- region/residency and availability class where the profile permits choice;
- customer-managed secret reference when applicable;
- migration authorization mode; and
- optional artifact and audit profiles.

The command validates Host authority, profile compatibility, uniqueness of the
active `(host_id, environment)` binding, and policy constraints. It emits the
request and an outbox record atomically.

### 3. Claim And Reconcile

A worker claims the desired generation with a lease. It loads the profile,
compares desired and observed resources, and resumes from the last completed
checkpoint. A second worker may take over after lease expiry without creating
a second database.

### 4. Provision Or Validate Infrastructure

For a managed profile, the worker creates or adopts the database, network
policy, encryption, backup, secret entries, object prefixes, and audit sink.
For a customer-managed profile, it validates them. Provider resource tags
include the binding ID and desired generation so retries can rediscover them.

### 5. Install Roles, Scope, And Schemas

The worker uses a short-lived bootstrap identity to:

- install `operational_meta` and the immutable scope root;
- create migration-owner and service-runtime roles;
- apply versioned service schema migrations;
- revoke implicit or public grants not allowed by the profile;
- install pooled-tenant Row-Level Security where applicable; and
- record migration checksums and compatibility generations.

The bootstrap credential is not the runtime credential and is revoked or
expired after completion.

### 6. Validate Readiness

Readiness requires successful checks for:

- expected engine, database, Host, environment, scope, and binding digest;
- TLS and encryption profile;
- service-role grants and denial of cross-service writes;
- migration checksum and minimum schema version;
- backup or point-in-time-recovery policy;
- pooled isolation policy when selected;
- object and audit bindings when required; and
- runtime secret materialization without exposing its value to Portal View.

Failures produce stable diagnostic codes and redacted detail. The binding
remains unpublished.

### 7. Activate And Publish

The worker emits `OperationalStoreBindingReady`. The publication compiler then
produces service- and instance-specific Config Server projections. Instances
receive only the schemas and references they require.

Activation does not require all services to start simultaneously. Instance
readiness reports which schema contract and binding digest each service has
accepted, allowing Host Admin to show rollout and drift.

## Binding State Machine

```text
 NOT_REQUESTED
       |
       v
   REQUESTED ---> PROVISIONING ---> MIGRATING ---> VALIDATING ---> READY
       ^                |               |              |
       |                +---------------+--------------+
       |                                |
       +--------------------------- FAILED
                                        |
                                  authorized retry

 READY ---> ROTATING -----------------> READY
   |
   v
 DECOMMISSION_REQUESTED ---> RETENTION_HOLD
              |                    |
              +--------------------+
                       |
                       v
               DECOMMISSIONING ---> DECOMMISSIONED
```

`FAILED` is not a terminal identity state. A retry increments the desired
generation or reuses the same generation when the operation is provably
idempotent. A changed profile, region, database identity, or secret binding
requires a new generation and review of relocation implications.

## Runtime Projection

The common semantic contract published to a runtime is:

```yaml
operationalStore:
  contractVersion: ${operationalStore.contractVersion:1}
  bindingId: ${operationalStore.bindingId:}
  bindingDigest: ${operationalStore.bindingDigest:}
  profileId: ${operationalStore.profileId:}
  deploymentProfile: ${operationalStore.deploymentProfile:DEDICATED_HOST}
  scopeKind: ${operationalStore.scopeKind:HOST_ENVIRONMENT}
  scopeId: ${operationalStore.scopeId:}
  hostId: ${operationalStore.hostId:}
  environment: ${operationalStore.environment:}
  serviceOwner: ${operationalStore.serviceOwner:}
  schema: ${operationalStore.schema:}
  minimumSchemaVersion: ${operationalStore.minimumSchemaVersion:1}
  expectedDatabase: ${operationalStore.expectedDatabase:operations}
  databaseUrlFile: ${operationalStore.databaseUrlFile:/run/secrets/operational-database-url}
  objectStoreProfileId: ${operationalStore.objectStoreProfileId:}
  auditSinkProfileId: ${operationalStore.auditSinkProfileId:}
```

The compiler binds this block using the existing Host, service ID, `envTag`,
and instance context:

- native `light-agent` receives it in its agent audience projection;
- `light-a2a` receives the same semantic contract in its own audience
  projection, not an `agent.yml` copied from `light-agent`;
- workflow and gateway services receive their owned schema bindings; and
- an instance never receives credentials or schemas for another service.

The runtime combines the projection with the deployment-owned secret file and
then validates the scope root. It fails closed on a Host, environment,
database, binding-digest, schema-owner, or compatibility mismatch. It may use a
last-known-good binding during a temporary Config Server outage, subject to
validity and revocation policy.

## Host Admin Experience

### Creation Wizard

Host Admin should use a two-step wizard while preserving separate commands.

#### Step 1: Host Identity

The existing fields remain the authority for Host identity:

- domain;
- subdomain;
- description; and
- owner.

#### Step 2: Operational Storage

The form displays profile-dependent structured fields:

| Field | Managed dedicated | Managed pooled | Customer managed |
| --- | --- | --- | --- |
| Environment | Required | Required | Required |
| Storage profile | Required | Fixed or policy-filtered | Required |
| Region/residency | Policy-filtered | Fixed by cloud placement | Validated against policy |
| Availability/backup class | Policy-filtered | Fixed by plan | Declared and validated |
| Connection secret reference | Hidden | Hidden | Required |
| Migration authorization | Managed | Managed | Apply or validate-only |
| Database/schema name | Generated and read-only | Shared and hidden | Optional expected identity |
| Artifact/audit profiles | Policy-filtered | Fixed or plan-derived | Required when not supplied by store profile |

The user can choose “configure later.” The Host is then visible with storage
state `NOT_REQUESTED`, and runtime instances that require operational storage
cannot be activated for that environment.

### Host List And Detail

The Host table should show a compact storage summary, while a detail panel
contains provider diagnostics:

- Host and environment;
- profile and deployment mode;
- lifecycle state and desired/observed generation;
- region and residency;
- last transition and redacted failure code;
- database and schema compatibility status;
- backup, restore, credential rotation, and audit-sink status;
- runtime instances using the binding; and
- authorized actions such as retry, revalidate, rotate, relocate, hold, and
  decommission.

Portal View calls command and query APIs. It never connects to the operational
database and never retrieves the connection secret.

### Authorization

Creating a Host does not automatically grant every storage action. Suggested
fine-grained permissions include:

```text
host:create
operational-store-binding:create
operational-store-binding:read
operational-store-binding:retry
operational-store-binding:validate
operational-store-binding:rotate
operational-store-binding:relocate
operational-store-binding:decommission
operational-store-binding:retention-hold
```

Provider administration, retention hold, and decommission permissions should
be narrower than ordinary Host administration. Normal fine-grained access
control applies; the storage design does not introduce a separate break-glass
content-access model.

## Customer-Managed Input And Secret Safety

The browser should submit a logical secret reference, not a password-bearing
database URL. Portal may offer integrations for Vault, Kubernetes Secret,
cloud secret managers, or an enterprise-approved provider.

For a deployment that cannot integrate a secret manager, a one-time secret
submission endpoint may be offered only if it:

- is explicitly enabled for that deployment;
- encrypts directly to the secret provider;
- never records the plaintext in request logs, traces, events, projections,
  dead-letter queues, or browser storage;
- returns only a new secret reference;
- supports immediate replacement and deletion; and
- is unavailable to ordinary query APIs.

Connection validation is also an outbound-network security boundary. The
validator must enforce allowlisted engines, schemes, ports, networks, DNS
resolution policy, TLS verification, timeouts, and response limits. A Host
administrator must not be able to turn it into an unrestricted SSRF or network
scanning service.

## Database Roles And Privileges

| Role class | Lifetime | Authority |
| --- | --- | --- |
| Provider identity | Provisioning only | Create/adopt database and provider resources allowed by the profile |
| Migration owner | Short-lived or controlled job | Own and migrate only the assigned service schemas |
| Service runtime role | Rotated runtime credential | Read/write only the service-owned schema and approved shared interfaces |
| Read/query role | Optional and audited | Read explicitly approved operational views, not arbitrary tables |
| Audit publisher | Runtime | Append to audit outbox or publish to the configured sink |
| Backup/restore identity | Controlled operations | Perform profile-approved backup and restore without runtime authority |

Cross-service access uses authenticated APIs, stable references, integration
events, and outboxes. Permanent cross-schema foreign keys and broad database
roles are not allowed merely for implementation convenience.

## Credential Lifecycle

Credential rotation is a first-class binding transition:

1. issue a new runtime credential under the same binding generation or an
   explicitly compatible new generation;
2. materialize a new secret version;
3. publish the reference/version and wait for required instances to report
   acceptance;
4. preserve a bounded overlap window;
5. revoke the old credential; and
6. record rotation evidence without recording either secret.

Failure to rotate one service role must not require exposing a database-owner
credential or rotating unrelated Hosts. Emergency revocation can make a
binding temporarily unavailable, but it does not rewrite historical Portal
events.

## Idempotency And Reconciliation

Provisioning is a desired-state reconciliation loop rather than a linear shell
script.

- Provider resources are tagged with stable binding and generation IDs.
- Each checkpoint records a digest of the desired profile and observed result.
- Retrying discovery occurs before creation.
- Migrations use checksums and a schema-version ledger.
- Leases prevent concurrent active workers but do not provide correctness by
  themselves.
- Event/outbox consumers are idempotent.
- A worker crash after an external success but before event publication is
  recovered by rediscovery and reconciliation.
- Operators can distinguish transient provider failure, invalid customer
  input, policy denial, schema incompatibility, and manual action required.

## Failure Semantics

| Failure | Required behavior |
| --- | --- |
| Host command fails | No binding request is made for a nonexistent Host |
| Host succeeds, binding request fails | Host remains valid with `NOT_REQUESTED` or failed request diagnostics |
| Provider times out after creating a database | Retry discovers the tagged database; it does not create another one |
| Customer secret cannot be resolved | Binding remains unpublished; no secret detail enters the event stream |
| Scope root points to another Host/environment | Validation fails closed and raises a high-severity audit event |
| Migration checksum differs | Stop before activation and require an authorized compatibility decision |
| One service schema fails | Binding is not globally `READY`; partial infrastructure remains reconcilable |
| Config Server is unavailable after readiness | Worker retains desired/observed state; runtimes follow last-known-good policy |
| Portal is unavailable during runtime work | Runtime continues direct operational writes according to local readiness |
| Audit sink is temporarily unavailable | Owning service uses its bounded durable audit outbox/spool and applies backpressure policy |

## Deactivation, Retention, And Decommissioning

Host lifecycle and data destruction are intentionally decoupled.

- `DISABLED` Host or instance state blocks new authorized work but preserves
  operational data for recovery, audit, retention, and legal obligations.
- Decommission requires explicit scope, impact preview, retention evaluation,
  backup/export decision, active-instance drain, and approval.
- A legal or retention hold prevents destruction while allowing credentials to
  be revoked and the store isolated.
- The worker first fences writers, revokes runtime credentials, drains audit
  outboxes, and records final scope and backup evidence.
- Physical deletion occurs only after the configured recovery window.
- Tombstone and destruction evidence remains in the control plane without
  retaining the deleted credential or operational content.

For pooled storage, decommission deletes or anonymizes Host-scoped rows and
artifact prefixes through service-owned erasure workflows; it does not drop the
shared database. Dedicated storage can be destroyed as one resource only after
all owned services and retention obligations reach the same terminal state.

## Promotion, Clone, And Relocation

A storage binding is environment-specific and must not be silently copied by
configuration promotion.

- Promoting agent, API, gateway, or workflow policy creates target-environment
  intended state but references an independently approved target binding.
- Cloning a Host does not copy operational content by default.
- A data migration is a separate export/import or relocation workflow with
  tenant scope, integrity digests, retention, and audit evidence.
- Changing region, residency, database identity, or deployment profile creates
  a new binding generation and a controlled dual-read/write or cutover plan;
  it is not an in-place form edit.
- Rollback retains a bounded, read-only source until the target passes
  consistency and recovery gates.

## Relationship To Agents And A2A

The operational store is bound to the Host/environment and then compiled for
the instances that require it. It is not bound to each agent definition.

- Several native `light-agent` instances can use the Host's `agent_ops` under
  their own service and agent authorization scopes.
- External agents integrated by `light-a2a` use the Host's `a2a_ops`; the
  external agent developer does not receive direct database credentials.
- `light-gateway` receives its own operational/audit schema binding and never
  becomes the owner of agent sessions or A2A tasks.
- Agent definitions, skills, routes, access policy, public cards, and store
  bindings remain control-plane publications.
- Sessions, turns, task correlation, idempotency, memories, artifacts, and
  audit delivery state remain operational data.

This provisioning contract and the operational schema roots should be frozen
before production A2A persistence. Protocol parsing, Portal A2A authoring, and
stateless gateway routing may proceed in parallel.

## Implementation Plan

### Phase 0: Freeze Contracts And Threat Model

Deliver:

- operational-store profile, binding, state, event, and runtime-projection
  schemas;
- explicit Host/environment scope-root contract;
- secret-provider and provider-adapter interfaces;
- provisioning permission and audit-event vocabulary;
- PostgreSQL role, schema, migration, and pooled-isolation conventions;
- customer-managed outbound-network threat model; and
- conformance fixtures usable with and without Portal.

Exit gates:

- no contract field requires credential material in Portal events or Config
  Server;
- Host identity creation remains backward compatible;
- a runtime rejects a valid connection to the wrong scope; and
- retry and crash-recovery fixtures prove idempotent provider behavior.

### Phase 1: Standalone And Customer-Managed Bootstrap

Deliver:

- open-source bootstrap/migration tooling;
- scope-root and expected-database validation;
- service-owned roles and schemas;
- local binding document and secret-file integration;
- customer-managed validate-only workflow; and
- backup/restore and upgrade documentation.

This phase proves that operational storage does not depend on proprietary
Portal code.

### Phase 2: Portal Binding Control Plane

Deliver:

- binding aggregate, events, projections, outbox, and query API;
- structured Host Admin storage step and status/detail views;
- profile administration and fine-grained permissions;
- Config Server audience compilation for `READY` bindings; and
- runtime registration/readiness reporting for accepted binding digests.

Exit gates include stale-version conflict tests, secret-redaction tests, failed
binding non-publication, and exact Host/service/environment/instance audience
tests.

### Phase 3: Managed Dedicated Provisioning

Deliver:

- privileged worker and PostgreSQL provider adapter;
- database, network, encryption, backup, secret, role, schema, object, and
  audit reconciliation;
- rotation and provider-failure recovery; and
- enterprise plugin interface for infrastructure provisioning.

Start here before managed pooling because it matches the preferred enterprise
boundary and makes backup/restore and tenant identity easiest to prove.

### Phase 4: Managed Pooled Provisioning

Deliver:

- pooled profile compiler and placement policy;
- tenant-scoped schema migrations;
- Row-Level Security and non-owner runtime roles;
- cross-tenant query, cache, outbox, artifact, export, erasure, and restore
  tests; and
- capacity and noisy-neighbor controls.

Pooled production activation requires independent security review and
cross-tenant penetration tests.

### Phase 5: Lifecycle Completion

Deliver:

- credential rotation and emergency revocation;
- relocation and target-environment migration;
- deactivation, retention hold, export, erasure, and decommission workflows;
- drift reconciliation and provider adoption; and
- operational dashboards and alerts.

## Verification Matrix

### Contract And Publication

- Host creation works without requesting a store;
- one active ordinary binding exists per Host/environment;
- failed or stale bindings cannot be published;
- compiled projections contain no credential material;
- each service sees only its own schema binding;
- standalone and Portal-managed runtimes accept the same contract; and
- `light-agent` and `light-a2a` use separate audience templates with the same
  store-binding semantics.

### Provisioning And Recovery

- duplicate events and concurrent workers create one logical resource set;
- a crash after every external checkpoint resumes safely;
- provider adoption verifies ownership tags and database scope;
- migration checksums prevent incompatible activation;
- rotation completes with bounded credential overlap; and
- restore recreates the scope root and binding evidence without changing Host
  identity.

### Isolation And Security

- a runtime refuses another Host or environment database;
- service roles cannot write another service schema;
- pooled tests prove guessed IDs, missing predicates, table-owner behavior,
  caches, outboxes, artifacts, exports, and erasure cannot cross Hosts;
- secret values never appear in events, projections, logs, traces, browser
  storage, dead-letter records, or diagnostics;
- customer connection validation cannot reach forbidden networks or downgrade
  TLS; and
- decommission is impossible through ordinary Host update/delete permission.

### Availability And Lifecycle

- Portal and Config Server outages do not interrupt accepted runtime writes;
- provisioning provider outages do not corrupt Host identity state;
- disabled Hosts retain data until an authorized retention workflow completes;
- legal hold blocks physical deletion;
- dedicated and pooled decommission paths retain required audit evidence; and
- runtime readiness exposes binding and schema drift.

## Resolved Decisions

1. Host identity creation and operational-store provisioning are separate
   aggregates and state machines.
2. Host Admin may expose them as one wizard but must show their independent
   outcomes.
3. Portal-managed deployments provision asynchronously through a privileged,
   idempotent worker.
4. Customer-managed deployments create the database first and supply a
   secret-backed binding that Portal validates.
5. No Host operational schema or data is created in the Config Server database.
6. The preferred enterprise profile is one operational database per Host and
   environment with service-owned schemas and roles.
7. Portal cloud may use a pooled database with service schemas, Host-scoped
   rows, and defense-in-depth Row-Level Security.
8. Schema-per-Host is not a platform default and never uses the Config Server
   database.
9. Database credentials remain in a secret provider or deployment-owned secret
   file; Portal events and Config Server carry only references.
10. Only `READY` bindings are compiled for runtime audiences.
11. Runtime scope-root validation includes Host, environment, database,
    binding digest, service ownership, and schema compatibility.
12. Native agents, external A2A integration, workflows, and gateways share the
    Host operational boundary but own separate schemas and roles.
13. `light-knowledge` retains its separately governed, potentially
    organization-shared operational database.
14. Disabling a Host never implies immediate database deletion.
15. Standalone tooling implements the same contracts as Portal-managed
    provisioning.

## Open Questions

1. Should the first provisioning worker be a `light-deployer` operating mode,
   a dedicated open-source executable, or a shared library used by both?
2. Which secret providers are mandatory for the first enterprise release?
3. Which PostgreSQL major versions define the initial compatibility envelope?
4. Does the first release support Portal-managed database creation, or begin
   with customer-managed validation plus standalone bootstrap?
5. Which enterprise infrastructure adapters are open-source core versus
   commercial Portal integrations?
6. What recovery point and recovery time classes appear in the initial store
   profiles?
7. Is a constrained schema-per-Host profile needed in the first release, or is
   dedicated-database plus pooled-row storage sufficient?
8. Which runtime service owns the common scope-root migration and validation
   library?
