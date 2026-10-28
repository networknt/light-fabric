# Guarded Light Lambda merge qualification

This change adds the Rust Lambda gateway, shared gateway-policy extraction,
serverless AWS transport, conformance fixtures and an explicit in-process
JSON Schema Validator entry. Ordinary `bootstrap` keeps its existing activation
policy. Selecting `validator-in-process` is a separate build choice.

## Review map

- `crates/gateway-policy` and `frameworks/light-pingora`: policy extraction and
  adapters; shared security/runtime/client changes retain the current master
  authentication fixes.
- `crates/serverless-core`, `crates/serverless-aws` and
  `frameworks/light-lambda-runtime`: admission, factory/lifecycle, routing,
  request state, transport, error and completion behavior.
- `crates/light-lambda-validator`: in-process engine with isolated precision
  JSON dependencies. `SOURCE-PROVENANCE.json` retains upstream hashes and records
  formatting derivations; `verify_provenance.py` verifies/replays them.
- `apps/light-lambda`: application entries, supported configuration/layers,
  packaging and the existing L1/L2 harness.
- `tests/serverless-conformance` and `crates/serverless-conformance`: original
  fixtures, versioned Rust expectations, approved differences, retained failures
  and execution/admission reporting. Large generated reviews and characterization
  corpora are evidence, not new production code.

## Local results

Qualification used base `9267419d3ac9288688c41ac5e99193bbb105d6e3`, Rust 1.99.1,
locked dependencies and owned local dependencies. No AWS operation was performed.

| Check | Result |
|---|---|
| Workspace formatting and all-target compilation | Passed |
| Strict Clippy: all targets of the seven Lambda-related packages | Passed, no new lint waiver |
| Complete workspace tests | 3,765 passed; 0 failed; 242 ignored |
| Explicit isolated PostgreSQL CI lanes | 49 passed, separately counted |
| Python packaging/L2/static-inventory tests | 51 passed |
| Workflow contracts, client templates, image orchestration, namespace preflight | Passed |
| Runner image and Kubernetes rendering/placeholder checks | Passed locally |
| Vendor provenance | 160 files verified; 11 formatting transformations replayed |
| Optimized package | Two independent clean builds are byte-identical |
| Packaged RIE | 19/19 jobs; 310 main checks; 317 request starts including separate probes |
| Local dependency observations | 187 Invoke requests/unique gateway IDs; 20 JWKS GETs |

Source test runs used debug-symbol/incremental-cache reductions to bound disk
use; test selection and assertions were retained. The test-only SHA-256 build
optimization preserves the original self-image admission deadline. No ignored
precision/Router execution gate was enabled. PostgreSQL CI fixtures were owned,
loopback-only, synthetic and removed afterward.

The SDK operation-cap, HTTP client-setup and Gateway fixture repairs preserve
timeouts, response contracts and measured downstream counts. Added priming calls
are separately counted. Empty TCP probes are not HTTP requests, and selected
Gateway test ports cannot be reused by another test in the same process.

The original D-08e concurrent test passes unchanged. Its historical zero-probe
failure remains preserved: per-fixture counters are isolated, but the old record
lacks timing sufficient to reconstruct exactly where the 50 ms budget expired.
No expectation, retry or suite-wide serialization was introduced to hide it.

Workspace-wide Clippy is a separate non-green baseline check, also failing on
pristine master. These findings were not waived or silently fixed through
unrelated API/layout changes. Remote CI must qualify the committed PR head.

## Local package identity and rerun

The bootstrap-only x86-64 AL2023 package has SHA-256:

`2f07f2446c5c50280f724d328ad3d4eb66affb7b19b81101ac885b963afd1308`

It is a local qualification artifact, not an accepted-release replacement.
The accepted ZIP remains
`d095e89c07f6079823d6d07395b8716f280add5e7e09f7a75b1d03e121af756a`.
The build manifest records source/tool identities and both independent builds.
No executable ZIP or private trial credential is committed in this change.

See [portable build and rerun commands](tests/l2/in_process/README.md) and
[delivery handoff](DELIVERY-HANDOFF.md) for artifact locations and supported
profiles. The supported tooling does not require RTK. Production compiler
fingerprints show the conformance-worker hook disabled and canonical
serde_json precision features unchanged; the private precision crate is isolated.

