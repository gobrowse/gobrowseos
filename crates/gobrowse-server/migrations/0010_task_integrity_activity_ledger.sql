-- 0010_task_integrity_activity_ledger.sql
--
-- Repair the task/activity vertical slice without trusting application checks.
-- Existing bad links are recorded before being nulled, and only then are
-- composite foreign keys installed.  This keeps a forward upgrade safe even
-- when an early task API writer accepted cross-workspace references.
--
-- Activity ordering contract:
--   * A writer starts its transaction.
--   * It takes the workspace activity advisory transaction lock.
--   * It takes any task/agent row locks and writes the event.
--   * It commits, releasing the workspace lock.
--
-- This order prevents an event id from being allocated by a transaction that
-- later commits after a larger cursor id.  The INSERT trigger repeats the
-- lock before allocating the sequence value, so direct SQL writers cannot
-- bypass the ordering contract.  The lock is hash-based because PostgreSQL
-- advisory locks are 64-bit; a collision only serializes unrelated
-- workspaces, it cannot weaken their integrity.

CREATE TABLE task_integrity_quarantine (
    id bigserial PRIMARY KEY,
    source_table text NOT NULL,
    source_id text NOT NULL,
    workspace_id uuid NOT NULL,
    link_type text NOT NULL,
    referenced_id uuid,
    source_ordinal integer,
    reason text NOT NULL,
    quarantined_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX task_integrity_quarantine_workspace_idx
    ON task_integrity_quarantine (workspace_id, quarantined_at);

-- Preserve invalid array entries, including duplicates, before normalizing.
WITH expanded AS (
    SELECT task.workspace_id, task.id AS task_id, dependency.dependency_id,
           dependency.ordinal::integer AS source_ordinal
    FROM tasks task
    CROSS JOIN LATERAL unnest(task.dependencies) WITH ORDINALITY
        AS dependency(dependency_id, ordinal)
)
INSERT INTO task_integrity_quarantine
    (source_table, source_id, workspace_id, link_type, referenced_id,
     source_ordinal, reason)
SELECT 'tasks', expanded.task_id::text, expanded.workspace_id,
       'dependency', expanded.dependency_id, expanded.source_ordinal,
       CASE
           WHEN expanded.dependency_id IS NULL THEN 'null_dependency'
           WHEN expanded.dependency_id = expanded.task_id THEN 'self_dependency'
           WHEN NOT EXISTS (
               SELECT 1 FROM tasks candidate
               WHERE candidate.id = expanded.dependency_id
           ) THEN 'missing_dependency'
           WHEN NOT EXISTS (
               SELECT 1 FROM tasks candidate
               WHERE candidate.id = expanded.dependency_id
                 AND candidate.workspace_id = expanded.workspace_id
           ) THEN 'cross_workspace_dependency'
           ELSE 'duplicate_dependency'
       END
FROM expanded
WHERE expanded.dependency_id IS NULL
   OR expanded.dependency_id = expanded.task_id
   OR NOT EXISTS (
       SELECT 1 FROM tasks candidate
       WHERE candidate.id = expanded.dependency_id
         AND candidate.workspace_id = expanded.workspace_id
   )
   OR EXISTS (
       SELECT 1 FROM unnest((
           SELECT source.dependencies FROM tasks source
           WHERE source.id = expanded.task_id
       )) WITH ORDINALITY AS prior(dependency_id, ordinal)
       WHERE prior.dependency_id = expanded.dependency_id
         AND prior.ordinal < expanded.source_ordinal
   );

-- Composite keys are prerequisites for every same-workspace foreign key.
ALTER TABLE tasks ADD CONSTRAINT tasks_workspace_id_id_key UNIQUE (workspace_id, id);
ALTER TABLE agents ADD CONSTRAINT agents_workspace_id_id_key UNIQUE (workspace_id, id);

CREATE TABLE task_dependencies (
    workspace_id uuid NOT NULL,
    task_id uuid NOT NULL,
    dependency_task_id uuid NOT NULL,
    ordinal integer NOT NULL CHECK (ordinal >= 0),
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (task_id, dependency_task_id),
    UNIQUE (task_id, ordinal),
    CHECK (task_id <> dependency_task_id),
    CONSTRAINT task_dependencies_task_fk
        FOREIGN KEY (workspace_id, task_id)
        REFERENCES tasks (workspace_id, id) ON DELETE CASCADE,
    CONSTRAINT task_dependencies_dependency_fk
        FOREIGN KEY (workspace_id, dependency_task_id)
        REFERENCES tasks (workspace_id, id) ON DELETE CASCADE
);

INSERT INTO task_dependencies
    (workspace_id, task_id, dependency_task_id, ordinal)
SELECT task.workspace_id, task.id, dependency.dependency_id,
       dependency.ordinal::integer - 1
FROM tasks task
CROSS JOIN LATERAL unnest(task.dependencies) WITH ORDINALITY
    AS dependency(dependency_id, ordinal)
WHERE dependency.dependency_id IS NOT NULL
  AND dependency.dependency_id <> task.id
  AND EXISTS (
      SELECT 1 FROM tasks candidate
      WHERE candidate.id = dependency.dependency_id
        AND candidate.workspace_id = task.workspace_id
  )
  AND NOT EXISTS (
      SELECT 1
      FROM unnest(task.dependencies) WITH ORDINALITY AS prior(dependency_id, ordinal)
      WHERE prior.dependency_id = dependency.dependency_id
        AND prior.ordinal < dependency.ordinal
  );

-- Quarantine links that the old single-column FKs allowed to cross tenants.
INSERT INTO task_integrity_quarantine
    (source_table, source_id, workspace_id, link_type, referenced_id, reason)
SELECT 'tasks', task.id::text, task.workspace_id, 'parent_task', task.parent_task_id,
       CASE WHEN task.parent_task_id = task.id THEN 'self_parent' ELSE 'cross_workspace_parent' END
FROM tasks task
WHERE task.parent_task_id IS NOT NULL
  AND (task.parent_task_id = task.id OR NOT EXISTS (
      SELECT 1 FROM tasks parent
      WHERE parent.id = task.parent_task_id
        AND parent.workspace_id = task.workspace_id
  ));

INSERT INTO task_integrity_quarantine
    (source_table, source_id, workspace_id, link_type, referenced_id, reason)
SELECT 'tasks', task.id::text, task.workspace_id, 'assigned_agent', task.assigned_agent_id,
       'cross_workspace_agent'
FROM tasks task
WHERE task.assigned_agent_id IS NOT NULL
  AND NOT EXISTS (
      SELECT 1 FROM agents agent
      WHERE agent.id = task.assigned_agent_id
        AND agent.workspace_id = task.workspace_id
  );

INSERT INTO task_integrity_quarantine
    (source_table, source_id, workspace_id, link_type, referenced_id, reason)
SELECT 'agents', agent.id::text, agent.workspace_id, 'parent_agent', agent.parent_agent_id,
       CASE WHEN agent.parent_agent_id = agent.id THEN 'self_parent' ELSE 'cross_workspace_parent' END
FROM agents agent
WHERE agent.parent_agent_id IS NOT NULL
  AND (agent.parent_agent_id = agent.id OR NOT EXISTS (
      SELECT 1 FROM agents parent
      WHERE parent.id = agent.parent_agent_id
        AND parent.workspace_id = agent.workspace_id
  ));

INSERT INTO task_integrity_quarantine
    (source_table, source_id, workspace_id, link_type, referenced_id, reason)
SELECT 'activity_events', event.id::text, event.workspace_id, 'activity_task', event.task_id,
       'cross_workspace_task'
FROM activity_events event
WHERE event.task_id IS NOT NULL
  AND NOT EXISTS (
      SELECT 1 FROM tasks task
      WHERE task.id = event.task_id
        AND task.workspace_id = event.workspace_id
  );

INSERT INTO task_integrity_quarantine
    (source_table, source_id, workspace_id, link_type, referenced_id, reason)
SELECT 'activity_events', event.id::text, event.workspace_id, 'activity_agent', event.agent_id,
       'cross_workspace_agent'
FROM activity_events event
WHERE event.agent_id IS NOT NULL
  AND NOT EXISTS (
      SELECT 1 FROM agents agent
      WHERE agent.id = event.agent_id
        AND agent.workspace_id = event.workspace_id
  );

-- Quarantine is the audit trail; nulling is the conservative repair that lets
-- the new constraints protect all subsequent writes.
UPDATE tasks task
SET parent_task_id = NULL
WHERE task.parent_task_id IS NOT NULL
  AND (task.parent_task_id = task.id OR NOT EXISTS (
      SELECT 1 FROM tasks parent
      WHERE parent.id = task.parent_task_id
        AND parent.workspace_id = task.workspace_id
  ));
UPDATE tasks task
SET assigned_agent_id = NULL
WHERE task.assigned_agent_id IS NOT NULL
  AND NOT EXISTS (
      SELECT 1 FROM agents agent
      WHERE agent.id = task.assigned_agent_id
        AND agent.workspace_id = task.workspace_id
  );
UPDATE agents agent
SET parent_agent_id = NULL
WHERE agent.parent_agent_id IS NOT NULL
  AND (agent.parent_agent_id = agent.id OR NOT EXISTS (
      SELECT 1 FROM agents parent
      WHERE parent.id = agent.parent_agent_id
        AND parent.workspace_id = agent.workspace_id
  ));
UPDATE activity_events event
SET task_id = NULL
WHERE event.task_id IS NOT NULL
  AND NOT EXISTS (
      SELECT 1 FROM tasks task
      WHERE task.id = event.task_id
        AND task.workspace_id = event.workspace_id
  );
UPDATE activity_events event
SET agent_id = NULL
WHERE event.agent_id IS NOT NULL
  AND NOT EXISTS (
      SELECT 1 FROM agents agent
      WHERE agent.id = event.agent_id
        AND agent.workspace_id = event.workspace_id
  );

ALTER TABLE tasks DROP CONSTRAINT IF EXISTS tasks_parent_task_id_fkey;
ALTER TABLE tasks DROP CONSTRAINT IF EXISTS tasks_agent_fk;
ALTER TABLE agents DROP CONSTRAINT IF EXISTS agents_parent_agent_id_fkey;
ALTER TABLE activity_events DROP CONSTRAINT IF EXISTS activity_events_task_id_fkey;
ALTER TABLE activity_events DROP CONSTRAINT IF EXISTS activity_events_agent_id_fkey;
ALTER TABLE activity_events DROP CONSTRAINT IF EXISTS activity_events_workspace_id_fkey;

ALTER TABLE tasks ADD CONSTRAINT tasks_parent_same_workspace_fk
    FOREIGN KEY (workspace_id, parent_task_id)
    REFERENCES tasks (workspace_id, id)
    ON DELETE SET NULL (parent_task_id);
ALTER TABLE tasks ADD CONSTRAINT tasks_agent_same_workspace_fk
    FOREIGN KEY (workspace_id, assigned_agent_id)
    REFERENCES agents (workspace_id, id)
    ON DELETE SET NULL (assigned_agent_id);
ALTER TABLE tasks ADD CONSTRAINT tasks_parent_not_self_check
    CHECK (parent_task_id IS NULL OR parent_task_id <> id);
ALTER TABLE agents ADD CONSTRAINT agents_parent_same_workspace_fk
    FOREIGN KEY (workspace_id, parent_agent_id)
    REFERENCES agents (workspace_id, id)
    ON DELETE SET NULL (parent_agent_id);
ALTER TABLE agents ADD CONSTRAINT agents_parent_not_self_check
    CHECK (parent_agent_id IS NULL OR parent_agent_id <> id);

-- Activity references are RESTRICT, not SET NULL: immutable ledger rows must
-- retain their subject and a parent cannot be deleted out from under history.
ALTER TABLE activity_events ADD CONSTRAINT activity_events_workspace_fk
    FOREIGN KEY (workspace_id) REFERENCES workspaces (id) ON DELETE RESTRICT;
ALTER TABLE activity_events ADD CONSTRAINT activity_events_task_same_workspace_fk
    FOREIGN KEY (workspace_id, task_id)
    REFERENCES tasks (workspace_id, id) ON DELETE RESTRICT;
ALTER TABLE activity_events ADD CONSTRAINT activity_events_agent_same_workspace_fk
    FOREIGN KEY (workspace_id, agent_id)
    REFERENCES agents (workspace_id, id) ON DELETE RESTRICT;

ALTER TABLE tasks DROP COLUMN dependencies;

-- Sequence values must be allocated after the workspace lock, rather than by
-- a column default (PostgreSQL evaluates defaults before BEFORE INSERT).
ALTER TABLE activity_events ALTER COLUMN id DROP DEFAULT;

CREATE OR REPLACE FUNCTION lock_activity_workspace_and_assign_id()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    PERFORM pg_advisory_xact_lock(hashtextextended(NEW.workspace_id::text, 0));
    IF NEW.id IS NOT NULL THEN
        RAISE EXCEPTION 'activity_events.id is generated and cannot be supplied';
    END IF;
    NEW.id := nextval(pg_get_serial_sequence(TG_TABLE_SCHEMA || '.activity_events', 'id'));
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS activity_events_workspace_ordering ON activity_events;
CREATE TRIGGER activity_events_workspace_ordering
    BEFORE INSERT ON activity_events
    FOR EACH ROW
    EXECUTE FUNCTION lock_activity_workspace_and_assign_id();

CREATE OR REPLACE FUNCTION reject_activity_events_mutation()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION 'activity_events is append-only';
END;
$$;

DROP TRIGGER IF EXISTS activity_events_append_only ON activity_events;
CREATE TRIGGER activity_events_append_only
    BEFORE UPDATE OR DELETE ON activity_events
    FOR EACH ROW
    EXECUTE FUNCTION reject_activity_events_mutation();

DROP TRIGGER IF EXISTS activity_events_append_only_truncate ON activity_events;
CREATE TRIGGER activity_events_append_only_truncate
    BEFORE TRUNCATE ON activity_events
    FOR EACH STATEMENT
    EXECUTE FUNCTION reject_activity_events_mutation();

UPDATE schema_metadata SET schema_version = 10, updated_at = now() WHERE singleton;
