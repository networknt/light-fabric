# User, Application, And Workflow Authorization

Status: accepted design, September 13, 2026. The
[A0 contract baseline v1](user-application-workflow-authorization-a0.md) is frozen
after review and records the selected issuer profiles, contracts and migration
inventory. A1–A3
implementation qualification remains pending. This is the shared authorization
foundation for [issue #374](https://github.com/networknt/light-fabric/issues/374)
and a prerequisite for development workflow orchestration implementation.
Existing dual-token, OAuth, and workflow code provides foundations; the complete
unattended grant and renewal contract below is not yet qualified.

## Scope And Related Designs

Cover user and immediate-caller identity at API/MCP and Resource/Knowledge
boundaries, original-token forwarding, scheduled and long-running execution,
credential renewal, revocation, and replacement of workflow-issued `lad1`
delegation tokens. Apply the same contracts to personal and enterprise profiles.

[LLM User and Agent Authorization](../product/light-agent/llm-dual-token-authorization.md)
covers interactive model calls and explicitly excludes unattended delegation.
Its user/workload separation, model assignments, billing principal, and alias
restrictions remain applicable. [Access Control](access-control.md) and
[Fine-Grained Authorization](fine-grained-authorization.md) provide the existing
policy context; this design does not introduce a parallel role or ACL system.

The [personal](../product/light-agent/development-workflow-orchestration.md) and
[enterprise](../product/light-agent/development-workflow-orchestration-enterprise.md)
orchestration designs own feature stages, reviews, budgets, and publication.
Their grant references and reauthorization transitions depend on this design.
Enterprise execution envelopes and accounting receipts remain separate from
the user and app credentials specified here.

## Decisions

1. Preserve the original user access token in `Authorization` during an
   interactive call chain within explicitly approved forwarding destinations,
   where issuer audience and sender-binding rules permit. The local issuer's
   shared audience does not by itself approve a destination or calling app.
   Evaluate trusted `uid`, `role`, `grp`, `pos`, `att`, and other policy claims.
2. Put the immediate caller's independently verified app credential in
   `X-Scope-Token`. Every forwarding component supplies its own app token.
3. A valid user token does not bypass an endpoint's allowed-caller policy.
   Gateway-only API/MCP endpoints reject direct agent, workflow, and test-tool
   calls unless a separate route profile explicitly authorizes them.
4. Obtain fresh short-lived user credentials under a renewable, revocable
   grant for unattended work. The original expired JWT cannot remain the
   credential. Preserve the authenticated user identity and approved authority,
   not the original token bytes indefinitely.
5. The OAuth issuer issues credentials. Workflow must not extend expiry,
   disable expiry checks, sign copied user claims itself, or substitute a
   service account when user authorization expires. Keep the issuer's
   `auth_refresh_token_t` records and rotation, but load current user status
   and authorization claims on every refresh; saved claims are not continuing
   permission authority.
6. Separate credential replay protection, request authorization, and durable
   effect idempotency. Removing `lad1` must preserve or explicitly replace its
   request binding, replay, and workflow-permit protections.

## Current Implementation Boundary

Source inspected September 13, 2026. These are source observations, not a claim
that current deployment settings or external identity providers are qualified.

| Area | Existing foundation and remaining gap |
| --- | --- |
| Workflow ingress | `apps/light-workflow/src/rule_api.rs::authenticate` verifies user and `X-Scope-Token` credentials separately; `validate_invocation_caller` checks configured service IDs, host, and environment |
| Workflow HTTP | `executor.rs` forwards stored `user_authorization` and the workflow service token; formatting these headers does not acquire a renewed user token |
| Interactive renewal | `rule_api.rs::load_status` can replace a stored bearer with a newer authenticated caller token after subject/claims checks; this depends on a live caller |
| Nested MCP | `executor.rs` still mints `DelegationClaims` from saved subject claims, with expiry bounded by the workflow deadline and `now + 300`; this does not refresh the user's authority at the issuer |
| Gateway delegation | `apps/light-gateway/src/main.rs` verifies the workflow token and consumes a shared replay record before constructing the user principal |
| Agent execution origin | `apps/light-agent/src/gateway_credentials.rs` supplies the configured workload credential to each turn; it does not distinguish Chat from workflow execution. Separate Agent registrations, runtime credentials, and admission profiles are A2 work |
| Nested gateway protections | `frameworks/light-pingora/src/mcp.rs` derives child depth, execution class, and the synchronous permit pool from delegation; `privateVersionTarget` access requires its workflow invocation and `tool_ref`. All need verified replacements before removing `lad1` |
| Workflow action decisions | Workflow already owns invocation/dependency/budget records; Gateway has delegation replay storage. The authenticated Workflow action-decision API and Gateway client specified below do not exist yet |
| Durable dispatch intent | `agent_delegation_replay_t` records consumed delegation IDs, not the proposed send-intent state machine. The Workflow-owned PostgreSQL dispatch ledger and Gateway write/recovery APIs below are A2 prerequisites |
| OAuth client runtime | `frameworks/light-pingora/src/token.rs` and `spa_auth.rs` implement acquisition/exchange/refresh paths. `crates/light-client` has configuration and other OAuth operations; a shared unattended credential provider remains integration work |
| OAuth issuer | In the `portal-service` repository, `apps/light-oauth/src/main.rs` has refresh and token-exchange handlers. Exchange is limited to configured MSAL/CCAC client profiles; this is not a general qualified workflow-grant endpoint |
| Issuer client authentication | `light-oauth::authenticate_client` checks `client_secret`; its current TLS setup does not supply verified client-certificate identity to token handlers. Issuer-side `tls_client_auth`, client certificate registration, and broker endpoint wiring are new A1 requirements |
| Issuer token purpose | `generate_user_jwt` and `generate_client_jwt` share the normal provider key and emit no purpose marker; only `generate_long_lived_jwt` selects the separate long-lived key. A1 must add issuer-owned `token_use` emission/reservation and shared verifier enforcement |
| Specialized issuer grants | `client_authenticated_user` accepts a trusted client's user/role assertions; `long_lived` issues app tokens with registered/extra claims, including possible user-like fields. These supported features require explicit grant provenance and token-profile rules at workflow enrollment |
| Local user audience | `generate_jwt_with_key` uses the configured shared audience, default `urn:com.networknt`. Request-specific resource indicators are supported only for `client_credentials`; user-token forwarding needs a separate destination trust policy |
| Permission freshness | The issuer's `issue_access_token_from_refresh_token` uses stored claims in both normal refresh and duplicate-retry handling. `handle_refresh_token` copies those claims into the replacement persisted by transactional `replace_refresh_token`; a tenant-bound current authorization-context query is required |
| Refresh recovery | The issuer authenticates the client before its configurable grace path returns the live replacement refresh token. The strict unattended profile needs issuer-enforced per-client/grant selection, consumed-token replay handling, and broker recovery changes |
| Workflow lifetime | `workflow_invocation_t` stores the bearer and expiry today. A protected renewal-secret store, grant references, and unattended reauthorization/revocation need implementation |

Inventory each affected API/MCP/Knowledge route before rollout. Existing support
at Workflow ingress or the LLM gateway does not prove every resource verifies
both identities or enforces a gateway-only caller policy.

## Supported Issuer Grants And Token Profiles

Trusted-client custom claims remain supported for specialized integrations.
Registered application metadata is owned by client administration; mutable user
permissions are owned by their authoritative identity/policy source. Claims
used for authorization need an explicit owner and update rule even when custom
claims are uncommon. Do not require all application metadata to come from user
tables, or let it silently override current user permissions.

`client_authenticated_user` remains a specialized issuer-approved assertion
grant. It is not used by the proposed Workflow broker and is not a fallback for
expired, revoked, or missing user authorization. `long_lived` remains supported
for app credentials in `X-Scope-Token`. Its custom `uid`/`role`/`grp`/`pos`/`att`
fields do not convert an app token into a user session or a `WorkflowGrant`.

For the unattended profile, the issuer restricts the broker client to its
approved enrollment and refresh grants. Workflow enrollment verifies grant
provenance through issuer-owned grant/session metadata, including grant type,
eligible client, subject/tenant, and consent. An ordinary signed JWT with a
`uid` is insufficient enrollment evidence. Build and qualify this metadata
lookup with the issuer in A1; the current handlers do not provide this contract.
Specialized clients retain their separately approved use cases; disabling those
features across the platform is not a prerequisite.

User and app verification profiles must be distinguishable using issuer-owned
token-purpose/key-purpose metadata or a qualified issuer lookup. Header position,
token lifetime, or the presence of `uid` alone is insufficient. Reject app
credentials presented as user credentials, including long-lived tokens carrying
user-like custom claims. These rules prevent substitution without removing
trusted-client customization. [JWT validation profiles](https://www.rfc-editor.org/rfc/rfc8725.html#section-3.12)

The local A0 profile selects issuer-signed `token_use=user|app`, reserved against
custom-claim overrides, with A1 issuance and shared verifier implementation.
The [A0 purpose contract](user-application-workflow-authorization-a0.md#selected-issuer-profiles)
defines grant mapping and the app-only legacy long-lived key-purpose exception.
User purpose does not replace issuer provenance checks for Workflow enrollment.

## User And Immediate Caller Identity

| Hop | `Authorization` | `X-Scope-Token` |
| --- | --- | --- |
| Interactive Agent to light-gateway | Current user access token | Interactive Agent app token |
| Workflow Agent to light-gateway | Current user access token | Workflow Agent app token; action reference required |
| Workflow to light-gateway | Current user access token | Workflow app token |
| light-gateway to API/MCP | Current user access token | Gateway app token |

Each receiver validates signature, issuer, audience, expiry, and applicable
host/environment constraints independently for both tokens. Bind identities to
trusted issuer and tenant context; a bare `uid` from different issuers is not a
globally unique identity. Normalize claims through configured issuer mappings.
Never merge app roles into the user's claims or accept unsigned identity headers.

Keep three concepts distinct: the application that obtained the login token,
the workload authorized to act for the user, and the immediate calling service.
The login JWT's `client_id` does not establish the immediate caller. A trusted
service registration maps app-token identity, including the existing `sid`
where applicable, to the route's allowed callers.

If a delegated JWT uses the RFC 8693 `act` claim, validate the current actor
under that issuer's profile; prior actor history is audit information. It does
not replace authentication of the immediate hop. A profile that binds the user
token to a particular sender may require another issuer exchange before a
different service forwards it.

For gateway-only MCP access, both conditions are required: the current user may
call the tool, and the authenticated app is an allowed gateway. The gateway
evaluates the selected tool and request against existing fine-grained policy.
The resource still enforces its user/resource ACL and caller restriction.
Possession of a valid gateway token does not mean every user request is permitted.

Service-to-service forwarding replaces client-supplied scope headers with the
forwarder's own credential. Reject duplicate/malformed credentials and do not
fall back from failed app authentication to user-only authentication. Direct
interactive ingress uses its own explicit profile; a browser receives no
gateway app secret to satisfy a downstream gateway-only rule.

### Credential-Based Agent Execution Origin

For the initial profile, deploy separate interactive and workflow Agent service
instances using the same binary but distinct app registrations, scope tokens,
mTLS identities, and secret mounts. The workflow instance cannot access the
interactive instance's credentials. Trusted admission selects the instance;
prompts, request headers, and tool arguments cannot select the credential profile.
The interactive instance accepts the approved Chat ingress and rejects Workflow
dispatch callers; it cannot provide a second route for running workflow jobs.

Gateway classifies the verified issuer/client identity through an administrator-
owned registration mapping, checking its mTLS peer binding. Calls from Workflow
or the workflow Agent registration always require an active action reference,
including model, API/MCP, and Knowledge calls. Missing or invalid references
deny the request even on an interactive URL. Root interactive admission accepts
only its allowed registrations and ingress profile. Rewriting a mode header or
path cannot turn a workflow credential into an interactive one.

The same verified app identity cannot be registered in both profiles in this
pilot. A shared Agent runtime that holds both credentials and chooses between
them from caller input is not qualified. A future shared-runtime design needs
an independently verified job binding; it is not required for the pilot.

## Gateway-Only Transport And App Credential Lifetime

A bearer token proves possession, not the physical origin of a request. Enforce
the gateway-only boundary with backend ingress restrictions and authenticated
service transport. The target profile uses mTLS, binding the allowed gateway
workload to its peer identity and, where supported, certificate-bound app access
tokens. A copied bearer alone must not allow an unapproved peer to impersonate
the gateway. Any weaker rollout profile must state its residual replay exposure.
Check that the authenticated peer maps to the same registered app as the scope
token; accepting any certificate from the platform CA is insufficient. At a TLS
terminator, propagate peer identity only through an authenticated internal path
that strips externally supplied certificate/identity headers.

Use distinct app registrations and credentials for Gateway, Workflow, Agent,
and authorized test clients. The personal/local-issuer profile supports existing
long-lived app tokens with mTLS and app/peer binding. Short-lived
`client_credentials` tokens are an optional alternative; migrating to them is
not a prerequisite. Long-lived tokens still require expiry validation and an
operational revocation/replacement path. Keep official-environment tokens and
client keys/secrets in protected runtime configuration.

Checked-in app tokens and certificates may remain deliberate local-development
fixtures so developers can start the stack without repeated credential setup.
Treat them as public credentials: official environments must use independent
keys/credentials and reject development fixtures. Native workers and test tools
use their own registered identities; sharing a fixture gateway identity is not
proof that gateway-only authentication has been qualified.

### Shared Audience And Forwarding Destinations

The local issuer intentionally uses a shared platform audience because a user
token traverses multiple services. Keep that audience and validate it at every
receiver. Qualify a bounded set of trusted services for original-user-token
forwarding through an administrator-controlled route/credential policy; an
audience match or MCP registration alone does not enroll a recipient.

That policy binds each destination's service identity and allowed origin/path
to its credential profile. A demo or third-party MCP server does not receive
the platform user bearer merely because it is behind Gateway. For destinations
outside the approved set, use their own authorization flow or an issuer-qualified
resource-specific exchange. The current local user-token grants do not provide
that exchange; block user-token forwarding until an appropriate profile exists.
Do not replace the missing profile with a service account and copied user claims.

Forward user tokens only to trusted configured destinations accepted by their
issuer/audience and sender-binding rules. Do not disable audience validation or
follow credential-bearing redirects to make forwarding work. Every recipient
in the shared audience still enforces user policy and allowed-caller rules.
A compromised approved recipient can expose a broadly usable user bearer;
mTLS/app checks reduce where it can be replayed but do not make that bearer
resource-specific. This is an explicit trust-domain tradeoff, not proof of
isolation between every platform service. [OAuth audience restrictions](https://www.rfc-editor.org/rfc/rfc9700.html#section-2.3)

## Grant Ownership And Credential Storage

Use a trusted Workflow credential broker to mediate unattended user renewal.
For the first implementation it is a module hosted by `light-workflow`, using
shared OAuth client/provider code; it does not require a new deployed service.
It is separate from model prompts, native worker execution, ordinary workflow
inputs, and the artifact store. The OAuth issuer remains the token authority.

| Component | Responsibility |
| --- | --- |
| OAuth issuer or approved federation service | Authenticate the subject/client, approve renewable authorization, issue tokens, and enforce issuer-side expiry, revocation, and claim rules |
| Workflow credential broker | Redeem approved grants, protect renewal credentials, serialize renewal, verify issued tokens, and supply credentials to trusted outbound service calls |
| Workflow grant store | Persist user/issuer/client bindings, consent and resource ceilings, permitted workflow/schedule, grant version/status, deadlines, and credential reference |
| Gateway and resource services | Authenticate both identities, enforce current policy/ACLs, and validate applicable workflow/grant/action bindings |
| Controller/runner and native workers | Execute admitted work; do not receive refresh tokens, issuer client secrets, or signing authority |

A `WorkflowGrant` records its ID/generation, verified subject and issuer mapping,
host, authorized backend client, workflow definition/version or approved schedule,
allowed targets/resources/actions, authorization ceiling, consent evidence,
validity period, revocation state, and opaque renewal-credential reference.
These are proposed schema fields, not existing configuration keys.

Store refresh credentials in a dedicated encrypted credential store controlled
by the broker; keep encryption keys outside workflow definitions and ordinary
database rows. Persist only references and authorization metadata in workflow
state. Neither a grant ID nor a workflow ID is bearer authority: broker access
requires an authenticated allowed workload bound to that grant and execution.

Access tokens may be cached briefly in trusted service memory. Keep all reusable
credentials out of prompts, conversations, review artifacts, issue comments,
logs, and ordinary task results. Audit subject/app/grant/action identifiers,
policy decisions, generations, and failure reasons without token bytes.

## Interactive, Scheduled, And Long-Running Execution

Interactive execution may forward a currently valid original token. If it
expires without an unattended grant, require authenticated renewal or pause.
Do not turn a normal interactive login into unlimited background authority.

For unattended user-authorized work:

1. While the user is present, obtain authorization for a bounded workflow or
   schedule. Use an approved confidential backend OAuth client to establish a
   renewable grant. Preserve the user's identity and issuer-approved claims.
2. Prefer an issuer-qualified token exchange profile that can issue a renewable
   grant for offline use. If the issuer instead requires a backend authorization
   code flow, complete that flow for the backend client. Do not copy a refresh
   token issued to the SPA into an unrelated client's credential store.
3. At a scheduled start, create the run bound to the grant and check its current
   status, generation, scope, and deadline. Acquire a fresh short-lived user
   token before dispatching protected calls; the old browser token is unnecessary.
4. During execution, obtain or reuse a sufficiently fresh token for each target.
   Attach the immediate service's own app token and apply current authorization
   and the run's approved ceiling before every protected action.
5. On one-off completion/cancellation, disable further use of that run's grant
   binding. An approved recurring schedule may retain its parent grant until
   expiry/revocation; each scheduled run has separate execution/action identities.

```mermaid
sequenceDiagram
    participant U as User
    participant W as Workflow and credential broker
    participant O as OAuth issuer
    participant G as light-gateway
    participant R as API or MCP resource
    U->>W: Authorize bounded workflow or schedule
    W->>O: Establish approved renewable user grant
    O-->>W: Backend-bound renewal credential
    Note over W,O: Later, the original access token has expired
    W->>O: Redeem active grant with issuer-verified mTLS client authentication
    O-->>W: Fresh short-lived user access token
    W->>G: User token plus Workflow app token and action reference
    G->>G: Validate identities, destination profile, and tool policy
    G->>W: Fixed action-decision API with Gateway identity and user token
    W->>W: Check live grant, run, dependency, and budget; claim dispatch
    W-->>G: Bound decision, pinned target, depth, and dispatch lease
    G->>G: Validate decision binding
    G->>W: Begin dispatch for the same attempt and decision
    W->>W: Recheck authority and atomically persist send intent
    W-->>G: Acknowledge newly recorded send intent
    G->>G: Acquire connection, complete TLS, and obtain required capacity
    G->>G: Atomically check deadline and transition send guard at write boundary
    alt Deadline valid and send guard acquired
        G->>R: Accepted user token plus Gateway app token
        R->>R: Validate caller and user/resource authorization
        R-->>G: Result
        G-->>W: Result and action evidence
    else Deadline elapsed and request never initiated
        G->>G: Atomically close send guard
        G->>W: complete(NOT_INITIATED) for the same owner and generation
        W->>W: Record outcome and release unused reservation
        W-->>G: Safe to seek fresh authorization under retry policy
    end
```

The final forwarding edge assumes an approved forwarding destination and a
valid token for that resource. A required external-resource exchange must be
qualified before enabling that route. The action-decision callback is the
internal service operation defined below, not another workflow or MCP dispatch.

An organization-owned schedule may instead run under an explicitly authorized
service identity and its own permissions. Record the human creator for audit,
but do not fabricate their `uid` or impersonate an administrator. The execution
mode is fixed at authorization time; expiry cannot switch modes automatically.

RFC 8693 defines token exchange and permits refresh tokens for offline cases,
but does not require issuers to provide them. A one-time exchange into another
short-lived token is not an unattended renewal strategy. Issuer support for
this profile must be qualified before enabling scheduled user execution.
[RFC 8693, section 2.2.1](https://www.rfc-editor.org/rfc/rfc8693.html#section-2.2.1)

## Refresh-Token Claims And Authorization Freshness

Keep `auth_refresh_token_t` and refresh-token rotation. Change the source of
authorization claims when issuing a refreshed access token: query current
authoritative user status and permissions instead of treating the stored
snapshot as current authority. The record remains necessary to validate the
renewal credential, bind it to its user/client/provider/tenant and session,
preserve the approved grant scope, and support rotation and revocation.
This issuer-side record is separate from the Workflow broker's protected store
of client-held renewal credentials.

### Current Issuer Behavior

In `portal-service`, the normal username/password authorization-code login
already queries the database. `portal-core::login_user_by_email` assembles
roles, groups, positions, and attributes from user and relationship tables.
`light-oauth::post_code` copies these values into the authorization-code record;
`handle_authorization_code` then copies them into `auth_refresh_token_t`.
Creating that refresh record uses the code's snapshot without another live
permission query.

On refresh, `get_refresh_token_detail` reads the refresh record, and
`issue_access_token_from_refresh_token` builds the access token from its saved
roles and other claims. `handle_refresh_token` copies those fields into the
replacement record. `replace_refresh_token` inserts the replacement, deletes
the old record using its aggregate version, and writes session/audit updates
in one transaction. Rotation therefore changes the credential, not the
freshness of the permission data.

The issuer also has a configurable duplicate-retry grace path:
`find_recent_refresh_token_rotation` can resolve a recently consumed token to
its replacement, and the same issuance helper mints another access token from
that replacement's claims and returns the live replacement refresh token.
Client authentication happens first, so possession of the old token alone is
not sufficient for a confidential client. However, a presenter able to
authenticate as that client can follow the rotation within the grace window.
Rechecking claims does not restore theft detection. Retained interactive grace
profiles need current authorization checks; unattended grants use the strict
profile below.

A removed role can consequently survive repeated refreshes unless another
mechanism revokes the session or grant. The exposure can last for the renewable
session's lifetime, not just one access token's lifetime.

### Alternatives And Tradeoffs

| Approach | Benefit | Cost or limitation |
| --- | --- | --- |
| Reuse the stored claims snapshot | Simple; avoids user/permission joins during refresh | Permission and account-status changes can remain invisible across rotations |
| Load current claims on every refresh — initial choice | Direct freshness rule; future tokens reflect current account and permission state | Adds authorization-context queries and depends on that source being available |
| Cache claims with an authorization version | Reuses the snapshot while the current version matches; reduces repeated joins | Every relevant user, membership, role, group, position, attribute, and policy change must reliably invalidate the affected context; an unchecked version inside the token proves nothing |
| Revoke affected sessions/grants on permission changes | Forces a new authorization flow; useful for explicit revocation and security events | Disrupts users and background workflows; requires complete, race-safe invalidation and does not by itself invalidate issued access tokens |

Start with live claims on each refresh. Refresh already reads and writes the
database; the incremental cost is resolving the current authorization context,
not introducing the first database call. This occurs at renewal, not on every
API request. Measure query cost before adding a versioned cache. Explicit
session/grant revocation remains available alongside the live-query approach.

### Required Refresh Behavior

1. Authenticate the OAuth client and validate the refresh credential's
   client/provider, subject/tenant, session/grant status, lifetime, and applicable
   sender binding. A successful credential lookup alone is insufficient.
2. Load current account status and authorization context using the verified
   issuer/provider mapping and the record's stable `user_id` and `host_id`.
   Reject disabled, locked, removed, or otherwise ineligible users and invalid
   tenant memberships. Resolve currently effective roles, groups, positions,
   attributes, and other policy-bearing claims from their authoritative sources.
3. Issue a short-lived access token with that verified identity and current
   claims, constrained by current client policy and the existing OAuth grant.
   A refresh request cannot expand the originally granted scopes. Keep the
   workflow's approved tools/resources/actions as a separate ceiling: newly
   acquired user permissions do not expand an already approved workflow.
   [RFC 6749, section 6](https://www.rfc-editor.org/rfc/rfc6749.html#section-6)
4. On normal refresh, atomically persist the replacement credential, consume
   the old one, and record the rotation/session audit before returning success.
   Preserve grant bindings and revocation semantics across generations. If
   claims columns remain, populate the replacement with the current snapshot;
   do not copy old permission values forward as authority.
5. If a separately qualified interactive profile permits duplicate retries,
   recheck current status and claims before minting against the replacement.
   Never enter that grace path for an unattended grant. Neither profile may
   restore removed permissions or revive a revoked session.
6. If the authoritative status/claims source is unavailable, fail renewal with
   an appropriate retryable outcome; do not fall back to saved permissions.
   Coordinate this with the broker's bounded retry and expiry behavior below.

The implementation needs a dedicated tenant-bound authorization-context query.
The existing `get_user_by_id` returns `NULL` for roles, groups, positions, and
attributes, and selects the user's currently chosen host. Reusing it would not
load the required claims. Nor should refresh simply call the email login query:
switching the user's current Portal host must not move an existing grant to a
different tenant. Apply the relevant account, membership, and relationship
eligibility rules explicitly.

Classify stored claims during migration. Immutable grant bindings and approved
scope remain authoritative grant data; mutable permission claims must be
reloaded. Policy-bearing values in `custom_claim` need the same treatment as
`role`, `grp`, `pos`, and `att`. Existing snapshot columns may remain for
compatibility or controlled historical use, but renewal must not authorize from
them. Dropping the table or its claim columns is not required for this change.

For third-party identities, the live source may be the external issuer, an
approved federation service, or a qualified synchronized authorization store.
Portal's database is authoritative only for claims it actually owns. Qualify
provider mappings, account-status checks, and any synchronization delay before
enabling unattended renewal.

### Strict Rotation For Unattended Grants

The initial Workflow broker profile uses confidential-client authentication
bound to its registered mTLS workload/key, one-time refresh rotation, and **no
replacement-token recovery through a consumed token**. The issuer selects this
policy from protected client/grant metadata, including retained consumed-token
history; request parameters cannot opt into an interactive grace profile.
Per-client/grant selection is new A1 work, not an existing override of the global
`refreshTokenRotationGraceSeconds` setting.

Implement issuer-side OAuth `tls_client_auth` for the broker, not just mTLS
between Workflow and Gateway. The initial local-issuer profile terminates TLS
at a dedicated `light-oauth` broker listener, validates the client certificate
chain and validity, and passes verified peer identity to the handler through
server request context. Match the certificate's registered subject/SAN to the
client/provider/tenant and enforce the configured certificate revocation policy.
Do not accept certificate identity from caller-supplied HTTP headers.
[OAuth mTLS client authentication](https://www.rfc-editor.org/rfc/rfc8705.html#section-2.1)

Persist the broker client's authentication method and certificate identity in
issuer-controlled registration metadata. Every broker token, grant-lookup, and
revocation operation requires that method. A correct client secret without the
registered certificate fails, including at legacy token URLs; there is no
secret-only fallback for this client. Other clients retain their configured
authentication methods. A1 includes listener/trust configuration, handler peer
identity plumbing, registration/schema publication, broker certificate/key
loading, and certificate renewal/revocation qualification. This client
authentication requirement does not automatically bind the forwarded user access
token to the broker's certificate; access-token sender binding is a separate
issuer profile.

This protects against theft of the refresh token and client secret without the
mTLS key. Full host compromise can still use or steal a software-held key;
profiles requiring protection against key export need an isolated or
non-exportable key facility. Do not claim that mTLS alone prevents that attack.

Keep enough family/session and consumed-token history to detect reuse across
rotations. An authenticated replay by the bound client returns `invalid_grant`,
revokes the affected refresh family, and records the event without returning
its live replacement. An unauthenticated or wrong-client request cannot obtain
replacement credentials or revoke another client's family. Continue checking
current user authority on every successful refresh. Rotation and sender binding
serve complementary purposes. [Refresh-token protection](https://www.rfc-editor.org/rfc/rfc9700.html#section-4.14.2)

The broker serializes renewal across replicas and durably records the attempt
and credential generation. If a response is lost after possible issuer commit,
or the broker crashes before saving the replacement, mark the credential state
uncertain, fence its use, and require reauthorization for the initial profile.
Revoke the uncertain family through an authenticated issuer operation. Do not
retry the old token or create a new grant automatically. Retry is safe only
when the issuer contract or transport establishes that no rotation occurred.
For the local provider, a connection-establishment failure (`is_connect`) or a
JWKS failure before the token POST is `NOT_SENT`. Only the same live renewal
owner/boot, attempt and generation may atomically restore an unrevoked grant to
`ACTIVE`, preserving its encrypted token and recording that outcome. Lost owner
proof, post-send failures and crashes still use the uncertain path. An HTTP error
response alone does not prove no commit.

Fetch/cache verification keys before rotating (five-minute cache lifetime), with
one refetch for an unknown signing key ID. Publish new issuer keys before use.
A failed refetch or invalid token after rotation remains uncertain. Recovery
store errors are logged and retried on the next periodic tick; they must not
close admission for unrelated workflows. Configuration errors remain startup
failures. Save a valid rotation for its shared grant even if its requesting run
was canceled, then deny that run the access token. Grant revocation and expired
renewal ownership still prevent saving the rotation.

The enrollment API accepts an optional `credentialBroker.legacyLongLivedAppKeys`
list of approved issuer/key-ID pairs, default empty. This permits only the A0
local long-lived app fixtures in `X-Scope-Token`; signatures, expiry, caller
service identity and explicit purpose markers remain enforced. It never relaxes
`Authorization`. Official deployments keep the list empty and use marked app
tokens. Qualify the actual Gateway credentials before A1 activation.

A future recovery protocol must bind a durable refresh operation and its result
to the original authenticated sender/key, recheck current grant state, and
prevent a consumed token from retrieving its successor by itself. It requires
separate qualification; an idempotency key or a time-based grace window alone
does not supply that proof.

## Permission Freshness, Revocation, And Recovery

Authorization requires all applicable checks: current user policy/ACL, allowed
calling app, the approved grant/run ceiling, and the current action permit and
budget. Newly acquired roles cannot silently widen the approved workflow.
Removed roles, disabled accounts, and revoked grants must stop or narrow future
actions according to current policy.

The live refresh query changes future tokens; it does not rewrite or invalidate
an already issued access token. A resource that only checks a self-contained
JWT may continue accepting its old claims until expiry. Earlier enforcement
requires a current policy/status, authorization-version, revocation, or
introspection check at the authorization boundary. A policy check using only
the old JWT's attributes cannot discover changed user membership. State the
maximum enforcement delay for each qualified profile, including changes racing
with token issuance; refresh-time checks alone are not immediate revocation.
[RFC 7009, section 3](https://www.rfc-editor.org/rfc/rfc7009.html#section-3)

Workflow and Gateway check current grant/run/action state before dispatching
workflow-originated protected calls. Do not trust a caller-supplied invocation
ID without checking the actor, tenant, target, request binding, and generation.
For the initial unattended profile, failure to establish current grant authority
blocks new dispatch. Any later caching profile must specify its maximum
revocation delay; a self-contained JWT alone cannot prove immediate revocation.

Changes to stable user claims require authorization re-evaluation against the
accepted disclosure ceiling. The current status API's exact claims-digest check
is not a general renewal protocol. Reauthorization records a verified new
binding and decision; it cannot silently rewrite accepted inputs or reset
workflow budgets, action identities, or review state.

Renewal is single-flight per credential generation, including across Workflow
replicas. Persist replacement credentials atomically with their generation.
A lost response during refresh-token rotation is uncertain credential state:
the initial unattended profile requires reauthorization as specified above.
Transient failures known not to have rotated the credential may retry within
bounded limits while credentials remain valid. After expiry, or when the grant
is fenced/revoked, stop protected dispatch and enter `REAUTHORIZATION_REQUIRED`
or an explicit retryable wait appropriate to the failure.

Reauthorization requires the same verified user/tenant and the necessary current
grant. Grant revocation does not become permission to auto-create a new one.
Unknown in-flight effects remain fenced and reconciled; renewed credentials
must not replay those effects under a new attempt identity. Local saved work can
remain recoverable while new model/tool/API actions wait for authorization.

## Replay, Action Authorization, And Accounting

Short token lifetime limits exposure but does not stop a stolen bearer from
being reused before expiry. Use protected transport, sender-constrained tokens
where supported, and client-bound renewal credentials with appropriate rotation.
[OAuth Security BCP](https://www.rfc-editor.org/rfc/rfc9700.html#section-2.2)
and [certificate-bound access tokens](https://www.rfc-editor.org/rfc/rfc8705.html#section-3)
describe those credential protections.

Separately, persist each workflow action's identity and request digest, bound to
the host, subject, app, invocation, target, and fencing/budget generations.
Gateway validates an active server-owned action permit and records permit/deny
with its policy/permit version and reason. Authentication success is not an
authorization decision. A request reference selects state; it does not grant
authority by itself.

For effects, an identical retry reconciles the recorded result or uncertain
execution; the same action identity with a different request digest fails.
Changing a client-supplied idempotency key cannot allocate an additional
authorized workflow attempt or budget. Revalidate access before disclosing a
cached result. Do not mark the user access token itself as consumed after one
request: it can legitimately authorize several calls.

Keep invocation depth/class, policy and response ceilings, action reservations,
budget ledger/generation, and live cancellation state in authoritative workflow
records or separately qualified execution envelopes. Do not clone them into
user tokens as mutable authorization truth. Gateway must resolve or validate
them at decision time when replacing the current delegation verifier.

## Gateway To Workflow Action Decisions

Implement fixed internal HTTP operations in `light-workflow`, proposed as
`POST /internal/workflow/actions/authorize`, `.../begin-dispatch`, `.../complete`,
and `.../status`, with a trusted Gateway client. These are service operations,
not model-visible tools, workflow definitions, or calls routed back through
Gateway. Workflow owns the state and transactions; Gateway receives bounded
results and needs no direct database credentials.

Before outbound dispatch, Workflow reserves an action against the admitted run
and its pinned dependency/budget records. Calls carry an opaque action reference
alongside the user and immediate-app tokens. Workflow and workflow-bound Agent
registrations require this reference, using the credential-based origin rules
above. Omitting it must not reclassify the request as a root interactive call.

Gateway validates the incoming user and app credentials, app/peer binding,
destination profile, and tool policy, then calls the fixed operation directly
over authenticated service transport. It sends the current user token in
`Authorization` and its own app token in `X-Scope-Token`; Workflow verifies both
and requires an allowed Gateway peer matching that app. This internal caller
profile permits only these fixed control operations, not arbitrary bypass of
Gateway-protected business APIs.

The request includes the action reference, invocation/tenant, canonical request
digest, selected logical tool/operation and target, verified original calling
app, and a durable Gateway dispatch-attempt/replica identity. Original-caller
fields are assertions from this authenticated Gateway, checked against the
action's stored actor binding; public request headers are not authority.
Workflow compares the user identity and current claims with the admitted grant
and applies the claims-change rules above.

In one transaction against authoritative current state, Workflow locks the
relevant grant, run, action, and budget records, validates their generations,
deadline, cancellation/revocation status, actor, request digest, and pinned
dependency, and atomically claims the dispatch against its budget reservation.
It must not authorize from a lagging read replica or a Gateway cache. The
decision returns the bound subject/app/tenant, action and attempt IDs, grant/
run/budget/fencing generations, exact tool reference and target/contract digest,
execution class, parent/child depth where applicable, policy/disclosure ceilings,
and a short dispatch lease. Persist allow/deny evidence with the decision ID;
only an allow creates an `AUTHORIZED` dispatch-ledger row. The result is
authenticated service data, not a replacement user JWT or a
transferable bearer credential.

### Durable Dispatch Ledger And Recovery

Use a new **`workflow_action_dispatch_t` table in Workflow's operational
PostgreSQL database**, owned and migrated by `light-workflow`. It is distinct
from Gateway's `agent_delegation_replay_t` and survives removal of that table.
Gateway durably records intent by calling the fixed API; no local file, memory
cache, or additional Gateway database is the authority. Database unavailability
blocks sending. A2 must build the table, APIs, Gateway client, and recovery logic.

The ledger binds tenant, action, canonical effect/dispatch-attempt ID, request
digest, target, decision/lease generation, Gateway owner/boot identity and fencing
generation, reservation, state, and outcome/evidence references. Uniqueness and
compare-and-set transitions allow only one active dispatch attempt per action.
Keep transition history and retain unresolved rows irrespective of lease expiry;
retain terminal evidence for the invocation's recovery/audit period. No tokens
or private keys are stored in this ledger.

Before sending, Gateway calls `begin-dispatch` for the authorized attempt.
Workflow checks ownership, generations, lease, live grant/run/action state and
budget again, then atomically changes `AUTHORIZED` to `SEND_INTENT`. Only the
acknowledgement of that new transition permits the owner to send. A duplicate
call reports existing state; it does not issue another send permission. Gateway
records the outcome through `complete`, transitioning to a known terminal state
or `UNCERTAIN`, or recording retryable `NOT_INITIATED` as specified below.
Status/result reads recheck caller and disclosure authority.
Completion reporting uses a dedicated service profile: the authenticated
Gateway may record evidence for its stored send-intent binding after user-token
expiry or grant revocation. It cannot authorize another send or disclose a
business result. Such reporting does not require renewed user authority;
authorize/begin and business-result disclosure retain their current user checks.

`complete(NOT_INITIATED)` handles deadline expiry before the target request
starts, including during connection preparation or after a late send-intent
acknowledgement. Before reporting it, the original live Gateway owner must
atomically close a local send guard:
`NOT_STARTED -> ABORTED_NOT_INITIATED` competes exclusively with
`NOT_STARTED -> STARTED`. Keep the guard `NOT_STARTED` while acquiring the
pooled connection, completing TLS/protocol setup, and obtaining all capacity
permits. Preparation may establish a connection but must not send the target
operation's headers or body, including through early data or automatic retries.

At the final transport write boundary, use one guarded operation to check the
original monotonic deadline and transition `NOT_STARTED -> STARTED` only if the
deadline is still valid and the send-intent acknowledgement is valid. Immediately
initiate the first request write on the prepared connection, with no intervening
queue, connection/permit acquisition, handshake, or asynchronous yield. This
guard belongs inside the transport at that boundary; wrapping a high-level
HTTP client's queued `send` call is insufficient.
Disable automatic request retries for these routes throughout the outbound
transport, including retrying a failed reused connection on a fresh connection.
A send-intent acknowledgement permits only one guarded request initiation;
client or proxy retry logic cannot obtain another by resetting the local guard.

Expiry before this transition closes the guard as `ABORTED_NOT_INITIATED` and
allows the owner completion. Closing it prevents late callbacks or queued
continuations from sending. Once `STARTED` wins, failure or expiry remains
`UNCERTAIN` unless the outcome is known; the guard never authorizes a late write
or an automatic retry. Abort further initiation if the deadline has elapsed.
A transport timeout, absent result, or lack of a receipt is not proof of
non-initiation. If the guard was crossed or its state is unknown, use
`UNCERTAIN` instead.

Workflow accepts this completion only from the authenticated Gateway whose owner,
boot, fencing, decision/lease, action, and attempt identities match the current
`SEND_INTENT` record. Lease expiry alone does not prohibit this cleanup, but a
superseded owner/generation cannot assert it. In one transaction record
`NOT_INITIATED`, invalidate that send permission, and release the unused
reservation exactly once. A repeated accepted completion returns its receipt;
it cannot release a newer generation's reservation or change another outcome.
The trusted owner's assertion is required; Workflow cannot infer it from time.

Recovery is state-based across all Gateway replicas:

- `AUTHORIZED` with no send intent: after fencing the old owner, fresh online
  authorization may replace the expired lease under the same canonical attempt
  and budget reservation. The old decision generation can no longer begin a send.
- `NOT_INITIATED`: the recorded owner completion proves that this send was
  abandoned before transport initiation. Under normal backoff and retry limits,
  fresh online authorization may reuse the same canonical action/effect attempt
  and request digest with a new decision/lease and reservation generation. It
  must reacquire available budget and recheck current grant/run authority;
  incurred costs and retry counters are not reset. No operator is needed.
- `SEND_INTENT` or `UNCERTAIN`: a send may have happened, even if an acknowledgement
  was lost. A status read or Gateway restart never authorizes another send.
  Reconcile through a target receipt or qualified target idempotency protocol
  using the same effect identity; otherwise require operator resolution.
- Known terminal outcome: return or reconcile recorded evidence after current
  authorization checks, without executing the effect again.

Lost authorize/begin/complete responses and a crash before or after a network
send follow these rules. If the owner crashes before `NOT_INITIATED` is durably
accepted, a new boot or another replica cannot reconstruct the local guard and
assert non-initiation. If the completion committed but its response was lost,
the recorded `NOT_INITIATED` receipt is sufficient for the safe retry path.
Do not reset an ambiguous attempt to `AUTHORIZED`,
allocate another reservation, or infer that lease expiry proves no effect.
Admission failure known to precede send intent settles the unused reservation.

Qualified receiver reconciliation uses the existing `complete` operation, not a
new dispatch operation. It authenticates the receiver app and its exact mTLS
peer, checks that the receiver is registered for the action's pinned tool, and
accepts only a terminal evidence digest for the same action and generation while
that generation is `UNCERTAIN`. It can settle the retained reservation after the
user token or run deadline expires, but it cannot authorize, begin, retry, read a
business result, or change an existing terminal receipt. An identical receipt is
idempotent; conflicting evidence fails closed.

### Dispatch Lease Without Cross-Host Clock Comparison

There is no positive authorization cache. Gateway records local monotonic time
`t0` immediately before sending `authorize`, and its send deadline is
`t0 + 5 seconds`. Count authorization latency, local admission, the
`begin-dispatch` round trip, connection/TLS/protocol preparation, capacity
acquisition, and all intervening waits against that same budget. Check the
deadline and transition the send guard together at the final write boundary
described above, after all preparation. A response received after the deadline
cannot permit a send.
Do not reset `t0` on retries or on receipt of either response. Since the Workflow
decision occurs after `t0`, this conservatively bounds decision-to-send elapsed
time to five seconds without subtracting timestamps from different hosts.

Workflow separately expires its authorization lease at its own decision time
plus five seconds, or the earlier grant/run/action deadline, using its database
clock sampled after acquiring the relevant locks. `begin-dispatch` rejects a
late claim on that clock and rechecks current authority in the same transaction
as `SEND_INTENT`. It also returns the remaining authorized duration after
applying those deadlines. Gateway sets its deadline
to the earlier of `t0 + 5 seconds` and the monotonic begin-request start plus
that duration; it never extends the original limit. UTC times are useful for audit
and issuer expiry checks, not for reconstructing the Gateway elapsed timer.

Gateway restart, suspend/resume, migration, or loss of timer continuity
invalidates the outstanding local lease. Recovery consults the durable ledger;
it cannot rebuild a send deadline from wall-clock timestamps. A fresh lease
requires the safe `AUTHORIZED` or recorded `NOT_INITIATED` recovery path above.
Unknown references, invalid responses, unavailable storage, or elapsed deadlines
fail closed before sending.
An elapsed deadline after send-intent acknowledgement uses `NOT_INITIATED` only
when the original owner can close the unstarted send guard; otherwise it retains
the uncertain-execution recovery path.

`begin-dispatch` is the final authorization decision point. Revocation committed
before it denies the action; afterwards the action is admitted/in flight and
subject to cancellation/fencing. The lease bounds initiation of the request,
not network delivery, completion, or rollback of an already sent effect.
Long-running work rechecks permits at subsequent protected actions.

### Nested Depth, Capacity, And Private Version Targets

Replace each use of `context.delegation` in Gateway's workflow admission with
the verified action context, not fields copied from the user JWT or request.
Only an independently admitted root starts at depth zero. Workflow derives a
nested workflow's depth with checked `parent.permit_depth + 1`, bounds it by
the parent grant/dependency ceiling and the target's
`maximum_delegation_depth`, and preserves the permitted execution class and
remaining deadline. Missing context, depth overflow, or an unknown class denies
the nested call; it must not silently select root defaults.

Gateway continues selecting the synchronous permit pool by the verified child
depth. Preserve the existing nonblocking `try_acquire_owned` behavior: an absent
pool or exhausted capacity returns the defined capacity error. A parent holding
a synchronous permit must not make its child wait for that same pool. Async
children also obey the depth/lineage limits. When Gateway starts a child through
`StartInvocationRequest`, carry the parent action/decision reference; Workflow
revalidates it and atomically binds the child to that action and lineage.
Retries cannot create another child or reset depth, class, or budget.

For `privateVersionTarget`, Workflow resolves the exact target from the admitted
run's pinned dependency registry. Gateway compares the verified `stableToolRef`,
private target name/version, endpoint, and contract digest with its published
mapping and authorizes using the logical `authorizationToolName`. An invocation
ID or tool reference supplied by a caller is insufficient. Keep private targets
out of public discovery; direct guesses, cross-run/tenant references, wrong
versions, and binding drift fail without revealing the private target. Valid
pinned calls remain reachable even when a public alias advances to a new version.

## Implementation Order And Exit Gates

### A0: Freeze Contracts And Issuer Profiles

The [A0 baseline](user-application-workflow-authorization-a0.md) captures the
initial local-issuer selection, current client inventory, contract records,
claim ownership, transport inventory and deployment-manifest requirements.
It selects backend authorization-code enrollment with PKCE S256 for the broker;
the issuer must persist and validate the challenge and consent/provenance in A1.
This baseline does not qualify a running stack or an external issuer.

Define the identity context, allowed-caller rules, grant/action schemas,
credential-store boundary, issuer mappings, renewal/revocation behavior, and
failure states. Inventory existing endpoint bypasses and all `lad1` consumers.
Pin the supported light-oauth and third-party issuer profiles. Provider settings
or a token-exchange handler alone do not satisfy offline execution qualification.
Classify grant bindings versus mutable claims, identify each claim's authoritative
source, and define tenant binding and permission-change enforcement delays.
Inventory trusted clients and their `client_authenticated_user`, `long_lived`,
exchange, and refresh permissions. Freeze the broker's eligible grant provenance,
strict refresh policy, user/app verification profiles, shared-audience forwarding
destinations, separate Agent registrations/admission, issuer mTLS registration,
and the internal action-decision/dispatch-ledger contracts. Define the local
monotonic deadline and restart/fencing rules without a cross-host skew allowance.

### A1: Implement Issuer Grants And Broker Renewal

See the [A1 implementation and qualification report](artifacts/user-application-workflow-authorization-a1-progress.md)
for source changes, test evidence and the remaining selected-stack activation gate.

Implement the A0 `token_use` marker in every local access-token issuance path,
including refresh and specialized grants. Derive it from validated grant
semantics, reserve it against registered/request custom claims, and implement
shared verifier checks for user versus app positions. Preserve only the explicit
legacy long-lived app key-purpose exception; markerless short-lived tokens do
not gain user authority from their claims or shared signing key. A2 must wire
these checks at every receiver in the selected profile.

Emit purpose as a dedicated `JwtClaims` field serialized as `token_use`, with
the validated purpose passed explicitly to `generate_jwt_with_key`. Keep the
name reserved in custom claims, but do not insert the issuer value into `extra`:
the helper applies `remove_reserved_claims(extra)` before signing and would
remove it. A1 tests must verify exactly one correct purpose marker in the
actual signed token after filtering and serialization, including override cases.

Implement the shared OAuth acquisition/provider layer and Workflow-hosted
broker, encrypted renewal storage, grant lifecycle, and serialized refresh
recovery. Extend/qualify the local issuer for backend-bound unattended grants,
revocation, and the live refresh behavior above. Implement the tenant-bound
authorization-context query for normal refresh and any retained interactive
grace profile. Implement broker-client grant restrictions, issuer grant-provenance
lookup, and strict unattended rotation with consumed-token history and family
revocation. The broker must fence uncertain rotations and require reauthorization
instead of recovering through the legacy grace path. Retain transactional
rotation and grant ceilings.
Implement `tls_client_auth` at light-oauth's dedicated broker listener, verified
TLS-peer request context, registered certificate identities/authentication
methods, revocation checks, and broker key/certificate loading. Publish and wire
the listener, trust, registration, and certificate-rotation settings in the
selected stack. All broker grant/refresh/lookup/revocation paths enforce the
registered method, including requests sent to legacy endpoints.
Wire trusted client settings and runtime-secret mounts without placing client
secrets in workflow definitions.

Exit gate: after the original user JWT expires and the browser disconnects,
a scheduled run and an hours-long run obtain valid credentials under the same
approved identity/ceiling. Test restart, key rotation, concurrent renewal, lost
refresh responses, revoked grants, disabled users, removed roles, and issuer
outage. No stale-claim or service-account fallback is accepted.

For a multi-hour normal-load soak, record elapsed run-hours, renewal lead time,
refresh attempts, successful rotations, uncertain rotations, and reauthorization
counts/reasons. Report uncertainty per refresh attempt and the fraction of runs
requiring reauthorization. Report injected lost-response cases separately from
ordinary-load observations. With 600-second tokens, renewal occurs before each
expiry, so an hours-long run crosses many rotation boundaries. Measure this
operational cost without weakening strict rotation or treating a short,
failure-free sample as a production reliability guarantee.

Issuer-specific checks must also prove that:

- A `client_credentials` token carrying registered `uid`/`role` claims still
  has `token_use=app` and fails user `Authorization` verification. Registered
  and request-supplied purpose overrides cannot alter issuer purpose. Exercise
  all issuance/refresh paths, missing/unknown/conflicting markers, user tokens
  in `X-Scope-Token`, and markerless long-lived fixtures under the explicit
  app-only key-purpose profile. The exception never admits a token as a user.
- Removing a role/group/position or changing an authorization attribute affects
  the next refreshed token, including through any retained interactive grace
  path; unattended grants can never opt into that path.
- Locked/deleted users, removed tenant membership, and revoked sessions cannot
  renew; changing the current Portal host cannot change the grant's tenant.
- Added user permissions cannot expand the OAuth grant or workflow ceiling;
  permission-bearing custom claims cannot bypass the live query.
- A failed authorization query returns no token based on saved claims, and
  failed/concurrent rotation cannot return an uncommitted replacement.
- Authenticated reuse of a consumed unattended refresh token fails and revokes
  its family even inside the interactive grace window. Wrong-client and
  unauthenticated requests neither retrieve replacements nor revoke that family.
- The broker succeeds with its registered mTLS identity. Missing certificates,
  a different client's certificate, invalid/revoked certificates, forged peer
  headers, and secret-only requests fail before issuing or rotating a token,
  including at legacy endpoints. Exercise approved certificate renewal and
  retirement; stolen refresh-token/client-secret material without the mTLS key
  cannot renew. Existing non-broker client authentication remains compatible.
- Lost refresh responses and a crash before broker persistence fence the grant
  and require reauthorization; no automatic old-token retry retrieves a successor.
- Workflow enrollment rejects app tokens containing user-like custom claims and
  ineligible assertion grants. The broker client cannot use specialized grants
  to manufacture a substitute user session; separately approved clients retain
  their supported specialized use cases.
- An existing access token and a permission change racing with refresh obey the
  profile's documented enforcement delay; renewal is not claimed to revoke all
  previously issued access tokens.

### A2: Enforce Both Identities And Replace Nested Delegation

Apply the per-route user/app contract to Workflow, Agent, Gateway, API/MCP, and
Resource/Knowledge paths that the feature uses. Replace nested MCP minting with
broker-provided user credentials and the workflow's own app credential. Integrate
the fixed Workflow authorize/begin/complete/status APIs and Gateway client,
`workflow_action_dispatch_t` migrations and durable transitions, online dispatch
claims, audit decisions, and effect reconciliation. Implement the monotonic
send deadline, transport-level atomic send guard after connection/capacity
preparation, owner-only `NOT_INITIATED` completion,
and recovery/fencing rules. For the selected Pingora/reqwest outbound paths,
identify and qualify the transport write hook or adapter against pinned dependency
versions. A custom connector alone is sufficient only if it controls that final
write boundary, including reused connections. Explicitly wire and verify the
route-specific retry settings at every client/proxy layer; a handler-level
guard or reliance on default retry behavior does not qualify the path.
Provision separate interactive and
workflow Agent registrations/instances, isolated credentials, peer mappings,
and admission policies; Gateway classifies origin from verified credentials.
Replace delegated depth/class and private-target checks with the verified action
context, including Workflow-side child-lineage validation.

Exit gate: direct calls using a valid user token and an unauthorized app fail;
permitted gateway calls still require user tool/resource permission. A copied
gateway bearer from an unapproved transport peer fails the qualified gateway-only
profile. Test wrong issuer/audience/host, duplicate headers, missing claims,
renewal during a tool loop, replay across replicas, changed request payloads,
revoked action permits, exhausted budgets, and cancellation races.

Additional exit cases:

- Approved shared-audience service chains retain original-token forwarding;
  an unapproved demo/third-party target receives no platform bearer. App tokens
  cannot authenticate as users, and valid certificates from the wrong workload
  cannot authenticate as Gateway.
- A workflow Agent credential with no action reference, a forged interactive
  header, or an interactive URL is denied. Workflow callers cannot enter through
  the Chat Agent or select its credentials. Normal Chat still succeeds with its
  own registration; workflow Agent calls succeed only with active bound actions.
- Nested sync and async calls retain increasing depth and inherited limits;
  missing context, overflow, and excess depth deny admission. Saturating a root
  sync pool still permits a qualified child in its separate depth pool; a full
  child pool fails promptly without waiting on the parent. Restart/retry cannot
  reset depth or create a second child.
- Valid pinned private-version calls succeed and stay absent from discovery.
  Guessed IDs/names, wrong tenants/runs/versions, and dependency/contract drift
  fail; updating the public alias does not redirect an accepted pinned call.
- Across Gateway replicas, duplicate action claims authorize at most one
  dispatch. Workflow outage, stale generations, altered request digests, expired
  leases, and a lost decision response cannot bypass authorization or replay an
  uncertain effect. Crash before send intent, after its commit but before its
  acknowledgement, after sending, and before completion persistence. Recover on
  a different Gateway replica from the Workflow ledger; never resend a possibly
  executed effect without qualified reconciliation. Repeat with the old
  delegation replay table absent and with the new store unavailable.
- Use independently controlled wall clocks and local elapsed timers: large host
  clock offsets and wall-clock jumps cannot extend Gateway's five-second bound.
  Delayed authorize/begin replies, lock waits, local pauses, and earlier grant
  deadlines cause timely rejection. Restart or timer discontinuity invalidates
  local leases. Verify late begin claims fail on Workflow's own clock and retries
  cannot reset the original timer. Test revocation before and after the
  `SEND_INTENT` transaction, with no cached approvals.
- Delay the send-intent acknowledgement beyond Gateway's deadline: assert zero
  target requests, accepted owner-only `NOT_INITIATED`, one reservation release,
  and successful fresh authorization of the same canonical attempt without an
  operator. Also delay pooled-connection acquisition, TLS/protocol readiness,
  and capacity preparation after send intent: the guard stays `NOT_STARTED`,
  expiry produces `NOT_INITIATED`, and no target operation headers/body are sent.
  Race readiness and the deadline at the final write boundary: exactly one local
  guard transition wins, with no further queueing before the write. A started
  request cannot report `NOT_INITIATED`; expiry after that transition cannot
  trigger a late write or automatic resend. Duplicate completions cannot
  double-release; stale owners/boots/generations are rejected.
  Test a lost completion response, a crash before completion persistence, and
  grant revocation before retry: only recorded non-initiation enables this safe
  retry of a `SEND_INTENT` attempt, and fresh authorization still enforces
  revocation and retry budgets.
- Fail a reused upstream connection at the first write and after a partial
  write. Verify the actual configured transport never replays the operation on
  a fresh connection, even if the request body is replayable. Count outbound
  request initiations as well as target observations: only one initiation may
  cross the guard, and an unknown outcome remains `UNCERTAIN`. Exercise each
  qualified outbound client/proxy path with its effective retry settings.

### A3: Migrate, Remove Old Authority, And Qualify Orchestration

Migrate active invocations to valid grant references through authorized
reauthorization or drain them; an expired persisted bearer is not enrollment
proof. Stop persisting reusable access tokens in ordinary invocation state.
Coordinate sender/receiver rollout so routes cannot fall back to weaker
authentication when a new credential fails.

Inventory the app tokens, issuer/signing trust, mTLS identities, and forwarding
profiles used by the selected stack. Keep deliberate local-development fixtures
in Git; official installations use independent runtime credentials and reject
those fixtures. If an official installation has reused development credentials
or exposed its own tokens, replace/revoke the affected credentials and remove
their trust before qualification. A long expiry alone is not a reason to remove
the supported app-token profile, and blanket rotation of development fixtures
is not a prerequisite. Test the actual user/app/certificate combinations at the
official boundary rather than relying on a `dev` label.

Remove workflow delegation signer/verifier wiring, its deployment secrets,
and `crates/agent-delegation`/`agent_delegation_replay_t` only after verifying no
remaining consumer and qualifying their replacement protections. Preserve
active evidence and recovery behavior during schema/config migration. Install
and qualify `workflow_action_dispatch_t` and all fixed dispatch APIs before
removing the old replay table. Drain/reconcile old delegation attempts first;
their replay rows cannot be treated as evidence of send outcome. The new ledger
and its unresolved attempts are retained across A3 and subsequent restarts.

Exit gate: run the complete authenticated API/MCP/Knowledge paths, scheduled
execution, renewal, revocation, and effect-recovery cases on the selected stack.
Verify that specialized grant support, local shared-audience forwarding, and
development startup remain compatible with the explicitly selected profiles.
Then admit development orchestration Phase 1. Enterprise accounting and sandbox
qualification remain additional profile gates, not replacements for this work.

## References

- [Issue #374](https://github.com/networknt/light-fabric/issues/374)
- [LLM User and Agent Authorization](../product/light-agent/llm-dual-token-authorization.md)
- [Workflow-Backed MCP Tools](../product/light-gateway/workflow-backed-mcp-tools.md)
- [Stateless Auth Handler](stateless-auth.md)
- [MSAL Exchange Handler](msal-exchange.md)
- [Token Handler](token-handler.md)
