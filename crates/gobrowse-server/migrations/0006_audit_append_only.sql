-- 0006_audit_append_only.sql
-- Make audit_events append-only at the database layer.
-- Even a raw SQL session bypassing the application cannot UPDATE,
-- DELETE, or TRUNCATE audit history.
--
-- Two triggers share one function:
--   1. audit_events_append_only           – row-level BEFORE UPDATE/DELETE
--   2. audit_events_append_only_truncate  – statement-level BEFORE TRUNCATE
--
-- Row-level is needed so the function can compare OLD/NEW and allow
-- FK-cascade updates (ON DELETE SET NULL from users/profiles) that only
-- clear actor_user_id or profile_id without changing any audit data.

CREATE OR REPLACE FUNCTION reject_audit_events_mutation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF TG_OP = 'UPDATE' THEN
        -- Allow FK-cascade updates: when a parent user or profile is deleted,
        -- ON DELETE SET NULL fires an UPDATE that clears only actor_user_id
        -- or profile_id.  Every other column must be unchanged.
        IF OLD.sequence = NEW.sequence
           AND OLD.action = NEW.action
           AND OLD.resource_type = NEW.resource_type
           AND OLD.resource_id IS NOT DISTINCT FROM NEW.resource_id
           AND OLD.outcome = NEW.outcome
           AND OLD.detail IS NOT DISTINCT FROM NEW.detail
           AND OLD.created_at = NEW.created_at
           AND OLD.request_id IS NOT DISTINCT FROM NEW.request_id
        THEN
            RETURN NEW;
        END IF;
    END IF;

    RAISE EXCEPTION 'audit_events is append-only';
END;
$$;

DROP TRIGGER IF EXISTS audit_events_append_only ON audit_events;
CREATE TRIGGER audit_events_append_only
    BEFORE UPDATE OR DELETE ON audit_events
    FOR EACH ROW
    EXECUTE FUNCTION reject_audit_events_mutation();

DROP TRIGGER IF EXISTS audit_events_append_only_truncate ON audit_events;
CREATE TRIGGER audit_events_append_only_truncate
    BEFORE TRUNCATE ON audit_events
    FOR EACH STATEMENT
    EXECUTE FUNCTION reject_audit_events_mutation();

UPDATE schema_metadata SET schema_version = 6, updated_at = now() WHERE singleton;
