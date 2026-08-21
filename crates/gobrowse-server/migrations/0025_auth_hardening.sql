-- 0025_auth_hardening.sql
-- Schema 24 -> 25. M25a auth hardening: auth methods, session events,
-- account lockout, session revocation, step-up tracking.

-- 1. Auth methods (WebAuthn, OIDC, password tracking)
CREATE TABLE auth_methods (
    id uuid PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    method_type text NOT NULL CHECK (method_type IN ('password', 'webauthn', 'oidc')),
    method_data jsonb NOT NULL,
    label text,
    is_primary boolean NOT NULL DEFAULT false,
    last_used_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (user_id, method_type, label)
);

-- Backfill: existing password users get a password auth_method
INSERT INTO auth_methods (id, user_id, method_type, method_data, label, is_primary, created_at)
SELECT
    gen_random_uuid(), id, 'password',
    jsonb_build_object('hash', password_hash),
    'Password', true, created_at
FROM users
ON CONFLICT DO NOTHING;

-- 2. Session events (append-only session lifecycle audit)
CREATE TABLE session_events (
    id bigserial PRIMARY KEY,
    session_hash bytea NOT NULL,
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    event_type text NOT NULL CHECK (event_type IN ('created','rotated','expired','revoked','step_up','login','logout')),
    ip_hash bytea,
    user_agent_hash bytea,
    metadata jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX session_events_user_idx ON session_events (user_id, created_at DESC);
CREATE INDEX session_events_session_idx ON session_events (session_hash, created_at);

-- 3. User account lockout tracking
ALTER TABLE users ADD COLUMN consecutive_failures integer NOT NULL DEFAULT 0;
ALTER TABLE users ADD COLUMN locked_until timestamptz;

-- 4. Session revocation list (for forced logout)
CREATE TABLE session_revocations (
    token_hash bytea PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    revoked_by uuid REFERENCES users(id) ON DELETE SET NULL,
    reason text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX session_revocations_user_idx ON session_revocations (user_id, created_at DESC);

-- 5. Step-up auth tracking
ALTER TABLE sessions ADD COLUMN last_step_up_at timestamptz;

UPDATE schema_metadata SET schema_version = 25, updated_at = now() WHERE singleton;