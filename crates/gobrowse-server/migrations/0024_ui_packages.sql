-- 0024_ui_packages.sql
-- Custom UI System: UI packages, assets, capabilities, profile permission level.
-- Schema 23 -> 24.

-- 1. UI permission level on profiles.
ALTER TABLE profiles ADD COLUMN ui_permission_level text NOT NULL DEFAULT 'ASK'
    CHECK (ui_permission_level IN ('DENY', 'ASK', 'ALLOW_WORKSPACE', 'ALLOW_GLOBAL'));

-- 2. UI packages (analogous to plugins table).
CREATE TABLE ui_packages (
    id uuid PRIMARY KEY,
    profile_id uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    workspace_id uuid REFERENCES workspaces(id) ON DELETE CASCADE,
    name text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 200),
    version text NOT NULL CHECK (char_length(version) BETWEEN 1 AND 64),
    ui_kind text NOT NULL CHECK (ui_kind IN ('THEME', 'FULL_UI')),
    description text NOT NULL DEFAULT '',
    source_type text NOT NULL CHECK (source_type IN ('github_release', 'local_package', 'marketplace')),
    source_uri text NOT NULL,
    manifest jsonb NOT NULL DEFAULT '{}'::jsonb,
    artifact_digest text,
    install_path text,
    entry_point text,
    api_version text NOT NULL DEFAULT 'v1',
    state text NOT NULL DEFAULT 'discovered'
        CHECK (state IN ('discovered','staged','validated','candidate','active','previous','failed','rolled_back')),
    trust text NOT NULL DEFAULT 'UNTRUSTED'
        CHECK (trust IN ('VERIFIED','USER_PROVIDED','AGENT_INFERRED','EXTERNAL','UNTRUSTED')),
    created_by uuid REFERENCES users(id) ON DELETE SET NULL,
    activated_by uuid REFERENCES users(id) ON DELETE SET NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    activated_at timestamptz,
    UNIQUE (profile_id, name)
);

-- Only one active UI package per profile at a time.
CREATE UNIQUE INDEX ui_packages_one_active_per_profile
    ON ui_packages (profile_id) WHERE state = 'active';

-- 3. Asset hashes for CSP computation.
CREATE TABLE ui_package_assets (
    id uuid PRIMARY KEY,
    ui_package_id uuid NOT NULL REFERENCES ui_packages(id) ON DELETE CASCADE,
    file_path text NOT NULL,
    content_type text NOT NULL,
    sha256_hash text NOT NULL,
    file_size bigint NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (ui_package_id, file_path)
);

-- 4. Declared capabilities (from manifest, informational).
CREATE TABLE ui_package_capabilities (
    id uuid PRIMARY KEY,
    ui_package_id uuid NOT NULL REFERENCES ui_packages(id) ON DELETE CASCADE,
    capability_name text NOT NULL,
    min_api_version text,
    UNIQUE (ui_package_id, capability_name)
);

-- 5. Indexes.
CREATE INDEX ui_packages_profile_state_idx ON ui_packages (profile_id, state);
CREATE INDEX ui_package_assets_package_idx ON ui_package_assets (ui_package_id);

-- 6. Companion Book: add GOBROWSE_UI to books_kind_check.
ALTER TABLE books DROP CONSTRAINT IF EXISTS books_kind_check;
ALTER TABLE books ADD CONSTRAINT books_kind_check
    CHECK (kind IS NULL OR kind IN ('SOURCE','SKILL','MCP','PLUGIN','AUTOBIOGRAPHY','GOBROWSE_UI'));

-- 7. Bump schema.
UPDATE schema_metadata SET schema_version = 24, updated_at = now() WHERE singleton;
