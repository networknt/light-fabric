# Deploy Light Lambda through the AWS console

This guide deploys a **separate Rust gateway Lambda** that invokes the existing
Java pet-store business functions. Use the Lambda console's **Test** button to
invoke it directly. No ALB, API Gateway, Function URL, VPC, subnet, security group,
public endpoint, client CIDR, certificate, or DNS setup is required.

The console workflow needs an authorized AWS console session, not an artifact
bucket or AWS CLI profile. The optional CloudFormation workflow uses S3; CLI
commands need their own authenticated CLI session.

## Deployment status and scope

The trial in account `964637446810`, region **US East (Ohio), `us-east-2`**, was
created on October 22, 2026:

| Setting | Trial value |
| --- | --- |
| Function | `light-lambda-rust-direct-trial-gateway` |
| Runtime | Amazon Linux 2023, `provided.al2023` |
| Architecture | `x86_64` |
| Handler setting | `bootstrap` |
| Memory / timeout | 512 MB / 20 seconds |
| Reserved / provisioned concurrency | 2 / none |
| Configuration layer | `arn:aws:lambda:us-east-2:964637446810:layer:light-lambda-config:1` |
| Execution role | `service-role/light-lambda-rust-direct-trial-gateway-role-ppenixvm` |
| Runtime policy | `PetstoreTrialInvokeAndScopedLogs` |

The executable upload and AWS checksum were verified. **No AWS invocation was
run at that checkpoint.** Local application tests do not establish AWS startup,
JWKS connectivity, backend execution, or IAM qualification. The memory, timeout,
and concurrency settings are trial choices, not production sizing evidence.

Validator, Router rewriting, and paused precision/worker experiments remain
excluded. Traceability's instrumented-only proof is separate. This deployment
does not establish full native parity, full WP12/WP13/WP17 completion, or ALB
qualification. See the [product design](design.md) for the broader scope.

## 1. Identify the two ZIP files

The **executable ZIP** and **configuration-layer ZIP** go to different places.
Do not upload the executable as a layer or the configuration as function code.

| Artifact | Console destination | ZIP layout |
| --- | --- | --- |
| `bootstrap.zip` | Function → Code → Update → Update from a .zip file | Executable `bootstrap` at ZIP root, mode `0755` |
| `aws-config-layer.zip` | Lambda → Layers → Create layer or Create version | Configuration files and `light-lambda-layer.json` at ZIP root; Lambda mounts them at `/opt` |

The pinned executable for this trial is:

```text
/home/steve/workspace/artifacts/implementation/light-lambda/WP13-D08e-release-20261021T155728Z/release-package/bootstrap.zip

SHA-256 (hex):
d095e89c07f6079823d6d07395b8716f280add5e7e09f7a75b1d03e121af756a

AWS CodeSha256 (base64):
0JXonAf2B5gj1tBzlbhxbygK3V5+CfenWx0D4SGvdWo=
```

The JWT-enabled layer used in this trial is:

```text
/home/steve/workspace/artifacts/implementation/light-lambda/WP17-jwt-config-20261021/rie-v2/aws-config-layer.zip

SHA-256 (hex):
6980390808bfa98b0eccd8bcef16e2bd465cb37a5196bc921bafb12e5a1c4984

Configuration digest:
dae9e51b7a727f46e81d8f34208604e561a2745556c8b4c12a44c2bebdc2a726
```

These are workspace-local artifact paths. The repository's ordinary release
script is not the source of this Lambda ZIP. Keep the pinned package unchanged
when editing configuration; rebuild only the layer. Preserve older ZIPs,
digests, and evidence rather than replacing them in place.

## 2. Configure and build the layer

The trial configuration source is
`apps/light-lambda/deploy/l3/config/`. Edit `values.yml` to supply values consumed
by the application's embedded configuration templates. JSON syntax in that file
is valid YAML. A Portal instance or configuration snapshot export is unnecessary.

The existing `client.yml` template reads the JWKS endpoint from these values:

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

