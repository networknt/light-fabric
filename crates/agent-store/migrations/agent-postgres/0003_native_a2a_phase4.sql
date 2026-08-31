-- Phase 4 pins client message idempotency and the complete public-skill map to
-- every native task. Artifact lifecycle remains independent from turn history.

ALTER TABLE agent_ops.agent_a2a_task_alias_t
  ADD COLUMN IF NOT EXISTS message_id VARCHAR(256) NOT NULL,
  ADD COLUMN IF NOT EXISTS skill_mapping JSONB NOT NULL,
  ADD COLUMN IF NOT EXISTS skill_mapping_digest VARCHAR(71) NOT NULL;

ALTER TABLE agent_ops.agent_a2a_artifact_t
  ADD COLUMN IF NOT EXISTS provenance_digest VARCHAR(71) NOT NULL;

DO $constraints$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname='agent_a2a_task_message_uk') THEN
    ALTER TABLE agent_ops.agent_a2a_task_alias_t
      ADD CONSTRAINT agent_a2a_task_message_uk UNIQUE
      (host_id,principal_subject,agent_def_id,publication_id,message_id);
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname='agent_a2a_task_skill_mapping_digest_ck') THEN
    ALTER TABLE agent_ops.agent_a2a_task_alias_t
      ADD CONSTRAINT agent_a2a_task_skill_mapping_digest_ck
      CHECK (skill_mapping_digest ~ '^sha256:[0-9a-f]{64}$');
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname='agent_a2a_artifact_provenance_digest_ck') THEN
    ALTER TABLE agent_ops.agent_a2a_artifact_t
      ADD CONSTRAINT agent_a2a_artifact_provenance_digest_ck
      CHECK (provenance_digest ~ '^sha256:[0-9a-f]{64}$');
  END IF;
  IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname='agent_a2a_artifact_deletion_state_ck') THEN
    ALTER TABLE agent_ops.agent_a2a_artifact_t
      ADD CONSTRAINT agent_a2a_artifact_deletion_state_ck
      CHECK (deletion_state IN ('RETAINED','TOMBSTONED'));
  END IF;
END
$constraints$;

CREATE INDEX IF NOT EXISTS agent_a2a_task_owner_list_idx
  ON agent_ops.agent_a2a_task_alias_t
  (host_id,principal_subject,agent_def_id,publication_id,created_ts DESC,public_task_id);

