# PLAN.md — M25: Auth Hardening & Enterprise Authorization

Authoritative architecture and implementation plan for M25 (M25a + M25b). **Current batch**.

**M24 is COMPLETE.** M24b (Custom UI System) is COMPLETE. Branch: `initial-agent-os`.

**Schema version: 24 (post-M24b). M25 adds migrations 0025+.**

---

## Goal

M25 delivers two complementary security milestones:

**M25a — Auth Hardening**: WebAuthn/OIDC support, login rate-limit hardening, CSRF hardening,
session rotation hardening, session lifecycle hardening. Every auth path is hardened while
single-user installs stay simple (no mandatory config, password-only still works).

**M25b — Enterprise Authorization**: A single centralized ALLOW/ASK/DENY authorization engine
covering every actor (human, agent, plugin, MCP, sandbox, context retrieval). Multi-user/workspace
isolation, agent/tool permissions, approval policies, secrets protection, and enterprise-grade
auditing with complete attribution. Single-user installs default to OWNER = everything with no
mandatory enterprise config; enterprise features activate when multiple users/workspaces exist.

M25 MUST NOT regress any M24/M24b optimization or break any security/verification gate:
`unsafe_code = "forbid"`, `clippy -D warnings`, `rustfmt`, nextest, `wasm-opt -Oz`,
cheapest-capable routing, lazy tool schemas, bounded context, strict CSP (no unsafe-inline).

---

## Current State (observed post-M24b, HEAD 0168513)

### What exists today

| System | State |
|--------|-------|
| **Auth** | Argon2id passwords, opaque sessions (`__Host-gobrowse_session`), `origin_guard` CSRF middleware, login throttling (5 attempts / 300 s window via `check_login_throttle`), session rotation via `POST /auth/rotate`. users.role: OWNER/ADMIN/MEMBER/VIEWER. Sessions table: `token_hash`, `auth_epoch`, `expires_at`, `absolute_expires_at`, `ip_hash`, `user_agent_hash`. Password hashing with bounded concurrency semaphore. |
| **CSRF** | `origin_guard` middleware enforces Origin header match on POST/PUT/PATCH/DELETE, rejects `Sec-Fetch-Site: cross-site`, exempts `/api/v1/webhooks/`. Good but missing: SameSite cookie attribute enforcement on the server side, Fetch Metadata `Sec-Fetch-Dest`/`Sec-Fetch-Mode` checks. |
| **Plugins** | Full install pipeline. `plugin_permissions` table: `filesystem_read`, `filesystem_write`, `network`, `secrets`, `subprocess`, `admin`, `system_info` domains. Static policy inspection (`validate_permissions`). Workspace-scoped plugins require workspace owner. |
| **MCP** | `mcp_client.rs`: per-server permissions + approval policy, OAuth PKCE states (migration 0018), vault-backed secrets. Bounded: `MAX_MCP_TOOLS=16`, `MAX_MCP_TOOLS_SCHEMA_BYTES=64KB`, `MAX_MCP_CALL_RESULT_BYTES=64KB`. |
| **Runs** | `risk_class_for_tool` classifies tool names as `"read"` or `"write"` (binary). `gobrowse_core::policy::RiskClass`: Read/Write/Execute/ExternalSideEffect/Destructive/Admin (6 levels). `PolicyEngine` evaluates tool_id × RiskClass → Allow/Ask/Deny. tool_calls table tracks `risk_class`, `idempotency_key`, `status`. `approvals` table (tied to tool_call_id): pending/approved/denied/expired with `decided_by`, `decided_at`, `expires_at`. |
| **Sandbox** | `sandbox_client.rs` — typed Unix-socket client. Sandboxd NOT wired into the app (`SandboxHandle` lazily connected, `features.sandbox` can be enabled). |
| **SSRF** | `outbound_http.rs` — `PinnedHttpsTransport`, `validate_resolved_target`: deny loopback/private/link-local/metadata ranges. `MAX_TARGET_URL_LENGTH=2048`, 2 s DNS, 5 s connect/request timeout. |
| **Vault** | `vault.rs` — AES-256-GCM envelope encryption, master key external (file or env), key rotation (`POST /vault/rotate`). MCP OAuth purposes: `mcp_oauth_access_token`, `mcp_oauth_refresh_token`, `mcp_oauth_client_secret`, `mcp_oauth_pkce_verifier`. |
| **Audit** | `audit_events` table (append-only via trigger): `sequence bigserial`, `actor_user_id`, `profile_id`, `action`, `resource_type`, `resource_id`, `outcome`, `detail jsonb`, `request_id`, `created_at`. Index: `(profile_id, sequence DESC)`. Activity events also append-only with per-workspace ordering. |
| **Workspace isolation** | `workspace_memberships` (OWNER/EDITOR/VIEWER). Queries filter by `workspace_id` + `profile_id`. Task integrity enforced via composite FKs (migration 0010). Cross-workspace references quarantined. |
| **CSP** | Strict (M24b): no unsafe-inline, hash-verified WASM/JS/CSS. `csp.rs` computes headers from `ui_package_assets` hashes. Built-in bootstrap hash handled. |
| **UI** | Recovery UI at `/recovery`. Capabilities endpoint `GET /api/v1/capabilities`. Active UI state in `AppState.active_ui: Arc<RwLock<Option<ActiveUiState>>>`. |

### Key gaps identified (verified in code)

1. **No WebAuthn/OIDC**: Only Argon2id passwords. No passkey, OIDC, or TOTP support.
2. **Single auth method per user**: No concept of linked auth methods — a user has exactly one password.
3. **`risk_class_for_tool` is binary**: Maps everything to read/write. The full 6-level `RiskClass` enum exists in `gobrowse-core` but is unused at dispatch time.
4. **No centralized authorization decision point**: Each module (plugin_api, mcp_client, run_api, sandbox_api) does its own ad-hoc checks. No unified `authorize(actor, action, resource) -> Decision` function.
5. **Plugin permissions are install-time only**: `validate_permissions` runs at install but there's no runtime enforcement of whether a plugin actually uses only its declared permissions.
6. **Agent permissions stored as JSONB but unenforced**: `agents.permissions jsonb` column exists but is never checked at tool dispatch time.
7. **No secrets access control**: Vault encrypts secrets but any OWNER/ADMIN can read any profile's secrets. Secret access is not audited per-read.
8. **Approval model is tool-call-specific**: `approvals` table is only for tool calls. No generic approval policy for plugins, MCP servers, agents, sandbox operations, webhook deliveries.
9. **Session hardening gaps**: No concurrent session limit, no device/browser fingerprint tracking, no step-up auth for sensitive operations, no session revocation list.
10. **Audit coverage is incomplete**: Plugin installs, MCP server mutations, sandbox operations, and secret access are not consistently audited.
11. **No enterprise audit viewer**: No API endpoint or UI for querying/searching the audit trail.
12. **Login rate-limit is per-email only**: No IP-based rate limiting, no global rate limit, no account lockout after N failures.

---

## Architectural Decisions

### AD-25.1: Single centralized authorization decision point

**Rationale**: Every human, agent, plugin, MCP, sandbox action, and context retrieval MUST pass
through the same centralized authorization system. No bypasses. One function, one code path, one
audit trail.

**Decision**: New module `crate::authorize` in `gobrowse-server` with a single public API:

```rust
pub async fn authorize(
    state: &AppState,
    actor: &Actor,
    action: &Action,
    resource: &Resource,
    context: &AuthorizationContext,
) -> Result<AuthorizationDecision, AppError>
```

Every call site that currently does ad-hoc permission checks MUST call `authorize()` instead.
The function returns `Allow`, `Ask`, or `Deny` with a mandatory reason.

**Actor** enum:
```rust
pub enum Actor {
    Human { user_id: Uuid, session_id: Option<Vec<u8>> },
    Agent { agent_id: Uuid, run_id: Uuid, user_id: Uuid },
    Plugin { plugin_id: Uuid, installation_id: Uuid },
    Mcp { server_id: Uuid, profile_id: Uuid },
    Sandbox { workspace_id: Uuid },
    Webhook { webhook_id: Uuid },
    System,
}
```

**Action** enum:
```rust
pub enum Action {
    // CRUD
    Create { resource_type: ResourceType },
    Read { resource_type: ResourceType },
    Update { resource_type: ResourceType },
    Delete { resource_type: ResourceType },
    // Operations
    Execute { tool_name: String, risk_class: RiskClass },
    Approve { approval_id: Uuid },
    AccessSecret { secret_id: Uuid, purpose: String },
    ContextRetrieve { book_ids: Vec<Uuid> },
    WebhookDeliver { url: String },
    // Admin
    ManageUsers,
    ManageWorkspace,
    ManageBilling,
    ViewAudit,
}
```

**Resource** enum:
```rust
pub enum Resource {
    Conversation(Uuid),
    Workspace(Uuid),
    Book(Uuid),
    Plugin(Uuid),
    McpServer(Uuid),
    SandboxWorkspace(Uuid),
    Secret(Uuid),
    Webhook(Uuid),
    Profile(Uuid),
    AuditTrail,
    SystemConfig,
}
```

