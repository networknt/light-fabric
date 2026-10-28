# Light Lambda conformance v1 (incremental WP13 started)

The runnable Rust subset and copy-paste `run` / `compare` commands are documented
in [REGISTERED-DELIVERY.md](../../apps/light-lambda/REGISTERED-DELIVERY.md). Coverage is
separate from result-v1, unsupported selected fixtures exit non-zero, and full
WP13/Phase 2 conformance remains incomplete. The historical WP3 scope below
describes the original fixture/Java tooling, not the newly added Rust subset.

Coverage now separates missing fixture execution/assertion adapters from actual
app registrations, unregistered native handlers, deferrals and test-probe classes.
The original eight-case profile passes; the expanded24-case profile preserves
native execution failures and stale-difference findings rather than suppressing them.

This is L0 tooling and reviewed synthetic Java assertions. Schema validity and
Java replay do not establish Rust behavior, ALB acceptance, native compatibility,
binary build provenance, or deployed compatibility. The incremental Rust runner
and comparator now exercise an explicit subset through real app Init/service.

`LEDGER.md` is the current approved WP1 evidence ledger, including CP1 amendments
and its historical provenance. Links retain their original evidence destinations.
No commit is implied by this repository copy. `coverage.json` distinguishes
admitted fixtures from held observations and later ownership; a row link alone is
not behavioral equivalence. `candidate-review.json`, `proxy-enablement-review.json`
and `observation-fixture-review.json` record conversion decisions. Expectations
are copied/reviewed from WP2 observations or authored from recorded assertions;
they are never overwritten from runner output.

## Offline commands

From the Rust worktree:

```sh
rtk cargo run -p serverless-conformance --locked -- digest
rtk cargo run -p serverless-conformance --locked -- validate
rtk cargo test -p serverless-conformance --locked
```

From the Java worktree:

```sh
rtk proxy scripts/run-conformance.sh /home/steve/workspace/light-fabric-light-lambda/tests/serverless-conformance/fixtures/v1
```

Build the Rust digest/validate binary first. `CONFORMANCE_HOME` can specify its
contract directory. The script validates inputs, builds the test classpath once,
runs `ConformanceIsolationTest` first, then launches exactly one fresh JVM per
fixture. All steps in one fixture share one initialized `LambdaApp`. Each process
launch supplies `-Dlight-4j-config-dir=<fresh runner-owned temp>` before any Java
configuration/gateway initialization. No static production fields are reset.
Processes have 45-second bounds; only runner-owned process groups and temporary
configuration directories are cleaned up. Evidence and results are retained in a
new `target/conformance-results/<UTC>/` directory for every run. `CREATE_NEW`
prevents accepting old results. Missing results, stale digests, duplicate IDs,
unexpected outcomes and child failures fail the aggregate. Result files are
validated against result-v1 and tied to their exact fixture digest.

The isolation test is a CLI with no JUnit annotations; it does not execute under
Surefire. WP2's dedicated characterization fork settings remain untouched.
The new test-only `ConformanceProxy` delegates to the actual middleware using
WP2's existing package-private injected-client constructor. It respects the real
enablement check. Twelve WP2 exports omitted `lambda-proxy.enabled` while their
capturing wrapper always returned true; fixtures explicitly enable the real proxy
instead. No production constructor, behavior, or reflection hook was changed.

## Schema and outcomes

Both schemas use JSON Schema draft 2020-12. There are three fixture kinds:

- Equivalent: both flags false, no `expectJava` or `differences`.
- Intentional difference: both flags false, Java asserts `expectJava`, Rust will
  assert `expect`, and an approved intentional-change row plus nonempty declared
  differences are required. Equal/equivalent related rows are still independently
  asserted in Java; an E-13 rejection fixture is not Rust policy coverage.
- Single-runner: exactly one flag true; no `expectJava` or differences. Only
  intentional-change/deferred ledger mappings are admitted. The other runner
  records `execution: skipped`, empty observations, and a reason.

Each request-bearing case/step has exactly one `request` or `rawEvent`. Generated
requests require explicit `headerMode`; array order and last-value single-mode
collapse are defined by the envelope generator. Raw events are mapped with the
Runtime mapper's unknown-property-disabled setting. No query decoding, header
normalization, API Gateway context synthesis, or map repair occurs. Deliberately
synthetic non-ALB fields in recorded component inputs stay explicit and do not
claim that ALB supplies those fields.

`observations` is always an array, one entry per warm step (or one Init outcome).
Response outcomes record the exact returned Java POJO fields, including nullable
status/base64 flags and null header values where observed; they do not assert that
ALB accepts such an output. Headers retain original names, values and multiplicity.
`responseMaps` preserves the exact single/multi namespaces independently from
header pairs. Exceptions have an explicit phase and Java class, with the message
asserted only where WP2 recorded it. Init errors are separate from HTTP responses.
`initOnly` performs initialization without inventing an invocation. The fixture
Init category `InvalidConfiguration` asserts rejection, not a prescribed future
Rust enum spelling or evidence of `/init/error` reporting; exact variant mapping
must be resolved before WP13 replay.

`bodyBase64` and `outcome` extend the difference vocabulary narrowly so approved
body/Init/exception differences cannot be disguised as HTTP body differences.
Backend calls are exact in number/order/target; their event fields are subset
assertions with exact array lengths; an empty expected object imposes no field assertions. Raw absence, explicit
null and empty body input remain distinct in fixture identity; Java may map
absence and null to the same POJO value.

The only nondeterministic allowance currently admitted is the generated
`X-Correlation-Id` scalar at an explicit backend-header JSON pointer. Its value
must match `^[A-Za-z0-9_-]{22}$`. Presence, call count, target, all other fields and
header multiplicity are still asserted. No authorization, status, body bytes,
backend targets or call counts may be excluded. Results retain the observed
correlation value, without normalizing the actual header maps.

## Tokens and clock

`clock.mode: relative-wall` states that recipes use runner-start wall time.
Production verification uses its existing wall clock; no deterministic clock
injection is claimed. Absolute exp/nbf/iat claims are forbidden in recipes.
Per-run RSA keys are generated in memory. `kid`, optional signing-key selection,
algorithm and relative issued/expiry/not-before offsets are declared separately.
The optional loopback JWKS dependency publishes declared public key IDs; warm
steps can change its publication set without reinitializing the gateway.
Raw token replacements require explicit JSON-pointer declarations. No other raw
input substitution occurs. Configuration may refer to `${dependency:jwks}`.
Token values in results are represented by their declared recipe references after
assertion, and runner logging is disabled before gateway initialization to avoid
persisting generated bearer tokens. Key generation is not production key handling.

The broad JWT/JWKS observations are still pending fixture admission and Init
compatibility review; helper support alone is not JWT behavioral coverage.
Time-derived quota headers, simultaneous callers, deadline behavior and explicit
cache mutations remain individually recorded in `gap-dispositions.json`; no
general scenario language or Rust deadline/concurrency implementation was added.

## Canonical JSON and DIGEST

Canonicalization recursively sorts object keys by Unicode scalar order, preserves
array order and scalar values, and emits compact UTF-8 JSON with no BOM, whitespace
or trailing newline. Strings use JSON escapes for control characters, quotation
marks and backslashes; non-ASCII text is emitted in UTF-8. Numeric lexemes come
directly from the original file, with no floating-point conversion; their spelling
is preserved. Non-finite numbers are invalid JSON. The shared qualification vectors
below cover fractions, exponents and supplementary Unicode keys. Duplicate object
keys fail identity generation, including keys made identical by escape decoding.

