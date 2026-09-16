# Phase 1 implementation progress

Status: **in progress, not qualified for the personal pilot**. This is a partial
implementation record for issue #392, not a Phase 1 completion announcement.

## Live continuation update (2026-09-15)

Latest continuation: historical UNKNOWN retirement is qualified (see the operator
retirement section). The dedicated Claude runner now uses a private, stable copy
of the already-qualified `2.1.269` binary, verified against the canonical SHA-256
pin; the auto-updated host installation and published CLI contract are unchanged.
Workflow review turns request schema-constrained Claude output and retain the
original raw answer alongside `reviewResult`. The CLI-facing schema uses Draft 7,
which the pinned CLI accepts; the canonical Draft 2020-12 schema and Workflow's
binding/finding-ledger validation remain unchanged. Ordinary conversations are
not forced into the Workflow result schema.

Worker tests: 75 passed, with the host bubblewrap isolation test and no-credential
exact-CLI schema preflight additionally passing. Workflow's native structured
result regression checks raw-answer preservation and rejection of substituted
bindings. Installer tests: 32 passed. The provider-container lost-response gate
again passed with exactly three mocked issue/comment/document writes.

Fresh feature `322ae203-36bd-475b-8cb0-0b1edaca60d4` completed intake and reached
the design fix/re-review cycle. Workflow was recreated during that run using the
installer publication override and a separate local qualification image based on
`2.3.5-dev.20260909.2338`; release tags and existing data volumes were preserved.
The provider now runs separately with the persistent
`all-in-lt_github-publication-state` volume rather than a manually launched process
in Workflow's container layer. Final live publication/release and full installer
storage-failure/cache-loss qualification are still pending.

The owner-authenticated dispatcher is now connected to fixed GitHub issue,
comment and immutable-document publication routes. It selects policy and source
files from the pinned Finalize definition and verified retained candidate, commits
the effect intent before I/O, and retains provider verification with confirmation
in one transaction. Acceptance verifies every required publication slot rather
than trusting a caller-supplied receipt list. The fixed reviewed-design terminal
handoff uses the normal acceptance and generation-fenced VM release transaction.
Gateway publication, finalization and cancellation routes have targeted tests.

The provider's lost-response test commits a remote issue but returns an error,
reopens its SQLite journal, reconciles the receipt and verifies exactly one write.
Opt-in Compose wiring now gives both local and installer stacks a persistent
provider journal and loopback-only endpoint; full installer qualification remains open.

Provider-only container qualification subsequently passed using the built
`light-github-action-provider:phase1-local` image and
`light-portal-install/tests/publication-provider-container-gate.mjs`: mocked
GitHub commits each issue/comment/document effect but returns an error; container
recreation preserves the intent, status observes the receipt, exact replay returns
it and changed input is rejected. Exactly three writes occurred across the three
destinations. This is not application-level FeatureRun or full installer proof.
The installer base-plus-publication Compose override validates successfully.
Current checks pass: Workflow 94 unit tests, runner 53 unit tests, gateway
lifecycle route/schema test, extended provider lost-response test, and the fresh
PostgreSQL stage-store gate (`phase1_review_terminal_1789498244909`).

The earlier qualification incident started a new feature after owner cancellation released the
expired prior run. The new run completed author/snapshot work but stalled in
Claude intake review: the pinned CLI `2.1.269` no longer exists, while the installed
CLI is `2.1.272`. The runner returned UNKNOWN with failed cleanup because no durable
native process identity exists. The run is not replayed and its VM remains held.
Admission now revalidates CLI paths before staging or starting, preserving a
durable NotRequired-cleanup rejection for this failure on future attempts. It does
not retroactively rewrite the uncertain attempt's evidence.

During that incident, the local runner configuration pointed to the installed real `2.1.272` CLI
path, not a mutable symlink. The running runner and uncertain execution are not
restarted or automatically replayed by this configuration edit.

Current qualification is **not complete**: no live dispatcher issue/document/
comment receipt or new terminal VM-release receipt has been obtained, and
installer application-level failure/recreation qualification remains outstanding.
The earlier paragraphs below describe preceding contract/journal-only milestones,
not the current dispatcher wiring status.

The connected PostgreSQL finalization qualification now passes on fresh database
`phase1_review_terminal_1789498725303`. It accepts a reviewed retained design,
starts a pinned fixed Finalize claim, rejects missing and uncertain publication,
reconciles the lost provider response without a second write, verifies required
publication evidence, finalizes the design and releases the VM. Exact finalization
replay succeeds; changed predecessor version and nil operation ID fail. After a
replacement feature reserves the next generation, old finalization replay cannot
release its VM. These are application-core storage fixtures, not live native runs.

This gate found and fixed two production gaps: the claim-derived finalization
snapshot ID exceeded the manager ID bound, and acceptance always required a new
review even for a fixed already-reviewed design. The bounded snapshot ID is now
deterministic. Review-free finalization requires a pinned reviewed-design terminal
definition, reviewed design-only history, unchanged retained repository contents/
trees/modes/checkpoint and the sole design package output. Declared reviewers and
implementation finalization cannot use the exception. Contract regressions and
all 94 Workflow unit tests pass; the musl Workflow/Gateway release builds pass.

Publication persistence now has a PostgreSQL implementation in
`apps/light-workflow/src/publication_journal.rs`, reusing `workflow_task_effect_t`.
Unlike the generic task-effect claim helper, its insert result distinguishes
the first writer (`Dispatch`) from an unresolved retry (`Reconcile`). Confirmed
results are immutable. The caller must authenticate and validate the active
stage, pinned policy and retained content in the same transaction, commit before
dispatch, and retain provider verification evidence before confirmation.
These are storage primitives, **not a connected publication dispatcher**.

The fresh-database stage-store gate passed with the new journal tests: rollback
before dispatch, concurrent first claims (exactly one dispatch), lost-response
reconciliation, changed request rejection, confirmation rollback, immutable
confirmation replay and conflicting/null result rejection. Evidence:
`/tmp/phase1-design-cycle.R5XWTj/phase1_review_terminal_1789495941563.log`;
disposable database `phase1_review_terminal_1789495941563` was retained.
No live publication, feature finalization, or VM-release mutation was performed.

Publication preparation now has a typed contract in
`crates/development-workflow-contract/src/publication.rs` for issue, comment and
document destinations. Trusted policy pins repositories, document branches and
paths; issue/comment capabilities are independent. Plans bind the accepted
candidate and retained content reference. A feature/slot/revision effect identity
does not change when content or destination changes, so altered retry input
conflicts instead of allocating a duplicate write. Prepared effects can dispatch
once; in-flight/unknown effects require remote reconciliation, and confirmation
is immutable. Four new tests and all 20 existing contract tests pass.
This is a **contract-only component**, not a connected dispatcher or live GitHub
qualification. The host must still verify retained bytes and feature ownership,
persist transitions atomically, and validate remote evidence. No publication,
feature finalization, or VM-release mutation was performed in this slice.

**Separate intake and design author/review/fix qualification passed.** Feature
`0bc64472-861a-4323-a8ba-4ac6771b6d6e` completed intake invocation
`01a0a634-f881-7352-b4ae-9fc67ff2a58e` and design invocation
`01a0a636-253c-79e0-bad9-921168964175`. Both were accepted through the authenticated
Gateway stage API, reaching feature version 5, ready for `finalize`.
The first design review rejected one blocking finding; resumed Codex remediation
changed the retained candidate, and resumed Claude independently marked canonical
finding `sha256:3f415d158436b2a622d71eae159a7c5dccf971f487893a458a75e19439414b64`
verified-resolved. The final review accepted and fixed validation had no failures.
Before package digest: `sha256:66d5281d3611de6ff91b7d4a03b1de556f10df2fa94bdea85ff6dbcd834c1b8d`;
accepted package digest: `sha256:7f6667d8e3e3577d296d8a2a3ecead98340cb8b1051e899cbff68a6047455695`.
All 15 Agent jobs succeeded with CONFIRMED native cleanup. No native work remains
running, but VM personal is still reserved to this feature at generation 30 for
finalization. Cancelling completed stage IDs does not cancel a ready successor;
do not describe this reservation as released or reset it through SQL.

Current fixture catalog is `catalog-v1.0.7.json`: intake 1.0.6 and design 1.0.7.
The latter binds fix/re-review `thread.expectedCheckpoint` to the saved author/
reviewer native receipts. The previous fixture omitted those fields and was
rejected before fix dispatch. The standalone validator now checks both bindings.
Gateway snapshot `01a0a633-a55e-7d23-9858-e03fc54cc27b` is active, permissions
unchanged. Detailed local report: `/tmp/phase1-design-cycle.R5XWTj/cycle-report.json`.
The diagnostic driver is not yet a durable daily-test implementation. Publication,
finalization/VM release and installer qualification remain open; Phase 1 is not
complete and no phase-completion GitHub comment has been posted.

Malformed native review delivery is now terminal: after authenticated peer,
subject, cleanup, claim and execution-fence checks, invalid review JSON/binding
or finding-ledger output commits the immutable original report and consumed turn,
sets the Workflow job FAILED with `NATIVE_REVIEW_OUTPUT_INVALID`, and creates no
approval receipt. Exact report retries return acknowledgement after cancellation;
conflicting reports remain rejected. Database failures remain retryable transport
errors. There is no automatic native model replay or arbitrary prose extraction.
The real disposable PostgreSQL gate covers preamble, substituted binding, unknown
finding reference and valid review, including report replay/conflict and identity
guards; it passed, along with all 94 Workflow unit tests. The local Workflow
container binary is updated, with backup in
`/tmp/phase1-design-cycle.R5XWTj/terminal-review-deploy/`. The successful live cycle
above uses this binary; malformed-output failure cases were exercised in the
disposable PostgreSQL gate, not induced in the successful model run. This does
not close Phase 1 or qualify a rebuilt image.

Fresh-workspace admission and provisioning now compare the actual file checkpoint,
not the optional saved review checkpoint. Execution-time checks remain in place.
The workspace suite passed 31 tests (2 ignored); worker tests passed 74 (2 ignored)
on a serial rerun. Both native worker binaries and their Controller admission
digests were updated locally without changing owner, repository or intent grants.

Workflow supplies the exact review binding in verified review material. The
durable Agent bridge consumes and cross-checks that Workflow-only envelope before
strict interactive-message parsing; interactive callers cannot supply it. Agent
tests passed 46, Workflow tests passed 94, and the disposable PostgreSQL gate
passed, including the stored review-material binding assertion. Review parsing
accepts raw JSON or one exact JSON fence, while retaining strict schema and binding
checks and preserving the original answer. General malformed-review report retry
handling was subsequently hardened as described above.

Intake invocation `01a0a5c9-8187-71c1-9c1a-f6ecf5a7f337` completed native authoring,
retained snapshot transfer, fixed checks and accepted Claude review. Stage handoff
then failed because Gateway lacked `workflow_get_feature@call` access mapping.
Normal cancellation released the VM at generation 27; no SQL state reset was used.
Two owner-only mappings (`workflow_get_feature@call`, `workflow_accept_stage@call`)
were added through the versioned Portal command, preserving all existing entries.
Snapshot `01a0a5d4-7762-7713-8b3b-f66c81aa01f6` was activated and Gateway reloaded;
authenticated feature read succeeded. Subsequent cycle results are recorded above.

Intake `01a0a5d5-06f4-7ec1-bd1f-021e106de9ea` subsequently passed authenticated
stage acceptance. Design admission exposed a 70-task fixture exceeding the
runtime's 64-task bound; design 1.0.5 reduces bounded snapshot slots to 62 tasks.
Its author then stopped before a model call because intake and design shared a
native session UUID. Cleanup was confirmed and the VM released at generation 28.