## Open scope

This is an incremental guarded merge, not full WP12/WP13 closure. Default
Validator production activation/resource qualification, Router rewriting and
real-worker provenance/execution, Traceability's instrumented-only proof and
AWS qualification of this package remain separate. Paused precision experiments
remain paused. Original D-08, H-04, Java L-02/D03 conflicts, approved differences
and the broader Java exchange-state defect remain visible. The five existing
Router forwarding comparisons and separate runner counts are retained.

Response/contract parity, byte identity, local HTTP attempts and AWS execution
identity are distinct claims. These local observations do not measure raw
SDK-wire behavior or establish AWS/freeze-thaw/resource bounds.

## Remote CI template repair

The first PR CI run rejected an unregistered Lambda client template. The manifest
now records its existing 30,000 ms early-refresh and 4,000 ms request fallbacks;
the application YAML bytes and qualified package are unchanged. The synchronizer
also resolves the logical light-fabric manifest paths against its own checkout,
so a renamed worktree cannot accidentally check a different sibling clone.
Seven hermetic regression tests cover active/shadow checkouts, drift, missing
and unregistered templates, sibling repositories and write targeting. They pass
on the repair and fail on the old script. CI runs these tests before parity.
The failed remote run remains preserved; this tooling fix requires a new CI run.

The next remote run passed four jobs but the Rust job exhausted disk during
linking, before test assertions. Its logs are retained. The Rust CI job now
uses the locally qualified no-debug-symbol/nonincremental settings and two build
jobs, with a separate cache prefix to avoid restoring large debug-enabled caches.
Debug assertions, test selection, fixture expectations and deadlines are unchanged.

A subsequent remote run reached the unchanged offline dependency-boundary test,
then failed because Linux compilation had not downloaded a pinned Android-only
crate. CI now runs `cargo fetch --locked` before checks to populate the complete
all-target metadata graph. The boundary test remains offline and unchanged;
there are no exclusions, relaxed assertions or dependency-version updates.

## D-08e remote recurrence and timing investigation

Run 40667543846 at aa494c9 passed four jobs, including the repaired offline
boundary check, but Rust CI reproduced the retained D-08e count failure:
56 wp13_families tests passed and one failed, expecting one response-probe entry
and observing zero. This remains a merge blocker, not a waived or repaired test.

The runner now records a bounded, per-invocation monotonic timing witness at
service start/return, fake Invoke receipt/response preparation and response-probe
entry/completion. The result schema admits only enumerated phases, elapsed times,
step and budget. No event contents or credentials are collected. This witness is
separate from response expectations and is not raw SDK-wire proof. Failure messages
include it. No fixture, digest, assertion, deadline or production code is changed.

The first observed local run passed all 57 wp13_families tests. Four D-08e copies
received Invoke at 15-27 ms and entered the probe at 16-28 ms, within the nominal
40 ms response deadline. This passing run does not establish the earlier CI cause.
A new paused-clock component control proves that resuming after that deadline
correctly produces one backend call, zero response entries and one finalization.
It supplements rather than replaces the wall-clock fixture. Deterministic fixture
timing and exact attribution of the CI failure remain open; no retries, broader
serialization, tolerance expansion or count normalization have been applied.

## JWKS timeout-test setup repair

Run 40675615110 failed earlier than D-08e: light-security had 47 passes and one
failure waiting for the timeout test's real loopback request to arrive. The other
four jobs passed. D-08e was not reached, so there is no new D-08e phase evidence.

The 50 ms timeout could expire during HTTP setup before the test captured the
active attempt. A bounded local scheduling control (yield to start the task,
then 75 ms executor delay) exposed this race at active-attempt capture. The exact
CI request-arrival failure and this distinct local failure are both retained.
A delay before task start did not reproduce it and is also recorded.

