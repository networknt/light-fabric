# WP17 direct-Invoke trial — JWT-enabled configuration

Account 964637446810, region us-east-2. NEW Rust gateway only; existing Java gateway and business functions remain unchanged. No ALB, API Gateway, Function URL, public permission, VPC, subnet, security group, certificate or DNS is required. Optional ALB design remains under optional-alb/. No automatic TTL or budget alarm. Manual cleanup only. This narrower owner-directed trial does not complete the full ALB-oriented WP17 gates.

## Current artifacts

Bootstrap ZIP (unchanged): /home/steve/workspace/artifacts/implementation/light-lambda/WP13-D08e-release-20261021T155728Z/release-package/bootstrap.zip

Bootstrap SHA256: d095e89c07f6079823d6d07395b8716f280add5e7e09f7a75b1d03e121af756a

Bootstrap AWS CodeSha256: 0JXonAf2B5gj1tBzlbhxbygK3V5+CfenWx0D4SGvdWo=

JWT-enabled layer ZIP: /home/steve/workspace/artifacts/implementation/light-lambda/WP17-jwt-config-20261021/rie-v2/aws-config-layer.zip

Layer SHA256: 6980390808bfa98b0eccd8bcef16e2bd465cb37a5196bc921bafb12e5a1c4984

Config digest: dae9e51b7a727f46e81d8f34208604e561a2745556c8b4c12a44c2bebdc2a726

Older auth-disabled configurations, artifacts and qualification remain preserved. The previous runbook and configuration are saved in WP17-jwt-config-20261021/auth-disabled-runbook-before.md and auth-disabled-config-before/. The legacy qualify_direct.py targets the auth-disabled configuration; use qualify_jwt.py for the current profile.

## What changed

The existing embedded client.yml resolves oauth.token.key.server_url and uri from layer values.yml:

```yaml
client.tokenKeyServerUrl: https://dev.lightapi.net
client.tokenKeyUri: /oauth2/AZZRJE52eXu3t1hseacnGQ/keys
security.enabled: true
security.enableVerifyJwt: true
security.enableVerifyScope: true
security.ignoreJwtExpiry: false
security.issuer: urn:com:networknt:oauth2:v1
security.audience:
  - urn:com.networknt
unified-security.enabled: true
security.logJwtToken: false
security.logClientUserScope: false
```

These values are now present in config/values.yml (JSON is valid YAML). No separate client.yml or Portal instance/export is necessary. light-security SecurityRuntime uses light_client::ClientFactory to fetch and cache JWKS. Startup key-service bootstrap remains false, so a cold environment lazily fetches keys when verification needs them; warm requests reuse resident keys subject to existing refresh policy. TLS validation remains enabled. No real token, password, private signing key or AWS credentials belong in the layer.

Permanent source OpenAPI and trial copy now declare portal.r for GET, portal.r plus portal.w for POST, portal.w for DELETE. Important correction: the existing native Java matchedScopes and Rust matches_scope both accept ANY matching listed scope. POST currently accepts a token with either portal.r or portal.w; it does not require both. Earlier prose claiming both was incorrect. No production matching behavior or scope declaration was changed to conceal this observation. Requiring write-only POST or all-scopes matching would need an explicit authorization-policy decision.

## Exact backend identities

| Method/path | Unqualified ARN |
|---|---|
| GET /v1/pets | arn:aws:lambda:us-east-2:964637446810:function:PetsGetFunction |
| POST /v1/pets | arn:aws:lambda:us-east-2:964637446810:function:PetsPostFunction |
| GET /v1/pets/{petId} | arn:aws:lambda:us-east-2:964637446810:function:PetsPetIdGetFunction |
| DELETE /v1/pets/{petId} | arn:aws:lambda:us-east-2:964637446810:function:PetsPetIdDeleteFunction |

Verified against native config/values.yml and OpenAPI's /v1 server base. Owner accepted these unqualified identities, which invoke $LATEST. No aliases or backend changes are invented. Runtime Invoke IAM in template.yaml and console-execution-policy.json is limited to these four functions; logs access is limited to the new gateway log group.

## Console upload and direct Test workflow