At that point cycle fixtures were 1.0.6: 1.0.3 isolates native request IDs by stage,
1.0.4 supplies stage-specific review criteria and the strict result contract,
and 1.0.6 keys native sessions by stage transition UUID while preserving same-stage
resume. The standalone validator now checks this session isolation and the
64-task bound; both latest definitions pass. Gateway snapshot
`01a0a623-c2c8-7994-b531-bfc28748d5f5` was activated, with permissions unchanged.
The fresh 1.0.6 intake `01a0a625-eb42-7012-b4a7-eaed03158f41` completed authoring
and snapshot transfer, but Claude returned a prose preamble before its review
JSON. Strict parsing rejected the result; the completed Agent job's report
remained PENDING and retried. Normal owner cancellation ended the qualification
and released VM personal at generation 29. This failure motivated the terminal
handling fix above; no answer rewriting or automatic native turn replay was used.
The subsequent successful run is recorded above.
Imported historical files remain unchanged. Local Agent/Workflow/Gateway binary
overrides are container-layer deployments, not rebuilt release images. Publication
and installer qualification remain open; the design cycle subsequently passed.
The historical checkpoints below describe earlier slices, not current gates.

## Earlier live continuation: membership aligned, checkpoint gate pending

The authenticated Gateway lifecycle now exposes `workflow_get_feature` and
`workflow_accept_stage` through the existing owner/user/service-authenticated
Workflow channel. It does not permit direct SQL handoffs or worker-selected
identity headers. Routes validate UUIDs and bounded acceptance bodies; Workflow
still verifies owner, current claims, version, candidate bytes, fixed checks,
review ledger and pinned successors. Gateway tests: 451 passed, 5 ignored;
Workflow unit tests: 94 passed. Both local musl binaries are deployed in their
container layers and must be included in the next image build.

Resumed review can name `reviewArtifacts.previousReviewTask`. Workflow supplies
the saved immutable review receipt and canonical finding IDs only when its
feature, stage, reviewer and before-candidate match. Caller-provided evidence
does not replace that ledger receipt. This path still requires live success.

Separate intake/design definitions and owner-only Tools were imported and
published from `light-portal-event/workflow/20260915-design-cycle/`. The corrected
intake invocation `01a0a59c-bc26-78b2-8b76-5f1e025a435a` reached the native worker
but was rejected before a model call: the registered repository catalog hashes
to `sha256:6869f6609d0563e3412bad1de2633c9e4f226a7ba3d1acff6b02d8874d2152fc`,
while the then-published Agent and runner bindings pinned a different digest.
After operator approval, both Workflow policies were republished, both runner
workspace pins aligned, and both Agents reloaded. Owner, grants and authorization
revisions are unchanged. Runner-generated admission documents match the Controller
pins and both runners report ready. Immutable cycle 1.0.2 and daily snapshot 1.0.6
fixtures are published; the actual Rust membership/typed-definition validator passes.

Fresh intake `01a0a5af-6c60-7050-bdd7-7c11023503e8` passed membership validation
but failed at `checkpoint precondition failed` before any model request.
Execution `01a0a5af-7827-7b73-8d4f-c8b671a6c336` reports cleanup CONFIRMED.
The checkpoint gate is the next unresolved blocker; the full cycle remains
unqualified. No membership or checkpoint checks were bypassed.
See that event directory's README and `validate_design_cycle` example for the
exact next step and retained run evidence. No phase completion comment was posted.

## Candidate handoff evidence and fixed document checks (2026-09-15)

The live manager snapshot export now publishes a complete `CandidateSnapshot`
receipt and a durable JSON `SnapshotRepository` manifest for each repository.
Package, manifests and the accepted job result share one metadata transaction.
Manifest identities are deterministic and domain-separated by Host, process,
snapshot and repository, so an immutable report retry uses the same identities.
No worker gets artifact-store credentials or a writable artifact-store mount.

Live report `light-portal-test/reports/snapshot-qualification/`
`daily-ecbe1d67-2efb-4ca0-b31d-59463934f203.json` passed native transfer and
confirmed cleanup. The database held exactly two VERIFIED / BOUND / RETAINED
artifacts for this one-repository run. The daily driver independently checked
both file hashes/sizes and equality of the manifest with the repository in the
package, including exact feature/stage/task/checkpoint binding. Its 35 adapter
and lifecycle tests pass. This manifest addition is deployed only in the local
Workflow container; the previous binary is saved in
`/tmp/phase1-candidate-deploy.62DmwL/light-workflow.original`.

A subsequent source-only addition evaluates pinned
`document.metadata.developmentWorkflowDocumentChecks` against the verified
package. Each named check has shape
`{"kind":"document-lines","repository":"repo","path":"design.md","requiredLines":["# Design"]}`.
It performs bounded, exact UTF-8 line checks; it does not execute a shell, trust
model validation claims, or claim semantic design correctness. Unknown kinds,
empty or oversized policies and unsafe paths fail closed. Missing files, invalid
UTF-8 and missing lines produce failed checks. The export records each result
as durable `{candidate,check,passed}` bytes and returns a `ValidationReceipt`
plus `validationFailures`; only passing checks enter `passedChecks`. Existing
acceptance still requires its pinned check set and independently verifies the
evidence bytes. Definitions without this metadata gain no fabricated checks.
All 94 Workflow unit tests passed, including the two new fixed-check tests.
This check evaluator still needs deployment and live qualification with the
author/review fixture; the passing transfer report above predates it.

### Concrete remaining integration work

- Integrate the now-passing separate intake/design cycle into durable daily-test
  wiring, preserving exact-run reconciliation and native checkpoint boundaries.
- Complete finalization and owner-authorized release of the successful feature's
  VM reservation; the completed-stage cancel calls do not release ready successors.
- Integrate Workflow-owned fixed GitHub issue/comment/document publication and
  its durable effect identities with the existing task-workspace delivery
  primitives. The helper primitives and contract tests alone are not this live
  path. Use the approved `networknt/light-agent` qualification destination.
- Qualify lost responses/restart without duplicate GitHub effects, and both
  local and installer application evidence recovery after recreation/cache loss
  plus disabled/unwritable/full/interrupted storage. Existing filesystem-helper
  tests alone do not establish installer application qualification.

No new events were imported during this slice. No Configserver data was reset.
Issue #392 remains open; do not post a Phase 1 completion comment yet.

## Durable review dispatch verification (source complete, live gate pending)

Workflow startup now supplies its existing configured artifact store to the task
executor. Only pinned development-review turns require it; the existing enqueue
entrypoint without a store fails closed for reviews. No store credentials, paths,
or writable mounts are passed to Agent or runner.

Review input must include `reviewArtifacts.candidate: {id, digest}` and may include
`reviewArtifacts.before: {id, digest}`. A resumed workspace thread requires `before`.
Both artifacts must be VERIFIED, BOUND, RETAINED, unexpired (or legally held), and
owned by the authenticated Host and current stage process. Workflow reads and
re-hashes actual bytes, independently reconstructs Git trees, and verifies the
feature/stage, repository set, candidate digest, workspace/task identity and exact
`workspace.expectedCheckpointDigest`. Before/after deltas additionally require
matching scope and base commits. Reconstruction runs off the async executor thread.

These checks run before turn reservation, review allocation, or Agent-job insertion.
Verified review material, artifact reference and optional delta are embedded in
the immutable job instruction; caller-supplied `verifiedReviewMaterial` is rejected.
The default `inline` delivery embeds candidate contents. Explicit
`reviewArtifacts.contentDelivery: "checkpoint-workspace"` instead embeds the
verified manifest and UTF-8 Git delta. Repository contents are read through the
existing read-only `task_workspace` session, locked to the exact checkpoint.
Both delivery modes independently verify the complete retained packages before
dispatch. The existing 64-KiB instruction bound remains: oversized material is
rejected, not silently truncated. Unknown delivery modes fail closed.

The real disposable-PostgreSQL gate tests missing storage; wrong Agent/review IDs;
missing, digest-mismatched, unverified, unbound, deleted, expired, wrong-process,
missing-byte and corrupted-byte artifacts; wrong workspace/task/checkpoint;
forged verified material; oversized input; and missing before-evidence for resume.
Failures leave zero jobs and turns. Exact successful replay retains one job,
one turn and one review allocation with verified candidate/delta material.

The 20 contract tests, 92 Workflow library tests, disposable database gate and
all-target compilation pass. Only `light-workflow` needs a rebuild/redeploy for
this slice; no migrations, Config Server resets, or runner rebuilds are needed.
After the operator's rebuild, the live snapshot gate passed in report
`daily-952c61ff-3d4b-4656-9bce-69577b3491f7.json`. Both delivery modes then passed
the disposable PostgreSQL gate, 20 contract tests and 92 Workflow unit tests.
The checkpoint-workspace addition was deployed as a local musl debug binary to
the Workflow container only; it must be included in the next image rebuild.
The original binary is retained in `/tmp/phase1-review-deploy.9hrgHP/`.
The live author/validation/review/resumed-fix fixture, publication, and installer
qualification remain pending; the passing snapshot gate does not prove that cycle.

## Review-cycle wiring continuation (source-only)

Pinned development-review dispatch now binds generated feature/stage/review/job
identities before computing the immutable Agent request digest. Callers may omit
these server-owned values; explicit conflicts fail. The selected reviewer and
Agent must match the definition, workspace intent must be read-only `review`, and
the model instruction carries the exact Workflow-owned binding. Replaying the
binding step leaves the request unchanged. Author turns and manager snapshot reads
do not use this normalization.

Native workspace adapters return their answer in `finalMessage`. Review result
reconciliation now parses that bounded answer as strict `ReviewResult` JSON and
requires equality with the persisted job binding before completing the turn and
applying the finding ledger. Invalid JSON, Markdown wrappers, unknown fields,
oversized answers, and substituted identities are rejected. The normalized ledger
output is separate from the original authenticated execution payload and digest.

`SnapshotPackage::verified_delta` reconstructs both retained trees in a disposable
private Git object database, checks package digests, feature/stage/task/workspace,
repository sets and base commits, and derives a bounded binary/full-index diff.
It does not read or modify the live task checkout, HEAD, index, or retention refs.
The regression compares its result with the existing manager delta before and
after removing the before-snapshot ref and package cache; binary/deleted/new
executable files survive and corrupt or substituted inputs fail closed.

Verification: 92 Workflow library tests, Workflow all-target checking, and all
32 workspace integration tests passed (30 in the default suite, plus both Linux
user-namespace tests explicitly run). These changes are not deployed or
live-qualified yet. Durable artifact loading and verification must be wired into
review dispatch, followed by the pinned author/validation/review/resumed-fix live
fixture. These helpers alone do not complete that gate. GitHub publication and
installer application end-to-end qualification remain open.

## Latest qualification: unfinished native cleanup interruption (2026-09-15)

The live kill-during-cleanup gate passed at 14:16 UTC. Execution
`01a0a56d-7552-7d12-8980-5fde479d23a8` reached the qualification-only barrier
inside native containment cleanup with STARTED / REQUIRED and no terminal result.
The automated test killed only the dedicated Codex runner's main process. While
it was down, Controller retained lease `01a0a56d-7552-7d12-8980-5fed4f64dacf`,
fencing token 1, and VM generation 15 owned by feature
`58f121fd-19a3-4f43-8fbc-816d0f9e0ca6`. After restart, positive containment cleanup
produced UNKNOWN / CONFIRMED for that same lease and fence. Workflow cancellation
released the VM to generation 16 with no owner. No SQL recovery writes, worker
redispatch, fabricated success, or configserver reset were used.

The snapshot report is intentionally FAILED / cleanup CONFIRMED: interruption
prevents a successful snapshot, while the surrounding fault gate verifies safe
recovery. Report: `light-portal-test/reports/snapshot-qualification/`
`daily-7b0c5556-1125-4a3c-9e57-306434927de6.json`. Automated driver:
`light-portal-test/scripts/qualify-native-cleanup-interruption.mjs`.
The local before/interrupted/recovered record is
`/tmp/phase1-native-crash.2Wjh2J/evidence.json` (temporary evidence, not a release artifact).

