SET search_path TO workflow_ops, pg_catalog;

-- This is the operational half of an issuer-owned LONG binding. The row is
-- written in the same transaction as accepted invocation/action authority.
-- Only the reconciler may advance ACTIVATE_PENDING after issuer acknowledgement.
CREATE TABLE workflow_ops.workflow_long_owner_binding_t (
    host_id uuid NOT NULL,
    workflow_instance_id uuid NOT NULL,
    binding_id uuid NOT NULL,
    owner_user_id uuid NOT NULL,
    acceptance_digest text NOT NULL CHECK (acceptance_digest ~ '^[0-9a-f]{64}$'),
    state text NOT NULL CHECK (state IN ('ACTIVATE_PENDING','ACTIVE','CLOSE_PENDING','CLOSED','REVOKED')),
    close_id uuid,
    terminal_state text CHECK (terminal_state IN ('COMPLETED','CANCELED')),
    terminal_version bigint CHECK (terminal_version > 0),
    issuer_version bigint CHECK (issuer_version > 0),
    attempts integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    next_attempt_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_error_code text,
    created_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (host_id, workflow_instance_id),
    UNIQUE (binding_id),
    FOREIGN KEY (host_id, workflow_instance_id)
      REFERENCES workflow_ops.workflow_invocation_t(host_id, workflow_instance_id)
      ON DELETE RESTRICT,
    CHECK ((state IN ('CLOSE_PENDING','CLOSED','REVOKED')) = (close_id IS NOT NULL)),
    CHECK ((close_id IS NULL AND terminal_state IS NULL AND terminal_version IS NULL)
       OR (close_id IS NOT NULL AND terminal_state IS NOT NULL AND terminal_version IS NOT NULL))
);
CREATE INDEX workflow_long_owner_binding_delivery_idx
    ON workflow_ops.workflow_long_owner_binding_t(next_attempt_ts, host_id, workflow_instance_id)
    WHERE state IN ('ACTIVATE_PENDING','CLOSE_PENDING');
GRANT SELECT, INSERT, UPDATE ON workflow_ops.workflow_long_owner_binding_t
    TO operations_workflow_runtime;

-- Every terminal transition fences local dispatch and records close delivery
-- in the same operational commit, even when the caller is cancellation or a
-- runner reconciler rather than the normal host-task executor.
CREATE FUNCTION workflow_ops.workflow_long_owner_terminal_v1()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE workflow_ops.workflow_long_owner_binding_t b SET
        state='CLOSE_PENDING',
        close_id=COALESCE(b.close_id, gen_random_uuid()),
        terminal_state=CASE NEW.state WHEN 'COMPLETED' THEN 'COMPLETED' ELSE 'CANCELED' END,
        terminal_version=NEW.state_version,
        next_attempt_ts=CURRENT_TIMESTAMP,
        updated_ts=CURRENT_TIMESTAMP
      WHERE b.host_id=NEW.host_id AND b.workflow_instance_id=NEW.workflow_instance_id
        AND b.state IN ('ACTIVATE_PENDING','ACTIVE');
    IF FOUND THEN
        UPDATE workflow_ops.workflow_action_authority_t a SET active=false
          FROM workflow_ops.workflow_long_owner_binding_t b
         WHERE a.host_id=NEW.host_id AND a.run_id=NEW.workflow_instance_id
           AND b.host_id=a.host_id AND b.workflow_instance_id=a.run_id
           AND a.grant_id=b.binding_id;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER workflow_long_owner_terminal_v1
AFTER UPDATE OF state ON workflow_ops.workflow_invocation_t
FOR EACH ROW WHEN (OLD.state NOT IN ('COMPLETED','FAILED','CANCELLED')
                   AND NEW.state IN ('COMPLETED','FAILED','CANCELLED'))
EXECUTE FUNCTION workflow_ops.workflow_long_owner_terminal_v1();
