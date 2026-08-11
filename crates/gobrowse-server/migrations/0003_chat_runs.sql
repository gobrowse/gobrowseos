ALTER TABLE profiles
    ADD COLUMN active_chat_model_id text REFERENCES models(id) ON DELETE SET NULL;

CREATE TABLE workspace_memberships (
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    access text NOT NULL CHECK (access IN ('OWNER','EDITOR','VIEWER')),
    PRIMARY KEY (workspace_id,user_id)
);
CREATE INDEX workspace_memberships_user_idx ON workspace_memberships (user_id,workspace_id);

ALTER TABLE workspaces ADD COLUMN created_by_user_id uuid REFERENCES users(id) ON DELETE RESTRICT;
UPDATE workspaces AS workspace SET created_by_user_id=audit.actor_user_id
FROM audit_events AS audit WHERE audit.action='workspace.created' AND audit.resource_id=workspace.id::text
AND audit.actor_user_id IS NOT NULL;
UPDATE workspaces AS workspace SET created_by_user_id=(
    SELECT users.id FROM users WHERE users.primary_profile_id=workspace.profile_id
    ORDER BY (users.role='OWNER') DESC,(users.role='ADMIN') DESC,users.created_at,users.id LIMIT 1
) WHERE workspace.created_by_user_id IS NULL;
ALTER TABLE workspaces ALTER COLUMN created_by_user_id SET NOT NULL;
INSERT INTO workspace_memberships (workspace_id,user_id,access)
SELECT id,created_by_user_id,'OWNER' FROM workspaces ON CONFLICT DO NOTHING;

ALTER TABLE conversations ADD COLUMN created_by_user_id uuid REFERENCES users(id) ON DELETE RESTRICT;
UPDATE conversations AS conversation SET created_by_user_id=book.created_by_user_id
FROM books AS book WHERE book.conversation_id=conversation.id AND book.book_type='CONVERSATION';
UPDATE conversations AS conversation SET created_by_user_id=(
    SELECT users.id FROM users WHERE users.primary_profile_id=conversation.profile_id
    ORDER BY (users.role='OWNER') DESC,users.created_at,users.id LIMIT 1
) WHERE conversation.created_by_user_id IS NULL;
ALTER TABLE conversations ALTER COLUMN created_by_user_id SET NOT NULL;
CREATE INDEX conversations_owner_idx ON conversations (created_by_user_id,updated_at DESC);
INSERT INTO workspace_memberships (workspace_id,user_id,access)
SELECT DISTINCT workspace_id,created_by_user_id,'EDITOR' FROM conversations
WHERE workspace_id IS NOT NULL ON CONFLICT DO NOTHING;
INSERT INTO workspace_memberships (workspace_id,user_id,access)
SELECT DISTINCT workspace_id,coalesce(owner_user_id,created_by_user_id),'EDITOR' FROM books
WHERE workspace_id IS NOT NULL AND coalesce(owner_user_id,created_by_user_id) IS NOT NULL
ON CONFLICT DO NOTHING;

CREATE TABLE model_fallback_routes (
    profile_id uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    primary_model_id text NOT NULL REFERENCES models(id) ON DELETE CASCADE,
    fallback_model_id text NOT NULL REFERENCES models(id) ON DELETE CASCADE,
    position integer NOT NULL CHECK (position >= 0),
    PRIMARY KEY (profile_id,primary_model_id,fallback_model_id),
    UNIQUE (profile_id,primary_model_id,position),
    CHECK (primary_model_id <> fallback_model_id)
);

ALTER TABLE agent_runs ALTER COLUMN agent_id DROP NOT NULL;
ALTER TABLE agent_runs
    ADD COLUMN profile_id uuid REFERENCES profiles(id) ON DELETE CASCADE,
    ADD COLUMN requested_by uuid REFERENCES users(id) ON DELETE SET NULL,
    ADD COLUMN requested_model_id text REFERENCES models(id) ON DELETE SET NULL,
    ADD COLUMN selected_model_id text REFERENCES models(id) ON DELETE SET NULL,
    ADD COLUMN input_message_id uuid REFERENCES messages(id) ON DELETE SET NULL,
    ADD COLUMN output_message_id uuid REFERENCES messages(id) ON DELETE SET NULL,
    ADD COLUMN context_snapshot jsonb,
    ADD COLUMN error_code text,
    ADD COLUMN error_detail text,
    ADD COLUMN execution_owner text,
    ADD COLUMN execution_token uuid,
    ADD COLUMN lease_expires_at timestamptz,
    ADD COLUMN execution_attempts integer NOT NULL DEFAULT 0 CHECK (execution_attempts >= 0),
    ADD COLUMN client_submission_id uuid,
    ADD COLUMN request_fingerprint bytea,
    ADD COLUMN run_kind text NOT NULL DEFAULT 'agent' CHECK (run_kind IN ('agent','conversation_turn'));

