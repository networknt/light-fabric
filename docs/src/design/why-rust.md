# Why Rust

Light-Fabric is written in Rust. Not because Rust is fashionable, and not
because the team enjoys fighting a borrow checker, but because the platform is
built for a specific world: **enterprise backend services and agentic workflows
that are increasingly written by AI agents and verified by automated gates
rather than read line-by-line by humans.**

In that world the traditional trade-off — "Python is faster to write, Rust is
faster to run" — no longer holds the way it did. The cost of *writing* code is
collapsing. The cost of *running* it, *trusting* it, and *operating* it is not.
This document explains the reasoning.

## 1. The premise has changed

The classic argument for Python and JavaScript on the backend was never really
about the machine. It was about people:

- Fewer lines to type, so features ship faster.
- A larger hiring pool, so code is easier to staff and maintain.
- A shallower learning curve, so review and onboarding are cheap.

Every one of those advantages is a *human throughput* argument, and human
throughput is exactly what stopped being the bottleneck. When a model writes the
first draft, another model reviews it, and CI gates decide whether it merges,
"how long does it take a developer to type this" is no longer the constraint
worth optimizing. What remains is what the machine has to do afterwards, forever:
execute it, pay for it, and not break in production at 3 a.m.

Rust wins on everything that is left.

## 2. The compiler is the cheapest reviewer we have

An AI agent is a probabilistic writer. It produces plausible code. The
engineering question is not "will it make mistakes" — it will — but **how early
and how cheaply the mistakes are caught.**

Light-Fabric's quality pipeline has several gates: `cargo build`, `clippy` with
warnings denied, unit and integration tests, contract/conformance suites, and a
model-based review pass. They are not equally priced:

| Gate | Cost per run | Catches |
|---|---|---|
| `cargo check` / `cargo build` | seconds, deterministic, no tokens | types, ownership, lifetimes, exhaustiveness, nullability, data races |
| `clippy -D warnings` | seconds | misuse patterns, unchecked unwraps, sloppy idioms |
| Tests / conformance | minutes | behavior the tests anticipated |
| Model review | tokens and wall-clock, non-deterministic | intent, design, what the tests did not anticipate |

The compiler is the only gate that is *free, total and deterministic*. It does
not sample, it does not get tired, and it does not need a test case to have been
written in advance. In a dynamic language, every class of error the Rust
compiler rejects outright must instead be caught by a test someone remembered to
write or by a reviewer who happened to look — and in an agentic pipeline, both of
those are paid for in tokens.

Rust's type system also encodes decisions the platform actually cares about and
that reviewers routinely miss:

- `Result<T, E>` makes every failure path explicit; an ignored error is a
  compile-time warning, not a silent production incident.
- `Option<T>` eliminates the null-dereference class entirely.
- `Send`/`Sync` and the borrow checker make data races across the thousands of
  concurrent agent sessions Light-Fabric runs *unrepresentable*, not merely
  "unlikely if the code is careful".
- Exhaustive `match` means adding a new variant to an event, message or state
  enum forces every handler to be revisited. Across a workspace this large, that
  single property has caught more would-be regressions than any test suite.

This is the real answer to "if it compiles, it runs." It is not that Rust code
is bug-free. It is that **a whole family of bugs cannot reach the tests, the
reviewer, or production**, so the expensive gates spend their budget on logic and
design instead of on null checks and race conditions.

## 3. Verbosity is a one-time cost; runtime is a forever cost

The standard objection is the "token tax": Rust is more verbose, so agents spend
more tokens generating it. That was a real argument. It has weakened
considerably, and it was always measured against the wrong denominator.

The generation cost is paid once per change. The runtime cost is paid on every
request, in every environment, for the life of the service. Light-Fabric runs
gateways, agent runtimes and workflow engines that sit on the hot path of every
call in the fabric; the asymmetry is not close:

- **Memory.** Rust services hold a resident set measured in tens of megabytes,
  with no GC heap to size and no GC pause to tune. The equivalent JVM or Python
  service reserves an order of magnitude more before it serves a single request.
  On a per-pod basis in Kubernetes, that is the difference between packing
  dozens of services on a node and packing a handful.
- **Latency tail.** There is no stop-the-world collector, so p99 tracks p50
  instead of spiking. For a gateway that fronts every LLM and MCP call, tail
  latency *is* the product.
- **Startup.** A statically linked binary is serving traffic in milliseconds.
  This is what makes scale-to-zero, fast rollouts, and short-lived sandboxed
  workers practical rather than theoretical.
- **Footprint.** A single self-contained binary with no runtime, no interpreter,
  and no dependency tree to install at deploy time. Containers are small, the
  attack surface is small, and the supply chain is one artifact.

