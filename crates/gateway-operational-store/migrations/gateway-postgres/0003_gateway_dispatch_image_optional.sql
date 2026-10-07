-- Preserve historical image values and compatibility with previous binaries.
-- New observers omit image_digest; release image provenance belongs to the
-- generic product-version release pipeline, not Gateway configuration.
SET search_path TO gateway_ops, pg_catalog;
ALTER TABLE gateway_ops.gateway_dispatch_identity_t
  ALTER COLUMN image_digest DROP NOT NULL;
ALTER TABLE gateway_ops.gateway_dispatch_identity_t
  DROP CONSTRAINT gateway_dispatch_identity_t_image_digest_check;
