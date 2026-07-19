# ADR 0003: One ArcSwap Runtime Root Per Request

- Status: Accepted
- Date: 2026-07-18
- Gate: LF-2 before LF-5

## Decision

The LLM runtime publishes one immutable `Arc<CompiledLlmRoot>` through
`ArcSwap`. Request admission captures the root once and all routing, provider,
policy, pricing, accounting, and client choices come from that Arc. The request
path must not repeat current-config reads.

The reload worker builds and validates a complete candidate off-path, reuses
unchanged Arc subgraphs, materializes clients/secrets, and performs one atomic
store. A failed candidate leaves the previous root active. Dynamic counters and
circuit state have stable identities and are not rebuilt merely because the
configuration root changes. Retired roots live until the last in-flight Arc is
dropped.

## Evidence

`benchmarks/llm-gateway/evidence/snapshot.json` compares repeated
`light_runtime::ConfigManager` RwLock reads with a single capture through the
existing ArcSwap-backed `config_loader::ConfigManager`, and proves the captured
root remains generation-coherent across publication.

