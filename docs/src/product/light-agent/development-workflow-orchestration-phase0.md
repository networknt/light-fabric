# Development Workflow Phase 0 Qualification

Phase 0 of [issue #392](https://github.com/networknt/light-fabric/issues/392) is
implemented in `crates/development-workflow-contract`. The parent
[personal workflow design](development-workflow-orchestration.md) remains the
lifecycle specification. This milestone makes no model calls and does not start
or deploy the personal workflow pilot.

## Implementation

The new crate defines requirements and phase manifests, feature/stage identities,
claims and results, snapshots/deltas, findings/remediation, review coverage,
budgets, sign-off, VM reservation/release, document/publication revisions and
comment intents. Strict Serde contracts reject unknown worker fields. Checked-in
JSON Schemas cover review and remediation outputs.

Canonical finding IDs are deterministic, domain-separated SHA-256 identities
over the feature, preallocated review and local finding ID. Replay returns the
saved mapping, with a digest conflict for altered output. Alias history is kept;
ambiguous/cross-feature references and simultaneous alias/closure are rejected
without partially mutating the ledger. Implementer dispute evidence cannot close
a finding; reviewer or authorized human disposition is required.

Coverage preserves the original full-review digests and a contiguous chain of
saved candidate transitions. Codex may fix and resume before Claude starts its
first full review. A later local Claude-verified fix can carry Codex's unaffected
coverage explicitly. Broad or uncertain changes require every already-started
final reviewer; subsequent full review covers a reviewer not yet started.
Changed sessions, missing snapshots/deltas, skipped checks and forged verdicts
block publication.

Claim preflight checks historical replay before current feature version/owner;
a changed request conflicts. Budgets charge logical dispatch once and retain
consumption across replacement/reopened stages. Snapshot/result acceptance checks
current inputs/claim and persisted reviewer evidence. VM release needs matching
owner/generation, terminal state and confirmed execution/effect evidence.

## Verification

Run from the repository root:

```sh
bash scripts/run-development-workflow-phase0-gates.sh
```

The gate runs formatting, 20 deterministic tests, strict Clippy and whitespace
checks. Tests include:

- review replay and serialization, explicit rewording/duplicates, ambiguous
  references, disputes, reopen and non-progress without retry inflation;
- local/broad final fixes, Codex fixes before Claude full review, missing coverage,
  invalid snapshots/deltas, skipped checks and replacement-session rejection;
- optional/exact-digest sign-off, stale/rejected approval and cumulative budgets;
- matching JSON handoff fixtures, duplicate historical claims, stale/changed
  inputs and conflicting stage state;
- stage acceptance with persisted review evidence, missing durable snapshot,
  fenced VM release, immutable publication tasks and monotonic status comments;
- JSON Schema/Rust parsing of worker fixtures and rejection of unknown fields.

`mdbook build docs` also passes. No required test is environment-skipped.

## Phase 1 Enforcement Boundary

These are pure reference rules and typed receipts, not a database implementation
or an authentication boundary. Phase 1 must verify artifact contents and receipt
provenance; load trusted review allocations, policies, clocks and budget scopes;
and persist state atomically under feature/effect locks. A worker must never
supply its own accepted ledger or scope-impact decision.

Claim, invocation/process/initial task and ownership must commit together. Tests
of simultaneous starts, rollback and lost responses belong to that store path.
Dispatch/effects must require the claim. Snapshots need durable export and fixed
runner → Controller → Workflow transfer; native conversation history is separate.
Phase 1 also implements the filesystem artifact backend, actual workflow Agent
jobs, sign-off controls, fixed GitHub actions and confirmed VM fencing/release.

No current Workflow listener or publication action is wired to the new crate in
Phase 0. Do not infer runtime enforcement or pilot readiness from these tests.
