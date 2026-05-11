# Client Configuration And Modules

## Status

Brainstorming proposal for standardizing `client.yml` across Light Fabric
runtime, framework modules, and products.

The immediate trigger is that different Rust modules currently interpret
`client.yml` differently. For example, `light-runtime` reads a small top-level
`verifyHostname` field for controller and config-server clients, while
`light-pingora` token and SPA modules read a Java-style nested `tls` section.
That split makes a single `client.verifyHostname: false` value unreliable.

This document proposes a common contract so every Rust module uses the same
`client.yml` file and the same typed configuration model.

## Purpose

`client.yml` should describe outbound client behavior for a running service:

- TLS trust, hostname verification, and optional client identity.
- HTTP request timeout, retry, circuit breaker, connection pool, and HTTP/2
  behavior.
- OAuth 2.0 token, key, sign, dereference, and provider-selection behavior.
- Path-prefix-to-service mapping used when different downstream services use
  different OAuth providers.

The file should be loaded once through the runtime configuration system,
registered once in the module registry with secrets masked, then shared by all
modules that make outbound calls.

## Compatibility Contract

The Java `light-4j` `client.yml` remains the compatibility baseline. Rust can
clean up the internal model, but it should not remove behavior that Java
`http-client` and `client-config` expose.

Important Java sections:

```yaml
tls:
  verifyHostname: ${client.verifyHostname:true}
  loadDefaultTrustStore: ${client.loadDefaultTrustStore:true}
  loadTrustStore: ${client.loadTrustStore:true}
  trustStore: ${client.trustStore:client.truststore}
  trustStorePass: ${client.trustStorePass:password}
  loadKeyStore: ${client.loadKeyStore:false}
  keyStore: ${client.keyStore:client.keystore}
  keyStorePass: ${client.keyStorePass:password}
  keyPass: ${client.keyPass:password}
  defaultCertPassword: ${client.defaultCertPassword:changeit}
  tlsVersion: ${client.tlsVersion:TLSv1.3}

oauth:
  multipleAuthServers: ${client.multipleAuthServers:false}
  token:
    cache:
      capacity: ${client.tokenCacheCapacity:200}
    tokenRenewBeforeExpired: ${client.tokenRenewBeforeExpired:60000}
    expiredRefreshRetryDelay: ${client.expiredRefreshRetryDelay:2000}
    earlyRefreshRetryDelay: ${client.earlyRefreshRetryDelay:4000}
    server_url: ${client.tokenServerUrl:}
    serviceId: ${client.tokenServiceId:com.networknt.oauth2-token-1.0.0}
    proxyHost: ${client.tokenProxyHost:}
    proxyPort: ${client.tokenProxyPort:}
    enableHttp2: ${client.tokenEnableHttp2:true}
    authorization_code: {}
    client_credentials: {}
    refresh_token: {}
    token_exchange: {}
    key: {}
  sign: {}
  deref: {}

pathPrefixServices: ${client.pathPrefixServices:}

request:
  errorThreshold: ${client.errorThreshold:2}
  connectTimeout: ${client.connectTimeout:2000}
  timeout: ${client.timeout:3000}
  resetTimeout: ${client.resetTimeout:7000}
  injectOpenTracing: ${client.injectOpenTracing:false}
  injectCallerId: ${client.injectCallerId:false}
  enableHttp2: ${client.enableHttp2:true}
  connectionPoolSize: ${client.connectionPoolSize:1000}
  connectionExpireTime: ${client.connectionExpireTime:1800000}
  maxReqPerConn: ${client.maxReqPerConn:1000000}
  maxConnectionNumPerHost: ${client.maxConnectionNumPerHost:1000}
  minConnectionNumPerHost: ${client.minConnectionNumPerHost:250}
  maxRequestRetry: ${client.maxRequestRetry:3}
  requestRetryDelay: ${client.requestRetryDelay:1000}
  poolMetricsEnabled: ${client.poolMetricsEnabled:false}
  poolWarmUpEnabled: ${client.poolWarmUpEnabled:false}
  poolWarmUpSize: ${client.poolWarmUpSize:1}
  healthCheckEnabled: ${client.healthCheckEnabled:true}
  healthCheckIntervalMs: ${client.healthCheckIntervalMs:30000}
```

