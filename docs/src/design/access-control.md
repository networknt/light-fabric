# Access Control Handler Design

The access-control handler enforces fine-grained authorization for normal HTTP
API endpoints. It should reuse the same Light-Rule policy model that the MCP
router uses for tool authorization:

- `access-control.yml` controls whether policy is enabled, whether missing
  endpoint rules deny by default, how multiple request access rules combine,
  and which endpoint prefixes are skipped.
- `rule.yml` contains CEL rule bodies and endpoint mappings.
- `req-acc` rules run before the upstream API endpoint is called.
- `res-fil` rules run after the upstream API endpoint responds and before the
  response is returned to the caller.

The access-control handler and the MCP router should share the same
`frameworks/light-pingora/src/access_control.rs` runtime. The difference is the
boundary where that runtime is applied. The MCP router protects MCP tools. The
access-control handler protects API endpoints in the normal handler chain.

## Goals

- Enforce fine-grained access control for REST or HTTP API endpoints.
- Reuse the existing `req-acc` and `res-fil` rule phases.
- Reuse the built-in action classes:
  `RoleBasedAccessControlAction`, `ResponseRowFilterAction`, and
  `ResponseColumnFilterAction`.
- Keep rule definitions portable between gateway products when the endpoint key
  and context fields are equivalent.
- Support exact endpoint rules, Java-style path templates, and parent path
  entries.
- Keep the business API unaware of caller-specific row and column filtering.

## Non-Goals

- Do not create a second rule engine for HTTP APIs.
- Do not support the legacy native condition-row format in Light-Fabric.
  Light-Fabric rules must use `conditionLanguage: cel`.
- Do not replace base authentication. The access-control handler assumes an
  earlier security handler has already built the caller principal.
- Do not push row or column filtering into business handlers.

## HTTP routing and policy identity

Handler selection and HTTP ACL resolution are independent. A handler route such
as `/github/repos/*` selects a shared chain; it does not become an ACL grant.
The Gateway retains its routing endpoint for router rewrites, metrics, LLM
routing policy, and evidence. The access-control handler uses the original URI
path without the query and the original HTTP method, captured before header,
path, or upstream method rewriting. Verified caller claims remain the rule input.

Ordinary HTTP policies resolve in this order:

1. An exact path for that method wins, including over templates.
2. A unique matching full-path `{parameter}` template wins over literal prefixes.
   Multiple matching templates deny, even if one has longer parameter names or
   more literals. Resolve overlap with an exact policy or disjoint templates.
3. Existing literal parent-path policies retain longest-prefix semantics, with
   a `/` boundary and the same method. They explicitly authorize descendants;
   default-deny cannot make an endpoint unknown beneath such a configured grant.
4. With no match, retain the concrete `path@method` identity and apply configured
   `defaultDeny`. No wildcard handler fallback supplies permission.

Methods compare case-insensitively; policies differing only in method case are
ambiguous and deny. Ambiguity never falls back to a broader prefix or to
`defaultDeny: false`. An exact policy resolves otherwise overlapping templates.
Paths remain case-sensitive. Gateway preserves the received URI: routing uses
legacy raw matching and forwarding retains punctuation, escapes, repeated and
trailing slashes, apart from existing configured routing rewrites. There is no ingress rewrite or new path-signing requirement.
Ordinary HTTP ACL lookup uses a separate policy identity: decode one level,
validate UTF-8, retain URI segment characters, and use uppercase escapes for
space/UTF-8 data. Encoded unreserved aliases select their stricter exact policy.
A single trailing slash is equivalent for policy lookup only; the wire slash
remains, preserving directory redirects and signed paths.