UPDATE agent_runs AS run
SET profile_id=workspace.profile_id
FROM agents agent JOIN workspaces workspace ON workspace.id=agent.workspace_id
WHERE agent.id=run.agent_id AND run.profile_id IS NULL;
UPDATE agent_runs AS run
SET profile_id=conversation.profile_id
FROM conversations conversation
WHERE conversation.id=run.conversation_id AND run.profile_id IS NULL;
UPDATE agent_runs
SET state='failed',error_code='legacy_state',error_detail='Legacy run state was not recognized',finished_at=now()
WHERE state NOT IN (
    'queued','building_context','awaiting_model','awaiting_policy','awaiting_approval',
    'running_tool','paused','completed','failed','canceled'
);

ALTER TABLE agent_runs
    ALTER COLUMN profile_id SET NOT NULL,
    ADD CONSTRAINT agent_runs_lease_fields_check CHECK (
        (execution_owner IS NULL AND execution_token IS NULL AND lease_expires_at IS NULL) OR
        (execution_owner IS NOT NULL AND execution_token IS NOT NULL AND lease_expires_at IS NOT NULL)
    ),
    ADD CONSTRAINT agent_runs_terminal_lease_check CHECK (
        state NOT IN ('completed','failed','canceled') OR
        (execution_owner IS NULL AND execution_token IS NULL AND lease_expires_at IS NULL)
    ),
    ADD CONSTRAINT agent_runs_state_check CHECK (state IN (
        'queued','building_context','awaiting_model','awaiting_policy','awaiting_approval',
        'running_tool','paused','completed','failed','canceled'
    ));
CREATE UNIQUE INDEX one_run_per_input_message
    ON agent_runs (input_message_id) WHERE input_message_id IS NOT NULL;
CREATE INDEX agent_runs_profile_created_idx ON agent_runs (profile_id,created_at DESC);
CREATE UNIQUE INDEX one_conversation_submission ON agent_runs (conversation_id,requested_by,client_submission_id)
    WHERE run_kind='conversation_turn' AND client_submission_id IS NOT NULL;
CREATE UNIQUE INDEX one_active_conversation_turn ON agent_runs (conversation_id)
    WHERE run_kind='conversation_turn' AND state NOT IN ('completed','failed','canceled');
CREATE INDEX agent_runs_claim_idx ON agent_runs (coalesce(lease_expires_at,created_at),created_at,id)
    WHERE run_kind='conversation_turn' AND state NOT IN ('completed','failed','canceled');

ALTER TABLE agent_runs DROP CONSTRAINT agent_runs_conversation_id_fkey;
ALTER TABLE agent_runs
    ADD CONSTRAINT agent_runs_conversation_id_fkey FOREIGN KEY (conversation_id)
    REFERENCES conversations(id) ON DELETE SET NULL;

ALTER TABLE messages ADD COLUMN agent_run_id uuid;
ALTER TABLE messages
    ADD CONSTRAINT messages_agent_run_fk FOREIGN KEY (agent_run_id) REFERENCES agent_runs(id) ON DELETE SET NULL;
CREATE UNIQUE INDEX one_output_message_per_run ON messages (agent_run_id) WHERE agent_run_id IS NOT NULL;

ALTER TABLE run_events ADD COLUMN profile_id uuid REFERENCES profiles(id) ON DELETE CASCADE;
UPDATE run_events AS event SET profile_id=run.profile_id FROM agent_runs AS run WHERE run.id=event.run_id;
ALTER TABLE run_events ALTER COLUMN profile_id SET NOT NULL;
CREATE INDEX run_events_profile_replay_idx ON run_events (profile_id,sequence);

UPDATE schema_metadata SET schema_version=3,updated_at=now() WHERE singleton;
