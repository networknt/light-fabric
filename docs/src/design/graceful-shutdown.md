# Graceful Service Shutdown

Status: Implemented; deployment qualification pending

## Purpose

Light Fabric services must stop promptly when an orchestrator asks a container
to terminate, while still protecting requests and durable background work that
are already in progress.

The required behavior is:

1. install shutdown signal handlers before the service becomes ready
2. react to both `SIGINT` and `SIGTERM` on Unix
3. stop accepting new work immediately
4. allow in-flight work to drain
5. release service-owned resources and unregister where required
6. exit as soon as draining and cleanup finish
7. enforce a configured application deadline for work that does not finish

When a service has no in-flight work, shutdown should normally complete in less
than one second. A container stop timeout is a last-resort safety boundary, not
a delay that the application should consume on every stop.

## Problem Statement

Container engines normally stop a container by sending `SIGTERM` to PID 1,
waiting for a configured timeout, and then sending `SIGKILL` if the process is
still running.

Several current Rust applications wait only for:

```rust
tokio::signal::ctrl_c().await?;
```

That future handles `SIGINT`, but not the `SIGTERM` sent by Docker, Podman,
Kubernetes, and most process supervisors. Other applications run an Axum server
or a set of background tasks forever without installing either handler.

This is especially visible in the local Portal deployment. The external
`lightapi/portal-config-loc` repository invokes Compose with
`down --timeout 30` in `scripts/deploy-local.sh` inside
`stop_docker_compose()`. A Rust binary running as container PID 1 does not exit
through its intended shutdown path when it does not handle `SIGTERM`; the
engine waits for the entire timeout and then kills the process. The delay is
therefore unrelated to request volume. This motivating deployment setting does
not live in the `light-fabric` repository.

The current code also has inconsistent graceful-shutdown behavior:

- `light-runtime` applications call `RunningRuntime::shutdown()` only after
  their application-level signal future completes.
- `light-pingora` applies `server.shutdownGracefulPeriod` as a maximum drain
  time, but the application must first initiate runtime shutdown.
- `light-axum` initiates graceful shutdown without a deadline and currently
  does not apply `server.shutdownGracefulPeriod`.
- `RunningRuntime::shutdown()` awaits transport shutdown and each module hook
  without a wall-clock backstop.
- `PingoraTransport::stop()` joins a blocking server thread without an outer
  bound. The pinned `pingora-core 0.8.0` graceful path also sleeps for its full
  configured runtime shutdown timeout, even if no work remains.
- standalone Axum applications such as `controller-rs`, `light-oauth`, and
  `config-server` do not share a shutdown contract.
- task-oriented applications such as `light-workflow` need cancellation and
  task-join behavior in addition to HTTP request draining.

## Goals

- Provide one cross-platform shutdown-signal implementation in
  `light-runtime`.
- Make `SIGTERM` the canonical orchestrator signal and retain `SIGINT` for
  interactive use.
- Make graceful shutdown the default path for Light Runtime transports.
- Use `server.shutdownGracefulPeriod` consistently as a maximum drain period.
- Enforce one wall-clock deadline around the complete application shutdown
  sequence, including deregistration, transport drain, and module cleanup.
- Exit immediately after the listener, in-flight work, and cleanup complete.
- Give background workers an explicit cancellation and join contract.
- Preserve a larger orchestrator timeout as protection against process bugs.
- Make forced termination observable in automated tests and operations.

## Non-Goals

- Do not guarantee completion of arbitrary work after the graceful deadline.
- Do not use an orchestrator timeout as an application sleep period.
- Do not change the meaning of readiness, liveness, or startup timeouts.
- Do not make `stop_signal: SIGINT` the permanent deployment solution.
- Do not treat `docker compose down --timeout 0` or `SIGKILL` as graceful
  shutdown.
- Do not require all applications to adopt the same HTTP framework.

## Shutdown Contract

### Accepted signals

On Unix, every long-running service must handle both:

- `SIGTERM`, used by container engines and orchestrators
- `SIGINT`, used by an interactive Ctrl-C and local development tools

On non-Unix platforms, the shared implementation waits for the platform's
Ctrl-C event. Platform-specific service-manager integration can be added behind
the same API later.

The first accepted signal starts graceful shutdown. A second `SIGINT` or
`SIGTERM` collapses the remaining drain budget to zero, starts only the
mandatory cleanup floor described below, and then terminates with the
deadline-exceeded exit status if the process has not already stopped. This
makes Ctrl-C twice useful during interactive development without making the
first signal destructive. The second signal sets the hard exit deadline to the
earlier of the existing hard deadline and `now + MANDATORY_CLEANUP_FLOOR`; it
never extends shutdown.

### Shutdown phases

The service lifecycle gains the following terminal phases:

```text
Starting -> Ready -> Quiescing -> Draining -> CleaningUp -> Stopped
    |
    +-------> AbortingStartup ---------------------------> Stopped
```

- **AbortingStartup:** cancel the active startup phase, keep admission closed,
  unwind resources recorded by `StartupGuard`, and apply only the mandatory
  cleanup floor.
- **Quiescing:** atomically mark the instance unready, close the admission gate
  to new application work, and send a bounded deregistration request so peers
  stop advertising the instance.
- **Draining:** wait for accepted HTTP requests, WebSockets, streams, and
  claimed background work according to their component policy.
- **CleaningUp:** run module shutdown hooks, flush durable buffers, close
  clients, and release leases or registrations.
- **Stopped:** return from `main` with exit code zero only if the sequence
  completed before its hard deadline.

The application deadline begins when shutdown is accepted. Components receive
the same shutdown context and remaining deadline rather than each receiving
the full configured period sequentially.

### Readiness and admission semantics

There is no fixed pre-drain dwell. Shutdown changes the runtime readiness state
and closes a shared admission gate synchronously before awaiting network I/O.
The initial implementation uses one cloneable `AdmissionGate`, created closed
by the runtime and opened exactly once at the `Ready` transition:

```rust
pub enum AdmissionKind {
    Application,
    Control,
}

#[derive(Clone)]
pub struct AdmissionGate { /* atomic state and in-flight counters */ }

impl AdmissionGate {
    pub fn open(&self);
    pub fn close(&self);
    pub fn try_enter(
        &self,
        kind: AdmissionKind,
    ) -> Result<AdmissionPermit, AdmissionClosed>;
}
```

`AdmissionPermit` increments the relevant in-flight counter before dispatch and
decrements it from `Drop`. `Application` admission fails whenever the gate is
closed. `Control` admission remains available during `Quiescing` only for
framework-declared liveness, readiness, and shutdown-status handlers; it cannot
claim work, mutate application state, or start an unbounded operation. Readiness
reads the same gate and reports `not ready` as soon as `close()` returns.

The default classification is `Application`. An application migration may
declare an exact method-and-path route as `Control` only in its reviewed route
inventory; prefix and wildcard bypasses are forbidden. Until such an inventory
exists, existing health routes also receive the shutdown `503`, which is a
valid not-ready response. This makes the initial behavior fail closed and keeps
the admission exception set auditable.

Axum installs the admission layer outside the application router. Pingora calls
the same gate before handler dispatch. A rejected HTTP application request gets
`503 Service Unavailable`, `Connection: close`, and `Retry-After: 0`. A worker
must acquire an `Application` permit before claiming a unit; failure means it
stops its claim loop. WebSocket and stream upgrades retain the permit for their
full lifetime. No application may implement a second, unsynchronized shutdown
flag.

The runtime then sends the bounded deregistration request. Once it is
acknowledged or its small bound expires, transport drain begins. Upstream
readiness propagation delay is not modeled as a sleep and does not create a
second grace period. Time used by deregistration is charged against the one
application deadline.

For Axum, `Handle::graceful_shutdown` combines listener close and connection
drain. The observable `Quiescing` phase therefore comes from the runtime state,
admission gate, deregistration event, and phase metrics, not a distinct Axum
transport state. During the bounded deregistration step the socket may still
accept a connection, but new application work receives the defined `503`.
After deregistration is acknowledged or bounded, the handle is invoked and new
TCP connections are refused. Transport tests distinguish these two observable
boundaries instead of treating admission rejection and listener closure as the
same event.

### Deadline behavior

`server.shutdownGracefulPeriod` is the application-level maximum, in
milliseconds. It is not a minimum wait.

The top-level runtime supervisor is the deadline enforcer. It creates one
absolute graceful deadline and wraps the entire normal shutdown sequence with
`tokio::time::timeout_at`. Transport-local timeouts are cooperative component
bounds, not substitutes for this backstop.

For example, with a value of `2000`:

- zero in-flight work should stop immediately
- a 500 ms request may finish normally
- work still active at two seconds is cancelled or disconnected according to
  the component contract
- cleanup uses only the time remaining in the same deadline

A graceful deadline expiry cancels the shared shutdown context. The internal
sequence then allows only `MANDATORY_CLEANUP_FLOOR` for emergency cleanup that
was prepared in advance and returns `ShutdownOutcome::DeadlineExceeded`. The
production `run_until_shutdown` supervisor emits the final deadline-exceeded
record to stderr and calls `std::process::exit(1)`; the lower-level API returns
the corresponding error. Calling `process::exit` at the production boundary is
intentional: merely returning an error can still hang while Tokio drops a
runtime that owns an unbounded `spawn_blocking` task such as Pingora's
server-thread join. Exit code `1` means application shutdown failure; exit code
`137` still means the container engine had to send `SIGKILL` and is a stronger
qualification failure.

`shutdownGracefulPeriod: 0` skips request and worker drain. It does not remove
the emergency cleanup budget. Define a non-configurable initial
`MANDATORY_CLEANUP_FLOOR` of 250 ms for readiness change, admission closure,
best-effort deregistration/socket close, and already-prepared checkpoints. The
hard process deadline is therefore:

```text
signal time + shutdownGracefulPeriod + MANDATORY_CLEANUP_FLOOR
```

Ordinary module cleanup remains inside `shutdownGracefulPeriod`; it is not
added again. The floor is exclusively for emergency cleanup after cancellation
and does not authorize starting a new durable flush after the graceful
deadline. Deployments should use zero only for tests or an explicitly
documented emergency policy.

## Shared Runtime Design

### Signal API

Add a small signal module to `light-runtime`. Handler installation and waiting
must be separate operations so the process cannot become ready before it owns
the handlers:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownReason {
    Interrupt,
    Terminate,
    Programmatic,
}

pub struct ShutdownWatcher { /* platform signal streams */ }

impl ShutdownWatcher {
    pub fn install() -> std::io::Result<Self>;
    pub async fn recv(&mut self) -> ShutdownReason;
}
```

`ShutdownWatcher::install()` synchronously creates the platform signal streams.
On Unix it installs streams for both interrupt and terminate. It must not defer
registration until the first poll of `recv()`. The non-Unix implementation
constructs the available platform Ctrl-C stream behind the same API.
`ShutdownWatcher::recv()` returns only `Interrupt` or `Terminate`;
`Programmatic` is reserved for the lower-level embedding and test API.

Tokio's Unix signal stream requires an active reactor and panics when created
outside a runtime context. `ShutdownWatcher::install()` must therefore be the
first lifecycle statement inside the asynchronous body created by
`#[tokio::main]`, not a call made in synchronous code before entering the Tokio
runtime:

```rust
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let watcher = ShutdownWatcher::install()?;
    // Logging and configuration follow handler installation.
    let runtime = build_runtime()?;
    runtime.run_until_shutdown(watcher).await?;
    Ok(())
}
```

The API documentation and a subprocess panic test must call out this reactor
precondition explicitly. Installing before logging is acceptable; a signal
received during later startup remains pending in the watcher.

The production lifecycle is therefore:

```rust
let watcher = ShutdownWatcher::install()?;
runtime.run_until_shutdown(watcher).await?;
```

`LightRuntime::run_until_shutdown` owns cancellable startup, readiness
publication, signal receipt, and shutdown so the ordering is enforced by
construction. The lower-level `start()` API remains available for embedding
and tests, but its documentation must require an installed watcher or another
programmatic cancellation owner before `start()` can publish readiness.

