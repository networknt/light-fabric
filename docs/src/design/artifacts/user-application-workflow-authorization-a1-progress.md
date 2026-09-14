# A1 Implementation And Qualification Status

Status: **A1 source implementation complete; selected-stack acceptance pending**.

The implementation follows the frozen [A0 baseline](../user-application-workflow-authorization-a0.md).
It does not admit personal orchestration Phase 1: production receiver enforcement
and dispatch are A2/A3, and live deployment qualification remains separate.

## Implemented

- **Issuer:** dedicated, reserved `JwtClaims.token_use`; live tenant-bound refresh
  authority; explicit custom-claim sources; PKCE and browser consent; dedicated
  certificate-authenticated broker listener; strict rotation, consumed-token
  history, grant lookup and revocation. Broker clients cannot use secret-only
  authentication through another provider binding. Specialized non-broker grants
  retain their supported behavior.
- **Recovery:** revocation tombstones serialize with enrollment and redemption,
  including when revocation arrives before the grant exists. Uncertain refreshes
  require reauthorization; the broker never retrieves a replacement through
  interactive retry grace.
- **Workflow:** encrypted credential storage, a store-to-issuer/client binding,
  durable enrollment and renewal ownership, run binding, key rotation, restart
  recovery, cancellation/revocation fencing, and retryable issuer revocation.
  Enrollment APIs return references and an authorization URL, never refresh
  credentials. The browser callback has a separate optional TLS listener and
  needs stored OAuth state plus backend PKCE, not the original browser JWT.
- **Shared client/verifiers:** fixed HTTPS endpoints, explicit CA trust, mTLS,
  bounded responses, signed-token validation and disabled redirects/retries;
  user/app purpose validation including duplicate-marker rejection and the
  explicit app-only legacy-key exception. Receiver-wide wiring remains A2.
- **Deployment:** the canonical Portal patch and fresh schema, regenerated
  bootstrap SQL for both distributions, a separate credential database/runtime
  principal, private mount preparation and ownership, registration/replay checks,
  certificate/key rotation procedures, opt-in Compose overlays, and a null-default
  Portal configuration catalog delta. The OAuth image build now includes the
  complete external Cargo path-dependency manifests needed by A1.

The issuer schema belongs to `portal-db/postgres/patch_20260913_01_workflow_broker.sql`
and its canonical `schema/base.sql` / generated `ddl.sql`. The SQL under
`portal-service/apps/light-oauth/migrations` is only an isolated-test fixture.
The patch includes client relationship policies; runtime authentication rejects
inactive clients and provider bindings even though their configuration is retained.
Both distributions' checked-in `postgres-db/init.sql` now include the migration.
Install the patch before starting the new issuer binary on a preserved database.

The Workflow credential schema is
`light-fabric/apps/light-workflow/migrations/credential_broker.sql`.
Provisioning assets live in `light-fabric/deployment/workflow-broker` and are
synchronized to `portal-config-loc/all-in-lt/workflow-broker` and
`light-portal-install/workflow-broker`. Catalog events live in
`light-portal-event/config/20260913-workflow-broker`; they have not been imported.

## Qualification

[Machine-readable results and local image IDs](authorization-a1/qualification.json)
and [working-tree source hashes](authorization-a1/source-sha256.json) identify the
earlier qualified artifacts. The image IDs are local builds, not published registry
receipts, and predate the availability fixes below. Rebuild and requalify the issuer
image before deployment.