1. Sign into the target account/region. Use a NEW function named light-lambda-rust-direct-trial-gateway; stop if it already exists unless ownership is established. Leave Java functions alone.
2. Create its dedicated log group /aws/lambda/light-lambda-rust-direct-trial-gateway (7-day retention) and a new dedicated execution role using console-role-trust.json and console-execution-policy.json. If the function name changes, update its log policy and identity consistently.
3. Upload the JWT-enabled ZIP above as a NEW version of the separate light-lambda-rust-direct-trial-config layer. This upload is not yet completed: previous browser filechooser timed out and CLI identity verification failed InvalidClientTokenId. Browser file upload needs the ChatGPT Chrome extension's Allow access to file URLs setting; CLI is an alternative only with working credentials. No backend/public permissions are required for publishing the layer.
4. Create the new gateway as provided.al2023, x86_64, 512 MB, timeout 20 seconds, reserved concurrency 2. Upload the unchanged bootstrap ZIP. Attach only the exact new configuration layer version. Do not use the older auth-disabled layer for JWT tests. No VPC, public URL or trigger is configured.
5. Set LIGHT_LAMBDA_CONFIG_DIR=/opt, RUST_LOG=INFO and LIGHT_LAMBDA_GATEWAY_ARN=arn:aws:lambda:us-east-2:964637446810:function:light-lambda-rust-direct-trial-gateway. Never set test endpoint/credential, LoggerSet or worker-activation flags. The human invoking Test separately needs lambda:InvokeFunction on the new gateway; execution-role permission does not grant that to the human.
6. Create PRIVATE Console Test events from events/list.json, post.json, get.json and delete.json. For successful requests add a multiValueHeaders entry: "authorization": ["Bearer YOUR_FRESH_ACCESS_TOKEN"]. Do not duplicate it into headers or paste tokens into shared events, repository files or evidence. The actual portal token inspected earlier expires; obtain a fresh access token at execution time. Use issuer/audience above and matching portal scopes.
7. Test missing token (401, zero Invoke), unrelated scope (403, zero Invoke), then valid list/post/get/delete, malformed JSON with a valid token (400 ERR10015, zero Invoke), then valid list for warm recovery. Console execution success alone is insufficient: inspect returned statusCode and body. Preserve sanitized responses and request IDs. Do not infer Java execution or zero dispatch from status alone.

Generated events are existing-adapter ALB-shaped JSON including requestContext.elb and multi-value maps. Direct Invoke passes that JSON unchanged; no ALB resource exists. The owner's supplied native console event is API Gateway-shaped and is rejected with 400 ERR14010 before JWT verification. A token-free structural copy is events/native-console-sanitized.json. No API Gateway compatibility is claimed.

POST uses synthetic Pet {id:1792600000001,name:light-lambda-direct-trial-1792600000001,tag:light-lambda-direct-trial}; DELETE includes a synthetic key header and the same ID. Confirm deployed endpoints still match the owner's dummy implementations before writes. Checked-in POST returns 200 {} (OpenAPI says 201); DELETE returns a fixed pet id 1 response, with no persistence in either implementation. Never delete id 1 because a dummy response mentions it. If real persistence exists, first establish absence and choose a fresh ID/tag; delete only a record actually created and owned by this trial. No bulk cleanup. Validator is unavailable, so {} schema rejection is not expected. Dummy backends lack an error-injection switch; local backend-error proof remains separate.

## Optional IaC and CLI

Choose one deployment method. Console uploads require no artifact bucket or CLI profile. The unchanged minimal template.yaml creates exactly gateway, layer, role and logs; parameters.example.json needs an existing us-east-2 artifact bucket. A CLI profile is needed only if using a named CLI profile. These commands are preparation, not authorization to deploy or attach a layer:

```bash
export AWS_REGION=us-east-2 AWS_PROFILE=OWNER_APPROVED_PROFILE
export STACK=light-lambda-rust-direct-trial BUCKET=OWNER_ARTIFACT_BUCKET
rtk proxy aws sts get-caller-identity # stop unless account is 964637446810
rtk proxy aws s3 cp /home/steve/workspace/artifacts/implementation/light-lambda/WP13-D08e-release-20261021T155728Z/release-package/bootstrap.zip "s3://$BUCKET/light-lambda-direct-trial/d095e89c/bootstrap.zip"
rtk proxy aws s3 cp /home/steve/workspace/artifacts/implementation/light-lambda/WP17-jwt-config-20261021/rie-v2/aws-config-layer.zip "s3://$BUCKET/light-lambda-direct-trial/69803908/layer.zip"
# Fill parameters.json from parameters.example.json, including bucket and exact keys.
rtk proxy aws cloudformation create-stack --stack-name "$STACK" --template-body file://apps/light-lambda/deploy/l3/template.yaml --parameters file://parameters.json --capabilities CAPABILITY_IAM
rtk proxy aws cloudformation wait stack-create-complete --stack-name "$STACK"
rtk proxy aws cloudformation describe-stacks --stack-name "$STACK"
```

