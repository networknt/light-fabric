-- Owner arrives only through the authenticated Workflow transport. Legacy rows
-- remain NULL and cannot authorize personal-workspace dispatch.
ALTER TABLE agent_ops.agent_job_t ADD COLUMN end_user_subject text
    CHECK (end_user_subject IS NULL OR
        (length(end_user_subject) BETWEEN 1 AND 255 AND
         end_user_subject = btrim(end_user_subject)));
