# Shared Task Workspaces

Status: proposed design, September 10, 2026. This document specifies new
workspace execution behavior; it does not claim that the current bundle-based
coding request, runner, or Claude adapter implements it.

Implementation has started in `crates/task-workspace` and `apps/light-workspace`.
The [local workspace service guide](shared-task-workspaces-runtime.md) describes
the runnable CLI/MCP service, qualification, and remaining integration gaps.
The [Chat and Workflow Integration design](shared-task-workspaces-chat-workflow.md)
defines the next implementation: authenticated workspace jobs, interactive task
selection, durable agent handoff, and deployment gates.

This design extends [Development Workflow Orchestration](development-workflow-orchestration.md)
and [Coding Harness Integration](coding-harness-integration.md). The workflow
remains the lifecycle authority. The runner owns local workspace materialization,
process access, and exclusive mutation. Agents perform implementation and review
through the existing adapter boundary.

## Decision

Register a persistent workspace on a runner host containing a collection of Git
repositories. Granting an agent access to this workspace grants access to all
repositories in it. There is no per-repository permission matrix. Choosing which
repositories a task changes is planning information, not an authorization boundary.

For each task, create a task workspace containing a Git worktree for each
registered repository. Each worktree checks out that repository's task branch,
created from its recorded `develop` revision. A branch is a Git ref; a worktree
is the directory where that branch is checked out. They are related but are not
the same object. Git manages worktrees independently for each repository; the
runner groups them into a single multi-repository task workspace.

Codex and Claude can use the same task workspace and see the same files,
including uncommitted changes. Only one execution may write to a task workspace
at a time. Independent tasks use different worktrees and branches and can run
in parallel. Branches belong to tasks, not permanently to an agent or model.

The normal delivery sequence is:

```text
Existing or new issue
  -> task branches from develop
  -> implementation across repositories
  -> stable review of uncommitted changes by another agent
  -> remediation and renewed review
  -> commit and push task branches
  -> related PRs targeting develop
  -> integration testing on develop
  -> release PR from develop to master
```

## Workspace layout and identity

Example managed layout; actual roots are deployment configuration:

```text
/workspaces/portal/
  repositories/                 # runner-managed source repositories
    light-fabric/
    light-portal/
    portal-view/
  tasks/
    task-384/
      light-fabric/            # branch agent/task-384 in light-fabric
      light-portal/            # branch agent/task-384 in light-portal
      portal-view/             # branch agent/task-384 in portal-view
    task-391/
      light-fabric/            # branch agent/task-391 in light-fabric
      light-portal/            # branch agent/task-391 in light-portal
      portal-view/             # branch agent/task-391 in portal-view
```

The branch names may match across repositories but their histories are separate.
Use a stable task identifier, not an issue number alone: issue numbers can collide
across repositories. Record issue identity as repository plus issue number.
The example task identifiers are illustrative.

A workspace registration records its owner/Host, runner binding, canonical root,
repository identities and remotes, default integration/release branches, and
indexing configuration. Repository membership is versioned. A task pins that
membership revision so adding a repository does not silently alter an active
review. An explicit refresh may add newly registered repositories to the task;
it invalidates review evidence. All repositories in the pinned membership are
available to every agent granted access to that task's parent workspace.

The runner creates a worktree and task branch per repository without switching
branches in the source checkout. Unchanged repositories need no commits or PRs.
Creation records the fetched base commit independently for each repository;
there is no global Git revision shared across repositories. Provisioning is
idempotent and partial creation is journaled. A retry verifies existing
worktrees and ownership instead of overwriting directories or moving branches.
Existing user checkouts and their dirty changes are never adopted implicitly.

## Ownership and proposed records

Names below describe proposed contracts, not existing API fields or tables.

| Record | Essential state |
| --- | --- |
| Workspace | Workspace identity, Host/owner, runner, managed root, membership revision, repository catalog, agent grants |
| Task workspace | Workflow/task identity, parent workspace and membership revision, directory, lifecycle state |
| Repository checkout | Repository identity, worktree path, task branch, base ref and commit, current HEAD, remote tracking state |
| Write lease | Task workspace, execution owner, fencing generation, expiration and renewal state |
| Review checkpoint | Repository membership, all HEADs, index and working-tree content manifest, untracked files, aggregate digest |
| Review result | Checkpoint digest, reviewer identity/adapter, findings, disposition and test evidence |
| Delivery record | Per-repository commits, pushed refs, issue/PR identifiers, expected remote revisions, effect idempotency keys |

