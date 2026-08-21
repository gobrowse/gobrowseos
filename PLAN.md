# PLAN.md — M22: Unified Library + Plugin System Architecture

Authoritative architecture and lane contracts for M22. Prior M4/M7/M8/M10 external blocker records are
preserved and out of scope. This batch covers *architecture design + shared lane contracts only*; zero
implementation.

## Goal

Implement a unified retrieval-driven Library, a sandboxed Plugin system, and on-demand capability
loading, per the M22 spec. Consolidate Source/Skill/MCP/Plugin Books into one searchable capability
registry, indexed by compact per-Book metadata. Plugins are sandboxed by default; activation is
on-demand and transient. The sandbox daemon is wired into the server for agent tool use.

## Current State (observed facts — key excerpts)

### Schema (version 19)

*   `books` table (migration 0001) already has: `id`, `profile_id`, `title`, `body`, `book_type`
    (`NOTE|DOCUMENT|CONVERSATION|PROJECT|AUTOBIOGRAPHY|SUMMARY|INSTRUCTION|IMPORTED`), `scope`, `tags`,
    `provenance`, `trust`, `source jsonb`, `author`, `workspace_id`, `conversation_id`,
    `security_classification`, `embedding_status`, `embedding_model_id`, `metadata jsonb`, `revision`,
    `search_document tsvector`, timestamps.
*   `book_links` table already exists: `(from_book_id, to_book_id, relation)` PK. The `relation`
    column is free text with no CHECK today; the spec vocabulary is
    `PROVIDES|CONTAINS|REFERENCES|DEPENDS_ON|REQUIRES|RELATED_TO|SUPERSEDES|OWNED_BY` (to be
    enforced at the API layer, or a CHECK added in migration 20) — reusable for Plugin→child
    relationships.
*   `skills` table (0001): `profile_id`, `workspace_id`, `name`, `description`, `active_revision`,
    `promotion_policy` + `skill_revisions` (content/author/reason/evaluation/promoted).
    `skills_api.rs` has `list_skills`, `create_skill`, `create_revision`, `propose_revision`,
    `history`, `evaluate`, `evaluate_skill`, `promote`, `promote_skill`, `rollback`.
*   `mcp_servers` table (0001): `profile_id`, `name`, `transport` (`stdio|streamable_http`),
    `configuration jsonb`, `auth_secret_reference`, `enabled`. `mcp_api.rs` has `list`, `create`,
    `update`, `delete`.
*   `gobrowse_core::sandbox` is COMPLETE: protocol v2 with `RequestEnvelope` (`version`, `request_id`,
    `token`), `SandboxOperation` enum (Health, Start, Input, ReadOutput, AckOutput, Resize,
    Interrupt, Processes, Kill, Terminate, Inspect, Reconnect, FsList, FsRead, FsMetadata, FsSearch,
    FsWrite, FsPatch, FsMkdir, FsMove, FsCopy, FsDelete), `ResponseEnvelope` (version, request_id,
    `Result<SandboxResult, SandboxProtocolError>`), `SandboxErrorCode` enum (InvalidRequest,
    UnsupportedVersion, Unauthorized, NotFound, PolicyDenied, LimitExceeded, DeadlineExceeded,
    Conflict, OutcomeUnknown, Internal), `ResourceLimits`, `HARD_RESOURCE_LIMITS`,
    `NetworkPolicy { None, Restricted, Full }`, `workspace_volume_name(workspace_id)`,
    path validation, `RestrictedNetworkAttestation`, bounded constants.
*   `gobrowse-sandboxd` is COMPLETE: standalone daemon with `Daemon`, `DaemonConfig`, `Authenticator`,
    `ConnectionConfig`, `PodmanConfig`, `PodmanRuntime`, `SessionLimits`, `Filesystem`,
    `TerminalJournal`, `SocketConfig`. CLI takes `--socket`, `--workspace-root`,
    `--auth-token-file`, `--socket-mode`, `--allowed-peer-uid`, `--deployment-id`, `--podman`,
    `--image`, `--restricted-network`, `--allow-full-network`, resource limits, session limits.
    Unit-tested with real rootless Podman. **NOT wired into server** (zero client code; doctor.rs
    only reports `features.sandbox` as enabled/disabled).
*   Server routes (lib.rs): workspaces, conversations, worktrees, tasks, activity, library books
    + search + history, skills CRUD + revisions, MCP servers, autobiography, models, chat, providers
    catalog, usage summary, runs/turns/events/cancel, vault secrets, webhooks, embedding configs
    + retry, realtime upgrade. AppState: `pool`, `settings`, `passwords`, `vault`, `run_cancellations`.
    TimeoutLayer 30 s.
*   Chat tool runner: `run_tools.rs` provides two native tools (`library_search`, `library_add`) via
    `impl Tool`; `run_api.rs` has a bounded agent tool loop (max 8 rounds, 16 calls, 10 s timeout,
    64 KiB output cap); `chat.rs` supports `ToolDefinition` wire-format in requests. Model routes
    already have `supports_tools: bool`.
*   Context assembly: `run_api.rs::build_messages` assembles `PinnedBook` (priority 400),
    `LibraryRetrieval` (300), `Worktree` (250) candidates into a token-budgeted context.
    `ContextSource` enum already has `Skill`, `ToolSchema`, `CompactionSummary` variants (unused
    for Skill/MCP assembly today).

### Constraints and invariants

*   Schema version **19**; two test assertions at `postgres_integration.rs:42` and
    `worktrees_integration.rs:50` assert `schema_version == 19` — must become 20.
*   `unsafe_code = "forbid"`, edition 2024, Rust 1.94, nextest, forward migrations.
*   `book_links` already has `PROVIDES|CONTAINS|REFERENCES|DEPENDS_ON|REQUIRES|RELATED_TO|SUPERSEDES|OWNED_BY`.
*   No existing `impl Tool` for sandbox, terminal, or MCP operations.
*   Sandbox daemon requires `--pull=never` with immutable image; no fallback to pull on demand.
*   `features.sandbox` defaults `false`; prod (178.128.179.216) has Docker 29, no podman
    (apt-installable). Local dev has rootless podman 5.7.0 (uid 1000, subuid 100000:65536).

## Architectural Decisions

### 1. Book kind is a new `kind` column, NOT a `book_type` sub-type

`book_type` describes *content format* (Note, Document, etc.). `kind` describes the *registry role*:
`SOURCE` (existing books), `SKILL`, `MCP`, `PLUGIN`, `AUTOBIOGRAPHY`. Both columns coexist. No
existing `book_type` values change. The `books` table becomes the unified index.

`kind` is nullable with `NULL` treated as `'SOURCE'` (the default for all existing rows);
backfill is `UPDATE books SET kind = 'AUTOBIOGRAPHY' WHERE book_type = 'AUTOBIOGRAPHY'`.
Skills and MCP servers get companion Book rows created on `create_skill`/`create_mcp_server`.

### 2. Skill and MCP Books are index-proxies, not duplicates

Skill/MCP companion Books carry: `id = <skill|mcp_server>_id + stable UUID`, `title = name`,
`body = description` (compact, ≤ 500 chars), `tags = extracted keywords`, `kind = SKILL|MCP`,
`trust = USER_PROVIDED|EXTERNAL`, `metadata` contains `skill_id`/`mcp_server_id` for back-link.
The real bodies (skill revision content, MCP tool schemas) stay in their respective tables.

### 3. Plugin persistence is fully normalized (no giant jsonb)

*   `plugins` — identity, origin, trust, state
*   `plugin_components` — what the plugin contains: skills, embedded MCP, source books, executables, assets
*   `plugin_permissions` — scoped allowlist per permission domain (filesystem, network, secrets, etc.)
*   `plugin_installations` — versioned installation records for staged upgrade/rollback

### 4. GitHub install is the canonical plugin source path

GitHub releases (parent abstract `PluginSource` trait) + a `Marketplace` adapter trait for future
extensions. The marketplace trait is minimal (search, inspect, versions, fetch_manifest, resolve_artifact).
No over-design; local package and generic Git are secondary paths following the same trait.

### 5. Sandbox client in server uses the EXISTING core::sandbox protocol verbatim

Server opens a Unix-domain socket to sandboxd, authenticates with a short-lived MAC token, then
sends `RequestEnvelope` and receives `ResponseEnvelope` — using the exact existing types. No second
runtime API. Server mints auth tokens, sandboxd validates them against its `--auth-token-file`.

### 6. Progressive loading: agent decides what to load

The current `build_messages` already assembles candidates. The extension is:
1.  User message → unified library search with kind filtering
2.  Return ranked Book summaries (metadata only — no body, no tool schemas)
3.  Agent tool calls `library_load` (new) to load a Book's full body/schema
4.  For MCP/Plugin Books: loading resolves capabilities (tools, schemas) on demand
5.  Token metrics are tracked per-load, never preloaded

### 7. Schema 19 → 20 (one migration, `0020_unified_library.sql`)

A single forward migration adds: `books.kind`, `plugins*` tables, `plugin_installations`,
`plugin_components`, `plugin_permissions`. Backfills `kind = 'AUTOBIOGRAPHY'` for existing
autobiography books.

### 8. UI is a unified Library page with kind filter

A single Library page replaces the distributed Library/Skills/MCP/plugins pattern. The sidebar
may still offer quick-jump links. The Library page gets filters: ALL, SOURCE, SKILL, PLUGIN, MCP,
with an Autobiography shortcut view preserved. Kind-specific detail pages show relevant fields.

### 9. Trust model is preserved and extended

Existing `books.trust` enum (`VERIFIED|USER_PROVIDED|AGENT_INFERRED|EXTERNAL|UNTRUSTED`) covers all
book kinds. Plugin provenance carries additional fields (publisher, signature, verified digest) in
the `plugins` table. Popularity is separate from crypto trust — displayed distinctly in the UI but
never promoting an UNTRUSTED plugin.

### 10. No in-process dlopen; all plugin execution is sandboxed

Plugins execute in the sandbox (MCP stdio, WASI, or subprocess via sandboxd). The server never
`dlopen`s plugin code. Plugin activation spawns a sandboxed process or connects an MCP transport
through the sandbox.

## Data Model

### Migration 20: `0020_unified_library.sql`

```sql
-- 1. Add book kind column (nullable; NULL = SOURCE for backward compat)
ALTER TABLE books ADD COLUMN kind text;
ALTER TABLE books ADD CONSTRAINT books_kind_check
    CHECK (kind IS NULL OR kind IN ('SOURCE','SKILL','MCP','PLUGIN','AUTOBIOGRAPHY'));

-- Backfill: existing Autobiography books get kind = 'AUTOBIOGRAPHY'
UPDATE books SET kind = 'AUTOBIOGRAPHY' WHERE book_type = 'AUTOBIOGRAPHY' AND kind IS NULL;

-- 2. Component Book proxy for each existing Skill
INSERT INTO books (id, profile_id, title, body, book_type, scope, tags, provenance, trust,
    source, author, workspace_id, security_classification, kind, metadata)
SELECT
    gen_random_uuid(), s.profile_id, s.name, s.description,
    'INSTRUCTION', COALESCE(
        CASE WHEN s.workspace_id IS NOT NULL THEN 'WORKSPACE' ELSE 'PROFILE' END,
        'PROFILE'),
    '{}', 'SKILL', 'USER_PROVIDED',
    '{}', 'system', s.workspace_id, 'INTERNAL',
    'SKILL', jsonb_build_object('skill_id', s.id)
FROM skills s
WHERE NOT EXISTS (
    SELECT 1 FROM books b
    WHERE (b.metadata->>'skill_id')::uuid = s.id
      AND b.kind = 'SKILL'
)
ON CONFLICT DO NOTHING;

-- 3. Component Book proxy for each existing MCP server
INSERT INTO books (id, profile_id, title, body, book_type, scope, tags, provenance, trust,
    source, author, workspace_id, security_classification, kind, metadata)
SELECT
    gen_random_uuid(), ms.profile_id, ms.name,
    COALESCE(ms.configuration->>'description', ms.transport || ' MCP server'),
    'INSTRUCTION', 'PROFILE',
    '{}', 'MCP', 'USER_PROVIDED',
    '{}', 'system', NULL, 'INTERNAL',
    'MCP', jsonb_build_object('mcp_server_id', ms.id)
FROM mcp_servers ms
WHERE NOT EXISTS (
    SELECT 1 FROM books b
    WHERE (b.metadata->>'mcp_server_id')::uuid = ms.id
      AND b.kind = 'MCP'
)
ON CONFLICT DO NOTHING;

-- 4. Plugins table (normalized identity/origin/trust/state)
CREATE TABLE plugins (
    id uuid PRIMARY KEY,
    profile_id uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    workspace_id uuid REFERENCES workspaces(id) ON DELETE CASCADE,
    name text NOT NULL CHECK (char_length(name) BETWEEN 1 AND 200),
    description text NOT NULL,
    version text NOT NULL CHECK (char_length(version) BETWEEN 1 AND 64),
    source_type text NOT NULL CHECK (source_type IN ('github_release','generic_git','marketplace','local_package')),
    source_uri text NOT NULL,
    commit_sha text,
    artifact_digest text,
    publisher text,
    signature jsonb,
    verified boolean NOT NULL DEFAULT false,
    trust text NOT NULL CHECK (trust IN ('VERIFIED','USER_PROVIDED','AGENT_INFERRED','EXTERNAL','UNTRUSTED')),
    state text NOT NULL DEFAULT 'discovered'
        CHECK (state IN ('discovered','staged','installed','enabled','dormant','active','unhealthy','update_available')),
    install_path text,
    manifest_version integer NOT NULL DEFAULT 1,
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskClass {
    Coding,
    Research,
    DataAnalysis,
    DocumentCreation,
    GeneralQA,
    ShellAutomation,
    Ecommerce,
    SystemAdministration,
}

/// Returns a static mapping of task classes to relevant capability keywords.
pub fn task_capability_map() -> std::collections::HashMap<TaskClass, Vec<&'static str>> {
    use std::collections::HashMap;
    let mut map = HashMap::new();
    map.insert(TaskClass::Coding, vec!["rust", "python", "javascript", "typescript", "go", "code", "debug", "test", "implement"]);
    map.insert(TaskClass::Research, vec!["search", "find", "lookup", "investigate", "explore", "documentation", "docs"]);
    map.insert(TaskClass::DataAnalysis, vec!["data", "csv", "json", "analyze", "chart", "graph", "statistics", "pandas"]);
    map.insert(TaskClass::DocumentCreation, vec!["write", "document", "markdown", "report", "summary", "create"]);
    map.insert(TaskClass::GeneralQA, vec!["what", "how", "why", "explain", "describe", "tell"]);
    map.insert(TaskClass::ShellAutomation, vec!["bash", "shell", "command", "script", "terminal", "execute", "run"]);
    map.insert(TaskClass::Ecommerce, vec!["shop", "buy", "purchase", "cart", "checkout", "payment", "order"]);
    map.insert(TaskClass::SystemAdministration, vec!["system", "config", "service", "daemon", "log", "monitor", "admin"]);
    map
}

    sandbox_policy jsonb NOT NULL DEFAULT '{}'::jsonb,
    network_policy text NOT NULL DEFAULT 'NONE'
        CHECK (network_policy IN ('NONE','RESTRICTED','FULL')),
    resource_limits jsonb,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (profile_id, name)
);

-- 5. Plugin components (what the plugin provides)
CREATE TABLE plugin_components (
    id uuid PRIMARY KEY,
    plugin_id uuid NOT NULL REFERENCES plugins(id) ON DELETE CASCADE,
    component_type text NOT NULL CHECK (component_type IN ('skill','mcp_server','source_book','executable','asset','schema')),
    name text NOT NULL,
    manifest_ref text NOT NULL,
    metadata jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (plugin_id, name, component_type)
);

-- 6. Plugin permissions (scoped allowlist)
CREATE TABLE plugin_permissions (
    id uuid PRIMARY KEY,
    plugin_id uuid NOT NULL REFERENCES plugins(id) ON DELETE CASCADE,
    permission_domain text NOT NULL CHECK (permission_domain IN (
        'filesystem_read','filesystem_write','network','secrets','subprocess','admin','system_info')),
    scope_value text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (plugin_id, permission_domain, scope_value)
);

-- 7. Plugin installations (versioned install records for staged upgrades)
CREATE TABLE plugin_installations (
    id uuid PRIMARY KEY,
    plugin_id uuid NOT NULL REFERENCES plugins(id) ON DELETE CASCADE,
    version text NOT NULL,
    artifact_digest text NOT NULL,
    status text NOT NULL DEFAULT 'staged' CHECK (status IN ('staged','installing','active','failed','rolled_back')),
    installed_by uuid REFERENCES users(id) ON DELETE SET NULL,
    sandbox_image text,
    sandbox_digest text,
    peer_port integer,
    self_test_result jsonb,
    installed_at timestamptz,
    activated_at timestamptz,
    rolled_back_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (plugin_id, version)
);

-- 8. Indexes
CREATE INDEX books_kind_idx ON books (kind) WHERE kind IS NOT NULL;
CREATE INDEX books_kind_profile_updated_idx ON books (profile_id, kind, updated_at DESC);
CREATE INDEX plugins_profile_state_idx ON plugins (profile_id, state);
CREATE INDEX plugin_components_plugin_idx ON plugin_components (plugin_id);

-- 9. Add a companion PLUGIN Book for each plugin (via trigger, not inline — see Lane B)
-- 10. Bump schema
UPDATE schema_metadata SET schema_version = 20, updated_at = now() WHERE singleton;
```

