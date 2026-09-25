-- Owner source material is encrypted under the Agent app keyring and kept out
-- of agent_job_t, turn input, reports and ordinary projections.
CREATE TABLE agent_ops.agent_long_binding_t (
    host_id uuid NOT NULL,
    job_id uuid NOT NULL,
    owner_user_id uuid NOT NULL,
    binding_id uuid NOT NULL UNIQUE,
    issuer_client_id text NOT NULL,
    key_id text NOT NULL,
    ciphertext bytea NOT NULL,
    acceptance_digest text NOT NULL CHECK (acceptance_digest ~ '^[0-9a-f]{64}$'),
    registration_version bigint NOT NULL CHECK (registration_version > 0),
    state text NOT NULL CHECK (state IN ('ACTIVATION_PENDING','ACTIVE','CLOSE_PENDING','CLOSED')),
    close_id uuid,
    close_reason text CHECK (close_reason IN ('COMPLETED','CANCELED')),
    created_ts timestamptz NOT NULL DEFAULT now(),
    closed_ts timestamptz,
    PRIMARY KEY (host_id, job_id),
    FOREIGN KEY (host_id, job_id) REFERENCES agent_ops.agent_job_t(host_id, job_id)
);
