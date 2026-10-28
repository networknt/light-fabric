# serverless-aws (WP8)

Local ALB/REST-v1 codecs and the Lambda Invoke backend. Uses the existing
`serverless-core` invocation, verified-principal boundary, response, status,
target, cancellation and absolute deadlines. No Lambda Runtime API loop, security
verification, policy extraction, fixture runner or deployment is implemented.

`alb::decode` receives explicit runtime context. It classifies ALB by the ELB
target-group ARN, requires a multi-value envelope, retains the original JSON,
percent-decodes once (literal plus stays plus), validates HTTP fields and preserves
absent/null versus empty bodies. Query keys use lexical encoded-key order and
values retain array order; normalized collisions append in that order. ALB does
not expose the original HTTP query's cross-key wire order. `raw_query` is a
deterministic reconstruction of the provided raw map, not a wire capture.

Source identity requires an ALB configured to append XFF, with no untrusted hop
after it. Only the rightmost address in the last XFF value is used; malformed or
missing addresses remain unknown. Multi-value mode, ALB casing/encoding/body
behavior and the trusted upstream topology still require operational inventory
and L3 qualification. No deployed settings are inferred here.

`v1::build_v1_event` uses finalized normalized fields and an explicit UTC clock.
It never restores fields from `Invocation.original`. R-03 copies captures into
the scalar query map while preserving the normalized query multimap. Metadata
comes only from configured values, runtime context and the verified principal.
`Principal::from_verified` in tests supplies mapping evidence, not authentication.

`v1::parse_response` treats FunctionError independently from SDK HTTP success,
validates the proxy response and applies E-02a multi-value precedence. Header
names serialize lowercase. `alb::encode` runs after the core response policies
and finalization; it checks the actual serialized UTF-8 envelope including
headers, escaping and base64. It returns final JSON bytes for runtime wiring.
Oversize replaces the result with minimal ERR14016, without invoking policies,
finalizers or dispatch. Only content-type and one existing correlation value
bounded to 256 bytes are retained; oversized IDs are omitted, never truncated.
The replacement is also checked. Response caps below 4096 fail validation.

`Targets` maps trusted configured aliases to validated function names, qualified
names, partial ARNs or full ARNs. All configured targets are checked for self
invocation and, where supplied, account/region/partition agreement. Runtime
wiring must provide the gateway's trusted identity, not infer it from requests.
`EndpointOverride` validates HTTPS; numeric HTTP loopback requires the explicit
test switch. WP9 must derive that switch from
`LIGHT_LAMBDA_ALLOW_INSECURE_TEST_ENDPOINTS=1`. The backend clears inherited
endpoint overrides and installs only the validated override passed to it.

SDK configuration is provided by trusted Init wiring. Production execution-role
provider-chain loading belongs there. Tests construct the SDK config directly
with synthetic static credentials/region and numeric loopback; they never load
aws-config, IMDS, SSO, STS or the host credential chain. The default HTTP client
does not opt into environment proxy discovery. No TLS validation is disabled.

Invoke uses RequestResponse and LogType None. Request policies' elapsed time is
consumed: remaining time is computed from `inv.deadline`, with no recomputation
of the deadline or subtraction of reserves. Per-operation timeouts and the outer
absolute timeout/cancellation bound all attempts and backoff. apiCallTimeout and
apiCallAttemptTimeout cap the operation/attempt; connectionTimeout caps connect.
readTimeout/writeTimeout have no independent Java-equivalent socket controls;
they further cap operation time. Core owns dispatch latency accounting; metric
sink/name wiring is later policy/runtime work.

Default retries are zero. The first-priority classifier returns RetryForbidden
for all but the four approved modeled throttle codes and positively identified
connection-refused failures with no response. Other connection failures are
conservatively not retried. RetryForbidden terminates the SDK classifier chain;
NoActionIndicated would leave unsafe default retry behavior enabled. SDK HTTP
5xx, read/attempt timeouts, ambiguous socket closure and FunctionError do not
retry. Cancellation cannot undo a business invocation already delivered.

The selected SDK marks Invoke payloads sensitive, but its HTTP TRACE exposed an
unexpected x-amz-log-result header. A connector wrapper removes this unused
header before SDK tracing receives the response. All-target TRACE tests remain
enabled and assert both secret absence and permitted metadata presence. The
wrapper retains SDK TLS, pooling and timeout behavior; it performs no retry.
The SDK also prints access-key IDs in nested Debug metadata. A subscriber scoped
to the SDK future forwards every enabled event/target/level and field, redacting
access_key_id and SigV4 Credential identifiers before the caller subscriber sees
them. SDK debug fields are rendered as display text; application event fields
outside the SDK future retain their original types. Secret/session keys remain
SDK-redacted. The TRACE test verifies all three synthetic credential components
as well as payload and log-tail markers; no target filter is used.

Dependencies: aws-sdk-lambda exactly 1.139.0 with default features disabled and
rt-tokio/default-https-client enabled. Runtime API 1.14.0 and HTTP client 1.3.0
are explicit existing-lock pins for the classifier/privacy boundary. MSRV 1.94.1;
tested toolchain 1.98.1. No pre-existing package version changed; aws-lc-rs is
cryptography, not an AWS service client, and was neither removed nor upgraded.

Tests are L0 adapter/component and real-SDK-to-fake-loopback evidence. They are
not live AWS qualification, gateway authentication, Runtime API execution, or
WP13 conformance replay. See the WP8 evidence directory for commands, failures,
reruns, dependency analysis and ledger mappings.