The supervisor retains the watcher after the first signal and selects between
shutdown completion and `watcher.recv()` again. A second accepted signal
cancels the drain context and moves directly to mandatory cleanup.

### Runtime convenience API

Add a production convenience method that owns the complete lifecycle. The
following pseudocode shows the required concurrency; helper types may package
the select and deadline handling differently:

```rust
impl<T: TransportRuntime> LightRuntime<T> {
    pub async fn run_until_shutdown(
        self,
        mut watcher: ShutdownWatcher,
    ) -> Result<(), RuntimeError> {
        let startup_cancel = CancellationToken::new();
        let mut startup = Box::pin(
            self.start_cancellable(startup_cancel.child_token()),
        );

        let running = tokio::select! {
            biased;
            reason = watcher.recv() => {
                startup_cancel.cancel();
                return finish_startup_abort(
                    reason,
                    startup,
                    &mut watcher,
                ).await;
            }
            result = &mut startup => result?,
        };

        let reason = watcher.recv().await;
        tracing::info!(?reason, "shutdown signal received");
        running.shutdown_with_watcher(reason, &mut watcher).await
    }
}
```

Applications using `LightRuntimeBuilder` then use:

```rust
let watcher = ShutdownWatcher::install()?;
runtime.run_until_shutdown(watcher).await?;
```

This replaces app-local `ctrl_c()` calls. `RunningRuntime::shutdown()` remains
available for tests, embedding, and programmatic lifecycle management. It
delegates to the same internal shutdown sequence with
`ShutdownReason::Programmatic`; it has no second-signal branch. The production
watcher path calls `shutdown_with_watcher`, which selects between that sequence
and another accepted signal. Neither public entry point duplicates the
shutdown implementation. The internal sequence returns a structured
`ShutdownOutcome`. `RunningRuntime::shutdown()` converts a deadline outcome to
`RuntimeError::ShutdownDeadlineExceeded` and never terminates its caller's
process. `LightRuntime::run_until_shutdown()` is the production policy boundary:
after emergency cleanup and the final stderr record, it converts that same
outcome to `std::process::exit(1)`. An embedding caller that uses the lower-level
API owns its own escalation policy.

### Cancellable startup

Startup is cancellable. Retaining a signal until `start()` finishes is not
sufficient because remote bootstrap and controller registration perform network
I/O, and registration alone is allowed five seconds by default.

`start_cancellable()` owns a `StartupGuard` from its first operation. As startup
progresses, the guard records every resource that requires asynchronous unwind:

- remote bootstrap/config fetch and any staged config-cache write
- registered lifecycle participants and application resources
- a partially or fully bound transport handle
- controller registration state, socket, and reconnect task
- readiness/admission state

Ownership must reach the guard before the next cancellation point. This is an
API invariant, not a convention. In particular, controller startup is split
into two operations:

```rust
let registry_session = registry_client.start_session(/* ... */)?;
startup_guard.set_registry_session(registry_session);
startup_guard
    .registry_session()
    .wait_until_registered(startup_cancel.child_token())
    .await?;
```

`RegistrySession` owns the client, socket-generation state, reconnect task, and
task join handle. `start_session()` may spawn the task, but after spawning it
must return the owning session without another `.await`. Dropping a registration
wait therefore cannot detach the task; startup abort calls the session's
deadline-aware shutdown operation through `StartupGuard`.

The same rule applies to transport binding. A `bind()` implementation owns an
internal `BindingGuard` until it returns `BoundTransport`. A listener, thread,
or task created inside `bind()` must either remain owned by that guard across
every `.await`, or be created as the final non-awaiting operation immediately
before the handle is returned. Cancelling `bind()` must synchronously close any
listener and cancel any task that has not been handed to `StartupGuard`; if a
resource requires asynchronous unwind, `bind()` must expose a staged owned
handle before beginning that operation. A transport implementation that can
detach work when its future is dropped does not satisfy `TransportRuntime`.

Each startup phase selects between its work and the startup cancellation token.
Configuration-cache writes and other persistent startup effects must use an
atomic stage-and-commit pattern so cancellation cannot expose a partial file or
half-published state.

If the watcher wins before `Ready`, the runtime:

1. cancels the in-progress bootstrap or registration future
2. keeps readiness false and seals admission closed
3. seals the lifecycle registry against new participants
4. asks `StartupGuard` to close any bound listener and partial registration
5. invokes already-registered participants in startup-abort mode
6. exits zero if unwind completes within `MANDATORY_CLEANUP_FLOOR`

There is no request-drain period because the service never reached `Ready`.
The mandatory cleanup floor is the complete startup-abort budget. If unwind
does not finish inside it, the supervisor uses the same final stderr record and
`std::process::exit(1)` policy as a graceful-deadline failure. A second signal
while aborting startup collapses to the remaining portion of that floor and
never extends it.

Dropping the `start()` future alone is not the abort mechanism: that would lose
ownership of partially bound resources without awaited cleanup. The
resource-owning `StartupGuard` and cancellation-aware phase boundaries are
required implementation mechanisms.

### Shutdown context and module contract

The deadline requires changing the `Module` trait.
Wrapping today's `on_shutdown(&RuntimeConfig)` future in `timeout()` would stop
waiting but would cancel that future at an arbitrary await point. That is not a
safe contract for a durable flush or checkpoint.

Add a shared context and pass it into every hook:

```rust
pub enum ShutdownMode {
    Graceful,
    StartupAbort,
    Emergency,
}

pub struct ShutdownContext {
    pub reason: ShutdownReason,
    pub mode: ShutdownMode,
    pub deadline: tokio::time::Instant,
    pub cancellation: tokio_util::sync::CancellationToken,
}

impl ShutdownContext {
    pub fn remaining(&self) -> Duration;
    pub async fn cancelled(&self);
}

#[async_trait]
pub trait Module: Send + Sync {
    // Existing lifecycle methods omitted.
    async fn on_shutdown(
        &self,
        config: &RuntimeConfig,
        context: &ShutdownContext,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }
}
```

