# Light-Agent

`light-agent` is the interactive agent service in Light Fabric.

It provides a WebSocket chat interface, integrates with model providers,
invokes MCP tools through `mcp-client`, and stores conversation memory through
`hindsight-client`.

## Key Dependencies

- `light-runtime`
- `light-axum`
- `model-provider`
- `mcp-client`
- `hindsight-client`
- `portal-registry`

## Runtime

The app follows the standard runtime pattern:

- load config from `config/`
- implement an Axum app
- start through `LightRuntimeBuilder`
- optionally register through portal registry