A preceding barrier-expiry run exposed a distinct late-cleanup gap. Controller
now accepts a separately fenced `RunnerLeaseCleanupCompleted` receipt from the
same authenticated enrollment after reconnect. It updates resource cleanup only;
the original normalized result and outcome remain immutable. Cancellation routes
unconfirmed terminal cleanup to that reconnected enrollment. Native session
cleanup also resolves the persisted execution rather than calling the mock backend.
An acknowledgement alone remains insufficient: local reclamation requires fresh
positive containment proof. Regression tests cover missing evidence, stale fences,
wrong enrollment, replay, and unchanged terminal payloads.

That preceding execution `01a0a53c-cb1c-7f12-be7e-2fc257e5e36c` recovered as
UNKNOWN / CONFIRMED with a separate native containment receipt. The supported
snapshot resume reconciled its timed-out report to FAILED / cleanup CONFIRMED,
without submitting another invocation. It is not claimed as a crash inside the
barrier: its barrier had already expired before the manual kill.

Verification: 51 runner library tests and the expanded Controller repository test
against a disposable real PostgreSQL execution schema passed. Controller and both
native runners use the updated code locally. Normal runner binary SHA-256:
`05305d6a0792b80ea845a2126deb0dd992f1268017a9b54714fa2afdedeb79ae`.
The fault-only binary and `Restart=no` override were removed after qualification;
approved per-unit `Delegate=yes` remains. Controller's local binary deployment is
container-layer qualification and must be included in the next image rebuild.

After restoring normal settings, the automatic-login snapshot smoke passed:
`light-portal-test/reports/snapshot-qualification/`
`daily-394fc262-56e4-42cd-ba9c-cbe55c0cdfff.json`. All three native chunks reported
SUCCEEDED / CONFIRMED. Final checks also passed the runner WebSocket integration,
four execution protocol tests, and 26 snapshot qualification adapter/lifecycle tests.

Phase 1 remains open for the full author/validation/review/fix cycle, workflow-owned
GitHub publication/deduplication, and installer application end-to-end qualification.
Scheduled/hours-long renewal remains explicitly waived. Historical sections below
record earlier states and do not override this latest cleanup qualification.

## Approved historical recovery (2026-09-15 02:00 UTC)

The operator approved administrative recovery of the exact stranded execution
`01a0a2b2-0969-7ce2-bd80-4788c8001c2e`. The incident-specific operator script and
evidence are retained in sibling `controller-rs/scripts/recover-phase1-prejournal-rejection.sql`
and its companion Markdown record. This script is not a migration or automatic
cleanup fallback. Default rollback rehearsal and mismatched-fence rejection
passed before application; committed replay was idempotent.

Local execution audit ID 2 records `ADMIN_PREJOURNAL_RECOVERY`, actor
`operator:steve`, and `workerReported=false`. Outcome remains UNKNOWN with
inspect-required retry classification and no normalized worker result. Resource
release is administratively CONFIRMED; no worker cleanup receipt was fabricated.
The normal Agent cancellation poll reported the job at 02:00:28.241877 UTC;
Workflow recorded CANCELLED and released the personal VM, now generation 3 with
no feature owner. No direct Agent/Workflow/VM/configserver edits were used.

The historical fence no longer blocks a fresh qualification feature. Successful
native turns, snapshot/restart recovery, in-flight cancellation, installer
application qualification, and GitHub publication qualification remain unfinished.

## Runner deployment continuation (2026-09-15 01:57 UTC)

The explicit Workflow-origin allowlist and durable pre-start rejection fixes are
now deployed to both local personal runners. The full runner library suite passed
38 tests, including an expanded admission-document assertion retaining the
interactive origin and adding only the configured Workflow origin. Four broker
socket tests required an unsandboxed rerun; that full rerun passed. The release
build and `git diff --check` passed.

Controller is healthy. Its live registry confirms Codex generation 15 and Claude
generation 14 CONNECTED with binary digest
`sha256:ac4bf700aaf7d7ba3d81152ceeea55691ad03b990d1c189773cf4102b6ffd24c`.
Effective config digests are
`94cc3b9b0f83f3aeeb7eab06d9f0fad0dfe731dad52e3c004786b74eb2c7faa5`
(Codex) and `8f779363c68a51398f043801f2742458103bad084adf4d3fb5af9f710f07295d`
(Claude). The existing single-slot limits and Controller origin list were not
broadened. Previous binaries, runner configs and admission JSON are backed up in
`/tmp/phase1-runner-deploy.RZ0GsD`. These remain local qualification deployments.

The stranded execution is **`01a0a2b2-0969-7ce2-bd80-4788c8001c2e`**;
`01a0a2b2-015e-7fc0-8a66-4150b2d76be3` is its request ID, not execution ID.
Read-only inspection of `operations.execution_ops.execution_attempt_t` still
shows LEASED / REQUIRED. The runner's SQLite journal has no row for that execution;
the original 01:32:51 UTC log records origin rejection before journal admission.
No historical worker cleanup receipt was fabricated, no ownership fence cleared,
and no replacement feature started. Explicit administrative recovery of this
legacy attempt remains unresolved; deploying the preventive fix cannot supply
missing historical evidence.

The user authorized `networknt/light-agent` for issue/comment/document publication
qualification, with a dedicated `qualification/phase1-publication` branch. No
GitHub test writes or implementation commits were made during this continuation.
Native success, snapshot recovery, in-flight cleanup, and installer application
qualification remain open; Phase 1 is not complete.

## Live admission continuation (2026-09-15 UTC)

The signed-in Portal accepted qualification invocation
`01a0a29c-2630-71d0-9830-def7ede52f05`. Its Codex job reached Agent admission,
but no native turn launched: Workflow-only startup had not persisted its accepted
policy snapshot. Cancellation through the same Portal session subsequently
returned `CANCELLED`; the Agent reported `not-dispatched`, the feature became
`cancelled`, and its personal VM generation was released. This proves pre-dispatch
cancellation only, **not** in-flight native cleanup.

Corrections implemented during this continuation:

- Admit only pinned, async, bounded development Agent tasks through the existing
  invocation validator; general/unpinned Agent calls remain rejected.
- Compare typed stage claims so an omitted optional `phaseId` matches the same
  typed claim without weakening immutable raw invocation replay checks.
- Add forward Workflow migration `0010_workflow_action_runtime_privileges`,
  granting the runtime only SELECT/INSERT/UPDATE on its six action tables.
- Persist and verify the accepted Agent policy before enabling Workflow polling.
- Include workspace-only jobs in native dispatch; decode the internal manager
  snapshot envelope separately from strict interactive `ClientMessage` input.
  Missing text/request IDs derive from the typed workspace request; explicit
  conflicting values, unknown fields and missing thread authority are not waived.
- Carry the authenticated invocation owner through the typed job transport and
  Agent-local immutable admission. Forward Agent migration `0005_workflow_job_owner`
  persists this identity; session and workspace policy checks use the user rather
  than a synthetic Workflow Agent subject. No workspace subjects or consent broadened.
- Wire review allocation and successful result recording into native transactions.
  End-to-end review-input construction and bounded review-loop qualification remain
  unfinished; these hooks alone are not a completed design loop.

All 88 Workflow library tests, two job-contract tests, three coding-dispatch tests,
the Agent store inventory test, and Agent/Workflow all-target checking passed.
A real disposable PostgreSQL Agent test additionally proved first-start policy
persistence, exact replay, changed-owner replay rejection, user-owned sessions,
and selection of workspace-only jobs. The installer fresh/repeated-bootstrap gate
passed with 28 migrations across three isolated Host databases. Earlier in this
continuation, expanded stage-store and snapshot-transfer component gates passed.
These are not installer application end-to-end qualification.

Both additive migrations are staged in canonical/local/installer bundles and
applied to the three local operational databases, without wiping configserver.
Local qualification images are now `networknt/light-agent:phase1-owner-20260915`
and `networknt/light-workflow:phase1-owner-20260915`. Workflow is healthy and both
Workflow Agents registered. The release pin remains unchanged. Recreate overlay
and a pre-deploy Workflow binary backup are in `/tmp/phase1-owner-deploy.KqZcoR`;
these temporary paths are not durable release artifacts.

Qualification definition `1.0.2` explicitly pins each native runner/conversation
and closes it after the inspect-only turn. Its digest is
`sha256:9c760f680c03d45bf175846d454cc6e6e7a3e58d9c760d0cef87e7fde40f0207`.
Events are retained in sibling `light-portal-event/workflow/20260914-phase1-native-qualification`.
The four thread-fix events were imported; the active Tool create event was a
projection no-op, corrected by the subsequent `thread-tool-update-events.json`
forward update. Verified definition version is `1.0.2`/aggregate 3 and Tool version
`1.0.2`/aggregate 4. Publication was staged with the existing explicit owner and
rule; snapshot activation and a successful native invocation are still pending.

Phase 1 remains open: native success, snapshot transfer/recovery, in-flight
restart/cancellation cleanup, installer application qualification, and remaining
operator/review/fixed-effect integration must still be demonstrated. GitHub-effect
qualification additionally needs an explicitly selected disposable repository and
branch. Scheduled/hours-long renewal remains waived, not passed or reinstated.

### Subsequent live dispatch and publication findings

The legacy Java publication runtime advanced all three ConfigInstance projections
without their companion events. This continuation's publication reproduced that
defect: endpointRules projection 5 versus stream 4. Three forward reconciliation
events in `config/20260915-gateway-publication-reconciliation` recorded the exact
materialized values, restoring history parity without SQL projection writes.
The existing working-tree Java companion-event fix was built (six publication
tests passed) and deployed to `hybrid-command`; the server and genai-command JAR
backups are in `/tmp/phase1-owner-deploy.KqZcoR`. This Java server update is still
container-layer qualification, not a rebuilt release image.

The ordinary Portal editor then removed only `workflow_authorize@call`, advancing
endpointRules to version 6. Snapshot `01a0a2b0-68f6-799c-bbba-95a7195cca2f` was
captured and activated through Portal. Comparing every property with previous
snapshot `01a0a222-a6dc-7288-9845-f709ead64d8e` found only `tools` changed; access
rules and every other property were identical. Gateway restarted successfully.

Invocation `01a0a2b1-fec7-79b2-91c6-3e7e1dec3d62` then reached Agent admission and
Controller dispatch. Process: `01a0a2b1-ff0a-7562-8a2d-050a4b406c8b`; Agent job:
`01a0a2b1-ff0a-7562-8a2d-0513e8a170f3`; execution request:
`01a0a2b2-015e-7fc0-8a66-4150b2d76be3`. The personal Codex runner rejected the
lease before worker launch: `agent lease origin is not admitted by this runner`.
Its worker configuration pins only the interactive Agent service ID, although
Controller admission already includes the approved Workflow Agent ID.

Runner source now supports bounded explicit `agentWorker.additionalOriginServiceIds`
(default empty), used consistently by worker lease checks and generated admission
documents. Unknown IDs and wildcards remain rejected. Targeted allowlist and
admission-document tests passed. This runner change is **not yet deployed**.

The second invocation was cancelled through Portal. Agent job and scheduling
request are CANCELLED, but the execution attempt remains LEASED with cleanup
REQUIRED and Workflow has not received a cleanup report. The runner rejected the
lease before journaling it and reconnected with a new generation. Therefore do
not treat this cancellation response as VM release or cleanup proof, or bypass
the ownership fence to start another feature. Durable pre-start rejection and
reconciliation of this exact attempt are the next cleanup work.

Pre-start Agent rejection now persists a terminal policy failure before sending
any acknowledgement, with cleanup not required because no inputs were staged or
worker launched. A new regression test passes for lost response, restart, exact
failure replay, unchanged capacity, and no active execution. The preceding full
runner library suite passed 37 tests; the additional rejection test passed
separately. These new runner changes remain source-only and do not retroactively
resolve the old live lease or establish native success.

## Implemented slices

- `task-workspace` retains exact candidate bytes, Git trees and private refs;
  tests cover binary content, deletion, executable mode, later edits, digest
  rejection, lost refs, and interruption between ref creation and record write.
