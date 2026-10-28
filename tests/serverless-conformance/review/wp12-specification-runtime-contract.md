# WP12 approved bounded-input Specification runtime contract

Owner approved runtime-proposal.md: selected active Specification execution checks Invocation.path UTF-8 byte length <=8192 before any path copy, match, permit acquisition or task submission. Existing deadline/cancellation precedence remains. Oversize is static414 / ERR14029 / SPECIFICATION_PATH_TOO_LARGE / `Request path exceeds the supported Specification size.` The code was checked against the source Java catalogue,134hash-verified WP2 artifacts/status resources,Rust sources and authoritative plan; no previous allocation found. This is a Rust safety limit, not a customer/ALB limit.

Two process-wide permits shared across all Specification instances; acquire before bounded input ownership/submission and hold inside closure until actual completion. Only isolated bounded input and immutable matching configuration enter workers. Discard late results, preserve ingress/Route/principal, no retry/redispatch, ordinary once-only response/finalization on denial. No absolute elapsed-time/total-memory guarantee. Runtime implementation and qualification are pending a new source-reviewed HIGH integration-scope disposition; Specification stays unregistered.

## Explicit runtime expectation amendment; original evidence unchanged

Preserve the entire392-case parsing/Java normalization matrix, including original no-cutoff evidence. These declarations are prototype/component inputs, not approved WP13 invocation fixtures:

| Original matrix case | Prior normalization expectation | Approved selected runtime expectation |
|---|---|---|
| 307 / no-new-path-cutoff-4096 | Normalize4096bytes | Still admitted by the path bound; normal first-match semantics |
| 308 / no-new-path-cutoff-4097 | Normalize4097bytes | Still admitted by the path bound; normal first-match semantics |
| 309 / no-new-path-cutoff-1048576 | Normalize1MiB | **414 ERR14029**,zero worker/copy/permit/match/provider/backend dispatch; ordinary single response-policy/finalization |

This changes runtime admission only, not regex parsing/configuration admission or Java observations. No approved v1 fixture outcome conflict identified from current native declarations; no fixture expected outcomes/DIGEST/admission history rewritten. Runtime tests must explicitly add8192accepted/8193rejected including multibyte UTF-8,initial expiry/cancellation overriding414,saturation across separate handlers,waiting/running cancellation,permit lifetime,panic cleanup,late result/warm isolation,positive normalization and real JWT scope/cache/count behavior. Declarations here are not executed qualification.

## Subsequent accepted integration and executed qualification

Owner accepted SpecificationHandler.on_request sourceHIGH plus bounded neutral entry,worker and status helper; graphLOW0/stale missing callers/failed refresh remain preserved. Integration now enforces8192admission and process-wide2permit lifetime with isolated inputs/results,deadline/cancel arbitration and completed-view-only publication. Constructor/pipeline/finalizer behavior unchanged. No index repair. Selected1MiB runtime outcome explicitly qualified414zero backend/once-only response and finalization; original392goldens/Java observations unchanged.

Evidence: WP12/specification-runtime-qualified-20261014T140008Z/qualification-summary.md.228affected tests and29built-appL1baseline tests passed;worker lifecycle/races/scratch/stress qualified with measured limits,not elapsed/RSS guarantees. Positive base-path regression unchanged/now passes. Specification still unregistered: new app-local-source offline/startup gate0pass/1fail/exit101 awaits new source-reviewedHIGH factory/preparation/identity scopes. No fixture invocation corpus execution or WP13 comparison claimed.
