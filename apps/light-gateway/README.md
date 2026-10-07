# light-gateway

## Workflow delegation

`light-agent` no longer mints delegations. It forwards the caller's original
access token, which the gateway verifies as a normal JWT and evaluates through
access control like any other request. `LIGHT_GATEWAY_AGENT_DELEGATION_SECRET`
has been removed; remove it from deployments.

`light-workflow` still mints a delegation for nested tool calls. Set
`LIGHT_GATEWAY_WORKFLOW_DELEGATION_SECRET` to the same 32-byte-or-longer secret
it uses. Delegated MCP requests are signature checked, audience, expiry, policy,
data-boundary, turn/action, stable-tool, alias, and replay bound, then
intersected with the gateway's current access-control and tool catalog.
Delegation does not bypass normal gateway authorization or response filtering.
X-Scope-Token-only, Basic-auth, and API-key callers are unsupported for
workflow-backed tools; they may continue to call non-workflow MCP tools.

When delegation is enabled, configure
`LIGHT_GATEWAY_DELEGATION_DATABASE_URL` (or `DATABASE_URL`) for the shared
PostgreSQL replay ledger and apply
`portal-db/postgres/patch_20260711_10_agent_delegation_replay.sql`. Every gateway
replica atomically consumes the token replay ID from that ledger. Duplicate
consumption and database outages fail closed. `LIGHT_GATEWAY_INSTANCE_ID` is
optional audit metadata and otherwise defaults to the configured service ID.
Light-gateway in rust based on light-pingora

The gateway uses `light-runtime` for config-server bootstrap and controller
registration. Local defaults are under `config/`; config-server values and files
are cached into the runtime external config directory before Pingora starts.

## Operational evidence

`gateway-evidence.yml` controls the bounded, metadata-only request evidence
spool. It is disabled by default. A deployment that enables it must mount the
permission-restricted `operations_gateway_runtime` URL file and publish the
exact Host/environment binding ID and digest. The Gateway refuses a different
database, role, binding, Host, environment, or schema generation.

Authorization denials and rate-limit decisions are required audit evidence;
ordinary request completion records are optional traffic evidence. When the
bounded spool is full, optional traffic records are counted and dropped while
required evidence reports a high-severity failure. Records contain endpoint
templates, status, duration, byte counts, and one-way digests only. Headers,
credentials, prompts, request/response bodies, tool arguments, and application
task/session state are not representable in the record type.

The publisher supports an external HTTP ingestion endpoint and keeps failed
records durable for retry across Gateway restart. Redirects are disabled so a
configured sink cannot redirect evidence or its bearer token. The
`stdout://collector` endpoint is a development-only sink profile; production
must use an approved tenant telemetry or audit collector. `gateway_ops` is a
bounded delivery spool, not the long-term traffic warehouse.

### Upstream dispatch observation

`gateway-evidence.dispatchObservationEnabled` defaults to `false`. Configure it
through Config Server alongside the other evidence properties; it is not a
release-image setting and needs no additional per-service configuration file.
When `true`, durable evidence must also be enabled and the dispatch migrations
must be installed. The observer currently covers only GET issue and issue-comment
requests under `/github/repos/{owner}/{repo}/issues/{number}` and its `/comments`
suffix, not all Gateway endpoints.

The observer gives each covered request an audit identity before authentication
and ACL processing, then records start, upstream attempts, request handoffs and
completion. A complete denied lifecycle with zero attempts and zero handoffs
demonstrates that this request did not dispatch upstream. A 401/403 response on
its own does not establish that. An attempt can fail before handoff; handoff does
not establish that GitHub processed the request. Retries count as additional
attempts. Incomplete observation cannot prove non-dispatch.

This adds durable metadata writes and associated latency/storage cost; it does
not grant access or change routing. It exports no tokens, headers, request bodies,
response bodies or full repository/issue paths. Observation persistence failure
marks proof incomplete rather than introducing a new request denial policy.
Use it for scoped qualification or an explicit audit requirement; leave it off
when that evidence is unnecessary. Disabled observation does not disable the
separate general evidence facility or its existing required audit events.

Observer identity uses the automatically computed executable/configuration hashes
and process identity. It does not require an image digest property. Install
`0003_gateway_dispatch_image_optional.sql` after the existing observer migration
using the operational migration mechanism before running the updated observer.
The migration preserves all historical rows and permits older binaries to keep
writing their image values. Rolling back the binary does not require reversing
this additive schema change.

## Docker

Build a local image from the workspace root context:

```bash
./apps/light-gateway/build.sh 0.1.0 --local
```

Run with the local compose file:

```bash
cd apps/light-gateway
docker compose up --build
```

## Native Binary

Build the gateway binary from the `light-fabric` workspace:

```bash
cargo build --release -p light-gateway
```

Start it from this app directory with bootstrap and controller registration
settings supplied by environment or an env file:

```bash
cd apps/light-gateway
LIGHT_PORTAL_AUTHORIZATION="Bearer <token>" \
LIGHT_CONFIG_SERVER_URI="https://localhost:8435" \
LIGHT_GATEWAY_SERVICE_ID="com.networknt.light-gateway-1.0.0" \
LIGHT_GATEWAY_ENV="dev" \
STARTUP_BOOTSTRAPCACERTPATH="config/ca.pem" \
STARTUP_HOST="dev.lightapi.net" \
PORTAL_REGISTRY_URL="https://localhost:8438" \
SERVER_ADVERTISED_ADDRESS="127.0.0.1" \
./run.sh
```

Do not leave a blank line inside the continued command. A blank line after a
trailing `\` ends the first shell command, so `./run.sh` will not receive the
earlier environment variables.

`LIGHT_PORTAL_AUTHORIZATION` is the gateway service's single generic service
token. It is used for bootstrap/registration and is forwarded as
`X-Scope-Token` when the gateway invokes `light-workflow`; the original user's
JWT remains in `Authorization`. Upgrade `light-gateway`, `light-workflow`, and
the workflow user-authorization database patch together; mixed versions do not
share the same invocation authentication contract.

For repeated local runs, keep the token in an ignored env file:

```bash
cp light-gateway.env.example light-gateway.env
$EDITOR light-gateway.env
./run.sh
```

The launcher runs `target/release/light-gateway` by default, keeps the working
directory at `apps/light-gateway`, loads `config/`, writes downloaded
config-server files to `config-cache/`, and registers `server.advertisedAddress`
with controller.
