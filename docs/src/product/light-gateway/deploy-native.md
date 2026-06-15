# Deploy Native

This page describes the recommended VM deployment model for the Rust
`light-gateway` native binary.

Use this model when a customer wants to run `light-gateway` as a microgateway on
a VM to protect backend MCP servers. The gateway starts from a small local
bootstrap config, downloads runtime config from config-server, then registers
itself with controller.

## Recommended Model

Deliver a versioned install bundle, not an ad hoc runtime script.

The bundle should contain:

- `light-gateway` native binary.
- Minimal bootstrap config files.
- A `systemd` unit.
- An install script for filesystem setup.
- A root-owned environment file for secrets.

The install script can create users, directories, symlinks, permissions, and the
`systemd` unit. It should not be the long-running process wrapper, and it should
not pass secrets as command-line arguments.

Use `systemd` to run the service:

- It restarts the process on failure.
- It keeps logs in the host journal.
- It avoids shell-history and process-list leakage from command-line secrets.
- It gives the customer a standard operational surface: `start`, `stop`,
  `restart`, `status`, and `journalctl`.

## Runtime Layout

`light-gateway` uses relative runtime paths:

- `config`
- `config-cache`

The `systemd` service should therefore set `WorkingDirectory` to the installed
application directory.

Recommended VM layout:

```text
/opt/light-gateway/
  light-gateway
  config -> /etc/light-gateway
  config-cache -> /var/lib/light-gateway/config-cache

/etc/light-gateway/
  startup.yml
  server.yml
  portal-registry.yml
  client.yml
  values.yml
  ca.pem
  light-gateway.env

/var/lib/light-gateway/
  config-cache/
```

The local `config` directory contains only bootstrap-time files. Runtime config
downloaded from config-server is written to `config-cache` before Pingora starts.
Keep `config-cache` writable by the `light-gateway` service user.

## Build Artifact

Build a release binary from `light-fabric`:

```sh
cargo build --release -p light-gateway
```

The artifact is:

```text
target/release/light-gateway
```

Build on a compatible Linux distribution for the customer VM. If the customer
fleet has mixed Linux versions, prefer a static or target-compatible build so
the binary does not fail on an older `glibc`.

Package with a versioned filename:

```text
light-gateway-<version>-linux-amd64.tar.gz
```

For customers with package-management standards, wrap the same layout in a
`.deb` or `.rpm` later. Start with `tar.gz` until the runtime contract is stable.

## Bootstrap Config

The local bootstrap config only needs enough information to reach config-server,
identify the gateway instance, and trust TLS.

Example `values.yml`:

```yaml
startup.host: customer.example.com
startup.timeout: 3000
startup.connectTimeout: 3000
startup.bootstrapCaCertPath: config/ca.pem

light-config-server-uri: https://config-server.customer.example.com:8435

server.serviceId: com.customer.mcp-gateway-1.0.0
server.environment: prod
server.ip: 0.0.0.0
server.advertisedAddress: mcp-gateway-01.customer.example.com
server.httpPort: 8080
server.enableHttp: true
server.httpsPort: 8443
server.enableHttps: false
server.enableRegistry: true
server.startOnRegistryFailure: true

portalRegistry.portalUrl: https://controller.customer.example.com:8438
```

`server.advertisedAddress` must be a stable address that controller and clients
can use to reach the VM gateway. Do not advertise `127.0.0.1` or `0.0.0.0`.

Example `startup.yml`:

```yaml
host: ${startup.host:dev.lightapi.net}
serviceId: ${server.serviceId:com.networknt.light-gateway-1.0.0}
envTag: ${server.environment:dev}
acceptHeader: application/yaml
timeout: ${startup.timeout:3000}
connectTimeout: ${startup.connectTimeout:3000}
configServerUri: ${light-config-server-uri:https://local.localhost}
authorization: ${light_portal_authorization:}
bootstrapCaCertPath: ${startup.bootstrapCaCertPath:config/ca.pem}
```

Example `server.yml`:

```yaml
ip: ${server.ip:0.0.0.0}
advertisedAddress: ${server.advertisedAddress:127.0.0.1}
httpPort: ${server.httpPort:8080}
enableHttp: ${server.enableHttp:true}
httpsPort: ${server.httpsPort:8443}
enableHttps: ${server.enableHttps:false}
tlsCertPath: ${server.tlsCertPath:}
tlsKeyPath: ${server.tlsKeyPath:}
serviceId: ${server.serviceId:com.networknt.light-gateway-1.0.0}
enableRegistry: ${server.enableRegistry:true}
startOnRegistryFailure: ${server.startOnRegistryFailure:true}
dynamicPort: ${server.dynamicPort:false}
environment: ${server.environment:dev}
shutdownGracefulPeriod: ${server.shutdownGracefulPeriod:2000}
```

Example `portal-registry.yml`:

```yaml
portalUrl: ${portalRegistry.portalUrl:https://localhost:8438}
portalToken: ${light_portal_authorization:}
controllerDiscoveryToken: ${portalRegistry.controllerDiscoveryToken:}
```

