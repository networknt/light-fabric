# Operational Store Registration Contract V2

This fixture freezes the Host-scoped registration consumed by Rust runtimes and
shared with the Java and TypeScript contract tests.

- scope is one active registration per `hostId`;
- `environment` is not a registration field;
- plaintext passwords and password-bearing URLs are not accepted;
- lifecycle values are `REGISTERED`, `DEACTIVATED`, and `UNREGISTERED`;
- updates require `aggregateVersion`; and
- `bindingDigest` is SHA-256 over RFC 8785 canonical JSON containing the ordered
  field set in `registration::DIGEST_FIELDS`.

Version-1 provisioning events remain replayable without side effects and require
an explicit command for conversion. This contract does not enable registration
commands or change current runtime loading; those are later implementation phases.