`light-workflow` records durable progress and dispatches bounded jobs. The runner
resolves workspace identifiers to registered paths; browser-supplied absolute
paths do not grant access. The worker receives its task workspace, operation
mode, and lease/checkpoint identity. Authentication and adapter choice remain
separate from workspace identity, so an authorized `claude-personal` can review
a task implemented by `codex-personal`.

## Shared implementation and review

1. The implementer obtains the task workspace write lease and edits/tests any
   repository in the workspace. Other agents may inspect it, but observations
   made during writes are provisional and cannot constitute review approval.
2. Before review, the runner stops or waits for all mutating processes, including
   background tools, and captures a checkpoint across every repository.
3. The reviewer receives a fresh execution context, task requirements and the
   checkpoint. It reads the same worktrees, including staged, unstaged, deleted,
   binary and untracked files. Hidden conversation state is not transferred.
4. Review mounts/access are read-only. Tests that generate files run in a
   disposable copy of the checkpoint with separate build output. A reviewer
   asking to edit must enter a remediation stage and obtain the write lease.
5. Findings are bound to the checkpoint digest. Remediation releases the review
   freeze, grants a new writer lease, and invalidates the previous approval.
6. Once review and required tests pass, the trusted commit action verifies the
   checkpoint again and stages/commits exactly its intended file manifest.
   Untracked intended files are included; local caches and generated index data
   are excluded. Any unexpected mutation blocks finalization and requires review.

A checkpoint cannot be represented solely by `git diff`: it must cover HEAD,
the index, working-tree content and untracked intended files in each repository.
Ignored files are not implicitly deliverable; promotion of an ignored file must
be explicit and appear in a new checkpoint. Submodule changes need explicit
repository registration and coordinated checkout handling; a parent worktree
does not automatically materialize or authorize an external repository.

The write lease is enforced by runner-managed process lifetime and filesystem
access, not by advisory text in an agent prompt. Lease expiration alone does not
permit a new writer: the runner must fence or terminate the previous execution
and confirm quiescence. If that cannot be confirmed, the task stays blocked for
recovery. During review, even the prior implementer must lose write access.

Worktrees share Git metadata and objects with their source repository. Serialize
operations that mutate shared repository administration, such as fetch, worktree
creation/removal and maintenance, with a repository-level lock. Ordinary edits
in independent task worktrees remain concurrent. Git operations that can affect
other tasks must use the runner's authorized tooling; filesystem write access
to all shared Git administrative state must not be handed to arbitrary workers.

## Git and GitHub operations

Agents can request clone/fetch/checkout, branch, commit, push, issue creation or
updates, and PR operations through authorized tools. Host-owned credential
helpers or GitHub integrations execute these actions without placing reusable
credentials in prompts, repository files, or task artifacts. Workspace access
covers every repository; operation-level policy still distinguishes reading,
editing, publishing, merging and release actions.

The existing orchestration design's trusted fixed actions remain the mechanism
for externally visible mutations. This supports agent-driven GitHub work without
requiring arbitrary credential-bearing shell execution. Standing workflow
policy can authorize routine actions; an additional user confirmation is needed
only when the action exceeds that authorization or an explicit gate requires it.

For a new task, create an issue if requested by the workflow; otherwise link the
existing issue. Review occurs before committing locally. After approval, commit
and push each changed repository's task branch, then create or update one PR per
repository targeting `develop`. Link related PRs, their issue(s), test evidence
and cross-repository merge dependencies. Persist returned GitHub identifiers so
retries reconcile completed effects instead of duplicating issues or PRs.

Changes after review, including conflict resolution, rebasing, merging a newer
`develop`, or hook-generated edits, require a new checkpoint and the applicable
review/test gates. Validate final commit content against the reviewed manifest.
Do not automatically force-push over unknown remote changes. Reconcile partial
multi-repository commits/pushes from the delivery journal; do not reset completed
repositories to simulate an atomic operation.

PRs merge to `develop` under the project's normal checks. Cross-repository
merges are not atomic: use backward-compatible changes, a documented merge
order and integration tests against the recorded combination of revisions.
A release workflow promotes a qualified `develop` revision to `master` through
a release PR. Task completion does not implicitly authorize a release merge.

## Shared indexing and tools

