-- RETIRED records logical retirement while legacy shared bytes remain present.
-- DELETED continues to mean provider deletion followed by verified absence.
ALTER TABLE workflow_ops.workflow_artifact_t
  DROP CONSTRAINT workflow_artifact_t_deletion_state_check;
ALTER TABLE workflow_ops.workflow_artifact_t
  ADD CONSTRAINT workflow_artifact_t_deletion_state_check
  CHECK (deletion_state IN ('RETAINED','DELETE_PENDING','DELETING','DELETED','DELETE_FAILED','RETIRED'));
