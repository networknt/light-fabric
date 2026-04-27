# LightAPI Description Design

LightAPI Description is the endpoint capability specification used by Light-Fabric agents, workflows, live tests, and portal API administration.

It describes how an API endpoint is discovered, invoked, explained, and verified. It is intentionally separate from the Agentic Workflow Specification. Workflow describes process orchestration. LightAPI describes endpoint capability.

## Why LightAPI

OpenAPI is useful for REST APIs, and OpenRPC is useful for JSON-RPC APIs, but Light-Fabric needs a common description model across multiple enterprise protocols:

- REST / HTTP
- OpenAPI-described HTTP
- JSON-RPC 2.0
- OpenRPC-described JSON-RPC
- gRPC
- MCP tools, resources, and prompts

LightAPI provides a single agent-facing and workflow-facing description layer over these protocols.

The goal is not to replace OpenAPI or OpenRPC. The goal is to reference them where they exist and add the missing information needed by agents and workflow live tests.

## API-Level Authoring, Endpoint-Level Consumption

Light-Portal may let teams author descriptions at the API level for convenience. However, workflows and agents consume descriptions at the endpoint level.

This distinction is important because real workflow processes rarely use a whole API. They usually combine selected endpoints from multiple APIs.

For example, onboarding an API to an AI gateway may consume:

- one endpoint from API registration
- one endpoint from API version management
- one endpoint from API instance management
- one endpoint from config server
- one endpoint from gateway linking
- one endpoint from controller reload
- one or more MCP tools exposed through the gateway

Each consumed operation should have an endpoint-level description with a stable `endpointId`.

API-level descriptions are still useful as catalogs. Endpoint-level descriptions may inherit shared API context such as:

- environments
- authentication
- secrets
- sources
- common tags
- lifecycle metadata

## Relationship To Agentic Workflow

Agentic Workflow and LightAPI have different responsibilities.

| Concern | Agentic Workflow | LightAPI Description |
| :--- | :--- | :--- |
| Process order | Yes | No |
| Branching and retries | Yes | No |
| Human-in-the-loop | Yes | No |
| Endpoint invocation contract | Reference only | Yes |
| Input and result examples | Optional workflow fixtures | Yes |
| Result verification expectations | Calls `assert` | Describes expected result cases |
| Agent progressive disclosure | Uses selected endpoints | Defines disclosure levels |
| Live testing | Orchestrates execution | Supplies examples and expected results |

In live tests, the workflow should use example data from LightAPI descriptions and workflow fixtures instead of asking for user input.

In interactive runs, the workflow may ask the user for missing values, then invoke endpoints described by LightAPI.

## Relationship To Centralized Agent Skills

LightAPI endpoint descriptions are a source of agent skills.

The centralized skill registry should not require every API operation to be manually rewritten as a separate skill. Instead, Light-Portal can publish selected LightAPI endpoint descriptions into the skill registry as invokable capabilities.

The skill registry adds:

- permission-aware discovery
- semantic search
- skill grouping
- agent persona scoping
- audit around skill disclosure and execution

LightAPI provides:

- endpoint identity
- protocol details
- input schema
- request mapping
- result shape
- examples
- behavior notes
- result cases

Together, they allow an agent to discover a capability as a skill, progressively load only the endpoint details it needs, and execute through the workflow or controller runtime.

See [Centralized Agentic Skill Registry](centralized-agent-skills.md) for the skill registry design.

## Core Document Concepts

A LightAPI document should support both API-level catalogs and endpoint-level documents.

Important top-level concepts:

- `lightapi`: specification version
- `profile`: `api` or `endpoint`
- `info`: name, title, version, namespace, owner, contact
- `context`: inherited catalog context for endpoint-level documents
- `sources`: OpenAPI, OpenRPC, protobuf, MCP, or raw protocol references
- `environments`: environment-specific server details
- `secrets`: required secret names
- `authentications`: reusable authentication policies
- `operations`: endpoint operation descriptions
- `testSequences`: linear endpoint test sequences
- `agent`: progressive disclosure and skill metadata

For `profile: endpoint`, the document should describe at most one operation.

## Operation Model