Enumerate only JSON fixtures beneath `fixtures/v1`, sorted by their stable relative
POSIX path. `DIGEST` itself has no JSON extension and is excluded. Generated results
and logs live outside that tree, as do held review documents. Each line is
`<sha256(canonical JSON)>  <relative path>\n`. The total is SHA-256 over those
per-file lines in order, followed by `<total>  TOTAL\n`. SHA-256 provides identity,
not publisher authenticity. Schema validation is not behavioral conformance.

## Closure blockers

WP3 is incomplete. ServerInfo's heap-field representation is resolved by the
owner-approved amendment below and validated by the focused assertion helper.
The remaining observations require individual implementation/review; see current
`gap-dispositions.json` and `coverage-summary.json`. Unfinished implementation is
not an owner-approval blocker. Full acceptance suites are reserved for final
qualification after the outstanding changes are ready.

The held E-02 nonoverlap draft remains preserved outside fixture enumeration; a
separately reviewed nonoverlap fixture is admitted. Neither is called equivalent.
The current E-02a ledger documents lowercase Rust HeaderMap encoding. The lowercase
Origin/null header B-03/E-06 observation also remains unadmitted, with its exact
Java data preserved rather than normalized. Remaining observation and ledger
coverage gaps are explicit; no later ownership label claims they are implemented.


## Owner-approved WP3 ServerInfo amendment — X-02/X-02a

Owner authorization: the current resumed WP3 instruction explicitly approves this narrow amendment. Enabled Java ServerInfo may use a structured JSON body expectation. Only /environment/runtime/freeMemory, /environment/runtime/totalMemory, and /environment/runtime/maxMemory use constraints in place of historical numeric values: all three must exist, be nonnegative integers, and satisfy freeMemory <= totalMemory <= maxMemory. All remaining approved JSON fields are compared strictly, including complete field sets and stable values. Malformed JSON, duplicate fields, missing fields, unexpected fields, and incorrect stable values fail.

Actual body strings remain unchanged in result-v1. Expectations must be reviewed from approved source/evidence and never regenerated from current runner output. Rust may have a distinct approved X-02a ServerInfo shape without Java heap fields. Exact body-byte comparison remains the default for every other fixture. This grants no general JSON-pointer masks, exclusions, input transformations, or new production behavior.

Coverage accounting must distinguish Java assertions exercised, proposed Rust behavior reachable on the same declared input, and future Rust execution evidence. E-13 rejection of single-value input establishes ingress behavior only; downstream JWT/routing/CORS/transformation/ServerInfo references are not reached Rust coverage. Incidental ledger references are not behavioral coverage. No Rust behavioral execution occurs in WP3.

Schema representation: response expectations choose exactly one of `body` (string/null, exact) and `bodyJson: {"kind":"serverInfoJavaHeap","stable":{...}}`. The stable object contains the complete approved Java JSON shape except the three named heap fields. Those fields must be absent from stable; the actual body must contain them. The Java helper validates them at the fixed paths, then compares an assertion-only copy of the remaining JSON by strict equality. It never edits the recorded body. Only X-02/X-02a Java expectations may use this representation; Rust expectations cannot inherit Java heap constraints. Base64 must be false. Result schema continues to require the actual body string.

### Reviewed dispatch outcome declarations

Backend `respond.nullPayload: true` declares an absent SDK payload; its JSON `payload` placeholder must be null and is not serialized. This differs from an ordinary `payload: null`, which sends the JSON bytes `null`. `respond.sdkFailure` selects only the closed WP2 list (IllegalStateException, SdkClientException, TooManyRequestsException, ResourceNotFoundException, LambdaException), with a synthetic message and service status where applicable. It is mutually exclusive with nullPayload; neither is a real SDK/network retry test. Observed backend calls optionally include `invoke.logType` and `invoke.invocationType`, retaining actual request values. Expectations may assert those fields explicitly. No production clients or constructors change.

Coverage is in `fixture-behavior-review.json`, `coverage.json`, and `coverage-summary.json`: Java behavior assertions, proposed Rust reached behavior, and future Rust execution are separate. Incidental ledger references never count. Unadmitted observations remain category 1 WP3 work unless individually justified otherwise; they are not automatically owner-decision blockers.
# Lossless fixture identity qualification

Fixture-file identity is calculated from the original UTF-8 JSON bytes, before
materializing numeric values in a Java or Rust tree. Object keys are decoded,
sorted recursively by unsigned UTF-8 bytes (Unicode scalar order), and serialized
with compact JSON string escaping. Arrays retain their order. No whitespace or
trailing newline is emitted. String escape spelling is normalized; duplicate keys,
unpaired surrogates, trailing content, and malformed JSON fail. Nesting is bounded
to 128 levels in both implementations.

JSON numeric lexemes are preserved exactly, including fractional precision,
exponent spelling, and negative zero. Thus `1`, `1.0`, and `1e0` have distinct
identities; none is rounded through a floating-point value. This rule defines the
algorithm explicitly and does not claim RFC 8785 canonicalization. File digests
use this lossless algorithm; parsed-value utility hashes are not fixture identities.
The shared 19-vector qualification file covers high-precision fractions, integers
beyond 2^53, exponents, negative zero, supplementary Unicode keys and strings,
escaping, duplicate keys and malformed inputs. Both languages check the same
independently specified canonical bytes and SHA-256 values.

`eventAbsent` assertions require genuine absence at the declared backend-event
JSON pointers; null is not absence. `eventExact` asserts complete top-level event
fields, including map keys and repeated value order/count. These directives affect
the assertion copy only; the actual business event remains unchanged in results.
Shared backend stubs must match each runner's expected call targets/order and have
exactly the larger expected call count. An intentional difference may dispatch in
only one runner, while both retain exact call-count assertions.

Warm steps may declare `waitBeforeMs` (integer 0..5000), a bounded real wall-clock
wait before the step in the same initialized JVM. It is not a virtual clock or a
deadline simulation. Exact limiter reset-duration header strings remain asserted.

`dependencies.http.responses` declares a finite ordered HTTP/1 loopback protocol:
method, exact raw URI, status, repeated response-header pairs and UTF-8 body.
The raw responder emits those headers exactly; it does not add a wall-clock Date.
Content-Length framing and socket cleanup are runner-owned. Actual inbound header
names/value arrays, targets, methods, URIs and bodies are recorded. `httpCalls`
assertions require exact call counts and declared target/URI/method order; dependency
URL placeholders are resolved before execution/assertion, not excluded from results.
The existing Java omitted queries and collapsed HTTP response headers remain exact
assertions. No external HTTP endpoint or transport/retry/TLS scenario is implied.

Warm fixtures whose shared config Rust must reject use one top-level
`expectRustInit` (`initError` and exact code). Their Rust step expectations are
`notExecuted` with reason `initError`, zero calls and zero request fetches.
They do not assert repeated Init attempts or fictitious HTTP responses. Results
have a separate `initialization` outcome and explicit unexecuted steps; Java
records successful initialization and executes every warm step in one gateway.
This representation is restricted to intentional differences and is checked by
schema and cross-file validation. It establishes Rust Init reach, not downstream
policy reach or Rust execution evidence.

A token recipe may declare `signAtStep` to sign once after Init and after that
step's bounded wait, immediately before request execution. Later warm steps reuse
that token. Token use before its signing step is invalid. This prevents a short
cached-expiry recipe from expiring during Init; it still uses real wall time and
does not claim deterministic verifier clock control.


## Approved CORS ALB multimap contract

Owner approved the CORS multimap amendment in the current WP3 continuation: shared raw ALB multiValueHeaders origin with no single-value headers remains unchanged for both runners. Java CORS reads getHeaders and ignores multi-value-only Origin, retaining its observed dispatch/response behavior. Proposed Rust reads ALB multimap Origin for CORS request validation and response header output. Separate exact Java/Rust expectations are required. This amendment does not repair Java, authorize single-value normalization, alter preflight dispatch semantics, or grant Rust execution evidence. Ledger B-02a/B-03a.