On a chain containing access-control, ambiguous separators, dot segments,
malformed escapes, ASCII controls, invalid UTF-8, decoded percent signs and
repeated slashes reject. Literal or encoded semicolons also reject on these
chains: some upstream routers strip segment path parameters, which could let
`/api/private;x=1` miss an exact policy and select a broader prefix. This
restriction includes semicolons intended as literal segment data.
If raw routing would select an unprotected chain but
an alias would select an ACL chain, a guard rejects before dispatch. Its
conservative alias probe is never used for forwarding or handler selection;
it honors route order and recognizes separators, double encoding, segment
parameters and dot collapse. Parameter stripping is only a denial probe; raw
unprotected paths such as `/x/;jsessionid=...` remain unchanged unless their
alias crosses an ACL boundary. Protected route patterns and base paths are validated and compiled
once at load/reload; invalid candidates fail with diagnostics and retain the
active runtime. Unprotected patterns are not canonicalized or silently dropped.
An explicitly spelled unprotected alias route (for example `/open%2Fprivate`)
fails load/reload if its alias overlaps a protected route or default chain
without earlier complete unprotected coverage. Validation respects methods,
route order, base paths, segment templates and terminal wildcards; partial
unprotected coverage is conservatively insufficient. Use an unambiguous route
spelling or explicitly protect the route, retaining endpoint-specific ACL
policies. Do not broaden grants to silence this diagnostic. Rejected reloads
retain the active handler set and pinned requests. Dynamic aliases in ordinary
template/wildcard captures remain guarded at request time.
Normal route matching performs no per-pattern canonicalization.

Spaces and UTF-8 remain supported in ACL-protected segment data. Queries are
unchanged. Direct `/branches/feature%2Ftopic` remains restricted on an ACL chain,
because upstream separator/data interpretation is not uniform. An applicable
query `ref` is unchanged; it does not make the direct branch endpoint supported.
Unprotected `%2F`, `%25` and repeated slashes retain legacy behavior unless an
alias would cross into an ACL chain. HMAC sees the original request path.

The legacy `/@method` policy matches the root itself; it does not implicitly
grant all descendants. A non-root trailing slash selects its canonical policy.
`skipPathPrefixes` is checked against the canonical concrete identity and the
selected policy, and disabled/skipped ACLs retain their shared request/response
gates.

The selected endpoint key and immutable ACL runtime generation are retained for
request authorization and response filtering. Ordinary body-bearing proxy/router
requests are bounded and authorized before upstream dispatch, then replayed
through Pingora's existing prebuffered-body hook. Earlier tokenization is applied
once before authorization, as in the former body-filter path. Portal command/query
envelope decoding, MCP tool identities, A2A policy identities, and the MCP/LLM-owned
body authorization paths keep their protocol-specific behavior.

## Handler Placement

The access-control handler should run after authentication and before routing to
the upstream API service:

```text
request
  -> TLS / CORS / rate-limit / header handlers
  -> security or unified-security handler
  -> access-control req-acc
  -> proxy or route handler
  -> access-control res-fil
  -> response
```

If no authenticated principal is available, a `req-acc` rule can still evaluate
headers and endpoint metadata, but role, group, user, and claim-based rules will
normally fail closed.

## Shared Runtime

The existing runtime already models the common policy engine:

```rust
AccessControlRuntime
  -> authorize_tool(...)
  -> filter_mcp_response(...)
```

For API endpoints, these functions should be generalized rather than duplicated.
The MCP-specific names can remain as compatibility wrappers, but the shared
runtime should expose endpoint-neutral operations:

```rust
authorize_request(
  endpoint,
  headers,
  auth,
  request_context,
  correlation_id
)

filter_response(
  endpoint,
  headers,
  auth,
  request_context,
  response_status,
  response_body,
  correlation_id
)
```

The MCP router can keep passing `toolName` and `toolArguments`. The API handler
should pass API-oriented values such as path parameters, query parameters,
request method, and request body metadata.

## Endpoint Keys

Endpoint rule keys should use the same stable format as the MCP router:

```text
{path}@{method}
```

Examples:

```text
/offers@get
/v1/accounts/{accountId}@get
/v1/accounts@post
```

