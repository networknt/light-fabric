# WP12 local Specification source admission and identity

Owner disposition, 2026-10-14: select `openapi.yml` → `openapi.yaml` →
`openapi.json` → `spec.yaml`. The `openapi.*` order preserves the preserved
Java Config implementation; `spec.yaml` is explicitly a Rust-only fallback.
For each filename, preserve external → application → default → embedded
precedence. First present wins; selected malformed, unreadable, non-file or
oversized sources reject Init rather than falling back. The approved ordered
loader's 1 MiB source bound and regex profile remain unchanged.

One bounded raw capture supplies both the actual factory and its source identity.
The identity domain-separates the selected filename and exact raw bytes. Active
Specification configuration digests replace canonical specification documents
with that captured identity, excluding ignored competitors and never reopening
the captured source. Identity-absent providers retain the legacy digest path.
Declaration-order changes therefore change the digest; post-Init file mutation
cannot replace the immutable factory or authorize a different operation.

KEY-004's approved effective-pattern restriction and exact redacted profile error
are unchanged. KEY-344/345/346 (`multipleSpec`, `ignoreInvalidPath`,
`pathSpecMapping`) remain unsupported: absent/default false/empty-map values
produce warnings; non-default/malformed values fail offline/startup validation
even if disabled or unselected. No new authentication factor is admitted.

Targeted real-provider tests and built-app loopback L1 qualify source preparation,
matching, ordered operation scopes, real JWT denial/cache behavior and snapshot
isolation. They do not execute WP13 differential conformance or establish
request-schema validation. Existing Java observations and fixture expectations
are unchanged; no fixture declaration or corpus DIGEST change is needed.
Whole-corpus admission still validates 480 declarations.

WP5 remains open; neither local precedence nor loopback qualification establishes
customer layer contents, deployed configuration or operational readiness.
