-- Apply with the dedicated credential-store owner, not workflow_ops credentials.
BEGIN;
CREATE SCHEMA IF NOT EXISTS workflow_secret;
REVOKE ALL ON SCHEMA workflow_secret FROM PUBLIC;
-- One issuer client per credential store. A configuration edit cannot silently
-- point recovery at another client's grants or revoke them under a wrong peer.
CREATE TABLE IF NOT EXISTS workflow_secret.identity_t (
    singleton boolean PRIMARY KEY DEFAULT true CHECK(singleton),
    issuer text NOT NULL,
    client_id text NOT NULL,
    token_url text NOT NULL
);
CREATE TABLE IF NOT EXISTS workflow_secret.enrollment_t (
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
CREATE TABLE IF NOT EXISTS workflow_secret.grant_t (
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
CREATE TABLE IF NOT EXISTS workflow_secret.run_t (
    run_id uuid PRIMARY KEY,
    grant_id uuid NOT NULL REFERENCES workflow_secret.grant_t,
    host_id uuid NOT NULL,
    user_id uuid NOT NULL,
    binding jsonb NOT NULL,
    expires_at timestamptz NOT NULL,
    active boolean NOT NULL DEFAULT true
);
CREATE TABLE IF NOT EXISTS workflow_secret.renewal_t (
    renewal_id uuid PRIMARY KEY,
    grant_id uuid NOT NULL REFERENCES workflow_secret.grant_t,
    generation bigint NOT NULL,
    started_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    finished_at timestamptz,
    result text CHECK (result IN ('ROTATED','UNCERTAIN','REAUTHORIZATION_REQUIRED','NOT_SENT'))
);
-- Upgrade preserved A1 credential stores as well as fresh installations.
ALTER TABLE workflow_secret.renewal_t DROP CONSTRAINT IF EXISTS renewal_t_result_check;
ALTER TABLE workflow_secret.renewal_t ADD CONSTRAINT renewal_t_result_check
    CHECK (result IN ('ROTATED','UNCERTAIN','REAUTHORIZATION_REQUIRED','NOT_SENT'));
REVOKE ALL ON ALL TABLES IN SCHEMA workflow_secret FROM PUBLIC;
COMMIT;