Only this test now holds Tokio time paused while driving real loopback I/O with
a runnable yield branch and the original five-second wall watchdog. It advances
the unchanged 50 ms timeout only after actual request arrival, observes cleanup,
retains the immediate cooldown rejection, advances the existing 31-second cooldown,
and drives recovery without racing I/O against virtual clock auto-advance.
All error/status, completion, active-state and exact request-count assertions are
preserved. JWT claim time, production security code, fixture contracts and other
tests are unchanged. The 75 ms scheduling control remains in the repaired test.
All 62 security library/integration tests passed locally. This repair does not
resolve the separate retained D-08e wall-clock fixture finding.

## JWKS deadline-family setup repair

Run 40681312915 passed the repaired 48-test security library group, then failed
caller_and_joiner_deadlines_do_not_cancel_later_success in the 14-test integration
group (13 passed, one failed). The assertion was `verification must be waiting`.
D-08e was again not reached. Four other CI jobs passed. A local 75 ms setup delay
reproduced the exact joiner assertion against the old test.

Both remaining short-deadline JWKS integration tests now start their Tokio clock
paused and use the existing bounded real-I/O driver during setup/recovery. This
includes the analogous shared-fetch timeout setup hazard. The helper is shared
through test support with cfg(test), excluded from the conformance-worker build.
All original 50 ms deadlines, 51 ms advancement, 150 ms virtual-time assertion,
errors, continued long-waiter success, expired-caller rejection and exact request
counts remain. The assertions measure controlled caller/refresh timers, not
wall-clock network latency. The 75 ms scheduling controls remain enabled.
All 62 security tests passed. No production code, fixture, expectation, dependency,
retry, serialization or deadline expansion was introduced. The separate D-08e
wall-clock contract remains unresolved. Existing strict security Clippy findings
in unchanged dual_identity.rs and token_purpose.rs remain recorded without waiver.

## D-08e controlled-clock fixture repair

Run 40720066462 passed all 39 portal-registry tests, including the repaired
command-flood regression. Rust CI then reached wp13_families: 55 passed and two
D-08e consumers failed with zero response-probe entries. Their timing witnesses
showed service return at 51-53 ms against a 50 ms invocation budget, without an
Invoke receipt or response entry witness.

The isolated emission child now controls Tokio time for the existing
`response-100ms-v1` recipe. Time remains stationary while the real SDK traverses
the loopback dependency and until every caller enters the actual response probe.
Tokio then advances through the original deadline and cancels the 100 ms probe.
A runnable driver prevents idle auto-advance during socket IO. The parent retains
its real 15-second child timeout and kill/reap behavior for missing probe entries.
Clock control ends before subsequent observations. Fixture bytes, digests,
expectations, budgets and production service code are unchanged.

This qualifies response-timeout semantics under a controlled monotonic clock;
it does not qualify wall-clock SDK/loopback latency. The timing witness and EMF
wall timestamps remain actual wall observations and must not be interpreted as
virtual elapsed time. The existing core scheduling regression continues to cover
zero response entries when execution resumes after the response deadline.

Local validation: the initial wp13_families run passed 57/57; the final run after
the concurrent-entry adjustment passed 57/57 in 98.50 seconds. Core deadline and
deadline_scheduling tests passed 7/7. Workspace formatting and git diff whitespace
checks passed. Remote full-workspace CI subsequently passed at 3a937b9; see final review below.

## Token-provider timeout fixture cancellation repair

Run 40725727641 stopped in serverless-core wp12_token before reaching D-08e.
The timeout provider accepted a TCP connection but read EOF: the production
20 ms request deadline cancelled the HTTP operation before request bytes arrived.
The fixture now accepts no connection or EOF only in its timeout branch, with a
bounded one-second accept wait. Non-timeout denial/redirect branches still require
both a connection and request bytes. All status/code, privacy, zero-dispatch,
response-entry and finalization assertions and the production deadline are kept.
Local full serverless-core validation passed 192 tests across 36 suites.

## Final CI repair review and remote qualification

Reviewed the three CI repairs from ddd156c through 3a937b9. No blocking finding
was identified within that scope. The registry loop sends its own command
responses directly rather than awaiting capacity in the queue it alone drains;
external producer backpressure remains bounded. Its forced-full-queue regression
failed under the old behavior and passed with the repair. The conformance clock
is controlled only in the isolated response-timeout emission worker, and the
parent retains real timeout/cleanup. Provider EOF/no-connect acceptance is limited
to the timeout fixture branch, preserving exact response and zero-dispatch checks.
GitNexus mapped the registry changes as low risk; its index did not map the new
conformance code, so source review and focused/remote tests supply that evidence.
This is a review of the three CI repairs, not a new whole-PR parity audit.