### AD-25.2: Permission matrix is the authority; policy rules are layered on top

**Rationale**: A clear, auditable permission matrix defines defaults. Policy rules (per-user,
per-agent, per-plugin allowlists/denylists) can tighten but never relax the matrix defaults.

**Decision**: Permission matrix stored in code (constant, compiled, testable). Policy rules stored
in database (runtime-configurable). The effective decision is `matrix_default ∩ policy_rules`:
policy can only restrict, never expand.

### AD-25.3: Single-user installs default to OWNER = everything

**Rationale**: The roadmap requires single-user installs stay simple. When only one user exists
(role=OWNER, one profile, one workspace), every authorization check short-circuits to ALLOW with
a `"single_user_mode"` reason. No enterprise config, no approval prompts, no permission setup
required.

**Decision**: `authorize()` checks `is_single_user_mode(state)` as the first branch. If true,
return `Allow` for all actions that don't involve external actors (plugins, MCP, webhooks still
go through their own permission checks). Single-user mode is detected by: exactly one user with
role=OWNER, one profile, ≤1 workspace.

### AD-25.4: Approval policies tiered by risk + actor

**Rationale**: Different actors have different trust levels. A human OWNER approving a plugin
install is different from an agent requesting a sandbox exec. Approval policies must be
configurable per (actor_kind, risk_class, resource_type) tuple.

**Decision**: New `approval_policies` table with rows for (actor_kind, risk_class, action,
resource_type) → decision (Allow/Ask/Deny). Default policies ship as migration data. Users
with ADMIN+ role can customize policies. Policies are cached in `AppState` with a reload
endpoint.

### AD-25.5: Audit trail is the existing `audit_events` table, extended

**Rationale**: The `audit_events` table already has the right schema (append-only, per-profile
index, request_id, detail jsonb). Adding columns for the new authorization model extends it
without breaking existing queries.

**Decision**: Add columns to `audit_events` and create an audit query API:
- `authorization_decision` text: `'ALLOW' | 'ASK' | 'DENY'`
- `authorization_reason` text: human-readable reason
- `authorization_policy_id` uuid: which policy row matched (nullable)
- `actor_kind` text: kind of actor (human/agent/plugin/mcp/sandbox/webhook/system)
- `actor_id` text: specific actor identifier
- `resource_workspace_id` uuid: workspace scope for the resource

The existing `actor_user_id` and `profile_id` columns are preserved.

### AD-25.6: WebAuthn/OIDC as linked auth methods, not replacement

**Rationale**: Users should be able to add multiple auth methods (password, passkey, OIDC)
without losing existing sessions. This is a linked-identity model, not an either/or model.

**Decision**: New `auth_methods` table:
```sql
CREATE TABLE auth_methods (
    id uuid PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    method_type text NOT NULL CHECK (method_type IN ('password', 'webauthn', 'oidc')),
    method_data jsonb NOT NULL,  -- credential_id/public_key for webauthn, issuer/subject for OIDC
    label text,  -- user-friendly name like "YubiKey 5" or "Google"
    is_primary boolean NOT NULL DEFAULT false,
    last_used_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (user_id, method_type, label)
);
```

WebAuthn follows the WebAuthn Level 3 spec with `attestation=none` for simplicity.
OIDC follows the Authorization Code Flow with PKCE. Both are optional — password-only
still works.

### AD-25.7: CSRF hardening adds Fetch Metadata checks

**Rationale**: The existing `origin_guard` is good but browsers now send `Sec-Fetch-*` headers
that provide additional CSRF protection. Adding these checks with `Sec-Fetch-Site` already
handled — add `Sec-Fetch-Mode: navigate` rejection for state-changing endpoints and
`Sec-Fetch-Dest` verification.

**Decision**: Extend `origin_guard` middleware to also check:
- `Sec-Fetch-Mode: navigate` → reject on state-changing endpoints (navigations should use GET)
- `Sec-Fetch-Dest: empty` on POST/PUT/PATCH/DELETE → reject (should be `empty` only for
  same-origin XHR/fetch)
- Add `Cross-Origin-Opener-Policy: same-origin` and `Cross-Origin-Embedder-Policy: require-corp`
  headers for Spectre/transient execution protection

### AD-25.8: Session hardening via limits + tracking

**Rationale**: Defense-in-depth against session hijacking. Limit concurrent sessions per user,
track device/browser fingerprint, require step-up auth for sensitive operations (vault access,
user management, billing).

**Decision**:
- `MAX_CONCURRENT_SESSIONS` config (default 10, 1 for VIEWER)
- `users.auth_epoch` column already exists — increment on password change, session rotation,
  or forced logout; invalidates all existing sessions
- New `session_events` table: `{session_hash, event_type (created/rotated/expired/revoked/
  step_up), ip_hash, user_agent_hash, created_at}` — append-only
- Step-up auth: POST/PUT/DELETE on `/vault/*`, `/admin/*`, `/auth/rotate` require session
  age < `STEP_UP_MAX_AGE` (default 5 minutes) or re-authentication

### AD-25.9: Centralized authorization for context retrieval

**Rationale**: The roadmap explicitly requires context retrieval to pass through authorization.
Currently `build_messages` in `run_api.rs` fetches books from the library without checking
whether the agent is allowed to read each book.

**Decision**: Before a book is included in context, `authorize(Actor::Agent{...}, Action::ContextRetrieve{book_ids}, Resource::Book(id), ...)` is called. The authorization engine checks:
- Agent's declared permissions (`agents.permissions` JSONB)
- Book's `security_classification` (PUBLIC/INTERNAL/CONFIDENTIAL/RESTRICTED)
- Book's `scope` (GLOBAL/USER/PROFILE/WORKSPACE/CONVERSATION/AGENT/PRIVATE)
- Workspace membership (for WORKSPACE-scoped books)

Books the agent is not authorized to read are omitted from context with an audit log entry.

---

## Data Model (Migrations 0025+)

### Migration 0025: `auth_methods` + `session_events` + auth hardening schema

```sql
-- 0025_auth_hardening.sql
-- Schema 24 -> 25.

-- 1. Auth methods (WebAuthn, OIDC, password tracking)
CREATE TABLE auth_methods (
    id uuid PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    method_type text NOT NULL CHECK (method_type IN ('password', 'webauthn', 'oidc')),
    method_data jsonb NOT NULL,
    label text,
    is_primary boolean NOT NULL DEFAULT false,
    last_used_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (user_id, method_type, label)
);

-- Backfill: existing password users get a password auth_method
INSERT INTO auth_methods (id, user_id, method_type, method_data, label, is_primary, created_at)
SELECT
    gen_random_uuid(), id, 'password',
    jsonb_build_object('hash', password_hash),
    'Password', true, created_at
FROM users
ON CONFLICT DO NOTHING;

-- 2. Session events (append-only session lifecycle audit)
CREATE TABLE session_events (
    id bigserial PRIMARY KEY,
    session_hash bytea NOT NULL,
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    event_type text NOT NULL CHECK (event_type IN ('created','rotated','expired','revoked','step_up','login','logout')),
    ip_hash bytea,
    user_agent_hash bytea,
    metadata jsonb NOT NULL DEFAULT '{}'::jsonb,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX session_events_user_idx ON session_events (user_id, created_at DESC);
CREATE INDEX session_events_session_idx ON session_events (session_hash, created_at);

-- 3. User account lockout tracking
ALTER TABLE users ADD COLUMN consecutive_failures integer NOT NULL DEFAULT 0;
ALTER TABLE users ADD COLUMN locked_until timestamptz;

-- 4. Session revocation list (for forced logout)
CREATE TABLE session_revocations (
    token_hash bytea PRIMARY KEY,
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    revoked_by uuid REFERENCES users(id) ON DELETE SET NULL,
    reason text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX session_revocations_user_idx ON session_revocations (user_id, created_at DESC);

-- 5. Step-up auth tracking
ALTER TABLE sessions ADD COLUMN last_step_up_at timestamptz;

UPDATE schema_metadata SET schema_version = 25, updated_at = now() WHERE singleton;
```

### Migration 0026: `approval_policies` + `authorization_policies`

