# Codex embedded prototype

This isolated crate compile-probes the exact `codex-core` revision associated
with Codex `0.153.2` and measures only the typed-call versus JSON boundary cost.
It does not make a model call, read native credentials, or claim behavioral
parity with App Server.

The upstream Rust crates are not a documented stable SDK and currently pull a
large internal workspace graph. Their root lock and Cargo patches are part of
the compatibility surface. Run the opt-in probe with:

```bash
LIGHT_RUN_CODEX_EMBEDDED_PROBE=1 ./scripts/run-coding-harness-phase5-gates.sh
```

The adapter remains `prototype-only` and cannot be selected by a worker until
all qualification dimensions have independent evidence.

