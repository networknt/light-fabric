# Personal Development Workflow Orchestration

Status: architecture corrected September 30, 2026 by owner direction. External
API integration belongs in Gateway configuration and workflow definitions.
The GitHub capture/provider implementation track under #427 is withdrawn; the
independent #425 artifact fix is retained. This is a documentation/plan revision,
not a runtime rollback, deployment or live qualification.

The immediate direct API pilot and Ownership sections supersede conflicting
September 29 capture/provider instructions. Broader development/review/workspace
contracts remain for separate Phase 1 review; they do not authorize implementing
external integrations inside the Workflow engine.

This document covers a developer using `codex-personal` and `claude-personal`
with their own subscriptions on a local machine or dedicated VM. The separate
[Enterprise Development Workflow Orchestration](development-workflow-orchestration-enterprise.md)
design covers `codex-enterprise` inside a corporate network with `llm-gateway`.

## Decision And Scope

Use Codex for requirements, design, optional planning, implementation, and fixes.
Use Claude to review the design, plan, and every completed implementation phase.
For implementation scope, after all phases pass, run a Codex final review and
then a Claude final review.
Start each final reviewer fresh once; resume its own conversation for subsequent
fix verification. Workflow definitions invoke registered APIs and generic workspace
tools to publish documents, update comments and commit/push accepted changes.
External-system integration does not introduce subsystem-specific engine tasks.

[User, Application, And Workflow Authorization](../../design/user-application-workflow-authorization.md)
provides the shared authorization foundation. Apply its current contracts
together with [Workflow Invoke And Tool Binding Publication](../light-workflow/workflow-invoke.md)
and the execution-authority rules below. Personal subscriptions do not authorize
Portal/API access. Historical credential-broker enrollment and database-bridge
instructions are not prerequisites for the current native MCP path.

The pilot uses independently started top-level stage workflows connected by
accepted artifact references. A parent coordinator and durable child workflows
come later. The personal pilot does not depend on enterprise sandboxing,
billing ledgers, new Chat interaction cards, a CLI, or full installer conformance.
Phase 1 still requires the storage/recovery qualification for the local and
personal installer variants specified below. Extend the existing Workflow Admin
and Worklist surfaces for stage starts and human decisions; their current generic
controls do not yet implement every development-feature transition.

The first pilot admits one active feature per VM. Shared-runner concurrency and
coordinated routing across multiple VMs are Phase 4 qualifications; the capacity
design below is not a claim that the first pilot supports them.

Keep these business contracts compatible with the enterprise profile: accepted
requirements, optional planning, phase review, finding identity, review coverage,
publication receipts, and completion criteria. Changing execution profile does
not waive a lifecycle gate; enterprise authorization/accounting add profile gates.

## Ownership

| Component | Responsibility |
| --- | --- |
| Developer and workflow author | Define requirements, review steps, data mappings, pagination, retry/error branches and coding-agent inputs |
| Portal UI and backend | Author/register API tools and workflow definitions, configure grants and approvals, and manage existing product/instance/configuration snapshots |
| Gateway / mcp-router | Verify user/application credentials, enforce configured ACLs, route registered APIs/tools and apply the configured upstream credential |
| Identity/OAuth | Supply original-user and demand-driven LONG exchange behavior; reject ineligible renewal |
| `light-workflow` | Execute generic tasks and transitions, persist ordinary workflow context, enforce existing execution/lease/cancellation rules and invoke registered tools/agents |
| Registered external APIs | Own external resource semantics; GitHub REST/GraphQL is reached directly through Gateway configuration |
| `light-agent` and Controller | Admit and route coding-agent work through existing execution mechanisms |
| Personal runner / workspace manager | Host workers, workspaces, generic Git commands, checkpoints and validation tools |
| Existing artifact storage | Retain artifacts for existing consumers; issue input does not create a special capture store, receipt or admission subsystem |

The API integration boundary is generic. Adding GitHub, Bitbucket, ServiceNow,
Salesforce, filesystem or network tools does not add subsystem-specific parsing,
credentials, policy catalogs or task kinds to the Workflow engine. API-specific
schemas and workflow expressions are configuration/definition assets. A real
missing generic capability or defect requires a separate, reviewed change and
an integration-independent regression.

The earlier `light-github-action-provider` architecture is withdrawn. That app
currently has branch/PR and document-publication consumers; its retirement must
migrate or explicitly retire those consumers before removing deployment wiring.
It is not extended with an issue reader. Existing broader development-stage
implementation is historical context, not authorization to expand engine-owned
GitHub orchestration.

### Component Diagram

```mermaid
flowchart LR
    U[Developer] --> P[Portal editor / API catalog / approvals]
    P -->|workflow_start| G[Gateway / mcp-router]
    G --> W[Generic workflow engine]
    W -->|API/tool call with user and app credentials| G
    G -->|ACL checked; configured GitHub credential| H[GitHub REST / GraphQL]
    H -->|API result| G
    G -->|result or error| W
    W --> V[Workflow context and definition expressions]
    V -->|next task / explicit retry or failure branch| W
    W -->|existing coding-agent task; context input| A[Agent / Controller / runner]
    W -->|only when a call needs renewal| I[Identity / LONG exchange]
```

The user token authenticates to Gateway; the configured GitHub token authenticates
the outbound GitHub request. Secrets stay in the established protected Gateway
configuration/secret mechanism, not tool arguments, context, returned JSON or
model input. The selected upstream and credential are fixed by published API/tool
configuration, not by an arbitrary URL supplied with a credential.

Workflow definitions map raw API responses into context and coding-agent input.
Pagination loops, limits and retry/wait/failure decisions are definition behavior
using supported generic capabilities. There is no autonomous GitHub polling or
snapshot/receipt creation in Workflow. REST is the initial proposal; GraphQL can
be selected at the API-contract checkpoint using a configured query and variables.
Neither choice adds a GitHub-specific engine path.

The dispatch scheduler's shared fairness and multi-VM routing are Phase 4 work;
the initial pilot still admits one active feature per VM. Optional future
`feature-delivery` parent/child orchestration, the deferred light-cli TUI and
enterprise `llm-gateway` are outside this personal pilot diagram.

The Agent never advances a workflow stage based on conversational text.
The workflow selects an admitted Agent and sends typed jobs; it does not launch
the native CLI directly. GitHub and native conversations are not workflow state.

`operations.workflow_ops` owns Workflow runtime processes, tasks, feature records,
stage claims, Agent dispatch intents, and effect receipts. Agent owns its separate
`agent_ops.agent_job_t` admission/execution records. Registered Agents pull bounded
jobs and report results through the authenticated job transport. The shared job
identity correlates these records; it does not grant either service access to the
other's database. Portal owns definition authoring and its publication ledger,
while Workflow persists the acknowledged definitions and execution policy it
uses. Portal runtime projections and old started events are not execution authority.

## Invocation And Execution Authority

Development stages use asynchronous `workflow_start` through Gateway MCP. The
Editor currently reaches it through Portal's `StartWorkflow` command, which
acknowledges definition/grant synchronization and supplies
`expectedDefinitionDigest`. A development stage start additionally carries its
typed claim and pinned inputs. Every entry point must reach the same
claim-and-start application transaction; using the Editor must not bypass the
feature ownership check.

```mermaid
flowchart LR
    UI[Workflow Admin or Editor] --> P[Portal start adapter and definition sync]
    P --> G[Gateway MCP and ACLs]
    O[Other authorized async callers] --> G
    G --> S[Workflow workflow_start]
    S --> C[Atomic feature claim and run admission]
    C --> J[Workflow-owned Agent job intent]
    J --> A[Agent admission and execution record]
    A --> R[Controller and runner]
    R --> A
    A --> J
```

An Agent invoking a workflow-backed Tool calls the published Tool on Gateway.
Gateway internally calls synchronous `workflow_invoke`, which enforces its
published binding and returns output, terminal failure, or a bounded timeout.
Clients do not call `workflow_invoke` directly. This path does not replace the
asynchronous development-stage lifecycle or implement durable child workflows.
Native development Agent turns currently require asynchronous claimed stages.

At Gateway ingress, `Authorization` must contain the acting user's bearer.
An optional caller `X-Scope-Token` is independently validated as an application
token and never substitutes for the user. Gateway forwards the user bearer and
supplies its own application token to Workflow. Workflow retains Host/owner,
definition-grant, feature-version, assignment, and execution-authority checks.
Gateway-to-Workflow JWT authentication does not require mTLS; the separately
configured Agent/Controller/runner transports retain their own authentication.

Async runs use Workflow-owned LONG authority for work that outlives the initiating
bearer. New stage admission must establish that run's authority exactly once;
replay must retain the existing binding. Synchronous Invoke uses its bounded
run credential and does not register LONG. Neither route revives the retired
credential broker. Native workers receive no refresh credentials. Published
workspace Agent identities and complete runner bindings remain independently
required for native admission.

Persist feature and stage deadlines and consumed budgets independently of token
expiry, HTTP waits, and credential renewal. Human/capacity waits do not consume a
remediation round, but do not reset the feature deadline. Revocation or unavailable
authority prevents new execution; recovery cannot silently widen authority or
replay uncertain work. Any reauthorization must use the supported explicit
authority flow, retain consumed budgets and evidence, and recheck current policy.