### Plugin Manifest JSON Schema (version 1)

```json
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "title": "Gobrowse Plugin Manifest",
  "type": "object",
  "required": ["manifest_version", "name", "version", "publisher", "components"],
  "properties": {
    "manifest_version": { "type": "integer", "const": 1 },
    "name": { "type": "string", "minLength": 1, "maxLength": 200 },
    "version": { "type": "string", "pattern": "^[0-9]+\\.[0-9]+\\.[0-9]+(-[a-zA-Z0-9.]+)?$" },
    "description": { "type": "string", "maxLength": 2000 },
    "publisher": {
      "type": "object",
      "required": ["name"],
      "properties": {
        "name": { "type": "string" },
        "url": { "type": "string", "format": "uri" },
        "email": { "type": "string", "format": "email" }
      }
    },
    "license": { "type": "string" },
    "homepage": { "type": "string", "format": "uri" },
    "repository": { "type": "string", "format": "uri" },
    "sandbox": {
      "type": "object",
      "properties": {
        "image": { "type": "string" },
        "entrypoint": { "type": "array", "items": { "type": "string" } },
        "network": { "type": "string", "enum": ["NONE", "RESTRICTED", "FULL"] },
        "resource_limits": {
          "type": "object",
          "properties": {
            "cpu_millis": { "type": "integer", "minimum": 1 },
            "memory_bytes": { "type": "integer", "minimum": 1 },
            "writable_storage_bytes": { "type": "integer", "minimum": 1 },
            "pids": { "type": "integer", "minimum": 1 },
            "execution_seconds": { "type": "integer", "minimum": 1 }
          }
        }
      }
    },
    "permissions": {
      "type": "object",
      "properties": {
        "filesystem_read": { "type": "array", "items": { "type": "string" } },
        "filesystem_write": { "type": "array", "items": { "type": "string" } },
        "network": { "type": "array", "items": { "type": "string" } },
        "secrets": { "type": "array", "items": { "type": "string" } },
        "subprocess": { "type": "boolean" },
        "admin": { "type": "boolean" }
      }
    },
    "components": {
      "type": "array",
      "minItems": 1,
      "items": {
        "type": "object",
        "required": ["type", "name", "ref"],
        "properties": {
          "type": { "type": "string", "enum": ["skill", "mcp_server", "source_book", "executable", "asset", "schema"] },
          "name": { "type": "string", "minLength": 1 },
          "ref": { "type": "string" },
          "description": { "type": "string" },
          "metadata": { "type": "object" }
        }
      }
    },
    "self_test": {
      "type": "object",
      "properties": {
        "command": { "type": "array", "items": { "type": "string" } },
        "expected_exit": { "type": "integer" },
        "timeout_seconds": { "type": "integer", "default": 30 }
      }
    }
  }
}
```

## BookKind and shared types (in `gobrowse_core::library`)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum BookKind {
    Source,
    Skill,
    Mcp,
    Plugin,
    Autobiography,
}

