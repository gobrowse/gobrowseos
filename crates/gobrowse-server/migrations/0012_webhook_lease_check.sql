-- 0012_webhook_lease_check.sql
-- 0009's boolean equivalence accepted a half-filled lease on non-running rows:
-- false = false.  Require both fields together for running rows and neither
-- field for every other status.

-- Normalize malformed legacy rows before replacing the constraint.  A running
-- row without a complete lease cannot be safely resumed by a worker.
ALTER TABLE webhook_deliveries DROP CONSTRAINT IF EXISTS webhook_deliveries_lease_check;

UPDATE webhook_deliveries
SET status = 'queued',
    lease_token = NULL,
    lease_expires_at = NULL,
    next_attempt_at = clock_timestamp(),
    last_error = 'incomplete lease reset during lease constraint migration'
WHERE status = 'running'
  AND (lease_token IS NULL OR lease_expires_at IS NULL);

UPDATE webhook_deliveries
SET lease_token = NULL,
    lease_expires_at = NULL
WHERE status <> 'running'
  AND (lease_token IS NOT NULL OR lease_expires_at IS NOT NULL);

ALTER TABLE webhook_deliveries ADD CONSTRAINT webhook_deliveries_lease_check
    CHECK (
        (status = 'running'
            AND lease_token IS NOT NULL
            AND lease_expires_at IS NOT NULL)
        OR
        (status <> 'running'
            AND lease_token IS NULL
            AND lease_expires_at IS NULL)
    );

UPDATE schema_metadata SET schema_version = 12, updated_at = now() WHERE singleton;