- Workflow supports a filesystem artifact backend, bounded digest-verified reads,
  fsync on writes/promotions, and a write probe for stage admission.
- `development_store` creates owner-scoped pristine feature records and VM
  generations, and atomically stores stage claims together with the existing
  invocation/process/initial-task transaction. Replays bind invocation inputs,
  policy, budget, deadline and authority, not a newly generated transport run ID.
- Migration `0008_development_workflow.sql` adds feature, VM, stage and turn
  records. A deferred process guard rejects unclaimed development starts, including
  legacy event inserts; a task guard rejects new dispatch after ownership fencing.
- Turn reservations persist budget charges and a dispatch intent. An unresolved
  intent returns `Uncertain`, never permission to resend. Completed results replay
  exactly, and conflicting or late results are rejected.
- Trusted completed review turns populate a durable finding ledger; worker output
  cannot allocate its own review identity. Stage acceptance uses pinned definition
  policy and successors, verifies retained artifact bytes and independently
  rebuilds candidate Git trees, and requires completed execution with cleanup
  evidence for remote tasks. Fixed-check and publication evidence binds the exact
  candidate/check or publication receipt.
- Acceptance and limited replan transitions have immutable operation receipts.
  Replan can reopen an accepted stage with its still-current original inputs,
  dropping downstream inputs without deleting history or resetting budgets.
- Controller result reconciliation records matching execution cleanup fences and
  completes bound turn results in the task transaction before acknowledgement.
  Exact committed-result replay recovers acknowledgement loss without republishing.
  Actual stage dispatch does not yet call the turn-binding API.
- Owner-scoped cancellation releases idle features whose claims all have accepted
  results, using the VM owner and generation. Unsettled execution remains
  `vm-release-pending`; recording cleanup does not yet finalize that release.
- A pinned terminal finalize stage can complete the feature and release the VM
  atomically with acceptance. Unresolved fixed effects block acceptance. Replaying
  the old completion cannot release a replacement feature's VM generation.
- Migration `0008` is staged in the canonical operational bundle and both personal
  deployment bundles. Workflow startup requires its tables and migration ledger.
  Both Compose files mount a Workflow-only evidence volume; the Workflow image
  installs Git and owns its evidence directory as its non-root runtime user.
  Local `all-in-lt` now runs the rebuilt qualification image and migration;
  the installer changes remain staged and unqualified.

The HTTP routes inherit the existing invocation identity/grant boundary:

| Method | Path | Payload/result |
| --- | --- | --- |
| POST | `/v1/workflow-invocations/development-stage` | `{claim, invocation}`; ordinary invocation status |
| GET | `/v1/workflow-invocations/development-features/{feature_id}` | Owner-scoped feature and VM holder |
| DELETE | `/v1/workflow-invocations/development-features/{feature_id}` | `{expectedVersion}`; cancelled or release-pending feature |
| POST | `/v1/workflow-invocations/development-features/{feature_id}/accept` | Typed `AcceptStage`; current invocation claims and durable evidence required |
| POST | `/v1/workflow-invocations/development-features/{feature_id}/replan` | Typed `ReplanStage`; owner and historical invocation claims checked |

`AcceptStage.nextStage` is a selector for a handoff or `null` for completion.
Completion requires a finalize stage with pinned
`document.metadata.developmentWorkflowTerminal: true` and an empty
`developmentWorkflowSuccessors` array. No caller-supplied flag overrides this.

Development definitions require `document.metadata.developmentWorkflowStage`
matching the `StageSelector`. `invocation.input.stageClaim` must equal the supplied
claim. The definition digest, definition ID, workspace binding and stage deadline
are checked. The ordinary authenticated invocation endpoint also accepts a
marked development definition with `input.stageClaim`; unclaimed development
starts remain rejected. Initial intake may create pristine feature state in the
same transaction under pinned definition policy, as described below. There is
no public API accepting arbitrary serialized feature state.

## Verification

Run against a **fresh disposable database**:

```bash
export DEVELOPMENT_WORKFLOW_TEST_DATABASE_URL='postgresql://.../fresh_test_database'
bash scripts/run-development-workflow-store-gate.sh
```

The gate refuses an existing `workflow_ops` schema. It creates migration roles,
installs the base and development migrations, then exercises the store as
`operations_workflow_runtime`. It does not silently skip PostgreSQL.

Passed locally on PostgreSQL 17: concurrent identical starts produce one instance;
changed inputs conflict; lost-response replay; owner isolation; rollback without
orphan claims/processes; generic/event bypass rejection; dispatch fencing; turn
intent/result replay without a second charge; stale completion rejection; initial
VM release and late-release protection. The gate also runs all 20 Phase 0 contract
tests and all 83 Workflow library tests.

The expanded PostgreSQL gate also covers review reservation/completion, corrupted
artifact rejection, candidate tree mismatch, pinned successor checks, acceptance
and replan replay/conflicts, budget/history retention, idle release after accepted
history, and runner fence rollback, identity/policy/cleanup checks and exact
acknowledgement recovery. These are database fixtures, not a live Controller run.
The terminal-stage fixture also proves completion releases the slot, uncertain
fixed effects block it, a replacement feature increments its generation, and old
completion replay leaves the replacement reservation intact.
Candidate verification requires Git in the Workflow runtime image; the Dockerfile
now installs it and includes `contracts/` needed by the workspace dependencies.
The new image runs as the non-root `workflow` user and has Git available.

Six generated catalog/mapping/instance events are in sibling repository
`light-portal-event/config/20260914-development-workflow-evidence/events.json`.
Live conflict checks passed; after explicit approval all six were imported and
published to local snapshot `a229fbf8-da56-48be-baf9-37c96c65ecee`. They target the
verified local Workflow instance, not arbitrary installer tenants. The
event-generation skill kept generation separate from the approved live import.

### Local deployment qualification, 2026-09-14

- Configserver and operations were backed up before mutation in private directory
  `/tmp/development-phase1-qualification.RKqqlF/`; no databases were reset.
- Migration `0008` and its checksum ledger were installed in one transaction.
  The Workflow runtime role can access all six new tables.
- Running image: `networknt/light-workflow:phase1-local-20260914` (manifest
  `sha256:7157c4162e79d44e6a6f94ee3321d269ca15ab08a769d19b25da2bbac3ac5f48`).
  The pinned release image/env file was not overwritten. Subsequent normal
  deployments still select the pinned image unless this override is supplied.
- Only Workflow was recreated; existing broker, action and credential overlays
  were retained. `/ready` is ready with Controller connected and the new remote
  snapshot active. The feature route rejects an unauthenticated request (403).
- `examples/development_artifact_gate.rs`, compiled in the same Debian builder,
  exercised the actual backend as UID/GID 999 inside Workflow: stage, recreate,
  promote/retry, recreate, exact binary read, bounded reads, tenant isolation and
  corrupted-object rejection. Isolated test objects were deleted afterward.
- Separate network-disabled containers proved read-only storage is rejected
  during initialization and ENOSPC makes the write probe fail on a 16 KiB tmpfs.
  The live volume was never filled. Six artifact-store unit tests also passed.
- Workflow alone mounts `/var/lib/light-workflow/evidence`; neither Workflow
  Agent has that mount. No artifact credentials were sent to a runner.

This qualifies the local storage slice, not runner → Controller → Workflow
transfer, metadata/promotion crash recovery, a live design loop, installer parity,
or full owner/grant HTTP authorization. Phase 1 remains incomplete.

This proves the store slice, not live worker execution or HTTP authentication
qualification. Runtime-role testing does not stand in for the separate runner
and Workflow container gates.

`cargo check -p light-workflow --all-targets` passed. Strict all-target
`workflow-store` Clippy is blocked by the pre-existing redundant `matches!`
assertion in `tests/postgres_binding.rs:45`; that unrelated test was not edited.
The separate binding/restart PostgreSQL test was not run in this session.

## Remaining Phase 1 work

### Portal invocation entry continuation (2026-09-14)

`portal-view` now has a separate **Invoke Workflow Tool** row action and dialog;
the API-endpoint invocation path is unchanged. It loads the current Gateway's
authorized live catalog, matches the Tool name, shows its published input schema,
and accepts bounded JSON arguments and an optional existing grant UUID (never a
token). It uses ordinary session cookies/CSRF and an in-memory MCP session on a
fixed `/mcp` route. The dev proxy now forwards that path. Submission requires
explicit confirmation and is attempted once; failures leave submission disabled
with an uncertain-outcome warning. Closing does not cancel or release a VM.

Verification: 19 targeted client/dialog/fetch-wrapper tests passed, targeted
ESLint and whitespace checks passed, and the production build succeeded (existing
dependency/chunk warnings remain). Repository-wide TypeScript checking still
reports errors in other files and is not claimed green.

Browser qualification reached the new dialog through the real Tool row. Its
MCP initialization was rejected with HTTP 401; Invoke remained disabled and no
`tools/call` was sent. Gateway audit evidence at 20:55:12 UTC confirms
`POST /mcp`, 401, no authenticated principal. The active `mcpChain` is
`[exception, cors, security, mcp]`, unlike the Portal routes that process the
session through `stateless`.

With operator approval, current local snapshot
`01a0a1b9-5082-71a6-8908-40098f781c3a` adds `stateless` before `security` in
`mcpChain`. Snapshot comparison changed only `chains`; all other chains and
Tool permissions remain unchanged. The Gateway restarted at 21:01:54 UTC,
registered with Controller and retained access-control revision
`809fcf46c220fa12d0647036dd87349fe2b59bf079e1343240bdf655cb66fe63` with default-deny
enabled. Anonymous initialization still returns 401. The signed-in dialog now
returns 403 and remains disabled. The MCP-specific `originAllowlist` is absent
from the active snapshot and its catalog default is `[]`; MCP preflight rejects
browser origins not explicitly listed. The existing WebSocket Origin allowlist
does not cover MCP.

With subsequent explicit operator approval, snapshot
`01a0a1c0-e753-7bea-b5b0-add24ed27ffa` is now current for `portal-bff-loc`.
Comparison against the preceding snapshot confirms only `originAllowlist`
was added, with value `["https://localhost:3000"]`. Gateway restarted at
21:10:00 UTC with the same access-control revision and default-deny enabled;
anonymous initialization remains 401. The signed-in dialog now identifies the
403 as **Workflow caller authorization was denied**, after MCP Origin preflight.
The Gateway action context authenticates dual identity even for initialization;
the specific failing identity condition still needs diagnosis. No additional
caller permission was granted and no native invocation was submitted.
The client exposes only fixed HTTP-error diagnoses, not arbitrary response
bodies; focused client/dialog/fetch tests pass (20 tests), and focused client
ESLint plus diff whitespace checks pass. This is not full Phase 1 qualification.

Follow-up diagnosis: the mounted Gateway `workflow-actions.yml` admits only
`com.networknt.workflow-1.0.0` with `origin: workflow`, bound to its approved
mTLS peer. It has no interactive caller registration. The MCP handler calls
`action_gateway::Runtime::context` even for initialization; this calls strict
`dual_identity::authenticate`, requiring a transport peer and an application
`X-Scope-Token` as well as the user token. Workflow-origin callers additionally
require an action reference. The Portal client sends session cookies/CSRF, and
its Vite `/mcp` proxy supplies neither application credentials nor an upstream
client identity. Session routing and the Origin exception therefore cannot make
this browser path satisfy the service-only contract. The generic live 403 does
not distinguish which individual check failed first.

The missing implementation is an explicit trusted Portal ingress/forwarding
profile with server-held application credentials and authenticated transport,
preserving user/Host, CSRF, Tool ACL and root-grant checks. Do not put app secrets
in the browser, relabel a Workflow caller as interactive, or skip dual identity
for browser requests. Activation requires approval of the new ingress access;
no caller-policy or credential changes were made during this diagnosis.

### Approved local Portal ingress (2026-09-14)