The query string must not be part of the endpoint key. Query parameters belong
in the rule context so CEL can inspect them without multiplying endpoint rule
entries.

Endpoint matching order should remain:

1. Exact endpoint key.
2. Java-style path template match, such as `/v1/accounts/{id}@get`.
3. Parent path entry, such as `/v1/accounts@get` for
   `/v1/accounts/123@get`.

## Configuration

`access-control.yml` is the handler-level switch:

```yaml
enabled: true
accessRuleLogic: any
defaultDeny: true
defaultInclude: false
skipPathPrefixes:
  - /health
  - /adm
claimMappings: {}
```

Fields:

- `enabled`: when false, the handler allows requests and does not filter
  responses.
- `accessRuleLogic`: `any` allows a request if any `req-acc` rule passes; `all`
  requires every listed `req-acc` rule to pass. This setting applies only to
  `req-acc`; it does not apply to `res-fil`.
- `defaultDeny`: when true, a request with no matching endpoint rule or no
  `req-acc` rule is denied.
- `defaultInclude`: controls response row-filter behavior when a row filter is
  configured but no caller claim matches any configured row-filter entry. When
  `false`, the row filter returns no rows. When `true`, the row filter preserves
  the legacy include-all behavior.
- `skipPathPrefixes`: endpoint prefixes that bypass access-control entirely.
- `claimMappings`: maps permission dimensions to JWT claim names for built-in
  request-access, row-filter, column-filter, and MCP tool-visibility behavior.
  Standard keys are `roles`, `groups`, `positions`, `attributes`, and `users`.
  Custom row or column dimensions use the dimension name as the mapping key.

For example, a deployment with custom role and tenant claims can use:

```yaml
claimMappings:
  roles:
    - custom_roles
  tenant:
    - tenant_id
```

Standard aliases remain active when a dimension has no configured mapping.
Existing `toolsListAccessControl.claimMappings` configuration remains supported
as a compatibility fallback, but top-level `claimMappings` takes precedence and
applies consistently to authorization and response filtering.

`rule.yml` contains the reusable rules and endpoint policy:

```yaml
ruleBodies:
  allowOfferRead:
    common: Y
    ruleId: allowOfferRead
    ruleName: Allow offer read
    ruleType: req-acc
    conditionLanguage: cel
    conditionSecurityProfile: strict
    expression: >
      auditInfo.subject_claims.ClaimsMap.role != null
    actions:
      - actionClassName: com.networknt.rule.RoleBasedAccessControlAction

  filterOfferRows:
    common: Y
    ruleId: filterOfferRows
    ruleName: Filter offer rows
    ruleType: res-fil
    conditionLanguage: cel
    conditionSecurityProfile: strict
    expression: >
      statusCode == 200
      && responseBody != ""
      && auditInfo.subject_claims.ClaimsMap.role != null
    actions:
      - actionClassName: com.networknt.rule.ResponseRowFilterAction

  filterOfferColumns:
    common: Y
    ruleId: filterOfferColumns
    ruleName: Filter offer columns
    ruleType: res-fil
    conditionLanguage: cel
    conditionSecurityProfile: strict
    expression: >
      statusCode == 200
      && responseBody != ""
      && auditInfo.subject_claims.ClaimsMap.role != null
    actions:
      - actionClassName: com.networknt.rule.ResponseColumnFilterAction

endpointRules:
  /offers@get:
    req-acc:
      - allowOfferRead
    res-fil:
      - filterOfferRows
      - filterOfferColumns
    permission:
      roles: offer-viewer offer-admin
      row:
        role:
          offer-viewer:
            - colName: priority
              operator: "<"
              colValue: 50
            - colName: active
              operator: "="
              colValue: true
      col:
        role:
          offer-viewer: offerId,title,segment,state,category,priority
```

