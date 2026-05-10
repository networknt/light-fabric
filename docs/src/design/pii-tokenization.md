# PII Tokenization

## Status

Proposed design for migrating the `light-tokenization` capability into
`light-fabric` as `light-pingora` handlers used by `light-gateway`.

## Purpose

PII tokenization protects sensitive employee/customer data when a request is
sent from inside the organization to an external cloud service through the
gateway. The outbound request replaces configured PII fields with generated
tokens. When the cloud response returns, the gateway replaces those tokens with
the original cleartext values so internal employees can complete their work.

This is a request/response hot-path concern. The first Rust implementation
should therefore run inside `light-gateway` and access PostgreSQL directly
instead of making a network call to a tokenization service for every field.

## Current Java Behavior

The current `light-tokenization` service exposes REST endpoints:

- `POST /v1/token`: body `{ "schemeId": <int>, "value": "<cleartext>" }`;
  returns a token string. If the value already exists, it returns the existing
  token.
- `GET /v1/token/{token}`: returns the cleartext value.
- `DELETE /v1/token/{token}`: deletes the token mapping.
- `GET /v1/scheme` and `GET /v1/scheme/{id}`: return token format schemes.

Startup loads multiple JDBC pools from `datasource.yml`. One database is named
`tokenization`; the others are vault databases such as `vault000`. The
`tokenization` database maps `client_id` to a vault database through
`client_database`. Each vault database has a `token_vault` table.

Java tokenization flow:

1. Read `client_id` from the JWT audit info.
2. Resolve `client_id -> db_name`.
3. Select a vault datasource by `db_name`.
4. For tokenization, look up by cleartext `value`; return existing `id` if
   found.
5. If not found, generate a token with the configured `schemeId`, insert
   `(id, value)`, cache `token -> value`, and return the token.
6. For detokenization, check the cache first, then query by token `id`.

The current Java MCP router also uses tokenization through `token-client`.
Tool input schemas can mark fields with `x-tokenize`; the router extracts
JsonPath rules from the schema and calls the tokenization service.

## Design Direction

Use direct PostgreSQL access for the initial `light-fabric` implementation.

Reasons:

- It removes one HTTP hop per tokenized field in the gateway hot path.
- It avoids running and scaling another service only to perform local database
  lookups.
- PostgreSQL connection pooling is already used in nearby light-fabric apps
  with `sqlx`.
- The same database will also support other gateway handlers that need local
  data access, such as vector search for MCP routing.
- Multi-tenancy is cleaner with `host_id` in the schema than with one vault
  database per tenant.

If this capability is later exposed as a standalone service, prefer gRPC over
MCP for the hot-path service API. gRPC gives a strongly typed protobuf
contract, HTTP/2 multiplexing, compact binary payloads, deadlines, and
well-understood client pooling. MCP is useful when tokenization is exposed as
an agent tool or administrative capability, but it adds JSON-RPC/tooling
semantics that are not needed for a low-latency service-to-service data-plane
call.

## Goals

- Implement `TokenizeHandler` and `DetokenizeHandler` in `light-pingora`.
- Activate handlers only through `handler.yml`.
- Use one PostgreSQL database with `host_id` tenant isolation.
- Integrate schema into `portal-db/postgres/ddl.sql` and future patch files.
- Preserve the Java token schemes and stable tokenization behavior.
- Avoid storing/indexing cleartext PII directly in PostgreSQL.
- Support request-body tokenization before proxy/router sends to the external
  service.
- Support response-body detokenization before the gateway returns to the
  internal caller.
- Reuse the same runtime for MCP tool argument tokenization.

## Non-Goals

- Do not preserve multiple vault databases.
- Do not preserve MySQL or SQLite runtime support in light-fabric.
- Do not make tokenization an MCP-only service.
- Do not require a separate tokenization service for the first implementation.
- Do not try to tokenize arbitrary binary payloads in the first pass.

## Handler Model

Use two public handler ids:

- `tokenize`: request-phase handler that replaces cleartext fields with tokens.
- `detokenize`: response-phase handler that replaces configured token fields
  with cleartext.

Both handlers share one runtime:

```text
frameworks/light-pingora/src/pii_tokenization.rs
```

Primary types:

```rust
pub struct PiiTokenizationConfig {
    pub database: PiiDatabaseConfig,
    pub host_id_claim: String,
    pub max_body_size: usize,
    pub cache: PiiTokenCacheConfig,
    pub crypto: PiiTokenCryptoConfig,
    pub rules: Vec<PiiTokenizationRule>,
}

pub struct PiiTokenizationRuntime {
    pub config: Arc<PiiTokenizationConfig>,
    pub pool: PgPool,
    pub tokenizers: TokenizerRegistry,
    pub value_cache: TokenCache,
    pub token_cache: TokenCache,
    pub keyring: PiiKeyring,
}

pub struct PiiTokenizationRule {
    pub path_prefix: String,
    pub methods: Vec<String>,
    pub request: Vec<PiiFieldRule>,
    pub response: Vec<PiiFieldRule>,
}

pub struct PiiFieldRule {
    pub path: String,
    pub scheme: String,
    pub required: bool,
}
```

The handler should fail startup if an active config references an unknown
scheme, has invalid field paths, cannot initialize the keyring, or cannot
connect to PostgreSQL within the configured startup timeout.

## Resolved Decisions

- Handler ids are `tokenize` and `detokenize` to align with other
  light-fabric handler names.
- Encrypt stored cleartext with AES-256-GCM. Resolve key material from
  environment variables first, with direct config values allowed only as a
  local-development fallback.
- Detokenization fails closed by default when a configured token field cannot
  be resolved.
- Field selection uses a constrained compiled JsonPath subset rather than full
  dynamic JsonPath evaluation.
- Cleartext reverse caching is configurable through `cache.cacheCleartext`.
- Request/response mutation buffers are bounded by configurable `maxBodySize`.

## Handler Chain

For a BFF or gateway that calls an external cloud service:

```yaml
handlers:
  - correlation
  - security
  - tokenize
  - router
  - detokenize

chains:
  external-cloud:
    - correlation
    - security
    - tokenize
    - router
    - detokenize

paths:
  - path: /claims
    method: POST
    exec:
      - external-cloud
```

`tokenize` must run after authentication so it can resolve `host_id` from the
verified JWT principal. It must run before `router` or `proxy` so the external
service never receives cleartext PII. `detokenize` must run after the upstream
response body is available and before response delivery.

This likely requires extending the existing gateway handler model with a
response-body filter phase:

```rust
pub trait PingoraBodyHandler {
    async fn request_body_filter(&self, ctx: &mut GatewayRequestContext, body: Bytes)
        -> Result<Bytes, HandlerRejection>;

    async fn response_body_filter(&self, ctx: &mut GatewayRequestContext, body: Bytes)
        -> Result<Bytes, HandlerRejection>;
}
```

The first implementation can wire this directly in `light-gateway`; later it
can be generalized for other body-mutating handlers.

## Configuration

Primary file: `pii-tokenization.yml`.

`enabled` is not needed. If neither `tokenize` nor `detokenize` appears in
`handler.yml`, this config is not loaded. If either handler is active, the
config is required and invalid config fails startup.

Example:

```yaml
database:
  url: ${pii-tokenization.database.url:${database.url:}}
  maxConnections: ${pii-tokenization.database.maxConnections:8}
  minConnections: ${pii-tokenization.database.minConnections:1}
  connectTimeoutMs: ${pii-tokenization.database.connectTimeoutMs:2000}

hostIdClaim: ${pii-tokenization.hostIdClaim:host_id}
maxBodySize: ${pii-tokenization.maxBodySize:1048576}

crypto:
  algorithm: ${pii-tokenization.crypto.algorithm:AES-256-GCM}
  keyId: ${pii-tokenization.crypto.keyId:default}
  valueEncryptionKeyEnv: ${pii-tokenization.crypto.valueEncryptionKeyEnv:PII_TOKENIZATION_VALUE_ENCRYPTION_KEY}
  valueHashKeyEnv: ${pii-tokenization.crypto.valueHashKeyEnv:PII_TOKENIZATION_VALUE_HASH_KEY}
  valueEncryptionKey: ${pii-tokenization.crypto.valueEncryptionKey:}
  valueHashKey: ${pii-tokenization.crypto.valueHashKey:}

cache:
  enabled: ${pii-tokenization.cache.enabled:true}
  maxEntries: ${pii-tokenization.cache.maxEntries:10000}
  ttlSeconds: ${pii-tokenization.cache.ttlSeconds:86400}
  cacheCleartext: ${pii-tokenization.cache.cacheCleartext:true}

rules:
  - pathPrefix: /claims
    methods: [POST]
    request:
      - path: $.claimant.ssn
        scheme: LN
        required: false
      - path: $.payment.cardNumber
        scheme: CC4
        required: false
    response:
      - path: $.claimant.ssn
        scheme: LN
        required: false
      - path: $.payment.cardNumber
        scheme: CC4
        required: false
```