```sql
-- 0026_authorization_policies.sql
-- Schema 25 -> 26.

-- 1. Approval/authorization policy rules
CREATE TABLE authorization_policies (
    id uuid PRIMARY KEY,
    profile_id uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    priority integer NOT NULL CHECK (priority >= 0),
    actor_kind text NOT NULL CHECK (actor_kind IN ('human','agent','plugin','mcp','sandbox','webhook','system')),
    risk_class text CHECK (risk_class IS NULL OR risk_class IN ('read','write','execute','external_side_effect','destructive','admin')),
    action_pattern text NOT NULL DEFAULT '*',  -- glob pattern for action
    resource_type text CHECK (resource_type IS NULL OR resource_type IN (
        'conversation','workspace','book','plugin','mcp_server','sandbox_workspace',
        'secret','webhook','profile','audit_trail','system_config')),
    decision text NOT NULL CHECK (decision IN ('ALLOW','ASK','DENY')),
    reason text NOT NULL,
    enabled boolean NOT NULL DEFAULT true,
    scope text NOT NULL DEFAULT 'profile' CHECK (scope IN ('profile','workspace','agent','plugin','mcp_server','global')),
    scope_id uuid,  -- FK target depends on scope
    created_by uuid REFERENCES users(id) ON DELETE SET NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX authorization_policies_profile_idx ON authorization_policies (profile_id, priority);
CREATE INDEX authorization_policies_lookup_idx ON authorization_policies (profile_id, actor_kind, risk_class, enabled)
    WHERE enabled = true;

-- 2. Default policies (shipped with migration)
-- Single-user installs get empty policy table (OWNER = everything).

-- 3. Extend audit_events with authorization fields
ALTER TABLE audit_events ADD COLUMN authorization_decision text;
ALTER TABLE audit_events ADD COLUMN authorization_reason text;
ALTER TABLE audit_events ADD COLUMN authorization_policy_id uuid;
ALTER TABLE audit_events ADD COLUMN actor_kind text;
ALTER TABLE audit_events ADD COLUMN actor_id text;
ALTER TABLE audit_events ADD COLUMN resource_workspace_id uuid REFERENCES workspaces(id) ON DELETE SET NULL;

CREATE INDEX audit_events_actor_idx ON audit_events (actor_kind, actor_id, created_at DESC);
CREATE INDEX audit_events_decision_idx ON audit_events (authorization_decision, created_at DESC);

UPDATE schema_metadata SET schema_version = 26, updated_at = now() WHERE singleton;
```

### Migration 0027: Agent permissions + workspace isolation hardening

```sql
-- 0027_agent_permissions_isolation.sql
-- Schema 26 -> 27.

-- 1. Agent permission allowlists (checked at tool dispatch)
CREATE TABLE agent_tool_permissions (
    id uuid PRIMARY KEY,
    agent_id uuid NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    tool_pattern text NOT NULL,  -- glob: 'sandbox_*', 'library_search', '*'
    risk_class_limit text NOT NULL CHECK (risk_class_limit IN ('read','write','execute','external_side_effect','destructive','admin')),
    workspace_id uuid REFERENCES workspaces(id) ON DELETE CASCADE,  -- NULL = all workspaces
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (agent_id, tool_pattern, workspace_id)
);

-- 2. Agent secrets access
CREATE TABLE agent_secret_permissions (
    id uuid PRIMARY KEY,
    agent_id uuid NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    secret_id uuid NOT NULL REFERENCES secret_references(id) ON DELETE CASCADE,
    purpose text NOT NULL,
    max_uses integer,  -- NULL = unlimited
    use_count integer NOT NULL DEFAULT 0,
    expires_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (agent_id, secret_id, purpose)
);

-- 3. Workspace isolation: resource_workspace_id on all resource tables
-- (most already have workspace_id; this formalizes the audit trail)
-- No schema changes needed — existing workspace_id columns are authoritative.

-- 4. Workspace-level authorization policies
ALTER TABLE authorization_policies ADD CONSTRAINT authorization_policies_scope_check
    CHECK (
        (scope = 'workspace' AND scope_id IS NOT NULL) OR
        (scope != 'workspace')
    );

UPDATE schema_metadata SET schema_version = 27, updated_at = now() WHERE singleton;
```

### Migration 0028: Secrets access audit + enterprise audit viewer schema

```sql
-- 0028_secrets_audit_enterprise.sql
-- Schema 27 -> 28.

-- 1. Secret access log (append-only, separate from audit_events for volume)
CREATE TABLE secret_access_log (
    id bigserial PRIMARY KEY,
    secret_id uuid NOT NULL REFERENCES secret_references(id) ON DELETE CASCADE,
    actor_kind text NOT NULL,
    actor_id text NOT NULL,
    profile_id uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    purpose text NOT NULL,
    access_type text NOT NULL CHECK (access_type IN ('resolve','create','rotate','delete','denied')),
    authorization_decision text NOT NULL CHECK (authorization_decision IN ('ALLOW','DENY')),
    ip_address inet,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX secret_access_log_secret_idx ON secret_access_log (secret_id, created_at DESC);
CREATE INDEX secret_access_log_profile_idx ON secret_access_log (profile_id, created_at DESC);

-- Make the table append-only
CREATE OR REPLACE FUNCTION reject_secret_access_mutation()
RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'secret_access_log is append-only';
END; $$;

CREATE TRIGGER secret_access_log_append_only
    BEFORE UPDATE OR DELETE OR TRUNCATE ON secret_access_log
    FOR EACH STATEMENT EXECUTE FUNCTION reject_secret_access_mutation();

-- 2. Audit retention policy
ALTER TABLE audit_events ADD COLUMN retention_policy text NOT NULL DEFAULT 'permanent'
    CHECK (retention_policy IN ('permanent', '7_years', '1_year', '90_days'));
CREATE INDEX audit_events_retention_idx ON audit_events (retention_policy, created_at);

UPDATE schema_metadata SET schema_version = 28, updated_at = now() WHERE singleton;
```

---

## Central Authorization Flow

```mermaid
flowchart TD
    Request["Incoming request\n(HTTP / tool call / MCP / sandbox)"] --> ExtractActor["Extract Actor\n(headers, session, run context)"]
    ExtractActor --> SingleUser{"is_single_user_mode?"}
    SingleUser -->|yes, internal| AllowSU["ALLOW\n(reason: single_user_mode)"]
    SingleUser -->|no or external| BuildAction["Build Action + Resource\n(from request params)"]
    BuildAction --> CheckCache{"Policy cache hit?"}
    CheckCache -->|yes| CachedDecision["Use cached decision"]
    CheckCache -->|no| QueryPolicies["Query authorization_policies\nORDER BY priority"]
    QueryPolicies --> MatchPattern{"Pattern match\nactor_kind + risk_class + action + resource_type"}
    MatchPattern -->|no match| MatrixDefault["Use permission matrix default"]
    MatchPattern -->|matched| EffectiveDecision["decision ∩ matrix_default"]
    CachedDecision --> Audit
    MatrixDefault --> Audit
    EffectiveDecision --> Audit
    Audit["Write audit_events row\n(actor, action, resource, decision, reason, policy_id)"]
    Audit --> Decision{"Decision?"}
    Decision -->|ALLOW| Proceed["Proceed to handler"]
    Decision -->|ASK| CreateApproval["Create approval request\nnotify user"]
    Decision -->|DENY| Reject["Return 403 Forbidden\nwith reason"]
    CreateApproval --> Pending["Await user decision\n(timeout → DENY)"]
    Pending -->|approved| Proceed
    Pending -->|denied/timeout| Reject
```

**Plain-text summary**:

1. Every request extracts an `Actor` from the authentication context (session, run, plugin
   identity, MCP server identity).
2. If single-user mode (one OWNER, one profile, ≤1 workspace) and the actor is internal
   (human/agent/system), short-circuit to ALLOW — no policy queries, no approvals.
3. For multi-user mode or external actors, build an `Action` + `Resource` from the request.
4. Query `authorization_policies` ordered by priority. First matching policy (by
   `actor_kind`, `risk_class`, `action_pattern`, `resource_type`, `scope`) determines the
   decision.
5. The permission matrix provides a hard ceiling: even if a policy says ALLOW, the matrix
   default for that (actor_kind, action) tuple cannot be exceeded.
6. Write an `audit_events` row with the decision, reason, policy_id, actor_kind, actor_id.
7. ALLOW → proceed. ASK → create approval, notify user, block until decision. DENY → 403.
8. Policy cache in `AppState` (reloaded via `POST /api/v1/admin/policies/reload` or on
   policy mutation). Cache is an `Arc<RwLock<HashMap<(ActorKind, RiskClass, String), Decision>>>`.

---

## Permission Matrix (defaults)

Defaults are compiled constants. Policy rules can restrict (never expand) these.