In this example, `offer-viewer` can call `GET /offers` but only receives active
offers with priority below 50, and the `active` field is removed from the final
payload. With `defaultInclude: false`, `offer-admin` can call the same endpoint
only if a matching row-filter entry exists or the endpoint omits row filtering.
If an endpoint has a `row` block but no row entry matches the caller's role,
group, position, attribute, or user claim, the filtered result is empty. Set
`defaultInclude: true` only when a deployment intentionally wants the legacy
include-all behavior for unmatched row-filter claims.

## Rule Context

The access-control handler should build the same core context shape as the MCP
router so existing CEL rules and actions stay reusable:

| Field | Description |
| --- | --- |
| `auditInfo` | Normalized authenticated principal claims and correlation id. |
| `headers` | Lower-cased request headers. |
| `endpoint` | Stable endpoint key, such as `/offers@get`. |
| `permission` | The endpoint `permission` object. |
| `correlationId` | Correlation id when present. |
| `statusCode` | Response status code during `res-fil`. |
| `responseBody` | Response body string during `res-fil`. |

For API endpoints, add API-specific fields:

| Field | Description |
| --- | --- |
| `requestMethod` | HTTP method. |
| `requestPath` | Path without query string. |
| `queryParameters` | Parsed query parameter map. |
| `pathParameters` | Values captured from a path template when available. |
| `requestBody` | Parsed JSON request body when available and within size limits. |
| `requestBodyText` | Raw request body string when parsing is not enabled. |

The existing MCP fields can remain optional:

| Field | Usage |
| --- | --- |
| `toolName` | Present for MCP router calls, absent or empty for API endpoints. |
| `toolArguments` | Present for MCP router calls. API endpoints should prefer `queryParameters`, `pathParameters`, and `requestBody`. |

Permission values should continue to be injected twice:

- as the namespaced `permission` object
- as top-level convenience fields, such as `roles`, `row`, and `col`

This preserves compatibility with existing rule bodies and built-in action
classes. Runtime-owned context fields are the exception: permission keys named
`auditInfo`, `headers`, `endpoint`, `toolName`, `toolArguments`,
`correlationId`, `permission`, `responseBody`, `responseBodyJson`, `statusCode`,
or `accessControl` remain available under `permission` but are not promoted to
the top level. This prevents endpoint configuration from replacing verified
identity, request, or response context.

## Request Access

`req-acc` runs before the upstream API call.

The handler should:

1. Build the endpoint key from request path and method.
2. Skip the request if the endpoint matches `skipPathPrefixes`.
3. Find endpoint rules by exact, template, or parent match.
4. Deny when `defaultDeny: true` and no matching `req-acc` rule exists.
5. Build the rule context from auth, headers, endpoint, request fields, and
   endpoint permissions.
6. Execute the listed `req-acc` rules with `accessRuleLogic`.
7. Return `403` when access is denied.

When `accessRuleLogic: any`, each candidate rule should receive a cloned
context, and the first passing rule should win. When `accessRuleLogic: all`,
rules should run sequentially against the same context and all must pass.

## Response Filtering

`res-fil` runs after the upstream API response returns.

The handler should:

1. Only filter response payloads that are safe and useful to parse, starting
   with JSON arrays, JSON objects containing an `items` array, and single JSON
   objects for column filtering.
2. Buffer the full response body before filtering.
3. Decode or avoid upstream compression before JSON parsing.
4. Add `statusCode`, `responseBody`, and the parsed mutable JSON value to the
   same rule context shape.
5. Execute `res-fil` rules sequentially in the order listed on the endpoint.
6. Serialize the filtered JSON once after all `res-fil` actions complete.
7. Replace the response body with the final filtered JSON.
8. Recompute response headers that depend on body size, such as
   `content-length`.

Ordering matters. Row filters must run before column filters when the row
predicate depends on a field that should be hidden in the final response. For
example, a row filter can use `active == true`, and the later column filter can
remove `active` from the returned rows.