`light-security::SecurityRuntime` uses `light_client::ClientFactory` to retrieve
and cache public signing keys. With startup key-service bootstrap disabled, the
first request needing keys fetches them; warm requests reuse resident keys under
the existing refresh policy. TLS validation stays enabled. Do not place access
tokens, passwords, private signing keys, or static AWS credentials in the layer.

The permanent pet-store OpenAPI scopes are:

| Operation | Listed scopes |
| --- | --- |
| GET list / GET pet | `portal.r` |
| POST pet | `portal.r`, `portal.w` |
| DELETE pet | `portal.w` |

**Existing native Java and Rust matching accepts any listed scope.** POST
therefore currently accepts either `portal.r` or `portal.w`; it does not require
both. This documents existing behavior, not a claim of standard OpenAPI
all-scope semantics. Requiring write-only POST or all listed scopes is a separate
authorization-policy change.

From the repository root, build a fresh layer using the pinned executable's
offline configuration validation:

```bash
TRIAL_OUT=/home/steve/workspace/artifacts/implementation/light-lambda/layer-$(rtk proxy date -u +%Y%m%dT%H%M%SZ)
rtk proxy python3 -B apps/light-lambda/scripts/package_baseline.py layer \
  apps/light-lambda/deploy/l3/config \
  "$TRIAL_OUT/aws-config-layer.zip" \
  --bootstrap /home/steve/workspace/artifacts/implementation/light-lambda/WP13-D08e-release-20261021T155728Z/release-package/bootstrap
```

The command prints the ZIP SHA-256, configuration digest, and registered
handlers. Choose a fresh output path; it refuses to overwrite an existing ZIP.
Offline validation does not contact the development JWKS server or AWS.

For the configuration-specific local application test, reuse the existing RIE
harness and owned fake dependencies:

```bash
rtk proxy python3 -B apps/light-lambda/deploy/l3/qualify_jwt.py \
  --out /home/steve/workspace/artifacts/implementation/light-lambda/jwt-rerun-$(rtk proxy date -u +%Y%m%dT%H%M%SZ)
```

The recorded run passed 11 service requests, with seven fake Invokes, one JWKS
fetch, and no HTTP/OAuth calls. It used synthetic HS256 credentials; it does not
prove live development JWKS TLS fetching or real RS256 verification through the
Rust application in AWS.

## 3. Upload a configuration-layer version

1. Confirm account `964637446810` and region `us-east-2` in the console.
2. Open **Lambda → Layers**. For a new dedicated layer choose **Create layer**;
   for an existing trial layer choose **Create version**.
3. Select **Create from a .zip file**, then **Choose file** and select the
   configuration-layer ZIP. Confirm its filename appears before submitting.
4. Set compatible architecture `x86_64` and runtime Amazon Linux 2023 when
   creating a new version. These fields are metadata, not execution tests.
5. Submit and record the complete **version ARN**. Layer versions are immutable.
   Publishing a version does not automatically update a function using an older
   version.

The owner-created `light-lambda-config:1` omitted compatibility metadata. Its
downloaded bytes were verified against the JWT-layer digest above before it was
attached by exact ARN. Missing metadata alone is not proof of incompatibility.

## 4. Create the Rust event function

On the Lambda dashboard, **Create Event Function** is the correct entry point
for this direct-invocation workflow. Rust is under an OS-only runtime, not a
separate managed Rust entry.

1. Choose **Author from scratch** and an unused name, such as
   `light-lambda-rust-direct-trial-gateway`. Do not overwrite a Java gateway or a
   business function.
2. In **Runtime**, choose **Amazon Linux 2023**. The menu describes it as
   “OS-only runtime for Go, Rust, C++, custom”; its API name is `provided.al2023`.
3. Expand **Additional settings**. Leave **ARM64 architecture** off to retain
   `x86_64`, matching the executable ZIP. Leave **Function URL**, **VPC**,
   **Durable execution**, and **EC2 capacity provider** off.
