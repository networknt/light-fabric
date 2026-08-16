# Graceful Service Shutdown

Status: Proposed

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
HTTP middleware and worker claim loops must consult that gate and reject new
application work while shutdown is in progress. Health endpoints may remain
available long enough to report `not ready`.

The runtime then sends the bounded deregistration request. Once it is
acknowledged or its small bound expires, transport drain begins. Upstream
readiness propagation delay is not modeled as a sleep and does not create a
second grace period. Time used by deregistration is charged against the one
application deadline.

For Axum, `Handle::graceful_shutdown` combines listener close and connection
drain. The observable `Quiescing` phase therefore comes from the runtime state,
admission gate, deregistration event, and phase metrics, not a distinct Axum
transport state. The handle is invoked after the bounded deregistration step.

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

A graceful deadline expiry cancels the shared shutdown context. The supervisor
then allows only `MANDATORY_CLEANUP_FLOOR` for emergency cleanup that was
prepared in advance, emits a final deadline-exceeded record to stderr, and
calls `std::process::exit(1)`. Calling `process::exit` is intentional: merely
returning an error can still hang while Tokio drops a runtime that owns an
unbounded `spawn_blocking` task such as Pingora's server-thread join. Exit code
`1` means application shutdown failure; exit code `137` still means the
container engine had to send `SIGKILL` and is a stronger qualification failure.

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

