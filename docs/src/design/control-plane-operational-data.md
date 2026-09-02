# Control Plane And Operational Data

## Status

Proposed.

This design defines the storage and authority boundary for Light-Fabric
configuration, runtime state, memory, artifacts, audit evidence, and analytical
data. It also defines the database work that must precede production A2A
runtime persistence.

It complements:

- [Database Design](database-design.md);
- [Tenant Operational Store Registration](tenant-operational-store-provisioning.md);
- [Hindsight Memory](hindsight-memory.md);
- [Light-Agent Execution](light-agent-execution.md);
- [Light-Workflow Runner](light-workflow-runner.md);
- [Tracing](tracing.md); and
- [A2A Gateway](../product/light-gateway/a2a-gateway.md).

## Decision

Light-Fabric separates control-plane data from operational data by authority
and lifecycle, not merely by whether a row was created through an event.

- Light Portal authoring uses Event Sourcing and CQRS. Its events, authoring
  projections, publication records, and Config Server snapshots remain in the
  Config Server database or schema.
- Config Server distributes bounded, immutable, audience-specific runtime
  instructions. It does not become a database for sessions, tasks, memories,
  artifacts, traffic records, or other facts created by runtime activity.
- Each tenant/host operational boundary uses an operational database. Services
  own separate schemas and database roles within that boundary; they do not
  share tables merely because they share one physical database.
- Runtime services write their authoritative operational state directly in
  local transactions. They may also use append-only domain ledgers and a
  transactional outbox for audit, integration, or projection work. Those
  operational events do not become Portal configuration events.
- Light Portal may manage operational data through open, authenticated runtime
  administration APIs. Portal View does not write operational tables directly
  and does not move operational content through Config Server.
- `light-knowledge` keeps its operational data in its Knowledge database. An
  organization may share that database across multiple authorized hosts when
  the Knowledge Base scope is explicitly organization-wide.
- `light-gateway` remains stateless for application work except for bounded
  caches, retry state, and a durable telemetry or audit spool when required. It
  does not own agent sessions, A2A tasks, memories, or workflow state.

The default is not one database per agent and not one gateway-owned database
shared by every agent. The recommended enterprise topology is one operational
database per tenant/host and environment, with service-owned schemas such as
`agent_ops`, `a2a_ops`, `workflow_ops`, `gateway_ops`, and `audit_ops`. Physical
pooling is a deployment choice described below and does not change logical
ownership.

## Why This Boundary Is Needed

The current repository contains a transitional mixture:

- Portal configuration is compiled into immutable `agent.yml` and gateway
  projections;
- `light-agent` sessions, turns, actions, approvals, pinned policy evidence,
  quotas, and memory are operational state;
- the current memory adapter can send some writes through Portal commands while
  reading session history and recall data directly from PostgreSQL; and
- several deployment examples still point runtime services at the
  `configserver` database.

That mixture is useful during bootstrapping but should not become the target
architecture. It couples runtime availability and write throughput to the
Portal command path, makes backup and retention boundaries ambiguous, and
makes it difficult to run Light-Fabric without Light Portal.

The target keeps Light Portal valuable as the management plane without making
it a required hop for a conversation, workflow transition, A2A task update, or
memory enrichment.

## Goals

- Preserve Event Sourcing and CQRS for Portal-managed authoring and publication.
- Give every operational record one authoritative service and store.
- Support enterprise dedicated databases, Portal cloud pooling, and
  customer-managed standalone deployments through the same contracts.
- Let every Light-Fabric service run without Light Portal using local
  configuration, secrets, and an operational store.
- Let Light Portal manage the same services more effectively through live
  configuration, status, operational administration, and audit views.
- Keep high-volume or high-churn data out of Config Server snapshots and
  authoring projections.
- Apply consistent host, principal, agent, workflow, task, and retention
  authorization to operational APIs.
- Define memory placement without confusing centrally managed content with
  control-plane configuration.
- Give A2A implementation phases a storage contract that does not create new
  operational tables in the Config Server schema.
- Permit independent backup, restore, retention, erasure, residency, and scale
  policies for configuration, operational state, Knowledge, artifacts, and
  analytics.

## Non-Goals

- Do not require a separate physical PostgreSQL server for every service.
- Do not create one database for every logical agent definition.
- Do not make `light-gateway` the owner of agent, A2A, or workflow state.
- Do not put database credentials or other secrets in Config Server values.
- Do not copy mutable operational content into `values.yml`.
- Do not require all existing runtime tables to move before protocol-only A2A
  work can begin.