Field paths should support the Java-compatible JsonPath subset used by
`mcp-router` tokenization rules: object fields and `[*]` arrays. For
performance and predictable mutation, the Rust implementation should compile
rules at startup and avoid dynamic path parsing on every request.

For MCP tools, keep supporting `x-tokenize` in input schemas. The MCP router
can convert schema annotations into the same compiled field rules and call the
shared `PiiTokenizationRuntime` directly.

## PostgreSQL Schema

Replace the old split between `tokenization` and vault databases with
tenant-scoped tables in portal-db.

Recommended DDL:

```sql
CREATE TABLE pii_token_scheme_t (
    scheme_id        SMALLINT PRIMARY KEY,
    scheme_code      VARCHAR(16) NOT NULL UNIQUE,
    description      TEXT NOT NULL,
    active           BOOLEAN DEFAULT TRUE NOT NULL,
    update_ts        TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,
    update_user      VARCHAR(126) DEFAULT SESSION_USER NOT NULL
);

CREATE TABLE pii_token_vault_t (
    host_id           UUID NOT NULL,
    token             TEXT NOT NULL,
    scheme_id         SMALLINT NOT NULL,
    value_hash        BYTEA NOT NULL,
    value_ciphertext  BYTEA NOT NULL,
    value_nonce       BYTEA NOT NULL,
    key_id            VARCHAR(128) NOT NULL,
    created_ts        TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,
    expires_ts        TIMESTAMP WITH TIME ZONE,
    active            BOOLEAN DEFAULT TRUE NOT NULL,
    update_ts         TIMESTAMP WITH TIME ZONE DEFAULT CURRENT_TIMESTAMP NOT NULL,
    update_user       VARCHAR(126) DEFAULT SESSION_USER NOT NULL,
    PRIMARY KEY(host_id, token),
    FOREIGN KEY(scheme_id) REFERENCES pii_token_scheme_t(scheme_id)
);

CREATE UNIQUE INDEX pii_token_vault_value_uk
ON pii_token_vault_t(host_id, scheme_id, value_hash)
WHERE active = TRUE;

CREATE INDEX pii_token_vault_expiry_idx
ON pii_token_vault_t(expires_ts)
WHERE expires_ts IS NOT NULL;
```

Seed schemes:

| Id | Code | Meaning |
| --- | --- | --- |
| `0` | `UUID` | UUID v4 token |
| `1` | `GUID` | URL-safe base64 UUID token |
| `2` | `LN` | Luhn compliant numeric token |
| `3` | `N` | Random numeric token, length preserving |
| `4` | `LN4` | Luhn numeric token retaining last four digits |
| `5` | `AN` | Random alpha-numeric token, length preserving |
| `6` | `AN4` | Alpha-numeric token retaining last four characters |
| `7` | `CC` | Credit-card-shaped Luhn token retaining first digit |
| `8` | `CC4` | Credit-card-shaped Luhn token retaining first and last four digits |

The old `database_owner` and `client_database` tables are not needed. Tenant
isolation is by `host_id`, resolved from the authenticated request. If a
legacy client only has `client_id`, handle that with normal portal auth/client
metadata rather than recreating tokenization-specific vault routing.

### Cleartext Storage

The Java schema stores cleartext PII in `token_vault.value` and indexes it.
The Rust schema should not.

Use:

- `value_hash`: deterministic HMAC-SHA-256 of
  `(host_id, scheme_id, canonical_value)` with `valueHashKey`; used for
  idempotent token lookup.
- `value_ciphertext` and `value_nonce`: encrypted cleartext value, for example
  AES-GCM or ChaCha20-Poly1305 with `valueEncryptionKey`.
- `key_id`: identifies which key encrypted the row so key rotation is possible.

This keeps tokenization idempotent without indexing cleartext PII.

## Tokenization Algorithm

Shared runtime operation:

```text
tokenize(host_id, scheme_id, value)
  -> canonicalize value
  -> compute value_hash
  -> cache lookup by (host_id, scheme_id, value_hash)
  -> SELECT token WHERE host_id, scheme_id, value_hash, active
  -> if found, cache and return
  -> generate scheme-specific token
  -> encrypt cleartext
  -> INSERT row
  -> on token collision, retry generation
  -> on value_hash conflict, SELECT existing token and return it
```

Use PostgreSQL uniqueness instead of application locks:

```sql
INSERT INTO pii_token_vault_t (...)
VALUES (...)
ON CONFLICT DO NOTHING;
```

If no row is inserted, determine whether the conflict was on
`(host_id, token)` or `(host_id, scheme_id, value_hash)`. Token collision means
retry with a new token. Value conflict means another request already inserted
the mapping; select and return the existing token.

