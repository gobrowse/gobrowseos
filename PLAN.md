# PLAN.md — M24b: Custom UI System

Authoritative architecture and implementation plan for M24b. **Current batch** — M24 is the
completed optimization baseline. Branch: `initial-agent-os`, HEAD `0168513`.

**Schema version: 23 (post-M24). M24b adds migration 0024.**

---

## Goal

Agent-editable UI with permissions. THE UI IS CUSTOMIZABLE; THE SECURITY MODEL IS NOT.

Users and agents can install, preview, and activate custom UI packages (themes or full WASM
frontends) over the same stable versioned backend APIs. Third-party UI packages are untrusted:
CSP-enforced, hash-verified, no raw secrets, no DB/sandbox creds, no internal tokens, server-side
auth always authoritative. Agent UI edits require explicit user approval; silent activation is
prohibited. A built-in recovery UI always remains available and cannot be overwritten.

M24b MUST NOT regress any M24 optimization or break any security/verification gate: `unsafe_code =
"forbid"`, `clippy -D warnings`, `rustfmt`, nextest, schema version 23 asserted in
`postgres_integration.rs:42` + `worktrees_integration.rs:50`, `wasm-opt -Oz`, cheapest-capable
routing, lazy tool schemas, bounded context.

---

## Current State (observed post-M24, HEAD 0168513)

### What exists today

| System | State |
|--------|-------|
| **Book model** | `books` table with `kind` column: SOURCE, SKILL, MCP, PLUGIN, AUTOBIOGRAPHY. Companion Books created transactionally for skills/MCP/plugins. |
| **Plugin system** | Full install pipeline: preview → install → activate/rollback. `plugins` + `plugin_components` + `plugin_permissions` + `plugin_installations` tables. Artifacts in `settings.features.plugins_dir` (default `./data/plugins`). |
| **Static serving** | `ServeDir::new(static_dir).fallback(ServeFile::new(index.html))` at `lib.rs:392`. Single SPA from `dist/`. |
| **Auth** | Session cookies (`__Host-gobrowse_session`), OWNER/ADMIN/MEMBER/VIEWER roles, `origin_guard` middleware (`lib.rs:415`). |
| **Frontend** | Leptos WASM SPA, 6205 LOC `app.rs`, 12 pages (Chat, Workspaces, Library, Tasks, Agents, Terminals, Skills, Autobiography, MCP, Models, Diagnostics), CSS custom properties for theming. |
| **CSP** | **None.** No Content-Security-Policy headers anywhere. |
| **Capabilities endpoint** | **Does not exist.** No `GET /api/v1/capabilities`. |
| **UI packages** | **Does not exist.** No theme/UI package infrastructure. |
| **Recovery UI** | **Does not exist.** No fallback mechanism. |
| **UI permission model** | **Does not exist.** No DENY/ASK/ALLOW levels. |
| **API versioning** | `GET /api/v1/version` returns `{ version, api_version: "v1", schema_version, build_commit }` (`api.rs:38-48`). |

### M24 baseline constraints (non-negotiable)

- Schema version 23 (migration `0023_agent_retrieval_index.sql` applied)
- Docker image: `debian:bookworm-slim` + `ca-certificates` only; healthcheck via `gobrowse health`
- WASM: `wasm-opt -Oz --enable-bulk-memory`
- Lazy tool schemas: round 1 advertises only 3 base tools
- Lazy sandbox connection via `SandboxHandle` (OnceLock)
- Cheapest-capable routing via `cost_ranking`
- `unsafe_code = "forbid"`, `clippy::all = "warn"`, Rust 1.94, edition 2024

### M24 optimization checks per M24b batch

Every M24b batch MUST verify:
1. `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings`
2. `cargo clippy -p gobrowse-web --target wasm32-unknown-unknown -- -D warnings`
3. `cargo nextest run --workspace` (with `GOBROWSE_TEST_DATABASE_URL`)
4. No schema_version regressions (integration tests assert 23)
5. No `unsafe` blocks added
6. No removal of existing tools, features, or endpoints
7. Docker build succeeds and `gobrowse health` works
8. WASM builds with `trunk build --release` and `wasm-opt -Oz`
9. Context budget stays bounded (no new large constant payloads)

---

## Architectural Decisions (M24b)

### AD-B1: UI packages follow the Plugin pattern with a Companion Book

**Rationale**: Plugins already have a proven install lifecycle (preview → install →
activate/rollback), a companion Book for library search, artifact storage on disk, and a
state machine with serialization via `SELECT ... FOR UPDATE`. UI packages have identical
needs. Reusing the pattern means less code, familiar security boundaries, and the agent
discovers UI packages through the same library search.

**Decision**:
- New `ui_packages` table (analogous to `plugins`) with `state` column driving the lifecycle.
- Companion Book with `kind = 'GOBROWSE_UI'` (analogous to `kind = 'PLUGIN'`), created in
  the same transaction as the `ui_packages` row.
- Artifacts stored in `settings.features.ui_packages_dir` (default `./data/ui-packages`),
  analogous to `plugins_dir`.
- Same digest verification (TOCTOU protection: preview returns digest, install requires it).

**New `BookKind` variant**: Add `GobrowseUi` to the `BookKind` enum in `gobrowse-core/src/library.rs`,
and `'GOBROWSE_UI'` to the `books_kind_check` constraint via migration.

### AD-B2: Safe change pipeline — CANDIDATE / ACTIVE / PREVIOUS states

**Rationale**: The roadmap requires preserving CURRENT + PREVIOUS + CANDIDATE. Plugins use
`discovered | staged | installed | enabled | dormant | active | unhealthy | update_available`.
UI packages need a simpler state machine focused on safe transitions.

**Decision**:
```
discovered  →  staged  →  validated  →  candidate  →  active
                                                  ↘  failed
active  →  previous  (on activation of a new package)
previous →  active    (on rollback)
any     →  rolled_back (on explicit rollback of non-active)
```

