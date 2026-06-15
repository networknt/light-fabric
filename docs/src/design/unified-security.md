# Unified Security Handler

Status: Phase 4 partially implemented; `jwkServiceIds` and `sjwkServiceIds` per-prefix JWK routing are wired, SJWT routing is implemented, and SWT introspection remains outstanding.

## Purpose

Light Fabric's `light-gateway` serves as a shared API gateway for multiple upstream
services that may belong to different organizations or security domains. In this shared
model, different request path prefixes need different authentication strategies:

- An internal `/admin` route may require HTTP Basic authentication.
- A customer-facing `/api/orders` route may require a JWT from the company's own
  identity provider.
- A partner `/salesforce` route may require a JWT issued by Salesforce with its own
  JWK endpoint.
- A webhook `/webhook` route may require an API key.

The `UnifiedSecurityHandler` (Java) / `unified-security` handler (Rust) solves this by
providing a single, path-prefix-aware security dispatch point. It replaces the need to
wire separate security handlers into independent handler chains for each path family.

## Java Reference

The canonical implementation lives in:

- **Handler**: `light-4j/unified-security/src/main/java/com/networknt/security/UnifiedSecurityHandler.java`
- **Config**: `light-4j/unified-config/src/main/resources/config/unified-security.yml`

The Java handler:

1. Loads `UnifiedSecurityConfig` on every request (double-checked locking, hot-reload safe).
2. Checks `anonymousPrefixes` first — if the path matches, all security checks are skipped.
3. Iterates `pathPrefixAuths`; the **first** matching prefix wins.
4. For the matched rule, checks which auth methods are enabled (`basic`, `jwt`, `sjwt`,
   `swt`, `apikey`) and dispatches to the corresponding sub-handler.
5. Passes `jwkServiceIds` / `sjwkServiceIds` / `swtServiceIds` to the sub-handler so
   it can fetch JWKs from the correct per-prefix OAuth/JWK server.
6. Returns `ERR10078 MISSING_PATH_PREFIX_AUTH` if no rule matches any prefix.

## Rust Implementation Location

```
frameworks/light-pingora/src/unified_security.rs
```

The Rust implementation is loaded in `apps/light-gateway/src/main.rs` when the
`unified-security` or `unified` handler IDs appear in the active handler chain:

```rust
let unified_security_config = load_unified_security_config(
    &runtime_config,
    handler_active(&active_handlers, &["unified-security", "unified"]),
)?;
```

## Configuration

### `unified-security.yml`

```yaml
# Enable or disable this handler.
enabled: ${unified-security.enabled:true}

# Paths that bypass all security checks.
# Accepts comma-separated string, JSON array string, or YAML list.
anonymousPrefixes: ${unified-security.anonymousPrefixes:[]}

# Per-prefix authentication rules.
# Accepts comma-separated string, JSON array string, or YAML list of objects.
pathPrefixAuths: ${unified-security.pathPrefixAuths:[]}
```

### Per-Prefix Rule Fields

| Field           | Type            | Purpose                                                                     |
|-----------------|-----------------|-----------------------------------------------------------------------------|
| `prefix`        | `String`        | Path prefix to match. Longest matching prefix wins (Rust) / first wins (Java). |
| `basic`         | `bool`          | Allow HTTP Basic authentication for this prefix.                             |
| `jwt`           | `bool`          | Require Bearer JWT verification for this prefix.                             |
| `sjwt`          | `bool`          | Allow Simple-JWT (no scopes) for this prefix.                                |
| `swt`           | `bool`          | Allow SWT (opaque token introspection) for this prefix.                      |
| `apikey`        | `bool`          | Allow API key authentication for this prefix.                                |
| `jwkServiceIds` | `Vec<String>`   | JWK service IDs (from `client.yml`) used to verify JWT tokens for this prefix. |
| `sjwkServiceIds`| `Vec<String>`   | JWK service IDs used to verify SJWT tokens for this prefix.                 |
| `swtServiceIds` | `Vec<String>`   | Introspection service IDs used to verify SWT tokens for this prefix.         |