The change is source-breaking in type-system terms, but its known migration set
is currently empty. A workspace-wide source audit finds no `impl Module for ...`
and no `.with_module(...)` call site. This is the lowest-cost point to correct
the signature. Phase 4 must repeat the audit in external consumers; if it finds
an implementation, that repository is named and migrated explicitly. The
design does not assume an external coordination cost without such evidence.

### Making cleanup real

Today the `modules` vector is always empty, so the `CleaningUp` loop has no
participants. Most resources currently rely on Rust `Drop` behavior, including
application state that owns database pools; other tasks are aborted or left to
process teardown. `Drop` is useful as a final safety net but does not provide an
awaited pool close, durable flush, checkpoint acknowledgement, or observable
deadline outcome.

Adopting lifecycle participants for durable cleanup is in scope for this
design, not a prerequisite assumed to exist. Lifecycle registration is
transport-neutral. Add `LifecycleRegistry`, a cloneable registration-only
`LifecycleRegistrar`, and one object-safe participant contract to
`light-runtime`:

```rust
#[async_trait]
pub trait LifecycleParticipant: Send + Sync {
    fn name(&self) -> &'static str;

    async fn shutdown(
        &self,
        config: &RuntimeConfig,
        context: &ShutdownContext,
    ) -> Result<(), RuntimeError>;
}

impl LifecycleRegistrar {
    pub fn register(
        &self,
        participant: Arc<dyn LifecycleParticipant>,
    ) -> Result<(), RuntimeError>;
}
```

Participant names are unique within a runtime; duplicate registration is a
startup error. The registrar can add a participant but cannot enumerate,
invoke, or seal the set. The registry invokes participants sequentially in
reverse registration order, which is also reverse resource-construction order.
Every hook is attempted even after an earlier error, and the runtime returns an
aggregate error after the bounded sequence. Phase 1a does not run hooks in
parallel and does not add dependency declarations; a later optimization may
add explicit parallel groups without changing the default ordering.

The runtime wraps each builder-supplied `Arc<dyn Module>` in an internal
`ModuleParticipantAdapter`; `name()` delegates to the module and `shutdown()`
calls its deadline-aware `on_shutdown()`. `Module` does not extend
`LifecycleParticipant`. Modules are inserted at their construction position in
the same registry, while application-owned resources implement
`LifecycleParticipant` directly.
Transport handles themselves retain explicit transport ownership and are not
also registered as participants, which prevents double shutdown.

The reviewed initial ownership inventory is:

| Owner | Resource | Shutdown owner | Delivery phase |
| --- | --- | --- | --- |
| `light-runtime` | portal-registry socket, terminal state, reconnect task, and join handle | `RegistrySession::shutdown` before transport drain | Phase 1b |
| `light-axum` | listener handle and server task | `AxumBoundHandle` through `TransportRuntime::stop` | Phase 1c |
| `light-pingora` | controlled-shutdown sender and Pingora server thread | `PingoraBoundHandle` through `TransportRuntime::stop` | Phase 1c |
| builder modules | resources explicitly owned by each module | reverse-order lifecycle participant | Phase 1a and consumer migration |
| application pools, buffers, leases, and task supervisors | resource identified in that application's migration inventory | application lifecycle participant | Phases 2 and 3 |

Each application migration PR must add a checked inventory table naming every
pool, durable buffer, lease owner, and spawned-task supervisor and either name
its participant or state why synchronous `Drop` is sufficient. Phase 1a is not
blocked on undiscovered application resources, and a later application phase
cannot claim completion without its reviewed table.

The initial application migration inventory is:

| Service | Owned asynchronous resource | Shutdown ownership |
| --- | --- | --- |
| `light-agent` | SQLx application pool | `light-agent-database` participant closes and awaits the pool |
| `light-knowledge` | SQLx application pool | `light-knowledge-database` participant closes and awaits the pool |
| `light-workflow` | SQLx pool; consumer, executor, reconciler, rule API, scheduler, lease, fixed-action, and retention tasks | `light-workflow-database` participant closes the pool after the task supervisor cooperatively cancels and joins every task; abort is deadline-only |
| `light-gateway` | transport-owned listener/server thread; in-memory configuration and bounded caches | transport stop owns the thread; cache/configuration owners require only synchronous `Drop` |
| `light-deployer` | transport-owned listener/server task; in-memory service state | transport stop owns the task; service state requires only synchronous `Drop` |
| `light-workflow-runner` | execution supervisor, transport, health/watchdog/reconciler tasks, SQLite journal | its standalone shutdown path drains the supervisor and transport, joins or deadline-aborts tasks, and returns failure on timeout; SQLite cleanup is synchronous `Drop` |
| `light-knowledge-worker` | command-scoped SQLx pool and bounded command tasks | its standalone shutdown path bounds each command and awaits `PgPool::close`; command tasks do not outlive the selected command |

Both transport construction paths receive the registrar alongside
`&RuntimeConfig`:

```rust
pub trait TransportRuntime {
    async fn bind(
        &self,
        config: &RuntimeConfig,
        lifecycle: &LifecycleRegistrar,
        admission: &AdmissionGate,
        startup_cancel: CancellationToken,
    ) -> Result<BoundTransport<Self::Handle>, RuntimeError>;

    async fn stop(
        &self,
        handle: &mut Self::Handle,
        context: &ShutdownContext,
    ) -> Result<(), RuntimeError>;
}

pub trait PingoraApp: Send + Sync + 'static {
    type Proxy: ProxyHttp + Send + Sync + 'static;

    fn proxy(
        &self,
        config: &RuntimeConfig,
        lifecycle: &LifecycleRegistrar,
        admission: &AdmissionGate,
    ) -> Result<Self::Proxy, RuntimeError>;
}
```

Light Axum's `ServerContext` re-exposes clones of the light-runtime registrar
and admission gate to `AxumApp::router()`. It does not own or define either
contract. Light Pingora passes the same values to `PingoraApp::proxy()`, which
closes the construction-order gap for light-gateway proxies that create durable
buffers, pools, or clients. Standalone applications can construct the same
light-runtime registry and gate directly without depending on either framework
context type.

