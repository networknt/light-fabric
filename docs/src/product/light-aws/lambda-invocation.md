# Light AWS: Lambda invocation module evaluation

Status: evaluation scope only. This page does not authorize implementation, a repository merge, Java retirement, or changes to [Light Lambda](../light-lambda/design.md) parity commitments. The first Light AWS artifact remains S3-only; see the [Light AWS design](design.md).

Date: 2026-10-16.

## Question

Should the **Lambda invocation capability** be consolidated into reusable AWS libraries and an optional `light-aws` service module? Lambda is an AWS service alongside S3, but the Lambda-hosted gateway has separate transport and lifecycle responsibilities. Consolidating the invocation backend does not require merging the entire Light Lambda product or repositories.

Distinguish **calling AWS Lambda** from **hosting the gateway inside AWS Lambda**:

| Capability | Proposed ownership |
| --- | --- |
| Long-running HTTP gateway invoking AWS Lambda | Optional Lambda handler/module in `light-aws` |
| Streaming S3 upload/download | S3 handler/module in `light-aws` |
| Gateway hosted inside Lambda, including Runtime API lifecycle and ALB event conversion | Existing Light Lambda runtime/product |
| AWS credential profiles, STS refresh, credential-provider caching, and validated client configuration | Proposed `crates/aws-client-core`, shared by both products; it links no service SDK (see the [shared AWS client crate](design.md#shared-aws-client-crate)) |
| Lambda SDK client construction/caching, `Invoke` call, and AWS error translation | Light Lambda's proposed `crates/serverless-aws`, depending on `aws-client-core` for credentials and configuration. The Light Lambda design still assigns credentials to `serverless-aws`; aligning its table is an open follow-up, and neither crate exists yet |
| Legacy HTTP-to-event and function-response conversion | Explicit compatibility adapters with their own conformance tests |

## Module structure and configuration

```text
light-aws
  aws-client-core: credential profiles / STS / client profiles -> aws.yml
  S3 module       -> object-storage.yml
  Lambda module   -> function-invocation.yml (future)
  handler chains and routes -> handler.yml
```

Build features such as `s3` and `lambda` determine which SDKs and handlers are linked. Configuration selects enabled handlers, routes, resource bindings, and operations within that build. A configured service that was not compiled in must fail validation.

`function-invocation.yml` would own logical function bindings, allowed invocation modes, payload/response adapter selection, limits, deadlines, and authorization-policy references. Each binding references a credential profile in `aws.yml` and an allowlisted function ARN, preferably qualified by version or alias. `handler.yml` connects HTTP routes to that binding. Use separate roles from S3 where permissions differ, enforce the required `lambda:InvokeFunction` resource permissions, and prevent callers from overriding function ARNs or invocation settings. For Lambda-hosted deployments, reject bindings that invoke the gateway itself and address indirect recursion in topology review.

## Existing Java customer compatibility

Customers currently using Java light-gateway with `light-aws-lambda/lambda-invoker` are a concrete reason to evaluate this module. `light-aws` can become a migration destination, but moving Java behavior into Rust is a compatibility project, not a source-file relocation. Retain the Java path while qualifying the replacement.

Before deciding package moves or implementation reuse, inventory the Java invoker and the current Rust Lambda backend. Record exact artifacts/configurations, call paths, and supported behavior. Determine which credential/client/invocation operations can be shared without pulling Lambda runtime or HTTP-framework dependencies into the shared backend. Existing backend reuse is a candidate, not an assumed completed extraction.

Build a compatibility ledger covering inbound authentication/authorization, target resolution, HTTP-to-event construction, headers and query encoding, binary/base64 handling, invocation mode, payload limits, deadlines, response/status/header mapping, SDK failures, function execution errors, retries, and observability. A successful AWS Invoke transport response does not necessarily mean the function succeeded; asynchronous acceptance does not mean execution completed. A timeout may leave the function executing, so retries can duplicate side effects. Define these outcomes explicitly instead of applying generic HTTP retry rules.

Preserve ALB/API Gateway-shaped event contracts when existing business functions depend on them, through named adapters and golden fixtures. Do not silently replace their event envelopes with a new generic payload. Compare the Java implementation and proposed Rust path with focused component tests, then qualify real AWS invocation and the intended deployment separately. Migration requires explicit acceptance of any deliberate contract differences and a rollback route to the Java deployment.

## Function invocation across providers

Define a typed function-invocation interface independently of object storage. Compare AWS Lambda invocation with the selected Azure and Google serverless invocation mechanisms before freezing it: SDK invocation and authenticated HTTP endpoints may differ in identity, synchronous results, asynchronous acceptance, response streaming, retry delivery, cancellation, and payload limits. Specify a narrow baseline with explicit optional capabilities; do not advertise semantic equivalence based only on all providers offering serverless functions.

Keep provider-native event envelopes and legacy gateway conversions in explicit adapters outside the portable core. The shared interface must distinguish transport failure, function failure, confirmed synchronous result, asynchronous acceptance, and uncertain execution outcome. Provider-specific routing, credentials, and function resource identifiers remain server-controlled configuration.

## Deliverables

The Java/Rust inventory, reuse/dependency proposal, compatibility ledger, provider capability comparison, and a separately scoped Lambda-module plan. A later Lambda module repeats the relevant Light AWS delivery gates.
