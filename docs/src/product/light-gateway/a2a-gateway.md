# A2A Gateway

Status: Proposed design; implementation and runtime qualification have not started

Related control-plane designs:

- [AI Agent Registration In Task Center](https://github.com/lightapi/light-portal-doc/blob/master/src/design/portal-view/agent-registration.md)
  defines the logical Agent, native runtime link, base Agent publication, and
  explicit handoff into optional A2A publication.
- [Control-Plane Policy Publication Through Config Server](https://github.com/lightapi/light-portal-doc/blob/master/src/design/light-portal/control-plane-policy-config-server.md)
  defines the immutable snapshot, `(host, serviceId, envTag)` workload identity,
  `/configs`, reload, acknowledgement, last-known-good, and rollback contract
  reused here.

This document defines how `light-gateway`, native A2A support in `light-agent`,
and a first-class `light-a2a` integration service should expose and govern
Agent2Agent (A2A) traffic while Light Portal remains the catalog and policy
authority. `light-agent` embeds the managed A2A boundary for Portal-native
agents. `light-a2a` supplies that boundary only for external business agents or
remote A2A federation.

The core principle is:

> `light-gateway` protects the public edge, the selected native or integration
> runtime enforces A2A-specific protocol and policy from shared modules, and the
> selected agent implementation owns business reasoning and domain effects.

## Summary

Introduce shared A2A protocol, policy, card, and task modules; embed them in
`light-agent`; provide a registered, horizontally scalable `light-a2a` external
integration service; and add a small `a2a-router` edge module in
`light-gateway`. Together they support three onboarding paths:

1. **Portal-managed generic agent**: publish a `light-agent` assembled from
   Portal-managed prompts, models, capabilities, skills, memory, knowledge,
   tools, and workflows. `light-agent` terminates A2A and enforces the shared
   A2A security and policy contract in-process; no sidecar is deployed.
2. **External business agent with managed sidecar**: run `light-a2a` beside
   custom agent code. The developer implements a narrow business interface;
   the sidecar owns A2A, platform security, fine-grained access control,
   policy, task correlation, limits, audit, and telemetry.
3. **Existing remote A2A agent**: use shared-service `light-a2a` federation to
   expose or call an already compliant remote agent through an approved Portal
   catalog binding.

The same `light-a2a` binary supports two external-integration deployment
profiles. Shared-service mode is the default for many remote agents and
horizontal scaling. Sidecar mode is reserved for private/local external
business implementations, isolated credentials, or backends that do not
implement A2A themselves. It is never inserted beside `light-agent`.

The first external-developer profile uses the private
`light-a2a-backend/v1` HTTP/JSON contract over a fixed loopback origin, with
SSE when a backend declares streaming. Python, Java, and TypeScript SDKs are
production release requirements; Rust provides the reference implementation
and shared conformance harness. This local backend contract is not the public
A2A HTTP+JSON binding.

The first delivery should support A2A 1.0 JSON-RPC with an explicit A2A 0.3
compatibility profile and no activated protocol extensions. Public A2A
HTTP+JSON, public A2A gRPC, push notifications, custom bindings, and individual
extension profiles belong in later phases after the abstract task and message
contract is proven.
The first production milestone includes both governed inbound publication and
governed outbound invocation. Implementation remains sequenced so the inbound
server, identity, policy, and task foundations land before outbound completion,
but inbound-only operation is a development canary rather than the production
release boundary.

The design intentionally does not copy the AgentGateway implementation. It
adopts the useful behavioral baseline—traffic classification, Agent Card URL
rewriting, and A2A-aware telemetry—but uses the existing `light-gateway`
handler chain, configuration publication, security, registry, and reload
models. It adds shared A2A modules because protocol-semantic authorization and
durable task handling do not belong in the gateway process, and adds the
`light-a2a` application for external-agent adaptation, federation, and
developer-facing runtime isolation.

## Background

The AgentGateway repository implements A2A as an empty traffic-policy marker
on top of its ordinary HTTP proxy. When enabled, it:

- recognizes the legacy and current well-known Agent Card paths;
- classifies JSON `POST` requests and records their JSON-RPC method;
- rewrites backend Agent Card URLs to the public gateway address;
- recognizes selected A2A 0.3 and 1.0 response fields for telemetry; and
- relies on general gateway policies for authentication, authorization,
  routing, rate limiting, TLS, and transformations.

That is a useful interoperability baseline, but it does not own agent
registration, skill assignment, memory, task persistence, or agent execution.

Light-Fabric already has richer platform foundations:

- Light Portal registers an agent as an API version with API type `agt`.
- `agent_definition_t` binds that agent identity to an authorized model alias or
  model policy and to the remaining Agent profile. Direct provider/model/key
  fields are legacy compatibility inputs, not the native publication model.
- `skill_t`, `agent_skill_t`, and `skill_tool_t` represent governed skills and
  their assigned tools.
- `genai-query/getEffectiveAgentCatalog` compiles an agent-scoped catalog.
- the immutable `light-agent/agent` projection carries definition, model,
  skills, catalog, memory, knowledge, execution, channel, and session policy.
- `light-agent` owns durable sessions, turns, actions, event history, memory
  banks, recall, and retention.
- `light-gateway` already provides handler-chain dispatch, JWT validation,
  access control, delegation, request and response filtering, rate limiting,
  TLS, service discovery, telemetry, and last-known-good configuration reload.

The current checkout does not yet provide an A2A handler in `light-gateway` or
native A2A routes in `light-agent`, and it does not contain a `light-a2a`
application. The current `light-agent` effective catalog is loaded from its
immutable configured projection; a future live Portal query must not be
assumed to be part of the first A2A implementation.

## Use Cases

### Existing A2A Server Behind Light Gateway

An organization already operates an A2A-compatible agent. It registers the
agent and backend binding in Portal, then exposes it through `light-gateway`
and shared-service `light-a2a`. The remote server stays authoritative for its
tasks; `light-a2a` validates the protocol and applies the approved binding,
delegation, fine-grained policy, limits, and telemetry.

### External Business Agent With A Managed Sidecar

An external developer implements a narrow local business interface instead of
implementing A2A and every Light platform concern. A `light-a2a` sidecar
terminates A2A, loads its Portal-published policy, validates callers and
operations, manages protocol task correlation, and invokes the business
backend over a protected localhost, Unix-domain-socket, or mutually
authenticated connection.

The sidecar passes a short-lived signed authorized invocation context. It does
not forward the caller's raw bearer token. The business implementation still
enforces domain invariants, but it does not recreate platform authentication,
tenant isolation, A2A task handling, audit, or observability.

### Light Agent Published As A2A

A Portal-defined `light-agent` is published for external or internal A2A
clients. Portal compiles the public Agent Card. The gateway exposes its public
route and routes directly to the registered `light-agent`. Shared A2A server
modules inside `light-agent` serve the card and map A2A contexts and tasks onto
its durable Light sessions and turns without a sidecar or internal network hop.

### Light Agent Calling An External Agent

A Light agent selects an external agent from its effective, policy-filtered
catalog. The call is sent through `light-gateway` using a stable server-owned
agent reference. `light-a2a` resolves the approved destination, attaches
server-owned credentials or delegation, and enforces outbound data policy
after gateway edge admission.

### Public And Extended Discovery

An anonymous caller can receive a deliberately limited public Agent Card in the
first production profile. Phase 6 may add an independently authorized profile
through which an authenticated caller can request an extended Agent Card
containing additional policy-approved skills or interfaces. Neither form
exposes internal skill instructions, tool bindings, credentials, memory, or
topology.

## Goals

- Support interoperable A2A Agent Card discovery and message/task operations.
- Deliver both governed inbound A2A exposure and governed outbound A2A calls in
  the first production milestone.
- Make the selected native `light-agent` or external-integration `light-a2a`
  runtime the A2A protocol-semantic and fine-grained policy enforcement point
  while reusing the same shared Light runtime, A2A, and security crates.
- Reuse `light-gateway` public-edge authentication, routing, filtering, rate
  limiting, TLS, service discovery, and telemetry without duplicating policy
  authority.
- Let external developers implement business logic behind a narrow trusted
  backend contract without implementing A2A or Light platform plumbing.
- Use stable Portal agent identity instead of client-selected target URLs.
- Publish Agent Cards from immutable, versioned Portal projections.
- Preserve the distinction between public discovery metadata and the richer
  internal effective agent catalog.
- Map Portal-assigned skills to a safe A2A `AgentSkill` projection.
- Keep durable session, task, model-loop, skill execution, knowledge, and
  memory responsibilities in the agent runtime.
- Support transparent external A2A backends without requiring them to adopt
  Light Portal's internal runtime model.
- Offer one `light-a2a` binary in shared-service and sidecar deployment modes
  for external integrations, never as a required companion to `light-agent`.
- Make version support, body limits, timeouts, streaming, and failure mapping
  explicit and testable.
- Support safe horizontal scaling and last-known-good configuration reload.

## Non-Goals

- Do not embed an agent runtime or model loop in `light-gateway`.
- Do not make the external business backend parse public A2A requests, validate
  raw platform tokens, or query Portal policy.
- Do not make `light-a2a` own prompts, model selection, memory recall, knowledge
  retrieval, tool selection, or business-domain decisions.
- Do not store A2A tasks or conversation state only in gateway memory.
- Do not let an Agent Card, request body, model response, or memory grant tools,
  credentials, network destinations, or authorization.
- Do not publish raw `contentMarkdown`, tool schemas, workflow configuration,
  memory content, API keys, or internal service locations in public cards.
- Do not query Portal authoring/projection tables on the request path. A
  runtime may read its own operational task/correlation store when required by
  an authorized A2A operation.
- Do not accept arbitrary caller-provided upstream URLs, credential references,
  service IDs, or agent definition IDs.
- Do not claim public A2A HTTP+JSON, public A2A gRPC, push notification, or
  custom-binding support until each binding passes its own conformance and
  operational gates.
- Do not translate the existing `/chat` WebSocket protocol inside the gateway
  into a second, gateway-owned task engine.
- Do not deploy one sidecar per remote SaaS agent when a shared federation
  service provides the required network and credential boundary.
- Do not expose every registered Agent through A2A automatically. Registration
  and native runtime linking are prerequisites; A2A exposure is an explicit,
  independently authorized publication decision.

## Decisions

### Portal Owns Authoring And Publication

Portal owns agent identity, descriptive metadata, assigned skills, visibility,
security declarations, supported interfaces, backend bindings, and publication
lifecycle. It compiles those records into an immutable A2A runtime projection.

The gateway consumes only the approved projection. It does not reconstruct an
Agent Card by joining Portal tables and does not decide which skills should be
public.

### Public Skill IDs Are Stable Publication Aliases

An A2A `AgentSkill.id` is a public protocol identifier. The
[A2A contract](https://github.com/a2aproject/A2A/blob/main/specification/a2a.proto)
requires a unique string; it does not require or benefit from exposing an
implementation database key. Portal therefore publishes a stable,
tenant-scoped `publicationAlias`, such as `billing.refund-review`, and never
publishes the `skill_t.skill_id` UUID as the A2A skill ID.

Portal stores the UUID and alias as separate structured identities:

```text
skillId             internal Portal identity, joins, policy and audit
publicationAlias    stable public A2A AgentSkill.id
```

Portal View suggests an alias from the skill name, lets an authorized owner
confirm or change it before first publication, validates normalized uniqueness
within the host, and displays it on the Skill form and effective-card preview.
After the first successful publication, compatible skill revisions retain the
alias and advance their version and digest. An incompatible semantic replacement
requires a new alias and normally a new skill identity; an ordinary update must
not silently transfer a public ID to different behavior.

Each immutable agent publication records the exact
`publicationAlias -> skillId + skillVersion + skillDigest` mapping used to
compile its card and runtime projection. That mapping supports deterministic
dispatch, rollback, audit, and historical task interpretation without a
request-path Portal lookup. The alias is discovery and correlation metadata,
not authority: a caller that knows it gains no skill, tool, workflow, or agent
permission.

### Gateway Owns The Public Edge

`light-gateway` owns:

- public listener, host, path, and TLS policy;
- coarse caller authentication and endpoint admission;
- bounded edge request admission;
- routing only to the registered `light-agent` or `light-a2a` service selected
  by the published implementation kind;
- optional generic request/response filtering;
- edge rate limiting and denial-of-service controls; and
- public-edge audit, metrics, and trace correlation.

The edge may classify A2A traffic and extract bounded metadata for routing and
telemetry. It is not the authoritative parser or fine-grained A2A policy
decision point.

### Agent APIs Use Deployment-Scoped Gateway Policy Identities

Each logical agent is modeled as an API and API version in Portal. The existing
identity rule remains `agentDefId == apiVersionId`. A deployable `agt` product
and product version describe the compatible Light agent runtime and its
configuration contract; they do not replace the API-version identity of an
individual agent.

Publishing an agent through a Gateway requires an active `instance_api_t`
association between that agent API version and the target `light-gateway`
instance. Its `instanceApiId` is the deployment-scoped binding identity used to
compile routing and coarse edge authorization. This relationship is distinct
from the binding that selects the native `light-agent`, external sidecar, or
remote A2A implementation.

For a native Agent, these are two distinct deployment relationships:

```text
Agent API version -> native light-agent runtime
Agent API version -> public light-gateway instance
```

The Agent registration flow owns or verifies the first relationship. The A2A
publication flow owns or verifies the second. Their `instanceApiId` values are
not interchangeable. An internal `instanceId` or `instanceApiId` may appear in
Portal commands, manifests, associations, and audit evidence, but neither is a
Config Server workload identity or query parameter.

Multiple agent APIs intentionally expose the same A2A protocol endpoints. Raw
keys such as `/@post` or `/message:send@post` therefore cannot be used directly
as keys in the Gateway's combined `rule.endpointRules` map. Portal compiles
opaque, exact-match policy endpoint keys in this namespace:

```text
a2a:instance-api:<instanceApiId>:card
a2a:instance-api:<instanceApiId>:invoke
a2a:instance-api:<instanceApiId>:endpoint:<endpointId>
```

The initial profile requires `card` and `invoke`. The endpoint-specific form is
available when a later binding exposes independently governed HTTP surfaces.
These are authorization resource identities, not public URLs and not A2A
operation names. Portal View never asks an administrator to enter them.

Each agent also receives a unique, human-readable public path prefix on a
Gateway, for example `/a2a/order-agent`. Portal stores it with the Instance API
path-prefix association and rejects normalized public host-and-path collisions.
The route projection maps that public identity to `instanceApiId`,
`apiVersionId`/`agentDefId`, implementation kind, registered target service,
and the generated policy endpoint keys. Changing a public alias does not
silently transfer authority because authorization remains bound to the
Instance API association.

Gateway authorization is deliberately two-level. `light-gateway` uses the
generated `card` or `invoke` key for coarse access to one published agent. The
selected `light-agent` or `light-a2a` runtime then authoritatively parses A2A
and authorizes the specific abstract operation, skill, task/context ownership,
delegation, and data boundary. The edge-level `invoke` class must never be
treated as permission for every A2A operation inside the selected runtime.

### The Selected Runtime Owns The Managed A2A Boundary

Shared A2A modules embedded in the selected runtime own:

- well-known Agent Card rendering and disclosure-class selection;
- A2A version, binding, extension, and operation negotiation;
- bounded protocol parsing and schema validation;
- caller, target-agent, skill, operation, tenant, data-boundary, task-ownership,
  delegation-depth, budget, and limit authorization;
- deterministic backend resolution;
- server-owned credential and trust-policy resolution;
- task/context/idempotency correlation required by adaptation;
- safe public interface construction;
- A2A-aware request/output validation and redaction;
- streaming and callback transport enforcement;
- response and error normalization at the protocol boundary; and
- protocol-level audit, metrics, traces, and policy-decision evidence.

For a `LIGHT_AGENT` binding, `light-agent` embeds these modules and loads the A2A
server policy in its immutable Agent audience projection. For
`EXTERNAL_SIDECAR` and `REMOTE_A2A`, `light-a2a` embeds the same modules and
loads an immutable `light-a2a` audience projection. Each real runtime registers
with the Controller. Neither runtime reconstructs live authority by joining
Portal authoring tables on the request path.

### Agent Runtime Owns Work

The selected runtime owns:

- message interpretation and model execution;
- durable contexts, tasks, turns, actions, and cancellation;
- tool selection and execution through the approved placement;
- knowledge retrieval and evidence;
- memory-bank selection, recall, retention, and reflection; and
- final task results and artifacts.

For an external A2A backend, that backend is the task authority. For a
Portal-native agent, `light-agent` and its durable database model are the task
authority. For a non-A2A external business backend, `light-a2a` owns the A2A
task facade and correlation record, while the backend owns business execution
and effects.

### A2A Task Artifacts Have Independent Retention And Visibility

An A2A artifact is the concrete output of a task, such as a document, image,
structured result, report, or generated file. It is not chat history and it is
not Hindsight memory. The
[A2A specification](https://a2a-protocol.org/latest/specification/) likewise
separates task history messages from task output artifacts and permits an
expired or purged task to produce `TaskNotFoundError`. The selected runtime owns
the artifact lifecycle together with the durable task; `light-gateway` never
becomes an artifact repository.

The three data classes remain independently governed:

| Data class | Purpose | Runtime authority |
|------------|---------|-------------------|
| A2A task artifact | Exact task deliverable, integrity evidence, and retrievable output. | A2A artifact policy and operational artifact store. |
| Chat/session history | Conversation reconstruction and continuation. | Durable session events and the session-history policy. |
| Hindsight memory | Derived facts, experiences, and mental models selected for later recall. | Memory-bank policy and bank-scoped authorization. |

`TASK_OWNER` is the default artifact visibility, not a separate or exclusive
ACL system. Artifact access is default-deny and uses the platform's existing
fine-grained access-control decision with authenticated host, principal,
calling client or agent, delegated user when present, target publication,
skill, task/context ownership, requested artifact operation, data
classification, and applicable obligations. Policy may explicitly grant
additional principals or workloads access. Operators and Portal administrators
receive no implicit artifact-content authority. There is no special break-glass
authorization path: successful and denied decisions use the normal audit
pipeline, and audit recording never grants access.

At minimum, authorization distinguishes artifact metadata read, content read or
download, export, deletion, and promotion to memory. The same ownership and
policy checks apply when an artifact is reached through `GetTask`, `ListTasks`,
subscription, a task response, or a download URL. A task ID, context ID,
artifact ID, object reference, or URL is an identifier rather than a
credential. A resource that is absent, expired, or inaccessible produces the
binding-correct not-found result without disclosing which condition applied.
The initial production profile creates no public, tenant-wide, or anonymous
link visibility by default; any later sharing is an ordinary explicit
fine-grained policy grant, not a new artifact-specific security mechanism.

Artifact bytes live in tenant-scoped, content-addressed managed object storage.
The operational database stores bounded metadata such as task and agent
ownership, logical name, media type, size, content digest, storage reference,
classification, policy and publication digests, provenance, verification
state, retention deadline, legal hold, and deletion evidence. Object-store
references and credentials are never exposed as public artifact identities.
Small inline A2A parts remain subject to the same lifecycle and limits; an
implementation must not escape retention by copying them into an ungoverned
task JSON column.

The platform ships these conservative defaults, which Portal may replace with
an approved host or agent artifact-retention profile:

| Record | Initial default |
|--------|-----------------|
| Incomplete streaming chunks | Compact into the final artifact and remove residual chunks within 24 hours. |
| Final artifact content | Retain for 30 days after the task becomes terminal. |
| External A2A task and artifact visibility | Retain for the same 30-day retrieval window. |
| Metadata, digest, provenance, and deletion tombstone without content | Retain for 365 days or the approved compliance period. |
| Legal hold | Suspend ordinary content deletion until the hold is released by authorized policy. |

The effective profile and deadlines are frozen when the task is admitted, so a
later configuration change cannot silently extend access to an existing
artifact. When the external retrieval window ends, `GetTask` treats the task as
expired even if non-content audit metadata remains internally. Expiration or
deletion removes managed bytes, previews, temporary objects, and caches,
verifies absence, and retains bounded deletion evidence. A privacy-erasure
workflow follows recorded lineage into authorized derived copies; an ordinary
artifact TTL does not implicitly delete separately governed chat or memory.

Chat history may store a bounded artifact ID, digest, and display reference but
not duplicate the artifact bytes. Hindsight does not automatically retain an
artifact. Promotion to memory is a separate fine-grained operation that
extracts or summarizes bounded content, applies memory-bank visibility and
redaction, records the source artifact ID and digest as provenance, and creates
a new memory record with its own retention. The derived memory may outlive
ordinary artifact expiration, while privacy erasure can still find it through
the provenance lineage.

For an external result, a remote URI is not assumed durable. If Light-Fabric
promises local retrieval, `light-agent` or `light-a2a` validates, scans, digests,
and imports the content into the tenant-owned store. Otherwise it marks the
reference ephemeral and makes no availability promise beyond the upstream
server's accepted lifetime. Raw remote or object-store URLs are never treated
as permanent platform artifact references.

Reuse the existing `light-workflow` artifact mechanics: tenant-scoped staging
and content-addressed promotion, digest verification, quarantine, legal holds,
retryable deletion, verified absence, and durable tombstones. Extract shared
artifact policy, validation, storage, and retention contracts rather than
inserting every A2A result directly into `workflow_artifact_t`, whose required
workflow execution identity does not fit native model-only or external A2A
tasks. The A2A operational schema may use a dedicated task-artifact ownership
table or a future generalized platform artifact table, but it must use the
same lifecycle state machine.

### A2A Discovery Is Not Runtime Authority

An Agent Card is descriptive input for discovery. Its skill list, security
schemes, interfaces, and capabilities do not grant access. Effective authority
is the intersection of:

```text
authenticated caller authority
  intersect published agent visibility
  intersect agent-definition policy
  intersect gateway edge admission
  intersect selected-runtime A2A operation and binding policy
  intersect runtime policy snapshot
  intersect live backend capability
```

### Extensions Are Registered And Disabled By Default

An A2A extension can add structured metadata, constrain the core message
profile, introduce methods, or refine task-state behavior. It is therefore
executable protocol policy rather than an arbitrary metadata field. The first
production profiles advertise and activate no extensions: their Agent Cards
omit `capabilities.extensions` or publish an empty list, and their compiled
inbound and outbound allowlists are empty.

Every future extension that Light-Fabric advertises, activates, interprets, or
forwards must use an exact, versioned URI from a Portal-managed registry. All
activated extensions are allowlisted; `required: true` has a stricter gate and
is initially prohibited. A required declaration is a compatibility constraint
from an agent, not proof that the extension is safe. It becomes eligible only
after its implementation, dependencies, parameter schemas, directions,
operations, security review, and conformance evidence are approved for the
selected runtime profile.

For A2A 1.0 over HTTP, a client requests activation with the `A2A-Extensions`
service parameter. The selected `light-agent` or `light-a2a` runtime performs
authoritative negotiation:

- an allowlisted requested extension is activated only after its parameters and
  dependencies validate, and the response lists the extensions actually
  activated;
- an unknown optional extension is not activated or echoed, and its namespaced
  metadata is excluded from policy, model, backend, and artifact inputs;
- invalid content for a recognized and requested extension is rejected with the
  binding-correct validation or extension error;
- omission of an extension that the published card marks required returns
  `ExtensionSupportRequiredError`; and
- an unapproved required extension in a remote Agent Card prevents onboarding
  or publication rather than failing for the first time on a production call.

`A2A-Extensions` negotiation and `ExtensionSupportRequiredError` are 1.0-only
in this design. The 0.3 compatibility profile cannot advertise, require,
accept, or activate extensions. Portal publication and runtime projection
compilation reject any attempt to configure them for 0.3 rather than inventing
a non-standard wire parameter or error mapping.

See the [A2A extension negotiation and required-extension
rules](https://github.com/a2aproject/A2A/blob/main/docs/topics/extensions.md).
Light-Fabric authentication, delegation, tenant binding, data-boundary policy,
and task ownership remain trusted platform context; the first release does not
re-express them as a proprietary required A2A extension.

### Memory Never Lives In The Gateway

The gateway may propagate authenticated subject, agent, context, task, and
correlation identifiers. It must not recall memory, inject remembered text,
choose a memory bank, or retain model output.

An A2A `contextId` is an external correlation identifier, not proof of session
ownership. A runtime must bind it to the authenticated host, principal, agent
definition, and policy snapshot before resuming a session or accessing memory.

## Architecture

```text
                         CONTROL PLANE

 Portal API/Agent Catalog
   |-- API version type agt
   |-- agent definition and implementation kind
   |-- assigned skills and tools
   |-- managed extension registry and profiles
   |-- public/extended disclosure policy
   |-- environment-specific agent bindings
   |-- backend, trust, and credential references
   `-- publication/signing policy
          |                         |
          |                         v
          |              light-oauth signing authority
          |              |-- purpose-bound A2A key profiles
          |              |-- Agent Card JWS signing
          |              `-- public JWKS and rotation
          |                         |
          `----------- signed publication
                    |
                    v
         Config Server                         Controller
         immutable audience projections        live agent/A2A instances
                    |                              ^
                    v                              |
                         DATA PLANE

 A2A client --> light-gateway --> light-agent native A2A endpoint
                 public edge       |-- shared A2A server modules and policy
                                   `-- native durable sessions and turns

 A2A client --> light-gateway --> light-a2a shared service
                                   |-- A2A semantics and federation policy
                                   `--> existing remote A2A server

 A2A client --> light-gateway --> light-a2a sidecar --> external business code

 Light agent --> light-gateway --> light-a2a --> approved external A2A server
```

## Component Responsibilities

| Component | Responsibilities | Explicitly does not own |
|-----------|------------------|-------------------------|
| Portal | Agent authoring, implementation kind, catalog identity, skill assignment, extension registry and profiles, disclosure and access policy, backend binding, signing policy, publication lifecycle. | Request-path routing, extension negotiation, or task execution. |
| `light-oauth` | Resolve host, environment, and purpose-bound signing profiles; sign final canonical Agent Cards; publish profile JWKS; execute key rotation and revocation; and record signing audit. | Agent metadata authoring, card rendering, A2A authorization, or reuse of OAuth token keys as Agent Card keys. |
| Config Server | Publish immutable, audience-specific Gateway, A2A, and Agent projections. | Live policy evaluation, agent reasoning, or per-request database joins. |
| Controller | Discover, register, health-check, and route to live `light-agent` and `light-a2a` instances. | External-agent catalog identity or complete agent policy. |
| `a2a-router` | Resolve a published public route to its Instance API binding and generated policy endpoint, perform coarse admission, route by implementation kind to the registered native or integration runtime, apply generic filtering, and emit edge telemetry. | Authoritative A2A parsing, fine-grained A2A policy, durable tasks, skills, memory, or model calls. |
| `light-a2a` | External-agent A2A server/client bindings, cards, semantic validation, fine-grained policy, secure backend resolution, adapter correlation, delegation, limits, redaction, audit, and telemetry. | Portal-native agent execution, model loops, memory recall, tool selection, or business-domain effects. |
| `light-agent` | Native A2A server boundary, Agent Card serving, fine-grained A2A policy, model loop, durable sessions/turns/actions, effective tools, knowledge, memory, results, audit, and telemetry. | Gateway edge routing or external-agent adaptation/federation. |
| External business backend | Custom reasoning, domain validation, and domain effects through a narrow trusted interface. | Public A2A, raw token validation, Portal policy lookup, Controller integration, or platform telemetry. |
| External A2A server | Its own task semantics and results behind an approved federation binding. | Light Portal or Light policy authority. |

## Agent Implementation And Binding Model

Portal should distinguish the stable agent definition from its deployable
binding. The definition declares what the agent is; a binding declares how one
environment reaches and governs an implementation.

Recommended implementation kinds are:

| Kind | Meaning |
|------|---------|
| `LIGHT_AGENT` | Portal-managed generic `light-agent`; prompt, model, skills, memory, knowledge, tools, execution policy, and native A2A server policy are projected to that runtime. No sidecar is used. |
| `EXTERNAL_SIDECAR` | Custom business implementation reached through a local or private `light-a2a` sidecar backend contract. |
| `REMOTE_A2A` | Existing A2A-compliant server reached through shared-service `light-a2a` federation. |

An agent binding contains environment, network zone, selected interfaces,
backend location, trust and credential references, active publication, and
operational limits. Multiple bindings may implement the same definition in
different environments without creating a second agent identity.

The Controller registers actual Light runtime instances. It registers each
native `light-agent`, `light-a2a` shared-service replica, or `light-a2a` sidecar
process, not every remote external agent as a virtual service instance. A
Light-managed external implementation may register independently when it is
itself a real runtime.

## Protocol Scope

### Initial Profile

| Capability | Initial decision |
|------------|------------------|
| A2A version | 1.0 primary, explicit 0.3 compatibility profile. |
| Binding | JSON-RPC over HTTP. |
| Agent Card | `/.well-known/agent-card.json`; optional legacy `/.well-known/agent.json`. |
| Message | Non-streaming and SSE streaming when the selected runtime or backend declares support. |
| Task lookup | Supported only when the native runtime or external integration provides durable task lookup. |
| Cancellation | Supported only through a durable backend operation; never gateway-local cancellation state. |
| Extended card | Deferred to Phase 6 as an independently authorized disclosure profile. |
| Push notifications | Deferred. |
| Public A2A HTTP+JSON and gRPC | Deferred binding profiles; this does not prohibit the private sidecar backend HTTP/JSON contract. |
| Extensions | 1.0 advertises and activates none initially; 0.3 extension configuration is rejected before publication. |

Every request must resolve one configured profile. After applying the
version-specific missing-value rule below, an unsupported version, binding,
operation, content type, or capability must produce the corresponding
A2A-compatible error rather than silently falling through as an ordinary proxy
request. For 1.0, extension handling follows the separate optional, required,
and malformed rules above: an unsupported optional URI is not activated, while
a missing required extension or invalid activated extension is an error. The
0.3 compatibility profile rejects extension configuration before publication
or runtime activation.

For HTTP bindings, the selected native or integration runtime accepts the A2A
version from the normative `A2A-Version` header or request parameter. A 1.0
client must provide it; an absent value is interpreted as 0.3 and then checked
against the selected interface.
A 1.0-only interface therefore returns `VersionNotSupportedError` for an
absent value instead of inferring 1.0 from payload shape. The gateway edge does
not interpret the version. See the
[A2A 1.0 versioning requirements](https://a2a-protocol.org/latest/specification/#36-versioning).

### Abstract Operation Layer

Define one internal operation model independent of transport:

```rust,ignore
enum A2aOperation {
    GetAgentCard,
    GetExtendedAgentCard,
    SendMessage,
    SendStreamingMessage,
    GetTask,
    ListTasks,
    CancelTask,
    SubscribeToTask,
    SetPushNotificationConfig,
    GetPushNotificationConfig,
    ListPushNotificationConfigs,
    DeletePushNotificationConfig,
}
```

Binding adapters parse the transport into this model. Authorization, target
selection, task ownership, limits, and telemetry use the abstract operation
instead of transport-specific method strings.

## Managed A2A Runtimes

Embed the shared A2A server modules in `apps/light-agent` for `LIGHT_AGENT`
bindings. Create `apps/light-a2a` on `light-axum` and the existing
`light-runtime` lifecycle for `EXTERNAL_SIDECAR` and `REMOTE_A2A` bindings. Each
application uses `startup.yml` and `portal-registry.yml`, loads its own
audience-specific configuration through Config Server, registers its real
service instance with the Controller, and participates in managed startup,
reload, quiesce, and shutdown.

The gateway retains a small `a2a-router` edge handler whose configuration maps
approved public routes to their Instance API binding, generated coarse policy
endpoints, implementation kind, and registered `light-agent` or `light-a2a`
service identity. Protocol models, Agent Card rendering, fine-grained
decisions, backend bindings, and task correlation live outside the gateway.

### Shared Crate Boundaries

`light-agent` and `light-a2a` depend on shared crates; neither application
depends on the other application's internal modules.

```text
crates/a2a-protocol/       versioned wire types, parsing and conformance
crates/a2a-runtime/        operations, errors, task mapping and backend traits
crates/a2a-client/         outbound bindings and Agent Card client
crates/a2a-policy/         A2A authorization inputs, decisions and obligations
crates/a2a-backend/        private backend v1 models and Rust reference adapter
crates/agent-policy-core/  publication envelope, identity, digests and limits
crates/artifact-core/      shared artifact validation, storage and retention contracts

apps/light-a2a/            external integration server/client and adapters
apps/light-agent/          generic agent runtime with native A2A server
```

Both applications also reuse existing `light-runtime`, `light-security`,
`light-client`, `agent-core`, and `agent-delegation`. The common policy crate
contains only immutable cross-runtime authority. Prompt, model, memory,
knowledge, tool-selection, approval, and model-loop configuration remain in a
`light-agent`-specific policy layer.

Do not move `light-agent` SQL repositories, session/turn orchestration, memory
logic, or model execution into shared A2A crates merely because A2A exposes
tasks. Share stable contracts and validation, not application ownership.

### Deployment Profiles

| Profile | Placement and scope |
|---------|---------------------|
| `native` | Shared A2A modules embedded in each `light-agent`; handles only that runtime's `LIGHT_AGENT` binding and durable agent state. There is no sidecar or adapter network hop. |
| `shared` | Registered, horizontally scalable `light-a2a` service for remote external agents within one host/tenant policy boundary. Centralizes federation connections, cards, trust, policy and telemetry. |
| `sidecar` | One `light-a2a` process beside a private/custom external backend that is not part of the Light-Fabric agent runtime. Its projection pins one or a small bounded set of agent bindings and denies arbitrary destinations. |

A native `light-agent` registers its A2A capability, supported versions, and
active configuration generation with its existing service identity. A sidecar
registers with tags such as `mode=sidecar`, network zone, supported A2A
versions, active configuration generation, and agent binding ID. A shared
replica registers `mode=shared` and its supported binding capabilities. Full
agent policy remains in Config Server, not Controller metadata. A
`LIGHT_AGENT` publication must never select `mode=sidecar`.

Each accepted runtime projection is single-host. All agent bindings in that
projection inherit and must match `runtimePolicy.host`. Shared mode means
many agents within that boundary, not one mixed-host policy document. A future
multi-host fleet must load separately signed and isolated host partitions; it
must not weaken the host check or combine entries under one envelope.

### Proposed `light-a2a` Configuration Shape

```yaml
runtimePolicy:
  publicationId: ${runtimePolicy.publicationId:}
  releaseVersion: ${runtimePolicy.releaseVersion:0}
  policySnapshotId: ${runtimePolicy.policySnapshotId:}
  policyVersion: ${runtimePolicy.policyVersion:0}
  policyDigest: ${runtimePolicy.policyDigest:}
  contentDigest: ${runtimePolicy.contentDigest:}
  audience: ${runtimePolicy.audience:light-a2a}
  host: ${runtimePolicy.host:}
  serviceId: ${runtimePolicy.serviceId:}
  envTag: ${runtimePolicy.envTag:}
  sourceEventSequence: ${runtimePolicy.sourceEventSequence:0}
  schemaVersion: ${runtimePolicy.schemaVersion:1}
  createdAt: ${runtimePolicy.createdAt:}
  validFrom: ${runtimePolicy.validFrom:}
  refreshAfter: ${runtimePolicy.refreshAfter:}
  expiresAt: ${runtimePolicy.expiresAt:}
  revocationEpoch: ${runtimePolicy.revocationEpoch:0}
  compatibilityGeneration: ${runtimePolicy.compatibilityGeneration:1}

a2aPolicy:
  enabled: ${a2aPolicy.enabled:true}
  mode: ${a2aPolicy.mode:shared}
  maxRequestBodyBytes: ${a2aPolicy.maxRequestBodyBytes:1048576}
  maxResponseInspectionBytes: ${a2aPolicy.maxResponseInspectionBytes:4194304}
  maxAgentCardBytes: ${a2aPolicy.maxAgentCardBytes:262144}
  maxJsonDepth: ${a2aPolicy.maxJsonDepth:128}
  maxConcurrentRequests: ${a2aPolicy.maxConcurrentRequests:1024}
  maxConcurrentRequestsPerPrincipal: ${a2aPolicy.maxConcurrentRequestsPerPrincipal:32}
  requestTimeoutMs: ${a2aPolicy.requestTimeoutMs:120000}
  streamIdleTimeoutMs: ${a2aPolicy.streamIdleTimeoutMs:30000}
  cardCacheTtlSeconds: ${a2aPolicy.cardCacheTtlSeconds:60}

  artifacts:
    defaultVisibility: ${a2aPolicy.artifacts.defaultVisibility:TASK_OWNER}
    transientRetentionHours: ${a2aPolicy.artifacts.transientRetentionHours:24}
    contentRetentionDays: ${a2aPolicy.artifacts.contentRetentionDays:30}
    taskVisibilityDays: ${a2aPolicy.artifacts.taskVisibilityDays:30}
    metadataRetentionDays: ${a2aPolicy.artifacts.metadataRetentionDays:365}
    memoryPromotion: ${a2aPolicy.artifacts.memoryPromotion:EXPLICIT_AUTHORIZATION}
    externalReferenceMode: ${a2aPolicy.artifacts.externalReferenceMode:IMPORT_OR_EPHEMERAL}
    requireMalwareScan: ${a2aPolicy.artifacts.requireMalwareScan:true}

  cardSigning:
    delivery: ${a2aPolicy.cardSigning.delivery:PRE_SIGNED}
    profileId: ${a2aPolicy.cardSigning.profileId:}
    jwksUrl: ${a2aPolicy.cardSigning.jwksUrl:}
    signingServiceUrl: ${a2aPolicy.cardSigning.signingServiceUrl:}

  profiles:
    a2a-v1-jsonrpc:
      binding: ${a2aPolicy.profiles.v1.binding:JSONRPC}
      versions: ${a2aPolicy.profiles.v1.versions:["1.0"]}
      acceptVersionRequestParameter: ${a2aPolicy.profiles.v1.acceptVersionRequestParameter:true}
      missingVersion: ${a2aPolicy.profiles.v1.missingVersion:assume-0.3}
      extensions:
        unknownOptionalAction: ${a2aPolicy.profiles.v1.extensions.unknownOptionalAction:IGNORE}
        advertised: ${a2aPolicy.profiles.v1.extensions.advertised:[]}
        allowedInbound: ${a2aPolicy.profiles.v1.extensions.allowedInbound:[]}
        allowedOutbound: ${a2aPolicy.profiles.v1.extensions.allowedOutbound:[]}
        required: ${a2aPolicy.profiles.v1.extensions.required:[]}
        maxCount: ${a2aPolicy.profiles.v1.extensions.maxCount:8}
        maxHeaderBytes: ${a2aPolicy.profiles.v1.extensions.maxHeaderBytes:2048}
    a2a-v03-jsonrpc:
      binding: ${a2aPolicy.profiles.v03.binding:JSONRPC}
      versions: ${a2aPolicy.profiles.v03.versions:["0.3"]}
      acceptVersionRequestParameter: ${a2aPolicy.profiles.v03.acceptVersionRequestParameter:true}
      missingVersion: ${a2aPolicy.profiles.v03.missingVersion:assume-0.3}
      extensions:
        unknownOptionalAction: ${a2aPolicy.profiles.v03.extensions.unknownOptionalAction:IGNORE}
        advertised: ${a2aPolicy.profiles.v03.extensions.advertised:[]}
        allowedInbound: ${a2aPolicy.profiles.v03.extensions.allowedInbound:[]}
        allowedOutbound: ${a2aPolicy.profiles.v03.extensions.allowedOutbound:[]}
        required: ${a2aPolicy.profiles.v03.extensions.required:[]}
        maxCount: ${a2aPolicy.profiles.v03.extensions.maxCount:0}
        maxHeaderBytes: ${a2aPolicy.profiles.v03.extensions.maxHeaderBytes:0}

  agents: ${a2aPolicy.agents:[]}
```

The checked-in template exposes placeholders and conservative defaults. The
runtime `agents` list and each binding's effective artifact access-control and
retention profile are compiled by the control plane rather than manually
maintained in every A2A deployment. The artifact fields above contain immutable
rules, never artifact rows, bytes, remote URLs, object-store credentials, or
legal-hold commands. Fine-grained grants remain in the normal access-control
projection. Activation rejects a profile whose task-visibility period exceeds
its managed-content period or whose metadata period cannot cover managed
content and deletion evidence. Retention values are bounded by deployment and
compliance limits rather than accepted as arbitrary integers. For external
bindings, Gateway configuration contains only the public route, catalog and
Instance API binding identities, generated coarse policy endpoints,
implementation kind, and registered `light-a2a` service destination.

Extension policy is profile-scoped, not instance-global, because each agent
binding selects exactly one profile and one runtime projection may serve both
generations. Every profile is single-generation: its `versions` list resolves to
one A2A generation, and a profile mixing 1.0 and 0.3 is rejected. A 0.3 profile
rejects every non-empty extension collection during publication and projection
compilation. A 1.0 profile's extension sets apply only to agents that select
that profile and can never activate, advertise, or relax negotiation for an
agent bound to another profile.

The first-production compiler requires the four extension collections to be
empty in every profile. `unknownOptionalAction: IGNORE` means do not activate or
echo an unsupported optional URI; it does not permit its metadata to reach the
agent, model, backend, policy engine, or output. A future non-empty entry is a
compiled registry record containing the exact URI, direction, allowed
operations, parameter-schema digest, handler identity, dependency set, metadata
limits, and required-eligibility decision. Within its own profile, `required`
must be a subset of `advertised` and the applicable inbound or outbound
allowlist. The runtime rejects a projection whose extension handler or schema
digest is unavailable, whose profile is not single-generation, or whose 0.3
profile carries any extension configuration.

For a native `LIGHT_AGENT` binding, Portal compiles an optional `a2aPolicy`
section into the existing `agent.yml` Agent audience projection. The base Agent
policy and A2A overlay are compiled, validated, snapshotted, activated, and
acknowledged as one immutable generation for the target
`(host, serviceId, envTag)`. The overlay is not a separately activated native
Agent configuration. It contains the
inbound binding, server profiles, disclosure policy, authorization policy, task
and artifact policy, limits, accepted signed card, and logical signing-profile
metadata. It
never contains private-key material or a KMS/HSM key reference. It does not
create a separate `light-a2a` projection or deploy a sidecar. The Gateway route targets the actual
registered service ID, currently `com.networknt.agent.account-1.0.0`; publication
must not invent a generic `com.networknt.light-agent-1.0.0` identity. The same
Rust `A2aPolicy` type is embedded by `light-agent` and by the `light-a2a`
configuration model even though their enclosing audience projections differ.
`PRE_SIGNED` is the production default: Config Server delivers the immutable
signed card. `signingServiceUrl` is populated only for an explicitly approved
activation-time signing fallback; even then, the runtime sends a logical
`profileId` to `light-oauth` and never receives the selected key reference.
The compiler derives `profileId`, `jwksUrl`, and `signingServiceUrl` from the
approved signing profile and registered platform service; an agent binding or
runtime cannot supply an arbitrary signer or JWKS origin.

### Proposed Gateway Route And Access Projection

The checked-in Gateway templates remain placeholder based:

```yaml
# a2a-router.yml
routes: ${a2a-router.routes:[]}

# rule.yml
ruleBodies: ${rule.ruleBodies:{}}
endpointRules: ${rule.endpointRules:{}}
```

Portal compiles the effective Config Server values. The following is an
illustrative expanded projection for one agent; the UUIDs and rules are
generated or selected authoring references, not manually maintained YAML:

```yaml
a2a-router.routes:
  - publicPathPrefix: /a2a/account-agent
    allowedHosts: [agents.example.com]
    instanceApiId: 018f0000-0000-7000-8000-000000000001
    apiVersionId: 01900000-0000-7000-8000-000000000001
    agentDefId: 01900000-0000-7000-8000-000000000001
    implementationKind: LIGHT_AGENT
    targetServiceId: com.networknt.agent.account-1.0.0
    targetEnvTag: prod
    policyEndpoints:
      card: a2a:instance-api:018f0000-0000-7000-8000-000000000001:card
      invoke: a2a:instance-api:018f0000-0000-7000-8000-000000000001:invoke

rule.endpointRules:
  a2a:instance-api:018f0000-0000-7000-8000-000000000001:card:
    req-acc: [account-agent-card-access]
  a2a:instance-api:018f0000-0000-7000-8000-000000000001:invoke:
    req-acc: [account-agent-invoke-access]
    permission:
      role: account-agent-user
```

The route resolver selects the route and policy endpoint from trusted
configuration before access control. It passes the generated policy endpoint
as the exact authorization resource and separately records the actual request
host, path, `instanceApiId`, `agentDefId`, and public route in the rule context
and audit event. Rules can therefore use deployment or request attributes
without accepting identity fields supplied by the caller.

Publishing a second agent with the same A2A API endpoints produces a different
`instanceApiId` namespace and cannot overwrite the first agent's entries. A
publication or reload fails closed if route and rule projections disagree,
refer to an inactive binding, contain a duplicate normalized public route, or
reuse a generated policy endpoint across Instance API owners.

### External Integration Backend Kinds

`light-agent` is a direct A2A runtime target, not a backend kind behind
`light-a2a`. The integration service supports these explicit backend kinds:

| Kind | Purpose |
|------|---------|
| `external-backend` | Invoke the narrow trusted backend contract used by a sidecar deployment. |
| `remote-a2a` | Call an approved remote A2A server through a pinned interface and trust policy. |

All destinations are validated configuration, not request data. Hostname,
scheme, port, DNS/IP ranges, TLS policy, redirect behavior, and network zone
must be checked to prevent SSRF and destination substitution.

### External Business Backend Contract

For `EXTERNAL_SIDECAR`, expose a small SDK contract such as:

```rust,ignore
#[async_trait]
trait AgentBackend {
    fn capabilities(&self) -> BackendCapabilities;

    async fn invoke(
        &self,
        context: AuthorizedInvocation,
        request: BusinessRequest,
    ) -> Result<BusinessResponse, BusinessError>;

    async fn invoke_stream(
        &self,
        context: AuthorizedInvocation,
        request: BusinessRequest,
    ) -> Result<BusinessEventStream, BusinessError>;

    async fn status(
        &self,
        context: AuthorizedInvocation,
    ) -> Result<BusinessOperationStatus, BusinessError>;

    async fn cancel(
        &self,
        context: AuthorizedInvocation,
    ) -> Result<(), BusinessError>;
}
```

`AuthorizedInvocation` contains the approved principal and agent actor, host,
tenant, environment, selected agent and skill, allowed operation, policy and
data-boundary digests, task/context/idempotency IDs, deadline, budget, and trace
context. It is short-lived and signed. The sidecar never forwards the caller's
raw bearer token to business code.

For cancellation, the signed context contains exactly one required task ID;
there is no separate unsigned task parameter. Any task or context ID repeated
inside `BusinessRequest` must equal the signed value. A response that starts
detached or long-running work also returns an opaque `backendOperationId`, which
`light-a2a` stores with its durable task correlation. A later `status` or
`cancel` context binds both that operation ID and the task ID; neither method
accepts an unsigned alternate target. This reconciliation operation is required
to meet the Phase 3 sidecar/backend restart guarantee rather than guessing the
outcome of work that survived a sidecar restart.

`BackendCapabilities` declares streaming, cancellation, status reconciliation,
accepted content modes, and other bounded features used to compile the Agent
Card. `invoke_stream` returns ordered status and artifact events and is callable
only when the published binding declares streaming support.

### First-Release Sidecar Backend Transport

The first external-developer release defines one language-neutral private
application protocol, `light-a2a-backend/v1`. Its canonical source is a pinned
[OpenAPI 3.1](https://spec.openapis.org/oas/v3.1.2.html) document and referenced
JSON Schemas under
`contracts/a2a-backend/v1/`, with golden request, response, error, and event
fixtures. SDK models and documentation derive from that source; a language SDK
must not redefine the wire contract independently.

The required production transport is HTTP/1.1 with JSON over one fixed loopback
origin. Streaming uses `text/event-stream` on the same origin when the backend
declares support. The v1 surface is deliberately small:

```text
GET  /v1/capabilities
POST /v1/invoke
POST /v1/invoke-stream
POST /v1/status
POST /v1/cancel
GET  /health/live
GET  /health/ready
```

All business operations carry the short-lived signed
`AuthorizedInvocation` separately from developer-controlled business input.
The SDK validates its issuer, audience, expiry, deadline, replay identifier,
host, environment, agent, skill, operation, task/context/idempotency bindings,
backend operation ID when present, and policy and data-boundary digests before
calling business code. Loopback address or process placement is defense in
depth, not authentication. The configured origin, version, methods, and paths
are immutable; proxy environment variables, redirects, arbitrary destinations,
and wildcard backend listeners are rejected.

Portal authoring selects only an approved backend transport profile. The
activated Config Server projection pins its contract version and digest,
transport profile, loopback origin, allowed methods and paths, signed-context
audience, timeouts, and resource limits for the target `light-a2a` instance.
Neither the external developer nor an A2A request may override those values.

HTTP over a peer- and filesystem-permission-protected Unix-domain socket may be
qualified as an optional Linux hardening profile using the identical v1
semantics and fixtures. Its absence does not block the first external-developer
release. Mutually authenticated private-network HTTP, gRPC, WebSocket, stdio,
and in-process FFI or plugin transports are deferred. A backend requiring a
different host is not treated as a local sidecar merely to bypass the remote
A2A or future private-transport governance.

This private HTTP/JSON interface does not activate the public A2A HTTP+JSON
binding. `light-a2a` still terminates the selected public A2A JSON-RPC profile
and adapts it to the smaller backend contract. Making the business backend
implement A2A would defeat the sidecar's purpose.

The backend accepts calls only from its sidecar and still validates
business-domain invariants and durable business idempotency. It does not make
platform authorization decisions, interpret Portal policy, or receive the
caller's raw token.

### First-Release Backend SDKs

The first external-developer production release requires supported Python,
Java, and TypeScript/Node.js SDKs. A Rust implementation in `crates/a2a-backend`
is the reference adapter and conformance oracle, not a fourth externally
supported SDK release gate. Go, .NET, and other languages remain eligible after
the v1 contract is stable and demand justifies their compatibility burden.

| SDK | First-release requirement |
|-----|---------------------------|
| Python | Production package, async unary/streaming adapter, examples, and full conformance evidence. |
| Java | Production library and standalone server adapter usable without reimplementing Light security, with full conformance evidence. |
| TypeScript/Node.js | Production package, unary/SSE adapter, examples, and full conformance evidence. |
| Rust | Checked-in reference adapter, golden-vector producer/consumer, and shared test harness. |
| Go, .NET, and others | Deferred; developers may use the published wire contract without a supported SDK claim. |

Each production SDK owns the local HTTP server adapter, signed-context and
replay validation, identifier equality checks, deadlines, limits, typed error
mapping, SSE framing, cancellation and status reconciliation, health endpoints,
artifact descriptors, and trace propagation. The developer implements only the
`AgentBackend` business callbacks. Generated models are useful but insufficient:
the thin handwritten runtime and all generated types must pass the same
cross-language conformance suite against the same `light-a2a` build.

## Request Lifecycle

### Handler Selection

The configured gateway handler chain resolves and admits the request before
forwarding it to the registered runtime selected by the published
implementation kind. `LIGHT_AGENT` routes directly to `light-agent`;
`EXTERNAL_SIDECAR` and `REMOTE_A2A` route to `light-a2a`. The edge handler
matches only configured public paths and their well-known card suffixes. It
must not treat every JSON `POST` as A2A traffic.

```text
request
  -> admission
  -> handler-chain resolution
  -> CORS where configured
  -> JWT/session authentication
  -> public A2A route and Instance API binding resolution
  -> coarse authorization with the generated card or invoke policy endpoint
  -> registered native or integration runtime routing
  -> A2A version, content-type, body and operation validation
  -> fine-grained A2A policy decision and obligations
  -> backend selection and narrow delegation/credential injection
  -> native light-agent execution or external integration invocation
  -> response validation, filtering and protocol normalization
```

### Agent Card Request

For a Portal-published card:

1. Resolve the configured public host and path to an active Instance API
   binding, its generated `card` policy endpoint, and disclosure class.
2. Authenticate before extended-card disclosure.
3. Authorize coarse card access using that generated policy endpoint, then
   verify the publication generation is active and not expired or revoked.
4. Select the immutable card whose final public URL, optional fields, digest,
   and `light-oauth` signature were accepted with the active publication.
5. Authorize disclosure before conditional-request evaluation, then attach an
   ETag derived from publication digest, disclosure class, applicable
   authorization-policy digest, and revocation epoch. Authenticated cards use
   private cache policy and never reuse an ETag across disclosure classes.
6. Return the bounded card without a request-path signing call, starting
   business execution, or joining Portal authoring tables.

For a proxied upstream card:

1. Fetch the card through the configured backend.
2. Enforce status, content type, body size, and JSON-depth limits.
3. Validate the declared versions, interfaces, and schemes against route policy.
4. Rewrite only complete, approved interface URLs.
5. Handle upstream signatures explicitly.

Rewriting a signed upstream card invalidates its signature. `light-a2a` must
either reject rewriting, publish a separately signed public facade card, or
remove the invalid signature and obtain an external-facade publication
signature from `light-oauth`. It must never forward an upstream signature over
mutated content. The controlled publication or cache-refresh path performs the
signing operation and caches the result; an ordinary Agent Card request does
not trigger signing. Signing keys never enter `light-a2a`, `light-agent`, the
gateway `a2a-router`, or Config Server runtime projections.

### Message Or Task Request

1. The gateway resolves the public route to an active `instanceApiId`, verifies
   the published `agentDefId`, authorizes its generated `invoke` policy
   endpoint, and selects the published implementation kind and a healthy
   registered `light-agent` or `light-a2a` instance. It does not accept any of
   those identities or a destination from the body.
2. The selected runtime independently validates the delegated binding identity,
   then validates `A2A-Version`, extensions, envelope,
   operation, IDs, and body limits through the shared A2A server modules.
3. Bind authenticated host and principal to the selected agent definition and
   environment binding.
4. Authorize caller, calling agent, selected skill, abstract operation, tenant,
   data boundary, delegation depth, and budget.
5. Validate context/task ownership for `GetTask`, `ListTasks`, `CancelTask`,
   `SubscribeToTask`, and push-configuration operations.
6. For `LIGHT_AGENT`, admit the operation directly into its durable native
   session/turn model. For `EXTERNAL_SIDECAR`, create adapter correlation and a
   narrowly scoped, short-lived `AuthorizedInvocation`. For `REMOTE_A2A`,
   select the pinned remote interface and server-owned credential or delegation.
7. Invoke the native agent, approved remote A2A server, or external business
   backend according to that binding.
8. Validate the response or stream event within configured bounds, apply
   decision obligations such as redaction, and have the selected runtime scan,
   verify, classify, and materialize any artifact for which Light-Fabric
   promises managed retention.
9. Apply gateway response filtering before returning data to the caller.
10. Record edge, policy, protocol, backend, and runtime outcomes without
    logging sensitive content.

### Streaming

Streaming is end-to-end. The gateway enforces generic edge limits;
the selected runtime enforces A2A event framing, setup deadline, idle timeout,
event limits, cancellation propagation, and disconnect behavior. Neither turns
an ephemeral gateway stream into the authoritative business task record.

For a Portal-native agent, `light-agent` streams its durable turn events
directly through the embedded A2A server. A client that reconnects uses the A2A
task/context contract to recover state; it does not depend on the same gateway
process retaining a stream session. Incremental artifact chunks are bounded and
assembled under the selected runtime's artifact policy; residual chunks are not
retained as an accidental second copy of the final artifact.

## Portal Publication Model

### Stable Identity

Use the existing Portal identity rule:

```text
agentDefId == API version ID for the agent API asset
```

The A2A public path is a publication attribute, not a second agent identity.
Task, audit, policy, skill, and memory records continue to refer to the stable
agent definition ID.

The four related identities have separate purposes:

| Identity | Purpose |
|----------|---------|
| `apiVersionId` / `agentDefId` | Logical, versioned agent catalog identity. |
| `instanceApiId` | Binding of that agent API version to one Gateway instance; namespace for compiled edge policy. |
| Public host and path prefix | Human-readable routing and Agent Card interface identity. |
| `agt` product and product version | Runtime software and configuration compatibility, not business-agent identity. |

### Public Agent Card Metadata Authority

For public Agent Card metadata, **authoritative** means the Portal authoring
record and deterministic precedence rule from which the publication compiler
must obtain a value when several records could describe the same agent. It does
not mean the Agent Card grants runtime authority. Cards remain descriptive;
authentication, authorization, task ownership, and effective skills continue
to come from policy.

There are three metadata authority stages:

1. Portal authoring records are the source of truth for edits and review.
2. The immutable, digest-bound A2A publication freezes the effective values.
3. `light-agent` or `light-a2a` serves that accepted publication without joining
   Portal tables or allowing runtime configuration to override individual
   fields.

Use this source and precedence contract:

| Agent Card field | Authoritative Portal source | Precedence and validation |
|------------------|-----------------------------|---------------------------|
| Name | `api_t.api_name` | Required. The A2A binding cannot rename the agent. |
| Description | `api_version_t.api_version_desc`, then `api_t.api_desc` | Use the nonblank version description first and the nonblank API description as fallback. A selected publication profile may require a result. |
| Provider | Referenced public provider profile, then the host's default public provider profile | Provider name and URL come from an approved structured profile. Absence is allowed only when the selected A2A profile permits it. |
| Documentation URL | Version-scoped agent public metadata | Must be an approved absolute public URL. It is not inferred from `api_version_t.spec_link` or `api_t.git_repo`. |
| Icon | Version-scoped managed `iconAssetId` | The compiler resolves the asset to an approved absolute public URL and validates scheme, media type, size, and availability. Arbitrary remote icon URLs are not accepted. |
| Semantic version | `api_version_t.api_version` | This is the business-agent version and must pass the selected publication profile's SemVer validation. |

These versions are independent and must not be substituted for one another:

```text
api_version_t.api_version       business-agent semantic version
A2A-Version                     wire-protocol version
agt product version             runtime software/config compatibility
publication version             immutable control-plane generation
model or model-policy version   model selection, not agent identity
```

In particular, `agent_definition_t.model_provider` identifies the LLM vendor
or model-selection provider. It must never populate the Agent Card provider,
which identifies the organization responsible for the published agent.

Add structured logical authoring records such as:

```text
public_provider_profile_t
  host_id
  provider_profile_id
  provider_name
  provider_url
  provider_description
  aggregate_version
  active

agent_public_metadata_t
  host_id
  agent_def_id              # API version ID
  provider_profile_id
  documentation_url
  icon_asset_id
  aggregate_version
  active
```

The preferred first implementation also adds a nullable structured
`publication_alias` column to `skill_t`, with a normalized partial unique
constraint for active aliases within `host_id`. Private skills need no alias
until selected for publication. The publication compiler requires one before a
skill can appear in an Agent Card and freezes the alias after its first
successful use. A schema-validated JSON extension must not substitute for this
queryable identity field.

Portal also owns a managed extension registry. A logical first schema is:

```text
a2a_extension_t
  host_id
  extension_id
  extension_uri             # exact, versioned public identity
  display_name
  extension_version
  extension_class           # DATA, PROFILE, METHOD or STATE_MACHINE
  lifecycle_status          # EXPERIMENTAL, APPROVED, DEPRECATED or REVOKED
  allowed_directions        # INBOUND, OUTBOUND or BOTH
  required_eligible
  allowed_operations
  parameter_schema          # schema-validated JSONB
  parameter_schema_digest
  handler_ref
  handler_digest
  maximum_metadata_bytes
  security_review_ref
  aggregate_version
  active

a2a_extension_dependency_t
  host_id
  extension_id
  dependency_extension_id
  required
  aggregate_version
  active
```

Portal View manages these records through structured registry forms and lets an
A2A Binding select only active, direction-compatible entries. The URI,
classification, directions, lifecycle, handler, review, and required
eligibility are structured fields; only schema content and schema-validated
extension parameters use JSONB. The binding form does not accept an arbitrary
URI or enable `required` when the registry record is not required-eligible. The
initial production registry may contain reviewed draft records, but no binding
can activate or advertise them until a later extension profile is qualified.

The host also selects at most one active default public provider profile. Exact
DDL names may follow Portal conventions, but provider profiles must be reusable,
tenant-scoped, versioned, and soft-deletable, while agent public metadata must
be version-scoped through `agentDefId`. These fields are structured because
Portal View must validate, query, review, and audit them. JSONB remains limited
to schema-validated extension metadata.

The A2A binding references these records and selects disclosure; it does not
duplicate or freely override their values. For `REMOTE_A2A`, an upstream card
may seed a reviewed draft and remains provenance input, but it does not replace
Portal authority or silently update an active facade card.

Portal View manages name and general description on the API form, semantic
version and version description on the API Version form, reusable provider
profiles in Host Administration, and provider selection, documentation URL,
and icon in an Agent Public Metadata panel. The A2A Binding workflow shows a
read-only effective-card preview with the source of each field. Publication
fails rather than inventing a value when required metadata is missing,
ambiguous, inactive, invalid, or no longer accessible.

### Logical A2A Publication

Portal needs one versioned logical publication per exposed agent and
environment. The physical schema uses normalized authoring tables plus an
immutable versioned JSONB publication aggregate and compiled runtime
projections. This is a deliberate hybrid, not a choice between an editable
table and an editable JSON document.

The normalized authoring model contains the provider and agent-public-metadata
records above plus a core `agent_a2a_binding_t`, with
`agent_a2a_interface_t`, `agent_a2a_access_grant_t`, and
`agent_a2a_disclosure_t` child relations for repeating interfaces, grants, and
disclosure selections. `agent_a2a_publication_t` stores the immutable compiled
manifest and `agent_a2a_instance_publication_t` records application and rollback
for a target runtime instance. Exact DDL names may be aligned with Portal schema
conventions during implementation, but these logical ownership boundaries are
settled.

The normalized model gives Portal View queryable columns, foreign keys,
uniqueness constraints, optimistic aggregate versions, and soft-delete or
revocation behavior. JSONB is limited to explicitly schema-validated extension
options, validation evidence, source aggregate-version maps, and immutable
compiled publication content. Raw credentials and signing-key material are
never stored in the binding; only server-owned references are accepted.

The logical contract contains:

- `hostId`, `agentDefId`/`apiVersionId`, Gateway `instanceApiId`, API version,
  and environment;
- implementation kind (`LIGHT_AGENT`, `EXTERNAL_SIDECAR`, or `REMOTE_A2A`);
- environment-specific agent binding ID and network zone;
- publication ID, version, digest, lifecycle, validity, and revocation epoch;
- unique public path prefix and allowed host names;
- generated coarse policy endpoints for card and invocation admission;
- public and extended visibility rules;
- compiled provider, documentation, icon, business-agent version, input modes,
  and output modes, with source-record identities and aggregate versions;
- supported binding/version/interface declarations in preference order;
- capability declarations, including streaming and push support;
- exact advertised, inbound, outbound, and required extension selections with
  registry version, handler/schema digests, dependencies, operations, and
  metadata limits;
- security schemes and requirements;
- selected public AgentSkill projections and their immutable
  `publicationAlias`, internal `skillId`, skill-version, and skill-digest
  mappings;
- backend kind, service identity, environment, path, and TLS policy;
- server-owned credential or delegation policy references;
- signing-profile ID, signature policy, signed-card JWS/JWKS metadata, and
  signing audit reference; and
- source aggregate versions used to compile the publication.

The publication compiler rejects missing required card fields, ambiguous paths,
duplicate skill IDs, unsupported protocol combinations, insecure public
interfaces, unresolved backend identities, and secret material embedded in
card content. It also rejects an inactive or mismatched Gateway Instance API
association, a normalized public host-and-path collision, a generated policy
key collision, or any route whose `instanceApiId`, `apiVersionId`, and
`agentDefId` do not form one consistent binding.

### Portal Authoring And Publication Workflow

Portal View exposes an explicit **Publish through A2A** handoff from Agent
registration or Agent detail. The handoff opens the **A2A Bindings** action or
workspace; completing ordinary Agent registration never creates a public A2A
route implicitly. Because an agent can have zero or more environment-specific
bindings, the primary UI is a table with binding name, environment,
implementation kind, deployment mode, public path, selected profiles, target
kind, visibility, validation status, publication version/state, and last update.
Create and update use structured, conditional forms for interface, backend,
security, disclosure, access, and limit fields. Compiled JSON is a read-only
preview; only explicitly supported extension fields may use a schema-validated
advanced JSON editor.

Create, update, and delete commands modify Draft authoring state through the
normal Portal command, CloudEvent, and query-projection path. They do not mutate
live runtime configuration. An explicit validate-and-publish workflow:

1. reads a consistent set of API, API-version, public-provider,
   agent-public-metadata, binding, agent, skill, disclosure, policy, backend,
   and target-instance projections;
2. validates references, skill-alias uniqueness and stability, extension
   registry/direction/dependency/required-eligibility rules, and records every
   source aggregate version;
3. verifies or creates through the authorized deployment workflow the active
   Gateway `instance_api_t` association and its unique public path prefix;
4. compiles the immutable unsigned Agent Card with final environment-specific
   public URLs and optional fields, plus the publication manifest, content
   digest, public-skill mappings, generated deployment-scoped policy endpoint
   keys, and audience-specific Config Server property sets;
5. invokes `light-oauth` with the authorized host, environment, and signing
   profile; `light-oauth` validates the profile purpose, canonicalizes the card
   without existing signatures, signs it with the current profile key, and
   returns the Agent Card signature, `kid`, and JWKS location;
6. verifies the returned signature against the selected profile JWKS and stores
   the complete signed card, signing audit reference, and canonical digest in
   the immutable publication;
7. emits the Config command/events that stage the property sets in
   `instance_property_t` for the target registered instances;
8. creates and validates immutable `config_snapshot_t` Config Server snapshots
   for every target `(host, serviceId, envTag)`;
9. activates the release manifest and its exact target-to-snapshot mapping;
10. asks the Controller to reload each target by `host`, `serviceId`, and
    `envTag`;
11. each runtime calls `/configs`, validates the selected immutable snapshot,
    and atomically applies it or retains its still-valid last-known-good
    generation; and
12. records each applied or rejected snapshot ID and digest for Portal
    diagnostics.

Portal may update all current pointers atomically in its database, but
independently operated Gateway, `light-agent`, and `light-a2a` processes observe
and apply them at different times. A release therefore requires compatible
adjacent generations or an explicit staged protocol, per-target reload and
acknowledgement, and exact-generation rollback. It must not claim instantaneous
cross-service activation.

Retiring a published binding creates and activates a new generation without
the route and, when immediate invalidation is required, advances the revocation
epoch. Historical publications remain immutable for audit and bounded rollback;
Portal does not hard-delete the active runtime contract.

Portal View manages A2A artifact-retention profiles through structured fields
for transient, content, task-visibility, and metadata periods, external-reference
handling, scanning, and memory-promotion posture. A host default may be selected
and an agent binding may select an approved override. Artifact access itself
continues to use the existing fine-grained access-control authoring and policy
projection; the artifact form does not introduce a parallel ACL or special
administrator bypass. Publication validates the combined retention and access
policy, freezes its digest into the runtime generation, and exposes the
effective read-only result in the compiled preview.

### Runtime Projections

Portal publishes separate least-privilege projections for the gateway and the
selected runtime. The gateway projection contains only public route admission,
the active `instanceApiId`, `agentDefId`/`apiVersionId`, generated card and
invoke policy endpoints, implementation kind, and registered target-service
routing data. Its combined `rule.endpointRules` uses those generated policy
endpoints rather than the repeated raw A2A specification paths. For
`EXTERNAL_SIDECAR` and `REMOTE_A2A`, the `light-a2a` projection contains Agent
Cards including accepted publication signatures, fine-grained policy, backend
bindings, trust, credential references, logical signing-profile metadata, and
protocol, artifact-access, and artifact-retention limits. For `LIGHT_AGENT`,
the existing `agent.yml` projection retains
prompt, model, `agentPolicy.skills`,
`agentPolicy.catalog.effectiveCatalog`, memory, knowledge, tool, and execution
policy and adds the native inbound `a2aPolicy`, including the same artifact
policy schema, and accepted signed card required to serve that publication.
Portal stages these compiled values in
`instance_property_t`; snapshot creation copies the candidate values into the
immutable Config Server generation selected by host, service ID, and environment
tag. Property definitions or an editable instance row alone do not constitute a
published runtime contract.

`GET /configs?host&serviceId&envTag` is the only current-workload configuration
path. The Config Server does not resolve mutable A2A authoring data, does not
serve `instance_property_t` directly, and does not support an A2A-specific
runtime-config endpoint or a secondary `instanceId`, `productId`, or
`productVersion` lookup mode. Publication alone does not hot-load a process;
the explicit Controller reload causes the runtime to call `/configs` again.

The selected runtime renders public metadata only from the accepted publication;
an environment variable, upstream card refresh, or backend response cannot
replace an individual name, description, provider, documentation, icon, or
business-version field.

Every runtime audience projection is immutable and digest-bound. The runtime
checks:

- audience matches the selected runtime (`agent` or `light-a2a`);
- host, service ID, and environment tag match the running target service;
- schema version and compatibility generation are supported;
- content digest and publication digest match canonical content;
- validity, refresh, expiry, and revocation constraints pass;
- every profile is single-generation, each 0.3 profile carries no extension
  configuration, and each 1.0 profile's advertised, inbound, outbound, and
  required extension sets match the accepted card and registry digests with
  required entries eligible, implemented, and dependency-complete; and
- every agent route is unique after path normalization.

The Gateway rejects a projection when two routes have the same normalized
public host and path, two Instance API bindings produce the same policy
endpoint, or a route's policy endpoint is not owned by its declared
`instanceApiId`. Route resolution supplies these trusted identities to access
control; request headers, query parameters, and bodies cannot override them.

Publication failure retains the last-known-good generation only within its
validity window. An expired or revoked publication fails closed even if it was
previously last known good. Runtime request handling does not query Portal
authoring or projection tables.

## Agent Card And Portal Skill Mapping

Portal skills are richer than A2A Agent Skills. Publish a deliberate projection:

| A2A AgentSkill field | Portal source |
|----------------------|---------------|
| `id` | Stable, tenant-scoped `skill_t.publication_alias`; never the Portal UUID. |
| `name` | Approved `skill_t.name`. |
| `description` | Approved public description, not instruction Markdown. |
| `tags` | Policy-filtered tags and selected category paths. |
| `examples` | Explicit reviewed examples; never inferred from private history. |
| `inputModes` | Publication policy or approved capability metadata. |
| `outputModes` | Publication policy or approved capability metadata. |
| security requirements | Publication policy intersection for that skill. |

Only active skills assigned to the exact agent definition and approved for the
selected disclosure class are candidates. Assignment alone does not make a
skill public. The compiler rejects an absent, duplicate, normalized-colliding,
or previously rebound alias. A compatible skill revision keeps its alias while
the immutable publication records the new version and digest.

Do not expose:

- `contentMarkdown` or internal prompt instructions;
- tool IDs, schemas, backend paths, workflow bindings, or execution placement;
- skill configuration supplied for one tenant or user;
- semantic embeddings or internal ranking scores;
- approval rules, cost limits, or private policy diagnostics; or
- anything obtained from memory, session history, tool output, or model text.

The internal effective catalog remains the source for agent-side progressive
disclosure and tool selection. The A2A Agent Card is a smaller interoperability
surface, not a replacement catalog.

See [MCP Tool Metadata Usage](mcp-tool-metadata-usage.md) and
[Centralized Agentic Skill Registry](../../design/centralized-agent-skills.md)
for the internal catalog and execution-placement boundaries.

### Skill Runtime Projection And Executable Packages

The public Agent Card skill list and the internal agent skill projection have
different purposes. The card contains bounded discovery metadata. The runtime
projection contains the assigned instruction content and the independently
authorized tool and workflow descriptors required by one Agent publication.

For `LIGHT_AGENT`, Config Server is the runtime authority. At startup or an
explicit reload, `light-agent` resolves the current immutable generation into
`agent.yml`, validates the envelope and content digests, compiles the projected
skill Markdown into its system instructions, and caches the projected effective
catalog locally. It does not call `genai-query/getEffectiveAgentCatalog` on the
request path or treat live Portal authoring rows as a fallback. At design time,
the template and strict runtime loader exist; completing the Portal compiler,
snapshot activation, acknowledgement, and last-known-good reload path remains
implementation work owned by the publication phases below.

`genai-query/getEffectiveAgentCatalog` remains useful on the control plane for
Portal View preview, assignment validation, publication compilation,
administrative diagnostics, and semantic ranking. Its current live authoring
result is not a runtime authority. If a future catalog is too large for a
bounded Config Server property, the snapshot may contain an immutable catalog
artifact URI and digest. A future runtime search API must be scoped by
publication ID and content digest and may only rank or narrow entries already in
that immutable manifest; it must not add a capability from newer authoring
state.

A skill is discovery, instructions, and capability composition. Executable
behavior belongs to a governed tool, workflow, fixed service, or reviewed skill
package. Use the following distribution boundaries:

| Content | Runtime distribution and execution |
|---------|------------------------------------|
| Skill alias, public metadata, bounded instruction Markdown, version, and digest | Immutable Config Server projection. |
| Tool aliases, schemas, stable references, execution placement, and policy bindings | Immutable Config Server projection; runtime intersects them with live Gateway or runner authority. |
| API or MCP implementation | Deployed backend invoked through the governed Gateway path; code is not downloaded by the agent. |
| Workflow | Immutable workflow identity, version, and digest in the projection; execution remains in `light-workflow`. |
| Python, JavaScript, WASM, plugin, binary, template, or other package assets | Reviewed, scanned, signed, content-addressed artifact storage; a trusted runner verifies and stages them under the selected sandbox policy. |
| Credentials and secrets | Server-owned secret or delegation service; never skill content, Config Server values, Agent Cards, or hybrid-query results. |

Config Server may carry an immutable artifact reference, digest, media type,
entrypoint, supported runtime profile, and sandbox-policy reference. It must not
carry large package bytes or mutable source for in-process evaluation. Existing
`tool_t.script_content` is an authoring/legacy input only: production publication
packages it as a signed artifact with runner placement instead of delivering it
through Config Server or allowing `light-agent` to retrieve and execute it from
a live Portal query.

Artifact verification and staging complete before a new configuration
generation becomes active. Failure retains only a still-valid last-known-good
generation. Sessions and tasks remain pinned to their accepted publication and
skill digests so an alias cannot change meaning midway through a turn.

## Portal-Native `light-agent` Integration

### Native A2A Server Integration

The current `light-agent` public interaction is a `/chat` WebSocket. A2A has
message, task, streaming, lookup, cancellation, version, and error semantics.
Putting those semantics into `light-gateway` would make the gateway a second
agent runtime and create non-durable state that cannot survive routing changes
or gateway restarts. Putting a `light-a2a` sidecar beside `light-agent` would
instead add a redundant network hop and split task and policy ownership across
two managed processes.

`light-agent` therefore embeds the shared A2A server, card, policy, and task
modules and maps their abstract operations directly onto its durable domain
operations. It does not emulate its browser `/chat` WebSocket client and does
not call a sidecar. A2A wire and policy code lives in shared A2A crates; durable
Light session, turn, action, memory, and model-loop ownership remains in
`light-agent`.

### Identity Mapping

| A2A concept | Light runtime mapping |
|-------------|-----------------------|
| Published agent | `host_id + agent_def_id + definition_version`. |
| Authenticated caller | Bound principal and optional user from validated gateway delegation. |
| `contextId` | External alias for a durable `agent_session_t` row scoped to host, principal, agent, and policy. |
| A2A task ID | External alias for a durable agent turn/job. |
| Message ID | Idempotency key for durable turn admission. |
| Task status | Projection of durable turn/action state into A2A task state. |
| Artifact | Bounded projection of durable result/artifact metadata. |

External identifiers may be opaque gateway-safe IDs rather than raw database
UUIDs. Every lookup must include authenticated ownership and agent binding.

### State Mapping

Define an explicit, tested mapping rather than string substitution. For example:

| Light state | A2A task state |
|-------------|----------------|
| queued/received | submitted |
| running model/action/reconciliation | working |
| waiting approval or additional user input | input-required or auth-required as appropriate |
| completed | completed |
| failed | failed |
| cancelled | canceled |
| policy or agent refusal before/during work | rejected |
| unknown/operator-required with indeterminate outcome | unspecified; never reported as completed or failed without evidence |

For A2A 1.0 these correspond to `TASK_STATE_REJECTED` and
`TASK_STATE_UNSPECIFIED`; compatibility profiles map to their version-specific
wire names. The exact vocabulary must be pinned to the selected A2A version.
Lossy mapping preserves the original Light state in internal audit, not in an
ungoverned public extension.

### Memory Boundary

The embedded A2A server binds the authenticated session identity before native
`light-agent` domain admission. `light-agent` selects and validates the memory
bank, loads session history, recalls relevant memory, and retains accepted
experience according to its immutable memory policy.

Recalled memory remains untrusted context. It cannot change A2A routing,
published skills, authorization, destination, credentials, or task ownership.
See [Hindsight Memory](../../design/hindsight-memory.md).

## Governed Outbound A2A

Outbound A2A requires a catalog binding distinct from public inbound
publication. An external agent registration should include:

- stable host-scoped external-agent or API-version identity;
- discovered Agent Card URL and last accepted card digest;
- discovery time, expiry, trust status, reviewer, and revocation state;
- selected protocol binding and version;
- approved destination and redirect policy;
- credential/delegation policy reference;
- allowed calling agents, principals, environments, and data classifications;
- card signature verification state and trust anchor;
- declared skills and a policy-filtered internal search projection;
- declared extensions, required flags, selected allowlist decisions,
  dependencies, and implementation/schema digests; and
- connection, request, stream, concurrency, and cost limits.

Discovery is an onboarding or refresh workflow, not a request-path operation.
The refresh worker fetches the card with restricted egress, validates it,
records provenance and signature results, computes a digest, and produces an
approved runtime binding. A changed card remains pending until automatic policy
or human review accepts the new capabilities and destination. A newly declared
required extension, an optional-to-required transition, a changed extension URI
or dependency, or a previously approved extension becoming deprecated or
revoked always requires review. An unapproved required extension makes the
binding non-executable; it is never passed through transparently.

Governed outbound is mandatory for the first production milestone. At runtime
the calling `light-agent` chooses a stable Portal `agentRef` from its effective
catalog and sends the call through the published Gateway and `light-a2a` path.
The model, workflow payload, and caller never select the Agent Card URL, target
service, credential reference, or physical destination. `light-a2a` resolves
the approved binding, verifies its active trust and revocation state, applies
server-owned credentials or delegation, and enforces the calling principal,
calling agent, target agent, operation, skill, environment, data boundary,
delegation depth, budget, and task/context policy.

The initial outbound production profile is deliberately bounded to the same
selected JSON-RPC message, streaming, task lookup, subscription, and
cancellation capabilities qualified for inbound use and supported by the
target binding. Arbitrary runtime discovery, model-selected destinations, push
notifications, public A2A HTTP+JSON, public A2A gRPC, and custom bindings remain
deferred.

### Workflow `call: a2a` Migration

The existing Workflow DSL model has optional author-supplied `agentCard` and
`server` fields even though `light-workflow` currently rejects `call: a2a` as
unimplemented. Those fields must not become an escape path around this design.

Before enabling execution, add a required stable Portal catalog `agentRef` and
compile it to an approved `light-a2a` binding. In governed mode validation must
reject `agentCard`, raw `server` URIs, embedded credentials, and any combination
of legacy destination fields with `agentRef`. If legacy syntax must remain
parseable for schema compatibility, it stays non-executable and produces a
specific migration error. Workflow runtime and model-generated data never
select the physical endpoint.

## Authentication, Authorization, And Delegation

### Inbound

- Public card access may be anonymous only when publication policy says so.
- Extended cards and A2A operations require the declared authentication scheme.
- Gateway JWT/session authentication binds host, principal, environment, and
  audience before A2A authorization.
- Gateway route resolution binds the request to an active `instanceApiId` and
  uses its generated `card` or `invoke` endpoint in `rule.endpointRules` for
  coarse per-agent admission. Portal-generated identities, not caller data,
  populate the rule context.
- The selected `light-agent` or `light-a2a` runtime independently validates the
  delegated identity and evaluates the stable agent, skill, abstract operation,
  tenant, task/context ownership, data-boundary, delegation-depth, budget, and
  limit policy.
- Native `light-agent` admits the authorized operation directly. `light-a2a`
  mints a remote credential or `AuthorizedInvocation` bound to the target
  agent, operation, context/task when known, expiry, environment, and policy
  digest only for its external integration path.
- The external Authorization header is never blindly forwarded when a
  server-owned backend credential or delegation is required.

### Outbound

- The calling agent must have the external agent assigned or otherwise allowed
  by its immutable policy snapshot.
- `light-a2a` authorizes both the caller's agent identity and originating
  human/workload principal after gateway edge admission.
- `light-a2a` resolves credentials from server-owned references and scopes them
  to the configured destination.
- Delegation tokens are short-lived, audience-bound, non-replayable where
  required, and excluded from logs and Agent Cards.

### Task Ownership

Authorization for `GetTask`, `CancelTask`, resumption, or push configuration
must prove that the task belongs to the authenticated caller or that the normal
fine-grained policy explicitly grants the requested operation. Artifact
metadata, content, download, export, deletion, and memory promotion apply the
same rule. No Portal role or operator identity receives implicit task or
artifact authority. Possession of a task ID, context ID, artifact ID, or URL is
never sufficient.

### Fine-Grained Decision Contract

The A2A policy engine evaluates an explicit tuple:

```text
caller principal and calling agent
  x target agent and selected skill
  x A2A operation
  x host, tenant, environment and network zone
  x task/context/artifact ownership
  x data classification and boundary
  x delegation depth, deadline, budget and rate limits
```

Do not collapse every operation into one `a2a.invoke` permission. At minimum,
distinguish card read, extended-card read, message send, message stream, task
read, task list, task cancel, task subscribe, and each push-configuration
operation. Artifact metadata read, content read or download, export, deletion,
and promotion to memory are also distinct operations; an allow decision for
task read does not automatically grant every artifact operation.

This requirement applies to the selected runtime's authoritative A2A policy.
The Gateway's generated `invoke` policy endpoint is only coarse admission to a
particular agent and does not replace or satisfy any operation-specific runtime
decision.

The existing delegation contract is tool- and knowledge-oriented. A2A support
must add versioned, operation-specific delegation kinds with target agent,
skill, task/context, tenant, policy digest, data-boundary digest, replay, and
expiry bindings. A generic path or arbitrary-operation grant is not acceptable.

## Security And Privacy

### Agent Card Poisoning

- Validate every card field and URI against the selected version and binding.
- Reject credentials, inline secret material, unsupported required extensions,
  and internal-only addresses in public interfaces.
- Preserve source digest, signature verification, and review provenance.
- Treat descriptions, examples, tags, and extension parameters as untrusted
  display/model context.

### SSRF And Destination Safety

- Resolve only server-owned service identities or approved remote bindings.
- Deny link-local, loopback, private, or metadata-service addresses unless the
  backend is explicitly admitted for that network zone.
- Revalidate resolved addresses and redirect destinations.
- Apply TLS hostname and trust policy after final target resolution.
- Never let card refresh or runtime fallback select an undeclared interface.

### Prompt And Capability Injection

Agent Cards, external agent results, skills, memories, and artifacts are data.
They cannot add tools, elevate execution placement, widen network access,
change credentials, or override system and policy instructions.

### Resource Exhaustion

Enforce independently configurable limits for:

- request, response-inspection, Agent Card, message-part, artifact, and event
  sizes;
- JSON depth, collection size, extension count, and interface/skill count;
- total and per-principal concurrent requests and streams;
- header bytes and header count;
- request, upstream setup, stream idle, and total task wait time; and
- card cache entries, retained runtime generations, and telemetry label
  cardinality.

### Filtering

Request access control may inspect bounded A2A message metadata and content only
when the endpoint policy enables body access. Response filtering applies to
bounded JSON responses and individual bounded stream events. The gateway must
not buffer an unbounded stream to run a whole-response filter.

## Agent Card URL And Signature Rules

Portal-published cards should contain their final public URL before signing.
Public host and scheme come from approved publication configuration, not
untrusted `Host` or forwarding headers.

### Signing Authority And Identity

`light-oauth` is the first-production signing and JWKS authority for Light
Agent Card publications. This reuses the platform's existing operational
boundary for asymmetric signing, `kid` selection, public-key distribution, and
rotation, but it does not reuse an OAuth access-token or long-lived-token key
as an Agent Card key. OAuth JWT issuance and Agent Card publication are
different cryptographic purposes with independent compromise, rotation,
revocation, and audit boundaries.

The first implementation extends `light-oauth`; it does not introduce a second
network service solely for A2A keys. Reusable signing-profile, KMS/HSM adapter,
JWS, and JWKS lifecycle modules should remain separable so a future general
`light-signing` service can be extracted if additional platform artifacts adopt
the same authority. That extraction must preserve profile IDs, trust URLs, and
audit semantics and is not an A2A production prerequisite.

The default Agent Card signing identity is the tuple:

```text
host/tenant + environment + publication purpose
```

The initial purposes are:

| Purpose | Signs | Meaning to a verifier |
|---------|-------|-----------------------|
| `A2A_CARD_NATIVE` | Native `LIGHT_AGENT` publications | The named Light host and environment approved this native Agent Card publication. |
| `A2A_CARD_EXTERNAL_FACADE` | `EXTERNAL_SIDECAR` and rewritten `REMOTE_A2A` facade publications | The named Light host and environment validated and published this governed external facade; it does not claim that the upstream vendor signed the rewritten content. |

The signing profile is the issuer identity, a rotating `kid` identifies one key
under that profile, the individual agent publication is the signed subject, and
the `light-agent` or `light-a2a` process is only the serving runtime. Runtime
fleet, replica, and instance IDs never define the issuer because topology may
change without changing publication ownership. One key per agent publication
is not the default; an independently delegated per-agent profile is an optional
high-assurance override with its own lifecycle and approval.

The native and external-facade profiles use separate key rings even when they
belong to the same host and environment. A caller authorized for one purpose
cannot request the other purpose, select an arbitrary `kid`, or use an OAuth
provider key. `light-oauth` chooses the current key after resolving the
authorized profile. If the signature includes `jku`, it points to the stable
public JWKS for that exact profile; verifiers must trust the profile authority
and must not treat possession of any key published by the same service as
equivalent. Runtime verification pins the expected `light-oauth` origin and
profile from the accepted control-plane projection. It never follows an
arbitrary card-provided `jku`; when `jku` is present, it must equal the projected
profile JWKS URL.

### Signing Profile And Key Lifecycle

Portal adds logical signing-profile and signing-key records. Exact DDL names may
follow Portal conventions during implementation, but the model contains:

```text
signing profile
  hostId, environment, profileId, purpose, algorithm, jwksUrl
  rotationPolicy, validity, revocationEpoch, active

signing key
  profileId, kid, publicJwk, privateKeyRef
  state = CURRENT | PREVIOUS | REVOKED
  validFrom, validUntil, rotation/revocation audit
```

`privateKeyRef` is a server-side reference to a KMS, HSM, or approved secret
provider. Production A2A private keys are not stored as plaintext Portal table
values. A privileged Portal form may select an approved logical managed-key
provider or alias, but private material and the resolved provider resource
reference are not exposed in forms, Config Server runtime properties, Agent
Cards, logs, or telemetry. Only `light-oauth` resolves the backing key
reference. Existing OAuth provider and provider-key records remain OAuth-owned
and are not repurposed for A2A.

Portal View exposes a structured **Signing Profiles** table under the host and
environment administration boundary. The form manages purpose, algorithm,
approved managed-key provider or logical alias, rotation policy, validity,
status, and revocation. It displays the platform-derived JWKS URL,
current/previous `kid` values, and audit history but never private material. A2A
bindings inherit the environment's default native or external-facade profile;
selecting a non-default or per-agent profile requires an explicit authorized
override. Raw JSON is not the primary editor.

Key generation, scheduled or administrative rotation, retirement, and
emergency revocation use the normal Portal command, event, projection, and
audit path. The `light-oauth` public JWKS contains the current key and previous
keys only for the documented verification overlap. Normal rotation signs new
publications with the new current key only after that public key is retrievable
from the profile JWKS. It retains old public keys through the maximum publication
validity, card-cache, and rollback windows. Emergency revocation removes the key
from the published JWKS, advances the affected revocation epoch, immediately
blocks local serving of cards that depend exclusively on that key, and requires
a new signed publication before service resumes. External verifiers observe the
removal within the documented JWKS cache bound. Multiple Agent Card signatures
may be assembled by the controlled publication workflow during rollover when
supported by the selected A2A profile.

### Signing And JWKS Service Contract

`light-oauth` exposes a purpose-specific Agent Card signing operation and a
public profile JWKS operation. Exact HTTP paths are finalized with the
`light-oauth` OpenAPI, but the semantic operations are:

```text
SignAgentCard(profileId, publicationId, finalCardWithoutSignatures)
GetSigningProfileJwks(profileId)
```

`SignAgentCard` is authenticated and fine-grained-authorized. It resolves host,
environment, purpose, publication authority, algorithm, and current key from
server-owned state; validates that the workload may use the profile for the
publication; binds the authorization to `agentDefId`, `publicationId`, purpose,
and canonical content digest; requires the signing payload to omit existing
signatures; validates and JCS canonicalizes the final Agent Card; and returns
the A2A JWS signature, `kid`, JWKS location, canonical digest, and audit
reference. It is not a generic
arbitrary-byte or caller-selected-key signing endpoint. The public JWKS
operation exposes only verification material and applies bounded cache headers
compatible with rotation and emergency revocation.

An independently verified upstream or rollover signature is retained outside
the `SignAgentCard` request and may be assembled into the final `signatures`
array only when it covers the same canonical digest. It is never treated as
input authority for selecting the Light signing profile or key.

The normal publication workflow calls this operation once after all public URLs
and optional fields are final, verifies the result, and projects the complete
signed card. `light-agent` and `light-a2a` validate the signature and digest when
activating a projection and then serve the accepted immutable card without a
request-path `light-oauth` call. An explicitly approved activation-time fallback
may call `light-oauth` with a logical profile ID when the final card can only be
constructed at that boundary; it caches the result and still never receives a
key or key reference. `light-gateway` never calls the signing operation.

For transparent proxy cards:

- rewrite only an interface URL whose origin and complete path match the
  configured backend agent base;
- preserve an approved relative subpath and query according to binding rules;
- reject interface URLs that escape the backend binding;
- never partially match path segments;
- remove stale `Content-Length`, ETag, and upstream signature after mutation;
- generate a new ETag from final canonical content, disclosure class,
  authorization-policy digest, and revocation epoch; and
- obtain a new signature only through the configured
  `A2A_CARD_EXTERNAL_FACADE` `light-oauth` profile.

Legacy top-level `url` and current `supportedInterfaces` are handled by separate
version profiles. A malformed hybrid card is rejected rather than guessed.

## Errors And Failure Mapping

Use stable internal error codes plus binding-correct A2A errors. At minimum,
distinguish:

- unsupported A2A version, binding, operation, or media type;
- invalid or unavailable explicitly activated extension;
- missing client activation of a published required extension in the 1.0
  profile as `ExtensionSupportRequiredError`;
- invalid JSON-RPC envelope or A2A payload;
- unknown or unauthorized agent, context, or task;
- task not cancelable;
- public or extended card unavailable;
- card signature or publication validation failure;
- request or response size/depth limit exceeded;
- backend unavailable, timeout, protocol violation, or invalid agent response;
- access-control denial and response-filter denial; and
- stale, expired, or revoked publication.

Do not translate a JSON-RPC error returned with HTTP 200 into success telemetry.
Conversely, do not expose internal topology, database state, policy expressions,
credential identifiers, or parsing details in public error messages.

## Observability

### Traces And Logs

Record bounded, low-cardinality fields such as:

- `a2a.version` and `a2a.binding`;
- `a2a.operation` and safe JSON-RPC method;
- publication ID/version/digest prefix;
- signing profile purpose, safe profile identifier, `kid`, and signing or
  verification outcome;
- stable agent definition or external-agent reference;
- route and backend kind;
- response outcome and A2A error class/code;
- result kind and task state;
- hashed or otherwise policy-safe context/task correlation;
- streaming/non-streaming mode;
- config generation; and
- request, upstream, first-event, and total duration.

Do not log prompts, message parts, artifacts, memory content, credentials,
complete JWTs, or high-cardinality raw task/context IDs by default.

### Metrics

Provide counters and histograms for:

- requests by operation, version, binding, route, and outcome;
- card serves, upstream fetches, cache hits, validation failures, and signature
  outcomes;
- active streams, stream setup/idle failures, events, and disconnects;
- task create/get/cancel outcomes;
- backend latency and protocol violations;
- authorization and filtering denials;
- rejected config reloads and last-known-good retention; and
- admission, concurrency, body-size, and timeout rejections.

Agent ID, task ID, context ID, user ID, and arbitrary skill names must not become
unbounded metric labels.

## Reload, Caching, And Availability

- Compile and validate a complete generation off the request path.
- Keep every target snapshot internally complete and atomic. Coordinate the
  cross-service release through one exact target-to-snapshot manifest,
  compatible adjacent generations, explicit reload, per-target
  acknowledgement, and exact-generation rollback.
- Keep the previous valid generation when a refresh is malformed.
- Fail closed when a publication is expired or revoked.
- Cache final public cards by publication digest, disclosure class,
  authorization-policy digest, and revocation epoch.
- Cache each pinned signing-profile JWKS only for its bounded cache lifetime.
  Refresh on an unknown `kid` during projection activation; if the trusted JWKS
  is unavailable or the key/profile check fails, reject the new generation and
  retain only a still-valid last-known-good generation. Agent Card requests do
  not perform JWKS network fetches.
- Use ETag/conditional requests without allowing stale authenticated disclosure
  after authorization or revocation changes.
- Preserve only bounded edge correlation in gateway memory.
- Store external adapter correlation durably in `light-a2a` when required.
  Native `light-agent` stores its A2A context/task aliases with its durable
  session and turn state; the selected agent runtime remains authoritative for
  business task state.
- A gateway restart must not change task ownership or make a durable task
  permanently unreachable.

## Implementation Phases

These phases describe implementation order, not optional production scope.
Inbound paths may be enabled first in development and canary environments, but
the first production release requires the applicable Phase 0 through Phase 5
exit gates. Phase 6 capabilities remain explicitly deferred. Release evidence
must include at least one governed inbound native-agent path, one governed
inbound external-integration path, and one governed outbound remote-agent path.

### Phase 0: Contract And Threat Model

Deliver:

- an exact A2A 1.0 normative tag or commit, accepted errata level, TCK version,
  and separate pinned 0.3 compatibility fixtures; the implementation must not
  depend on an unversioned `latest` specification page;
- canonical internal operation and error models;
- request/response and Agent Card size/depth limits;
- inbound and outbound route, task-ownership, signing, SSRF, confused-deputy,
  delegation, data-exfiltration, and disclosure threat model;
- task-artifact ownership, fine-grained operation, retention, legal-hold,
  deletion-evidence, external-reference, and memory-promotion contracts;
- private `light-a2a-backend/v1` OpenAPI/JSON Schema authority, signed-context,
  loopback HTTP/JSON, SSE, restart-reconciliation, SDK, and conformance
  contracts;
- purpose-separated native and external-facade signing-profile, `light-oauth`
  signing/JWKS, rotation, revocation, and audit contracts;
- extension registry, exact-URI/versioning, optional-ignore, required-error,
  dependency, metadata-isolation, and runtime-handler contracts;
- profile-scoped extension configuration, single-generation profile validation,
  and 1.0/0.3 isolation rules for the runtime projection schema;
- compatibility matrix and explicit deferred features; and
- handler/config/module contracts.

The baseline record must cite the official
[A2A specification repository](https://github.com/a2aproject/A2A/blob/main/docs/specification.md)
and [changelog](https://github.com/a2aproject/A2A/blob/main/CHANGELOG.md), then
freeze the exact revision used by generated models and conformance fixtures.

Exit gates:

- all selected normative fixture shapes parse and canonicalize deterministically;
- malformed, oversized, ambiguous-version, invalid-activated-extension, and
  destination-escape fixtures fail closed, while an unknown optional 1.0
  extension remains inactive and isolated;
- a missing published required extension in the 1.0 profile returns
  `ExtensionSupportRequiredError`, the 0.3 profile rejects every extension
  declaration or activation during publication and projection compilation, and
  an unapproved required extension cannot enter an active publication;
- extension configuration is expressible only per profile, a multi-generation
  profile is rejected, and a 1.0 profile's extension set cannot reach an agent
  bound to a 0.3 profile in the same projection;
- card mutation tests prove no stale signature is preserved;
- no implementation phase starts with unresolved ownership or retention of
  durable tasks and task artifacts; and
- the public A2A binding and private sidecar backend protocol are represented as
  distinct contracts and cannot be enabled or versioned through each other's
  configuration.

### Phase 1: Shared A2A Foundation And Transparent Federation

Deliver:

- shared A2A protocol, runtime, client, policy, policy-envelope, and artifact
  lifecycle contracts;
- registered `apps/light-a2a` service on `light-axum` and `light-runtime`;
- minimal gateway `a2a-router` edge module and implementation-kind service
  routing contract, including public-route-to-Instance-API resolution and
  exact generated `card` and `invoke` policy endpoints;
- JSON-RPC 1.0 plus explicit 0.3 compatibility;
- well-known card proxying and safe URL rewriting;
- gateway edge authentication plus shared fine-grained A2A policy integration;
- shared 1.0 parsing and negotiation for `A2A-Extensions`, with profile-scoped
  extension configuration, empty advertised, inbound, outbound, and required
  sets in every production profile, and compile-time rejection of extension
  configuration in the 0.3 profile;
- immutable `light-a2a` projection loading, validation, last-known-good reload,
  expiry, and revocation;
- streaming pass-through; and
- A2A telemetry.

Exit gates:

- unit and integration matrices cover both card generations, complete path
  matching, public host rules, JSON-RPC errors with HTTP 200, streaming, body
  limits, timeout, client disconnect, and reload;
- unsupported methods and versions never fall through to a generic proxy;
- unknown optional extension metadata cannot reach runtime policy, models,
  backends, artifacts, logs, or telemetry dimensions;
- two-node gateway and `light-a2a` tests prove no accidental affinity to one
  edge process and correct registered-service routing;
- two agents with identical raw A2A specification endpoints retain distinct
  routes and coarse authorization decisions without `endpointRules` overwrite
  or permission leakage; and
- soak testing shows bounded memory and stream cleanup.

### Phase 2: Portal-Published Agent Cards

Deliver:

- reuse of the Agent registration publication foundation for native
  `LIGHT_AGENT`; Phase 2 must not create a second Agent compiler, snapshot
  lifecycle, current pointer, reload protocol, or acknowledgement store;
- Portal A2A publication authoring and validation;
- managed extension registry and dependency records, structured Portal View
  forms, and binding selectors that ship with no extension eligible for initial
  production activation;
- structured Portal View artifact-retention profiles, host defaults, agent
  overrides, fine-grained access-control policy linkage, effective preview, and
  immutable Config Server compilation;
- reusable public provider profiles and version-scoped Agent Public Metadata
  authoring, validation, source attribution, and effective-card preview;
- public disclosure projection; extended disclosure remains Phase 6 work;
- structured, tenant-scoped public skill aliases managed on the Skill form,
  frozen after first publication, and mapped immutably to internal skill UUID,
  version, and digest;
- safe AgentSkill mapping that never exposes Portal UUIDs, instructions,
  executable source, or internal tool/workflow placement;
- normalized Portal authoring tables, immutable versioned JSONB publication
  aggregates, and separate compiled Gateway and selected `light-agent` or
  `light-a2a` Config Server projections;
- publication of bounded `agentPolicy.skills` and effective-catalog values or
  immutable catalog/package references into instance properties and activated
  Config Server snapshots;
- active Gateway Instance API association, unique public path prefix, and
  deployment-scoped `rule.endpointRules` compilation;
- Portal View A2A Bindings table, structured forms, compiled preview,
  validation, explicit publication, revocation, and history diagnostics;
- Portal View host/environment Signing Profiles administration with inherited
  native and external-facade defaults and authorized overrides;
- purpose-separated A2A signing profiles and key lifecycle in `light-oauth`,
  including authenticated Agent Card signing, public profile JWKS, rotation,
  revocation, and signing audit;
- publication-time signed-card generation and verification, plus `light-agent`
  and `light-a2a` activation-time signature validation, ETag, cache, expiry, and
  revocation; and
- Portal UI/API visibility and publication diagnostics.

Exit gates:

- the generic Agent publisher can activate and reload a non-A2A Agent before
  the optional native `a2aPolicy` overlay is enabled, and adding the overlay
  produces one new combined Agent snapshot generation rather than a separately
  activated A2A generation;
- aggregate-version and publication-digest changes are deterministic;
- effective name, description, provider, documentation URL, icon, and semantic
  version follow the pinned precedence rules and retain source provenance;
- public skill aliases are normalized, unique within the host, stable across
  compatible revisions, reproducible from the publication, never expose a
  Portal UUID, and never become rebound to a different internal skill;
- inactive/unassigned/private skills never leak into public cards;
- runtime skill and catalog loading succeeds from the activated Config Server
  generation while Portal query is unavailable, and mutable authoring changes
  have no effect until a new publication is activated;
- artifact-retention defaults and agent overrides compile deterministically,
  load without Portal availability, and reference the existing fine-grained
  access-control policy without creating a parallel ACL;
- first-production cards and runtime projections contain empty extension sets in
  every profile, and arbitrary binding JSON cannot introduce or require an
  extension or move extension configuration outside its profile;
- native and external-facade cards verify against different host/environment
  signing profiles after final public URL generation;
- OAuth token keys, a wrong-environment profile, a wrong-purpose profile, a
  caller-selected `kid`, and an unauthorized signing workload all fail closed;
- rotation proves new cards use the current `kid`, cached and rollback cards
  remain verifiable only for the bounded overlap, and emergency revocation
  blocks affected cards until a newly signed publication is active;
- no publication using a new `kid` activates before that key is available from
  the pinned profile JWKS, and JWKS failure rejects the new generation without
  breaking a still-valid last-known-good card;
- revocation removes or denies the card within the documented propagation
  bound; and
- gateway, `light-agent`, and `light-a2a` operate without request-path access to
  Portal authoring or projection tables.

### Phase 3: External Business Agent Sidecar

Deliver:

- `EXTERNAL_SIDECAR` Portal implementation and binding model;
- shared and sidecar deployment profiles for the same `light-a2a` binary;
- versioned `AgentBackend` SDK and signed `AuthorizedInvocation` contract;
- canonical `contracts/a2a-backend/v1` OpenAPI/JSON Schema contract, Rust
  reference adapter, golden vectors, and language-neutral TCK;
- fixed-loopback HTTP/JSON backend transport with bounded unary operations and
  SSE for declared streaming backends;
- production Python, Java, and TypeScript/Node.js SDKs that expose only the
  business callbacks and own the transport/security plumbing;
- task, context, cancellation, status reconciliation, idempotency, and
  streaming adaptation;
- managed sidecar artifact validation, scanning, tenant-scoped storage,
  fine-grained access, expiry, legal hold, and verified deletion;
- sidecar Controller registration metadata; and
- developer templates containing business logic only.

Exit gates:

- the business backend receives neither raw caller tokens nor Portal policy;
- Python, Java, and TypeScript reference backends pass the same contract,
  signed-context, unary, streaming, status, cancellation, artifact, deadline,
  error, and restart TCK against the same `light-a2a` build;
- forged, expired, replayed, wrong-audience, wrong-agent, wrong-skill,
  wrong-operation, wrong-task, and wrong-context invocation attempts fail
  closed;
- the sidecar cannot call an unconfigured destination or cross tenant/network
  boundaries;
- sidecar/backend restarts reconcile detached work by signed task and backend
  operation identity without duplicating effects or guessing a terminal state;
- task-owner defaults, explicit fine-grained artifact grants and denials,
  expiry, legal hold, verified deletion, and deletion tombstones survive
  sidecar/backend restarts; and
- a reference external agent passes conformance, security, reload, audit, and
  telemetry gates without implementing platform plumbing.

### Phase 4: Portal-Native `light-agent` Integration

Deliver:

- shared A2A server, card, policy, and task modules embedded in `light-agent`;
- direct Gateway routing to the registered `light-agent` for `LIGHT_AGENT`;
- native inbound `a2aPolicy` compiled into the immutable `agent.yml`
  projection, with no `light-a2a` sidecar;
- reuse of Config Server-projected `agentPolicy.skills` and
  `agentPolicy.catalog.effectiveCatalog`, with no live Portal catalog query as
  runtime authority;
- durable context/session and task/turn mapping;
- message idempotency, task lookup, cancellation, and streaming event mapping;
- native task-artifact persistence through the shared lifecycle, independently
  governed from session history and Hindsight memory;
- authenticated task and artifact ownership and fine-grained operation checks,
  with normal access-control audit; and
- memory and effective-catalog integration through existing `light-agent`
  boundaries.

Exit gates:

- gateway and `light-agent` restarts preserve native task lookup and ownership
  without requiring a `light-a2a` process;
- duplicate message IDs do not create duplicate turns or effects;
- cross-principal and cross-agent context/task probes fail closed;
- cross-principal artifact probes, guessed download URLs, and implicit Portal
  administrator access fail closed unless the normal fine-grained policy
  explicitly grants the requested operation;
- cancellation and terminal-state races are deterministic;
- artifact expiry does not delete chat history or Hindsight memory, and memory
  retention does not keep an otherwise expired artifact retrievable;
- memory failure does not retry an accepted effectful action;
- a task pinned to one publication retains the same public-alias-to-skill digest
  mapping across a compatible skill update; and
- A2A and existing `/chat` paths produce equivalent governed agent behavior for
  an agreed test corpus.

### Phase 5: Required Governed Outbound A2A

Deliver:

- external-agent onboarding and card refresh;
- end-to-end `light-agent` outbound invocation through the published Gateway
  route and `light-a2a` binding;
- Workflow `call: a2a` `agentRef` contract, validator, and migration errors for
  legacy `agentCard` or `server` destinations;
- trust, signature, review, digest, and revocation lifecycle;
- remote-card extension review that permits core calls without activating
  unapproved optional extensions and blocks any unapproved required extension;
- policy-filtered external-agent discovery for Light agents;
- server-owned destination and credential resolution;
- managed import or explicitly ephemeral handling of remote task artifacts,
  without treating an upstream URI as a durable platform object;
- outbound operation, skill, task/context, delegation-depth, budget,
  loop-prevention, and data-boundary enforcement;
- bidirectional principal, calling-agent, target-agent, task, and policy-snapshot
  audit correlation; and
- changed-card review workflow.

Exit gates:

- arbitrary URL, redirect, DNS-rebinding, private-address, and credential
  substitution tests fail closed;
- a production-profile `light-agent` completes an outbound message, stream,
  task lookup, and cancellation through a multi-node Gateway and `light-a2a`
  deployment without accepting caller- or model-selected destination data;
- an agent can call only assigned external agents and approved operations;
- delegation loops, excessive depth, expired budgets, cross-environment calls,
  and disallowed data classifications fail closed;
- changed or revoked cards cannot silently widen capabilities;
- a retained outbound artifact is available from managed tenant storage after
  the remote reference disappears, while a binding configured for ephemeral
  references makes no local-retention promise;
- a newly required extension, optional-to-required transition, changed extension
  URI/dependency, or revoked registry entry quarantines the remote binding; and
- outbound audit correlates human/workload principal, calling agent, target
  agent, task, and policy snapshot without exposing credentials.

### Phase 6: Additional Bindings, Extensions, And Push

Add public A2A HTTP+JSON, public A2A gRPC, extended cards, push notifications,
custom bindings, sidecar private-network mTLS or gRPC, additional backend SDK
languages, or individual extensions only as independent profiles with
conformance fixtures and operational qualification. Start with optional
data-only extensions. A profile, method, state-machine, transport, SDK language,
or required extension needs explicit threat model, compatibility, dependency,
handler, error-mapping, downgrade, and rollback evidence before activation.

Push notification delivery additionally requires approved callback
registration, callback ownership verification, SSRF controls, HMAC or mTLS,
replay protection, retry budgets, dead-letter handling, and durable delivery
state outside the gateway process.

## Testing Strategy

### Unit And Property Tests

- version and binding parsing;
- JSON-RPC envelope and A2A model validation;
- Agent Card canonicalization, mapping, URL rewriting, and signing;
- host/environment/purpose signing-profile selection, JWS/JWKS validation,
  key-overlap, and revocation behavior;
- public metadata precedence, SemVer, provider-profile, documentation-URL, and
  managed-icon validation;
- public skill-alias normalization, host uniqueness, first-publication freeze,
  immutable UUID/version/digest mapping, and no-rebinding validation;
- extension URI/version matching, direction and operation selection, dependency
  closure, required eligibility, optional-ignore behavior, required-error
  behavior, schema validation, and activated-response echoing;
- deterministic policy-endpoint generation and route-to-Instance-API binding
  validation;
- full-segment path matching and host/scheme construction;
- state and error mapping;
- `light-a2a-backend/v1` OpenAPI and JSON Schema validation, canonical golden
  vectors, unknown-field behavior, and public-A2A/private-backend version
  isolation;
- signed backend invocation validation for unary, streaming, status, and cancel,
  including exact task, context, idempotency, and backend-operation bindings;
- artifact-retention profile resolution and admission-time freezing;
- artifact operation authorization, digest and reference validation, managed
  import, expiry, legal-hold, deletion retry, verified absence, and tombstones;
- chat-reference and explicit memory-promotion lineage without payload
  duplication or implicit retention coupling;
- body, depth, collection, header, and event limits; and
- redaction and telemetry-cardinality guards.

Use property/fuzz tests for URI rewriting, JSON nesting, JSON-RPC IDs, extension
lists, streaming event framing, and malformed cards.

### Integration Tests

- A2A 1.0 client to external backend through `light-gateway`;
- A2A 0.3 compatibility route isolated from 1.0-only routes;
- public Agent Card access, plus authenticated extended-card access only when
  its independently authorized Phase 6 profile is enabled;
- JWT, endpoint authorization, delegation, and response filtering;
- two agent APIs with identical raw A2A endpoints, different public prefixes,
  and different roles proving isolated allow and deny decisions;
- service discovery, TLS, backend failures, and reload;
- Portal publication through `light-oauth` signing and JWKS verification for
  both native and external-facade profiles, including key rotation;
- Portal skill assignment through instance-property staging, immutable snapshot
  activation, `agent.yml` loading, and runtime acknowledgement, including
  Portal-query unavailability after activation;
- empty first-production extension profiles, unknown optional extension
  isolation, activated-extension response negotiation, missing-required errors,
  and remote-card required-extension quarantine;
- activation of one runtime projection serving both a 1.0 profile and a 0.3
  profile, proving profile-scoped extension isolation, rejection of 0.3
  extension configuration, and rejection of a multi-generation profile;
- direct `light-gateway` to native `light-agent` message, stream, lookup, cancel,
  duplicate, and subscription/reconnection;
- fixed-loopback `light-a2a-backend/v1` unary, SSE streaming, status
  reconciliation, and cancellation, including sidecar and backend restarts;
- Python, Java, and TypeScript/Node.js reference business backends passing the
  same language-neutral TCK against one `light-a2a` build;
- task-owner artifact access plus explicit fine-grained grants and denials for
  metadata, content, download, export, deletion, and memory promotion;
- Config Server artifact-policy activation, 24-hour chunk cleanup, 30-day task
  and content expiry, longer metadata retention, legal hold, and verified
  object-store deletion without a live Portal query;
- independent chat, artifact, and Hindsight retention, including explicit
  artifact-to-memory promotion with provenance and privacy-erasure lineage;
- governed `light-agent` outbound message, stream, lookup, and cancellation via
  `agentRef`, including target revocation during an active task;
- multi-gateway routing during a long-running durable task; and
- Workflow `call: a2a` alias resolution plus rejection of raw `agentCard` and
  `server` destinations.

### Security Tests

- cross-host/principal/agent/task access;
- guessed artifact IDs and download URLs, cross-artifact operation escalation,
  task-read-to-content-read escalation, implicit administrator access, expired
  artifact probes, and object-reference leakage;
- Agent Card prompt injection and secret scanning;
- Portal UUID, private instruction, executable source, and artifact credential
  leakage through public AgentSkill projections;
- extension-header count/size abuse, URI dereference/SSRF, untrusted parameter or
  metadata injection, dependency cycles, handler/schema substitution, downgrade,
  optional-to-required transition, and required-flag bypass;
- signed-card mutation and downgrade attempts;
- cross-host, cross-environment, cross-purpose, OAuth-key reuse, unauthorized
  signing-call, caller-selected-key, stale-key, and revoked-key attempts;
- arbitrary destination and DNS/redirect SSRF attempts;
- oversized/deep JSON and streaming resource exhaustion;
- replayed message/delegation/push requests;
- outbound confused-deputy, delegation-loop, cross-environment,
  model-selected-destination, and disallowed-data-boundary attempts;
- caller attempts to forge `instanceApiId`, `agentDefId`, policy endpoint, or
  target service through headers, parameters, or request bodies;
- forged or replayed `AuthorizedInvocation`, raw-token leakage, sidecar bypass,
  unauthorized local backend access, wrong-task cancellation, wrong-operation
  status lookup, non-loopback or wildcard backend origins, redirects, proxy
  environment substitution, and SDK validation bypass in each supported
  language;
- memory or skill content attempting to alter runtime authority; and
- mutable hybrid-query results, changed artifact references, digest mismatch,
  unsigned packages, and alias-rebinding attempts altering an active runtime.

## Rollout And Compatibility

- Ship the A2A handler disabled until a validated profile and route exist.
- Enable one internal transparent-proxy canary before Portal-published cards.
- Enable and qualify a governed outbound canary after the inbound contracts are
  stable; do not declare production readiness until both directions pass their
  release gates.
- Keep A2A 0.3 and 1.0 as separate route profiles and metrics dimensions.
- Version the private `light-a2a-backend/v1` contract independently from public
  A2A versions; a public-protocol upgrade must not silently change a business
  backend callback or SDK wire model.
- Do not automatically upgrade an external registration to a new major version.
- Do not automatically activate a new extension, required flag, dependency, or
  extension version discovered during card refresh.
- Publish compatibility and deprecation windows through Portal.
- Retain a one-generation rollback target while it remains valid and unrevoked.
- Roll back by publication generation, not by editing live card JSON.

## Resolved Design Decisions

1. Portal uses normalized A2A binding authoring tables, an immutable versioned
   JSONB publication aggregate, and compiled audience-specific Config Server
   projections. Portal View manages bindings through a table and structured
   forms; raw compiled JSON is a read-only preview. CRUD changes Draft state,
   while an explicit publication workflow validates, snapshots, activates, and
   records runtime acknowledgement.
2. A `LIGHT_AGENT` binding routes directly to native A2A modules embedded in
   `light-agent`; it never deploys a `light-a2a` sidecar. The `light-a2a`
   sidecar is exclusively for external business agents that do not implement
   Light-Fabric platform concerns. Shared-service `light-a2a` remains the
   governed federation boundary for remote A2A servers.
3. Each logical agent remains an API/API-version asset, with
   `agentDefId == apiVersionId`. An `agt` product version describes deployable
   runtime compatibility and does not replace the logical agent identity.
4. Publishing an agent through a Gateway requires an active `instance_api_t`
   association and unique public path prefix. Portal compiles opaque policy
   endpoint keys scoped by `instanceApiId`; Gateway uses them for coarse card
   or invocation admission, while the selected runtime retains fine-grained
   A2A operation and skill authorization.
5. Agent Card public metadata uses deterministic Portal sources: name from the
   API, version-specific description with API-description fallback, semantic
   version from the API version, provider from an approved public provider
   profile, and documentation/icon from version-scoped agent public metadata.
   The immutable publication is runtime-authoritative; bindings, model-provider
   fields, runtime overrides, and unreviewed upstream cards are not.
6. The first production milestone requires both governed inbound publication
   and governed outbound invocation. Inbound may land first for development and
   canary qualification, but Phase 5 catalog resolution, trust, credentials,
   delegation, data-boundary enforcement, loop/budget controls, and correlated
   audit are production gates. Phase 6 bindings and push remain deferred.
7. `light-oauth` is the first-production Agent Card signing and JWKS authority.
   Native and external-facade publications use separate key rings scoped by
   host, environment, and purpose. The agent publication is the signed subject;
   a runtime fleet or instance is not the issuer. Portal manages structured
   signing profiles and lifecycle, Config Server projects only the signed card
   and logical profile metadata, and no OAuth token key, private key, or backing
   KMS/HSM reference is projected to `light-agent`, `light-a2a`, or Gateway.
8. Public A2A skill IDs are stable, tenant-scoped publication aliases managed as
   structured Portal skill data; Portal UUIDs remain internal. Every immutable
   agent publication records the alias-to-UUID/version/digest mapping. Config
   Server delivers bounded skill instructions and capability descriptors as
   runtime authority, while executable packages live in signed,
   content-addressed artifact storage. Live Portal hybrid queries support
   authoring, validation, compilation, and diagnostics; any runtime search is
   bounded to an immutable publication and cannot hot-load mutable skills or
   executable authority.
9. The first production profiles advertise and activate no A2A extensions. All
   future activated extensions use exact, versioned URIs from a Portal-managed
   allowlist; required extensions are initially prohibited and later require
   explicit required eligibility, implementation, schema, dependency, security,
   and conformance approval. Unknown optional requests remain inactive and
   isolated. Extension configuration is profile-scoped rather than
   instance-global: every profile is single-generation, a 1.0 profile's sets
   bind only the agents selecting it, and one runtime projection may serve both
   generations without leaking extension policy between them. In the 1.0
   profile, missing support for a published required extension returns
   `ExtensionSupportRequiredError`; the 0.3 profile rejects every extension
   declaration or activation during publication and projection compilation. An
   unapproved required extension in a remote card prevents onboarding or
   activation.
10. A2A task artifacts have retention and visibility independent from chat
    history and Hindsight memory. `TASK_OWNER` is the default, while every
    additional metadata, content, download, export, deletion, or promotion to
    memory uses the existing fine-grained access-control policy; there
    is no implicit administrator authority or separate break-glass path. The
    selected runtime owns tenant-scoped managed storage and freezes the approved
    retention profile at task admission. Initial defaults are 24 hours for
    residual chunks, 30 days for managed content and external task visibility,
    and 365 days for non-content metadata and deletion evidence, with legal
    holds and approved compliance overrides. Config Server distributes only
    immutable rules. Chat stores bounded references, Hindsight ingestion is an
    explicitly authorized derived operation, and external URIs are imported or
    declared ephemeral rather than assumed durable.
11. The first external-developer release standardizes one private
    `light-a2a-backend/v1` contract: HTTP/1.1 with JSON on a fixed loopback
    origin, plus SSE when the backend declares streaming. Python, Java, and
    TypeScript/Node.js are supported production SDKs; the checked-in Rust
    adapter is the reference implementation and conformance oracle. Status
    reconciliation is part of v1 so detached work can survive sidecar restarts.
    Unix-domain-socket HTTP is an optional non-blocking Linux hardening profile.
    Private-network mTLS, gRPC, WebSocket, stdio, FFI/plugins, Go, .NET, and
    other SDK languages are deferred and require independent qualification.
    The private backend contract is versioned independently and never activates
    a public A2A HTTP+JSON binding.
12. Runtime configuration identity is exactly `(host, serviceId, envTag)` and
    current configuration is loaded only through `/configs`. Portal
    `instanceId` and `instanceApiId` values remain internal association,
    publication-target, and audit identifiers; no A2A runtime projection or
    Config Server query treats them as workload identity. Product ID and product
    version describe compatibility and never select runtime configuration.
13. Agent registration, native runtime linking, and A2A exposure are separate
    lifecycle decisions. A native Agent uses one immutable Agent audience
    snapshot containing the base policy and optional `a2aPolicy` overlay. The
    Gateway and any external-integration `light-a2a` runtime receive their own
    least-privilege snapshots under one coordinated release manifest. Current
    pointers may change in one control-plane transaction, but runtime
    application is completed only through explicit reload and per-target
    acknowledgement.

## Completion Criteria

The A2A Gateway feature is complete only when:

- selected A2A profiles pass protocol and negative conformance fixtures;
- Portal can publish and revoke a versioned Agent Card without direct gateway or
  runtime database access;
- every Gateway, `light-agent`, and `light-a2a` target loads its current
  immutable snapshot from `/configs` using only `(host, serviceId, envTag)`, and
  mutable authoring or staged properties have no runtime effect before snapshot
  activation and explicit reload;
- a native Agent can be registered, linked, and activated without A2A, while an
  explicit later A2A publication adds the overlay through one new combined
  Agent snapshot rather than parallel independently active Agent/A2A
  generations or confused native and Gateway `instanceApiId` associations;
- public card content is demonstrably smaller and less privileged than the
  internal effective agent catalog;
- public skill IDs are stable aliases, never Portal UUIDs, and every active
  publication can reproduce their internal skill/version/digest mappings;
- every public metadata field is reproducibly compiled from its documented
  Portal source and cannot be silently replaced by runtime or upstream data;
- assigned skill instructions and capability descriptors load from an activated
  immutable Config Server generation without a live Portal authoring query;
- first-production Agent Cards and runtime projections advertise and activate no
  extensions in any profile; the 1.0 negotiation layer correctly isolates
  unknown optional metadata and returns `ExtensionSupportRequiredError` for
  missing published requirements, while the 0.3 profile rejects extension
  configuration before publication or activation;
- extension policy is expressible only per profile, every profile is
  single-generation, and a projection serving both generations keeps each
  profile's extension policy isolated;
- every later extension is reproducibly compiled from an active registry entry,
  exact versioned URI, approved direction/operations, dependency closure,
  handler/schema digests, metadata limits, and required-eligibility decision;
- executable skill packages are content-addressed, signed, scanned, verified,
  sandboxed, and never transported as Agent Card, Config Server, or live
  hybrid-query source content;
- every served Light-signed card verifies against the correct host,
  environment, and native or external-facade `light-oauth` profile, while
  OAuth-token, wrong-purpose, wrong-environment, and revoked keys are rejected;
- every request resolves an approved agent identity and backend without
  caller-selected destination data;
- first-production release evidence includes governed inbound native and
  external-integration calls plus governed outbound remote-agent calls, with
  end-to-end authorization and audit correlation;
- multiple published agents may share the same raw A2A specification endpoints
  without sharing, overwriting, or ambiguously matching Gateway authorization;
- durable Portal-native tasks survive gateway and `light-agent` restarts without
  a sidecar and can be looked up or canceled only by an authorized caller;
- A2A artifacts use the existing fine-grained authorization system for every
  protected operation, honor the frozen retention profile and legal holds,
  delete managed content with verified evidence, and remain independently
  governed from chat history and Hindsight memory;
- an external developer can deploy a conformant business agent without
  implementing A2A, raw platform authentication, Portal policy lookup,
  Controller registration, audit, metrics, or tracing;
- the Python, Java, and TypeScript/Node.js SDKs pass identical unary, SSE,
  status, cancellation, restart, signed-context, artifact, error, and negative
  conformance cases against the same `light-a2a-backend/v1` contract and
  `light-a2a` build;
- shared-service and sidecar deployments enforce the same policy-decision and
  delegation contracts;
- skills remain descriptive discovery metadata and memory remains runtime-owned
  untrusted context;
- security, filtering, telemetry, reload, scale, and rollback gates pass in a
  deployed multi-node environment; and
- unsupported bindings and capabilities are documented and rejected rather
  than partially emulated.
