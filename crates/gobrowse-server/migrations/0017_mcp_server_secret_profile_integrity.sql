-- 0017_mcp_server_secret_profile_integrity.sql
-- Enforce that an MCP server can reference only a secret in the same profile.
-- Legacy cross-profile links are nulled; no secret or server row is repointed or removed.

LOCK TABLE secret_references, mcp_servers IN ACCESS EXCLUSIVE MODE;

UPDATE mcp_servers AS server
SET auth_secret_reference = NULL
WHERE server.auth_secret_reference IS NOT NULL
  AND NOT EXISTS (
      SELECT 1
      FROM secret_references AS secret
      WHERE secret.id = server.auth_secret_reference
        AND secret.profile_id = server.profile_id
  );

ALTER TABLE mcp_servers
    ADD CONSTRAINT mcp_servers_auth_secret_same_profile_fk
    FOREIGN KEY (profile_id, auth_secret_reference)
    REFERENCES secret_references (profile_id, id)
    ON DELETE SET NULL (auth_secret_reference)
    NOT VALID;

ALTER TABLE mcp_servers
    VALIDATE CONSTRAINT mcp_servers_auth_secret_same_profile_fk;

ALTER TABLE mcp_servers
    DROP CONSTRAINT mcp_servers_auth_secret_reference_fkey;

UPDATE schema_metadata SET schema_version = 17, updated_at = now() WHERE singleton;
