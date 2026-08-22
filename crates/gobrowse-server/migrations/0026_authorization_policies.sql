-- Migration 0026: Authorization policies table and audit event extensions.
-- Centralized ALLOW/ASK/DENY authorization engine (M25b).

-- Authorization policies: per-profile overrides for the default matrix.
-- Single-user installs have NO policies (the short-circuit covers them).
CREATE TABLE authorization_policies (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    profile_id uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    actor text NOT NULL,
    action text NOT NULL,
    resource text NOT NULL,
    decision text NOT NULL CHECK (decision IN ('ALLOW', 'ASK', 'DENY')),
    priority integer NOT NULL DEFAULT 100,
    enabled boolean NOT NULL DEFAULT true,
    created_by uuid REFERENCES users(id) ON DELETE SET NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (profile_id, actor, action, resource, priority)
);
CREATE INDEX authorization_policies_lookup_idx
    ON authorization_policies (profile_id, actor, action, resource)
    WHERE enabled = true;

-- Extend audit_events with authorization decision fields.
ALTER TABLE audit_events ADD COLUMN IF NOT EXISTS authorization_decision text;
ALTER TABLE audit_events ADD COLUMN IF NOT EXISTS authorization_reason text;
ALTER TABLE audit_events ADD COLUMN IF NOT EXISTS policy_id uuid REFERENCES authorization_policies(id) ON DELETE SET NULL;

-- Bump schema version.
UPDATE schema_metadata SET schema_version = 26, updated_at = now() WHERE singleton;
