# LLM gateway rollout and rollback

Owner: `gateway-sre`

Do not begin REL-1 until the release manifest closes PERF-3, OBS-1, and SEC-1,
the selected deployments have `captured_sanitized` conformance evidence, and
live codec validation has passed. Keep `llm-router.enabled: false` everywhere
until the disabled stage has proved two-replica projection convergence, audit
health, WAL headroom, and last-valid snapshot retention.

Promote in the exact order in `rollout-plan.json`: disabled, one internal host
and public alias, a small authenticated-principal allowlist, then explicit
alias/host batches. Observe every stage for at least its declared duration.
Record only bounded labels and sanitized evidence. Stop promotion immediately
when any declared threshold is exceeded; a later healthy sample does not erase
the failed window without owner review.

## Rollback drill

1. Record the active sequence and digest on every replica.
2. Publish the previous immutable resource set as a **new manifest with a
   higher sequence**. Never decrement or reuse a projection sequence.
3. Wait for every replica to acknowledge the rollback sequence and expose the
   rollback digest. Retain the failed and prior snapshots for diagnosis.
4. If the data plane itself is suspect, set `llm-router.enabled: false` and
   reload every replica. Disable requests cancellation of the projection and
   audit-sink tasks. `JoinHandle::abort` is not a synchronous join; confirm the
   workers have quiesced and no stale snapshot can subsequently publish.
5. Confirm the audit WAL drains, no canary request remains incomplete, and the
   previous public behavior is restored before closing the drill.

An admitted request retains its audit reservation after disable. The WAL writer
therefore keeps the directory lock until terminal audit records and queued
non-durable records drain. A rapid re-enable can safely fail with `LLM audit WAL
directory already has an active writer`. Do not delete `writer.lock` or bypass
the flock. Wait for in-flight requests to finish and retry the reload; escalate
if the audit/WAL dashboards show no progress or the lock outlives the maximum
request deadline plus the configured drain window.

Store sanitized evidence at the paths declared by `rollout-plan.json`. Run
`./scripts/run-llm-rollout-gates.sh --require-live-evidence`; a missing file,
failed threshold, divergent digest, non-monotonic rollback, or missing approval
must fail the gate.
