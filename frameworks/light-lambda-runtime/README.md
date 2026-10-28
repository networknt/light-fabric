# Light Lambda runtime — WP9 complete under approved amendments

The framework prepares local configuration and an immutable execution snapshot,
reports redacted Init failures, and connects the existing core and AWS adapter to
the Lambda Runtime API. It contains no production binary. WP9a registers the real
JWT and JWT-only unified-security factories supplied by `serverless-core`.

`runtime::run_initialized` takes an input-preparation closure so environment errors
and panics occur inside the Init reporting boundary. Embedded templates, handler
factories and the existing WP8 `GatewayIdentity` are explicit inputs. WP10 owns the
deployment source for trusted identity; optional `light-lambda.accountId` remains
business-event metadata. No new account-ID environment variable or AWS lookup is
introduced. The required external configuration directory defaults to `/opt`;
the local `config` directory remains optional. No Config Server, registration,
transport binding or reload occurs during preparation.

Enabled handlers require injected factories. Disabled handlers use the core
compiler's disabled registration. Shared security bootstrap uses its existing API.
Test JWT handlers and the synthetic JWKS key establish wiring only, not
authentication. Tests use injected backends or static synthetic credentials and
loopback endpoints; the production AWS provider path has not been exercised.

`lambda_runtime` is pinned to 1.4.0. Source inspection and characterization show
its private response service logs raw event JSON at TRACE and continues polling
after a response POST receives HTTP 500. Production now registers `service::handle`
through the SDK's supported service path. The SDK owns polling, serialization and
invocation `/response` and `/error` posting. The earlier custom invocation poster
has been removed; its historical evidence is superseded for current wiring.
Framework-owned pre-run `/init/error` reporting remains separate.

Deadlines are computed once before decoding. Response policies and finalization
run through the core once; telemetry uses the final encoded outcome, including
oversize replacement. Production reserve sizing remains an empirical L2/L3 boundary;
the 100 ms default post reserve is provisional. Structured stdout respects RUST_LOG and redacts
dependency diagnostic values while preserving reviewed framework/adapter fields.

Latest focused qualification: 19 passed, exit 0. The real-SDK
protocol matrix passes for success, rejection, dispatch failure, timeout, internal
service error, refused POST connection and connection loss after a received POST.
Owner-approved L1-08 now matches the pinned SDK: a response POST receiving HTTP500
is logged and polling resumes. The amended closed contract verifies one dispatch
and one POST for each of two distinct invocations, the redacted SDK error between
them, and no duplicate first dispatch/post or `/error`. The fake shuts down through
a separate terminal polling error. The original failed termination assertion is
preserved as contract-correction evidence. No alternative poster, independent
framework retry, SDK patch/version change or log-controlled termination exists.

A failed post does not establish caller delivery. Observed local invocation counts
do not guarantee end-to-end exactly-once business execution. Connection refusal
and loss remain distinct transport errors with verified propagation.

The SDK configuration enum payload is boxed; its synthetic SDK-injection test
passes. Framework-only strict Clippy, formatting and locked workspace check pass.
Dependency-inclusive Clippy still exits 101 for the two unchanged config-loader
findings, covered by the scoped exception. Final locked workspace tests passed:
2,785 passed, 226 ignored, exit 0. WP9 is complete under the owner-approved L1-08
runtime limitation and the scoped dependency Clippy exception. Original failures
and historical passes remain separate evidence.

Remaining boundaries: WP9a authentication, WP10 app/full L1, WP12 remaining policy
construction, WP13 fixture execution/comparison, and L3 AWS qualification. No live
AWS or credential-provider path was exercised.