Example `client.yml` should include the customer CA path and hostname
verification policy for outbound HTTPS calls:

```yaml
tls:
  caCertPath: ${client.caCertPath:config/ca.pem}
  verifyHostname: ${client.verifyHostname:true}
```

Keep the full gateway behavior, including MCP routing, authentication, rule
configuration, and downstream MCP targets, in config-server. The VM should not
need local edits for normal policy or route changes.

## Secrets

Keep secrets in a root-owned environment file or in the customer's secret
manager. Do not pass secrets in command-line arguments.

Example `/etc/light-gateway/light-gateway.env`:

```sh
LIGHT_PORTAL_AUTHORIZATION=Bearer <token>
light_4j_config_password=<config-password-if-needed>
RUST_LOG=info
```

Permissions:

```sh
chown root:light-gateway /etc/light-gateway/light-gateway.env
chmod 0640 /etc/light-gateway/light-gateway.env
```

`LIGHT_PORTAL_AUTHORIZATION` is used for config-server bootstrap. The same token
is also used by portal registry startup when `portal-registry.yml` resolves
`portalToken` from `light_portal_authorization`.

## Systemd Unit

Example `/etc/systemd/system/light-gateway.service`:

```ini
[Unit]
Description=Light Gateway
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=light-gateway
Group=light-gateway
WorkingDirectory=/opt/light-gateway
EnvironmentFile=/etc/light-gateway/light-gateway.env
ExecStart=/opt/light-gateway/light-gateway
Restart=on-failure
RestartSec=5
LimitNOFILE=65535

NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=full
ProtectHome=true
ReadWritePaths=/var/lib/light-gateway/config-cache

[Install]
WantedBy=multi-user.target
```

Install and start:

```sh
systemctl daemon-reload
systemctl enable light-gateway
systemctl start light-gateway
systemctl status light-gateway
```

View logs:

```sh
journalctl -u light-gateway -f
```

## Install Script Scope

An install script is useful, but keep it deterministic and small.

It should:

- Create the `light-gateway` user and group.
- Create `/opt/light-gateway`, `/etc/light-gateway`, and
  `/var/lib/light-gateway/config-cache`.
- Install the binary with executable permissions.
- Install bootstrap config files.
- Install or update the `systemd` unit.
- Set file ownership and permissions.
- Print the next operator steps for adding secrets and starting the service.

It should not:

- Embed bearer tokens.
- Pass tokens to `ExecStart`.
- Rewrite customer config-server state.
- Start the process before secrets and CA files are installed.

## Startup Flow

The expected runtime flow is:

```text
systemd
  -> /opt/light-gateway/light-gateway
  -> read local config/values.yml and startup.yml
  -> call config-server with LIGHT_PORTAL_AUTHORIZATION
  -> write downloaded config and files into config-cache
  -> start Pingora with resolved runtime config
  -> register gateway to controller using portalRegistry.portalUrl
  -> route protected MCP traffic to downstream MCP servers
```

When `startup.yml` configures config-server, the runtime tries to download the
latest `values.yml` before starting. If that download fails for any reason, the
runtime continues startup with the available local and cached config, including
`config-cache/values.yml` when present.

## Upgrade And Rollback

Use versioned binary releases:

```text
/opt/light-gateway/releases/2.2.1/light-gateway
/opt/light-gateway/releases/2.2.2/light-gateway
/opt/light-gateway/light-gateway -> releases/2.2.2/light-gateway
```

Upgrade:

```sh
systemctl stop light-gateway
ln -sfn /opt/light-gateway/releases/2.2.2/light-gateway /opt/light-gateway/light-gateway
systemctl start light-gateway
```

Rollback:

```sh
systemctl stop light-gateway
ln -sfn /opt/light-gateway/releases/2.2.1/light-gateway /opt/light-gateway/light-gateway
systemctl start light-gateway
```

Do not delete `config-cache` during a normal binary rollback. It is the local
cache of the config-server-delivered runtime state.

## Validation Checklist

Before handing the VM to the customer:

- `systemctl status light-gateway` is active.
- `journalctl -u light-gateway` shows successful config-server bootstrap.
- `journalctl -u light-gateway` shows successful controller registration.
- The controller shows the gateway registered with the expected service id,
  environment, address, and port.
- The gateway health endpoint responds from the VM network.
- An MCP `tools/list` call reaches the gateway.
- An MCP `tools/call` call reaches the configured backend MCP server.
- Restarting the VM starts the gateway automatically.

## Security Checklist

- Store bearer tokens and config passwords outside the install bundle.
- Use a customer CA file instead of disabling TLS verification in production.
- Use a stable DNS name for `server.advertisedAddress`.
- Restrict inbound VM firewall rules to required gateway ports.
- Restrict outbound VM firewall rules to config-server, controller, and backend
  MCP server addresses.
- Run as the dedicated `light-gateway` user.
- Keep `/etc/light-gateway/light-gateway.env` readable only by root and the
  service group.
- Rotate `LIGHT_PORTAL_AUTHORIZATION` through the customer secret process.
