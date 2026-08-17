-- 0019_conversation_pinned_books.sql
-- Add pinning (Library book → conversation) join table with FK cascade.

CREATE TABLE conversation_pinned_books (
    conversation_id uuid NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    book_id uuid NOT NULL REFERENCES books(id) ON DELETE CASCADE,
    pinned_by uuid REFERENCES users(id) ON DELETE SET NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (conversation_id, book_id)
);

CREATE INDEX conversation_pinned_books_conversation_idx
    ON conversation_pinned_books (conversation_id, created_at);

UPDATE schema_metadata SET schema_version = 19, updated_at = now() WHERE singleton;
