# A00 approach checkpoint — 2026-09-30

Status: owner approved implementation on 2026-09-30, including deferred legacy physical cleanup. A00 and S00 remain open. Scope is the general artifact bug only.

## Verified starting state and evidence

Worktree `/home/steve/workspace/light-fabric-issue425-a00`, branch `issue-425-red-regression`, HEAD `38f3718cd2dc11b9c2c029ee1e93a16789f8fdeb`; registered against the main repository. Initial dirty state consists only of the existing untracked regression, checkpoint and red log. These are preserved unchanged.

Read the applicable repository instructions, RTK instructions, checkpoint, complete regression, saved log, issue draft and A00/tracker sections. No tests or database operations were rerun. Historical disposable PostgreSQL/filesystem evidence is **0 passed, 1 failed, exit 101**, with future-retention, legal-hold and direct process-helper scenarios all losing survivor bytes. The helper test is not a live deletion flow. Reuse of the old disposable database is prohibited without a fresh fixture: its schema is already used.

Source confirms digest-only durable keys and reads. `publish_artifact_in_transaction` already locks and checks replay metadata, whereas `publish_artifact` promotes before a sufficiently strong deletion/replay fence and `promote_artifact_evidence` can return success for a BOUND deleted row. Cleanup completion checks DELETING but not the claimed attempt. `development_handoff::verify_artifact` locks authorized metadata but then derives the object key from digest rather than reading its stored reference.

GitNexus query returned no relevant processes; context for `read_verified` returned symbol not found. This is UNKNOWN graph coverage, not a clean impact result. Full required symbol impact analysis and direct caller audit precede production edits after approval; no symbols were edited here.

## Choice and tradeoffs

| Approach | Safety and cost |
| --- | --- |
| Keep shared keys with bounded delete recovery | Preserves deduplication, but needs object-level binding/delete serialization, reference accounting and generations. HTTP timeout plus margin cannot prove a remote DELETE stopped. Recreating the same unversioned key after that bound retains a late-delete risk; eliminating it requires distinct keys/generations or a verified provider guarantee. |
| Per-artifact keys for new promotions — recommended | Distinct artifact IDs have distinct keys even for identical bytes. Stalled deletes cannot block the digest. Costs duplicate storage and requires producer/recovery/read API changes plus explicit legacy handling. No concrete source obstacle found: every publication path already knows artifact identity. |

Implementation adjustment: the first real filesystem regression proved that appending an ID below the legacy digest path collides with its existing file. Use a sibling `artifacts` namespace so both layouts coexist. New canonical object path: `{prefix}/tenants/{host}/artifacts/sha256/{digest-prefix}/{digest-hex}/{artifact_id}`. Keep SHA-256 integrity verification. Stable retries of a live artifact retain its identity and reference. Never reuse a retired artifact identity/key: reject publication in DELETE_PENDING, DELETING, DELETE_FAILED or DELETED, and preserve terminal row tombstones. Any future tombstone purge needs a separate durable non-reuse/generation policy; it is not part of A00.

## Publication, reads and concurrency

All three publication paths must serialize on the tenant/artifact metadata row before durable promotion and binding, validate immutable replay fields and current retention/legal-hold/deletion authority, and retain that lock through binding. Transaction-owned publication uses its existing transaction without another pool connection. Cleanup and process-deletion updates participate in the same row locking. If publication wins, cleanup observes committed eligibility; if cleanup wins, same-identity publication fails before durable resurrection. A different artifact ID with the same digest proceeds independently. Rollback may leave an unreferenced object, but cannot authorize a row or result.

An already BOUND live legacy replay keeps its stored reference and verifies those bytes; it must not silently rewrite historical storage evidence. Pending legacy promotions bind to the new per-artifact layout under the row fence. No new promotion writes the old shared durable key. Verify existing destination bytes on retries and verify copied destination bytes before binding, preserving the current protection against changed staged content.

Read the stored reference from tenant/process-authorized, locked metadata. The storage reader accepts only exact canonical configured-prefix paths for the expected Host and digest, and, for new paths, the expected artifact ID. Accept both legacy and new layouts; reject staging paths, traversal, alternate tenant/ID/digest and malformed references. Enforce streaming size bounds and actual SHA-256 verification. Digest-only derivation is not a fallback for new rows. Cleanup also validates the reference against row identity; arbitrary or shared/malformed references must never become deletion authority.