GitHub Actions run 40729726762 passed all five jobs at
3a937b9b03873ba16cb01c31872bdb0424e5a536: rust, postgres,
host-tool-workflow-access, workflow-retry-and-timer and runner-image-and-manifests.
The Rust job completed in approximately 22 minutes. This supersedes the pending
remote-CI gate and the historical D-08e CI blocker above; earlier failures remain
as historical evidence. Controlled-clock conformance is not wall-clock latency or
AWS qualification. Existing broader product/qualification gates remain unchanged.
The documentation-only follow-up commit requires its own ordinary CI result.

## Twelve-finding review follow-up

The PR returned to draft for this review. The earlier green CI result applies to
3a937b9, not to the subsequent review corrections. This section supersedes the
narrow three-repair review as a merge-readiness statement.

| Finding | Resolution |
| --- | --- |
| Decoded Router path reinterpreted as URL syntax | Serialize decoded bytes as path data; reject dot segments before normalization. Query/fragment/backslash/percent/Unicode regression covers the configured base prefix. |
| Content-Type header spelling | HTTP case-insensitive ingress capture, first-value selection, header-mutation proof and validator projection. The user explicitly approved HTTP semantics and contract amendments. |
| Query ordering and plus decoding | Retain the approved E-04 once-decoding contract: literal plus and repeated-value order are preserved; lexical cross-key order is a deterministic reconstruction from an ALB map, not an original wire-query claim. |
| Duplicate oversize encoder | Legacy encode delegates to prepare_response; finalization still uses the prepared response outcome. |
| Comment-only bounded YAML | Empty streams without a document are admitted as null, including comment-only values/rules. |
| Malformed typed scalar fallback | Distinguish parser syntax failure from resource-budget failure; only syntax failure may fall back to the original string. Oversized collections still fail before materialization. |
| Lambda early token refresh | Retain an unexpired cached token after refresh failure or shared retry backoff; cached return still checks the invocation deadline and token bounds. |
| Client identity rate-limit keys | Remove the Client-Id fallback in shared and Lambda implementations. Verified clients share their bucket despite changing caller headers; user-only identities cannot fabricate a client bucket. |
| Akamai source labels | This remains an explicit opt-in trusted-ingress quota-label mode, never verified identity. True-Client-IP must be overwritten/sanitized at a trusted edge and the origin must prevent bypass. It is unsuitable behind an ALB alone if clients can choose the header. Default server/address/client/user modes do not read True-Client-IP. No Akamai deployment is qualified here. |
| Shared JWT compatibility | Gateway entry point retains its prior nbf policy, refresh-on-validation-error behavior and missing-key message. Lambda deadline-aware/diagnostic entry points retain stricter nbf and signature-only refresh. Lambda verification does not reuse a Gateway principal-cache result. |
| Insecure environment switches | Default runtime builds ignore both switches. An explicit insecure-test-config feature is required; conformance enables it only as a dev dependency. Ordinary workspace/bootstrap normal/build dependency graphs do not enable it. |
| Eager peer-address formatting | Pingora computes the peer-address label only for Address keys. |
| Duplicate reserved-route guard | Service owns the single reservation table; application tooling delegates to it and the compatibility callback no longer repeats admission. |

The 16 archived Java header characterization rows remain byte-for-byte unchanged.
The separate wp12-header-validator-http-amendment.json binds their SHA-256 and
records the six status/body/backend-count differences caused by HTTP header-name
semantics. The composition test checks the original digest and all 16 cases.
This is an approved Java/Rust behavior difference, not a claim that Java changed.

Production builds must omit insecure-test-config. Synthetic standalone bootstrap
qualification that requires HTTP/insecure fixture prerequisites must explicitly
use `--features insecure-test-config`; that artifact is not deployable evidence.
The default-build child regression sets both environment switches to 1 and proves
they have no effect. Fixture content and private credential values are not logged.

