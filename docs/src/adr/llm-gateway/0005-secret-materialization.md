# ADR 0005: Off-Path Secret Materialization

- Status: Accepted
- Date: 2026-07-18
- Gate: LF-2 before production provider publication

## Decision

Portal resources carry only `credential://` reference IDs. The first
production resolver seam is the gateway's already resolved runtime
configuration: config-loader decrypts CRYPT values, RuntimeConfig exposes
resolved values to authorized module construction, and ModuleRegistry masks
sensitive values in inspection output. A later provider integration must
implement the same narrow `SecretResolver` contract rather than changing the
request path.

Reload performs three stages: parse/validate the secret-free resource graph;
authorize and resolve every enabled credential reference and construct reusable
clients; publish only the fully materialized root. Resolution, decryption, token
exchange, and client construction never occur during inference.

Missing, denied, expired, blank, or malformed references reject the candidate
and preserve the last valid root. Runtime config reload is the rotation
notification. Rotation rebuilds only affected provider subgraphs; in-flight
requests may retain the old secret-bearing client Arc until their old root
retires.

Ordinary logs, metrics, traces, audit events, projection/root digests, benchmark
artifacts, crash reports, and module inspection contain neither secret values
nor credential reference IDs. Repair-only operator diagnostics require explicit
authorization and still prefer deployment/error IDs.

## Evidence

`projection-secret.json` exercises success, missing, denied, rotation,
redaction, last-valid-root, and zero request-time lookup assertions.

