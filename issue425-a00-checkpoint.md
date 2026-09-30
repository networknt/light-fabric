# A00 red-regression checkpoint — 2026-09-29

Baseline: `38f3718cd2dc11b9c2c029ee1e93a16789f8fdeb`.
Worktree: `/home/steve/workspace/light-fabric-issue425-a00`.
Branch: `issue-425-red-regression`. No commit, push, production fix or shared-service change.

## Regression

New test: `apps/light-workflow/tests/artifact_retention_shared_postgres.rs`.
Uses real `publish_artifact`, filesystem-backed `DurableArtifactStore`, PostgreSQL metadata and `ArtifactRetentionReconciler::reconcile_once`. Two distinct artifact rows in the same Host publish identical bytes. After cleanup of one artifact, the other must remain readable.

Three scenarios execute within the single test: future retention, legal hold, and direct invocation of `mark_process_deleted` for only the first artifact's process. The latter does not claim the helper has a production caller.

The fixture refuses a database with an existing `workflow_ops` schema. PostgreSQL ran in a dedicated `postgres:17-alpine` container `issue425-a00-postgres`, bound only to `127.0.0.1:55425`, with disposable trust authentication and database `issue425_a00`. No external credentials. The temporary filesystem store is removed by `TempDir` even on the terminal assertion failure.

Executed from the worktree:

```bash
rtk proxy env CARGO_TARGET_DIR=/home/steve/workspace/light-fabric/target ARTIFACT_RETENTION_TEST_DATABASE_URL=postgres://postgres@127.0.0.1:55425/issue425_a00 cargo test --locked -p light-workflow --test artifact_retention_shared_postgres -- --ignored --nocapture
```

Final saved run: **0 passed, 1 failed, 0 ignored, 0 measured, 0 filtered; exit 101**. All three cases reached the intended assertion: survivor metadata is `RETAINED`, while object existence and digest-verified read are both false. Raw output: `issue425-red-regression.log`.

One initial attempt failed on sandbox-denied TCP connection before setup; it is not regression evidence. Two subsequent authorized runs against freshly initialized disposable state reproduced the semantic failure; the log records the final run. Build succeeded. `rustfmt --check` and whitespace checks pass.

## Impact evidence

GitNexus impact queries for `reconcile_once` and `publish_artifact` both returned target not found / UNKNOWN, not a clean impact result. Direct source confirms the retention worker is called from `main.rs`, with existing coverage in `admin_api.rs`. This checkpoint changes only a new integration test. No production symbol was edited.

## Fix comparison for owner review

| Choice | Benefit | Required work / residual risk |
|---|---|---|
| Shared keys with bounded recovery | Preserves deduplication and existing digest-based reads | Needs object-level metadata/locking, all-reference retention checks, publication/deletion fencing and attempt generations. A client timeout plus margin is an operational assumption, not proof a remote S3 delete cannot execute late; recreating the same key can still be unsafe without a provider guarantee or a new object generation/key. |
| Per-artifact keys for new writes (recommended) | A delete for artifact A cannot remove B's object; avoids indefinite digest-level admission fences | Adds duplicate storage. Pass artifact identity into promotion, read the stored tenant-validated reference rather than derive a key from digest, and preserve retries on the same artifact identity. Existing shared-key references need a separate safe treatment. |

Recommended new path: tenant + digest + artifact ID. Never reuse a deleted artifact ID for a new publication. A late delete on the old artifact key cannot touch a newly published artifact's distinct key. Existing row-based retention machinery still needs stale-attempt guards; unique keys do not make same-identity resurrection safe.

Legacy safety is mandatory: old shared rows must not be physically deleted while any protected row references their object. A conservative transition can retire eligible row metadata while retaining shared legacy bytes, then migrate surviving references to per-artifact keys under an explicit maintenance policy. Future promotion must never recreate a legacy key. If a legacy key has an uncertain in-flight delete, copy from verified surviving/staged bytes to a new key; do not treat a timeout as proof of safety. Already deleted bytes are unrecoverable unless another source exists. Physical orphan cleanup may remain deferred without blocking new captures of the same content, because new captures use new keys.

