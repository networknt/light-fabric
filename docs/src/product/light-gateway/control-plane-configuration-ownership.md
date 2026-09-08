# LLM gateway configuration ownership and publication

Status: Proposed design. This document describes intended behavior, not completed implementation.

## Problem

LLM Model Control Plane publications generate several `llm-router` properties in
`instance_property_t`. The generic Instance Config editor also offers edits to
those rows, but the two write paths do not share an aggregate history.

The observed failure was an `agentDelegation` row at aggregate version 3 while
its `ConfigInstance` event stream was at version 1. The generic editor submitted
version 3 and the event store correctly rejected it. Retrying or reducing the
submitted version does not resolve the conflicting ownership.

There is also no dedicated control-plane editor for the endpoint policy inside
`agentDelegation`. The Publication tab offers a read-only generated preview.
Users should not need to edit a generated JSON object to decide whether direct
user inference is allowed.

## Current implementation evidence

These paths are relative to sibling repositories under the workspace:

| Component | Current behavior |
|---|---|
| `light-portal/db-provider/.../persistence/LlmModelPersistenceImpl.java`, `applyInstancePublication` | Applies a publication to multiple instance properties and increments their `aggregate_version` directly; records publication ownership. |
| `light-portal/db-provider/.../persistence/AgentGatewayProjection.java`, `compile` | Derives bindings from current Agent publications and client registrations; reads issuer/audience and endpoint choices from the previous generated instance property; returns null when no user issuer is available. This generated-output input must be removed after migration. |
| `light-portal/db-provider/.../persistence/LlmModelPersistenceImpl.java`, publication revision lookup | Currently deduplicates by host, environment, and property-set digest only, so changed source provenance can reuse an older revision. |
| `light-fabric/apps/light-gateway/src/main.rs`, security handler selection | Looks up the trusted endpoint key in `agent_authorization.endpoints`; absent entries retain the legacy handler chain, while a selected profile colliding with HMAC returns 503. |
| `portal-view/src/pages/genai/llm-model/PublicationPanel.tsx` | Generates a read-only preview and publishes it to an instance. |
| `light-portal/db-provider/.../persistence/GlobalSnapshotPersistenceImpl.java` | Maps `instance_property_t` to ConfigInstance-created events and includes LLM publication ownership data in snapshot support. The import ordering and duplicate-materialization behavior need explicit qualification. |
| `light-fabric/crates/llm-gateway/src/authorization.rs`, `authenticate` | On a selected dual-token route, a required workload token that is absent produces `401 workload_token_required`. Optional workload authentication still validates the user and any supplied workload token. |

## Decision

Use one authoritative control-plane write path for managed configuration. Keep
one complete publication event as the deployment revision, and treat its instance
properties as generated projections. Add a typed Agent Delegation editor and
make ownership explicit in the generic configuration UI and backend.

A domain event may project into many rows. Generating a separate event for each
row is not necessary for event sourcing. Conversely, multiple property events
do not establish which system owns a value or prevent the next publication
from overwriting a generic edit.

No automatic reverse synchronization from arbitrary generated JSON is proposed.
The current `previous.endpoints` fallback and previous issuer/audience inputs are
a legacy reverse-synchronization channel, not the intended ownership contract.
Remove them from normal compilation when authoritative authoring is enabled;
reading generated output is allowed only in the explicit validated bootstrap.
No new agent-to-model assignment store is introduced: existing internal aliases
and `boundPrincipal` remain authoritative for model binding.

## Alternatives

| Approach | Benefits | Costs and limitations |
|---|---|---|
| One publication event with owned projections — selected | Coherent revision, transactional projection, straightforward provenance and replay | Requires managed-edit restrictions and ownership-aware export/import |
| Publication emits individual ConfigInstance events | Per-property aggregate histories; generic versioning can be valid | Requires coordinated expected versions, publication correlation, complete-batch activation, and partial-failure recovery; still needs ownership rules |
| Independent Config edits with reverse synchronization | Either interface can modify raw values | Ambiguous mapping to domain records, feedback loops, conflicts, and security-sensitive interpretation of generated bindings |
| Explicit instance overrides | Supports deliberate deployment-specific exceptions | Adds precedence, validation, provenance, and rollback rules; defer until required |