## Owner-approved WP3 C-07 component representation — 20261011T002922.042170Z

Owner approved a closed Java-only C-07 component representation: streamInput may hold exact malformed stream bytes instead of an ALB request/rawEvent. Stream output bytes and Init exceptions have explicit expectations and are exercised against existing LambdaStreamApp in one fresh JVM per fixture. This exception applies only to C-07 deferred Java-only component fixtures. Ordinary ALB cases still require exactly one request or rawEvent. No Rust stream support, production repair, or general scenario language is authorized. Approval: /home/steve/artifacts/implementation/light-lambda/WP1/approval-history/20261011T002922.042170Z/approval.md

### Closed APM sink assertions

Owner approved closed APM sink expectation for M-01/M-02: exact metric names and types, exact request/success counts, nonnegative integer timing strings for response_time.min/mean/max with min <= mean <= max; reject missing/extra metrics and preserve the actual sink body. Historical 41/41/40 are not expectations. No deterministic clock or Rust metrics execution claimed. This exception is exclusive to the APM sink, not application HTTP bodies or a general JSON mask. Exact bytes remain default elsewhere. Approval: user answered "Approve closed APM sink expectation" in this continuation.

### Closed Java P-01a bootstrap component

Owner approved deferred Java-only P-01a closed bootstrap component for exact Java bootstrap source and subprocess assertions. Original bootstrap runs with an owned fake lambda-native; actual exit/output and exact /opt Java configuration/logback arguments are retained. P-01 remains equivalent at the ZIP/layer deployment-family level, with packaging qualification owned by WP15. No native ZIP, deployed executable, Rust packaging execution, or Java-command fabrication in Rust is claimed. Approval reply: "Approve Java-only P-01a component".

### Controlled SDK read-timeout input

Owner approved separately authored D-08 SDK read-timeout fixture with apiCallTimeout=5000ms, same readTimeout=300ms and delayed replies400ms, and exactly four legacy HTTP attempts. Original1800ms observation and source at-least-one assertion remain historical timing-budget evidence; no deterministic four-attempt claim for that original budget. No call-count exclusion, Java behavior change, or Rust gateway/timeout implementation. Approval reply: "Approve controlled 5000ms fixture".

### Approved JWT multimap distinction S-01a

Owner approval: "Approve JWT multimap amendment". S-01a narrowly authorizes JWT policy to read ALB multi-value authorization headers. Shared raw input with only multiValueHeaders.authorization=["Bearer not-a-jwt"] causes Java to skip verification and dispatch FnX (200, body "ok", one call); proposed Rust rejects invalid JWT (401 ERR10000, zero calls). Java remains unchanged. Both runners receive the same raw event; no per-runner input adaptation. Separate reviewed expectations and explicit differences are required. S-03 remains equivalent for supported common behavior; E-13 is not used as downstream authorization. No Rust execution evidence is implied.

## Approved exact ServerInfo shape and response casing

Owner approved "Approve exact redacted shape": X-02a Rust ServerInfo response is status 200, content-type application/json, exact UTF-8 body {"runtime":"rust","modules":["serverless-core","serverless-aws"]}. No configuration, paths, credentials, versions or Java heap fields are exposed. Complete Java structured-body assertions and actual body remain preserved. This is an output contract, not Rust execution or implementation.

Owner approved "Approve response casing amendment": new E-06a intentional-change row authorizes gateway response header-name serialization casing only: Java original names/maps remain exact, Rust HeaderMap serializes lowercase names. Values, multiplicity, status and body are unchanged by this amendment. It grants no comparator normalization, Java repair, request input transformation, or relaxation of E-02a overlap precedence.

## Approved Basic ALB multimap contract

Owner approved Basic multimap amendment, including the corrected reproduced admin-chain case: S-07a intentionally reads ALB multiValueHeaders authorization in Rust while preserving Java single-map behavior. In the configured authenticated /adm/server/info chain Java returns 401 ERR10002 with Basic challenge; proposed Rust authenticates the declared local user and reaches enabled ServerInfo. Existing generic /x fail-open dispatch observations remain distinct and do not prove Java authentication. Inputs stay shared and unchanged; separate exact expectations are required. This does not authorize Java repairs, runtime implementation, or Rust execution claims.

## Approved completion probe and version mapping

Owner approved closed C-03 completion probe: fixed named test callbacks register through existing exchange APIs outside production handler configuration; assert reverse order, removal and repeated-finalization rejection. Same input and declaration for both runners; Rust adapter belongs to WP13. No general callback/scenario language, production resets or Rust runtime implementation. Semantic rejection/order/count observations are asserted; actual Java exception classes are preserved in result runner diagnostics, not fabricated for Rust.

Owner approved E-10h version omission: for shared raw ALB input carrying version="2.0", Java v1 mapper recognizes and forwards that field, while the approved Rust business-envelope table omits version. Preserve exact distinct backend events and original failed draft; ALB classification still follows requestContext.elb.targetGroupArn. No input transformation, header normalization or event-family change.

D-03 diagnostics are a closed assertion representation for already-approved payload logging behavior. Java captures actual formatted proxy logger records at all enabled levels and requires exactly one reviewed payload TRACE message. Rust may emit no FunctionError logs; each emitted diagnostic must contain only approved structured metadata fields with declared exact values. No body, header, authorization, target or Invoke-count allowance is introduced. Actual Java records remain in result observations; no log text is rewritten. Java diagnostic collection enables only the proxy logger for the declared invocation, avoiding unrelated token logs. WP13 must collect all gateway FunctionError diagnostics for Rust, across levels.

## WP3 version amendment identifier correction

Identifier correction: approved version omission is recorded as E-10i, because existing E-10h already covers resource/resourcePath/httpMethod/path/userAgent/identity metadata. Owner approved the described version behavior; its scope is unchanged. Preserve the original approval text and duplicate-ID validation failure 20261011T022015.012231Z. Existing E-10h is unchanged.
# Gateway request clock assertions

Fixture identity enumerates UTF-8 relative JSON paths in stable order. The fixed
root directories `results/` and `generated/` are reserved for generated outputs
and excluded; fixtures may not be authored there. `DIGEST` is also excluded.
There are no extension-independent ignore patterns or configurable exclusions.
Explicit result validation enumerates every JSON file under its separate result
root, including nested directories, so this fixture-only exclusion cannot conceal
missing or stale results. Canonicalization sorts decoded object keys by UTF-8
byte order, preserves array order and numeric lexemes, emits compact UTF-8 JSON
with deterministic string escaping, and rejects duplicate keys and malformed
JSON. Generated outputs cannot affect the fixture manifest or its TOTAL line.

The optional E-08 backend marker `eventAuthorizer` binds the entire authorizer
object to the declared signed input token (`claimsFromToken`) and authored
`principalId` and optional `scope`. The recipe supplies stable claims and relative
signing offsets. Exact generated input claims, including iat/exp/nbf, are checked;
no gateway output supplies expectations. Cross-validation binds the marker to the
same ALB multimap Authorization token and approved principal/scope precedence.
Missing or extra authorizer fields fail. Unauthenticated controls use exact field
absence. Actual Java supplied authorizer forwarding remains observable and is
asserted separately; it is never replaced with the Rust verified shape.

