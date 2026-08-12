# Workflow Invocation Contract V1

This directory is the Phase 0 interoperability package for workflow-backed MCP
tools. It is authoritative for the gateway, Portal publication, and
`light-workflow` invocation boundary. The Phase 1 runtime implements this
contract, while production promotion remains fail-closed until the manifest's
CEL and end-to-end topology evidence is satisfied.

Files:

- `openapi.yaml` defines start, status, wait, result, and cancellation.
- `start-request.schema.json`, `delegation-claims.schema.json`, and
  `publication-event.schema.json` are strict publication/runtime schemas.
- `mcp-result.schema.json` pins the one-text-item plus `structuredContent`
  rendering contract for compact JSON, summaries, and technical errors.
- `fixtures/` contains cross-runtime positive and negative vectors.
- `threat-model.md` records trust boundaries, threats, and required evidence.

Canonical input uses `rfc8785-safe-json-v1`: recursive RFC 8785 UTF-16 property
ordering, preserved arrays and Unicode code points, distinct absent and `null`,
duplicate-key rejection, and JSON integers restricted to the interoperable
range ±(2^53-1). Decimal values and larger identifiers are schema-declared
strings in V1. This deliberate narrowing avoids claiming an ECMAScript
binary64 formatter that the currently selected Rust stack does not provide.

`qualification-manifest.json` freezes the numeric Phase 0/Phase 1 handoff
gates. A selected threshold is not promotion evidence: the manifest remains
fail-closed until a report for the exact build and topology satisfies every
required scenario.