Rust should add fields such as `tls.caCertPath`, `tls.clientCertPath`, and
`tls.clientKeyPath` because PEM files are the native Rust deployment shape.
Rust does not need to support Java-specific JKS/JCEKS truststore or keystore
formats. If those Java-only fields appear in a Rust `client.yml`, they can be
ignored because config-server should control which fields it injects for Rust
services.

## Current Rust Gaps

Today, the Rust implementation has three separate interpretations of client
configuration:

| Area | Current behavior | Problem |
| --- | --- | --- |
| `light-runtime` config-server and portal-registry clients | Reads `ClientConfig { verify_hostname }` from top-level `client.yml` | Does not understand the Java nested `tls.verifyHostname` shape |
| `light-pingora` token, security JWKS, stateless auth, and MSAL exchange | Reads `ClientTokenConfig` with `tls`, `oauth`, `pathPrefixServices`, and `request` | Closer to Java, but it is framework-local and does not drive runtime clients |
| `light-gateway` upstream proxy | Reads the resolved flat value `client.verifyHostname` directly from `values.yml` | Bypasses typed `client.yml` and can disagree with other modules |

Current Rust support is also partial compared with Java:

| Java capability | Rust status |
| --- | --- |
| `tls.verifyHostname` | Supported by Pingora token/SPAs, not by runtime controller/config-server clients |
| CA trust | Supported through Rust `caCertPath`; Java truststore fields are not modeled |
| Client certificate and key for mTLS | Not yet modeled for outbound clients |
| TLS version | Not yet modeled |
| Request connect and total timeout | Supported for token/SPAs |
| Retries, circuit breaker, pool sizing, pool health | Not yet modeled as shared client behavior |
| OAuth `authorization_code` | Supported by SPA auth |
| OAuth `client_credentials` | Supported by token handler |
| OAuth `refresh_token` | Supported by SPA auth |
| OAuth `token_exchange` | Supported by MSAL exchange and SPA auth |
| OAuth token `key` / JWKS | Partially supported by security runtime |
| `token.key.serviceIdAuthServers` and audience | Not fully modeled in Rust |
| OAuth `sign` | Not yet modeled |
| OAuth `sign.key` / sign JWKS | Not yet modeled |
| OAuth `deref` | Not yet modeled |
| Multiple auth providers by service id | Supported for client credentials, but should become a shared resolver |
| `pathPrefixServices` | Supported in token handler, but should become shared resolver logic |

## Goals

- Keep `client.yml` as the only config file for outbound client behavior.
- Make the Java nested shape canonical: `tls.verifyHostname`, not top-level
  `verifyHostname`.
- Load and register the resolved `client.yml` once through `light-runtime`.
- Share one typed `ClientConfig` across runtime, Pingora, gateway, agent,
  deployer, MCP clients, model-provider clients, and future products.
- Preserve Java-compatible field names and config-server placeholder names.
- Support direct URL, direct registry, and portal registry service discovery
  consistently for token, key, sign, deref, and generic outbound calls.
- Keep secrets masked in module registry snapshots and logs.
- Make invalid active client config fail startup or reject reload before it
  changes live runtime behavior.
- Allow Rust-native PEM fields without forcing Java keystore names into every
  Rust deployment.

## Non-Goals

- Do not move handler activation into `client.yml`. Handler-specific files such
  as `token.yml`, `statelessAuth.yml`, and `msal-exchange.yml` still decide
  whether a handler runs.
- Do not implement every Java-only low-level connection-pool behavior in the
  first phase. The shared schema should include the fields so config is not
  lost, but unsupported fields can be ignored deliberately until the transport
  supports them.
- Do not expose decrypted client secrets, tokens, or legacy Java password
  fields through module registry, MCP tools, logs, metrics, or cache output.
- Do not require every module to use OAuth. The shared config must support
  simple TLS-only clients too.

## Resolved Decisions

- Create a separate `light-client` crate now so the shared config, HTTP client
  factory, OAuth client, and provider resolver can be reused without coupling
  every consumer to `light-runtime`.
- Standardize Rust outbound TLS material on PEM paths. Java truststore and
  keystore formats are not required for Rust services.
- `client.yml` reload should not force an immediate portal-registry reconnect.
  Reload is primarily for newly onboarded JWKS/JWT access and future outbound
  requests. Existing long-lived controller connections can keep running until
  their normal reconnect or service restart.