Async start acceptance proves durable admission, not completed execution or stage
acceptance. Status/result/wait operations attach to the recorded instance. A lost
response retains the same start/operation identity for reconciliation; a new
transport ID is not permission to repeat work. A timeout is not proof that an
execution or external effect stopped. Preserve structured failure, retryability,
and `afterEffect` evidence through the UI and fixed actions.

## Workflow Composition

| Top-level workflow | Pilot responsibility | Output |
| --- | --- | --- |
| `feature-intake` | Normalize an existing issue or agreed Codex requirement draft | Feature issue and accepted requirement version |
| `feature-design` | Codex authoring, validation, Claude review/fixes, optional human sign-off, document publication | Accepted design, commit permalink, and PR reference |
| `feature-plan` | Optional detailed plan with the same review/publication loop | Accepted plan and phase manifest |
| `feature-implement` | Implement and review one selected phase | Accepted phase checkpoint and evidence |
| `feature-finalize` | For implementation scope, full-feature reviews and delivery; for design-only scope, fixed verification of the already-reviewed design and its delivery | Evidence for the pinned delivery scope and completion target |

For the pilot, an operator starts the next top-level stage with the preceding
stage's accepted output. Start `feature-implement` once for each declared phase,
in dependency order. Each phase performs its own review/fix loop. No stage
starts an untracked background workflow, and a stage's acceptance does not
declare the whole feature complete.

Persist a `FeatureRun` record across these stage instances. Inputs include the
feature/run and issue identity, stage/phase ID, expected previous accepted
version, applicable artifact digests, workspace/task and repository bases,
Agent/runner bindings, deadlines, and remaining budgets. Only one stage owns
mutable task execution at a time. Starting a successor atomically claims the
expected accepted predecessor/version; duplicate starts return the same instance.
A deliberate replan or reopened phase gets a new stage execution identity and
retains the same feature ledger and consumed budgets.

A stage returns an immutable `StageResult` containing its input/output digests,
acceptance status, findings, validation and publication receipts, and native/file
checkpoints and diffable snapshot references where applicable. The next stage
validates this result instead of trusting an issue comment or copying an entire
conversation.

The pilot uses bounded stage-local iteration and reliable Agent job completion.
Pinned definitions can materialize a finite set of review-round task slots; a new
logical turn uses a distinct durable task/job identity, while a retry keeps its
identity. The current service-call idempotency key includes process/task IDs,
so reusing one completed task ID is not a new remediation turn.

Later, `feature-delivery` can automate these handoffs and use `implement-phase`
children. That extension must add durable child start/wait/result/cancel,
definition/input pinning, parent budget accounting, and restart recovery.
`run.workflow` is modeled but currently rejected by the executor; it is not a
prerequisite for the standalone design pilot.

### Enforced Stage Handoffs

Use the existing `FeatureRun` store in the `light-workflow` operational
database and its claim-and-start transaction behind native MCP. This is required
even while an operator selects and starts each top-level stage. Persist the current feature
version, accepted artifact/phase versions, active stage owner, consumed budgets,
immutable `StageResult` references, and stage-claim receipts.

The start transaction locks the feature record and first checks for an existing
claim to replay. For a new claim it validates the expected accepted predecessor
and all current input versions, and checks that the selected stage is an allowed
successor with no conflicting owner. It atomically records the
claim, workflow instance/process and initial task, pinned inputs/definition, and
new stage owner. `development_store.rs::claim_and_start` already composes feature
checks with invocation/process creation. Ordinary invocation idempotency alone
does not implement the feature-version/ownership check. A claim followed by a
separate unprotected workflow start is insufficient.

The application result must distinguish a newly created claim from a replay.
For new work, admission supplies the configured artifact store, verifies its
readiness, and establishes the run authority before any task can dispatch.
Persist the authority binding with the admitted instance. If external authority
registration is uncertain, reconcile its stable registration identity before
execution; never leave an unauthorized runnable process or mint another binding
on a blind retry. A replay returns the original claim/run and does not renew a
completed run or reacquire a released VM. Readiness checks for new work must not
prevent authenticated retrieval of an already-committed historical receipt.

Give each logical transition a store-owned identity, including the feature,
predecessor version, and selected stage/phase. A unique claim and normalized
request digest make concurrent identical starts return the same instance,
even with different transport request IDs. Changed inputs for an existing claim
conflict; an unclaimed stale predecessor cannot start. Retrying an older completed
claim returns its historical receipt and never launches a new stage. A new
replan/reopen transition must be explicitly recorded by the state machine.

Workflow Admin stage starts use this MCP-backed transaction. Dispatch and fixed
effects require the claim bound to their process; a generic workflow start cannot bypass it. Stage
acceptance verifies durable output/snapshot receipts and current input bindings,
then atomically records the result and releases ownership for the next stage.
Document supersession/replan invalidates affected input bindings in this store.
Unknown worker execution keeps ownership fenced until reconciled. Restart after
a committed start but lost response recovers the recorded instance; rollback
leaves neither a claim nor a runnable process. Feature budgets survive handoffs.

### Feature Operations And Human Decisions

Expose development-feature operations through native Workflow MCP on Gateway,
using shared application handlers rather than a separate REST workflow for the
UI. The following are semantic operations; missing Tool names and wire schemas
must be specified and qualified in the implementation plan before publication.

| Operation | Required binding and result | Current interface boundary |
| --- | --- | --- |
| Start a stage | Pinned definition, claim, predecessor and inputs; return durable instance/claim receipt | `workflow_start` exists; development admission integration remains open |
| Read process/feature and VM holder | Host/owner filtering, current versions, active instance and release state | Native list/get Tools and Workflow Admin views exist |
| Accept a stage | Expected feature version, operation ID, validated output/review/sign-off/publication evidence and allowed successor | Application handler exists; native MCP exposure remains open |
| Replan or reopen | Expected version, operation ID, affected input versions and acceptance invalidation; preserve budgets/fences | Application handler exists; native MCP exposure remains open |
| Publish a fixed intent | Pinned operation and target, approved retained content, stable effect identity; confirmed or unresolved receipt | Finalize-only HTTP dispatcher exists; lifecycle and native MCP integration remain open |
| Record design sign-off | Assignment identity/version, feature/stage and exact design digest, authorized decision and durable evidence | Generic human-task Tools exist; development sign-off producer remains open |
| Finalize design-only delivery | Pinned scope/terminal definition, unchanged approved contents and required delivery receipts | Application handler exists; native MCP exposure remains open |
| Cancel feature and release VM | Expected feature version, store-resolved reservation generation and fencing evidence | Native `workflow_cancel_feature` and UI control exist; full release qualification remains open |

Mutations retain an operation identity and normalized request digest. Identical
retries return the recorded outcome; changed requests conflict. UI clients retain
pending operations after transport uncertainty, refresh authoritative state, and
distinguish durable decision recording from executor continuation. Legacy private
HTTP adapters may delegate to the same handlers during migration, but completing
an HTTP-only diagnostic flow does not qualify the Gateway/UI boundary.

## End-To-End Lifecycle

```mermaid
flowchart TD
    I[Existing issue or Codex requirement dialogue] --> R[Accepted requirements and feature issue]
    R --> D[Codex design, Claude review, and fixes]
    D --> S{Design sign-off}
    S -- disabled or approved --> DP[Publish accepted document revision]
    S -- changes requested --> D
    S -- rejected --> H[Human resolution required]
    DP --> DS{Pinned delivery scope}
    DS -- design-only --> DF[Fixed design finalization and delivery checks]
    DF --> DD[Complete design-only run and fence VM release]
    DS -- implementation --> P{Separate plan needed?}
    P -- yes --> PL[Codex plan, Claude review, fixes, and publication]
    P -- no --> PH[Start implementation phase]
    PL --> PH
    PH --> C[Codex implementation and fixed validation]
    C --> V[Claude phase review]
    V -- findings --> C
    V -- accepted --> N{More phases?}
    N -- yes --> PH
    N -- no --> CF[Codex full-feature review]
    CF -- findings --> CX[Codex fixes and validation]
    CX --> CV[Resume Codex reviewer to verify changes]
    CV -- findings --> CX
    CF -- accepted --> CL[Claude full-feature review]
    CV -- accepted --> CL
    CL -- findings --> FX[Codex fixes and validation]
    FX --> RV[Resume Claude reviewer to verify changes]
    RV -- findings --> FX
    CL -- accepted --> G[Review coverage and publication gate]
    RV -- accepted --> G
    G -- coverage missing or broad change --> OR[Resume required other final reviewer]
    OR -- findings --> OF[Codex fixes and validation]
    OF --> OR
    OR -- accepted --> G
    G -- complete coverage --> COMMIT[Commit exact accepted changes]
    COMMIT --> PUSH[Push GitHub task branches]
    PUSH --> PR[Create PRs and post delivery links]
    PR --> DONE[Verify completion target]
```

The stage-to-stage edges are operator handoffs in the pilot. Review loops run
inside their stage. All loops are bounded. Scope expansion, stale inputs, or
missing review coverage route to the appropriate additional review or replan
before publication, as specified below.

