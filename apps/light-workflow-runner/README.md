# light-workflow-runner

`light-workflow-runner` executes fenced workflow and agent actions through an
operator-selected execution backend. The first implementation uses the mock
backend to prove registration, recovery, result normalization, and cleanup
without credentials or host shell interpretation.

Set `LIGHT_WORKFLOW_RUNNER_CONFIG_FILE` to a runner YAML file. Start from
`config/runner.example.yml`. The JWT file must be a regular file containing one
token and, on Unix, must have mode `0600` or stricter.

For Cube Sandbox, start from `config/runner.cube.example.yml`. Backend selection
is configuration-driven: the legacy mock shape selects the deterministic mock;
`implementation: cube` selects the authenticated Cube HTTP/ConnectRPC client.
The Cube API key is read only from `apiKeyFile`, which receives the same regular
file and private-permission validation as the runner JWT. HTTP is rejected by
default; `allowInsecureHttp` exists only for an explicitly isolated local Cube
development network.

The Cube client creates one secure ephemeral sandbox with `onTimeout: kill`,
public traffic disabled, IPv4/IPv6 deny-all egress, no environment secrets, and
the complete fenced ownership metadata. It resets Cube's idle timeout to the
remaining absolute lease duration immediately before command execution, uses
the E2B-compatible binary ConnectRPC process service without reconstructing a
shell command, bounds the full response plus stdout/stderr, and performs
idempotent synchronous deletion. Metadata lookup is bounded to Cube's current
200-item API limit; multiple idempotency matches fail as `UNKNOWN`.

The Cube client can upload digest-verified immutable regular-file inputs through
envd 0.5.7 or newer. The first coding fixture uses a Git bundle uploaded to the
fixed `/inputs/repository.bundle` path; the guest verifies its digest and base
commit before creating a writable checkout. Directory inputs and extracted
skill-package trees still fail closed until an approved remote materializer is
configured. Cube artifact collection is likewise not advertised until a
trusted export mechanism is installed.

The controller admission file must contain digests calculated from the exact
runner binary and effective configuration. Generate the complete admission
document instead of copying or manually recreating those digests:

```bash
LIGHT_WORKFLOW_RUNNER_CONFIG_FILE=/etc/light-workflow-runner/runner.yml \
  light-workflow-runner print-admission \
  urn:lightapi:runner:local-runner light-workflow
```

Install the resulting JSON at the controller's
`CONTROLLER_RUNNER_ADMISSION_PATH`. The runner JWT `sub` must equal the first
argument. Its host, runner ID, enrollment ID, audience, and `runner.connect`
scope must also match the controller and generated admission.

Run the service without arguments after enrollment:

```bash
LIGHT_WORKFLOW_RUNNER_CONFIG_FILE=/etc/light-workflow-runner/runner.yml \
  light-workflow-runner
```

The readiness endpoint is `/readyz`; liveness and cleanup evidence are exposed
at `/healthz` on `healthAddress`.

Before opening its health listener or controller transport, the runner performs
a bounded backend orphan-reconciliation pass. Startup fails if discovery or
cleanup cannot complete within `orphanReconcileStartupTimeoutMs`; consequently
an OCI runner cannot accept a new lease while stale owned containers remain
unexamined. Later passes run every `orphanReconcileIntervalMs` (minimum one
second). A failed periodic pass makes readiness and health fail until a
subsequent pass succeeds. OCI discovery is ownership-label scoped, retains all
operations still present in the durable journal, and deletes only resources
whose runner-owned expiry label has elapsed.

Agent worker execution is disabled unless `agentWorker` is present. That block
pins the admitted origin service ID, absolute worker path, exact binary digest,
and canonical capability digest. Agent leases cannot choose or override those
values. The runner clears the child environment, owns its stdin/stdout pipes,
binds every event to the execution ID, lease ID, fencing token, and a fresh
transport nonce, journals events before accepting terminal state, bounds event
and stderr frames, and kills the entire process group on cancellation or the
earlier of the worker and lease deadlines. Enabling the block also adds only
the configured agent origin and `agent-turn`/`agent-action` subjects to the
generated controller admission document.

The optional nested `broker` block keeps model, network, and credential
authority in the runner. A lease must carry a matching attempt grant; otherwise
worker launch fails closed. The worker receives only a mode-`0600` Unix-socket
path. The broker verifies the socket peer is the exact worker PID before it
parses a request, so generated subprocesses cannot reuse the channel. Requests
are bound to execution, lease, fence, policy, data boundary, operation, target,
expiry, request count, response size, and model token/cost ceilings. Routes are
operator-owned aliases, redirects are disabled, and provider credentials are
read from owner-only regular files and injected only into the outbound request.
Neither a bearer token nor provider endpoint authority enters the worker
environment. Model routes must use a trusted `costPer1kTokensMicros`; the
broker derives a conservative charge from the admitted output ceiling and
request size rather than trusting worker-reported usage.

Broker admission, charged usage, and terminal response evidence are persisted
in the runner's synchronous SQLite journal before authority is exercised or a
response is returned. Reusing a completed request ID, including after runner
restart, returns the exact recorded response without calling the provider
again. Network and credential routes also receive a runner-derived
`Idempotency-Key`. If the runner cannot prove whether an admitted upstream call
completed, it durably marks that request `UNKNOWN`, terminates the worker, and
reports the execution attempt as `UNKNOWN`; it never silently reissues the
effect. Origin reconciliation must resolve that request ID through provider
status evidence or operator policy before scheduling new work.

Treat the configured executable as part of the runner's trusted computing
base. Its file and every parent directory must be owned by the deployment
administrator and not writable by the runner account or workload users;
production images should provide it from an immutable, read-only layer. The
pre-spawn digest check detects drift but is not a substitute for filesystem
ownership and immutability, which prevent path replacement between validation
and process creation.

For the credential-free local vertical slice, start from
`config/runner.mock.yml` and use the workflow-side
`apps/light-workflow/config/runner-execution.mock.yml` and
`apps/light-workflow/examples/run-shell-mock-v1.yaml`. The compatibility digest
and canonical `print-message` template digest already match across these local
fixtures. Replace the host, JWT, and enrollment identity, then generate the
admission document. The placeholders in `runner.example.yml` are intentionally
not runnable.

## Full-stack integration test

`tests/controller_cube_full_stack.rs` joins the real `controller-rs`
PostgreSQL scheduler and lease runtime, runner WebSocket transport, supervisor
and SQLite journal, and the Cube execution backend around a deterministic
in-process Cube service. It verifies reservation, fenced lease accept/start,
execution, terminal-result persistence and acknowledgement,
and confirmed backend cleanup.

Run it only against a disposable Portal database with the current canonical
schema:

```bash
DATABASE_URL=postgres://postgres:secret@localhost:5432/portal_test \
  cargo test -p light-workflow-runner --test controller_cube_full_stack -- --nocapture
```

Without `DATABASE_URL`, the test is compiled and skipped. The fixture uses
dedicated origin, runner, workflow-instance, and policy identifiers and removes
its rows after completion or before a rerun.
