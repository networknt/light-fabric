-- Preserve the selected published Skill across detached status and cancel
-- operations. The backend operation identity alone is insufficient because
-- every private callback is bound to the original Skill in its signed context.

SET search_path TO a2a_ops, pg_catalog;

ALTER TABLE a2a_ops.a2a_backend_correlation_t
  ADD COLUMN selected_skill_id VARCHAR(256);
