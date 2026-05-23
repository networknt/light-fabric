# Embedded Configuration Templates

## Status

Initial implementation completed. Rust applications in `light-fabric` and
related `portal-service` applications keep template configuration files under
each app's `config` directory. Container images may copy those files into
`/app/config-defaults`, then runtime overlays local `config`, downloaded
`config-cache`, remote `values.yml`, and environment variables.

That works well for container deployments. It is awkward for native binary
deployments on a VM because the operator must copy a full template directory
beside the binary even when they only want to provide `values.yml`, certs, or a
small local override.

This design embeds the template files into the Rust binary while keeping the
app `config` directories in source control as the readable template source.

## Purpose

Embedded configuration templates should make the Rust deployment model match
the Java module model more closely:

1. The application binary carries its default template files.
2. Operators provide only overrides, usually `values.yml`, `startup.yml`, certs,
   keys, or environment variables.
3. Config-server can still return `values.yml` after bootstrap, plus external
   files for explicit migration or operational exceptions.
4. Developers and operators can still inspect the app's `config` directory in
   source control to learn supported properties.

The embedded files are defaults. They are not runtime state and should not be
written out automatically unless an explicit diagnostic/export command is added
later.

## Current Model

The current runtime model has these filesystem layers:

| Layer | Example | Purpose |
| --- | --- | --- |
| Default templates | `config-defaults/server.yml` | App-provided templates copied into the container image |
| Local config | `config/values.yml`, `config/startup.yml` | Operator overrides and bootstrap inputs |
| External/cache config | `config-cache/values.yml` | Files downloaded from config-server |
| Remote values | config-server response body | Runtime values fetched during bootstrap |
| Environment variables | `CLIENT_VERIFYHOSTNAME=false` | Last-mile process overrides during placeholder expansion |

For `light-fabric` runtime applications, `LightRuntimeBuilder` passes
`default_config_dir`, `config_dir`, and `external_config_dir` into
`light-runtime`. `load_bootstrap_config()` reads bootstrap-time `values.yml`,
`startup.yml`, and `client.yml` before remote config-server bootstrap. After
remote bootstrap, runtime config loads `server.yml`, `client.yml`,
`portal-registry.yml`, and framework/application module files through the same
merged configuration path.

Some `portal-service` apps share the `light-runtime` path, while standalone apps
such as `config-server` and `light-oauth` have local helper functions that merge
`config-defaults` and `config`.

## Goals

- Allow a native binary deployment to start with embedded templates and a small
  external `config/values.yml`.
- Keep `apps/<app>/config/*.yml` as the source of truth for template content.
- Keep container deployment behavior compatible with the current
  `/app/config-defaults` copy.
- Preserve the existing overlay order and placeholder expansion behavior.
- Support bootstrap-time files such as `startup.yml` and `client.yml`.
- Support runtime module files such as `handler.yml`, `proxy.yml`,
  `model-provider.yml`, provider configs, and product-specific files.
- Provide one reusable loading abstraction for `light-fabric` and
  `portal-service` instead of app-specific parsing logic.
- Avoid writing embedded templates to disk during normal startup.

## Non-Goals

- Do not embed secrets, certificates, private keys, trust bundles, static web
  assets, or downloaded config-server files.
- Do not remove the source `config` directories. They remain the reviewable,
  documented template source.
- Do not make `values.yml` mandatory. Apps should keep current defaults where
  they are already valid.
- Do not make config-server responsible for delivering template files that are
  already part of the binary.
- Do not change the meaning of `values.yml` placeholders or environment
  variable expansion.

## Proposed Layer Order

The new effective source order should be:

1. Embedded template file from the binary.
2. Filesystem default template from `config-defaults`, if present.
3. Local operator file from `config`.
4. External/cache file from `config-cache`, when runtime loading supports it.
5. Remote `values.yml` payload from config-server.
6. Environment variables during placeholder resolution.

This keeps existing container images compatible. If `config-defaults` exists, it
can override the embedded template. That gives operators and image builders a
transition path and a deliberate escape hatch for patched images.

For native binary deployment, `config-defaults` is simply absent and the binary
falls back to embedded templates.

Structured config files and `values.yml` should use different overlay
semantics:

