# Mortgage Approval Workflow Test Plan

This document provides repeatable test cases and the workflow definition for the `mortgage-approval-process` (Serverless Workflow v1.0.3).

---

## 1. Workflow Definition (YAML)
Copy this content into the "Definition" field in the UI when creating a new workflow.

```yaml
document:
  dsl: '1.0.3'
  namespace: financial.services
  name: mortgage-approval-process
  version: '1.0.0'
input:
  schema:
    type: object
    properties:
      applicantId: { type: string }
      loanAmount: { type: number }
      creditScore: { type: number }
do:
  - evaluateRisk:
      call: http
      with:
        method: post
        endpoint: 
          uri: "http://risk-engine.internal/evaluate"
        body: 
          applicantId: "${{ .applicantId }}"
          score: "${{ .creditScore }}"
          amount: "${{ .loanAmount }}"
      export:
        evaluation: ".output"
  - riskBranching:
      switch:
        - if: "${{ .evaluation.riskScore < 30 }}"
          then: autoApprove
        - if: "${{ .evaluation.riskScore > 80 }}"
          then: autoReject
      default: manualApproval
  - autoApprove:
      set:
        status: "APPROVED"
        decisionSource: "Automatic"
        message: "Loan approved automatically based on low risk score."
      end: true
  - autoReject:
      set:
        status: "REJECTED"
        decisionSource: "Automatic"
        message: "Loan rejected automatically due to high risk score."
      end: true
  - manualApproval:
      listen:
        to:
          one:
            with:
              type: "mortgage.manual.decision"
        timeout:
          after: "PT24H"
      export:
        manualStatus: ".output.status"
        comment: ".output.comment"
      then: finalizeDecision
  - finalizeDecision:
      set:
        status: "${{ .manualStatus }}"
        decisionSource: "Human"
        message: "${{ .comment }}"
      end: true
```

---

## 2. Test Scenarios

### Scenario A: Automatic Approval
- **Goal**: Verify low-risk applications are approved instantly.
- **Workflow Input**:
  ```json
  {
    "applicantId": "APP-LOW-RISK",
    "loanAmount": 100000,
    "creditScore": 820
  }
  ```
- **Simulated Mock Response** (from Risk API):
  ```json
  { "riskScore": 15 }
  ```
- **Expected Final State**: `status: APPROVED`, `decisionSource: Automatic`.

### Scenario B: Automatic Rejection
- **Goal**: Verify high-risk applications are rejected instantly.
- **Workflow Input**:
  ```json
  {
    "applicantId": "APP-HIGH-RISK",
    "loanAmount": 950000,
    "creditScore": 450
  }
  ```
- **Simulated Mock Response**:
  ```json
  { "riskScore": 92 }
  ```
- **Expected Final State**: `status: REJECTED`, `decisionSource: Automatic`.

### Scenario C: Manual Review (Human-in-the-loop)
- **Goal**: Verify moderate-risk applications wait for an external decision event.
- **Workflow Input**:
  ```json
  {
    "applicantId": "APP-MOD-RISK",
    "loanAmount": 300000,
    "creditScore": 710
  }
  ```
- **Simulated Mock Response**:
  ```json
  { "riskScore": 45 }
  ```
- **Action**: Post a `mortgage.manual.decision` event to the system.
  ```json
  {
    "status": "APPROVED",
    "comment": "Good income-to-debt ratio."
  }
  ```
- **Expected Final State**: `status: APPROVED`, `decisionSource: Human`.

---

## 3. Database Verification

To verify that events are being persisted and processed correctly by the engine, run the following SQL queries:

### View Latest Workflow Events
```sql
SELECT aggregate_id, event_type, aggregate_version, payload 
FROM event_store_t 
WHERE aggregate_type = 'WorkflowInstance' 
ORDER BY event_ts DESC;
```

### Check Outbox for Engine Consumption
```sql
SELECT event_type, aggregate_id, payload 
FROM outbox_message_t 
ORDER BY event_ts DESC;
```
