# Agent LLM dual-token Phase 4 rollout and qualification

Policy lifetime update: Agent policies and derived gateway assignments now remain
effective until replaced or revoked through an applied update. Mandatory policy
lease renewal is removed; authentication tokens still expire normally. See
[Policy lifetime](llm-dual-token-authorization.md#policy-lifetime) for offline
files, cached startup, compatibility, and deployment requirements. This source
change is not proof that an existing container image includes it.

Status: implementation and qualification tooling prepared. The default deployment
target is `portal-config-loc/all-in-lt`; the live `/app/genai/chat` exit check
remains pending. A passing
implementation gate or direct gateway probe is not Phase 4 completion.

## Local deployment target

Use `/home/steve/workspace/portal-config-loc/all-in-lt` by default. Preserve the
running Compose project's personal-runner and credentials overlays and its release
and private environment files when updating individual services.

The UI entry point is `https://localhost:3000`; use
`https://localhost:3000/app/genai/chat` for browser qualification.

Preflight on 2026-09-07 found the three deployed Agent definitions using the
public `assistant-dev` alias, with no published dual-token policy. Qualification
requires an internal alias bound to the selected Agent through normal publication.
The mounted OAuth and LLM gateway certificates cover `localhost` and selected IP
addresses, but not their internal service DNS names; use validated endpoint names
or issue suitable certificates before enabling hostname verification. The earlier
browser certificate failure at `https://local.lightapi.net` was at the wrong UI
entry point and does not establish a blocker for `https://localhost:3000`.
Login at the correct URL succeeded. The baseline Tech Support chat reproduced
the reported gateway 403 before policy activation. This is baseline evidence,
not a passing dual-token qualification.

The local `llm_audit` database has now received migration
`0006_authorization_context.sql` in one transaction; the JSONB column and its
constraint were verified afterward. This additive migration does not enable the
profile or constitute persisted dual-token audit evidence.

Local deployment now uses `portal-config-loc/all-in-lt/docker-compose.yml`;
the temporary dual-token overlay has been retired. Public certificates reside in
each service's `config/cert.pem`. The separate renewable OAuth workload credential
remains in the external Tech Support credential volume.
Event artifacts and historical qualification notes are retained in
`light-portal-event/genai/20260908-agent-llm-dual-token/`. The historical notes are
not current deployment instructions. No bearer credentials belong in that record.

## Remaining implementation

The normal `light-oauth` client-credentials grant now derives `host` from the
authenticated OAuth registration and reads only `env`/`environment`, `routeAlias`,
and `billingSubject` from its registered `custom_claim` JSON. Conflicting host or
environment values and invalid workload field types fail closed. The grant does
not accept form fields as workload identity, and does not project custom user IDs,
roles, audiences, subjects, expiry, or scope overrides. Existing scope validation,
issuer-wide audience, RSA signature, and 600-second lifetime remain authoritative.

Registrations without custom claims now include their authoritative host; no
additional environment or alias is invented. Existing deployments must author
those claims through the normal OAuth-client administration path before enabling
the Agent profile. Malformed registered custom JSON, previously ignored by this
grant, now causes issuance to fail rather than producing a partial identity.

The dual-token generation profile now returns `model_not_found` (404) for a request
outside its assigned route alias, matching the unknown-model response. The legacy
single-token signed-route restriction retains its existing 403 response.

The Portal gateway candidate compiler now promotes every generation alias to
`local_durable` when it emits a non-null delegation profile, including a retained
profile with no remaining bindings. This matches the gateway's startup invariant.
Embedding-only aliases and publications without the profile retain their existing
audit mode. The source route resources are not mutated. The publisher regression
suite passed 43 tests with no failures or skips on 2026-09-08.

Live publication exposed a second omission: the event consumer's managed-property
allowlist rejected `agentDelegation`. The projection now accepts that property
while retaining its metadata checks. Fresh-publication and replay tests include
the ninth property and its ownership write; all 43 targeted tests pass. The local
Portal images tagged `2.3.5-local.dualtoken.20260908.3` contain this correction.
The original failed event must be recovered through the independently approved
replay workflow; deployment of the fix alone does not resolve its DLQ entry.

## Ordered rollout

1. Record exact image digests and current published snapshot IDs for the issuer,
   gateway, Agent, UI, and Portal publisher. Retain the previous compatible policy
   revisions for rollback. Use the selected environment's normal deployment and
   publication process; do not activate by editing generated snapshot values.
2. Deploy the compatible issuer and Portal publisher. Register the optional
   `agent-policy-authoring.gatewayDelegation` authoring map and gateway
   `llm-router.agentDelegation` metadata if absent. Assign the OAuth client to the
   Agent definition in the same host, with an active provider-client registration.
   Author the environment and any signed alias/billing claims on that registration.
3. Apply audit migrations through `0006_authorization_context.sql`. Provision a
   persistent writable WAL and PostgreSQL sink. Deploy the compatible gateway
   before Agent forwarding. Keep generation aliases on `local_durable` audit.
4. Obtain normal client-credentials tokens through the configured TLS endpoint
   and mounted secret. Verify the declared issuer/audience for both user and
   workload tokens. The issuer's audience remains issuer-wide; the request form
   cannot select it. Missing or unsuitable claims block activation. Never enable
   expiry bypass or disable CA/hostname checking to make the test succeed.
5. Deploy the compatible Agent and UI. Mount the acquisition secret at the
   published `/run/secrets/...` reference with restrictive permissions. Publish
   the immutable dual-token Agent contract and its derived gateway projection
   through the existing candidate/validation/apply workflow. The projection must
   resolve the client to the Agent definition and agree with the existing internal
   alias `boundPrincipal`. Configure the selected inference route to require both
   credentials for the restricted-path qualification.
6. Run gateway admission probes, followed by the browser checks below. Capture
   audit and publication identifiers; do not capture bearer values, cookies,
   client secrets, or a raw authenticated network trace in the report.

## Repeatable implementation gate

Set `LIGHT_OAUTH_TEST_DATABASE_URL` to a disposable PostgreSQL database and run:

```bash
./scripts/run-agent-llm-phase4-gates.sh implementation
```

The issuer integration test uses connection-local temporary tables, signs a real
RSA JWT through the production grant handler, verifies its signature and claims,
and proves that injected form host/environment/role/alias values do not override
registered authority. It explicitly runs with `--ignored`; absence of the database
is an error. The gate also runs issuer unit tests, gateway data-plane regressions,
qualification-runner tests, and documentation/diff checks. These do not deploy
services and produce only `IMPLEMENTATION_CHECKS_PASSED`.

## Live gateway probe

`scripts/agent-llm/qualify_gateway.py` requires a trusted CA and an explicit HTTPS
chat-completions endpoint. It refuses redirects and reads tokens only from
owner-only files. Configure libpq `PGHOST`, `PGDATABASE`, `PGUSER`, and
`PGPASSFILE` for read access to the gateway audit database. Credentials must not
appear in command arguments or checked-in config.

Supply a public expectations JSON object with:

- `endpoint`: the deployed HTTPS `/v1/chat/completions` URL.
- `alias`: the internal alias assigned to this Agent.
- `unassignedAlias`: an existing alias assigned to another Agent.
- `userId`, `workloadClientId`, `agentDefId`, `hostId`: expected verified UUIDs.
- `agentPolicyDigest`: the active Agent content/policy evidence digest from the
  gateway's derived binding projection, not an assumed local source revision.
- `requireWorkload`: `true`, matching trusted route configuration.

Run directly with `--config`, `--user-token-file`, `--workload-token-file`,
`--ca-file`, and `--report`, or use `run-agent-llm-phase4-gates.sh live-gateway`
with the corresponding `AGENT_LLM_*` variables documented in that script.

The probe performs one small inference and four denials: missing workload,
invalid workload, another Agent's alias, and an unknown alias. It requires
persisted terminal audit records for all five, checks successful user/Agent/model/
policy attribution, and rejects any denied case with a provider-attempt record.
Reports contain request/correlation identifiers and status only. Starting a run
invalidates an older pass at the report path. The result is explicitly
`GATEWAY_ADMISSION_PASSED` with `browserQualification: NOT_RUN`.

## Browser exit evidence

Session admission must permit a forward publication transition for the same
active runtime scope. The Agent uses existing accepted `AGENT_POLICY` reference
evidence to establish the previous policy version and requires a strictly greater
incoming version before replacing the scope's publication and content digest.
Host, instance, service, environment and audience remain bound; missing,
inconsistent or revoked baseline evidence fails closed. Scope changes, new policy
evidence and session creation commit in the same database transaction. Existing
sessions retain their original pinned authority and are not silently migrated.

The local OAuth leaf certificate must include `light-oauth` because the Agent
fetches JWT keys through that internal DNS name. A healthy Agent listener alone
does not prove that authenticated WebSocket admission works. The normal
`all-in-lt/docker-compose.yml` mounts the DNS-valid OAuth certificate as well as
the Config Server and LLM gateway certificates; no TLS bypass is required.

Use the deployed `/app/genai/chat` page under an authorized user account. Select
the intended deployed Agent, submit a small uniquely identifiable turn, and
confirm a model response. Correlate that time window and verified user/Agent with
the gateway's PostgreSQL audit and the Agent's durable turn record. Record the
actual user, client, Agent, alias, registration version, Agent policy digest,
gateway snapshot revision, and durable request/turn IDs.

Repeat with a denied user or an incompatible assignment; no provider attempt may
occur. Restore the compatible assignment through normal publication and verify
the next authorized admission. Exercise user expiry, HTTP cookie renewal and
same-owner reconnect without resubmitting accepted turns. Exercise workload
renewal across expiry, temporary acquisition failure, recovery and mounted-secret
rotation. Inspect retained audit delivery after a sink outage. Use an isolated
qualification deployment for destructive outage/rotation exercises.

Only mark Phase 4 complete after this deployed UI-to-Agent-to-gateway response
and persisted audit evidence exist. Do not substitute mock-provider unit results,
a direct curl response, decoded but unverified JWT claims, or a manually edited
report for that exit evidence.

## Rollback

Keep the compatible gateway enforcing the active profile while reverting an
Agent or UI change. Disable affected traffic before restoring a gateway version
that cannot enforce the active policy. Revert compatible publication and binary
versions together; never remove `agentDelegation` merely to bypass a denial.
Drain the audit WAL using the new consumer before any rollback to an older audit
consumer that would discard dual-token identity fields. Preserve database audit
history and additive migrations. Requalify both allowed and denied admissions
before restoring traffic.