The coverage gate selects every required reviewer of each saved delta, including
the original reviewer again if an escalated review causes a further broad fix.
The other-reviewer edge resumes an existing final conversation; it does not
restart the initial full reviews. Document revisions discovered during phase or
final work follow the revision loop below before that work can resume.

## Requirement Intake

### Immediate Pilot: Direct API Issue Input

The owner supplies an issue URL to a workflow started with asynchronous
`workflow_start`. Reuse the demonstrated demo3-workflow editor/tool setup and
approval path using `call: http` and registered `lightapi://` API targets.
Native binding-less MCP is not required by this flow. Obtain its saved definition and API configuration during the new
plan's first checkpoint; do not infer that a custom capture dispatch path is
required. Workflow-backed tools continue to use `workflow_invoke` with the target
tool's existing setup; the calling native workflow needs no outer Tool binding.

The definition performs these ordinary steps:

1. Validate/derive the registered API arguments from the input URL using supported
   expressions. The Gateway upstream remains fixed by configuration.
2. Call registered GitHub REST operations (or a reviewed GraphQL query) through
   Gateway for the issue and all required comment pages. Authentication, ACLs and
   upstream credentials follow the normal Gateway configuration path.
3. Accumulate returned data in ordinary workflow context. Use supported CEL or
   another already-installed generic transformation facility; jq/JavaScript
   availability must be demonstrated rather than assumed.
4. Use the standard HTTP task `retry:` policy for bounded attempts and delay
   when desired; the engine durably schedules eligible retries via
   `next_attempt_ts`. No retry policy or exhausted retries fails the workflow.
   P01 Wait is for polling loops over successful responses, not a prerequisite of
   failed-call retry. Try/Raise and status-specific error branches are not v1
   requirements. GraphQL HTTP 200 can carry errors; inspect those if selected.
5. Pass the structured context to the existing coding-agent task, then expose the
   design output for owner review. If the existing agent handoff is incomplete,
   report its exact generic integration gap before changing the engine.

Issue bodies/comments and attachment links are input data, not executable
instructions or authority. Specify attachment metadata versus downloaded bytes
at the API checkpoint. Download only through approved routes without forwarding
GitHub credentials to unrelated attachment hosts. Do not claim that one response
contains every comment page or every attachment's contents.

There is no `feature-design-input` capture subsystem, capture/status API family,
READY capture row, immutable creator/task projection, receipt admission, policy
currentness lookup or capture retention job for this flow. The issue URL and API
results are regular context values. Existing artifacts used by other features
are unaffected. A second start reads the API again unless the definition explicitly
chooses another behavior.

Normal local/timer execution does not inspect token expiry or renew a token.
When an operation needs a user token, use the current credential if sufficiently
valid; otherwise exchange through LONG. Gateway enforces ACLs. Definitive renewal
denial fails the operation/workflow; transient handling follows the existing
bounded generic contract. There is no continuous Portal membership check or
cross-database membership fence. Preserve original user identity through existing
LONG behavior without adding a special creator table for GitHub.

