# Portal-Managed MCP Tool Access Control

## Status

- **Decision state:** Proposed
- **Owners:** Light Portal and Light Gateway maintainers
- **Design date:** 2026-08-23
- **Scope:** Caller authorization and response filtering for Portal-managed MCP
  tools published to `light-gateway`

## Purpose

Portal users can manage access rules and permissions for API endpoints from
API Admin, but a standalone tool created in Tool Admin has no equivalent
access-control workflow. This forces operators to either maintain gateway-local
configuration or add the tool name to `access-control.skipPathPrefixes`.

Neither is an acceptable steady state. Local files override config-server
snapshots, and a skipped MCP tool bypasses both request authorization and
response filtering.

This design adds first-class Tool Access Control to Light Portal while keeping
the existing Light Gateway policy format and enforcement behavior.

## Current Contracts

### Gateway Tool Identity

Every published tool has an authorization endpoint key. An explicit tool
`endpoint` is used when present; otherwise the gateway derives the key as
`{path}@{method}`. It does not derive the key from the tool name. Because
`path` defaults to an empty string, a workflow-backed tool with neither
`endpoint` nor `path` resolves to `@call`; multiple such tools therefore
collapse onto the same policy key. Managed tools must not rely on that default.
Explicit logical identities may use names such as:

```text
customer_360@call
workflow_mcp_smoke@call
```

The same endpoint key is used for:

- `tools/call` request authorization;
- MCP response filtering;
- optional `tools/list` visibility filtering;
- policy logs, metrics, and audit context.

The endpoint key, not `toolId` or `stableToolRef`, is the runtime lookup key in
`rule.endpointRules`.

Three adjacent tool identities must remain visible and distinct:

- `endpointKey` selects the access-control endpoint rule;
- `authorizationToolName`, when configured in `toolMetadata`, is the tool name
  used by call authorization, response filtering, and CEL list authorization;
- `endpointName` is the downstream MCP backend operation name.

The public tool `name` remains the lookup name for `tools/call` and is currently
used by permission-mode `tools/list` authorization. Portal preview must display
all four values so an operator can see any divergence.

### Gateway Policy Format

The gateway already accepts tool policies through the standard access-control
snapshot:

```yaml
rule.endpointRules:
  customer_360@call:
    req-acc:
      - req-access-light-portal.lightapi.net
    permission:
      roles: admin customer-service-agent
```

No new gateway policy document or tool-specific runtime handler is required.
`tools/call` remains the final enforcement point even when `tools/list`
visibility filtering is enabled.

### Portal API Access Management

Endpoint Access Overview manages rules, permissions, and response filters for
an `api_endpoint_t` record. The current permission and filter projections are
therefore API-endpoint-centric. API access publication compiles those records
into `rule.endpointRules` and `rule.ruleBodies` for a target gateway instance.

### Portal Tool Management

A standalone Tool Admin record can have no `endpointId`. Workflow-backed tools
created directly in Tool Admin commonly have this shape. They can be published
into `mcp-router.tools`, but they cannot enter Endpoint Access Overview and
cannot contribute access rules to the gateway snapshot.

Workflow Tool Access is a separate security boundary. It grants a workflow
definition permission to use a pinned dependency. It does not authorize an end
user or agent to invoke a gateway-published MCP tool.

### Current Storage Reality

`rule.endpointRules` and `rule.ruleBodies` are registered as `map` properties.
Portal currently contains more than one resolution implementation, so the
physical table layout alone does not determine effective behavior.

The canonical `portal-db` `create_snapshot` procedure copies only active
instance, instance-API, instance-app, instance-app-API, association, and lower
inheritance rows. It combines the four instance-scoped sources into one
`InstancePool`; map properties are merged with `jsonb_object_agg` and recorded
as source level `instance_merged`. The runtime-config query used by the deployed
Portal service likewise reads active rows from all four instance scopes, and
the config-server assembler merges map values before generating `values.yml`.

`ConfigPersistenceImpl.insertEffectiveConfigSnapshot`, however, contains an
alternate Java resolver that omits active filters and applies
`MAX(effective_value)` across per-API groups. That path would select one whole
JSON value lexicographically rather than perform the canonical cross-API merge.
It must be removed, delegated to `create_snapshot`, or brought under the same
contract before carrier migration is considered portable across deployments.
Its presence is a resolver-parity defect; it is not evidence that the current
loc gateway is dropping 41 API policies.

On 2026-08-23, the running gateway's `host`, service ID, and `loc` environment
tag resolved to `portal-bff-loc`. That instance had 42 active instance-API rows
for each rule property. The per-API endpoint maps contributed 831 distinct keys
and the instance row contributed one more. Both the running gateway cache and
the latest database snapshot contained all 832 keys; the snapshot reported
source level `instance_merged`. The loc deployment therefore has multiple
contributors but is not currently suffering the Java `MAX` winner loss.

The same deployment explicitly sets `access-control.enabled: true` and
`defaultDeny: true`, then skips `workflow_mcp_smoke` and `customer_360`. It is a
protected instance with two local bypasses, not an instance masked by the
shipped disabled default.