The current red test deliberately asserts that baseline promotion uses equal references. After selecting the fix, preserve explicit legacy-shared fixtures and add new-layout tests rather than simply changing that equality assertion and claiming legacy coverage. Additional fix gates must cover sibling release, stale attempts, concurrent promotion, retained/legal-held legacy siblings, new IDs after uncertain deletes, and safe stored-reference reads.

## Approved implementation checkpoint — 2026-09-30

The owner approved per-artifact keys and deferred legacy physical cleanup. Implementation and isolated component gates are complete; **A00 is APPROVED at the code-review checkpoint**, with publication, deployment and issue closure still pending. S00 remains separately open. No deployment, commit, push, PR publication or issue closure was performed.

New references use `{prefix}/tenants/{host}/artifacts/sha256/{digest-prefix}/{digest}/{artifact_id}`. The first green fixture exposed a concrete filesystem collision with the originally proposed `objects/.../{digest}/{id}` path: the legacy digest is a file, not a directory. The sibling `artifacts` namespace fixes coexistence without changing legacy references. This implementation adjustment is recorded in the approach notes.

All publishers lock the artifact row through replay validation, durable promotion/verification and metadata binding; standalone publication delegates to the transaction-owned path. All deletion states outside RETAINED, including RETIRED, reject replay. New IDs with identical bytes remain admissible while old deletes are stalled. Existing live BOUND references are verified in place. Development handoff fetches the stored reference under its existing authorization lock; canonical reference checks enforce prefix, Host, digest and new-layout artifact ID, with bounded reads and actual SHA-256 verification.

The DELETED consumer audit found native deletion receipts could report COMPLETE. Migration `0020_artifact_legacy_retirement` introduces **RETIRED**, preserving DELETED as physical deletion plus verified absence. Legacy cleanup records physicalDeleteDeferred and never verifiedAbsent. Receipts expose deferred counts and DEFERRED when no other outstanding retention/cleanup remains. Runtime admission requires the new migration ledger entry. Release manifest/order/checksums listed the migration, but the first submission omitted the packaged SQL copy (corrected below); no deployment configuration or copied deployment assets were changed.

Cleanup rejects references not owned by the row, never physically deletes legacy shared keys, and fences success/failure updates by Host, artifact ID, DELETING, attempt and reference. Stale requeue does not cancel I/O. Delayed-delete fixtures complete attempt one while attempt two is DELETING, proving stale success/failure cannot mutate it; fresh same-content IDs remain readable after the actual simulated late local delete. The concurrency fixture observes a real PostgreSQL lock wait from the direct process helper while promotion owns the row, then checks cleanup/replay behavior in both orders. These are deterministic local simulations, **not actual S3 qualification**.

### Original submission evidence (review corrections below supersede package claim and focused counts)

| Gate | Result | Evidence boundary |
| --- | --- | --- |
| Original baseline red regression | 0 passed, 1 failed; three named scenarios; exit 101 | Preserved historical PostgreSQL/filesystem log and source |
| Focused shared/new artifact regression | 8 passed, 0 failed, 0 ignored | Fresh per-case databases, real filesystem; local late-operation fixtures |
| Workflow library | 148 passed, 0 failed, 15 ignored | Unit/local HTTP stubs; ignored DB tests not counted as passes |
| Development store/handoff/publication compatibility | 1 passed, 0 failed, 0 ignored | Fresh DB, runtime role, filesystem; one umbrella integration test |
| Native process deletion compatibility | 1 passed, 0 failed, 0 ignored, 162 filtered | Dedicated migrated DB; local mock deletion backend; includes DEFERRED receipt |
| workflow-store | 1 passed; 1 PostgreSQL test ignored; 0 doc tests | Unit only, not a runtime binding qualification |
| All-target check; formatting; whitespace | Pass | Source/build |
| Migration bundle, original claim withdrawn | Root-source digests passed, but the package was incomplete: packaged 0020 missing | The original custom validator masked the missing package file; corrected review evidence below supersedes this claim |
| Clippy --no-deps | Exit 0, existing warnings | Affected packages/all targets; only warning in artifact modules is the pre-existing runner publisher argument count |
| Strict Clippy -D warnings | Blocked by baseline dependency warnings | config-loader, workflow-core, model-provider; no unrelated fixes |