| Actor | Action | Default | Notes |
|-------|--------|---------|-------|
| **Human (OWNER)** | All | ALLOW | Single-user: all ALLOW. Multi-user: OWNER = all ALLOW on own profile. |
| **Human (ADMIN)** | All (own profile) | ALLOW | Can manage users, workspaces. Cannot access other profiles unless shared. |
| **Human (MEMBER)** | Read (own workspace) | ALLOW | Can read conversations, books, sandbox in own workspace. |
| **Human (MEMBER)** | Write/Create (own workspace) | ASK | Create/update requires approval from OWNER/ADMIN. |
| **Human (MEMBER)** | Delete, Execute (destructive) | DENY | Cannot delete workspace resources or execute destructive tools. |
| **Human (VIEWER)** | Read (own workspace) | ALLOW | Read-only on assigned workspaces. |
| **Human (VIEWER)** | Write/Create/Delete/Execute | DENY | No mutations. |
| **Agent** | Read (library, context) | ALLOW | Agent can read books the user can read. |
| **Agent** | Execute (read tools) | ALLOW | library_search, library_load, sandbox_read_file, etc. |
| **Agent** | Execute (write tools) | ASK | library_add, sandbox_exec, terminal_* — requires per-tool approval. |
| **Agent** | Execute (destructive) | DENY | sandbox_remove, process_kill — blocked by default. |
| **Agent** | Create (conversation, book) | ASK | Agent creates content → user approves. |
| **Agent** | Delete | DENY | Agents never delete. |
| **Agent** | ContextRetrieve | ALLOW | Filtered by book scope + security_classification. |
| **Agent** | AccessSecret | DENY | Agents never access raw secrets. Tool auth via vault injection only. |
| **Plugin** | Read (filesystem) | ASK | Plugin declares `filesystem_read` at install; runtime access requires approval. |
| **Plugin** | Write (filesystem) | ASK | Scope-limited to declared paths. |
| **Plugin** | Network | ASK | Restricted to declared `host:port` pairs. |
| **Plugin** | Secrets | DENY | Plugins never access raw secrets. |
| **Plugin** | Subprocess/Admin | DENY | Blocked. |
| **MCP** | Tool call | ASK | Per-server approval policy; first tool call requires approval. |
| **MCP** | OAuth token refresh | ALLOW | Automatic; audited. |
| **Sandbox** | Exec (read-only) | ALLOW | ls, cat, stat, git log. |
| **Sandbox** | Exec (network) | ASK | curl, git fetch — requires network policy = FULL. |
| **Sandbox** | Exec (write/destructive) | ASK | rm, git push, process kill. |
| **Webhook** | Deliver | ALLOW | HMAC-verified, SSRF-checked. Audited. |
| **System** | Maintenance | ALLOW | Migrations, health checks, CSP header injection. |

---

## Approval Policy Model

### Lifecycle

```
Trigger (ASK decision)
    → Create approval_request row
    → Notify user (SSE / run event / webhook)
    → User decides: approve / deny
    → If timeout (configurable, default 5 min): auto-deny
    → Decision recorded in approval_request + audit_events
```

### New `approval_requests` table (replaces the existing `approvals` table)

```sql
CREATE TABLE approval_requests (
    id uuid PRIMARY KEY,
    profile_id uuid NOT NULL REFERENCES profiles(id) ON DELETE CASCADE,
    actor_kind text NOT NULL,
    actor_id text NOT NULL,
    action text NOT NULL,
    resource_type text NOT NULL,
    resource_id text,
    risk_class text,
    reason text NOT NULL,
    policy_id uuid REFERENCES authorization_policies(id) ON DELETE SET NULL,
    status text NOT NULL DEFAULT 'pending' CHECK (status IN ('pending','approved','denied','expired','canceled')),
    decided_by uuid REFERENCES users(id) ON DELETE SET NULL,
    decided_at timestamptz,
    expires_at timestamptz NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    metadata jsonb NOT NULL DEFAULT '{}'::jsonb
);
CREATE INDEX approval_requests_profile_status_idx ON approval_requests (profile_id, status, created_at DESC);
```

The existing `approvals` table (tied to `tool_call_id`) is deprecated and migrated.

### Approval UX

- **SSE/WebSocket push**: `run_events` with `event_type = 'approval_required'` and payload
  `{approval_id, action, resource, reason, expires_at}`.
- **UI**: Approval prompt rendered by the frontend as an actionable card (designer agent
  for UI details).
- **API**: `POST /api/v1/approvals/{id}/approve` and `POST /api/v1/approvals/{id}/deny`
  with optional `reason` in body.
- **API**: `GET /api/v1/approvals?status=pending` lists pending approvals for the user.

---

## Secrets Protection

### Current state (M24b)

Secrets are envelope-encrypted (AES-256-GCM) with an external master key. The vault
resolves secrets by purpose (e.g., `mcp_oauth_access_token`). Access is gated by
profile_id + secret_id. No per-read audit trail.

### M25b hardening

1. **Secret access goes through `authorize()`**: Every `Vault::resolve()` call checks
   `authorize(actor, Action::AccessSecret{secret_id, purpose}, Resource::Secret(secret_id), ...)`.
2. **Secret access logged**: Every resolve/create/rotate/delete writes a `secret_access_log`
   row (migration 0028).
3. **Agent secret access**: Agents never resolve secrets directly. Tool auth tokens are
   injected by the server at tool dispatch time (vault injection, not agent resolution).
   `agent_secret_permissions` table (migration 0027) controls which secrets an agent's
   tools can use — the agent itself never sees the raw value.
4. **Plugin secret access**: Plugins declare `secrets` permission domain at install time
   with scope values that are vault secret reference IDs (never values). Runtime access
   requires approval (ASK) and is audited.
5. **Step-up auth for vault management**: `POST/PUT/DELETE /api/v1/vault/*` requires
   `last_step_up_at < STEP_UP_MAX_AGE` (5 min) or re-authentication.

---

## Multi-User/Workspace Isolation

### Current state (M24b)

Workspace isolation is enforced at the SQL query level: every query filters by
`workspace_id` (or `profile_id`) based on the authenticated user's memberships.
Composite foreign keys enforce same-workspace references (migration 0010).

### M25b hardening

1. **All resource access goes through `authorize()`**: The authorization function checks
   workspace membership for workspace-scoped resources. Opaque IDs are not authorization.
2. **Cross-workspace access requires explicit policy**: By default, users can only access
   resources in their own workspaces. Cross-workspace sharing requires an explicit
   `authorization_policies` row or workspace membership.
3. **Agent workspace scoping**: Agents inherit the workspace scope of their parent run.
   An agent in workspace A cannot access workspace B's books, sandbox, or secrets.
4. **Plugin workspace scoping**: Workspace-scoped plugins (migration 0020,
   `plugins.workspace_id`) can only access that workspace's resources.
5. **MCP server isolation**: MCP servers are profile-scoped. Tool calls from an MCP server
   are authorized against the profile's policies.

---

## Enterprise Audit

### API

- `GET /api/v1/admin/audit?profile_id=&actor_kind=&actor_id=&action=&resource_type=&decision=&after=&before=&limit=` — requires ADMIN+ role.
- `GET /api/v1/admin/audit/export?format=csv|json&after=&before=` — export for SIEM integration.
- `GET /api/v1/admin/audit/summary?profile_id=&days=30` — summary: action counts by decision, top actors, policy effectiveness.

### Retention

- `audit_events.retention_policy` (migration 0028): `'permanent'` by default. Configurable
  per profile to `'7_years'`, `'1_year'`, `'90_days'`.
- A background task (configurable cron) deletes rows beyond the retention window.

### Attribution guarantee

Every `audit_events` row includes:
- `actor_kind` + `actor_id`: who performed the action
- `action` + `resource_type` + `resource_id`: what was done to what
- `authorization_decision` + `authorization_reason`: what the authz engine decided and why
- `authorization_policy_id`: which policy rule matched (traceability)
- `request_id`: correlation ID for tracing across services
- `profile_id` + `resource_workspace_id`: tenant + workspace scope

---

## API Changes

### M25a new/modified endpoints

| Method | Path | Auth | Description |
|--------|------|------|-------------|
| `GET` | `/api/v1/auth/methods` | User | List linked auth methods. |
| `POST` | `/api/v1/auth/methods/webauthn/register` | User | Start WebAuthn registration (returns challenge). |
| `POST` | `/api/v1/auth/methods/webauthn/complete` | User | Complete WebAuthn registration (verify attestation). |
| `POST` | `/api/v1/auth/methods/webauthn/login` | None | Start WebAuthn login (returns challenge). |
| `POST` | `/api/v1/auth/methods/webauthn/login/complete` | None | Complete WebAuthn login (verify assertion). |
| `POST` | `/api/v1/auth/methods/oidc/start` | None | Start OIDC flow (returns redirect URL). |
| `GET` | `/api/v1/auth/methods/oidc/callback` | None | OIDC callback (exchanges code for tokens). |
| `DELETE` | `/api/v1/auth/methods/{id}` | User | Remove an auth method (cannot remove last). |
| `POST` | `/api/v1/auth/sessions/revoke` | User | Revoke all other sessions. |
| `GET` | `/api/v1/auth/sessions` | User | List active sessions with device info. |
| `POST` | `/api/v1/auth/sessions/{hash}/revoke` | User | Revoke a specific session. |
| `POST` | `/api/v1/auth/step-up` | User | Re-authenticate for sensitive operations. |

### M25b new/modified endpoints

