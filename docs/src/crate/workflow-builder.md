# Workflow Builder

`workflow-builder` provides fluent builders for creating Agentic Workflow
definitions programmatically.

It depends on `workflow-core` for the actual model types and layers a builder
API on top so applications and tests can construct valid workflows without
manually assembling nested maps.

## Main Areas

- workflow metadata construction
- authentication definitions
- task definitions
- nested `do`, `for`, `fork`, `try`, and other task structures
- YAML/JSON serialization through `workflow-core` model types

## Usage

```rust
use workflow_builder::services::workflow::WorkflowBuilder;

let workflow = WorkflowBuilder::new()
    .use_dsl("1.0.0")
    .with_namespace("lightapi")
    .with_name("example")
    .with_version("1.0.0")
    .build();
```

## Relationship To Workflow Core

Use `workflow-core` when you need direct access to the schema model. Use
`workflow-builder` when you want an ergonomic construction API.