The successful startup publication order is fixed: run all `on_ready` hooks
while admission remains closed, seal the registry, transition the state to
`Ready`, and open admission as the final synchronous step. Registration after
sealing is an error. Startup cancellation seals the registry against new
participants before invoking the already-registered participants' abort
cleanup.

Each participant owns its concrete resource and implements the deadline-aware
hook. Standalone applications use the same `ShutdownContext` and participant
contract even when they do not use `LightRuntimeBuilder`. The phase is complete
only when the resource inventory is explicit; an empty module loop is not
accepted as successful cleanup.

Hooks must use `context.remaining()` for their own I/O bounds, observe
`context.cancellation`, and leave durable work committed, checkpointed, or
recoverable before returning. The top-level `timeout_at` remains the hard
backstop for defective or legacy components. At expiry it may cancel a
mid-flight hook; the nonzero process exit and component timeout telemetry make
that failure explicit rather than reporting a graceful stop.

A participant is registered only after its owned resource is internally
consistent. It must handle both `Graceful` and `StartupAbort`; the latter may be
called before the overall service reaches readiness. `Emergency` permits only
the prearranged bounded cleanup described by the mandatory floor.

### Runtime shutdown ordering

`RunningRuntime::shutdown()` should perform a single deadline-aware sequence:

1. create the absolute graceful and hard deadlines and shared
   `ShutdownContext`
2. transition runtime state to `Quiescing`, mark readiness false, and close the
   admission gate synchronously
3. atomically put the registry session in terminal mode so it can never reconnect
4. send an explicit bounded deregistration/goodbye on the current WebSocket,
   wait for acknowledgement, close the socket, and join the reconnect task
5. ask the transport to stop accepting connections and drain existing work
6. invoke deadline-aware module hooks with the same context
7. log the duration and return the structured outcome; the production
   `run_until_shutdown` boundary enforces process exit on expiry

The current `registration_task.abort()` is not sufficient. `RegistrySession`
uses an atomic `Running -> Stopping -> Stopped` state. `shutdown(context)` wins
the `Running -> Stopping` transition before sending anything. The reconnect
loop observes `Stopping` in connection attempts, the active connection loop,
and retry sleeps. It may finish the current goodbye exchange, but after that
connection ends it exits instead of sleeping or registering again. Concurrent
or repeated shutdown calls join the same terminal operation.

The codec-neutral logical request is frozen as:

```json
{
  "jsonrpc": "2.0",
  "id": "shutdown-generated-request-id",
  "method": "service/deregister",
  "params": {
    "runtimeInstanceId": "019...",
    "reason": "terminate"
  }
}
```

The successful result is:

```json
{
  "runtimeInstanceId": "019...",
  "status": "deregistered"
}
```

`reason` uses the lowercase shutdown reason names `interrupt`, `terminate`, or
`programmatic`. The controller rejects a `runtimeInstanceId` that does not
match the authenticated session with JSON-RPC `-32602`. For the negotiated
binary profile, add `ClientGoodbyeV1 { request_id, runtime_instance_id, reason
}` and `ServerGoodbyeV1 { request_id, runtime_instance_id }` to
`controller-wire`; assign new message-kind values without changing any existing
v1 discriminant. The legacy JSON and binary adapters map to the same
`SessionInput::Deregister` and `SessionOutput::Deregistered` values.

Controller handling is idempotent for a repeated request on the same session.
On the first valid request it marks the session terminal, removes the instance
only when the connection id still matches, fails pending commands, emits the
discovery and MCP removal notifications, records the disconnect event, and
then queues the acknowledgement. The route must flush that acknowledgement
before sending the WebSocket close frame; it cannot abort the writer task first.
The existing connection-id comparison remains the stale-socket protection. A
normal socket close without goodbye continues to use the same cleanup routine,
so an old client remains safe.

`RegistrySession::shutdown` returns `Acknowledged`, `Disconnected`, or
`TimedOut`. Only `Acknowledged` proves the controller removed the instance
before transport drain. `Disconnected` and `TimedOut` are logged and shutdown
continues because the controller's ordinary socket cleanup remains the
fallback. The operation's bound is
`min(context.remaining(), registration_timeout)`, where the existing builder
registration timeout defaults to five seconds. For the normal two-second
shutdown setting, the remaining application deadline is therefore the tighter
bound. Deregistration never creates an additional deadline.

Errors from one cleanup hook must not silently prevent the remaining hooks from
running. The runtime should collect cleanup failures and return a combined
error after all bounded cleanup attempts finish.

## Framework Integration

### Light Axum

`AxumTransport` should store the configured shutdown duration in its bound
handle and pass it to `axum_server::Handle`:

```rust
handle.graceful_shutdown(Some(Duration::from_millis(
    shutdown_graceful_period,
)));
```

The listener stops accepting new connections when transport drain starts,
after bounded deregistration. From the earlier admission-close boundary until
then, new application requests receive the defined `503`. Existing accepted
connections drain until they complete or the deadline expires. With no active
connections, the server task should join immediately.

The transport receives the shared absolute deadline and derives its remaining
duration immediately before calling the handle. It must not restart the full
configured period after deregistration. The top-level runtime timeout remains
the enforcer if `Handle` or its task join fails to return.

Tests must include ordinary requests, streaming bodies, keep-alive
connections, and WebSockets. A connection being idle must not hold shutdown
open indefinitely.

### Light Pingora

`PingoraTransport` uses a controlled shutdown channel. The shared runtime first
closes admission and drains the gateway's application permits against the
absolute shutdown deadline. Pingora's internal graceful timeout is therefore
zero: it must not restart the original configured period after the shared drain.
The transport joins the Pingora thread using `ShutdownContext::remaining()`. The current
implementation uses the crates.io `0.8.1` release. That release contains a
redundant sleep after `Runtime::shutdown_timeout` for nonzero internal timeout
values. Light-Fabric does not exercise that path: the shared admission gate
owns application draining, and both Pingora internal shutdown periods are set
to zero before the server starts. The redundant upstream sleep is therefore
zero-duration, while the outer thread join remains bounded by the shared
absolute deadline. No vendored Pingora source or Cargo override is required.