`res-fil` is always a sequential `all` pipeline. `accessRuleLogic: any` applies
only to `req-acc`; response filters never use `any` semantics.

Response filtering requires a full payload. It is not compatible with streaming
or indefinite responses unless the gateway buffers the entire response first.
For `Transfer-Encoding: chunked`, the gateway must buffer and then emit a normal
filtered response. Server-Sent Events and other long-lived streaming responses
should bypass `res-fil` or be rejected when an endpoint requires response
filtering.

Compressed upstream responses need explicit handling. The gateway should either
strip or normalize `Accept-Encoding` on the upstream request so the backend
returns plaintext JSON, or it must decompress before filtering and recompress
afterward. Filtering compressed `gzip`, `br`, or `deflate` bytes as JSON must
fail closed.

If a `res-fil` rule is missing, fails, or returns false, the handler should fail
closed for protected API endpoints. For early rollout, a deployment can choose a
fail-open compatibility mode only if it is explicit in configuration and emits a
high-severity log or module-registry status.

CEL must not directly rewrite the HTTP response body. A `res-fil` rule-level CEL
expression decides whether the filter action should run. The response-filter
pipeline owns JSON parsing, final serialization, response body replacement, and
header updates such as `content-length`. Actions own row or column mutation of
the parsed response value.

The default model should remain declarative:

- `ResponseRowFilterAction` applies permission-defined row filters.
- `ResponseColumnFilterAction` applies permission-defined field keep or remove
  lists.

### Row Filter Default Behavior

Row filtering must fail closed by default. If `ResponseRowFilterAction` runs for
an endpoint with a configured `permission.row` block, but the caller has no
matching entry under any supported dimension (`role`, `group`, `position`,
`attribute`, or `user`), the action must return an empty row set when
`defaultInclude: false`.

This prevents a common policy gap:

```yaml
permission:
  row:
    role:
      teller:
        - colName: accountType
          operator: "="
          colValue: C
```

In the legacy include-all behavior, a caller without the `teller` role would
match no row-filter entry and receive every row. With `defaultInclude: false`,
the same caller receives no rows. A caller with the `teller` role receives only
rows where `accountType == "C"`.

`defaultInclude` applies only to row-filter miss behavior:

- `false`: unmatched row-filter dimensions retain no rows. This is the secure
  default and should be used for new deployments.
- `true`: unmatched row-filter dimensions retain all rows. This is a
  compatibility mode for deployments that relied on the old behavior.

If a row-filter entry matches the caller, normal row predicate evaluation still
applies. If multiple dimensions match, the configured filter groups are combined
with the existing sequential `all` behavior so a row must satisfy every matched
group.

If an API needs a richer row predicate, add a CEL-aware action rather than
making rule-level CEL mutate JSON:

```yaml
ruleBodies:
  filterOfferRowsWithCel:
    ruleId: filterOfferRowsWithCel
    ruleType: res-fil
    conditionLanguage: cel
    conditionSecurityProfile: strict
    expression: >
      statusCode == 200 && responseBody != ""
    actions:
      - actionClassName: com.networknt.rule.ResponseCelRowFilterAction
        actionValues:
          rowExpression: >
            auditInfo.subject_claims.ClaimsMap.role == "offer-admin"
            || (row.priority < 50 && row.active == true)
```

`ResponseCelRowFilterAction` should compile `rowExpression` during rule load and
evaluate it once per row with a curated context containing `row`, `auditInfo`,
`headers`, `endpoint`, `permission`, and API request metadata. It owns row
retention and failure handling for the parsed response value. It should not
deep-clone the full base context for every row; use a child context that shadows
`row`, or reuse one mutable context and update only the `row` binding.

If a row-level CEL evaluation fails for one row, for example because `priority`
is missing, the action should drop that row and continue. Compile errors,
invalid `actionValues`, or other configuration errors should fail the whole
action closed.

## MCP Router Comparison