- `discovered`: initial preview, manifest parsed, digest computed.
- `staged`: package installed on disk (files extracted, hashes verified).
- `validated`: automated validation passed (manifest schema, CSP hash computation, WASM
  load check, capability compatibility check).
- `candidate`: ready for activation, awaiting user approval.
- `active`: currently serving. Exactly one per profile at a time.
- `previous`: was active before the current activation. Single rollback target.
- `failed`: validation or activation failed.
- `rolled_back`: explicitly deactivated via rollback.

Transitions enforced in application code (not DB CHECK, to allow the full graph). Only one
row per profile can be `active` at a time (enforced via unique partial index).

### AD-B3: CSP computed and injected server-side

**Rationale**: Third-party UI packages are untrusted. CSP is the primary browser enforcement
mechanism. The server MUST compute hashes and inject headers; the UI package manifest can
declare additional allowed origins, but the server is always authoritative.

**Decision**:
- Server computes SHA-256 hashes of all served assets (WASM, CSS, JS) at install time and
  stores them in `ui_package_assets` table.
- On every response serving UI assets, inject `Content-Security-Policy` header:
  ```
  default-src 'none';
  script-src 'sha256-<wasm_hash>' 'sha256-<js_hash>';
  style-src 'sha256-<css_hash>';
  img-src 'self' data:;
  connect-src <api_origin>;
  font-src 'self';
  frame-ancestors 'none';
  base-uri 'self';
  form-action 'self';
  ```
- No `'unsafe-inline'`, no `'unsafe-eval'`, no wildcard origins.
- `connect-src` restricted to the server's own API origin (from `settings.http.public_origin`).
- CSP is injected via middleware (`csp_headers`) on all `/ui/` asset routes and the SPA
  fallback route.
- THEME packages: CSP uses the built-in WASM hash + theme CSS override hash.

**CSP middleware**: new module `crate::csp` with a `CspHeaders` struct that holds computed
hashes for the active UI package. Injected as a tower layer in `router()`.

### AD-B4: Recovery UI — minimal built-in, never overwritable

**Rationale**: A broken UI package must not lock the user out. The recovery UI is a separate,
minimal Leptos WASM binary (`recovery_ui.wasm`) compiled into the Docker image at a known
path (`/app/recovery/`). It provides login + UI package management + diagnostics.

**Decision**:
- `crates/gobrowse-recovery/` — separate crate, separate Leptos app.
  - Pages: Login, UI Package List (activate, rollback, delete), Diagnostics, Built-in Restore.
  - Minimal CSS inline, no external dependencies.
  - Compiled to `recovery_ui.wasm`, placed in Docker image at `/app/recovery/`.
- Server route: `GET /recovery` serves the recovery SPA. `GET /recovery/*` serves recovery assets.
  These routes bypass the active UI package entirely.
- Auto-fallback: the server's SPA fallback logic checks whether the active UI package's WASM
  file exists on disk. If missing or hash mismatch, requests for `/` get a 302 redirect to `/recovery`.
- `DELETE /api/v1/ui/packages/{id}` is rejected for `state = 'active'` or when it's the only
  remaining installed package (last-known-good protection).
- Recovery UI HTML contains a `<meta http-equiv="refresh">` fallback if WASM fails to load.

### AD-B5: Capabilities endpoint — stable versioned contract

**Rationale**: UI packages need to know what APIs/tools/capabilities are available without
hardcoding assumptions. The capabilities endpoint is the machine-readable contract between
backend and UI.

**Decision**:
- `GET /api/v1/capabilities` — unauthenticated, always available.
- Response format:
  ```json
  {
    "api_version": "v1",
    "schema_version": 24,
    "server_version": "0.1.0",
    "auth": { "methods": ["session_cookie"], "setup_required": false },
    "features": {
      "sandbox": true,
      "browser": false,
      "messaging": false,
      "plugins": true,
      "ui_packages": true,
      "webhooks": true
    },
    "tools": [
      { "name": "library_search", "kind": "library" },
      { "name": "sandbox_exec", "kind": "sandbox" }
    ],
    "endpoints": {
      "conversations": { "base": "/api/v1/conversations" },
      "library": { "base": "/api/v1/library" }
    },
    "ui": {
      "active_package_id": "uuid-or-null",
      "active_package_kind": "BUILT_IN",
      "permission_level": "ASK"
    }
  }
  ```
- Tool list is derived from the existing `tool_definitions()` function (already cached in
  `AppState.tool_descriptors`).
- Feature flags from `settings.features`.
- UI packages declare `min_api_version` in their manifest; activation is rejected if the
  server's `api_version` is lower.

### AD-B6: THEME vs FULL_UI — same table, different `ui_kind`

**Rationale**: Both themes and full UI packages share the same lifecycle (install, validate,
activate, rollback), permissions model, and library discovery path. Separating them would
duplicate code and confuse the state machine.

**Decision**:
- `ui_kind` column: `'THEME'` | `'FULL_UI'` (CHECK constraint).
- THEME: `manifest.theme` contains CSS custom property overrides.
  ```json
  { "theme": { "variables": { "--ink": "#1a1a2e", "--canvas": "#fafafa" } } }
  ```
  Activation injects a `<style>` block or serves an overridden `styles.css`.
  The built-in WASM and HTML remain unchanged.
- FULL_UI: `manifest.entry` points to the WASM entry point (`index.html` path within the
  package). The entire frontend is replaced.
- Both kinds produce a companion Book (kind=GOBROWSE_UI) for library search.
- Both go through the same permission checks.

### AD-B7: UI permission levels — per-profile setting

**Rationale**: The roadmap specifies DENY / ASK (default) / ALLOW_WORKSPACE / ALLOW_GLOBAL.
This is a profile-level setting controlling what the agent can do without explicit human
approval.

**Decision**:
- `profiles.ui_permission_level` column: `'DENY' | 'ASK' | 'ALLOW_WORKSPACE' | 'ALLOW_GLOBAL'`.
  Default: `'ASK'`.