The current API access compiler is scoped to one `(hostId, apiId, apiVersion)`
and replaces the complete endpoint-rules and rule-bodies values on its selected
`instance_api_property_t` rows. Whole-property replacement is therefore a
hazard within one contributor row, while the canonical snapshot/runtime merge
retains unrelated API contributors. It still lacks explicit endpoint ownership,
duplicate-key conflict rejection, and a publication event describing which
source owns each merged key. Those are pre-existing API publication gaps, not
hazards introduced only by Tool Access.

The canonical database can mechanically merge instance-pool maps, but that is
not sufficient ownership control: it cannot reject two sources claiming the
same endpoint key, and a standalone tool has no natural instance-API carrier.
The map branch also calls `jsonb_object_agg` without `ORDER BY`. PostgreSQL
keeps one value for a duplicate key, so two contributors claiming the same
endpoint can produce an order-dependent winner. Portal must reject that
collision before it reaches the resolver and compute the publication digest
from its own canonical, key-sorted pre-merge; digest determinism must not be
inherited from database input order.

As defense in depth, the database merge should use a documented total order.
Ordering only by `update_ts` is insufficient when timestamps tie; the order
must also include stable source rank and source identity. That deterministic
tie-break still does not establish ownership or make duplicate keys valid.
Portal must therefore merge every active API and tool contribution with
explicit ownership before writing one canonical property value.
The existing `mcp-router.tools` publisher is the precedent: it performs an
identity-keyed `mergeExistingTools`, writes the authoritative result to
`instance_property_t`, and retires legacy per-API property rows.

## Problem Statement

The control plane has two independently working halves:

1. Tool publication creates the MCP catalog entry.
2. API access publication creates endpoint access rules.

There is no Portal-owned connection between a standalone tool and the access
rules for its published endpoint key. As a result:

- Tool Admin cannot assign roles, groups, positions, attributes, or users;
- Tool Admin cannot attach `req-acc` or `res-fil` rules;
- policy publication cannot prove that a protected tool has a matching rule;
- tool lifecycle changes can leave manually maintained policy keys stale;
- operators may use `skipPathPrefixes`, which bypasses enforcement;
- `tools/list` visibility and `tools/call` authorization can drift.

## Goals

- Add an **Access Control** action to Tool Admin.
- Reuse the established Endpoint Access Overview user experience.
- Give API endpoints and tools one logical access-target contract.
- Publish tool rules into the existing `rule.endpointRules` and
  `rule.ruleBodies` properties.
- Use the exact endpoint key produced by tool publication.
- Keep `defaultDeny: true` as the safe fallback.
- Support roles, groups, positions, attributes, users, request rules, response
  filters, and list visibility.
- Make create, update, retirement, replay, and republish deterministic.
- Keep tenant, instance, environment, and publication ownership explicit.
- Remove the need for gateway-local tool bypasses.

## Non-Goals

- Replacing Light Gateway's access-control runtime.
- Combining caller authorization with Workflow Tool Access grants.
- Changing workflow binding, digest, or environment validation.
- Making MCP authentication optional.
- Creating a second tool-only gateway policy format.
- Treating `tools/list` filtering as a substitute for `tools/call`
  authorization.
- Requiring users to model every standalone tool as a real HTTP API.

## Proposed Control-Plane Model

### Access Target

Introduce a Portal access-target abstraction:

```text
AccessTarget
  hostId
  accessTargetId
  targetType        API_ENDPOINT | TOOL
  targetId          endpointId | toolId
  endpointKey       exact Light Gateway policy key
  sourceVersion     source aggregate version
  active
```

An access target is a control-plane identity. The gateway continues to receive
only endpoint-keyed rules and does not need to understand `targetType` or
`targetId`.

For an API endpoint:

```text
targetType  = API_ENDPOINT
targetId    = api_endpoint_t.endpoint_id
endpointKey = the published path-or-logical-operation key
```

For a tool:

```text
targetType  = TOOL
targetId    = tool_t.tool_id
endpointKey = the authorization endpoint from the compiled tool publication
```

`accessTargetId` should be stable and host-scoped. It must not change when the
display name or description changes.

### Endpoint-Key Authority

The tool publication compiler is authoritative for `endpointKey`. Tool Access
Control must consume the compiled authorization endpoint rather than
independently reconstructing it from a mutable display name.

Portal should require a stable explicit endpoint key before a tool becomes
access-policy-ready. Migration must read the endpoint key from the compiled
tool publication; it must never reconstruct it from a name. Portal may offer
to pin a non-empty compiled `{path}@{method}` value after showing it in preview.
It must not auto-pin `@call` when the path is empty, because that value is not
tool-unique.

Publication must reject:

- an empty endpoint key;
- an endpoint key without the expected operation suffix;
- exact duplicate keys in one gateway instance;
- a prefix or path-template rule that would shadow the endpoint key;
- a tool and API endpoint claiming the same key with different policy owners;
- a policy whose source version does not match the selected tool publication.

`stableToolRef` remains the immutable tool identity used by workflow bindings
and grants. It must not be substituted for the endpoint key in gateway policy.

### Permissions and Rules

Access-target assignments should represent the existing principal dimensions:

- roles;
- groups;
- positions;
- attributes and attribute values;
- users.