The MCP router and access-control handler should share configuration and runtime
semantics:

| Concern | MCP router | Access-control handler |
| --- | --- | --- |
| Protected target | MCP tools | HTTP API endpoints |
| Endpoint key | Tool endpoint or derived `{path}@{method}` | Request `{path}@{method}` |
| Request input | `toolArguments` | query parameters, path parameters, request body |
| Request access phase | `req-acc` before backend tool call | `req-acc` before upstream API call |
| Response filter phase | `res-fil` before JSON-RPC result | `res-fil` before HTTP response |
| Response body target | MCP `structuredContent` or text content | HTTP response body |
| Rule language | CEL only | CEL only |

This split keeps MCP behavior specialized for JSON-RPC tool calls while letting
API endpoint authorization use the same policy and action implementation.

## Reload And Observability

The loader should continue to support both standalone files and `values.yml`
projection:

- `access-control.yml` or `access-control.yaml`
- `rule.yml` or `rule.yaml`
- `access-control.*` values in `values.yml`
- `rule.ruleBodies` and `rule.endpointRules` values in `values.yml`

The root-level access-control setting below controls CEL context trace detail:

```yaml
logFullCelContext: false
```

At the default `false`, failed or rejected CEL expressions log referenced paths
and structural metadata at `TRACE`. When set to `true`, the same event includes
bounded values for only the referenced properties. Full mode is intended only
for local or development environments. Access-control context construction
excludes credential headers in both modes.

The module registry should report:

- whether access control is enabled
- whether rule config is loaded
- number of rule bodies
- number of endpoint mappings
- last reload status
- validation errors for rejected CEL, missing rule ids, or invalid endpoint keys

Config reload should build a new immutable runtime and swap it atomically after
validation succeeds. If validation fails, the handler should keep the last
known-good runtime.

## Implementation Notes

The current `AccessControlRuntime` is already close to the shared runtime. The
main implementation work is to remove MCP-specific naming from the reusable API
and add an HTTP response-body adapter:

- Keep `authorize_tool` and `filter_mcp_response` as wrappers for the MCP
  router.
- Add endpoint-neutral authorization and response-filter methods.
- Add API-specific request context fields without changing the existing
  `auditInfo`, `headers`, `endpoint`, `permission`, `responseBody`, and
  `statusCode` fields.
- Reuse `find_service_entry`, `rule_ids_for`, `permission_for`, and the default
  action registry.
- Reuse `ResponseRowFilterAction` for JSON arrays, object payloads with an
  `items` array, and single top-level JSON objects. A denied single object is
  replaced with an empty object for HTTP responses; the MCP adapter returns a
  tool error with `isError: true`.
- Reuse `ResponseColumnFilterAction` for JSON arrays, object payloads with an
  `items` array, and single top-level JSON objects.
- Add handler-level tests for exact endpoint, path template endpoint, parent
  path endpoint, default deny, skip prefixes, row filtering, and column
  filtering.


## Ordinary HTTP capture and chain compatibility

Wildcard handler routes (including `/github/repos/*` for every method) are
independent of ACL keys and remain supported. Wildcard ordinary HTTP ACL keys
containing `*` are unsupported: startup/reload rejects them with a migration
diagnostic under both default-deny modes. Migrate ACL rules to exact paths,
full-path templates, or deliberate literal-prefix policies; do not change
wildcard handler routing or broaden grants. A rejected reload retains the valid
active ACL runtime. Already admitted requests retain their selected key and
runtime for both authorization and response filtering across a valid reload.

`access-control.yml` has two explicit capture settings, independent of HMAC,
LLM, and tokenizer configuration:

```yaml
bodyReadTimeoutMillis: 10000
maxBufferedBodyBytes: 268435456
```

