# ADR 0001: LLM Public Compatibility Profile

- Status: Accepted
- Date: 2026-07-18
- Gate: LF-2 before LF-3

## Decision

The first public surface is OpenAI Chat Completions: buffered JSON, SSE, and
`GET /v1/models`. Models are authorized aliases, never provider deployment
identifiers. Errors use the OpenAI envelope while retaining a typed, sanitized
internal category.

A typed parse failure is terminal. Same-format OpenAI forwarding may retain
unknown fields only in a size-bounded compatibility envelope and only for an
alias/deployment allowlist. Cross-format OpenAI-to-Anthropic routing uses
canonical typed content and rejects unrepresentable fields. No detect-and-opaque
fallback is allowed.

The checked-in corpus under `benchmarks/llm-gateway/payloads` is the Phase 0
compatibility baseline. Account/CLI providers are not eligible for the shared
gateway.

## Consequences

LF-3 codecs can define one closed canonical model. Compatibility behavior is
observable and cannot turn parse failures into arbitrary upstream forwarding.

