# Light Runtime

`light-runtime` is the shared service runtime for Light Fabric applications.

It owns bootstrap, configuration loading, transport startup, graceful shutdown,
and optional portal registry registration. Apps such as `light-agent` and
`light-deployer` should start through this crate instead of binding sockets
directly.

## Main Types

- `LightRuntimeBuilder`: builds a runtime from a transport.
- `LightRuntime`: configured runtime before start.
- `RunningRuntime`: running service handle with shutdown support.
- `Module`: lifecycle hook abstraction.
- `RuntimeConfig`: resolved runtime configuration.
- `ServerConfig`: HTTP/HTTPS bind and service identity settings.
- `BootstrapConfig`: remote config bootstrap settings.
- `PortalRegistryConfig`: portal registry connection settings.

## Startup Pattern

```rust
use light_axum::AxumTransport;
use light_runtime::LightRuntimeBuilder;

let runtime = LightRuntimeBuilder::new(AxumTransport::new(app))
    .with_config_dir("config")
    .build();

let running = runtime.start().await?;
running.shutdown().await?;
```

## Configuration

At minimum, runtime services need `server.yml`. Optional files include
`startup.yml`, `client.yml`, and `portal-registry.yml`.

## Related Frameworks

`light-runtime` is transport-neutral. `light-axum` supplies the Axum transport
implementation.