- `DENY`: All UI package operations forbidden for agents. User can still manage via recovery UI.
- `ASK` (default): Agent must obtain explicit user approval before activating a UI package.
  Install and preview are allowed; activation blocks pending approval.
- `ALLOW_WORKSPACE`: Agent can activate workspace-scoped UI packages without asking.
  Global packages still require approval.
- `ALLOW_GLOBAL`: Agent can activate any UI package. This requires OWNER or ADMIN role to change
  to, and is logged as a security event.
- Approval flow: agent calls `POST /api/v1/ui/packages/{id}/activate` → if `ASK` and caller
  is an agent (determined by run context), the activation enters `awaiting_approval` substate
  → user receives notification (via run event) → user approves via `POST .../approve`.

### AD-B8: Agent path = human path

**Rationale**: The roadmap explicitly requires this. No special agent-only APIs for UI
management. The agent uses the same HTTP endpoints.

**Decision**:
- All UI endpoints accept both human (session cookie) and agent (run-scoped) auth.
- Agent auth is determined by `require_user_or_run` pattern (similar to how
  conversation APIs work with run context).
- Agent installs/activations are recorded with `installed_by`/`activated_by` pointing to the
  agent's run or user identity.
- The run loop's tool dispatch does NOT auto-approve UI changes; the agent must call the
  same endpoints and face the same permission checks.
- The agent's browser-test step uses the `browser` sandbox capability (M24b does not
  implement browser automation — that's a future milestone; for now, agent can preview
  screenshots via the server's `/ui/packages/{id}/preview` route that returns a static
  snapshot).

### AD-B9: `ui_packages` table design

```sql
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
    artifact_digest text,  -- SHA-256 of the source archive
    install_path text,     -- on-disk directory under ui_packages_dir
    entry_point text,      -- relative path to index.html (FULL_UI) or null (THEME)
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

-- Only one active UI package per profile.
CREATE UNIQUE INDEX ui_packages_one_active_per_profile
    ON ui_packages (profile_id) WHERE state = 'active';

-- Asset hashes for CSP computation.
CREATE TABLE ui_package_assets (
    id uuid PRIMARY KEY,
    ui_package_id uuid NOT NULL REFERENCES ui_packages(id) ON DELETE CASCADE,
    file_path text NOT NULL,   -- relative path within package
    content_type text NOT NULL,-- MIME type
    sha256_hash text NOT NULL, -- hex-encoded SHA-256
    file_size bigint NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (ui_package_id, file_path)
);

-- Declared capabilities (from manifest).
CREATE TABLE ui_package_capabilities (
    id uuid PRIMARY KEY,
    ui_package_id uuid NOT NULL REFERENCES ui_packages(id) ON DELETE CASCADE,
    capability_name text NOT NULL,
    min_api_version text,
    UNIQUE (ui_package_id, capability_name)
);

-- Profile-level UI permission.
ALTER TABLE profiles ADD COLUMN ui_permission_level text NOT NULL DEFAULT 'ASK'
    CHECK (ui_permission_level IN ('DENY', 'ASK', 'ALLOW_WORKSPACE', 'ALLOW_GLOBAL'));
```

### AD-B10: UI package manifest format

```json
{
  "kind": "gobrowse-ui",
  "manifest_version": 1,
  "name": "dark-theme",
  "version": "1.0.0",
  "ui_kind": "THEME",
  "description": "Dark color scheme for Gobrowse OS",
  "publisher": { "name": "Alice", "url": "https://example.com" },
  "license": "MIT",
  "api_version": "v1",
  "min_api_version": "v1",
  "entry": null,
  "capabilities": [],
  "permissions": {
    "ui_kind": "THEME"
  },
  "theme": {
    "variables": {
      "--ink": "#e0e0e0",
      "--canvas": "#1a1a2e",
      "--surface": "#16213e",
      "--blue": "#7ea0ff"
    }
  },
  "source": {
    "repository": "https://github.com/alice/gobrowse-dark-theme",
    "revision": "abc123"
  }
}
```

For FULL_UI:
```json
{
  "kind": "gobrowse-ui",
  "manifest_version": 1,
  "name": "custom-dashboard",
  "version": "1.0.0",
  "ui_kind": "FULL_UI",
  "entry": "index.html",
  "capabilities": ["conversations", "library", "sandbox"],
  "theme": null
}
```

### AD-B11: No BookKind enum change in gobrowse-core

**Decision**: Rather than adding a new variant to the `BookKind` enum (which would break
existing match exhaustiveness), store the UI package companion Book with `kind = NULL`
(meaning SOURCE in the current model) and distinguish via `metadata->>'ui_package_id'`.
This avoids touching the shared `gobrowse-core` enum and keeps M24b changes server-local.
Alternatively, use `kind = 'GOBROWSE_UI'` as a new BookKind variant — but this requires a
core change.

**Revised decision**: Add `GobrowseUi` to `BookKind` and `'GOBROWSE_UI'` to the CHECK
constraint. The existing match arms on BookKind use `#[serde(default)]` on `kind: Option<BookKind>`,
so adding a variant is backward-compatible — existing code that matches on `Some(Source)`,
`Some(Skill)`, etc. will get a compile error from non-exhaustive match, which forces audit.
This is the correct, boring approach. Add the variant.

### AD-B12: Security boundaries (third-party UI = untrusted)

- CSP enforced server-side; manifest CSP declarations are advisory only.
- UI package WASM runs in the browser, NOT on the server. No server-side execution.
- UI package has NO access to: DB credentials, sandbox socket, vault secrets, session tokens
  (HttpOnly cookies), internal provider credentials.
- ALL auth/RBAC decisions are server-side and authoritative.
- UI package CANNOT: modify CSP, access `/api/v1/vault/*`, access `/api/v1/admin/*`, override
  recovery UI routes.
