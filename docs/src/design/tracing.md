# Tracing

Light-Fabric uses Rust `tracing` for application logs and runtime diagnostics.
The same tracing events must support two different consumers:

- operators and developers reading live logs from the console or control plane
- log platforms such as Splunk that ingest structured JSON

The logging design should keep one source of truth for emitted events and make
the output format configurable at the edge of the process.

## Goals

- Preserve the current human-readable console format for local development and
  controller-streamed logs.
- Support newline-delimited JSON logs for Splunk and other log ingestion
  systems.
- Allow deployments to choose text or JSON console output without changing
  application code.
- Allow authorized control-plane users to change log levels and logger targets
  without restarting the service.
- Avoid coupling Light-Fabric services directly to Splunk availability,
  credentials, retry policy, or backpressure handling.
- Keep log fields stable enough for portal-view, controller, and Splunk queries.

## Non-Goals

- Implement a Splunk HTTP Event Collector client inside every Light-Fabric
  service.
- Mix human text logs and JSON logs on the same stream.
- Use `values.yml` to mutate process environment variables. Environment
  variables are startup inputs; runtime changes should use an explicit logging
  configuration model.

## Current State

The application binaries initialize `tracing_subscriber` locally. The current
format is text-oriented and is easy to read in a terminal, Docker logs, or a
controller stream. Some binaries also support an ANSI toggle so container logs
can avoid escape sequences.

This works well for humans, but it is less reliable for Splunk field extraction.
Splunk can ingest text logs, but structured JSON gives predictable fields for
filtering, dashboards, alerts, and correlation.

## Output Formats

Light-Fabric should support the following output formats:

| Format | Intended Consumer | Notes |
| --- | --- | --- |
| `text` | humans, local development, controller live log stream | Existing behavior. Best for direct reading. |
| `json` | Splunk, OpenTelemetry Collector, Kubernetes log collectors | Newline-delimited JSON. Best for machine ingestion. |

The output should be selected with an environment variable:

```text
LIGHT_LOG_FORMAT=text
```

or:

```text
LIGHT_LOG_FORMAT=json
```

If the variable is absent, the default should remain `text` to preserve existing
operator behavior.

`RUST_LOG` should continue to provide the startup filter:

```text
RUST_LOG=info
RUST_LOG=light_gateway=debug,info
RUST_LOG=light_workflow=debug,info
```

## Single Console Stream

For most deployments, the preferred model is a single console stream with a
configurable format:

```text
application tracing event
        |
        v
tracing_subscriber fmt layer
        |
        +-- stdout/stderr as text or JSON
```

This has the lowest runtime overhead because each event is formatted and written
once. It also keeps container logging simple: the platform captures the process
console stream, and the customer chooses whether that stream is text or JSON.

When `LIGHT_LOG_FORMAT=json`, the console output should be newline-delimited
JSON:

```json
{"timestamp":"2026-06-03T14:12:41.233Z","level":"INFO","target":"light_gateway","fields":{"message":"proxy request completed","method":"GET","path":"/api/customer","status":200,"elapsed_ms":18,"correlation_id":"abc-123"}}
```

Raw JSON is readable, but it is not as pleasant as the text format. For the
control plane, portal-view should parse JSON log lines and render a human
projection:

```text
14:12:41.233  INFO  light_gateway  proxy request completed
method=GET path=/api/customer status=200 elapsed_ms=18 correlation_id=abc-123
```

This lets Splunk receive structured logs while portal-view remains readable for
operators.

## Portal-View Rendering

The controller should stream log lines without needing to understand every field.
Portal-view can detect whether a line is JSON:

1. Trim the line.
2. If it starts with `{`, try to parse it as JSON.
3. If parsing succeeds, render common fields in a stable layout.
4. If parsing fails, render the original line as plain text.

The renderer should treat JSON parsing as an enhancement, not a hard
requirement. This keeps mixed historical output, startup messages, and unrelated
tool output usable.

Recommended display fields:

| JSON Field | Display Use |
| --- | --- |
| `timestamp` | leading timestamp |
| `level` | severity badge/text |
| `target` | module or service source |
| `fields.message` | main message |
| `fields.correlation_id` | request correlation |
| `fields.request_id` | request identifier, when present |
| `fields.status` | HTTP or operation status |
| `fields.elapsed_ms` | latency |

Unknown fields can be shown in an expandable details view or appended as
`key=value` pairs.

## Splunk Ingestion

A log file is not the only option for Splunk ingestion.

### Console JSON in Containers

For Kubernetes and container deployments, console JSON is usually the best
default. The service writes JSON to stdout/stderr, and the platform logging
agent collects the container log stream. Splunk Connect for Kubernetes,
OpenTelemetry Collector, or an equivalent customer-managed collector can parse
the JSON and send it to Splunk HTTP Event Collector.

This avoids application-level Splunk credentials and keeps retry, batching, and
backpressure in the collector.

### JSON Log File

