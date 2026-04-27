# Workflow Client Architecture

Light-Fabric workflow execution should run as a containerized backend service, not as logic embedded in a portal screen, CLI, scheduler, or agent. The workflow service owns process state, task state, audit records, API invocation, agent invocation, and human-in-the-loop transitions. Clients are thin interaction surfaces over the same service APIs.

This separation lets the same workflow instance be driven by a portal chat session, a worklist user, a CLI command, a scheduler, or an AI agent without creating multiple execution models.

## Goals

- Provide one authoritative workflow runtime for long-running enterprise processes.
- Support human-in-the-loop tasks from both conversational and worklist interfaces.
- Support headless execution for live tests, scheduled runs, and CI/CD.
- Keep all clients stateless or lightly stateful; workflow state lives in `light-workflow`.
- Make role assignment, audit, and retry behavior consistent across UI, CLI, scheduler, and agent use.

## Runtime Service

`light-workflow` should be deployed as a portal service in a container alongside the other portal services. It should expose APIs for workflow definitions, workflow instances, task claiming, task completion, event streaming, and operational control.

The service is responsible for:

- loading workflow definitions
- starting workflow instances
- persisting `process_info_t` and `task_info_t`
- executing API calls and assertions
- invoking agents for agent-owned tasks
- pausing on `ask` and approval tasks
- assigning human tasks to users or roles
- resuming workflows when a human answer is submitted
- emitting workflow and task events
- recording audit history

Clients should never execute workflow steps themselves. They should only start workflows, inspect workflow state, and complete assigned tasks.

## Client Surfaces

### Portal Chat

The portal chat client is the guided conversational interface for a single user working through a process. It is useful when the workflow needs to ask clarifying questions, explain the next action, or guide a user through a complex multi-endpoint operation.

Typical uses:

- API onboarding
- API endpoint publication to an AI gateway
- guided configuration
- troubleshooting and remediation workflows
- interactive approval with explanation

The chat client should call the workflow service for current state and submit answers to waiting tasks. It may stream workflow events and render agent explanations, but it should not own workflow state.

### Worklist

The worklist is the enterprise task inbox. It is the right interface for multi-user coordination, role-based assignment, approvals, escalations, and audit-sensitive operations.

Typical uses:

- approval tasks
- compliance review
- operations handoff
- role-based queue processing
- task claim and release
- delegated work
- due-date and priority management

The worklist should be built around waiting human tasks. A task may have:

- assigned user
- candidate roles
- assigned role
- priority
- due time
- claim status
- comments
- completion payload
- audit trail

The worklist is especially important because many enterprise workflows are not purely conversational. They need accountable ownership and coordination between multiple humans.

### CLI

The CLI is a developer and automation client. It should use the same workflow service APIs as portal-view and should not contain separate execution logic.

Typical uses:

- local workflow testing
- live parity tests
- CI/CD automation
- scheduled headless runs
- debugging stuck workflow instances
- submitting test data
- completing simple waiting tasks from scripts

Example commands:

```bash
light-workflow start portal.onboard-api --input input.yaml
light-workflow status <instance-id>
light-workflow tasks --role portal-admin
light-workflow claim <task-id>
light-workflow answer <task-id> --value approve
light-workflow logs <instance-id>
light-workflow cancel <instance-id>
```

The CLI should be added after the workflow APIs stabilize. It will be valuable for developers and automation, but the worklist and portal chat should drive the primary enterprise UX.

## API Boundary

The workflow service should expose a stable API boundary that all clients use. The API can be HTTP, JSON-RPC, or both, but the concepts should remain the same.

Core operations:

```text
workflow.start
workflow.getInstance
workflow.listInstances
workflow.getEvents
workflow.listTasks
workflow.getTask
workflow.claimTask
workflow.releaseTask
workflow.completeTask
workflow.delegateTask
workflow.cancelInstance
```

For streaming clients, the service should expose workflow events through Server-Sent Events, WebSocket, or another portal-standard event mechanism.

Important event types:

- workflow started
- task started
- task completed
- task failed
- task waiting for input
- task assigned
- task claimed
- task completed by human
- agent started
- agent completed
- workflow completed
- workflow failed

## Human Task State

`ask` and approval-style tasks should enter a waiting state. While waiting, the workflow instance remains active, but the task is no longer executable by the worker loop until a human answer is submitted.

Recommended states:

```text
A = active
W = waiting for input
C = completed
F = failed
X = canceled
```

The waiting task should include enough metadata for all clients:

- prompt
- input mode
- options
- validation rules
- default value
- sensitivity flag
- assignment metadata
- explanation metadata
- timeout policy

The completion API should validate submitted input against the task definition before resuming the workflow.

## Assignment Model

Human tasks should support both direct assignment and role-based queues.

Recommended fields:

```text
assigned_user
assigned_role
candidate_roles
claimed_by
claimed_ts
due_ts
priority
comments
```

A role-based task can appear in the worklist for all users with a matching role. Once a user claims it, the task becomes owned by that user until completed, released, delegated, or timed out.

## Recommended Build Order

1. Implement stable workflow service APIs for start, status, events, task list, task claim, and task completion.
2. Harden the `ask` resume path and waiting task state machine.
3. Build the worklist because it forces the assignment, audit, and state model to be correct.
4. Build the portal chat workflow interaction on top of the same task APIs.
5. Add the CLI after the API shape stabilizes.
6. Add scheduler integration for hourly live tests and headless workflow runs.

## Design Rule

There must be one workflow runtime and one task state model. Chat, worklist, CLI, scheduler, and agents are only clients of that runtime.

This keeps enterprise workflow behavior auditable, testable, and consistent regardless of how a workflow is started or resumed.
