-- 0016_worktree_integrity.sql
-- Harden inert worktree metadata. Unsafe legacy rows are retained in an
-- append-only quarantine before they are removed; no tenant link is repaired.

CREATE TABLE worktree_integrity_quarantine (
    quarantine_id bigserial PRIMARY KEY,
    id uuid NOT NULL,
    workspace_id uuid NOT NULL,
    task_id uuid NOT NULL,
    owner_agent_id uuid NOT NULL,
    branch text NOT NULL,
    base_commit text NOT NULL,
    path text NOT NULL,
    status text NOT NULL,
    changed_files text[] NOT NULL,
    last_activity_at timestamptz NOT NULL,
    reason text NOT NULL,
    quarantined_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX worktree_integrity_quarantine_workspace_idx
    ON worktree_integrity_quarantine (workspace_id, quarantined_at);

CREATE OR REPLACE FUNCTION reject_worktree_integrity_quarantine_mutation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION 'worktree_integrity_quarantine is append-only';
END;
$$;
DROP TRIGGER IF EXISTS worktree_integrity_quarantine_append_only
    ON worktree_integrity_quarantine;
CREATE TRIGGER worktree_integrity_quarantine_append_only
    BEFORE UPDATE OR DELETE ON worktree_integrity_quarantine
    FOR EACH ROW EXECUTE FUNCTION reject_worktree_integrity_quarantine_mutation();
DROP TRIGGER IF EXISTS worktree_integrity_quarantine_append_only_truncate
    ON worktree_integrity_quarantine;
CREATE TRIGGER worktree_integrity_quarantine_append_only_truncate
    BEFORE TRUNCATE ON worktree_integrity_quarantine
    FOR EACH STATEMENT EXECUTE FUNCTION reject_worktree_integrity_quarantine_mutation();

CREATE OR REPLACE FUNCTION gobrowse_valid_worktree_branch(value text)
RETURNS boolean LANGUAGE sql IMMUTABLE AS $$
    SELECT value <> ''
       AND value !~ '^/'
       AND value !~ '[[:cntrl:]]'
       AND value !~ '[ ~^:?*@]'
       AND position('[' in value) = 0
       AND position(chr(92) in value) = 0
       AND value !~ '(^|/)\.'
       AND value !~ '//'
       AND value !~ '(^-|/$|\.$|\.lock$|\.\.)'
       AND value !~ '^refs/';
$$;

CREATE OR REPLACE FUNCTION gobrowse_valid_worktree_base_commit(value text)
RETURNS boolean LANGUAGE sql IMMUTABLE AS $$
    SELECT (length(value) IN (40, 64) AND value ~ '^[0-9A-Fa-f]+$');
$$;

CREATE OR REPLACE FUNCTION gobrowse_valid_worktree_path(value text, task uuid)
RETURNS boolean LANGUAGE sql IMMUTABLE AS $$
    SELECT value ~ '^/[^[:cntrl:]]*$'
       AND value !~ '(^|/)\.(\.?)(/|$)'
       AND value !~ '//'
       AND value ~ ('/worktrees/task-' || left(replace(task::text, '-', ''), 12) || '$');
$$;

CREATE OR REPLACE FUNCTION gobrowse_valid_changed_files(value text[])
RETURNS boolean LANGUAGE sql IMMUTABLE AS $$
    SELECT cardinality(value) <= 500
       AND coalesce((SELECT sum(octet_length(item) + 1) FROM unnest(value) AS item), 0) <= 100000
       AND NOT EXISTS (
           SELECT 1 FROM unnest(value) AS item
           WHERE item = '' OR octet_length(item) > 4096
              OR item ~ '[[:cntrl:]]'
              OR position(chr(92) in item) > 0
              OR item ~ '(^/|//|(^|/)(\.|\.\.)(/|$))'
       )
       AND value = ARRAY(SELECT item FROM unnest(value) AS item ORDER BY item COLLATE "C")
       AND cardinality(value) = (SELECT count(DISTINCT item) FROM unnest(value) AS item);
$$;

INSERT INTO worktree_integrity_quarantine
    (id, workspace_id, task_id, owner_agent_id, branch, base_commit, path,
     status, changed_files, last_activity_at, reason)
SELECT worktree.id, worktree.workspace_id, worktree.task_id,
       worktree.owner_agent_id, worktree.branch, worktree.base_commit,
       worktree.path, worktree.status, worktree.changed_files, worktree.last_activity_at,
       concat_ws(';',
           CASE WHEN NOT EXISTS (
               SELECT 1 FROM tasks task
               WHERE task.id = worktree.task_id
                 AND task.workspace_id = worktree.workspace_id
           ) THEN 'cross_workspace_task' END,
           CASE WHEN NOT EXISTS (
               SELECT 1 FROM agents agent
               WHERE agent.id = worktree.owner_agent_id
                 AND agent.workspace_id = worktree.workspace_id
           ) THEN 'cross_workspace_owner_agent' END,
           CASE WHEN NOT gobrowse_valid_worktree_branch(worktree.branch)
                THEN 'unsafe_branch' END,
           CASE WHEN NOT gobrowse_valid_worktree_base_commit(worktree.base_commit)
                THEN 'unsafe_base_commit' END,
           CASE WHEN NOT gobrowse_valid_worktree_path(worktree.path, worktree.task_id)
                THEN 'unsafe_path' END,
           CASE WHEN NOT gobrowse_valid_changed_files(worktree.changed_files)
                THEN 'unsafe_changed_files' END,
           CASE WHEN worktree.status <> 'ACTIVE' THEN 'unsafe_status' END)
FROM worktrees worktree
WHERE NOT EXISTS (
          SELECT 1 FROM tasks task
          WHERE task.id = worktree.task_id
            AND task.workspace_id = worktree.workspace_id
      )
   OR NOT EXISTS (
          SELECT 1 FROM agents agent
          WHERE agent.id = worktree.owner_agent_id
            AND agent.workspace_id = worktree.workspace_id
      )
   OR NOT gobrowse_valid_worktree_branch(worktree.branch)
   OR NOT gobrowse_valid_worktree_base_commit(worktree.base_commit)
   OR NOT gobrowse_valid_worktree_path(worktree.path, worktree.task_id)
   OR NOT gobrowse_valid_changed_files(worktree.changed_files)
   OR worktree.status <> 'ACTIVE';

DELETE FROM worktrees worktree
WHERE NOT EXISTS (
          SELECT 1 FROM tasks task
          WHERE task.id = worktree.task_id
            AND task.workspace_id = worktree.workspace_id
      )
   OR NOT EXISTS (
          SELECT 1 FROM agents agent
          WHERE agent.id = worktree.owner_agent_id
            AND agent.workspace_id = worktree.workspace_id
      )
   OR NOT gobrowse_valid_worktree_branch(worktree.branch)
   OR NOT gobrowse_valid_worktree_base_commit(worktree.base_commit)
   OR NOT gobrowse_valid_worktree_path(worktree.path, worktree.task_id)
   OR NOT gobrowse_valid_changed_files(worktree.changed_files)
   OR worktree.status <> 'ACTIVE';

ALTER TABLE worktrees DROP CONSTRAINT IF EXISTS worktrees_task_id_fkey;
ALTER TABLE worktrees DROP CONSTRAINT IF EXISTS worktrees_owner_agent_id_fkey;
ALTER TABLE worktrees ADD CONSTRAINT worktrees_task_same_workspace_fk
    FOREIGN KEY (workspace_id, task_id)
    REFERENCES tasks (workspace_id, id) ON DELETE RESTRICT;
ALTER TABLE worktrees ADD CONSTRAINT worktrees_owner_same_workspace_fk
    FOREIGN KEY (workspace_id, owner_agent_id)
    REFERENCES agents (workspace_id, id) ON DELETE RESTRICT;
ALTER TABLE worktrees ADD CONSTRAINT worktrees_status_check CHECK (status = 'ACTIVE');
ALTER TABLE worktrees ADD CONSTRAINT worktrees_branch_integrity_check
    CHECK (gobrowse_valid_worktree_branch(branch));
ALTER TABLE worktrees ADD CONSTRAINT worktrees_base_commit_integrity_check
    CHECK (gobrowse_valid_worktree_base_commit(base_commit));
ALTER TABLE worktrees ADD CONSTRAINT worktrees_path_integrity_check
    CHECK (gobrowse_valid_worktree_path(path, task_id));
ALTER TABLE worktrees ADD CONSTRAINT worktrees_changed_files_integrity_check
    CHECK (gobrowse_valid_changed_files(changed_files));

UPDATE schema_metadata SET schema_version = 16, updated_at = now() WHERE singleton;