For VM or bare-metal deployments where the customer already uses Splunk
Universal Forwarder, a JSON log file is also valid. In that mode the application
would write newline-delimited JSON to a rotating file, and the forwarder or
OpenTelemetry filelog receiver would tail it.

This mode is useful when stdout is reserved for human-readable controller logs,
but it formats and writes each event through an additional sink if text console
output remains enabled.

### Direct Splunk HEC

Direct HTTP Event Collector delivery from the application is possible but should
not be the default. It adds Splunk endpoint configuration, token management,
retry policy, buffering, and failure handling to every service. A collector or
forwarder is a cleaner boundary for production deployments.

## Dual Sink Option

If a deployment must keep text console logs and produce JSON at the same time,
Light-Fabric can use multiple tracing layers:

```text
application tracing event
        |
        v
tracing subscriber registry
        |
        +-- text layer -> stdout/stderr
        |
        +-- JSON layer -> rolling file
```

This preserves the current control-plane stream and gives Splunk a clean JSON
source. The tradeoff is extra formatting and I/O work per event.

Use this mode only when a single JSON console stream is not acceptable for the
operator experience.

## Configuration

The design supports both single-stream and dual-sink logging through
configuration. The two common deployment profiles are:

| Deployment | Console Output | JSON File | Typical Splunk Path |
| --- | --- | --- | --- |
| Kubernetes/container | `json` | disabled | container log collector to Splunk HEC |
| Bare metal/VM with human console | `text` | enabled | Splunk Universal Forwarder or filelog receiver tails the JSON file |
| Local development | `text` | disabled | terminal or controller stream only |

The minimal configuration should be:

```text
LIGHT_LOG_FORMAT=text
LIGHT_LOG_ANSI=false
RUST_LOG=info
```

JSON console mode:

```text
LIGHT_LOG_FORMAT=json
LIGHT_LOG_ANSI=false
RUST_LOG=info
```

Optional dual-sink file mode:

```text
LIGHT_LOG_FORMAT=text
LIGHT_LOG_ANSI=false
LIGHT_LOG_JSON_FILE_ENABLED=true
LIGHT_LOG_JSON_FILE_DIR=/var/log/light-fabric
LIGHT_LOG_JSON_FILE_NAME=light-gateway.jsonl
LIGHT_LOG_JSON_FILE_ROTATION=daily
RUST_LOG=info
```

In this dual-sink mode, the application emits the same tracing event to both
sinks: text to stdout/stderr for humans and controller-streamed logs, and JSON
to the configured file for Splunk ingestion.

Service-specific aliases such as `GATEWAY_LOG_ANSI`, `AGENT_LOG_ANSI`, or
`WORKFLOW_LOG_ANSI` can remain during migration, but the long-term interface
should converge on `LIGHT_LOG_*` variables shared by all Light-Fabric binaries.

## Runtime Logging Control

Light-Fabric should support the Java control-plane behavior where an authorized
operator changes log levels and logger targets from portal-view without
restarting the service.

Rust can support this through `tracing_subscriber::reload`. Instead of installing
a fixed `EnvFilter`, the runtime should wrap the filter in a reloadable layer and
keep a reload handle in a shared logging controller:

```text
application tracing event
        |
        v
reloadable EnvFilter
        |
        v
text/json formatting layers
```

The reloadable part is the filter only. A filter can change the global level and
individual logger targets:

```text
info
debug
info,light_gateway=debug
info,light_gateway=debug,light_pingora::security=trace
info,light_pingora::security=off
```

This matches the practical Java use case: enable debug or trace for one logger
while keeping the rest of the service at `info`.

### Dynamic Versus Restart-Only Settings

| Setting | Dynamic | Reason |
| --- | --- | --- |
| Global log level | yes | Updates the reloadable `EnvFilter`. |
| Per-target logger level | yes | Updates the reloadable `EnvFilter`. |
| Disable a target with `target=off` | yes | Updates the reloadable `EnvFilter`. |
| Console format `text`/`json` | no | Requires rebuilding formatter layers. |
| JSON file enabled/disabled | no | Requires adding or removing a writer layer. |
| JSON file directory/name/rotation | no | Requires replacing the appender and guard. |
| ANSI setting | no | Formatter setting; treat as startup-only. |

### Startup Precedence

The startup filter should use this precedence:

1. `RUST_LOG`, when present.
2. `logging.filter` from `values.yml`.
3. The service default, such as `info` or `light_workflow=debug,info`.

This preserves existing `RUST_LOG` behavior for local and container deployments
while allowing managed deployments to define a persistent default filter in
config.

Example `values.yml`:

```yaml
logging.filter: info
```

More targeted example:

```yaml
logging.filter: info,light_gateway=debug,light_pingora::security=trace
```

`values.yml` should not overwrite environment variables and should not be the
normal path for day-to-day control-plane log-level changes. It should provide the
baseline filter that the logging module reads at startup. If an operator wants to
restore that baseline after a live debugging change, `reload_modules` can reload
`runtime/logging` from the latest resolved values.

