# Deploy Native

This page describes the recommended VM deployment model for the Rust
`light-agent` native binary.

Use this model when a customer wants to run an agent service on a VM and expose
the chat UI/WebSocket endpoint outside Kubernetes. The agent serves the local
chat UI, connects to an LLM provider, calls MCP tools through `light-gateway`,
stores conversation memory in Postgres, and registers with controller.

## Recommended Model

Deliver a versioned install bundle, not an ad hoc runtime script.

The bundle should contain:

- `light-agent` native binary.
- `public/` static assets for the chat UI.
- Minimal bootstrap config files.
- A `systemd` unit.
- An install script for filesystem setup.
- A root-owned environment file for secrets.

Use `systemd` to run the service:

- It restarts the process on failure.
- It keeps logs in the host journal.
- It avoids shell-history and process-list leakage from command-line secrets.
- It gives the customer a standard operational surface: `start`, `stop`,
  `restart`, `status`, and `journalctl`.

Do not use a long-running shell wrapper to pass the bootstrap token, database
URL, or model configuration. Use config files and an environment file instead.

## Runtime Layout

`light-agent` uses relative runtime paths:

- `config`
- `public`

The `systemd` service should therefore set `WorkingDirectory` to the installed
application directory.

Recommended VM layout:

```text
/opt/light-agent/
  light-agent -> releases/2.2.1/light-agent
  releases/
    2.2.1/
      light-agent
  config -> /etc/light-agent
  public/
    index.html

/etc/light-agent/
  startup.yml
  server.yml
  portal-registry.yml
  client.yml
  mcp-client.yml
  ollama.yml
  values.yml
  ca.pem
  light-agent.env

/var/lib/light-agent/
  config-cache/
```

The local `config` directory contains bootstrap and agent-specific config.
Runtime config downloaded from config-server should be written to
`/var/lib/light-agent/config-cache` by setting `externalConfigDir` in
`startup.yml`.

Keep `/etc/light-agent` readable by the service user. Keep
`/var/lib/light-agent/config-cache` writable by the service user.

## Build Artifact

Build a release binary from `light-fabric`:

```sh
cargo build --release -p light-agent
```

The artifact is:

```text
target/release/light-agent
```

For a static Linux build that matches the Docker build target:

```sh
rustup target add x86_64-unknown-linux-musl
cargo build --release -p light-agent --target x86_64-unknown-linux-musl
```

The static artifact is:

```text
target/x86_64-unknown-linux-musl/release/light-agent
```

Build on a compatible Linux distribution for the customer VM. If the customer
fleet has mixed Linux versions, prefer a static or target-compatible build so
the binary does not fail on an older `glibc`.

Package with a versioned filename:

```text
light-agent-<version>-linux-amd64.tar.gz
```

Include the static assets from:

```text
apps/light-agent/public/
```

## Runtime Dependencies

The VM must be able to reach:

- Controller, through `portalRegistry.portalUrl`.
- Config-server, through `startup.configServerUri`.
- `light-gateway`, through `mcp-client.gatewayUrl` and `mcp-client.path`.
- The model provider, currently Ollama by default.
- Postgres, through `DATABASE_URL`.

The Postgres database must contain the Hindsight memory tables used by
`light-agent`, including:

- `agent_memory_bank_t`
- `agent_memory_unit_t`
- `agent_session_history_t`

`LIGHT_AGENT_HOST_ID` must be a valid host UUID for the target tenant/host. The
agent stores memory and session history under this host id.

## Agent Roles

The same binary can run different logical agents. Use a different service id,
port, install directory, and `systemd` unit for each concurrently running role.

Common service ids are:

```text
com.networknt.agent.account-1.0.0
com.networknt.agent.advisor-1.0.0
com.networknt.agent.tech-support-1.0.0
```

For a single account agent, keep the service name `light-agent`. For multiple
agents on the same VM, use names such as:

```text
light-agent-account
light-agent-advisor
light-agent-tech-support
```

Each role needs a unique listener port if they run on the same VM.

## Bootstrap Config

The local bootstrap config needs enough information to reach config-server,
controller, `light-gateway`, Ollama, and Postgres.

Example `values.yml` for an account agent:

```yaml
startup.host: customer.example.com
startup.timeout: 3000
startup.connectTimeout: 3000
startup.bootstrapCaCertPath: config/ca.pem
startup.externalConfigDir: /var/lib/light-agent/config-cache

light-config-server-uri: https://config-server.customer.example.com:8435

server.serviceId: com.networknt.agent.account-1.0.0
server.environment: prod
server.ip: 0.0.0.0
server.advertisedAddress: agent-account-01.customer.example.com
server.httpPort: 8083
server.enableHttp: true
server.httpsPort: 8443
server.enableHttps: false
server.enableRegistry: true
server.startOnRegistryFailure: true

portalRegistry.portalUrl: https://controller.customer.example.com:8438

client.verifyHostname: true

mcp-client.gatewayUrl: https://mcp-gateway.customer.example.com
mcp-client.path: /mcp
mcp-client.timeoutMs: 5000

ollama.ollamaUrl: http://ollama.customer.example.com:11434
ollama.model: llama3.1:8b
```

`server.advertisedAddress` must be a stable address that controller and clients
can use to reach the VM agent. Do not advertise `127.0.0.1` or `0.0.0.0`.

Example `startup.yml`:

```yaml
host: ${startup.host:dev.lightapi.net}
serviceId: ${server.serviceId:com.networknt.agent.account-1.0.0}
envTag: ${server.environment:dev}
acceptHeader: application/yaml
timeout: ${startup.timeout:3000}
connectTimeout: ${startup.connectTimeout:3000}
configServerUri: ${light-config-server-uri:https://local.localhost}
authorization: ${light_portal_authorization:}
bootstrapCaCertPath: ${startup.bootstrapCaCertPath:config/ca.pem}
externalConfigDir: ${startup.externalConfigDir:/var/lib/light-agent/config-cache}
```

Example `server.yml`:

```yaml
ip: ${server.ip:0.0.0.0}
advertisedAddress: ${server.advertisedAddress:127.0.0.1}
httpPort: ${server.httpPort:8083}
enableHttp: ${server.enableHttp:true}
httpsPort: ${server.httpsPort:8443}
enableHttps: ${server.enableHttps:false}
serviceId: ${server.serviceId:com.networknt.agent.account-1.0.0}
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

Example `client.yml`:

```yaml
tls:
  verifyHostname: ${client.verifyHostname:true}