Exact test/check commands, from this worktree:

```bash
rtk proxy env CARGO_TARGET_DIR=/home/steve/workspace/light-fabric/target ARTIFACT_RETENTION_TEST_DATABASE_URL=postgres://postgres@127.0.0.1:55426/issue425_green cargo test --locked -p light-workflow --test artifact_retention_shared_postgres -- --ignored --nocapture
rtk proxy env CARGO_TARGET_DIR=/home/steve/workspace/light-fabric/target cargo test --locked -p light-workflow --lib
rtk proxy env CARGO_TARGET_DIR=/home/steve/workspace/light-fabric/target DEVELOPMENT_WORKFLOW_TEST_DATABASE_URL=postgres://postgres@127.0.0.1:55426/issue425_green_development cargo test --locked -p light-workflow --test development_store_postgres -- --ignored --nocapture
rtk proxy env CARGO_TARGET_DIR=/home/steve/workspace/light-fabric/target WORKFLOW_NATIVE_OPS_TEST_DATABASE_URL=postgres://postgres@127.0.0.1:55426/issue425_green_native cargo test --locked -p light-workflow --lib native_note_delete_owner_retry_hold_and_definition_preservation -- --ignored --nocapture
rtk proxy env CARGO_TARGET_DIR=/home/steve/workspace/light-fabric/target cargo test --locked -p workflow-store
rtk proxy env CARGO_TARGET_DIR=/home/steve/workspace/light-fabric/target cargo check --locked -p light-workflow --all-targets
rtk proxy env CARGO_TARGET_DIR=/home/steve/workspace/light-fabric/target cargo clippy --locked -p light-workflow -p workflow-store --all-targets -- -D warnings
rtk proxy env CARGO_TARGET_DIR=/home/steve/workspace/light-fabric/target cargo clippy --locked -p light-workflow -p workflow-store --all-targets --no-deps
rtk proxy cargo fmt --all -- --check
rtk proxy git diff --check
rtk proxy python3 issue425-validate-bundle.py
```

The URLs above contain only disposable local trust-auth fixture identities. The new container is `issue425-a00-green-postgres`, PostgreSQL 17, port 55426 bound to 127.0.0.1. Fresh databases have suffixes retained, hold, process, identity, runner, late_success, late_failure, concurrent, development and native. Initial sandbox TCP/socket failures were infrastructure failures, not bug regressions; authorized reruns completed. The initial semantic green run was 5 passed/3 failed due to the filesystem path collision. Compatibility fixture repairs supply missing current admission migrations and approved binding fields, and use stored references for corruption probes. No production constraints were relaxed. Reruns reset only these dedicated disposable schemas. The old red container/database was not touched. The new green container is now stopped; its used schemas are preserved for inspection and require fresh databases or explicit disposable-only resets before rerunning fixtures.

Evidence logs (verbose Clippy logs remain local workingtree artifacts): `issue425-green-regression.log`, `issue425-development-compatibility.log`, `issue425-native-compatibility.log`, `issue425-library-compatibility.log`, `issue425-workflow-store.log`, `issue425-clippy.log`. The library log is tool-output-limited but retains the terminal counts. The original `issue425-red-regression.log` is unchanged. Original test source is preserved byte-for-byte as `issue425-red-regression.rs` (SHA-256 `8b50aa412646411b95aabcec24f9047021b9937129c91e5e83087b815b8123af`); the executable target now retains explicit equal-reference legacy fixtures and separate new-layout coverage.

GitNexus upstream impacts returned UNKNOWN/not found for artifact publishers/store/retention, the handoff reader, receipt and runtime validator. Generic run/validate matches were unrelated high/critical candidates; file-qualified queries returned not found. No clean graph result is claimed. Direct source caller audit covers result_reconciler, snapshot_transfer, development_finalize, publication_dispatch, review_artifacts, development_handoff, main retention worker, native receipts and the backend example. The authorized detect-changes compare against master reported 11 tracked files, 14 coarse symbols, 0 mapped processes and LOW risk. Missing artifact methods in the graph and exclusion of the new untracked regression/migration limit that result; it is not authoritative clean flow coverage. No symbols outside these boundaries and the migration/fixture support were changed.

