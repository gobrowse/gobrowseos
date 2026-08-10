CREATE EXTENSION IF NOT EXISTS vector;
CREATE EXTENSION IF NOT EXISTS pgcrypto;

CREATE TABLE schema_metadata (
    singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
    schema_version bigint NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);
INSERT INTO schema_metadata (schema_version) VALUES (1);

CREATE TABLE profiles (
    id uuid PRIMARY KEY,
    name text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 200),
    autobiography_update_policy text NOT NULL DEFAULT 'propose'
        CHECK (autobiography_update_policy IN ('manual', 'propose', 'automatic')),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE users (
    id uuid PRIMARY KEY,
    email text NOT NULL,
    display_name text NOT NULL CHECK (char_length(display_name) BETWEEN 1 AND 200),
    password_hash text NOT NULL,
    role text NOT NULL CHECK (role IN ('OWNER', 'ADMIN', 'MEMBER', 'VIEWER')),
    primary_profile_id uuid NOT NULL REFERENCES profiles(id) ON DELETE RESTRICT,
    auth_epoch bigint NOT NULL DEFAULT 1,
    disabled_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE UNIQUE INDEX users_email_lower_unique ON users (lower(email));

CREATE TABLE sessions (
    token_hash bytea PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    auth_epoch bigint NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    last_seen_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL,
    absolute_expires_at timestamptz NOT NULL,
    ip_hash bytea,
    user_agent_hash bytea
);
CREATE INDEX sessions_expiry_idx ON sessions (expires_at);

CREATE TABLE workspaces (
    id uuid PRIMARY KEY,
    profile_id uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    title text NOT NULL CHECK (char_length(title) BETWEEN 1 AND 300),
    description text NOT NULL DEFAULT '',
    model_preference text,
    network_policy text NOT NULL DEFAULT 'RESTRICTED'
        CHECK (network_policy IN ('NONE', 'RESTRICTED', 'FULL')),
    sandbox_policy jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX workspaces_profile_idx ON workspaces (profile_id, updated_at DESC);

CREATE TABLE conversations (
    id uuid PRIMARY KEY,
    profile_id uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    workspace_id uuid REFERENCES workspaces(id) ON DELETE SET NULL,
    title text NOT NULL CHECK (char_length(title) BETWEEN 1 AND 500),
    status text NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'archived', 'deleted')),
    forked_from_id uuid REFERENCES conversations(id) ON DELETE SET NULL,
    forked_at_message_id uuid,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX conversations_profile_updated_idx ON conversations (profile_id, updated_at DESC);

CREATE TABLE messages (
    id uuid PRIMARY KEY,
    conversation_id uuid NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    ordinal bigint NOT NULL,
    role text NOT NULL CHECK (role IN ('system', 'user', 'assistant', 'tool')),
    content jsonb NOT NULL,
    provider text,
    model text,
    usage jsonb,
    tool_calls jsonb,
    reasoning_metadata jsonb,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (conversation_id, ordinal)
);
CREATE INDEX messages_content_fts_idx ON messages USING gin
    (to_tsvector('english', coalesce(content->>'text', '')));

ALTER TABLE conversations
    ADD CONSTRAINT conversations_fork_message_fk
    FOREIGN KEY (forked_at_message_id) REFERENCES messages(id) ON DELETE SET NULL;

CREATE TABLE books (
    id uuid PRIMARY KEY,
    profile_id uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    title text NOT NULL CHECK (char_length(title) BETWEEN 1 AND 512),
    body text NOT NULL,
    book_type text NOT NULL CHECK (book_type IN
        ('NOTE', 'DOCUMENT', 'CONVERSATION', 'PROJECT', 'AUTOBIOGRAPHY', 'SUMMARY', 'INSTRUCTION', 'IMPORTED')),
    scope text NOT NULL CHECK (scope IN
        ('GLOBAL', 'USER', 'PROFILE', 'WORKSPACE', 'PROJECT', 'CONVERSATION', 'AGENT', 'PRIVATE')),
    tags text[] NOT NULL DEFAULT '{}',
    provenance text NOT NULL CHECK (provenance IN
        ('USER', 'CONVERSATION', 'AGENT', 'FILE', 'WEB', 'MCP', 'IMPORT', 'SKILL', 'SYSTEM')),
    trust text NOT NULL CHECK (trust IN
        ('VERIFIED', 'USER_PROVIDED', 'AGENT_INFERRED', 'EXTERNAL', 'UNTRUSTED')),
    source jsonb NOT NULL DEFAULT '{}'::jsonb,
    author text NOT NULL,
    workspace_id uuid REFERENCES workspaces(id) ON DELETE CASCADE,
    conversation_id uuid REFERENCES conversations(id) ON DELETE SET NULL,
    security_classification text NOT NULL CHECK (security_classification IN
        ('PUBLIC', 'INTERNAL', 'CONFIDENTIAL', 'RESTRICTED')),
    embedding_status text NOT NULL DEFAULT 'pending' CHECK (embedding_status IN
        ('pending', 'processing', 'ready', 'failed', 'stale')),
    embedding_model_id text,
    metadata jsonb NOT NULL DEFAULT '{}'::jsonb,
    revision bigint NOT NULL DEFAULT 1 CHECK (revision > 0),
    search_document tsvector GENERATED ALWAYS AS
        (setweight(to_tsvector('english', coalesce(title, '')), 'A') ||
         setweight(to_tsvector('english', coalesce(body, '')), 'B')) STORED,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CHECK (book_type <> 'AUTOBIOGRAPHY' OR char_length(body) <= 99999),
    CHECK (scope <> 'WORKSPACE' OR workspace_id IS NOT NULL),
    CHECK (scope <> 'CONVERSATION' OR conversation_id IS NOT NULL)
);
CREATE UNIQUE INDEX one_autobiography_per_profile ON books (profile_id)
    WHERE book_type = 'AUTOBIOGRAPHY';
CREATE INDEX books_profile_updated_idx ON books (profile_id, updated_at DESC);
CREATE INDEX books_workspace_updated_idx ON books (workspace_id, updated_at DESC);
CREATE INDEX books_tags_idx ON books USING gin (tags);
CREATE INDEX books_fts_idx ON books USING gin (search_document);

CREATE TABLE book_revisions (
    id uuid PRIMARY KEY,
    book_id uuid NOT NULL REFERENCES books(id) ON DELETE CASCADE,
    revision bigint NOT NULL,
    title text NOT NULL,
    body text NOT NULL,
    tags text[] NOT NULL,
    metadata jsonb NOT NULL,
    changed_by uuid REFERENCES users(id) ON DELETE SET NULL,
    change_reason text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (book_id, revision)
);

CREATE TABLE book_chunks (
    id uuid PRIMARY KEY,
    book_id uuid NOT NULL REFERENCES books(id) ON DELETE CASCADE,
    ordinal integer NOT NULL CHECK (ordinal >= 0),
    text text NOT NULL,
    token_estimate integer NOT NULL CHECK (token_estimate >= 0),
    source_start integer NOT NULL CHECK (source_start >= 0),
    source_end integer NOT NULL CHECK (source_end >= source_start),
    embedding vector,
    embedding_model_id text,
    metadata jsonb NOT NULL DEFAULT '{}'::jsonb,
    search_document tsvector GENERATED ALWAYS AS (to_tsvector('english', coalesce(text, ''))) STORED,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (book_id, ordinal)
);
CREATE INDEX book_chunks_book_idx ON book_chunks (book_id, ordinal);
CREATE INDEX book_chunks_fts_idx ON book_chunks USING gin (search_document);

CREATE TABLE book_links (
    from_book_id uuid NOT NULL REFERENCES books(id) ON DELETE CASCADE,
    to_book_id uuid NOT NULL REFERENCES books(id) ON DELETE CASCADE,
    relation text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (from_book_id, to_book_id, relation),
    CHECK (from_book_id <> to_book_id)
);

CREATE TABLE autobiography_proposals (
    id uuid PRIMARY KEY,
    profile_id uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    book_id uuid NOT NULL REFERENCES books(id) ON DELETE CASCADE,
    before_body text NOT NULL,
    after_body text NOT NULL CHECK (char_length(after_body) <= 99999),
    reason text NOT NULL,
    source_book_ids uuid[] NOT NULL DEFAULT '{}',
    source_conversation_ids uuid[] NOT NULL DEFAULT '{}',
    status text NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'accepted', 'rejected')),
    proposed_by_agent_id uuid,
    reviewed_by uuid REFERENCES users(id) ON DELETE SET NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    reviewed_at timestamptz
);