- Do not make Portal View a database client.
- Do not use the Portal event log as a high-volume traffic-log sink.
- Do not treat a log, metric, trace, artifact, chat message, or recalled memory
  as runtime authority.

## Terminology

### Control Plane

The control plane owns intended state: what an authorized administrator wants
a service to do. Examples include an agent definition, effective skill set,
route binding, access policy, memory policy, retention profile, model alias,
workflow definition, and approved A2A publication.

Portal command events and their CQRS authoring projections belong here. A
published generation is compiled into an immutable runtime projection and
made available through Config Server.

### Config Server Projection

A Config Server projection is the bounded, audience-specific input accepted by
one runtime instance. It is selected by the registered host, service ID,
environment tag, and instance binding. It contains policy, identifiers,
digests, limits, and references. It must be usable without a live query to
Portal authoring tables.

The word projection here does not mean a query view of operational activity.
An operational read model belongs to the operational domain even if it is also
built asynchronously.

### Operational Or Data Plane

The operational plane owns facts created while work runs: what happened, what
is happening, and the durable content required to continue or explain it.
Examples include sessions, turns, workflow tasks, A2A correlation, memory
units, action attempts, artifact metadata, quota consumption, idempotency
records, audit evidence, and deletion tombstones.

Operational tables are normally written directly in the owning service's
transaction. An append-only domain event stream or outbox can accompany that
transaction without changing the data into control-plane configuration.

### Management Plane

Portal View is a management surface over both planes:

- configuration forms send commands to the Portal control plane; and
- operational screens call authenticated administration or query APIs exposed
  by the owning runtime service or a shared operational service.

The UI location does not determine data ownership. Editing a user memory in
Portal View does not make that memory Config Server data.

### Observability And Analytical Plane

Logs, metrics, traces, and analytical traffic records are derived operational
evidence. A collector or durable audit publisher sends them to an approved log,
telemetry, or analytics store. The control plane contains their collection,
redaction, sampling, retention, and destination policy, not the collected
records.

## Classification Test

Use the following questions for every new field or table.

| Question | Control-plane signal | Operational signal |
| --- | --- | --- |
| Who is authoritative? | An authorized administrator, publication workflow, or policy compiler. | A running service, authenticated user interaction, backend result, or reconciler. |
| What does the value describe? | Intended behavior and allowed authority. | Work performed, content learned, current state, or evidence. |
| What causes change? | Explicit authoring, review, approval, publication, revocation, or promotion. | Requests, conversations, task transitions, timers, callbacks, retries, or cleanup. |
| What is the write rate? | Low to moderate and versioned. | Potentially high-volume and high-churn. |
| Can a runtime snapshot reproduce it? | Yes; the published generation is the authority. | No; it can be recreated only by replaying operational history or not at all. |
| Does it require independent retention or erasure? | Usually tied to authoring and publication history. | Often tied to user, session, task, legal hold, artifact, or audit policy. |
| Does the runtime need to write it while Portal is unavailable? | Normally no. | Normally yes. |

When signals are mixed, split the concept into policy and instance state rather
than storing one ambiguous aggregate. For example, a memory-retention profile
is control-plane data; the expiry and deletion evidence for a particular
memory unit are operational data.

## Target Architecture

```text
                                LIGHT PORTAL

 Portal View configuration forms                 Portal View operations views
               |                                             |
               v                                             v
       Portal command APIs                         Open operational APIs
               |                                             |
               v                                             v
     Event store and CQRS                              Owning service
     authoring projections                                  |
               |                                             v
               v                                  Tenant/host operational DB
      publication compiler                           and artifact storage
               |
               v
    Config Server snapshots
               |
               +------------------+--------------------------+
                                  |
                                  v
                    gateway / agent / A2A / workflow
                         immutable runtime policy

 Runtime logs, metrics, traces, and audit outboxes
               |
               v
        collector / audit publisher -----> audit and analytics stores

 Standalone deployment:
 values.yml + secret files + the same operational APIs and operational stores
```

There are two management routes, not one overloaded configuration route:

1. the configuration route authors and publishes intended state; and
2. the operational route reads or mutates runtime-owned content under the same
   fine-grained authorization model used by the runtime.

An audit route collects evidence from both without becoming authoritative for
either.

## Data Ownership Matrix

