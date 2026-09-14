BEGIN;
-- Installed by workflow-store into the authoritative operational database.
-- No refresh/access tokens or issuer signing keys belong in this schema.
CREATE TABLE IF NOT EXISTS workflow_ops.workflow_action_authority_t (
 host_id uuid NOT NULL, run_id uuid NOT NULL, grant_id uuid NOT NULL, user_id uuid NOT NULL,
 grant_generation bigint NOT NULL CHECK(grant_generation>0),
 run_generation bigint NOT NULL CHECK(run_generation>0),
 budget_generation bigint NOT NULL CHECK(budget_generation>0),
 active boolean NOT NULL, deadline timestamptz NOT NULL,
 action_limit bigint NOT NULL CHECK(action_limit>=0),
 parent_action_id uuid,
 parent_run_id uuid,
 depth integer NOT NULL DEFAULT 0 CHECK(depth>=0),
 maximum_depth integer NOT NULL DEFAULT 16 CHECK(maximum_depth>=0),
 reserved bigint NOT NULL DEFAULT 0 CHECK(reserved>=0),
 used bigint NOT NULL DEFAULT 0 CHECK(used>=0),
 PRIMARY KEY(host_id,run_id), CHECK(reserved+used<=action_limit),
 CHECK((parent_action_id IS NULL)=(parent_run_id IS NULL)),
 CHECK((depth=0)=(parent_action_id IS NULL)), CHECK(depth<=maximum_depth)
);
CREATE TABLE IF NOT EXISTS workflow_ops.workflow_action_permit_t (
 host_id uuid NOT NULL, action_id uuid NOT NULL, run_id uuid NOT NULL,
 attempt_id uuid NOT NULL, binding jsonb NOT NULL CHECK(jsonb_typeof(binding)='object'),
 active boolean NOT NULL DEFAULT true,
 retry_limit bigint NOT NULL CHECK(retry_limit>0),
 authorization_count bigint NOT NULL DEFAULT 0 CHECK(authorization_count>=0),
 PRIMARY KEY(host_id,action_id), UNIQUE(host_id,run_id,attempt_id),
 FOREIGN KEY(host_id,run_id) REFERENCES workflow_ops.workflow_action_authority_t(host_id,run_id)
);
CREATE TABLE IF NOT EXISTS workflow_ops.workflow_action_dispatch_t (
 host_id uuid NOT NULL, action_id uuid NOT NULL,
 generation bigint NOT NULL CHECK(generation>0),
 decision_id uuid NOT NULL UNIQUE, owner jsonb NOT NULL, decision jsonb NOT NULL,
 state text NOT NULL CHECK(state IN ('AUTHORIZED','SEND_INTENT','NOT_INITIATED','UNCERTAIN','SUCCEEDED','FAILED')),
 authorized_at timestamptz NOT NULL, lease_deadline timestamptz NOT NULL,
 reservation_held boolean NOT NULL DEFAULT true,
 evidence_digest text,
 PRIMARY KEY(host_id,action_id,generation),
 FOREIGN KEY(host_id,action_id) REFERENCES workflow_ops.workflow_action_permit_t(host_id,action_id)
);
CREATE TABLE IF NOT EXISTS workflow_ops.workflow_action_audit_t (
 event_id uuid PRIMARY KEY, host_id uuid NOT NULL, action_id uuid NOT NULL,
 generation bigint, event_type text NOT NULL, recorded_at timestamptz NOT NULL DEFAULT clock_timestamp()
);

CREATE TABLE IF NOT EXISTS workflow_ops.workflow_gateway_owner_t (
 peer_sha256 text PRIMARY KEY, gateway_service text NOT NULL, replica uuid NOT NULL,
 boot uuid NOT NULL, fencing_generation bigint NOT NULL CHECK(fencing_generation>0),
 active boolean NOT NULL DEFAULT true, UNIQUE(gateway_service,replica)
);
CREATE TABLE IF NOT EXISTS workflow_ops.workflow_gateway_boot_t (
 peer_sha256 text NOT NULL, boot uuid NOT NULL, fencing_generation bigint NOT NULL,
 registered_at timestamptz NOT NULL DEFAULT clock_timestamp(),
 PRIMARY KEY(peer_sha256,boot)
);
ALTER TABLE workflow_ops.workflow_action_authority_t
 ADD CONSTRAINT workflow_action_authority_parent_action_fk
 FOREIGN KEY(host_id,parent_action_id)
 REFERENCES workflow_ops.workflow_action_permit_t(host_id,action_id);
ALTER TABLE workflow_ops.workflow_action_authority_t
 ADD CONSTRAINT workflow_action_authority_parent_run_fk
 FOREIGN KEY(host_id,parent_run_id)
 REFERENCES workflow_ops.workflow_action_authority_t(host_id,run_id);
COMMIT;