Attempt completion predicates include Host, artifact ID, DELETING state, deletion_attempt and the claimed storage reference. Stale requeue does not revoke remote I/O; a newer attempt can retry the same permanently retired key, but an older completion cannot overwrite its metadata. No legal hold or retention extension may resurrect an identity after physical deletion has been admitted. A hold established before the claim prevents deletion. This fence must be documented and exercised, without claiming that late SQL changes can recall issued DELETEs.

## Legacy-object safety policy

**Do not issue automatic physical DELETE for legacy shared durable keys in A00, even when every row is eligible.** This conservative policy avoids a reference-check/promotion race and protects retained and legal-held siblings. Retire only eligible rows logically, preserving tombstones. If the existing DELETED state is used for logical retirement, evidence explicitly records `physicalDeleteDeferred: true`, the reference and reason `legacy-shared-object`; omit `verifiedAbsent: true`, and document that DELETED does not certify physical erasure in this case. Audit existing consumers of this state before implementation; add a distinct state/migration if they require physical absence. Held rows remain untouched. Retries and stale legacy claims follow this policy rather than dispatching another delete.

Legacy retained rows continue reading their unchanged stored references. Malformed/unrecognized references fail closed without object deletion. New per-artifact objects still undergo real deletion plus absence verification once eligible. Thus the all-references-eligible case yields verified physical deletion for new objects, and explicit deferred physical cleanup for legacy objects; owner approval includes this departure from immediate legacy reclamation.

Automatic legacy reclamation and bulk migration are deferred, not delegated to capture work. A later owner-authorized maintenance operation can copy verified surviving bytes to never-used per-artifact/generation keys, verify destination digest/size, and atomically change only storage bindings while preserving IDs, digests and provenance. It must account for old workers and outstanding remote deletes before reclaiming old shared keys. No blanket deletion by digest and no timeout-only safety argument.

The code change cannot cancel DELETEs already issued by old binaries. Before owner-run rollout, old cleanup/promotion workers must be fenced from further operations; retained legacy bytes with uncertain prior deletes require verified copy to fresh keys from surviving/staged/backup content. A successful HEAD or read is only an observation, not proof that a late DELETE cannot happen. Missing bytes cannot be reconstructed from metadata. A00 does not claim repair of historical loss or safe mixed-version operation; prepare this as an owner-run runbook, without executing deployment or repair.

Identical content remains publishable immediately using a new artifact ID, regardless of a stalled legacy or new-object deletion. No digest blacklist and no remote-settlement waiting period are required for that recovery path.

## Required implementation gates after approval

Preserve the historical red test/log. Add explicit legacy fixtures with two rows referencing the same physical object, keeping the equality assertion: future retention, legal hold and direct process-helper behavior, plus all-eligible logical retirement with truthful deferred evidence. Do not replace legacy coverage with new-layout publication.

Add distinct-ID/equal-content publication and deletion; stable live retries; rejection of retired identity replay in every publisher; legacy stored-reference reads; malformed/cross-tenant/cross-artifact references; size/digest corruption; new-object physical absence; stale requeue and attempt completion fencing. Use local barriers and a delayed-delete store fixture for publication/cleanup ordering and late-delete execution: old deletes hit only retired keys while new same-content IDs remain readable. These deterministic simulations are not S3 qualification.

Run the focused disposable PostgreSQL/filesystem regression with a freshly initialized dedicated fixture, then affected artifact store/publication/retention/admin and development-handoff compatibility tests, formatting/whitespace and relevant lint gates. Resolve exact target/filter names during implementation and record commands, counts and unexecuted gates. Broaden only for concrete caller/schema risk. No credentials, shared database, service restart, deployment, commit, push, PR creation or issue closure.

## Owner checkpoint

The owner approved this approach, including deferred legacy physical cleanup. The DELETED audit selected a distinct RETIRED state and DEFERRED receipts. Implementation and component evidence are recorded in issue425-a00-checkpoint.md; A00 remains open at the reviewable-diff checkpoint. This document preserves the design comparison, not test evidence.
