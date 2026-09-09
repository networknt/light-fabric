# MCP support and migration

| Revision / transport | Gateway frontend | Gateway backend | Reusable HTTP client |
|---|---|---|---|
| 2026-07-28 Streamable HTTP | Opt-in, discover/list/call and bounded subscriptions | Explicit stateless profile | Modern default, discover/list/call |
| 2025-11-25 Streamable HTTP | Default legacy target | Explicit legacy adapter | Explicit initialized/session adapter |
| 2025-06-18 Streamable HTTP | Retained compatibility | Legacy adapter | Explicit adapter |
| 2025-03-26 Streamable HTTP | Retained, session-bound batches | Legacy adapter | Explicit single-request adapter |
| 2024-11-05 | Rejected | Rejected, including negotiated responses | Unsupported |
| Original two-endpoint HTTP+SSE | Unsupported | Unsupported | Unsupported |

The gateway is a tools facade, not an implementation of every optional MCP
capability. Resources, prompts, completion, MRTR, sampling/elicitation/roots,
Tasks and Apps are not advertised. The client handles JSON and request-scoped
SSE, not subscriptions or independent server requests. Controller WebSocket
contracts remain separate and use their existing initialized control profile.

Legacy arbitrary-root output is adapted to an object with a `value` property
and matching output schema for June/November 2025. March retains text content
and omits unsupported structuredContent/outputSchema fields. Modern calls keep
arbitrary JSON structured output. Modern-to-legacy backend calls remain rejected;
explicit sessionIndependent does not create an implicit bridge.

2024 retirement is effective immediately on 2026-09-09 for this development
release. The product owner confirmed there are no 2024 consumers and authorized
the cutoff in networknt/light-fabric#379's implementation discussion. No consumer
migration or advance notice is required. Explicit 2024 configuration fails
validation and cannot re-enable the retired adapter.

Drain bounded in-flight work before replacing running binaries. Reload evicts
retired frontend sessions, prunes retired backend sessions even for retained
frontends, and requests backend DELETE cleanup. Clients reinitialize their
selected supported legacy profile after eviction; mutations are never silently
replayed under another profile. Rollback configuration should keep November
2025 and stateless disabled, never restore 2024. Local deployment qualification is recorded under remediation R6; production
rollout is outside this development task.
