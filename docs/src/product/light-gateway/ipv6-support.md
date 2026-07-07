# Light-Gateway IPv6 Support

`light-gateway` can run in IPv4-only, IPv6-only, and dual-stack networks. The
gateway uses `light-pingora` for the inbound HTTP and HTTPS listener, and uses
the same routing model for IPv4 and IPv6 upstream services.

## Configuration

The inbound bind address is controlled by `server.ip` and projected into
`server.yml`:

```yaml
ip: ${server.ip:0.0.0.0}
```

The default remains IPv4 wildcard binding:

```yaml
server.ip: 0.0.0.0
```

Use IPv6 wildcard binding when the host or container network should accept IPv6
connections:

```yaml
server.ip: "::"
```

Use a specific IPv6 address when the gateway should bind only to one interface:

```yaml
server.ip: "fdd0:0:0:1::10"
```

`server.advertisedAddress` is separate. It is the address registered with the
controller and shown to peers. Do not set it to `0.0.0.0` or `::`; use a stable
DNS name or a reachable address for the deployment:

```yaml
server.advertisedAddress: ai-microgateway.light-gateway
```

## Listener Behavior

`light-gateway` validates `server.ip` as an IP address before starting the
Pingora listener. It then builds the listener socket with the parsed IP and the
configured HTTP or HTTPS port.

Examples:

```text
server.ip: 0.0.0.0, server.httpsPort: 8443 -> 0.0.0.0:8443
server.ip: "::",    server.httpsPort: 8443 -> [::]:8443
```

This avoids the invalid IPv6 address form that results from concatenating the
IP and port as strings.

## Upstream Routing

Gateway upstream routes can use DNS names, IPv4 literals, or bracketed IPv6
literals.

For `proxy.hosts`:

```yaml
hosts: https://[fdd0:0:0:1::20]:8443
```

For `direct-registry.directUrls`:

```yaml
directUrls:
  com.example.orders-1.0.0: https://[fdd0:0:0:1::21]:8443
```

Discovery responses may also contain IPv6 addresses. The router and websocket
router bracket IPv6 discovery addresses before constructing the upstream
authority.

When an upstream is referenced by DNS name, the selected address family depends
on DNS resolution and the connector behavior. In dual-stack Docker or
Kubernetes networks, make sure the backend listens on the address family that
DNS returns first, or use service discovery/configuration that points to a
reachable address.

## Native Deployment

For native host deployment, keep the bind address aligned with the host network:

```yaml
server.ip: "::"
server.advertisedAddress: gateway.example.com
server.httpsPort: 8443
server.enableHttps: true
```

Verify the host firewall and TLS certificate cover the advertised hostname.

## Kubernetes Deployment

For Kubernetes, use IPv6 binding only when the cluster and service are intended
to expose IPv6 traffic:

```yaml
server.ip: "::"
server.advertisedAddress: ai-microgateway.light-gateway
```

The Service, pod network, DNS policy, and any ingress or Gateway API resources
must also support IPv6. The gateway bind address alone does not make the cluster
dual-stack.

## Verification

Inside the same network namespace or from a peer pod/container:

```bash
getent ahosts <gateway-service-name>
curl -k -g https://[<gateway-ipv6>]:8443/health
curl -k -v https://<gateway-service-name>:8443/health
```

For an upstream backend reached through `light-gateway`, confirm both sides:

```bash
getent ahosts <backend-service-name>
curl -k -v https://<gateway-host>/<gateway-route>
```

If the gateway log shows connection refused to an IPv6 upstream address, the
backend service is likely not listening on IPv6 or the network does not route
that address family.