- Manifest `permissions` field is informational/metadata; actual enforcement is via
  `ui_permission_level` on the profile and server-side auth.
- CSP `connect-src` restricted to `public_origin` (the server itself) — UI cannot phone home.

---

## Data Model (Migration 0024)

### New migration file

`crates/gobrowse-server/migrations/0024_ui_packages.sql`

```sql
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
--    SQLite-style: drop and recreate the constraint.
ALTER TABLE books DROP CONSTRAINT IF EXISTS books_kind_check;
ALTER TABLE books ADD CONSTRAINT books_kind_check
    CHECK (kind IS NULL OR kind IN ('SOURCE','SKILL','MCP','PLUGIN','AUTOBIOGRAPHY','GOBROWSE_UI'));

-- 7. Bump schema.
UPDATE schema_metadata SET schema_version = 24, updated_at = now() WHERE singleton;
```

### BookKind enum addition

In `gobrowse-core/src/library.rs`, add variant:
```rust
pub enum BookKind {
    Source,
    Skill,
    Mcp,
    Plugin,
    Autobiography,
    /// Custom UI package (M24b).
    GobrowseUi,
}
```

The `serde(rename_all = "SCREAMING_SNAKE_CASE")` derives the wire/DB value `GOBROWSE_UI`.

### FeatureSettings addition

In `crates/gobrowse-server/src/config.rs`:
```rust
pub struct FeatureSettings {
    // ... existing fields ...
    /// Root directory for UI package artifacts.
    #[serde(default = "default_ui_packages_dir")]
    pub ui_packages_dir: PathBuf,
}

fn default_ui_packages_dir() -> PathBuf {
    PathBuf::from("./data/ui-packages")
}
```

---

## API Changes

### New endpoints

| Method | Path | Auth | Description |
|--------|------|------|-------------|
| `GET` | `/api/v1/capabilities` | None | Capability discovery. Always available. |
| `POST` | `/api/v1/ui/preview` | User/Run | Preview/validate a UI package (no install). |
| `POST` | `/api/v1/ui/install` | User/Run | Install with approve + digest verification. |
| `GET` | `/api/v1/ui/packages` | User/Run | List UI packages for profile. |
| `GET` | `/api/v1/ui/packages/{id}` | User/Run | Get package details + assets. |
| `PATCH` | `/api/v1/ui/packages/{id}` | User | Update trust/state (admin action). |
| `POST` | `/api/v1/ui/packages/{id}/activate` | User/Run | Activate (may require approval). |
| `POST` | `/api/v1/ui/packages/{id}/approve` | User | Approve a pending activation. |
| `POST` | `/api/v1/ui/packages/{id}/rollback` | User | Rollback to PREVIOUS. |
| `DELETE` | `/api/v1/ui/packages/{id}` | User | Uninstall (rejects active/last-known-good). |

### Server-side recovery route

| Method | Path | Auth | Description |
|--------|------|------|-------------|
| `GET` | `/recovery` | None | Recovery UI SPA (login page if no session). |
| `GET` | `/recovery/*` | None | Recovery UI static assets. |

### Modified static serving logic

`router()` in `lib.rs` must change to serve the active UI package when one is set:

```
if active_ui_package is FULL_UI and state = 'active':
    ServeDir::new(active_ui_package.install_path).fallback(ServeFile::new(entry_point))
else if active_ui_package is THEME:
    ServeDir::new(builtin_dist) with injected theme.css overlay
else:
    ServeDir::new(builtin_dist).fallback(ServeFile::new("index.html"))
```

This requires `AppState` to hold a cached reference to the active UI package (reloaded on
activation/rollback). Use an `Arc<RwLock<Option<ActiveUiState>>>`.

---

## Backend Changes

### New modules

| Module | File | Responsibility |
|--------|------|----------------|
| `ui_api` | `crates/gobrowse-server/src/ui_api.rs` | UI package CRUD, preview, install, activate, rollback, approval. |
| `csp` | `crates/gobrowse-server/src/csp.rs` | CSP header computation and middleware. |
| `capabilities_api` | `crates/gobrowse-server/src/capabilities_api.rs` | `GET /api/v1/capabilities` response builder. |

### Modified modules

| Module | Change |
|--------|--------|
| `lib.rs` | Add `ActiveUiState`, `ui_packages_dir` to `AppState`; add UI + recovery routes; inject CSP middleware; modify SPA fallback logic. |
| `config.rs` | Add `ui_packages_dir` to `FeatureSettings`. |
| `main.rs` | Create `ui_packages_dir` on startup. |
| `api.rs` | No changes (capabilities is a separate module). |
| `auth.rs` | Add `require_user_or_run` helper for UI endpoints. |

### ActiveUiState

```rust
#[derive(Clone)]
pub struct ActiveUiState {
    pub package_id: Uuid,
    pub ui_kind: UiKind,        // THEME or FULL_UI
    pub install_path: PathBuf,  // disk path for FULL_UI assets
    pub entry_point: String,    // relative path to index.html
    pub assets: Vec<UiAsset>,   // for CSP
    pub theme_variables: Option<HashMap<String, String>>, // for THEME
}

#[derive(Clone)]
pub struct UiAsset {
    pub file_path: String,
    pub content_type: String,
    pub sha256_hash: String,
}
```

`ActiveUiState` is loaded from DB on server startup and updated on activation/rollback.
Stored in `AppState` as `Arc<RwLock<Option<ActiveUiState>>>`.

---

## Frontend Changes

### `gobrowse-web` changes

- New `Page::UiPackages` variant for the side navigation.
- New `UiPackagesPage` component:
  - List installed UI packages with state, version, kind, trust.
  - Activate button (with confirmation for FULL_UI packages).
  - Rollback button (to PREVIOUS).
  - Delete button (disabled for active and last-known-good).
  - Install form: source URI + version + workspace selection.
  - Preview/details view showing manifest, capabilities, asset list.