| File type | Semantics | Reason |
| --- | --- | --- |
| Structured config files such as `server.yml`, `handler.yml`, `proxy.yml`, and `model-provider.yml` | Source-level override. The highest-priority source that contains the file supplies the whole template. | Avoids surprising hybrid files assembled from embedded, image, local, and cache layers. Operators should use `values.yml` for partial property overrides. |
| `values.yml` | Key-level overlay in source order, followed by remote values and environment variables. | `values.yml` is explicitly the property override surface. Partial overlays are expected and useful. |

After the structured file source is selected, placeholders in that file are
resolved from the merged values map and environment variables.

## Embedded Template Representation

`include_dir` is a possible embedding mechanism. It embeds the entire app
`config` directory at compile time and avoids custom directory-scanning build
scripts in every application crate:

```rust
use include_dir::{include_dir, Dir};

pub static EMBEDDED_CONFIG: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/config");
```

The runtime should hide the concrete embedding mechanism behind a small config
source abstraction. A typed file representation is still useful as the stable
runtime boundary:

```rust
pub struct EmbeddedConfigFile {
    pub name: &'static str,
    pub content: &'static str,
}
```

Application code should pass a flattened static file list into the runtime:

```rust
LightRuntimeBuilder::new(transport)
    .with_embedded_config(embedded_config::FILES)
    .build();
```

`include_str!` is still acceptable for one or two files, but application
`main.rs` files should not accumulate hand-maintained `include_str!` lists.
`include_bytes!` is not preferred for YAML templates because configuration
templates should be valid UTF-8 before they are parsed.

The initial implementation uses a shared build-time generator instead of adding
an external embedding dependency. Each app has a small `build.rs` that calls
`config-embed-build`, which scans the committed `config` directory and produces
a manifest like this under `OUT_DIR`:

```rust
pub const FILES: &[config_loader::EmbeddedConfigFile] = &[
    config_loader::EmbeddedConfigFile {
        name: "server.yml",
        content: include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/config/server.yml")),
    },
    config_loader::EmbeddedConfigFile {
        name: "startup.yml",
        content: include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/config/startup.yml")),
    },
];
```

## Build-Time Generation Fallback

The project currently uses the build-time manifest path. Each app uses a shared
`build.rs` helper to scan its `config` directory and generate the embedded
manifest. The generator lives in one reusable crate so apps do not carry
duplicated build logic.

The generated manifest should:

- Include only known text config extensions, initially `.yml`, `.yaml`,
  `.json`, and `.toml`.
- Preserve the file name relative to the app `config` directory.
- Emit `cargo:rerun-if-changed=config`.
- Fail the build if a template file cannot be read as UTF-8.

Nested config paths are not needed for current app templates, but the manifest
should allow names such as `oauth/server.yml` if a future product needs them.

## Runtime API

Add embedded defaults to `LightRuntimeBuilder`:

```rust
LightRuntimeBuilder::new(transport)
    .with_embedded_config(embedded_config::FILES)
    .with_default_config_dir(DEFAULT_CONFIG_DIR)
    .with_config_dir(CONFIG_DIR)
    .with_external_config_dir(EXTERNAL_CONFIG_DIR)
    .build();
```

`RuntimeConfig` should carry the embedded source as skipped runtime state, the
same way it carries `default_config_dir` and registries today:

```rust
pub struct RuntimeConfig {
    // existing fields
    #[serde(skip, default)]
    pub embedded_config: &'static [EmbeddedConfigFile],
}
```

The stable contract is lookup by relative file name and iteration for diagnostics
or dumping. The concrete representation can remain a static file slice or later
move behind a provider abstraction if needed.

The low-level loader should accept named in-memory content as another config
source:

```rust
pub enum ConfigSource {
    Embedded { name: &'static str, content: &'static str },
    File(PathBuf),
}
```

`ConfigLoader` can then parse embedded and filesystem sources with the same
YAML/JSON/TOML parser. Structured config loading should select the highest
priority source for the requested file. `values.yml` loading should continue to
merge maps in source order.

## Bootstrap Behavior

Bootstrap must support embedded templates because this is the path that native
deployments need most.

`load_bootstrap_values()` should merge:

1. Embedded `values.yml`, if present.
2. `config-defaults/values.yml`, if present.
3. `config/values.yml`, if present.

`load_bootstrap_config()` should load `startup.yml` and `client.yml` from:

1. Embedded templates.
2. `config-defaults`.
3. `config`.

For `startup.yml` and `client.yml`, the highest-priority source that contains
the file should be used as the full template. Placeholder resolution still uses
the merged bootstrap values.

After bootstrap fetches remote values, `load_values_map()` should merge embedded
`values.yml` before the existing file and remote layers. This allows remote
values to override embedded placeholders exactly as they override copied
template files today.

## Application Integration

### Light-Gateway

`light-gateway` should be the first `light-fabric` application to adopt the
runtime API because it has the richest template set:

- bootstrap and server files
- client and portal registry files
- handler chain files
- proxy, resource, MCP, websocket, auth, token, metrics, and rule-related files

After integration, a native gateway deployment can run with the binary plus a
small `config/values.yml` and any required cert/key files.

### Light-Agent

`light-agent` should use the same runtime API for all provider templates. The
embedded set should include `model-provider.yml`, `mcp-client.yml`, and every
provider-specific template such as `openai.yml`, `bedrock.yml`, `codex.yml`,
`anthropic.yml`, and `ollama.yml`.

Runtime provider selection should still happen after bootstrap. Embedded
templates do not mean provider clients are created before config-server values
are loaded.

### Light-Deployer

`light-deployer` currently has a separate app-level config load for
`deployer.yml`. It should either move to the shared embedded-source helper or
set embedded defaults on `LightRuntimeBuilder` and use the same merged source
logic for its application config.

### Portal-Service App

`portal-service/apps/portal-service` already uses `LightRuntimeBuilder`, but it
loads `portal-service.yml` before runtime startup to create the database pool.
That pre-runtime load should use the same shared embedded-source helper.

The `portal-service.yml` config remains non-reloadable because `dbUrl` and
`hostId` feed process-owned state.

### Portal-Service Config-Server And Light-OAuth

`portal-service/apps/config-server` and `apps/light-oauth` do not bootstrap from
config-server. They should still embed their `server.yml` templates so native
deployment does not require a copied `config-defaults` directory.

Because these apps have local merge helpers today, they should consume a shared
`config-loader` helper that can merge:

1. Embedded defaults.
2. Filesystem defaults.
3. Local config.

This keeps their behavior aligned with `light-runtime` without requiring them
to become runtime-bootstrap applications.

## Operator Model

For a native deployment, the recommended layout becomes:

```text
/opt/light-gateway/
  light-gateway
  config/
    values.yml
    startup.yml        # optional, only when values/env defaults are not enough
    cert.pem           # optional external asset
    key.pem            # optional external asset
```

The operator no longer needs to copy every template file beside the binary.
They only provide files that are deployment-specific.

For a container deployment, the current layout continues to work:

```text
/app/light-gateway
/app/config-defaults/*.yml
/config/values.yml
/app/config-cache/values.yml
```

In the long term, the `/app/config-defaults` copy can become optional. Keeping it
during migration is useful because it lets operators inspect templates inside
the image and provides a familiar override layer.

After embedded templates are stable across production deployments, Docker images
should deprecate and then remove the unconditional `/app/config-defaults` copy.
Template inspectability should move to explicit dump/print commands rather than
extra image layers.

## Diagnostics

The runtime should expose enough information to make source precedence clear:

- Log whether embedded templates were registered for the application.
- When a required config file is missing, include the searched source names:
  embedded, `config-defaults`, `config`, and `config-cache`.
- Module registry snapshots should show the resolved config, not the raw
  embedded template.
- Module registry metadata should include config source provenance when
  available, for example `embedded`, `file:/app/config-defaults/server.yml`, or
  `file:/config/server.yml`.
- Normal startup should not write embedded templates to disk.

Native operators should have explicit inspection commands:

```text
light-gateway --print-default-config server.yml
light-gateway --dump-default-configs ./config-defaults
```

The print command writes one embedded template to stdout. The dump command
writes all embedded templates to a target directory so operators can inspect,
copy, and customize them.

## Controller Server Info Compatibility

Rust services register with the controller, and the controller can call the
runtime MCP service-info path to inspect runtime configuration. This behavior
must continue to work with embedded templates.

The service-info response should expose resolved runtime configuration, not raw
templates. The implementation contract is:

1. Select the effective structured config source, such as embedded
   `server.yml`, filesystem `config/server.yml`, or cached `config-cache`
   `server.yml`.
