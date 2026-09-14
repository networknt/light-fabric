# Authorization A3 Migration and Removal Progress

Status: **A3 has started; removal is not yet safe.** The selected local A2 stack
starts with separate workflow-only Agent identities, and new grant-backed action
invocations no longer persist the caller's reusable access token in ordinary
Workflow invocation state. The complete A2 integration matrix and the A3
consumer drain remain open.

## First migration slice

- Workflow action admission still authenticates the current user and immediate
  caller, binds the run to the approved renewable grant, and records the action
  authority and dispatch ledger.
- For the dedicated grant-backed action listener, `user_authorization` and
  `user_authorization_exp` are inserted as null. The broker remains the source of
  fresh credentials for bound MCP actions and workflow Agent jobs.
- A later authenticated status read cannot repopulate a deliberately null token.
  Ordinary interactive invocations retain the existing token behavior until a
  qualified non-durable credential handoff replaces it.
- Terminal cleanup remains idempotent. Existing active rows have not been
  rewritten: rows with valid grant authority must be migrated, while legacy rows
  without it must drain or be reauthorized.

## Selected-stack credential inventory

| Workload | App credential | Peer identity | Current purpose |
| --- | --- | --- | --- |
| Gateway | issuer-signed app token | Gateway client certificate | Workflow action control and backend dispatch |
| Workflow | issuer-signed app token | Workflow server/client certificates | Action API and Gateway calls |
| Codex workflow Agent | issuer-signed app token with `execution.invoke` | Codex client/server certificates | Workflow jobs and Controller execution API |
| Claude workflow Agent | issuer-signed app token with `execution.invoke` | Claude client/server certificates | Workflow jobs and Controller execution API |

The local preparation tool generates separate app tokens and mTLS material for
these identities. Gateway and Workflow keep `portal.r portal.w`; only the two
Agent identities receive `execution.invoke`. Private PKI remains runtime-owned
and is not checked into Git.

## Legacy authority still present

The `agent-delegation` crate still has five direct package consumers:
`light-agent`, `light-workflow`, `light-gateway`, `light-knowledge`, and
`light-pingora`. Workflow still has signer configuration; Gateway and Knowledge
still verify legacy delegation; Pingora still derives nested workflow context
from it. The Agent now requires its legacy signer only when a direct Knowledge
endpoint is configured, which removes the dependency from workflow-only Agents
without weakening configured Knowledge access.

`agent_delegation_replay_t` also remains in the Agent operational schema and its
validation/reset tooling. It cannot be dropped until old attempts are drained
and the `workflow_action_dispatch_t` evidence/recovery path passes the selected
stack's first-write, partial-write, uncertainty, restart and receiver-receipt
tests.

## Remaining gates before removal

1. Run the complete A2 authenticated action matrix, including workflow Agent and
   Knowledge paths, cancellation, unauthorized callers, nested depth, private
   targets and effect recovery.
2. Qualify enrollment, scheduled renewal, rotation, revocation, permission
   changes and crash recovery against the selected issuer profile.
3. Migrate active grant-backed invocation rows to null token storage. Drain or
   explicitly reauthorize active legacy rows that have no valid grant authority.
4. Replace each remaining Gateway/Pingora/Knowledge delegation decision with the
   live action and receiver contracts, then prove no legacy consumer remains.
5. Remove signer/verifier configuration and deployment secrets in one
   coordinated rollout. Only then remove `agent-delegation` and apply a migration
   that drops `agent_delegation_replay_t` while retaining the action ledger and
   unresolved dispatch evidence.
