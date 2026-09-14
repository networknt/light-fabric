# A1 local issuer / Workflow broker profile

The profile is opt-in. It does not enable A2 action dispatch or admit personal
orchestration Phase 1. `local-profile.json` reserves a new broker client ID;
it is not one of the existing Workflow application clients.

## Prepare and activate

1. Install the normal `portal-db` release (fresh `ddl.sql` or
   `patch_20260913_01_workflow_broker.sql`) **before** the new OAuth image.
   Run `refresh-claims-preflight.sql` against the issuer schema. For clients
   using refresh, classify each remaining custom claim in
   `auth_refresh_claim_source_t` with its approved live user-attribute source or
   administrator-owned app metadata source. Unknown claims fail closed during
   renewal; do not classify permission-bearing claims as app metadata to bypass
   this check. The new broker registration deliberately has no custom claims.
2. Supply approved TLS material in a private input directory:
   `server.pem` / `server.key` for `light-oauth`, `client-ca.pem` for the broker
   workload trust, `client-identity.pem` containing its certificate and key,
   `issuer-ca.pem` trusting both ordinary and broker issuer endpoints, and
   `callback.pem` / `callback.key` for the browser callback hostname.
   The client certificate URI SAN must equal the manifest's `sanUri`.
   Never use checked-in application bearer fixtures as this identity.
3. Put the dedicated database URL in input `database-url` (mode 0600): database
   `workflow_credentials`, user `workflow_broker_runtime`, a newly generated
   password, and the deployment's PostgreSQL hostname/TLS policy. The database
   is independent of `workflow_ops` and is dedicated to this one broker client.
   Startup rejects existing credential records without a matching recorded issuer/
   client binding; never delete that binding to repurpose a store.
4. Run `python3 prepare.py --manifest local-profile.json --input PRIVATE_INPUT
   --output NEW_PRIVATE_OUTPUT`. Parent directory must already exist. Output
   contains secrets; keep it outside Git and artifacts. Reusing an output path
   fails rather than resetting keys. Review the non-secret manifest and SQL.
5. Apply `storage-bootstrap.sql` using a PostgreSQL administrator connection
   to the database server named in `database-url`. Apply `register.sql` to the
   issuer database with its selected schema `search_path`. Both are replayable;
   existing inactive/conflicting registrations fail rather than revive.
   Test a runtime connection with the supplied URL before activation. The
   runtime role must not be a member of the schema owner or operational roles.
6. Import the catalog events in `light-portal-event/config/20260913-workflow-broker`.
   Set `workflow.credentialBroker` on the verified Workflow instance to the
   exact contents of `workflow-settings.json` using Portal's normal config
   update event/command. No secret values belong in that property. Publish a
   new snapshot with the existing distribution publication command, explicitly
   setting `LIGHT_WORKFLOW_CONFIG_INSTANCE_ID` from the manifest. Its legacy
   default does not identify the currently active local instance.
7. Set `WORKFLOW_BROKER_DIR` to the absolute prepared directory and use
   `compose.yml` alongside the distribution's normal Compose file. Pin the
   locally built image digests and record snapshot IDs in the deployment
   manifest before qualification. The dedicated broker port is internal only;
   the browser callback binds host loopback port 8447. Its certificate must be
   trusted by the browser. Official environments supply their approved hostname,
   ingress and PKI rather than copying the local hostname or development keys.

Before starting the containers, run `docker run --rm --network none --entrypoint
id PINNED_IMAGE` for each image. With the needed filesystem privilege, run
`python3 set-ownership.py NEW_PRIVATE_OUTPUT --issuer-owner UID:GID
--workflow-owner UID:GID` using those measured IDs (both cached local images
currently use `999:999`). The helper changes only the component mount trees,
keeps directories 0700 and files 0600, and records the owners in the manifest.
Verify that each image can read its own mount using its default non-root user.
Do not solve unreadable mounts by running services as root or widening file
permissions. Certificate/key replacement must preserve this ownership.

Start enrollment through `POST /workflow/credentials/enroll` with the original
user token and the allowed caller app token. Open the returned `authorizationUrl`.
The issuer shows the tenant, scope, expiry and work binding; password and explicit
consent produce a PKCE-bound redirect to Workflow. The callback returns only a
grant reference. Scheduled execution retains no browser token or password.

## Rotation and rollback

For a certificate rollover, register the replacement fingerprint with the same
approved URI SAN while the old certificate remains active. Replace the private
client identity atomically and restart Workflow (configuration changes require
restart). Verify grant lookup and renewal through actual mTLS, then set the old
fingerprint's `active=false`. Do not delete its row: provisioning cannot revive
a retired fingerprint. To revoke the workload immediately, deactivate its broker
registration or client; every endpoint checks these flags. Updating the trust
bundle alone does not retire an already registered certificate.

For encryption-key rotation, append a fresh 32-byte base64 key to `keys` in the
private keyring and set `activeKeyId`; retain old keys until no stored enrollment
or grant uses them. Restart and verify successful renewal rewrites `key_id`.
Never regenerate the keyring to recover a lost refresh response: strict rotation
requires user reauthorization. Restore encrypted storage and its keyring together.

To disable the profile, revoke active issuer grants, disable the broker
registration, publish `workflow.credentialBroker: null`, then remove the overlay
and restart. Keep the schema, revocation history, and encrypted backups according
to retention policy. Do not roll back issuer tables while running the new binary.

Deployment activation is distinct from source qualification. Keep image digests,
snapshot versions, certificate fingerprints (never private keys), migration
receipts and test evidence with the non-secret deployment manifest.

### Availability review updates

Apply the updated `credential_broker.sql` to preserved credential stores before
starting the updated Workflow image: it adds the durable `NOT_SENT` renewal outcome.
This is separate from the issuer schema requirement: **patch the issuer database
before starting its new image**.

`workflow-settings.json` accepts `legacyLongLivedAppKeys`, defaulting to `[]`.
For the approved local development fixture only, add entries of the form
`{"issuer":"<verified local issuer>","kid":"<separate long-lived app key ID>"}`
to the input manifest before preparation. Verify both values against the issuer's
registered long-lived key; do not allow the shared user/short-lived signing key.
The exception applies only to `X-Scope-Token`, still requires a valid signature,
expiry and allowed caller service ID, and never authenticates `Authorization`.
Official environments should keep the list empty and use a marked app token.
Qualify enrollment through the actual Gateway with the selected setting before
admitting unattended workflows.

Renewals return a retryable result only when the token request was not sent
(connection establishment failed, or verification keys could not be obtained
before the request). The recorded renewal becomes `NOT_SENT` and the same live
owner restores `ACTIVE`; a later attempt may reuse the unchanged refresh token.
Responses lost after sending remain uncertain. JWKS are cached for five minutes,
loaded before rotation, and refreshed once for an unknown key ID. Issuers must
publish new keys before signing with them; failed verification after a committed
rotation still requires reauthorization. Canceling a run denies that run's token
without discarding a valid shared-grant rotation. A grant revocation or expired
renewal-owner fence still prevents saving it.