With subsequent operator approval, the local Vite server now has an opt-in
server-only MCP ingress (`portal-view/server/workflowIngress.mjs`). A dedicated
one-day app identity, `com.networknt.portal.workflow-ingress-local-1.0.0`, is
registered as interactive in the local Gateway action policy with exact mTLS
peer fingerprint `5f40f4a0d659586579dc87a29ed4c07d1fd56a2d2886c841b8ff93dbfaa28805`.
The existing Workflow entry and its CA trust are preserved. Gateway restarted
at 21:37:33 UTC with unchanged Tool ACL revision and default-deny. This is an
overlay activation, not another configserver snapshot or release-image change.

The browser receives no app credential. The ingress rejects supplied service
identity/action headers, enforces the exact local Origin/path/method, forwards
the ordinary cookies/CSRF to Gateway over verified TLS with a server-held client
certificate, and leaves user/Host, Tool ACL and root grant admission intact.
It has bounded bodies/time, filtered headers and no automatic retries. HTTP/2
split Cookie fields are supported without accepting duplicate session/CSRF
cookie names. Runtime credentials and private key URLs are denied by Vite.

**Live success:** signed-in initialization and catalog discovery now display the
isolated `phase1_native_binding_intake` published schema in the Portal dialog.
No Tool invocation or grant enrollment was submitted. Negative live checks:
anonymous direct MCP and forged-session ingress 401; foreign Origin, supplied
app identity, duplicate CSRF cookies and credential-file URL 403. Seven new
ingress tests and 20 existing client/dialog/fetch tests pass, as do focused
client ESLint and whitespace checks. This does not qualify native dispatch or
the complete authorization matrix.

The opt-in environment file and credentials are ignored by Git. Preparation,
activation and guarded rollback instructions are in
`portal-config-loc/all-in-lt/workflow-actions/README.md`. The app credential
expires **2026-09-15 21:37:22 UTC**. Production/installer packaging and managed
credential rotation are not implemented by this local Vite plugin. Grant
enrollment and authenticated status/cancel controls remain prerequisites for
the native qualification run; Phase 1 is not complete.

### Portal lifecycle controls continuation (2026-09-14)

The invocation dialog now includes a collapsed **Manage existing workflow run**
panel for explicit status/result reads and confirmed cancellation by exact
`workflowInstanceId`. It discovers the existing Gateway lifecycle tools before
calling them, uses the signed-in MCP ingress, and does not start work or read
status automatically. A cancellation attempt disables further cancellation and
ID edits while preserving status reads for reconciliation. It does not equate
an accepted cancellation with completed cleanup or released VM ownership.

The focused controls/dialog/client suite passes 20 tests. Live read-only
qualification with random nonexistent ID
`78bc469f-459a-48f6-82fa-3f5f5e9d0834` reached Gateway and was rejected:
`Gateway RPC -32001: Access denied: no access control rule defined for workflow_get_status@call`.
No cancellation was sent and no run was created. Activation of scoped rules for
`workflow_get_status@call`, `workflow_get_result@call`, and `workflow_cancel@call`
requires explicit approval; no ACL changes were made in this continuation.

Grant enrollment is still pending. Source inspection confirms the broker
`/workflow/credentials/enroll`, `/complete`, and `/revoke` routes use separate
user/app authentication and the issuer consent/PKCE flow. They are merged into
the ordinary Workflow router, not the existing action mTLS router. Exposing
enrollment requires a deliberate authenticated forwarding path; the local MCP
ingress must not simply forward browser-supplied app credentials or fabricate
an enrolled grant. The actual root binding pins include profile
`workflow-action-v1`, workflow definition ID, and definition/policy/response
policy digests. Native execution remains blocked until enrollment and lifecycle
authorization/transport are qualified.

Root workflow admission also requires an owner-authorized renewable grant;
the UI does not fabricate or auto-enroll one. Grant enrollment and authenticated
status/cancel controls remain work before a native run can be safely qualified.

### Isolated qualification fixture continuation (2026-09-14)

Imported an isolated two-turn, inspect-only intake definition, asynchronous Tool
binding and its parameters in `light-portal-event/workflow/20260914-phase1-native-qualification`.
The initial definition exposed Java/Rust digest disagreement for explicit nulls;
corrective events published version 1.0.1 without optional null fields. Both
digest implementations now agree, and the definition, Tool and parameter
projections are verified. Original event history and three failed legacy
projection records remain intact. This is a fixture correction, not a general
fix to cross-language digest canonicalization.

Added read-only examples `validate_development_qualification` and
`validate_qualification_owner_rule`. The first checks typed workflow parsing,
round budget scopes and pristine intake. The second passed six strict CEL
owner/Host/missing-claim cases against the exact imported qualification rule.
The current generic JWT rule does not test users, so the fixture uses an explicit
owner-and-Host rule rather than relying on an Allowed users field alone.

With operator approval, the owner-only Tool is now published on `portal-bff-loc`
(`com.networknt.portal.gateway-1.0.0`, environment `loc`), publication
`01a0a1ac-fa40-797c-9e8f-c5399a3ce4b4`, current snapshot
`01a0a1ad-28b6-707c-b612-f6ee61cd2767`. Preview preserved all seven existing Tools
and all 873 existing endpoint rules; snapshot comparison changed only `tools`,
`endpointRules` and `ruleBodies`. The local Gateway restarted and registered with
Controller at 20:48:36 UTC, loading access-control revision
`809fcf46c220fa12d0647036dd87349fe2b59bf079e1343240bdf655cb66fe63` with
default-deny still enabled.

An initial activation mistakenly targeted the catalog's Rust AI Gateway in
`dev`, not the running local Gateway. Its previous snapshot
`2e1268cf-2e37-40b3-bcb4-5352aed96a7a` was restored with operator approval.
Staged desired properties remain on that unused instance and must be reviewed
before any future snapshot capture/activation; the event fixture README records
the full rollback and publication identities.

No native job/grant has been created. Configuration activation is not live
execution qualification. Native results, cancellation/release, snapshot transfer
and installer application qualification remain outstanding. Both native runners
were CONNECTED at the start of the earlier fixture continuation.

### Native policy activation continuation (2026-09-14)

With explicit operator approval, imported four native coding-profile authoring
updates and four corrective contract-digest events. The first preview rejected
the unchanged digest after worker pins changed; correction retained the original
import history and the normal guarded append path. All eight imports succeeded.
Artifacts and publication IDs are recorded in
`light-portal-event/genai/20260914-phase1-native-policy/README.md`.

All four policies were published through candidate-checked Portal activation,
with explicit action-time confirmation for the interactive policies. Their full
workspace bindings match authorization revision 3, retaining the same owner and
inspect/implement/review intents while adding the existing Workflow service IDs.
Native binaries, runner bindings/configuration and combined Controller admission
were backed up and replaced together; both Workflow-Agent origins are admitted.
Controller and all four Agents were recreated, preserving interactive image tags.

Live startup exposed a Rustls provider-selection panic in the runner transport
task. Explicit provider initialization at the runner entry point fixed it. The
corrected binary and matching admission pins are deployed. Both native runners
return HTTP 200 `ready: true` from `/readyz`; Controller reports both `CONNECTED`
with the expected binary and effective configuration digests. Exact digests and
publication IDs are in the event README. Transport identity gates passed, but
only exercised empty authenticated polls, not native jobs or grants. These checks
do not establish loaded Agent policy or end-to-end Phase 1 qualification.

Configserver was backed up to `/tmp/phase1-native-policy.jbANDG/configserver-before.dump`.
No database was wiped. The isolated development workflow fixture and native,
snapshot, cancellation and installer application qualification remain pending.

### Atomic snapshot publication continuation (2026-09-14)

Snapshot publication now uses the same database transaction as the accepted
Agent report, without acquiring a second pool connection. A rollback leaves no
committed artifact metadata or accepted result. Content-addressed bytes can remain
unreferenced after rollback; the immutable report retry repeats promotion with
the same artifact ID. Replays check execution/process/task identity, content,
policy and retention/deletion fences, not only artifact ID and digest.

The fresh PostgreSQL runtime-role gate passed with a one-connection publication
pool: rollback leaves zero metadata rows; retry after reopening filesystem storage
and committed replay leave exactly one; changed content/process/execution and
deletion-pending evidence are rejected. All 20 contract tests and 86 Workflow
library tests passed, along with the snapshot assembly/recovery test and
`cargo check --locked -p light-workflow --all-targets`. These are component and
database checks, not the remaining authenticated native/installer exit gates.

Deployed only Workflow as `networknt/light-workflow:phase1-snapshot-tx-20260914`
(manifest list `sha256:cc6513e232db7f5df06957d7a94dc44b54c290a183880be5a9065d817bea7dca`)
using the existing local qualification override. It is healthy; both Workflow
Agent mTLS gates passed again (authorized empty poll 200, wrong Host and missing
scope 403). The release pin and configserver data remain unchanged.

No new policy events were imported and no native runner policy was changed in
this continuation. Additional policy/qualification imports and coordinated
runner admission changes require the requested operator approval. Preflight
confirmed all four native profiles still pin the old worker binary digests.

### Live deployment and cancellation continuation (2026-09-14)

The ordinary invocation cancellation route now enters development feature
cancellation before taking the invocation lock. It preserves the configured
cancellation policy and effect fence, checks ownership, and cannot let an old
stage cancel a newer active claim. Cooperative cancellation fences new dispatch;
it does not manufacture compensation or native cleanup confirmation. VM release
still requires terminal cleanup evidence for the exact generation.

Agent job and development cancellation reconciliation now starts independently
of Workflow's direct runner switch. The local deployment keeps direct runner
execution disabled while enabling this reconciliation for Agent-owned jobs.

Fresh private PostgreSQL custom-format backups of configserver and all three
operational databases are retained under `/tmp/phase1-deploy.9vy24c` (temporary
local evidence, not a durable backup location). Installed and digest-checked
Workflow migrations `0008`/`0009` and Agent migration `0004` in `operations`,
`operations_networknt`, and `operations_taiji`. Existing migrations were retained.
The initial migrator-role attempt failed a REFERENCES privilege check and rolled
back; the unchanged canonical SQL then succeeded under the existing PostgreSQL
table owner. No databases were wiped and configserver data was not changed.

Targeted local Compose overrides now run these qualification images:

| Service | Image | Manifest digest |
| --- | --- | --- |
| Workflow | `networknt/light-workflow:phase1-intake-20260914` | `sha256:ec6fa5ea701088a45f346577a52faa84b1ab2dd978be0dbbd5a42085e50d83d1` |
| Controller | `networknt/controller-rs:phase1-qualification-20260914` | `sha256:db4d3df50a7adf5c048e75a5da0e86ba29a15c666c1effb543edc35eb9290d57` |
| Both Workflow-specific Agents | `networknt/light-agent:phase1-intake-20260914` | `sha256:a8a237a73b1d8b45cc0302c7f186424a98fe6ecb34aab3bb90109b1226970132` |

The release pin remains `2.3.5-dev.20260909.2338`; interactive Agent images and
native runner binaries/configuration remain unchanged. The local override and
targeted recreation script are in `/tmp/phase1-deploy.9vy24c`; a normal deployment
without that override does not select these qualification images.

Verification: the fresh PostgreSQL runtime-role gate passed, including disabled
cancellation, effect-fenced cancellation, cooperative release-pending, ownership,
post-cancel dispatch rejection, terminal cleanup and replacement-generation
protection. All 20 contract tests, 86 Workflow library tests and 9 Workflow binary
tests passed. Workflow and Controller are healthy; both Workflow Agents registered
and both existing native runners reconnected. The new read-only local gate
`bash scripts/run-development-workflow-transport-gate.sh` passed for both Agent
mTLS identities: empty poll 200, foreign Host 403 and missing scope 403. This is
transport qualification, not native dispatch, snapshot or cancellation proof.

Live startup logs also explain the earlier legacy UI smoke: the legacy event
consumer is intentionally disabled (`local_event_source_unavailable`); direct
grant-bound invocation admission remains active.

