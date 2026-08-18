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
