-- Phase 5 extends the immutable governed outbound binding with the policy
-- dimensions required by Workflow admission. Migration 0002 is already
-- deployed and remains byte-for-byte immutable.

ALTER TABLE workflow_ops.workflow_a2a_binding_t
  ADD COLUMN IF NOT EXISTS environment VARCHAR(64),
  ADD COLUMN IF NOT EXISTS data_boundary_digest VARCHAR(80),
  ADD COLUMN IF NOT EXISTS maximum_delegation_depth INTEGER,
  ADD COLUMN IF NOT EXISTS maximum_budget_units BIGINT;

DO $migration$
BEGIN
  IF EXISTS (
    SELECT 1
      FROM workflow_ops.workflow_a2a_binding_t
     WHERE environment IS NULL
        OR data_boundary_digest IS NULL
        OR maximum_delegation_depth IS NULL
        OR maximum_budget_units IS NULL
  ) THEN
    RAISE EXCEPTION
      'existing Workflow A2A bindings must be republished before applying 0003_governed_a2a_outbound_policy';
  END IF;
END
$migration$;

ALTER TABLE workflow_ops.workflow_a2a_binding_t
  ALTER COLUMN environment SET NOT NULL,
  ALTER COLUMN data_boundary_digest SET NOT NULL,
  ALTER COLUMN maximum_delegation_depth SET NOT NULL,
  ALTER COLUMN maximum_budget_units SET NOT NULL,
  ADD CONSTRAINT workflow_a2a_binding_environment_ck
    CHECK (environment ~ '^[a-z][a-z0-9_-]{0,63}$'),
  ADD CONSTRAINT workflow_a2a_binding_data_boundary_ck
    CHECK (data_boundary_digest ~ '^sha256:[0-9a-f]{64}$'),
  ADD CONSTRAINT workflow_a2a_binding_delegation_depth_ck
    CHECK (maximum_delegation_depth BETWEEN 1 AND 65535),
  ADD CONSTRAINT workflow_a2a_binding_budget_ck
    CHECK (maximum_budget_units > 0);