### Example `values.yml` Entry

```yaml
handler.handlers:
  - correlation
  - headers
  - unified-security
  - proxy

handler.defaultHandlers:
  - default

unified-security.anonymousPrefixes:
  - /health
  - /server/info

unified-security.pathPrefixAuths:
  - prefix: /salesforce
    jwt: true
    jwkServiceIds:
      - com.networknt.oauth2-salesforce-1.0.0
  - prefix: /blackrock
    jwt: true
    jwkServiceIds:
      - com.networknt.oauth2-blackrock-1.0.0
  - prefix: /admin
    basic: true
  - prefix: /webhook
    apikey: true
  - prefix: /internal
    jwt: true
```

### Why `unified-security.yml` Is Not in the `light-gateway` Config Folder

The `config/` directory in `apps/light-gateway` contains only **active** handler
configurations that the current local development profile uses. The local profile
(defined by `config/values.yml`) activates only `correlation`, `headers`, and `proxy`.
Because `unified-security` is not in that handler chain, `load_unified_security_config`
returns `None` and the file is never needed.

A production or staging deployment that enables unified security would receive
`unified-security.yml` from config-server, populated by the `light-portal` product
configuration for that deployment. To use it locally, add `unified-security` to
`handler.handlers` and `handler.defaultHandlers` (or a path-specific chain) in
`values.yml`, then add a `unified-security.yml` to the `config/` directory.

## Prefix Matching: Java vs. Rust

| Behavior | Java | Rust |
|---|---|---|
| Match algorithm | First matching prefix in list order | Longest matching prefix (most specific wins) |
| Tie-breaking | Order in config list | Longest `prefix.len()` |

The Rust `best_auth_rule` function uses `max_by_key(|rule| rule.prefix.len())`, which
is intentionally more deterministic than Java's iteration order. This means `/api/v2`
will match before `/api` regardless of declaration order.

## Authentication Dispatch Logic

```text
Request arrives at unified-security handler
│
├── anonymousPrefixes match? → Pass through (no auth)
│
├── No matching pathPrefixAuth rule? → 403 ERR10078
│
└── Matched rule:
    ├── basic=true OR jwt=true OR sjwt=true OR swt=true?
    │   ├── No Authorization header → 401
    │   ├── Scheme=Basic AND basic=true → BasicAuth verify
    │   ├── Scheme=Bearer:
    │   │   ├── jwt=true → JWT verify (using jwkServiceIds)
    │   │   ├── sjwt=true → SJWT verify (using sjwkServiceIds)
    │   │   └── swt=true  → SWT introspect (using swtServiceIds) [⚠ Gap: not implemented]
    │   └── Unknown scheme → 401
    └── apikey=true (only) → API Key verify
```

## Current Implementation Status

### Implemented ✅

| Capability | Location |
|---|---|
| `UnifiedSecurityConfig` and `UnifiedPathAuth` deserialization | `unified_security.rs:15–55` |
| `anonymousPrefixes` bypass | `unified_security.rs:153–158` |
| `pathPrefixAuths` parsing (YAML, JSON-string, comma-string) | via `deserialize_typed_list` |
| Longest-prefix rule selection (`best_auth_rule`) | `unified_security.rs:160–169` |
| Basic auth dispatch | `unified_security.rs:113–123` |
| JWT/SJWT dispatch (Bearer) | `unified_security.rs:126–131` |
| `jwkServiceIds` / `sjwkServiceIds` JWK routing | `security.rs` |
| API key dispatch | `unified_security.rs:145–149` |
| Hot-reload via `ConfigManager` and `UnifiedSecurityReloader` | `main.rs:1374–1410` |
| Handler IDs: `unified-security`, `unified` | `main.rs:114, 121, 128, 131, 133` |