Rule bindings should support:

- `req-acc` request authorization;
- `res-fil` response row filtering;
- `res-fil` response column filtering;
- list visibility derived from the call permission block for protected tools;
  and
- one canonical, audited allow-all `req-acc` rule for public tools.

An explicit `visibility` block short-circuits claim evaluation in both
`tools/list` and the early claim-only stage of `tools/call`. The first release
therefore must derive visibility from permission rather than accept an
independently broader value. If independent visibility is added later, the
publisher must prove `visibility` is a subset of `permission` for every
principal dimension under the same claim mappings.

The logical model should be shared by API endpoints and tools. A staged schema
migration may preserve the existing endpoint-specific tables while generic
access-target projections are introduced, but new Tool Admin behavior must not
create a second gateway policy representation.

## Portal User Experience

### Tool Admin

Add an **Access Control** row action for active, publishable tools. Keep it
visually and semantically separate from **Workflow Access**:

| Action | Security boundary |
|--------|-------------------|
| Access Control | Which users and agents may discover or invoke this published tool |
| Workflow Access | Which workflow definitions may use this tool as a pinned dependency |

The Tool Access Overview header should display:

- tool name and `toolId`;
- `stableToolRef`;
- execution placement;
- explicit authorization endpoint key;
- effective `authorizationToolName`;
- downstream `endpointName`;
- current tool version and aggregate version;
- selected gateway instance and environment;
- policy readiness and publication status;
- last published snapshot revision.

### Reused Access Panels

Reuse the existing access-management panels with an `AccessTargetContext`
instead of an API-only route context:

```text
hostId
targetType
targetId
endpointKey
instanceId
environment
```

The overview should expose:

- request rules;
- role permissions;
- group permissions;
- position permissions;
- attribute permissions;
- user permissions;
- row filters;
- column filters;
- tools-list visibility;
- preview and publication status.

The components may retain API-specific adapters during migration, but commands
and queries should use the generic access-target identity at their boundary.

### Readiness States

Tool Admin should show one of these states:

| State | Meaning |
|-------|---------|
| `UNCONFIGURED` | No access target or request rule exists |
| `CONFIGURED` | Policy exists but is not published to the selected instance |
| `PUBLISHED` | Active snapshot has the exact owned rule and matching tool and policy source versions |
| `STALE` | Desired property values differ from the current snapshot, or the reviewed base values changed before apply |
| `CONFLICT` | Another owner or runtime pattern collides with or shadows the endpoint key |
| `RETIRED` | Tool or access target is inactive and its policy is being removed |

State precedence is `RETIRED`, `CONFLICT`, `STALE`, `UNCONFIGURED`,
`CONFIGURED`, then `PUBLISHED`; the UI should retain all secondary reasons.
Portal must not infer that an unconfigured tool is non-callable from
`defaultDeny` alone: the gateway may fall back to a prefix or path-template
rule. A protected publication requires an exact owned endpoint rule and must
block every conflict, stale source, and shadow match.

## Commands, Queries, and Events

The exact Portal service names can follow existing naming conventions, but the
contract should cover these operations:

### Queries

- get access overview by target;
- list rules and principal assignments;
- preview compiled endpoint rules;
- list target gateway instances and environments;
- compare source versions with the active snapshot;
- report endpoint-key ownership conflicts.

### Commands

- create or reactivate an access target;
- set or retire a stable endpoint key;
- assign or remove principals;
- attach or detach request and response rules;
- preview list visibility derived from permission;
- publish access policy to a gateway instance;
- retire tool-owned policy from an instance.

Commands must use expected aggregate versions. Events must carry `hostId`,
`targetType`, `targetId`, `accessTargetId`, and the source aggregate version so
replay cannot attach a policy to the wrong tenant or stale tool revision.

## Publication Design

### Inputs

A tool access publication candidate contains:

- the selected tool publication and binding;
- the compiled tool endpoint key;
- the effective tool name, `authorizationToolName`, and `endpointName`;
- tool ID, stable reference, version, and aggregate version;
- target instance and environment;
- access-target rules and principal assignments;
- current `rule.endpointRules` and `rule.ruleBodies` ownership metadata;
- the resolved property IDs, active flags, and `value_type` registrations;
- the current snapshot ID and digest used as the review baseline; and
- the existing carrier values, digests, and aggregate versions used for the
  compare-and-apply guard.

### Compilation

Compile a tool access target into the existing gateway shape:

```yaml
rule.endpointRules:
  customer_360@call:
    req-acc:
      - req-access-light-portal.lightapi.net
    permission:
      roles: admin customer-service-agent
      groups: customer-operations
```

Rule bodies remain deduplicated by rule ID in `rule.ruleBodies`. Endpoint maps
and rule ID lists must be deterministically ordered before digesting or
publishing. Protected-tool list visibility is derived from the permission block
in the first release; it is not a separately editable allow set. A public tool
instead binds the canonical public rule described below.

### Ownership and Merge Rules

Portal must merge all active contributors into one physical carrier per target
gateway instance. The authoritative homes are single `instance_property_t`
rows for `rule.endpointRules` and `rule.ruleBodies`, keyed by
`(hostId, instanceId, propertyId)`. API and tool publishers must not write
independent complete values at different source levels and expect the config
server to merge them.