If property events are adopted later, commands must append them through the event
store, with a publication manifest and explicit completion barrier. A projection
consumer must not emit an uncontrolled cascade of new business events on replay.

## Authoritative Agent Delegation policy

### Scope and storage

Persist typed endpoint-policy authoring for the target host and gateway instance.
The initial scope is the same instance selected for publication; avoid introducing
environment-wide inheritance of endpoint requirements in this change. Reuse an existing suitable LLM
policy aggregate if the implementation inventory identifies one. Otherwise add
one narrowly scoped authoring aggregate, with its own event-store version.

The implementation must document the selected table, aggregate ID convention,
event names, and command/query schemas before migration. An authoring aggregate
is distinct from derived agent bindings and is not another assignment store.

The authoring record contains:

- Host and gateway instance identity.
- Supported endpoint requirements: whether an agent workload token is required.
- An explicit reference to the host-and-environment user issuer/audience profile.
- Schema version and normal authoring concurrency/version metadata.

Endpoint requirements remain per instance, but the user issuer/audience profile
is shared by host and logical environment, matching the current binding compiler's
scope. Reuse an existing suitable security profile or introduce an authoritative
profile at that scope. All instance references and Agent policies in that scope
must agree; reject conflicts on authoring validation and recheck at publication.
The compiler must receive the target instance's endpoint policy and the referenced
environment profile explicitly. It must not infer trust settings from generated
output or require an active Agent registration to obtain the user profile.

### UI

Add an **Agent Delegation** tab to LLM Model Control Plane with gateway instance
selection and separate editable and derived sections.

| Endpoint | Editable setting |
|---|---|
| `/v1/chat/completions@post` | Require agent workload token |
| `/v1/responses@post` | Require agent workload token |
| `/anthropic/v1/messages@post` | Require agent workload token |

Explain that optional means an authenticated direct-user request may proceed.
It does not disable user verification, permit an invalid supplied workload token,
or grant direct users access to agent-bound aliases.

Show bindings, client IDs, registration versions, Agent policy digests, and alias
restrictions read-only, with navigation to their owning records. Do not provide
raw binding edits. Save authoring changes separately from publishing and activating
a runtime snapshot. Show unpublished changes and expected runtime consequences.

For the local mixed-use gateway, the intended chat-completions setting is optional:
public alias tests use a user credential; Tech Support sends both credentials.
This is a deployment choice, not a universal security default.

### Validation

Require host-scoped authorization for edits. Check instance ownership, supported
endpoints, boolean types, and issuer/audience compatibility. Use optimistic
concurrency on the authoring aggregate. Retain strict JWT expiry, audience,
issuer, TLS, workload registration, and model-binding checks.

For every authored endpoint, emit a non-null `agentDelegation` profile containing
that endpoint's explicit boolean, even with zero active bindings or no previous
publication. Reject preview/publish if the user profile is unavailable or compilation
would emit null or omit an authored endpoint. In particular, required policy must
never disappear into legacy single-token parsing. Removing the last binding must
preserve the profile and endpoint requirements.

Parsing behavior remains selected by trusted route configuration. Never retry
with the legacy parser after new-profile verification fails. An optional endpoint
remains in the new profile; it is not the same as removing the endpoint entry.

## Publication lifecycle

1. Read authoritative model, alias, registration, Agent publication, and endpoint
   policy records into a consistent candidate.
2. Derive bindings and the complete managed property set.
3. Include authoring versions and compiler-relevant defaults in the source
   fingerprint. Include all generated values in the property-set digest.
4. Present a read-only preview and differences from the target instance.
5. On publish, regenerate and check the reviewed digest and source versions.
   Reject stale previews rather than overwriting concurrent changes.
6. Append one publication event containing the validated complete revision and
   source provenance.