| Method | Path | Auth | Description |
|--------|------|------|-------------|
| `GET` | `/api/v1/admin/policies` | ADMIN+ | List authorization policies. |
| `POST` | `/api/v1/admin/policies` | ADMIN+ | Create authorization policy. |
| `PATCH` | `/api/v1/admin/policies/{id}` | ADMIN+ | Update authorization policy. |
| `DELETE` | `/api/v1/admin/policies/{id}` | ADMIN+ | Delete authorization policy. |
| `POST` | `/api/v1/admin/policies/reload` | ADMIN+ | Reload policy cache. |
| `GET` | `/api/v1/approvals` | User | List pending/completed approvals. |
| `GET` | `/api/v1/approvals/{id}` | User | Get approval details. |
| `POST` | `/api/v1/approvals/{id}/approve` | User | Approve a pending approval. |
| `POST` | `/api/v1/approvals/{id}/deny` | User | Deny a pending approval. |
| `GET` | `/api/v1/admin/audit` | ADMIN+ | Query audit trail. |
| `GET` | `/api/v1/admin/audit/export` | ADMIN+ | Export audit trail. |
| `GET` | `/api/v1/admin/audit/summary` | ADMIN+ | Audit summary statistics. |
| `GET` | `/api/v1/admin/secret-access` | ADMIN+ | Query secret access log. |

### Modified existing endpoints

- All existing endpoints: middleware injects `authorize()` call before handler dispatch.
  No API signature changes — authorization is transparent.
- `GET /api/v1/capabilities`: add `auth.methods` field listing supported methods
  (`["password", "webauthn", "oidc"]`).
- `GET /api/v1/auth/me`: add `auth_methods_count` field.
- `POST /api/v1/auth/login`: rate-limit hardening (IP-based + global limits).
- `POST /api/v1/vault/secrets`: requires step-up auth or recent re-auth.

---

## Backend Changes

### New modules

| Module | File | Responsibility |
|--------|------|----------------|
| `authorize` | `crates/gobrowse-server/src/authorize.rs` | Centralized authorization engine: `authorize()`, permission matrix, policy evaluation, single-user-mode detection. |
| `webauthn` | `crates/gobrowse-server/src/webauthn.rs` | WebAuthn registration + login: challenge generation, attestation/assertion verification, credential storage. |
| `oidc` | `crates/gobrowse-server/src/oidc.rs` | OIDC Authorization Code Flow with PKCE: discovery, redirect, token exchange, userinfo, account linking. |
| `session_manager` | `crates/gobrowse-server/src/session_manager.rs` | Session lifecycle management: creation, rotation, revocation, concurrent limits, step-up tracking, session events. |
| `approval_api` | `crates/gobrowse-server/src/approval_api.rs` | Approval request CRUD, approve/deny handlers, notification dispatch. |
| `admin_api` | `crates/gobrowse-server/src/admin_api.rs` | Admin endpoints: policy management, audit query/export, secret access log. |
| `rate_limiter` | `crates/gobrowse-server/src/rate_limiter.rs` | Multi-dimensional rate limiting (per-IP, per-email, global) using sliding window counters. |

### Modified modules

| Module | Change |
|--------|--------|
| `auth.rs` | Add WebAuthn/OIDC login handlers, `auth_methods` CRUD, session listing/revocation, step-up auth, enhanced rate limiting. `require_user_or_run` → delegates to `authorize()`. |
| `lib.rs` | Add `auth_policy_cache: Arc<RwLock<PolicyCache>>` to `AppState`. Add authorization middleware layer. |
| `config.rs` | Add `WebAuthnSettings`, `OidcSettings`, `RateLimitSettings`, `SessionSettings` to `Settings`. |
| `run_api.rs` | Replace ad-hoc `risk_class_for_tool` with `authorize()` call. Add `RiskClass` mapping for every tool (not just read/write). Context retrieval passes through `authorize()`. |
| `plugin_api.rs` | Runtime permission enforcement via `authorize()` (not just install-time `validate_permissions`). Plugin sandbox op authorization. |
| `mcp_client.rs` | Tool dispatch passes through `authorize()`. OAuth refresh audited. |
| `sandbox_client.rs` / `sandbox_api.rs` | Sandbox operations pass through `authorize()`. |
| `library_api.rs` | Book access checks via `authorize()` when scope/classification restricts. |
| `vault.rs` / `vault_api.rs` | Secret access passes through `authorize()` + `secret_access_log`. Step-up auth required for mutations. |
| `webhooks.rs` | Webhook delivery authorization. |
| `outbound_http.rs` | No changes (SSRF checks are network-layer, not authz). |
| `csp.rs` | No changes. |
| `ui_api.rs` | No changes. |

---

## Frontend Changes

### `gobrowse-web` changes

All UI for M25a/M25b enterprise features MUST be routed to the designer agent (frontend-design skill).
The following pages/components are REQUIRED but their internal design is NOT specified here:

1. **Auth Methods page**: List linked methods (password, passkeys, OIDC providers). Add/remove methods. Set primary method.
2. **WebAuthn registration flow**: Guided passkey creation with platform/browser prompts.
3. **OIDC login flow**: Redirect to provider, callback handling, account linking.
4. **Sessions page**: List active sessions with device/browser info. Revoke individual or all other sessions.
5. **Step-up auth modal**: Re-authentication prompt for sensitive operations.
6. **Approval center**: Inbox of pending approvals with approve/deny buttons. Approval history.
7. **Policy editor (admin)**: CRUD interface for authorization policies. Priority ordering, drag-to-reorder.
8. **Audit viewer (admin)**: Queryable audit trail with filters (actor, action, resource, decision, date range). Export button.
9. **Secret access log (admin)**: Queryable log of secret access events.

### `gobrowse-recovery` changes

- Add WebAuthn/OIDC login options to recovery login page.
- Add "Revoke all sessions" button (break-glass).

---

## Security Requirements

### Auth hardening (M25a)

1. WebAuthn: `attestation=none` (no hardware attestation — simplicity and privacy). RP ID from `public_origin`. User verification `preferred`. Resident key `preferred` (passkey).
2. OIDC: Authorization Code Flow with PKCE (S256). State parameter with HMAC. `nonce` in ID token. Claims verified: `iss`, `aud`, `exp`, `iat`, `sub`, `nonce`. No implicit flow.
3. Rate limiting: per-email (existing, 5/300s), per-IP (new, 20/300s), global (new, 100/60s per endpoint). Account lockout after 10 consecutive failures (30-minute lock).
4. CSRF: extend `origin_guard` with Fetch Metadata checks. Add `SameSite=Strict` on session cookie (already `__Host-` prefix implies Secure+Path=/).
5. Session rotation: on login (always), on role change (always), on password change (always), on sensitive operation (step-up). Increment `auth_epoch` on rotation.
6. Session hardening: max idle 60 min, max absolute 24 h, max concurrent 10 (OWNER) / 5 (ADMIN) / 3 (MEMBER) / 1 (VIEWER). Idle timeout resets on activity.

### Enterprise authorization (M25b)

1. Every resource access MUST go through `authorize()`. No direct DB queries that bypass authorization.
2. Every authorization decision MUST be audited (actor, action, resource, decision, reason, policy_id).
3. Agent tool dispatch MUST check `agent_tool_permissions` before execution. No default-allow for unknown tools.
4. Context retrieval MUST filter out books the agent cannot read (by scope + classification).
5. Secret access MUST be logged and step-up authenticated.
6. Approval timeouts MUST be enforced (default 5 min, configurable). Expired approvals auto-deny.
7. Policy cache MUST be invalidated on policy mutation. Stale cache window ≤ 1 second.
8. Single-user mode MUST NOT bypass plugin/MCP/sandbox authorization (external actors always checked).

### No regressions

1. CSP stays strict (no unsafe-inline, no unsafe-eval).
2. HttpOnly + Secure + SameSite=Strict session cookies preserved.
3. Origin guard + Fetch Metadata checks preserved.
4. Digest-verified artifacts (plugins, UI packages) preserved.
5. Server-side auth always authoritative — no client-side claims trusted.
6. No AI automatic elevation — agent approval required for ASK decisions.
7. Wasm builds still `wasm-opt -Oz --enable-bulk-memory`.
8. Docker image still minimal (debian:bookworm-slim + ca-certificates, no curl/git).

---

## Concurrency Requirements

1. **Policy cache**: `Arc<RwLock<HashMap<PolicyKey, CachedDecision>>>` with `RwLock::read()` for
   authorization checks (hot path) and `RwLock::write()` for cache reload. No per-request locking
   beyond the read guard.
2. **Rate limiting**: sliding window counters in memory with `tokio::sync::Mutex` per bucket.
   Counters expire after window elapses. No DB queries on the hot path.
3. **Session revocation**: `session_revocations` table checked on every session validation.
   Revoked sessions immediately invalid. Concurrent revocations are idempotent.
4. **Approval serialization**: `SELECT ... FOR UPDATE` on `approval_requests` row during
   approve/deny. Prevents double-processing.
5. **Audit append**: existing append-only triggers prevent any concurrent modification.
   New `authorization_decision` columns are write-once (set at INSERT, never updated).

---

## Ordered Implementation Steps (Batches)

