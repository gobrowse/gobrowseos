-- 0009_webhook_delivery_fencing.sql
-- Add a per-claim lease token so late workers cannot persist stale outcomes.

ALTER TABLE webhook_deliveries ADD COLUMN IF NOT EXISTS lease_token uuid;

-- Rows already running when this migration is applied came from the pre-token
-- scheduler. Give valid leases an identity and make malformed ones retryable.
UPDATE webhook_deliveries
SET status='queued', next_attempt_at=clock_timestamp(), lease_expires_at=NULL,
    lease_token=NULL, last_error='lease reset during lease fencing migration'
WHERE status='running' AND lease_expires_at IS NULL;

UPDATE webhook_deliveries
SET lease_token=gen_random_uuid()
WHERE status='running' AND lease_token IS NULL;

-- Lease fields are present exactly while a delivery is running. Terminal and
-- queued states must not retain an identity that could be mistaken for active.
UPDATE webhook_deliveries
SET lease_token=NULL, lease_expires_at=NULL
WHERE status <> 'running' AND (lease_token IS NOT NULL OR lease_expires_at IS NOT NULL);

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.table_constraints
        WHERE constraint_name = 'webhook_deliveries_lease_check'
          AND table_name = 'webhook_deliveries'
    ) THEN
        ALTER TABLE webhook_deliveries ADD CONSTRAINT webhook_deliveries_lease_check
            CHECK ((status = 'running') =
                   (lease_token IS NOT NULL AND lease_expires_at IS NOT NULL));
    END IF;
END $$;

UPDATE schema_metadata SET schema_version = 9, updated_at = now() WHERE singleton;
