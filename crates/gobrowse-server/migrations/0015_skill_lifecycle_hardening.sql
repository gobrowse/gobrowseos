-- Skills lifecycle hardening. Repair legacy evidence before installing the
-- constraints that protect new writes. This migration is intentionally not
-- idempotent: a partial deployment must fail loudly.

CREATE TABLE skill_revision_integrity_quarantine (
    id bigserial PRIMARY KEY,
    revision_id uuid NOT NULL,
    skill_id uuid NOT NULL,
    issue text NOT NULL,
    original_evaluation jsonb,
    original_source_conversation_ids uuid[],
    repaired_source_conversation_ids uuid[],
    detail jsonb NOT NULL DEFAULT '{}',
    quarantined_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX skill_revision_integrity_quarantine_skill_idx
    ON skill_revision_integrity_quarantine (skill_id, revision_id, quarantined_at);

CREATE OR REPLACE FUNCTION reject_skill_integrity_evidence_mutation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION 'skill integrity quarantine is append-only'
        USING ERRCODE = '55000';
END;
$$;

CREATE TRIGGER skill_integrity_quarantine_immutable
    BEFORE UPDATE OR DELETE ON skill_integrity_quarantine
    FOR EACH ROW EXECUTE FUNCTION reject_skill_integrity_evidence_mutation();
CREATE TRIGGER skill_integrity_quarantine_no_truncate
    BEFORE TRUNCATE ON skill_integrity_quarantine
    FOR EACH STATEMENT EXECUTE FUNCTION reject_skill_integrity_evidence_mutation();
CREATE TRIGGER skill_revision_integrity_quarantine_immutable
    BEFORE UPDATE OR DELETE ON skill_revision_integrity_quarantine
    FOR EACH ROW EXECUTE FUNCTION reject_skill_integrity_evidence_mutation();
CREATE TRIGGER skill_revision_integrity_quarantine_no_truncate
    BEFORE TRUNCATE ON skill_revision_integrity_quarantine
    FOR EACH STATEMENT EXECUTE FUNCTION reject_skill_integrity_evidence_mutation();

CREATE OR REPLACE FUNCTION skill_evaluation_is_valid(value jsonb)
RETURNS boolean
LANGUAGE plpgsql IMMUTABLE STRICT
AS $$
DECLARE
    key text;
    number_value numeric;
BEGIN
    IF jsonb_typeof(value) <> 'object'
       OR (SELECT count(*) FROM jsonb_object_keys(value)) <> 8
       OR EXISTS (
           SELECT 1 FROM jsonb_object_keys(value) AS object_key
           WHERE object_key NOT IN (
               'deterministic_checks_passed', 'attempts', 'successful_attempts',
               'steps', 'retries', 'errors', 'duration_ms', 'user_corrections'
           )
       )
    THEN
        RETURN false;
    END IF;
    IF jsonb_typeof(value->'deterministic_checks_passed') <> 'boolean' THEN RETURN false; END IF;
    FOREACH key IN ARRAY ARRAY['attempts','successful_attempts','steps','retries','errors','duration_ms','user_corrections'] LOOP
        IF jsonb_typeof(value->key) <> 'number' THEN RETURN false; END IF;
        number_value := (value->>key)::numeric;
        IF number_value < 0 OR trunc(number_value) <> number_value THEN RETURN false; END IF;
    END LOOP;
    IF (value->>'attempts')::numeric > 1000000
       OR (value->>'successful_attempts')::numeric > (value->>'attempts')::numeric
       OR (value->>'steps')::numeric > 10000000
       OR (value->>'retries')::numeric > 10000000
       OR (value->>'errors')::numeric > 1000000
       OR (value->>'duration_ms')::numeric > 86400000
       OR (value->>'user_corrections')::numeric > (value->>'attempts')::numeric
    THEN
        RETURN false;
    END IF;
    RETURN true;
EXCEPTION WHEN OTHERS THEN
    RETURN false;
END;
$$;

INSERT INTO skill_revision_integrity_quarantine
    (revision_id, skill_id, issue, original_evaluation, detail)
SELECT id, skill_id, 'invalid_evaluation', evaluation, '{"action":"cleared"}'::jsonb
FROM skill_revisions
WHERE evaluation IS NOT NULL AND NOT skill_evaluation_is_valid(evaluation);

UPDATE skill_revisions
SET evaluation = NULL
WHERE evaluation IS NOT NULL AND NOT skill_evaluation_is_valid(evaluation);

-- Preserve the first valid occurrence in submitted order. The source repair is
-- deliberately explicit so operators can inspect both arrays after migration.
DROP TRIGGER skill_revisions_immutable ON skill_revisions;
DO $$
DECLARE
    revision_row record;
    repaired uuid[];
    source_count integer;
BEGIN
    FOR revision_row IN
        SELECT r.id, r.skill_id, r.source_conversation_ids,
               s.profile_id, s.workspace_id
        FROM skill_revisions r
        JOIN skills s ON s.id = r.skill_id
        WHERE cardinality(r.source_conversation_ids) > 100
           OR EXISTS (SELECT 1 FROM unnest(r.source_conversation_ids) x(id) WHERE x.id IS NULL OR x.id = '00000000-0000-0000-0000-000000000000')
           OR cardinality(r.source_conversation_ids) <> (SELECT count(DISTINCT x.id) FROM unnest(r.source_conversation_ids) x(id))
           OR (SELECT count(*) FROM conversations c WHERE c.id = ANY(r.source_conversation_ids)
                 AND c.status <> 'deleted' AND c.profile_id = s.profile_id
                 AND (s.workspace_id IS NULL OR c.workspace_id = s.workspace_id))
              <> cardinality(r.source_conversation_ids)
    LOOP
        SELECT COALESCE(array_agg(c.id ORDER BY first_seen.ord), '{}'::uuid[])
        INTO repaired
        FROM (
            SELECT x.id, min(x.ord) AS ord
            FROM unnest(revision_row.source_conversation_ids) WITH ORDINALITY x(id, ord)
            WHERE x.id IS NOT NULL
              AND x.id <> '00000000-0000-0000-0000-000000000000'::uuid
              AND EXISTS (
                  SELECT 1 FROM conversations c
                  WHERE c.id = x.id AND c.status <> 'deleted'
                    AND c.profile_id = revision_row.profile_id
                    AND (revision_row.workspace_id IS NULL OR c.workspace_id = revision_row.workspace_id)
              )
            GROUP BY x.id
            ORDER BY min(x.ord)
            LIMIT 100
        ) first_seen
        JOIN conversations c ON c.id = first_seen.id;

        SELECT count(*) INTO source_count
        FROM unnest(revision_row.source_conversation_ids) x(id);
        INSERT INTO skill_revision_integrity_quarantine
            (revision_id, skill_id, issue, original_source_conversation_ids,
             repaired_source_conversation_ids, detail)
        VALUES (revision_row.id, revision_row.skill_id, 'invalid_source_conversation_ids',
                revision_row.source_conversation_ids, repaired,
                jsonb_build_object('original_count', source_count));
        UPDATE skill_revisions
        SET source_conversation_ids = repaired
        WHERE id = revision_row.id;
    END LOOP;
END;
$$;

-- Source repairs are the one controlled update to immutable provenance.
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
       OR (OLD.evaluation IS NOT NULL AND OLD.evaluation IS DISTINCT FROM NEW.evaluation)
    THEN
        RAISE EXCEPTION 'skill revisions are immutable' USING ERRCODE = '55000';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER skill_revisions_immutable
    BEFORE UPDATE ON skill_revisions
    FOR EACH ROW EXECUTE FUNCTION reject_skill_revision_mutation();

ALTER TABLE skill_revisions
    ADD CONSTRAINT skill_revisions_evaluation_valid
    CHECK (evaluation IS NULL OR skill_evaluation_is_valid(evaluation));

CREATE OR REPLACE FUNCTION validate_skill_revision_sources()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    profile uuid;
    workspace uuid;
    source uuid;
    locked_count integer := 0;
BEGIN
    IF cardinality(NEW.source_conversation_ids) > 100
       OR EXISTS (SELECT 1 FROM unnest(NEW.source_conversation_ids) x(id) WHERE x.id IS NULL OR x.id = '00000000-0000-0000-0000-000000000000'::uuid)
       OR cardinality(NEW.source_conversation_ids) <> (SELECT count(DISTINCT x.id) FROM unnest(NEW.source_conversation_ids) x(id))
    THEN
        RAISE EXCEPTION 'invalid skill revision sources'
            USING ERRCODE = '23514', CONSTRAINT = 'skill_revisions_sources_valid';
    END IF;
    SELECT s.profile_id, s.workspace_id INTO profile, workspace
    FROM skills s WHERE s.id = NEW.skill_id;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'invalid skill revision skill'
            USING ERRCODE = '23514', CONSTRAINT = 'skill_revisions_sources_valid';
    END IF;
    FOR source IN
        SELECT c.id
        FROM conversations c
        WHERE c.id = ANY(NEW.source_conversation_ids)
          AND c.status <> 'deleted'
          AND c.profile_id = profile
          AND (workspace IS NULL OR c.workspace_id = workspace)
        ORDER BY c.id
        FOR KEY SHARE OF c
    LOOP
        locked_count := locked_count + 1;
    END LOOP;
    IF locked_count <> cardinality(NEW.source_conversation_ids) THEN
        RAISE EXCEPTION 'skill revision sources are missing or outside scope'
            USING ERRCODE = '23514', CONSTRAINT = 'skill_revisions_sources_valid';
    END IF;
    RETURN NEW;
END;
$$;
CREATE TRIGGER skill_revisions_sources_valid
    BEFORE INSERT OR UPDATE OF skill_id, source_conversation_ids ON skill_revisions
    FOR EACH ROW EXECUTE FUNCTION validate_skill_revision_sources();

INSERT INTO skill_integrity_quarantine
    (skill_id, profile_id, workspace_id, name, issue, detail)
SELECT s.id, s.profile_id, s.workspace_id, s.name, 'promotion_state_mismatch',
       json_build_object('active_revision', s.active_revision,
                         'promoted_revision', p.revision)::text
FROM skills s
LEFT JOIN skill_revisions p ON p.skill_id = s.id AND p.promoted
WHERE (s.active_revision IS NULL AND p.revision IS NOT NULL)
   OR (s.active_revision IS NOT NULL AND (p.revision IS NULL OR p.revision <> s.active_revision));

-- Demote a conflicting winner before promoting the active revision. The
-- schema-13 partial unique index rejects a single statement that swaps both
-- rows when PostgreSQL updates the active row first.
UPDATE skill_revisions r
SET promoted = false
FROM skills s
WHERE s.id = r.skill_id
  AND s.active_revision IS NOT NULL
  AND r.promoted
  AND r.revision <> s.active_revision;

UPDATE skill_revisions r
SET promoted = true
FROM skills s
WHERE s.id = r.skill_id
  AND s.active_revision IS NOT NULL
  AND r.revision = s.active_revision
  AND NOT r.promoted;
UPDATE skills s
SET active_revision = p.revision
FROM skill_revisions p
WHERE p.skill_id = s.id AND p.promoted AND s.active_revision IS NULL;

CREATE OR REPLACE FUNCTION assert_skill_promotion_consistency()
RETURNS trigger
LANGUAGE plpgsql
AS $$
DECLARE
    skill_key uuid;
    active bigint;
    promoted_count bigint;
    promoted_revision bigint;
BEGIN
    IF TG_TABLE_NAME = 'skills' THEN
        skill_key := COALESCE(NEW.id, OLD.id);
    ELSE
        skill_key := COALESCE(NEW.skill_id, OLD.skill_id);
    END IF;
    SELECT s.active_revision INTO active FROM skills s WHERE s.id = skill_key;
    IF NOT FOUND THEN RETURN COALESCE(NEW, OLD); END IF;
    SELECT count(*), max(r.revision) INTO promoted_count, promoted_revision
    FROM skill_revisions r WHERE r.skill_id = skill_key AND r.promoted;
    IF NOT ((active IS NULL AND promoted_count = 0)
        OR (active IS NOT NULL AND promoted_count = 1 AND promoted_revision = active)) THEN
        RAISE EXCEPTION 'skill active and promoted state is inconsistent'
            USING ERRCODE = '23514', CONSTRAINT = 'skills_active_promoted_consistency';
    END IF;
    RETURN COALESCE(NEW, OLD);
END;
$$;
CREATE CONSTRAINT TRIGGER skills_active_promoted_consistency
    AFTER INSERT OR UPDATE OR DELETE ON skills
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION assert_skill_promotion_consistency();
CREATE CONSTRAINT TRIGGER skill_revisions_active_promoted_consistency
    AFTER INSERT OR UPDATE OR DELETE ON skill_revisions
    DEFERRABLE INITIALLY DEFERRED FOR EACH ROW
    EXECUTE FUNCTION assert_skill_promotion_consistency();

UPDATE schema_metadata SET schema_version = 15, updated_at = now() WHERE singleton;