- Unsupported Java fields can be ignored by Rust. Config-server should avoid
  injecting unsupported fields into Rust service config.
- Ignored Java-only fields should be ignored silently. Rust startup does not
  need to warn about fields that config-server may omit for Rust services.
- `oauth.multipleAuthServers` remains accepted for Java compatibility, but Rust
  should infer multi-provider mode when `serviceIdAuthServers` is configured.
- `pathPrefixServices` stays in `client.yml`. It is outbound-client provider
  selection and is different from inbound path routing to downstream services.
- Circuit breaker behavior is only needed by Pingora. Shared request config can
  carry the Java-compatible fields, but non-Pingora clients do not need to own
  circuit breaker state.
- SAML bearer is not required for Light Fabric and should remain out of scope
  unless a future product explicitly needs it.

## Proposed Canonical Shape

The canonical Rust `client.yml` should stay close to Java:

```yaml
tls:
  verifyHostname: ${client.verifyHostname:true}
  caCertPath: ${client.caCertPath:}
  clientCertPath: ${client.clientCertPath:}
  clientKeyPath: ${client.clientKeyPath:}
  tlsVersion: ${client.tlsVersion:TLSv1.3}

request:
  connectTimeout: ${client.connectTimeout:2000}
  timeout: ${client.timeout:3000}
  maxRequestRetry: ${client.maxRequestRetry:3}
  requestRetryDelay: ${client.requestRetryDelay:1000}
  errorThreshold: ${client.errorThreshold:2}
  resetTimeout: ${client.resetTimeout:7000}
  injectCallerId: ${client.injectCallerId:false}
  enableHttp2: ${client.enableHttp2:true}
  connectionPoolSize: ${client.connectionPoolSize:1000}
  connectionExpireTime: ${client.connectionExpireTime:1800000}
  maxReqPerConn: ${client.maxReqPerConn:1000000}
  maxConnectionNumPerHost: ${client.maxConnectionNumPerHost:1000}
  minConnectionNumPerHost: ${client.minConnectionNumPerHost:250}
  poolMetricsEnabled: ${client.poolMetricsEnabled:false}
  poolWarmUpEnabled: ${client.poolWarmUpEnabled:false}
  poolWarmUpSize: ${client.poolWarmUpSize:1}
  healthCheckEnabled: ${client.healthCheckEnabled:true}
  healthCheckIntervalMs: ${client.healthCheckIntervalMs:30000}

oauth:
  multipleAuthServers: ${client.multipleAuthServers:false}
  token:
    cache:
      capacity: ${client.tokenCacheCapacity:200}
    tokenRenewBeforeExpired: ${client.tokenRenewBeforeExpired:60000}
    expiredRefreshRetryDelay: ${client.expiredRefreshRetryDelay:2000}
    earlyRefreshRetryDelay: ${client.earlyRefreshRetryDelay:4000}
    server_url: ${client.tokenServerUrl:}
    serviceId: ${client.tokenServiceId:com.networknt.oauth2-token-1.0.0}
    proxyHost: ${client.tokenProxyHost:}
    proxyPort: ${client.tokenProxyPort:}
    enableHttp2: ${client.tokenEnableHttp2:true}
    authorization_code:
      uri: ${client.tokenAcUri:/oauth2/token}
      client_id: ${client.tokenAcClientId:}
      client_secret: ${client.tokenAcClientSecret:}
      redirect_uri: ${client.tokenAcRedirectUri:}
      scope: ${client.tokenAcScope:}
    client_credentials:
      uri: ${client.tokenCcUri:/oauth2/token}
      client_id: ${client.tokenCcClientId:}
      client_secret: ${client.tokenCcClientSecret:}
      scope: ${client.tokenCcScope:}
      serviceIdAuthServers: ${client.tokenCcServiceIdAuthServers:}
    refresh_token:
      uri: ${client.tokenRtUri:/oauth2/token}
      client_id: ${client.tokenRtClientId:}
      client_secret: ${client.tokenRtClientSecret:}
      scope: ${client.tokenRtScope:}
    token_exchange:
      uri: ${client.tokenExUri:/oauth2/token}
      client_id: ${client.tokenExClientId:}
      client_secret: ${client.tokenExClientSecret:}
      scope: ${client.tokenExScope:}
      subjectToken: ${client.subjectToken:}
      subjectTokenType: ${client.subjectTokenType:urn:ietf:params:oauth:token-type:jwt}
      requestedTokenType: ${client.requestedTokenType:}
      audience: ${client.tokenExAudience:}
    key:
      server_url: ${client.tokenKeyServerUrl:}
      serviceId: ${client.tokenKeyServiceId:com.networknt.oauth2-key-1.0.0}
      uri: ${client.tokenKeyUri:/oauth2/key}
      client_id: ${client.tokenKeyClientId:}
      client_secret: ${client.tokenKeyClientSecret:}
      enableHttp2: ${client.tokenKeyEnableHttp2:true}
      serviceIdAuthServers: ${client.tokenKeyServiceIdAuthServers:}
      audience: ${client.tokenKeyAudience:}
  sign:
    server_url: ${client.signServerUrl:}
    serviceId: ${client.signServiceId:com.networknt.oauth2-token-1.0.0}
    uri: ${client.signUri:/oauth2/sign}
    timeout: ${client.signTimeout:2000}
    client_id: ${client.signClientId:}
    client_secret: ${client.signClientSecret:}
    proxyHost: ${client.signProxyHost:}
    proxyPort: ${client.signProxyPort:}
    enableHttp2: ${client.signEnableHttp2:true}
    key:
      server_url: ${client.signKeyServerUrl:}
      serviceId: ${client.signKeyServiceId:com.networknt.oauth2-key-1.0.0}
      uri: ${client.signKeyUri:/oauth2/key}
      client_id: ${client.signKeyClientId:}
      client_secret: ${client.signKeyClientSecret:}
      enableHttp2: ${client.signKeyEnableHttp2:true}
      audience: ${client.signKeyAudience:}
  deref:
    server_url: ${client.derefServerUrl:}
    serviceId: ${client.derefServiceId:com.networknt.oauth2-token-1.0.0}
    uri: ${client.derefUri:/oauth2/deref}
    client_id: ${client.derefClientId:}
    client_secret: ${client.derefClientSecret:}
    proxyHost: ${client.derefProxyHost:}
    proxyPort: ${client.derefProxyPort:}
    enableHttp2: ${client.derefEnableHttp2:true}

pathPrefixServices: ${client.pathPrefixServices:}
```