7. Apply its generated rows, ownership, and publication status in one projection
   transaction. Preserve idempotency on replay and transactional failure recovery.
8. Create and activate the runtime snapshot only after complete projection.
   A runtime reload is a separate observable step; publication success alone
   does not prove that a running gateway has loaded the revision.

Revision identity and reuse must include the source fingerprint as well as host,
environment, and property-set digest; the source fingerprint must include target
instance identity and the referenced profile version. Update the deterministic ID,
reuse query, and manifest contract together. The current manifest contains only
`schemaVersion` and `propertySetDigest`, while `llm_gateway_publication_t` enforces
`UNIQUE (host_id, environment, manifest_digest)` in `portal-db/postgres/ddl.sql`.
The new versioned manifest must also contain the source fingerprint, and its digest
must cover that field. This preserves the uniqueness constraint while allowing
identical output from different sources; changing only the revision ID and reuse
query would fail on insert. Preserve historical manifests and digests during
migration and restore. Byte-identical
properties from different authoring versions require distinct provenance-bearing
revisions. Deduplicate retries of the same source and output, not different source
histories; replay preserves the recorded revision identity.

If authoring changes between publication and activation, the reviewed publication
remains immutable. Operators can activate that exact revision or publish a newer
one. The UI must identify which revision is active and which is pending.

## Instance Config behavior and versioning

Display managed properties with the owning product, publication ID/revision,
active snapshot, and an **Edit in LLM Model Control Plane** action. Alternatively,
open the same domain editor in place, submitting the same domain command.

Reject ordinary ConfigInstance create/update/delete commands for an actively
publication-owned property. Enforce this on the server before appending an event;
UI restrictions alone are insufficient. Publication claiming a previously generic
property must validate the expected value/version and ownership transactionally.

Separate two meanings currently conflated by `aggregate_version`:

- Event-store aggregate version: concurrency position of the owning command stream.
- Projection/publication revision: provenance of the generated value.

Do not increment a ConfigInstance event version merely because a publication
updates its projection. Use publication ownership metadata for generated revision
tracking. Audit query contracts, snapshot comparison, generic editors, and
publication fingerprinting before deciding whether the existing column can be
retained with narrower semantics or requires a separate field.

Historical generic streams remain historical; managed ownership does not rewrite
them. Deliver an explicit ownership-release command for transfer back to generic
operation. It must authorize the host/instance, check expected publication ownership
and versions, deactivate ownership rows, and establish a valid generic event-stream
baseline transactionally. Preserve historical publication provenance. Reject stale
release attempts and make replay idempotent. A subsequent publication must explicitly
reclaim ownership with the same concurrency checks as an initial claim.
Release must not happen implicitly when a publication or authoring record disappears.

## Global snapshot export and import

Export must distinguish unmanaged properties from publication-managed output.

### Managed restore

Restore authoritative control-plane records, exact published material and
provenance, and ownership. Reconstruct managed properties once through the
publication restore path. Exclude those same rows from independent generic
ConfigInstance-created event generation in this mode.

Do not recompile historical publications with today's compiler defaults during
restore: that could silently change the deployed policy. Preserve the exported
revision and digest, validate its supported schema, then allow a later explicit
publication to adopt new defaults.

Restore dependencies before publication activation. Imported event stream
versions and references must follow the import contract rather than copying a
projection counter as an assumed event-store position. Repeated import must not
create duplicate publications or multiply property writes.

### Detached restore

If a deployment intentionally needs configuration without the control plane,
provide an explicit detached mode. Materialize plain configuration, omit active
publication ownership, and establish valid generic aggregate histories. Restoring
into an already managed target must use the ownership-release contract in the
restore transaction so no active ownership rows remain. Explain
that subsequent control-plane publication is not automatically synchronized.

Downloaded immutable `values.yml` remains a valid offline runtime input. This
proposal does not introduce policy expiration or require Portal availability for
an already configured agent or gateway to continue operating.

### Legacy snapshots