2. Build the merged values map from embedded, filesystem, cached, remote
   `values.yml`, and environment variables.
3. Resolve placeholders in the selected config source.
4. Deserialize the resolved config into the typed runtime or module config.
5. Register that typed config in `ModuleRegistry`.
6. Return `ModuleRegistry` component configs from the controller service-info
   MCP call.

With that flow, the controller still sees every registered config file with
defaults and overrides applied. Embedded templates only replace the missing
filesystem default-template layer. They should not bypass typed config loading,
masking, module registration, reload validation, or service-info reporting.

Source provenance can be added as metadata beside each registered config, but it
must not replace the resolved config payload that operators and the controller
depend on.

## Testing Strategy

Add unit tests at the shared loader boundary:

- Embedded-only `server.yml` loads successfully.
- Local `config/server.yml` replaces embedded `server.yml` rather than deep
  merging with it.
- `config-defaults/server.yml` replaces embedded `server.yml`.
- `config-cache/server.yml` replaces local config during runtime loads.
- Embedded `values.yml` is overridden by local `values.yml`.
- Remote `values.yml` overrides embedded and filesystem values.
- Missing required config reports all searched layers.
- Source provenance is recorded for resolved module configs.
- `--print-default-config` and `--dump-default-configs` expose embedded
  templates without changing normal startup behavior.
- Controller service-info output includes resolved values from embedded defaults
  plus local, cached, remote, and environment overrides.

Add application-level smoke tests for:

- `light-gateway` startup with no filesystem `server.yml`, using embedded
  templates plus local `values.yml`.
- `light-agent` provider config loading from embedded templates after bootstrap.
- `portal-service/apps/portal-service` pre-runtime `portal-service.yml` load
  from embedded templates.
- `portal-service/apps/config-server` standalone `server.yml` load from embedded
  templates.

## Migration Plan

1. Add embedded source support to `config-loader` and `light-runtime`.
2. Add shared build-time template embedding for `light-gateway`.
3. Wire `light-gateway` to pass embedded templates to `LightRuntimeBuilder`.
4. Keep Docker `config-defaults` copies unchanged and verify container parity.
5. Add native startup tests that run without a copied template directory.
6. Roll the same pattern to `light-agent` and `light-deployer`.
7. Add the shared embedded-source helper to `portal-service` and migrate
   `portal-service`, `config-server`, and `light-oauth`.
8. Add print and dump commands for embedded templates.
9. After several releases, deprecate Docker `config-defaults` copies and rely on
   embedded defaults plus explicit dump commands for inspectability.

## Risks And Mitigations

| Risk | Mitigation |
| --- | --- |
| Embedded templates drift from source templates | Embed the committed `config/` directory directly with `include_dir`, or generate a manifest from that directory at build time |
| Operators cannot inspect templates in native deployment | Keep source templates in repo and add print/dump commands for embedded templates |
| Docker behavior changes unexpectedly | Keep `config-defaults` above embedded defaults during migration |
| Config-server remote values stop overriding defaults | Preserve remote values as the highest non-env value layer |
| Apps duplicate merge logic | Move embedded-source merging into shared loader/runtime helpers |
| Secrets accidentally embedded | Embed only committed template files and keep secrets in values, env, or external files |
| Structured config becomes hard to reason about | Use source-level override for config files and reserve key-level merging for `values.yml` |

## Resolved Decisions

- Native operators should get `--print-default-config <name>` and
  `--dump-default-configs <directory>` commands.
- Module registry should expose resolved config first, with source provenance as
  metadata when available.
- Docker images should keep `/app/config-defaults` during migration, then
  deprecate it once embedded templates and dump commands are stable.
- Rust deployments should standardize on embedded templates plus remote
  `values.yml`. Config-server should not normally deliver full template files
  for Rust services.

## Decision Summary

Embed app `config/*.yml` templates into the binary as the lowest-priority
default configuration source. The initial implementation uses a shared
build-time manifest generator, with `include_dir` remaining a possible future
implementation detail. Keep the existing source `config` directories for
documentation and build input. Use source-level override for structured config
files and key-level overlay for `values.yml`. Preserve current filesystem and
remote value layers so container deployments keep working, while native
deployments can run with only the binary and a small deployment-specific config
directory.
