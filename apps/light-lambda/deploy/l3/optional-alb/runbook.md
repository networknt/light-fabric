# WP17 isolated petstore trial — prepared, not deployed

Target account `964637446810`, region `us-east-2`. Dedicated ALB is the proposed topology, pending owner selection. No existing topology was identified in the inspected project configuration. This template does not attach either gateway or business Lambdas to a VPC. An internet-facing ALB separately requires two public subnets in distinct AZs; this stack owns its VPC, subnets, routing and security group. No NAT gateway is needed. Existing-ALB mode is not implemented: selecting it requires listener/subnet/security-group details and a reviewed template adaptation.

## Inputs and scope

Required: AWS profile with approved deployment permissions; explicit confirmation of dedicated ALB; client public IPv4 /32; an existing us-east-2 ACM certificate and a controlled DNS name matching it; dedicated artifact bucket/key prefix. Supply DNS management details or create the DNS alias manually. No unauthenticated worldwide ingress is supported by this template. No TTL automation or budget alarm is created.

The user supplied function names, not literal ARNs. Parameters expand them into account/region-specific **unqualified** ARNs, preserving their identities. No alias/version is invented, and no explicit `$LATEST` is substituted. Invoking an unqualified ARN uses AWS's mutable unqualified-function semantics. Confirm acceptance of that risk or supply actual qualified ARNs before deployment. If qualified ARNs are supplied, update both parameters and config/lambda-proxy.yml and rebuild only the layer.

| Gateway route | Backend name |
|---|---|
| GET /v1/pets | PetsGetFunction |
| POST /v1/pets | PetsPostFunction |
| GET /v1/pets/{petId} | PetsPetIdGetFunction |
| DELETE /v1/pets/{petId} | PetsPetIdDeleteFunction |

Verified against lambda-petstore/config/values.yml and config/openapi.yaml: OpenAPI paths omit `/v1` because its server URL supplies that base. Do not configure bare `/pets` gateway routes.

Pet requires integer int64 `id` and string `name`; `tag` is optional. DELETE additionally requires string header `key`. Checked-in BusinessHandlers are dummy fixed-response implementations: POST returns 200 `{}` (OpenAPI documents 201); DELETE returns fixed pet id 1. Neither shows persistence. Record this source/spec difference; do not weaken schema expectations to claim business conformance. Confirm deployed code is the dummy implementation before exercising writes. Tests must never delete preexisting id 1 merely because it appears in a dummy response.

## Artifact/configuration identities and adaptations

Unchanged ZIP: `/home/steve/workspace/artifacts/implementation/light-lambda/WP13-D08e-release-20261021T155728Z/release-package/bootstrap.zip`, SHA256 `d095e89c07f6079823d6d07395b8716f280add5e7e09f7a75b1d03e121af756a`.

New layer: `/home/steve/workspace/artifacts/implementation/light-lambda/WP17-petstore-trial-preparation-20261021/aws-config-layer.zip`, SHA256 `74c9f8f07589744d571295afd34e0d6fd4ff4f4fafc311bf8f726b13fd637157`; configDigest `525bcee16ce03de91a7fc1ab6c81aaae41cde9c0ff4035e555924f00409e3330`. Actual pinned-bootstrap offline Init/layer validation passed. Not yet RIE/AWS-qualified for this adapted configuration.

config/ is separate from preserved native files and the historical local layer. It retains the supported auth-disabled composed settings, project OpenAPI and test quotas (not native `10/m 10000/d`). Changes: region/account and four real target identities; remove fake SDK endpoint; remove synthetic OAuth client settings/issuer/audience; empty fake direct registry; disable Token and remove its request-chain invocation because the trial has no OAuth dependency; change stage. LoggerGet stays enabled to preserve admitted admin bindings; no LoggerSet alias or trusted startup opt-in is installed. Disabled authentication remains an explicit supported-subset policy, not native-chain equivalence. OpenAPI's localhost token URL is descriptive only and never used by this auth-disabled profile. Validator absent, rewrite rules empty, no Traceability claim. Fixed Transformer /x rules do not transform pet paths.