Review validation: config-loader/gateway-policy/light-security/serverless-aws/
serverless-core passed 370 tests across 67 suites. The affected light-lambda,
light-lambda-runtime, light-pingora and serverless-conformance run passed 1,394
with 21 existing ignored tests across 109 suites. The default-feature runtime
library run separately passed 236 with six ignored and included the child-process
insecure-switch regression. Final Pingora policy parity passed 11/11; final
serverless-aws passed 34/34. No suite exclusions or fixture-digest edits were used.

Selected strict library Clippy passed for config-loader, gateway-policy,
serverless-aws, serverless-core, light-lambda-runtime and light-lambda. The ordinary
bootstrap build (`cargo build --locked -p light-lambda --bin bootstrap
--no-default-features`) passed; its runtime compiler fingerprint has no
insecure-test-config feature. Both checks reported the existing deprecated
usize::max_value call in the vendored validator dependency; no vendor edit or
warning suppression was applied. Remote full-workspace CI remains pending for
this review correction. The PR remains draft until that evidence and review of
the explicit HTTP contract amendments are complete.

The final explicit Gateway compatibility regression also passed: light-security
completed 64/64 tests, including legacy Gateway refresh/error wording versus
Lambda expiry-error behavior. Formatting and whitespace checks passed.

## Residual review-gap correction

This supersedes the preceding Akamai prerequisite-only disposition. Akamai's
compatibility resolver name now uses trusted transport/source metadata rather than
True-Client-IP: Pingora uses its socket peer, Lambda its admitted source_ip. Header
rotation cannot obtain fresh buckets. Behind proxies this deliberately groups
traffic under the admitted proxy/source identity; forwarded-header per-client
identity is not inferred. Existing default resolver choices are unchanged.

The shared limiter now permits at most 4,096 active buckets and 4,096 bytes per
stored key. Under pressure it rejects new identities rather than evicting active
counters and resetting quotas. Configured quotas/windows continue to govern each
bucket's event history; no lower per-bucket throughput ceiling is introduced.
Expired counter buckets can be reclaimed. Lambda's insertion-only traversal
metadata is separately capped at 4,096 labels; its existing labels are not
forgotten, so saturation can continue rejecting new labels until the snapshot is
replaced. Oversized configured server-prefix sets are rejected at Init. These are
explicit safety ceilings, not performance or deployment memory qualification.
Tests cover capacity, live-counter preservation, expiry recovery, traversal
growth and header rotation with a stable trusted source.

Gateway key selection now retains its legacy single/service-key fallback on kid
mismatch; Lambda remains strict. Real verification and service-scoped cached-key
regressions distinguish those entry points. No globally weaker Lambda lookup was
introduced. BoundedLocalSources::capture/values now has comment-only layer coverage
for full and bootstrap-only views. Typed scalar fallback also covers YAML accepted
by libyaml but rejected by serde_yaml, including duplicate mapping keys, after the
bounded streaming preflight. Resource-budget errors still reject before allocation.

Fixed literal configuration replacements may carry a query: the replacement path
is serialized as data and the separately configured query is set explicitly.
Caller paths and dynamic Rewriter/Candidate outputs remain decoded path-only data;
`?` and `#` are not URL authority/query syntax there. Dynamic query rewriting is
not supported by that interface. The wire regression proves distinct targets for
fixed configuration queries and decoded caller question marks, without re-parsing
the combined untrusted URL. This does not claim arbitrary Java URL-rewrite parity.

Validation for the residual correction: the broad eight-package run passed 1,737
with 21 existing ignored tests across 165 suites. A final four-package run after
retaining the original per-bucket quota throughput passed 341 tests across 56
suites. Child-process selections account for the two reported filtered tests;
no dependency-boundary exclusion or failure waiver was used. All three router
serialization tests passed, including the bounded real HTTP wire test. The new
key-selection, captured-values, duplicate-key scalar and quota-pressure tests run
in these suites. Original characterization/fixture JSON and digests are unchanged.

