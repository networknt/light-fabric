# Light-Workflow

`light-workflow` is the workflow execution service for Agentic Workflow
documents.

It loads workflow definitions, executes workflow tasks, integrates with
`light-rule` for rule-backed checks, and exposes workflow execution APIs.

## Key Dependencies

- `workflow-core`
- `light-rule`
- `axum`
- `sqlx`
- `reqwest`

## Role

`light-workflow` is the runtime service that turns workflow specifications into
long-running execution state. It is used by agentic flows, human-in-the-loop
orchestration, and integration-test style automation.