### M25a Batch 1: Auth method infrastructure + rate limiter

**Schema**: Migration 0025 (auth_methods, session_events, session_revocations, user lockout).

**Files**:
- `crates/gobrowse-server/migrations/0025_auth_hardening.sql` (new)
- `crates/gobrowse-server/src/rate_limiter.rs` (new)
- `crates/gobrowse-server/src/config.rs` (add RateLimitSettings, SessionSettings)
- `crates/gobrowse-server/src/auth.rs` (integrate rate limiter, add session helpers)
- `crates/gobrowse-server/src/session_manager.rs` (new)
- `crates/gobrowse-server/src/lib.rs` (add to modules, no functional change)
- `crates/gobrowse-server/tests/postgres_integration.rs` (schema version → 25)
- `crates/gobrowse-server/tests/worktrees_integration.rs` (schema version → 25)

**Steps**:
1. Add `RateLimitSettings` and `SessionSettings` to `config.rs`.
2. Build `rate_limiter.rs`: sliding window counters, per-IP, per-email, global, configurable windows.
3. Write migration 0025.
4. Add `auth_methods` CRUD to `auth.rs` (list, delete auth methods; backfill existing passwords).
5. Build `session_manager.rs`: session creation with concurrent limits, session listing, revocation.
6. Integrate rate limiter into login endpoint (IP-based + global, in addition to existing per-email).
7. Update schema version assertions.

**Acceptance**:
- Migration applies cleanly: `cargo run -p gobrowse-server --bin gobrowse -- migrate`
- `POST /api/v1/auth/login` rate-limited: 20+ attempts from same IP in 300s → 429
- `GET /api/v1/auth/methods` returns at least password method for existing users
- `DELETE /api/v1/auth/methods/{id}` rejects last method
- `GET /api/v1/auth/sessions` lists active sessions
- `POST /api/v1/auth/sessions/{hash}/revoke` removes session
- `POST /api/v1/auth/sessions/revoke` removes all other sessions
- **Preserve M24/M24b**: `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings`
- **Preserve M24/M24b**: `cargo nextest run --workspace`
- **Preserve M24/M24b**: No CSP regression, no WASM size increase > 2%, Docker image ≤ +3 MB

### M25a Batch 2: WebAuthn registration + login

**Files**:
- `crates/gobrowse-server/src/webauthn.rs` (new)
- `crates/gobrowse-server/src/auth.rs` (add WebAuthn handlers)
- `crates/gobrowse-server/src/lib.rs` (add routes, webauthn module)
- `crates/gobrowse-server/src/config.rs` (add WebAuthnSettings: rp_id, rp_name, rp_origin)

**Steps**:
1. Add `WebAuthnSettings` to config. RP ID defaults to `public_origin` host.
2. Build `webauthn.rs`: challenge generation, `navigator.credentials.create()` options,
   attestation verification (none), assertion verification, credential storage in
   `auth_methods.method_data`.
3. Registration flow: `POST /auth/methods/webauthn/register` → returns creation options →
   browser creates credential → `POST /auth/methods/webauthn/complete` → verify + store.
4. Login flow: `POST /auth/methods/webauthn/login` → returns request options →
   browser gets assertion → `POST /auth/methods/webauthn/login/complete` → verify + create session.
5. Integration tests for registration and login flows (mock WebAuthn server-side).

**Acceptance**:
- WebAuthn registration: create options returned, attestation verified, credential stored
- WebAuthn login: assertion verified, session created
- Invalid assertion → 401
- Duplicate credential registration → 409
- **Preserve M24/M24b**: Full CI green
- **Preserve**: Password login still works (no regression)

### M25a Batch 3: OIDC login + CSRF hardening

**Files**:
- `crates/gobrowse-server/src/oidc.rs` (new)
- `crates/gobrowse-server/src/auth.rs` (add OIDC handlers)
- `crates/gobrowse-server/src/lib.rs` (add routes, extend origin_guard)
- `crates/gobrowse-server/src/config.rs` (add OidcSettings)

**Steps**:
1. Add `OidcSettings` to config: `providers: Vec<OidcProvider>` with `issuer`, `client_id`,
   `client_secret` (vault reference), `scopes`, `label`.
2. Build `oidc.rs`: OIDC discovery (`.well-known/openid-configuration`), Authorization Code
   Flow with PKCE (S256), token exchange, ID token verification (iss, aud, exp, iat, sub, nonce),
   userinfo, account linking.
3. Routes: `POST /auth/methods/oidc/start` (returns redirect URL), `GET /auth/methods/oidc/callback`
   (exchanges code, creates/links user, creates session).
4. Extend `origin_guard` middleware: add `Sec-Fetch-Mode: navigate` rejection on state-changing
   endpoints, `Sec-Fetch-Dest` verification, COOP/COEP headers.
5. Integration tests for OIDC flow (mock OIDC provider).

**Acceptance**:
- OIDC discovery fetches provider config
- Authorization URL includes PKCE challenge + state
- Callback verifies state, exchanges code, verifies ID token
- New user: account created, linked to OIDC. Existing user by email: OIDC linked.
- Sec-Fetch-Mode: navigate on POST /api/v1/conversations → 403
- COOP/COEP headers present on all responses
- **Preserve M24/M24b**: Full CI green

### M25a Batch 4: Session hardening + step-up auth

**Files**:
- `crates/gobrowse-server/src/session_manager.rs` (extend)
- `crates/gobrowse-server/src/auth.rs` (add step-up, session tracking)
- `crates/gobrowse-server/migrations/0025_auth_hardening.sql` (already applied, no new migration)

**Steps**:
1. Concurrent session limit enforcement: on login, count active sessions; if ≥ limit, revoke
   oldest session.
2. Session idle timeout: `last_seen_at` updated on each authenticated request. Background task
   expires idle sessions (every 60 s).
3. Step-up auth: `POST /auth/step-up` accepts password/passkey → updates `last_step_up_at`.
   Middleware checks step-up age for sensitive routes.
4. Account lockout: after 10 consecutive login failures, set `locked_until = now() + 30 min`.
   Lockout counts reset on successful login.
5. `auth_epoch` incremented on: password change, session rotation all, forced logout.
   Invalidates all existing sessions.
6. Session events written on: created, rotated, expired, revoked, step_up, login, logout.

**Acceptance**:
- 11th concurrent session for VIEWER → 403
- Idle session (60+ min) → expires, next request → 401
- Step-up required for vault mutations: old session → 403, re-auth → 200
- Account lockout after 10 failures → 423 Locked
- Auth epoch increment → all sessions invalidated
- Session events queryable per user
- **Preserve M24/M24b**: Full CI green

### M25b Batch 5: Centralized authorization engine

**Schema**: Migration 0026 (authorization_policies, audit_events extension).

**Files**:
- `crates/gobrowse-server/src/authorize.rs` (new)
- `crates/gobrowse-server/migrations/0026_authorization_policies.sql` (new)
- `crates/gobrowse-server/src/lib.rs` (add `auth_policy_cache` to AppState, add authorize middleware)
- `crates/gobrowse-server/src/admin_api.rs` (new — policy CRUD, reload)
- `crates/gobrowse-server/tests/postgres_integration.rs` (schema version → 26)
- `crates/gobrowse-server/tests/worktrees_integration.rs` (schema version → 26)

**Steps**:
1. Build `authorize.rs`: `Actor`, `Action`, `Resource` enums. `authorize()` function.
   `is_single_user_mode()` detection. Permission matrix constants. Policy evaluation by
   priority. Policy cache in `AppState`.
2. Write migration 0026.
3. Add policy CRUD to `admin_api.rs`: `GET/POST/PATCH/DELETE /api/v1/admin/policies`.
4. Add `POST /api/v1/admin/policies/reload` to invalidate cache.
5. Ship default policies as migration data (empty for single-user, sensible defaults for multi-user).
6. Extend `audit_events` with authorization columns (migration 0026).
7. Update audit helper in `auth.rs` to accept new fields.
8. Unit tests for every permission matrix entry × decision.

**Acceptance**:
- `authorize(Human(OWNER), Create{Book}, ...)` → ALLOW in single-user mode
- `authorize(Agent{...}, Execute{sandbox_rm, Destructive}, ...)` → DENY
- Policy `(agent, write, ASK)` overrides `(agent, execute, ASK)` → effective: ASK
- Policy cache: 1000 authz checks < 1 ms each
- Admin policies CRUD works
- Audit events include authorization_decision, reason, policy_id
- **Preserve M24/M24b**: Full CI green

### M25b Batch 6: Wire authorization into all call sites

