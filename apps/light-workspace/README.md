# light-workspace

Owner-local CLI and stdio MCP service for persistent multi-repository task
workspaces. The implementation is shared in `crates/task-workspace`.

This is the local workspace service portion of the
[Shared Task Workspaces design](../../docs/src/product/light-agent/shared-task-workspaces.md).
Portal Chat can dispatch standalone inspect/implement tasks through the personal
native runner when a matching `codingProfile.workspaceBindings` policy is
published. The CLI/MCP registration alone does not enable that integration.
See [Chat and Workflow integration](../../docs/src/product/light-agent/shared-task-workspaces-chat-workflow.md)
for the implemented path and the remaining workflow milestone.

For a guided walkthrough, see the [shared task workspace tutorial](https://doc.lightapi.net/tutorial/workspace/shared-task-workspaces.html).

## Build and register

Run these commands in a Linux terminal as the user that will launch the agents.
The examples use Steve's host paths. Prerequisites are Rust/Cargo, Git, `jq`,
bubblewrap (`/usr/bin/bwrap`) for `execute`, and authenticated `gh` for GitHub
operations. Git must be able to read each registered origin without interactive
credential prompts. Index providers are optional and must be configured before
registration if you intend to use them.

```bash
cd /home/steve/workspace/light-fabric
cargo build --release -p light-workspace
umask 077
# Replace HOST_ID with the Portal Host ID that owns this workspace.
# Discovery reads origins and local refs without changing source checkouts.
target/release/light-workspace discover /home/steve/workspace personal HOST_ID \
  com.networknt.agent.codex-personal-1.0.0 \
  com.networknt.agent.claude-personal-1.0.0 > discovery.json

# Set grants BEFORE the first registration. Do not redirect onto discovery.json.
jq '.workspace | .operations = ["edit", "execute", "review", "commit", "push", "issue", "pull-request"]' \
  discovery.json > workspace.json
jq '{id, hostId, agents, operations, indexers, repositoryCount: (.repositories | length)}' workspace.json
```

If you already registered successfully, skip discovery and registration and go to
[Connect a local agent](#connect-a-local-agent). Use the persisted registration
when checking IDs and grants; changing the input file does not update it.

The result has `workspace`, `integrationBranchNotKnownLocally`, and
`skippedRepositories` with directory/reason diagnostics for unsuitable names or
unreadable origins. A skipped child does not stop discovery. Extract the
`workspace` object into a private registration file. Select its operation grants
explicitly; discovery grants none. Each agent grant covers all repositories.
Example (replace paths and identities):

```json
{
  "schemaVersion": 1,
  "id": "personal",
  "hostId": "HOST_ID",
  "agents": ["com.networknt.agent.codex-personal-1.0.0", "com.networknt.agent.claude-personal-1.0.0"],
  "operations": ["edit", "execute", "review", "commit", "push", "issue", "pull-request"],
  "repositories": [
    {
      "name": "service",
      "source": "git@github.com:OWNER/SERVICE.git",
      "integrationBranch": "develop",
      "releaseBranch": "master"
    }
  ],
  "indexers": {
    "gitnexus": {
      "executable": "/absolute/path/to/gitnexus",
      "args": ["analyze", "--force"],
      "timeoutSeconds": 300
    },
    "codebase-memory-mcp": {
      "executable": "/absolute/path/to/codebase-memory-mcp",
      "args": [],
      "timeoutSeconds": 300
    }
  }
}
```

Register the extracted `workspace.json`, not the discovery wrapper. Keep the
persistent store outside the source tree:

```bash
install -d -m 700 /home/steve/.local/share/light-workspace
target/release/light-workspace \
  /home/steve/.local/share/light-workspace register workspace.json
```

Expected output: `{"registered":"personal"}`. The private directory stores managed
repositories, task worktrees, checkpoints, and indexes. Both agents use this path.

Registration is metadata-only and idempotent for identical input. It rejects
changes to an existing registration. The current implementation does not yet
provide membership migration or live grant revocation. Do not edit registration
files while tasks or clients are active. Credentials stay in host-managed Git
and GitHub authentication, not this JSON or the model's tool arguments.

## Connect a local agent

Here, **local agent** means the Codex CLI or Claude Code application running as
`steve` on this Linux host. Open an ordinary terminal to run the commands below.
These steps do not configure the `codex-personal` service in Portal Chat or require
a Compose restart. The MCP client starts `light-workspace` as a child process and
exchanges JSON over stdin/stdout; there is no URL or port to enter, and you do not
start `serve` manually in another terminal.

### Check the registered identities

```bash
jq '{id, agents, operations}' \
  /home/steve/.local/share/light-workspace/personal/workspace.json
```

The examples below assume the full agent IDs produced by the discovery command.
Every launch identity must match an entry in `agents` exactly. The MCP server name
`personal-workspace` is only a client-side label; the workspace ID is `personal`.
Both clients use the same private store, with a different agent identity. This is
an owner-local launch setting, not network authentication or isolation between
host administrators. Do not run the clients as different Linux users against this
0700 store without designing an authenticated shared service first.

### Add the server to Codex CLI

Run this once in your Linux terminal, using your existing Codex installation and
login. It updates your local Codex configuration, not `workspace.json`:

```bash
codex mcp add personal-workspace -- \
  /home/steve/workspace/light-fabric/target/release/light-workspace \
  /home/steve/.local/share/light-workspace serve personal \
  com.networknt.agent.codex-personal-1.0.0

codex mcp list
```

The list should contain `personal-workspace` and the command above. Start a new
Codex CLI session by running `codex`; in its interactive prompt enter `/mcp`.
Check that `personal-workspace` connects and exposes `task_workspace`.
An existing session may need to be restarted to pick up the new configuration.

Codex stores this entry in `~/.codex/config.toml`. Do not paste an `mcpServers`
JSON object into that TOML file. For long workspace operations you can add
`tool_timeout_sec = 600` inside the existing `[mcp_servers.personal-workspace]`
table. That is a client timeout, not a guarantee that indexing all repositories
will finish in ten minutes. Prefer the direct CLI for the initial clone and large
index builds. See the [official Codex MCP documentation](https://developers.openai.com/codex/mcp/).

### Add the server to Claude Code (optional second agent)

In your Linux terminal, with Claude Code installed and signed in:

```bash
claude mcp add --transport stdio --scope user personal-workspace -- \
  /home/steve/workspace/light-fabric/target/release/light-workspace \
  /home/steve/.local/share/light-workspace serve personal \
  com.networknt.agent.claude-personal-1.0.0

claude mcp get personal-workspace
```

Start a new session with `claude`, then use `/mcp` to inspect the connection.
`--scope user` keeps this setting in your user configuration instead of creating
a repository `.mcp.json`. See the [Claude Code MCP documentation](https://code.claude.com/docs/en/mcp).
You can connect Codex first and add Claude later; two separate processes share
persistent tasks through the same store and task locks.

### Create the first task outside the model session

Run this in your Linux terminal after checking Git access to the registered
origins. The first creation clones **every registered repository** and fetches its
integration branch. For a 129-repository workspace this can take time and disk
space; registration itself did not download these repositories. The managed copies
come from the recorded origins, so uncommitted edits in `/home/steve/workspace`
are not imported.

```bash
printf '%s\n' '{"operation":"create","task":"workspace-smoke-1"}' | \
  /home/steve/workspace/light-fabric/target/release/light-workspace \
  /home/steve/.local/share/light-workspace call personal \
  com.networknt.agent.codex-personal-1.0.0
```

Expected output includes `"state": "ready"` and `checkouts` with one managed path
per repository, each on `agent/workspace-smoke-1`. Retrying the same task ID resumes
provisioning or returns the existing task. Do not invent a new task ID on every
retry. Missing `develop` branches must be resolved explicitly; there is no fallback
to `master`. Task creation makes no commits, pushes, issues, or PRs.

### Ask the connected agent to use it

In the **Codex conversation**, enter:

> Use the personal-workspace MCP server's task_workspace tool. Call status for
> task workspace-smoke-1 and report its state and repository names. Use only the
> workspace tools for this task; do not access its files through host shell or
> native file tools. Do not edit, commit, push, or create GitHub resources.

The tool arguments for that first call are:

```json
{"operation":"status","task":"workspace-smoke-1"}
```

Then ask it to read a specific file, for example:

> For workspace-smoke-1, use task_workspace read to read README.md in repository
> light-fabric and summarize the project. Do not change files.

Use a repository name returned by `status` if `light-fabric` is not registered.
This confirms the model can use the tool rather than merely seeing its name.
The model client handles its own model login; the workspace server does not log
in to Codex/Claude on your behalf.

The manager must remain the only agent write path. Instructions to use MCP are
workflow guidance, not a security boundary: an agent with unrestricted host file
or shell access can bypass the freeze. This tutorial does not configure a hardened
native-agent sandbox. Workspace `execute` does sandbox the commands it launches.

## Implement, review and deliver

1. `create` a task. `files` returns the repository contents and revision evidence;
   `read` returns UTF-8 file content and its digest (up to 1 MiB).
2. `edit` takes a nested `edit` object: `{operation: "edit", task, edit: {repository, path, content, expectedDigest}}. For a new file use
   `expectedDigest: null`; for deletion use `content: null`. Digest preconditions
   prevent overwriting another agent's intervening changes.
3. For local commands, `execute` takes `access: "implement"`, an absolute
   executable under `/usr`, an argument array, and `timeoutSeconds` (1–300).
   Linux bubblewrap provides an isolated environment with writable task files,
   read-only Git administration, no host home/credentials, and no network.
   This command is for inspection/build tools; it does not launch an authenticated
   native Codex or Claude model session. Network-dependent builds need a future
   explicitly authorized dependency mechanism.
4. `freeze` captures the cross-repository HEAD, staging state, file content,
   executable modes, deletions and untracked files. All further edits are rejected.
5. The second agent reads the same files or runs `execute` with `access: "review"`.
   Review execution mounts task files read-only. Build/test commands that need
   source-tree writes must use an appropriate disposable environment; a general
   writable review-test-copy command is not implemented yet.
6. Submit `review` with the exact checkpoint digest, `approved` and `findings`.
   Findings are limited to 64 KiB. An agent that contributed edits cannot approve its own task changes. `remediate` invalidates
   approval and reopens editing. Any content or staging change makes review stale.
7. `commit` accepts a message after approval. It stages the reviewed files, rejects
   clean-filter transformations, disables Git hooks, and commits changed repositories.
   Configure the host's committer identity beforehand. Repeating the same action
   reconciles already-created commits; it does not duplicate them.
8. `push` creates remote task refs only if absent or already at the reviewed commit.
   An absence lease prevents a racing push from overwriting a newly created ref.
   Unknown remote changes require reconciliation. It never pushes integration or
   release refs.
9. `github` with `action: "issue"` creates an issue; `action: "pull-request"` creates
   a PR after a successful push. Supply `repository`, `title` and `body`; use the
   body to link existing issues and related repository PRs. PRs target the configured
   integration branch (`develop` by default), and their head is checked against the
   reviewed commit. GitHub.com SSH/HTTPS origins are supported. The host needs `gh`
   authenticated for those repositories. No GitHub Enterprise URL adapter is supplied.

Task worktrees share a task-wide kernel lock. Separate tasks run concurrently.
Running commands journal their generation before launch. A timeout or lost process
leaves an interrupted task; no new writer is automatically admitted. A failure to
spawn the sandbox restores the previous state. After confirming that the old
execution and its descendants have terminated, an operator can recover using the
generation returned by `status`:

```bash
target/debug/light-workspace /absolute/private/store recover personal TASK_ID codex-personal --fenced-generation GENERATION
```

The command rejects stale generations and active task locks. The flag records the
operator's fencing assertion; it does not terminate processes itself. Unchanged
read-only review executions retain their checkpoint and approval. Writer recovery
invalidates them. Legacy interrupted records without execution context also
invalidate them. There is no model-facing recovery override. Automatic crash
reconciliation and task lifecycle retention still require runner integration.

GitHub effects persist intent before execution. On uncertain issue retries, the
service enumerates paginated issue bodies through the GitHub API and matches the
exact recorded marker, excluding pull requests. It does not rely on full-text
search indexing of HTML comments. PR retries match the marker on the task branch.
Missing or ambiguous matches remain uncertain and never cause duplicate creation. The current service creates issues/PRs but does not edit existing issue
content, merge PRs, run release automation, or track required CI checks.

## Indexing

`index` takes a configured `provider`. It requires a frozen checkpoint and makes
an independent clone per repository with the reviewed working-tree contents.
Generated `AGENTS.md`, `.gitnexus`, and `.codebase-memory` files stay in those
copies. Index caches and registry HOME are isolated per task/index generation.
Host-owned indexing commands are trusted administrative tools, not arbitrary
model-provided executable paths. Repeated indexing of the same checkpoint reuses
the active index. A replacement is published only after every repository succeeds;
a failed build preserves the previous receipt and removes the failed copy. Under
the task lock, the service reclaims obsolete generation directories, retaining the
active generation plus at most one build in progress. Provider processes inherit
the lock so restart cleanup cannot remove a still-running provider's generation.

`index-status` reports the input checkpoint digest and freshness. Freshness means
that source identity matches; it does not prove complete parsing or complete
cross-repository relationships. Per-repository provider output/coverage is retained
in private `*-output.json` files under the returned index root.

`index-query` requires `provider`, `repository` and `query`. GitNexus receives a
concept query; codebase-memory-mcp receives a symbol-name regex through
`search_graph`. Queries reject stale input and use the correct task's private
provider cache. Automatic cross-repository graph linking and provider-native
architecture/impact tools are not yet exposed by this facade.

The codebase-memory adapter uses the upstream
[one-shot CLI contract](https://github.com/DeusData/codebase-memory-mcp#cli-mode),
with no installer or watcher activation. Qualified locally with GitNexus's
installed CLI and codebase-memory-mcp 0.10.8 on a small Python repository.
Both indexed and found its function without changing the frozen source.

## Current limits and qualification

This first implementation rejects checkpoint symlinks and submodules rather than
following paths outside managed worktrees. File tools handle UTF-8 text; binary
changes can be inspected through file digests and made by sandboxed commands.
The manager must be the sole agent write path. Giving a native agent unrestricted
host shell access alongside these tools would bypass the review freeze.

Run:

```bash
cargo test -p task-workspace -p light-workspace
# Explicit host qualification, including the normally ignored namespace tests:
cargo test -p task-workspace --test workspaces -- --include-ignored
cargo clippy -p task-workspace -p light-workspace --all-targets -- -D warnings
```

Tests use disposable repositories and a mock GitHub CLI. They cover three-repository
parallel tasks, shared edits, digest preconditions, stale/self-review rejection,
read-only review, host-file isolation, timeout recovery, index copies/freshness,
reviewed multi-repository commits, local-remote pushes, PR base/head verification,
uncertain-effect retry, and the stdio MCP transport. They do not perform billable
model calls or create real GitHub issues/PRs.

## Setup troubleshooting

| Symptom | Check or next step |
|---|---|
| `No such file or directory` from registration | Run from the directory containing `workspace.json`, or give its absolute path. Extract `.workspace` from discovery first. |
| Shell says the binary does not exist | Build with `--release` and use `target/release/light-workspace` consistently. |
| `workspace already registered with different membership or grants` | Registrations are immutable. Before any tasks or active clients exist, back up the unused workspace directory and register the corrected input. With existing tasks, retain the registration and use a new workspace ID/store until migration is implemented. Never overwrite stored metadata to bypass the guard. |
| `agent has no workspace grant` | Match the full agent ID in the stored `agents` array; a shortened name is a different identity. |
| `serve` appears to hang in a terminal | It is waiting for MCP JSON. Let the client launch it; use `call` for a direct JSON operation. |
| `/mcp` does not show the server | Check the client registration and absolute binary path, then start a new client session. |
| Tool timeout during first task creation | Use the terminal `call` command above and retry the same task ID; check Git authentication and integration branches. |
| `index provider is not configured by host` | Discovery leaves `indexers` empty. Add provider configuration before registration; merely installing an indexer does not register it. Existing registrations have no update command. |
| Review command cannot write | Review mounts source files read-only. Writable build/test copies are not yet implemented. |
| Portal Chat still requests a repository bundle | Expected: this local MCP setup is not wired into Portal Chat yet. |
