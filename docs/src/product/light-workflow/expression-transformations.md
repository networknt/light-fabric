# Bounded Workflow Expressions — Evaluator Decision Pending

Status: proposed design for review, with acceptance criteria revised by the
owner on 2026-10-02 (see below). Extended CEL was selected at E02. The E03
production contract for profile `cel-workflow-v2` (revision 3) is in
`implementation/light-workflow/2026-10-02-ExpressionEvaluatorE03ProductionContract.md`. It is summarized in 2.1, 2.3, 2.5, 2.7, 5.1 and 6; that contract
governs the details. No evaluator is selected and no runtime
support is implemented. Every numerical budget in this document is a proposal
pending review, not an established guarantee.

Tracking: [light-fabric #431](https://github.com/networknt/light-fabric/issues/431).
Source baseline: `900ddd1a4aa94540de272ecc03a47d3357bd3db5`, plus the locally
uncommitted test-only G03 probes in `apps/light-workflow/src/executor.rs`.

## Owner decision 2026-10-02: revised operating model

After reviewing the E01 CEL investigation, the owner revised the acceptance
criteria for **both** candidates. This is an explicit owner decision made after
results were seen, not an executor adjustment. It is recorded here and in the
E01 plan, §13. The original E00 criteria are kept below and labelled
**historical**. Where historical text conflicts with this section, this section
governs.

- **Operating model:** workflow definitions are written by trusted users and
  reviewed before publishing. Arbitrary untrusted users cannot publish
  executable expressions. If that changes, strong isolation must be revisited
  before such publishing is allowed.
- **Strong isolation is future hardening, not a prerequisite.** Provable
  per-expression work and allocation bounds, and a separate evaluator process
  (3.3), are not required for expression support. Neither CEL nor jq is
  designed as a sandbox, and neither is required to become one. The product
  must not advertise hard per-expression isolation.
- **Still required:** capability isolation (2.9), correctness, the size limits
  in 4.3, predictable errors, and reviewed publication.
- Pathological expressions can still exhaust a worker's CPU or memory. That is
  an operational limitation, contained by worker and container limits. An OOM
  can interrupt other work in the same process.
- Evidence already collected is preserved. The E01 CEL result stays "Fail
  against the original strict criteria". Reassessment under these criteria is
  recorded separately.

Revised gates, identical for both candidates (full definitions in 4.4):

| Gate | Revised status |
| --- | --- |
| F1–F9, projection, input limits, capability isolation, result contract, existing CEL unchanged, effort and modification ceilings | Required, unchanged |
| Allocation peaks on realistic fixtures, performance | Reported, not pass/fail |
| Amplification outcomes, work/allocation enforcement arguments | **Not executed in the current scope** (scope reduction below); earlier results kept as history |
| Determinism | Required for outcomes the evaluator *returns* on ordinary fixtures and limit-boundary cases |
| Crash resistance | **Unqualified.** Documented limitation and future hardening item (scope reduction below). The earlier draft's crash-free gate is withdrawn |
| JSON number boundary | **New, required** (2.3) |
| Error-category mapping | **New, required** (2.7) |

### Scope reduction, 2026-10-02 (owner-approved)

Later the same day, the owner removed deliberate crash and resource-exhaustion
testing from the current spike, **equally for CEL and jq**, under the
trusted-author, reviewed-publication model.

- **Not executed:** deliberate crash, OOM, timeout and resource-amplification
  executions, including repeated guard stress probes. The A1–A10 cases (4.2)
  and the guard-based A-case runs are out of the current scope. A4's
  compile-time limit checks are kept as ordinary validation tests (see below).
- **Kept:** ordinary correctness tests, realistic G03 fixtures (F1–F9,
  projection), result-contract cases, bounded validation tests for the
  configured limits (input-limit boundaries; source, AST and nesting limits),
  static source review, and benchmarks on realistic fixtures.
- **Unqualified, not passed:** crash resistance and resource isolation. No
  claim of crash resistance or strong resource isolation is made. Parser
  safeguards are assessed by source review and ordinary validation tests only.
- **Known observation:** the E01 CEL run had a stack overflow in a debug test
  build on a 2 MiB thread while compiling a large expression. The input was not
  kept. This is recorded as a limitation, not investigated further in the
  current scope.
- Historical results, including the earlier amplification and guard evidence,
  are preserved unchanged.
- Crash resistance and resource isolation are future hardening (5.1).

## Purpose

Workflow definitions need generic JSON transformations over ordinary tool
results: string splitting and extraction, field selection, array projection,
JSON serialization and UTF-8 size accounting. These operations are reusable
across API integrations and belong in the definition, not in provider-specific
engine code.

The immediate consumer is the GitHub issue-to-design workflow (G03). Gateway
continues to own authentication, ACL enforcement, upstream credentials and
routing. The engine does not acquire a GitHub provider, capture records,
receipts or integration-specific authorization.

Two candidates are compared on equal terms:

- **Extended CEL**: standard CEL extensions and bounded comprehensions added to
  the existing workflow value profile.
- **In-process jq**: a Rust jq implementation, for example `jaq`, selected by
  `evaluate.language: jq`.

Neither outcome is predetermined. This document authorizes no spike execution,
runtime change, publication or deployment. Approval of this revision is a
documentation decision only.

## 1. Executable expression-field inventory

This inventory records how expressions execute at the baseline. Both candidates
must account for every row. Line numbers drift, so rows cite functions.

### 1.1 Admission

`validate_runtime_definition` (`runtime_definition.rs`) accepts only an explicit
`evaluate.language: cel`. It rejects other languages and an omitted `evaluate`
block. It does not inspect `evaluate.mode`.

`workflow-core` defaults a missing `language` to `jq` during deserialization,
so `evaluate: {}` currently reaches validation as jq and is rejected. Any change
must preserve that rejection by checking the original definition, not the
defaulted model.

Admission performs no expression compilation for most fields. Invalid
expressions are discovered at run time, where most of them fall back silently
(see 1.3).

### 1.2 Expression sites

| DSL position | Executor entry | Context (`.` / CEL variables) | Result use |
| --- | --- | --- | --- |
| `set` map values and `set` expression | `resolve_json_value` | run context | task output |
| `call: http` endpoint URI | `resolve_template_to_string` | run context | string |
| `call: http` endpoint `{name}` placeholders | rewritten to `${{ name }}`, then template | run context | string |
| `call: http` `body` | `resolve_json_value` | run context | JSON |
| `call: http` `query`, `headers` | `resolve_http_string_map` | run context | strings |
| `idempotencyKey` (HTTP, MCP, A2A) | `resolve_template_to_string` | run context | string |
| `call: jsonrpc`/`openrpc` URI, `params`, `headers` | template / `resolve_json_value` | run context | JSON / strings |
| `call: mcp` `params`, resource URI | `resolve_json_value` / template | run context | JSON / string |
| `call: a2a` `parameters` | `resolve_json_value` | run context | JSON |
| `call: agent` `input`, `mockOutput` | `resolve_json_value` | run context | JSON |
| Agent `instructions`, prompt | `resolve_template_to_string` | run context | string |
| `ask` assignment category, reason, assignee, role | `resolve_template_to_string` | run context | string |
| `switch` case `when` (or the case name when `when` is absent) | `evaluate_condition` | run context | boolean |
| `assert` `value`, `equals`, `contains` | `resolve_json_value` | run context | JSON |
| `assert.json` comparison expression | `evaluate_condition` | **the selected value**, not the run context | boolean |
| `export` map values | `apply_exports` | see 1.4 | context entries |
| Workflow `output.as` string | direct `evaluate_cel_value` | final run context | public output object |
| Workflow `output.as` object | `resolve_json_value` | final run context | public output object |

`call` output selectors (`output: result | response | raw`) and `assert.json`
paths are fixed selectors handled by `lookup_json_path`, not expressions.

The run context starts as the invocation input (`process_info_t.context_data`
is initialized from `input_data`). Task output enters the context only through
`export`.

### 1.3 Template and fallback layer

`TEMPLATE_REGEX` recognizes `${{ ... }}` and `${ ... }`. A whole-string
template keeps the JSON type of its result; embedded templates are stringified
into the surrounding string. The regex does not understand quotes or nested
braces.

`evaluate_expression_to_value` is not a pure CEL evaluator. In order, it:

1. tries the CEL value profile when the expression does not start with `.`;
2. evaluates as a predicate if a comparison operator appears in the text;
3. treats `.a.b` as a jq-style path lookup;
4. parses `true`, `false` and `null`;
5. parses a quoted string;
6. parses a number as **f64**;
7. returns the expression text itself as a string.

A failed template is replaced by its original text. `evaluate_condition`
tries a CEL predicate, then splits on the first comparison operator and
compares with f64 arithmetic, and finally applies truthiness. A failed operand
becomes `null` and a failed expression becomes `false`.

Consequences:

- Existing "CEL" definitions may already contain jq-shaped `.path`
  expressions.
- A typo in a template produces literal text or `false`, not a failure.
- The f64 parse and the f64 comparison contradict the CEL value profile, which
  rejects doubles.

### 1.4 Export semantics

`get_export_map` reads `export.as` (or `export`) from the raw YAML into a
`HashMap`. `apply_exports` then, per entry:

- `.output` → the whole task output;
- `.output.<path>` → a path lookup into the task output;
- anything else → `evaluate_expression_to_value` against the context being
  built, which already contains previously applied exports.

Because `HashMap` iteration order is unspecified, an export that reads another
export key from the same map is nondeterministic. Failed lookups are skipped
silently. Exports cannot evaluate a general expression over the task output.

### 1.5 Parsed but not executed

`light-workflow` never reads task-level `if`, `input`, or `output`, and the
workflow-level `input` is not executed as a transformation. Admission does not
reject them, so a definition that uses them is accepted and the field is
ignored. This is an existing gap. New-profile admission must reject these
unsupported fields; implementing them is not required by this work. Existing
legacy definitions and runs retain their behavior. Broader legacy validation
changes require separate review.

`run.script` is accepted by admission and policy mapping. That does not
establish a qualified script evaluator, and neither candidate uses it.

### 1.6 Existing CEL value profile

`evaluate_cel_value` (`crates/light-rule/src/engine.rs`) compiles with the
standard profile, rejects comprehensions, caps the AST node count and converts
results through the workflow JSON profile. That profile rejects doubles, unsafe
integers, non-string map keys and opaque values on output. Inputs are added as
CEL variables, one per top-level context key that is a valid identifier. The
installed context lacks string splitting, substring extraction and JSON
serialization. Evaluation is wrapped in `catch_unwind`; there is no runtime
work or memory metering.

## 2. Shared contract, independent of evaluator

These rules apply to whichever candidate is selected.

### 2.1 Language selection

- One language per definition through `evaluate.language`. No per-task
  selector, mixing or automatic detection.
- An omitted `evaluate` block, `evaluate: {}`, an unknown language and any
  nonempty `evaluate.mode` are rejected through every admission and
  publication path for the new profile. Existing legacy definitions and runs
  retain their current validation and execution behavior; this does not
  retroactively reject an existing CEL definition using `evaluate.mode`.
- If extended CEL is selected, it remains `language: cel`, with a new
  evaluator profile (2.6). If jq is selected, `language: jq` is accepted only
  after implementation.
- A CEL definition opts into the new profile only through an explicit selector
  in the definition. Because legacy and new CEL definitions share
  `language: cel`, the profile is never inferred from publication date, DSL
  version or any other implicit signal, and republishing an existing definition
  without the selector keeps the legacy profile. **E03 selector:**
  `document.metadata.lightExpressionProfile: cel-workflow-v2`, together with
  `evaluate.language: cel` and no `evaluate.mode`. An absent selector means the
  implicit legacy profile `cel-workflow-v1`. An unknown or disabled identifier
  is rejected with `EVALUATOR_PROFILE_UNSUPPORTED`, never downgraded to
  legacy. The key is reserved: E04's migration preflight aborts if a stored
  definition or snapshot already contains it.

### 2.2 Results

- A value position produces exactly one JSON value. JSON `null` is a value.
  For jq, zero results and two or more results fail; the evaluator consumes
  enough of the stream to detect a second result or a subsequent error.
- A predicate produces exactly one boolean. There is no truthiness conversion.
- An embedded template must produce a string; other values are serialized
  explicitly in the expression.
- The new profile has no literal-text fallback, no `false` fallback and no
  silent truncation. Existing CEL definitions keep the 1.3 behavior unless an
  explicit migration is approved.

### 2.3 Numbers

Numbers are handled per stage, not by validating the whole context:

- **Input preservation**: values the expression does not touch pass through
  unchanged, including decimals and large integers. An unrelated decimal in the
  context must not fail an expression.
- **Input conversion** (revised 2026-10-02): an integer that fits `i64`
  becomes the language's signed integer (CEL `Int`). An integer in
  `(i64::MAX, u64::MAX]` is preserved exactly (CEL `UInt` is acceptable), or
  represented so that any operation reading it returns an error. Rejecting the
  whole input is not allowed (see input preservation above). A non-integer
  passes through as the language's floating type. Conversion never truncates,
  wraps, rounds or reinterprets a number.
- **Arithmetic** (revised 2026-10-02): inside the language, arithmetic follows
  that language's documented semantics. CEL integer division truncates toward
  zero and integer overflow is an error; jq division yields a float. These
  results are recorded, not treated as failures. Extension functions the
  profile adds use checked arithmetic and no truncating casts. Only silent
  corruption at the JSON boundary fails.
- **Output (production, `cel-workflow-v2`, E03):**
  - integers are emitted exactly over the full `i64`/`u64` range;
  - finite doubles are emitted in shortest round-trip form;
  - NaN, ±Infinity, non-string map keys and non-JSON values fail with the
    JSON-profile category.

  Values are preserved as parsed, not as original text. Consumers that use
  binary64 numbers lose precision above 2^53; a public output schema protects
  them only if it constrains the range or declares such fields as strings.
  v2 adds checked `double()`, `int()` and `uint()` conversions; implicit
  int/double mixing stays an error. See the E03 contract §4.
- *E01 qualification output boundary (historical, not the product policy):*
  "Integers in `[-9007199254740991, 9007199254740991]` are emitted exactly.
  Integers outside that range, floats and non-string map keys fail with the
  invalid-JSON-profile category." The spike evidence was collected under that
  boundary and is unchanged.
- **Number precision before conversion:** in E01, the shared harness parses
  JSON with `serde_json` without `arbitrary_precision`, so integers beyond the
  `u64`/`i64` range, and decimals, arrive as `f64` for both candidates. This is
  recorded as a harness property, not a candidate failure. Production
  integration must decide its own parse precision.
- *Historical E00 arithmetic rule (superseded 2026-10-02):* "operations on
  numbers outside the safe integer range `[-9007199254740991,
  9007199254740991]`, or producing a fraction, fail rather than rounding.
  Whether decimal arithmetic is supported at all is a spike output."

### 2.4 Scope and size

- Only ordinary workflow JSON crosses the evaluator boundary: no token caches,
  bearer tokens, LONG registrations, credentials, authority objects or service
  handles.
- The run context is converted into evaluator form once per task step and
  shared by that step's expressions, not once per expression.
- The proposed spike input ceiling is **2 MiB of compact UTF-8 JSON**, depth 64
  and 100,000 JSON nodes, across context and task-output inputs together.
  Measure a fixed harness envelope `{"context": ..., "taskOutput": ...}`;
  count its keys and structure too. This is a test interface, not new DSL
  syntax. `MAX_HTTP_RESPONSE_BYTES` remains 1 MiB. The byte budget accommodates
  a 1 MiB response alongside a bounded context, but the node and depth limits
  apply independently and may still reject it: a dense 1 MiB response, such as
  an array of small objects or numbers, can exceed 100,000 nodes. The envelope
  does not promise to accommodate arbitrary accumulated context and does not
  raise any production limit.
- Projection before accumulation is required: a new-profile export must be
  able to transform the separate task output while reading the pre-export
  context, then atomically merge only selected results into context. Raw task
  output must not be automatically copied into context by this operation.
  This does not change any separate task-output persistence contract.
- The spike tests this operation through a harness adapter with separate
  context and task-output inputs. E03 chooses the actual language bindings and
  export syntax after evaluator selection. Neither task-level `output.as`
  support nor a new DSL surface is a prerequisite for the spike.
- Runtime integration must check the combined input before evaluator
  conversion, without first creating an unbounded copy. Existing production
  contexts above the new profile's limit fail explicitly only when opting
  into that profile; legacy CEL behavior is unchanged.

### 2.5 Variable scopes and wrapper syntax

**Resolved at E03 for extended CEL** (E03 contract §2–§3):

- **Bindings:** `context`, `workflow.input`, `output` (exports only) and
  `value` (`assert.json` comparisons only). Context keys are not bound as top-
  level identifiers, so nothing can shadow a binding.
- **Delimiter:** a single `${ … }`, recognized by a scanner that understands
  CEL strings. `$${` writes a literal `${`, and `${{` is rejected. A string
  that is exactly one expression keeps its JSON type; expressions embedded in
  text must produce strings. Predicates and export strings must be expressions.
- **Exports:** every entry reads one pre-export snapshot. They are merged
  together only if all succeed.
- **`switch`:** every non-default case needs `when`, and `default` comes last.
- **Endpoints:** `{name}` placeholders are allowed only in the URI path. The
  value comes from `context`; `.`, `..` and empty values are rejected; the
  value is encoded once as a path segment and never rescanned. Endpoint URIs
  may not contain expression spans: this is a documented v2 restriction. Query
  expressions stay available through `with.query`, and legacy behavior is
  unchanged.

The original open questions, kept for reference:

- The task output in `export` must be reachable without a reserved `.output`
  key that can shadow a context key. For jq, the modeled Serverless Workflow
  DSL passes runtime arguments as variables (`$context`, `$input`, `$output`,
  `$task`, `$workflow`), which avoids shadowing. The exact variable set must be
  checked against the DSL version `workflow-core` models and against the
  positions the executor actually runs (section 1), not assumed.
- `${{ ... }}` is ambiguous with a jq object constructor (`${{a: .x}}`). One
  rule must be chosen, and the template scanner must understand quoted strings,
  escapes and nested braces. For CEL, map literals have the same issue.
- Export entries must read one pre-export snapshot and be applied together
  after all succeed, removing the ordering dependency in 1.4.
- The `{name}` endpoint placeholder rewrite generates expressions. Under jq,
  `name` is not a valid path; the rewrite must emit syntax that is valid for the
  selected language.

### 2.6 Evaluator profile enforcement

Every definition admitted under the new contract records an evaluator profile
identifier (for example `cel-workflow-v2` or `jq-workflow-v1`). Existing
definitions are treated as an implicit legacy CEL profile. Runs already keep
`process_info_t.definition_snapshot`; where the profile identifier lives is
decided at E04. Claim-time filtering likely needs the identifier in a
queryable column rather than inside the snapshot, which may require a
migration; E04 assesses this (section 5.1).

Enforcement policy:

- A worker keeps an implementation for every profile it advertises. Profiles
  are immutable: a behavior change is a new profile identifier.
- A worker that does not support a run's profile does not claim that run's
  tasks. Before admitting any new-profile definition, every task-claiming
  worker must understand profile filtering, or deployment must stop the old
  workers first. Today's workers cannot be assumed to implement this rule.
- Admission rejects a profile not supported by the deployment's explicitly
  configured capability policy. Temporary absence of a compatible worker
  does not prove a profile is unsupported and does not fail a persisted run.
  Compatible tasks remain pending under existing operational timeout rules.
  An explicit operator decision to retire a profile may fail affected runs
  with non-retryable `EVALUATOR_PROFILE_UNSUPPORTED`; runtime integration must
  identify the authority applying that decision. Never execute a different
  profile as a fallback.
- Removing a profile requires that no active run uses it, or an explicit
  operator decision to fail those runs.

### 2.7 Errors and effects

- Stable error categories: invalid expression, unsupported capability, invalid
  JSON profile, wrong result count or type, and resource-limit exhaustion.
- Category mapping (2026-10-02). E01 keeps the spike's frozen category set and
  tells cases apart by category plus phase:

  | Condition | Category | Phase |
  | --- | --- | --- |
  | Syntax or parse error; malformed macro | invalid expression | compile |
  | Function or feature outside the admitted subset | unsupported capability | compile |
  | Source, AST or nesting limit | resource limit | compile |
  | Runtime language error: type mismatch or no overload, missing field or key, index out of range, division or modulo by zero, integer overflow, invalid extension-function argument, jq `error/1` | invalid expression | evaluation |
  | Number/JSON boundary violation (2.3) | invalid JSON profile | conversion or evaluation |
  | Zero or multiple results; non-boolean predicate | wrong result count or type | evaluation |
  | Output bytes, nodes or depth | resource limit | evaluation |

  When more than one condition applies, the evaluator returns the first one it
  meets in a deterministic traversal order (for example, ascending map-key
  order), never one that depends on hash-iteration order. Production
  integration (E04) adds a separate **evaluation error** category for runtime
  language errors, rather than reusing invalid expression.
- **Production categories (E03):** `EXPRESSION_INVALID`,
  `EXPRESSION_UNSUPPORTED`, `EXPRESSION_LIMIT`, `EXPRESSION_EVALUATION`,
  `EXPRESSION_JSON_PROFILE`, `EXPRESSION_RESULT_TYPE` and
  `EVALUATOR_PROFILE_UNSUPPORTED`, with the phase rules in the E03 contract
  §7.4. The table above remains the E01 spike mapping.
- Diagnostics identify definition, task, field and source offset. Admission
  errors may include an excerpt of the definition-authored expression. Runtime
  errors must not include context values, returned data or interpolated library
  errors.
- Compile errors fail admission. Runtime expression errors fail the task through
  the existing failure machinery. They are not transient upstream errors and do
  not create their own retry loop.
- All request arguments are resolved before dispatch; failure means zero
  dispatches. A failure after a successful external call does not roll that call
  back, and existing retry and idempotency contracts still apply.

### 2.8 Scheduling and leases

Evaluation is CPU-bound and synchronous. It must not run on Tokio worker
threads; it runs on `spawn_blocking` or a dedicated bounded pool. The executor
runs up to `DEFAULT_HOST_EXECUTOR_CONCURRENCY` (8) tasks with 30-second task
leases, so slot acquisition, cancellation and lease renewal must be designed
together:

- The total time a task can spend waiting for and holding evaluation slots must
  fit inside a renewable lease, or the lease must be renewed while waiting.
- A queue timeout alone does not prevent duplicate execution. A task whose lease
  is lost must not commit an evaluation result.
- Cancellation must stop evaluation and release its resources before releasing
  its slot. A watchdog is defense in depth, never the primary bound.
- *Revised operating model (2026-10-02):* an in-process evaluation cannot be
  preempted unless the selected library offers metering. Cancellation and lease
  loss are therefore honored at evaluation boundaries: a result produced after
  either is never committed. A runaway evaluation is contained by worker and
  container limits, not by a per-expression bound. Crash resistance is
  unqualified (scope reduction above). As defense in depth, not
  qualification, integration should compile and evaluate on threads whose
  stack is larger than the 2 MiB default of Rust `std::thread` and Tokio's
  blocking pool, and record the chosen size.

### 2.9 Isolation and determinism

No environment, filesystem, network, module import, external input stream,
debug output, clock or random access. The same expression, input and profile
yields the same result or the same deterministic limit failure. Library
defaults are audited, not only the functions the application calls.

Capability isolation remains a required gate. Resource isolation, meaning hard
per-expression work and memory bounds, is future hardening under the
2026-10-02 operating model. Determinism applies to outcomes the evaluator
returns. A process killed by an OS guard is recorded separately.

## 3. Candidates

### 3.1 Extended CEL

Scope under evaluation:

- Standard string extensions: `split`, `substring`, and related functions from
  the cel-go `strings` extension, implemented with the same signatures.
- `map`, `filter`, `all` and `exists` macros with a fixed nesting limit.
- One JSON serialization function with pinned key ordering and escaping.
- UTF-8 byte sizing (already possible through `size(bytes(s))`).

Questions the spike must answer:

- Can the `cel` crate's public API add these functions and macros without a
  fork?
- A nesting limit and AST cap do not bound work by themselves. Work depends on
  list sizes, intermediate collections and string growth. Can a cost estimate
  computed from the AST and actual input sizes reject an expression before
  execution? If not, can execution be metered? *(Since 2026-10-02 the answer
  is reported, not required.)*
- Does widening the value profile change behavior for existing CEL
  definitions? Any change requires a new profile identifier (2.6).

### 3.2 In-process jq

Scope under evaluation: a pinned Rust jq library with a published
function/operator inventory, unqualified features rejected at compilation.

Questions the spike must answer:

- Can the library meter work and live allocation during evaluation, including
  built-ins, through its public API? An async timeout cannot stop a CPU-bound
  evaluator, and a post-evaluation length check cannot prevent allocation
  exhaustion. *(Since 2026-10-02 the answer is reported, not required.)*
- Restricting the subset does not suffice by itself. Without recursion, ranges
  or loops, comma duplication, variable cross products and string repetition
  still grow exponentially or polynomially within the AST cap.
- Can number pass-through (2.3) be preserved?

### 3.3 Isolated evaluator process (future hardening, separately approved)

Under the 2026-10-02 operating model this is future hardening, tracked
separately. Failing the resource gates no longer leads to it. It becomes
necessary if untrusted users are ever allowed to publish expressions.
*Historical E00 wording:* "If neither in-process candidate passes, a dedicated
evaluator child process may provide stronger isolation." This document does not approve it. A proposal would
need its own contract covering:

- Memory: `RLIMIT_AS` limits virtual address space, which allocator
  reservations can trip; a cgroup memory limit is the likely mechanism.
- Time: `RLIMIT_CPU` is not a per-request wall-clock deadline; the parent must
  enforce the deadline and kill the child.
- Bounded IPC framing, process pooling and reuse rules, cancellation and
  restart behavior, and filesystem/network confinement.
- Its own qualification, separate from the spike in section 4.

No `jq` CLI, shell or external executable is permitted in the two in-process
spike candidates. A dedicated evaluator worker executable belongs only to the
separately approved fallback above; it is not a general script runner.

## 4. Spike criteria, fixed before execution

These criteria are set before the spike starts and are not adjusted after
results are known. Budgets are proposals pending review; review may change them
only before the spike begins.

**Exception, 2026-10-02:** the owner explicitly revised the pass criteria after
the CEL investigation (see the owner decision at the top of this document). The
fixtures, amplification cases and limits in 4.1–4.3 are unchanged as
definitions. The work and allocation ceilings are not enforced gates. After the
scope reduction, the amplification cases (4.2) are not executed in the current
spike. 4.4 gives the revised gates and keeps the original ones as historical.

### 4.1 Fixtures

The G03 fixtures are JSON files at three sizes: typical, page-maximum
(30 comments of realistic length) and limit-sized (an input at the proposed
input limit).

| ID | Transformation |
| --- | --- |
| F1 | Split a canonical GitHub issue URL into owner, repository and issue number |
| F2 | Project an issue to `{title, body}` with a nullable `body` |
| F3 | Project a comment page to `[{author, body}]` with nullable `user` |
| F4 | Concatenate comment pages into one list |
| F5 | Serialize the comment list to a JSON string |
| F6 | Measure the UTF-8 byte size of the serialized aggregate |
| F7 | Predicate: issue response contains `pull_request` |
| F8 | Predicate: a page has exactly 30 entries (continue pagination) |
| F9 | Pass through an unrelated decimal value in context while evaluating F2 |

Each candidate writes F1–F9 in its own syntax. A fixture passes when the result
matches the expected JSON exactly.

Shared setup must freeze the exact JSON bytes, expected outputs, sizes and
SHA256 hashes in a manifest before either candidate starts. Typical and
page-maximum fixtures use identical inputs for both candidates. Limit-sized
fixtures use bounded unrelated padding where necessary so valid F1–F9 results
still fit the output ceiling; an oversized serialization result is an A5
rejection case, not a successful F5 fixture. Include a projection fixture with
a 1 MiB task response and a 48 KiB context, preserving an existing context key
named `output`, and prove only projected data enters the next context.

Limit-boundary fixtures test each input limit separately. Each set has one input
exactly at the limit, which must be accepted, and one just over it, which must
be rejected with the input-limit category:

| ID | Binding limit | Construction |
| --- | --- | --- |
| L1 | Bytes (2 MiB) | Few large strings: under 100,000 nodes and depth 64 |
| L2 | Nodes (100,000) | Dense small values: well under 2 MiB and depth 64 |
| L3 | Depth (64) | Narrow nesting: well under 2 MiB and 100,000 nodes |

The manifest records, for every limit-sized fixture, its byte size, node count
and depth, and which limit binds first. E00
approves these construction rules; shared setup materializes the files before
candidate measurements. Later changes invalidate affected comparisons and
require review before rerunning either candidate.

### 4.2 Amplification cases

*Not executed in the current scope (scope reduction, 2026-10-02). The
definitions are kept for future hardening, and earlier results are preserved.*

Each case is written in the most damaging form each candidate's admitted subset
allows. Cases that cannot be expressed in a candidate are recorded as
"not expressible", with the compile-time rejection shown.

| ID | Case |
| --- | --- |
| A1 | Duplication chain: repeatedly double a list (`[.[],.[]]` in jq; list concatenation in CEL) |
| A2 | Cross product: nested iteration over the same large list |
| A3 | String growth: repeated concatenation or repetition of a long string |
| A4 | Deep nesting: deeply nested input, and construction of deeply nested output |
| A5 | Large serialization: serialize a limit-sized input, and serialize repeatedly |
| A6 | Unbounded constructs: recursion, ranges, loops and generators |
| A7 | Sorting, grouping and comparison over a limit-sized array |
| A8 | Number edge cases: unsafe integers, fractional division, overflow |
| A9 | Large result streams (jq) or large list construction (CEL) |
| A10 | Regex or pattern functions, if included in the admitted subset |

### 4.3 Proposed limits (pending review)

| Resource | Proposed ceiling per evaluation |
| --- | --- |
| Expression source | 16 KiB UTF-8 |
| Parsed expression | 2,048 AST nodes; nesting depth 64 |
| Input | 2 MiB combined envelope (2.4); depth 64; 100,000 JSON nodes |
| Work | 100,000 metered units, accepted only under the 4.3.2 conditions. *Reported, not a gate, since 2026-10-02* |
| Live allocation | 16 MiB charged evaluator memory, under the 4.3.1 conditions. *Reported, not a gate, since 2026-10-02* |
| Output | 1 MiB compact JSON; depth 64; 100,000 JSON nodes |
| Result count | Exactly one value |
| Compilation cache | 128 entries; 16 MiB retained per process |
| Concurrency | Bounded per worker; value decided with 2.8 |

These are evaluator ceilings, not increases to task, HTTP, agent or context
limits. The lowest applicable limit wins, and definitions cannot raise them.

#### 4.3.1 Allocation ceiling conditions

*Since 2026-10-02 these conditions govern how allocation is measured and
reported. A missing enforcement mechanism or bound is no longer a failure.*

16 MiB is an experimental ceiling. It is evaluated only after both of these are
fixed in the spike plan, before any measurement:

- **Accounting**: peak simultaneously live incremental bytes attributable to
  compilation, input conversion, evaluation and result serialization, including
  converted inputs, intermediate values, retained compiled expressions and
  output buffers. Freed bytes cease to count; this is not cumulative allocation
  traffic. Report three peaks separately: input conversion, compilation and
  evaluation, with cold and warm evaluation peaks reported separately. Warm
  measurements include the retained compiled expression and converted input
  even though allocation happened before timing. Each evaluation is charged
  only for its own compiled expression; the shared compilation cache budget
  (4.3) is measured and enforced separately. Report any excluded fixed library/harness overhead
  separately and bound it before measurements.
- **Input size**: the proposed 2 MiB envelope, depth and node bounds from 2.4.

If either is still open when measurement would start, the allocation criterion
is reported as inconclusive for both candidates.

A counting allocator supplies measurement evidence, not production enforcement.
Passing also requires a reviewable enforcement mechanism or conservative bound
covering all admitted operations, including compilation and conversion. Passing
the fixed tests alone is insufficient proof for arbitrary admitted expressions.

#### 4.3.2 Work ceiling conditions

*Since 2026-10-02 a metered definition or cost bound is reported as an extra
capability when a candidate provides one. Its absence is not a failure.*

A unit count is not comparable across engines. The 100,000-unit ceiling is
accepted for a candidate only if that candidate provides one of:

- **Metered accounting**: a definition of what one unit charges, covering AST
  steps, built-in loops, sorting and comparison, string expansion and
  serialization, with evidence that every A1–A10 case is charged; or
- **A conservative cost bound**: a pre-execution bound computed from the AST
  and actual input sizes, shown to be an upper bound for every A1–A10 case.

Engines are compared on measured outcomes at the limit, not on unit counts: for
each amplification case, the wall time and peak charged allocation at the point
of rejection or completion.

### 4.4 Pass criteria

#### Revised pass criteria (2026-10-02, governing)

Both candidates are judged by the same gates:

- **Compatibility:** F1–F9 pass; the projection fixture holds; existing CEL
  qualification tests pass with the existing profile unchanged.
- **Input limits:** inputs exactly at each limit pass validation, and inputs
  just over are rejected with the input-limit category.
- **Capability isolation:** as in 2.9.
- **Result contract:** as in 2.2. Zero results, multiple results, or a value
  followed by an error never return a value. Predicates are strictly boolean.
  For jq, the stream is not consumed past the second result.
- **Determinism of returned outcomes:** for every limit-boundary case and
  every ordinary fixture that is repeated, the returned outcome and category
  agree across runs. Errors are chosen in a deterministic traversal order
  (2.7).
- **Configured-limit validation:** source, AST and nesting limits reject with
  the correct category at one unit over the limit and accept at the limit,
  shown by ordinary tests.
- **JSON number boundary:** as in 2.3.
- **Error-category mapping:** as in 2.7.
- **Effort and modification ceilings:** as in 4.5.

Reported, not pass/fail: allocation peaks per phase on realistic fixtures, and
performance.

Unqualified in the current scope (scope reduction at the top): crash
resistance, resource isolation, amplification behavior and work/allocation
enforcement. Reports list these as **excluded qualification**, never as passed.
If an ordinary test crashes, the crash is reported as a finding.

Each candidate's report states its outcome under the revised criteria and,
from the same evidence, under the historical criteria below.

#### Historical E00 pass criteria (superseded 2026-10-02)

**Safety**, enforceable limits:

- Every amplification case either completes within all limits or is rejected
  deterministically, at compile time or at run time, before any limit is
  exceeded. "Usually terminates" is a failure.
- Peak live allocation is measured with a counting allocator in the test
  harness and must stay within the allocation ceiling plus a documented,
  bounded fixed overhead.
- Repeated runs of the same case produce the same outcome and the same failure
  category.

**Compatibility**:

- F1–F9 pass.
- Existing CEL qualification tests still pass with the existing profile
  unchanged.

**Performance**, measured and reported, not an enforced limit:

- p50 and p99 latency per fixture at each size, over at least 1,000
  evaluations on a recorded machine, reported in three separate measurements:
  - **Cold compilation**: parsing, validation and compilation of an expression
    not in the cache.
  - **Context conversion**: converting the run context into evaluator form,
    once per task step (2.4).
  - **Warm evaluation**: evaluating an already-compiled expression against
    already-converted input.
- Provisional target, pending review: warm-evaluation p99 under 5 ms per F1–F9
  fixture at page-maximum size. The target informs review; meeting it does not
  approve a candidate, and missing it is a finding, not a safety failure.

### 4.5 Effort and modification ceilings

Time:

- Time is counted in active engineering hours, not calendar days. One working
  day is 6 active hours.
- Each candidate gets the same budget: proposed 18 active hours (3 days).
- The whole spike, including shared setup and the report, is time-boxed to a
  proposed 42 active hours (7 days).
- Shared setup is done once, before either candidate's clock starts, and is
  charged only to the overall time-box: fixtures, amplification inputs, the
  counting allocator, the benchmark harness and the report template.
- Candidate-specific setup is charged to that candidate: adding and building
  the dependency, license review, and learning its API.

Code:

- Each candidate may use only the library's public API. A vendored fork, a
  patched dependency or a `[patch]` override exceeds the ceiling.
- Implementation code is limited to a proposed 800 non-test lines per
  candidate, counted as non-blank, non-comment lines. The count includes helper
  crates, build scripts, macros and generated implementation code, whether
  committed or produced by a build script. It excludes tests, fixtures and the
  shared harness.
- Exceeding a ceiling ends that candidate's investigation with the result
  "exceeds modification ceiling".

### 4.6 Outcomes

Each candidate ends as **pass**, **fail** or **inconclusive**, with the following
precedence:

1. **Fail**: a required pass/fail criterion has failed with evidence. A
   demonstrated failure remains a failure even if the time-box expires before
   other criteria are answered. List those criteria as unanswered; do not
   infer their results.
2. **Pass**: every required pass/fail criterion has passed with evidence.
3. **Inconclusive**: no required criterion has demonstrably failed, but required
   evidence remains missing when the time-box expires.

The measured latency target is advisory and does not determine pass or fail.
This precedence matches the E01 implementation plan's section 8; it does not
change any fixture, resource limit or candidate budget.

Rows are read top to bottom; the first matching row applies.

| Result | Next step |
| --- | --- |
| One passes; the other is inconclusive | Report the qualified pass and what remains unanswered for the other. The owner may select the passing candidate or approve extending the other investigation. Inconclusive is not treated as failure, and the report claims no comparative superiority |
| Both inconclusive | Report what remains unanswered; no selection recommendation; extending the spike requires approval |
| One inconclusive; the other fails | Report evidence for both; no selection recommendation; extending the inconclusive investigation or proposing the fallback (3.3) requires approval |
| Only CEL passes; jq fails | Propose extended CEL to unblock G03; jq remains a separate track |
| Only jq passes; CEL fails | Propose jq integration under the shared contract |
| Both pass | Owner decides on effort, compatibility and measured performance; the report does not choose |
| Both fail | Report evidence; the owner decides the next step. *Since 2026-10-02 this does not imply the isolated process (3.3), which is future hardening. Historical wording: "the isolated process (3.3) may be proposed for separate approval".* |

Since 2026-10-02, this table is applied to the outcomes under the **revised**
criteria (4.4). Outcomes under the historical criteria are reported as history
and do not choose the row.

The report records for each candidate: dependency name, version and license;
admitted function inventory; enforced limits and how each is enforced, including
the work accounting or cost bound (4.3.2); results for F1–F9 and A1–A10 with wall
time and peak charged allocation; the three latency measurements (4.4); active
hours used; and implementation line count.

## 5. Checkpoints

| Checkpoint | Deliverable and review gate |
| --- | --- |
| E00 | Approve this revision, including section 1 and the section 4 criteria |
| E01 | Run the spike under section 4; deliver the report; no runtime integration |
| E02 | Owner selects a candidate (or the fallback proposal) from the report |
| E03 | Resolve 2.5 for the selected candidate; add the complete YAML example (section 6) |
| E04 | Integrate admission, dispatch, profile enforcement, errors, scheduling and publication/digest compatibility; qualify CEL regressions |
| E05 | Review executor and hostile-input evidence, upgrade behavior and rollout instructions |
| G03 resume | After separate approval, implement and qualify the G03 definition |

Stop for owner review at each checkpoint. Keep the test-only G03 probes and
unrelated workspace changes intact.

### 5.1 Open items

| Item | Resolved at | Requirement |
| --- | --- | --- |
| CEL profile selector | E03 (**resolved**) | `document.metadata.lightExpressionProfile: cel-workflow-v2` (2.1) |
| Profile storage | E04 | E03 contract §7.3 proposes:<br>• `process_info_t.expression_profile` with a snapshot-consistency `CHECK`<br>• a database-held v2 admission switch<br>• claim function v2, a legacy-only predicate on claim v1, and profile checks on every execution path<br>The schema alone doesn't protect paths in already-running old binaries. Rollout order: install the schema → upgrade or stop every admitting, evaluating or completing binary → confirm the capability inventory → enable admission, with reserved-key preflights before and after and no automatic rewriting. Rollback checks are split: the database checks the switch and active v2 runs and keeps the legacy-only protection; the deployment verifies every admission writer honors the switch. SQL alone doesn't prove application admission is disabled |
| Compiled-expression accounting | E04 | Charge each evaluation for its own compiled expression; enforce the shared compilation cache budget separately (4.3.1) |
| Memory reporting | E04 | Report input-conversion peak memory separately from compilation and evaluation peaks, as in the spike (4.3.1) |
| Runtime error category | E04 | Implement `EXPRESSION_EVALUATION` and the other E03 categories (2.7) |
| Strong isolation | Future hardening | Per-expression work/memory bounds or an isolated process (3.3). Required before untrusted users may publish expressions |
| Crash resistance | Future hardening | Qualify compiler/evaluator stack safety (recursion bounded before each recursive phase, a defined thread stack) and amplification behavior. Unqualified in E01; see the scope reduction at the top |

## 6. Complete workflow example

E03 provides one complete, neutral `cel-workflow-v2` example (E03 contract §6).
It is labelled as proposed and not executable until E04. It shows workflow
input and schema, `set`, HTTP arguments with path placeholders, `switch`,
atomic exports and workflow `output.as`, with explicit page-request,
aggregate-count and serialized-size bounds enforced in the definition.

It is deliberately not a G03 definition. G03 still has to supply GitHub's
canonical owner and repository validation (rejecting `.` and `..`) and the
registered `lightapi://<capabilityRef>` calling pattern with its Portal-issued
`metadata.workflowTool` pin, after E04.

## 7. Integration requirements for the selected candidate

- One internal dispatch boundary for all section 1 positions, with distinct
  value, string and predicate result contracts. Direct calls, such as the
  workflow `output.as` path, must not bypass it.
- Definition validation, Portal publication, native start and Invoke admission
  agree on the supported language and profile. All known expressions are
  compiled at admission. Runtime checks remain for input-dependent types and
  resource use.
- The definition digest includes the language selector and expression source
  through its normal canonical representation. Earlier definitions are not
  rewritten and their identities are not recomputed.
- No changes to Gateway ACLs, ToolBinding rules, creator identity, task leases
  beyond 2.8, cancellation, deadlines or LONG token semantics. Local
  transformations do not trigger token exchange.

### Operational requirements

- Metrics for evaluations, latency and limit failures by category.
- An author validation path: compile and evaluate an expression against a
  supplied sample input without starting a run.
- A versioned process for adding built-ins: each addition is qualified against
  the amplification cases and produces a new profile identifier (2.6).
- Operator documentation states the operational limitation: under the
  2026-10-02 operating model, a pathological expression in a reviewed
  definition can exhaust a worker's CPU or memory. Containment is worker and
  container limits plus definition review. Hard per-expression isolation is
  not advertised.
- Crash resistance is unqualified. Operator documentation says so, and
  evaluation threads use a recorded stack size larger than the 2 MiB default
  as defense in depth.

## 8. Required qualification after integration

1. Language admission: legacy CEL unchanged; the new profile accepted; missing
   block or language, unknown language and nonempty mode rejected on every
   new-profile admission and publication path without retroactive legacy
   validation changes.
2. Syntax and scope: every section 1 position, the wrapper rule, quotes and
   braces, literal strings and export semantics. Unsupported fields from 1.5
   fail rather than being ignored.
3. Results: null, result count, strict predicates, serialization, Unicode byte
   counts and number handling per 2.3.
4. Hostile input: deferred to the crash-resistance and strong-isolation future
   hardening items (5.1). Integration qualifies the configured-limit
   validation and deterministic returned outcomes only. Historical E00 wording:
   "asserting bounded termination and memory, not only an error string".
5. Isolation: attempted environment, file, network, module, input-stream and
   clock access; sentinel secrets absent from evaluator input and diagnostics.
6. Executor: Set, HTTP arguments, switch, assertions, exports, agent arguments
   and public output; invalid arguments cause zero dispatches; restart and
   resume; lease loss during evaluation; profile enforcement across a rolling
   upgrade.
7. Publication and digest: language or expression changes change the definition
   identity; native and Tool-bound execution select the same evaluator.

Component tests and simulated fixtures are component evidence. Live Portal
publication, deployed execution and a real coding-agent handoff are reported
separately. No GitHub credential is needed for evaluator qualification.

## 9. G03 follow-through

The expression capability removes a transformation gap; it does not complete
the integration. The G03 definition still needs canonical URL checks,
registered Gateway HTTP calls, body-based pagination at `per_page=30`, an extra
empty request for exact page multiples, explicit aggregate and page bounds,
fixed-delay task retries and rejection of issue responses containing
`pull_request`.

Issue and comment text remains untrusted content in ordinary context.
Attachment links are metadata unless a later definition explicitly retrieves
them. The existing coding-agent entry requires valid workspace and task setup
and its existing authority; the expression capability does not create those
inputs. The real design handoff is qualified directly, rather than treating a
prepared JSON payload as successful execution.

## Related documentation

- [Workflow Invoke and Tool Binding Publication](workflow-invoke.md)
- [Native Agent Call](native-agent-call.md)
- [Personal Development Workflow Orchestration](../light-agent/development-workflow-orchestration.md)
