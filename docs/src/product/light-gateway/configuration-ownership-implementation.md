# Configuration ownership implementation and qualification

The four delivery phases are implemented and locally qualified on 2026-09-08.
The deployment gate defaults to disabled for other installations until their
migration and release/restore qualification complete. See the [design](control-plane-configuration-ownership.md).

## Authoring contracts

| Aggregate | Table | Stream subject |
|---|---|---|
| LlmGatewaySecurityProfile | llm_gateway_security_profile_t | hostId / securityProfileId |
| LlmGatewayDelegationPolicy | llm_gateway_delegation_policy_t | hostId / instanceId / llm-gateway-delegation-policy |
| LlmGatewayOwnershipRelease | llm_gateway_ownership_release_t | hostId / ownershipReleaseId |

Stream components use the existing Portal pipe delimiter. Security profiles are
unique by host and logical environment, with immutable environment identity.
They contain userIssuer, userAudience, and schemaVersion 1. Instance policies
reference securityProfileId and contain explicit endpoint booleans for all three
supported inference routes. Updates require the observed aggregateVersion.

The genai service exposes createLlmGatewaySecurityProfile,
updateLlmGatewaySecurityProfile, createLlmGatewayDelegationPolicy,
updateLlmGatewayDelegationPolicy, and releaseLlmGatewayOwnership at version 0.1.0.
The corresponding list queries are getLlmGatewaySecurityProfile and
getLlmGatewayDelegationPolicy; getFreshLlmGatewayDelegationPolicy selects an
instance, and getLlmGatewayOwnershipState returns current managed ownership.

Publication requires the preview's expectedPropertySetDigest and
expectedSourceDigest. The instance-properties-v2 manifest includes sourceDigest
and propertySetDigest. Historical v1 manifests and digests remain unchanged.
Replay policy v4 registers the new event types while retaining registries v1–v3.

## Rollout

Apply portal-db/postgres/patch_20260908_llm_configuration_ownership.sql before
deploying the command/query services. LLM_MANAGED_CONFIGURATION_ENABLED defaults
to false. Deploy the ownership-release command with the guard, and enable the
guard only after migration and release/restore qualification.

Run `portal-db/postgres/tests/llm_configuration_ownership_inventory.sql` against
the target Portal schema first. It reports accepted material, conflicts, authoring
versions, and generic event-stream heads. Bootstrap only reconciled records;
unmanaged or conflicting values require explicit operator review.

Bootstrap authoring explicitly from each instance's last accepted profile,
preserving endpoint requirements. Save the environment security profile before
the instance policy. Normal compilation does not read generated JSON as authoring.
An instance without an authoritative policy cannot publish.

Release submits hostId, instanceId, and the expected instancePublicationId. The
server emits a release event and generic ConfigInstance baseline events, verifies
ownership inside the append transaction, and deactivates ownership atomically.
The current runtime snapshot is unaffected.

## Snapshot contracts

The snapshot root accepts llmConfigurationMode with managed (default) or detached.
Managed mode exports current ownership as metadata folded into publication
restoration, excludes duplicate generic property events, and rejects conflicting
legacy values. Historical applications use restoreOnly so they do not reactivate
old ownership. Managed cross-host restore is rejected because rebinding would
change host-bound immutable material.

Detached mode omits publication/authoring state and produces generic configuration.
Import into a managed target prepares the explicit release and baseline events
in the same append batch, with transactional stale-ownership validation.

## Qualification

Run LlmGatewayOwnershipPostgresTest with LLM_OWNERSHIP_TEST_JDBC_URL pointing at an
isolated PostgreSQL database. The disposable test database uses test-only
postgres credentials; the test creates and removes its own schemas. Qualification
requires zero skipped tests.

After publication, snapshot activation, and gateway reload, run make llm and
make genai-chat-ui in light-portal-test. Verify durable audit rows and rollback
separately. Unit and projection tests alone do not establish runtime activation.

## Local qualification evidence

The local `llm-gateway` instance is `391447f5-fb0d-5c80-92a7-a8d98f6d07c7`
in logical environment `dev`. Its security profile is
`01a08267-912b-7816-86e1-7e0118fca834`. The bootstrap compared the current generated
profile with its owning accepted publication and retained the publication ID and
profile digest in migration provenance. Endpoint policy version 4 permits direct
user inference on all three supported routes. Agent bindings remain derived.

Verified through authenticated Portal commands and queries:

- Required → optional → required generated identical final property bytes but
  distinct source fingerprints and immutable revision IDs. A stale preview was
  rejected before publication.
- Release emitted nine generic baselines, left no active ownership rows, and
  reconciled every property version with its event stream. A generic edit then
  succeeded; explicit publication reclaimed ownership. With the guard enabled,
  another generic write returned `409 LLM_CONFIGURATION_MANAGED`.
- Publication, snapshot activation, and reload with required chat authentication
  returned HTTP 401 to a valid user-only request. Exact rollback to the stored
  optional revision returned HTTP 200. Invalid supplied workload tokens returned
  HTTP 401 in optional mode. Durable audit records identify
  `workload_token_required` and `invalid_workload_token` respectively.
- `make llm` passed both buffered and streaming requests. `make genai-chat-ui`
  passed a newly accepted turn and a unique response marker through Tech Support.
  The browser test used a separate qualification session file because the older
  saved session no longer matched the running Agent's durable session ownership.
- The final snapshot contains all nine publication values, including the current
  Agent binding. Database audit rows contain successful direct-user and Agent
  requests, user attribution, and workload attribution for delegated calls.

The local services use the `ownership-20260908` image tags. The local-only Compose
file `portal-config-loc/all-in-lt/.runtime/llm-ownership.compose.yml` selects those
images and enables the guard. Other deployments must set
`LLM_MANAGED_CONFIGURATION_ENABLED=true` only after their rollout checks.

Additional fixes proven necessary by qualification:

- The delegation-policy event subject has an explicit suffix so it cannot collide
  with the existing `hostId|instanceId` Instance event stream.
- Binding compilation follows `agentPolicy.policySnapshot.snapshotId` in the
  current configuration snapshot of the same Agent instance. It uses the instance's
  logical environment, allowing a different deployment tag, and excludes revoked
  policy evidence and inactive registrations.
- New inference requests receive independent UUID request IDs rather than reusing
  caller correlation headers. Older WAL records with non-UUID request IDs receive
  stable host/day-scoped UUIDv8 storage IDs. Their original IDs remain in
  `transport_context.legacyRequestId`; WAL records are not discarded or rewritten.
  This preserves the historical grouping by correlation ID; it cannot separate
  distinct old calls that reused the same ID. The retained local backlog drained
  successfully after deployment.

Regression coverage includes the actual exporter/converter/PostgreSQL restore,
historical applications arriving after current ownership, replay, conflicting
legacy snapshots, detached restore, authoring updates across a repeatable-read
snapshot, and append failure rolling back event/outbox writes and nonce reservation
with the publication. A generic payload cannot use restore metadata to bypass
the managed-write guard. Fourteen focused ownership/restore tests ran with zero
skips. Two PostgreSQL audit tests verify duplicate delivery and
legacy-ID recovery. UI tests exercise authoring versions, explicit endpoint flags,
manual instance selection, and release.

The full canonical DDL loads successfully in a disposable PostgreSQL database.
The repository-wide DDL documentation gate still rejects pre-existing historical
`ADD COLUMN` statements in `ddl.sql`; those statements also exist in `HEAD`.

## Review follow-up

The source fingerprint now uses only the selected compiler resource payloads and
source resource versions, together with the authoritative policy/profile versions
and derived Agent delegation. It excludes row audit metadata, unrelated catalog
rows, envelope sequence numbers, and synthetic next-publication versions. No
additional source-table scans run while the publication lock is held. Historical
publication manifests remain unchanged; refresh an outstanding preview after
upgrading the compiler.

Detached imports advance each property's stream version for every consumed event.
Only snapshot restore can supply a revision UUID; normal publication derives its
identity. Security profiles use an active-only unique environment index and a
16-character environment limit in SQL, request schemas, and backend validation.
The rerunnable migration upgrades the initial unique constraint. Existing values
longer than 16 characters must be reconciled before migration; they are not truncated.
The canonical definitions precede the dump trailer and include column comments.

The checked-in Compose default remains disabled intentionally. The running local
command service was independently inspected with the guard enabled; the earlier
live rejection qualification applies to that enabled configuration. A deployment
using only the disabled default cannot claim the managed-write rejection check.

Follow-up validation passed 54 focused persistence tests (including 11 PostgreSQL
ownership tests) and four request-schema tests, with no skips. The canonical DDL
loaded successfully in isolated PostgreSQL and the documentation build passed.
These review corrections have not yet been redeployed to the local services.
