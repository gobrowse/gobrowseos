-- 0018_mcp_auth_states_vault_pkce.sql
-- Migrate PKCE verifier from inline encrypted column to vault-backed
-- same-profile secret_reference. The table is dead schema (zero Rust/SQL
-- writers), so a zero-row guard aborts if any legacy data exists.

LOCK TABLE mcp_auth_states, secret_references IN ACCESS EXCLUSIVE MODE;

DO $$
DECLARE
    row_count bigint;
BEGIN
    SELECT count(*) INTO row_count FROM mcp_auth_states;
    IF row_count > 0 THEN
        RAISE EXCEPTION 'mcp_auth_states must be empty for schema-18 migration, found % rows', row_count;
    END IF;
END $$;

ALTER TABLE mcp_auth_states
    ADD COLUMN profile_id uuid;

UPDATE mcp_auth_states AS a
SET profile_id = s.profile_id
FROM mcp_servers AS s
WHERE a.mcp_server_id = s.id;

ALTER TABLE mcp_auth_states
    ALTER COLUMN profile_id SET NOT NULL;

ALTER TABLE mcp_auth_states
    ADD COLUMN pkce_verifier_secret_ref text;

ALTER TABLE mcp_auth_states
    ADD CONSTRAINT mcp_auth_states_pkce_verifier_same_profile_fk
    FOREIGN KEY (profile_id, pkce_verifier_secret_ref)
    REFERENCES secret_references (profile_id, id)
    ON DELETE SET NULL (pkce_verifier_secret_ref)
    NOT VALID;

ALTER TABLE mcp_auth_states
    ADD CONSTRAINT mcp_auth_states_profile_id_fkey
    FOREIGN KEY (profile_id)
    REFERENCES profiles (id)
    ON DELETE CASCADE;

ALTER TABLE mcp_auth_states
    VALIDATE CONSTRAINT mcp_auth_states_pkce_verifier_same_profile_fk;

ALTER TABLE mcp_auth_states
    DROP COLUMN pkce_verifier_encrypted;

UPDATE schema_metadata SET schema_version = 18, updated_at = now() WHERE singleton;