Changing config server values and then triggering reload is therefore a
persistence/reset workflow, not the primary live-control workflow.

### MCP Tools

The runtime MCP tool surface should expose logging control alongside existing
runtime tools such as `get_service_info`, `get_modules`, and `reload_modules`.

Recommended tools:

| Tool | Purpose |
| --- | --- |
| `get_logging_filter` | Return the current effective filter and startup source. |
| `set_logging_filter` | Validate and apply a new live filter immediately. This is the normal portal-view control path. |
| `reload_modules` with `runtime/logging` | Reset the live filter from the configured baseline in `values.yml` or remote values. |

Example live filter update:

```json
{
  "name": "set_logging_filter",
  "arguments": {
    "filter": "info,light_gateway=debug"
  }
}
```

Example reset from the configured baseline:

```json
{
  "name": "reload_modules",
  "arguments": {
    "modules": ["runtime/logging"]
  }
}
```

The service response should include the active filter and status:

```json
{
  "status": "success",
  "filter": "info,light_gateway=debug"
}
```

Invalid filters should be rejected without changing the current filter:

```json
{
  "status": "error",
  "message": "invalid logging filter: ..."
}
```

### Portal-View Flow

The portal-view control plane should follow the same route used for other runtime
management tools:

```text
portal-view
  -> controller
  -> portal-registry/runtime instance connection
  -> service runtime MCP handler
  -> logging control
```

The UI can offer:

- a global level selector: `off`, `error`, `warn`, `info`, `debug`, `trace`
- per-target rows for Rust targets such as `light_gateway` or
  `light_pingora::security`
- an advanced filter text box for the full `EnvFilter` expression
- an apply action that calls `set_logging_filter`
- a reset action that reloads `runtime/logging` from the configured baseline
- an optional "save as default" action that persists the filter to config server

The advanced filter is important because Rust logger targets are module paths,
and operators may need precise target-level control during incident debugging.

The default portal-view workflow should be:

```text
operator changes filter
  -> portal-view calls set_logging_filter
  -> service updates the reloadable EnvFilter immediately
```

Portal-view should not require this slower path for a temporary debug change:

```text
operator changes filter
  -> portal-view updates config server
  -> portal-view calls reload_modules
  -> service reloads values.yml
```

That slower path is still useful when the operator intentionally wants the new
filter to survive service restart or redeploy.

## JSON Field Shape

JSON logs should be stable enough for both portal-view rendering and Splunk
searches. Recommended fields include:

| Field | Meaning |
| --- | --- |
| `timestamp` | event time in UTC |
| `level` | `ERROR`, `WARN`, `INFO`, `DEBUG`, or `TRACE` |
| `target` | Rust module or logical component |
| `fields.message` | human message |
| `fields.service` | logical service name, such as `light-gateway` |
| `fields.instance_id` | runtime instance, when known |
| `fields.host_id` | tenant/host context, when safe to log |
| `fields.correlation_id` | cross-service request correlation |
| `fields.request_id` | request identifier |
| `fields.method` | HTTP method, when applicable |
| `fields.path` | request path without sensitive query string |
| `fields.status` | response or operation status |
| `fields.elapsed_ms` | operation duration |

Sensitive values must not be logged in either format. This includes tokens,
API keys, session cookies, full authorization headers, raw secrets, and request
or response payload fields that may contain PII.

## Implementation Notes

Use `tracing_subscriber` as the formatting boundary. The JSON format requires
the `json` feature:

```toml
tracing-subscriber = { version = "0.3", features = ["env-filter", "fmt", "json"] }
```

File output should use `tracing_appender`:

```toml
tracing-appender = "0.2"
```

If non-blocking file output is used, the returned `WorkerGuard` must be kept
alive until process shutdown so buffered log lines are flushed.

The implementation should move per-binary `init_tracing()` logic into a shared
runtime helper so `light-gateway`, `light-agent`, `light-workflow`, and
`light-deployer` expose the same behavior.

For dynamic filtering, the shared helper should:

1. Build the initial `EnvFilter` from `RUST_LOG`, `logging.filter`, or the
   service default.
2. Install the filter through `tracing_subscriber::reload`.
3. Keep the reload handle in a `LoggingControl` value.
4. Register a reloadable module named `runtime/logging` with `ModuleRegistry`.
5. Add runtime MCP handlers for `get_logging_filter` and `set_logging_filter`.
6. Reject invalid filter expressions before swapping the active filter.

## Recommendation

Start with configurable single-stream console output:

- default `LIGHT_LOG_FORMAT=text`
- production/Splunk option `LIGHT_LOG_FORMAT=json`
- portal-view JSON parsing and human-friendly rendering
- no direct Splunk dependency in the application

Add dual-sink JSON file output only for customers who cannot change the console
stream to JSON but still require structured Splunk ingestion.
