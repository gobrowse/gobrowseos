-- 0007_webhook_scheduler.sql
-- Add outbound webhook delivery scheduler columns, constraint, and index.
-- Idempotent: ADD COLUMN IF NOT EXISTS + guarded CHECK constraint + IF NOT EXISTS index.

ALTER TABLE webhook_deliveries ADD COLUMN IF NOT EXISTS status text NOT NULL DEFAULT 'accepted';
ALTER TABLE webhook_deliveries ADD COLUMN IF NOT EXISTS attempts integer NOT NULL DEFAULT 0;
ALTER TABLE webhook_deliveries ADD COLUMN IF NOT EXISTS next_attempt_at timestamptz DEFAULT now();

-- Ensure next_attempt_at is nullable (the scheduler sets it to NULL for
-- in-flight deliveries). This is a no-op if the column was already added
-- as nullable, and fixes databases where a previous migration version
-- added it with NOT NULL.
ALTER TABLE webhook_deliveries ALTER COLUMN next_attempt_at DROP NOT NULL;
ALTER TABLE webhook_deliveries ADD COLUMN IF NOT EXISTS last_response_code integer;
ALTER TABLE webhook_deliveries ADD COLUMN IF NOT EXISTS last_error text;
ALTER TABLE webhook_deliveries ADD COLUMN IF NOT EXISTS target_url text;
ALTER TABLE webhook_deliveries ADD COLUMN IF NOT EXISTS secret_key bytea;

-- Guard the CHECK constraint behind an information_schema check because
-- ADD CONSTRAINT IF NOT EXISTS does not exist in PostgreSQL.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.table_constraints
        WHERE constraint_name = 'webhook_deliveries_status_check'
          AND table_name = 'webhook_deliveries'
    ) THEN
        ALTER TABLE webhook_deliveries ADD CONSTRAINT webhook_deliveries_status_check
            CHECK (status IN ('accepted','queued','running','succeeded','failed','dead'));
    END IF;
END $$;

-- Index for efficient claim queries (only rows due for delivery).
CREATE INDEX IF NOT EXISTS webhook_deliveries_claim_idx
    ON webhook_deliveries (next_attempt_at) WHERE status IN ('queued','accepted');

UPDATE schema_metadata SET schema_version = 7, updated_at = now() WHERE singleton;