Provide host-managed GitNexus and codebase-memory-mcp integrations to all
workspace-authorized agents. Their configuration and lifecycle belong to the
workspace service rather than a particular Codex or Claude conversation.
Repository instructions such as `AGENTS.md` remain visible in every worktree.

Index identity includes workspace, task worktree, repository, HEAD and content
checkpoint/generation. A source checkout's index must not silently answer as
though it describes a task branch or its uncommitted edits. Index queries return
freshness metadata. Review uses indexes matching its checkpoint or reports stale
coverage and falls back to direct source inspection. Cross-repository queries
operate over the task's pinned repository membership and identify the contributing
repository/revision for each result.

Shared indexing processes must not write into frozen source trees. Keep index
storage and lock files outside reviewed worktrees where the tool permits it;
otherwise index a disposable checkpoint copy. Serialize incompatible index
updates. Qualification must establish how each tool handles worktrees, repository
identity, uncommitted changes and cross-repository relationships. This design
does not assume both tools already provide those capabilities or that their
results are interchangeable.

## Chat and execution contract

Keep turn type, repository input mode, and adapter routing separate:

- **Turn type** expresses the interaction supported by the agent's published
  policy. A workspace does not itself grant an otherwise forbidden turn type.
- **Input mode** selects an immutable bundle or a registered task workspace.
- **Adapter/authentication profile** selects the coding harness and credentials;
  a question about code must not imply an automatic switch to `llm-gateway`.

For workspace execution, Chat selects a registered workspace and starts or resumes
a task. It displays repositories, branches, implementation/review stage, active
writer, indexing freshness, findings and related PRs. It does not ask for bundle
URI/hash/size. Repository selection may help scope a prompt but is not an access
control. The coding-only agent can support code-understanding jobs through its
coding adapter with read-only execution authority; this requires a defined job
contract rather than pretending every question is a patch implementation.

Extend the versioned request protocol with a discriminated input contract, such
as `bundle` versus `taskWorkspace`. Workspace jobs bind workspace/task identity,
expected membership/checkpoint, execution role and lease generation. Keep bundle
validation intact; do not relax its single-repository digest requirements to
smuggle in mutable directories. Unsupported runtimes reject the new contract.

## Recovery and lifecycle

Task states include provisioning, implementing, review-frozen, remediating,
ready-to-publish, publishing, PR-open, integrated, paused and archived. A failure
records the interrupted stage and evidence rather than erasing working changes.
On restart, reconcile the journal, actual worktrees, active processes, local Git
refs and remote effect records before resuming. Never infer review approval from
an agent's completion message alone.

A paused task retains its worktrees and branch state. Cancellation terminates
executions and retains recoverable changes until the retention policy permits
cleanup. Cleanup requires no active leases and explicit checks for dirty files,
unpushed commits and delivery status. Preserve referenced review/test evidence.
Remove worktrees with Git's worktree management and never recursively delete a
path supplied by the client. Removing an agent grant stops new access and fences
active access according to the workspace revocation policy.

## Delivery phases and acceptance criteria

### Phase 1: Persistent multi-repository tasks

Implement workspace registration, membership snapshots, per-task worktrees,
versioned input contracts and runner lease/journal handling. Demonstrate two
parallel tasks over the same three repositories with different task branches;
changes and branch operations in one task must not modify the other. Existing
bundle requests continue to pass their admission and staging tests.

### Phase 2: Shared review and indexing

Implement checkpoints, enforced review freeze, handoff, disposable test copies
and indexing integrations. A second authorized agent must see the implementer's
uncommitted changes in all repositories. Attempts to mutate a frozen workspace
must fail. An edit after review must invalidate approval. Verify index freshness
for different branches and untracked files, plus recovery from a lost writer.

### Phase 3: GitHub delivery and release integration

Implement authorized Git/GitHub tools, idempotent delivery records and linked
PRs targeting `develop`. Exercise a partial push failure and retry without
lost commits or duplicate PRs. Exercise a changed remote branch and conflict
resolution requiring renewed review. Test the recorded multi-repository revision
set before release promotion to `master`.

### Phase 4: Adapter interoperability

Qualify Codex implementation followed by Claude review and the reverse against
the same workspace contracts. Adapter qualification, subscription authentication
and tool access remain independent gates. Switching agents must preserve files,
checkpoint identity and workflow state without reusing private model context.
