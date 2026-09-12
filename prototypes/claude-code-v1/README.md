# Claude personal worker Phase 0

This standalone Python-standard-library harness qualifies native CLI feasibility.
It is test infrastructure, not an SDK bridge, Rust worker implementation, or
production adapter. It does not modify production selection or advertise Claude
capabilities. The Phase 1 adapter will be Rust.

## Pinned candidate

`contracts/claude-code/v2.1.269/phase0.json` pins Linux x86_64 Claude Code 2.1.269
by exact artifact hash. A newer installation is rejected, not silently accepted.
The current tested model mapping is `sonnet` -> `claude-sonnet-5`. Add another
mapping only with explicit live evidence; do not infer entitlement from a name.

## Run

From the light-fabric root:

```bash
./scripts/run-claude-personal-phase0-gates.sh
```

That runs offline tests and explicitly reports the live gate as NOT RUN.
For native qualification, use the existing owner login:

```bash
LIGHT_RUN_CLAUDE_PERSONAL_SMOKE=1 \
LIGHT_CLAUDE_EXECUTABLE=/absolute/path/to/claude \
LIGHT_CLAUDE_NATIVE_MODEL=sonnet \
LIGHT_CLAUDE_PHASE0_REPORT=/tmp/claude-phase0-report.json \
./scripts/run-claude-personal-phase0-gates.sh
```

The live gate consumes four small subscription turns, requires a first-party
`claude.ai` authentication status, and creates two native probe conversations.
Native history stays in the owner's configured Claude store. It does not log out,
copy credentials, edit user settings, or purge existing/native conversations.
Temporary test workspaces are removed; native conversation retention remains a
user-controlled CLI concern. Native mode deliberately loads the owner's hooks
and plugins, so run it only in the dedicated trusted personal environment.

## Evidence

The live probe:

1. Checks platform, binary digest, exact version, required flags, and sanitized
   authentication class before any model turn.
2. Creates a disposable project with a harmless SessionStart hook that writes a
   marker, native `dontAsk` permission mode, and a model default different from
   the explicit requested model.
3. Runs `new` and exact-UUID `resume` in separate CLI processes with Light-managed
   `--safe-mode`, explicit `dontAsk`, and an empty native tool surface.
4. Runs the same pair using native configuration discovery without overriding
   native permission mode. Only this pair must execute the project hook.
5. Requires the same native session identity and random conversation-only marker
   on resume without including that marker in the second prompt. Requires the
   observed model and permission mode to match expectations on every turn.
6. Stores only event field names, model, permission mode, counts, and pass/fail
   evidence. Raw transcripts, stderr, account identifiers, settings, and tokens
   are never included in the report.

Stdout frames, total stdout, stderr, and elapsed time are bounded. The supervisor
kills the process group on completion/failure. This is a Linux probe, not proof
against descendants deliberately escaping their process group; the production
runner must provide the stronger per-attempt enforcement.

Offline tests cover wrong authentication, environment contamination, source-mode
launch differences, malformed/truncated/duplicate/error results, session identity
mismatch, binary mismatch, oversized frames, stderr flood, and a deadline with a
child retaining the output pipe. Synthetic JSON fixtures are labeled synthetic;
observed event-key inventories are shape evidence, not a vendor JSON Schema.

## Scope and remaining gates

A successful report means these native feasibility cases passed. It does not
prove file editing, canonical patches, read-only review isolation, tool approval
mediation, explicit bypass mode, every user/managed configuration combination,
resident-process mode, close/checkpoint handling, or Portal/runner integration.
Those belong to the later adapter and integration phases.

`productionQualified` remains false. The eligibility record explicitly leaves
supported third-party distribution unresolved because the vendor guidance needs
a separate determination. User-requested personal CLI evaluation is not claimed
as permission to distribute a subscription-backed product. No report is converted
into a production qualification record automatically.

## Phase 1 Rust candidate

The deterministic adapter now lives in
`apps/light-agent-worker/src/claude_code.rs`, compiled by tests or the explicit
`claude-prototype` Cargo feature. It is not registered in production dispatch;
`print-capabilities` still advertises Codex even with that feature enabled.

```bash
./scripts/run-claude-personal-phase1-gates.sh
```

`ClaudeTurn` is a strict candidate envelope containing `coding: CodingTurnSpec`,
`policy: LaunchPolicy`, and optional `nativeModel`. The policy separates available
`tools` from pre-approved `allowedTools` rules. Native source rejects both kinds
of Light overrides; managed source rejects `inherit`. Model choices resolve
through a server-owned alias-to-observed-model map. Host paths, native home, and
trusted session scope arrive separately in `HostContext`, never from task JSON.
The Phase 2 admission layer must construct these values; the candidate API is not
an authorization boundary for untrusted callers.

`execute` verifies the exact native binary/version and subscription auth,
starts or resumes the UUID selected by the private checkpoint, and emits bounded
normalized events. It drains stderr without exposing its contents. A deadline,
cancellation, disconnected/backpressured event consumer, malformed stream, or
nonzero exit fails the operation and kills the process group. Escaped process
groups still require the production runner's isolation contract.

A successful native turn returns a `Proposal`, not a committed coding result.
The trusted core must perform artifact/review validation before calling
`accept_validated`; dropping the proposal leaves an uncertain `IN_FLIGHT`
checkpoint. Permission denials cannot be committed as a successful result.
Session locks remain held during acceptance. `close` makes a local tombstone
without a model call; `closeAfterTurn` commits the validated result as closed.
Native transcripts are not erased or claimed to be archived by a vendor API.

Adapter-private checkpoint state preserves the selected native model when a
resume omits it. An explicit different model, changed policy, stale checkpoint,
foreign scope, duplicate new, or resume after close is rejected. Existing Codex
checkpoints need no migration. Explicit lock release on guard drop also prevents
a transient fork-inherited descriptor from extending checkpoint ownership.

The launch manifest is
`contracts/claude-code/v2.1.269/phase1-launch.json`; its digest participates in the
private checkpoint binding. It remains prototype-only. Current tests use a
synthetic executable whose digest is accepted only by a private test helper;
public `execute` always uses the compiled vendor binary pin.

Phase 2 still owns workflow/Agent input admission, actual runner dispatch,
workspace materialization, canonical patches, independent review validation,
and live worker integration. Phase 1 does not claim those product paths work.
