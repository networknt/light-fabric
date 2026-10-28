# Router rewriting: code-only qualification profile

The owner-approved Init/backend handoff is implemented. The ordinary application
still selects `ApplicationRouterPolicyProvider` and rejects nonempty rewrite
rules at Init. The qualified release ZIP is unchanged; none of the following is
new package/RIE or Java-child qualification.

`input_with_router_qualification` is explicit trusted Rust composition. It uses
`QualificationRouterPolicyProvider`, captured sources, the immutable program,
the real RouterHandler factory and controller-backed terminal. No event, layer,
environment switch or ordinary CLI selects a child executable. A real
`JavaControllerFactory` delegates to the existing pinned-artifact Java controller;
constructing it does not verify/launch artifacts. Runtime lifecycle activation
does that; offline validation rejects before controller construction.

The selected qualification loading profile is fixed at 1 MiB/source, 4 MiB/raw
aggregate, 64 rules, 16 KiB/rule, 256 KiB/rule aggregate and 8192 UTF-16 units per
pattern/replacement. These are approved qualification inputs, not production
defaults or demonstrated memory bounds. Existing config-loader expansion/tree/
effective-document restrictions remain in force and may reject earlier (including
the effective-document 64 KiB limit). The pure program's aggregate boundary test
is not proof that an equally large application configuration is admitted.

Actual gateway request/response limits are passed from Init to the HTTP backend;
the provider does not substitute `Limits::default()`. Java tokenization,
pattern/replacement bytes/order and executor semantics are retained. Request
policies do not rewrite early: terminal dispatch uses the final invocation path.
Errors never dispatch the original path or fall through to Lambda.

| Outcome | Response |
|---|---|
| Failed request state | Existing state-derived ERR10087/400, before mutation/selection/controller/transport; first cause stays private |
| Deadline/cancellation | Existing ERR14017/504; queue/rewrite/send use the same invocation deadline |
| UTF-16/replacement/protocol/panic defects | Existing ERR14004/500 |
| Listed runtime unavailable/resource/profile/child/no-ready/cooldown/stale outcomes | ERR14035/503, ROUTER_REWRITE_UNAVAILABLE, `Router rewrite execution is unavailable.` |
| HTTP transport failure | Existing ERR10089/400 |
| Initialization/artifact/program rejection | Redacted InitError; no published Snapshot |

Init activates lifecycles before publication. Failed or panicking activation
unwinds partially started owners in reverse; later Init failure also awaits
rollback. Cancellation does not cancel cleanup already started. The final
Snapshot owns successful lifecycles, and final drop schedules cleanup on the
live runtime. The real controller ownership barrier waits for preparation and
private actor permits; it is not a substitute for actual child identity/exit/
owning-process wait/reap observation. Dropping a Snapshot after its Tokio runtime
has already gone away does not provide an async cleanup guarantee.

Permitted non-child rerun from the worktree:

```bash
rtk proxy env CARGO_INCREMENTAL=0 cargo test --locked --offline -p light-lambda --test router_rewrite_wiring
```

Tests use an in-process lifecycle endpoint that cannot construct successful
rewrite Candidates, plus real-controller invalid-artifact rejection before any
Java command construction. They exercise actual application Init/factory/service
and terminal error handling, not regex semantics. No replacement worker is used.
Default admission/ordinary forwarding is covered separately by built-app tests.

Real Java execution remains paused. A separately reviewed/pinned Java bundle,
worker controls, named allowed application test and platform clearance are still
required before actual rewriting/cleanup qualification. Validator precision
execution remains paused too. No AWS or full WP12/WP13 qualification is claimed.

Evidence: [checkpoint](/home/steve/workspace/artifacts/implementation/light-lambda/WP13-router-wiring-20261021T180432Z/CHECKPOINT.md)
and [individual bundle mismatches](/home/steve/workspace/artifacts/implementation/light-lambda/WP13-router-wiring-20261021T180432Z/BUNDLE-MISMATCHES.md).