Unexecuted: actual S3/provider late-delete qualification, live deployment/e2e, shared operational-store binding qualification, bulk legacy migration/reclamation and historical data repair. Legacy bytes deliberately remain stored. Preserve tombstones; do not mix old/new workers or assume old issued deletes have stopped. See `issue425-owner-runbook.md` and `issue425-pr-description.md`. Review the diff before authorizing further publication or operational work.

Original review artifact (superseded by the corrected patch): `issue425-review.patch` included 13 production/test/migration/bundle files, including the new untracked regression and migration (1,196 insertions, 131 deletions). `rtk proxy git apply --reverse --check issue425-review.patch` passed at the original submission. Checkpoint/design/evidence/PR/runbook files are supplied separately and are not included in that code patch. Nothing is staged or committed.

## Owner review corrections — P1/P2, 2026-09-30

**Historical requested-changes checkpoint: A00 remained open; S00 remained separate.** Primary-assistant review evidence: 8 artifact-store unit tests were independently rerun and all passed. This is not an owner rerun or delegated architectural review. No live/S3 qualification is claimed.

### P1: incomplete release package

Confirmed by running `rtk proxy sha256sum --check bundle.sha256` **inside the bundle**: missing packaged migration 0020, exit 1. The previous custom validator looked up SQL in the repository root and masked this defect. The earlier package-validation claim is withdrawn; root-source hashes did not qualify a self-contained release package. Failure evidence is preserved in `issue425-review-p1-red.log`.

Regenerated the package from all 39 manifest-pinned canonical SQL files, including the missing `crates/operational-store/release/bundle/crates/workflow-store/migrations/workflow-postgres/0020_artifact_legacy_retirement.sql`. Corrected validator resolves every checksum and migration path exclusively inside the package. A standalone temporary copy validates successfully; removing its packaged 0020 makes both the validator and sha256sum fail, even though canonical SQL remains present in the repository. This regression guards against the original masking error.

Corrected evidence: **41/41 packaged checksums pass** (39 SQL files, manifest and order). The 39 packaged migration digests and manifest/order agreement also pass. No deployment package outside this worktree was changed.

### P2: legitimate unbound references never terminate

Added four regressions before changing cleanup: persisted interrupted promotion with staging present, the same with staging absent, an unbound row pointing at another artifact, and an actual permanently failed runner promotion that commits QUARANTINED/REJECTED. The three filtered runs failed **0/1, 0/2, 0/1**, respectively; each reached DELETE_FAILED instead of terminal retirement. Saved evidence: `issue425-review-p2-red.log`. These are new failure findings against the first A00 implementation, separate from the original baseline red evidence.

Cleanup now captures promotion_state with its claimed row. Eligible STAGED/METADATA_COMMITTED/QUARANTINED rows become RETIRED without any object-store IO. Evidence records reason unbound-artifact, the captured promotion state/reference/attempt and physicalDeleteDeferred=true, with **no verifiedAbsent claim**. Attempt, reference and promotion-state predicates fence this retirement. Legal hold still prevents claims. Terminal tombstones reject publication replay.

This policy does not grant arbitrary staging paths deletion authority. Staging bytes may remain or may already be absent; interrupted copies may also have left durable orphan bytes. Physical reclamation remains deferred to separately authorized staging/orphan maintenance. The native receipt therefore reports DEFERRED with zero pending/failed counts, rather than falsely reporting COMPLETE or looping PENDING.

After correction: **12 passed, 0 failed, 0 ignored** in the complete disposable PostgreSQL/filesystem target, retaining all eight previous regressions; **1 passed, 0 failed, 0 ignored, 162 filtered** in the updated native receipt test. Logs: `issue425-review-green.log` and `issue425-review-native.log`. The same dedicated disposable container was restarted for these fixtures, only its disposable schemas were reset, and it is now stopped. Shared databases/services remain untouched.

