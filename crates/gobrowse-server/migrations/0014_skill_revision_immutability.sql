-- Revision content and provenance are immutable.  Evaluation and promotion are
-- deliberately lifecycle fields: evaluation is recorded once by the API and
-- promotion changes only the state of a revision, never its procedure.
CREATE OR REPLACE FUNCTION reject_skill_revision_mutation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    IF OLD.id IS DISTINCT FROM NEW.id
       OR OLD.skill_id IS DISTINCT FROM NEW.skill_id
       OR OLD.revision IS DISTINCT FROM NEW.revision
       OR OLD.content IS DISTINCT FROM NEW.content
       OR OLD.author IS DISTINCT FROM NEW.author
       OR OLD.reason IS DISTINCT FROM NEW.reason
       OR OLD.source_conversation_ids IS DISTINCT FROM NEW.source_conversation_ids
       OR OLD.created_at IS DISTINCT FROM NEW.created_at
    THEN
        RAISE EXCEPTION 'skill revisions are immutable' USING ERRCODE = '55000';
    END IF;
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS skill_revisions_immutable ON skill_revisions;
CREATE TRIGGER skill_revisions_immutable
    BEFORE UPDATE ON skill_revisions
    FOR EACH ROW EXECUTE FUNCTION reject_skill_revision_mutation();

UPDATE schema_metadata SET schema_version = 14, updated_at = now() WHERE singleton;