Compatibility aliases:

- Accept `serverUrl` in addition to Java `server_url` for Rust callers.
- Accept `clientId` and `clientSecret` in addition to Java `client_id` and
  `client_secret` only as aliases. The emitted template should keep Java names.
- Temporarily accept top-level `verifyHostname` only as a migration fallback,
  but register a warning and normalize it into `tls.verifyHostname`.

Serde strategy for the top-level `verifyHostname` fallback:

- The shared `ClientConfig` should deserialize into a struct that has a
  `tls.verifyHostname` field and a separate `#[serde(default)]` top-level
  `verify_hostname` field.
- After deserialization, a post-parse normalization step should check whether
  the top-level field was explicitly set. If so, it logs a deprecation warning
  and copies the value into `tls.verify_hostname` only when the nested field
  was not also explicitly set.
- When both the top-level and nested fields are present, the nested
  `tls.verifyHostname` value wins. The top-level value is ignored after the
  warning.
- Do not rely on two competing `#[serde(default)]` fields resolving the
  conflict. Use a custom `Deserialize` impl or an explicit post-parse step.

Serde strategy for Java-compatible but unimplemented sections:

- Do not use `#[serde(deny_unknown_fields)]` for the top-level `ClientConfig`
  or OAuth section during Phase 1.
- Known but not-yet-implemented Java sections such as `oauth.sign` and
  `oauth.deref` should deserialize into typed structs or `serde_json::Value`
  placeholders so representative Java fixtures load successfully.
- Demand-driven validation decides whether a section is required. If no active
  module consumes `oauth.sign` or `oauth.deref`, those sections can be present
  and ignored silently.

## Proposed Rust Modules

### Shared Config Model

Create one shared typed config model outside `light-pingora` and
`light-runtime`:

```text
crates/light-client/src/lib.rs
crates/light-client/src/config.rs
crates/light-client/src/http.rs
crates/light-client/src/oauth.rs
crates/light-client/src/provider.rs
```