| Domain | Control-plane authority | Operational authority | Recommended storage |
| --- | --- | --- | --- |
| Gateway routing and access | Routes, Instance API bindings, rule bodies, limits, redaction, and telemetry policy. | Bounded rate state, circuit state, accounting delivery, audit spool, and edge correlation. | Config Server projection plus `gateway_ops` or external telemetry systems. |
| Agent definition | Prompt, model, skills, tools, memory policy, knowledge bindings, execution policy, and public A2A publication. | None; the definition is intended state. | Portal event store, CQRS projection, and Config Server runtime projection. |
| Agent execution | Session and turn limits, approval rules, quota policy, retention, and data boundary. | Sessions, turns, action attempts, approvals, policy evidence, idempotency, quotas, and session events. | `agent_ops` in the tenant/host operational database. |
| Agent capacity and service pools | Pool definitions, agent assignments, compatibility dimensions and digests, enablement, and `maximum_concurrency`. | Accepted pool ID and digest, active occupancy, reservations, leases, and queue state. | Pool policy in the Agent projection; occupancy and reservation rows in `agent_ops`. |
| A2A external integration | Publications, backend bindings, protocol profiles, signing profiles, fine-grained policy, limits, and retention. | External adapter correlation, task facade state, idempotency, callbacks, cancellation, artifact metadata, and deletion evidence. | `a2a_ops` owned by `light-a2a`. |
| A2A native agent | Native A2A policy and publication. | Context/session and task/turn aliases plus native artifacts; the underlying session remains authoritative. | `agent_ops` owned by `light-agent`. |
| Workflow | Definitions, deployment policy, allowed callers, execution profiles, and retention policy. | Process/task state, worklists, attempts, leases, approvals, timers, outbox, and artifacts. | `workflow_ops` and shared runner-owned schemas. |
| Knowledge | Knowledge Base definitions, source bindings, ACL policy, embedding policy, and retention policy. | Ingested documents, chunks, embeddings, graph state, indexing jobs, and query evidence. | Dedicated Knowledge database and object storage. |
| Hindsight memory | Bank classes, sharing policy, retention, provider, recall limits, hard directives, and promotion rules. | Bank instances, memory units, links, entities, reflections, provenance, session history, erasure, and deletion evidence. | `memory_ops`, initially colocated with `agent_ops` but owned behind a Memory API. |
| Artifacts | Type, size, scan, visibility, retention, export, legal-hold, and promotion policy. | Bytes, immutable digest, owner, task linkage, scan result, expiry, holds, and tombstones. | Tenant object storage plus owning operational metadata schema. |
| Audit and traffic analysis | Required events, fields, redaction, sampling, retention, and sink policy. | Audit records, traffic observations, trace correlation, delivery cursor, and integrity evidence. | Tenant audit store and approved log/analytics platform; never Config Server. |

Service ownership must remain valid when schemas are later moved into separate
physical databases. Cross-service relationships therefore use stable IDs,
authenticated APIs, and integration events rather than cross-schema foreign
keys that require permanent colocation.

The service-pool row is a concrete example of a split that the current code has
not yet made. `light-agent` selects a pool by joining `agent_pool_assignment_t`
and `agent_service_pool_t` inside its admission transaction and takes row locks
on both, while `agentPolicy.execution.servicePools` already carries the same
definitions in the immutable Agent projection. After the split, pool
definitions, assignments, compatibility digests, and concurrency ceilings are
read from the accepted projection, and only occupancy and reservation rows may
be locked, because a runtime cannot hold a database lock on control-plane
content it no longer stores.

## Tenant And Host Storage Topology

### Recommended Logical Boundary

The isolation key is the Portal tenant/host and environment. A production
runtime must bind its operational store to the same host and environment as its
accepted Config Server projection. A connection that resolves to a different
boundary fails startup or reload validation.

Within the boundary:

- every service has a distinct database role;
- every service owns its migrations and schema version;
- roles cannot write another service's schema;
- tenant/host identifiers remain on authoritative rows as defense in depth;
- backup, restore, erasure, and legal-hold operations preserve service
  ownership; and
- cross-service reporting uses APIs, outbox events, or read-only analytical
  projections rather than shared write access.

### Physical Deployment Profiles

| Profile | Physical layout | Intended use | Required invariants |
| --- | --- | --- | --- |
| `DEDICATED_HOST` | One operational database per tenant/host and environment, with service-owned schemas. | Enterprise and regulated deployments. | Dedicated credentials, host binding, independent backup and residency. |
| `POOLED_TENANT` | Multiple tenants share a managed cluster or database; every row and partition is tenant-scoped. | Light Portal cloud for small and intermediate customers. | Enforced tenant key, row-level or equivalent isolation, per-tenant encryption context, noisy-neighbor limits, and export/erasure support. |
| `CUSTOMER_MANAGED` | Customer supplies the operational database and object store. | Standalone open-source or hybrid deployment. | Published migrations, readiness validation, least-privilege roles, and no Portal dependency. |