CREATE TABLE providers (
    id text PRIMARY KEY,
    profile_id uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    provider_type text NOT NULL,
    display_name text NOT NULL,
    base_url text,
    secret_reference text,
    enabled boolean NOT NULL DEFAULT true,
    configuration jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE models (
    id text PRIMARY KEY,
    provider_id text NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
    model_reference text NOT NULL,
    display_name text NOT NULL,
    context_window integer NOT NULL CHECK (context_window > 0),
    output_limit integer NOT NULL CHECK (output_limit > 0),
    capabilities text[] NOT NULL,
    enabled boolean NOT NULL DEFAULT true,
    priority integer NOT NULL DEFAULT 0,
    cost_metadata jsonb,
    UNIQUE (provider_id, model_reference)
);

CREATE TABLE embedding_models (
    id text PRIMARY KEY,
    provider_id text NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
    model_reference text NOT NULL,
    dimensions integer NOT NULL CHECK (dimensions > 0),
    enabled boolean NOT NULL DEFAULT true
);

CREATE TABLE embedding_jobs (
    id uuid PRIMARY KEY,
    book_id uuid NOT NULL REFERENCES books(id) ON DELETE CASCADE,
    embedding_model_id text NOT NULL REFERENCES embedding_models(id) ON DELETE RESTRICT,
    status text NOT NULL DEFAULT 'queued' CHECK (status IN ('queued', 'running', 'completed', 'failed', 'canceled')),
    attempts integer NOT NULL DEFAULT 0,
    available_at timestamptz NOT NULL DEFAULT now(),
    locked_at timestamptz,
    last_error_code text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (book_id, embedding_model_id, status)
);
CREATE INDEX embedding_jobs_queue_idx ON embedding_jobs (available_at) WHERE status = 'queued';

CREATE TABLE tasks (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    parent_task_id uuid REFERENCES tasks(id) ON DELETE SET NULL,
    title text NOT NULL,
    description text NOT NULL DEFAULT '',
    state text NOT NULL CHECK (state IN ('BACKLOG','READY','RUNNING','BLOCKED','REVIEW','DONE','FAILED','CANCELED')),
    assigned_agent_id uuid,
    dependencies uuid[] NOT NULL DEFAULT '{}',
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE agents (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    parent_agent_id uuid REFERENCES agents(id) ON DELETE SET NULL,
    name text NOT NULL,
    kind text NOT NULL,
    permissions jsonb NOT NULL,
    model_id text REFERENCES models(id) ON DELETE SET NULL,
    status text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
ALTER TABLE tasks ADD CONSTRAINT tasks_agent_fk FOREIGN KEY (assigned_agent_id) REFERENCES agents(id) ON DELETE SET NULL;

CREATE TABLE agent_runs (
    id uuid PRIMARY KEY,
    agent_id uuid NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    task_id uuid REFERENCES tasks(id) ON DELETE SET NULL,
    conversation_id uuid REFERENCES conversations(id) ON DELETE SET NULL,
    state text NOT NULL,
    step integer NOT NULL DEFAULT 0,
    cancellation_requested_at timestamptz,
    started_at timestamptz,
    finished_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE run_events (
    sequence bigserial PRIMARY KEY,
    run_id uuid NOT NULL REFERENCES agent_runs(id) ON DELETE CASCADE,
    event_type text NOT NULL,
    payload jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX run_events_replay_idx ON run_events (run_id, sequence);

CREATE TABLE tool_calls (
    id uuid PRIMARY KEY,
    run_id uuid NOT NULL REFERENCES agent_runs(id) ON DELETE CASCADE,
    tool_id text NOT NULL,
    risk_class text NOT NULL,
    input jsonb NOT NULL,
    output jsonb,
    status text NOT NULL,
    idempotency_key text NOT NULL UNIQUE,
    started_at timestamptz,
    finished_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE approvals (
    id uuid PRIMARY KEY,
    tool_call_id uuid NOT NULL UNIQUE REFERENCES tool_calls(id) ON DELETE CASCADE,
    status text NOT NULL CHECK (status IN ('pending', 'approved', 'denied', 'expired')),
    requested_reason text NOT NULL,
    decided_by uuid REFERENCES users(id) ON DELETE SET NULL,
    decided_at timestamptz,
    expires_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE worktrees (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    task_id uuid NOT NULL REFERENCES tasks(id) ON DELETE RESTRICT,
    owner_agent_id uuid NOT NULL REFERENCES agents(id) ON DELETE RESTRICT,
    branch text NOT NULL,
    base_commit text NOT NULL,
    path text NOT NULL,
    status text NOT NULL,
    changed_files text[] NOT NULL DEFAULT '{}',
    last_activity_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (workspace_id, path),
    UNIQUE (workspace_id, branch)
);

CREATE TABLE activity_events (
    id bigserial PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    task_id uuid REFERENCES tasks(id) ON DELETE SET NULL,
    agent_id uuid REFERENCES agents(id) ON DELETE SET NULL,
    kind text NOT NULL,
    payload jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX activity_workspace_replay_idx ON activity_events (workspace_id, id);

CREATE TABLE skills (
    id uuid PRIMARY KEY,
    profile_id uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    workspace_id uuid REFERENCES workspaces(id) ON DELETE CASCADE,
    name text NOT NULL,
    description text NOT NULL,
    active_revision bigint,
    promotion_policy text NOT NULL DEFAULT 'propose' CHECK (promotion_policy IN ('manual','propose','automatic')),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (profile_id, workspace_id, name)
);

CREATE TABLE skill_revisions (
    id uuid PRIMARY KEY,
    skill_id uuid NOT NULL REFERENCES skills(id) ON DELETE CASCADE,
    revision bigint NOT NULL,
    content text NOT NULL,
    author text NOT NULL,
    reason text NOT NULL,
    source_conversation_ids uuid[] NOT NULL DEFAULT '{}',
    evaluation jsonb,
    promoted boolean NOT NULL DEFAULT false,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (skill_id, revision)
);

CREATE TABLE secret_references (
    id text PRIMARY KEY,
    profile_id uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    backend text NOT NULL,
    locator text NOT NULL,
    encrypted_value bytea,
    nonce bytea,
    key_version integer,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CHECK ((backend = 'encrypted_database') = (encrypted_value IS NOT NULL AND nonce IS NOT NULL))
);

CREATE TABLE mcp_servers (
    id uuid PRIMARY KEY,
    profile_id uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    name text NOT NULL,
    transport text NOT NULL CHECK (transport IN ('stdio', 'streamable_http')),
    configuration jsonb NOT NULL,
    auth_secret_reference text REFERENCES secret_references(id) ON DELETE SET NULL,
    enabled boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE mcp_auth_states (
    id uuid PRIMARY KEY,
    mcp_server_id uuid NOT NULL REFERENCES mcp_servers(id) ON DELETE CASCADE,
    state_hash bytea NOT NULL UNIQUE,
    pkce_verifier_encrypted bytea NOT NULL,
    expected_issuer text NOT NULL,
    resource_uri text NOT NULL,
    redirect_uri text NOT NULL,
    expires_at timestamptz NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE scheduled_jobs (
    id uuid PRIMARY KEY,
    profile_id uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    workspace_id uuid REFERENCES workspaces(id) ON DELETE CASCADE,
    name text NOT NULL,
    schedule jsonb NOT NULL,
    action jsonb NOT NULL,
    enabled boolean NOT NULL DEFAULT true,
    last_run_at timestamptz,
    next_run_at timestamptz,
    last_status text,
    last_error_code text,
    last_duration_ms bigint,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX scheduled_jobs_due_idx ON scheduled_jobs (next_run_at) WHERE enabled;

CREATE TABLE webhooks (
    id uuid PRIMARY KEY,
    profile_id uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    name text NOT NULL,
    secret_reference text NOT NULL REFERENCES secret_references(id) ON DELETE RESTRICT,
    target jsonb NOT NULL,
    event_filter jsonb NOT NULL,
    enabled boolean NOT NULL DEFAULT true,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE webhook_deliveries (
    webhook_id uuid NOT NULL REFERENCES webhooks(id) ON DELETE CASCADE,
    delivery_id text NOT NULL,
    received_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (webhook_id, delivery_id)
);

CREATE TABLE terminal_sessions (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    sandbox_instance_id text NOT NULL,
    process_id text,
    cols integer NOT NULL CHECK (cols > 0),
    rows integer NOT NULL CHECK (rows > 0),
    status text NOT NULL,
    exit_code integer,
    last_output_sequence bigint NOT NULL DEFAULT 0,
    created_at timestamptz NOT NULL DEFAULT now(),
    last_activity_at timestamptz NOT NULL DEFAULT now(),
    terminated_at timestamptz
);

CREATE TABLE checkpoints (
    id uuid PRIMARY KEY,
    workspace_id uuid NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
    task_id uuid REFERENCES tasks(id) ON DELETE SET NULL,
    kind text NOT NULL,
    reference text NOT NULL,
    metadata jsonb NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE audit_events (
    sequence bigserial PRIMARY KEY,
    actor_user_id uuid REFERENCES users(id) ON DELETE SET NULL,
    profile_id uuid REFERENCES profiles(id) ON DELETE SET NULL,
    action text NOT NULL,
    resource_type text NOT NULL,
    resource_id text,
    outcome text NOT NULL,
    detail jsonb NOT NULL DEFAULT '{}'::jsonb,
    request_id uuid,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX audit_events_profile_idx ON audit_events (profile_id, sequence DESC);