**Files**:
- `crates/gobrowse-server/src/run_api.rs` (tool dispatch → authorize)
- `crates/gobrowse-server/src/run_tools.rs` (RiskClass mapping upgrade)
- `crates/gobrowse-server/src/plugin_api.rs` (runtime permission enforcement)
- `crates/gobrowse-server/src/mcp_client.rs` (tool call authorization)
- `crates/gobrowse-server/src/sandbox_api.rs` (sandbox op authorization)
- `crates/gobrowse-server/src/library_api.rs` (book access for context)
- `crates/gobrowse-server/src/vault_api.rs` (secret access authorization)
- `crates/gobrowse-server/src/vault.rs` (secret access logging)
- `crates/gobrowse-server/src/webhooks.rs` (delivery authorization)
- `crates/gobrowse-server/src/lib.rs` (middleware integration)

**Steps**:
1. **Run API**: Replace `risk_class_for_tool` binary mapping with full 6-level `RiskClass`.
   Before tool dispatch, call `authorize(Agent{...}, Execute{tool, risk}, ...)`.
   AD-25.9: context retrieval passes books through `authorize()`.
2. **Run tools**: Map each tool to its correct RiskClass (not just read/write):
   `sandbox_exec` → Execute, `sandbox_remove` → Destructive, `terminal_kill` → Destructive,
   `process_kill` → Destructive, `sandbox_mkdir` → Write, etc.
3. **Plugin API**: At runtime (not just install-time), check plugin sandbox operations
   against `plugin_permissions`. Call `authorize(Plugin{...}, Execute{tool, risk}, ...)`.
4. **MCP client**: Before `tools/call`, call `authorize(Mcp{...}, Execute{tool, risk}, ...)`.
   OAuth refresh: `authorize(Mcp{...}, AccessSecret{...}, ...)`.
5. **Sandbox API**: Every sandbox endpoint calls `authorize(Human{...} or Agent{...}, Execute{...}, ...)`.
6. **Library API**: Book creation/update/delete → `authorize()`. Context retrieval authorization
   in `run_api::build_messages`.
7. **Vault**: `Vault::resolve()` calls `authorize()` + writes `secret_access_log`.
   Vault mutations require step-up auth.
8. **Webhooks**: Delivery → `authorize(Webhook{...}, WebhookDeliver{...}, ...)`.
9. Add authorization middleware layer in `router()` that extracts actor and injects into
   request extensions (so handlers can access the pre-computed actor).

**Acceptance**:
- Agent calls `sandbox_exec` in workspace it belongs to → ALLOW (read tool)
- Agent calls `sandbox_remove` → DENY (destructive, no policy)
- Plugin with `filesystem_read` scope `/data/` tries to read `/etc/` → DENY (out of scope)
- MCP tool call first time → ASK (approval required), subsequent → ALLOW (after policy)
- Sandbox exec without sandbox enabled → 403
- Book with `security_classification=RESTRICTED` omitted from agent context
- Secret resolve logged in `secret_access_log`
- **Preserve M24/M24b**: Full CI green

### M25b Batch 7: Agent permissions + workspace isolation hardening

**Schema**: Migration 0027 (agent_tool_permissions, agent_secret_permissions).

**Files**:
- `crates/gobrowse-server/migrations/0027_agent_permissions_isolation.sql` (new)
- `crates/gobrowse-server/src/authorize.rs` (add agent permission checks)
- `crates/gobrowse-server/src/run_api.rs` (integrate agent tool permissions)
- `crates/gobrowse-server/tests/postgres_integration.rs` (schema version → 27)
- `crates/gobrowse-server/tests/worktrees_integration.rs` (schema version → 27)

**Steps**:
1. Write migration 0027.
2. `authorize.rs`: for `Actor::Agent`, check `agent_tool_permissions` for tool pattern match
   + risk_class_limit. If no matching permission row, DENY by default.
3. Agent secrets: `Vault::resolve()` checks `agent_secret_permissions` for agent actors.
   Use count increment, expiry check.
4. Workspace isolation: `authorize()` always checks `resource_workspace_id` against actor's
   workspace membership. Cross-workspace access requires explicit policy.
5. Integration tests for agent permission enforcement.

**Acceptance**:
- Agent with `tool_pattern='library_*' risk_class_limit='write'` can call `library_search` (read)
  and `library_add` (write) but not `sandbox_exec` (no permission row) → DENY
- Agent with expired `agent_secret_permissions` → DENY
- Agent with exhausted `max_uses` → DENY
- Cross-workspace agent → DENY without explicit policy
- **Preserve M24/M24b**: Full CI green

### M25b Batch 8: Approval system + notification dispatch

**Schema**: New `approval_requests` table (replaces `approvals`), migration 0027.

**Files**:
- `crates/gobrowse-server/src/approval_api.rs` (new)
- `crates/gobrowse-server/src/authorize.rs` (ASK decision → create approval)
- `crates/gobrowse-server/src/realtime.rs` (extend — push approval events)
- `crates/gobrowse-server/src/lib.rs` (add routes)

**Steps**:
1. Create `approval_requests` table in migration 0027.
2. `authorize()`: when decision is ASK, create `approval_requests` row, return
   `AuthorizationDecision::Ask { approval_id }`.
3. `approval_api.rs`: `GET /approvals`, `GET /approvals/{id}`, `POST /approvals/{id}/approve`,
   `POST /approvals/{id}/deny`.
4. Notification: push `run_events` with `event_type='approval_required'` for agent-initiated
   approvals. For human-initiated (MEMBER trying to write), push to workspace activity feed.
5. Timeout: background task expires pending approvals past `expires_at`. Auto-deny.
6. Migrate existing `approvals` table data to `approval_requests` (if any rows exist).

**Acceptance**:
- Agent tool call triggers ASK → approval_requests row created
- User approves → agent tool proceeds
- User denies → agent gets DENY error
- Timeout (5 min) → auto-deny
- Approval events appear in run events / SSE stream
- **Preserve M24/M24b**: Full CI green

### M25b Batch 9: Enterprise audit API + secret access hardening

**Schema**: Migration 0028 (secret_access_log, audit_events retention).

**Files**:
- `crates/gobrowse-server/migrations/0028_secrets_audit_enterprise.sql` (new)
- `crates/gobrowse-server/src/admin_api.rs` (extend — audit query, export, summary)
- `crates/gobrowse-server/src/vault.rs` (secret access logging)
- `crates/gobrowse-server/tests/postgres_integration.rs` (schema version → 28)
- `crates/gobrowse-server/tests/worktrees_integration.rs` (schema version → 28)

**Steps**:
1. Write migration 0028.
2. `secret_access_log`: `Vault::resolve()` writes a row on every secret resolution
   (actor, purpose, access_type, decision, IP). Append-only trigger.
3. Admin audit API: `GET /admin/audit` with filters (actor_kind, action, resource_type,
   decision, after, before, limit, offset). `GET /admin/audit/export` (CSV/JSON).
   `GET /admin/audit/summary` (counts by decision, top actors, policy effectiveness).
4. Secret access API: `GET /admin/secret-access` with filters.
5. Audit retention: background task deletes rows past `retention_policy`.
6. Integration tests for audit query + export.

**Acceptance**:
- Secret resolve logs to `secret_access_log`
- `GET /admin/audit?actor_kind=agent&decision=DENY` returns filtered results
- `GET /admin/audit/export?format=csv` returns CSV
- `GET /admin/audit/summary` returns correct counts
- `secret_access_log` is append-only (UPDATE/DELETE rejected)
- Retention background task deletes expired rows
- **Preserve M24/M24b**: Full CI green

### M25b Batch 10: Frontend integration + E2E + regression

**Steps**:
1. **Designer agent**: Route all UI components (auth methods, sessions, approval center,
   policy editor, audit viewer) to the designer agent for implementation.
2. Full CI: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
   `cargo clippy -p gobrowse-web --target wasm32-unknown-unknown -- -D warnings`,
   `cargo clippy -p gobrowse-recovery --target wasm32-unknown-unknown -- -D warnings`,
   `cargo nextest run --workspace`.
3. Docker build + smoke test: verify health, all auth flows, authorization enforcement,
   audit trail, CSP headers.
4. Multi-user scenario test: create OWNER + MEMBER, verify MEMBER cannot delete workspace,
   verify member write → approval, verify agent tool permissions.
5. Single-user regression: verify no approvals required, no policy setup, everything works.
6. Populate regression matrix against M24 optimization baselines.

**Acceptance**:
- All CI green
- Docker smoke passes
- WebAuthn registration + login works end-to-end
- OIDC login works end-to-end
- Rate limiting triggers correctly
- CSRF Fetch Metadata checks work
- Session revocation works
- Authorization engine: ALLOW/ASK/DENY decisions correct for every actor
- Approval flow: request → notify → approve → proceed
- Audit trail: every decision logged with attribution
- Secret access logged
- Single-user mode: no prompts, all ALLOW
- **Preserve M24/M24b**: Image size ≤ +10 MB, WASM size ≤ +15%, context budget unchanged,
  latency for non-auth endpoints ≤ +5%

---

## Rollout / Rollback Plan

### Rollout

1. **M25a first**: Auth hardening ships independently. WebAuthn/OIDC, rate limiting, CSRF,
   session hardening are additive — no breaking changes.