Next qualification prerequisite: refresh the published native worker policy and
matching runner admission together. The rebuilt Codex worker reports capability
digest `sha256:9793da1ba3b167e5172adbadfd40bced171986b65615b0d0b0056e93689e5f51`,
which differs from the currently pinned policy. Its binaries are staged, not
installed; existing policy hashes were not relaxed. Then publish an isolated
development definition/Tool binding and prove the authenticated native run,
snapshot acceptance, cancellation cleanup and installer application path.
Operator controls and fixed-action integration also remain outstanding.

### Gateway admission and initial intake continuation (2026-09-14)

Browser access and the signed-in Portal session were verified. The existing
Workflow Admin Start form sends the legacy `startWorkflow` event command; it
does not call the renewable-grant invocation boundary. A read-only
`workflow-mcp-smoke` start returned instance
`01a0a15b-21bf-712c-bf0e-73ad08852783`, but no matching runtime process or
quarantine row was found during the check. This is **command acceptance only**,
not a successful execution or native qualification. No development-stage
definitions were present among the ten active definitions shown in the Portal.

Implemented the missing Gateway-compatible lane: `POST /v1/workflow-invocations`
now resolves `input.stageClaim` only for a marked development definition and
uses the same atomic claim path as `/development-stage`. Both routes require
the input claim to match any explicit envelope. The existing mTLS, caller
identity, published binding/dependency checks and renewable-grant validation
remain in place. No Gateway token extraction or authorization bypass is used.

For first intake, the published definition must pin:

```yaml
document:
  metadata:
    developmentWorkflowStage: {kind: intake, phaseId: null}
    developmentWorkflowIntake:
      vmId: <pilot-vm>
      workspaceBinding: {id: <published-binding-id>, digest: "sha256:<digest>"}
      maximumTurns: 12
      maximumRemediationRounds: 3
      maximumDurationSeconds: 3600
```

The published input schema must allow `stageClaim` and `featureIntake`, where
`featureIntake` contains only an `issue` with `repository`, `number`, and its
matching GitHub issue URL. The initial claim uses canonical non-nil UUID feature
and transition IDs, predecessor version 1, intake stage, empty accepted inputs,
the exact published definition/binding and an absolute deadline no later than
the incoming Gateway authority. Workflow derives pristine state and budget
limits from the pinned policy; input cannot supply owners, VM generations,
historical results or larger limits. Issue references are validated syntactically;
this does not establish issue contents, frozen requirements or human acceptance.

Intake creation, VM acquisition, stage claim, invocation and initial task commit
together with the existing run-authority admission transaction. Replays compare
the immutable creation fingerprint even after VM release, without reacquiring
a replacement owner's slot. Gateway attempt-relative deadlines are narrowed to
the immutable claim deadline before persistence, keeping identical retries
stable without widening their authority.

Verification: all 20 contract tests and 86 Workflow library tests passed. The
fresh PostgreSQL runtime-role gate passed initial intake rollback with no orphan
feature/VM/process, concurrent identical intake/claim starts, changed-input and
owner rejection, deadline narrowing, and replay after a replacement VM holder.
These are component/database tests, not authenticated HTTP or native execution.

No application image was deployed or live schema changed in this continuation.
Next: publish an isolated development intake/stage workflow and its exact Tool
binding, deploy the coordinated qualification binaries/migrations, then execute
through the real grant boundary. Operator controls, native execution/recovery,
fixed-action integration and installer application gates remain outstanding.

### Snapshot and cancellation integration continuation (2026-09-14)

Implemented fixed `WorkspaceExecutionSpec.managerSnapshot` reads, admitted only
for a Workflow principal, published runner workspace binding, existing task,
read-only intent and pinned checkpoint. Interactive Chat never creates this
field. The worker does not launch a native model for this operation. Results
carry a bounded chunk and manifest through the existing Controller execution
channel and authenticated Agent job report. Workflow binds them to the original
request, assembles persisted reports, verifies the package digest and Git trees,
then stages/promotes the artifact with Workflow-owned storage access. Public
task results expose transfer progress and the completed artifact reference, not
all accumulated bytes. Snapshot task names must be pinned in definition metadata
`developmentWorkflowSnapshotTasks`; these fixed reads do not charge model turns.

Added service-authenticated Controller endpoint
`POST /internal/execution/requests/{request_id}/cancel`. It fences scheduling,
returns exact lease cancellation messages for delivered work, and reports
confirmation only after all issued attempts have confirmed cleanup. Missing
requests, disconnected runners, and timeouts do not produce confirmation.
Agent terminal reports now include Controller cleanup evidence or a local
never-dispatched receipt. Workflow cancellation revokes native and fixed-action
admission, cancels stage execution, and retains the VM while tasks, effects or
native reports remain unresolved. The background reconciler performs an exact
owner/generation CAS before releasing the slot. `controller-rs` is now part of
the uncommitted implementation set.

Passed in this continuation:

- Fixed manager read: correct scope, exact replay, interactive denial and digest
  mismatch rejection.
- Multi-chunk assembly: interrupted/incomplete and out-of-order transfer,
  identical duplicates, corruption/identity/bounds rejection and verification
  after removal of runner-local storage.
- Disposable PostgreSQL Workflow gate: cancellation holds before a report,
  releases after a verified fence, and late replay cannot release a replacement.
- Disposable PostgreSQL Controller runner gate: another service cannot cancel;
  delivered work remains unconfirmed; a matching confirmed terminal result
  settles cancellation. This is a real database test, not live native execution.
- Workflow library: 83 tests passed before the additional cleanup-proof unit
  test. Workspace protocol/library checks and all-target compilation passed.
- Debian qualification builds succeeded for Workflow, Agent, both native worker
  binaries, native runner and Controller. Workflow/Agent qualification image
  tags are `phase1-completion-20260914`; release pins remain unchanged.

Live qualification is **not complete**. Read-only inspection found zero active
Workflow run grants, zero live invocations and zero active execution attempts.
An authenticated Portal session and scoped run are required; no grant was
fabricated and no authentication check was bypassed. The user was asked to sign
in at `https://localhost:3000`. New binaries/migrations from this continuation
have not replaced live services. Native end-to-end/restart/cleanup and installer
application-level qualification remain pending. Previous installer component
and database evidence below remains separate.

`scripts/run-development-workflow-completion-gates.sh` runs the component and
database checks with explicitly supplied disposable Workflow and Controller
databases. It does not claim to run the authenticated live gates.

### Native dispatch continuation (2026-09-14; not live-qualified)

The working tree now uses Workflow-owned `workflow_agent_job_t` dispatch intent
instead of querying/writing Agent-owned catalog and queue tables through the
Workflow pool. Registered Workflow Agents pull immutable, bounded jobs and
report terminal results over the existing authenticated mTLS job boundary.
Service-mode calls require an exact registered Agent definition UUID and live
run authority; inline calls retain their separate catalog path. Published
`developmentWorkflowTurns[taskName]` metadata pins the turn kind and budget scope.
Reservation and dispatch intent commit together; task identity is the stable job
identity and changed retries are rejected.

Feature cancellation revokes native-job admission atomically. Agent polling
requests cancellation when authorization is revoked. This does **not** establish
confirmed cleanup or release a held VM. The terminal Agent report/fence bridge is
implemented but still needs authenticated transport/replay and live native
qualification, including cancellation races. Do not deploy this as qualified.

New canonical migrations are `workflow-store/0009_workflow_agent_dispatch` and
`agent-store/0004_workflow_job_transport`. Both deployment bundles contain all
26 migrations. Neither new migration was applied to the user's live databases
in this continuation, and no Agent/runner service was rebuilt or restarted.

Verification in this continuation:

- Fresh disposable PostgreSQL development-store gate passed, including native
  intent replay, changed-budget/Agent rejection, Host isolation, one turn charge,
  and cancellation preventing further reservation.
- Workflow library: 83 passed. Agent library: 21 passed, 4 PostgreSQL tests
  ignored (not claimed as live qualification). Typed transport bounds: passed.
- Installer Python tests: 32 passed. Its isolated operational-database runtime
  gate passed fresh bootstrap, repeated bootstrap, three-Host ledger parity,
  runtime-role isolation, and swapped-credential rejection using the new bundle.
- Strict Clippy was blocked by existing `collapsible_if` findings in
  `crates/config-loader/src/lib.rs:568` and `:586`; those files were not changed.

Still outstanding in the four requested areas: actual Codex/Claude dispatch and
result recovery; fixed runner-to-Controller snapshot chunk transfer and durable
export recovery; confirmed cancellation/cleanup receipts and VM-generation CAS
release; installer application/image/artifact restart qualification. The
installer database gate alone does not establish those application guarantees.

### Overall Phase 1 backlog

- Qualify migration/image/settings and recovery in the installer stack variant;
  local `all-in-lt` storage qualification is recorded above.
- Qualify the pinned-policy intake creation path and wire Workflow Admin controls.
- Connect the acceptance/replan routes to Workflow Admin and qualify HTTP auth;
  add human signoff/disposition producers and changed-input supersession.
- Connect turn reservations to real Codex/Claude stage rounds and schema repair;
  qualify the exact published and runner-local workflow-agent identities.
- Transfer snapshot chunks through runner → Controller → Workflow, export them
  with Workflow-owned credentials, and exercise interrupted transfer/recovery.
- Connect cancellation to confirmed Controller execution/effect fencing and
  release after cancelled/failed stages; no timeout-based release. Successful
  terminal acceptance now releases the slot in the database gate.
- Implement fixed validation, issue/comment effects and immutable document
  publication with reconciliation, then run all parent-design live exit gates.

Do not mark Phase 1 complete or deploy this as a qualified pilot based on the
store gate alone.

### Local lifecycle ACL qualification continuation (2026-09-14)

Reconciled three Gateway-generated ConfigInstance streams from version 1 to 2
using exact current values and guarded event import, preserving configserver
data and historical events. Fixtures and audit notes are in
`light-portal-event/config/20260914-gateway-publication-reconciliation/`.
The approved owner-and-Host lifecycle rules then applied through Portal at
endpointRules version 3. Snapshot `01a0a222-a6dc-7288-9845-f709ead64d8e` is current;
only endpointRules differs from predecessor
`01a0a1c0-e753-7bea-b5b0-add24ed27ffa`, which remains available for rollback.

After Gateway restart, access-control remained enabled with default deny and
revision `724263f9fb59a7b4a2c9650c97cc31efe75c0df37c7bbea8b473a3aa6b984562`.
A signed-in Portal status request for a nonexistent qualification UUID passed
Gateway authorization. It failed downstream with a non-JSON lifecycle response.
The configured ordinary HTTP Workflow listener returns an empty HTTP 403 for
this route; `enforce_action_receiver` requires authenticated dual identity and
the mTLS peer when action authorization is enabled. The Gateway lifecycle client
still uses the ordinary invocation URL/client. This is not a successful
Workflow lifecycle qualification, and no native run or cancellation was sent.

The publication versioning source patch adds atomic ConfigInstance companion
events plus a projection/history consistency guard. Six command tests and ten
existing publication persistence tests pass. These counts do not establish
live publication, replay, or deployment qualification; the source patch is not
yet deployed. Phase 1 remains incomplete.

### Gateway-to-Workflow mTLS continuation (2026-09-14)

The downstream transport blocker above is resolved locally. When the A2
`workflow-actions.yml` profile is present, MCP Workflow dispatch uses its
`authorization.control` endpoint, client identity, CA, and service token. This
fixed HTTPS transport takes precedence over the legacy
`mcp-router.workflow.invocationUrl` and scope-token environment settings. With no
A2 profile the legacy transport is unchanged. Invalid A2 transport configuration
rejects startup/reload; it does not fall back to the ordinary HTTP listener.

Only Workflow start, wait, ambiguous-start recovery, status, result, and
cancellation use this dedicated client. General Tool HTTP clients do not receive
the Workflow identity. The client trusts only the supplied CA, verifies hostname
and peer certificates, disables proxy use, redirects, and automatic retries,
and preserves the inbound user Authorization alongside the A2 app token.