- CSP awareness: the existing app MUST work with a restrictive CSP (no inline scripts,
  hash-verified WASM). This is already the case — Leptos uses wasm-bindgen and doesn't
  inject inline scripts.
- Theme loading: if active UI package is THEME kind, the app loads theme CSS variables
  by fetching `/api/v1/ui/packages/{id}/theme.css` (new endpoint that returns the CSS
  override).

### `gobrowse-recovery` (new crate)

- `crates/gobrowse-recovery/` — minimal Leptos SPA.
- Separate `Cargo.toml` with `leptos`, `gloo-net`, `serde`, `serde_json`, `wasm-bindgen`.
- No dependency on `gobrowse-core` or `gobrowse-server` — self-contained.
- Pages:
  - **Login**: same auth flow as main app (`POST /api/v1/auth/login`).
  - **UI Packages**: list + activate + rollback + delete (same API as main app).
  - **Restore Built-in**: one-click button that rolls back to built-in UI
    (deactivates all packages, sets active to NULL).
  - **Diagnostics**: shows server version, schema version, active UI info.
- Styling: inline `<style>` block (no external CSS), minimal but readable.
- Compiled via Trunk to `dist-recovery/`, installed at `/app/recovery/` in Docker image.
- HTML `index.html` includes CSP meta tag as fallback, plus a `<noscript>` message.

### UI-SDK.md (deliverable, not code)

Stored at `docs/UI-SDK.md`. Contents:
- Architecture overview: backend APIs, CSP model, security boundaries.
- Manifest schema reference (full JSON schema).
- THEME tutorial: how to create a theme package.
- FULL_UI tutorial: how to scaffold a Leptos UI package.
- Build/packaging commands.
- Capabilities endpoint reference.
- Testing/validation workflow.
- Agent workflow: clone → edit → build → preview → request activation.

---

## Security Requirements

### CSP enforcement (server-authoritative)

1. Every response serving UI assets (FULL_UI or built-in) MUST include a `Content-Security-Policy`
   header computed from `ui_package_assets` hashes.
2. `script-src`: only hash-verified sources. Never `'unsafe-inline'` or `'unsafe-eval'`.
3. `style-src`: hash-verified for FULL_UI, `'self'` + hash for built-in with THEME overlay.
4. `connect-src`: restricted to `public_origin` only.
5. `frame-ancestors: 'none'` — prevents clickjacking.
6. Recovery UI has its own CSP (computed from recovery WASM hash).

### Permission enforcement

1. `DENY`: All mutating UI endpoints return `403 Forbidden` for non-OWNER callers.
2. `ASK`: Agent-initiated activation returns `202 Accepted` with `approval_required: true`.
   Activation completes only after user posts to `/approve`.
3. `ALLOW_WORKSPACE`: Workspace-scoped packages (with `workspace_id` matching the agent's
   workspace) auto-activate. Global packages require approval.
4. `ALLOW_GLOBAL`: All packages auto-activate. Setting this level requires OWNER/ADMIN role;
   audit-logged.

### Digest verification

1. `POST /api/v1/ui/preview` downloads the source archive, extracts the manifest, computes
   SHA-256 of the archive, returns digest + manifest.
2. `POST /api/v1/ui/install` requires `expected_digest` matching the preview. Server
   re-downloads, re-computes, rejects on mismatch (TOCTOU protection).
3. Asset hashes in `ui_package_assets` are computed from extracted files, not from the
   manifest (which could lie).

### Agent guard

1. Agent run context is identified via `require_user_or_run` — agent calls carry run-scoped
   auth, not session cookies.
2. Agent UI edits are NEVER silently activated. The `activate` endpoint checks
   `ui_permission_level` and may require approval.
3. Agent approval flow: activation creates a `ui_activation_request` row; user receives a
   notification in the run events; user clicks approve/deny; the run loop's tool result
   reflects the outcome.

---

## Concurrency Requirements

1. **Activation serialization**: `SELECT ... FOR UPDATE` on the `ui_packages` row being
   activated, plus the currently-active row. This prevents race between two concurrent
   activations.
2. **Single active invariant**: enforced by the unique partial index
   `ui_packages_one_active_per_profile`. Database layer, not application-level.
3. **CSP hash refresh**: after activation, `ActiveUiState` is updated atomically via
   `RwLock::write()`. New requests pick up the new CSP immediately.
4. **Install concurrency**: two installs of the same name are serialized by the
   `UNIQUE(profile_id, name)` constraint. Concurrent installs of different names are
   independent.

---

## Tests Required

### Integration tests (new file: `tests/ui_packages_integration.rs`)

| Test | What it verifies |
|------|-----------------|
| `preview_valid_ui_package` | Preview downloads, parses manifest, returns digest. |
| `preview_invalid_manifest_rejects` | Broken manifest returns validation error. |
| `install_requires_matching_digest` | Digest mismatch rejects install. |
| `install_requires_approve` | `approve: false` rejects. |
| `install_creates_companion_book` | Companion Book created with kind=GOBROWSE_UI. |
| `activate_sets_active_and_previous` | Activation transitions states correctly. |
| `activate_requires_approval_in_ask_mode` | ASK mode blocks agent activation. |
| `activate_allows_workspace_scoped` | ALLOW_WORKSPACE permits workspace packages. |
| `rollback_restores_previous` | Rollback swaps back to PREVIOUS. |
| `delete_rejects_active` | Cannot delete active package. |
| `delete_rejects_last_known_good` | Cannot delete only remaining package. |
| `csp_header_present_on_ui_assets` | Responses include computed CSP. |
| `csp_header_no_unsafe_inline` | CSP never contains unsafe-inline. |
| `capabilities_endpoint_returns_tools` | GET /capabilities lists tools from AppState. |
| `capabilities_endpoint_no_auth_required` | Unauthenticated access works. |
| `recovery_ui_accessible` | GET /recovery returns recovery page. |
| `recovery_ui_no_auth_required` | Recovery accessible without session. |
| `schema_24_migration_applies` | Migration runs and schema_version = 24. |

