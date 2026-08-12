# Workflow-Backed MCP Phase 0 Threat Model

## Assets and boundaries

- The gateway owns caller authentication, logical-tool authorization, argument
  filtering, admission, and final MCP rendering.
- The invocation service owns durable acceptance, idempotency, mutable budgets,
  workflow state, public output, and cancellation.
- Portal owns immutable publication bundles, dependency impact analysis, and
  promotion/retirement approval.
- Delegation tokens authenticate immutable ceilings and ledger identity; they
  never carry authoritative mutable remaining counters.

## Required threat controls

| Threat | Required control and Phase 0 evidence |
|---|---|
| Cross-tenant instance lookup | Tenant and subject binding on every operation; negative fixtures and database composite keys. |
| Definition or policy substitution | Exact workflow, schema, policy, and response-policy digests stored at acceptance and compared on every start/replay. |
| Duplicate side effects | Atomic scoped idempotency reservation; changed input on an explicit key is a conflict; event replay dedup remains separate. |
| Copied-token budget amplification | Durable conditional budget reservation and fenced idempotent reconciliation. |
| Nested permit deadlock | Non-borrowable pools per signed depth; direct callers enter only depth zero. |
| Priority spoofing | Effective execution class selected at root and inherited only from a signed delegation token. |
| Private-target ACL bypass | Private dispatch carries the logical tool name and endpoint key; dispatch identity never becomes authorization identity. |
| Dependency retirement outage | Reverse-index gate blocks retirement while active outer references exist; emergency revocation is explicit and audited. |
| Poison event partition outage | Direct acceptance does not depend on the shared event log; quarantine retains replayable payload and ordering evidence. |
| Stale worker commit | Renewable lease and monotonically increasing fencing token on task completion/transition. |
| Post-effect retry | Durable `none`/`possible`/`confirmed` effect state; possible or confirmed work requires proven downstream idempotency before replay. |
| Result disclosure after revocation | Stored response policy is a ceiling; current subject authorization is evaluated on every result read. |
| Canonicalization ambiguity | Strict duplicate rejection, versioned canonical profile, UTF-16 key ordering, safe integers, preserved Unicode, shared golden vectors. |
| CEL resource exhaustion or type drift | Phase 0 decision: current `cel` 0.14 is not qualified for value evaluation because it lacks schema checking and deterministic cost hooks. Phase 1 cannot ship until an augmented or replacement evaluator passes the pinned conversion/cost suite. |

## Phase 0 release decision

The wire, storage, idempotency, budget, and canonicalization contracts are
implementable and versioned. The existing CEL crate and host-task scheduler are
not production-qualified by declaration. Phase 1 remains gated on measured
end-to-end latency/fairness/crash evidence and a CEL implementation satisfying
the published checker, cost, and CEL-to-JSON contract.

