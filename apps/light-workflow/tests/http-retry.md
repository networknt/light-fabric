# HTTP completion retry regressions

The v2 completion path must schedule configured durable retries for a failed
HTTP response before terminal failure routing. It uses the existing scheduler;
this suite does not define a new status policy or retry transport exceptions.

The ordinary classifier tests run without PostgreSQL:

```sh
cargo test --locked -p light-workflow --lib executor::http_failure_tests::
```

The database suite is `src/http_retry_tests.rs`, a test-only executor child module
so it can exercise private completion, claiming and reconciliation methods. It
uses the existing `expression_test_support` permit/engine helpers, repository
Workflow migrations and a small synthetic definition. No G03 candidate,
qualification directory, external checkout, source overlay or application
database is required.

Database cases are explicitly `#[ignore]`d in ordinary runs. To execute them,
provision a **dedicated disposable PostgreSQL** instance on loopback with a
synthetic administrator able to create/drop databases, an empty base database
whose name starts with `workflow_retry_`, and these fixture-only roles:

```sql
CREATE ROLE operations_workflow_migrator NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE;
CREATE ROLE operations_workflow_runtime NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE;
CREATE DATABASE workflow_retry_local;
```

Use PostgreSQL 17.11, available as
`postgres@sha256:18cfe3ef5e6815560c98237d6216d1e5119702fb0f3894c8785dd58b8bbe5d73`.
Use tmpfs data, fresh owned resource names and an unused loopback port; never
mount application volumes or reuse application credentials. Inspect resource
identity before removing the fixture. Do not run the SQL against an application
instance. Supply its synthetic URL explicitly, for example:

```sh
export W4_TEST_DATABASE_URL='postgres://fixture_admin:fixture_only@127.0.0.1:55449/workflow_retry_local'
cargo test --locked -p light-workflow --lib executor::http_retry_tests:: -- --list
cargo test --locked -p light-workflow --lib executor::http_retry_tests:: -- --ignored --test-threads=1
```

The explicit gate fails when the variable is absent or the database is not
scratch-named/loopback. It never falls back to `DATABASE_URL`. Each case creates
a UUID-named child database, applies all Workflow migrations on one migrator
connection, seeds a DRAFT definition and inactive binding, and drops only its
child after success. Failed cases may leave children for diagnosis; removing
the owned disposable instance clears them. Tests use the fixture administrator,
so passing them does not establish effective runtime-role ACL qualification.

The normal PostgreSQL CI job creates its own `workflow_retry_ci` base and runs
this gate. The ordinary workspace suite discovers the classifier cases and
reports the database cases as ignored rather than successful early returns.

Coverage includes attempts 1→2→3, configured delay, early-claim refusal,
reconstructed executor recovery, retry success/exhaustion, no failed-attempt
exports/advancement, one successor on success, stale completion refusal, legacy
retry, terminal expression/assert/export/output failures, profile/snapshot and
authority/lease/deadline/cancellation fences, effect/idempotency restrictions,
real local 503/404/429 responses, a forged error envelope in a 200 response,
native pending/terminal replay and invalid-output assertion, and exhausted
fork/compensation bookkeeping. Table/loop cases are subcases, not additional
Rust test counts.

The local HTTP case uses the legacy-event dispatch seam with its seeded
invocation removed; process/task fences remain. Native producer results and
fork/compensation state are synthetic. These tests do not establish authenticated
start, Tool grants, Gateway bindings, a real native runner, live GitHub/model
execution, process-crash durability, v2 activation or deployed behavior. They
do not change native public-output schema validation.

Other relevant checks, from the repository root:

```sh
cargo fmt --all --check
cargo check --locked -p light-workflow --lib --tests
cargo clippy --locked -p light-workflow --lib --tests
cargo test --locked -p light-rule -- --test-threads=1
```

Clippy has existing diagnostics; compare warning file/code/message multisets
with the integration baseline, ignoring source line shifts. The final command
is the E03 C11 compatibility lane, not a database or deployed-runtime proof.
