# light-gateway

## Agent delegation

Set `LIGHT_GATEWAY_AGENT_DELEGATION_SECRET` to the same 32-byte-or-longer secret
used by `light-agent`. Delegated MCP requests are signature checked, audience,
expiry, policy, data-boundary, turn/action, stable-tool, alias, and replay bound,
then intersected with the gateway's current access-control and tool catalog.
Delegation does not bypass normal gateway authorization or response filtering.
Workflow-backed MCP tools are excluded from agent delegation because their
execution contract requires the original end-user JWT to propagate through the
workflow and downstream API calls. X-Scope-Token-only, Basic-auth, and API-key
callers are likewise unsupported for workflow-backed tools; they may continue
to call non-workflow MCP tools.

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
