# Client CA Bundle Support

## Status

Draft proposal with reviewed decisions.

The immediate requirement is customer deployment of `light-gateway` with a CA
chain file, for example `config/ca-bundle.crt`, to connect to both
config-server and controller. Light Fabric should support the same trust file
for all outbound clients, whether the file contains one PEM certificate or a
concatenated PEM bundle with multiple CA certificates.

## Purpose

Light Fabric clients should have one clear trust-material contract:

```yaml
tls:
  caCertPath: ${client.caCertPath:config/ca-bundle.crt}
  verifyHostname: ${client.verifyHostname:true}
```

The same `tls.caCertPath` value should work for:

- a single-certificate PEM file such as `config/ca.pem`
- a multi-certificate PEM bundle such as `config/ca-bundle.crt`

The `values.yml` key remains:

```yaml
client.caCertPath: config/ca-bundle.crt
```

`startup.bootstrapCaCertPath` remains the bootstrap fallback for config-server
and controller registration when `client.caCertPath` is empty.

## Problem

Current support is inconsistent across clients.

Some code paths already parse bundles, while others parse only one PEM
certificate:

| Area | Current trust path | Current bundle behavior |
| --- | --- | --- |
| Config-server HTTP client through `light-client` | `client.tls.caCertPath`, falling back to `startup.bootstrapCaCertPath` | Uses `reqwest::Certificate::from_pem_bundle` and can accept multiple PEM certificates |
| Portal-registry WebSocket client | Runtime passes CA bytes from the configured CA path | Uses `rustls_pemfile::certs`, which can parse multiple PEM certificates |
| `mcp-client` | Caller passes CA PEM bytes | Uses `reqwest::Certificate::from_pem`, so only one PEM certificate is accepted |
| `light-agent` portal-query client | Reads bootstrap CA bytes directly | Uses `reqwest::Certificate::from_pem`, so only one PEM certificate is accepted |
| `light-pingora` upstream proxy | Passes a CA file path to Pingora | Needs explicit bundle validation so the gateway does not depend on undocumented Pingora behavior |

This means the same customer bundle can work for config-server but fail for
MCP, portal-query, controller registration, or gateway upstream calls depending
on which client builds the TLS connector.

## Goals

- Support a PEM file with one certificate.
- Support a PEM bundle file with multiple certificates.
- Allow either `.pem` or `.crt` file names when the content is PEM encoded.
- Use `client.tls.caCertPath` as the canonical resolved config field.
- Keep `client.caCertPath` as the config-server `values.yml` placeholder key.
- Keep `startup.bootstrapCaCertPath` as the bootstrap fallback when
  `client.tls.caCertPath` is absent or empty.
- Define whether explicit CA bundles append to or replace default roots.
- Apply the same trust behavior to config-server, portal-registry, MCP,
  portal-query, token, JWKS, gateway upstream proxy, and future outbound
  clients.
- Preserve `tls.verifyHostname` semantics. A CA bundle controls trust anchors;
  it must not silently disable hostname verification.
- Report the CA path and certificate count in diagnostics without logging
  certificate contents.

## Non-Goals

- Do not add Java truststore or keystore support in this change. JKS/JCEKS
  support remains out of scope for Rust-native deployments.
- Do not treat `.crt` as DER by default. The initial requirement is a PEM
  encoded bundle saved with a `.crt` extension.
- Do not add a separate `caBundlePath` setting. `caCertPath` should accept both
  single-cert and bundle files.
- Do not hot-reload CA bundle files in this phase. Clients read trust material
  when they are constructed; certificate rotation requires a process restart or
  an explicit client rebuild through a later reload feature.
- Do not add a strict custom-only trust mode in this phase. That should be a
  separate option if customers need to disable platform or built-in roots.

## Configuration Contract

The canonical Rust `client.yml` remains:

```yaml
tls:
  verifyHostname: ${client.verifyHostname:true}
  caCertPath: ${client.caCertPath:}
```

Valid `values.yml` examples:

```yaml
client.caCertPath: config/ca.pem
client.verifyHostname: true
```

```yaml
client.caCertPath: config/ca-bundle.crt
client.verifyHostname: true
```

Bootstrap fallback example:

```yaml
startup.bootstrapCaCertPath: config/ca-bundle.crt
client.caCertPath:
```

Selection order for clients that need CA trust:

1. `client.tls.caCertPath` after `client.yml` is resolved.
2. Resolved `client.caCertPath` only for legacy paths that have not yet moved
   fully to typed `ClientConfig`.
3. `startup.bootstrapCaCertPath`.
4. Platform default trust roots when no explicit CA path is configured and the
   client library supports them.

Empty strings must be treated as absent.

If `client.tls.caCertPath` or `startup.bootstrapCaCertPath` is set to a
non-empty path, the selected file is required. A missing or unreadable file is
a fail-fast client construction error. The client must not silently fall back to
platform roots after an explicit CA path was configured.