The first closed `ruleConfigurationPair` admits only the reviewed Java
TransformerTest RequestAction/ResponseAction and Rust light-rule CEL `true`
conditions with `request-body-literal`/`response-body-literal` action references.
Their fixed actionValues request `request-transformed` and `response-transformed`
bodies. The schema and Java resource freeze the complete pair. Shared rule/service
files, single-runner relabeling and unsupported overrides fail validation. The
Java runner writes the native rule and its existing MultiThreadRuleExecutor
binding before gateway initialization. This declares a future Rust adapter
contract; it adds no Rust execution or runtime implementation.

The closed backend-call marker `eventClock: "invocation-wall-clock"` is restricted
to E-10g with explicit `clock.mode: "relative-wall"`. It checks only the approved
gateway-generated `requestContext.requestTimeEpoch` and `requestContext.requestTime`
fields: a nonnegative integer epoch within the measured invocation interval, and
an English UTC `dd/MMM/uuuu:HH:mm:ss Z` string agreeing to the second. It does not
exclude other backend fields or claim a fixed verifier clock. Actual backend
events remain in results; `invocationWindows` retains the measured bounds for
each step. Historical Java inbound timestamps remain exact when their Java
expectation does not select this marker. Rust expectations are authored contract
assertions, with execution deferred to WP13.


## Approved rule configuration pair

Owner specifies native Java yaml-rule and Rust light-rule with CEL, and approves the scoped T-01/T-02 rule configuration pair. Java retains reviewed native rule/actionClassName declarations; Rust declares CEL conditions and explicit named actionRef/actionValues. Request/rawEvent inputs and non-rule configuration remain shared. Rule executor selection belongs to this closed rule configuration declaration. Each mutation must be authored from approved behavior and reviewed before admission. No portable generic action DSL, request transformation between runners, production repair, Rust gateway or WP13 adapter is authorized. Existing selected Java class-plugin Init rejection fixtures remain intact under T-03.

## Approved request-stage failure probe

Owner approves closed C-04 request-stage failure probe outside production configuration, with two fixed modes only: throw a test exception or return null status. Same input and probe declaration for both runners. Test adapters register it before dispatch: Java actual ChainExecutor continues, while proposed Rust fails closed with exact 500 ERR14004 and zero later dispatch. No arbitrary callbacks, production repair, Rust implementation or per-runner input transformation. Retain original Java-only configured-handler Init-rejection fixtures. Probe execution must be asserted, not merely inferred from a successful backend response.

## Approved process-start Init identity

Owner approves closed synthetic runtimeIdentity with functionName only, passed as AWS_LAMBDA_FUNCTION_NAME at per-fixture JVM process launch and later Rust runner launch. D-07 Init self-target rejection uses this process-start prerequisite. No credentials, general environment overrides, request transformations, production behavior change or Rust runtime implementation. Synthetic qualification identity is test-only-gateway; declarations are validated before launch and checked before Java gateway initialization.

## Approved closed EMF contract

Owner approves closed M-01 per-request EMF contract: namespace LightLambda; dimensions FunctionName and Route; Requests=1; Errors=0 for success and 1 for gateway error; Duration is nonnegative milliseconds; _aws.Timestamp is bounded to the invocation window. Exact field names/types and no extra fields. Preserve actual emitted output. Keep separate reviewed Java APM expectations. This freezes WP3 fixture assertions only; no Rust metrics implementation, general JSON exclusion or body mask is authorized.

## Approved closed JWT-cache prerequisite

Owner approves closed JWT-cache test prerequisite outside production configuration: Java existing CaffeineCacheManager with reviewed jwt and jwk caches, later Rust adapter uses its existing security caches. Shared requests, token recipes and policy configuration remain unchanged. No production cache behavior changes, general service binding language, static reset, Dynamo constructor seam or Rust cache implementation. Closed recipe caffeine-jwt-jwk-v1 uses jwt expiry15min, jwk expiry600min, each maxSize1000, established WP2 prerequisite.

## Closed M-01 EMF assertion

The owner-approved `emf` expectation is a closed recipe, not a body mask. For the declared synthetic context and route, require exactly one JSON stdout record per invocation. Its exact root fields are `_aws`, `FunctionName`, `Route`, `Requests`, `Errors`, and `Duration`. `_aws` has exactly `Timestamp` (integer Unix milliseconds bounded by invocation start/end) and `CloudWatchMetrics`, a one-item array containing exactly `Namespace: LightLambda`, `Dimensions: [[FunctionName, Route]]`, and `Metrics: [{Name: Requests, Unit: Count}, {Name: Errors, Unit: Count}, {Name: Duration, Unit: Milliseconds}]`. `Requests` is integer 1; `Errors` is integer 0 for success or 1 for gateway error; `Duration` is a finite nonnegative number in milliseconds. FunctionName and Route equal the explicitly declared stable values. Missing/extra fields, extra records, malformed JSON, incorrect names/types/units/dimensions or stable values fail.

Result `emf.rawLines` preserves actual emitted lines, including their framing. Its invocationStartUnixMs/invocationEndUnixMs record measured bounds. Malformed emitted content remains representable for failed results; schema validity alone does not assert EMF conformance. WP13 must parse and assert the closed recipe against the actual lines. WP3 implements no Rust emitter or comparator. Java APM expectations remain separate and actual Java output is unchanged. All other body expectations retain exact bytes by default.

## Approved loopback-JWKS and metrics error amendments

Owner approved two WP3 amendments. Closed loopback-JWKS test infrastructure prerequisite: Java may use generated synthetic JKS/client settings; Rust uses supported client settings without JKS. Logical JWKS service, keys, claims, responses, rotation and application policy are shared unchanged. Hostname/certificate verification stays enabled wherever HTTPS is exercised. Readiness and actual JWKS requests are mandatory for refresh/rotation coverage. JKS rejection fixtures remain required; JKS is not supported by Rust. Exact configuration-key and generated-artifact allowlists must be documented before admission; unrestricted overrides are prohibited.

M-01a enabled-metrics/missing-target intentional difference: preserve observed Java 500 ERR14004 and establish its cause; proposed Rust 400 ERR10086 with the approved closed EMF Errors=1 record. Assert missing target and zero business dispatch. No repair, metrics disabling or changes to unrelated missing-target cases. Proposed Rust assertions are not execution evidence.

Admission boundary: unadmitted drafts are separate from the admitted fixtures/v1 corpus and cannot count toward qualification or coverage. Historical manifests are preserved.

## Closed loopback JWKS prerequisite: synthetic-jks-client-v1

Only the Java test adapter may substitute client.tls.loadKeyStore=false, client.tls.loadTrustStore=true, client.tls.trustStore=test-only-truststore.jks, client.tls.trustStorePass=test-only-store, and client.tls.verifyHostname=true. The generated artifact is an empty local JKS under the runner-owned config directory; the password is synthetic. No certificates or production trust are imported. The shared supported Rust TLS configuration must be exactly loadKeyStore=false, loadTrustStore=false, verifyHostname=true. Other client keys, policy, raw events, tokens and JWKS responses/rotation stay shared and unchanged. The current JWKS dependency uses HTTP loopback; this recipe makes no HTTPS trust claim. HTTPS cases must retain certificate and hostname verification and need separately declared generated trust artifacts. Init readiness checks read and validate the JKS before gateway initialization; assertions must require actual JWKS calls, so failed Init or verification bypass earns no refresh/rotation coverage. Existing unsupported-JKS Rust Init fixtures remain separate.

## Closed bounded-work probe

Owner approves closed bounded-work probe outside production configuration for D-08a/D-08b/D-08c: fixed request/response stages and finite delays, shared invocation-budget inputs, exact timeout and dispatch assertions. Java uses existing test seams; Rust expectations describe approved deadline behavior, with no Rust execution implementation. No general per-runner configuration overrides, production changes, or expansion of the T-01/T-02 rule pair. Existing early-rejection/slow Java fixtures stay preserved.