The product contract is logical isolation, not a promise that every small
tenant receives a separate PostgreSQL server. A customer can promote from a
pooled profile to a dedicated profile without changing service APIs or data
semantics.

### Knowledge Exception

Knowledge is often organization-scoped rather than runtime-host-scoped. A
Knowledge database may therefore serve multiple hosts in the same organization
when all of the following are explicit:

- an organization and Knowledge Base scope;
- per-source and per-principal authorization;
- host-to-Knowledge-Base bindings in control-plane policy;
- data residency and retention compatibility; and
- query-time delegation that cannot broaden the caller's authority.

This exception does not authorize agent sessions, user memory, workflow tasks,
or A2A task state to cross host boundaries.

## Memory Boundary

Memory is the clearest example of why “managed in Portal View” does not mean
“stored in Config Server.” Portal administrators and end users can manage
memory centrally while the memory remains operational data.

| Memory concern | Plane | Reason |
| --- | --- | --- |
| Allowed bank types and scopes | Control | Defines which sharing models may exist. |
| Default bank selection and creation policy | Control | Constrains runtime behavior. |
| Recall, retention, reflection, promotion, and erasure policy | Control | Defines authority and lifecycle. |
| Provider, embedding model, limits, and residency | Control | Selects governed runtime dependencies. |
| System prompt and hard authorization directives | Control | Must be reviewed, versioned, and immutable for an accepted publication. |
| Concrete user, agent, shared, or session bank | Operational | It is a runtime instance with an owner and lifecycle. |
| Session transcript and history projection | Operational | It is created and enriched by conversation. |
| User profile fact or preference | Operational | It can be edited centrally but represents mutable user content. |
| Learned fact, experience, link, entity, or reflection | Operational | It is derived from runtime activity. |
| Provenance, expiry, legal hold, erasure status, and deletion evidence | Operational | It applies to concrete content and must survive independently of configuration. |

Content called a “directive” requires careful classification. A reviewed hard
rule that can change agent behavior or authority is control-plane content and
must be projected with a digest. A remembered preference or observation is
operational, untrusted model context and cannot grant tools, credentials,
network access, or policy exceptions.

### Memory Service Boundary

The first implementation may embed the Memory API and repository inside
`light-agent` while storing memory tables in `memory_ops` or `agent_ops`. The
API contract should nevertheless be independent of the repository so that a
shared open-source `light-memory` service can be introduced later for:

- user memory shared by several agents;
- organization-managed retention and erasure;
- centralized reflection and embedding work;
- Portal View administration; and
- scale or residency boundaries different from agent execution.

`light-agent` depends on the Memory API abstraction, not on Portal commands or
Portal table layouts. Portal View uses the same authenticated administration
API for search, correction, export, retention hold, and deletion. Every request
is checked against host, principal, agent, bank, operation, and fine-grained
policy.

The current Portal-command memory write mode is a migration adapter. It should
not be the final production authority because it makes Portal availability part
of the conversation write path while reads still depend on operational
PostgreSQL state.

## Configuration And Store Binding

Each runtime projection contains immutable store-binding policy, but no
credential and no operational content. The runtime combines that projection
with a deployment-owned secret file.

An illustrative shared contract is:

```yaml
operationalStore:
  contractVersion: ${operationalStore.contractVersion:1}
  profileId: ${operationalStore.profileId:}
  deploymentProfile: ${operationalStore.deploymentProfile:DEDICATED_HOST}
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

This block is a common semantic contract, not necessarily one shared Rust
configuration struct. A service may namespace it under `agent`, `a2a`,
`workflow`, or `gateway` while retaining the same validation rules.

The accepted binding must satisfy:

- projection audience, host, environment, service, and instance identity;
- an allowlisted deployment profile;
- exact service-owned schema and role;
- compatible migration and schema versions;
- an expected database identity check rather than only a syntactically valid
  connection string;
- secret-file availability and least-privilege connectivity;
- object and audit-store binding where required; and
- last-known-good reload semantics for mutable control-plane policy.

Config Server may distribute `profileId`, schema, version, expected database,
and secret reference. The actual connection URL, password, encryption key, and
object-store credential remain deployment secrets. Portal cloud may
materialize them through its secret-management integration; standalone users
mount the same files themselves.

The binding should eventually be reusable in `agent.yml`, the `light-a2a`
audience projection template, and workflow configuration rather than copying a
database URL into every product policy. `light-knowledge` retains its dedicated `knowledge.yml` connection and
database identity because its organization-shared topology is intentionally
different.

The Host Admin lifecycle, managed and customer-managed deployment profiles,
secret handling, provisioning state machine, and decommission workflow are
defined in
[Tenant Operational Store Registration](tenant-operational-store-provisioning.md).

## Portal And Standalone Operation

### With Light Portal

Portal provides:

- structured authoring, review, publication, revocation, and history;
- instance and store-profile binding;
- live Config Server generation and activation;
- runtime registration, health, schema compatibility, and drift visibility;
- operational views backed by service APIs;
- fine-grained administration of sessions, memories, tasks, artifacts, holds,
  export, and erasure; and
- audit and analytical dashboards backed by approved operational sinks.

Portal does not become a synchronous proxy for normal runtime database writes.

### Without Light Portal

Every open-source service must accept the equivalent intended state from a
local `values.yml` or other supported configuration source, load secrets from
files or a customer secret manager, run its own migrations or validation under
an explicit startup policy, and expose the same operational APIs.

Configuration export and operational export are deliberately separate:

- a configuration bundle contains templates, values, public artifacts,
  digests, and secret references; and
- an operational backup or export contains runtime data under its own
  authorization, encryption, retention, and privacy policy.

Downloading `values.yml` must never silently download user memories, sessions,
task payloads, artifacts, traffic logs, or database credentials.

## Operational API Requirements

Every service-owned operational domain exposes open contracts suitable for
Portal and standalone administration. The APIs must:

- derive host and principal scope from authenticated server-side identity;
- authorize the exact operation and resource rather than trusting an ID;
- support pagination, filtering, and bounded exports;
- use idempotency and optimistic concurrency for mutations;
- emit normal audit evidence for read, correction, export, hold, and deletion;
- avoid returning secret material or raw content without explicit content-read
  authority;
- preserve provenance and deletion tombstones where policy requires them; and
- remain available independently of Portal authoring services.

Portal may cache a display projection, but the operational service remains
authoritative. The UI must show source, freshness, and failures instead of
presenting a stale Portal copy as current state.

## Audit, Traffic, And Analysis

Traffic records are operational/analytical data, not Config Server data. The
control plane specifies which events are required and which fields must be
redacted. The runtime produces the records.

Use three complementary paths:

1. Rust `tracing` emits structured, policy-safe logs and trace correlation to
   stdout or an approved collector.
2. Authoritative security, accounting, approval, artifact, and deletion events
   are committed with the operational transaction through an outbox or an
   equivalent durable local handoff.
3. High-volume traffic analysis flows to a tenant-approved log or analytical
   store. A bounded operational index may retain correlation, status, latency,
   policy digest, and evidence digest without duplicating request content.

Do not perform a synchronous remote audit-database write on every gateway
request. A gateway that must survive collector failure uses a bounded,
encrypted, backpressured spool with explicit fail-open or fail-closed policy by
event class. Dropping a debug trace and losing a required authorization audit
record are not the same failure.

Prompts, messages, model output, memory, tool arguments, task payloads, and
artifact bytes are excluded from ordinary traffic logs by default. Fine-grained
content access applies through the owning operational API; there is no separate
implicit Portal-administrator bypass.

## Availability And Failure Semantics

| Failure | Required behavior |
| --- | --- |
| Portal unavailable | Published runtimes continue with accepted Config Server or local configuration and their operational stores. Portal authoring and operational UI are unavailable, but the runtime data path does not stop solely for that reason. |
| Config Server temporarily unavailable | A runtime may continue with a valid last-known-good generation until its expiry and revocation policy requires failure. It does not query Portal tables as a fallback. |
| Operational database unavailable | The owning service fails or queues work according to its durability contract. It never falls back to writing operational rows into Config Server. |
| Audit collector unavailable | Required events use the configured durable spool or fail policy; optional telemetry may be sampled or dropped with metrics. |
| Knowledge database unavailable | Knowledge-dependent operations fail or degrade according to the published policy; agent sessions and memory do not silently move into the Knowledge database. |
| Portal operational API proxy unavailable | Direct runtime administration remains possible for authorized standalone operators; normal runtime work is unaffected. |

## Migration From The Current Layout

Migration is organized by authority rather than by copying every table at once.

### Step 0: Freeze Contracts

Before new runtime schemas are implemented:

- classify every existing and proposed table;
- define the tenant/host operational-store binding;
- define service schema ownership and least-privilege roles;
- define migration, readiness, backup, restore, and rollback contracts;
- pin operational API and audit/outbox envelopes; and
- prohibit new runtime-written tables in the Config Server schema.

### Step 1: Bootstrap Tenant Operational Stores

- create repeatable database and schema provisioning;
- publish service-owned migrations;
- validate expected database, host, environment, role, and schema version;
- support dedicated, pooled, and customer-managed profiles; and
- add readiness and Portal diagnostics without exposing credentials.

### Step 2: Decouple Cross-Boundary Constraints

No table moves in this step. It removes the referential dependencies that would
otherwise make a physical move impossible, and it runs entirely inside the
current database so that every change is reversible.

The executable Phase 0 inventory identifies 24 removable foreign keys plus one
temporarily retained Agent-to-memory invariant:

| Crossing | Constraints | Replacement |
| --- | --- | --- |
| Operational to control plane (12) | Agent memory, policy evidence, quota usage, sessions, runner requests, and runner sessions reference Host, user, Agent definition, quota policy, service-pool, or runner-binding authoring tables. | Local scope root plus pinned ID, version, publication, digest, validity, and revocation evidence validated at admission. |
| Operational to later Workflow service (6) | Execution attempts and scheduling requests reference Workflow process, task, or approval state; fixed actions reference Workflow approvals. | Stable Workflow reference plus authenticated API/event validation and reconciliation. |
| Agent to execution service (5) | Agent sessions, turns, action attempts, and approval consumption reference execution sessions, attempts, or scheduling requests. | Stable execution reference plus authenticated result/status events and reconciliation. |
| Control plane to operational semantic target (1) | `agent_memory_directive_t` references a concrete runtime memory bank. | Versioned Agent policy targets a bank profile or scope selector rather than a runtime bank ID. |
| Within the agent boundary, pending the later memory split | `agent_session_t` references `agent_memory_bank_t`. | Retained until the Memory API owns the invariant. |

The exact names, source and target columns, replacement contract, and test
owner are frozen in
`implementation/light-portal/development-database-topology/phase0/foreign-key-boundary-v1.json`.
Constraints wholly inside one service transaction boundary stay in place;
`agent_session_t_host_id_bank_id_fkey` is the named retained invariant.

- add a local operational-scope root in each operational schema that records the
  expected host, environment, and, where applicable, organization scope, so that
  scope validation no longer depends on an FK into `host_t`;
- replace control-plane foreign keys with pinned identifiers, versions, and
  digests that are validated at admission against the accepted projection
  instead of enforced by the database;
- replace cross-service foreign keys with stable references plus API,
  outbox, or reconciliation checks that restore the invariant the constraint
  used to guarantee;
- keep the constraint and its replacement active together long enough to prove
  the replacement rejects the same violations; and
- drop each constraint only after its replacement has test coverage.

### Step 3: Move Shared Execution Foundations

Shared execution state moves before Agent cutover because Agent tables point at
it. Moving Agent first would strand `execution_session_t`,
`execution_attempt_t`, and `runner_scheduling_request_t` references across a
boundary that does not yet exist.

- migrate execution session, attempt, and runner scheduling state to its owning
  runner schema;
- establish the fencing, lease, replay, and reconciliation contracts that Agent
  and Workflow will both depend on; and
- verify that no Agent or Workflow table still requires a database-enforced
  reference into these tables.

### Step 4: Move Agent Execution And Embedded Memory

- migrate `agent_session_t`, `agent_turn_t`, action, approval, event,
  idempotency, quota, pool occupancy, and related operational tables to
  `agent_ops`;
- colocate concrete Hindsight banks, memory content, graph data, reflections,
  session history, and deletion evidence in `agent_ops` initially, because
  `agent_session_t` and `agent_memory_bank_t` still share integrity
  expectations that no service yet owns;
- keep memory policy, digests, and store bindings in the immutable Agent
  projection;
- migrate `agent_memory_directive_t` as a semantic exception rather than a
  table move, as described below;
- replace the Portal-command write authority with the Memory API and
  operational repository;
- make Portal View memory management call the Memory administration API; and
- backfill, compare, cut over, and remove the old write path without an
  unbounded dual-write period.

Split memory into a separate `memory_ops` schema only after the Memory API owns
the integrity that the `agent_session_t` to `agent_memory_bank_t` constraint
enforces today. Splitting the schema before the API owns the invariant converts
a database-enforced relationship into an unchecked one.

#### Hard Directives Are A Semantic Migration

`agent_memory_directive_t` is the one memory table that does not move to an
operational schema. A hard directive can change agent behavior or authority, so
it is control-plane content: it becomes versioned, reviewed, digest-bound
authoring data compiled into the Agent projection.

That reclassification also changes its shape. A directive currently references
a concrete operational bank; a published directive instead targets a bank
profile or scope selector, because control-plane content cannot depend on the
existence of one runtime bank instance. Remembered user preferences and
observations stay ordinary operational memory and are not promoted by this
change.

### Step 5: Move Remaining Workflow State

- migrate process, task, worklist, timer, approval, artifact, and outbox state
  under their owning services;
- remove cross-service write access and replace remaining cross-schema
  dependencies with stable contracts; and
- preserve replay, fencing, idempotency, and reconciliation evidence.

### Step 6: Complete Gateway, Audit, And Analytics Separation

- move any durable accounting, retry, circuit, and audit-spool state to
  `gateway_ops` or a purpose-built open service;
- publish structured audit records to the tenant audit store;
- connect logs and traces to approved collectors; and
- verify that Config Server stores only policy and publication history.

Each cutover must define source-of-truth time, read routing, write fencing,
backfill watermark, validation, rollback limit, and deletion of stale
credentials. Indefinite bidirectional dual write is not an acceptable steady
state.

## Relationship To A2A Gateway Delivery

The complete platform-wide database migration is not a prerequisite for
starting A2A work. The storage contract is a prerequisite, and separation must
be implemented for every operational record that the A2A release itself
creates.

| Work stream | May proceed before physical migration? | Storage dependency |
| --- | --- | --- |
| A2A Phase 0 protocol, threat model, canonical operations, errors, and conformance fixtures | Yes. | Must adopt the ownership, retention, store-binding, artifact, and audit contracts from this design. |
| Shared parsing, version negotiation, card handling, and stateless gateway routing | Yes. | No durable task state in `light-gateway`; bounded caches only. |
| Portal A2A authoring, Instance API binding, card publication, policy compilation, and Config Server projections | Yes, in parallel with operational-store bootstrap. | These are control-plane records and immutable runtime projections. |
| `light-a2a` external sidecar correlation, task facade, cancellation, artifact, and restart reconciliation | No for production. | `a2a_ops`, artifact storage, audit outbox, migrations, backup, and readiness must exist first. |
| Native `light-agent` A2A task/turn mapping and artifacts | No for production. | Agent sessions, turns, idempotency, memory, and artifact metadata must be authoritative outside Config Server first. |
| Governed outbound A2A with durable task ownership and retry | No for production. | The calling runtime's operational store and audit handoff must be ready. |

The recommended sequence is:

1. complete Step 0 and freeze the cross-service storage contracts;
2. start tenant operational-store bootstrap and A2A protocol/control-plane work
   in parallel;
3. decouple cross-boundary constraints and move shared execution foundations;
4. migrate Agent and embedded Memory operational state and create `a2a_ops`;
5. implement production external-sidecar and native-agent A2A persistence on
   those stores;
6. complete governed outbound A2A; and
7. migrate remaining Workflow, Gateway, and analytical state incrementally.

This avoids two undesirable extremes: blocking useful A2A protocol and Portal
work on a platform-wide database move, or shipping A2A quickly by adding more
operational tables to `configserver` and making the eventual migration harder.

For the first production A2A release, the hard gate is:

> No A2A task, context, idempotency record, callback state, artifact metadata,
> session/turn alias, retry record, or runtime audit evidence is authoritative
> in the Config Server database or schema.

## Security And Isolation Requirements

- A runtime accepts an operational-store binding only when host, environment,
  audience, service, and schema identity match its accepted policy.
- Database credentials are service-specific and loaded from secret files or a
  secret manager.
- Migration credentials are distinct from runtime credentials.
- Runtime roles have no write access to Portal event, authoring projection, or
  Config Server snapshot tables.
- Portal services have no direct write access to runtime-owned schemas.
- Pooled deployments enforce tenant isolation in the database and in every API;
  an application predicate alone is insufficient.
- Object keys are opaque and tenant-scoped. Possession of an object URL or
  artifact ID is not access authority.
- Operational exports are encrypted, audited, bounded, and authorized
  independently of configuration export.
- Backup, restore, replication, and analytical pipelines preserve tenant,
  residency, retention, and deletion requirements.
- Recalled memory and analytical data remain untrusted input and cannot modify
  configuration or grant authority.

## Verification And Exit Gates

### Contract Gates

- every table in the implementation plan has one plane, owning service, schema,
  retention authority, and migration owner;
- configuration and operational exports have separate schemas and endpoints;
- Config Server projections contain store bindings and policy but no
  operational records or credentials;
- every operational API binds host and principal from authenticated state; and
- no service requires cross-schema write access.

### Isolation Gates

- store-scope validation is storage-profile aware: an ordinary operational
  store validates the (host, environment) pair and refuses to start against a
  database bound to a different pair, while an organization-shared Knowledge
  store validates the (organization, knowledge-store binding, residency) tuple
  and additionally confirms that the requesting host is authorized for that
  Knowledge Base;
- a service role cannot write another service's schema;
- pooled-tenant tests prove that guessed IDs and missing tenant predicates
  cannot cross the boundary;
- backup, restore, export, erasure, and legal-hold tests preserve tenant scope;
  and
- Portal View operations use authenticated APIs rather than direct database
  access.

### Availability Gates

- Agent, Gateway, A2A, Workflow, and Knowledge runtimes continue their allowed
  work while Portal is unavailable;
- a valid last-known-good Config Server generation can be used according to its
  expiry and revocation contract;
- operational database failure never redirects writes to Config Server;
- required audit delivery survives a sink outage within the configured spool
  bounds; and
- standalone deployments pass the same operational API and migration tests as
  Portal-managed deployments.

### Migration Gates

- zero cross-schema or cross-database foreign keys remain, and every constraint
  removed during decoupling has a documented replacement with test coverage
  proving it rejects the violations the constraint used to reject;
- backfill counts, ownership keys, digests, and representative business queries
  match before cutover;
- the cutover fences the old writer and has a bounded rollback point;
- no indefinite dual write remains;
- obsolete database credentials and grants are revoked; and
- the old operational tables are archived or removed only after retention and
  rollback obligations are satisfied.

### A2A Gates

- `light-gateway` restart loses no authoritative A2A task state because it owns
  none;
- `light-a2a` restart reconciles sidecar tasks from `a2a_ops` without Portal or
  Config Server table queries;
- `light-agent` restart preserves native context/session and task/turn mapping
  from `agent_ops`;
- A2A artifacts use tenant-scoped metadata and object storage with independent
  retention from chat and Hindsight memory;
- inbound and outbound retry/idempotency state remains within the selected
  runtime's operational boundary; and
- A2A runtime and audit writes continue when Portal is unavailable.

## Resolved Decisions

1. Plane classification follows authority and lifecycle, not UI location or
   the mere presence of an event.
2. Portal Event Sourcing and CQRS remain the control-plane authoring model.
3. Config Server stores immutable runtime policy projections, not operational
   content.
4. The default enterprise boundary is one operational database per tenant/host
   and environment with service-owned schemas and roles.
5. There is no database per logical agent by default.
6. A gateway does not own the operational database of agents it routes.
7. Knowledge operational data stays in the Knowledge database and may be
   organization-shared under explicit policy.
8. Memory policy and hard directives are control-plane data; concrete banks,
   session/user/agent memory, history, reflections, and deletion evidence are
   operational data.
9. Portal View manages operational content through open authenticated APIs,
   not direct tables or Config Server publication.
10. Standalone and Portal-managed deployments use the same runtime and
    operational contracts.
11. Database credentials remain deployment secrets; Config Server carries
    bindings and references only.
12. A2A storage contracts and tenant-store bootstrap precede durable A2A
    implementation, but a complete platform-wide migration does not block
    protocol, gateway-routing, or Portal-publication work.
13. Cross-boundary constraints are decoupled, and shared execution foundations
    are moved, before any Agent or Memory table changes schema. Referential
    integrity is replaced deliberately rather than dropped as a side effect of
    a move.
14. Hard memory directives are control-plane content targeting a bank profile
    or scope, not operational rows bound to a concrete bank instance.
15. Service-pool definitions and concurrency ceilings are control-plane policy
    read from the Agent projection; only occupancy and reservation state is
    operational.

## Open Questions

1. Should the first shared Memory API ship embedded in `light-agent` only, or
   should `light-memory` be extracted immediately for user memory shared across
   agents?
2. Which operational administration APIs should be routed through
   `light-gateway`, and which should remain on a private management network?
3. Which audit records require synchronous local durability, and which may use
   best-effort collector delivery?
4. Should Portal cloud begin with database-per-tenant or pooled schemas with
   enforced tenant isolation, and what threshold promotes a tenant to a
   dedicated database?
5. Which existing Portal command events represent true configuration and which
   operational commands must migrate first?
6. Should artifact metadata begin in service-owned schemas or in a shared
   open-source artifact service with service-specific ownership tables?