### Gaps ⚠️

#### Gap 1 — SWT (opaque token) introspection not implemented (Low)

When `swt=true`, the Rust handler returns HTTP 501. SWT introspection requires
calling an OAuth2 introspection endpoint, which needs service discovery and client
credentials.

**Fix**: Implement SWT introspection using the existing `client.yml` OAuth provider
infrastructure once service discovery is stable.

### Recently Closed Gaps

#### `jwkServiceIds` and `sjwkServiceIds` per-prefix JWK routing

`verify_unified_security` now passes the matched rule's `jwkServiceIds` or
`sjwkServiceIds` list into JWT verification. The Rust verifier tries the configured
service IDs in order for JWK lookup and accepts any matching configured audience.

#### SJWT routing

Java supports two SJWT modes:
- `sjwt=true, jwt=false` — always treated as SJWT.
- `sjwt=true, jwt=true` — pre-parses the JWT to check for `scope`/`scp` claim to
  distinguish SJWT (no scope) from a full JWT (with scope).

Rust now implements the same routing split. Non-JWT Bearer tokens are routed to SWT
when `swt=true`; otherwise they are rejected as unsupported Bearer tokens.

#### `unified-security.yml` added to `light-gateway` config folder

The sample `config/` directory now includes an example `unified-security.yml`, making
the expected configuration clearer when activating the handler.

#### Java uses first-match; Rust uses longest-match (Design difference)

This is an intentional Rust improvement, not a bug, but it should be documented clearly
so operators migrating from Java understand that configuration ordering matters less in
Rust. The design difference is already captured in this document.

## Interaction with `security.yml`

`unified-security` and `security.yml` (standalone JWT handler) are mutually exclusive
in a given handler chain. Do not include both `unified-security` and `jwt` handler IDs
in the same chain; the security check would be applied twice.

When `unified-security` is active:

- `security.yml` is still loaded to provide the `SecurityRuntime` (JWK cache, config).
- `basic-auth.yml` is loaded if any rule has `basic: true`.
- `apikey.yml` is loaded if any rule has `apikey: true`.

## Interaction with `client.yml`

JWK source resolution uses the `client.yml` OAuth/JWK configuration:

```yaml
oauth:
  token:
    key:
      serviceId: com.networknt.oauth2-token-1.0.0
      serviceIdAuthServers:
        com.networknt.oauth2-salesforce-1.0.0:
          server_url: https://login.salesforce.com
          uri: /id/keys
        com.networknt.oauth2-blackrock-1.0.0:
          server_url: https://idp.blackrock.com
          uri: /.well-known/jwks.json
```

`jwkServiceIds: [com.networknt.oauth2-salesforce-1.0.0]` in a
`pathPrefixAuth` rule will cause the JWT verifier to fetch and cache JWKs from
`https://login.salesforce.com/id/keys` for that path prefix only.

## Verification Plan

### Existing Tests

- `tests::unified_security_accepts_java_style_lists` in `unified_security.rs` — verifies
  YAML/JSON deserialization for anonymousPrefixes and pathPrefixAuths.

### Tests to Add

1. **`jwkServiceIds` override** — mock two JWK servers; configure two prefixes pointing
   to different service IDs; verify that JWT verification for each prefix fetches from
   the correct server.

2. **SJWT scope detection** — provide a JWT with and without a `scope` claim; verify
   that `sjwt=true, jwt=true` routes to the correct verifier.

3. **SJWT-only rule** — `sjwt=true, jwt=false`; verify the handler always uses the SJWT
   verifier regardless of scope presence.

4. **SWT rule** — configure `swt=true` with a mock introspection endpoint; verify the
   handler calls introspection with the correct service ID.

5. **No-match returns 403** — request a path not covered by any prefix; verify 403
   with `ERR10078`.

6. **Anonymous prefix bypass** — request a path in `anonymousPrefixes`; verify no auth
   header is required.
