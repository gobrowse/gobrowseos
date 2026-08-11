CREATE TABLE IF NOT EXISTS login_attempts (
    email text NOT NULL,
    ip_hash bytea,
    occurred_at timestamptz NOT NULL DEFAULT now(),
    outcome text NOT NULL
);
CREATE INDEX IF NOT EXISTS login_attempts_email_occurred_idx
    ON login_attempts (email, occurred_at);
CREATE INDEX IF NOT EXISTS login_attempts_ip_occurred_idx
    ON login_attempts (ip_hash, occurred_at);

UPDATE schema_metadata SET schema_version = 4, updated_at = now() WHERE singleton;
