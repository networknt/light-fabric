# Light-Deployer

`light-deployer` is the cluster-local Kubernetes deployment executor for Light
Portal.

It renders Kubernetes templates, validates manifests, applies resources through
`kube-rs`, reports rollout status, and exposes deployment tools through an MCP
JSON-RPC endpoint for local and MicroK8s testing.

## Key Capabilities

- MCP JSON-RPC endpoint at `POST /mcp`
- AST-based YAML template rendering
- Git template fetching with `gix`
- Kubernetes dry-run, apply, delete, status, and prune
- redacted manifest summaries and diffs
- SSE deployment events

## Runtime

`light-deployer` uses `light-runtime`, `light-axum`, `config-loader`, and
`portal-registry` so it follows the same service boot model as `light-agent`.

## Testing Path

Use these pages in order when testing locally:

1. [Build Local](light-deployer/build-local.md)
2. [Prepare Config](light-deployer/prepare-config.md)
3. [Run Standalone](light-deployer/run-standalone.md)
4. [Run Kubernetes](light-deployer/run-kubernetes.md)

Start with standalone `noop` mode to validate template rendering. Then move to
MicroK8s real mode once the render request and target templates are correct.

For MCP clients, Light Portal, and AI agents, use `POST /mcp` with JSON-RPC
methods such as `tools/list` and `tools/call`. The `/mcp/tools/*` routes are
kept only as local debugging conveniences.