Each operation represents one endpoint-level capability.

Common fields include:

- `operationId`: local operation identifier
- `endpointId`: globally stable endpoint identifier
- `title`
- `summary`
- `description`
- `visibility`
- `lifecycle`
- `tags`
- `capability`
- `agent`
- `input`
- `request`
- `result`
- `examples`

The `input` section describes the logical interface the agent or workflow sees.

The `request` section describes how logical input maps to the wire protocol.

The `result` section describes expected output, result cases, and failure shapes.

## Protocol Coverage

### HTTP And OpenAPI

For raw HTTP, the operation describes method, endpoint, headers, query, path, and body mappings.

For OpenAPI, LightAPI references the OpenAPI document and operation, then adds agent-oriented behavior, examples, and result expectations.

### JSON-RPC And OpenRPC

For direct JSON-RPC, the operation describes endpoint, method, params, id behavior, notification behavior, and error policy.

For OpenRPC, LightAPI references the OpenRPC document and method. The workflow runtime can use the OpenRPC document to validate that the method exists and that required params are present before calling it.

### gRPC

For gRPC, the operation describes service, method, protobuf source, transport, metadata, request mapping, and result mapping.

For browser or gateway-mediated enterprise deployments, gRPC over WebSocket can be represented as a transport on the structured protocol operation.

### MCP

For MCP, the operation describes tool, resource, or prompt invocation.

Tool listing alone is not enough. The description must also include:

- input schema
- result shape
- examples
- behavior differences for important input cases
- error cases
- verification expectations

MCP stdio is not a priority for enterprise portal deployment. HTTP and streamable HTTP transports should be the main runtime targets.

## Result Cases And Verification

LightAPI should describe expected result behavior, but Agentic Workflow should execute the actual assertions.

This keeps verification orchestration in one place.

Recommended model:

- LightAPI operation result cases describe expected outputs, failure shapes, and examples.
- Workflow test steps invoke the operation.
- Workflow `assert` tasks verify actual output against expected result cases.
- Complex business checks can call the rule engine.

This allows the same endpoint description to support:

- agent skill usage
- workflow execution
- live integration testing
- failure diagnosis

## Progressive Disclosure For Agents

A LightAPI document should support progressive disclosure so an agent can load only the information needed at each stage.

Recommended levels:

- `index`: endpoint id, title, tags, visibility
- `summary`: purpose, capability group, lifecycle
- `invocation`: input schema, request mapping, authentication, examples
- `behavior`: result cases, edge cases, errors, assertions
- `full`: complete endpoint description

The portal can expose query APIs such as:

```text
lightapi.listOperations
lightapi.getOperation
lightapi.getCapabilityGroup
```

Agents should start with index or summary data, load invocation details only for selected endpoints, and load behavior details only for testing, troubleshooting, or failure repair.

## Portal Publishing Flow

Light-Portal should manage endpoint descriptions as part of API endpoint administration.

Recommended flow:

1. API owner creates or imports API metadata.
2. Portal extracts initial endpoint descriptions from OpenAPI, OpenRPC, protobuf, MCP, or raw endpoint configuration.
3. API owner enriches endpoint descriptions with examples, behavior notes, result cases, and visibility.
4. Portal stores endpoint-level LightAPI descriptions.
5. Authorized agents and workflows query descriptions by endpoint, tag, lifecycle, visibility, or capability.
6. Selected endpoints can be published into the centralized skill registry.
7. Workflow instances reference endpoint descriptions during execution and live testing.

## Live Test Use

Live tests should be workflow-driven.

LightAPI supplies:

- example input data
- expected result cases
- protocol invocation details
- error behavior

Agentic Workflow supplies:

- sequence
- fixtures
- environment selection
- endpoint invocation
- assertions
- failure routing
- task creation
- agent assignment

This avoids building a second test runner model outside the workflow engine.

## Design Rule

LightAPI describes endpoint capability. Agentic Workflow orchestrates endpoint use. Centralized Skills expose selected capabilities to agents.

Keeping these responsibilities separate lets Light-Fabric support API administration, agent skill discovery, workflow execution, and live integration testing without duplicating endpoint definitions across multiple systems.