CLI Invoke: prepare token-bearing event copies only in a private temporary directory, not the evidence directory. Use fresh credentials and restrict local file permissions. Run each write step only after inspection of the prior successful creation. Preserve metadata/response separately; the checker rejects FunctionError even when Lambda transport status is 200.

```bash
export GATEWAY=light-lambda-rust-direct-trial-gateway
export PRIVATE_EVENT=/PRIVATE/TEMP/list-with-fresh-token.json OUT=/ABSOLUTE/FRESH/evidence
rtk mkdir "$OUT"
rtk proxy aws lambda invoke --function-name "$GATEWAY" --invocation-type RequestResponse --cli-binary-format raw-in-base64-out --payload "file://$PRIVATE_EVENT" "$OUT/response.json" > "$OUT/metadata.json"
rtk proxy python3 -B apps/light-lambda/deploy/l3/check_response.py "$OUT/metadata.json" "$OUT/response.json" --status 200
# Repeat for post/get/delete and valid warm list with their corresponding private payloads.
rtk proxy aws lambda get-function-configuration --function-name "$GATEWAY"
rtk proxy aws logs tail "/aws/lambda/$GATEWAY" --since 15m
```

Check CodeSha256/runtime/architecture/layer version/role/environment, no VPC/URL/public permissions, and unchanged backend configurations. get-policy should report absence of a gateway resource policy, not a wildcard grant. Use a quiet isolated window and positive controls for backend invocation counts; unrelated traffic makes aggregate zero-dispatch claims inconclusive. No business logging or IAM changes are authorized by this runbook.

## Local qualification and limits

Pinned-bootstrap offline Init/layer validation passed. Current synthetic RIE checks: 11/11 requests, seven fake signed Invokes, one JWKS fetch/reuse, no OAuth/HTTP calls. Missing token, unrelated scopes, DELETE reader-only rejection and malformed JSON reject without dispatch; all routes, POST native any-match, backend 502 and warm success are covered. Synthetic HS256 tokens use the configured issuer/audience and permanent portal scope policy; only local JWKS/Invoke endpoints differ. Real development tokens are not saved or replayed. This does NOT prove live development JWKS TLS fetching, real RS256 verification through the Rust service, or AWS IAM/execution. No paused worker/precision activity occurred.

The first new JWT test mistakenly expected reader-only POST rejection. Preserved failure evidence: WP17-jwt-config-20261021/rie/. Native JwtVerifyMiddleware.matchedScopes and Rust handlers/jwt.rs matches_scope establish any-match behavior; the new assertion was corrected, not the implementation or original fixtures. Passing rerun: WP17-jwt-config-20261021/rie-v2/. Earlier auth-disabled 9/9 and CLI checker 4/4 results remain historical. cfn-lint was unavailable in the prior template checkpoint; template is unchanged, not AWS-validated.

```bash
rtk proxy python3 -B apps/light-lambda/deploy/l3/qualify_jwt.py --out /home/steve/workspace/artifacts/implementation/light-lambda/jwt-trial-rerun-$(rtk proxy date -u +%Y%m%dT%H%M%SZ)
```

## Cleanup, costs and remaining gates

Manual cleanup only: verify trial-owned data by ID/tag before deleting it; dummy implementations store nothing. For IaC delete only the named trial stack, wait for deletion and verify its gateway/layer/role/log resources are gone. Remove only trial-owned artifact objects/versions from the supplied bucket, never the bucket itself or unrelated keys. For console-created resources delete only the separate Rust function, dedicated layer version, dedicated log group and its dedicated role/inline policy. Never remove Java gateway/business resources. Delete private token-bearing local payload copies after the test.

```bash
rtk proxy aws cloudformation delete-stack --stack-name "$STACK"
rtk proxy aws cloudformation wait stack-delete-complete --stack-name "$STACK"
rtk proxy aws s3 rm "s3://$BUCKET/light-lambda-direct-trial/d095e89c/bootstrap.zip"
rtk proxy aws s3 rm "s3://$BUCKET/light-lambda-direct-trial/69803908/layer.zip"
```

