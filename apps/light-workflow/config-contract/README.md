# Light Workflow Phase 0 Configuration Contract

These files began as the checked Phase 0 inventory and now remain the
source-of-truth contract through the Config Server/controller rollout.

- `configuration-inventory.yml` inventories statically named environment reads
  and every leaf in the embedded `server.yml`, `security.yml`, and `client.yml`.
- `dynamic-resolvers.yml` defines computed environment-name and provider-library
  resolver classes that cannot be enumerated from literal `env::var` calls.
- `identity-registration.yml` separates Portal/config, durable runner-origin,
  and controller runtime identities and pins the registration payload contract.
- `observability.yml` pins metric, structured-event, field, and redaction names.
- `characterization.yml` maps Phase 0 behavior claims to executable tests.
- `reload-lifecycle.yml` pins the Phase 4 aggregate module, generation capture
  points, atomic activation, restart-required handling, and rollback contract.
- `fixtures/` contains equivalent local and Config Server `values.yml` inputs.

Run `scripts/run-light-workflow-config-controller-phase0-gate.sh`. Pass a
disposable PostgreSQL URL to include the runner recovery/fencing cases:

```bash
scripts/run-light-workflow-config-controller-phase0-gate.sh \
  postgresql://postgres:postgres@localhost:5432/workflow_phase0
```

The inventory gate intentionally fails when a new static environment read or
embedded YAML leaf is added without an ownership decision.