Per-source ownership belongs in Portal publication and binding records, not in
separate config-property rows. Each compiled endpoint contribution records:

```text
sourceType       API_ENDPOINT | TOOL
sourceTargetId
sourceVersion
endpointKey
publicationId
instanceId
ruleBodyIds
ruleBodyDigests
```

The instance policy compiler must:

1. load every active API-endpoint and tool contribution for the instance;
2. replace only the selected source's desired contribution in Portal state;
3. reject exact ownership collisions and runtime pattern shadowing;
4. retain unrelated API and tool contributions;
5. remove only endpoint keys owned by a retired source;
6. compute the union of referenced rule IDs and reject one rule ID with
   different bodies or digests;
7. retain each rule body while at least one active endpoint references it and
   retire it only after its last reference disappears;
8. deterministically rebuild the complete endpoint and rule-body maps;
9. assert that both selected config properties are active maps with the
   expected property IDs;
10. in one publication transaction, write both canonical instance values and
    deactivate every legacy instance-API and instance-app-API row for those
    properties against the accepted target revision.

This follows the identity-keyed merge shape already used by
`mergeExistingTools`. Resolver convergence is a strict predecessor to carrier
migration, not parallel work in the same rollout. Every supported snapshot and
runtime-config path must honor inactive rows and the canonical instance-pool
merge. The deployed `create_snapshot` procedure and runtime-config query meet
that requirement; the alternate Java `MAX` resolver does not. It must delegate
to the canonical procedure, implement identical semantics, or be proven
unreachable and removed. Physical deletion is not an acceptable compatibility
workaround because it discards the audit trail.

The migration transaction absorbs all active legacy contributions, writes the
merged instance carriers, and deactivates the higher-priority legacy rows as
one publication. Only the snapshot generated after that transaction may be
used for verification. Readiness permanently asserts that no active
`instance_api_property_t` or `instance_app_api_property_t` row exists for
`rule.endpointRules` or `rule.ruleBodies` on the target instance. The legacy
API publisher must be changed in the same phase so it cannot recreate one.

The tool publisher's `apiVersionId` remains selection scope and provenance; it
does not make an API property row the owner of standalone Tool Access.

### Runtime Matching and Conflict Analysis

Gateway request lookup tries an exact endpoint key first, then prefix and path
template matches, choosing the longest matching pattern. A missing `@method`
is treated as `@call`. Portal conflict analysis must reproduce those rules,
not merely compare strings.

For managed Tool Access, an exact rule owned by that access target is required
for `PUBLISHED`. Pattern fallback is legacy runtime compatibility and cannot
satisfy readiness. Preview must show every exact collision, ancestor-prefix
match, path-template match, implicit `@call` match, and the rule that the
gateway would select. This avoids disagreement with the gateway's exact-only
`validate_request_policy` readiness check.

### Effective Policy Context and Digest

Preview must load these effective instance settings from the target snapshot:

- `access-control.enabled`, `defaultDeny`, `accessRuleLogic`, and
  `defaultInclude`;
- `skipPathPrefixes` and its prefix relation to the endpoint key, public tool
  name, and `authorizationToolName`;
- `toolsListAccessControl.mode`, `unknownRuleFallback`,
  `maxCelEvaluations`, and claim mappings.

The preview digest must include those values, compiled tool identities, all
source owners and versions, the endpoint shadow report, referenced rule-body
digests, property IDs and value types, legacy-row inventory, property actions,
and accepted config revision. Identical property JSON is not an identical
authorization outcome when the instance-global settings or winning source
level differ. Portal computes this digest before persistence from a canonical,
key-sorted ownership merge, then requires the generated snapshot to reproduce
the same JSON and digest. The database aggregate is a verification target, not
the digest authority.

### Desired State and Promotion Behavior

Tool and access-policy desired state share one reviewed Portal workflow. The
review page compares the existing `instance_property_t` values with the proposed
canonical values and shows additions, removals, changed rules, public/protected
mode changes, and unrelated entries that will be retained. Applying the review
writes the catalog and policy carrier values with their expected aggregate
versions; a concurrent change rejects the apply and requires the comparison to
be refreshed.

Writing `instance_property_t` changes desired state but does not change a
running instance. Portal then creates and validates a candidate snapshot from
that desired state. Only an explicit promotion that makes the candidate current
activates the change. If property persistence, snapshot generation, validation,
or promotion fails, the previous current snapshot remains authoritative. Portal
reports the tool policy as `PUBLISHED` only after the current snapshot contains
the reviewed tool and policy values. The control plane must never temporarily
add the tool to `skipPathPrefixes` to bridge these stages.

## Gateway Runtime Behavior

Protected tools reuse the existing endpoint-keyed authorization algorithm. The
gateway needs two bounded extensions: recognize the canonical public rule as
visible in permission-mode `tools/list`, and resolve the restricted nested
response path before applying the existing row or column filter actions. The
normal `tools/call` rule engine executes the public rule's constant-true CEL
condition. Neither extension introduces a second policy source or bypasses
response filtering.

For `tools/call`:

1. Resolve the configured tool.
2. Resolve its authorization endpoint key.
3. Apply `access-control.enabled` and `skipPathPrefixes` gates.
4. Look up the exact endpoint rule, then any compatible prefix or path-template
   rule using longest-pattern precedence.
5. Evaluate `req-acc` with the authenticated principal, permission metadata,
   headers, and tool arguments.
6. Invoke the tool only when allowed.
7. Apply configured `res-fil` rules to the normalized result.

For `tools/list`, the gateway supports `none`, `permission`, and `cel` modes.
Protected deployments should use permission or CEL mode and must show the
effective choice in Portal. CEL evaluates no more than `maxCelEvaluations`;
tools after that bound are hidden. Call authorization remains mandatory because
list visibility can be stale or argument-insensitive.

`skipPathPrefixes` is a prefix test applied to both the endpoint key and a tool
name. Today permission-mode list filtering passes the public tool name, while
CEL list filtering, call authorization, and response filtering pass
`authorizationToolName`. Portal must test every skip prefix against all three
values and block publication on any match. The gateway should separately align
permission-mode list filtering to `authorizationToolName`; until then,
acceptance tests must cover both list and call paths when the two names differ.

The loc value `customer_360` demonstrates why this is a prefix check, not an
equality check: it bypasses `customer_360`, `customer_360_v2`, and every other
endpoint key or authorization tool name beginning with that string.

For response filtering, any unfilterable MCP result, missing rule body, rejected
rule, rule-execution error, missing filtered body, serialization/application
failure, or top-level row denial must suppress the original payload and return
a bounded MCP access-control error. No error path may return the unfiltered
backend result.

## Security Requirements

- Protected gateway instances use `access-control.enabled: true`.
- Protected gateway instances use `defaultDeny: true`.
- Preview and readiness include `accessRuleLogic`, `defaultInclude`, list mode,
  unknown-rule fallback, CEL limit, and effective claim mappings.
- Portal publication never creates `skipPathPrefixes` for managed tools.
- A prefix match against the endpoint key, public tool name, or
  `authorizationToolName` is reported as a policy-readiness error.
- Every mutation and publication is host-scoped and aggregate-versioned.
- Cross-tenant target IDs and endpoint ownership are rejected.
- An explicit access policy cannot silently fall back to another endpoint key.
- Missing exact rules, unknown rule bodies, stale source versions, ownership
  collisions, and pattern shadowing fail publication closed. At runtime a
  missing request-rule body denies a call; permission-mode list visibility may
  apply `unknownRuleFallback`, so list visibility alone is never proof of call
  authorization.
- `tools/call` always reauthorizes regardless of `tools/list` visibility.
- Response filtering uses the same endpoint key and principal as request
  authorization.
- Audit records identify the user, target, endpoint key, source versions,
  instance, environment, and publication digest.
- Secrets and bearer tokens are never persisted in access-target events or
  policy snapshots.

## Lifecycle Behavior

### Tool Update

Description and metadata changes do not change the endpoint key. A tool version
or access-relevant schema change marks the publication stale until republished.

### Tool Rename

A display-name change must not implicitly rename a managed endpoint key. An
endpoint-key change is an explicit migration that publishes the new denied or
protected key before retiring the old key.

### Tool Retirement

Retiring a tool deactivates its access target and removes only its owned
endpoint-rule contribution from selected gateway instances. Shared rule bodies
are retained while another active endpoint references them. Publication must
reject the same rule ID supplied with different body content; retirement
removes a body only after its reference set becomes empty.

### Replay

Projection replay must produce the same access target, ownership records,
compiled endpoint map, and digest. Replayed stale events cannot overwrite a
newer aggregate version or active publication.

## Migration Plan

### Phase 0: Demonstration Bridge

For an immediate demonstration, operators may create an internal API version
with `CALL` endpoints whose endpoint strings exactly match the tool keys, then
use Endpoint Access Overview and the current API access publisher. This is a
temporary authorization projection, not the final Tool Admin experience.

This bridge is destructive on a shared property row today: the API publisher
compiles one API version and replaces the complete `rule.endpointRules` and
`rule.ruleBodies` values. Phase 0 is permitted only on an isolated demonstration
instance where those property rows have no other owner. It must not be used on
an instance with another API or tool policy contributor. Shared-instance
rollout waits for the instance-level merger in Phase 1B.

Do not remove a tool from `skipPathPrefixes` until the effective snapshot has
its exact owned `req-acc` and permission entry and no runtime shadow conflict.

### Phase 1A: Snapshot Resolver Convergence

- Make every supported snapshot and runtime-config resolver use the canonical
  active-row and instance-pool merge semantics. Prefer one implementation by
  delegating Java snapshot creation to `create_snapshot`.
- Extend `config_snapshot_empty_collection_setup.sql` and its snapshot tests
  with active and inactive cases for every override priority, including
  instance-app-API, instance-API, instance-app, instance, product version,
  environment, product, and default registrations. Cover fallthrough to the
  next priority and preservation of intentional empty maps and lists.
- Add a parity gate that feeds the same contributors to every remaining
  resolver and requires identical canonical JSON, source level, and digest.
