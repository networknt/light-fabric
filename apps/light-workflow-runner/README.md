# light-workflow-runner

`light-workflow-runner` executes fenced workflow and agent actions through an
operator-selected execution backend. The first implementation uses the mock
backend to prove registration, recovery, result normalization, and cleanup
without credentials or host shell interpretation.

Set `LIGHT_WORKFLOW_RUNNER_CONFIG_FILE` to a runner YAML file. Start from
`config/runner.example.yml`. The JWT file must be a regular file containing one
token and, on Unix, must have mode `0600` or stricter.

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