Detokenization:

```text
detokenize(host_id, token)
  -> cache lookup by (host_id, token)
  -> SELECT encrypted value WHERE host_id, token, active
  -> decrypt cleartext
  -> cache and return
```

If token is not found, the handler fails the response with a handler error.
For gateway response-body detokenization, fail closed so employees do not see
partial or incorrect data without a signal.

## Runtime Caching

Use bounded in-process caches:

- `(host_id, scheme_id, value_hash) -> token`
- `(host_id, token) -> cleartext`

The cache must be tenant-scoped and bounded by count and TTL. Because the
reverse cache contains cleartext PII, make it configurable and register it
with the runtime cache registry only with masked summaries. A clear-cache
operation should be available through the runtime control plane.

The cache is an optimization only. PostgreSQL remains the source of truth.

## Request And Response Mutation

Only mutate supported structured content:

- `application/json` in phase 1.
- JSON arrays and nested objects through compiled path rules.
- Missing optional fields are ignored.
- Missing required fields reject the request or response with a handler error.

For outbound request tokenization:

1. Buffer the JSON request body within a configured max body size.
2. Parse to `serde_json::Value`.
3. Apply matching request rules.
4. Replace every string value with a token.
5. Serialize JSON, update `Content-Length`, and forward upstream.

For inbound response detokenization:

1. Buffer the JSON response body within a configured max body size.
2. Parse to `serde_json::Value`.
3. Apply matching response rules.
4. Replace every string token with cleartext.
5. Serialize JSON, update `Content-Length`, and return downstream.

For very large or streaming payloads, skip mutation and fail closed by default.
Streaming tokenization can be considered later only if a real product requires
it.

## Security

- Require a verified JWT principal before tokenization.
- Resolve `host_id` from a configured claim, default `host_id`.
- Reject active tokenization if `host_id` is missing.
- Do not log cleartext values, generated tokens, value hashes, ciphertext, or
  keys.
- Mask crypto keys in module registry summaries.
- Use least-privilege PostgreSQL credentials: only select/insert/update on the
  tokenization tables.
- Prefer encrypted cleartext storage, not plaintext `value`.
- Keep tokens scoped by `host_id`; the same token string in another tenant does
  not detokenize.

## Future Service API

The direct database implementation should be the first production path.
However, keep the core API independent from Pingora:

```rust
#[async_trait]
pub trait PiiTokenVault: Send + Sync {
    async fn tokenize(&self, host_id: Uuid, scheme_id: i16, value: &str)
        -> Result<String, PiiTokenError>;

    async fn detokenize(&self, host_id: Uuid, token: &str)
        -> Result<String, PiiTokenError>;
}
```

Then a future service can wrap the same trait.

Protocol recommendation:

- **gRPC** for request-path service-to-service tokenization if a standalone
  service becomes necessary.
- **MCP** only as an optional tool surface for agents or administrative
  workflows.
- **REST/JSON-RPC** only for compatibility or operational simplicity, not the
  preferred low-latency path.

The gRPC API can be very small:

```protobuf
service PiiTokenization {
  rpc Tokenize(TokenizeRequest) returns (TokenizeResponse);
  rpc Detokenize(DetokenizeRequest) returns (DetokenizeResponse);
  rpc BatchTokenize(BatchTokenizeRequest) returns (BatchTokenizeResponse);
  rpc BatchDetokenize(BatchDetokenizeRequest) returns (BatchDetokenizeResponse);
}
```

Batch operations are important if a future remote service is used; otherwise
per-field network calls will dominate latency.

## Implementation Phases

1. Add portal-db DDL and seed data for `pii_token_scheme_t` and
   `pii_token_vault_t`.
2. Add a `light-pingora` shared tokenization runtime with `sqlx::PgPool`,
   scheme registry, value hashing, encryption, token generation, and tests.
3. Add `pii-tokenization.yml` loader, module registry registration, and runtime
   reload.
4. Add gateway request-body and response-body filter support.
5. Implement `tokenize` and `detokenize` handler wiring in `light-gateway`.
6. Integrate MCP `x-tokenize` with the same runtime so MCP tools do not call a
   hardcoded tokenization service.
7. Add optional gRPC service wrapper only if deployment needs a separate
   tokenization service.

## Remaining Considerations

- KMS or light-portal managed keys can be added later, but the first
  implementation should read the configured environment variables before any
  resolved config fallback.
- Products that disable `cache.cacheCleartext` will still use PostgreSQL as the
  source of truth, with higher detokenization latency.
