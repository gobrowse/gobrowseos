-- 0020_unified_library.sql
-- Unified library: books.kind registry role + normalized plugin tables (schema 19 -> 20).

-- 1. Book kind column (nullable; NULL = SOURCE for backward compatibility)
ALTER TABLE books ADD COLUMN kind text;
ALTER TABLE books ADD CONSTRAINT books_kind_check
    CHECK (kind IS NULL OR kind IN ('SOURCE','SKILL','MCP','PLUGIN','AUTOBIOGRAPHY'));

-- Backfill: existing Autobiography books get kind = 'AUTOBIOGRAPHY'
UPDATE books SET kind = 'AUTOBIOGRAPHY' WHERE book_type = 'AUTOBIOGRAPHY' AND kind IS NULL;

-- 2. Component Book proxy for each existing Skill
INSERT INTO books (id, profile_id, title, body, book_type, scope, tags, provenance, trust,
    source, author, workspace_id, security_classification, kind, metadata)
SELECT
    gen_random_uuid(), s.profile_id, s.name, s.description,
    'INSTRUCTION', COALESCE(
        CASE WHEN s.workspace_id IS NOT NULL THEN 'WORKSPACE' ELSE 'PROFILE' END,
        'PROFILE'),
    '{}', 'SKILL', 'USER_PROVIDED',
    '{}', 'system', s.workspace_id, 'INTERNAL',
    'SKILL', jsonb_build_object('skill_id', s.id)
FROM skills s
WHERE NOT EXISTS (
    SELECT 1 FROM books b
    WHERE b.metadata->>'skill_id' = s.id::text
      AND b.kind = 'SKILL'
)
ON CONFLICT DO NOTHING;

-- 3. Component Book proxy for each existing MCP server
INSERT INTO books (id, profile_id, title, body, book_type, scope, tags, provenance, trust,
    source, author, workspace_id, security_classification, kind, metadata)
SELECT
    gen_random_uuid(), ms.profile_id, ms.name,
    COALESCE(ms.configuration->>'description', ms.transport || ' MCP server'),
    'INSTRUCTION', 'PROFILE',
    '{}', 'MCP', 'USER_PROVIDED',
    '{}', 'system', NULL, 'INTERNAL',
    'MCP', jsonb_build_object('mcp_server_id', ms.id)
FROM mcp_servers ms
WHERE NOT EXISTS (
    SELECT 1 FROM books b
    WHERE b.metadata->>'mcp_server_id' = ms.id::text
      AND b.kind = 'MCP'
)
ON CONFLICT DO NOTHING;

-- 4. Plugins table (normalized identity/origin/trust/state)
CREATE TABLE plugins (
    id uuid PRIMARY KEY,
    profile_id uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    workspace_id uuid REFERENCES workspaces(id) ON DELETE CASCADE,
    name text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 200),
    description text NOT NULL,
    version text NOT NULL CHECK (char_length(version) BETWEEN 1 AND 64),
    source_type text NOT NULL CHECK (source_type IN ('github_release','generic_git','marketplace','local_package')),
    source_uri text NOT NULL,
    commit_sha text,
    artifact_digest text,
    publisher text,
    signature jsonb,
    verified boolean NOT NULL DEFAULT false,
    trust text NOT NULL CHECK (trust IN ('VERIFIED','USER_PROVIDED','AGENT_INFERRED','EXTERNAL','UNTRUSTED')),
    state text NOT NULL DEFAULT 'discovered'
        CHECK (state IN ('discovered','staged','installed','enabled','dormant','active','unhealthy','update_available')),
    install_path text,
    manifest_version integer NOT NULL DEFAULT 1,
    sandbox_policy jsonb NOT NULL DEFAULT '{}'::jsonb,
    network_policy text NOT NULL DEFAULT 'NONE'
        CHECK (network_policy IN ('NONE','RESTRICTED','FULL')),
    resource_limits jsonb,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (profile_id, name)
);