```

Example `mcp-client.yml`:

```yaml
gatewayUrl: ${mcp-client.gatewayUrl:https://mcp-gateway.customer.example.com}
path: ${mcp-client.path:/mcp}
timeoutMs: ${mcp-client.timeoutMs:5000}
```

Example `ollama.yml`:

```yaml
ollamaUrl: ${ollama.ollamaUrl:http://localhost:11434}
model: ${ollama.model:llama3.1:8b}
```

For the current `light-agent` implementation, keep `ollama.yml` and
`mcp-client.yml` in the local bootstrap config. They are read during process
initialization before the runtime completes remote config bootstrap.

## Secrets

Keep secrets in a root-owned environment file or in the customer's secret
manager. Do not pass secrets in command-line arguments.

Example `/etc/light-agent/light-agent.env`:

```sh
LIGHT_PORTAL_AUTHORIZATION=Bearer <token>
light_4j_config_password=<config-password-if-needed>
LIGHT_AGENT_HOST_ID=<host-uuid>
DATABASE_URL=postgres://agent_user:<password>@postgres.customer.example.com:5432/configserver
RUST_LOG=info
AGENT_LOG_ANSI=false
```

Permissions:

```sh
chown root:light-agent /etc/light-agent/light-agent.env
chmod 0640 /etc/light-agent/light-agent.env
```

`LIGHT_PORTAL_AUTHORIZATION` is used for config-server bootstrap and controller
registration. It is not the end-user chat token. If downstream MCP tools require
caller identity, the browser or BFF should send the user's `Authorization`
header to the agent WebSocket endpoint so the agent can forward it to
`light-gateway`.

## Systemd Unit

Example `/etc/systemd/system/light-agent.service`:

```ini
[Unit]
Description=Light Agent
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=light-agent
Group=light-agent
WorkingDirectory=/opt/light-agent
EnvironmentFile=/etc/light-agent/light-agent.env
ExecStart=/opt/light-agent/light-agent
Restart=on-failure
RestartSec=5
LimitNOFILE=65535

NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=full
ProtectHome=true
ReadWritePaths=/var/lib/light-agent/config-cache

[Install]
WantedBy=multi-user.target
```

Install and start:

```sh
systemctl daemon-reload
systemctl enable light-agent
systemctl start light-agent
systemctl status light-agent
```

View logs:

```sh
journalctl -u light-agent -f
```

## Install Script Scope

An install script is useful, but keep it deterministic and small.

It should:

- Create the `light-agent` user and group.
- Create `/opt/light-agent`, `/etc/light-agent`, and
  `/var/lib/light-agent/config-cache`.
- Install the binary with executable permissions.
- Install the `public/` static assets.
- Install bootstrap config files.
- Install or update the `systemd` unit.
- Set file ownership and permissions.
- Print the next operator steps for adding secrets and starting the service.

It should not:

- Embed bearer tokens.
- Pass tokens to `ExecStart`.
- Rewrite customer config-server state.
- Start the process before secrets, CA files, and database access are ready.

## Startup Flow

The expected runtime flow is:

```text
systemd
  -> /opt/light-agent/light-agent
  -> read local config/values.yml, ollama.yml, and mcp-client.yml
  -> connect to Postgres with DATABASE_URL
  -> build the MCP client for light-gateway
  -> call config-server with LIGHT_PORTAL_AUTHORIZATION
  -> write downloaded runtime config into /var/lib/light-agent/config-cache
  -> start the Axum HTTP/WebSocket server
  -> register the agent with controller using portalRegistry.portalUrl
  -> serve the chat UI from public/
  -> forward tool discovery and tool calls to light-gateway
```

When `startup.yml` configures config-server, the runtime tries to download the
latest `values.yml` before starting. If that download fails for any reason, the
runtime continues startup with the available local and cached config, including
`config-cache/values.yml` when present.

## Endpoints

The native service exposes:

```text
GET /health
GET /
GET /chat
```

`/chat` upgrades to WebSocket. The static chat UI is served from `public/`.

For local testing on the VM:

```sh
curl -i http://127.0.0.1:8083/health
```

## Upgrade And Rollback

Use versioned binary releases:

```text
/opt/light-agent/releases/2.2.1/light-agent
/opt/light-agent/releases/2.2.2/light-agent
/opt/light-agent/light-agent -> releases/2.2.2/light-agent
```

Upgrade:

```sh
systemctl stop light-agent
ln -sfn /opt/light-agent/releases/2.2.2/light-agent /opt/light-agent/light-agent
systemctl start light-agent
```

Rollback:

```sh
systemctl stop light-agent
ln -sfn /opt/light-agent/releases/2.2.1/light-agent /opt/light-agent/light-agent
systemctl start light-agent
```

Do not delete `config-cache` during a normal binary rollback. It is the local
cache of the config-server-delivered runtime state.

## Validation Checklist

Before handing the VM to the customer:

- `systemctl status light-agent` is active.
- `journalctl -u light-agent` shows successful config-server bootstrap.
- `journalctl -u light-agent` shows successful controller registration.
- The controller shows the agent registered with the expected service id,
  environment, address, and port.
- `curl http://127.0.0.1:8083/health` returns `200 OK`.
- The chat UI loads from the VM address.
- The chat WebSocket connects to `/chat`.
- Logs show that the agent can connect to Postgres.
- Logs do not show MCP `tools/list` failures from `light-gateway`.
- A chat request can discover and call a tool through `light-gateway`.
- Restarting the VM starts the agent automatically.

## Security Checklist

- Store bearer tokens, config passwords, and database passwords outside the
  install bundle.
- Use a customer CA file instead of disabling TLS verification in production.
- Use a stable DNS name for `server.advertisedAddress`.
- Restrict inbound VM firewall rules to the required agent port.
- Restrict outbound VM firewall rules to config-server, controller,
  `light-gateway`, Ollama, and Postgres.
- Run as the dedicated `light-agent` user.
- Keep `/etc/light-agent/light-agent.env` readable only by root and the service
  group.
- Keep `/etc/light-agent` writable only by administrators.
- Keep only `/var/lib/light-agent/config-cache` writable by the service.
- Rotate `LIGHT_PORTAL_AUTHORIZATION` through the customer secret process.
