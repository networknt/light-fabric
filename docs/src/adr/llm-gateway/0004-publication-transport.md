# ADR 0004: Config-Server File Projection Transport

- Status: Accepted
- Date: 2026-07-18
- Gate: LF-2 before PDB-1, LP-1, and DIST-1

## Decision

Production topology uses the existing config-server `/files` snapshot delivery
seam. The runtime bootstrap already materializes remote files in
`config-cache`; the LLM projection is delivered there as one root manifest and
immutable resource files. Development may point to checked-in fixtures, but
production may not read arbitrary local topology.

Resources and manifests use UTF-8 canonical JSON: object keys sorted
lexicographically, no insignificant whitespace, and standard JSON scalar
encoding. A resource SHA-256 covers every field except `digest`. A root
SHA-256 covers every manifest field except `rootDigest`. Secret values are
never included. Digests are computed from the in-memory canonical serialization,
not from editor-specific line endings. Checked-in canonical fixtures may have
trailing ASCII whitespace, which is excluded only when verifying the fixture's
byte-for-byte canonical form.

Sequences are monotonic per host/environment. The next new publication must be
exactly last-applied + 1. An identical sequence/digest is an idempotent
duplicate; a conflicting duplicate or a gap rejects the delta and triggers a
full resync. Deletes are explicit tombstoned manifest entries. Full resync
fetches the manifest first, then every referenced immutable resource, with
bounded pagination/artifact size, validates the complete graph, and publishes
one root.

The acknowledgement is `{hostId, environment, sequence, rootDigest,
appliedAt, gatewayVersion}`. Unknown schema versions or a
`minimumGatewayVersion` newer than the runtime reject the candidate. The last
valid root remains active on all fetch, digest, schema, ordering, compatibility,
or compilation failures.

## Fixtures

The schemas and canonical digest fixtures are under
`benchmarks/llm-gateway/schemas` and `manifests/projection-*.json`.
