# Durable HTTP response retries

For `cel-workflow-v2`, retry supports only a finite total attempt count and a
fixed delay. Count includes the initial dispatch, is an integer from 1 through
65535, and defaults to 1 (no retry). Delay defaults to zero; it is an object of
nonnegative integer `days`, `hours`, `minutes`, `seconds` and `milliseconds`.
Units are added with checked arithmetic and must fit signed milliseconds.
No retry window is implied.

An inline policy or a string reference into `use.retries` has identical semantics:

```yaml
use:
  retries:
    fixed:
      limit:
        attempt:
          count: 3
      delay:
        seconds: 2
do:
  - fetch:
      call: http
      with:
        method: GET
        endpoint: http://example.invalid/fixture
      retry: fixed
```

V2 admission rejects `when`, `exceptWhen`, `backoff`, `jitter`,
`limit.duration`, `limit.attempt.duration`, nested policy `use`, aliases,
unknown keys and malformed policy/count/delay shapes. This applies to inline
policies, every declared retry component (including unused ones), references
and fork branches. Missing references are rejected. Current execution checks
immutable raw snapshots before dispatch so historical snapshots cannot silently
execute a policy that serde would partly discard. Receipt recovery remains
lookup-only and does not re-admit historical requests.

Only the HTTP adapter's internal typed provenance for a completed non-success
response can enter the configured response retry path. It follows the locked
HTTP library's full accepted status range (100–999), with no selective-status
or Retry-After policy. Success JSON, including an exact forged error envelope,
and unrelated completion producers never provide retry authority.
Connection/DNS/TLS/send/read/body-size errors remain terminal; this revision does
not add transport retries. Expression/assertion/export/output failures remain
terminal. Existing authority, cancellation, lease, deadlines and effect checks
remain mandatory.

A valid known v2 failed-response completion can schedule without an evaluator.
A worker lacking evaluator support defers undispatched work without consuming an
attempt. Corrupt snapshots fail explicitly; unknown future profiles are deferred
without parsing their definitions as current v2. Compensation retries preserve
COMPENSATING; existing host compensation-claim restrictions are unchanged.
Legacy admission semantics are unchanged. The legacy scheduler's pre-existing
use of `limit.duration` as a fallback inter-attempt delay, and lack of a correct
retry-window implementation, remain outside this v2 policy decision.

Ordinary tests require no PostgreSQL:

```sh
cargo test --locked -p workflow-expression --test retry_policy
cargo test --locked -p light-workflow --lib executor::http_failure_tests::
```

Durable cases are explicitly ignored in ordinary runs. Provision a dedicated
disposable PostgreSQL 17.11 fixture on loopback, with tmpfs data, fresh owned
resource names and an unused port; never use application volumes or credentials.
Pinned image:
`postgres@sha256:18cfe3ef5e6815560c98237d6216d1e5119702fb0f3894c8785dd58b8bbe5d73`.

The synthetic administrator must have CREATE/DROP DATABASE privileges, an empty
base named `workflow_retry_*`, and these fixture-only roles:

```sql
CREATE ROLE operations_workflow_migrator NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE;
CREATE ROLE operations_workflow_runtime NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE;
CREATE DATABASE workflow_retry_local;
```

Supply **HTTP_RETRY_TEST_ADMIN_URL**, separate from the schema-only W4 fixture
contract. The URL must use postgres, 127.0.0.1, a scratch-named base and no query:

```sh
export HTTP_RETRY_TEST_ADMIN_URL='postgres://fixture_admin:fixture_only@127.0.0.1:55451/workflow_retry_local'
cargo test --locked -p light-workflow --lib executor::http_retry_tests:: -- --list
cargo test --locked -p light-workflow --lib executor::http_retry_tests:: -- --ignored --test-threads=1
```

An explicitly selected gate fails on missing prerequisites; it never uses an
ambient DATABASE_URL or silently returns. The suite creates UUID-named child
databases, migrates on one fixture-migrator connection, and drops only its owned
children. Failed cases retain children for diagnosis inside the disposable fixture.
The small shared scratch lifecycle also serves timer tests, retaining that
suite's own strict URL and role contract. Each case keeps isolated migrations;
there is no shared mutable template or broad fixture framework.

Maintained real HTTP success/exhaustion cases execute every reclaimed attempt
through production preparation, send fencing, dispatch, effect re-claim and
completion. POST cases retain one idempotency key/request digest and one effect
row. Tests verify stable wire requests, increasing lease fences, retained step
snapshots, configured attempt/delay values, no failed-attempt exports/successors,
one successor after success and stale replay refusal. Scheduler interval checks
are separate from claim boundaries, controlled by persisted future/past timestamps;
no fixed sleeps assume transaction commit speed.

Further cases cover policy admission/persisted refusal, inline/reference parity,
nonstandard statuses, forged success bodies, terminal transport failures,
known-v2 evaluator-independent retry/deferral, corruption/future profiles,
compensation before exhaustion, and legacy/native/fork/expression compatibility.
Loop subcases are not separate Rust test counts.

The single-response/transport cases use the legacy-event dispatch seam. Real
reclaimed GET/POST cases retain a synthetic private-inline invocation and use the
existing injectable Dispatch authority interface, pinned to the fixture host and
process and checked on every attempt. Database authority/send fences and actual
effect rows remain. Gateway authorization is simulated; no grants or live binding
setup is involved. Native and fork/compensation state is synthetic. Passing these tests does not establish
runtime-role ACLs, authenticated start, Tool grants, Gateway bindings, a real
native runner, live GitHub/model execution, crash recovery, activation or deployed
behavior. The generic native output-schema validator is unchanged.

Other checks:

```sh
cargo fmt --all --check
cargo check --locked -p light-workflow --lib --tests
cargo clippy --locked -p light-workflow --lib --tests
cargo test --locked -p light-rule -- --test-threads=1
```

Compare Clippy with the integration baseline, normalizing source line shifts.
The final command is C11 compatibility, not database or deployed proof.
Normal PostgreSQL CI explicitly executes the ignored durable suite with its own
scratch base. YAML uses a folded run scalar for the test filter ending in ::.
