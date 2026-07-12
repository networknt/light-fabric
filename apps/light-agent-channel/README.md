# Light Agent Channel

The first production adapter is Slack Events API v1. It exposes
`POST /channels/slack/events`, verifies Slack's `v0:{timestamp}:{rawBody}` HMAC
before parsing, resolves an administrator-created `slack-events-v1` binding,
and admits one durable channel turn per Slack `event_id`.

Required environment variables:

- `DATABASE_URL`
- `LIGHT_AGENT_CHANNEL_HOST_ID`
- `SLACK_SIGNING_SECRET`
- `LIGHT_AGENT_CONNECTOR_CREDENTIALS_FILE`

Optional: `LIGHT_AGENT_CHANNEL_ADDR` (default `0.0.0.0:8440`). Configure the
Slack request URL to the public HTTPS route ending in
`/channels/slack/events`. Bindings use `external_identity = <team_id>:<user_id>`
and list exact allowed Slack channel IDs in `allowed_destinations`.

Attachments remain rejected unless both `LIGHT_AGENT_ATTACHMENT_SCANNER_URL`
and `LIGHT_AGENT_ATTACHMENT_SCANNER_TOKEN` configure an HTTPS scanner. Slack
downloads are origin-restricted and redirect-free. Both declared-length and
chunked bodies are streamed through a 32 MiB per-file and 64 MiB per-message
hard ceiling, further narrowed by the binding; scanner receipts are streamed
through a 64 KiB ceiling. Only scanner-approved immutable references are placed
in the turn; bytes and Slack credentials are not.

Attachment messages enter a durable `PENDING` scan state. Replicas claim work
with `FOR UPDATE SKIP LOCKED`, a random claim token, and a five-minute lease.
Expired claims are recoverable; failures use bounded exponential retry and
become terminal after five claims. Clean per-file evidence is idempotently
reused only when its media type and admitted size still match. This prevents
duplicate downloads/scans during normal concurrency and preserves recovery
after a crash. Scan evidence remains durable in `agent_channel_attachment_t`.

Signed connector events use `POST /channels/connectors/events`. The untrusted
`triggerId` is used only to select an active trigger and its exact grant. Grant
selection requires an active `CONNECTOR` trigger and an unexpired, unrevoked,
use-bounded `agent_connector_grant_t` that allows `events.receive`. Its mapped
credential must contain at least 32 bytes and authenticates the exact raw
request before any envelope field is trusted. Schedule and connector triggers
are claimed with `SKIP_LOCKED`, have durable fire idempotency, enforce maximum
delay, destinations, quiet hours, revocation, and connector-use limits, and
create ordinary FIFO channel turns. Connector-driven trigger firing requires
the separate `triggers.fire` operation.

Outbound provider authentication is grant-specific. For Slack replies, create
a live `agent_connector_grant_t` with `connector_alias = 'slack-api-v1'`, an
`allowed_operations` array containing `chat.postMessage`, and an opaque
`credential_reference`. Include `files.download` when the binding permits
Slack attachment ingestion. `LIGHT_AGENT_CONNECTOR_CREDENTIALS_FILE` points to
an owner-only JSON map from that reference to an owner-only token file; start
from `config/connector-credentials.example.json`. The database cannot select a
file path or provide token bytes. Before every delivery, the service revalidates
the exact snapshotted grant, connector alias, operation, data-boundary digest,
expiry, revocation, and remaining uses, then consumes one use transactionally.
Missing or mismatched credentials fail closed without placing a token in a
message, turn, log, or database row. A revoked grant can be replaced by a new
live grant for the binding; pending messages remain pinned to their original
grant and therefore cannot silently switch identity.

Personal local effects use `agent_edge_runner_binding_t`. Light-Agent verifies
the principal, action allowlist, capabilities, compatibility digest, expiry,
and revocation before creating an `agent-action` request pinned to the exact
runner/backend. Controller refuses to reserve that request on another runner.
The edge executable is a fixed template and receives structured JSON, never an
inbound shell or connector credential.

Every allowed edge action also requires a server-owned `action_policies` entry
containing `stableToolRef`, `schemaDigest`, `schema`, `effectClass`, and
`approvalRequired`. Light-Agent recomputes the schema digest and validates the
arguments before enqueueing. Only `read-only` actions may dispatch without an
approval. `local-mutation` and `external-effect` actions consume the fresh
`READY` attempt created by an unexpired `agent_approval_t` whose subject,
arguments, policy, schema, and tool identity match exactly. Controller
revalidates that approval and the live edge binding immediately before lease
creation, so queued work fails closed after revocation or expiry.

Bot messages and message subtypes are ignored. Outbound delivery uses
`chat.postMessage`, respects quiet hours and revocation, retries independently
without rerunning the turn, and becomes terminal after ten failed attempts.