2. **M25b batches 5-6**: Authorization engine ships with default policies that match existing
   behavior (ALLOW for everything existing users already could do). No behavioral change.
3. **M25b batches 7-9**: Enterprise features activate only when multi-user/workspace config
   exists. Single-user installs see no change.
4. **Schema migrations**: Apply sequentially (0025 → 0026 → 0027 → 0028). Each migration
   is backward-compatible (additive columns, no drops).

### Rollback

1. **Per-migration**: Each migration adds columns/tables only. No data is destroyed. Rollback
   = deploy previous binary (which ignores new columns).
2. **Auth methods**: Removing WebAuthn/OIDC support = deploy binary without those modules.
   Users with only WebAuthn/OIDC methods would be locked out — password method is always
   required as fallback (enforced: cannot delete last method, password method cannot be
   deleted if it's the only method).
3. **Authorization engine**: Policy cache can be cleared (`DELETE FROM authorization_policies`)
   and engine falls back to permission matrix defaults. Single-user mode bypasses policies
   entirely.

---

## Validation Commands (post-M25)

```bash
# ---- Pre-flight ----
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo clippy -p gobrowse-web --target wasm32-unknown-unknown -- -D warnings
cargo clippy -p gobrowse-recovery --target wasm32-unknown-unknown -- -D warnings

# ---- Full test suite ----
GOBROWSE_TEST_DATABASE_URL=postgres://gobrowse:test-only-password@localhost:5432/gobrowse_test \
  cargo nextest run --workspace

# ---- Authorization engine unit tests ----
cargo test -p gobrowse-server -- authorize::tests --nocapture

# ---- WASM builds ----
cd crates/gobrowse-web && trunk build --release && wasm-opt -Oz -o ../../dist/optimized.wasm ../../dist/*.wasm
cd crates/gobrowse-recovery && trunk build --release

# ---- Docker build + smoke ----
docker build -t gobrowse-os-app:m25 .
docker images gobrowse-os-app:m25 --format '{{.Size}}'
docker compose up -d && sleep 15 && docker compose ps
curl --fail http://127.0.0.1:8080/health/ready
curl http://127.0.0.1:8080/api/v1/version
curl http://127.0.0.1:8080/api/v1/capabilities | jq .
curl -I http://127.0.0.1:8080/ 2>&1 | grep -i content-security-policy
curl -I http://127.0.0.1:8080/ 2>&1 | grep -i cross-origin

# ---- Auth flow tests ----
# Password login (existing)
curl -X POST http://127.0.0.1:8080/api/v1/auth/login -H 'Content-Type: application/json' \
  -d '{"email":"test@example.com","password":"test"}'

# Auth methods
curl http://127.0.0.1:8080/api/v1/auth/methods -b cookies.txt

# Sessions
curl http://127.0.0.1:8080/api/v1/auth/sessions -b cookies.txt

# Rate limit test (run 25 times fast)
for i in $(seq 1 25); do
  curl -s -o /dev/null -w "%{http_code}\n" -X POST http://127.0.0.1:8080/api/v1/auth/login \
    -H 'Content-Type: application/json' -d '{"email":"wrong@example.com","password":"wrong"}'
done

# ---- Authorization tests (multi-user, post-setup) ----
# Admin: list policies
curl http://127.0.0.1:8080/api/v1/admin/policies -b cookies.txt

# Admin: query audit
curl "http://127.0.0.1:8080/api/v1/admin/audit?limit=10&decision=DENY" -b cookies.txt

# Admin: secret access log
curl "http://127.0.0.1:8080/api/v1/admin/secret-access?limit=10" -b cookies.txt

# Approvals
curl http://127.0.0.1:8080/api/v1/approvals?status=pending -b cookies.txt

docker compose down
```

---

## `Preserve M24/M24b Baseline` Checks (per batch)

Every batch MUST verify before completion:

| Check | Command/Verification |
|-------|---------------------|
| No `unsafe` added | `rg 'unsafe\b' crates/ --include '*.rs'` — only existing `unsafe` in wasm-bindgen glue |
| Clippy clean | `cargo clippy --workspace --all-targets -- -D warnings` |
| Format clean | `cargo fmt --check` |
| Tests pass | `cargo nextest run --workspace` |
| Schema version correct | Migration applies cleanly; integration test assertions updated |
| Endpoint count preserved | Every existing `/api/v1/*` route still registered |
| Tool defs unmodified | `tool_definitions()` returns same set as M24 (risk classes may change from `"read"/"write"` to specific levels — that's intentional) |
| Context budget unchanged | `build_messages` budget logic untouched except authorization filtering |
| WASM size ≤ M24b +15% | `ls -l dist/*.wasm` compared against M24b baseline |
| Docker image ≤ M24b +10 MB | `docker images` compared against M24b baseline |
| Lazy tool schemas still work | Round 1 still sends 3 base tools only |
| Sandbox lazy connection intact | `SandboxHandle` unchanged |
| CSP strict (no unsafe-inline) | `curl -I http://127.0.0.1:8080/ \| grep CSP` — no `unsafe-inline`, no `unsafe-eval` |
| Recovery UI accessible | `curl --fail http://127.0.0.1:8080/recovery` |
| Single-user mode unbroken | One OWNER, one profile: all operations ALLOW, no approval prompts |

---

## Open Questions

1. **WebAuthn library choice**: Options: `webauthn-rs` (mature, async), `passkey-rs` (lightweight),
   or handwritten WebAuthn (minimal deps, more code). **Recommend**: `webauthn-rs` for spec
   compliance and security review track record. Decision needed before Batch 2.

2. **OIDC provider configuration**: Should the OIDC provider list be config-file-only (static)
   or database-backed (runtime CRUD)? Config-file is simpler and more secure (no DB tampering).
   **Recommend**: config-file only for M25a; database-backed in later milestone if needed.

3. **Rate limiter storage**: In-memory (fast, lost on restart) vs Redis (shared, persistent)
   vs PostgreSQL (durable, slower). Single-server deployments (the normal case) only need
   in-memory. **Recommend**: in-memory with optional Redis backend gated by config flag.

4. **Approval timeout UX**: When an approval times out, should the agent retry automatically
   or fail permanently? **Recommend**: fail permanently with a clear error message; agent
   can re-request if the user wants.

5. **Audit retention background task**: Should retention cleanup run as a tokio background
   task in the server process, or as a separate cron-like command (`gobrowse audit cleanup`)?
   **Recommend**: background task in server (configurable interval, default 1 h) for
   simplicity; separate command for manual runs.

6. **Agent `permissions` JSONB migration**: The existing `agents.permissions` column is
   unstructured JSONB. Should M25b migrate it to `agent_tool_permissions` rows?
   **Recommend**: read `agents.permissions` at migration time and insert corresponding
   `agent_tool_permissions` rows; drop the JSONB column after migration 0027.

7. **Single-user mode detection trigger**: Is single-user mode re-evaluated on every
   request or cached? Cached would be faster but could be stale after user/workspace
   creation. **Recommend**: cached with `RwLock`, invalidated on user create/delete
   and workspace create/delete. Stale window ≤ 1 write operation.

8. **Frontend WebAuthn/OIDC UI**: The browser WebAuthn API requires user interaction
   (navigator.credentials.create/get). The OIDC flow requires redirects. The frontend
   designer agent must handle these browser-specific flows. **Recommend**: provide
   the designer agent with WebAuthn browser API docs and OIDC flow diagrams.

9. **MCP approval policy granularity**: Should MCP tool approval be per-server (first
   tool call requires approval) or per-tool (each tool independently approved)?
   **Recommend**: per-server default (matching existing behavior), with optional
   per-tool override via `authorization_policies` rows scoped to `mcp_server`.

10. **Plugin runtime permission enforcement mechanism**: How does the server enforce
    that a plugin doesn't exceed its declared `filesystem_read` scope at runtime? The
    sandbox client doesn't currently accept per-operation path restrictions.
    **Recommend**: pass `allowed_paths` and `allowed_hosts` from `plugin_permissions`
    to the sandbox client as operation parameters. Sandboxd enforces the restrictions.

---

## M24 Optimization Baseline (permanent, non-negotiable)

All M25 builds MUST maintain the M24 optimization baseline confirmed at M24b:

- `debian:bookworm-slim` + `ca-certificates` only (no curl/git)
- `wasm-opt -Oz --enable-bulk-memory` on all WASM binaries
- Lazy tool schemas: round 1 = 3 base tools only
- Lazy sandbox connection via `SandboxHandle` (OnceLock)
- Cheapest-capable routing via `cost_ranking`
- `unsafe_code = "forbid"`, `clippy::all = "warn"`, Rust 1.94, edition 2024
- Context budget: `build_messages` 2/3 recent, 1/3 optional, LIMIT 12 library, LIMIT 10 worktrees
- Docker image: read_only, tmpfs /tmp, no-new-privileges, cap_drop: ALL
- CSP strict: no unsafe-inline, hash-verified, server-authoritative