| Check | Result |
|---|---|
| OAuth unit suite | 21 passed; its two database tests were also explicitly run |
| Workflow library suite | 80 passed |
| Shared security and purpose suites | 17 and 2 passed |
| Existing workload grant compatibility | Passed against isolated PostgreSQL |
| Live authority | Passed: tenant pinning, removed membership and locked users |
| Complete consent flow | Passed over real HTTPS: consent form, PKCE redemption and production callback listener |
| Purpose and ceilings | App token with user claims rejected; scope/duration expansion rejected; purpose override filtering verified |
| Interactive refresh grace | Passed: removed role/group/position and changed attribute appear in the next token; account/session/membership revocation and failed live queries reject renewal |
| Strict issuer concurrency | Exactly one committed rotation; authenticated duplicate revokes the family; wrong-client reuse does not revoke it |
| Broker concurrency/recovery | Passed: concurrent renewals and callbacks, process replacement, key-file rotation and store/client mismatch rejection |
| Certificate lifecycle | Actual mTLS renewal/retirement, missing/wrong peer and secret-only rejection passed |
| Failure injection | Lost committed HTTP response, issuer outage, revocation retry and post-rotation persistence failure passed without refresh replay |
| In-flight fencing | Response delayed after issuer commit is rejected after run cancellation or recovery fencing |
| Enrollment revocation | Revocation before creation and before delayed code redemption prevents resurrection |
| Canonical schema | PostgreSQL 17.10 fresh/upgrade/replay, cascade policy and deterministic regeneration gates passed |
| Distribution bootstrap | Exact regenerated installer SQL loaded successfully; both distributions use identical canonical bootstrap SQL |
| Private provisioning | Credential database installation/replay and runtime role separation passed; registration replay cannot revive a retired certificate |
| Mounts | Both Compose overlays validated; cached non-root images can read only their own prepared test mounts |
| Two-hour renewal soak | Stopped at user request; manual qualification pending. Final-source run logged six successful rotations through 3,301 seconds; this is not a completed two-hour gate |
| Qualification images | Both separate A1 tags built; OAuth startup/mTLS smoke passed on the disposable schema; no `latest` replacement or live restart |

The HTTP tests use the real Workflow broker and local issuer together, with
PostgreSQL and actual TLS handshakes. Faults are injected around transport delivery
or database persistence, rather than replacing renewal with a mock success.
These are isolated integration tests, not a claim that the live Portal frontend
and deployed configuration have been qualified.

Reproduce the issuer/Workflow integration gates with:

```bash
portal-service/apps/light-oauth/scripts/run-a1-gates.sh DATABASE_URL_FILE EVIDENCE_DIRECTORY
# Deferred to manual testing: the two-hour scheduled/long-run test:
portal-service/apps/light-oauth/scripts/run-a1-gates.sh DATABASE_URL_FILE EVIDENCE_DIRECTORY --soak
```

The database must be the disposable `oauth_a1_qualification` database with a
schema-only `configserver` fixture. Credentials are supplied privately. The script
records base revisions and actual working-tree source hashes and rejects source
drift during qualification. Do not confuse the base Git revision with the
uncommitted A1 implementation. Remote GitHub CI has not been run.

## Availability Review Follow-up

Both portal-service availability findings are fixed:

- The broker listener starts TLS handshakes and certificate extraction in separate
  tasks, capped at 128 pending handshakes with a five-second timeout per task.
  Silent peers do not serialize acceptance. A real mTLS regression keeps four
  earlier TCP connections idle while an authenticated request completes.
- Broker redemption and renewal use their existing transaction connection for
  session authority, live claims, custom-claim sources and signing-key reads.
  They never acquire another pool connection while holding rotation locks.
  Twelve concurrent redemptions, then twelve concurrent renewals, complete with
  a one-connection pool alongside ordinary issuance. Strict reuse detection is
  preserved.

The complete short gate suite passed again on a fresh isolated PostgreSQL database:
21 OAuth unit tests, explicit live-authority and workload-compatibility tests, and
real HTTPS/mTLS broker integration including the new contention regression.
[Source hashes for this follow-up](authorization-a1/availability-source-sha256.json)
identify this tested revision. The earlier image receipts and partial soak describe
older source; no images or soak were rebuilt/rerun for this follow-up. The deployed
scheduled and hours-long runs remain pending, and the A1 exit gate has not passed.
The subsequent Workflow/client review below refines pre-request error classification.

## Workflow And Client Review Follow-up

All five light-fabric findings are addressed:

- Periodic recovery logs store failures and retries; it no longer terminates the
  managed task and closes unrelated Workflow admission. Startup configuration
  validation remains strict.
- Connection refusal, TLS establishment failure and pre-request JWKS failures
  return `NotSent`. The same live owner atomically records `NOT_SENT` and restores
  `ACTIVE` without changing the token or generation. Later caller attempts may
  retry. Post-send failures and lost ownership still fence the grant.
- Verification keys are cached for five minutes before rotation, with one refetch
  for an unknown key ID. A post-rotation verification failure remains uncertain;
  issuers must publish new keys before using them.
- Canceling one run preserves a valid committed shared-grant rotation and denies
  only that run's token. A sibling run can renew; grant revocation and owner
  fencing still reject late responses.
- `credentialBroker.legacyLongLivedAppKeys` explicitly configures approved local
  issuer/key pairs for markerless app fixtures. It defaults empty, applies only
  to `X-Scope-Token`, and cannot override explicit invalid purpose markers or
  authenticate a user. Both distribution preparation scripts carry the setting.