There is also a second-order effect that matters specifically for an AI
platform: compute spent on the runtime is compute not spent on inference. Every
gigabyte and every core the control plane does not consume is budget available
to the models the platform exists to serve.

## 4. Verbosity is also, increasingly, an illusion

Rust reads as verbose next to a Python one-liner, but the comparison usually
omits what the Python one-liner postponed: the type annotations added later, the
validation, the error handling, the test that exists only to catch a `None`, and
the runtime guard for the concurrency case. Light-Fabric's Rust states those up
front, where the compiler can enforce them, instead of scattering them through
tests and incident reports.

The asymmetry is also shrinking on the model side. Frontier models generate
idiomatic Rust competently today, and the "compiler loop" failure — an agent
thrashing against lifetime errors — is now mostly a symptom of poor architecture
rather than of the language. In practice it is contained by the same design
rules that make the code good for humans: prefer owned data and `Arc` at
boundaries, keep lifetimes out of public APIs, keep functions small, and let
`clippy` steer idiom. Where a module does provoke thrashing, that is a signal
about the design, and a useful one.

## 5. Where Rust would be the wrong answer, and what we do instead

This is a considered choice, not a purity rule. Rust is the wrong tool in three
places, and Light-Fabric does not pretend otherwise:

- **The browser.** The web is JavaScript and TypeScript. UIs for the portal and
  consoles belong there, and WASM does not change that.
- **Glue, scripting and one-off automation.** Build helpers, migration scripts,
  data wrangling and repo tooling are written in whatever is shortest — shell or
  Python. They are not on the hot path, they are not mission-critical, and
  compiling them buys nothing.
- **The ML/data ecosystem.** Where a mature Python library is the state of the
  art, the right move is to call it across a process or service boundary, not to
  reimplement it.

The rule is a boundary, not a language ban: **anything on the request path,
holding state, enforcing a security decision, or running unattended in
production is Rust. Everything around it can be whatever is convenient.** The
platform is the compiled tier; the conveniences live outside it.

## 6. What this buys Light-Fabric concretely

The architecture depends on properties that are difficult or expensive to obtain
elsewhere:

- **Shared engines, many services.** `light-agent`, `light-agent-worker`,
  `light-agent-channel`, `light-gateway` and `light-workflow` are thin
  trust-boundary executables over shared domain crates. Cargo's workspace and
  the type system make that sharing safe: a contract change in a shared crate
  fails to compile in every consumer that has not been updated. See
  [Agent Engine Pattern](agent-engine-pattern.md).
- **Hot-reload without restarts.** Configuration, rules and agent metadata swap
  under live traffic via `arc-swap`, with the type system guaranteeing readers
  never observe a torn state.
- **Gateway-grade proxying.** `frameworks/light-pingora` builds on Cloudflare's
  Pingora, a proxy engine that exists in Rust because that class of workload
  cannot afford a garbage collector.
- **Massive concurrency per instance.** `tokio` lets one instance carry
  thousands of concurrent agent sessions and streaming LLM responses on a small
  footprint, with the borrow checker — not convention — preventing the races.
- **Enterprise security posture.** Memory-safety defects are the dominant class
  of exploitable vulnerability in C and C++ infrastructure, and the class Rust
  removes by construction. For software that terminates TLS, holds credentials
  and enforces authorization, that is a compliance argument as much as an
  engineering one.

## 7. The longer arc

The direction of travel is toward specifications and exit criteria as the real
source of truth, with the implementation language demoted to an artifact of the
build. Light-Fabric is already organized that way in places: the
metadata-driven agent engine, the rule specification, and the workflow
specification all describe *what* should happen, while Rust implements the
engine that makes it happen safely and quickly.

If that arc completes and the implementation tier eventually becomes something
lower-level still, the property being preserved is not "we write Rust" — it is
**machine-checkable correctness at the boundary between a probabilistic author
and a deterministic machine.** Today, Rust is the most practical, widely
supported, production-proven form of that guarantee. It also happens to be the
fastest and leanest option on the table, which makes the choice easy rather than
merely defensible.

## Summary

| Concern | Why it favours Rust |
|---|---|
| AI-authored code | The compiler is a free, deterministic, total reviewer; errors are caught before any expensive gate runs |
| Correctness | Null, data-race and unhandled-error classes are eliminated by construction |
| Cost | Generation cost is paid once; memory and CPU are paid forever, on every request |
| Latency | No GC, so tail latency stays flat — decisive for a gateway on every call path |
| Operations | One static binary, millisecond startup, tiny images, small attack surface |
| Evolution | Workspace-wide type checking turns contract changes into compile errors instead of production incidents |
| Security | Memory safety by construction for software that holds credentials and enforces authorization |

Human readability was the argument for dynamic languages, and human readability
is the constraint that is going away. What remains is execution cost and
verifiable correctness — and those are the two things Rust was built to win.