// New field on Book:
pub struct Book {
    // ... existing fields ...
    pub kind: Option<BookKind>, // None = Source (backward compat)
}
```

### Compact per-Book searchable metadata (spec item 1)

The Level-0 index is a `SELECT` over the unified `books` table, never a preloaded prompt. Spec's
required metadata fields map to columns/expressions as follows (no full body is selected):

| Spec field | Source |
|------------|--------|
| `BOOK_ID` | `books.id` |
| `KIND` | `books.kind` (NULL → `'SOURCE'`) |
| `TITLE` | `books.title` |
| `DESCRIPTION` | SKILL/MCP/PLUGIN → `books.body` (description); SOURCE → `left(body,500)` snippet |
| `KEYWORDS` | `books.tags` |
| `CAPABILITIES` | `books.metadata->>'capabilities'` (JSON array of tool/component names; populated by Lanes C/E) |
| `TRUST` | `books.trust` |
| `PROVENANCE` | `books.provenance` |
| `VERSION` | `books.revision` (book revision) + `books.metadata->>'version'` (plugin/skill version) |
| `SCOPE` | `books.scope` |
| `SECURITY_CLASS` | `books.security_classification` |
| `UPDATED_AT` | `books.updated_at` |

`CAPABILITIES` is the only new field and lives in the existing `metadata jsonb` (no schema change
beyond `kind`); Lanes C/E write it (e.g. plugin component names, MCP tool names). Popularity is a
separate column only for marketplace results, never folded into `trust`.

## Marketplace Adapter Trait (`gobrowse_core`)

```rust
use async_trait::async_trait;
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplaceEntry {
    pub id: String,
    pub name: String,
    pub description: String,
    pub publisher: String,
    pub latest_version: String,
    pub download_count: u64,
    pub verified: bool,
    pub categories: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionInfo {
    pub version: String,
    pub published_at: OffsetDateTime,
    pub digest: String,
    pub changelog: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactLocation {
    pub url: Url,              // resolved download URL
    pub digest: String,        // expected SHA-256 hex
    pub size_bytes: Option<u64>,
    pub content_type: String,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum MarketplaceError {
    #[error("plugin not found")]
    NotFound,
    #[error("marketplace unavailable")]
    Unavailable,
    #[error("rate limited")]
    RateLimited { retry_after_seconds: Option<u64> },
    #[error("invalid response from marketplace")]
    InvalidResponse,
    #[error("request timed out")]
    Timeout,
}

#[async_trait]
pub trait PluginMarketplace: Send + Sync {
    async fn search(&self, query: &str) -> Result<Vec<MarketplaceEntry>, MarketplaceError>;
    async fn inspect(&self, id: &str) -> Result<MarketplaceEntry, MarketplaceError>;
    async fn versions(&self, id: &str) -> Result<Vec<VersionInfo>, MarketplaceError>;
    async fn fetch_manifest(&self, id: &str, version: &str) -> Result<serde_json::Value, MarketplaceError>;
    async fn resolve_artifact(&self, id: &str, version: &str) -> Result<ArtifactLocation, MarketplaceError>;
}
```

## PluginSource trait (abstraction over GitHub/generic Git/local/marketplace)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginIdentity {
    pub source_type: String,     // "github_release" | "generic_git" | "marketplace" | "local_package"
    pub source_uri: String,
    pub commit_sha: Option<String>,
    pub version: Option<String>,
}

#[async_trait]
pub trait PluginSource: Send + Sync {
    /// Resolve a specific immutable revision from an identity with optional version pin.
    async fn resolve(&self, identity: &PluginIdentity) -> Result<ResolvedPluginSource, PluginSourceError>;

    /// Fetch the plugin manifest at the resolved revision.
    async fn fetch_manifest(&self, resolved: &ResolvedPluginSource) -> Result<serde_json::Value, PluginSourceError>;

    /// Download the artifact bundle to a temp workspace path.
    async fn download_artifact(&self, resolved: &ResolvedPluginSource, dest: &Path) -> Result<(), PluginSourceError>;
}
```

## Sandbox Client Design (new `sandbox_client` module in `gobrowse-server`)

The server talks to sandboxd over a Unix-domain socket. The token flow:

1.  Server has `config.sandbox.socket_path: PathBuf` and `config.sandbox.auth_token: SecretString`
    (both read from the config file; the token MUST match what's passed to sandboxd's
    `--auth-token-file`).
2.  `SandboxClient::connect(socket_path)` → opens `UnixStream`.
3.  Every request: serialize `RequestEnvelope` (with `version: 2`, `request_id: Uuid::now_v7()`,
    `token: auth_token`, `operation: SandboxOperation`) to a JSON line (≤512 KiB); write to socket;
    read one JSON line (≤512 KiB) as `ResponseEnvelope`; verify `request_id` matches.
4.  Operations map 1:1 to `gobrowse_core::sandbox::SandboxOperation` — no translation layer.
5.  Error taxonomy:
    *   Connection refused / broken pipe → `SandboxClientError::Disconnected`
    *   Response `code: UNAUTHORIZED` → `SandboxClientError::Unauthorized`
    *   Response `code: NOT_FOUND` → `SandboxClientError::NotFound`
    *   Response `code: POLICY_DENIED` → `SandboxClientError::PolicyDenied`
    *   Response `code: LIMIT_EXCEEDED` → `SandboxClientError::LimitExceeded`
    *   Response `code: DEADLINE_EXCEEDED` → `SandboxClientError::Timeout`
    *   Response `code: INTERNAL` → `SandboxClientError::InternalProtocol(msg)`
    *   Invalid JSON / unknown op → `SandboxClientError::ProtocolViolation`
6.  Audit: every sandbox operation creates an `audit_events` row with `action: "sandbox.op"`,
    `resource_type: "sandbox"`, `resource_id: workspace_id`, detail includes request_id,
    operation op, and outcome (success/error code). No plaintext token or file contents in audit.

The sandbox client is a thin connection-pooled wrapper; no buffering, no reconnection layer
(the caller decides retry semantics; agent tools get one attempt per sandbox call, surfaced as
`ToolError::Execution` on sandbox failure).

### Config additions (`config.rs`)

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct FeatureSettings {
    pub sandbox: bool,                // existing
    // ... existing fields ...
    #[serde(default)]
    pub sandbox_socket_path: Option<PathBuf>,
    #[serde(default)]
    pub sandbox_auth_token: Option<SecretString>,
    #[serde(default)]
    pub sandbox_socket_timeout_seconds: u64, // default 30
    #[serde(default)]
    pub sandbox_plugin_image: Option<String>, // default workspace image for plugin exec
}
```

## New API Routes

### Library (extended)

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/library/search?q=&kind=&workspace_id=&limit=` | Existing search + new `kind` filter |
| `GET` | `/library/books/:id` | Extended response: includes `kind` and kind-specific data |
| `POST` | `/library/books/:id/load` | Progressive load: returns full body + resolved tools/schemas for SKILL/MCP/PLUGIN |
| `GET` | `/library/stats` | Token/usage metrics for the profile |

### Plugins

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/plugins/preview` | Resolve + validate manifest from a source URI, return permission preview |
| `POST` | `/plugins/install` | Stage + install a plugin (requires operator approval after preview) |
| `GET` | `/plugins` | List installed plugins for profile |
| `GET` | `/plugins/:id` | Plugin detail with components, permissions, installation history |
| `PATCH` | `/plugins/:id` | Update state (enable/disable), trust level |
| `POST` | `/plugins/:id/upgrade` | Stage a new version; returns permission diff |
| `POST` | `/plugins/:id/upgrade/:version/activate` | Activate staged upgrade (atomic) |
| `POST` | `/plugins/:id/rollback` | Roll back to previous active version |
| `DELETE` | `/plugins/:id` | Uninstall (removes sandbox workspace, deactivates Book proxy) |
| `POST` | `/plugins/search` | Marketplace search (requires `marketplace` feature) |

### Sandbox tools (agent-accessible)

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/sandbox/exec` | Execute command in sandboxed workspace |
| `POST` | `/sandbox/files/read` | Read file from sandbox workspace |
| `POST` | `/sandbox/files/write` | Write file to sandbox workspace |
| `POST` | `/sandbox/files/list` | List directory in sandbox workspace |
| `POST` | `/sandbox/files/stat` | Stat file/dir |
| `POST` | `/sandbox/files/mkdir` | Create directory |
| `POST` | `/sandbox/files/remove` | Remove file/dir |
| `POST` | `/sandbox/terminal/start` | Start a PTY session |
| `POST` | `/sandbox/terminal/:id/write` | Write to terminal |
| `POST` | `/sandbox/terminal/:id/read` | Read terminal output |
| `POST` | `/sandbox/terminal/:id/resize` | Resize terminal |
| `POST` | `/sandbox/terminal/:id/interrupt` | Send SIGINT |
| `POST` | `/sandbox/terminal/:id/close` | Close terminal |
| `POST` | `/sandbox/processes` | List processes |
| `POST` | `/sandbox/processes/:pid/kill` | Kill process |

#### Sandbox route access control

All sandbox routes require: authenticated user, workspace membership (OWNER/EDITOR for write ops,
VIEWER+ for read), `features.sandbox == true`, and a running sandboxd instance. The workspace's
`network_policy` and `sandbox_policy` apply.

## Chat Tool Integration (progressive loading)

### New tools (in `run_tools.rs` or `sandbox_tools.rs`)

Nine new `impl Tool` implementations, reusing the existing `ToolDescriptor`/`ToolContext`/`ToolError` pattern:

| Tool ID | Description | Schema Source |
|---------|-------------|---------------|
| `library_load` | Load a Book's full body + resolved capability schemas. For SKILL: returns the active revision content. For MCP: connects transport, discovers tools, returns tool list. For PLUGIN: returns component manifest. Bounded: 1 Book per call, max 5 loads per run. | Dynamic — reads from DB |
| `sandbox_exec` | Execute a command in the sandboxed workspace. | Map to `SandboxOperation::Start` |
| `sandbox_read_file` | Read a file from the sandbox workspace (returns base64-encoded content, max 256 KiB). | Map to `SandboxOperation::FsRead` |
| `sandbox_write_file` | Write/create a file. | Map to `SandboxOperation::FsWrite` |
| `sandbox_list_files` | List directory entries. | Map to `SandboxOperation::FsList` |
| `sandbox_stat` | Get file metadata. | Map to `SandboxOperation::FsMetadata` |
| `sandbox_mkdir` | Create a directory. | Map to `SandboxOperation::FsMkdir` |
| `sandbox_remove` | Remove a file or empty directory. | Map to `SandboxOperation::FsDelete` |
| `terminal_start` | Start a PTY session. | Map to `SandboxOperation::Start` |

All sandbox tools share `ToolDescriptor.risk = RiskClass::High` and `source = "sandbox"`.
They are offered only when `features.sandbox == true` AND the workspace has a sandbox policy.

### Progressive loading flow in `build_messages`

```text
1. User message received
2. Extract query from last user message
3. Unified library search (lexical FTS + optional semantic):
   SELECT id, title, kind, book_type, trust, provenance,
          ts_headline('english', CASE WHEN kind='SKILL' THEN description ELSE left(body,500) END, ...) AS snippet
   FROM books
   WHERE profile_id = $1 AND kind IN ('SOURCE','SKILL','MCP','PLUGIN') AND ...
   ORDER BY ts_rank_cd(...) DESC LIMIT 12
4. Each row → ContextCandidate(source: LibraryRetrieval, priority: 300, required: false)
   Content includes: kind, title, snippet, trust, but NOT the full body or tool schemas.
5. Agent receives these summaries + the `library_load` tool definition.
6. Agent MAY call `library_load(book_id)` → we resolve the Book body + capability schemas,
   append the result as a tool-result message, and continue the agent loop.
7. If the loaded Book is MCP → connect transport (via sandbox if needed), discover tools,
   add discovered ToolDefinitions to subsequent rounds.
8. Token metrics recorded: BOOK_SEARCHES, BOOKS_CONSIDERED, BOOKS_LOADED, BOOK_TOKENS_LOADED.
```

### Token metrics (run_internal counters, persisted as run_events)

```rust
struct RunTokenMetrics {
    book_searches: u32,          // how many times library search was invoked
    books_considered: u32,       // how many book summaries were shown to the model
    books_loaded: u32,           // how many Books had full body+schemas loaded
    book_tokens_loaded: u64,     // total tokens from loaded book bodies
    skill_book_loads: u32,       // SKILL books specifically loaded
    plugin_book_loads: u32,      // PLUGIN books specifically loaded
    mcp_book_loads: u32,         // MCP books specifically loaded
    source_book_loads: u32,      // SOURCE books specifically loaded
    plugin_tools_discovered: u32,// tools discovered from plugin-embedded MCP
    plugin_tools_loaded: u32,    // plugin tools whose schemas were loaded
    mcp_tools_discovered: u32,   // tools discovered from standalone MCP
    mcp_tools_loaded: u32,       // MCP tools whose schemas were loaded
}
```

Emitted as a single `run.event` of type `"token_metrics"` at run completion.

## UI Page Structure

### Unified Library page (`Page::Library` — replaces current Library + Skills + Autobiography)

```
┌────────────────────────────────────────────────────┐
│ [Filter: ALL | SOURCE | SKILL | PLUGIN | MCP]     │
│ [Search: ___________________________] [Add ▼]      │
│   Add options: Book | Skill | MCP Server | Plugin  │
├────────────────────────────────────────────────────┤
│ Results (search + list):                           │
│ ┌──────────────────────────────────────────────┐   │
│ │ 📗 SKILL  │ create-payment-credential        │   │
│ │           │ Gets secure one-time cards...    │   │
│ │           │ TRUST: USER_PROVIDED  ⭐ PIN      │   │
│ ├──────────────────────────────────────────────┤   │
│ │ 🔌 MCP    │ filesystem-mcp                   │   │
│ │           │ stdio MCP server                 │   │
│ │           │ TRUST: USER_PROVIDED  ☐ dormant  │   │
│ ├──────────────────────────────────────────────┤   │
│ │ 🧩 PLUGIN │ github-actions-runner            │   │
│ │           │ Self-hosted runner v1.2.3        │   │
│ │           │ TRUST: EXTERNAL  ⚠ staged        │   │
│ ├──────────────────────────────────────────────┤   │
│ │ 📄 SOURCE │ Deployment runbook               │   │
│ │           │ Step-by-step deploy procedure... │   │
│ │           │ TRUST: USER_PROVIDED             │   │
│ └──────────────────────────────────────────────┘   │
└────────────────────────────────────────────────────┘
```

### Plugin detail page (new)

Shows: identity, origin (source type, repo, commit, digest), trust (publisher, signature, verified
status), install source, components list, runtime sandbox policy, permissions (with names, not
secrets), lifecycle state, installation history, action buttons (enable/disable/upgrade/rollback).

### Plugin install flow (new modal/stepper)

1.  **Source input**: GitHub URL or generic Git URL or marketplace search box
2.  **Preview** (loading spinner → resolved manifest summary): name, version, publisher, components,
    permissions, resource requirements
3.  **Approval**: permission preview (`filesystem: /workspace/**`, `network: api.example.com:443`,
    `secrets: GITHUB_TOKEN` — references only, not values). Action: "Install" with loading state.
4.  **Install progress**: Stage → Validate → Self-test → Install → "Plugin installed (DORMANT)"
5.  **Error states**: validation failure → actionable error message; self-test failure → "Installation
    failed: self-test did not pass. Review logs."; network error → "Could not reach source."

### Sandbox terminal page (new, `Page::Terminals` extension)

Existing `Page::Terminals` placeholder. Add: terminal selection (by workspace), xterm.js-like PTY
embedding connecting to `/sandbox/terminal/:id/read` + `/sandbox/terminal/:id/write` via
WebSocket or polling, with resize, interrupt, and close controls.

## Lane Assignments

### Lane A: Migration 20 + Data Model (`0020_unified_library.sql` + `library.rs` extension)

**Files owned:**
*   `crates/gobrowse-server/migrations/0020_unified_library.sql` (new)
*   `crates/gobrowse-core/src/library.rs` — add `BookKind` enum, `kind` field on `Book`
*   `crates/gobrowse-server/src/db.rs` — no change (auto-picks up migration file)
*   `crates/gobrowse-server/tests/postgres_integration.rs` — bump schema version assertion from 19 → 20
*   `crates/gobrowse-server/tests/worktrees_integration.rs` — bump schema version assertion from 19 → 20

**Shared contracts delivered:**
*   `BookKind` enum in `gobrowse_core::library`
*   `PluginState` enum in `gobrowse_core::library`
*   Updated `Book` struct with `kind: Option<BookKind>`

**Acceptance:**
*   Migration 20 runs forward on a schema-19 database
*   Existing Skill and MCP server rows get companion Book rows with `kind = 'SKILL'`/`kind = 'MCP'`
*   Autobiography books get `kind = 'AUTOBIOGRAPHY'`
*   All existing source books have `kind IS NULL` (treated as `SOURCE` by application code)
*   `plugins`, `plugin_components`, `plugin_permissions`, `plugin_installations` tables exist with
    FK cascade
*   Re-run migration is idempotent (skill/MCP backfills have `WHERE NOT EXISTS` guards)
*   Schema version 20 asserted in both test files

### Lane B: Plugin Manifest + Marketplace Types + PluginSource trait (`gobrowse-core`)

**Files owned:**
*   `crates/gobrowse-core/src/plugin.rs` (new) — manifest schema types, marketplace trait, plugin source trait
*   `crates/gobrowse-core/src/lib.rs` — add `pub mod plugin;`

**Shared contracts delivered:**
*   `PluginManifest` (deserializable struct matching JSON schema)
*   `PluginManifestV1` (versioned wrapper)
*   `MarketplaceEntry`, `VersionInfo`, `ArtifactLocation` structs
*   `PluginMarketplace` trait
*   `PluginSource` trait
*   `PluginSourceError` error enum
*   `MarketplaceError` error enum
*   `ResolvedPluginSource` struct
*   `PluginPermission` struct (domain + scope_value)

**Acceptance:**
*   Manifest JSON schema validates against test fixtures (valid + invalid manifests)
*   `serde` round-trip: `PluginManifest` ↔ JSON ↔ `PluginManifest`
*   `PluginManifestV1::validate()` returns errors for missing required fields, invalid version format,
    empty components array
*   Trait is `Send + Sync` and object-safe where needed
*   No database or server dependencies in `gobrowse-core/src/plugin.rs`

### Lane C: Plugin Install Flow (server-side GitHub install + plugin CRUD)

**Files owned:**
*   `crates/gobrowse-server/src/plugin_api.rs` (new) — all route handlers
*   `crates/gobrowse-server/src/plugin_github.rs` (new) — `GitHubReleaseSource: PluginSource`
*   `crates/gobrowse-server/src/lib.rs` — register plugin routes
*   `crates/gobrowse-server/src/outbound_http.rs` — may reuse `PinnedHttpsTransport` for download

**Shared contracts consumed:** `BookKind`, `PluginManifest`, `PluginSource`, `MarketplaceEntry` (from Lanes A/B)

**Implementation for Lane C agent:**

1.  **`GitHubReleaseSource`**: implements `PluginSource` trait
    *   `resolve`: parses `owner/repo` from URI, resolves a tag/commit to an immutable SHA via
        `GET /repos/{owner}/{repo}/git/ref/tags/{tag}` or `/commits/{ref}` (GitHub API).
    *   `fetch_manifest`: downloads `plugin.toml` or `manifest.json` from the resolved tree
        (`GET /repos/{owner}/{repo}/contents/plugin.toml?ref={sha}`).
    *   `download_artifact`: downloads the release tarball to a temp path.
2.  **Preview flow** (`POST /plugins/preview`):
    *   Accept `{ source_uri, version }`
    *   Resolve the source → fetch manifest → validate against JSON schema
    *   Return manifest summary + permission list + trust assessment (signature verification if
        present; otherwise UNTRUSTED)
3.  **Install flow** (`POST /plugins/install`):
    *   Accept `{ source_uri, version, approved_permissions: Vec<String> }`
    *   State machine: `discovered → staged → installed → dormant`
    *   Staging: resolve immutable revision, validate manifest, verify digest/signature
    *   Static policy inspection: check permissions against workspace policy
    *   Permission preview: operator sees diff, approves
    *   Install: download artifact, create plugin Book proxy (`kind = 'PLUGIN'`), create
        `book_links` rows for each component (`PROVIDES` relation), insert `plugin_components`,
        create sandbox workspace volume
    *   Self-test: if `manifest.self_test` is present, execute in sandbox, verify exit code
    *   Final state: `dormant` — installed but not active
4.  **Upgrade flow** (`POST /plugins/:id/upgrade`):
    *   Stage new version → compute permission diff → operator approval → self-test →
        atomic activation → previous version retained → rollback possible
5.  **Rollback** (`POST /plugins/:id/rollback`):
    *   Deactivate current → activate previous active installation → update plugin Book proxy
6.  All mutations create `audit_events` rows.

**Acceptance:**
*   GitHub install flow end-to-end: preview → stage → validate manifest → verify digest →
    permission preview → install → self-test → dormant
*   Permission diff computed correctly between v1 and v2
*   `UNTRUSTED` by default for unsigned plugins; `VERIFIED` when signature checks out
*   `DELETE /plugins/:id` cascades to `plugin_components`, `plugin_permissions`,
    `plugin_installations`, and the plugin Book proxy
*   Upgrade → rollback preserves and restores the previous version
*   Installation requires `profile ADMIN/OWNER` or workspace `OWNER`
*   Error states surfaced as actionable API errors (not raw stack traces)

### Lane D: Sandbox Client Integration (server ↔ sandboxd)

**Files owned:**
*   `crates/gobrowse-server/src/sandbox_client.rs` (new) — `SandboxClient`, connection pool, token auth
*   `crates/gobrowse-server/src/config.rs` — add `sandbox_socket_path`, `sandbox_auth_token`,
    `sandbox_socket_timeout_seconds`, `sandbox_plugin_image` to `FeatureSettings`
*   `crates/gobrowse-server/src/doctor.rs` — update sandbox check: WARN → PASS when
    sandboxd socket is reachable and responds to `Health`; FAIL when enabled but unreachable
*   `crates/gobrowse-server/src/lib.rs` — initialize `SandboxClient` in `AppState` if `features.sandbox`

**Shared contracts consumed:** `RequestEnvelope`, `SandboxOperation`, `ResponseEnvelope`,
`SandboxResult`, `SandboxProtocolError`, `SandboxErrorCode`, `ResourceLimits`, `NetworkPolicy`,
`workspace_volume_name` — all from `gobrowse_core::sandbox` (existing).

**Implementation for Lane D agent:**

```rust
// sandbox_client.rs
pub struct SandboxClient {
    socket_path: PathBuf,
    auth_token: SecretString,
    timeout: Duration,
}

impl SandboxClient {
    pub async fn connect(config: &SandboxConfig) -> Result<Self, SandboxClientError>;
    pub async fn send(&self, operation: SandboxOperation) -> Result<SandboxResult, SandboxClientError>;
    pub async fn health(&self) -> Result<String, SandboxClientError>;
}

#[derive(Debug, Error)]
pub enum SandboxClientError {
    #[error("sandbox daemon is not reachable")]
    Disconnected,
    #[error("sandbox authentication failed")]
    Unauthorized,
    #[error("sandbox resource not found")]
    NotFound,
    #[error("sandbox operation denied by policy")]
    PolicyDenied,
    #[error("sandbox resource limit exceeded")]
    LimitExceeded,
    #[error("sandbox operation timed out")]
    Timeout,
    #[error("sandbox protocol violation: {0}")]
    ProtocolViolation(String),
    #[error("sandbox internal error: {0}")]
    Internal(String),
}
```

The `SandboxClient::send` serializes the `RequestEnvelope`, writes it as one JSON line, reads one
JSON line back, verifies the `request_id` match, and maps `SandboxErrorCode` variants to
`SandboxClientError`. The client MUST handle `OutcomeUnknown` by surfacing it (not retrying —
caller decides).

**Acceptance:**
*   `SandboxClient::health()` returns `"ok"` when sandboxd is running
*   `SandboxClient::send(SandboxOperation::FsList { ... })` lists files in workspace
*   `SandboxClient::send(SandboxOperation::FsWrite { ... })` writes a file
*   `SandboxClientError::Unauthorized` when token mismatches
*   `SandboxClientError::Disconnected` when socket is absent
*   Audit events emitted per sandbox operation
*   Doctor reports PASS when sandboxd is reachable
*   Connection failures surfacing as clear errors (no silent hangs)
*   Timeout honored on all socket I/O

### Lane E: Progressive Loading + Chat Tool Integration (`run_api`, `chat.rs` changes, new tools)

**Files owned:**
*   `crates/gobrowse-server/src/run_api.rs` — progressive loading in `build_messages`, new tool loop
    integration, token metrics
*   `crates/gobrowse-server/src/run_tools.rs` — add `library_load`, `sandbox_*`, `terminal_start` tools
*   `crates/gobrowse-server/src/chat.rs` — no changes expected (tool wire format already exists)
*   `crates/gobrowse-server/src/library_api.rs` — add `load_book` endpoint

**Shared contracts consumed:** `BookKind`, `SandboxOperation`, `SandboxResult`, `SandboxClient`,
`Tool`, `ToolDescriptor`, `ToolContext`, `ToolError` (all existing/shared from prior lanes).

**Implementation for Lane E agent:**

1.  **`library_load` tool**: accepts `book_id: Uuid`. Resolves kind:
    *   SOURCE: returns `{ body, revision }` (bounded to 96 KiB)
    *   SKILL: returns `{ id, name, content, revision, promoted }` from `skill_revisions`
    *   MCP: returns `{ id, name, transport, tools: [...] }` — connects MCP transport,
        calls `tools/list`, returns discovered tool schemas (bounded to 16 tools, 64 KiB total)
    *   PLUGIN: returns `{ id, name, components, permissions }` — reads from `plugin_components`
    *   AUTOBIOGRAPHY: returns `{ body }` (bounded to 99,999 chars)
2.  **Sandbox tools** (9 tools): each maps to one `SandboxOperation`, executed via `SandboxClient`.
    Input schema validates path against `validate_workspace_path`. Execution timeout: 10 s.
    Output bounded to 64 KiB.
3.  **Progressive loading in `build_messages`**:
    *   Phase 1 (always): system policy + recent messages
    *   Phase 2 (always): unified library search — `SELECT id, title, kind, book_type, trust,
        ts_headline(...) AS snippet FROM books WHERE profile_id = $1 AND kind IS NOT NULL AND
        search_document @@ websearch_to_tsquery($2) ORDER BY ts_rank_cd(...) DESC LIMIT 12`
    *   Phase 3 (always): pinned books + worktree (existing)
    *   Phase 4 (on `library_load`): full body + schemas appended as tool results
    *   Phase 5 (on MCP discovery): discovered tool definitions added to `ModelRequest.tools`
    *   Token metrics tracked throughout
4.  **Retrieval quality**: the library search returns Book summaries only; the model decides
    which to load. A `library_load` call resolves the minimal needed content (e.g., for MCP:
    tool names + descriptions only, schemas loaded only on `tools/call`). Multiple similar
    descriptions do not cause more than one load unless the agent explicitly requests.

**Acceptance:**
*   `library_load("skill-uuid")` returns skill content (not the MCP server list)
*   `library_load("mcp-uuid")` connects MCP, returns tools
*   Sandbox `exec` tool returns command output (bounded)
*   Sandbox `read_file` returns base64 file content
*   Sandbox `write_file` writes content; `read_file` verifies
*   Token metrics event emitted at run completion with all fields populated
*   Phase-2 search never returns full bodies (snippets only)
*   Phase-4 body load bounded to 96 KiB for source/skill

### Lane F: UI — Unified Library, Plugin Install, Sandbox Terminal

**Files owned:**
*   `crates/gobrowse-web/src/app.rs` — Unified Library page, plugin install stepper, sandbox terminal
    page, kind-filter tabs, existing page adaptations

**Shared contracts consumed:** `BookKind`, `PluginState`, `MarketplaceEntry` (as JSON DTO shapes,
not Rust types — the web client deserializes JSON).

**Implementation for Lane F agent:**

1.  **Unified Library page** (replaces current `LibraryPage`, `SkillsPage`, `McpPage` merge):
    *   Tabs: ALL | SOURCE | SKILL | PLUGIN | MCP (Autobiography as a shortcut in sidebar or
        as a filter-chip)
    *   Search box queries `GET /library/search?q=&kind=&limit=`
    *   Add menu dropdown: "New Book", "New Skill", "New MCP Server", "Install Plugin"
    *   Each result shows: kind icon, title, snippet, trust badge, quick actions (pin to
        conversation, load, delete)
    *   Clicking a result opens a kind-specific detail panel
2.  **Plugin install stepper** (new modal/dialog):
    *   Step 1: source input (GitHub URL text field)
    *   Step 2: loading → manifest preview (name, version, publisher, components list,
        permissions list with references, not secrets)
    *   Step 3: approval button ("Install") with confirm
    *   Step 4: progress bar (Stage → Validate → Self-test → Install)
    *   Step 5: completion ("Plugin installed · DORMANT") with enable button
    *   Error states: inline actionable messages
3.  **Plugin detail page**: identity/origin/trust/state, components, permissions,
    action buttons (enable/disable/upgrade/rollback/uninstall), installation history,
    upgrade diff preview
4.  **Sandbox terminal page** (real PTY embedding):
    *   Select workspace → start terminal → xterm.js rendering (or simple text-based PTY if
        WebSocket isn't wired)
    *   Input sends to `/sandbox/terminal/:id/write`
    *   Output reads from `/sandbox/terminal/:id/read` (poll or SSE)
    *   Resize, Ctrl-C (interrupt), close
5.  **Adapt existing pages**: `SkillsPage` and `McpPage` become views within the unified
    Library page or redirect; `AutobiographyPage` remains as a shortcut view

**Acceptance:**
*   Unified library page shows SKILL, MCP, SOURCE, PLUGIN books in one searchable list
*   Kind filter tabs work: clicking "SKILL" shows only SKILL kind books
*   Plugin install stepper completes end-to-end with a real GitHub URL (using the API)
*   Install errors rendered inline (not raw JSON or silent)
*   Plugin detail page shows permission names, not raw secret values
*   Sandbox terminal connects, echoes input, displays output
*   Existing Library/Skills/MCP/Autobiography pages are reachable via the unified page
    (not lost)

## Retrieval / Progressive Loading Design

### Stage 1: Compact searchable metadata (the Level-0 index)

```sql
-- Always: the compact search row (never includes body/body)
SELECT id, kind, title, book_type, trust, provenance, tags,
       ts_headline('english',
         CASE WHEN kind = 'SKILL'  THEN body
              WHEN kind = 'MCP'   THEN body
              WHEN kind = 'PLUGIN' THEN body
              ELSE left(body, 500)
         END,
         websearch_to_tsquery('english', $1),
         'MaxWords=24, MinWords=6, ShortWord=3') AS snippet
FROM books
WHERE profile_id = $2 AND search_document @@ websearch_to_tsquery('english', $1)
  AND ($3::text IS NULL OR kind = $3::text)  -- kind filter
ORDER BY ts_rank_cd(search_document, websearch_to_tsquery('english', $1)) DESC
LIMIT 12;
```

### Stage 2: Agent inspection

Agent receives 12 ranked book summaries (id, kind, title, snippet, trust, tags). Each ~200-500
characters. Token cost ~600-1500 tokens for all summaries.

### Stage 3: Load on demand

Agent calls `library_load(book_id)`. For each book kind:

| Kind | Resolved content | Token cost (max) |
|------|-----------------|-----------------|
| SOURCE | `body` truncated to 24,000 chars | ~6k tokens |
| SKILL | `skill_revisions.content` where `promoted=true` and `revision=active_revision` | ~4k tokens |
| MCP | `tools/list` result: names + descriptions only (schemas deferred) | ~1k tokens |
| PLUGIN | Component manifest (names + descriptions) | ~2k tokens |

### Stage 4: Tool schema resolution (for MCP/Plugin)

When the agent wants to call a specific tool, it loads just that tool's input/output schema via
a second `library_load` with `component: tool_name` parameter. Schemas are never preloaded.

### Stage 5: Activation (for Plugin-embedded MCP)

Plugin activation: resolve sandbox image, spawn via sandboxd, connect MCP transport through sandbox,
discover tools, expose only selected tools. Dormant after run completes (or after configurable
idle timeout).

## Journey Mapping (Journeys A–J from spec)

| Journey | Mapped to | Lane |
|---------|-----------|------|
| A: Sandbox terminal full PTY flow | Lane D (sandbox client) + Lane E (terminal_start tool) + Lane F (terminal UI) | D+E+F |
| B: Agent sandbox tools | Lane D (sandbox client) + Lane E (sandbox tools) | D+E |
| C: Source book search/use | Lane A (unified index) + Lane E (progressive loading) | A+E |
| D: Skill book unloaded→retrieval→load→use | Lane A (SKILL companion Book) + Lane E (library_load for SKILL) | A+E |
| E: MCP book dormant→activate→call→dormant | Lane A (MCP companion Book) + Lane E (library_load for MCP, tool discovery, activation) | A+E |
| F: GitHub plugin install | Lane B (manifest type) + Lane C (install flow) + Lane F (install UI) | B+C+F |
| G: Plugin activation relevant-only | Lane C (plugin state machine) + Lane E (activation) | C+E |
| H: Embedded MCP no duplicate | Lane A (backfill ensures no duplicate Book rows) | A |
| I: Update/rollback v1→v2 | Lane C (upgrade + rollback) | C |
| J: Retrieval scale bounded context | Lane E (progressive loading + token metrics) | E |

## Security Checklist Mapping

| Requirement | Mechanism | Lane |
|-------------|-----------|------|
| Plugin manifest contract required | Manifest JSON schema validation; unknown → rejection | B |
| Unknown = UNTRUSTED until promoted | `plugins.trust = 'UNTRUSTED'` default; only `signature` verification promotes to `VERIFIED` | C |
| No arbitrary cloned repo execution | Only installed manifest-validated plugins execute; github source downloads to temp, validates before install | C |
| Staged updates with permission diff + approval | `plugin_installations.status = 'staged'` → operator reviews permission diff → explicit approval → activate | C |
| Secrets: vault references, never plaintext | `plugin_permissions.scope_value` for `secrets` domain stores `secret_reference_id`, not value; UI shows reference name only | C+F |
| Sandbox: non-root/rootless/dropped caps | Existing sandboxd guarantees; server configures no privileged flags | D |
| Sandbox: network NONE/RESTRICTED/FULL preserved | `plugins.network_policy` mapped to `SandboxOperation::Start.network_policy` | C+D |
| Sandbox: read-only rootfs, writable scoped workspace | Existing sandboxd guarantees | D |
| No in-process dlopen | All execution via sandboxd subprocess or MCP stdio transport through sandbox | C+E |
| Audit events per action | Every plugin install/upgrade/activation, sandbox exec, and library load creates `audit_events` | C+D+E |
| Tool input schema validation | All new tools validate input against schema before execution; reject unknown fields | E |
| Bounded output | All tools: 64 KiB max output; `library_load`: 96 KiB body; MCP tools: 16 max | E |

## Concurrency

*   Plugin install/upgrade per plugin serializes on `SELECT ... FOR UPDATE` on the `plugins` row.
*   MCP connection per server is pooled; concurrent `library_load` calls for the same MCP server
    share one connection.
*   Sandbox client uses a per-workspace connection; concurrent sandbox tool calls on the same
    workspace are serialized (sandboxd handles sequential ops per terminal).
*   Progressive loading runs inside the existing run lease — same invariants as current tool loop.

## Migration Plan

1.  **Backup**: before migration, take a `pg_dump` of the production database.
2.  **Migration 20**: single transaction DDL (add column, create tables, backfill, bump version).
    Uses `IF NOT EXISTS` guards where possible. Skill/MCP backfills use `WHERE NOT EXISTS`
    to ensure idempotent re-run.
3.  **Post-migration verify**: run `cargo test --test postgres_integration` and
    `cargo test --test worktrees_integration` against a copy of the migrated database.
4.  **Deploy**: `docker compose up -d` with the new image; app auto-migrates on startup.
5.  **Rollback plan**: restore pre-migration dump and deploy previous image.

## Verification Strategy

*   **Per-lane unit tests**: each lane includes `#[cfg(test)]` module tests for its types/functions
*   **Integration tests**: new `tests/m22_unified_library_integration.rs` covering:
    *   Migration 20 idempotency
    *   Unified search across all five book kinds
    *   `library_load` for SKILL, MCP, SOURCE, PLUGIN
    *   Plugin install flow with mock marketplace
    *   Plugin upgrade/rollback
    *   Progressive loading: search returns snippets, load returns body
    *   Token metrics event emission
*   **Sandbox integration**: existing `#[ignore]` sandboxd tests + new server→sandboxd client tests
    (gated behind `GOBROWSE_SANDBOX_DOCKER=1` or similar)
*   **CI gate**: full nextest suite + fmt + clippy + WASM clippy + migrations + cargo deny + cargo audit
*   **Retrieval quality test** (manual, post-implementation):
    *   Seed a mixed library with 50 books across all kinds, including adversarial similar descriptions
    *   Execute 10 representative tasks
    *   Verify: relevant Books found, irrelevant not loaded, only needed schemas loaded, context bounded

## Risks and Open Questions

1.  **MCP transport through sandbox**: if an MCP server needs `stdio` transport but the plugin
    runs inside a sandbox container, the stdio pipe must go through sandboxd. This requires
    sandboxd to support forwarding stdio to a child process inside the container. The current
    sandboxd supports `SandboxOperation::Start` with a command — this IS stdio. The MCP stdio
    transport can wrap that terminal session. Lane E agent should verify this mapping.
2.  **Plugin sandbox image pinning**: the sandboxd runs with `--pull=never`. Plugin images must
    be pre-pulled and their digests recorded. Lane C must handle the case where an image is not
    available locally.
3.  **GitHub API rate limits**: the `GitHubReleaseSource` hits the GitHub API. Lane C should
    implement exponential backoff and surface rate-limit errors as `MarketplaceError::RateLimited`.
4.  **Skill/MCP Book sync**: when a skill is promoted to a new revision, or an MCP server
    configuration changes, the companion Book's `body` (description) must be updated. Lane A
    should add triggers or Lane C should update the Book on skill/mcp mutation.
5.  **WASM binary size**: adding plugin marketplace types to `gobrowse-core` increases WASM size
    for the web client. The types are lightweight structs + trait definitions — traits compile
    away. Mitigation: only DTO types in core; trait implementations stay in server.
6.  **Prod podman installation**: Ubuntu 24.04 server needs `apt install podman` +
    `/etc/subuid`/`/etc/subgid` config for uid 1000. This is a deployment prerequisite,
    not a code change. Document in deployment runbook.

## Compact Lane Contract Summary

```
┌────────┬──────────────────────────────────────────────────────────────┬──────────────────────────────────────┐
│ Lane   │ Owns                                                         │ Delivers (shared types / artifacts)  │
├────────┼──────────────────────────────────────────────────────────────┼──────────────────────────────────────┤
│ Lane A │ 0020_unified_library.sql, library.rs BookKind/Book update,   │ BookKind enum, PluginState enum,     │
│        │ postgres_integration.rs + worktrees_integration.rs bump 19→20│ updated Book struct, migration 20    │
├────────┼──────────────────────────────────────────────────────────────┼──────────────────────────────────────┤
│ Lane B │ plugin.rs (manifest types, marketplace trait, PluginSource), │ PluginManifest, MarketplaceEntry,    │
│        │ lib.rs module declaration                                    │ VersionInfo, ArtifactLocation,       │
│        │                                                              │ PluginMarketplace trait,             │
│        │                                                              │ PluginSource trait, PluginPermission │
├────────┼──────────────────────────────────────────────────────────────┼──────────────────────────────────────┤
│ Lane C │ plugin_api.rs, plugin_github.rs, lib.rs route reg            │ Plugin CRUD API, GitHub install flow │
│        │                                                              │ (consumes B's traits)                │
├────────┼──────────────────────────────────────────────────────────────┼──────────────────────────────────────┤
│ Lane D │ sandbox_client.rs, config.rs FeatureSettings update,         │ SandboxClient,                       │
│        │ doctor.rs sandbox check update, lib.rs AppState              │ SandboxClientError (consumes         │
│        │                                                              │ sandbox.rs existing contracts)       │
├────────┼──────────────────────────────────────────────────────────────┼──────────────────────────────────────┤
│ Lane E │ run_api.rs (progressive loading, token metrics, tool loop),  │ 10 new Tool impls,                   │
│        │ run_tools.rs (library_load + 9 sandbox tools),               │ library_load endpoint,               │
│        │ library_api.rs (load_book endpoint)                          │ token metrics event type             │
├────────┼──────────────────────────────────────────────────────────────┼──────────────────────────────────────┤
│ Lane F │ app.rs (unified Library, plugin install stepper, terminal)   │ Unified Library page,                │
│        │                                                              │ plugin install flow, sandbox terminal│
└────────┴──────────────────────────────────────────────────────────────┴──────────────────────────────────────┘

Lane order: A → (B, D parallel) → (C, E parallel) → F
A must complete first (schema foundation). B and D are independent (core types vs server client).
C depends on B (uses traits). E depends on A, B, D (uses book kinds, sandbox client, types).
F depends on C, E (UI integrates with API).
```

## Ordered Implementation Steps (per lane)

1.  **Lane A**: Migration 20 + BookKind + test bump + verify CI
2.  **Lane B**: Plugin manifest types + marketplace trait + tests (parallel with D)
3.  **Lane D**: SandboxClient + config + doctor + test with real sandboxd (parallel with B)
4.  **Lane C**: Plugin install flow (GitHub + CRUD) + test with mock source
5.  **Lane E**: Progressive loading + library_load + sandbox tools + token metrics
6.  **Lane F**: Unified Library page + plugin install UI + sandbox terminal
7.  **Integration**: full end-to-end test suite + retrieval quality test + deploy
## PLAN AMENDMENTS (post-review, 2026-08-18)

Incorporates the independent adversarial review (CheaperCheckerPlan). MUST-FIX + ISSUE
resolutions; lanes consume these as authoritative corrections to the sections above.

### A1. PLUGIN companion Book ownership (MUST-FIX)
- Lane C creates the PLUGIN Book row in the install handler AFTER self-test passes and
  AFTER operator approval, inside the same transaction as `INSERT INTO plugins ...` +
  `UPDATE plugins SET state='dormant'`.
- Values: `book_type='INSTRUCTION'`, `kind='PLUGIN'`, `provenance='SYSTEM'`,
  `trust='USER_PROVIDED'` (operator-approved install; `'VERIFIED'` only if artifact
  signature validates), `scope='WORKSPACE'` when the plugin row has workspace_id else
  `'PROFILE'`, `security_classification='INTERNAL'`, `author='system'`,
  `metadata=jsonb_build_object('plugin_id', p.id, 'capabilities', <component names array>)`.
- The migration comment "Add a companion PLUGIN Book for each plugin (via trigger, not
  inline — see Lane B)" is DELETED from 0020. Lane B is core types only; no DB code.

### A2. Companion Book idempotency guard (ISSUE)
- Skill/MCP companion INSERT guards use text comparison, not uuid cast:
  `WHERE NOT EXISTS (SELECT 1 FROM books b WHERE b.metadata->>'skill_id' = s.id::text AND b.kind='SKILL')`
  (same for `mcp_server_id`). Prevents cast ERROR on malformed metadata.

### A3. Phase-2 search must preserve authorization + NULL-kind semantics (security ISSUE)
- Lane E's unified search SQL MUST include the same tenant predicates the current
  `library_api::search_books` enforces (profile_id scoping, workspace authorization via
  `authorize_workspace`, conversation authorization) — copy the existing WHERE structure.
- Kind filter: `AND ($3::text IS NULL OR kind = $3::text)` AND include NULL-kind rows for
  the SOURCE filter: `AND (kind = 'SOURCE' OR kind IS NULL)` when filtering SOURCE; for
  ALL filters show `kind IS NULL OR kind IN ('SOURCE','SKILL','MCP','PLUGIN')` (NOT just
  `kind IN (...)`, which silently drops legacy NULL-kind books).

### A4. Skill/MCP companion Book sync (ISSUE → triggers in 0020)
- 0020 adds AFTER UPDATE triggers so companion Book body stays in sync with the source row:
  - `skill_book_sync` ON skills AFTER UPDATE OF description: `UPDATE books SET body=NEW.description, updated_at=now() WHERE kind='SKILL' AND metadata->>'skill_id'=NEW.id::text`
  - `mcp_book_sync` ON mcp_servers AFTER UPDATE: `UPDATE books SET body=COALESCE(NEW.configuration->>'description', NEW.transport || ' MCP server'), updated_at=now() WHERE kind='MCP' AND metadata->>'mcp_server_id'=NEW.id::text`
  - `skill_book_delete` ON skills AFTER DELETE: `DELETE FROM books WHERE kind='SKILL' AND metadata->>'skill_id'=OLD.id::text`
  - `mcp_book_delete` ON mcp_servers AFTER DELETE: `DELETE FROM books WHERE kind='MCP' AND metadata->>'mcp_server_id'=OLD.id::text`
- CREATE OR REPLACE FUNCTION bodies inline in 0020 (boring plain SQL).

### A5. MCP client gap (coverage ISSUE)
- Confirmed: `mcp_api.rs` is metadata-only ("No process spawning or MCP transport
  connection"). Lane E's `library_load` for MCP books needs an actual client. Lane E owns
  a NEW `crates/gobrowse-server/src/mcp_client.rs`: wraps `gobrowse_core::mcp` stdio
  transport (McpStdioTransport) or streamable HTTP for `tools/list` + `tools/call`,
  bounded (≤16 tools, ≤64 KiB schema total), per-server connection pool, timeout 30s.
  Auth: pass `auth_secret_reference` from vault; PKCE/OAuth flows remain future (MCP books
  that require OAuth report "auth required" instead of failing hard). Plugin-embedded MCP:
  connection spawned via sandboxd Start (stdio through the sandbox terminal session) —
  implement the plain stdio path first, sandbox-forwarded path second (journey H).
- Lane E is expanded to own mcp_client.rs; the lane brief below is updated accordingly.

### A6. Plugin state/provenance checks (from review)
- `plugins.trust` valid values: VERIFIED/USER_PROVIDED/AGENT_INFERRED/EXTERNAL/UNTRUSTED
  (matches books.trust CHECK — good). New installs start `trust='UNTRUSTED'` in `plugins`
  row; after operator approval the row's trust flips to `'USER_PROVIDED'` (or VERIFIED on
  valid signature) and the companion Book mirrors it.
- Popularity (download counts) is stored only in marketplace search results (MarketplaceEntry),
  never merged into books.trust or plugins.trust.

### A7. Lane re-sequencing after review
- Wave 2 (parallel, after Lane A merges): Lane B (core plugin/manifest/marketplace types)
  + Lane D (sandbox_client + config + doctor).
- Wave 3 (parallel, after B+D): Lane C (plugin install server flow, owns PLUGIN Book
  creation per A1) + Lane E (progressive loading + library_load + sandbox tools + mcp_client
  per A5 + token metrics; search per A3).
- Wave 4: Lane F (UI) after C+E routes exist.
- Lane A must incorporate A2 (text comparison guards) and A4 (sync triggers) in 0020.

### A8. ProvisionWorkspace protocol op (found in daemon verification — MUST-FIX)
- Verified: daemon Start requires `filesystem.workspace_storage(id)` to pre-exist
  (daemon.rs:537-541) and `start_spec` requires `WorkspaceProvisioning::NamedVolume`
  (runtime.rs:520-541). `ensure_workspace` is test-only; NO production provisioning path
  exists. The app must provision workspace storage through the protocol, never by
  touching the shared FS.
- Add to `gobrowse_core::sandbox::SandboxOperation`:
  `ProvisionWorkspace { workspace_id: Uuid }` → `SandboxResult::Provisioned { workspace_id }`.
  Daemon handler (daemon.rs, next to Health): `filesystem.ensure_workspace(workspace_id)?;
  Ok(SandboxResult::Provisioned{..})`. Idempotent (ensure_workspace handles EXIST).
  Validated by `workspace_id.is_nil()` rejection + bounded payload (already guaranteed).
- Lane D owns: core enum variant + SandboxResult variant + daemon handler + client
  `provision_workspace()` method + audit + tests. Deployed sandboxd must run with
  `--quota-managed-workspaces`.
- Server flow: workspace creation in the app → on first sandbox use (or eagerly at
  workspace create when sandbox enabled) call ProvisionWorkspace once; failure surfaces
  as actionable error ("sandbox unavailable — daemon not reachable or not provisioned").
# PLAN.md — M23: Adaptive Capability Router + Context Intelligence

Authoritative architecture and lane contracts for M23. Prior M22 section and amendments are
preserved and remain authoritative. This batch covers *architecture design only*; zero
implementation.

## Goal

Teach Gobrowse WHAT to load and use. Replace the current "search Library → dump all matched
summaries into context + let the model figure it out" flow with a deliberate, traceable
routing pipeline:

    request → classify task → search Library → rank capabilities → select minimum
    required Books → select model → activate plugin/MCP if needed → execute

Every routing decision is explainable to the user in plain language. The UI must display
context budget per category, why-loaded reasons, model-routing explanation, and activation
status — all from real state, never fake.

## Current State (observed facts — what M22 delivered that M23 builds on)

### Unified Library (schema 21)

*   `books` table has `kind` column (`SOURCE|SKILL|MCP|PLUGIN|AUTOBIOGRAPHY`; NULL = SOURCE).
*   Companion Book proxies exist for every Skill (`INSERT` trigger in 0021) and MCP server.
*   Plugin companion Books created in Lane C install handler (A1 amendment).
*   `book_links` with `PROVIDES|CONTAINS|...` relations — unused by routing logic today.

### Search and ranking (`library_api.rs`)

*   Hybrid lexical + semantic search via `search_books`: PostgreSQL FTS (`ts_rank_cd`) +
    optional pgvector cosine similarity, fused with weighted RRF (`RankingWeights`:
    `lexical: 1.0, semantic: 1.0, recency: 0.004, source: 0.003, workspace: 0.005, rrf_k: 60`).
*   Recency bonus decays `1/(1+age_days/30)`, trust bonus via enum mapping
    (`VERIFIED=1.0, USER_PROVIDED=0.8, AGENT_INFERRED=0.5, EXTERNAL=0.3`), workspace
    match bonus = `1.0` binary.
*   No capability-match signal, no past-success signal, no token-cost signal.
*   `BookSummary` DTO includes `capabilities: Vec<String>` (extracted from
    `metadata->>'capabilities'`), `relevance: f32`, `retrieval_mode: String`,
    `lexical_score: Option<f32>`, `semantic_score: Option<f32>`.

### Context assembly (`run_api.rs::build_messages`)

*   Bounded by `limits.context_window - limits.output_limit` tokens.
*   Phase 1: system policy + last 40 recent messages (truncated to 2/3 budget).
*   Phase 2: unified Library search — `SELECT` with `websearch_to_tsquery`, `ts_rank_cd`
    ordering, LIMIT 12, snippet-only (never full body). Candidates tagged as
    `ContextSource::LibraryRetrieval` (priority 300, required: false). Auth predicates
    applied (A3 amendment).
*   Phase 3: pinned books (priority 400) + worktree (priority 250).
*   Context budget allocation: greedy, not per-category. No reserved budgets per source kind.
*   Context snapshot persisted as `agent_runs.context_snapshot` JSONB: `selected` (array
    of stable IDs), `omitted` (array of IDs), `used_tokens`, `budget`, `recent_messages`.

### Progressive loading (`run_tools.rs` + `library_api.rs`)

*   `library_load` tool: resolves full body/schemas on demand. Bounded: ≤5 loads/run,
    ≤24k chars SOURCE, ≤24k chars SKILL content, ≤16 MCP tools, ≤64 KiB total.
*   Sandbox tools: 10 native tools (`sandbox_exec`, `sandbox_read_file`, …,
    `terminal_start`) via `SandboxClient`.
*   `RunTokenMetrics` tracks: `book_searches`, `books_considered`, `books_loaded`,
    `book_tokens_loaded`, `skill_book_loads`, `plugin_book_loads`, `mcp_book_loads`,
    `source_book_loads`, `plugin_tools_discovered`, `plugin_tools_loaded`,
    `mcp_tools_discovered`, `mcp_tools_loaded`.
*   Persisted as `run_events` row with `event_type = 'token_metrics'` at run completion.

### Model routing (`chat.rs` + `model_api.rs`)

*   `load_routes`: selects primary model (active_chat_model_id or highest priority),
    attaches `model_fallback_routes` as ordered cascade. Filtered to `'text'`-capable,
    enabled models, and known provider types (OpenAI-compatible, ollama).
*   `model_limits`: uses min(context_window) and min(output_limit) across the route
    chain as the budget.
*   Model selection is **task-unaware**: same primary model + same fallback chain for
    every request regardless of task nature (coding, research, shell, general QA).
*   Selected model persisted as `agent_runs.selected_model_id` +
    `run.model_selected` event with `{model_id}`.

### Web app (`app.rs`)

*   Chat page with streaming text deltas, tool-call inline rendering, run state
    (queued → awaiting_model → completed/failed/canceled), cancellation.
*   Unified Library page with kind filter tabs (ALL|SOURCE|SKILL|PLUGIN|MCP),
    search, progressive-load detail panel (`LoadedBookDetail`).
*   Plugin detail + install stepper + sandbox terminal (Lane F).
*   No context-inspector panel; no budget-breakdown UI; no model-routing explanation.

### Schema version

*   Current schema version: **21** (`0021_companion_book_inserts.sql`).
*   Test assertions: `postgres_integration.rs` and `worktrees_integration.rs` assert
    `schema_version == 21`.

### Constraints and invariants

*   Schema version MUST become **22** (one forward migration `0022_router.sql`).
*   `unsafe_code = "forbid"`, edition 2024, Rust 1.94, nextest, forward-only migrations.
*   Leptos CSR/WASM — reactive signals, no SSR, `gloo_net` fetch, `localStorage` for
    cross-page state (pending submission, active run, open terminal).
*   All HTTP requests through `gloo_net::Request`. WASM binary size budget preserved.
*   No new native dependencies; model-based classification uses the SAME model
    pipeline as chat (no separate classifier binary/endpoint).
*   `token_metrics` run event already exists — M23 extends its payload schema; never
    creates a new event type.
*   `context_snapshot` JSONB on `agent_runs` already exists — M23 extends its payload
    to include per-category budgets and routing decisions.
*   Library search authorization predicates MUST match existing `search_books` +
    `build_messages` Phase-2 predicates (A3 amendment); ranking is applied downstream
    of authorized rows.
*   Progressive loading bounds (5 loads/run, 24k chars, 16 tools, 64 KiB) remain.
*   No raw embedding scores, RRF coefficients, or similarity distances in primary UI.

## Architectural Decisions

### 1. Task classification is lightweight and rule-first, with optional model fallback

**Rationale**: A dedicated classifier model/endpoint adds latency, cost, and a new
failure mode. The dominant case (80%+ of requests) can be classified by a small set of
deterministic rules on the user message text: presence of code fences/backticks →
`coding`; shell commands (`$ `, `> `, `curl`, `git`, `npm`, `cargo`) → `shell_automation`;
URLs + "summarize"/"research"/"find" → `research`; file paths + "create"/"write" →
`document_creation`; "pay"/"buy"/"purchase"/"checkout" → `ecommerce`; fallback
`general_qa`. A model-based classifier (one short inference call using the chat model
pipeline with a classification prompt + constrained output) runs only when rules produce
`general_qa` or confidence < 0.9. This preserves the simplicity of the existing
`build_messages` flow while adding task awareness.

**Task classes** (closed set, stable):
`coding`, `research`, `data_analysis`, `document_creation`, `general_qa`,
`shell_automation`, `ecommerce`, `system_administration`.

### 2. Capability ranking extends the existing RRF pipeline with task-aware signals

**Rationale**: The existing `rank_fusion` in `library_api.rs` already computes a
composite score from lexical, semantic, recency, trust, and workspace signals. M23
extends this to include capability-match and past-success signals — computed entirely
in-memory (no new DB round-trips beyond the existing search query). The `book_links`
table is never in the hot path (too slow); capability mapping is driven by the existing
`metadata->>'capabilities'` JSONB array + a new `task_capability_map` static lookup
table in `gobrowse_core`.

**New ranking signals** (all computed in-memory after DB search):

| Signal | Weight | Source | Computation |
|--------|--------|--------|-------------|
| Capability match | `0.05` | `metadata->>'capabilities'` ∩ `task_capability_map[task_class]` | Jaccard similarity, clamped `[0,1]` |
| Past success | `0.002` | `book_usage_stats` (new in-memory cache, backed by DB) | Success rate (loads that produced tool usage / total loads), 0.5 default for unseen |
| Token cost proxy | `-0.001` | Book `kind` + estimated body size | SKILL ≤ SOURCE ≤ PLUGIN ≤ MCP (MCP penalized because activation is expensive) |
| Permission risk | `-0.003` | `security_classification` + `trust` | RESTRICTED + UNTRUSTED = -1.0 penalty; CONFIDENTIAL + EXTERNAL = -0.5 |

The existing `RankingWeights` struct gains new fields. The existing `rank_fusion`
function gains a `task_class: Option<TaskClass>` parameter (None = backward-compatible).

### 3. Model selection is task-aware via a new `model_task_routes` table

**Rationale**: The existing `model_fallback_routes` is a profile-wide static
primary→fallback chain. Different tasks need different models: coding benefits from
models with strong tool-calling and large context windows; general QA works fine with
cheaper models. M23 introduces `model_task_routes` — a task-specific override that
sits between the primary model selection and the existing fallback chain. When the task
class has a configured route, that preferred model becomes the primary (the existing
fallback chain is still used if the preferred model fails). When nothing is configured,
the existing path (active_chat_model_id → priority → fallback routes) is used unchanged.

**Explainable model selection**: The `run.model_selected` event payload gains a
`routing_reason` field: `"task 'coding' prefers model X over default Y; X is enabled
and meets capability requirements (tool_calls, context ≥ 128k)"`. In the default case:
`"no task-specific routing configured; using profile default (highest-priority enabled
chat model)"`.

### 4. Activation is anticipatory, not reactive

**Rationale**: The current M22 flow is reactive: the model sees book summaries, decides
to call `library_load`, THEN MCP/plugin activation happens on load. This wastes a
round-trip. In M23, during the ranking phase (after DB search, before context assembly),
the system identifies the top-ranked books by kind. If a plugin/MCP book ranks in the
top 3 and has `kind=PLUGIN|MCP`, the system pre-activates it: resolves tools/schemas
BEFORE the first model request, attaches tool definitions directly to the initial
`ModelRequest`. This eliminates one agent round-trip for the common case. Books outside
the top 3 are still loaded on-demand (existing `library_load` path unchanged).

Activation is bounded: at most 2 pre-activations per run (to prevent runaway startup),
and only for books with `trust >= USER_PROVIDED` (never auto-activate UNTRUSTED plugins).

### 5. Routing decisions are persisted in the context_snapshot, not a separate table

**Rationale**: The existing `agent_runs.context_snapshot` JSONB column already records
what was selected and omitted. Rather than adding a new `routing_decisions` table
(which complicates queries, adds migration risk, and duplicates the run lifecycle),
M23 extends the `context_snapshot` payload to include routing decisions inline. The
`run.context_built` event payload also gains routing fields. This means the full routing
trace is available to the UI via a single `GET /runs/:id` query + `list_run_events`.

Extended `context_snapshot` shape:

```json
{
  "task_class": "coding",
  "task_class_source": "rule",
  "selected": ["system-policy-v1", "book-uuid-1", "book-uuid-2"],
  "omitted": ["book-uuid-3"],
  "budget": {"total": 128000, "used": 8234, "by_category": {
    "conversation": 3200, "source_books": 1800, "skill_books": 1200,
    "plugin_books": 0, "mcp_schemas": 0, "workspace": 450, "system_policy": 1584
  }},
  "routing": {
    "books_ranked": 12,
    "books_selected": 2,
    "pre_activated": ["plugin-uuid-1"],
    "decisions": [
      {"book_id": "book-uuid-1", "action": "selected", "reason": "Capability match (git, repository); workspace-linked; high trust (USER_PROVIDED); recency 2d."},
      {"book_id": "book-uuid-2", "action": "selected", "reason": "Semantic relevance (deployment runbook); moderate trust (USER_PROVIDED); low token cost (~800 tokens)."},
      {"book_id": "book-uuid-3", "action": "omitted", "reason": "RESTRICTED classification requires ADMIN role not available in this context."}
    ],
    "model": {
      "selected": "claude-sonnet-4-20250514",
      "reason": "task 'coding' prefers model 'claude-sonnet-4-20250514' (configured in model_task_routes); meets capability requirements (tool_calls, context 200k); escalation none."
    },
    "activation": {
      "plugin-uuid-1": {"status": "activated", "tools_resolved": 3, "reason": "Top-3 ranked plugin Book; request involved repository operations matching component capabilities (pull_requests, issues)."}
    }
  },
  "used_tokens": 8234,
  "budget": 128000,
  "recent_messages": 12
}
```

### 6. Context budget accounting is per-category with soft reservations

**Rationale**: The current greedy allocation can starve certain categories (e.g.,
worktree info pushes out all library content). M23 introduces soft budget reservations:
conversation history gets ≤67% of budget (existing behavior), library retrieval shares
the remaining 33% across categories with minimum guarantees: source books ≥10% of
remaining, skill books ≥10%, plugin books + MCP schemas ≥5% each, workspace ≥5%. These
are soft floors; when a category has no candidates, its reservation is redistributed.

The per-category token counts are tracked in `RunTokenMetrics` (new fields:
`source_book_tokens`, `skill_book_tokens`, `plugin_book_tokens`, `mcp_schema_tokens`,
`workspace_tokens`) and emitted in the `token_metrics` event + included in the
`context_snapshot`.

### 7. "Why" reasons are machine-generated plain-language strings, not templates

**Rationale**: Template-based reasons ("Book X loaded because it matched your query")
are stale and unhelpful. M23 generates plain-language `reason` strings at routing time
by assembling signal-specific clauses. Each clause maps to a concrete, verifiable fact:

| Signal | Clause pattern |
|--------|---------------|
| Capability match | "Capability match (pull_requests, issues)" |
| Semantic relevance | "Semantic relevance (deployment runbook)" |
| Trust | "high trust (USER_PROVIDED)" / "low trust (EXTERNAL)" |
| Recency | "recently updated (2d ago)" / "stale (90d)" |
| Scope | "workspace-linked" / "profile-wide" |
| Token cost | "low token cost (~800 tokens)" / "high token cost (~6k tokens)" |
| Permission | "RESTRICTED classification requires ADMIN role" |
| Past success | "previously used successfully (3/3 loads)" |

Clauses are joined with semicolon separators. No raw scores. The UI renders these
verbatim. The reason strings live in the `context_snapshot.routing.decisions[].reason`
field — generated once at routing time, persisted, never recomputed.

### 8. Book usage statistics are lazily maintained in a new lightweight table

**Rationale**: Past-success ranking needs per-book load/use counts. A full-blown
analytics table is overkill. M23 adds `book_usage_stats` — a compact aggregate table
updated via a PostgreSQL trigger on `run_events` when `event_type = 'token_metrics'`:
increment `total_searches`, `total_loads`, `total_tool_uses` for each book. The
in-memory cache in the server reads this table once per run (a single `SELECT` for all
candidate book IDs) and computes a simple success rate. No new infrastructure; trigger
is boring `AFTER INSERT ON run_events FOR EACH ROW`.

### 9. The router is a new `router` module in `gobrowse-server`, not in `gobrowse-core`

**Rationale**: Task classification, capability ranking, and model selection are
server-side concerns that access the database, model pipeline, and sandbox client.
They do not belong in `gobrowse-core` (which is shared with WASM). The `router` module
(`crates/gobrowse-server/src/router.rs`) owns: `TaskClass` enum, `classify_task()`,
`rank_capabilities()`, `select_model_for_task()`, `generate_routing_reasons()`,
`pre_activate_capabilities()`. Core only receives: `TaskClass` enum (lightweight,
serializable), extended `RankingWeights`.

### 10. The UI Context Inspector is a progressive-disclosure panel, not a new page

**Rationale**: A separate "Run Inspector" page fragments the chat experience. M23 adds
a Context Inspector panel that slides in from the right (or toggles below at ≤768px
viewport). It is always available during an active run (via a "Context" chip/button in
the chat header) and remains accessible in the run history view. The panel renders
directly from `context_snapshot` (via `GET /runs/:id`) and `run_events` (via
`GET /runs/:id/events`). Three tabs/sections: Budget (bar chart per category + total
used/limit), Why Loaded (list of decisions with reasons), Model (selected model +
routing explanation). Quiet by default; details on demand.

## Data Model / Migration Sketch (Schema 21 → 22)

### Migration `0022_router.sql`

```sql
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
```

### Core type changes (`gobrowse_core::library`)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskClass {
    Coding,
    Research,
    DataAnalysis,
    DocumentCreation,
    GeneralQA,
    ShellAutomation,
    Ecommerce,
    SystemAdministration,
}

// Extended RankingWeights (new fields appended; Default preserves existing values)
pub struct RankingWeights {
    pub lexical: f32,         // 1.0
    pub semantic: f32,        // 1.0
    pub recency: f32,         // 0.004
    pub source: f32,          // 0.003
    pub workspace: f32,       // 0.005
    pub rrf_k: f32,           // 60.0
    // M23 additions:
    pub capability_match: f32, // 0.05
    pub past_success: f32,     // 0.002
    pub token_cost: f32,       // -0.001
    pub permission_risk: f32,  // -0.003
}
```

### Extended RunTokenMetrics (new fields added to existing struct)

```rust
pub struct RunTokenMetrics {
    // ... existing M22 fields (book_searches, books_considered, books_loaded, ...) ...
    // M23 additions:
    pub task_class: Option<TaskClass>,
    pub source_book_tokens: u64,
    pub skill_book_tokens: u64,
    pub plugin_book_tokens: u64,
    pub mcp_schema_tokens: u64,
    pub workspace_tokens: u64,
    pub pre_activations: u32,
    pub pre_activation_tools_resolved: u32,
    pub book_usage: Vec<(Uuid, u32, u32, u32)>, // (book_id, searches, loads, tool_uses)
}
```

## API Surface

### New endpoints

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/runs/:id/context` | Returns the `context_snapshot` + routing decisions for a completed/active run. Auth: conversation member. |
| `GET` | `/runs/:id/model-routing` | Returns the model selection explanation for a run (shorthand for the `routing.model` portion of context). |
| `GET` | `/models/task-routes` | List configured `model_task_routes` for the profile. |
| `PUT` | `/models/task-routes` | Set/update task-class → preferred model mappings. Body: `{ task_class, model_id }`. Auth: ADMIN/OWNER. |
| `DELETE` | `/models/task-routes/:task_class` | Remove a task-class routing override. |

### Extended existing endpoints

*   `GET /runs/:id` — `RunResponse` gains optional `task_class` field (string, null when
    run predates M23 or classification failed).
*   `GET /runs/:id/events` — No change (routing decisions live in `context_snapshot`,
    not as separate events).
*   `POST /conversations/:id/turn` — No change (routing is transparent; the response
    shape is unchanged — `RunResponse` now carries `task_class`).

### Response shape: `GET /runs/:id/context → 200`

```json
{
  "task_class": "coding",
  "task_class_source": "rule",
  "budget": {"total": 128000, "used": 8234, "by_category": {
    "conversation": 3200, "source_books": 1800, "skill_books": 1200,
    "plugin_books": 0, "mcp_schemas": 0, "workspace": 450, "system_policy": 1584
  }},
  "decisions": [
    {"book_id": "...", "title": "GitHub Plugin", "kind": "PLUGIN", "action": "selected",
     "reason": "Capability match (pull_requests, issues); workspace-linked; high trust (USER_PROVIDED)."},
    {"book_id": "...", "title": "Deployment Runbook", "kind": "SOURCE", "action": "selected",
     "reason": "Semantic relevance; low token cost (~800 tokens); moderate trust (USER_PROVIDED)."},
    {"book_id": "...", "title": "Secret Admin Script", "kind": "SKILL", "action": "omitted",
     "reason": "RESTRICTED classification requires ADMIN role not available."}
  ],
  "model": {
    "model_id": "claude-sonnet-4-20250514",
    "reason": "task 'coding' prefers model 'claude-sonnet-4-20250514'; meets capability requirements (tool_calls, context 200k); escalation none."
  },
  "activation": {
    "plugin-uuid-1": {"status": "activated", "tools_resolved": 3,
     "reason": "Top-3 ranked plugin Book; request involved repository operations."}
  }
}
```

## UI Contract

### Context Inspector panel (`ChatPage` extension)

The Context Inspector is a slide-in panel accessible from the chat header during an
active run, and from the run history view after completion. Three tabs:

1.  **Budget tab**: Horizontal stacked bar showing per-category token usage vs total
    budget. Labels: Conversation, Source Books, Skill Books, Plugin Books, MCP Schemas,
    Workspace, System Policy. Each segment shows token count (e.g., "1.8k"). The bar
    fills from 0 to total budget. Percentages underneath.

2.  **Why Loaded tab**: Scrollable list of routing decisions. Each card shows:
    kind icon + book title, action badge (green="selected", gray="omitted"), and the
    plain-language reason string. Clicking a selected book navigates to its Library
    detail.

3.  **Model tab**: Model display name + provider, the routing reason string (plain
    language), and context window / output limit info.

**Progressive disclosure contract**:
- Default state: a small "Context: 8.2k / 128k" chip in the chat header (no panel open).
  The chip is always visible during an active run.
- Clicking the chip opens the panel (slide from right at ≥1024px; full-width overlay
  at <1024px).
- Real state only: if a run has no routing decisions (pre-M23 run), the panel shows
  "Run predates adaptive routing (M22 or earlier)" with the raw context_snapshot
  available under an "Advanced" toggle.
- No raw scores, RRF coefficients, or embedding distances in the normal panel.
  An "Advanced / Debug" toggle (off by default, persisted per-session in localStorage)
  reveals lexical_score, semantic_score, and rank position.
- At 390px viewport: panel is full-width overlay, tabs stack vertically, bar chart
  switches to a simple text list.

### Routing activity in chat (real observable events)

During an active run, the chat UI emits small "router activity" inline notes between
the user message and the first assistant text delta. These are NOT fake — they are
backed by `run_events` of type `run.context_built` (which now carries the routing
payload). Examples:

```
🔍 Task classified as "coding" (rule-based)
📚 Loaded 2 of 12 matching books (GitHub Plugin, Deployment Runbook)
🧩 Pre-activated GitHub Plugin (3 tools)
🧠 Selected claude-sonnet-4-20250514 (task routing)
```

These are rendered as small, muted, collapsible inline notes; they do not block the
assistant stream. If the user clicks one, the Context Inspector opens to the relevant tab.

### Model chip (persistent in chat header)

The chat header always shows the currently selected model as a small chip:
`claude-sonnet-4-20250514`. Hovering shows the routing reason tooltip. During a run,
if the model changes (fallback escalation), the chip updates with a subtle animation
and the tooltip shows the escalation reason.

### Empty states

- Budget tab when no run is active: "Start a conversation to see context usage."
- Why Loaded tab when no books were loaded: "No capability books were loaded for this request."
- Model tab when run predates M23: "Model routing predates adaptive selection (M22 or earlier)."
- Model task routes not configured: "No task-specific model routing configured. Using profile default."

### Approval UX

Plugin pre-activation requires no user approval (it's deterministic based on ranking +
trust ≥ USER_PROVIDED). If an UNTRUSTED plugin ranks in the top-3, it is NOT
pre-activated; the system instead emits a run event `run.approval_required` with the
plugin details. The UI shows an inline approval card: "GitHub Plugin (UNTRUSTED) could
help with this task. [Activate] [Ignore]" — clicking Activate triggers the activation
and the run continues (an `approval` action endpoint resumes the paused run). This is
a future UX path; the immediate M23 design only specifies the data flow and event type.

### Responsive contract

| Viewport | Panel behavior | Bar chart | Decision cards |
|----------|---------------|-----------|----------------|
| ≥1024px | Slide-in right, 380px wide | Horizontal stacked bar | Full cards with kind icons |
| 768–1023px | Slide-in right, 320px wide | Horizontal stacked bar | Compact cards |
| 390–767px | Full-width overlay | Text list (no bar) | Compact cards, smaller reason text |

## Implementation Checklist (ordered, batch-sized)

### Batch 1: Core types + migration (schema 21 → 22)

1.  Add `TaskClass` enum to `gobrowse-core/src/library.rs`.
2.  Extend `RankingWeights` with new fields; update `Default` impl.
3.  Add `task_capability_map()` static lookup in `gobrowse-core`.
4.  Write `0022_router.sql` migration: `model_task_routes`, `book_usage_stats`, trigger, bump to 22.
5.  Extend `RunTokenMetrics` with new fields.
6.  Bump schema version assertions in `postgres_integration.rs` and `worktrees_integration.rs` (21 → 22).
7.  Run migration against test DB; verify idempotent re-run.

### Batch 2: Router module (server-side)

1.  Create `crates/gobrowse-server/src/router.rs`: `classify_task()`, `rank_capabilities()`,
    `select_model_for_task()`, `generate_routing_reasons()`, `pre_activate_capabilities()`.
2.  Implement rule-based classification (pattern matching on user message text).
3.  Implement model-based classification fallback (one inference call with classification prompt).
4.  Extend `build_messages` to call the router:
    - Classify task before Phase 2 search.
    - Pass `task_class` to modified `rank_fusion` call.
    - Pre-activate top-ranked plugin/MCP books.
    - Select model via `select_model_for_task()`.
    - Build extended `context_snapshot` with routing decisions.
5.  Extend `library_load` tool and `search_books` to accept optional `task_class` for ranking.
6.  Update `RunTokenMetrics` tracking throughout `execute_inner`.

### Batch 3: API endpoints

1.  `GET /runs/:id/context` — read `context_snapshot` from `agent_runs`, parse routing payload.
2.  `GET /runs/:id/model-routing` — shorthand for routing.model portion.
3.  `GET /models/task-routes` — list `model_task_routes` rows.
4.  `PUT /models/task-routes` — upsert a task→model mapping. Auth: ADMIN/OWNER.
5.  `DELETE /models/task-routes/:task_class` — remove mapping.
6.  Extend `RunResponse` with `task_class` field.
7.  Extend `model_limits` to accept optional `task_class` (uses preferred model when configured).

### Batch 4: UI — Context Inspector + routing activity

1.  Add `ContextInspector` component to `app.rs`: slide-in panel with Budget/Why Loaded/Model tabs.
2.  Context chip in chat header: reads `active_run` → fetches `GET /runs/:id/context` →
    renders "Context: 8.2k / 128k".
3.  Routing activity inline notes: listen for `run.context_built` events, render collapsible notes.
4.  Model chip with tooltip: show selected model + routing reason.
5.  Progressive disclosure: default quiet, details on demand; Advanced toggle in localStorage.
6.  Responsive layout: 1440/1024/768/390 breakpoints.
7.  Empty states for all three tabs.
8.  Pre-M23 run handling: "predates adaptive routing" message with raw context_snapshot under Advanced toggle.

### Batch 5: Book usage statistics + past-success signal

1.  Wire `book_usage_stats` trigger (already in migration; validate it fires correctly).
2.  In-memory cache in router: load stats for candidate book IDs, compute success rate.
3.  Wire past-success signal into `rank_capabilities()`.
4.  Extend `token_metrics` event payload with `book_usage` array.

### Batch 6: Integration tests + verification

1.  `tests/m23_router_integration.rs`: task classification, capability ranking, model selection,
    pre-activation, context snapshot shape, budget accounting.
2.  Test idempotent migration 22.
3.  Test backward compatibility: pre-M23 run context_snapshot still loads correctly.
4.  Test model-based classification fallback.
5.  Test pre-activation bounds (max 2, UNTRUSTED skip).
6.  CI gate: nextest + fmt + clippy + WASM clippy + migrations.

## Validation Steps

1.  **Migration**: forward from schema-21 DB; `model_task_routes` and `book_usage_stats` tables exist;
    trigger fires on `token_metrics` insert; schema version = 22.
2.  **Task classification**: "Write a Rust function to parse JSON" → `coding` (rule, code fences);
    "Summarize this article https://..." → `research` (rule, URL + summarize);
    "What's the capital of France?" → `general_qa` (rule, no strong signal).
3.  **Capability ranking**: seed library with mixed books; verify capability-matched books rank
    higher than pure-FTS when task class matches.
4.  **Model selection**: configure `model_task_routes` for `coding` → `claude-sonnet-4`;
    start coding task; verify selected model is Claude, reason mentions task routing.
5.  **Pre-activation**: plugin in top-3 + trust ≥ USER_PROVIDED → tools appear in first
    model request without explicit `library_load`.
6.  **Context Inspector**: active run → chip shows budget; click → Budget tab bar chart
    renders; Why Loaded shows decisions with reasons; Model tab shows routing explanation.
7.  **No fake data**: every fact in the panel is traceable to a DB row or event payload.
8.  **Backward compatibility**: pre-M23 runs show "predates adaptive routing"; context chip
    still shows budget from `context_snapshot.used_tokens`.
9.  **Responsive**: panel works at 390px (full-width overlay, text list).
10. **No raw scores in primary UI**: verify lexical_score/semantic_score/rank position
    only visible under Advanced toggle.

## Open Questions

1.  **Model-based classification cost**: one extra inference call per run for ambiguous
    requests. Mitigation: classify only on the first turn (cache per conversation_id until
    topic drift detected). If classification fails or times out (5s timeout), fall back to
    `general_qa` (the existing behavior). Q: is the cost acceptable for the UX improvement?
    A: classification prompt is ~200 tokens input, ~10 tokens output — negligible vs the
    main run. Acceptable.

2.  **Topic drift detection**: if a conversation shifts from "coding" to "deployment",
    should the router reclassify? Initial design: classify once per conversation (cached
    in `TaskClassCache` HashMap keyed by `conversation_id`, invalidated after 5
    non-trivial user messages). Future: reclassify on detected topic drift (keyword
    shift above threshold). Not in M23.

3.  **model_task_routes UI**: where does the operator configure task→model mappings?
    The existing Models page (`ModelsPage` component) is a natural fit — add a
    "Task Routing" section below the model list. This is Batch 4 scope.

4.  **Pre-activation of MCP books**: MCP activation requires a transport connection
    (stdio spawn or streamable HTTP). If this takes >2 seconds, it delays the first
    model request. Mitigation: pre-activation runs in a `tokio::spawn` with a 5-second
    timeout; if it doesn't complete before the model request is assembled, the MCP
    tools are omitted from the first round and loaded on-demand in round 2 (existing
    `library_load` path). The activation result is then attached to round 2's tool
    definitions. Silent degradation, not a failure mode.

5.  **WASM bundle size**: adding `TaskClass` enum and extended `RankingWeights` to
    `gobrowse-core` increases WASM size. Estimate: `<2 KB` (one enum, a few f32 fields,
    a `HashMap` for capability map — the map is compiled to a static slice). Acceptable.

6.  **Existing M22 runs**: the `context_snapshot` for pre-M23 runs lacks `routing`,
    `budget.by_category`, and `task_class` fields. The UI handles this with explicit
    "predates M23" empty states. No migration of old snapshots is required (they are
    immutable historical records).
---
---

# PLAN.md — M24: Full Token, Runtime, Container & Architecture Optimization

Authoritative architecture and implementation plan for M24. M22 and M23 are complete and deployed
(schema version 22). This milestone makes Gobrowse dramatically lighter without losing capability.
Same-or-better capability, less context/RAM/disk/CPU/latency/smaller containers/simpler code.
M24 optimizations are the permanent baseline — all subsequent milestones inherit them.

## Goal

Optimize Gobrowse OS architecture across every measurable dimension: WASM binary size, Docker
image size, idle RSS, startup time, search/chat latency, build time, token usage per run, and
dependency footprint. Non-negotiable: do NOT remove features. Pay for capabilities only when
needed. Keep boring, LLM-editable code. Maintain an architecture map. Stress-test with many
Books/plugins (context and RAM must not scale with installed count). Requires before/after
benchmark table and capability regression matrix.

## Constraints

*   **No feature removal.** Every M23 API endpoint, tool, and UI page must remain available.
*   **Schema version unchanged** (22). No new migrations unless strictly required for
    performance (e.g. a covering index). Schema changes require explicit justification.
*   **`unsafe_code = "forbid"`**, edition 2024, Rust 1.94, nextest, forward migrations.
*   **Every batch is independently deployable** and can be verified against baselines.
*   **M24 is permanent.** All subsequent milestone builds (M24b, M25, …) MUST use the same
    optimization standards. M24 sets the floor, not a ceiling.
*   **Boring code.** No exotic abstractions, no new frameworks, no proc macros beyond what
    already exists. The codebase must remain LLM-editable.

## Current State (observed facts — M24 pre-implementation)

### Artifact sizes (reported)
*   Docker image: ~66 MB gzipped (multi-stage: `rust:1.94-bookworm` → `debian:bookworm-slim`)
*   WASM bundle: ~12 MB (trunk build, no wasm-opt, no code splitting)
*   Server binary: release profile with `lto = "thin"`, `codegen-units = 1`, `strip = "symbols"`,
    `panic = "abort"`. No `wasm-opt` invocation, no `cargo-zigbuild`, no UPX.
*   WASM build: `trunk build index.html --release` with no `Trunk.toml` optimization flags
    (Trunk.toml only sets `target`, `dist`, `public_url`, `release = false`)

### Dockerfile (observed)
```dockerfile
FROM rust:1.94-bookworm AS tools          # 1.4 GB layer — installs trunk
RUN cargo install trunk --version 0.21.14 --locked

FROM tools AS builder                     # Reuses tools layer
WORKDIR /workspace
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
RUN rustup target add wasm32-unknown-unknown
WORKDIR /workspace/crates/gobrowse-web
RUN trunk build index.html --release --dist /workspace/dist
WORKDIR /workspace
RUN cargo build --locked --release -p gobrowse-server

FROM debian:bookworm-slim AS runtime      # ~75 MB base
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl git \
    && rm -rf /var/lib/apt/lists/*
# ... user creation, COPY binary, COPY dist, COPY migrations, EXPOSE 8080
```

Issues:
1. `rust:1.94-bookworm` base is ~1.4 GB; trunk installation adds ~400 MB.
2. No Docker layer caching for Cargo registry/index (full rebuild on any source change).
3. Runtime installs `curl` and `git` (useful for health probes but add ~15 MB).
4. No `.dockerignore` optimization verified (may copy `target/` into build context).
5. `trunk build` runs from inside Docker without incremental caching.

### Server binary (observed dependencies)
*   **Heavy deps always linked**: `argon2` (password hashing, startup-only), `flate2`
    (plugin tar extraction, on-demand), `zip` (plugin zip extraction, on-demand), `tar`
    (plugin tar extraction, on-demand), `pgvector` (embedding search, only when embeddings
    configured).
*   **Feature flags not used**: all workspace deps are unconditional. No `features = [...]`
    gating for optional capabilities.
*   **Startup path**: `main.rs` → `Settings::load()` → `PgPool::connect()` →
    `sqlx::migrate!()` → `AppState::new()` → `tokio::spawn(embedding::run_worker())` →
    `tokio::spawn(run_worker())` → `tokio::spawn(webhook_scheduler::run_worker())` →
    `axum::serve()`. Migration runs synchronously at startup.

### WASM bundle (observed)
*   `gobrowse-web/src/app.rs`: **5,500+ lines**, single-file component tree. All pages
    (Chat, Library, Workspaces, Terminals, Models, Diagnostics, Autobiography, Plugins) are
    compiled into one bundle. No lazy routing, no dynamic imports.
*   Dependencies: `leptos` (CSR), `gloo-net`, `js-sys`, `wasm-bindgen-futures`,
    `web-sys` (HtmlElement, HtmlInputElement, HtmlPreElement, Window, Location, Storage).
*   No `wasm-opt` post-processing. No `wasm-strip`. No feature gating for unused pages.

### Context assembly (observed in `run_api.rs::build_messages`)
1.  Fetches last 40 messages from `messages` table (no index on `conversation_id + ordinal`).
2.  Runs `websearch_to_tsquery` FTS search on `books` with `ts_headline` for snippets.
3.  Fetches up to 20 pinned books with `left(b.body, 3000)` — always full 3K chars even if
    token budget is small.
4.  Fetches up to 10 worktrees with all columns.
5.  Token estimation: `chars / 4` — rough but adequate for budgeting.
6.  Three sequential DB queries (no parallelism).
7.  `build_context()` in `gobrowse_core::context.rs` does a simple sort-and-fill — O(n log n)
    on candidate count.

### Tool definitions (observed in `run_tools.rs`)
*   `tool_definitions()` and `sandbox_tool_definitions()` build `Vec<ToolDefinition>` with
    JSON schemas on every call. The schemas are `serde_json::json!()` macros — constructed
    fresh each time, not cached.
*   `tool_descriptors()` similarly constructs `Vec<ToolDescriptor>` from statics each call.

### MCP client pool (observed in `mcp_client.rs`)
*   `McpClientPool` is a `HashMap<Uuid, Arc<Mutex<ProcessMcpClient>>>` — one child process
    per configured MCP server. Connections are created lazily. No eviction, no max-size
    enforcement. Pool lives for server lifetime.
*   Each `get_or_connect()` queries `mcp_servers` table on cache miss.

### Embedding worker (observed in `embedding.rs`)
*   Background worker claims jobs from `embedding_jobs` table, processes in batches of 16.
*   Creates a new `reqwest::Client` per provider (no client reuse across jobs).
*   `provider_http_client()` builds client with TLS configuration each time.

### Sandbox client (observed in `sandbox_client.rs`)
*   Fresh Unix socket connection per `send()` call. No connection pooling. No keepalive.
*   This is intentional per protocol design (one client per connection), but worth noting.

### Router (observed in `lib.rs`)
*   70+ routes registered in a flat `Router::new()` chain.
*   Tower layers: `DefaultBodyLimit`, `TimeoutLayer` (30s), `CompressionLayer`,
    `SetSensitiveRequestHeadersLayer`, `PropagateRequestIdLayer`, `SetRequestIdLayer`,
    `TraceLayer`, `CatchPanicLayer`, `origin_guard` middleware.
*   All layers applied unconditionally (no feature gating).

### Core crate (observed in `gobrowse-core`)
*   13 modules: `activity`, `agent`, `context`, `fake_model`, `library`, `mcp`, `model`,
    `plugin`, `policy`, `redaction`, `sandbox`, `scheduler`, `skills`, `tools`, `worktrees`.
*   All modules compiled unconditionally. No feature flags.
*   `sandbox.rs` has extensive protocol types with `deny_unknown_fields` on every struct —
    good for security but increases code size.

### Existing benchmarks (observed)
*   `crates/gobrowse-core/benches/core.rs`: three criterion benchmarks:
    - `library_rank_fusion_200x200`
    - `chunk_book_1m_chars`
    - `context_select_500_candidates`
*   No server-level benchmarks (latency, throughput, startup time).
*   No WASM size tracking.
*   No Docker image size tracking.

## Architectural Decisions

### 1. Measure before optimizing — baseline metrics first

Before any code change, establish reproducible baselines:

| Metric | How to measure | Target |
|---|---|---|
| WASM bundle size | `wc -c dist/*.wasm` after trunk build | ≤ 8 MB (from ~12 MB) |
| WASM gzipped | `gzip -k dist/*.wasm && wc -c dist/*.wasm.gz` | ≤ 3 MB |
| Docker image size | `docker images gobrowse:latest` | ≤ 40 MB gzip (from ~66 MB) |
| Docker build time | `time docker build .` (clean, no cache) | ≤ 5 min |
| Server binary size | `wc -c target/release/gobrowse` | ≤ 15 MB (current unknown) |
| Idle RSS | `ps -o rss= -p $(pgrep gobrowse)` after startup, no requests | ≤ 30 MB |
| Startup time | Time from `gobrowse serve` to "listening on :8080" log | ≤ 3 s |
| `build_messages` latency | Instrument `build_messages` with `tracing::info!(elapsed)` | ≤ 50 ms (p99) |
| FTS search latency | Instrument search_books with timing | ≤ 20 ms (p99) |
| Tool definition build time | Instrument `tool_definitions()` | ≤ 1 ms per call |
| Build time (incremental) | `cargo build -p gobrowse-server` after touching one file | ≤ 30 s |
| Build time (clean) | `cargo clean && cargo build --release -p gobrowse-server` | ≤ 5 min |
| Context assembly tokens | Track `RunTokenMetrics.used_tokens` in representative runs | ≤ 80% of budget |
| Book count scaling | Memory after loading 1,000 / 10,000 / 100,000 books | RSS must not grow > 2 MB |
| Plugin count scaling | Memory with 0 / 10 / 100 dormant plugins | RSS must not grow > 1 MB |

**Implementation**: Add a `scripts/benchmark.sh` that runs all measurements and outputs a
markdown table. This script is the authoritative source for before/after comparison.

### 2. Docker image optimization — distroless runtime, layer caching

**Decision**: Replace `debian:bookworm-slim` with a minimal runtime base. Reorder Dockerfile
for maximum layer cache hits.

**Approach**:
*   Runtime base: `gcr.io/distroless/cc-debian12` (no shell, no apt, ~2 MB) OR
    `debian:bookworm-slim` with `curl` removed (keep only `ca-certificates` for TLS).
*   Cargo registry cache: mount `~/.cargo/registry` and `~/.cargo/git` as build cache mounts.
*   Separate the trunk installation into a cached layer (only rebuilds on version change).
*   Copy `Cargo.toml`, `Cargo.lock`, and a minimal `src/lib.rs` first for dependency caching,
    then copy full source.
*   Add `.dockerignore` excluding `target/`, `dist/`, `*.md`, `.git/`, `node_modules/`.
*   Remove `curl` from runtime (health checks use the `/health/live` endpoint directly).
    Keep `git` only if required by plugin install at runtime (verify).

**Files**: `Dockerfile`, `.dockerignore` (new)

### 3. WASM bundle size — feature gates, code splitting, wasm-opt

**Decision**: Use Leptos router with lazy page loading. Add `wasm-opt` post-processing.

**Approach**:
*   **wasm-opt**: Add `trunk build --release` with `--optimized` flag or run `wasm-opt -Oz`
    on the output `.wasm`. Expected reduction: 20-40% on debug/info sections.
*   **Trunk.toml optimization**: Set `[tools] wasm_opt = "Oz"` (or `O4` for speed-critical
    paths). Set `[build] release = true` in Trunk.toml (currently `release = false`).
*   **Feature-gate gobrowse-core in WASM**: The WASM crate only needs `library`, `context`,
    `model`, `tools`, `policy`, `mcp`, `skills`, `plugin` from `gobrowse-core`. It does NOT
    need `sandbox`, `activity`, `worktrees`, `scheduler`, `fake_model`, `redaction`. Add
    feature flags to `gobrowse-core` Cargo.toml for WASM-only compilation.
*   **Leptos CSR code splitting**: Use `#[component]` with conditional rendering based on
    `Page` enum (already exists). The single `app.rs` (5,500+ lines) should remain a single
    file for LLM-editability, but component-level lazy loading can be added if trunk supports it.
    If Leptos CSR doesn't support route-level code splitting, accept the single bundle.
*   **web-sys feature trimming**: Audit `web-sys` features. Currently requests `HtmlElement`,
    `HtmlInputElement`, `HtmlPreElement`, `Window`, `Location`, `Storage`. If any are unused,
    remove. Each feature adds ~1-5 KB to the WASM binary.

**Files**: `crates/gobrowse-web/Trunk.toml`, `crates/gobrowse-core/Cargo.toml`,
`crates/gobrowse-web/Cargo.toml`

### 4. Server binary optimization — feature-gated optional capabilities

**Decision**: Make heavy, rarely-used dependencies optional via Cargo feature flags.

**Approach**:
*   Add `[features]` to `gobrowse-server/Cargo.toml`:
    ```toml
    [features]
    default = ["sandbox", "plugins", "embeddings"]
    sandbox = ["dep:gobrowse-sandboxd"]
    plugins = ["dep:flate2", "dep:tar", "dep:zip"]
    embeddings = ["dep:pgvector"]
    ```
*   Conditionally compile `sandbox_api.rs`, `sandbox_client.rs`, `plugin_api.rs`,
    `plugin_github.rs`, `embedding.rs`, `embedding_api.rs` behind `#[cfg(feature = "...")]`.
*   Gate the corresponding routes in `lib.rs` router construction.
*   `argon2` stays unconditional (auth is core). `reqwest` stays unconditional (chat provider).
*   Expected savings: removing `flate2`+`tar`+`zip`+`pgvector` saves ~500 KB-1 MB of binary
    when features are disabled. More importantly, it clarifies architecture.

**Files**: `crates/gobrowse-server/Cargo.toml`, `crates/gobrowse-server/src/lib.rs`,
per-module `#[cfg]` gates

### 5. Context assembly optimization — parallel queries, bounded pin loading

**Decision**: Parallelize DB queries in `build_messages`, bound pinned book body size by
remaining budget.

**Approach**:
*   Use `tokio::join!` or `futures::join!` for the three sequential queries in
    `build_messages`:
    1.  Recent messages (40 rows)
    2.  Library FTS search (12 rows)
    3.  Pinned books (20 rows)
    4.  Worktrees (10 rows)
    All four can run in parallel since they have no data dependencies.
*   **Bounded pin loading**: Currently fetches `left(b.body, 3000)` for each pinned book.
    Change to `left(b.body, LEAST(3000, $remaining_budget / $pin_count))` — scale body
    truncation to actual budget. This prevents 20 pinned books × 3K chars = 60K chars from
    consuming all budget before library search results are considered.
*   **Index for recent messages**: Add a covering index:
    ```sql
    CREATE INDEX IF NOT EXISTS idx_messages_conversation_ordinal
        ON messages (conversation_id, ordinal DESC)
        INCLUDE (role, content);
    ```
    This is the only schema change in M24 — justified by measurable latency improvement.
*   **Index for library FTS**: Verify `search_document` has a GIN index (should exist from
    migration 0001). If not, add one.

**Files**: `crates/gobrowse-server/src/run_api.rs`, `crates/gobrowse-server/migrations/`
(new index migration if needed)

### 6. Tool definition caching — avoid per-request reconstruction

**Decision**: Cache `ToolDefinition` and `ToolDescriptor` vectors in `AppState`.

**Approach**:
*   Add `tool_definitions: Vec<ToolDefinition>` and `sandbox_tool_definitions: Vec<ToolDefinition>`
    to `AppState` (computed once at startup, behind `Arc<Vec<...>>`).
*   Add `tool_descriptors: Vec<ToolDescriptor>` similarly.
*   `tool_definitions(sandbox_enabled)` becomes `state.tool_definitions.clone()` (Arc clone =
    pointer copy, ~8 ns).
*   The JSON schemas are currently `serde_json::json!()` macros — these are lazy-evaluated
    `Value` trees. Caching avoids re-parsing/allocating on every request.
*   For `sandbox_tool_definitions`, conditionally build based on `settings.features.sandbox`.

**Files**: `crates/gobrowse-server/src/run_tools.rs`, `crates/gobrowse-server/src/lib.rs`

### 7. Embedding worker — reuse HTTP client

**Decision**: Reuse `reqwest::Client` across embedding jobs instead of creating a new one
per provider per job.

**Approach**:
*   Cache `reqwest::Client` in `AppState` (one per server lifetime). `reqwest::Client` is
    designed to be shared — connection pooling is built in.
*   `provider_http_client()` becomes `state.http_client.clone()` or takes `&AppState`.
*   Expected savings: eliminates ~1-2 ms of TLS handshake setup per embedding job, reduces
    memory fragmentation from client teardown/creation.

**Files**: `crates/gobrowse-server/src/embedding.rs`, `crates/gobrowse-server/src/lib.rs`

### 8. MCP client pool — add max-size eviction

**Decision**: Cap the MCP client pool at a configurable max (default 8). Evict LRU on overflow.

**Approach**:
*   Add `max_pool_size: usize` to `McpClientPool` (default from settings).
*   On `get_or_connect()`, if pool is full, drop the oldest-unused entry.
*   The `ProcessMcpClient` drop impl kills the child process (tokio `Child` drops kill on
    drop). This is safe — the next `get_or_connect()` re-spawns.
*   Prevents unbounded child process growth with many MCP servers.

**Files**: `crates/gobrowse-server/src/mcp_client.rs`

### 9. Startup time — background migration, lazy worker spawn

**Decision**: Move migration to a background task. Spawn workers after the server starts
accepting connections.

**Approach**:
*   Run `sqlx::migrate!()` in a `tokio::spawn` that blocks on the migration but doesn't
    delay `axum::serve()`. The server starts serving health checks immediately; API calls
    that need the schema wait for migration to complete (race-safe because migration is
    idempotent).
*   Spawn `embedding::run_worker`, `run_worker`, `webhook_scheduler::run_worker` after
    `axum::serve()` starts, not before. This shaves ~100-500 ms off perceived startup.
*   The `/health/ready` endpoint should return 503 until migration completes (already
    should — verify).

**Files**: `crates/gobrowse-server/src/main.rs`, `crates/gobrowse-server/src/api.rs`

### 10. Core crate — feature-gate unused modules for WASM

**Decision**: Add feature flags to `gobrowse-core` so WASM compilation excludes server-only
modules.

**Approach**:
*   Add to `gobrowse-core/Cargo.toml`:
    ```toml
    [features]
    default = ["server"]
    server = ["sandbox", "scheduler", "activity", "worktrees"]
    sandbox = []
    scheduler = []
    activity = []
    worktrees = []
    fake_model = []
    redaction = []
    ```
*   Gate `pub mod sandbox;` etc. with `#[cfg(feature = "...")]`.
*   `gobrowse-web/Cargo.toml` depends on `gobrowse-core` with `default-features = false`,
    only enabling the modules it needs.
*   Expected savings: 10-30 KB WASM reduction (removes `sandbox.rs` types, `activity.rs`,
    `worktrees.rs`, `scheduler.rs` from the WASM binary).

**Files**: `crates/gobrowse-core/Cargo.toml`, `crates/gobrowse-core/src/lib.rs`,
`crates/gobrowse-web/Cargo.toml`

### 11. Architecture map — maintain a live architecture diagram

**Decision**: Add `docs/architecture.md` with a text-based architecture map showing crate
dependencies, data flow, and module responsibilities. Update it when M24 lands.

**Approach**:
*   Simple ASCII/text diagram showing: `gobrowse-core` (domain model) → `gobrowse-server`
    (binary) → `gobrowse-web` (WASM) → `gobrowse-sandboxd` (daemon).
*   Module map for each crate with 1-line descriptions.
*   Data flow: user → WASM → server → PostgreSQL + LLM providers + sandboxd.
*   This is a living document, not a one-off.

**Files**: `docs/architecture.md` (new)

## Implementation Batches

### Batch 1: Measurement Infrastructure (no code changes)

**Goal**: Establish reproducible baselines. Create measurement scripts.

**Files to create**:
-   `scripts/benchmark.sh` — runs all measurements, outputs markdown table
-   `scripts/measure-wasm.sh` — WASM binary size, gzipped size, wasm-opt potential
-   `scripts/measure-docker.sh` — Docker image size, build time
-   `scripts/measure-server.sh` — binary size, idle RSS, startup time

**Verification**:
-   Run `scripts/benchmark.sh` and confirm all metrics are captured
-   Store baseline table in `docs/m24-baselines.md`
-   All subsequent batches reference this baseline

**Dependencies**: None (first batch)

### Batch 2: Container & Binary Optimization (Dockerfile, Cargo features)

**Goal**: Slim Docker image, reduce binary size, add feature gates.

**Files to modify**:
-   `Dockerfile` — restructure for layer caching, slim runtime, `.dockerignore`
-   `.dockerignore` — exclude `target/`, `dist/`, `.git/`, `*.md`
-   `crates/gobrowse-server/Cargo.toml` — add `[features]` for sandbox/plugins/embeddings
-   `crates/gobrowse-core/Cargo.toml` — add feature flags for server-only modules
-   `crates/gobrowse-core/src/lib.rs` — gate modules with `#[cfg(feature)]`
-   `crates/gobrowse-web/Cargo.toml` — depend on core with minimal features
-   `crates/gobrowse-server/src/lib.rs` — gate module declarations and route registration
-   Per-module `#[cfg]` gates in: `sandbox_api.rs`, `sandbox_client.rs`, `plugin_api.rs`,
    `plugin_github.rs`, `embedding.rs`, `embedding_api.rs`

**Verification**:
-   `scripts/measure-docker.sh` shows image size reduction
-   `scripts/measure-wasm.sh` shows WASM size reduction from feature gating
-   `cargo build --release -p gobrowse-server` succeeds with default features
-   `cargo build --release -p gobrowse-server --no-default-features` succeeds (core-only)
-   All existing tests pass: `cargo nextest run`
-   Capability regression matrix: every API endpoint still reachable, every tool still callable

### Batch 3: WASM Optimization (Trunk.toml, wasm-opt, web-sys trimming)

**Goal**: Reduce WASM bundle size toward 8 MB target.

**Files to modify**:
-   `crates/gobrowse-web/Trunk.toml` — enable release build, add wasm-opt settings
-   `crates/gobrowse-web/Cargo.toml` — audit and trim `web-sys` features
-   `Dockerfile` — install `wasm-opt` in tools stage, run post-build

**Verification**:
-   `scripts/measure-wasm.sh` shows ≥ 20% WASM size reduction
-   WASM loads in browser without errors (manual smoke test or automated)
-   All UI pages render correctly (Chat, Library, Workspaces, Terminals, Models, Diagnostics)

### Batch 4: Runtime Performance (context assembly, tool caching, HTTP client reuse)

**Goal**: Reduce per-request latency and memory allocation.

**Files to modify**:
-   `crates/gobrowse-server/src/run_api.rs` — parallelize `build_messages` queries, bounded
    pin loading
-   `crates/gobrowse-server/src/run_tools.rs` — cache tool definitions in `AppState`
-   `crates/gobrowse-server/src/lib.rs` — add cached tool defs to `AppState`
-   `crates/gobrowse-server/src/embedding.rs` — reuse HTTP client from `AppState`
-   `crates/gobrowse-server/src/mcp_client.rs` — add pool max-size eviction
-   `crates/gobrowse-server/migrations/` — new migration for `idx_messages_conversation_ordinal`
    covering index

**Verification**:
-   `build_messages` latency ≤ 50 ms p99 (instrumented tracing output)
-   `tool_definitions()` returns cached Arc clone, not fresh construction
-   MCP pool eviction tested with > max_pool_size servers configured
-   All existing integration tests pass
-   Existing criterion benchmarks in `gobrowse-core/benches/core.rs` still pass

### Batch 5: Startup Optimization (background migration, lazy workers)

**Goal**: Server responds to health checks within 1 second of process start.

**Files to modify**:
-   `crates/gobrowse-server/src/main.rs` — background migration, deferred worker spawn
-   `crates/gobrowse-server/src/api.rs` — verify `/health/ready` returns 503 during migration

**Verification**:
-   Time from process start to `/health/live` responding: ≤ 1 s
-   Time from process start to `/health/ready` responding: ≤ 3 s
-   All API calls succeed after `/health/ready` returns 200
-   Migration idempotency: restart server twice, confirm no migration errors

### Batch 6: Stress Testing (many books, many plugins, context scaling)

**Goal**: Verify RSS and context usage don't scale with installed count.

**Verification**:
-   Load 1,000 books, measure RSS → must be within 2 MB of baseline
-   Load 10,000 books, measure RSS → must be within 5 MB of baseline
-   Configure 10 dormant plugins, measure RSS → must be within 1 MB of baseline
-   Run `build_messages` with 50 pinned books and 20 worktree candidates → must stay within
    token budget, no OOM
-   Run 100 concurrent chat runs (if feasible) → RSS must be bounded

**Files**: No code changes — measurement only. If scaling issues found, address in Batch 4.

### Batch 7: Cleanup & Documentation

**Goal**: Finalize architecture map, update docs, verify M24 is permanent baseline.

**Files to modify**:
-   `docs/architecture.md` — new, live architecture diagram
-   `docs/m24-baselines.md` — before/after benchmark table
-   `PLAN.md` — this section, updated with final results
-   `README.md` — update build instructions if Dockerfile changed

**Verification**:
-   `docs/architecture.md` reflects actual crate/module structure
-   `docs/m24-baselines.md` has complete before/after table
-   Capability regression matrix is complete (every M23 feature confirmed working)

## Verification Steps

### Capability Regression Matrix

Every row must be ✅ (working, same or better) after M24:

| Feature | Endpoint/Tool | Verification |
|---|---|---|
| Owner setup | `POST /api/v1/setup/owner` | Integration test |
| Login | `POST /api/v1/auth/login` | Integration test |
| Create workspace | `POST /api/v1/workspaces` | Integration test |
| Create book | `POST /api/v1/library/books` | Integration test |
| FTS search | `GET /api/v1/library/search?q=...` | Integration test |
| Progressive load | `POST /api/v1/library/books/{id}/load` | Integration test |
| Start chat run | `POST /api/v1/conversations/{id}/runs` | Integration test |
| Tool: library_search | Agent tool call | Integration test |
| Tool: library_add | Agent tool call | Integration test |
| Tool: library_load | Agent tool call | Integration test |
| Sandbox tools | Agent tool call (if sandbox enabled) | Integration test |
| Plugin install | `POST /api/v1/plugins/install` | Integration test |
| Plugin list | `GET /api/v1/plugins` | Integration test |
| Skill CRUD | `POST /api/v1/skills` | Integration test |
| MCP server CRUD | `POST /api/v1/mcp/servers` | Integration test |
| Embedding config | `POST /api/v1/embeddings/configurations` | Integration test |
| Worktree CRUD | `POST /api/v1/workspaces/{id}/worktrees` | Integration test |
| Task routing | `PUT /api/v1/models/task-routes` | Integration test |
| Autobiography | `GET /api/v1/autobiography` | Integration test |
| Vault secrets | `POST /api/v1/vault/secrets` | Integration test |
| Webhook delivery | `POST /api/v1/webhooks/{id}/deliver` | Integration test |
| Health endpoints | `GET /health/live`, `GET /health/ready` | Integration test |
| WASM loads | Browser smoke test | Manual or automated |
| All UI pages render | Each Page variant | Manual or automated |

### Benchmark Table (to be filled after Batch 1)

| Metric | Baseline (M23) | After M24 | Change |
|---|---|---|---|
| WASM size (raw) | ~12 MB | | |
| WASM size (gzipped) | ~3 MB | | |
| Docker image (gzipped) | ~66 MB | | |
| Server binary size | TBD | | |
| Idle RSS | TBD | | |
| Startup to /health/live | TBD | | |
| Startup to /health/ready | TBD | | |
| build_messages p99 | TBD | | |
| FTS search p99 | TBD | | |
| Tool def build time | TBD | | |
| Incremental build time | TBD | | |
| Clean build time | TBD | | |
| RSS at 1,000 books | TBD | | |
| RSS at 10,000 books | TBD | | |
| RSS at 100 plugins | TBD | | |

## Database / Migration Changes

**Only one new migration** (if justified by Batch 4 measurements):

```sql
-- Covering index for build_messages recent messages query
-- Justification: eliminates seq scan on messages table for the
-- conversation_id + ordinal DESC query that runs on every chat turn.
CREATE INDEX IF NOT EXISTS idx_messages_conversation_ordinal
    ON messages (conversation_id, ordinal DESC)
    INCLUDE (role, content);
```

No other schema changes. Schema version stays at 22 unless this index requires a bump
(verify sqlx::migrate behavior with additive-only indexes).

## API Changes

**None.** All existing endpoints remain identical. M24 is a pure optimization milestone.
No new endpoints, no changed response shapes, no removed endpoints.

## Security Requirements

*   All existing auth, CSRF, and origin guards remain unchanged.
*   Feature-gated modules (sandbox, plugins, embeddings) must still enforce authorization
    when enabled — no security regression from feature gating.
*   WASM `wasm-opt` must not introduce security issues (it's a well-austen optimizer; verify
    output matches input behavior with the capability regression matrix).
*   Background migration must be idempotent and safe to run concurrently with API requests
    (PostgreSQL DDL is transactional; `CREATE INDEX CONCURRENTLY` if needed).
*   MCP pool eviction must cleanly kill child processes (verify `Child` drop behavior).

## Concurrency Requirements

*   `build_messages` parallel queries must not deadlock (they use independent pool connections).
*   MCP pool eviction must handle concurrent `get_or_connect()` calls safely (already uses
    `tokio::sync::Mutex`).
*   Background migration must not conflict with API requests that touch the same tables
    (additive index creation uses `CONCURRENTLY` to avoid locks).
*   Embedding worker HTTP client reuse must be thread-safe (`reqwest::Client` is `Clone + Send + Sync`).

## Tests Required

1.  **Feature-gate compilation test**: `cargo check --no-default-features -p gobrowse-server`
    must succeed (proves core-only build works).
2.  **Tool definition caching test**: Verify `tool_definitions()` returns same Arc pointer
    on repeated calls (unit test).
3.  **MCP pool eviction test**: Create pool with max_size=2, connect 3 servers, verify
    oldest is evicted (unit test).
4.  **Parallel build_messages test**: Verify all four DB queries complete and results merge
    correctly (integration test).
5.  **Bounded pin loading test**: With 20 pinned books and small token budget, verify
    `build_messages` respects budget (integration test).
6.  **Startup timing test**: Measure time to `/health/live` in integration test (assert < 2s).
7.  **Capability regression**: Run full integration test suite — all existing tests must pass.

## Deployment Considerations

*   **Rolling deploy**: M24 changes are backward-compatible. No migration required (unless
    index is added). Deploy is a simple image replacement.
*   **Feature flag rollout**: If feature gates are used, the default feature set includes
    all capabilities. Operators can disable features with `--no-default-features` at build
    time if they want a slimmer binary.
*   **Docker build cache**: After M24, Docker builds will be significantly faster due to
    layer caching. First build may be slower (downloading cache mounts).
*   **Monitoring**: After deploy, monitor idle RSS, startup time, and p99 latency to confirm
    improvements match baselines.
*   **Rollback**: Since no schema changes are required (index is additive), rollback is a
    simple image revert.

## Open Questions

1.  **Distroless vs bookworm-slim**: `gcr.io/distroless/cc-debian12` is smaller but has no
    shell. Health probes must use HTTP directly (already the case via `/health/live`).
    If any runtime debugging requires a shell, bookworm-slim is safer. Recommendation:
    start with bookworm-slim (remove `curl`, keep `ca-certificates`), evaluate distroless
    in a follow-up if image size is still too large.

2.  **wasm-opt in Docker or locally?**: Running `wasm-opt` in Docker adds ~30s to build.
    Running locally (in CI or dev) means the Docker build doesn't include it. Recommendation:
    run `wasm-opt` in the Docker tools stage so every build is consistent.

3.  **Feature gate depth**: Should sandbox/plugins/embeddings be fully feature-gated (all
    code behind `#[cfg]`), or just the route registration (code compiles but routes are
    404)? Full gating saves more binary size but increases `#[cfg]` complexity. Recommendation:
    full gating for server binary (measurable savings), keep routes unconditional for
    simplicity if savings are negligible.

4.  **Background migration race condition**: If a request arrives before migration completes
    and touches a table that migration is altering, what happens? PostgreSQL DDL is
    transactional — the request will either see the old schema or the new schema, never an
    intermediate state. For additive changes (new index), this is safe. Recommendation:
    verify with a targeted test.

5.  **MCP pool max size**: Default of 8 seems reasonable for typical deployments. Should
    this be configurable via settings? Yes — add `mcp_max_pool_size` to `FeatureSettings`.
    Operators with many MCP servers may want higher limits.

6.  **Token estimation accuracy**: `chars / 4` is a rough heuristic. For non-Latin scripts
    (CJK, emoji), this overestimates. For code-heavy content, it may underestimate. Should
    M24 improve this? Recommendation: defer to a follow-up — the current estimator is
    adequate for budgeting (bounded error, never negative). Improving it doesn't affect the
    optimization goal.
