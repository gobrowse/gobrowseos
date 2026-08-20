-- 1. Task-class model routing overrides (sits between primary model and fallback chain)
CREATE TABLE model_task_routes (
    id uuid PRIMARY KEY,
    profile_id uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    task_class text NOT NULL CHECK (task_class IN (
        'coding','research','data_analysis','document_creation',
        'general_qa','shell_automation','ecommerce','system_administration'
    )),
    preferred_model_id text NOT NULL REFERENCES models(id) ON DELETE CASCADE,
    position integer NOT NULL DEFAULT 0 CHECK (position >= 0),
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (profile_id, task_class, position),
    CHECK (position = 0)
    -- single preferred model per task class; fallback uses model_fallback_routes
);

-- 2. Book usage statistics (aggregate, maintained by trigger)
CREATE TABLE book_usage_stats (
    book_id uuid PRIMARY KEY REFERENCES books(id) ON DELETE CASCADE,
    total_searches bigint NOT NULL DEFAULT 0,
    total_loads bigint NOT NULL DEFAULT 0,
    total_tool_uses bigint NOT NULL DEFAULT 0,
    last_loaded_at timestamptz,
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- Trigger: update book_usage_stats from token_metrics events
CREATE OR REPLACE FUNCTION book_usage_from_metrics_fn() RETURNS trigger AS $$
DECLARE
    rec record;
BEGIN
    IF NEW.event_type = 'token_metrics' AND NEW.payload ? 'book_usage' THEN
        FOR rec IN SELECT * FROM jsonb_to_recordset(NEW.payload->'book_usage')
            AS x(book_id uuid, searches int, loads int, tool_uses int)
        LOOP
            INSERT INTO book_usage_stats (book_id, total_searches, total_loads, total_tool_uses, last_loaded_at)
            VALUES (rec.book_id, rec.searches, rec.loads, rec.tool_uses,
                    CASE WHEN rec.loads > 0 THEN now() ELSE NULL END)
            ON CONFLICT (book_id) DO UPDATE SET
                total_searches = book_usage_stats.total_searches + rec.searches,
                total_loads = book_usage_stats.total_loads + rec.loads,
                total_tool_uses = book_usage_stats.total_tool_uses + rec.tool_uses,
                last_loaded_at = CASE WHEN rec.loads > 0 THEN now() ELSE book_usage_stats.last_loaded_at END,
                updated_at = now();
        END LOOP;
    END IF;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

CREATE TRIGGER book_usage_from_metrics
AFTER INSERT ON run_events
FOR EACH ROW EXECUTE FUNCTION book_usage_from_metrics_fn();

-- 3. Indexes
CREATE INDEX model_task_routes_profile_task_idx ON model_task_routes (profile_id, task_class);

-- 4. Bump schema
UPDATE schema_metadata SET schema_version = 22, updated_at = now() WHERE singleton;
