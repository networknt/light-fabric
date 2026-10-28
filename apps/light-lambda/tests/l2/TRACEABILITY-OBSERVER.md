# Test-only Traceability package observation

Existing consumers were inspected first:

| Consumer | Suitable proof |
|---|---|
| Lambda TimedBackend dispatch span | Reads actual ExchangeState.traceability_id, but baseline formatter suppresses current/span-list fields; no baseline log observation |
| Conformance trace_observer::BackendObserver | Reads actual state and delegates to real SDK backend via public init::with_backend_observer; suitable existing qualification composition |
| Head-handler/component capture | Real state proof, but component execution alone is not packaged/RIE evidence |
| Gateway-policy correlation response echo | Different adapter/profile; would not prove the registered Lambda Traceability handler |
| Audit selected headers / SDK forwarded header | Reads invocation headers rather than private Traceability state; not sufficient |

`examples/test-only-traceability.rs` uses the existing public observer hook after
real application Init, then calls the production runtime/service with the same
guard. It reads actual exchange state at each real SDK dispatch and delegates
the original invocation/state/target/response/error unchanged. No replacement
handler, fake state or expected response is injected.

Observation is explicit test-only stdout JSON. Null stays null; only fixed
`trace-probe-A`/`trace-probe-B` witnesses may be printed, and all other present
values become `<redacted>` with statePresent=true. Production telemetry/privacy
initialization is unchanged. This observes handler state, not hidden log-span
fields or a new production diagnostic contract.

No shared production symbol, dependency or Cargo target declaration changed.
New example/test files use already installed dependencies and the public
qualification hook. Plan §3.2 exempts new files/symbols from impact analysis;
this user request authorizes the focused test. **No new approval is required.**
GitNexus did not index the observer hook; source review supplies this finding,
not a claim of zero-risk graph coverage.

## Separately identified instrumented package

Profile: `test-only-traceability-observer-v1`, **not baseline qualification**.

- ZIP SHA-256: `279fa7d35fb534d955623a19f10d8dee407e8a1398777eb6e981f2e0cb5a92f8`
- ELF SHA-256: `a6a810a7d3f9681f348ddaacd68d451d1c091fbd4b554d6139ae33b8e7c76cac`
- Evidence: `/home/steve/workspace/artifacts/implementation/light-lambda/WP16-traceability-observer-20261019T150000Z`
- Exact source/build identity: instrumented-package-manifest.json in that directory.

The ZIP still contains only executable bootstrap0755 and passes the existing
AL2023 x86-64 ELF/dependency/GLIBC format checks. Its code is the explicitly
instrumented example, not the production main target. No two-clean-build
reproducibility claim is made for this instrumented package. The existing
reproducible baseline ZIP/ELF identities remain unchanged.

Copy-paste rerun from `/home/steve/workspace/light-fabric-light-lambda`:

```bash
rtk proxy python3 -B apps/light-lambda/tests/l2/test-only-traceability.py \
  --instrumented-package /home/steve/workspace/artifacts/implementation/light-lambda/WP16-traceability-observer-20261019T150000Z/test-only-traceability.zip \
  --baseline-bootstrap /home/steve/workspace/artifacts/implementation/light-lambda/WP14-WP16-baseline-20261019T115626Z/reproducible-package-v2/bootstrap \
  --out /home/steve/workspace/artifacts/implementation/light-lambda/my-traceability-observer-rerun
```

Build the example into a **fresh** instrumented directory, then package the
produced ELF as bootstrap, as recorded in package_observer.py and the manifest:

```bash
rtk proxy env CARGO_INCREMENTAL=0 /home/steve/workspace/artifacts/implementation/light-lambda/WP14-WP16-baseline-20261019T115626Z/tools/cargo-lambda-1.9.2/cargo-lambda lambda build \
  --locked --offline --profile lambda -p light-lambda --example test-only-traceability \
  --target x86_64-unknown-linux-gnu.2.34 --output-format zip \
  --lambda-dir /home/steve/workspace/artifacts/implementation/light-lambda/my-traceability-build
rtk proxy env CARGO_INCREMENTAL=0 cargo test --locked --offline -p light-lambda --example test-only-traceability
```

The built raw cargo-lambda ZIP is also usable by --instrumented-package; record
its own digest rather than attributing the canonical artifact identity to it.

## Focused results

Enabled chain: six service requests. Actual state sequence A/null/B/null/
redacted/A, including502 ERR14014 and warm success. One additional malformed
ingress400 ERR14012 has zero dispatch/state capture and counts as admission
only. Disabled-control chain: two service requests, A-header then absent; both
actual states null. Eight real signed Invoke calls, LogType=None, zero JWKS.
Two pure observation privacy/absence tests and focused example Clippy/fmt pass.

The existing89-request completed suite was not rerun. Default package remains
22 qualified aliases with Traceability's hidden-state limitation; this adds
**instrumented-only Traceability state proof**. Validator, Router rewriting,
paused precision experiments and AWS remain excluded.