Both must be positive. Ordinary HTTP ACL retains its 10 MiB per-body limit.
Declared lengths reject before allocation/interim responses; streamed bytes
remain bounded. The deadline covers the entire body read, including writing
`100 Continue`. An aggregate permit accounts for retained payload bytes and is
released with request ownership on completion, denial, failure, timeout, or
cancellation. HMAC-captured originals keep their existing HMAC permit; shared
`Bytes` avoid a second original-body allocation. ACL-transform output reserves
its configured tokenizer maximum (capped by the ACL limit) before tokenization,
then reduces its charge to actual retained output. Budget exhaustion returns
503, size excess 413, and read timeout 408. These payload quotas are not a
process-RSS bound: protocol chunks, allocator slack, JSON parsing/serialization,
and token-vault operations have separate resource costs. The capture deadline
does not bound token-vault database operations or rule evaluation.

Supported body chains authenticate first, run `tokenize` before `access-control`,
and dispatch only after authorization. HMAC (standalone or through
`unified-security`/`unified`) authenticates original bytes before ACL capture;
ACL observes tokenized bytes and upstream receives the cached transformation
once. Downstream framing is preserved until capture; transformed upstream
framing is updated independently. Detokenization requests uncompressed upstream
responses even when ACL has already authorized the body. Empty-body DELETE
remains valid. HMAC checks signature/selector/delivery-header syntax before body
capture but validates the MAC only over original captured bytes.

Startup and handler/security reload reject tokenizer-after-body-capturing-ACL,
recognized-security-gate-after-ACL, or tokenizer-before-HMAC/unified orders with precise
order diagnostics. `jwt` is a security alias. Existing HMAC validation also
rejects duplicate entry points or simultaneous standalone HMAC and unified
security on the same effective chain. The recognized gates are `hmac`,
`security`, `jwt`, `unified-security` and `unified`; this is not a validator of
all authentication handlers (such as `stateless`, `token`, Basic or API-key).
GET/HEAD ACL evaluates query/header data and does not require tokenizer-before-ACL;
other gates and original-byte HMAC order still apply. Wildcard-method routes
remain unsupported by handler configuration loading; the body-order helper
treats a wildcard conservatively when called directly. HMAC terminal chains
remain router-only. The shared MCP/A2A matcher is unchanged.

Deferred tokenization uses chunked HTTP/1.1 upstream framing or HTTP/2 DATA
frames, selected from the actual negotiated protocol, not its configuration
preference. Known empty bodies retain length zero; preauthorized transformed
bodies retain their exact length. Deferred transformation over HTTP/1.0 is
unsupported and fails explicitly.

The 10-second capture deadline is total elapsed read time, unlike an idle timeout
that resets after every chunk. Slow uploads that previously progressed indefinitely
can now receive 408. The ACL body ceiling remains 10 MiB; the tokenizer default
is 1 MiB. Owners should explicitly select a total deadline for supported upload
sizes and minimum acceptable throughput before rollout, without changing unrelated
proxy idle settings. Reserving a full tokenizer output maximum under the default
256 MiB budget reduces concurrent upload capacity: each original plus its maximum
output is charged until transformation completes. This conservative reservation
also accounts for originals shared from HMAC through its independent budget;
the combined budgets are not one process-wide memory ceiling.

Local regression commands (no live requests):

```sh
rtk cargo test -p light-gateway http_acl -- --test-threads=2
rtk cargo test -p light-pingora --lib http_policy -- --test-threads=2
rtk cargo test -p light-pingora --lib capture_headers -- --test-threads=2
rtk cargo test -p light-gateway -- --test-threads=2
rtk cargo test -p light-pingora --lib -- --test-threads=2
```

The cache-only composition fixture is enabled by a development-only
`test-support` dependency. It uses seeded token-cache entries and a disconnected
pool, exercises production Gateway/tokenizer methods, and does not qualify a
PostgreSQL vault or any deployed runtime. Recorder finish requires stopped
producers, drains a nonblocking OS listener through `WouldBlock`, and joins every
accepted worker. A pending asynchronous accept is never treated as queue proof.