Explicit CA bundles append to the default trust roots by default. This matches
the current reqwest behavior when `add_root_certificate` is used without
disabling built-in or platform roots, and it matches the Java default posture
where default trust stores remain enabled unless explicitly disabled. The
customer bundle adds private enterprise CAs without breaking public CA trust
for token servers, model providers, or other outbound HTTPS targets. Public CA
chains such as Let's Encrypt, DigiCert, or other platform-trusted roots remain
trusted when `caCertPath` is configured.

Rustls-only clients must follow the same policy. If the client builds its own
`RootCertStore`, it should populate that store from the default root source
first, then add every certificate from the configured bundle. A future strict
mode can add a separate setting to replace default roots, but that is not part
of this bundle-support change.

## File Format

The supported bundle format is PEM:

```text
-----BEGIN CERTIFICATE-----
...
-----END CERTIFICATE-----
-----BEGIN CERTIFICATE-----
...
-----END CERTIFICATE-----
```

The parser should accept:

- one certificate
- multiple certificates
- comments or whitespace tolerated by the underlying PEM parser

The parser should reject:

- an unreadable file
- a file with no certificates
- malformed PEM blocks
- recognized PEM blocks that are not certificates, such as private keys or
  certificate requests
- DER-encoded `.crt` files, until explicit DER support is designed and tested

File extension must not decide parsing. A `.pem` file can contain multiple PEM
certificates, and a `.crt` file can contain a PEM bundle.

Plain comments and whitespace outside certificate blocks may be ignored, but
the parser must not ignore another PEM object type. If a customer accidentally
concatenates a private key into the CA bundle, startup should fail for the
client that consumes that file.

## Affected Clients

### Shared HTTP Clients

`light-client` should be the reference implementation for reqwest-based
clients. Its TLS builder should continue to use bundle parsing and expose a
small helper that other reqwest clients can reuse instead of calling
`reqwest::Certificate::from_pem` directly.

Recommended helper behavior:

- read the configured path once
- parse with `reqwest::Certificate::from_pem_bundle`
- return all parsed certificates
- reject zero certificates
- reject non-certificate PEM blocks before adding trust roots
- include the path in read and parse errors
- return certificate count and the earliest certificate expiration date for
  diagnostics where the parser can extract them
- append parsed certificates to default roots instead of replacing them

### Config-Server Bootstrap

The config-server client should keep its current behavior:

- read `client.tls.caCertPath`
- fall back to `startup.bootstrapCaCertPath`
- parse all PEM certificates in the selected file
- honor `tls.verifyHostname` separately from CA trust

This path is the compatibility baseline for every other client.

### Portal-Registry Controller WebSocket

The runtime should pass the selected CA bundle bytes to `PortalRegistryClient`.
The portal-registry crate should keep parsing the bytes with
`rustls_pemfile::certs` and should add every parsed certificate to the rustls
root store.

The root store used for controller WebSocket connections should include default
roots plus the configured bundle. It should not switch to bundle-only trust just
because a custom CA path is configured.

When `verifyHostname` is false, the controller certificate chain should still
be validated against the configured bundle. The only skipped check should be
hostname/SAN matching.

### MCP Gateway Client

`crates/mcp-client` should replace `reqwest::Certificate::from_pem` with
bundle parsing. Its `ca_cert_pem` argument can keep the same name for API
compatibility, but the documentation should describe it as PEM trust bundle
bytes.

### Light-Agent Portal Query Client

The light-agent portal query client should use the same bundle parsing as
`mcp-client`. It should also use the same CA path selection as other clients:
`client.tls.caCertPath` first, then `startup.bootstrapCaCertPath`.

### Gateway Upstream Proxy

The Pingora gateway path should continue to pass the selected CA file to the
upstream TLS configuration only after proving that Pingora accepts PEM bundles
for that setting. If Pingora's native `ca_file` handling is bundle-safe, the
gateway can keep passing the selected file. If not, the gateway should parse
the bundle itself and construct or inject the upstream rustls root store through
the Pingora API.

The gateway should make the same path-selection decision as the runtime:

1. `client.tls.caCertPath`
2. resolved `client.caCertPath` compatibility fallback
3. `startup.bootstrapCaCertPath`

If Pingora/rustls rejects a malformed bundle, the error should include the CA
file path and the upstream target.

### Token, JWKS, And Future Outbound Clients

Any token, key/JWKS, sign, deref, model-provider, or generic outbound HTTP
client should use the shared HTTP TLS helper instead of parsing CA bytes
locally. This prevents new single-cert-only code paths from being introduced.

## Implementation Plan

1. Add shared bundle parsing utilities for reqwest-based clients.

   Suggested names:

   - `load_ca_cert_bundle(path: &Path) -> Result<Vec<reqwest::Certificate>, ClientBuildError>`
   - `add_ca_cert_bundle(builder, tls) -> Result<reqwest::ClientBuilder, ClientBuildError>`

   The helper should preserve default roots, append all certificates from the
   bundle, reject missing files, reject zero-certificate files, and reject
   unexpected PEM object types such as private keys.