The former GitHub Issue Input C0/S00/P01–P05/S01–S06 implementation track under
[issue #427](https://github.com/networknt/light-fabric/issues/427) is withdrawn.
Its successful component and owner-reported deployment tests remain historical
evidence, not approval of that architecture. The replacement implementation plan
is `implementation/light-agent/2026-09-30-DirectApiDesignWorkflowImplementationPlan.md`
in the sibling implementation repository. Old plans are archived and must not
be executed. Broader Phase 1 delivery requires separate revalidation.

Keep [issue #425](https://github.com/networknt/light-fabric/issues/425): it fixed a
pre-existing shared-object deletion defect affecting all artifacts. Its A00
commit `6bd01e0` (merged by `df64d06`) and migration 0020 are independent of the
withdrawn GitHub capture design. Do not revert them.

The #427 correction keeps P01 (`677a3bb`) as the generic durable DSL wait
capability, including migration 0021, nonblocking wake, cancellation and deadline
handling. Remove its capture-profile-specific absolute preparation cap and use
neutral polling fixtures. Current wait support is literal whole-second `PT<n>S`
for 1–600 seconds inclusive, outside fork branches. Definitions can reuse this
capability for explicit retry delays; it is not a GitHub subsystem.

Revert P02 (`5a41529`) capture-specific runtime and tests, while carrying its
independent generic fixes in a small separate change: guarded TLS stream support,
configured client mTLS loading, LONG fixture repair, inspected action-ID forwarding
and the timer's nullable binding read (needed for native async waits). Preserve
working demo3, async start, sync Invoke, LONG and unrelated changes. Keep deployed
0022 history immutable; after quiesced definition/reference/row-count checks,
remove its three unused verified-context/result tables with a new forward
migration (0023 if still free). Nonempty tables block cleanup for review.
No runtime or database rollback is performed by this design revision.

The adjusted P02 revert must retain those generic fixes and the original 0022
SQL/package/staging entries before it is committed. Do not merge a broken revert
and repair it in a later PR. Prove native NULL-bound wait and guarded TLS at the
revert commit as well as on the final cleanup tree; the implementation plan
lists the exact migration assets and safe commit order.

### Broader Intake Lifecycle

Support both entry paths:

1. **Existing issue:** a fixed read action loads the selected issue and relevant
   requirement comments. Preserve source IDs and a content snapshot. Codex
   identifies material gaps; unresolved scope decisions block acceptance.
2. **New requirement:** the developer works with `codex-personal`, which drafts
   the title, body, and `RequirementArtifact`. After agreement, a fixed action
   creates the issue and persists its repository, number, and URL.

The artifact records scope, non-goals, acceptance criteria, affected repositories
(or explicitly unknown ownership), compatibility/migration concerns, source
references, unresolved decisions, and a version/digest. Intake also selects a
completion target: `pr-ready`, `merged`, or `deployed`, with its required checks.

Also pin a delivery scope: **design-only** or **implementation**. This is a
required contract addition, not a claim that the current wire schemas already
expose such a field. Design-only ends after intake, reviewed design, optional
sign-off, document delivery and fixed finalization. Implementation continues through any
plan, every declared phase, and both final reviewers. The completion target
applies within that scope: a design-only `pr-ready` result needs its document PR
and declared checks, and cannot claim implementation delivery. Unsupported
scope/target combinations fail admission. A scope change requires an explicit
versioned replan, not selection of a weaker Finalize definition.

Use one feature issue by default; add linked repository or deferred-work issues
only when separately owned tracking is needed. Issue creation is idempotent by
feature and tracking purpose, not document revision. Existing issue content is
requirement data, not authority to change worker permissions or run arbitrary
commands. Changes after freeze create a new version and an explicit impact/replan
decision; they are not silently folded into the current implementation.

For the new-issue path, reserve a durable intake identity and its authorized issue
creation intent before GitHub I/O. Permit an issue-pending intake record until
the confirmed issue receipt is bound; no design or implementation successor may
start in that state. Retry reconciles the same intent. The current intake seed
requires an existing issue reference, so this pending state needs implementation
rather than a fabricated issue number or untracked pre-intake write.

## Design And Optional Plan

Choose one canonical design path, normally in `light-portal-doc/src/design`
or `light-fabric/docs/src`. Codex authors against the accepted requirements.
Fixed validation runs document/navigation checks; Claude reviews the current
artifact and finding ledger. Codex fixes, validation reruns as needed, and the
same Claude reviewer verifies changes until the closure contract passes.

A per-run `requireDesignSignoff` setting defaults to `false`. When enabled,
Claude's acceptance creates a Worklist decision for the exact design digest
before publication and implementation. Record the human's approval, rejection,
or requested changes. A new design digest requires a new sign-off when the
setting is enabled. Requested changes return to design authoring/review;
rejection enters `HUMAN_RESOLUTION_REQUIRED` until an explicit revise or cancel
decision. Routine review rounds do not add other human gates.

Workflow creates the assignment from the pinned sign-off policy and retained
design candidate. Completion verifies current assignee/role eligibility, claim
and assignment version, current feature/stage, and the exact design digest.
Persist the human-task decision, `DesignSignoff` authority evidence, and the
resulting feature transition atomically, or through a durable idempotent
continuation that keeps acceptance blocked until its evidence is committed.
An identical completion retry returns the same decision; a stale or changed
decision cannot approve a superseding candidate. Generic task completion or a
Tool-binding access approval is not a design sign-off receipt. Implement the
producer as well as the existing acceptance-side validation.

For implementation scope, decide once after design acceptance whether a separate
plan is necessary.
Cross-repository work, migrations, contract changes, and several dependent
phases usually benefit from one. Codex saves it in the implementation repository;
Claude reviews and verifies fixes using the same stage contract. A small feature
can use one phase derived directly from the accepted design. A sufficiently
detailed design can supply several phases without a duplicate plan.

The accepted phase manifest defines scope, owning repositories, dependencies,
validation commands, and exit criteria. Implementation consumes it; it never
starts by generating a second plan.

## Document Publication And Revision

The pilot uses **a separate immutable publication task and branch for each
accepted document revision**, with a PR targeting that repository's `develop`
branch. This works with the existing absent-or-identical-ref push rule and does
not depend on implementing updates to an already-published ref.

For example, a feature may publish design v1 and v2 through distinct tasks/refs
derived from `featureRunId + documentId + revision`. The actual ref uses the
manager's task-branch naming convention; the revision identity is durable and
is not regenerated on retry.

For each accepted design or plan revision:

1. Freeze the accepted files, including any navigation changes, with validation
   and review evidence and optional design sign-off.
2. A fixed action creates a dedicated publication task on a recorded base,
   materializes only that accepted document file set, verifies it, and commits it.
3. Push its unique task ref and create a document PR. Persist the commit SHA,
   ref, PR number/URL, and file permalink. The feature issue links to the commit,
   not just a local path or moving branch.
4. Retain the revision ref/commit for the feature's artifact-retention period.
   Record acceptance and supersession in the ledger/status comment. Publishing
   a document does not automatically merge its PR or close its feature issue.

Here, an accepted revision means an immutable **candidate approval** containing
its exact snapshot, checks, review coverage and any required sign-off. Persist
that approval while the design/plan stage still owns the feature. Fixed document
publication consumes it; only after required publication receipts are confirmed
does stage acceptance emit the final `StageResult` and enable a successor. This
avoids requiring an accepted `StageResult` to publish while simultaneously
requiring publication to accept that same stage. Publication failure retains the
approval and stage owner for reconciliation without another author/reviewer turn.

Delivered document bytes and modes must match the approved file set, including
navigation edits. Keep recovery markers in effect metadata, commit/PR metadata,
or comments; publication steps must not append markers to approved document contents.
Any intentional content transformation must happen before snapshot validation
and review. A GitHub Contents API write alone is not an immutable revision-task,
branch, PR and verification receipt.

Later implementation discoveries use the same path: propose a new document
version, assess its effect on requirements/plans/accepted phases, run the
applicable author/reviewer/sign-off cycle, then publish a **new revision task,
ref, and PR**. No old commit, review record, or permalink is rewritten.
If an old revision already merged, base the replacement on the current recorded
integration revision; if it has not merged, the new snapshot must contain the
complete desired document state. Mark the old unmerged PR as superseded in the
feature ledger; closing it is a fixed action under the run's current authority
and pinned publication policy.

The latest accepted document revision is authoritative for subsequent stages.
Canonical design/plan paths are delivered by their document PRs; implementation
PRs must not carry competing edits to those paths. If implementation edits one,
route the new contents through document acceptance/publication and reconcile
the implementation candidate before final review. Declare the latest document
PRs in the final delivery manifest. A `pr-ready` target may leave them open;
`merged`/`deployed` require their integration as declared by the feature.
Base changes caused by merging documents receive the same impact/validation
treatment as other integration changes.

Snapshot primitives and basic publication dispatch exist. Their integration into
this complete revision-publication path, including candidate approval,
supersession and document PR receipts, remains pilot work. Reusing distinct
manager tasks avoids its existing one-push/one-PR-per-task receipt conflict
without weakening that constraint.

```mermaid
flowchart TD
    W[Phase or final work discovers a document change] --> R[Record REPLAN_REQUIRED and affected input versions]
    R --> D[Re-enter affected design or plan stage]
    D --> A[Author, validate, review, and fix revision]
    A --> S{Required design sign-off}
    S -- changes requested --> A
    S -- rejected --> H[Human resolution required]
    S -- disabled or approved --> P[Publish new revision task, ref, and PR]
    P --> B[Supersede prior revision and reconcile implementation]
    B --> C[Claim affected stage with current inputs and retained budgets]
    C --> W2[Resume work and required review coverage]
```

Record the revision request and reconcile any active worker before handing off
the stage owner. Resume the earliest invalidated phase or final stage selected
by the impact decision; publication of a new document alone does not restore
invalidated implementation acceptance.

## Diffable Candidate Snapshots

The current `checkpoint::capture` in `crates/task-workspace/src/checkpoint.rs`
records file hashes/modes, HEAD, and index/status hashes. It retains no prior
file contents. `workspace.checkpointDigest` detects change; it cannot supply
historical review content or a diff after later edits overwrite those files.

Use manager-owned `CandidateSnapshot` receipts, implemented by
`crates/task-workspace/src/snapshot.rs`. Capture the initial
stage baseline, every candidate submitted for review, and every resulting fix
candidate before another edit can overwrite it. Accepted phase checkpoints
retain their snapshot references. This applies to design/plan fix rounds as
well as implementation and final review; saving accepted phases alone is too
late to verify intermediate fixes.

Under the task's exclusive lock, the manager writes a Git tree from a temporary
private index containing the checkpoint's complete admitted file set. Preserve
exact bytes, executable modes, additions, and deletions, including non-ignored
untracked files; do not apply content-changing Git filters. Keep the current
checkpoint restrictions on symlinks and submodules. An unsupported or out-of-scope
change blocks acceptance instead of silently disappearing from the diff.
Verify the captured bytes against the checkpoint manifest and recheck the
workspace before completing the receipt. Drift produces no accepted snapshot.

Pin each per-repository tree in a unique manager-only local ref, such as
`refs/light-workflow/<featureRunId>/snapshots/<snapshotId>`, without changing HEAD,
the task branch, or the real index. These are local tree snapshots, not feature
commits; fixed push actions never include the private refs. The receipt binds
the feature/stage/task, repository bases, file checkpoint digest, per-repository
tree IDs, and content-manifest digest. Native conversation checkpoints remain
separate. `light-workflow` exports the contents and manifest supplied by a fixed
manager read into the immutable artifact store before a review or stage result
can depend on them, so recovery does not depend on a surviving local Git object
database. The storage and transfer requirements are defined below.

The manager derives each complete delta from the saved before/after trees;
`light-workflow` persists its artifact/digest with both snapshot references.
Retain binary contents and mode/deletion evidence as well as the textual diff;
reviewer tools must be
able to read either saved version. This is a fixed read operation, not native
shell access or a model-authored patch. Workflow impact routing consumes this
evidence. Missing snapshots, failed reconstruction, or digest mismatch block
review/publication rather than falling back to an implementer summary.

Retain all baseline and transition snapshots referenced by `StageResult` or
`ReviewCoverage` for the feature's evidence-retention period. Garbage collection
cannot prune referenced trees/content. Final commit verification compares the
delivered file contents with the accepted snapshot; these snapshots do not move
implementation commit/push ahead of final closure.

### Personal Artifact Storage And Export

The personal pilot uses a **filesystem-backed durable artifact store owned by
`light-workflow`**. `DurableArtifactStore` now supports filesystem and S3 backends;
the separate `workflow.fixedActions.artifactRoot` scratch directory does not
enable the durable store or its publication/recovery contract.

The supported settings are `workflow.artifact.backend: filesystem` and
`workflow.artifact.filesystemRoot: /var/lib/light-workflow/evidence`, alongside
the existing artifact prefix/retention settings. The template accepts these keys;
qualify their effective published values and dedicated persistent volume in
`portal-config-loc/all-in-lt` and the personal stack in `light-portal-install`.
Only the Workflow service mounts this volume; it is separate from runner task
worktrees, fixed-action scratch, and the container's writable layer.

The workspace manager produces a snapshot package and its manifest through a
fixed authenticated runner job. Transfer follows runner → Controller execution
results → Agent result reporting → Workflow's result reconciler; it requires no
inbound manager listener.
`light-workflow` retrieves bounded chunks
bound to the feature/task/snapshot identity, checks lengths and digests, and
publishes the contents through its artifact service. The `snapshot_transfer.rs`
path exists; qualify it through current stage admission and recovery in Phase 1.
A local runner pathname is not a transferable artifact.
The runner and native agents receive neither store credentials nor write access
to the artifact volume. Review/recovery reads use fixed authorized operations
against the stored artifact identity and digest.

The filesystem backend preserves content-addressed, tenant-scoped references
and the existing stage/metadata/promote/verified-binding contract. Use durable
temporary writes and atomic promotion, verify any existing destination on retry,
and recover interrupted promotion before binding a receipt. Clean abandoned
staging files and apply retention without deleting referenced evidence. A
receipt is usable only after the complete package is durably stored and verified.
Feature admission checks that the configured store is available and writable;
a disabled store, full volume, or failed export blocks dependent review/stage
acceptance without replaying the author turn.

Qualification must recreate the Workflow container and recover after removing
the runner's snapshot refs/object cache. The persistent artifact volume and
Workflow database must survive that exercise. Recovery from loss of the volume
itself requires a backup of both evidence and metadata; this personal backend
does not provide automatic replication or VM failover. The enterprise profile
selects its own qualified store while preserving the same evidence contracts.

## Phase Implementation And Review

Run each phase before its dependent successor. **Codex starts a fresh implementer
conversation for every phase.** This is intentional: each `feature-implement`
run consumes the accepted requirements/design/plan, phase manifest, prior phase
results and snapshots, finding ledger, and validation evidence. It does not
depend on the phase-1 conversation surviving into phase 2. Resume the same
implementer and reviewer conversations within that phase's fix loop.

1. Codex implements the declared scope and returns a summary. The manager
   captures the candidate checkpoint/snapshot, and fixed validation operations
   run the phase's required checks. A check that changes deliverable files
   requires a new snapshot and validation bound to that candidate.
2. Claude reviews the manager-derived delta from the previous accepted phase
   snapshot (the recorded implementation baseline for phase 1), with
   access to the accumulated candidate, requirements, design/plan, ledger, and
   actual test evidence.
3. Codex addresses findings and returns a mapping from canonical finding IDs to
   changes and evidence. Rerun affected checks and resume Claude to verify the
   updated candidate.
4. When closure passes, record the accepted checkpoint/snapshot and next-stage
   inputs. Publish the concise round summary and update feature status.

**Claude reviews every completed phase.** Do not wait until all implementation
and Codex final review are finished before involving Claude. Judge each phase
against its declared exit criteria; deliberately scheduled later-phase work is
not a current-phase omission. It cannot excuse a failed current-phase check.

Acceptance requires all declared repository checks and cross-repository contracts.
An environment-skipped required test is unqualified, not passed. Phase acceptance
records a checkpoint; implementation commit/push happens after final closure.
Shared-workspace native tools provide file operations: tests/commands run through
fixed manager/test operations, not unsupported native shell access.

## Final Review And Fix Verification

For implementation scope, create a complete immutable candidate manifest covering
original repository
bases, accumulated changes, accepted requirements/design/plan versions, phase
results, and validation evidence. Final review checks full-feature behavior,
integration, migrations, documentation, and requirements coverage.

1. Start a fresh **Codex final-review conversation**, separate from its
   implementer. Review the whole candidate once. Codex implementation turns fix
   its findings; the same Codex reviewer resumes with the before/after snapshot
   references/digests, manager-derived complete diff, finding IDs, and validation
   evidence until accepted.
2. Start a fresh **Claude final-review conversation** over the resulting whole
   candidate. Codex implementation turns address its findings. Resume that
   Claude reviewer to verify the changed content and close the cited findings.
3. Routine final fixes remain inside `feature-finalize`. Run affected phase and
   integration checks, but do not automatically reopen a phase workflow or
   repeat a separate Claude phase review for a fix Claude is already verifying.
4. Evaluate the review coverage contract below before commit/push.

Each resumed reviewer focuses on the delta and finding closure while retaining
access to the whole current candidate. It may report a new evidenced regression;
“delta review” does not mean ignoring an effect outside the changed lines.

Reinvoke the other final reviewer when a change crosses the recorded scope of
the active fix, affects multiple phases or cross-repository contracts, changes
requirements/design/acceptance criteria, alters security/authorization,
migrations or public APIs, or cannot be shown to preserve earlier coverage.
The workflow owns this routing decision using declared phase/path/contract
mapping, saved snapshot deltas, required-check results, and the active reviewer's
scope assessment. Codex's fix summary alone cannot declare its own change harmless.
Uncertain impact requires the broader review. Reuse the other reviewer's session
when its binding remains valid; a new session is for changed binding, missing
history, explicit recovery, or a deliberately restarted design scope.

The publication gate consumes a `ReviewCoverage` record:

- retain original full-review verdicts and their exact candidate digests;
- record each subsequent candidate transition, its before/after snapshot
  references and full delta artifact/digest, canonical finding closures, impact
  decision, checks, and reviewing role;
- explicitly carry forward coverage only for unchanged/unaffected scope;
- require an unbroken accepted transition chain to the current manifest and no
  open actionable findings without an authorized disposition.

For example, Codex accepts A; Claude fully reviews A and finds F; Codex fixes F
to produce B; Claude resumes and accepts A-to-B with the required tests and a
confirmed local scope. Publication may accept B using that recorded coverage
chain. It must not relabel Codex's original verdict as a review of B. A broad
A-to-B change also requires resumed Codex review before closure.

The chain itself is a proposed higher-level contract. Existing fixed publication
validators must be explicitly integrated and qualified to consume it; never
reuse an old verdict with a forged current digest to bypass an existing
exact-candidate check.

Keep the first final reviews sequential. The current workspace manager takes
an exclusive task lock even for read-only review. Parallel final reviews would
require independently frozen read-only copies or qualified shared-reader access,
plus findings reconciliation; that optimization is outside the pilot.

## Finding Identity And Closure

`light-workflow` owns canonical finding IDs. The reviewer owns the semantic
assessment of whether an observation is new, an existing finding, or a regression
of a previously closed finding. Do not rely on model-generated `DESIGN-003`
labels or automatic fuzzy text matching for identity.

Allocate the review ID before dispatch and bind it to one logical review turn;
retries keep that ID. Canonical finding IDs remain unique for the feature run
across stage instances and reopened phases. The model cannot choose a new review
identity to evade an existing result or finding mapping.

Every reviewer receives the active ledger and a compact closed-finding history
with access to full records. Its schema separates:

- `existingFindingId`: a reference to a canonical ID supplied in that ledger,
  with disposition/evidence such as still-open, verified-resolved, or reopened;
- `newFindings`: turn-local IDs, repository/location, severity, concrete failure,
  evidence, and required resolution;
- proposed `duplicateOf` references with an explanation when two observations
  describe the same failure.

The workflow validates scope, referenced IDs, and required fields. It assigns a
canonical ID once for each admitted new item and persists the mapping keyed by
`featureRunId + reviewId + localFindingId`; replay returns the same mapping.
A validated duplicate relation preserves the old canonical ID and an alias/audit
record rather than deleting history. A cross-feature ID, conflicting disposition,
or ambiguous duplicate claim cannot silently close a finding: return it to the
reviewer for clarification, then use human resolution if disagreement persists.

Codex remediation references canonical IDs and records changed paths/evidence.
It may dispute a finding with evidence but cannot waive or self-close it.
The reviewing role verifies fixes; an authorized human records waivers/deferrals
with reasons and tracking references. A reviewer must explicitly cite a closed
ID to reopen the same failure with new evidence.

Stage closure requires a schema-valid accepted verdict/coverage, no unresolved
actionable finding without an authorized disposition, required checks passing
on the relevant candidate, valid input/policy/base bindings, and any required
sign-off. Keep advisory suggestions separate. Empty native text, transport success,
or malformed structured output never means “no findings.”

Use three remediation rounds after the initial review as the default, plus
configurable turn and wall-clock limits. Count validation-fix and output-repair
turns against those limits. Budgets count dispatched logical work, not the number
of unique findings: rewording or duplicating a finding cannot reset a budget.
Final remediation consumption survives reopened phases, new stage instances, and
reviewer replacement. Deterministic non-progress indicators include repeated
still-open canonical IDs with no accepted resolution; semantic disputes remain
explicit decisions. Exhaustion or unresolved disagreement enters
`HUMAN_RESOLUTION_REQUIRED`, never automatic approval.

## Conversations, Tasks, And Recovery

Codex and Claude for a feature use the same `workspaceId` and task ID, with
different `thread.sessionRef` values. The workflow controls `new`, `resume`,
and `close`. Persist the native `codingThread.checkpoint` independently of
the file `workspace.checkpointDigest`; neither authorizes the other.

Keep role conversations through their stage's fix loop. Close them on stage
completion and verify the close receipt; `closeAfterTurn` alone is not proof of
closure. The next top-level stage, including the next implementation phase,
uses new conversation identities and consumes accepted artifacts and snapshots.
A review has read-only access to the exact expected candidate and saved delta
versions, and must reread changed files on resume.

A task has one workflow-stage owner and one active manager turn. Stale checkpoints
or changed runner/policy/task bindings are rejected. Missing history requires
explicit replacement from accepted artifacts. Uncertain native execution stays
fenced until reconciliation; changing a request ID, stage, or VM is not permission
to replay edits. Completed execution with invalid output needs a bounded
output-repair/recovery step against saved state, not a blind rerun.

## GitHub Actions And Comment Volume

Codex/Claude write content; fixed workflow actions own GitHub effects. Reuse the
workspace manager's `gh` delivery boundary. Pass structured arguments and body
files/stdin; do not interpolate model text into shell source. The current
`run.shell` path disables network and credentials, and shared-workspace workers
do not receive arbitrary host `gh` access.

The following are product-level publication outcomes. Implement API calls and
reconciliation as definition/tool behavior using generic facilities, not new
GitHub operations or provider policies inside Workflow. The former provider
consumer migration must account for each required outcome:

| Operation | Eligible lifecycle point | Required content/identity and receipt |
| --- | --- | --- |
| Create feature issue | Intake, before issue identity is established | Agreed requirement artifact, feature/tracking-purpose identity; issue number and URL |
| Create/update status comment | Each active stage and terminal delivery | Feature-wide identity, monotonic version and stored comment ID; confirmed remote body/version |
| Create round comment | Completed review round in its owning stage | Stage execution, round and immutable summary digest; comment ID/URL |
| Publish document revision | Design/plan candidate approved, before stage handoff | Approved snapshot, revision task, recorded base and unique ref; commit, verified ref, PR and file permalink |
| Publish implementation delivery | Final review coverage and checks complete | Approved multi-repository manifest and per-repository commit/ref/PR receipts |

Pin permitted operations and destinations in the stage policy, then enforce the
operation's lifecycle precondition. Merely relaxing the current Finalize-only
dispatcher check would allow premature publication. Status and round summaries
can describe unaccepted work, but their saved content cannot substitute for
candidate approval. New external writes require current authority; uncertain
intents retain their identity and reconcile before any further write. Finalize
verifies earlier document receipts rather than republishing accepted revisions.

The retiring provider currently implements issue/comment creation and a document
Contents API write. Inventory its consumers before removal. Required status edits
and document delivery must be migrated to registered API calls and generic Git
tools, or explicitly retired with owner agreement. No replacement provider receipt
schema or GitHub-specific engine dispatcher is authorized by this design.

Persist immutable author/reviewer/remediation results before publication. Default
to **one status comment per feature run, edited in place**, plus **one short
comment per completed review round**. The status points to the current stage,
candidate, open finding IDs, latest design/plan PRs, delivery state, and full
artifacts. A round comment summarizes implementation or fixes, validation,
review disposition, and links to full results. Limit its prose, for example to
200 words plus links; large finding sets stay in the artifact store.

This preserves each response in durable artifacts and represents it in the issue
without pasting full responses repeatedly. Update status after completed author,
review, and remediation events; append the compact round comment when the round
is complete. Design acceptance and final delivery also publish explicit links.
The developer can choose a more verbose comment policy per run.

The workflow owns serialized GitHub action intents and their remote IDs:

- status identity: `featureRunId + status`, with monotonically increasing version;
- round identity: `featureRunId + stageExecutionId + round`, bound to its immutable
  summary digest;
- document/delivery identity: feature, artifact/revision, repository, and action.

Edits target the stored comment ID, never “edit my last comment.” Serialize
updates so an older retry cannot overwrite newer status. Record intended body
digest/version and reconcile uncertain results by fetching the known ID or
enumerating matching markers. Markers aid recovery; they do not replace an atomic
local effect claim. A failed comment retries the saved content without another
model turn. Required publications must complete before the corresponding stage
or feature is declared delivered. GitHub deletion/editing cannot change the
internal acceptance ledger.

## Commit, Push, PRs, And Completion

For implementation scope, after final review coverage and required checks pass,
fixed actions:

1. Freeze and verify the accepted manifest, authorized repositories/bases/refs,
   review coverage chain, and required publication authority. Unexpected changes
   return to review; a model's completion text is not publication authority.
2. Run declared pre-publication checks. Any deliverable modification from checks
   or hooks creates a new candidate transition requiring review.
3. Commit exactly the accepted files in each changed repository, including
   intended new files/deletions. Verify the tree and record SHA/parent; no-change
   repositories are explicit. Exclude unrelated changes, caches, and credentials.
4. Push each recorded SHA to its unique GitHub task branch and verify the remote
   ref. The normal integration target is a PR to `develop`; no direct feature
   push to `develop` or `master`.
5. Create the required PRs and verify head SHAs/target branches. Include the
   latest accepted document PRs in the delivery manifest. Update the feature
   issue with per-repository commit, branch, PR, and validation links.
6. Complete the declared CI/integration gates. `pr-ready` requires the intended
   PRs and checks; `merged`/`deployed` also require their authorized integration
   and runtime verification. Opening a PR alone does not close the feature issue.

Record per-repository commit/push/PR/comment receipts. Multi-repository delivery
is not atomic: retain successful effects and reconcile only incomplete actions.
A lost push response requires checking the remote SHA, not rerunning Codex or
creating another commit. Do not overwrite a conflicting remote ref.

For the pilot, a revised published implementation candidate uses a new immutable
publication task/ref/version, just as documents do; record which prior PRs it
supersedes and require current review coverage. Transparent advancement of an
already-published task ref can be added later with reviewed ref-update handling.
It is not required to publish design v2 or to recover a post-publication fix.

For design-only scope, a pinned fixed Finalize stage verifies the accepted design,
required sign-off, unchanged retained document contents, confirmed revision PR
receipts and the selected completion-target checks. It may then record terminal
completion and request the normal generation-fenced VM release without running
implementation or the two full-feature final reviews. This exception is permitted
only by the scope frozen at intake and a design-only history. It cannot terminate
an implementation feature, waive a required reviewer, or publish modified content
under an earlier approval. The existing fixed finalization handler is a reusable
component; scope enforcement and complete delivery evidence remain to integrate.

Record delivery scope, completion target and satisfied checks in the terminal
result and display them in Workflow Admin and the issue summary. A design-only
completion does not claim the designed feature has been implemented. Work that
later expands scope requires a tracked transition/new run with explicit lineage;
never reinterpret the old terminal receipt or reset budgets within a reopened run.

## Concurrent Instances And VM Placement

The Phase 4 target allows independent features to remain active concurrently;
their phases remain sequential. Each feature gets separate worktrees, branch
names, conversations, findings, and budgets even when it changes the same
repositories.

The current personal workspace runner requires `maximumConcurrency: 1`.
With one Codex runner and one Claude runner, Codex can implement feature B while
Claude reviews feature A. **`light-workflow` owns fairness**: its dispatch
scheduler persists ready turn intents and chooses eligible feature runs in
round-robin order per required runner, FIFO within a run, with bounded admission.
Persist queue order and the last-served run so restart cannot favor one feature.

`workflow_agent_job_t` persists Workflow dispatch intents; Agent's `agent_job_t`
persists its downstream admission/execution/results. Neither replaces the
Workflow fairness policy. Controller and runner remain authoritative for actual
capacity.
The workflow releases its dispatch reservation only after a terminal/reconciled
execution receipt; a confirmed pre-execution capacity rejection returns work to
the ready queue. A lost admission response requires reconciliation before release.
Waiting for capacity or human input consumes no remediation round, while the
configured overall deadline still applies. Fairness is among workflow-owned
turns; unrelated interactive traffic still competes for real runner capacity.

A second VM can host another independently enrolled Codex/Claude Agent set and
private workspace store. Both agents on one VM share that VM's store; matching
workspace names across VMs do not share files. Pin runs to their admitted
Agent/runner/store bindings. Moving an uncertain run is explicit recovery after
fencing, never copying live native state. Separate installed control stacks must
avoid assigning the same issue to competing owners.

Use globally distinct task refs, explicit inter-feature dependencies, and
separate test resources (or serialized access to shared resources). Local locks
cannot coordinate remote merges across VMs. Recheck target-base movement and
revalidate/review affected changes before integration. Another VM provides
execution capacity; it does not remove integration or upstream account limits.

The first pilot enforces one active feature per VM through a `light-workflow`
admission reservation retained across stage handoffs and uncertain execution.
Another separately operated VM can host its own pilot and Agent set, but shared
scheduling, coordinated multi-VM routing, and automatic failover are not qualified
by that arrangement.
They remain later milestones, not reasons to postpone the standalone design loop.

### Pilot VM Slot Release

`light-workflow` persists the reservation's VM/runner binding, feature owner,
generation, acquisition time, and release status. Workflow Admin shows the
holder with its issue, stage/state, waiting reason, outstanding execution, and
available resume/cancel actions. `READY_FOR_NEXT_STAGE` and
`HUMAN_RESOLUTION_REQUIRED` retain the slot until the operator resumes or cancels
the feature; they cannot hide an indefinite reservation from the operator.

On feature `COMPLETED`, `CANCELLED`, or `FAILED`, release the slot idempotently
once all admitted turns/fixed effects have terminal or confirmed fencing
receipts. Record the release against the same owner/generation in the feature
store. Completing an individual stage does not release it. A timeout, missing
heartbeat, or terminal stage/process label alone is insufficient proof that the
VM is safe to reuse.

Provide an authorized **Cancel and release VM** action for a stalled feature,
bound to its expected feature version and reservation generation. It blocks new
dispatch and successor claims, cancels queued work, requests cancellation of
active work, and fences outstanding worker/effect generations. Wait for
Controller/runner/manager confirmation that old execution cannot continue, and
reconcile already-issued external effects before completing cancellation and
releasing the slot. Clearing a database owner field alone is not fencing.
If confirmation is unavailable, show
`VM_RELEASE_PENDING` and the unresolved execution; the VM remains unavailable
until an authorized stop/reconciliation supplies the missing evidence.

Persist the action intent and release receipt so a lost response can be retried
without freeing a newer owner's reservation. Late old-generation dispatches,
results, and publication actions cannot reacquire the slot or advance the
cancelled feature. Retain its artifacts, findings, budgets, and publication
receipts; any later explicit reopen must reacquire a slot and stage claim.

## State And Evidence

| State or substate | Required transition evidence |
| --- | --- |
| `ISSUE_PENDING` | Reserved intake identity, agreed requirement artifact and unresolved issue-creation intent; successor admission blocked |
| `REQUIREMENTS_FROZEN` | Accepted requirement version, pinned delivery scope/target and confirmed issue reference |
| `DESIGN_ACTIVE` / `PLAN_ACTIVE` / `PHASE_ACTIVE` | Candidate, validation, review, and bounded remediation receipts |
| `DESIGN_SIGNOFF_PENDING` | Exact-digest human decision when enabled |
| `DOCUMENT_PUBLICATION_PENDING` | Immutable candidate approval and pending revision task/effects; confirmed commit, ref, PR and permalink receipts required to leave this state |
| `READY_FOR_NEXT_STAGE` | Durable accepted `StageResult` and snapshots; successor claim/start transaction validates the current feature/input versions |
| `FINAL_REVIEW` | Primary/independent reviewer substate, finding ledger, and current review coverage chain |
| `COMMIT_PENDING` / `PUSH_PENDING` / `PR_PENDING` | Verified per-repository publication receipts |
| `POST_PUBLICATION_VALIDATION` | Required completion-target checks |
| `REPLAN_REQUIRED` | Revised inputs, affected-scope decision, and explicit acceptance invalidation |
| `HUMAN_RESOLUTION_REQUIRED` | Dispute, exhausted budget, sign-off rejection, or ambiguous recovery decision |
| `REAUTHORIZATION_REQUIRED` | Explicit recovery of current user/run/action authority with policy recheck and preserved budgets/fences |
| `VM_RELEASE_PENDING` | Cancellation/release intent and outstanding stop/fencing receipts; reservation remains held |
| `COMPLETED` / `CANCELLED` / `FAILED` | Terminal feature result with delivery scope/target and separate VM release evidence; a successful stage alone is not `COMPLETED` |

These are conceptual lifecycle states/substates. Some are represented by separate
task/effect records rather than the current `FeatureState` enum. New persistence
and wire mappings require contract changes and gates; this table is not a list of
already-implemented enum values.

The workflow and artifact store retain feature versions/owners and claim receipts,
input versions, snapshot contents/deltas, finding mappings, review coverage,
tests, budgets, sessions, VM reservation/release receipts, queue state, and
external receipts. Restart resumes this state. Post-publication failures retain
the published history and require a tracked remediation/version; they do not
rewrite old acceptance or reset the feature budget.

## Current Implementation Boundary

Source reviewed September 29, 2026 against `light-fabric` `a67d424`,
`portal-view` `0a1efba`, and `light-portal` `81c8d3aee`. This is a source baseline,
not deployed-image, database, effective-configuration or runtime verification.
Historical test/live evidence remains in the
[Phase 1 progress record](development-workflow-orchestration-phase1.md); its
September 14–15 results must not be carried forward as qualification of the
later native MCP integration. No application/runtime gates were rerun for this
design revision.

Workflow source filenames below refer to `apps/light-workflow/src` in
`light-fabric`; other paths and repositories are identified explicitly.

| Capability | Current source support | Remaining integration or correction | Qualification boundary |
| --- | --- | --- | --- |
| Async and sync invocation | `rule_api.rs` implements native `workflow_start`; `invoke_api.rs` implements binding-scoped synchronous Invoke; Portal start synchronizes definitions/grants | Connect development admission to the current native path; preserve the distinct execution/credential contracts | Ordinary Editor/Tool behavior does not qualify development stages |
| Atomic stage claims | `apps/light-workflow/src/development_store.rs` persists features, VM reservations and atomic claims with replay/version checks | Native start passes no artifact store; its development branch also labels new claims `Replay`, bypassing the `Accepted` branch that registers LONG authority | Storage evidence exists historically; native admission regression and runtime gates required |
| Runtime and Agent ownership | `native_jobs.rs`, `job_authorization.rs`, `agent_job.rs` and Agent `domain.rs` implement Workflow-owned intents and Agent polling/reporting into separate stores | Qualify current authority, exact Agent bindings, transport/result recovery and cancellation | Do not restore obsolete cross-database catalog/job access |
| Personal turns and capacity | Shared task inspect/implement/read-only review, session controls and exclusive task locks exist | Integrate bounded stage rounds and current runner policy; shared fairness remains Phase 4 | Prior shared-session evidence does not qualify the whole feature lifecycle |
| Snapshots and artifact storage | `crates/task-workspace/src/snapshot.rs`, `snapshot_transfer.rs` and `artifact_store.rs` implement retained trees, package/delta reads, transfer and filesystem/S3 backends; `workflow.yml` accepts backend/root settings | Carry the store into native admission; qualify effective settings, persistent volumes, failure and recovery on each required stack | Historical local evidence exists; complete current-path and installer gates remain required |
| Feature operations and UI | Acceptance/replan/publication/finalize HTTP handlers exist; native feature/process list/get/cancel and Portal `ProcessInfo.tsx` VM controls exist | Add missing native MCP feature transitions and connect existing UI controls to them | Generic UI or HTTP-only diagnostics do not qualify a full feature run |
| Human sign-off | Generic native human-task claim/complete operations exist; `development_handoff.rs` validates persisted sign-off during acceptance | Produce digest-bound development sign-off evidence and route all three decisions; qualify stale/revoked/duplicate cases | A consumer of `DesignSignoff` is not an implemented producer |
| Existing publication consumers (retirement inventory) | `publication_dispatch.rs` and the retiring provider have existing consumers; `task-workspace/src/delivery.rs` supplies Git primitives | Migrate required effects to definition-driven registered APIs/generic tools or explicitly retire unused consumers; do not extend the provider/engine with GitHub operations | Historical mocked-provider evidence is not qualification of the direct API replacement |
| Completion and VM release | `development_finalize.rs` has a restricted reviewed-design terminal path; acceptance/cancellation use reservation fencing | Pin explicit delivery scope, verify complete delivery receipts, expose finalization through MCP, and qualify interrupted cleanup/late-generation cases | Existing terminal/storage fixtures do not prove full-feature delivery or safe live VM reuse |
| Feature rules and shell boundary | `development-workflow-contract` provides typed review/finding/budget/handoff rules; `run.shell` admission forbids network/credentials | Extend contracts for revised lifecycle semantics and integrate them into fixed actions | Phase 0 evidence covers the recorded rules only; new rules require new contract tests |
| Child workflows | `executor.rs` rejects `run.workflow`; native Invoke rejects nested parent-action admission | Durable child composition and shared multi-VM scheduling remain later work | Synchronous Tool support does not qualify parent/child orchestration |

### Revision Record — September 29, 2026

This document remains the authoritative design. The following stable review IDs
map the source review to the contracts above and the additional Phase 1 gates
below. They identify design-review findings, not runtime finding-ledger IDs.
The separate Phase 1 completion implementation plan is unchanged by this revision
and must be reconciled after design review; none of these items is marked closed
merely because its intended behavior is now documented.

| Review ID | Design disposition | Implementation or evidence follow-up |
| --- | --- | --- |
| DW-R01 | Async MCP stage admission uses one atomic feature/run path and distinguishes creation from replay | Supply artifact access, establish LONG authority exactly once, and test both through native MCP |
| DW-R02 | Fixed effects have operation-specific lifecycle gates; candidate approval precedes publication and stage completion | Add intake issue-pending support, ordered status updates and document task/ref/PR delivery with exact approved bytes |
| DW-R03 | Feature transitions use native MCP; design sign-off requires an authorized digest-bound producer | Expose missing transitions, extend existing UI, and integrate human decisions with acceptance |
| DW-R04 | Workflow and Agent own separate runtime/job stores with authenticated transport | Qualify current Agent admission/result/fencing paths without restoring database bridges |
| DW-R05 | Implemented components, remaining work and qualification are recorded separately | Reuse stores/snapshots/transport; refresh evidence against exact images, configuration and migrations |
| DW-R06 | Current user/application and LONG/Invoke authority contracts replace retired broker prerequisites | Test identity, revocation, expiry/recovery and retained deadlines/budgets on the selected stack |
| DW-R07 | Delivery scope is explicit; design-only finalization cannot satisfy implementation completion | Extend scope contracts, pin them at intake and verify scope-specific terminal receipts and VM release |

## Delivery Plan And Acceptance Gates

### Phase 0: Contracts And Deterministic Review Rules

Implemented and qualified by `bash scripts/run-development-workflow-phase0-gates.sh`.
Wire fixtures and worker JSON Schemas live in `contracts/development-workflow/v1`.
See the [qualification record](development-workflow-orchestration-phase0.md) for
the exact boundary between pure rules and Phase 1 runtime enforcement.

Define `FeatureRun`, stage-claim identity/version rules, `StageResult`,
`CandidateSnapshot`/delta receipts, finding/remediation schemas, review coverage,
budgets, VM reservation/release receipts, document/publication revisions, and
comment intent identities.

Exit gate: fixtures prove that replay preserves canonical finding IDs; reworded
findings citing an existing ID retain identity; invalid/ambiguous duplicate
references cannot close a finding; disputed findings require reviewer/human
disposition; and reopened phases/new instances retain consumed budgets.
Also prove a local final fix can close with resumed review and current coverage,
while a broad change or missing transition cannot publish. Test optional design
sign-off and stale sign-off rejection. Define fixtures for duplicate/stale
handoffs and missing snapshot evidence; Phase 1 must enforce these contracts in
the stores and start path. These gates require no model calls.

### Phase 1: Standalone Stage Execution And Fixed Actions

See the [Phase 1 implementation progress](development-workflow-orchestration-phase1.md)
for verified slices and remaining runtime gates. Phase 1 is not yet complete.

Prerequisite: qualify the shared
[authorization foundation](../../design/user-application-workflow-authorization.md#implementation-order-and-exit-gates)
as used by the current native MCP and LONG paths described above. Include current
user/app identity, revocation and authority recovery; historical broker-only or
ordinary Editor/Tool results do not satisfy development-stage gates.

Integrate and live-qualify the Workflow-owned Agent job transport and
exact workflow Agent bindings. Each separate workflow Agent instance needs its
own service ID in `codingProfile.workspaceBindings[].agents` and the matching
runner-local `RunnerWorkspaceConfig.bindings[].agents`. Publish and install the
same complete binding, including its authorization/membership revisions,
subjects, intents, runner, host, and environment: the runner compares the full
binding for equality. Authorize the separate workflow Agent service identity
explicitly; do not reuse the interactive Agent's identity or weaken that
comparison. Reuse the implemented components and complete their integration:

- the durable `FeatureRun`/`StageResult` store and atomic native MCP stage admission,
  including version/owner checks, dispatch enforcement, and acceptance/replan
  transitions; wire operator starts and feature mutations through Gateway MCP;
- manager-owned candidate trees, private retention refs, immutable content
  artifacts, and fixed delta/version reads tied to checkpoint receipts;
- the personal filesystem artifact backend, persistent volume and published
  settings in `portal-config-loc/all-in-lt` and `light-portal-install`, readiness
  checks, and Workflow-owned export/recovery through fixed manager reads;
- bounded stage-local round slots, schema validation, native/file checkpoints,
  cancellation, and restart recovery;
- fixed tests, issue creation, status/round comments, and immutable document
  publication tasks/PRs;
- one active feature per pilot VM, Workflow Admin holder visibility, terminal
  release, and authorized cancel/release with confirmed execution fencing.

Contract additions in this revision, including delivery scope, candidate approval
and issue-pending intake, need explicit schemas, persistence and deterministic
tests before integration. They are not covered by the historical Phase 0 result.

Do not implement `run.workflow` for this gate. Exit gates:

- Each workflow Agent service ID succeeds with matching published and runner-local
  workspace bindings. Missing IDs on either side, mismatched binding revisions,
  and an unauthorized interactive Agent identity are rejected before a native
  turn starts. Qualify the separate instances using their actual service IDs.
- A top-level design stage performs Codex authoring, fixed validation, Claude
  review, and a resumed fix round using a manager-derived before/after delta.
  After later edits and a restart, the earlier contents still reconstruct
  exactly, including new/deleted/binary files and executable modes. Exercise
  artifact recovery without the local snapshot refs. Missing/corrupt evidence
  blocks review/acceptance; the real index, HEAD, and task branch stay unchanged.
- Each personal stack variant publishes and restores evidence with its configured
  filesystem store after Workflow container recreation and removal of the local
  snapshot refs/object cache. Test disabled/unwritable/full storage, interrupted
  export and promotion, and retry without duplicate artifacts or another author turn.
  The runner receives no artifact-store credentials or writable store mount.
- Transactional store/start tests prove simultaneous identical successor starts
  return one instance; changed inputs conflict; stale/superseded inputs and
  unclaimed dispatch cannot run; commit followed by a lost response replays the
  same instance; rollback leaves no orphan claim or runnable process. Acceptance
  enables the next valid stage and preserves feature budgets and recovery fences.
- Completing a feature frees its VM slot. Cancelling a stalled feature in
  `READY_FOR_NEXT_STAGE` or `HUMAN_RESOLUTION_REQUIRED` frees the slot for another
  feature. An uncertain turn keeps release pending until confirmed fencing;
  afterwards a new feature can claim the slot. A replayed release or late old
  execution cannot free the new reservation or publish old results.
- Restart or lost result delivery repeats neither an uncertain model turn nor
  a GitHub effect. Durable handoffs are enforced, not left to operator discipline.

The September 29 review adds the following boundary cases to these exit gates:

- **DW-R01:** Through Gateway/native MCP, a real development definition admits
  with the configured artifact store and creates one claim/process/authority
  binding. Concurrent starts, restart and lost responses replay that identity.
  Disabled storage or denied authority creates no runnable work; uncertain
  registration reconciles without duplicate authority. Historical completed
  receipts remain readable without reacquiring a VM or starting new work.
- **DW-R02:** Intake creates or adopts an issue; design publishes its approved
  revision before successor admission; status and round effects run at their
  declared lifecycle points. Delayed status retries cannot overwrite newer text.
  Verify all delivered document bytes/modes, commit/ref/PR/permalink receipts and
  restart reconciliation. No marker injection, duplicate issue/comment/PR, or
  Finalize-only workaround may satisfy this gate. Optional plan publication
  adopts the same contract when its stage is added in Phase 3.
- **DW-R03:** An operator drives stage start, sign-off, acceptance/replan and
  design-only finalization through existing UI surfaces backed by Gateway MCP.
  Approve/request-changes/reject each produces the correct durable transition.
  Changed digests, stale assignment/feature versions, revoked eligibility and
  changed duplicate decisions fail closed. A recorded human decision is not
  displayed as completed execution before continuation is confirmed.
- **DW-R04 / DW-R06:** Current user-only and user-plus-app Gateway requests follow
  the identity contract; missing user, invalid supplied app, wrong Host/owner and
  unauthorized Agent bindings are refused. Exercise Agent polling/results and
  revocation across the separate stores. Async LONG renewal/recovery preserves
  deadlines, budgets and fences; synchronous Invoke never becomes a substitute
  for async stage admission or receives a LONG registration.
- **DW-R07:** A scope-pinned design-only run completes only after required document
  delivery/checks and releases its VM only after confirmed fencing. Implementation scope cannot use the
  review-free design terminal path, even with an otherwise valid definition.
  Reject scope substitution on retry and preserve the original terminal receipt.

For **DW-R05**, retain a qualification matrix identifying each gate's repository
revisions, deployed images, effective configuration/definition digests, migration
baseline, fixture/run IDs and evidence. Report component tests, disposable
database gates, mocked-provider tests and live application results separately,
including executed/skipped/failure/error counts. Required skipped or blocked
checks remain unqualified. Update the implementation plan from this design only
after design review; do not infer Phase 1 completion from the component table.

### Phase 2: Personal Design Pilot

Exercise both intake paths and operator handoff to standalone `feature-design`.
Publish accepted design v1, then revise it through the same review/sign-off
contract and publish v2 on a new task/ref/PR with an updated issue link.

Exit gate: both immutable permalinks remain valid, the ledger selects v2, no
existing remote ref is overwritten, and the Phase 1 claim API rejects a new
next-stage start with stale v1 inputs.
Status edits cannot regress on retry; round comments stay concise and link to
all stored author/reviewer/remediation results. A design-only run then completes
through its fixed Finalize contract; an implementation run remains ready for its
next stage. This is the first complete personal design pilot.

### Phase 3: Optional Plan, Phase Work, And Final Delivery

Add the optional plan and repeat standalone implementation by phase. Implement
initial full final reviews plus resumed fix verification, explicit review
escalation, commit/push/PR delivery, and completion checks.

Exit gate: implementation phase 2 cannot start before phase 1 passes and starts
with a fresh Codex conversation reconstructed from accepted artifacts/snapshots;
the no-plan path creates no duplicate plan; a Claude final finding is fixed and
verified without unconditionally restarting both reviews; a broad fix invokes
the other reviewer.
A multi-repository partial push failure reconciles receipts without duplicate
commits/PRs. Latest document revisions and code PRs agree on delivered content.

### Phase 4: Shared Capacity, VMs, And Optional Parent Coordinator

Qualify workflow-owned fairness and two independently enrolled VM agent sets.
Then add `feature-delivery` and durable child orchestration if automated
composition is needed, preserving the standalone contracts.

Exit gate: two features retain isolated results and refs; capacity waiting and
restart preserve fairness; cancellation affects only its feature. Parent/child
start, result, cancellation, and recovery are qualified separately and do not
change review closure or budget semantics.

## Revision Record — September 30, 2026

- Replaced the capture subsystem with direct registered API calls and ordinary
  context data; reused demo3 setup as the required baseline.
- Withdrew the capture-policy configuration framework, receipt UI/admission,
  provider extension and additional creator/membership authorization requirements.
- Planned retirement of light-github-action-provider after consumer migration.
- Kept independent #425 and generic P01 waits; planned removal of P02 capture
  machinery with its generic fixes retained separately and deployed migration
  history preserved. No rollback executed in this revision.
- Owner reports P02 redeployment and light-portal-test `make all` success; this
  remains owner-reported general regression evidence, not source/image provenance
  or proof of the proposed direct API flow.

## References

- [Workflow Invoke And Tool Binding Publication](../light-workflow/workflow-invoke.md)
- [User, Application, And Workflow Authorization](../../design/user-application-workflow-authorization.md)
- [Phase 1 Implementation Progress](development-workflow-orchestration-phase1.md)
- [Enterprise Development Workflow Orchestration](development-workflow-orchestration-enterprise.md)
- [Shared Native Coding Sessions](shared-native-coding-sessions.md)
- [Shared Task Workspaces](shared-task-workspaces.md)
- [Chat And Workflow Integration](shared-task-workspaces-chat-workflow.md)
- [Workflow Coding Thread Lifecycle](../light-agent-worker/workflow-thread-lifecycle.md)
- [Coding Harness Integration](coding-harness-integration.md)
