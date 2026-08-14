-- Skills integrity repair and tenant boundary constraints.
-- Legacy skills were created before NULL-aware global-name and composite
-- workspace checks existed.  Preserve every repaired row in quarantine before
-- making the conservative repair visible to the live tables.

CREATE TABLE skill_integrity_quarantine (
    id bigserial PRIMARY KEY,
    skill_id uuid NOT NULL,
    profile_id uuid NOT NULL,
    workspace_id uuid,
    name text NOT NULL,
    issue text NOT NULL,
    detail text NOT NULL,
    quarantined_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX skill_integrity_quarantine_skill_idx
    ON skill_integrity_quarantine (skill_id, quarantined_at);

-- A workspace is identified by both its tenant and its id for the composite
-- foreign key below.  The existing primary key remains useful to callers.
ALTER TABLE workspaces
    ADD CONSTRAINT workspaces_profile_id_id_key UNIQUE (profile_id, id);

-- A skill may not point at a workspace in another profile.  Detach malformed
-- legacy rows rather than silently moving them into another tenant.
INSERT INTO skill_integrity_quarantine
    (skill_id, profile_id, workspace_id, name, issue, detail)
SELECT skill.id, skill.profile_id, skill.workspace_id, skill.name,
       'workspace_profile_mismatch',
       'workspace was detached because it belongs to another profile'
FROM skills skill
JOIN workspaces workspace ON workspace.id = skill.workspace_id
WHERE workspace.profile_id <> skill.profile_id;

UPDATE skills skill
SET workspace_id = NULL
FROM workspaces workspace
WHERE workspace.id = skill.workspace_id
  AND workspace.profile_id <> skill.profile_id;

-- UNIQUE(profile_id, workspace_id, name) does not protect rows where
-- workspace_id is NULL.  Keep the oldest global row under its original name
-- and make later rows explicitly identifiable quarantined records.
WITH duplicates AS (
    SELECT id, profile_id, workspace_id, name,
           row_number() OVER (PARTITION BY profile_id, name ORDER BY created_at, id) AS ordinal
    FROM skills
    WHERE workspace_id IS NULL
)
INSERT INTO skill_integrity_quarantine
    (skill_id, profile_id, workspace_id, name, issue, detail)
SELECT id, profile_id, workspace_id, name, 'duplicate_global_name',
       'name was suffixed to preserve the legacy skill and global uniqueness'
FROM duplicates
WHERE ordinal > 1;

WITH duplicates AS (
    SELECT id,
           row_number() OVER (PARTITION BY profile_id, name ORDER BY created_at, id) AS ordinal
    FROM skills
    WHERE workspace_id IS NULL
)
UPDATE skills skill
SET name = skill.name || ' [quarantined:' || skill.id::text || ']'
FROM duplicates
WHERE duplicates.id = skill.id
  AND duplicates.ordinal > 1;

-- Preserve the extra promoted states, then retain one deterministic winner.
-- Prefer the active revision when it was promoted, otherwise retain the newest
-- promoted revision.  The partial unique index is installed only afterwards.
WITH promoted AS (
    SELECT revision.id, revision.skill_id, revision.revision,
           row_number() OVER (
               PARTITION BY revision.skill_id
               ORDER BY (revision.revision = skill.active_revision) DESC,
                        revision.revision DESC, revision.id DESC
           ) AS ordinal
    FROM skill_revisions revision
    JOIN skills skill ON skill.id = revision.skill_id
    WHERE revision.promoted
)
INSERT INTO skill_integrity_quarantine
    (skill_id, profile_id, workspace_id, name, issue, detail)
SELECT promoted.skill_id, skill.profile_id, skill.workspace_id, skill.name,
       'multiple_promoted_revisions',
       'extra promoted revision was demoted during integrity repair'
FROM promoted
JOIN skills skill ON skill.id = promoted.skill_id
WHERE promoted.ordinal > 1;

WITH promoted AS (
    SELECT id,
           row_number() OVER (
               PARTITION BY skill_id
               ORDER BY (revision = (SELECT active_revision FROM skills WHERE id = skill_revisions.skill_id)) DESC,
                        revision DESC, id DESC
           ) AS ordinal
    FROM skill_revisions
    WHERE promoted
)
UPDATE skill_revisions revision
SET promoted = false
FROM promoted
WHERE promoted.id = revision.id
  AND promoted.ordinal > 1;

-- An active revision must be a revision of the same skill.  Nulling an unsafe
-- pointer is safer than selecting content from a different skill.
INSERT INTO skill_integrity_quarantine
    (skill_id, profile_id, workspace_id, name, issue, detail)
SELECT skill.id, skill.profile_id, skill.workspace_id, skill.name,
       'active_revision_mismatch',
       'active revision pointer was cleared because it does not belong to skill'
FROM skills skill
WHERE skill.active_revision IS NOT NULL
  AND NOT EXISTS (
      SELECT 1 FROM skill_revisions revision
      WHERE revision.skill_id = skill.id
        AND revision.revision = skill.active_revision
  );

UPDATE skills skill
SET active_revision = NULL
WHERE skill.active_revision IS NOT NULL
  AND NOT EXISTS (
      SELECT 1 FROM skill_revisions revision
      WHERE revision.skill_id = skill.id
        AND revision.revision = skill.active_revision
  );

ALTER TABLE skills DROP CONSTRAINT IF EXISTS skills_workspace_id_fkey;
ALTER TABLE skills ADD CONSTRAINT skills_workspace_profile_fk
    FOREIGN KEY (profile_id, workspace_id)
    REFERENCES workspaces (profile_id, id) ON DELETE CASCADE;

ALTER TABLE skills ADD CONSTRAINT skills_active_revision_fk
    FOREIGN KEY (id, active_revision)
    REFERENCES skill_revisions (skill_id, revision);

CREATE UNIQUE INDEX skills_profile_global_name_unique
    ON skills (profile_id, name)
    WHERE workspace_id IS NULL;
CREATE UNIQUE INDEX skill_revisions_one_promoted
    ON skill_revisions (skill_id)
    WHERE promoted;

UPDATE schema_metadata SET schema_version = 13, updated_at = now() WHERE singleton;
