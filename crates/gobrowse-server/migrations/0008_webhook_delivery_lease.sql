-- 0008_webhook_delivery_lease.sql
-- Add lease expiry column and index for stuck-delivery reaping.
-- Idempotent: ADD COLUMN IF NOT EXISTS + IF NOT EXISTS index.

ALTER TABLE webhook_deliveries ADD COLUMN IF NOT EXISTS lease_expires_at timestamptz;

CREATE INDEX IF NOT EXISTS webhook_deliveries_lease_idx
    ON webhook_deliveries (lease_expires_at) WHERE status = 'running';

UPDATE schema_metadata SET schema_version = 8, updated_at = now() WHERE singleton;
