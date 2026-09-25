SET search_path TO workflow_ops, pg_catalog;

-- LONG source bearers belong to the Workflow operational store. A key_id of
-- 'plaintext' means the operator did not configure a keyring; encrypted rows
-- retain their key identifier so they remain readable after key rotation.
CREATE TABLE workflow_ops.workflow_long_identity_t (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    gateway_url text NOT NULL,
    provider_id text NOT NULL,
    client_id text NOT NULL
);

CREATE TABLE workflow_ops.workflow_long_credential_t (
    binding_id uuid PRIMARY KEY,
    run_id uuid NOT NULL UNIQUE,
    host_id uuid NOT NULL,
    owner_user_id uuid NOT NULL,
    issuer_client_id text NOT NULL,
    registration_key_sha256 text NOT NULL CHECK (registration_key_sha256 ~ '^[0-9a-f]{64}$'),
    subject_token_sha256 text NOT NULL CHECK (subject_token_sha256 ~ '^[0-9a-f]{64}$'),
    state text NOT NULL CHECK (state IN ('PENDING','ACTIVE','CLOSING','CLOSED','REVOKED')),
    issuer_version bigint NOT NULL CHECK (issuer_version > 0),
    key_id text NOT NULL,
    token_bytes bytea NOT NULL,
    acceptance_digest text,
    close_id uuid,
    terminal_state text CHECK (terminal_state IN ('COMPLETED','CANCELED')),
    terminal_version bigint,
    created_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_ts timestamptz NOT NULL DEFAULT CURRENT_TIMESTAMP
);

GRANT SELECT, INSERT, UPDATE ON workflow_ops.workflow_long_identity_t,
    workflow_ops.workflow_long_credential_t TO operations_workflow_runtime;

-- Preserved finite-broker records are copied from the former credential
-- database during deployment. These tables are archival after LONG cutover.
CREATE TABLE workflow_ops.workflow_broker_identity_t (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    issuer text NOT NULL,
    client_id text NOT NULL,
    token_url text NOT NULL
);
CREATE TABLE workflow_ops.workflow_broker_enrollment_t (
    enrollment_id uuid PRIMARY KEY,
    host_id uuid NOT NULL,
    user_id uuid NOT NULL,
    state_hash text NOT NULL UNIQUE,
    key_id text NOT NULL,
    ciphertext bytea NOT NULL,
    callback_uri text NOT NULL,
    scope text NOT NULL,
    binding jsonb NOT NULL,
    grant_expires_at timestamptz NOT NULL,
    expires_at timestamptz NOT NULL,
    state text NOT NULL CHECK (state IN ('PREPARING','READY','REDEEMING','COMPLETED','REAUTHORIZATION_REQUIRED')),
    owner_boot uuid,
    revocation_pending boolean NOT NULL DEFAULT false
);
CREATE TABLE workflow_ops.workflow_broker_grant_t (
    grant_id uuid PRIMARY KEY,
    host_id uuid NOT NULL,
    user_id uuid NOT NULL,
    binding jsonb NOT NULL,
    issuer_grant jsonb NOT NULL,
    generation bigint NOT NULL CHECK (generation > 0),
    expires_at timestamptz NOT NULL,
    state text NOT NULL CHECK (state IN ('ACTIVE','RENEWING','REAUTHORIZATION_REQUIRED','REVOKED')),
    key_id text NOT NULL,
    ciphertext bytea NOT NULL,
    renewal_id uuid,
    owner_boot uuid,
    renewal_deadline timestamptz,
    revocation_pending boolean NOT NULL DEFAULT false,
    CHECK ((state = 'RENEWING') = (renewal_id IS NOT NULL))
);
CREATE TABLE workflow_ops.workflow_broker_run_t (
    run_id uuid PRIMARY KEY,
    grant_id uuid NOT NULL REFERENCES workflow_ops.workflow_broker_grant_t,
    host_id uuid NOT NULL,
    user_id uuid NOT NULL,
    binding jsonb NOT NULL,
    expires_at timestamptz NOT NULL,
    active boolean NOT NULL DEFAULT true
);
CREATE TABLE workflow_ops.workflow_broker_renewal_t (
    renewal_id uuid PRIMARY KEY,
    grant_id uuid NOT NULL REFERENCES workflow_ops.workflow_broker_grant_t,
    generation bigint NOT NULL,
    started_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    finished_at timestamptz,
    result text CHECK (result IN ('ROTATED','UNCERTAIN','REAUTHORIZATION_REQUIRED','NOT_SENT'))
);