Exact correction commands, from the worktree unless a directory is specified:

```bash
rtk proxy env CARGO_TARGET_DIR=/home/steve/workspace/light-fabric/target ARTIFACT_RETENTION_TEST_DATABASE_URL=postgres://postgres@127.0.0.1:55426/issue425_green cargo test --locked -p light-workflow --test artifact_retention_shared_postgres unbound -- --ignored --nocapture
rtk proxy env CARGO_TARGET_DIR=/home/steve/workspace/light-fabric/target ARTIFACT_RETENTION_TEST_DATABASE_URL=postgres://postgres@127.0.0.1:55426/issue425_green cargo test --locked -p light-workflow --test artifact_retention_shared_postgres interrupted -- --ignored --nocapture
rtk proxy env CARGO_TARGET_DIR=/home/steve/workspace/light-fabric/target ARTIFACT_RETENTION_TEST_DATABASE_URL=postgres://postgres@127.0.0.1:55426/issue425_green cargo test --locked -p light-workflow --test artifact_retention_shared_postgres permanent_runner -- --ignored --nocapture
rtk proxy env CARGO_TARGET_DIR=/home/steve/workspace/light-fabric/target ARTIFACT_RETENTION_TEST_DATABASE_URL=postgres://postgres@127.0.0.1:55426/issue425_green cargo test --locked -p light-workflow --test artifact_retention_shared_postgres -- --ignored --nocapture
rtk proxy env CARGO_TARGET_DIR=/home/steve/workspace/light-fabric/target WORKFLOW_NATIVE_OPS_TEST_DATABASE_URL=postgres://postgres@127.0.0.1:55426/issue425_green_native cargo test --locked -p light-workflow --lib native_note_delete_owner_retry_hold_and_definition_preservation -- --ignored --nocapture
rtk proxy python3 issue425-regenerate-bundle.py
rtk proxy python3 issue425-validate-bundle.py
rtk proxy python3 issue425-bundle-regression.py
# Working directory: crates/operational-store/release/bundle
rtk proxy sha256sum --check bundle.sha256
```

All-target cargo check, fmt and whitespace checks pass after correction. Clippy --no-deps rerun succeeds with existing warnings (`issue425-review-clippy.log`); the previously blocked strict gate is not presented as passed. The unaffected full library/development/workflow-store gates were not rerun in this correction turn; their counts above remain prior-submission evidence. S3/live checks and physical legacy/staging/orphan reclamation remain unexecuted.

The revised review patch includes the packaged SQL and corrected package validator/regeneration/regression helpers, as well as the A00 code and tests. No staging, commit, push, PR publication or issue closure occurred.

Final patch: 17 files, 1,422 insertions and 131 deletions. Reverse apply-check passes against the worktree. The original baseline regression source/log hashes remain unchanged, and the reviewed patch was prepared at baseline HEAD `38f3718cd2dc11b9c2c029ee1e93a16789f8fdeb`.

## Code-review approval — 2026-09-30

Both P1/P2 findings are resolved, with no remaining blocking code-review findings. Review evidence independently verifies 41 package checksums, the missing-file regression, patch consistency and whitespace. The supplied 12 PostgreSQL/filesystem and 1 native receipt passing logs were reviewed; the database tests were not rerun by the reviewer. Deferred cleanup remains explicit, with no physical-deletion claim.

A00 is review-approved, not deployed or issue-closed. S00 remains open; strict Clippy and live/S3 limits remain recorded. Commit/push/PR publication was subsequently explicitly authorized. Deployment, merge and issue closure remain outside that authorization.

## Historical checkpoint boundary — 2026-09-29

At this historical checkpoint, no production fix existed. Owner review should select the key strategy and legacy cleanup/migration policy before implementation. No S3 or live-stack qualification was performed. The disposable PostgreSQL container is stopped; its disposable metadata is retained for inspection. Restarting it preserves the already-used schema, so reruns require a fresh database or resetting only this dedicated fixture. The worktree and regression are preserved.