## X-03a closed filter prerequisite

Owner approves X-03a filter-response intentional change: with explicitly configured synthetic info filter, Rust GET /adm/logger returns exact UTF-8 JSON {"filter":"info"}, status200, application/json. Java named-logger results remain exact and separate. No logging mutation or Rust endpoint implementation. Closed fixture-only loggerFilterPrerequisite=info-v1 declares only the future Rust test adapter filter info; it grants no arbitrary environment/configuration overrides and does not change Java logging or request events.

## Q-02 generated correlation representation

Backend eventCorrelation is closed to java-base64url128-v1 or rust-base64url128-v1. It validates the generated ID as exactly22 base64url characters and substitutes only the explicit ${generated:correlation} expectation marker in asserted exact header maps. Java asserts X-Correlation-Id in the single-value forwarded map and the unchanged exact multimap. Rust asserts x-correlation-id in its single-value map and exactly one identical value in its multimap. Additional/missing headers, multiplicity changes, incorrect formats or unequal map values fail. The raw result is preserved. This represents Q02 already-approved generation; it grants no body/status/auth/backend-target/call-count exclusions or arbitrary JSON-pointer masks. Existing nondeterministic declarations remain unchanged.

## JWKS Init and request evidence

Closed loopback-JWKS fixtures explicitly assert expectJavaInit and expectInit as typed Init outcomes, separate from request expectations. Init HTTP responses are invalid. Bootstrap counts and exact GET method/logical jwks target/URI traces are asserted independently from per-request jwksFetches and jwksCalls; trace arrays have exact lengths. Init exceptions preserve actual request counts and exception identity. Bootstrap is explicitly disabled in the failed-refresh cooldown test so the first request establishes the attempted refresh. Rust Init bootstrap records cooldown per approved S13a. No readiness or rotation credit follows from an Init failure or verification bypass. Raw outputs remain unchanged.

## Closed blocking-lease prerequisite

Owner approves the closed D-08c blocking-lease prerequisite: test capacity exactly 1, fixed finite work, started/completed/active counts at response and after work returns, and exact late-result discard assertions. This is test infrastructure and authored Rust expectations only; no Rust semaphore/runtime implementation, arbitrary capacity overrides, production changes, or request transformations. Java retains its synchronous observed behavior. Named recipe request-300ms-capacity1-v1 uses one request-stage test worker, a capacity-one test semaphore, 300ms work and bounded post-response observation. Actual observations are preserved.

Blocking-lease recipe is `response-300ms-capacity1-v1` (response stage, correcting the initial request-stage description). It requires one fixture invocation, empty ordinary response chain, capacity one, fixed 300ms work, shared 100ms budget with responseReserveMs=40/postReserveMs=20. Java records synchronous completion and no discarded result. Rust must record held permit/active work at its 504 response and released permit/completed work after return, with exactly one discarded late result. These are exact observations, not timing/count exclusions. WP13 must exercise the production lease path and boundedly wait for actual completion; no Rust runtime is implemented by WP3.

## V-03a decoded-body sanitizer

Owner approves V-03a decoded-body sanitizer contrast. For shared valid ALB multimap input body eyJ2YWx1ZSI6IjxiPiJ9, isBase64Encoded=true and HTML body sanitization, Java forwards the encoded string unchanged; Rust decodes exactly once, sanitizes JSON bytes to {"value":"&lt;b&gt;"}, and re-encodes the business envelope because the input was base64. Separate exact backend bytes and flags are required. No per-runner event transformations, production repairs, unrestricted exclusions, or Rust execution claim. V-03 remains equivalent for the other supported sanitizer behavior.

## Closed test-config flag prerequisite

Owner approves the closed S-14/S-17 insecureTestConfigPrerequisite:"allow-v1" process prerequisite. The exact single environment key is LIGHT_LAMBDA_ALLOW_INSECURE_TEST_CONFIG=1. Default fixture child processes remove any inherited value; declared fixture processes set exactly1. Requests, policy settings, and expected outcomes are unchanged by the adapter. Existing production-default rejection fixtures remain. No general environment overrides, credentials, production changes, or Rust runtime implementation. The override never relaxes the maximum Light Lambda clock skew of60 seconds.

## Closed trace-context probe

Owner approves Q-01a warm trace-context isolation and closed traceContextProbe:"dispatch-v1". At actual business dispatch, Java records its native MDC tid and exchange attachment, while Rust records actual request-local log-context and exchange trace IDs. First supplied trace is present in both; a warm absent-header request preserves Java MDC tid but has a fresh empty exchange attachment, whereas Rust has empty request-local IDs. Probe is read-only, outside production handler configuration, and never derives log context from headers or rewrites responses. Java MDC is not cleared or normalized. Earlier observer-class Init rejection fixture stays preserved. No Rust implementation or execution evidence.
Result traceContext has exact captures, logContextTraceabilityId and exchangeTraceabilityId fields. Null is an absent native context value; the same gateway and Java thread execute warm steps without MDC cleanup. Rust adapter must inspect the actual span/request context at dispatch, not fabricate IDs from input.

## Closed Init failure reporting

Owner approves a closed CFG-05-only initFailureReporting expectation: endpoint /2018-06-01/runtime/init/error, reportCount=1, exitCode=1. It is valid only for Rust Init failure expectations/results. Java actual Init exception or response remains separate and unchanged. This represents the approved runtime failure/reporting clause for later Rust execution; WP3 implements no Rust runtime/runner. No general endpoint or exit-code masking.

Q-01a source-reviewed qualification correction: Java Attachable is a record keyed by Class; LambdaApp.APP_ID and trace attachment both use String.class. The warm absent-header exchange therefore contains test-only-gateway, while MDC retains the prior trace ID. Initial null prediction/failure is preserved at 20261011T050406.859732Z; corrected exact assertions passed 20261011T050844.828187Z. Rust request-local null expectations remain unchanged. This corrects the Java source prediction, not production behavior or an approved Rust contract.

## Bounded OAuth, logging and observation prerequisites

Owner-approved bounded WP3 decisions:

1. B-01 closed loopback OAuth test prerequisite. Java may install exactly client.request.connectTimeout=1000, timeout=1000, maxRequestRetry=1; client.tls.loadKeyStore=false, loadTrustStore=true, trustStore=test-only-truststore.jks, trustStorePass=test-only-store, verifyHostname=true; service.singletons CacheManager -> CaffeineCacheManager; cache.caches exactly token expiryInMinutes=15/maxSize=100. Rust shared client TLS uses loadKeyStore=false/loadTrustStore=false/verifyHostname=true and supported token cache infrastructure. No token prepopulation, arbitrary overrides, event/policy/provider/scope/response changes or TLS verification bypass. Generated trust artifact is runner-owned synthetic JKS; actual logical dependency readiness, OAuth request fields/counts, warm cache reuse and forwarding/errors must be asserted. Init rejection does not establish B-01. Unsupported JKS fixtures remain.

2. CFG-01 permits closed Rust-only Init/component logging expectations for structured stdout, default info and explicit RUST_LOG filtering, with exact required fields, forbidden file/Logback output and bounded dynamic fields. Java bootstrap observations remain separate. This is authored contract, not fabricated Rust output: WP9/WP10 implement/execute; WP13 integrates fixture execution.

3. T-04 remains deferred: missing enforceable bounds must reject Init; a Java class-plugin rejection does not prove that reason. Finite slow-handler, blocking lifetime and completion probes belong to applicable D-08 rows, are test-only harness inputs outside production configuration, and may not define a new supported plugin mechanism or a mock Rust pipeline.

