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

Apply `portal-db/postgres/patch_20260711_light_agent_runtime.sql` after the
workflow-runner migration before starting this version.