- Give the database map aggregate an explicit total order by update timestamp,
  stable source rank, and source identity; add a duplicate-key fixture proving
  that the resolver is reproducible even though Portal rejects the publication.
- Deploy and prove that convergence before enabling any carrier-migration
  command on a deployment where the alternate Java path is reachable.

### Phase 1B: Authoritative Instance Merger and Access-Target Read Model

- Establish authoritative `instance_property_t` carrier rows for endpoint
  rules and rule bodies.
- Assert active `map` registrations for both properties on the target host.
- Load all API and tool contributors, merge by owned identity, and atomically
  write the carriers while deactivating legacy higher-priority rows.
- Prevent the API publisher from recreating per-API rows for these properties.
- Add rule-body reference tracking and conflicting-body detection.
- Add the generic access-target identity and query contract.
- Backfill API endpoint access targets.
- Create tool targets from compiled gateway publication bindings.
- Detect exact collisions, pattern shadowing, and stale source versions.
- Keep existing API access commands working through adapters.

### Phase 2: Tool Admin Access Control

- Add the Tool Admin Access Control action and overview.
- Reuse permission, rule, and filter panels.
- Add the bounded, schema-validated nested response target path to row- and
  column-filter editing and preview.
- Keep Workflow Access as a separate action.
- Add readiness and publication status.

### Phase 3: Tool Policy Publication

- Compile tool targets into `rule.endpointRules`.
- Use the Phase 1B ownership and deterministic instance merger.
- Compare proposed carrier values with current `instance_property_t`, apply
  them with expected aggregate versions, create and validate a candidate
  snapshot, and explicitly promote it to current.
- Compile and digest nested response target paths with their output-schema
  dependency.
- Support retire and replay without deleting unrelated API rules.

### Phase 4: Protected Rollout

- Require Phase 1A resolver qualification before any protected multi-API
  publication. If a deployment can reach the alternate Java `MAX` resolver,
  freeze further API access publication on an already-enabled instance and do
  not enable a new instance until resolver convergence and carrier migration
  succeed. A qualified canonical `instance_merged` deployment, including the
  checked loc instance, is not subject to that winner-swap freeze.
- Even on a qualified instance, do not publish Tool Access or remove a tool
  bypass until Phase 1B carrier ownership and exact-rule validation succeed.
- Configure policies for the demonstration tools.
- Confirm the effective snapshot contains their endpoint keys.
- Remove their `skipPathPrefixes` bypasses.
- Remove the gateway-local `access-control.yml` override when all remaining
  settings are config-server-owned. The gateway loads that file as the complete
  access-control config and consults `values.yml` only when no local file is
  found; the two sources are not merged.
- Enable permission-based `tools/list` filtering where discovery must match
  call permissions.

## Validation Plan

### Persistence and Replay

- API endpoint and tool access targets are host-isolated.
- Duplicate active endpoint keys and runtime pattern shadows are rejected per
  gateway instance.
- Principal and rule mutations enforce aggregate versions.
- Replay is idempotent and preserves source ownership.
- Tool retirement removes only tool-owned contributions.
- A shared rule body survives until its last reference is retired; conflicting
  content for one rule ID is rejected.

### Publication

- Tool access preview shows the exact endpoint key, public name,
  `authorizationToolName`, `endpointName`, compiled policy, effective global
  settings, and shadow analysis.
- Unrelated API and tool endpoint rules survive republish.
- Missing exact `req-acc`, stale tool versions, ownership conflicts, shadow
  matches, and skip-prefix matches block a protected publication.
- Deterministic input produces a deterministic snapshot digest.
- Two contributors claiming one endpoint key produce `CONFLICT` before any
  database write, regardless of whether their compiled rule values are equal.
- Repeated snapshot generation of a deliberately colliding resolver fixture is
  byte-stable under the database total order, while publication of that fixture
  remains forbidden.
- Concurrent publication rejects a review whose base property value, digest, or
  aggregate version is stale.
- Carrier write and legacy-row deactivation share one publication transaction;
  verification uses only a snapshot generated afterward.
- Readiness rejects any active instance-API or instance-app-API row for either
  property on the target instance.
- Every supported resolver ignores inactive association and property rows and
  produces the same instance-pool merge.
- Snapshot tests cover active/inactive behavior and priority fallthrough at
  every override level, including empty map and list values.
- Resolver parity tests require identical canonical JSON and digest; the Java
  `MAX` implementation cannot remain as an alternate result.
- Both properties resolve to active `map` registrations for the selected host;
  changing a registration blocks publication until reviewed.
- After migration, both effective properties have the canonical
  `instance_merged` source level with only the instance carrier contributing,
  and a later API publication cannot recreate a per-API carrier.

### Portal UI

- Standalone tools expose Access Control without requiring `endpointId`.
- Workflow Access and caller Access Control are clearly distinguished.
- Existing API Endpoint Access Overview behavior is unchanged.
- Readiness states explain the exact missing or stale prerequisite.
- Preview and publish actions show the selected instance and environment.

### Gateway

- An authorized caller can list and call the tool.
- An unauthorized caller cannot call the tool.
- Permission-mode `tools/list` hides unauthorized tools.
- The canonical public rule makes a public tool visible and callable for every
  caller that reaches MCP routing, while route-level authentication remains
  authoritative.
