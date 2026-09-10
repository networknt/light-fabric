# Shared Task Workspace Runtime

The first implementation is an owner-local CLI and stdio MCP service named
`light-workspace`, backed by the `task-workspace` Rust crate. It provides workspace
registration, multi-repository task worktrees, shared file tools, review freezes,
reviewed commits, GitHub task-branch delivery and checkpoint-scoped indexing.

The operational guide is maintained with the executable:

{{#include ../../../../apps/light-workspace/README.md:8:}}