-- 5. Plugin components (what the plugin provides)
CREATE TABLE plugin_components (
    id uuid PRIMARY KEY,
    plugin_id uuid NOT NULL REFERENCES plugins(id) ON DELETE CASCADE,
    component_type text NOT NULL CHECK (component_type IN ('skill','mcp_server','source_book','executable','asset','schema')),
    name text NOT NULL,
    manifest_ref text NOT NULL,
    metadata jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (plugin_id, name, component_type)
);

-- 6. Plugin permissions (scoped allowlist)
CREATE TABLE plugin_permissions (
    id uuid PRIMARY KEY,
    plugin_id uuid NOT NULL REFERENCES plugins(id) ON DELETE CASCADE,
    permission_domain text NOT NULL CHECK (permission_domain IN (
        'filesystem_read','filesystem_write','network','secrets','subprocess','admin','system_info')),
    scope_value text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (plugin_id, permission_domain, scope_value)
);

-- 7. Plugin installations (versioned install records for staged upgrades)
CREATE TABLE plugin_installations (
    id uuid PRIMARY KEY,
    plugin_id uuid NOT NULL REFERENCES plugins(id) ON DELETE CASCADE,
    version text NOT NULL,
    artifact_digest text NOT NULL,
    status text NOT NULL DEFAULT 'staged' CHECK (status IN ('staged','installing','active','failed','rolled_back')),
    installed_by uuid REFERENCES users(id) ON DELETE SET NULL,
    sandbox_image text,
    sandbox_digest text,
    peer_port integer,
    self_test_result jsonb,
    installed_at timestamptz,
    activated_at timestamptz,
    rolled_back_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (plugin_id, version)
);

-- 8. Indexes
CREATE INDEX books_kind_idx ON books (kind) WHERE kind IS NOT NULL;
CREATE INDEX books_kind_profile_updated_idx ON books (profile_id, kind, updated_at DESC);
CREATE INDEX plugins_profile_state_idx ON plugins (profile_id, state);
CREATE INDEX plugin_components_plugin_idx ON plugin_components (plugin_id);

-- 9. Companion Book sync triggers (SKILL/MCP source rows -> books registry).
--    The PLUGIN companion Book is created by the install handler (Lane C), not here.
CREATE OR REPLACE FUNCTION skill_book_sync_fn() RETURNS trigger AS $$
BEGIN
    UPDATE books SET body = NEW.description, updated_at = now()
    WHERE kind = 'SKILL' AND metadata->>'skill_id' = NEW.id::text;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER skill_book_sync
AFTER UPDATE OF description ON skills
FOR EACH ROW EXECUTE FUNCTION skill_book_sync_fn();

CREATE OR REPLACE FUNCTION mcp_book_sync_fn() RETURNS trigger AS $$
BEGIN
    UPDATE books SET body = COALESCE(NEW.configuration->>'description', NEW.transport || ' MCP server'), updated_at = now()
    WHERE kind = 'MCP' AND metadata->>'mcp_server_id' = NEW.id::text;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER mcp_book_sync
AFTER UPDATE ON mcp_servers
FOR EACH ROW EXECUTE FUNCTION mcp_book_sync_fn();

CREATE OR REPLACE FUNCTION skill_book_delete_fn() RETURNS trigger AS $$
BEGIN
    DELETE FROM books WHERE kind = 'SKILL' AND metadata->>'skill_id' = OLD.id::text;
    RETURN OLD;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER skill_book_delete
AFTER DELETE ON skills
FOR EACH ROW EXECUTE FUNCTION skill_book_delete_fn();

CREATE OR REPLACE FUNCTION mcp_book_delete_fn() RETURNS trigger AS $$
BEGIN
    DELETE FROM books WHERE kind = 'MCP' AND metadata->>'mcp_server_id' = OLD.id::text;
    RETURN OLD;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER mcp_book_delete
AFTER DELETE ON mcp_servers
FOR EACH ROW EXECUTE FUNCTION mcp_book_delete_fn();

-- 10. Bump schema
UPDATE schema_metadata SET schema_version = 20, updated_at = now() WHERE singleton;
