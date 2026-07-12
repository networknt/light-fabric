# Light Agent Channel

The first production adapter is Slack Events API v1. It exposes
`POST /channels/slack/events`, verifies Slack's `v0:{timestamp}:{rawBody}` HMAC
before parsing, resolves an administrator-created `slack-events-v1` binding,
and admits one durable channel turn per Slack `event_id`.

Required environment variables:

- `DATABASE_URL`
- `LIGHT_AGENT_CHANNEL_HOST_ID`
- `SLACK_SIGNING_SECRET`
- `SLACK_BOT_TOKEN`

Optional: `LIGHT_AGENT_CHANNEL_ADDR` (default `0.0.0.0:8440`). Configure the
Slack request URL to the public HTTPS route ending in
`/channels/slack/events`. Bindings use `external_identity = <team_id>:<user_id>`
and list exact allowed Slack channel IDs in `allowed_destinations`.

Attachments remain rejected unless both `LIGHT_AGENT_ATTACHMENT_SCANNER_URL`
and `LIGHT_AGENT_ATTACHMENT_SCANNER_TOKEN` configure an HTTPS scanner. Slack
downloads are origin-restricted, redirect-free, and size/digest checked. Only
scanner-approved immutable references are placed in the turn; bytes and Slack
credentials are not. Scan evidence is durable in `agent_channel_attachment_t`.

Signed connector events use `POST /channels/connectors/events` and
`LIGHT_AGENT_CONNECTOR_SIGNING_SECRET` (at least 32 bytes). They require an
active `CONNECTOR` trigger and an unexpired, unrevoked, use-bounded
`agent_connector_grant_t`; the credential reference is never loaded by this
service. Schedule and connector triggers are claimed with `SKIP LOCKED`, have
durable fire idempotency, enforce maximum delay, destinations, quiet hours,
revocation, and connector-use limits, and create ordinary FIFO channel turns.

Personal local effects use `agent_edge_runner_binding_t`. Light-Agent verifies
the principal, action allowlist, capabilities, compatibility digest, expiry,
and revocation before creating an `agent-action` request pinned to the exact
runner/backend. Controller refuses to reserve that request on another runner.
The edge executable is a fixed template and receives structured JSON, never an
inbound shell or connector credential.

Bot
messages and message subtypes are ignored. Outbound delivery uses
`chat.postMessage`, respects quiet hours and revocation, retries independently
without rerunning the turn, and becomes terminal after ten failed attempts.