Returning Pingora's `FastShutdown` is not an acceptable normal-path workaround
because it forfeits request draining.

The migration must verify that:

- the controlled signal stops listener acceptance immediately
- with no active downstream exchange, transport stop completes in less than one
  second
- active proxy requests can finish inside the deadline
- WebSockets and streaming exchanges cannot exceed the deadline
- the Pingora thread is joined before module cleanup completes

For upstream `pingora-core 0.8.1`, `Some(0)` is a zero-duration runtime shutdown;
it does not mean wait forever. A transport configuration test must pin both
zero values so a dependency upgrade cannot silently restart an internal grace
period after the shared drain.

The separate `grace_period_seconds` setting is equally load-bearing. Pingora
performs another unconditional sleep before runtime shutdown and defaults a
missing value to `EXIT_TIMEOUT`, currently five minutes. `light-pingora`
explicitly sets `grace_period_seconds = Some(0)`; Phase 1c must preserve that
assignment and pin it in the same configuration and latency tests. Removing it
must fail a test rather than turn a normal stop into a five-minute pre-drain
sleep.

### Standalone Axum services

Services that do not use `LightRuntimeBuilder` must still use the shared signal
API. They should create an `axum_server::Handle`, run the server and shutdown
future concurrently, then invoke `graceful_shutdown` with their configured
deadline.

Migration to `LightRuntimeBuilder` is preferred when it does not introduce an
unrelated architectural change, but signal correctness must not wait for that
migration.

### Background workers

Long-running loops must accept a `CancellationToken` or equivalent cancellation
receiver. On shutdown they must stop claiming new work before joining already
spawned tasks.

Each worker must classify its work as one of:

- **drain:** finish the current unit inside the deadline
- **checkpoint:** persist progress and release the unit for retry
- **cancel:** abort work that is side-effect free or transactionally safe

Dropping a Tokio `JoinHandle` does not stop its task and is not an acceptable
shutdown implementation. Every service must cancel, join, or deliberately
abort each owned task.

`light-workflow` needs a service-level cancellation token shared by its event
consumer, executor, reconcilers, rule API, scheduler, reaper, and fixed-action
workers. Lease-backed work must be released or left in a state that its normal
reconciliation policy can recover.

### Existing prior art

`apps/light-workflow-runner/src/main.rs` is the closest in-repository precedent.
After Ctrl-C it calls `supervisor.drain()`, publishes shutdown through a watch
channel, and wraps the transport join in
`timeout(config.shutdown_grace, transport)`. Its application-prefixed
`shutdownGraceMs` demonstrates the intended supervisor-drain-bound shape.

It is a starting point, not yet the completed contract: it handles only
Ctrl-C, installs no eager watcher, discards the timeout result, does not use the
shared exit policy, and does not account for every spawned health/watchdog task.
Its migration should preserve the drain-first behavior while adopting the
shared signal, context, deadline outcome, and task-ownership rules.

## Container And Orchestrator Contract

The image and deployment contract remains `SIGTERM`. A service-specific
`stop_signal: SIGINT` may be used only as a temporary compatibility measure
while an older image is being migrated.

The outer stop timeout must be greater than the application graceful period:

```text
orchestrator stop timeout
  >= application graceful period
  + mandatory cleanup floor
  + scheduling allowance
```

Deregistration, drain, and normal asynchronous cleanup all share the configured
application graceful period. Only the fixed emergency cleanup floor sits
outside it, so ordinary cleanup is not double-counted.

If termination arrives during startup, cancellation begins immediately and the
service gets only the mandatory cleanup floor; it does not finish the remaining
bootstrap or registration timeout and does not add a graceful drain period.
Therefore the normal running-service inequality above is also the worst-case
bound for startup termination. If a future startup phase cannot honor
cancellation, its maximum remainder must be added explicitly to this inequality
until that phase is repaired.

For a two-second application grace period, a 5-10 second container timeout is
usually sufficient. Retaining a 30-second Compose timeout is also safe once the
application handles `SIGTERM`, because the engine stops waiting as soon as the
process exits.

Kubernetes deployments should set `terminationGracePeriodSeconds` using the
same inequality. A future `preStop` hook must not duplicate the application
grace period or merely sleep.

If the application deadline expires during a deliberate Pod deletion, the
container's terminated state records exit code `1`. That is the expected
representation of an application-level graceful-shutdown failure, not exit
code `137` from an orchestrator kill. Kubernetes dashboards and alerts must
correlate the nonzero exit with Pod deletion/termination context: record and
trend it, but do not page solely on that exit code during an intentional
rollout. Repeated deadline expiry or exit `1` outside termination remains
actionable.

As a preventive rule, shell entrypoints must end with `exec` so the Rust binary
becomes PID 1. If an init or wrapper process is required, it must forward
`SIGTERM` and reap child processes. This is not a currently identified
`light-fabric` image defect: in-repository Dockerfiles use exec-form
`CMD`/`ENTRYPOINT`, and the workflow runner uses `tini --` in
`apps/light-workflow-runner/docker/Dockerfile`.

## Configuration

The existing setting remains canonical:

```yaml
shutdownGracefulPeriod: ${server.shutdownGracefulPeriod:2000}
```

No separate `shutdownSleep`, Compose-specific, Axum-specific, or
Pingora-specific duration should be introduced.

Validation rules:

- warn when a value is zero outside tests
- retain millisecond precision in the global runtime deadline
- round a positive Pingora component timeout up to a whole second and log the
  configured and effective values
- log the configured and effective duration at startup
- never log that shutdown was graceful if the deadline expired

Task-oriented applications not yet using `ServerConfig` may temporarily expose
an application-prefixed duration, but they should converge on the shared
setting when adopting `light-runtime` lifecycle management.

`light-workflow-runner` currently rejects `shutdownGraceMs: 0`, while the shared
server contract permits zero with an emergency cleanup floor. That stricter
runner rule is deliberate for a lease-owning worker: it must preserve some
cooperative drain/checkpoint opportunity. Convergence means sharing signal,
deadline, outcome, and outer-timeout semantics; it does not require every
service class to permit zero. The runner validator and example documentation
must state this exception explicitly.