Strict library Clippy passed for config-loader, light-security, gateway-policy,
serverless-core and light-lambda-runtime. Including light-pingora failed with 53
errors in unchanged patches/pingora-core (async-trait must_use and other lints).
That broader result remains non-green; no suppression or unrelated patch repair
was applied. The unchanged vendored validator usize::max_value deprecation warning
also remains. Formatting/whitespace checks passed. Remote full-workspace CI for
this new correction is pending, and the PR remains draft for review.

The ordinary bootstrap build with --no-default-features passed for this correction;
its latest runtime compiler fingerprint again lists only the default feature.
The insecure qualification feature remains absent from that production build.

## Forwarded-address spoofing and CDN identity correction

This supersedes the previous transport-only Akamai disposition. The common
transport-neutral TrustedProxyPolicy compiles the optional trustedProxyCidrs
allowlist at configuration load/Init. Address mode ignores XFF from untrusted
peers and walks trusted chains right-to-left. Akamai mode requires an explicit
allowlist and accepts only one valid True-Client-IP from an allowlisted,
sanitizing peer; untrusted peers and malformed/missing assertions fail closed.
There is no transport-address fallback in Akamai mode, including for stale or
unknown CDN egress ranges. Direct clients should use Address mode. Untrusted direct
peers cannot use either header to choose bucket identities. Pingora passes a
typed socket IP without eagerly formatting/parsing it. Lambda uses admitted ALB
source metadata under the existing append-mode prerequisite.

Both application templates expose limit.trustedProxyCidrs, and the real Lambda
configuration test covers values.yml through the embedded template, offline
validation and startup. CIDR parsing uses the already-locked ipnet package, adding
one pure IP-parsing dependency edge; no provider SDK or transport dependency was
introduced into the shared policy. Allowlist size is capped at 64, catch-all /0
ranges are rejected, and trusted XFF work is bounded to 64 hops/8 KiB. Operators
must supply reviewed proxy egress ranges and enforce sanitization/origin access;
no actual production CIDRs or deployment were changed here.

The new pressure regression sends more than 4,096 arbitrary XFF labels from one
untrusted peer: only one bucket exists and another peer is still admitted.
Additional regressions cover two client IPs behind one trusted edge, source
normalization/IPv6, malformed/repeated assertions, trusted chain boundaries and
offline/startup parity. The real Pingora test fixture now installs the same
socket digest as the production listener rather than supplying a metadata-free
raw TCP stream. Real ALB/service composition dispatches twice for two clients,
returns 429 for the exhausted client and performs no dispatch for missing trusted
identity. Original native direct-fallback quota semantics remain unchanged.

An initial broader run reported two unchanged L1 assertions (Init outcome log
and JWKS request count); both passed focused without changes, and a later complete
run passed 1,653 tests with 21 existing ignored. Sanitized L1 evidence is retained
under /tmp/light-fabric-proxy-trust-full-20261028 and
/tmp/light-fabric-proxy-trust-l1-focused. That passing run does not establish the
cause of the earlier intermittent failures, and no assertion or timeout was
relaxed. Final validation after the template/typed-IP wiring follows below.

Parent bcc3305 passed all five jobs in remote CI run 40754203613. That result
predates this proxy-trust change and is not qualification of the new head.
The initial local L1 assertion output is retained at
/tmp/light-fabric-proxy-trust-initial-l1-failure.log in addition to the passing
sanitized fixture evidence named above. No source change to those L1 fixtures,
JWT policy, Runtime API reporting or timeouts was made in this correction.

Final proxy-trust validation passed 1,656 tests across 155 suites, with 21 existing
ignored tests and no suite exclusions. Sanitized L1 evidence is retained under
/tmp/light-fabric-proxy-trust-trusted-only-final-20261028. Selected strict library
Clippy passed for gateway-policy, serverless-core, light-lambda-runtime and
light-lambda. The ordinary bootstrap build passed with only the default runtime
feature in its compiler fingerprint; the insecure qualification feature remains
absent. Both checks retained the unchanged vendored validator deprecation warning.
Workspace formatting/whitespace checks and mdBook build passed; mdBook retained
its existing large-search-index warning. Inclusive Pingora/patch Clippy remains
the separately recorded non-green gate, without a new waiver or unrelated edit.
No shared stack, production allowlist, AWS resource or live CDN was changed.
New-head remote full-workspace CI is pending.
