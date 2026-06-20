# Light-Axum IPv6 Support

## Problem

`light-axum` binds application listeners from `server.yml` through the shared
`light-runtime` server configuration. The bind IP is configured with
`server.ip`, and most product templates default it to `0.0.0.0`.

IPv4 wildcard binding works for IPv4-only networks, but dual-stack container
and Kubernetes networks can publish both IPv4 and IPv6 service addresses. If a
client resolves an Axum service name to IPv6 first, the service must either
listen on IPv6 or the client must retry an IPv4 address. Relying on client
fallback is not enough for gateway and service-to-service traffic.

Before IPv6 support, the transport built bind addresses with string
concatenation:

```rust
format!("{}:{port}", config.server.ip).parse()
```

That works for `0.0.0.0:8080`, but fails for IPv6 wildcard binding because
`::` plus port becomes `:::8080` instead of `[::]:8080`.

## Goals

- Support IPv4 and IPv6 bind addresses for all applications using
  `AxumTransport`.
- Keep the existing default of `server.ip: 0.0.0.0`.
- Preserve current runtime configuration names and deployment templates.
- Fail early with a clear error when `server.ip` is not a valid IP address.

## Non-Goals

- Do not enable IPv6 by default.
- Do not change advertised address resolution or portal-registry registration.
- Do not change TLS behavior or application routing.
- Do not add client-side IPv4 fallback in this change.

## Configuration

The bind address remains the existing `server.ip` property.

IPv4 wildcard:

```yaml
server.ip: 0.0.0.0
```

IPv6 wildcard:

```yaml
server.ip: "::"
```

Specific IPv4 address:

```yaml
server.ip: 172.16.1.9
```

Specific IPv6 address:

```yaml
server.ip: "fdd0:0:0:1::9"
```

External templates should continue projecting this value into `server.yml`:

```yaml
ip: ${server.ip:0.0.0.0}
```

## Implementation

`light-axum` parses `server.ip` as an `IpAddr` and constructs the listener with
`SocketAddr::new(ip, port)`.

This keeps IPv4 output unchanged and produces bracketed IPv6 socket addresses
where required by Rust networking APIs:

```text
0.0.0.0 + 8080 -> 0.0.0.0:8080
:: + 8080      -> [::]:8080
```

The change lives in the framework transport, so it applies to products using
`AxumTransport`, including `portal-service`, `light-agent`, and
`light-deployer`.

## Deployment Guidance

Only set `server.ip: "::"` when the host or container network is intended to
serve IPv6 traffic. In dual-stack deployments, verify both sides:

- the runtime receives an IPv6 address;
- DNS or service discovery returns reachable addresses;
- dependent clients or gateways can connect to the IPv6 endpoint;
- health checks cover the selected address family.

If the environment is IPv4-only, keep `server.ip: 0.0.0.0`.

## Verification

For an Axum service configured with IPv6 wildcard binding:

```bash
server.ip: "::"
```

verify from a peer in the same network:

```bash
getent ahosts <service-name>
curl -k -g https://[<service-ipv6>]:<port>/health
```

If access goes through service DNS:

```bash
curl -k -v https://<service-name>:<port>/health
```

The first resolved address family must be reachable, or the caller must have a
retry/fallback strategy.