## Observability

Emit structured events for:

- accepted signal and shutdown reason
- transition into each shutdown phase
- number of active requests, streams, sockets, and worker tasks at drain start
- configured deadline and remaining time
- component completion or timeout
- total shutdown duration
- final graceful, deadline-exceeded, or failed outcome

Active-work cardinality is not currently available for free from
`axum_server` or Pingora. Reporting active requests, streams, sockets, and
worker tasks requires framework-owned admission/in-flight counters that are
incremented before dispatch and decremented by a drop guard. Counter delivery
is implementation work in the framework phases, not merely a logging change.

Recommended metric families are:

- `service_shutdown_total{reason,outcome}`
- `service_shutdown_duration_seconds`
- `service_shutdown_active_work{kind}`
- `service_shutdown_component_duration_seconds{component}`

Normal termination should exit with code zero. Startup failure, cleanup
failure, and graceful-deadline expiry exit nonzero; deadline expiry specifically
uses exit code `1`. Exit code 137 in the container qualification test indicates
forced `SIGKILL` and fails the graceful-shutdown gate.

## Migration Plan

### Phase 1a: Shared primitives and compile surface

1. add eagerly installed `ShutdownWatcher`, `ShutdownReason`,
   `ShutdownContext`, `AdmissionGate`, `LifecycleRegistry`, and
   `LifecycleRegistrar` to `light-runtime`
2. make the deadline-aware `Module::on_shutdown` change and add the fixed
   reverse-registration `LifecycleParticipant` behavior
3. update all five known `TransportRuntime` implementations in the same change:
   `AxumTransport`, `PingoraTransport`, the `light-runtime` test transport, and
   the headless transports in `light-workflow` and `light-knowledge-worker`;
   the headless implementations may ignore registrar/admission arguments until
   their Phase 3 behavioral migration, but the workspace must remain compiling
4. pass registrar and admission capabilities through `ServerContext` and
   `PingoraApp`, update the in-repository `GatewayApp` implementation in the
   same compile change, invoke `on_ready`, seal lifecycle registration,
   transition to `Ready`, and open admission in the specified order
5. add cancellation-aware startup phases, binding ownership guards,
   `StartupGuard`, and `LightRuntime::run_until_shutdown`; preserve
   `RunningRuntime::shutdown()` through `ShutdownReason::Programmatic`
6. add signal, admission, startup-abort, lifecycle-order, aggregate-error, and
   public-API tests

### Phase 1b: Registry terminal protocol

This is an explicitly coordinated `light-fabric` plus `controller-rs` phase,
not deferred external adoption:

1. add the codec-neutral deregister values and append-only v1 goodbye message
   kinds to `controller-wire`, including legacy JSON, rkyv, golden-fixture, and
   invalid-instance tests
2. split registry startup into owned `RegistrySession` creation followed by a
   cancellable registration wait
3. implement terminal/no-reconnect state, bounded goodbye, socket close, and
   task join in `portal-registry`
4. implement idempotent connection-matched removal, acknowledgement flush, and
   close ordering in `controller-rs`
5. run cross-repository tests for acknowledged shutdown, stale sockets,
   disconnect fallback, timeout, startup abort, and proof that no registration
   occurs after terminal state is entered

### Phase 1c: Runtime transports

1. pass the shared remaining deadline into `TransportRuntime::stop`
2. apply admission and the remaining deadline in `light-axum`
3. upgrade the Pingora crate family to the crates.io `0.8.1` release and pin
   Pingora's internal grace and runtime-shutdown periods to zero
4. verify Pingora rounded timeout, `grace_period_seconds = Some(0)`,
   zero-duration behavior, active drain, and no-load fast return
5. add Axum and Pingora request, stream, WebSocket, join, and global-backstop
   integration tests

### Phase 2: Light Runtime applications

Replace app-local `ctrl_c()` handling in:

- `light-gateway`
- `light-agent`
- `light-knowledge`
- `light-deployer`
- any example application built with `LightRuntimeBuilder`

Each migration must prove both `SIGINT` and `SIGTERM` paths.

### Phase 3: In-repository standalone services and workers

Adopt the shared signal API and explicit drain behavior in:

- `light-workflow`
- `light-workflow-runner`, preserving its existing
  `supervisor.drain()`/watch/timeout structure as prior art; replace the
  `shutdownGraceMs: 30000` examples with a validated value strictly below the
  external 30-second container timeout after reserving the 250 ms emergency
  floor and scheduling allowance
- `light-agent-channel`, including its HTTP server and three spawned delivery,
  trigger, and attachment-recovery loops
- `light-github-action-provider`
- `light-knowledge-worker` build and projection loops; its shipped
  `config/server.yml` already declares `shutdownGracefulPeriod: 2000`, which
  currently is not consumed by the worker
- long-running Rust example and MCP-server applications that ship from this
  workspace

`light-agent-worker` is deliberately excluded because its stdio lifecycle ends
on EOF from its supervisor. `light-pi-rpc-adapter` is excluded because it is a
one-shot adapter. If either becomes an independently orchestrated long-running
service, it enters this contract.

### Phase 4: External repository adoption

The following services are not implemented in `light-fabric`, so this phase
cannot be marked complete by a `light-fabric` change alone:

- `controller-rs` in the external `controller-rs` repository
- `portal-service`, `config-server`, and `light-oauth` in the external
  `portal-service` repository
- demo APIs and MCP servers in the external `light-example-rs` repository

These repositories consume the exported signal API without moving their
application code. Phase 1b already delivers the `controller-rs` deregistration
protocol; this phase migrates the controller process's own server lifecycle.

### Phase 5: Deployment qualification

1. keep the current outer timeout as a safety boundary
2. publish images containing the signal-handling changes
3. recreate containers so the new images are active
4. measure no-load and in-flight shutdown behavior under Docker and Podman
5. reduce local outer timeouts only if faster forced-failure feedback is useful
6. align Kubernetes termination grace values with the proven application bound

## Verification Strategy

### Signal tests

