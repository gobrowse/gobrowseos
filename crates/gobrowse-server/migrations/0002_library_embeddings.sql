-- Book authorization must be representable in data before retrieval can filter it.
ALTER TABLE books
    ADD COLUMN owner_user_id uuid REFERENCES users(id) ON DELETE CASCADE,
    ADD COLUMN owner_agent_id uuid REFERENCES agents(id) ON DELETE CASCADE,
    ADD COLUMN created_by_user_id uuid REFERENCES users(id) ON DELETE SET NULL;

-- V1 could not represent these owners safely. Quarantine ambiguous rows for administrators
-- instead of guessing an owner and disclosing private data.
UPDATE books
SET scope = 'PROFILE', security_classification = 'RESTRICTED'
WHERE scope IN ('USER', 'PRIVATE')
   OR scope = 'AGENT'
   OR (scope = 'PROJECT' AND workspace_id IS NULL);

ALTER TABLE books
    ADD CONSTRAINT books_user_scope_owner_check
        CHECK (scope NOT IN ('USER', 'PRIVATE') OR owner_user_id IS NOT NULL),
    ADD CONSTRAINT books_agent_scope_owner_check
        CHECK (scope <> 'AGENT' OR owner_agent_id IS NOT NULL),
    ADD CONSTRAINT books_project_scope_workspace_check
        CHECK (scope <> 'PROJECT' OR workspace_id IS NOT NULL);

WITH duplicate AS (
    SELECT id, row_number() OVER (
        PARTITION BY conversation_id ORDER BY created_at,id
    ) AS position
    FROM books WHERE book_type='CONVERSATION' AND conversation_id IS NOT NULL
)
UPDATE books AS book
SET book_type='SUMMARY',scope='PROFILE',conversation_id=NULL,
    security_classification='RESTRICTED',title=left(title || ' (quarantined duplicate)',512)
FROM duplicate WHERE duplicate.id=book.id AND duplicate.position>1;

ALTER TABLE books DROP CONSTRAINT books_conversation_id_fkey;
ALTER TABLE books
    ADD CONSTRAINT books_conversation_id_fkey
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE;
CREATE UNIQUE INDEX one_book_projection_per_conversation
    ON books (conversation_id) WHERE book_type = 'CONVERSATION';

-- Establish an immutable baseline for Books created before this migration.
INSERT INTO book_revisions (
    id, book_id, revision, title, body, tags, metadata, change_reason, created_at
)
SELECT gen_random_uuid(), id, revision, title, body, tags, metadata,
       'Schema v2 baseline snapshot', created_at
FROM books
ON CONFLICT (book_id, revision) DO NOTHING;

ALTER TABLE autobiography_proposals
    ADD COLUMN base_revision bigint,
    ADD COLUMN review_reason text;
UPDATE autobiography_proposals AS proposal
SET base_revision = book.revision
FROM books AS book
WHERE book.id = proposal.book_id;
ALTER TABLE autobiography_proposals
    ALTER COLUMN base_revision SET NOT NULL,
    ADD CONSTRAINT autobiography_proposals_base_revision_check CHECK (base_revision > 0);
UPDATE autobiography_proposals
SET status='rejected', review_reason='Proposal invalidated during schema v2 migration', reviewed_at=now()
WHERE status='pending';
CREATE UNIQUE INDEX one_pending_autobiography_proposal_per_profile
    ON autobiography_proposals (profile_id) WHERE status = 'pending';

-- Envelope encryption metadata. Ciphertext and wrapped data keys are never returned by APIs.
ALTER TABLE secret_references DROP CONSTRAINT secret_references_check;
ALTER TABLE secret_references
    ADD COLUMN purpose text NOT NULL DEFAULT 'provider_credential',
    ADD COLUMN allowed_hosts text[] NOT NULL DEFAULT '{}',
    ADD COLUMN algorithm text,
    ADD COLUMN wrapped_data_key bytea,
    ADD COLUMN wrap_nonce bytea;
UPDATE secret_references SET backend='legacy_encrypted_database'
WHERE backend='encrypted_database';
ALTER TABLE secret_references
    ADD CONSTRAINT secret_references_envelope_check CHECK (
        (backend = 'encrypted_database') =
        (encrypted_value IS NOT NULL AND nonce IS NOT NULL AND key_version IS NOT NULL
         AND algorithm IS NOT NULL AND wrapped_data_key IS NOT NULL AND wrap_nonce IS NOT NULL)
    );