Verification: `cargo test -p light-pingora --lib --locked --quiet` passed 450
tests, with five ignored. The two added tests cover unsafe endpoint and missing
credential rejection, a real mutual-TLS handshake, user/app header preservation,
redirect refusal, and plain-HTTP refusal. The musl release Gateway build passed.

The binary was installed into the existing local `light-gateway` container and
restarted at 23:33 UTC. SHA-256:
`656c934ca6ebf0fa302c32a3694841218b111addae123bded68bd674f81eff41`.
The release image pin remains `2.3.5-dev.20260909.2338`; this container-layer
qualification replacement is lost when the container is recreated. The image
must be rebuilt through the normal release workflow before durable deployment.
The old binary is retained at
`/tmp/phase1-gateway-mtls.o7RopZ/light-gateway.previous`, SHA-256
`98b83d592bff270e48bf9e51c688eead06722f2a219830cd06a1b2d580111e2a`.
Local rollback: stop this container, copy that saved binary to
`light-gateway:/app/light-gateway`, and start it again. No configuration, database,
credential, or policy changes are needed for this binary rollback.

Signed-in Portal status and result reads for nonexistent UUID
`78bc469f-459a-48f6-82fa-3f5f5e9d0834` both passed Gateway authorization and
Workflow's authenticated receiver, returning the expected JSON HTTP 404
`workflow invocation is unavailable`. Gateway default-deny and ACL revision
`724263f9fb59a7b4a2c9650c97cc31efe75c0df37c7bbea8b473a3aa6b984562` were unchanged.
No native invocation or cancellation was submitted. This establishes the local
read transport, not grant enrollment, native execution, cleanup, or installer
qualification. Phase 1 remains incomplete.

### Enrollment UI and authenticated broker routing (2026-09-14)

Added the `workflow_authorize` MCP lifecycle action and Portal's explicit
"Authorize workflow" panel. The action requires its own Gateway ACL and a
second authorization check on the selected published Workflow Tool. Gateway
derives the immutable `workflow-action-v1` binding from that Tool; caller inputs
contain only Tool name, scope and a 60–3600 second duration. It rejects nested
action/delegation enrollment and cleartext dispatch. The issuer remains the
authority for scope ceilings and final user consent. The response is bounded to
16 KiB and exposes an HTTPS consent URL and references, not reusable credentials.

The Workflow mTLS listener now mounts enroll/complete/revoke routes. Those routes
check the app/TLS-peer pair as well as the existing allowed-caller and user-token
checks. The public PKCE callback remains on its separate listener. Portal never
accepts passwords or tokens, never automatically opens consent, and fences an
uncertain enrollment submission. The selected Tool binding cannot be supplied
by the browser. No existing endpoint ACL was modified.

Verification: 25 focused Portal tests and ESLint passed; all 86 Workflow library
tests passed. MCP's full pre-new-contract-test suite passed 450 tests with five
ignored; two additional enrollment contract tests passed afterward. Initial
socket-suite failures were sandbox socket denials; the full suites passed with
local socket access. Gateway and Workflow musl release builds passed.

Local container binaries were replaced and restarted at 23:42 UTC, preserving
the existing configuration and release image references. These replacements are
container-layer qualification only and do not survive container recreation.
New Gateway SHA-256:
`830476a8c62e3d67bb3224023b25f8d69308e7d4d2046c648f0e9e6c4d456aef`.
New Workflow SHA-256:
`ae9f59e4f45ea9e5e6c7cdb27af0594ac55109143f66a79641eb6b3cb8bbd208`.
Previous binaries are in `/tmp/phase1-enrollment.p7tKSl/` as
`light-gateway.previous` and `light-workflow.previous`. Roll back by stopping the
corresponding container, copying its saved binary to `/app/light-gateway` or
`/app/light-workflow`, and starting it again; do not restore database snapshots.

After restart, the signed-in status check still returned the expected Workflow
JSON 404 for the nonexistent qualification run. An enrollment request over
valid client TLS but without app/user credentials returned 403. ACL revision
`724263f9fb59a7b4a2c9650c97cc31efe75c0df37c7bbea8b473a3aa6b984562` and default deny
were unchanged. `workflow_authorize@call` is deliberately absent from the
version-3 endpointRules map pending explicit activation approval.

The Portal form is prepared with proposed `portal.r portal.w` scopes and a
900-second duration, but its consent checkbox is unchecked. No enrollment,
grant, native invocation or cancellation was submitted. Next: approve the same
owner/Host ACL for this enrollment action, begin issuer consent for the isolated
qualification Tool, have the owner complete issuer authentication/consent, then
qualify the native execution and cleanup. Phase 1 remains incomplete.

### Superseding scope-consent decision and rollback (2026-09-14)

The user rejected additional workflow-specific consent: only the existing Portal
`portal.r` / `portal.w` scope consent is required. The enrollment UI and generated
`workflow_authorize` MCP tool have been removed from source. The running local
Gateway was restored to the pre-enrollment binary, SHA-256
`656c934ca6ebf0fa302c32a3694841218b111addae123bded68bd674f81eff41`, preserving
the mTLS status/result/cancel transport. Snapshot
`01a0a222-a6dc-7288-9845-f709ead64d8e` is current again; only endpointRules differed
from the deactivated enrollment snapshot. The editable instance property still
contains the enrollment assignment; reconcile it before capturing another snapshot.

The earlier enrollment request was submitted once, but no user approval or native
invocation was submitted by the agent. Do not reuse its expired browser link.
The older issuer/broker interactive contract has not yet been replaced with
backend reuse of existing scope authorization. Removing the Portal prompt alone
does not complete that integration. A1's scheduled and hours-long renewal tests
are waived by the user, not passed; Phase 1 remains incomplete.

Verification: Portal dialog/client/run-controls tests 20 passed; MCP module tests
175 passed and 3 ignored; lifecycle catalog regression 1 passed; focused ESLint
and diff whitespace checks passed. Live Portal no longer shows the enrollment
prompt; Gateway is running with exit code zero after restoration. No database
wipe, commit or push was performed.

### Backend acquisition correction (2026-09-14)

This supersedes the remaining acquisition gap in the preceding rollback note.
Root HTTPS Gateway invocation without a supplied grant now calls Workflow's
enrollment API internally. The API acquires and redeems a one-time issuer code
over mTLS using existing Portal scope authorization and returns only a grant ID.
There is no new consent Tool, browser redirect, or additional login. Nested
Workflow actions continue to inherit their parent authorization.

The issuer validates the exact source access token against its issuance audit,
active authorization-code session, current client scope and current user authority.
Only token fingerprints are recorded, not bearer tokens. Source-session revocation
or lost provenance prevents renewal. Older access tokens need ordinary Portal
refresh before this new acquisition path can use them.

The short isolated real-mTLS test covers initial and refreshed-token acquisition,
renewal, scope expansion, wrong Host, missing provenance and source revocation.
OAuth unit tests passed 22/22 executed; Workflow 86/86; MCP 175/175 executed
(2 OAuth and 3 MCP ignored tests are not counted as passes). No hours-long or
scheduled qualification was run, per user waiver. The production Portal/native
invocation sequence was not run by this correction; Phase 1 is still incomplete.

Local container-layer binaries installed:

- OAuth `/app/service`: `1b975116e94a344f86366e7fd8f4585abe10326cceda49afaf3c0fb130891c0c`
- Workflow `/app/light-workflow`: `1db52645044f907127f946f8bf206c040279767d78caf7ecf54fd8e4a8bc2b5d`
- Gateway `/app/light-gateway`: `88c304cc0929ad26e018d956d3a509d2bf92d9445422ed142f5d5134d64ef639`

All three were running with zero restarts after deployment. The unauthenticated
live Workflow enrollment probe returned 403. Backups are under
`/tmp/phase1-backend-acquisition.4FBLYy/`; no application database reset occurred.
These replacements must be incorporated in the normal image build before
recreating the containers. No commit, push or Phase 1 completion comment was made.

### Native qualification and policy upgrade (2026-09-15 UTC)

Native attempt `01a0a2ce-b655-7c12-89b2-57c1110a633e` reached the worker,
but failed before a model turn with `Codex qualification evidence digest mismatch`.
Execution `01a0a2ce-c261-73d1-a317-20c84750186b` reported FAILED with
CONFIRMED cleanup. This is dispatch/preflight evidence, not native success.

The Codex Workflow authoring profile was corrected by importing event
`01a0a2d1-4a7f-72f0-bcbf-9bee159d8f23` from
`light-portal-event/genai/20260915-codex-workflow-evidence/events.json`.
Only its qualification evidence digest changed, to the checked-in contract digest
`sha256:5bea40c988edd30a30aa7cd25e0be4ccc69be54fa66229f940769515fd39787b`.
Publication version 3 produced snapshot `a732ced2-8bb4-3f2f-a5ed-792256a3d4de`.
Although the UI reported synchronous projection failure, the event committed and
the asynchronous projection completed; preview then reported CURRENT. The
publication was not blindly retried.

Restart exposed missing accepted-version evidence for Workflow-only Agents.
`initialize_workflow_policy` now pins AGENT_POLICY reference evidence against
`agent_policy_snapshot_t`, and monotonic runtime-scope upgrades recognize that
source as well as interactive sessions. The real disposable-PostgreSQL regression
passed, including repeat startup, forward upgrade and downgrade rejection;
`cargo check --locked -p light-agent --lib` and the musl release build passed.

Local Codex Workflow Agent uses image
`networknt/light-agent:phase1-policy-upgrade-20260915`; its scoped recreation
overlay/script is under `/tmp/phase1-policy-upgrade.HdE1ir/`. The retained prior
snapshot was briefly selected to let the fixed runtime record its genuine accepted
version-2 evidence, then the corrected version-3 snapshot was restored. After
restart the Agent was running, and both accepted publication versions were verified
in PostgreSQL. No fabricated reference evidence or database reset was used.

Attempt `01a0a2d5-ec58-73e1-848b-6fb855e9c9ba`, accepted while the Agent
was down, was cancelled through Portal. Its feature
`21342de0-074b-4f92-b705-0ed4dd16aa91` still owns personal VM generation 4.
A subsequent Portal lifecycle read returned HTTP 401: current subject authorization
no longer matches the accepted disclosure ceiling. Do not release this fence by
hand or start overlapping work. Session authorization and normal cancellation
reconciliation need qualification next. Native success, snapshot transfer,
in-flight cancellation/restart, GitHub publication and installer end-to-end
qualification remain incomplete. No Phase 1 completion comment was posted.

### Offline Agent cancellation delivery (2026-09-15 UTC)

Signing out and in again did not restore lifecycle disclosure for historical run
`01a0a2d5-ec58-73e1-848b-6fb855e9c9ba`; the exact accepted-claims digest guard
remains unchanged. A separate cancellation-delivery bug was proven: Workflow's
poll excluded cancelled/expired PENDING jobs, so an Agent offline at acceptance
could never learn of them and acknowledge cleanup.

The pull message now carries optional `cancellationRequested` (default false).
The existing mTLS/app/Host/Agent checks still select the matching Agent. Cleanup-only
delivery skips execution authorization, not peer authentication; Agent validates
immutable job identity and records cancellation atomically with admission. Its
turn-creation query excludes cancellation requests. Previously admitted turns
retain their Controller identities and require existing positive cleanup proof.
Workflow never treats its own PENDING state as non-dispatch evidence.

Validation: three transport tests, the real disposable-PostgreSQL Agent regression
(early/expired cleanup, replay and existing-turn identity), and the exact cleanup
receipt test passed. Both musl application builds passed. Local images are
`networknt/light-agent:phase1-cancel-delivery-20260915` and
`networknt/light-workflow:phase1-cancel-delivery-20260915`; recreation overlay is
`/tmp/phase1-cancel-delivery.C3LNhy/compose.yml`.

After deploying Workflow and both dedicated Workflow Agents, the historical job
reported FAILED (deadline exceeded) with `not-dispatched` cleanup. Normal
reconciliation released personal VM generation 4 and advanced it to free generation
5, without administrative database writes. This qualifies the offline-before-
admission cancellation path, not in-flight native cancellation.

