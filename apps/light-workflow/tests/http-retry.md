# Durable HTTP response retries

For `cel-workflow-v2`, retry supports only a finite total attempt count and a
fixed delay. Count includes the initial dispatch, is an integer from 1 through
65535, and defaults to 1 (no retry). Delay defaults to zero; it is an object of
nonnegative integer `days`, `hours`, `minutes`, `seconds` and `milliseconds`.
Units use checked integer arithmetic. The summed delay must fit PostgreSQL's
signed-microsecond interval range (`i64::MAX / 1000` milliseconds), and adding it
to the admission clock must fit Chrono's finite UTC timestamp range. At scheduling,
the absolute timestamp is checked again against the database clock and bound
as a timestamp parameter; SQL performs no floating conversion or interval addition.
`update_ts` uses the same database-clock reading as `next_attempt_ts`, so their
difference records exactly the configured fixed delay even in a long transaction.
These are storage/runtime representation limits, not business timeouts.
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
and supported nested `do`, `for.do`, `try`, `catch.do` and fork branches. Missing
references are rejected. Retry placement is supported only on `call: http` tasks;
agent, MCP, JSON-RPC/OpenRPC, A2A, set, assert, wait, run and other task kinds reject
retry blocks, whether inline or referenced. An unused component is not placement.
Receipt recovery remains lookup-only, before mutable admission validation.

Existing immutable snapshots are not re-admitted during execution or completion.
Historically accepted unsupported policies on unrelated tasks/components, or on
non-HTTP tasks, do not reject successful native-agent, runner, timer, HTTP or other
completion. Non-HTTP tasks retain their historical behavior without new retries.
Only when an eligible completed failed HTTP response needs a retry decision do we
validate that task's raw policy and, for a reference, that one component. Unknown
fields and nested/unresolved references are not lost through typed conversion.
Unsupported persisted policy or an unrepresentable timestamp yields a durable,
terminal `WORKFLOW_RETRY_POLICY_UNSUPPORTED` through existing completion failure
machinery. No snapshots are rewritten. Transport/expression failures do not parse
a retry policy.

Only the HTTP adapter's internal typed provenance for a completed non-success
response can enter the configured response retry path. It follows the locked
HTTP library's full accepted status range (100–999), with no selective-status
or Retry-After policy. Success JSON, including an exact forged error envelope,
and unrelated completion producers never provide retry authority.
Connection/DNS/TLS/send/read/body-size errors remain terminal; this revision does
not add transport retries. Expression/assertion/export/output failures remain
terminal. Existing authority, cancellation, lease, deadlines and effect checks
remain mandatory.

Scheduling uses the earliest applicable task, process, invocation and inherited
action-authority deadline under the existing authority locks. Private execution
lifetime v1 retains its existing rule: the invocation's start envelope is not its
execution deadline. Successors with null task deadlines remain bounded by parents.
An attempt must start strictly before the deadline; equality refuses scheduling.
Refusal retains attempt count, retry timestamp and context, creates no successor,
and completes through the existing terminal machinery. Cancellation, expiry,
lease fencing, budgets and effect/idempotency checks remain in force.

A valid known v2 failed-response completion can schedule without an evaluator.
A worker lacking evaluator support defers undispatched work without consuming an
attempt. The evaluator-unavailable HTTP completion branch is defensive with today's
production producers: host claims filter supported profiles; `with_expression_engine`
sets capabilities during construction, and no production setter changes them;
`execute_task` checks support before the only adapter that sets `HttpResponse`
provenance. Native-agent, runner and timer results have `None` provenance. Thus an
unsupported host cannot produce this response in the current call graph. The
test-only capability mutation verifies bookkeeping, not production reachability.
The defensive branch remains evaluator independent, and success transitions still
require support. No host claims or future-profile grammar are widened.
Corrupt snapshots fail explicitly; unknown future profiles are deferred
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

The P01 timer suite keeps its separate exact URL contract:
`P01_TIMER_TEST_DATABASE_URL=postgres://fixture_admin:fixture_only@127.0.0.1:55431/p01_timer_gate`.
Its selected gate fails if prerequisites are missing. Both maintained suites run
in the dedicated `workflow-retry-and-timer` CI job with pinned PostgreSQL, tmpfs
storage and synthetic NOLOGIN roles. W4 keeps its schema-only URL contract.
Scratch child identifiers use `[a-z_][a-z0-9_]*` prefixes of 1..=30 ASCII bytes;
the underscore plus 32 UUID hex bytes keep the identifier within PostgreSQL's
63-byte limit. The helper validates before connecting or issuing SQL.
