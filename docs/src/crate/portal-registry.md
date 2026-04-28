# Portal Registry

`portal-registry` provides client support for registering services with Light
Portal or Light Controller.

It uses a JSON-RPC style WebSocket protocol for service registration, metadata
updates, and skill search. Runtime services normally use this through
`light-runtime`, but applications can also use the client directly when they
need custom registry behavior.

## Main Types

- `PortalRegistryClient`: WebSocket client for registry communication.
- `RegistryHandler`: trait for handling registry callbacks and messages.
- `RegistrationState`: client registration state.
- `RegistrationBuilder`: helper for constructing registration parameters.
- `ServiceRegistrationParams`: service identity and advertised endpoint.
- `ServiceMetadataUpdate`: metadata update payload.
- `SkillSearchRequest`, `SkillSearchResponse`: skill discovery messages.

## Usage

```rust
use portal_registry::RegistrationBuilder;

let registration = RegistrationBuilder::new(
    "com.networknt.service-1.0.0",
    "1.0.0",
    "http",
    "127.0.0.1",
    8080,
)
.with_env("DEV")
.with_jwt(token)
.build();
```

## Runtime Integration

`light-runtime` can register a service automatically when `server.yml` enables
registry support and `portal-registry.yml` supplies the portal connection.
