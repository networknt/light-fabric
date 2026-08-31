# A2A Phase 0 Contract Baseline

This directory freezes the inputs shared by the Portal Java publication
compiler and the Rust A2A runtimes. It is a contract baseline, not a claim that
the current implementation passes the complete A2A TCK.

## Pinned upstream inputs

- The [official A2A repository](https://github.com/a2aproject/A2A) supplies A2A
  1.0 tag `v1.0.1`, commit
  `3303592588e388e62e0f69f701af531d2f4e3991`.
- The compatibility profile is A2A tag `v0.3.0`, commit
  `210f03d426e2f2fa92000e14ef0de3b7ba15aee5`.
- The [official A2A TCK](https://github.com/a2aproject/a2a-tck) reports version
  `1.0.0`; Phase 0 pins commit
  `5996b79f9cefa6fc390980e383e358a66fb9e49e`.
- `light-a2a-projection-json-v1` sorts JSON object keys recursively and
  preserves array order. It intentionally does not claim RFC 8785/JCS numeric
  canonicalization.

## Existing implementation inventory

Reusable:

- `a2a-core` authorized-invocation signing, verification, task states, runtime
  identity, and projection canonicalization;
- `a2a-store` task admission, ownership, idempotency, and cancellation;
- native A2A operational persistence in `agent-store`;
- the `light-a2a` service shell and Portal A2A publication compiler; and
- Portal A2A binding/publication schema and relationship identifiers.

Incomplete:

- complete 0.3 and 1.0 protocol models, version negotiation, Agent Card
  publication, extension handling, and normative error mapping;
- the light-gateway A2A router and Instance-API resolution;
- production command/query wiring for publication and discovery;
- external backend transports, SDKs, streaming, and the complete TCK gate.

Temporary:

- the local JSON-RPC request structs and handlers in `light-agent` and
  `light-a2a`; later phases replace them with the shared versioned server
  module without changing the Phase 0 identity or digest contracts.

## Cutover rule

There is no production A2A data to migrate. Runtime projection identity is only
`host`, `serviceId`, and `envTag`. Portal `instanceId` and `instanceApiId` remain
relationship and audit evidence and must never be used as workload identity.