### Unit tests (in `ui_api.rs`)

- Manifest validation: required fields, version format, ui_kind constraints.
- State machine transitions: all valid and invalid transitions.
- CSP header builder: correct format, hash ordering, no unsafe-inline.
- Theme CSS generation: variable injection.

---

## Deployment Considerations

1. **Recovery UI in Docker image**: `crates/gobrowse-recovery/` compiled to WASM and placed at
   `/app/recovery/` in the `Dockerfile` runtime stage. This adds a build dependency (Trunk)
   to the Docker build but no runtime dependency.
2. **UI packages directory**: `data/ui-packages` on the persistent volume, same pattern as
   `data/plugins`. Created at startup via `tokio::fs::create_dir_all`.
3. **Backward compatibility**: Existing profiles with no `ui_permission_level` get the
   default `'ASK'`. No UI packages exist, so active UI is `BUILT_IN`. No behavioral change
   for existing deployments.
4. **Static asset serving change**: The `router()` function's SPA fallback now checks
   `ActiveUiState`. When no custom UI is active, behavior is identical to current.
5. **CSP for built-in UI**: When no custom UI is active, inject a CSP based on the built-in
   WASM hash. This is a tightening — previously no CSP was sent. This MUST NOT break the
   existing frontend. The built-in Leptos app already avoids inline scripts; CSP should be
   compatible. If the existing app uses any inline style attributes, those must be replaced
   with CSS classes (hash-verified style-src doesn't allow inline styles).

6. **Schema version bump to 24**: Update assertions in:
   - `postgres_integration.rs:42` (currently asserts `== 23`)
   - `worktrees_integration.rs:50` (currently asserts `== 23`)

---

## Ordered Implementation Steps (Batches)

Each batch is self-contained, buildable, and verifiable. Sized for one cheaper-coder agent.

### Batch 1: Data model + migration (schema 24)

**Files**:
- `crates/gobrowse-server/migrations/0024_ui_packages.sql` (new)
- `crates/gobrowse-server/tests/postgres_integration.rs` (schema version → 24)
- `crates/gobrowse-server/tests/worktrees_integration.rs` (schema version → 24)
- `crates/gobrowse-core/src/library.rs` (add `GobrowseUi` to `BookKind`)
- `crates/gobrowse-server/src/config.rs` (add `ui_packages_dir` to `FeatureSettings`)
- `crates/gobrowse-server/src/main.rs` (create `ui_packages_dir` on startup)

**Steps**:
1. Add `GobrowseUi` to `BookKind` enum in `gobrowse-core/src/library.rs`.
2. Write migration `0024_ui_packages.sql`.
3. Add `ui_packages_dir` to `FeatureSettings` (with default).
4. Add `create_dir_all` for `ui_packages_dir` in `main.rs` serve command.
5. Update schema version assertions in integration tests.
6. Verify migration applies: `cargo run -p gobrowse-server --bin gobrowse -- migrate`

**Acceptance**:
- `cargo test -p gobrowse-core -- library` passes (BookKind exhaustiveness).
- `cargo test -p gobrowse-server -- postgres_integration` passes.
- `cargo run --bin gobrowse -- migrate` applies cleanly.
- **Preserve M24**: `cargo fmt --check && cargo clippy --workspace -- -D warnings` passes.
- **Preserve M24**: Schema 23 → 24 upgrade; old tests still pass.

### Batch 2: Capabilities endpoint + AppState wiring

**Files**:
- `crates/gobrowse-server/src/capabilities_api.rs` (new)
- `crates/gobrowse-server/src/lib.rs` (add route + `ActiveUiState` field)

**Steps**:
1. Create `capabilities_api.rs` with `get_capabilities` handler.
2. Build response from `AppState.tool_descriptors`, `settings.features`, `settings.http`.
3. Add `ActiveUiState` struct and `Arc<RwLock<Option<ActiveUiState>>>` to `AppState`.
4. Add `GET /api/v1/capabilities` route.
5. On startup, query DB for active UI package and populate `ActiveUiState`.
6. Write unit test for capabilities response shape.

**Acceptance**:
- `curl http://127.0.0.1:8080/api/v1/capabilities` returns valid JSON with tools + features.
- Response includes `ui.active_package_kind: "BUILT_IN"`.
- Response includes all tool names from `tool_descriptors`.
- **Preserve M24**: all existing tests pass; no schema version regression.

### Batch 3: UI package preview + install (server-side)

**Files**:
- `crates/gobrowse-server/src/ui_api.rs` (new — preview + install handlers)
- `crates/gobrowse-server/src/lib.rs` (add routes)
- `crates/gobrowse-server/src/auth.rs` (add `require_user_or_run`)

**Steps**:
1. Add `require_user_or_run` helper to `auth.rs`.
2. Implement `POST /api/v1/ui/preview`: download source, extract, validate manifest,
   compute digest, return digest + manifest preview.
3. Implement `POST /api/v1/ui/install`: re-download, re-verify digest, extract to
   `ui_packages_dir/{id}`, compute asset hashes, create `ui_packages` row + assets +
   companion Book (all in one transaction), set state to `staged`.
4. Manifest validation: required fields, version format, ui_kind check.
5. Integration tests for preview + install.

**Acceptance**:
- Preview returns digest for a valid UI package source.
- Install with correct digest succeeds; incorrect digest fails.
- Install creates companion Book (kind=GOBROWSE_UI).
- Companion Book appears in `GET /library/search?q=&kind=GOBROWSE_UI`.
- **Preserve M24**: full CI green.

### Batch 4: CSP middleware + active UI serving

**Files**:
- `crates/gobrowse-server/src/csp.rs` (new)
- `crates/gobrowse-server/src/lib.rs` (inject CSP layer, modify SPA fallback)

**Steps**:
1. Build `CspMiddleware` that reads `ActiveUiState` from `AppState` and injects
   `Content-Security-Policy` header on asset responses.
2. Compute CSP directives from `ui_package_assets` hashes.
3. Enforce: no `'unsafe-inline'`, `script-src` = only hashes, `connect-src` = `public_origin`.
4. Modify `router()` SPA fallback: when `ActiveUiState` has a `FULL_UI` package, serve
   from `install_path` instead of built-in `dist/`. When `THEME`, serve built-in with
   theme CSS injection.
5. Add `/recovery` and `/recovery/*` routes (serve static recovery assets from
   `/app/recovery/`).
6. Add auto-fallback: if active UI package's WASM is missing, redirect `GET /` to `/recovery`.

**Acceptance**:
- Responses for active FULL_UI include CSP header.
- CSP header contains no `unsafe-inline`.
- Built-in UI (no active package) includes CSP with built-in WASM hash.
- Recovery UI is accessible at `/recovery`.
- Recovery UI assets are served with correct CSP.
- **Preserve M24**: existing frontend works with injected CSP (no breakage).
- **Test**: `curl -I http://127.0.0.1:8080/` shows CSP header.

### Batch 5: Activation, rollback, state machine

**Files**:
- `crates/gobrowse-server/src/ui_api.rs` (add activate, approve, rollback, delete handlers)
- `crates/gobrowse-server/src/lib.rs` (add routes)

**Steps**:
1. Implement state machine transitions in application code.
2. `POST .../activate`: validate state = `candidate` (or `previous`), check permissions
   (ui_permission_level + caller identity), set current active → `previous`, set target →
   `active`, update `ActiveUiState`.
3. `POST .../approve`: for `ASK`-blocked activations, user approves → transition to `active`.
4. `POST .../rollback`: find `state = 'previous'`, set active → `rolled_back`, previous →
   `active`. If no previous, deactivate to built-in.
5. `DELETE .../{id}`: reject if `state = 'active'` or if it's the only package with
   `state IN ('active', 'previous', 'candidate', 'validated')`.
6. `PATCH .../{id}`: update trust, description. Cannot change state directly.
7. All mutations serialize per package via `SELECT ... FOR UPDATE`.
8. Integration tests for all transitions + permission checks.

**Acceptance**:
- Activate transitions candidate → active, previous active → previous.
- Concurrent activate on same profile is serialized (no double-active).
- Rollback restores previous.
- Delete rejects active and last-known-good.
- Permission level ASK blocks agent activation.
- ALLOW_WORKSPACE permits workspace-scoped activation.
- **Preserve M24**: full CI green.

### Batch 6: Recovery UI crate + Docker integration

**Files**:
- `crates/gobrowse-recovery/Cargo.toml` (new)
- `crates/gobrowse-recovery/src/main.rs` (new)
- `crates/gobrowse-recovery/index.html` (new)
- `crates/gobrowse-recovery/Trunk.toml` (new)
- `Dockerfile` (add recovery build step)
- `docker-compose.yml` (no changes needed)

**Steps**:
1. Scaffold `gobrowse-recovery` crate with minimal Leptos app.
2. Build Login page, UI Package management page, Restore Built-in action.
3. Configure Trunk to output to `dist-recovery/`.
4. Add build step to `Dockerfile`: compile recovery WASM, copy to `/app/recovery/`.
5. Verify recovery UI is accessible and functional.

**Acceptance**:
- `cargo clippy -p gobrowse-recovery --target wasm32-unknown-unknown -- -D warnings` passes.
- `trunk build --release` produces valid WASM.
- Recovery UI login works.
- Recovery UI can list, activate, rollback, and delete UI packages.
- Recovery UI restore-built-in works (deactivates all packages).
- **Preserve M24**: Docker build still succeeds; final image under ~300 MB.
- **Preserve M24**: `wasm-opt -Oz` applied to recovery WASM.

### Batch 7: Frontend UI Packages page (gobrowse-web)

**Files**:
- `crates/gobrowse-web/src/app.rs` (add Page::UiPackages, UiPackagesPage component)

**Steps**:
1. Add `UiPackages` to the `Page` enum.
2. Add "UI Packages" to the side navigation (under Library group or a new group).
3. Build `UiPackagesPage` component: list, install form, activate/rollback/delete actions.
4. Theme loading: if active package is THEME, fetch `/api/v1/ui/packages/{id}/theme` and
   inject `<style id="gobrowse-theme">` into `<head>`.
5. Wire up install form: source URI, kind selection, workspace.
6. Add confirmation dialog for FULL_UI activation.

**Acceptance**:
- UI Packages page lists installed packages with state and kind.
- Install form submits to `/api/v1/ui/preview` then `/api/v1/ui/install`.
- Activate button works and updates the UI (theme change or FULL_UI switch).
- Rollback restores previous UI.
- Delete button rejects active/last-known-good with error message.
- `trunk build --release` succeeds.
- **Preserve M24**: `wasm-opt -Oz` applied; WASM size doesn't increase >10%.

### Batch 8: UI-SDK.md + manifest schema + examples

**Files**:
- `docs/UI-SDK.md` (new)
- `schema/ui-package-manifest.json` (new)
- `examples/starter-ui/` (new directory with minimal theme package)

**Steps**:
1. Write `docs/UI-SDK.md`: architecture, manifest reference, tutorials, agent workflow.
2. Create JSON Schema for `gobrowse-ui` manifest.
3. Create `examples/starter-ui/` with a minimal THEME package (manifest.json + theme.css).
4. Create `tools/ui-package` shell script: `build`, `package`, `validate`, `install` commands.

**Acceptance**:
- UI-SDK.md is complete, accurate, and LLM-readable (boring, explicit contracts).
- JSON Schema validates the example manifest.
- `tools/ui-package validate examples/starter-ui/` passes.
- Agent can follow UI-SDK.md to create, build, and install a theme package.

### Batch 9: Integration + E2E + regression

**Steps**:
1. Run full CI: `cargo fmt --check`, `cargo clippy --workspace -- -D warnings`,
   `cargo clippy -p gobrowse-web --target wasm32-unknown-unknown -- -D warnings`,
   `cargo clippy -p gobrowse-recovery --target wasm32-unknown-unknown -- -D warnings`,
   `cargo nextest run --workspace`.
2. Docker build + smoke test: `docker build`, `docker compose up -d`, verify health,
   verify capabilities endpoint, verify CSP header, verify recovery UI.
3. Install a theme package, activate, verify theme applied.
4. Rollback, verify built-in restored.
5. Install a FULL_UI package (minimal), activate, verify CSP serves correct assets.
6. Delete active → error. Delete last-known-good → error. Rollback then delete → success.
7. Verify agent flow: preview → install → activate (blocked in ASK) → approve → active.
8. Populate regression matrix: every M24 metric before/after M24b.

**Acceptance**:
- All CI green.
- Docker smoke passes.
- No M24 regressions (image size ≤ +5 MB, WASM size ≤ +10%, context budget unchanged,
  latency unchanged for non-UI endpoints).
- Capabilities endpoint returns correct data.
- CSP headers present and correct on all UI routes.

---

## Open Questions

1. **Inline style audit**: The existing `styles.css` uses CSS custom properties only (no
   inline styles). The Leptos `view!` macro generates DOM elements; need to verify no
   `style=` attributes are emitted. If any exist, they must be moved to classes for CSP
   compatibility. **Batch 4 must audit this before enabling CSP on built-in UI.**

2. **Recovery UI WASM size**: The recovery UI should be minimal (<100 KB WASM). If it
   grows larger, consider a pure HTML/CSS fallback instead of Leptos. **Batch 6 must
   measure and stay under budget.**

3. **FULL_UI CSP compatibility**: Third-party UI packages may break if they use inline
   scripts or styles. The manifest validation should warn (not block) if the package
   declares `"csp_compatible": false`. **Document in UI-SDK.md that CSP is mandatory.**

4. **Agent approval UX**: The approval flow inserts a run event that the frontend must
   display as an actionable "Approve UI change" button. This requires a new run event
   type (`ui_activation_request`). **Design the event payload in Batch 5.**

5. **UI package build tooling**: The `tools/ui-package` script is a bash wrapper. For
   FULL_UI packages, the build step requires Trunk+Rust toolchain — same as the built-in
   UI. **Document this requirement in UI-SDK.md; do not bundle Trunk in the Docker image.**

6. **Marketplace support**: The `source_type = 'marketplace'` is a placeholder. M24b does
   NOT implement a UI package marketplace. The route accepts `local_package` and
   `github_release` only. Marketplace is future work.

7. **Workspace-scoped UI packages**: Workspace-scoped packages apply only when the user
   is viewing that workspace. This adds complexity to the serving logic (active UI depends
   on session's current workspace). **Defer to a later milestone; M24b uses profile-scoped
   packages only.** The `workspace_id` column exists in the schema for future use but is
   always NULL in M24b.

8. **Theme CSS injection method**: THEME packages inject CSS custom properties. Options:
   (a) serve a synthetic `theme.css` from `/api/v1/ui/active-theme.css` that the
   frontend loads via `<link>`, (b) server injects `<style>` into index.html before
   serving. **Decision: use (a) — the frontend fetches the theme CSS, simpler CSP model,
   no HTML rewriting needed.** The `index.html` includes `<link rel="stylesheet"
   href="/api/v1/ui/active-theme.css">` as an optional stylesheet.

---

## Validation Commands (exact, post-Batch 9)

```bash
# ---- Pre-flight ----
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo clippy -p gobrowse-web --target wasm32-unknown-unknown -- -D warnings
cargo clippy -p gobrowse-recovery --target wasm32-unknown-unknown -- -D warnings

# ---- Full test suite ----
GOBROWSE_TEST_DATABASE_URL=postgres://gobrowse:test-only-password@localhost:5432/gobrowse_test \
  cargo nextest run --workspace

# ---- WASM builds ----
cd crates/gobrowse-web && trunk build --release && wasm-opt -Oz -o ../../dist/optimized.wasm ../../dist/*.wasm
cd crates/gobrowse-recovery && trunk build --release

# ---- Docker build + smoke ----
docker build -t gobrowse-os-app:m24b .
docker images gobrowse-os-app:m24b --format '{{.Size}}'
docker compose up -d && sleep 15 && docker compose ps
curl --fail http://127.0.0.1:8080/health/ready
curl http://127.0.0.1:8080/api/v1/version
curl http://127.0.0.1:8080/api/v1/capabilities | jq .
curl -I http://127.0.0.1:8080/ 2>&1 | grep -i content-security-policy
curl --fail http://127.0.0.1:8080/recovery
docker compose down
```

---

## `Preserve M24 Baseline` Checks (per batch)

Every batch MUST verify before completion:

| Check | Command/Verification |
|-------|---------------------|
| No `unsafe` added | `rg 'unsafe\b' crates/ --include '*.rs'` — only existing `unsafe` in wasm-bindgen glue. |
| Clippy clean | `cargo clippy --workspace --all-targets -- -D warnings` |
| Format clean | `cargo fmt --check` |
| Tests pass | `cargo nextest run --workspace` |
| Schema 23 still works | Integration tests asserting `schema_version == 24` pass; migration 0024 applies cleanly over 0023. |
| Endpoint count preserved | Every existing `/api/v1/*` route still registered. |
| Tool defs unmodified | `tool_definitions()` returns same set as M24. |
| Context budget unchanged | `build_messages` budget logic untouched. |
| WASM size ≤ M24 +10% | `ls -l dist/*.wasm` compared against M24 baseline (2,669,588 raw). |
| Docker image ≤ M24 +5 MB | `docker images` compared against M24 baseline (~75 MB compressed). |
| Lazy tool schemas still work | Round 1 still sends 3 base tools only. |
| Sandbox lazy connection intact | `SandboxHandle` unchanged. |