`LightRuntimeBuilder::run_until_shutdown` owns cancellable startup, readiness
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
impl<T: TransportRuntime> LightRuntimeBuilder<T> {
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
available for tests, embedding, and programmatic lifecycle management.

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
    Running,
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
design, not a prerequisite assumed to exist. Phase 1 must inventory these
ownership classes and register each concrete resource that is present:

- database-pool owners that need an awaited `Pool::close()` rather than only
  dropping handles
- gateway and knowledge durable audit, embedding, batch, or write-behind
  buffers that the inventory proves must flush or checkpoint
- the portal registry client, after its explicit deregistration acknowledgement,
  so its reconnect task and socket are closed rather than merely aborted
- application-owned task supervisors that must cancel and join spawned work

Lifecycle registration must be transport-neutral. Add `LifecycleRegistry` and
a cloneable, registration-only `LifecycleRegistrar` capability to
`light-runtime`. The registry owns the ordered participant set; the registrar
can add a participant during startup but cannot enumerate, invoke, or seal the
set. Builder-supplied modules are inserted into the same registry.

Both transport construction paths receive the registrar alongside
`&RuntimeConfig`:

```rust
pub trait TransportRuntime {
    async fn bind(
        &self,
        config: &RuntimeConfig,
        lifecycle: &LifecycleRegistrar,
    ) -> Result<BoundTransport<Self::Handle>, RuntimeError>;
}

pub trait PingoraApp: Send + Sync + 'static {
    type Proxy: ProxyHttp + Send + Sync + 'static;

    fn proxy(
        &self,
        config: &RuntimeConfig,
        lifecycle: &LifecycleRegistrar,
    ) -> Result<Self::Proxy, RuntimeError>;
}
```

Light Axum's `ServerContext` re-exposes a clone of this light-runtime registrar
to `AxumApp::router()`. It does not own or define the registry contract. Light
Pingora passes the same registrar to `PingoraApp::proxy()`, which closes the
construction-order gap for light-gateway proxies that create durable buffers,
pools, or clients. Standalone applications can construct the same light-runtime
registry directly without depending on either framework context type.

The runtime seals the registry atomically at the transition to `Ready`.
Registration after sealing is an error. Startup cancellation seals the registry
against new participants before invoking the already-registered participants'
abort cleanup.

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
consistent. It must handle both `Running` and `StartupAbort`; the latter may be
called before the overall service reaches readiness. `Emergency` permits only
the prearranged bounded cleanup described by the mandatory floor.

### Runtime shutdown ordering

`RunningRuntime::shutdown()` should perform a single deadline-aware sequence:

1. create the absolute graceful and hard deadlines and shared
   `ShutdownContext`
2. transition runtime state to `Quiescing`, mark readiness false, and close the
   admission gate synchronously
3. send an explicit bounded deregistration/goodbye and wait for acknowledgement
4. stop the registration reconnect loop and close its WebSocket cleanly
5. ask the transport to stop accepting connections and drain existing work
6. invoke deadline-aware module hooks with the same context
7. log the duration and final outcome, or enforce process exit on expiry

The current `registration_task.abort()` is not sufficient. Add a
`PortalRegistryClient::deregister` lifecycle operation and corresponding
controller protocol support. Receipt must remove the instance from routing
immediately and return an acknowledgement. Its bound is
`min(context.remaining(), registration_timeout)`, where the existing builder
registration timeout defaults to five seconds. For the normal two-second
shutdown setting, the remaining application deadline is therefore the tighter
bound. Failure or timeout is logged, the socket is closed, and shutdown
continues; deregistration never creates an additional deadline.

If a hook needs ordering, the module contract must declare it or the runtime
must document a stable reverse-startup order. Independent hooks may run
concurrently when doing so cannot violate resource ownership.

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

The listener stops accepting new connections when shutdown starts. Existing
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

`PingoraTransport` already maps `server.shutdownGracefulPeriod` to Pingora's
graceful shutdown timeout and uses a controlled shutdown channel. The current
implementation is not yet capable of the required fast path. In pinned
`pingora-core 0.8.0`, `GracefulTerminate` calls `shutdown_timeout(...)` and then
sleeps for the complete configured timeout before joining the shutdown helper
thread. Consequently a two-second configuration imposes roughly two seconds
even when there are no active exchanges; this is a fixed sleep, not a polling
interval.

Phase 1 must resolve that dependency before claiming Pingora conformance. The
acceptable options are, in preference order:

1. upgrade to a Pingora version that returns as soon as its runtimes drain
2. upstream or carry a narrowly scoped patch that removes the unconditional
   post-shutdown sleep while preserving the maximum timeout
3. implement a drain-aware Light Pingora shutdown path with explicit in-flight
   tracking

Returning Pingora's `FastShutdown` is not an acceptable normal-path workaround
because it forfeits request draining.

The migration must verify that:

- the controlled signal stops listener acceptance immediately
- with no active downstream exchange, transport stop completes in less than one
  second
- active proxy requests can finish inside the deadline
- WebSockets and streaming exchanges cannot exceed the deadline
- the Pingora thread is joined before module cleanup completes

Pingora accepts whole seconds. Keep the current `div_ceil(1000)` behavior rather
than rejecting existing sub-second configurations; light-gateway tests already
exercise `shutdownGracefulPeriod: 100`. A 100 ms value has an effective Pingora
component bound of one second. Log both configured and effective values when
rounding occurs. The top-level application deadline is still measured in
milliseconds and may expire before Pingora's rounded bound.

For the pinned `pingora-core 0.8.0`, `Some(0)` becomes a zero-duration runtime
shutdown and a zero-duration sleep; it does not mean wait forever. A transport
test must pin this behavior so a dependency upgrade cannot silently change the
zero case.

The separate `grace_period_seconds` setting is equally load-bearing. Pingora
performs another unconditional sleep before runtime shutdown and defaults a
missing value to `EXIT_TIMEOUT`, currently five minutes. `light-pingora`
explicitly sets `grace_period_seconds = Some(0)`; Phase 1 must preserve that
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

### Phase 1: Shared primitive and runtime transports

1. add eagerly installed `ShutdownWatcher`, `ShutdownReason`, and
   `ShutdownContext` to `light-runtime`
2. make the low-cost deadline-aware `Module::on_shutdown` trait change, confirm
   the known implementation set remains empty, inventory durable resources,
   and register the initial cleanup participants in the transport-neutral
   `LifecycleRegistry`
3. pass `LifecycleRegistrar` through both `AxumTransport`/`ServerContext` and
   `PingoraTransport`/`PingoraApp`, then seal it at `Ready`
4. add cancellation-aware startup phases, `StartupGuard`, and the top-level
   `LightRuntimeBuilder::run_until_shutdown` deadline/exit enforcer
5. add bounded portal deregistration and controller acknowledgement support
6. apply the shared remaining deadline in `light-axum`
7. resolve Pingora's unconditional full-timeout sleep and verify its rounded
   timeout, `grace_period_seconds = Some(0)`, and zero-duration behavior
8. add signal, startup-abort, lifecycle-registry, runtime, and transport
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
application code. Controller deregistration acknowledgement is also an explicit
cross-repository dependency on `controller-rs`.

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
- new connections are rejected after quiescing begins
- streaming and WebSocket connections obey the bound
- module shutdown hooks run after listener quiescence
- the whole runtime reaches its explicit deadline outcome even if transport
  stop or an underlying thread join does not cooperate

The Pingora no-load assertion is a release gate, not an aspirational timing
description. It must fail against the current unconditional sleep in pinned
`pingora-core 0.8.0` until Phase 1 resolves that dependency. The test also
records actual latency so a dependency upgrade cannot introduce a fixed poll
or sleep near one second. Configuration tests assert both
`grace_period_seconds == Some(0)` and the intended
`graceful_shutdown_timeout_seconds`, including its zero case.

### Module and deregistration tests

- every module receives the same absolute deadline and cancellation token
- the production cleanup participant registry matches the reviewed resource
  inventory and is empty only for an explicitly resource-free service
- a cooperative hook checkpoints before cancellation and returns
- a deliberately stuck hook triggers the global backstop and exit-code-1 path
- cleanup errors are aggregated without skipping later bounded hooks
- the runtime closes admission before sending deregistration
- the controller acknowledgement removes the instance from routing before
  transport drain starts
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
- in-flight HTTP work drains up to `server.shutdownGracefulPeriod`
- long-lived streams and background workers cannot delay exit beyond the bound
- `SIGINT` remains functional for interactive development
- a second signal skips remaining drain and enters mandatory cleanup
- controller deregistration is acknowledged or bounded before transport drain
- every inventoried durable resource owner is registered as a cleanup
  participant; an empty set is accepted only when the service inventory
  explicitly proves it owns no asynchronous cleanup
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
