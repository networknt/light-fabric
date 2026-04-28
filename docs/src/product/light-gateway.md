# Light-Gateway

`light-gateway` is the Pingora-based gateway product in Light Fabric.

It is intended to host gateway behavior such as routing, proxying, and
eventually AI/MCP gateway integrations while using the shared runtime and config
model.

## Key Dependencies

- `light-runtime`
- `light-pingora`
- `config-loader`

## Runtime

The gateway uses `light-pingora` as its transport framework and
`light-runtime` for lifecycle, bootstrap, and service configuration.