4. Use a dedicated execution role. The console can create a new basic logging
   role; it cannot invoke business functions until the scoped policy below is
   applied. Do not reuse a shared administrator or Java gateway role.
5. Create the function. AWS initially installs custom-runtime sample files.
   Creation success is not proof that the Rust executable is installed.

### Execution-role permissions

The role trusts the AWS Lambda service, `lambda.amazonaws.com`. Its final
permissions should contain only the following policy for this trial:

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": "lambda:InvokeFunction",
      "Resource": [
        "arn:aws:lambda:us-east-2:964637446810:function:PetsGetFunction",
        "arn:aws:lambda:us-east-2:964637446810:function:PetsPostFunction",
        "arn:aws:lambda:us-east-2:964637446810:function:PetsPetIdGetFunction",
        "arn:aws:lambda:us-east-2:964637446810:function:PetsPetIdDeleteFunction"
      ]
    },
    {
      "Effect": "Allow",
      "Action": "logs:CreateLogGroup",
      "Resource": [
        "arn:aws:logs:us-east-2:964637446810:log-group:/aws/lambda/light-lambda-rust-direct-trial-gateway",
        "arn:aws:logs:us-east-2:964637446810:log-group:/aws/lambda/light-lambda-rust-direct-trial-gateway:*"
      ]
    },
    {
      "Effect": "Allow",
      "Action": ["logs:CreateLogStream", "logs:PutLogEvents"],
      "Resource": "arn:aws:logs:us-east-2:964637446810:log-group:/aws/lambda/light-lambda-rust-direct-trial-gateway:*"
    }
  ]
}
```

In the new role's IAM page, use **Add permissions → Create inline policy →
JSON**, review the exact resources, and save it as
`PetstoreTrialInvokeAndScopedLogs`. After the replacement is attached, detach
the new role's default logging policy. Do not detach policies from other roles.
Record any newly generated customer-managed policy left unattached for later
trial cleanup. Browser-assisted IAM grants can require action-time confirmation.

The four unqualified backend ARNs are explicitly accepted for this trial and
invoke **$LATEST**. Do not invent qualifiers or modify business functions.
The human console operator separately needs permission to invoke the new
gateway; the gateway's execution role does not grant that to the operator.

## 5. Upload the executable ZIP

1. Open the **new Rust function → Code**.
2. Choose **Update → Update from a .zip file**. Older console versions call this
   **Upload from → .zip file**.
3. Choose the pinned **`bootstrap.zip`**, not the configuration layer.
4. Confirm the selected filename and size, about 10.5 MB, appear in the dialog.
   S3 is suggested for files over 10 MB but is not required for this ZIP.
5. Click the dialog's **Update** button and wait for AWS's successful-update
   notification. If the dialog changes size after selecting a file, use its
   current button position rather than a stale automation target.
6. Under **Code properties**, verify **SHA256 hash** equals the base64
   `CodeSha256` above. A fresh page load can distinguish persisted AWS state
   from stale editor contents. The ZIP hex digest and AWS's base64 digest are
   different encodings of the same hash.
7. Verify the editor lists `bootstrap`, rather than only
   `bootstrap.sh.sample`, `hello.sh.sample`, and `README.md`. Do not edit or
   redeploy the compiled executable through the text editor.

If the old 2.1 kB sample package or its old checksum remains, the executable
upload is incomplete even if a dialog closed. Do not run a test or report a
successful Rust deployment until the persisted checksum matches.

### Browser-assistant upload troubleshooting

For the ChatGPT Chrome extension, open `chrome://extensions`, select **Details**
under the extension, and enable **Allow access to file URLs**. The extension can
reconnect after this change; resume the existing AWS function tab. See the
[extension upload instructions](https://developers.openai.com/codex/app/chrome-extension#upload-files).

In this trial, a semantic Playwright click on **Choose file** still timed out
after permission was enabled. The supported native browser click on the visible
button, with the file-chooser listener registered first, worked. Observe the
selected ZIP, then click the current **Update** button and verify the AWS hash.
The browser's raw CDP file-upload command was unsupported; it was not used to
bypass the chooser. Manual selection in the console is also an option.

If the browser disconnects while an upload is pending, inspect persisted code
properties after reconnecting before submitting again. Do not infer either
success or failure solely from the lost browser connection.

An optional CLI upload requires valid CLI credentials; console login alone does
not establish them. An earlier `InvalidClientTokenId` was a CLI credential
failure, not a Rust ZIP failure. Once authenticated in the correct account:

```bash
rtk proxy aws sts get-caller-identity --region us-east-2
rtk proxy aws lambda update-function-code \
  --region us-east-2 \
  --function-name light-lambda-rust-direct-trial-gateway \
  --zip-file fileb:///home/steve/workspace/artifacts/implementation/light-lambda/WP13-D08e-release-20261021T155728Z/release-package/bootstrap.zip
rtk proxy aws lambda wait function-updated-v2 \
  --region us-east-2 \
  --function-name light-lambda-rust-direct-trial-gateway
rtk proxy aws lambda get-function-configuration \
  --region us-east-2 \
  --function-name light-lambda-rust-direct-trial-gateway \
  --query '{CodeSha256:CodeSha256,Runtime:Runtime,Architectures:Architectures,Handler:Handler,LastUpdateStatus:LastUpdateStatus}'
```

These are operator commands for an authorized upload, not prerequisites for
using the console. No bucket is needed for `--zip-file`.

## 6. Attach the layer and finish runtime settings

Under **Code → Layers → Edit → Add a layer**, select the exact configuration
version. If compatibility metadata prevents discovery under Custom layers,
choose **Specify an ARN**, enter the verified version ARN, then **Verify → Add
→ Save**. Do not select an older auth-disabled layer by name alone.

Under **Runtime settings**, retain Amazon Linux 2023 and `x86_64`; set the
handler label to `bootstrap`. A custom runtime starts the executable at
`/var/task/bootstrap`, not a managed Rust handler method.

Under **Configuration**, set:

```text
LIGHT_LAMBDA_CONFIG_DIR=/opt
LIGHT_LAMBDA_GATEWAY_ARN=arn:aws:lambda:us-east-2:964637446810:function:light-lambda-rust-direct-trial-gateway
RUST_LOG=INFO
```

Use the actual function identity if the name changes. Do not set static AWS
credentials, fake SDK endpoints, or LoggerSet/precision-worker activation flags.
Set memory to 512 MB, timeout to 20 seconds, and reserved concurrency to 2. Leave
provisioned concurrency unset. Confirm **No VPC configuration** and **No Function
URL**. Set the trial log group's retention to seven days once created, or
precreate it with that retention. Retention was still pending at the recorded
upload checkpoint.

## 7. Prepare private Console Test events

The current Rust adapter accepts **ALB-shaped JSON**, including when sent
directly through Lambda Test. Direct Invoke passes the payload unchanged; it
does not translate an API Gateway event into an ALB event. This does not require
an actual ALB or establish API Gateway compatibility.

The owner's earlier API Gateway example had no `requestContext.elb`, used
`/path/to/resource`, and marked plain JSON as base64. A sanitized structural
copy was rejected locally with `400 ERR14010`, zero Invoke. Use the generated
route events in `apps/light-lambda/deploy/l3/events/` instead; they are synthetic
inputs, not captured AWS fixtures.

For example, create a **private** GET-list test event from this shape:

```json
{
  "requestContext": {
    "elb": {"targetGroupArn": "synthetic-direct-invoke-no-alb"}
  },
  "httpMethod": "GET",
  "path": "/v1/pets",
  "multiValueHeaders": {
    "authorization": ["Bearer REPLACE_WITH_FRESH_ACCESS_TOKEN"],
    "content-type": ["application/json"],
    "x-forwarded-for": ["198.51.100.7"],
    "x-correlation-id": ["rust-console-list"]
  },
  "multiValueQueryStringParameters": {"limit": ["1"]},
  "body": null,
  "isBase64Encoded": false
}
```

Replace the token placeholder immediately before running. Use a fresh access
token with the configured issuer/audience and matching portal scopes. Do not
copy real tokens into the repository, layer, shared test events, screenshots,
or evidence. In this multi-value event shape, put authorization in
`multiValueHeaders`, not only in `headers`.

The other generated events cover:

| Method/path | Event file | Backend |
| --- | --- | --- |
| GET `/v1/pets` | `list.json` | `PetsGetFunction` |
| POST `/v1/pets` | `post.json` | `PetsPostFunction` |
| GET `/v1/pets/{petId}` | `get.json` | `PetsPetIdGetFunction` |
| DELETE `/v1/pets/{petId}` | `delete.json` | `PetsPetIdDeleteFunction` |

The `/v1` prefix comes from the native configuration and OpenAPI server base.
The generated POST supplies integer `id`, string `name`, and optional `tag`;
DELETE uses the same synthetic ID and supplies its required string `key` header.
The synthetic key is not an access credential.

Before writes, confirm deployed backends still match the owner's dummy
implementations. Checked-in POST returns `200 {}` although OpenAPI documents
201; DELETE returns a fixed example pet and neither implementation persists
data. Do not mistake the dummy response's id 1 for trial-created data. If
persistence is introduced, establish absence and choose a fresh ID/name/tag;
delete only a record actually created and owned by the trial. Never bulk-delete.

When execution is authorized, exercise missing token, unrelated scope, all four
valid routes, malformed JSON with a valid token, and a subsequent warm valid
request. Missing-token and scope rejections should make zero backend calls.
Malformed JSON is a parsing check, not schema validation; Validator remains
unavailable. Do not alter dummy business functions to manufacture backend errors.

Inspect both the Lambda invocation result and returned application's
`statusCode`/`body`. Console execution success alone does not mean the application
returned 200. The repository's `apps/light-lambda/deploy/l3/check_response.py`
also checks CLI Invoke metadata, including `FunctionError`. Record sanitized
request IDs, log streams, responses, and backend invocation observations. Warm
reuse is observed, not guaranteed by successive button presses.

## 8. Updates, costs, and cleanup

For configuration-only changes: edit source → build and validate a fresh layer
ZIP → publish a new version → explicitly update the function's attached version
→ verify identity → run the affected authorized checks. The executable stays
pinned. Publishing a new layer alone does not change the running configuration.

There are no ALB/network fixed charges in this workflow. A planning estimate for
1,000 x86 gateway calls at 512 MB and 200 ms is about $0.0017 compute plus $0.0002
requests, excluding backend compute and credits. Add logs, custom metrics,
transfer/provider costs, and optional artifact storage. The metrics handler
emits three custom metrics per observed route/function dimension; those can
cost more than light Lambda usage. Verify current regional prices before a
longer trial. Reserved concurrency is not a cost cap. There is no automatic TTL
or budget alarm; retained storage and applicable metrics remain billable until
manual cleanup.

After preserving sanitized evidence, manually delete only trial-owned resources:

1. Any persisted record positively identified as created by the trial.
2. The separate Rust gateway and its dedicated log group.
3. Its inline policy and dedicated execution role.
4. The generated default logging policy left unattached during replacement,
   after checking it is unused.
5. Trial layer versions only after checking whether other functions use them.

Leave the Java gateway, four business functions, and shared IAM/layer resources
unchanged. Recheck deletion failures and retained resources rather than assuming
cleanup completed.

Repository details and the optional four-resource CloudFormation template are in
`apps/light-lambda/deploy/l3/`. Historical local and console evidence lives under
`/home/steve/workspace/artifacts/implementation/light-lambda/`, including
`WP17-jwt-config-20261021`, `WP17-console-create-20261022`, and
`WP17-console-upload-20261022`. Configuration and uploaded-code identity are
verified separately from future AWS execution results.