4. C-09/D-09 permits closed test-only named response-handler entry/completion and finalization counters. Entry measures actual adapter entry; completion measures delegate return (including a thrown failure via finally); finalization measures actual exchange completion, not backend calls. Java observations must be captured transparently rather than hard-coded. Rust authored expectations distinguish ordinary error, response timeout and oversize replacement: no second response-policy pass, exactly one finalization. Rust execution remains deferred.

## Closed component and observation semantics

WP3 bounded decision representation details:

OAuth initialization order: start owned logical loopback service and resolve only declared dependency URL; prepare closed client/Caffeine configuration before gateway/static configuration; write runner-owned synthetic JKS and, for HTTPS, add only the generated loopback certificate; assert readable/loadable trust artifact and actual CacheManager.getInstance() Caffeine token cache before gateway Init; then acquire through production TokenMiddleware/OauthHelper. No cache token is prepopulated. client.request keys remain exactly connectTimeout=1000, timeout=1000, maxRequestRetry=1. The cache declaration has token expiry15min/max100. Shared application OAuth settings, scopes/client ID/secret and provider behavior are untouched. OAuth HTTP assertions compare one Authorization Basic credential value derived from test-only-client:test-only-secret, one application/x-www-form-urlencoded Content-Type and one application/json Accept value, with HTTP case-insensitive name semantics and exact multiplicity; raw wire header names/values remain in results. This does not normalize ALB input/output maps. Acquired signed token recipe remains generated at execution.

CFG-01 closed component calls the real production logging initializer in its eventual assigned WP, emits fixed DEBUG/INFO/WARN/ERROR test events (target light_lambda::init, fields exactly message=test-only-<level>), and captures whole component stdout/stderr/file writes. default-info-v1 leaves RUST_LOG absent and requires INFO/WARN/ERROR in order; configured-debug-v1 sets exactly RUST_LOG=debug and requires all four; configured-error-v1 sets exactly RUST_LOG=error and requires ERROR only. Each stdout JSON line has exactly timestamp, level, target, fields; timestamp is valid RFC3339 within recorded process start/end, fields has exactly message. Missing/extra records/fields, wrong levels/messages, malformed JSON, any stderr, any file writes and forbidden logback,/opt/logback.xml,test-only-secret output fail. Actual raw stdout/stderr and process window remain in result. WP3 validates only the contract and Java skipped representation, not Rust logging execution.

Response counters: entries increments at actual entry into fixed named response0 (original ResponseHeaderMiddleware) or fixtureWork (approved finite response probe). completions increments when that same delegate returns or propagates a thrown exception, using a Java finally block; it is not a successful-response count or backend count. finalizations increments through actual exchange response-completion callback installed once per exchange, including an unchanged OpenApiMiddleware test subclass for Java request-error short circuit. Ordinary returned error: Java entry0/completion0/finalization1 versus Rust1/1/1. Policy-produced oversize: 1/1/1, replacement does not reenter response policy. Cooperative response timeout: Java1/1/1 versus Rust1/0/1 at response, because canceled bounded work has not returned. Real Rust executor observes these only in its assigned implementation/qualification WPs. No mock Rust pipeline exists.

T-04 remains owner-deferred. Existing Java class-action rejection fixtures prove unsupported class selection only, not missing enforceable bounds. The missing-bounds admission reason remains assigned implementation qualification; finite work/cancellation/blocking lifetime probes are only D-08 harness prerequisites. No unbounded action support or plugin mechanism has been introduced.

Preserved failures: OAuth readiness initially used the wrong SingletonServiceFactory accessor (runnerError at 20261011T122309.857043Z); corrected accessor uses production CacheManager.getInstance(). Initial C09 counter prediction1/1 was disproved by actual0/0 (20261011T122652.740633Z), consistent with approved ChainExecutor short circuit; failed draft remains archived. A synthetic negative-test result omitted required headersAbsent and failed schema; corrected test adds that required field without weakening counter assertions. UNKNOWN qualified GitNexus test-symbol lookups were supplemented by sole test-runner callers, package/private visibility and direct constructor/reflection analysis; production constructors/behavior unchanged. cross_validate impact was LOW with one direct validate caller and no execution processes.

## S-12a narrow Unified scheme restriction

Owner approves narrow S-12a Basic-scheme restriction for a JWT-only Unified prefix. Exact input: valid synthetic Basic credential supplied in both unchanged ALB maps, both handlers configured, selected prefix jwt=true/basic=false. Preserve Java 200 ok/x-backend synthetic/one FnX call. Rust authored expectation is 401 ERR10000 with existing standard INVALID_AUTH_TOKEN JSON, application/json and zero calls; no request JWKS fetch. Init dependency readiness remains separately asserted. This is deliberate policy contrast, not E-13 ingress rejection, and no per-runner event/config transformation. Diagnostic was unadmitted before this approval and is preserved separately.

### Closed WP3 probe qualification additions (2026-10-11 13:xx UTC)

Bounded-work recipes remain fixed finite harness declarations outside production handler configuration. `request-response-100ms-v1` inserts the same fixed100ms test action in both stages, uses remainingTimeMs150, responseReserveMs20 and postReserveMs10, and asserts two actual entries. Request deadline is entry+130ms and response deadline entry+140ms; neither stage receives a fresh duration. `request-ingress50-100ms-v1` uses remainingTimeMs150 and ingressTimeoutMs50, with the same reserves and one100ms request action: the ingress hard limit wins. Existing request/response100ms recipes remain context50ms and one entry. Real Rust executor qualification is later; this crate only validates contracts.

`ruleInputProbe: native-response-map-v1` is a token-free, closed T-02/native-CEL observation. The Java probe reads existing TransformerTest.input before/after each step without writes or resets and reports zero executions if the action did not replace that map. It preserves the actual complete input. Java getReadOnlyRequest clones the current request when called; responseHeaders intentionally contains request headers, while requestBody is the current transformed request body. Fixture expectations reject missing/extra input fields and incorrect stable values. Rust must observe the real engine input in its later implementation, never manufacture this result.

The approved rule configuration pair now has four exact allowlisted resources: original body, metadata, local and denial. Rust executor remains light-rule with CEL. New named literal actions declare exactly the reviewed request-path/query/header/body changes, local202/body with actual business dispatch, or validation-error status/body. All events and non-rule policy are shared. Unknown pairs/actions/values fail schema admission; no arbitrary runner configuration or supported class-plugin mechanism is added. T-02 contextual references in T-01 effect cases do not earn behavior coverage. Separate selected-Java-plugin Init-rejection fixtures remain.

New V-03 fixtures cover html-content, xml and xml-content less-than/greater-than escaping with an ignored body field. Their limited assertions do not establish every supported encoder/input family or sanitizer ordering.

## Response cap and audit UTF-8 byte-bound amendments

Owner approvals: D-09 requires Init rejection when light-lambda.maxResponseBytes is below4096 UTF-8 bytes.4096 is accepted by this minimum gate; normal envelope and replacement size checks remain required. Default-limit oversize fixtures and exact Java behavior remain unchanged. This closes the undefined too-small replacement-cap outcome without manufacturing Runtime API errors. A-02 selected audit string values of at most1024 UTF-8 bytes are retained; longer values are replaced with exact <truncated>. This applies to the string value selected for the audit field (including selected header values), not to the underlying request, backend event, body, authorization outcomes or result payload. Approved audit field names and unsafe-selector rejection remain unchanged. Java actual unbounded selected values are preserved in separate expectations. Rust expectations are authored contract assertions, not executed evidence.