`light-runtime` should use `light-client` for loading, validating, and building
outbound clients, but the reusable client model should not live inside the
runtime crate.

Core types:

```rust
pub struct ClientConfig {
    pub tls: ClientTlsConfig,
    pub request: ClientRequestConfig,
    pub oauth: ClientOauthConfig,
    pub path_prefix_services: BTreeMap<String, String>,
}

pub struct ClientTlsConfig {
    pub verify_hostname: bool,
    pub ca_cert_path: Option<PathBuf>,
    pub client_cert_path: Option<PathBuf>,
    pub client_key_path: Option<PathBuf>,
    pub tls_version: Option<TlsVersion>,
}

pub struct ClientRequestConfig {
    pub connect_timeout_ms: u64,
    pub timeout_ms: u64,
    pub max_request_retry: u32,
    pub request_retry_delay_ms: u64,
    pub error_threshold: u32,
    pub reset_timeout_ms: u64,
    pub inject_caller_id: bool,
    pub enable_http2: bool,
    pub pool: ClientPoolConfig,
}
```

`TlsVersion` should be an enum with serde names for Java-compatible strings
such as `TLSv1.2` and `TLSv1.3`, rather than a raw string in runtime code.

Secrets should use a type that serializes as masked data for registry output,
or the registry masks should cover every secret field recursively.

### Runtime Loader

`light-runtime` should own the startup lifecycle for `client.yml` loading, but
delegate parsing and validation to `light-client`:

1. Load local `values.yml`.
2. Load local `startup.yml`.
3. Load local `client.yml` with resolved values for config-server bootstrap.
4. Fetch remote config if configured.
5. Rebuild the final `RuntimeConfig` with the remote `client.yml` overlay.
6. Register masked `light-client/client` in `ModuleRegistry`.

Every runtime client should use this shared config:

- config-server fetch client
- portal-registry WebSocket client
- MCP client
- future model-provider outbound clients
- framework/application clients through `RuntimeConfig.client`

For the earlier hostname-verification bug, the controller client should read:

```text
runtime_config.client.tls.verify_hostname
```

not a separate top-level `ClientConfig.verify_hostname`.

### HTTP Client Factory

Add a small factory that converts `ClientConfig` plus optional per-endpoint
overrides into concrete clients:

```rust
pub struct ClientFactory {
    config: Arc<ClientConfig>,
    direct_registry: DirectRegistryConfig,
    registry_client: Option<Arc<PortalRegistryClient>>,
}

pub struct EndpointOptions {
    pub server_url: Option<String>,
    pub service_id: Option<String>,
    pub proxy_host: Option<String>,
    pub proxy_port: Option<u16>,
    pub enable_http2: Option<bool>,
    pub timeout_ms: Option<u64>,
}
```

Responsibilities:

- Build `reqwest::Client` with consistent TLS, timeout, proxy, HTTP/2, retry,
  and pool settings for non-Pingora consumers.
- Build Pingora `HttpPeer` options from the same TLS config for gateway
  upstream proxying.
- Resolve endpoint base URL by priority:
  1. direct `server_url`
  2. `direct-registry.yml`
  3. portal-registry discovery by `serviceId`
- Apply per-service `AuthServerConfig` overrides without duplicating resolver
  logic in each handler.

The config-server bootstrap path still starts from `BootstrapConfig` because it
needs enough client settings before remote `client.yml` has been fetched. To
keep `light-client` independent from `light-runtime`, the factory should not
take a `BootstrapConfig` type directly. Instead, `light-runtime` should adapt
`BootstrapConfig.connect_timeout`, `BootstrapConfig.timeout`, authorization,
and bootstrap CA path into `EndpointOptions` or a small bootstrap options type
owned by `light-client`.

### OAuth Client

Add a shared OAuth client module that implements Java `http-client` behavior:

```text
oauth/client_credentials
oauth/authorization_code
oauth/refresh_token
oauth/token_exchange
oauth/key
oauth/sign
oauth/deref
```

The existing `light-pingora` `SpaTokenClient`, token handler client
credentials code, and security JWKS fetcher should delegate to this shared
module. Handler modules still own request-path decisions, cookies, headers,
and rejection mapping.

OAuth provider selection should be one reusable resolver:

```rust
pub struct OAuthProviderResolver {
    client: Arc<ClientConfig>,
}

impl OAuthProviderResolver {
    pub fn service_for_path(&self, path: &str) -> Option<&str>;
    pub fn client_credentials_provider(&self, service_id: Option<&str>) -> Result<AuthServerConfig>;
    pub fn key_provider(&self, service_id: Option<&str>) -> Result<AuthServerConfig>;
}
```

Rules:

- Single-provider mode uses global `oauth.token.*` defaults.
- Multi-provider mode is enabled when `oauth.multipleAuthServers: true` or
  when relevant `serviceIdAuthServers` maps are non-empty.
- Multi-provider mode selects the service id from an explicit request header
  first, then outbound `pathPrefixServices`.
- `client_credentials.serviceIdAuthServers[serviceId]` selects the token
  provider.
- `key.serviceIdAuthServers[serviceId]` selects the JWKS/key provider.
- Per-service config inherits unset values from global `oauth.token` defaults.
- Path-prefix matching should be boundary-aware in Rust. Java uses
  `startsWith`; the Rust implementation can be stricter as an intentional
  improvement. Exact rule: a prefix matches when the request path equals the
  prefix or starts with `prefix + "/"`. Therefore `/api` matches `/api` and
  `/api/orders`, but does not match `/api-v2`.
- `pathPrefixServices` is not an inbound routing table. It maps outbound
  request paths to service ids only for client-side OAuth provider selection.

### Consumer Modules

All modules should consume the same shared config:

| Module | Uses |
| --- | --- |
| `light-runtime/config-server` | `light-client` `tls`, `request` |
| `light-runtime/portal-registry` | `light-client` `tls`, `request` |
| `light-pingora/security` | `oauth.token.key`, `tls`, `request`, provider resolver |
| `light-pingora/token` | `oauth.token.client_credentials`, token cache settings, provider resolver |
| `light-pingora/stateless-auth` | `authorization_code`, `refresh_token`, token client |
| `light-pingora/msal-exchange` | `token_exchange`, token client |
| `light-gateway/proxy` | `tls.verifyHostname`, request timeout and pool settings where Pingora supports them |
| `light-agent` | controller/MCP outbound clients |
| `light-deployer` | controller/MCP/outbound clients as needed |

## Reload Behavior

`client.yml` should be reloadable as a module, but reload must be conservative:

1. Load and validate the new config into a fresh `ClientConfig`.
2. Build new shared client factories and OAuth clients.
3. Swap the config atomically for future requests.
4. Clear OAuth token caches because client credentials, scopes, providers, or
   trust settings may have changed.
5. Keep old in-flight requests on their existing client instances.
6. Reject the reload if active modules cannot build required clients from the
   new config.

Reload atomicity: all runtimes that consume `client.yml` must be swapped
together in the same reload callback. Today, the gateway `TokenReloader`
already rebuilds `token_runtime`, `stateless_auth`, and `msal_exchange` as a
unit. This must remain a hard requirement. A reload that updates the client
config without also rebuilding dependent runtimes would leave stale TLS or
OAuth state in the old runtime instances.

Controller registration is long-lived. Reloading `client.yml` should not force
an immediate portal-registry reconnect. New TLS and request settings should
apply to future outbound clients and the next normal controller reconnect, but
the active controller WebSocket can remain open.

## Validation Rules

Base validation:

- `tls.verifyHostname: false` requires explicit trust material unless the
  transport has a clear dev-only mode.
- If Rust-native mTLS is configured, both client certificate and client key
  paths are required.
- `request.connectTimeout` and `request.timeout` must be positive.
- `proxyPort` must be 0 to 65535.
- `pathPrefixServices` keys must start with `/`.
- Secret fields may be empty only when the consuming active module does not
  need that grant.

OAuth validation should be demand-driven:

- If `token` handler is active and enabled, validate `client_credentials`.
- If `stateless-auth` is active, validate `authorization_code` and
  `refresh_token`.
- If `msal-exchange` is active, validate `token_exchange`.
- If `security.yml` enables JWKS bootstrap from key service, validate
  `oauth.token.key`.
- If a future sign module is active, validate `oauth.sign`.
- If a future deref module is active, validate `oauth.deref`.

This avoids forcing every service to configure every Java OAuth section.