No ALB/network fixed charges. Planning estimate, not a live quote: 1000 x86 gateway calls at 512 MB/200 ms roughly $0.0017 compute + $0.0002 requests, excluding Java compute, free-tier effects, logs/custom metrics, optional S3 and public JWKS provider/transfer charges. Existing metrics emit three custom metrics per observed route/function dimension; customary first-tier planning rate is ~$0.30/metric-month with hourly proration. Verify current prices before AWS execution. Idle Lambda has no hourly compute charge; retained storage and applicable metrics remain billable until cleanup. Reserved concurrency is not a cost cap. No TTL or budget alarm was requested or created.

Remaining inputs: for console, working authorized account access/upload capability and unused trial names; for IaC additionally an approved artifact bucket/parameters and deployment identity. Successful auth tests require a fresh matching portal access token at run time. No Portal instance/export, public endpoint, CIDR or certificate is required. Prior layer upload was blocked by browser filechooser and InvalidClientTokenId; no layer has been published or attached. Validator, rewriting, child execution/platform clearance, failed bundle verification/debug provenance, original fixture failures and full WP12/WP13/WP17 remain open.


## 2026-10-22 actual console checkpoint

Owner authorized creation and explicitly approved scoped IAM at action time. Function light-lambda-rust-direct-trial-gateway now exists in 964637446810/us-east-2, non-VPC, no URL, provided.al2023/x86_64, handler bootstrap, 512 MB, timeout20, reserved concurrency2 and planned three environment variables. Verified and attached owner-created arn:aws:lambda:us-east-2:964637446810:layer:light-lambda-config:1 (JWT layer digest matches above). New generated role service-role/light-lambda-rust-direct-trial-gateway-role-ppenixvm has only PetstoreTrialInvokeAndScopedLogs, with exact four-target Invoke and own-log permissions. Broad default logging policy detached; its unattached customer-managed policy remains for explicit cleanup. Log group/7-day retention not yet configured.

Rust executable upload remains BLOCKED by browser filechooser timeout; function still has AWS sample ZIP, not pinned Rust. No invocation or business changes. Do not test until the pinned ZIP is uploaded and CodeSha256 verified. Actual remote state and screenshots: /home/steve/workspace/artifacts/implementation/light-lambda/WP17-console-create-20261022/. Creation is not AWS execution qualification.


## 2026-10-22 executable-upload checkpoint — blocker resolved

Pinned bootstrap ZIP is now uploaded to light-lambda-rust-direct-trial-gateway. AWS CodeSha256 verified as 0JXonAf2B5gj1tBzlbhxbygK3V5+CfenWx0D4SGvdWo=, exactly matching qualified ZIP d095e89c07f6079823d6d07395b8716f280add5e7e09f7a75b1d03e121af756a. Runtime provided.al2023/x86_64/handler bootstrap and verified JWT layer light-lambda-config:1 remain attached. Earlier sample-code/filechooser-blocked checkpoint is historical; current deployment-state and screenshot are under /home/steve/workspace/artifacts/implementation/light-lambda/WP17-console-upload-20261022/. No Test or backend invocation has been run; AWS execution/JWKS/IAM/warm qualification and log retention remain pending. No artifact rebuild or changes to Java/business functions.


## 2026-10-22 AWS direct-Invoke test checkpoint

Eight actual console tests passed against the unchanged pinned Rust ZIP and JWT layer: missing-token 401/ERR10002, invalid-token 401/ERR10000, all four authenticated pet routes 200, malformed JSON 400/ERR10015, and subsequent valid warm GET 200 (22.66 ms). Five successful gateway dispatch records selected the expected four aliases; rejection audits show no target and dispatch_ms=0. Exact independent backend call counts and JWKS fetch counts were not measured. Real development JWT verification succeeded with the configured HTTPS JWKS provider; token-bearing events were never saved and the editor was cleared. POST returned dummy 200 {}, rather than OpenAPI 201, as source predicts. Synthetic DELETE targeted only trial id 1792600000001, never response id 1.

Evidence and limitations: /home/steve/workspace/artifacts/implementation/light-lambda/WP17-aws-direct-tests-20261022T133153Z/REPORT.md and results.json. Backend-error injection and live restricted-scope denial remain unqualified; local fake-dependency proof is separate. No schema-validation, public-ingress or full WP17 completion claim. Validator, rewriting and paused experiments remain excluded. Log-retention configuration remains pending. Earlier no-invocation checkpoints are historical.