- A missing, modified, or mixed public rule fails publication; a missing runtime
  rule body hides the tool and denies the call.
- CEL-mode `tools/list` hides entries beyond `maxCelEvaluations`.
- A direct `tools/call` remains denied even if list visibility is stale.
- Row and column filters apply to top-level and configured nested
  workflow-backed structured content, and missing or mistyped nested targets
  fail closed.
- Every response-filter failure returns an MCP error without exposing the
  unfiltered result.
- Missing endpoint rules deny when `defaultDeny` is true.
- No skip prefix matches a managed endpoint key, public tool name, or
  `authorizationToolName`.
- An unqualified multi-API resolver cannot transition from disabled to
  protected, or accept further protected API publication, until resolver parity
  passes. Tool Access additionally requires the carrier and exact-rule gates.

### Demonstration Acceptance

For `customer_360@call` and `workflow_mcp_smoke@call`:

1. the config snapshot contains request rules and permissions for both keys;
2. no `skipPathPrefixes` value is a prefix of either endpoint key, public tool
   name, or `authorizationToolName`;
3. an allowed principal receives a successful result;
4. a denied principal receives an access-control error before workflow start;
5. `tools/list` behavior matches the configured visibility mode;
6. restart and snapshot regeneration preserve the same behavior without local
   `mcp-router.yml` or `access-control.yml` overrides.

## Alternatives Considered

### Keep Gateway-Local access-control.yml

Rejected. It overrides config-server ownership, is deployment-specific, and
cannot provide Portal audit, preview, lifecycle, or replay behavior.

### Keep skipPathPrefixes

Rejected except as a temporary development bypass. It disables both request
authorization and response filtering.

### Require Users to Create APIs Manually

Useful as a short-term bridge, but rejected as the final user experience. It
creates duplicate lifecycle ownership and makes a standalone tool appear to be
an API solely to reach permission screens.

### Add a Tool-Specific Gateway Policy File

Rejected. The gateway already has the necessary endpoint-keyed rule format.
A second format would create precedence, reload, audit, and migration problems.

### Reuse Workflow Tool Grants

Rejected. Workflow grants authorize definition dependencies and pin versions,
digests, capabilities, and environments. Caller permissions authorize users
and agents invoking an exposed tool. Combining them would weaken both models.

## Resolved Design Questions

### Access-Target Storage Migration

Do not replace the endpoint-specific tables in the first release. Introduce
generic access-target storage and projection adapters, migrate readers and
writers incrementally, and retire the old tables only after API Endpoint Admin
and Tool Admin both use the generic contracts.

The eventual replacement surface is the 16 tables whose authorization data is
keyed directly by `endpoint_id`:

- `api_endpoint_rule_t`;
- `role_permission_t`, `group_permission_t`, `position_permission_t`,
  `attribute_permission_t`, and `user_permission_t`;
- the role, group, position, attribute, and user variants of
  `*_row_filter_t`; and
- the role, group, position, attribute, and user variants of
  `*_col_filter_t`.

The generic model can collapse those into `access_target_rule_t`, a
principal-typed `access_target_permission_t`, `access_target_row_filter_t`, and
`access_target_col_filter_t`, all referencing `access_target_t`. It must retain
attribute values and the optional user validity interval. `api_endpoint_t`
remains the API catalog identity and `api_endpoint_scope_t` remains the OAuth
scope projection; neither is replaced by this migration.

This is a high-impact migration because the 16 tables participate in command,
query, event-replay, snapshot/export, cascade-lifecycle, and Portal UI paths.
Projection adapters keep the tool feature from requiring a flag-day conversion
and provide a period in which generic and legacy query results can be compared.

### Publication Command

Tool and policy publication use one command and one accepted source revision.
The preview presents Access Control as a separate, explicitly approved section
and compares the complete proposed carrier values with the current
`instance_property_t` values. The operator reviews the endpoint key,
public/protected mode, principals, filters, removals, retained unrelated entries,
and every overwrite before accepting the desired-state update. Snapshot creation
and current-snapshot promotion remain explicit subsequent gates in the same
workflow.

### Review Freshness and Promotion

There is no separate field-by-field "policy publication invalidation" action.
The authoritative review artifact is the comparison between the existing and
proposed complete property values, plus their canonical digest and expected
aggregate versions. Any desired access-relevant change appears in that diff. A
display-name or description-only edit that does not alter the compiled values
does not create a policy change.

If either carrier value or its aggregate version changes after the review is
rendered, applying that review is rejected as stale and the user must review a
fresh comparison. Once desired state is written, the current snapshot continues
to govern runtime behavior until a reviewed candidate snapshot is explicitly
made current. The `STALE` readiness state means desired/current snapshot drift,
not automatic revocation of the active policy. Retirement and emergency
disablement remain explicit, audited desired-state changes followed by snapshot
promotion.

### Public Tools

Support an explicit `PUBLIC` access mode in Portal, compiled as a canonical
shared allow-all request rule rather than a new endpoint marker:

```yaml
rule.endpointRules:
  public_tool@call:
    req-acc:
      - allow-public-access

rule.ruleBodies:
  allow-public-access:
    common: Y
    ruleId: allow-public-access
    ruleName: Allow public access
    ruleType: req-acc
    accessControlEffect: public
    conditionLanguage: cel
    conditionSecurityProfile: strict
    expression: "true"
```

The compiler owns the rule ID and exact body digest; users cannot edit or
substitute it. A missing or modified body fails publication and runtime access
closed. Permission-mode `tools/list` needs a small gateway change to recognize
only the canonical rule ID and complete rule shape as visible. It must not infer
public access from arbitrary constant expressions, rule names, or an
`accessControlEffect` value alone. A public endpoint cannot combine this rule
with principal-specific request rules or permission metadata. The normal call
path still executes the rule, so request auditing and configured response
filters remain active.

Public means no principal-specific per-tool restriction; it does not bypass
authentication enforced before MCP routing. If the MCP route permits anonymous
access, the tool is callable anonymously. Public access must not use
`skipPathPrefixes`, an empty permission block, or an independently broad
`visibility` block. Publication requires explicit public-access approval and
records the approver and reason. Moving between `PUBLIC` and `PROTECTED` changes
the compared property values and therefore requires a new review and snapshot
promotion.

### Permission-Mode Tool Listing

Permission mode is a discovery filter. For each tool, `tools/list` looks up its
endpoint rule and compares the rule's permission metadata with the caller's
normalized role, group, position, attribute, and user claims. It does not run
arbitrary CEL or argument-dependent rules. The subsequent `tools/call` remains
independently authorized, so list visibility is not an authorization grant.

Use permission mode by default for newly protected instances after the gateway
uses `authorizationToolName` consistently for list and call and recognizes the
canonical public rule. Keep unknown rules hidden. Existing instances migrate by
previewed opt-in because changing from `none` can remove tools from clients'
discovery results. CEL mode remains an explicit option for deployments that
accept its evaluation cost and argument-insensitive list semantics.

### Nested `structuredContent` Filtering

The gateway already filters a top-level object, an array of objects, and an
object containing an `items` array. It does not generically address an array or
object deeper in a composed workflow result. Add a restricted, schema-checked
response target path rather than recursively filtering every object with a
matching field name.

The first nested-filter version should:

- use a bounded path grammar, such as JSON Pointer plus one explicit array-item
  selector, rather than unrestricted JSONPath;
- validate the path against the published output schema and show the selected
  object or array in Portal preview;
- apply row filters to the selected array and column filters to its object
  elements, or apply column filters to one selected object;
- fail closed on a missing path, wrong node type, traversal-limit breach, or
  schema mismatch;
- bound path depth, selected node count, and filtered response size; and
- continue regenerating textual MCP content from the filtered
  `structuredContent`, as the current gateway does.

The JSON traversal itself is modest. The work is medium-sized because it spans
gateway filter semantics and tests, output-schema validation, generic filter
storage, Portal editing and preview, publication digests, and migration of
existing filters. A fixed pointer to one nested object or array is a reasonable
first increment; full recursive or general JSONPath filtering should remain out
of scope until its ambiguity and resource limits have a separate design.

## Recommended Decisions

- Adopt a first-class Access Target abstraction in Portal.
- Add Access Control directly to Tool Admin.
- Keep Workflow Access separate.
- Treat the compiled tool endpoint as the policy-key authority.
- Require an explicit endpoint for managed tools; permit an operator-confirmed
  pin of a non-empty compiled path-derived key, but never auto-pin `@call`.
- Reuse the `rule.endpointRules` and `rule.ruleBodies` property format. Compile
  public access as one canonical shared allow-all request rule rather than
  introducing an endpoint marker, a second policy file, or a skip prefix.
- Store their canonical merged values in one `instance_property_t` carrier per
  gateway instance and track source ownership in Portal publication records.
- Require active `map` registrations and atomically retire all higher-priority
  per-API carriers when the instance carrier is first written.
- Converge all resolvers on the deployed `create_snapshot` active-row and
  instance-pool semantics before exposing the carrier-migration command; never
  substitute physical deletion.
- Permanently reject active legacy rule carriers during readiness.
- Define source policy by host and access target, but compile, publish, and
  assess readiness per gateway instance. Environment is selection metadata,
  not another merge layer; any future override must be explicit.
- Treat exact endpoint ownership as required readiness and gateway pattern
  fallback as legacy compatibility only.
- Derive list visibility from permission until Portal can enforce a formal
  subset invariant.
- Include effective instance-global access settings and runtime shadow analysis
  in preview and digest.
- Make Portal's canonical ownership merge the digest authority; treat database
  ordering only as deterministic defense in depth and reject every duplicate
  endpoint owner before persistence.
- Keep `defaultDeny: true` and prohibit generated tool bypasses.
- Freeze protected publication only on deployments that cannot prove canonical
  resolver semantics; require carrier migration and exact-rule readiness before
  publishing Tool Access or removing its bypasses.
- Use a manually managed internal API only on an isolated transition instance
  with no other rule-property contributor.
- Remove local access-control overrides after Portal-published policies are
  verified in the effective snapshot.
