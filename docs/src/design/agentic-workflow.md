# Agentic Workflow Design

The **Agentic Workflow** in Light-Fabric implements a **Hybrid Orchestration Model** specifically designed for enterprise business processes. 

## The Enterprise Challenge: Autonomy vs. Compliance

In high-stakes environments (e.g., banking, insurance, healthcare), a purely autonomous AI agent is often unsuitable for managing entire end-to-end business processes. 
- **Compliance**: Regulated industries require deterministic paths for audits and legal accountability.
- **Reliability**: A long-running process (days or weeks) should not rely on a probabilistic "think/act" loop that might wander off track.
- **Safety**: Critical decisions (e.g., approving a million-dollar loan) require a fixed governance structure and often human-in-the-loop (HITL) approval.

## The Solution: The Hybrid Workflow Model

Light-Fabric solves this by separating **Orchestration** from **Execution**:

1.  **Deterministic Orchestration (The Skeleton)**: The overall business process is a fixed, stateful workflow (YAML/JSON). It defines the sequence, business rules, and error handling.
2.  **Autonomous Execution (The Muscle)**: Individual tasks within that skeleton are delegated to AI Agents. These agents have the flexibility to use tools, search memory, and reason through the specific task in a non-deterministic way.

### Comparison: Traditional vs. Agentic vs. Hybrid

| Feature | Traditional Workflow | Pure Agentic Loop | **Hybrid (Light-Fabric)** |
| :--- | :--- | :--- | :--- |
| **Path** | Fixed / Deterministic | Probabilistic / Dynamic | **Fixed Path, Flexible Steps** |
| **Control** | Centralized | Fully Autonomous | **Managed Autonomy** |
| **Audibility** | High | Low (Black Box) | **High (Step-level Audit)** |
| **Best For** | Routine, rigid tasks | Research, brainstorming | **Complex Enterprise Processes** |

---

## Core Components of Hybrid Workflows

### 1. The Fixed Skeleton (The "Manager")
Defined using the [Hybrid Agentic Workflow Specification](https://github.com/agentic-workflow/workflow-specification). It acts as the "Manager" that ensures the process stays within enterprise guardrails.
- **States**: Manages status (Active, Completed, Failed).
- **Gatekeeping**: Ensures human approval is obtained before moving to the next critical stage.
- **Data Integrity**: Maintains the "Source of Truth" for the business process state in `process_info_t`.

### 2. The Agent Task (The "Worker")
A new task type (`agent`) that delegates a specific goal to an LLM loop.
- **Boundaries**: The agent is given a specific **Goal** and **Constraints** (time, tools, budget).
- **Intelligence**: The agent uses the **Hindsight Memory** and **Centralized Skills** provided by the Fabric.
- **Output**: Once the agent completes its task, it returns a structured result to the Orchestrator.

---

## Enterprise Use Case Examples

### Example 1: Insurance Claim Processing
1.  **Step 1 (Deterministic)**: Intake form received via API.
2.  **Step 2 (Agentic)**: **Damage Assessment Agent** reviews uploaded photos and estimate. It uses tools to check market prices and past claims (Hindsight).
3.  **Step 3 (Deterministic)**: Rule engine checks if the estimate is > $5,000.
4.  **Step 4 (Human)**: If > $5,000, trigger a **Human Approval** task.
5.  **Step 5 (Agentic)**: **Communication Agent** drafts a personalized explanation for the customer.

### Example 2: Commercial Bank Account Opening
1.  **Step 1 (Deterministic)**: KYC (Know Your Customer) data collection.
2.  **Step 2 (Agentic)**: **Risk Analysis Agent** scans adverse media, corporate registers, and sanction lists. It synthesizes a "Risk Profile."
3.  **Step 3 (Deterministic)**: If "High Risk," the workflow routes to the Compliance Department.
4.  **Step 4 (Human)**: Compliance officer reviews the agent's synthesized report.

---

## Technical Implementation in Light-Fabric

The **`light-workflow`** application implements this design:

- **State Persistence**: Every state transition is recorded in `process_info_t` and `task_info_t`.
- **Agent Loops**: When an `agent` task is encountered, the workflow engine initializes a `light-agent` session with the specific skills and tools required for that task.
- **Observability**: Developers can see the "Fixed Path" in the UI, and "drill down" into the "Probabilistic Loop" of the agent for that specific step.

By adopting this **Hybrid** approach, Light-Fabric provides the predictability required for enterprise operations while unleashing the creative problem-solving power of AI agents.
