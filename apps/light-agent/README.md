# light-agent

The enterprise agent service runs on `light-axum` and persists authenticated
sessions, turns, actions, approvals, and event projections in PostgreSQL.

Production gateway calls require `LIGHT_AGENT_DELEGATION_SECRET` with at least
32 bytes. The same value must be configured as
`LIGHT_GATEWAY_AGENT_DELEGATION_SECRET` on `light-gateway`. Light-agent mints a
short-lived token for each `tools/list` or exact `tools/call`; it never forwards
the caller's broad bearer token in this mode.

`LIGHT_AGENT_ALLOW_BROAD_GATEWAY_TOKEN=true` enables the legacy bearer-forwarding
path for local compatibility only. It is disabled by default.

Memory writes use `portal-command` by default so authorization, events, and
auditing remain in the command boundary. `LIGHT_AGENT_MEMORY_WRITE_MODE=direct-pg`
is a development compatibility mode and is rejected unless
`LIGHT_AGENT_ALLOW_DIRECT_PG_MEMORY=true` is also set.

Apply `portal-db/postgres/patch_20260711_light_agent_runtime.sql` after the
workflow-runner migration before starting this version.

Every active production `agent_definition_t` must publish an immutable policy
by setting `policy_snapshot_id` to a non-revoked `agent_policy_snapshot_t` for
the same host and agent definition. Startup fails if the configured default
definition has no such snapshot, and an authenticated session for any selected
definition fails closed until its published snapshot exists. Light-Agent
deserializes the closed policy document, recomputes its SHA-256 digest, and
checks every component digest against the immutable columns before admission.
It does not synthesize policy, profile, catalog, model, execution, channel, or
data-boundary digests from the session or caller identity. Resuming a session
also requires the exact published snapshot and agent-definition version that
were originally admitted.

The process-wide `AgentState` contains only shared infrastructure and caches.
Provider, model, definition version, policy, data boundary, product profile,
service pool, compatibility digest, and effective catalog identity are resolved
again from the activated durable turn. The signed caller may select an agent
definition with the `agentDefId`/`agent_def_id` claim; deployments without that
claim retain the configured single-definition fallback. A definition update,
policy revocation, or pool-assignment revocation therefore cannot silently
change an already admitted turn or reuse a mismatched catalog entry.

Queued turns are dispatched across sessions under a host-scoped PostgreSQL
advisory transaction lock. Dispatch first favors principals with fewer running
turns, then the principal least recently activated, then durable FIFO creation
order. The same transaction enforces session exclusivity and pool concurrency;
all replicas use this path, so scale-out does not create a per-process fairness
island.

Waiting WebSockets do not poll PostgreSQL. Each replica owns one dedicated
`LISTEN` connection for queue, capacity, and activation notifications plus an
in-memory per-turn `Notify` registry. Admission and terminalization publish
transactional notifications; any replica may perform the serialized dispatch,
and the activation notification wakes the replica holding the corresponding
WebSocket. The listener subscribes before its catch-up pass and runs a bounded
five-second catch-up after reconnect, so notifications are latency hints rather
than the source of truth.

## Profile dispatch

Messages without a `profile` retain the enterprise gateway/model behavior.
Coding and personal edge execution use closed, typed payloads and are dispatched
only after the durable turn acquires its session fence and its published
`productProfileDigest` matches the operator-enabled profile.

Enable the Pi coding profile with all of the following settings; partial
configuration fails startup:

```bash
LIGHT_AGENT_CODING_PROFILE_DIGEST=sha256:<published-coding-profile-digest>
LIGHT_AGENT_CODING_REPOSITORY_URI_PREFIX=file:///var/lib/light-agent/repositories/
LIGHT_AGENT_CODING_COMPATIBILITY_DIGEST=sha256:<approved-cube-compatibility>
LIGHT_AGENT_PI_TEMPLATE_DIGEST=sha256:<approved-command-template>
LIGHT_AGENT_PI_BINARY_DIGEST=sha256:<pinned-pi-binary>
LIGHT_AGENT_PI_PROVIDER=brokered
LIGHT_AGENT_PI_MODEL=<approved-model-alias>
```

The repository source adapter must place immutable Git bundles under the
configured spool before the message is admitted. Arbitrary local paths and
remote URLs are rejected. Light-Agent constructs the materialization manifest
itself and currently admits no client-selected skill packages.

```json
{
  "clientMessageId": "01-coding-turn",
  "profile": "coding",
  "text": "Update the parser and its tests.",
  "coding": {
    "repository": {
      "artifactUri": "file:///var/lib/light-agent/repositories/acme/repo.bundle",
      "digest": "sha256:<bundle-digest>",
      "size": 12345,
      "mediaType": "application/x-git-bundle"
    },
    "baseRevision": "<40-or-64-hex-commit>",
    "workspaceRoot": "/workspace/repo",
    "writableRoots": ["/workspace/repo"],
    "allowedTools": ["fs.read", "fs.write"],
    "maximumPatchBytes": 1048576,
    "maximumChangedFiles": 100
  }
}
```

Personal edge actions require
`LIGHT_AGENT_PERSONAL_PROFILE_DIGEST=sha256:<published-profile-digest>`. The
typed `edgeAction` contains `edgeBindingId`, `action`, `arguments`,
`schemaDigest`, and an optional `approvalId`. The server revalidates the live
principal-bound edge runner, exact action schema, effect class, approval, runner
identity, backend identity, and compatibility digest before enqueueing. Direct
edge turns terminate from the accepted runner result; they do not leave a
session waiting for a nonexistent in-process model continuation.

## Trusted quota accounting

Token and cost quotas reserve the admitted turn ceiling transactionally.
Enterprise model usage is accepted only from the server-owned provider adapter
and cost is calculated from the immutable `agent_model_rate_t` rate snapshot
stored on the turn. Runner-backed model usage is accepted only from
runner-journal broker counters copied into terminal evidence; sandbox-provided
`usage` fields are ignored. Missing or uncertain usage settles at the reserved
ceiling instead of refunding capacity. Pre-dispatch failures explicitly release
their reservation. Every reconciliation records its accounting source and, for
trusted actuals, an evidence digest, making retries idempotent and auditable.

Deployments with cost quotas must publish an enabled model rate for the exact
host, provider, and model before admitting a turn. Rates are expressed as
micro-units per one million input or output tokens and are snapshotted at
admission so later rate changes cannot alter an in-flight turn.
