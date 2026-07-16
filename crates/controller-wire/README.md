# controller-wire

`controller-wire` is the dependency-light, transport-neutral source of truth
for controller wire-profile tokens, framing, archived roots, and validation.
The crate has its own package version and can be pinned or published
independently from runtime and gateway applications.

Version 1 policy:

- `rkyv` 0.8.12;
- little-endian, aligned archives with 32-bit relative pointers;
- bytecheck validation and UUID 1 integration;
- no unchecked archive access or unsafe code;
- fixed-width integers, sorted tag vectors, and bounded dynamic JSON bytes; and
- immutable released root field order and message-kind assignments.

Run its focused gates from the workspace root:

```bash
cargo test -p controller-wire
./scripts/check-controller-wire-deps.sh
./scripts/check-controller-wire-features.sh
./scripts/check-controller-wire-safety.sh
./scripts/check-controller-wire-targets.sh
```

No transport listener or negotiation policy belongs in this crate.