Validation failure behavior:

- At startup, validation failures are fatal. The process must exit with a
  clear error message identifying which active module requires which missing
  or invalid client config section.
- On reload, validation failures are non-fatal. The reload is rejected, the
  old config stays live, and the rejection reason is logged and reported
  through the module registry reload outcome.

## Masking

Mask these fields recursively in registry output:

- `client_secret`
- `clientSecret`
- `trustStorePass`
- `keyStorePass`
- `keyPass`
- `defaultCertPassword`
- `subjectToken`
- `access_token`
- `refresh_token`
- `id_token`
- `authorization`
- any field ending in `Token` whose value is a scalar string (not a nested
  object, list, or URN-typed field like `subjectTokenType` or
  `requestedTokenType`)
- any field ending in `Secret`

Explicit exclusions from suffix matching:

- `subjectTokenType` - a URN string, not a secret.
- `requestedTokenType` - a URN string, not a secret.

The registry should store only the masked snapshot. It should not store raw
config and mask later.

## Migration Plan

### Phase 0: Deprecation Logging

- Add a `tracing::warn!` in `light-gateway` where it reads
  `resolved_values["client.verifyHostname"]` to alert operators that this path
  is deprecated and will be replaced by `runtime_config.client.tls.verify_hostname`.
- This gives operators visibility into the migration before behavior changes.

### Phase 1: Unify The Schema

- Add the `light-client` crate with the full shared `ClientConfig` type.
- Make `light-runtime` load nested `tls.verifyHostname`.
- Keep top-level `verifyHostname` as a temporary compatibility fallback.
- Update Rust config templates to include only the canonical nested shape.
- Add tests proving `client.verifyHostname: false` reaches config-server,
  portal-registry, token, security JWKS, SPA auth, and gateway proxy clients.

### Phase 2: Move Consumers To Shared Config

- Replace `light-pingora::token::ClientTokenConfig` with the `light-client`
  shared type or a type alias.
- Replace gateway direct `resolved_values["client.verifyHostname"]` lookup with
  `runtime_config.client.tls.verify_hostname`.
- Move JWKS, token, and SPA token HTTP client construction behind the shared
  client factory.
- Register one masked `light-client/client` module instead of separate partial
  client registry entries.

### Phase 3: Shared OAuth Provider Resolver

- Extract provider selection from the token handler.
- Support `token.key.serviceIdAuthServers` and `audience`.
- Use the same resolver for token injection and JWT key lookup.
- Keep Java field names and config-server placeholders.

### Phase 4: Java Feature Completion

- Add sign client support.
- Add deref client support.
- Add mTLS support using Rust-native PEM files.
- Add retries, circuit breaker, and pool behavior where the Rust transport
  supports them.

## Open Questions

None at this stage.

## Test Plan

Unit tests:

- Parse the Java `client.yml` template into the shared Rust config.
- Parse the current Rust `client.yml` template into the shared Rust config.
- Resolve `client.verifyHostname` into `tls.verifyHostname`.
- Accept top-level `verifyHostname` only as a fallback and prefer nested TLS
  when both are set.
- Mask every secret field in the module registry snapshot.
- Validate provider selection by service id and path prefix.
- Validate per-service override inheritance for token and key providers.

Runtime tests:

- Config-server bootstrap uses `tls.verifyHostname`.
- Portal-registry controller WebSocket uses `tls.verifyHostname`.
- Gateway upstream proxy uses `tls.verifyHostname`.
- Token handler, stateless auth, MSAL exchange, and security JWKS all receive
  the same `ClientConfig` instance or snapshot.
- Client reload clears token caches and rejects invalid active grant config.
- Reload round-trip: verify that reloading from config A to config B swaps the
  `ClientConfig`, creates fresh token caches, and that in-flight requests on
  the old config are not affected. Verify that a reload from valid config to
  invalid config is rejected and the old config stays live.

Compatibility tests:

- Reuse representative Java `client.yml` fixtures for single provider,
  multiple providers, proxy, token key, sign, and deref sections.
- Confirm Java-compatible form bodies for `authorization_code`,
  `client_credentials`, `refresh_token`, and `token_exchange`.
- Confirm config-server injected YAML strings and structured YAML maps both
  deserialize for `serviceIdAuthServers` and `pathPrefixServices`.
