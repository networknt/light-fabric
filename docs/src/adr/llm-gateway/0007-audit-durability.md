# ADR 0007: Audit Durability Profiles and Group Commit

- Status: Accepted
- Date: 2026-07-18
- Gate: LF-2 evidence; implementation before LF-8

## Decision

The named profiles are `best-effort`, `bounded-async`,
`local-durable`, and `remote-durable`. MVP implements bounded-async as the
production default and local-durable under a separate SLO. `required` is an
admission policy, not a durability level.

Bounded-async reserves the complete metadata envelope and bounded queue/spool
capacity before dispatch but does not wait for disk commit. Local-durable waits
until every attempt-start record reaches the WAL durable watermark. The WAL is
single-writer, length-delimited, checksummed, sequence-numbered, and uses group
`fdatasync`; recovery stops and reports any corrupt/truncated committed
record. Full/read-only storage fails admission for required profiles rather than
silently degrading.

Remote-durable is reserved for a later authoritative sink transaction.
Best-effort is development-only and counts loss.

## Evidence

`benchmarks/llm-gateway/evidence/wal.json` measures grouped synchronization,
durable watermark/recovery, truncated-tail detection, and fail-closed capacity
and read-only behavior. It is feasibility evidence, not the production WAL
implementation.