Closed lease saturation recipe `response-300ms-capacity1-two-callers-v1`: exactly two identical request events, capacity1,300ms finite response work. Launch caller2 only after observing actual caller1 work-start publication, while work has not returned. No production environment/configuration or input transformation. Aggregate atResponse snapshot is taken after both responses; afterReturn waits for real finite work. Java reads existing test-probe counters without reset, executes actual synchronous pipeline, retains both200 responses and exact business calls; Rust later executor must timeout both while retaining permit until return. Only one work starts; late result discarded exactly once. S-13b concurrency recipe remains separate.

Closed ruleFailureProbe=caught-response-error-v1 distinguishes internally caught engine failures from C04 escaping handler failures. Exact native/CEL pair uses original Java action exception and Rust standard CEL conversion int(throw) evaluation failure before bounded literal action. Both preserve backend response and require one caught failure. Java runner temporarily enables ERROR only on ResponseTransformerMiddleware with an in-memory appender, restores prior level, and has no console sink; root remains OFF. Initial root-OFF counter0 failure retained; assertion stays1. No Rust execution.

## Closed JWKS gate cancellation component

Owner approves a closed Rust-only shared-gate cancellation component probe: abort exactly one actual in-flight synthetic JWKS fetch only after its request starts; assert cleared active state, recorded cooldown and no new fetch for immediate unknown-kid verification. Java has no asynchronous shared-gate counterpart; synchronous failure/deadline observations remain. WP3 authors schema and expectations only; real gate execution belongs to WP6 and fixture execution integration WP13. No mock Rust pipeline, general scenario language or production changes.

Owner-approved closed rustJwksGateCancellationS13a component: abort-started-fetch-v1 owns exactly one actual shared-gate fetch; waits at most1000ms for the declared synthetic GET /jwks to start, then aborts that fetch (not merely its caller). Request readiness and one request asserted, active state cleared, pre-I/O attempt record/cooldown retained, immediate unknown-kid repeat denied ERR10000 with zero new fetches. Published k1/key-generation/signature recipe and500ms delayed JWKS are synthetic. No HTTP gateway response or Init failure is fabricated. Java has no async gate counterpart and emits valid skipped result without Init. WP6 implements/qualifies actual gate and WP13 integrates fixture execution; WP3 schema validation and Java skip are not cancellation execution evidence.

## Closed H-04 router component

Owner approves a closed H-04 router-component fixture for the already-failed exchange branch. Actual LambdaApp cannot reach it because ChainExecutor stops before router entry on failed state. Initialize the real configured router, deserialize exactly one shared raw ALB event with service_id, mark test exchange failed before direct router call, assert actual handler Status ERR10087/400 and exact bytes with zero HTTP/business calls. Represent as handler component status, not invented gateway response. Normal gateway fixtures remain. Rust later uses the real router in WP11/WP13; no WP3 Rust implementation or execution evidence.

### Closed JWT logging observation
`securityLogProbe: jwt-trace-v1` temporarily observes only Java LambdaApp DEBUG and JwtVerifyMiddleware TRACE through in-memory appenders, restoring prior levels. Root logging stays OFF. The later Rust runner uses supported RUST_LOG=trace. The probe asserts token/fragment/synthetic-claim exposure flags; result securityLogEvidence retains formatted events with generated token material replaced by declared placeholders. It never changes response bytes, requests, verification or dispatch.

### Closed H-04 router component
`routerFailedExchangeH04` initializes the real configured router through LambdaApp, deserializes the shared raw ALB event with the Runtime-compatible mapper, and marks the test exchange failed before calling that router directly. The exact handler Status is represented as `handlerStatus`, not a gateway HTTP response. Failure state 10, ERR10087/400, exact Status JSON and zero HTTP/business/JWKS calls are asserted. Normal gateway cases remain intact; the Rust assertion is authored for the real router in WP11/WP13 and has no execution evidence.

Closed native/CEL pair recipes additionally cover request engine failure and response-header remove/update. Request native null-body switch and CEL integer conversion fail inside their engines and are caught; count is from actual named logger events. Response header literal action is finite and test-only; it cannot register production plugins or override shared inputs.

### Shared issuer policy contrast
Owner approves narrow S-04a: shared security.issuer=test-issuer and signed iss=wrong-issuer, with identical events and logical JWKS prerequisites. Java has no issuer policy setting and preserves accepted token/200/one FnX dispatch. Rust reuses supported issuer policy and returns401 ERR10000 with zero business dispatch and zero refresh on issuer failure. Matching issuer remains accepted. No input transformations, Java behavior repairs or Rust implementation; expectations authored, not executed Rust evidence. Original no-configured-issuer observation does not establish issuer rejection.

Backend expectation validation checks that event subset fields cannot contradict eventExact and that eventAbsent cannot name a field explicitly asserted by either map. These are authored-assertion consistency checks, not behavioral execution. Original contradictory Rust query assertions remain historical evidence. Encoder1.5.0 registry has22 contexts; golden significant encodings are source/binary reviewed. Cached1.3.1 source is not claimed aligned; actual1.5 XML-comment maps every hyphen to tilde and CDATA has its current bracket literal.

### Closed CFG-05 reports and Init panic probe
Owner approves closed CFG-05 reporting fields: exactly one /2018-06-01/runtime/init/error report; Lambda-Runtime-Function-Error-Type Runtime.InitError; errorType InvalidConfiguration; missing-stage message configuration: missing light-lambda.stage; maximum 1024 UTF-8 bytes; forbidden synthetic secret sentinels; exitCode 1; invocationLoopEntered false. Java actual outcome remains separate. Owner also approves Rust-only initPanicProbe prepare-local-config-panic-v1 at the real Init preparation step, one Runtime.InitPanic report, exit 1, no invocation loop. WP3 defines and validates authored assertions only; WP9/WP13 execute real catch_unwind, never a mock Init.

CFG-05 report assertions preserve earlier three-field Init expectations for other admitted invalid-configuration cases; these do not claim header/body/redaction coverage. Complete missing-stage and panic reports assert exact JSON objects and headers. The fixed test panic message is prepare_local_config: synthetic Init panic; the later adapter must trigger that panic at the real preparation step. Result reports preserve actual posted body/header values. No Rust records are generated by WP3.

### Closed Init-report faults and shared-fetch panic
Owner approves closed CFG-05 Runtime API faults: unset endpoint, refused loopback connection, non-success HTTP, and accepted connection with response withheld beyond 2000ms reporting budget. Deterministic missing-stage Init failure; zero attempts if unset, one attempt/no retry otherwise; received POST counts distinct from attempts; no invocation polling; exit1 within reporting budget plus explicit1000ms scheduling tolerance; bounded redacted diagnostic output, no config values/secrets. Rust-only authored contracts, real reporter WP9/WP10 and runner integration later. Owner also approves closed S-13c shared-fetch task-boundary panic after attempt recorded/active published: starter, two joiners via barriers, panic before network I/O, waiters terminate in their own budgets with no verified principals, active cleared/cooldown retained, post-cooldown new attempt succeeds. Counts for starts/joiners/network/cleanup/recovery separate. No exact public error code asserted because panic waiter mapping remains unspecified; later WP6 real-gate execution determines mapping within approved authorization outcomes. Harness faults are not production keys. No mock runtime/gate implementation.

### Closed Init reporting transport faults
These Rust-only components are declarations, not a WP3 reporter implementation. Each triggers the exact missing-stage typed failure before any invocation loop. The harness sets only the closed synthetic Runtime API endpoint selected by the mode. No credentials, production endpoint or arbitrary environment override is permitted.

