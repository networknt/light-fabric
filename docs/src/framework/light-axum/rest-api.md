# Building REST APIs With Light-Axum

`light-axum` lets a service use normal Axum routing while delegating listener
binding, TLS, config loading, runtime metadata, logging control, and graceful
shutdown to `light-runtime`.

The application owns the HTTP API shape. The framework owns how the service is
started and managed.

## Working Examples

The `light-example-rs` repository contains two REST API demos built with
`light-axum`:

| Demo | Purpose | Local port | OpenAPI |
|------|---------|------------|---------|
| `apps/demo-customer-profile-api` | Customer profile, preferences, policies, vehicles, and prior claims | `8085` | `apps/demo-customer-profile-api/openapi.yaml` |
| `apps/demo-offer-decision-api` | Offer search, offer decisions, claim triage, and settlement recommendations | `8086` | `apps/demo-offer-decision-api/openapi.yaml` |

Both demos follow the same service pattern:

1. Define request and response models with `serde`.
2. Build a standard Axum `Router`.
3. Implement `AxumApp` and return that router.
4. Start the app through `LightRuntimeBuilder::new(AxumTransport::new(app))`.
5. Keep runtime config in the app `config/` directory.
6. Publish an OpenAPI document for endpoint import and API management.

## Dependencies

A minimal REST API needs these crates:

```toml
[dependencies]
anyhow = { workspace = true }
async-trait = { workspace = true }
axum = { workspace = true }
light-axum = { workspace = true }
light-runtime = { workspace = true }
serde = { workspace = true }
tokio = { workspace = true }
tracing = { workspace = true }
```

Add `serde_json` when handlers accept or return dynamic JSON values.

## Application Shape

Create a service type and implement `AxumApp`. The runtime passes a
`ServerContext` into `router`. Most simple REST APIs do not need it, but it is
available when routes need runtime metadata.

```rust
use async_trait::async_trait;
use axum::{Json, Router, routing::get};
use light_axum::{AxumApp, ServerContext};
use light_runtime::RuntimeError;
use serde::Serialize;

#[derive(Clone, Default)]
struct CustomerProfileApp;

#[async_trait]
impl AxumApp for CustomerProfileApp {
    async fn router(&self, _context: ServerContext) -> Result<Router, RuntimeError> {
        Ok(build_router())
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct HealthResponse {
    status: &'static str,
    service: &'static str,
}

fn build_router() -> Router {
    Router::new().route("/health", get(health))
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "UP",
        service: "demo-customer-profile-api",
    })
}
```

Everything inside `build_router` is standard Axum. Use `Path`, `Query`,
`State`, `Json`, `HeaderMap`, middleware, extractors, and response types the
same way you would in a standalone Axum service.

## Runtime Startup

Start the service through `LightRuntimeBuilder` instead of binding a
`TcpListener` directly.

```rust
use anyhow::{Context, Result};
use light_axum::AxumTransport;
use light_runtime::{LightRuntimeBuilder, TracingOptions, init_tracing};
use tracing::info;

const CONFIG_DIR_ENV: &str = "CUSTOMER_PROFILE_CONFIG_DIR";
const EXTERNAL_CONFIG_DIR_ENV: &str = "CUSTOMER_PROFILE_EXTERNAL_CONFIG_DIR";
const LOG_ANSI_ENV: &str = "CUSTOMER_PROFILE_LOG_ANSI";
const DEFAULT_CONFIG_DIR: &str = "apps/demo-customer-profile-api/config";
const DEFAULT_EXTERNAL_CONFIG_DIR: &str =
    "apps/demo-customer-profile-api/config-cache";

#[tokio::main]
async fn main() -> Result<()> {
    let tracing_guard = init_tracing(
        TracingOptions::new("demo-customer-profile-api")
            .with_legacy_ansi_env(LOG_ANSI_ENV),
    )
    .context("failed to initialize tracing")?;

    let config_dir =
        std::env::var(CONFIG_DIR_ENV).unwrap_or_else(|_| DEFAULT_CONFIG_DIR.to_string());
    let external_config_dir = std::env::var(EXTERNAL_CONFIG_DIR_ENV)
        .unwrap_or_else(|_| DEFAULT_EXTERNAL_CONFIG_DIR.to_string());

    let runtime = LightRuntimeBuilder::new(AxumTransport::new(CustomerProfileApp))
        .with_config_dir(config_dir)
        .with_external_config_dir(external_config_dir)
        .with_logging_control(tracing_guard.logging_control())
        .build();

    let running = runtime
        .start()
        .await
        .context("failed to start demo customer profile API")?;

    info!("demo customer profile API started");

    tokio::signal::ctrl_c()
        .await
        .context("failed to listen for shutdown signal")?;

    running
        .shutdown()
        .await
        .context("failed to shut down demo customer profile API")?;

    Ok(())
}
```

This startup path gives the application the same runtime behavior as other
Light services:

- listener configuration comes from `server.yml`
- local and external config directories are resolved by `light-runtime`
- TLS is controlled by runtime config, not by route code
- graceful shutdown goes through the runtime handle
- logging can be controlled by the runtime logging control object

## Routing Patterns

The customer profile demo shows read-only REST endpoints:

```rust
fn build_router() -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/customers/{customer_id}", get(get_customer))
        .route(
            "/customers/{customer_id}/preferences",
            get(get_customer_preferences),
        )
        .route(
            "/customers/{customer_id}/policies",
            get(get_customer_policies),
        )
        .route(
            "/customers/{customer_id}/vehicles/{vehicle_id}",
            get(get_covered_vehicle),
        )
        .route(
            "/customers/{customer_id}/prior-claims",
            get(get_prior_claims),
        )
        .with_state(AppState::seeded())
}
```

The offer decision demo shows query parameters, request bodies, headers, and
shared mutable state:

```rust
fn build_router() -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/offers", get(search_offers))
        .route("/offer-decisions", post(record_offer_decision))
        .route("/claim-triage", post(triage_claim))
        .route("/settlement-recommendations", post(recommend_settlement))
        .with_state(AppState::seeded())
}
```

Use typed handlers for predictable API behavior:

```rust
async fn search_offers(
    State(state): State<AppState>,
    Query(query): Query<OfferQuery>,
) -> Json<Vec<Offer>> {
    Json(state.search_offers(&query))
}
```

For request bodies:

```rust
async fn record_offer_decision(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<OfferDecisionRequest>,
) -> Result<Json<OfferDecisionResponse>, ApiError> {
    // validate request and return a typed API response
}
```

## Errors

Define one service error type and implement `IntoResponse`. This keeps handlers
small and ensures failures return stable JSON.

```rust
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ErrorResponse {
    code: &'static str,
    message: String,
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorResponse {
                code: self.code,
                message: self.message,
            }),
        )
            .into_response()
    }
}
```

The customer profile API returns `404` with `CUSTOMER_NOT_FOUND`. The offer
decision API returns `400` with `INVALID_DECISION_REQUEST` when request content
is invalid.

## Configuration

Each application should keep its runtime configuration under its own `config/`
directory:

```text
apps/<service-name>/
  Cargo.toml
  openapi.yaml
  src/main.rs
  config/
    client.yml
    portal-registry.yml
    server.yml
    startup.yml
    values.yml
```

`server.yml` controls the listener and service identity:

```yaml
ip: ${server.ip:0.0.0.0}
advertisedAddress: ${server.advertisedAddress:127.0.0.1}
httpPort: ${server.httpPort:8085}
enableHttp: ${server.enableHttp:true}
httpsPort: ${server.httpsPort:8443}
enableHttps: ${server.enableHttps:false}
tlsCertPath: ${server.tlsCertPath:}
tlsKeyPath: ${server.tlsKeyPath:}
serviceId: ${server.serviceId:com.networknt.demo.customer-profile-1.0.0}
enableRegistry: ${server.enableRegistry:false}
startOnRegistryFailure: ${server.startOnRegistryFailure:true}
dynamicPort: ${server.dynamicPort:false}
environment: ${server.environment:demo}
shutdownGracefulPeriod: ${server.shutdownGracefulPeriod:2000}
```

Use a unique `server.serviceId` per service. The demos use:

- `com.networknt.demo.customer-profile-1.0.0`
- `com.networknt.demo.offer-decision-1.0.0`

`values.yml` supplies defaults for template variables:

```yaml
server.serviceId: com.networknt.demo.customer-profile-1.0.0
server.environment: demo
server.ip: 0.0.0.0
server.advertisedAddress: 127.0.0.1
server.httpPort: 8085
server.enableHttp: true
server.enableHttps: false
server.enableRegistry: false
server.startOnRegistryFailure: true
```

Enable registry integration only when the service should register with Portal
or Controller discovery. For local standalone development, keep
`server.enableRegistry: false`.

## OpenAPI

Keep the OpenAPI document beside the service source. The OpenAPI file is the
contract used by API management and workflow tooling, while `src/main.rs` is the
runtime implementation.

The demo specs are:

- `light-example-rs/apps/demo-customer-profile-api/openapi.yaml`
- `light-example-rs/apps/demo-offer-decision-api/openapi.yaml`

When adding or changing a route, update both the Axum router and `openapi.yaml`.
Use operation IDs that match the business action, such as
`getCustomerProfile`, `searchOffers`, or `recordOfferDecision`.

## Running Locally

From the `light-example-rs` repository:

```bash
cargo run -p demo-customer-profile-api
```

Then verify the health endpoint:

```bash
curl http://127.0.0.1:8085/health
```

Run the offer decision API the same way:

```bash
cargo run -p demo-offer-decision-api
curl http://127.0.0.1:8086/health
```

Override config locations with environment variables when running from a
different working directory:

```bash
CUSTOMER_PROFILE_CONFIG_DIR=/path/to/config \
CUSTOMER_PROFILE_EXTERNAL_CONFIG_DIR=/path/to/config-cache \
cargo run -p demo-customer-profile-api
```

## Checklist

Use this checklist when creating a new REST API with `light-axum`:

- create an app crate under `apps/`
- add `axum`, `light-axum`, `light-runtime`, `tokio`, `serde`, and
  `async-trait`
- define typed request, response, and error models
- build routes with a standard Axum `Router`
- implement `AxumApp` for the service type
- start the service with `LightRuntimeBuilder` and `AxumTransport`
- add `config/server.yml`, `startup.yml`, `portal-registry.yml`, `client.yml`,
  and `values.yml`
- assign a stable `server.serviceId`
- add or update `openapi.yaml`
- verify `/health` and one representative business endpoint locally
