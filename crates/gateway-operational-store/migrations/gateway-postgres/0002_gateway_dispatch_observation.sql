-- Additive, opt-in dispatch metadata. Install only through an approved bundle.
SET search_path TO gateway_ops, pg_catalog;
CREATE TABLE gateway_ops.gateway_dispatch_identity_t (
  deployment_config_digest VARCHAR(71) PRIMARY KEY CHECK (deployment_config_digest ~ '^sha256:[0-9a-f]{64}$'),
  process_id UUID NOT NULL,
  image_digest VARCHAR(71) NOT NULL CHECK (image_digest ~ '^sha256:[0-9a-f]{64}$'),
  binary_digest VARCHAR(71) NOT NULL CHECK (binary_digest ~ '^sha256:[0-9a-f]{64}$'),
  configuration_digest VARCHAR(71) NOT NULL CHECK (configuration_digest ~ '^sha256:[0-9a-f]{64}$'),
  observer_contract_version INTEGER NOT NULL CHECK (observer_contract_version=1),
  created_ts TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);
ALTER TABLE gateway_ops.gateway_evidence_spool_t ADD COLUMN dispatch_observation JSONB;
ALTER TABLE gateway_ops.gateway_evidence_spool_t ADD CONSTRAINT gateway_dispatch_metadata_check CHECK (
  dispatch_observation IS NULL OR ((
    event_class='REQUIRED_AUDIT' AND method='GET' AND endpoint='/github/repos/*@get'
    AND correlation_digest IS NOT NULL
    AND jsonb_typeof(dispatch_observation)='object'
    AND dispatch_observation ?& ARRAY['requestAuditId','dispatchPhase','dispatchSequence','upstreamAttemptCount','upstreamHandoffCount','observationComplete','deploymentConfigDigest','observerContractVersion','completionPhase']
    AND dispatch_observation - ARRAY['requestAuditId','dispatchPhase','dispatchSequence','upstreamAttemptCount','upstreamHandoffCount','observationComplete','deploymentConfigDigest','observerContractVersion','completionPhase'] = '{}'::jsonb
    AND (dispatch_observation->>'requestAuditId')::uuid <> '00000000-0000-0000-0000-000000000000'::uuid
    AND dispatch_observation->>'deploymentConfigDigest' ~ '^sha256:[0-9a-f]{64}$'
    AND (dispatch_observation->>'observerContractVersion')::integer=1
    AND (dispatch_observation->>'dispatchSequence')::bigint BETWEEN 0 AND 4294967295
    AND (dispatch_observation->>'upstreamAttemptCount')::bigint BETWEEN 0 AND 4294967295
    AND (dispatch_observation->>'upstreamHandoffCount')::bigint BETWEEN 0 AND (dispatch_observation->>'upstreamAttemptCount')::bigint
    AND jsonb_typeof(dispatch_observation->'observationComplete')='boolean'
    AND dispatch_observation->>'dispatchPhase' IN ('started','attempt','handoff','terminal')
    AND (dispatch_observation->>'dispatchSequence')::bigint = (dispatch_observation->>'upstreamAttemptCount')::bigint + (dispatch_observation->>'upstreamHandoffCount')::bigint + CASE WHEN dispatch_observation->>'dispatchPhase'='terminal' THEN 1 ELSE 0 END
    AND (dispatch_observation->>'dispatchPhase'<>'started' OR (dispatch_observation->>'dispatchSequence')::bigint=0)
    AND (dispatch_observation->>'dispatchPhase'='terminal' OR dispatch_observation->>'observationComplete'='false')
    AND event_type=CASE dispatch_observation->>'dispatchPhase' WHEN 'started' THEN 'gateway.dispatch.observation.started' WHEN 'attempt' THEN 'gateway.upstream.attempt' WHEN 'handoff' THEN 'gateway.upstream.handoff' WHEN 'terminal' THEN 'gateway.dispatch.observation.terminal' END
    AND dispatch_observation->>'completionPhase' IN ('in_progress','response','error')
  ) IS TRUE)
);
CREATE UNIQUE INDEX gateway_dispatch_sequence_idx ON gateway_ops.gateway_evidence_spool_t
  (host_id,(dispatch_observation->>'requestAuditId'),((dispatch_observation->>'dispatchSequence')::bigint))
  WHERE dispatch_observation IS NOT NULL;
REVOKE ALL ON gateway_ops.gateway_dispatch_identity_t FROM operations_gateway_runtime;
GRANT SELECT, INSERT ON gateway_ops.gateway_dispatch_identity_t TO operations_gateway_runtime;
