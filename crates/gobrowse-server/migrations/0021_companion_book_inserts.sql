-- 0021_companion_book_inserts.sql
-- Companion Books for newly created Skills and MCP servers (schema 20 -> 21).
-- Migration 0020 backfilled only pre-existing rows and ships UPDATE/DELETE
-- triggers; without INSERT triggers the unified Library index would miss
-- skills/MCP servers created after the migration (checker finding F2).

CREATE OR REPLACE FUNCTION skill_book_insert_fn() RETURNS trigger AS $$
BEGIN
    INSERT INTO books (id, profile_id, title, body, book_type, scope, tags, provenance, trust,
        source, author, workspace_id, security_classification, kind, metadata)
    VALUES (
        gen_random_uuid(), NEW.profile_id, NEW.name, NEW.description,
        'INSTRUCTION', CASE WHEN NEW.workspace_id IS NOT NULL THEN 'WORKSPACE' ELSE 'PROFILE' END,
        '{}', 'SKILL', 'USER_PROVIDED',
        '{}', 'system', NEW.workspace_id, 'INTERNAL',
        'SKILL', jsonb_build_object('skill_id', NEW.id)
    )
    ON CONFLICT DO NOTHING;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER skill_book_insert
AFTER INSERT ON skills
FOR EACH ROW EXECUTE FUNCTION skill_book_insert_fn();

CREATE OR REPLACE FUNCTION mcp_book_insert_fn() RETURNS trigger AS $$
BEGIN
    INSERT INTO books (id, profile_id, title, body, book_type, scope, tags, provenance, trust,
        source, author, workspace_id, security_classification, kind, metadata)
    VALUES (
        gen_random_uuid(), NEW.profile_id, NEW.name,
        COALESCE(NEW.configuration->>'description', NEW.transport || ' MCP server'),
        'INSTRUCTION', 'PROFILE',
        '{}', 'MCP', 'USER_PROVIDED',
        '{}', 'system', NULL, 'INTERNAL',
        'MCP', jsonb_build_object('mcp_server_id', NEW.id)
    )
    ON CONFLICT DO NOTHING;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER mcp_book_insert
AFTER INSERT ON mcp_servers
FOR EACH ROW EXECUTE FUNCTION mcp_book_insert_fn();

UPDATE schema_metadata SET schema_version = 21, updated_at = now() WHERE singleton;