CREATE UNIQUE INDEX secret_references_wrap_nonce_unique
    ON secret_references (key_version, wrap_nonce)
    WHERE backend = 'encrypted_database';
ALTER TABLE secret_references ADD CONSTRAINT secret_references_profile_id_unique UNIQUE (profile_id, id);
UPDATE providers AS provider SET secret_reference=NULL
WHERE secret_reference IS NOT NULL AND NOT EXISTS (
    SELECT 1 FROM secret_references secret
    WHERE secret.id=provider.secret_reference AND secret.profile_id=provider.profile_id
);
ALTER TABLE providers
    ADD CONSTRAINT providers_secret_reference_fk
    FOREIGN KEY (profile_id, secret_reference) REFERENCES secret_references(profile_id, id)
    ON DELETE SET NULL (secret_reference);

CREATE TABLE vault_key_state (
    profile_id uuid PRIMARY KEY REFERENCES profiles(id) ON DELETE CASCADE,
    current_key_version integer NOT NULL CHECK (current_key_version > 0),
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- V1 allowed duplicate provider/model references. Repoint dependents to one canonical ID
-- before adding the registry uniqueness constraint.
DO $$
DECLARE
    duplicate_model record;
BEGIN
    FOR duplicate_model IN
        SELECT id, first_value(id) OVER (
            PARTITION BY provider_id,model_reference ORDER BY id
        ) AS canonical_id
        FROM embedding_models
    LOOP
        IF duplicate_model.id <> duplicate_model.canonical_id THEN
            UPDATE books SET embedding_model_id=duplicate_model.canonical_id
            WHERE embedding_model_id=duplicate_model.id;
            UPDATE book_chunks SET embedding_model_id=duplicate_model.canonical_id
            WHERE embedding_model_id=duplicate_model.id;
            DELETE FROM embedding_jobs AS duplicate_job
            USING embedding_jobs AS canonical_job
            WHERE duplicate_job.embedding_model_id=duplicate_model.id
              AND canonical_job.embedding_model_id=duplicate_model.canonical_id
              AND duplicate_job.book_id=canonical_job.book_id
              AND duplicate_job.status=canonical_job.status;
            UPDATE embedding_jobs SET embedding_model_id=duplicate_model.canonical_id
            WHERE embedding_model_id=duplicate_model.id;
            DELETE FROM embedding_models WHERE id=duplicate_model.id;
        END IF;
    END LOOP;
END;
$$;

ALTER TABLE embedding_models
    ADD CONSTRAINT embedding_models_provider_reference_unique UNIQUE (provider_id, model_reference),
    ADD CONSTRAINT embedding_models_id_dimensions_unique UNIQUE (id, dimensions);
ALTER TABLE profiles
    ADD COLUMN active_embedding_model_id text REFERENCES embedding_models(id) ON DELETE SET NULL;

CREATE TABLE book_chunk_embeddings (
    chunk_id uuid NOT NULL REFERENCES book_chunks(id) ON DELETE CASCADE,
    embedding_model_id text NOT NULL,
    dimensions integer NOT NULL CHECK (dimensions > 0),
    book_revision bigint NOT NULL CHECK (book_revision > 0),
    embedding vector NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (chunk_id, embedding_model_id),
    FOREIGN KEY (embedding_model_id, dimensions)
        REFERENCES embedding_models(id, dimensions) ON DELETE CASCADE,
    CHECK (vector_dims(embedding) = dimensions)
);
CREATE INDEX book_chunk_embeddings_model_idx
    ON book_chunk_embeddings (embedding_model_id, chunk_id);

INSERT INTO book_chunk_embeddings
    (chunk_id, embedding_model_id, dimensions, book_revision, embedding, created_at)
SELECT chunk.id, chunk.embedding_model_id, model.dimensions, book.revision, chunk.embedding, chunk.created_at
FROM book_chunks chunk
JOIN books book ON book.id=chunk.book_id
JOIN embedding_models model ON model.id=chunk.embedding_model_id
WHERE chunk.embedding IS NOT NULL AND chunk.embedding_model_id IS NOT NULL
  AND vector_dims(chunk.embedding)=model.dimensions
ON CONFLICT DO NOTHING;

UPDATE profiles AS profile
SET active_embedding_model_id = candidate.embedding_model_id
FROM (
    SELECT profile_id, min(embedding_model_id) AS embedding_model_id
    FROM books
    WHERE embedding_model_id IS NOT NULL
    GROUP BY profile_id
    HAVING count(DISTINCT embedding_model_id)=1
) AS candidate
WHERE candidate.profile_id=profile.id;

UPDATE books AS book
SET embedding_status='stale'
WHERE embedding_status='ready' AND NOT EXISTS (
    SELECT 1 FROM book_chunks chunk
    JOIN book_chunk_embeddings embedding ON embedding.chunk_id=chunk.id
    WHERE chunk.book_id=book.id AND embedding.book_revision=book.revision
      AND embedding.embedding_model_id=book.embedding_model_id
);

ALTER TABLE book_chunks DROP COLUMN embedding, DROP COLUMN embedding_model_id;

ALTER TABLE embedding_jobs DROP CONSTRAINT embedding_jobs_status_check;
ALTER TABLE embedding_jobs DROP CONSTRAINT embedding_jobs_book_id_embedding_model_id_status_key;
ALTER TABLE embedding_jobs
    ADD COLUMN target_revision bigint,
    ADD COLUMN max_attempts integer NOT NULL DEFAULT 8 CHECK (max_attempts > 0),
    ADD COLUMN lease_owner text,
    ADD COLUMN lease_token uuid,
    ADD COLUMN lease_expires_at timestamptz,
    ADD COLUMN last_error_detail text,
    ADD COLUMN completed_at timestamptz,
    ADD CONSTRAINT embedding_jobs_status_check
        CHECK (status IN ('queued', 'running', 'retry', 'completed', 'failed', 'canceled'));
UPDATE embedding_jobs
SET status='retry', locked_at=NULL, available_at=now()
WHERE status='running';
ALTER TABLE embedding_jobs ADD CONSTRAINT embedding_jobs_lease_check CHECK (
    (status = 'running') =
    (lease_owner IS NOT NULL AND lease_token IS NOT NULL AND lease_expires_at IS NOT NULL)
);
UPDATE embedding_jobs AS job
SET target_revision = book.revision
FROM books AS book
WHERE book.id = job.book_id;
ALTER TABLE embedding_jobs ALTER COLUMN target_revision SET NOT NULL;
UPDATE embedding_jobs
SET status='failed',last_error_code='retry_limit',
    last_error_detail='Job exceeded retry limit before schema v2 migration',updated_at=now()
WHERE status IN ('queued','retry') AND attempts>=max_attempts;
WITH duplicate AS (
    SELECT id, row_number() OVER (
        PARTITION BY book_id, embedding_model_id, target_revision ORDER BY created_at, id
    ) AS position
    FROM embedding_jobs WHERE status IN ('queued','running','retry')
)
UPDATE embedding_jobs AS job
SET status='canceled', last_error_code='migration_duplicate',
    last_error_detail='Duplicate active job canceled during schema v2 migration', updated_at=now()
FROM duplicate WHERE duplicate.id=job.id AND duplicate.position>1;
CREATE UNIQUE INDEX one_active_embedding_job_per_revision
    ON embedding_jobs (book_id, embedding_model_id, target_revision)
    WHERE status IN ('queued', 'running', 'retry');
DROP INDEX embedding_jobs_queue_idx;
CREATE INDEX embedding_jobs_claim_idx
    ON embedding_jobs (available_at, created_at)
    WHERE status IN ('queued', 'retry');
CREATE INDEX embedding_jobs_expired_lease_idx
    ON embedding_jobs (lease_expires_at)
    WHERE status = 'running';

INSERT INTO embedding_jobs (id,book_id,embedding_model_id,target_revision)
SELECT gen_random_uuid(),book.id,profile.active_embedding_model_id,book.revision
FROM books book JOIN profiles profile ON profile.id=book.profile_id
WHERE book.embedding_status='stale' AND profile.active_embedding_model_id IS NOT NULL
ON CONFLICT DO NOTHING;

CREATE FUNCTION reject_book_revision_update() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'book revisions are immutable' USING ERRCODE = '55000';
END;
$$;
CREATE TRIGGER book_revisions_immutable
BEFORE UPDATE ON book_revisions
FOR EACH ROW EXECUTE FUNCTION reject_book_revision_update();

UPDATE schema_metadata SET schema_version = 2, updated_at = now() WHERE singleton;
