# Light-Axum

`light-axum` adapts Axum applications to `light-runtime`.

Applications implement `AxumApp` and return an `axum::Router`. The framework
owns binding, optional TLS, runtime metadata resolution, and graceful shutdown
through the runtime transport contract.

## Main Types

- `AxumApp`: trait implemented by an application.
- `AxumTransport`: transport passed to `LightRuntimeBuilder`.
- `ServerContext`: runtime context passed into the app when building routes.
- `AxumBoundHandle`: running Axum server handle.

## Pattern

```rust
use light_axum::{AxumApp, AxumTransport, ServerContext};
use light_runtime::LightRuntimeBuilder;

#[derive(Clone)]
struct App;

impl AxumApp for App {
    fn router(&self, _context: ServerContext) -> axum::Router {
        axum::Router::new()
    }
}

let runtime = LightRuntimeBuilder::new(AxumTransport::new(App))
    .with_config_dir("config")
    .build();
```

## Consumers

`light-agent` and `light-deployer` use this framework.
