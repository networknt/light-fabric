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

Attachments are rejected until the common scanner/broker is configured. Bot
messages and message subtypes are ignored. Outbound delivery uses
`chat.postMessage`, respects quiet hours and revocation, retries independently
without rerunning the turn, and becomes terminal after ten failed attempts.
