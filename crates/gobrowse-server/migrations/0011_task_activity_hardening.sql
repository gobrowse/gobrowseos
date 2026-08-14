-- 0011_task_activity_hardening.sql
-- Repair legacy self-links, attribute public notes, and make quarantine evidence immutable.

-- 0010 records self-links before installing the self-parent checks, but its
-- repair predicates only handled missing/cross-workspace references.  Keep the
-- repair idempotent for databases that already completed 0010 and preserve
-- every legacy self-link before clearing it.
INSERT INTO task_integrity_quarantine
    (source_table, source_id, workspace_id, link_type, referenced_id, reason)
SELECT 'tasks', id::text, workspace_id, 'parent_task', parent_task_id, 'self_parent'
FROM tasks
WHERE parent_task_id = id;

INSERT INTO task_integrity_quarantine
    (source_table, source_id, workspace_id, link_type, referenced_id, reason)
SELECT 'agents', id::text, workspace_id, 'parent_agent', parent_agent_id, 'self_parent'
FROM agents
WHERE parent_agent_id = id;

UPDATE tasks SET parent_task_id = NULL WHERE parent_task_id = id;
UPDATE agents SET parent_agent_id = NULL WHERE parent_agent_id = id;

-- Human notes are the only activity kind exposed to workspace editors.  The
-- actor is deliberately separate from agent_id: a caller cannot claim that an
-- agent performed a lifecycle action.
ALTER TABLE activity_events ADD COLUMN IF NOT EXISTS actor_user_id uuid;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_constraint
        WHERE conname = 'activity_events_actor_user_fk'
          AND conrelid = 'activity_events'::regclass
    ) THEN
        ALTER TABLE activity_events ADD CONSTRAINT activity_events_actor_user_fk
            FOREIGN KEY (actor_user_id) REFERENCES users(id) ON DELETE RESTRICT;
    END IF;
END $$;

-- Quarantine rows are evidence of a repair and must never be rewritten or
-- removed, including by a raw SQL session.
CREATE OR REPLACE FUNCTION reject_task_integrity_quarantine_mutation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION 'task_integrity_quarantine is append-only';
END;
$$;

DROP TRIGGER IF EXISTS task_integrity_quarantine_append_only
    ON task_integrity_quarantine;
CREATE TRIGGER task_integrity_quarantine_append_only
    BEFORE UPDATE OR DELETE ON task_integrity_quarantine
    FOR EACH ROW
    EXECUTE FUNCTION reject_task_integrity_quarantine_mutation();

DROP TRIGGER IF EXISTS task_integrity_quarantine_append_only_truncate
    ON task_integrity_quarantine;
CREATE TRIGGER task_integrity_quarantine_append_only_truncate
    BEFORE TRUNCATE ON task_integrity_quarantine
    FOR EACH STATEMENT
    EXECUTE FUNCTION reject_task_integrity_quarantine_mutation();

UPDATE schema_metadata SET schema_version = 11, updated_at = now() WHERE singleton;
