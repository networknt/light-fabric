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
