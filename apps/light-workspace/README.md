# light-workspace

Owner-local CLI and stdio MCP service for persistent multi-repository task
workspaces. The implementation is shared in `crates/task-workspace`.

This is the local workspace service portion of the
[Shared Task Workspaces design](../../docs/src/product/light-agent/shared-task-workspaces.md).
It is **not yet connected to Portal Chat, signed workspace policy publication,
or the controller's execution dispatch**. The current Portal coding form still
requires a bundle. Registering this service does not change that form or enable
a new native coding adapter.

## Build and register

```bash
cargo build -p light-workspace
# Discovery reads direct child Git repositories and their origin URLs.
# It does not fetch, checkout, push, or change existing branches.
target/debug/light-workspace discover /home/steve/workspace personal HOST_ID \
  com.networknt.agent.codex-personal-1.0.0 \
  com.networknt.agent.claude-personal-1.0.0 > discovery.json
```

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
  "agents": ["codex-personal", "claude-personal"],
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

Register using a new private directory (mode 0700):

```bash
target/debug/light-workspace /absolute/private/store register workspace.json
```

Registration is metadata-only and idempotent for identical input. It rejects
changes to an existing registration. The current implementation does not yet
provide membership migration or live grant revocation. Do not edit registration
files while tasks or clients are active. Credentials stay in host-managed Git
and GitHub authentication, not this JSON or the model's tool arguments.

## Use from a local agent

Configure one stdio MCP server in each native client's supported MCP configuration.
Use a separate fixed identity for each agent:

```json
{
  "mcpServers": {
    "personal-workspace": {
      "command": "/absolute/path/to/light-workspace",
      "args": ["/absolute/private/store", "serve", "personal", "codex-personal"]
    }
  }
}
```

The JSON above is a launch descriptor; translate it into the native client's
configuration format where required. For the reviewer use `claude-personal` as
the last argument. This identity is a trusted local launch setting, not proof
of authentication for a network service. Never expose this process through an
unauthenticated network wrapper. Host administrators can access/change the store;
the process does not isolate one host administrator from another.

The server exports `task_workspace`, with a strict operation-specific input
schema. No request can override the bound workspace or agent identity. Supported
operations are `create`, `status`, `files`, `read`, `edit`, `execute`, `freeze`,
`review`, `remediate`, `commit`, `push`, `github`, `index`, `index-status` and
`index-query`. The `call` command accepts the same input JSON on stdin for local
administration and testing:

```bash
printf '%s\n' '{"operation":"create","task":"task-384"}' |
  target/debug/light-workspace /absolute/private/store call personal codex-personal
```

Creation clones dedicated managed bare repositories, fetches each integration
branch and creates one worktree/task branch per repository. It never attaches
worktrees to your existing checkout. Integration branches must already exist
on the configured remotes; no automatic fallback or remote branch creation
occurs. A task's branch is `agent/TASK_ID` in every repository.

## Implement, review and deliver

1. `create` a task. `files` returns the repository contents and revision evidence;
   `read` returns UTF-8 file content and its digest (up to 1 MiB).
2. `edit` with `{repository, path, content, expectedDigest}`. For a new file use
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