Short gates passed on a disposable PostgreSQL database with actual HTTPS/mTLS:
21 OAuth unit tests, explicit live-authority/workload tests, full broker integration,
80 Workflow library tests, 20 light-client tests, 17 security tests, two purpose
contract tests, and two preparation tests. The integration exercises refused and
failed-TLS connections without token POSTs, JWKS outage/rollover, post-rotation
verification failure, recovery store failure, sibling-run cancellation, and the
actual API legacy-key allowlist. [Source hashes](authorization-a1/fabric-review-source-sha256.json)
identify this follow-up. No soak or deployed-stack acceptance was run. Both
previous image receipts predate these fixes and require rebuilding/requalification.
Apply the updated credential-store SQL (the `NOT_SENT` result constraint) before
starting the updated Workflow image, as well as patching the issuer database
before its image. No live database or service was changed.

## Local Database Migration Applied

The local `all-in-lt` PostgreSQL instance now has the issuer patch in
`configserver.configserver` and the credential migration in the newly provisioned
`workflow_credentials.workflow_secret`. All seven issuer tables, five credential
tables, the `NOT_SENT` constraint, runtime password authentication and restricted
role privileges were verified. Cascade validation passes after repairing three
stale A2A schema references and adding four missing canonical gateway policies.
[Migration receipt](authorization-a1/local-migration-receipt.json) records hashes
and checks without secrets. The runtime URL is in the ignored mode-0600 file
`portal-config-loc/all-in-lt/postgres-db/secrets/workflow-broker-database-url`;
use it as the private `database-url` input during broker preparation.

This supersedes earlier statements that no live database was changed. No images
were rebuilt and no application services were restarted. Broker registration,
certificates, configuration activation and selected-stack qualification remain
pending; the broker profile is not enabled by this migration alone.

## Local Broker Activated

The user rebuilt the issuer and Workflow images. The local broker is now enabled
with a dedicated private client CA and a registered 180-day client certificate.
The existing local issuer HTTPS certificate is used for its internal server and
localhost callback. Private mounts remain ignored by Git, directories 0700 and
files 0600, owned by the measured service UID/GID 999:999. No user grant was created.

The catalog and instance configuration were imported through event-importer using
the registered `ConfigInstanceCreatedEvent` with `commandkind: MUTATION` for the
instance property. Workflow snapshot `04c798da-c95b-499f-9167-a86bd248a145` is active.
The broker JWKS URL uses `light-oauth:6881` inside Docker; its authorization URL
uses `localhost:6881` for the browser. The local legacy app exception is restricted
to the verified LC signing key. OAuth and Workflow were recreated and are healthy
with zero restarts. Registered mTLS reaches grant lookup; no certificate fails TLS,
wrong client and secret-only broker authentication return 401. JWKS returns 200.
The callback returns 400 without state/code over verified HTTPS.

[Activation receipt](authorization-a1/local-activation-receipt.json) records image
IDs, snapshot IDs and certificate fingerprint. `deploy-local.sh lt` includes the
broker overlay when the ignored `workflow-broker/.runtime/enabled` marker exists.
The host system trust store does not trust the local issuer CA; explicit CA-file
verification passes. Browser trust must be established before consent if absent.

This activation supersedes the historical no-import/no-restart statements above.
The live preflight returned nine unclassified names on four clients: Support
Triage Local Demo, Tech Support LLM Workload Dev, mcp379-local-qualification and
pylon. No claim-source classifications were changed. The broker has no custom
claims, so these do not block its registration or activation.

## Remaining For Selected-Stack Acceptance

1. Review custom-claim sources for existing clients that use refresh. In particular,
   do not classify pylon's roles/userId as static metadata without authority review.
2. Use a newly issued user token to start enrollment through the actual Gateway,
   then complete browser login and explicit consent. Verify browser trust first.
   No dedicated enrollment page exists in portal-view yet; the API returns the
   issuer authorization URL. Reissuing other pre-marker tokens remains an A2
   receiver-enforcement prerequisite.
3. Run the scheduled and full hours-long renewal qualification on this deployed
   stack, after browser access-token expiry, and record reliability metrics with
   injected response loss separate from ordinary load.
4. Assess A1 only after those results. A2/A3 and orchestration admission remain
   separate. This activation is not an A1 exit-gate pass.

Changes remain uncommitted.