Define a versioned compatibility path for exports containing both generic property
rows and publication ownership. Reconcile them by verified value/digest and
provenance; reject conflicting content with an actionable report. Do not silently
choose whichever event happens to arrive last.

## Development setup and existing data

Apply the database patch before starting the command/query services. The domain
editor, ownership-release command, and managed-write protection are part of the
same implementation. Protection is always enforced, with no rollout flag.

For existing data, inventory managed properties, ownership records, generic event
streams, and projection versions. Bootstrap authoritative authoring from each
instance's last accepted configuration, preserving endpoint requirements and
recording provenance through supported domain/import commands. Reject conflicts;
do not lower versions or fabricate historical edits. Normal compilation never
uses previous generated endpoints or issuer/audience values as authoring inputs.

Publish the desired endpoint policy, activate a new snapshot, and verify public-user
and agent-mediated inference. Tests cover source-aware revision identity, ownership
release, and managed, detached, and legacy export/import handling.

Rollback retains immutable prior publications and restores an explicitly selected
compatible revision. Generic editing requires explicit ownership release.

## Verification matrix

| Scenario | Required result |
|---|---|
| Save endpoint choice and regenerate | Candidate reflects authoritative authoring; prior generated JSON cannot override it |
| Two instances in one environment | Independent endpoint choices; shared issuer/audience reference; conflicting profiles rejected |
| Fresh instance or last binding removed | Non-null profile retains every authored endpoint; missing trust profile rejects publication; required route rejects user-only requests |
| Concurrent edits/publications | Stale version or reviewed digest rejected; no lost update |
| Optional endpoint, valid user, public alias | Direct buffered and streaming inference succeed |
| Required endpoint, missing workload token | Request rejected before provider dispatch |
| Invalid supplied workload token on optional endpoint | Rejected; no user-only or legacy fallback |
| User-only request for agent-bound alias | Existing non-disclosing model authorization failure retained |
| Valid dual-token Tech Support request | Correct agent binding, alias authorization, billing identity, and audit provenance |
| Generic edit/delete of managed property | Clear domain-owned error and navigation guidance; no event appended |
| Publication projection failure midway | Transaction rolls back all managed changes; no partial snapshot activation |
| Required → optional → required with identical final output | Final revision records the new source fingerprint; older provenance is not reused |
| Explicit ownership release and retry | Ownership deactivated with valid generic baseline; stale release rejected; replay idempotent |
| Duplicate event/replay | Same values and ownership; no extra revisions or writes |
| Export/import managed publication | Same values/digests and ownership; one materialization path |
| Import then domain edit and republish | Valid versions; no ConfigInstance mismatch |
| Legacy conflicting snapshot | Explicit rejection/report rather than last-writer-wins behavior |
| Detached restore | No residual managed ownership; generic edits have valid event histories |
| Portal/config-server unavailable | Accepted runtime configuration continues without policy renewal |

Run `make llm` for public-user inference and `make genai-chat-ui` for the real
browser-to-Agent-to-LLM path in `light-portal-test`. A browser pass alone does not
prove direct-user access, export/import correctness, or durable audit delivery.
Use transactional PostgreSQL integration tests for projection and restore cases.

## Delivery phases

1. **Contracts and inventory:** settle authoring/profile ownership, aggregate and
   schema changes, managed-version semantics, and import compatibility fixtures.
2. **Authoring and UI:** commands/queries, Agent Delegation editor, source fingerprint,
   managed-edit guards and ownership release, removal of previous-output compilation
   inputs, non-null profile validation, and links from Instance Config.
3. **Projection and restore:** migration, atomicity/idempotency tests, version semantics,
   source-aware revision identity, ownership-aware global snapshot round trips
   and detached mode.
4. **Local qualification:** publish reviewed endpoint settings, activate/reload,
   pass direct buffered/streaming and GenAI Chat tests, verify audit delivery,
   and exercise rollback.

Completion requires both configuration paths to remain consistent after replay
and export/import, not merely a successful edit or one successful model request.