2. Replace direct calls to `reqwest::Certificate::from_pem` in
   `mcp-client` and `light-agent` with bundle parsing.

3. Keep `light-client` config-server behavior as the reference and add tests
   proving one-cert and two-cert bundles are accepted.

4. Add portal-registry tests for multiple PEM certificates in the configured
   CA bytes. The test does not need a full WebSocket handshake; it can verify
   root-store loading behavior through an extracted parser helper if that makes
   the client easier to test.

5. Verify Pingora upstream TLS behavior with a real two-certificate PEM bundle.
   If native `ca_file` parsing does not accept the bundle, add explicit bundle
   parsing and root-store injection for gateway upstream TLS.

6. Add runtime tests for CA path selection:

   - `client.tls.caCertPath` wins over `startup.bootstrapCaCertPath`
   - empty `client.tls.caCertPath` falls back to `startup.bootstrapCaCertPath`
   - `.crt` paths are accepted the same as `.pem` paths
   - explicit missing CA path fails instead of falling back silently

7. Add diagnostics:

   - selected CA path
   - whether an explicit CA path is configured
   - parsed certificate count where available
   - earliest expiration date in the selected bundle where available
   - client name, such as `config-server`, `portal-registry`, `mcp-client`, or
     `portal-query`

8. Update product docs and examples to show `config/ca-bundle.crt` as the
   recommended customer deployment value.

## Validation Matrix

| Client | Single PEM | PEM bundle `.pem` | PEM bundle `.crt` | Malformed bundle | Empty path fallback |
| --- | --- | --- | --- | --- | --- |
| Config-server | required | required | required | required | required |
| Portal-registry | required | required | required | required | required |
| MCP client | required | required | required | required | not applicable if caller passes bytes |
| Light-agent portal query | required | required | required | required | required |
| Gateway upstream proxy | required | required | required | required | required |
| Token/JWKS clients | required | required | required | required | required |

The tests should use real PEM certificate fixtures with at least two
certificates. Synthetic strings that are not valid certificates are not enough
because reqwest and rustls parse and validate certificate structure before
adding roots.

Additional required tests:

- configured missing CA file fails client construction
- bundle with zero certificates fails client construction
- bundle with a private key PEM block fails client construction
- explicit CA bundle is appended to default roots where the client exposes a
  testable root-store path
- CA bundle rotation is not picked up until a new client is constructed

## Operational Guidance

For a customer-controlled CA chain, generate a bundle in PEM format:

```sh
cat root-ca.pem intermediate-ca.pem > ca-bundle.crt
```

Then set:

```yaml
client.caCertPath: config/ca-bundle.crt
client.verifyHostname: true
```

This custom bundle is additive. Public CAs from the platform or built-in root
store remain trusted unless a future strict replacement mode is explicitly
configured.

Use `startup.bootstrapCaCertPath` only when the same trust material is needed
before remote `client.yml` is loaded, or when the deployment wants one
bootstrap fallback for config-server and controller registration.

For Kubernetes Secret or ConfigMap rotation, roll the pod after updating the
bundle. Existing clients keep the trust roots they loaded at construction time.

If the certificate does not contain the controller or config-server hostname in
its SAN, fix the certificate when possible. Setting `client.verifyHostname:
false` should be a temporary development or emergency workaround; the chain
must still validate against the configured bundle.

## Debugging

When a customer reports a TLS failure, collect:

- the resolved `client.yml`
- the resolved `startup.yml`
- the CA file path selected by the client
- whether the selected file exists in the running container or VM
- certificate count parsed from the bundle
- earliest certificate expiration date in the bundle
- the server certificate SANs
- the hostname used in the URL

Useful checks:

```sh
openssl x509 -in config/ca-bundle.crt -noout -subject -issuer
openssl crl2pkcs7 -nocrl -certfile config/ca-bundle.crt | openssl pkcs7 -print_certs -noout
openssl s_client -connect controller.example.com:8438 -servername controller.example.com -showcerts
```

The first command only prints the first certificate in a bundle. Use the
`pkcs7` command to inspect all certificates in a PEM bundle.

## Resolved Decisions

- Deployment docs should require PEM encoding for every Rust product. DER
  `.crt` files are not supported by this design.
- Runtime diagnostics should expose the selected CA path, parsed certificate
  count, and earliest expiration date for each active constructed client.
- Invalid CA bundle files should fail only clients that are actually
  constructed. Disabled modules or dormant config should not block startup.

## Remaining Questions

- Should a later phase add a strict mode to replace default roots with only the
  configured bundle?
- Should certificate subject and issuer names be exposed in module registry
  diagnostics, or should diagnostics limit themselves to count and expiration
  to avoid unnecessary environment detail in normal admin views?
