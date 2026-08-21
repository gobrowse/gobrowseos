-- M24 Batch 4: agent retrieval partial index + cheapest-capable model cost ranking.
--
-- 1. Partial GIN index supporting the build_messages implicit retrieval query
--    (run_api.rs build_messages). That query filters books by
--    security_classification / scope / kind / book_type and matches
--    search_document with websearch_to_tsquery. The partial predicate mirrors
--    those static filters so the planner can scan this index instead of the
--    whole books table for agent retrieval. (Plain CREATE INDEX is used because
--    sqlx wraps each migration in a transaction, where CONCURRENTLY is illegal;
--    the end result is the same index.)
CREATE INDEX books_agent_retrieval_idx ON books USING gin (search_document)
WHERE security_classification <> 'RESTRICTED'
  AND scope <> 'AGENT'
  AND scope NOT IN ('USER', 'PRIVATE')
  AND scope IN ('GLOBAL', 'PROFILE', 'WORKSPACE', 'PROJECT')
  AND (kind IS NULL OR kind IN ('SOURCE', 'SKILL', 'MCP', 'PLUGIN'))
  AND book_type <> 'AUTOBIOGRAPHY';

-- 2. Cost ranking used by load_routes / select_model_for_task to prefer the
--    cheapest equally-capable model as a tie-break. Primary model stays first;
--    a 0.0 default preserves existing behaviour when no ranking is set.
ALTER TABLE models ADD COLUMN IF NOT EXISTS cost_ranking real NOT NULL DEFAULT 0;

-- 3. Bump schema version.
UPDATE schema_metadata SET schema_version = 23, updated_at = now() WHERE singleton;