Initial preparation failures were real admission checks: missing offline account identity, then disabled factories still referenced by the chain (Token and LoggerGet). The final configuration supplies account identity, removes disabled Token from execution and retains LoggerGet. No production code changed.

Rebuild the layer only when configuration changes:

```bash
rtk proxy python3 -B apps/light-lambda/scripts/package_baseline.py layer apps/light-lambda/deploy/l3/config /ABSOLUTE/FRESH/layer.zip --bootstrap /home/steve/workspace/artifacts/implementation/light-lambda/WP13-D08e-release-20261021T155728Z/release-package/bootstrap
```

## Deployment commands — DO NOT RUN before approval

Use a dedicated existing bucket in us-east-2. Keep objects immutable by digest-specific keys; verify downloaded digests before deployment. Copy parameters.example.json to a private parameters.json and fill REQUIRED values; parameter targets must match the layer. Validate with cfn-lint locally if installed; no CloudFormation validation or deployment has been run here.

```bash
export AWS_PROFILE=OWNER_APPROVED_PROFILE AWS_REGION=us-east-2
export STACK=light-lambda-petstore-trial BUCKET=OWNER_DEDICATED_BUCKET PREFIX=light-lambda-petstore-trial
rtk aws sts get-caller-identity # STOP unless Account is 964637446810
rtk aws s3 cp /home/steve/workspace/artifacts/implementation/light-lambda/WP13-D08e-release-20261021T155728Z/release-package/bootstrap.zip "s3://$BUCKET/$PREFIX/d095e89c/bootstrap.zip"
rtk aws s3 cp /home/steve/workspace/artifacts/implementation/light-lambda/WP17-petstore-trial-preparation-20261021/aws-config-layer.zip "s3://$BUCKET/$PREFIX/74c9f8f0/layer.zip"
rtk aws cloudformation create-stack --stack-name "$STACK" --template-body file://apps/light-lambda/deploy/l3/template.yaml --parameters file://parameters.json --capabilities CAPABILITY_IAM --tags Key=project,Value=light-lambda-l3
rtk aws cloudformation wait stack-create-complete --stack-name "$STACK"
rtk aws cloudformation describe-stacks --stack-name "$STACK"
# Copy AliasArn and TargetGroupArn outputs exactly:
export ALIAS_ARN=OUTPUT_ALIAS_ARN TARGET_GROUP_ARN=OUTPUT_TARGET_GROUP_ARN
rtk aws elbv2 register-targets --target-group-arn "$TARGET_GROUP_ARN" --targets Id="$ALIAS_ARN"
```

Target registration is deliberately after scoped Lambda permission creation, avoiding the target-group/permission circular dependency. Target health polling is disabled; explicitly test `/health` and its zero-dispatch property. Add an owner-controlled DNS alias to AlbDns and use its matching HTTPS certificate. Never use curl -k to work around a name mismatch.

Runtime IAM is in template.yaml: InvokeFunction on exactly four parameter ARNs, log stream/write access only to the precreated group. No InvokeAsync or backend modification. Only ALB may invoke the gateway alias through its target-group-scoped permission with SourceAccount. No backend ALB permissions are added. Deployer permissions are distinct: CloudFormation stack actions, owned EC2/ELB provisioning, dedicated Lambda/layer/log resources, dedicated IAM role/policy creation and PassRole to Lambda only, plus access to the exact artifact prefix. Existing backends need read inspection, not update/delete privileges. Broad deployment administrator permissions are not a runtime requirement. Record the actual approved deployer policy before execution.

## Tests and evidence

First locally RIE-qualify the adapted four-target configuration using owned fake Invoke; this is a justified configuration-specific test, not another generic wiring suite. It remains pending. Then capture stack outputs, ZIP/layer/CodeSha256, actual backend configurations (including absence of VpcConfig), gateway alias/version, ALB header mode and invocation baselines. Do not log request credentials or JWTs.

After deployment and code-identity confirmation, use a fresh trial ID and run the following against the controlled HTTPS hostname. Save responses and status codes. POST id is a synthetic int64 below 2^53, never an existing observed record. Dummy handlers do not create state; if deployed behavior differs, stop and establish existence/ownership before writing.

