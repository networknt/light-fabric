# Personal Codex permissions and models

The `codex-personal-policy-v1` extension applies to immutable-repository `coding`
workflows using `codex-app-server-v1`, pinned to Codex **0.153.4**. The enterprise
route and separate workspace-service tool bridge retain their existing contracts.

Publish this optional object under
`agentPolicy.execution.codingProfile.codexPolicy`, and add
`codex-personal-policy-v1` to the profile's `requiredFeatures`:

```json
{
  "schemaVersion": 1,
  "permissionSource": "codex-cli",
  "permissionMode": "inherit",
  "interaction": "unattended",
  "allowedModels": ["gpt-6-astra"]
}
```

Use exact model identifiers available to the owner's account. Optional
`defaultModel` must belong to `allowedModels`. Omitting the entire object preserves
the existing Light-managed behavior and serialized spec.

| Source / mode | Native mapping |
|---|---|
| `agent-policy` / `managed` | Existing role sandbox and `approvalPolicy: never` |
| `codex-cli` / `inherit` | Omit thread and turn approval/sandbox overrides |
| `codex-cli` / `trusted-personal-unattended` | `approvalPolicy: never` plus thread `danger-full-access` and turn `dangerFullAccess` |

The explicit automation mode requires published personal policy. Native managed
requirements and OS/service restrictions still apply; `never` alone does not
grant filesystem authority. Unattended approval requests are declined, with no
raw request payload in evidence. Interactive approvals are not qualified here.
Workflow coding input cannot supply permissions, sandbox settings, flags, or
policy objects, and prompt text cannot change the admitted source or mode.

Native mode requires root-owned, non-writable `/usr/bin/bwrap`. An outer mount/PID
namespace keeps the host and Git metadata read-only, hides Light checkpoints,
and permits admitted implementation paths or review scratch plus native state
storage and a private `/tmp` for shell helpers. Native `config.toml`, `rules`, `skills`, `plugins`, `AGENTS.md`,
`managed_config.toml`, and staged `.codex` policy files are mounted read-only during
a turn. Missing native surfaces are initialized as empty defaults so a turn
cannot install new permissions.
The worker retains canonical patch validation, protected paths, review candidate
immutability, owner/host binding, runner leases, cancellation, deadlines, and
checkpoint ordering. Publication still uses the existing workflow contract.
External tools require service-side role restrictions: a filesystem namespace
cannot enforce read-only access to a remote MCP service. Qualify those installed
tools for their intended role before enabling this profile.

## Workflow model and conversation binding

Set optional `coding.nativeModel` to an exact admitted native catalog identifier.
On `new`, it overrides the published or native default. It is independent of
`modelAlias`; role alias validation and enterprise alias routing are preserved.
The worker checks the account catalog and the thread response's model/provider.
Inference may still be rejected by the account; that error fails the turn.
Unknown/unavailable models, aliases, reroutes, and alternate API providers fail
without silent substitution or paid API fallback.

The selected model is bound into private checkpoint state and sanitized
`nativeSelection` result evidence. On `resume`, omission retains and explicitly
forwards the saved model even when the owner's local default changed. An explicit
different model or changed published policy requires a new session. Existing
sessions cannot adopt the extension in place. Close uses the saved selection
without a new catalog lookup or model turn. Uncertain native operations remain
unrecoverable and are never automatically replayed.

## Configuration discovery and reload

Each invocation launches a fresh App Server with the runner-projected
`LIGHT_CODEX_HOME` as `CODEX_HOME`. Native-mode `HOME` and `PATH` come from the
runner service. Native login, rules, configured MCP, skills, and applicable
customization remain available. Installed relative paths retain their locations;
commands run in the reconstructed repository (review uses scratch). Referenced
executables must be available under the service's OS restrictions.

A Git bundle contains committed files. Ignored `.codex/config.toml`, untracked
skills, and other source-checkout local files are not implicitly copied. Codex
applies its own trust rules to staged project configuration; original-checkout
trust does not automatically transfer. The worker never trusts the whole spool.
Use installed user configuration and absolute tool references for settings that
must survive arbitrary staged paths.

Owner updates are observed on subsequent invocations without republishing native
rules. Already-running processes are not promised to reload. Evidence contains
source, mode, model, published-policy digest, a digest of opaque native layer
versions, and an ignored-layer count. It excludes settings, credentials, paths,
and disabled-reason text. Revisions reveal drift; model and published-policy
bindings remain fixed across resume.

## Upgrade and qualification

Rebuild Agent and worker together, regenerate image/executable, capability,
adapter-contract, and qualification admission through the deployment generator,
then re-publish source policy and start new workflow sessions. The extension
changes both the worker capability digest and qualification evidence digest;
the upstream schema and native CLI version remain pinned. Do not edit generated snapshots or substitute an
arbitrary installed Codex version for the pinned binary.

```bash
python3 scripts/test-codex-personal-config.py --codex /absolute/path/to/codex
python3 scripts/run-coding-thread-smoke.py \
  --codex /absolute/path/to/codex --model gpt-6-astra \
  --personal-policy inherit
```

The first gate is credential-free: inheritance, explicit overrides, managed
requirements, project trust, and reload revisions. Rust tests exercise admission,
injection, model binding, uncertain checkpoints, and actual namespace write denial.
The live gate uses a temporary private copy of the native login and consumes plan
usage. It checks explicit selection, separate-process new/resume/close, remembered
context, patch continuity, and omission after a changed local default. Repeat
with `--personal-policy trusted-personal-unattended` on the intended runner.
Skipped, failed, or timed-out live cases are **unqualified**, not passed.
Remote-tool role enforcement and the deployed workflow require deployment-specific
qualification.

The mapping is checked against the pinned schemas and binary, alongside the
[App Server documentation](https://learn.chatgpt.com/docs/app-server).

The worker reports capability version `0.153.4-personal-policy-v1`, independently
of the native CLI pin `0.153.4`. A worker with the previous capability digest
cannot advertise `codex-personal-policy-v1`; regenerate admission after upgrading.
The pinned native config loader rejects attempts to redefine the reserved
`openai` provider. This is verified through `config/read` and `thread/start`,
since the serialized Config schema does not expose `model_providers`.

Cancellation is observed at native protocol waits. Active turns receive
`turn/interrupt` with a bounded grace period. Event frames and committed checkpoint
receipts finish delivery even if cancellation arrives during shutdown.