* runtime-api-unset-v1: remove AWS_LAMBDA_RUNTIME_API; zero connection attempts and zero received POSTs.
* loopback-connection-refused-v1: reserve a local TCP port without listening; one connection attempt and zero received POSTs. Keep the reservation until child exit to prevent port reuse.
* loopback-http-503-v1: ready local listener receives exactly one POST with the approved header/body and returns503; one attempt, no retry.
* loopback-response-withheld-v1: ready listener receives exactly one POST, withholds response beyond2000ms, and releases runner-owned resources after child exit; one attempt, no retry.

Every mode asserts zero Runtime API invocation polls, exit1, and actual elapsed time from classification of the deterministic Init failure to process exit <=3000ms (2000ms reporting budget plus1000ms explicit scheduler tolerance). Record that timestamp from the real Init reporting boundary, not a fabricated duration. Capture actual raw diagnostic lines: a structured error carries the missing-stage redacted message, an additional transport diagnostic identifies the fault; no configuration values or secret sentinels are allowed. The contract maximum1024UTF8 bytes applies to report body, not character count. An attempted send is not proof the server received a request. Implement/execute the real reporter only in WP9/WP10; integrate later runner.

### Closed shared-fetch task panic and recovery
The test fault is outside production configuration. Start one unknown-kid attempt using a valid relative-time RSA token and actual synthetic JWKS service. Hold the task at the boundary after last_attempt and active are published but before I/O. Register exactly two joiners with their own1000ms budgets. Barriers record attempt/active publication, two joiners attached, panic release, all waiters settled, actual one-second cooldown expiration and recovery release; each barrier wait is bounded1000ms. Never replace these barriers with guessed sleeps.

Failure counters separately measure one spawned task, two joined waiters, zero received network requests, three terminated waiters and zero verified principals. Observe actual cleared active state and retained cooldown. After actual cooldown expiration, start one new attempt, receive exactly one GET /jwks, publish k2 and verify the recovery token. Record raw waiter errors; the approved contract does not specify their public error-code mapping. This unresolved mapping does not weaken termination or authorization assertions and must be resolved against real WP6 behavior before claiming executed error-code conformance. WP3 does not run a mock Rust gate.

### Deferred configuration contrasts
The paired K-01 fixture reuses only Java's closed CRUD probe, unchanged fake SDK/loopback setup and actual manager. Rust rejects the explicit shared service binding at Init and never executes that probe. No constructor seam or production change is introduced. The M-03 pair uses shared valid ingress and an explicitly selected enabled production Influx handler; Java fail-open response remains exact, while Rust rejects Init. These rejection contracts differ from execution of deferred functionality.

### Remaining clause controls and coverage boundaries
E13's closed warm diagnostic count observes only the required multi-value configuration diagnostic: first failed ingress1, next0 in one environment; preserve the actual raw diagnostic line in Rust results. Java's two successful dispatches remain exact. This is ingress coverage only. No downstream policy credit follows.

CFG04 KEY002 uses shared handler.reportHandlerDuration: absent/defaultfalse are accepted with one validator warning for that key; non-defaulttrue fails Init. Java actual Init success is separately asserted. The targeted warning observation is a validator report, not an invented stdout message or unrestricted exclusion; preserve the complete actual warning report in Rust results.

R03 collision controls assert path value42 in both pathParameters/queryStringParameters, while preserving the input multimap value original. R02 invalid-method control preserves Java's dynamic missing-target string using the supplied lowercased method; proposed Rust encoding error is exact. E14 controls separate decoded UTF8 bytes (4 accepted/5 rejected, including base64) from serialized Invoke envelope512 cap, and valid-ingress invalid base64. V02 now reaches path/query/header validation under shared identical single+multi maps; V01 includes explicit3.1 spec control. §5.3 requires each empty business envelope map to serialize null; four original authored drafts/corpus assertions were corrected from that table, with originals retained. Java maps were not changed.

R01 remains gated on a concrete wildcard/deeper-template conflict. The failed draft stays outside the admitted corpus. Existing literal precedence remains represented, but does not close longest-template precedence. No premature acceptance or equivalent label is assigned to the unapproved contrast.

Transport counter definitions: attempts counts entry into the single reporter send operation; connectionAttempts counts actual TCP connection attempts, acceptedConnections counts acceptance by the owned listener, receivedPosts counts complete HTTP POSTs received/parsed. Unset: all0; refused: send/TCP1 but accept/POST0;503/withheld: all1. prerequisiteReady records absence of the environment variable, reservation of the nonlistening refused port, or ready owned listener as appropriate; runtimeApiConfigured is explicitlyfalse only for unset. None of these fields is fabricated by WP3. Future result raw diagnostic lines reject declared secret sentinels at schema validation; the later real runner must additionally check structured diagnostic messages, redaction and all behavioral assertions. Schema validity alone never establishes conformance.

Backend match.target is unchanged. match.targets permits exactly two distinct declared alternatives for one shared response at the same call index; cross-file validation requires that set to equal the precise Java/Rust expected targets and requires both runners to make the call. Extra targets, extra calls, unused stubs and mixed target/targets are rejected. No per-runner dependency response or input rewrite is allowed. This harness capability does not authorize any routing/policy difference: each fixture still needs approved ledger review/admission. R01 diagnostics using it remain unadmitted.

### R-01a wildcard overlap
Owner approved R-01a: shared valid ALB /pets/42/toys/7 with /pets/{id}/* and /pets/{id}/toys/{toy} preserves Java wildcard precedence (FnWildcard; id=42, *=toys/7) and requires Rust longest-template precedence (FnLongest; id=42, toy=7). Exact separate backend targets and captured parameters; identical input/configuration and shared synthetic response. No Rust execution evidence. Original unadmitted drafts and failed diagnostic runs remain preserved.

## WP9 Runtime API component contract

`runtime-api/l1-08-response-post-http-500.json` records the owner-approved
L1-08 amendment for `lambda_runtime 1.4.0`: the SDK logs a non-success invocation
response-post status and resumes polling. The framework does not retry the post
or redispatch the business invocation. The closed component test exercises two
distinct invocations through the SDK's production handler/posting path, with
explicit endpoints, IDs and counts. Connection refusal/loss are separate
transport failures. A failed post does not acknowledge caller delivery, and
these assertions do not guarantee end-to-end exactly-once execution.

This is a framework component contract consumed by a focused test, not WP13
fixture-runner/comparator execution or the WP10 built-app L1 matrix. Previous
termination-after-HTTP500 assertions and their failures remain historical.



## WP13 approved closure and E-08 signing witness

The two owner-approved Rust expectation amendments are active in metadata/wp13-rust-expectations-v2.json, frozen against proposal-v2 and original digests. The prior eleven v1 records and original native fixtures/Java expectations are unchanged. H-04-transport-refused-enabled-contract-v1 is an additive explicit-activation input; the original missing-activation conflict remains visible. HTTP refusal assertions bind only the declared loopback origin and preserve all other body bytes.

E-08 execution uses production Init/JWT/service and fake Invoke. The runner records optional tokenSigningEpochs records (step, epochs map) from its own signing operation. The comparator reconstructs exact expected input claims from the fixture recipe and those epochs, checks the complete outgoing authorizer, and retains raw observations. Missing/duplicate witness, changed claims, wrong principal and extra authorizer fields fail. This metadata is not verification or authentication; production alone verifies. No unsigned input token is decoded by the adapter and no encoded token/key is stored in this witness. Existing result-v1/skipped rules remain unchanged.

Reproduce with apps/light-lambda/scripts/run-java-selected.py and the commands in apps/light-lambda/REGISTERED-DELIVERY.md / the current evidence RERUN.md. The helper uses the unchanged native runner, owned configurations and explicit fixture IDs; Java L-02 is excluded. WP12/full WP13, Router rewrites, Validator integration and paused experiments remain open.