```bash
export BASE=https://OWNER_CONTROLLED_HOST
export PET_ID=$(rtk proxy date +%s) # confirm absent before POST if backend persists records
export TRIAL_TAG=light-lambda-trial-$PET_ID
rtk curl --fail-with-body "$BASE/health"
rtk curl --fail-with-body "$BASE/v1/pets?limit=1"
rtk curl --fail-with-body -H 'Content-Type: application/json' -d "{\"id\":$PET_ID,\"name\":\"$TRIAL_TAG\",\"tag\":\"$TRIAL_TAG\"}" "$BASE/v1/pets"
# Continue only after POST succeeded and its trial ownership is established:
rtk curl --fail-with-body "$BASE/v1/pets/$PET_ID"
rtk curl --fail-with-body -X DELETE -H "key: $TRIAL_TAG" "$BASE/v1/pets/$PET_ID"
rtk curl -i -H 'Content-Type: application/json' -d '{' "$BASE/v1/pets" # expect gateway 400, zero Invoke
rtk curl --fail-with-body "$BASE/v1/pets?limit=1" # warm isolation after rejection
```

Do not send schema-invalid `{}` as an expected validation rejection: Validator is unavailable. Authentication-error tests are inapplicable to this explicitly disabled profile. Dummy backend error injection is not implemented; do not modify or misuse existing functions to manufacture an error. Local existing 502 evidence remains separate. WP17 delay/error/oversize/throttle/rollback/JWKS/freeze-thaw cases require additional owned fixtures or an auth profile, individually documented before execution. This initial trial does not complete all WP17 gates.

Prove zero dispatch using per-function invocation observations in a quiet isolated window, including warm successful calls as positive controls. Aggregate CloudWatch counts alone cannot prove per-request absence with concurrent traffic. Abort that assertion if unrelated clients invoke these functions. Do not add unsafe logging or broaden IAM to obtain evidence.

## Manual cleanup

If a real backend persisted a record, delete only the exact successfully created trial ID after verifying the trial tag/name. Never bulk-delete or delete fixed dummy response IDs. DNS alias must be removed explicitly if created outside the stack.

```bash
rtk aws elbv2 deregister-targets --target-group-arn "$TARGET_GROUP_ARN" --targets Id="$ALIAS_ARN"
rtk aws cloudformation delete-stack --stack-name "$STACK"
rtk aws cloudformation wait stack-delete-complete --stack-name "$STACK"
rtk aws s3 rm "s3://$BUCKET/$PREFIX/d095e89c/bootstrap.zip"
rtk aws s3 rm "s3://$BUCKET/$PREFIX/74c9f8f0/layer.zip"
```

Do not delete the bucket, certificate, Java backends or unrelated DNS. Verify stack resources are deleted, including log group, layer version and gateway versions. Inspect DELETE_FAILED and retained resources; CloudFormation deletion is not evidence of backend-record cleanup. Store sanitized evidence outside resources scheduled for deletion.

## Cost estimate and remaining gates

Planning estimate, not a live AWS quote: ALB about $0.0225/hour plus ~$0.008/LCU-hour; two public IPv4 addresses about $0.010/hour total. Low-traffic baseline roughly $0.033/hour ($0.78/day, $24/730-hour month), before LCUs. One sustained LCU adds ~$5.84/month. Lambda x86 compute about $0.000016667/GB-second plus $0.20/million requests; 1000 gateway calls at 512 MB and 200 ms cost about $0.0017 compute, excluding Java compute. Add logs, S3, DNS, transfer and applicable certificate charges. No NAT or provisioned concurrency. Verify current us-east-2 prices before approving. Existing ALB reuse could avoid new fixed ALB/IP costs but introduces shared-topology scope.

No automatic TTL or budget alarm. ALB/public-IP and retained storage/log charges continue until manual deletion; reserved concurrency is not a cost cap. Operator must schedule and verify cleanup.

Remaining deployment inputs: topology selection, client /32, certificate/DNS, artifact bucket/profile and explicit unqualified-ARN acceptance or supplied qualifiers. Local config-specific RIE and IaC lint review remain pending. Validator/rewriting worker execution remains blocked, failed bundle verifier remains visible, and the original qualified ZIP is unchanged. No AWS calls were made. No full WP12/WP13, WP17 or AWS qualification claim.
