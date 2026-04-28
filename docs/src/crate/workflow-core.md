# Workflow Core

`workflow-core` contains the Rust model for the Agentic Workflow DSL.

The crate is schema-oriented: its structs and enums represent workflow
documents, tasks, authentication blocks, durations, timeouts, errors, and
supporting map types.

## Main Areas

- workflow document metadata
- task definitions
- call task protocol definitions
- ask and assert task definitions
- duration and timeout models
- error definitions
- ordered map support for workflow task lists

## Usage

```rust
use workflow_core::models::workflow::{
    WorkflowDefinition,
    WorkflowDefinitionMetadata,
};

let document = WorkflowDefinitionMetadata::new(
    "lightapi",
    "example",
    "1.0.0",
    Some("Example".to_string()),
    None,
    None,
    None,
);
let workflow = WorkflowDefinition::new(document);
```

## Consumers

`workflow-builder` builds on this crate. `light-workflow` and workflow-related
services use the model for loading, validating, and executing workflow
documents.