Fresh inspect-only run `01a0a2e7-3720-7872-ba49-1e68dc775505` was accepted through
Portal and its lifecycle status read succeeded using the refreshed session.
Feature `b6ce8b8a-c72b-4fc8-a3fb-741f853512fc` reached native execution
`01a0a2e7-3eee-76c2-b004-fd5c74973473`. Both Codex job
`01a0a2e7-3766-7191-bd13-c61d3b50d2b3` and Claude job
`efe47edd-823d-4c2f-b5fe-03bf2724d331` succeeded. Both Controller attempts
reported SUCCEEDED / CONFIRMED cleanup, the workflow completed, and Portal
displayed Claude's acknowledgement. Normal post-qualification cancellation
released the development feature and advanced the VM to free generation 6.
This qualifies successful native inspection for both engines, not editing or
publication effects.

### Portal refresh disclosure correction (2026-09-15 UTC)

The fresh run reproduced lifecycle HTTP 401 after its initial successful status
read. Portal's `RefreshCoordinator::renew` generates a new `csrf` value at every
refresh, but the stable subject digest retained this nonce. The correction excludes
only that nonce from disclosure normalization; HTTP CSRF validation is unchanged.
Historical accepted claim objects are checked against their original digest before
being normalized for comparison. No stored authorization evidence is rewritten.
Identity, Host, client, role and scope changes still fail closed.

Ten contract/fixture tests, 18 Workflow API tests and 10 Gateway workflow tests
passed, plus both musl builds. Local Gateway/Workflow images use tag
`phase1-csrf-disclosure-20260915`; the overlay is
`/tmp/phase1-csrf-disclosure.muCAoO/compose.yml`. The existing completed run became
readable through Portal after deployment with no additional login or consent.
Two snapshot component tests (chunk recovery/corruption and manager scoping/replay)
also passed, but native snapshot-transfer/restart qualification remains separate.

### In-flight native cancellation (2026-09-15 UTC)

Run `01a0a2ef-8227-7561-b78c-f4901fbd5bcd`, feature
`9968a37f-f222-49c3-bebd-188942639abb`, was accepted for the same bounded
inspect-only definition. Controller execution
`01a0a2ef-999b-79e1-b063-e10370a30af6` was verified STARTED before Portal
cancellation. Portal returned CANCELLED at `2026-09-15T02:40:41.185780Z`.
Controller then reported CANCELLED / CONFIRMED cleanup; personal VM advanced to
free generation 7. Workflow was restarted immediately after the cancellation
request, and the released state survived. Because cleanup completed quickly, this
is not evidence of crashing a runner while cleanup is still pending.

Remaining live gates include native snapshot transfer/restart, interruption during
unfinished runner cleanup, GitHub publication, and installer application end-to-end
qualification. Phase 1 remains incomplete; no completion comment, commit or push
was made. Configuration data was preserved throughout.

### Snapshot qualification preparation (2026-09-15 UTC)

Snapshot requests previously required the caller to know the server-generated
stage execution ID. Native enqueue now fills an omitted `managerSnapshot.stageId`
from the accepted stage receipt before computing the immutable job digest.
Explicit mismatches fail; existing pinned-task, active-feature and VM-owner checks
remain. The focused native-job tests pass, including exact replay and malformed or
conflicting stage rejection. This source change has not yet been deployed.

Read-only sizing of the successful native task found 128 repository checkouts and
381,228,402 bytes of tracked Git blobs. Snapshot packages retain complete file
contents and enforce a 33,554,432-byte serialized bound, so the existing personal
task is unsuitable for this bounded transfer qualification. Do not raise the cap
or omit repositories silently. A separate qualification workspace restricted to
the approved `networknt/light-agent` repository is proposed; its registration and
matching policy/runner binding require operator approval. No new events were
imported and no GitHub effects were performed in this continuation.

### Native snapshot transfer qualified (2026-09-15 UTC)

The subsequent owner-approved isolated `phase1-qualification` workspace contains
only `networknt/light-agent`. Definition v1.0.5 and its Gateway binding are active.
Run `01a0a32e-d304-7112-b51e-896b8e0b1b19` completed a three-chunk, 323591-byte
snapshot transfer. All worker reports were SUCCEEDED / CONFIRMED cleanup.
Portal returned `transferComplete: true`; retained artifact
`d9c2397c-95e0-78cc-976c-32c4ad898eca` is VERIFIED / BOUND / RETAINED. Independently
hashed stored bytes match receipt SHA-256
`9562fcf4727f80baafd309ee3f8bb420ac8852e6913a5681ab5817fb16f945f7`.

Fixes preserve owner-bound workspace authorization and strict snapshot-only
request/result shapes. Normal model-output requirements no longer apply to a
fixed manager read. Workflow compares typed requests so absent/null optional
first-chunk package digests do not prevent reconciliation. The saved first
report was accepted after a Workflow restart without rerunning that worker.
Both later chunks completed, and qualification cleanup released VM `personal`
at generation 10. The live transfer used the Codex runner; Claude compatibility
is covered by the shared code change, not a separate live transfer claim.

Verification: 41 runner unit tests, one runner WebSocket integration test,
optional-digest regression, and snapshot chunk corruption/recovery test passed.
Detailed event, deployment, failure, and passing evidence is in sibling repo
`light-portal-event/workflow/20260915-snapshot-qualification/README.md`.
Daily automation, interruption during unfinished runner cleanup, GitHub
publication, and installer application end-to-end gates remain separate.
Phase 1 remains incomplete; no completion comment or commit was made.

### Unfinished cleanup recovery correction (2026-09-15)

Runner `Supervisor::recover` discarded backend cleanup errors and then assigned
CONFIRMED solely because a backend operation ID existed. The new regression
failed against that behavior. Recovery now persists CLEANUP_REQUIRED and returns
an error when bounded cleanup retries fail, without publishing a terminal result
that could falsely authorize release. A later recovery retries cleanup, not the
original execution; only positive cleanup permits an UNKNOWN terminal outcome
with CONFIRMED cleanup.

Regression coverage includes reopening the SQLite journal after failed cleanup,
successful later cleanup, and killing a child process after it durably records
unfinished cleanup. Reopening that killed process's journal with unavailable
backend evidence leaves cleanup pending and emits no confirmation. The child
test uses an injected backend, not a live Controller/VM or model worker.

After the user's full local rebuild/restart, both dedicated personal Workflow
runners were verified idle and updated to binary SHA-256
`3eb58ccfdd948a8a66c5c5efc89e2313e6b11534aeb9e52844e1e8a0e7ecbf69`.
Only the two runner binary admission hashes changed; policy/configuration hashes
were preserved. Controller restarted to load admission, and both runners
reconnected healthy. Previous binaries/admission are retained under
`/tmp/phase1-cleanup-runner.wQTyT1` for this local deployment's rollback.

The automatic-login daily snapshot gate passed against the rebuilt Compose stack
and updated runners: invocation `01a0a51c-7da5-7d81-bd04-ce0d034f1019`, report
`light-portal-test/reports/snapshot-qualification/daily-dbc66af1-6755-4d24-bba0-2a6be0117ef0.json`.
The report confirms artifact verification and VM cleanup. The isolated child
process-kill regression also passed after the build. Full live unfinished-native-
cleanup/Controller-fencing qualification remains pending; neither this regression,
normal snapshot smoke nor the earlier in-flight cancellation qualifies it.

### Native cleanup interruption prerequisites (2026-09-15)

Tracing the live native path uncovered a second cleanup-proof bug:
`result_accepted` treated a Controller acknowledgement as local cleanup evidence,
cleared the terminal payload and marked CLEANUP_CONFIRMED even for a result with
FAILED cleanup. The new regression failed before the fix. Acknowledgement now
rejects unconfirmed cleanup without changing its durable terminal payload/backlog;
staged-file cleanup also runs before clearing that payload. This additional
source change is not yet deployed.

The full native interruption gate is still blocked on durable native-process
recovery: native executions currently persist an `agent-worker:<execution-id>`
operation identifier, while restart recovery delegates that identifier to the
configured backend. It is not evidence that the native process group has stopped.
Before deliberately interrupting the live worker, implement durable process or
supervisor-owned containment identity and positive cleanup proof, including
PID-reuse/unknown-evidence rejection and replay-safe Controller cleanup delivery.
Do not force-clear a reservation or present acknowledgement, missing backend
state, process timeout, or the isolated injected-backend test as that proof.

Native launch now captures Linux PID/start ticks, boot ID, PID namespace and
cgroup membership, then persists them in a separate SQLite native-process journal
before sending execution input. The identity is immutable and tied to the exact
execution/lease/fencing token. A failed capture or journal write terminates the
just-spawned worker instead of delivering execution input. Tests cover process
stat parsing, live identity capture, durable reopen, exact replay, conflicting
process identity, stale fencing, and missing historical evidence.

This is identity evidence only: a missing PID or empty process group is not proof
that descendants which created new sessions have stopped. Positive containment
cleanup/recovery and Controller cleanup receipt delivery still need implementation
before the live interruption gate. These changes are not deployed yet, and no
live native worker was interrupted for this test.

### Delegated native containment deployment (2026-09-15)

With explicit owner approval, only `light-workflow-runner-personal` and
`light-workflow-runner-claude-personal` received `Delegate=yes` drop-ins named
`zz-native-cleanup.conf`; existing `KillMode=control-group` remains. Opt-in
`agentWorker.nativeCgroup: true` creates a separate execution cgroup and attaches
the worker before execution input. Its recorded containment identity binds the
execution UUID, parent cgroup, inode, boot and cgroup namespace. Cleanup targets
only that child, uses `cgroup.kill`, and requires `populated 0`. Normal native
completion and nonterminal restart recovery use this evidence, not mock-backend
absence. Historical missing containment evidence fails closed.

The isolated `qualify-native-containment` example passed in a disposable delegated
systemd service: a worker and `setsid` descendant were removed; repeated cleanup
passed. All 50 runner unit tests and the WebSocket integration test passed.
This component test is not live Controller/VM interruption proof.

Deployed runner SHA-256:
`58af49a73582fed11d6351c72096b67d9fc991c96c6bf7b0426709d9d02e0b13`.
The two binary/config admission hashes were regenerated, Controller restarted,
and both dedicated runners reconnected ready. Backups of prior runner/configs/
admission are under `/tmp/phase1-cgroup-recovery.QPhrDE`.

The ordinary live snapshot gate passed with containment enabled; report:
`light-portal-test/reports/snapshot-qualification/daily-e259bce5-514d-47c1-a984-0e538d3d3631.json`.
All three execution cgroups reported unpopulated after cleanup. Deliberate live
interruption during unfinished cleanup and Controller-fenced VM release remain
unqualified; do not infer those results from the normal snapshot gate.

### Operator-authorized retirement of historical UNKNOWN (2026-09-15)

With explicit owner approval, `reset-native-scope --confirm-fence` fenced only
the dedicated Claude user service. It checked the exact configured process,
unit invocation, boot, cgroup namespace and inode, stopped the control-group
unit, and required an inactive unit and empty/absent kernel cgroup. A separate
durable operator confirmation remains bound to the original execution, lease,
fence and terminal digest; this is not an automatic model retry or a database
override. Controller authenticates the matching enrollment and current session
before accepting `operator-unit-fence:<sha256>` cleanup evidence.

Runner tests: 55 passed, with additional acknowledged-proof/reopen/stale-lease
regression passing. The real PostgreSQL Controller fencing test passed in
disposable database `phase1_operator_controller_1789499514520`.
Live failed feature `5a961638-261b-4b1f-9c64-7797c09d11f1` was retired through
the normal owner cancellation flow: state `cancelled`, VM generation 31 released.
The runner reconnected with zero cleanup backlog. No uncertain turn was replayed
and no Configserver database was wiped. This qualifies historical operator
retirement, not deliberate interruption of a newly running native worker.

The fresh publication cycle and full installer application qualification remain
pending; Phase 1 is not complete.