Use a subprocess fixture rather than sending termination signals to the test
runner itself. The fixture must:

- report ready only after both handlers are installed
- exit zero after `SIGINT`
- exit zero after `SIGTERM` on Unix
- record exactly one accepted shutdown reason
- collapse the remaining drain budget after a second accepted signal
- prove a signal delivered between watcher installation and runtime readiness
  immediately cancels startup rather than waiting for startup completion or
  taking the default signal disposition
- document and test that watcher installation outside a Tokio reactor is an
  invalid call, while the first statement inside `#[tokio::main]` succeeds

### Startup cancellation tests

Inject a controllable future into each startup phase and deliver `SIGTERM`
while it is pending:

- remote bootstrap is cancelled without publishing a partial cache file
- a bound Axum or Pingora listener is closed by `StartupGuard`
- partial controller registration is explicitly closed or deregistered
- lifecycle participants registered before cancellation receive startup-abort
  cleanup, while later registration is rejected after sealing
- the process exits zero within the 250 ms floor when cleanup cooperates
- a stuck startup resource triggers exit code `1` at the floor rather than
  waiting for the five-second registration timeout
- a startup signal and readiness completion in the same scheduler turn choose
  cancellation because the supervisor select is biased toward the signal

### Transport tests

For both Axum and Pingora:

- with no active request, transport stop completes in less than one second
- a request completing inside the deadline returns its normal response
- a request exceeding the deadline is terminated at the bound
- new application requests receive `503` as soon as quiescing begins
- new TCP connections are refused after bounded deregistration starts
  transport drain
- streaming and WebSocket connections obey the bound
- module shutdown hooks run after listener quiescence
- the whole runtime reaches its explicit deadline outcome even if transport
  stop or an underlying thread join does not cooperate

The Pingora no-load assertion is a release gate, not an aspirational timing
description. The shared admission drain and remaining-budget join must prevent
a dependency upgrade from introducing a fixed poll or sleep near one second.
Configuration tests assert both
`grace_period_seconds == Some(0)` and
`graceful_shutdown_timeout_seconds == Some(0)`.

### Module and deregistration tests

- every module receives the same absolute deadline and cancellation token
- participants run sequentially in reverse registration order; duplicate names
  fail startup and one hook error does not skip later hooks
- the production cleanup participant registry matches the reviewed resource
  inventory and is empty only for an explicitly resource-free service
- a cooperative hook checkpoints before cancellation and returns
- a deliberately stuck hook triggers the global backstop and exit-code-1 path
- cleanup errors are aggregated without skipping later bounded hooks
- the runtime closes admission before sending deregistration
- entering registry terminal state before goodbye prevents reconnect during
  send, acknowledgement, close, retry sleep, and concurrent shutdown calls
- legacy JSON and binary goodbye requests map to the same logical operation
- a mismatched runtime instance id fails closed and a stale connection cannot
  remove the replacement instance
- the controller removes the instance from routing and flushes acknowledgement
  before the client closes and transport drain starts
- an already disconnected socket returns the fallback outcome without trying
  to reconnect
- deregistration failure consumes only its share of the global remaining time
- Axum and Pingora resources created during router/proxy construction register
  through the same light-runtime lifecycle registry and are sealed at `Ready`

### Worker tests

- cancellation prevents new work from being claimed
- drainable work completes inside the deadline
- retryable work preserves or releases its durable lease correctly
- all owned tasks are joined, cancelled, or explicitly aborted
- database pools and durable buffers close without data loss
- runner example grace values leave the documented emergency and scheduling
  margin below the deployment's outer stop timeout

### Container qualification

Run each production image as PID 1, wait for readiness, send `SIGTERM`, and
assert:

- the service logs receipt of `Terminate`
- no-load shutdown completes within the target fast-path threshold
- in-flight work follows its drain policy
- a normal drain exits zero before the engine timeout
- a deliberate application deadline expiry exits `1`, not `0` or `137`
- container inspection does not report an out-of-memory kill or exit code 137

Run the matrix with Docker and Podman because signal forwarding and wrapper
entrypoints are deployment concerns, not only Rust unit-test concerns.

## Acceptance Criteria

The design is complete when:

- every production Rust service handles orchestrator `SIGTERM`
- signal handlers are installed before readiness is published
- a signal received during startup cancels and unwinds startup within the
  mandatory cleanup floor rather than waiting for bootstrap or registration
- no-load shutdown normally completes in less than one second
- application admission closes synchronously and returns the defined `503`
  before deregistration performs network I/O; TCP refusal follows bounded
  deregistration when transport drain starts
- in-flight HTTP work drains up to `server.shutdownGracefulPeriod`
- long-lived streams and background workers cannot delay exit beyond the bound
- `SIGINT` remains functional for interactive development
- a second signal skips remaining drain and enters mandatory cleanup
- registry terminal state is entered before controller goodbye, no reconnect or
  re-registration occurs afterward, and deregistration is acknowledged or
  bounded before transport drain
- every inventoried durable resource owner is registered as a cleanup
  participant; an empty set is accepted only when the service inventory
  explicitly proves it owns no asynchronous cleanup
- lifecycle participants execute in deterministic reverse registration order
  and cleanup errors are aggregated
- the Pingora dependency resolves to crates.io `0.8.1`, both internal shutdown
  periods remain zero, and the outer join observes the shared remaining budget
- application logs distinguish graceful exit from deadline expiry
- normal container tests prove exit code zero without forced termination;
  deadline-expiry tests prove exit code `1`
- Compose and Kubernetes retain an outer timeout greater than the application
  deadline

## Operational Guidance

If a service consistently consumes the full container stop timeout, treat it as
a shutdown defect. Check, in order:

1. whether the Rust binary is PID 1 or receives forwarded signals
2. whether it installed a `SIGTERM` handler
3. whether the shutdown path stopped listener acceptance
4. which request, stream, task, or cleanup hook remains active
5. whether the application deadline is actually connected to that component

Lowering the container timeout can shorten the symptom, but it does not repair
the lifecycle. The correct steady state is a cooperative application that exits
as soon as its real work is safe, with the orchestrator deadline unused during
normal shutdown.
