# PLAN.md — M24: Full Token, Runtime, Container & Architecture Optimization

Authoritative architecture and implementation plan for M24. This is the **current batch plan**;
prior M22 architecture is preserved in git history. M24 lands on branch `initial-agent-os`.

**Schema version: 22. No new migrations unless explicitly listed. All M24 work is code-level optimization.**

## M24 Goal

Dramatically lighter Gobrowse OS without losing capability. Same-or-better capability, less
context/RAM/disk/CPU/latency, smaller containers, simpler code. Non-negotiable: **do NOT remove
features**. Pay for capabilities only when needed. M24 is the **permanent baseline** for all later
builds.

## Current State (observed facts from actual code)

### Image/Binary baselines (M23, commit f54a2ca)
| Metric | M23 value |
|---|---|
| Docker image (`gobrowse-os-app:m23`) | 275 MB (69.9 MB compressed) |
| dist WASM raw | 2,669,588 bytes |
| dist WASM gzip | 798,996 bytes |
| Release profile | thin-LTO, codegen-units=1, strip=symbols, panic=abort |

### Runtime architecture (observed)
- **Schema version 22** — last migration `0022_router.sql`. Asserted at `postgres_integration.rs:42`.
- **Tool definitions**: 3 base tools (`library_search`, `library_add`, `library_load`) + ~15 sandbox tools
  (`sandbox_exec`, `sandbox_read_file`, `sandbox_write_file`, `sandbox_list_files`, `sandbox_stat`,
  `sandbox_mkdir`, `sandbox_remove`, `terminal_start`, `terminal_input`, `terminal_read_output`,
  `terminal_resize`, `terminal_interrupt`, `terminal_close`, `process_list`, `process_kill`).
- **Tool schemas sent IN FULL every round** in `execute_inner` (`run_api.rs:~804`):
  `tool_defs.to_vec()` clones the full `Arc<Vec<ToolDefinition>>` inner Vec each of up to 8 rounds.
- **Context budget** (`run_api.rs:build_messages`): `budget = context_window - output_limit`.
  SYSTEM_POLICY reserved first; 2/3 of remainder for recent messages; remainder for library/worktree/pinned
  candidates via `build_context` priority selection.
- **Agent loop**: max 8 rounds, 16 tool calls, 10 s per-tool timeout, 64 KiB output cap.
- **Token estimation**: `chars.count().div_ceil(4)` — ~4 chars/token heuristic.
- **Model routing**: `select_model_for_task` checks `model_task_routes` table for task-specific route,
  falls back to position-ordered fallback chain. No cost-based ordering within chain.
  `model_limits` takes MIN(context_window) and MIN(output_limit) across chain.
- **Sandbox client**: connected eagerly in `AppState::new`, stored as `Option<SandboxClient>`.
  A down daemon does NOT fail startup (connection error → `None`), but connection IS attempted.
- **MCP clients**: already lazy via `McpClientPool` (created on first `library_load` for MCP book).
- **Plugins**: already dormant (installed in DB, loaded on demand via `library_load`).

### Constraints & invariants
- `unsafe_code = "forbid"` (workspace-level)
- `clippy::all = "warn"` (workspace-level)
- Rust 1.94, edition 2024, nextest
- Schema version 22; DO NOT bump unless adding an index/DEFAULT
- No feature removal — every existing API, tool, endpoint MUST still work
- Keep boring, LLM-editable code
- Architecture map doc required
- Before/after benchmark table required
- Capability regression matrix required

## Two Running Slices (do NOT duplicate)

### Slice A: WasmSizeSlice (`WasmSizeSlice-3`, running)
- Measure wasm-opt -Oz delta on M24 build
- Remove unsupported `Trunk.toml [tools] wasm_opt` key if present
- Wire wasm-opt into CI + Dockerfile with hard failure (remove `|| true`)

### Slice B: RuntimeBaselineSlice (`RuntimeBaselineSlice-3`, running)
- Docker compose baselines → `docs/m24-baselines.md`
- New integration test: `crates/gobrowse-server/tests/m24_stress_integration.rs`
  (500 books + 100 plugins bounded-context regression test)

## Architectural Decisions (M24)

### AD-1: Lazy tool schemas — send only what the agent can use

**Current**: All ~18 tool definitions (full JSON schema with descriptions, input_schema, property
constraints) are sent to the provider every single round (up to 8 rounds). This wastes ~15 sandbox
tool schemas × ~200-500 chars each × 8 rounds ≈ 24-60K chars of context (~6-15K tokens) every run
where sandbox tools are never called.

**Decision**: Two-phase tool advertisement.
1. **Round 1**: Advertise only the 3 base tools (`library_search`, `library_add`, `library_load`).
2. **After first sandbox tool is called**: Unlock all sandbox tools for subsequent rounds.
3. **After first terminal tool is called**: All terminal tools already visible (they share the
   sandbox unlock gate — once any sandbox tool is used, the full sandbox suite is available).

**Implementation**: In `execute_inner` (`run_api.rs`), replace the single `tool_defs` variable with
a stateful filter. Track `sandbox_activated: bool`. On each round rebuild:
```rust
let active_tools: Vec<ToolDefinition> = if sandbox_activated {
    tool_defs_inner.clone()  // all tools
} else {
    tool_defs_inner.iter().filter(|t| {
        matches!(t.id.as_str(), "library_search" | "library_add" | "library_load")
    }).cloned().collect()
};
```
Set `sandbox_activated = true` when any sandbox tool is executed (the match branch for `_` with
`sandbox_tool_kind` returning `Some`).

**Tradeoff**: Agent cannot discover sandbox tools until it has a reason to use them. This is
intentional — the agent discovers tools through library search results (which describe
capabilities) and loads them on demand. If an agent needs to use sandbox tools without a
library search, it can still call `library_search` to discover them. This matches the
"pay for capability only when needed" principle.

**Files**: `crates/gobrowse-server/src/run_api.rs` — `execute_inner` function only.

### AD-2: Tool schema size reduction

**Current**: Each tool definition carries verbose input_schema JSON with `additionalProperties: false`,
redundant `maxLength` annotations, verbose property descriptions.

**Decision**: Strip the following from ALL tool schemas:
- `additionalProperties: false` — not required by any supported provider (OpenAI-compatible and
  Ollama both accept schemas without it).
- Redundant `maxLength` on strings when already constrained by the tool's server-side validation.
- Verbose `description` strings on properties (keep the short form or remove).

**Implementation**: Edit `run_tools.rs` `tool_definitions` and `sandbox_tool_definitions` functions.
Before/after character count must be measured. Estimated saving: 30-40% of tool schema tokens.

**Files**: `crates/gobrowse-server/src/run_tools.rs` only.

### AD-3: Container image minimization

**Current**: `debian:bookworm-slim` (≈74 MB base) + `ca-certificates` + `curl` + `git`.

**Decision**: Three-tier approach:
1. **Remove curl from runtime**: Docker compose healthcheck uses curl. Replace with a native
   health endpoint check via the gobrowse binary itself: `gobrowse health` subcommand that hits
   `GET /health/ready` and exits 0/1. Remove `curl` apt package.
2. **Remove git from runtime**: Only needed for plugin source resolution. Make it optional —
   if git is absent, plugin installs from git sources fail gracefully with "git not available".
   Remove `git` apt package from runtime image.
3. **Switch base to `debian:bookworm-slim` stays**: `scratch` requires static binary (we link
   glibc dynamically). `distroless` has no package manager. `alpine` uses musl — incompatible
   with the glibc binary. **Stay with bookworm-slim** but minimize packages to `ca-certificates`
   only.

**Implementation**: 
- New `gobrowse health` subcommand: `crates/gobrowse-server/src/main.rs` — simple HTTP GET
  to `http://127.0.0.1:{port}/health/ready`, exit 0 on 200, exit 1 otherwise.
- `Dockerfile`: runtime stage `apt-get install -y --no-install-recommends ca-certificates` only.
- `docker-compose.yml`: healthcheck → `["CMD", "gobrowse", "health"]`

**Tradeoff**: Plugin GitHub source resolution via `git clone` will fail if git is absent. The
server already has `plugin_github.rs` with fallback to release tarball downloads (no git needed).
Git is only used for `source_type = 'generic_git'` plugins. Mark this as a documented limitation
in M24 release notes.

**Files**: `Dockerfile`, `docker-compose.yml`, `crates/gobrowse-server/src/main.rs`.

### AD-4: Library search covering index

**Current**: `build_messages` runs this query every run:
```sql
SELECT b.id, b.title, b.kind, b.book_type, b.trust, b.provenance, b.tags, b.revision,
       ts_headline(...) AS snippet
FROM books b
WHERE b.profile_id = $2
  AND b.security_classification <> 'RESTRICTED'
  AND b.scope <> 'AGENT' AND b.scope NOT IN ('USER','PRIVATE')
  AND b.scope IN ('GLOBAL','PROFILE','WORKSPACE','PROJECT')
  AND (b.scope NOT IN ('WORKSPACE','PROJECT') OR EXISTS(...))
  AND ($3::uuid IS NULL OR b.scope IN ('GLOBAL','PROFILE') OR b.workspace_id=$3)
  AND (b.kind IS NULL OR b.kind IN ('SOURCE','SKILL','MCP','PLUGIN'))
  AND b.book_type <> 'AUTOBIOGRAPHY'
  AND b.search_document @@ websearch_to_tsquery('english', $1)
ORDER BY ts_rank_cd(b.search_document, websearch_to_tsquery('english', $1)) DESC, b.updated_at DESC
LIMIT 12
```

**Decision**: Add a partial index covering the common filter path. The current GIN index on
`search_document` handles the `@@` clause, but the ORDER BY + filter selectivity can be improved.

```sql
CREATE INDEX books_agent_retrieval_idx ON books (profile_id, updated_at DESC)
    WHERE security_classification <> 'RESTRICTED'
      AND book_type <> 'AUTOBIOGRAPHY'
      AND scope NOT IN ('AGENT', 'USER', 'PRIVATE');
```

This is a **migration 0023** (non-breaking, additive only). Does not change schema_version test
assertions if they only check `>= 22`. Verify: `postgres_integration.rs:42` asserts
`schema_version == 22`. Must bump to 23 there.

**Tradeoff**: Small index overhead (~few MB per profile). The query already has LIMIT 12, so
the index benefit is moderate. Impact: reduced planning time + faster filtering for profiles
with many books.

**Files**: `crates/gobrowse-server/migrations/0023_agent_retrieval_index.sql`,
`crates/gobrowse-server/tests/postgres_integration.rs` (schema version assertion).

### AD-5: Sandbox client lazy connection

**Current**: `AppState::new` eagerly connects to sandboxd:
```rust
let sandbox = if settings.features.sandbox {
    match (&settings.features.sandbox_socket_path, &settings.features.sandbox_auth_token) {
        (Some(socket_path), Some(auth_token)) => Some(
            SandboxClient::connect(SandboxConfig { ... }).await.map_err(...)?
        ),
        _ => None,
    }
} else { None };
```
A failed connection → `AppError::Internal`, which fails server startup entirely.

**Decision**: Never fail startup for a down sandbox daemon. Defer connection to first sandbox
tool call. Store `SandboxConfig` in `AppState` and lazily connect on demand.

**Implementation**:
- `AppState.sandbox` changes from `Option<SandboxClient>` to `Option<Arc<tokio::sync::OnceCell<SandboxClient>>>` or a simpler `Option<Arc<Mutex<Option<SandboxClient>>>>` with lazy init.
- Sandbox tool execution (`run_tools.rs:execute_sandbox_op`) checks if client exists, connects if not, caches result.
- Startup always succeeds; first sandbox tool call may fail with "sandbox unavailable — daemon not reachable."

**Files**: `crates/gobrowse-server/src/lib.rs` (`AppState` struct + `AppState::new`),
`crates/gobrowse-server/src/run_tools.rs` (`execute_sandbox_op`).

### AD-6: Cheapest-capable routing in fallback chain

**Current**: `load_routes` orders by `route.position` (from `model_fallback_routes`). No cost
signal. `select_model_for_task` only selects task-specific routes or first-in-chain.

**Decision**: Add a `cost_ranking` column (integer, lower = cheaper, 0-100) to the `models` table.
Sort fallback routes by `cost_ranking ASC NULLS LAST, position ASC`. For task-specific routes,
still use the configured route but log when a cheaper model was available.

**Implementation**:
- Migration 0023 (shared with AD-4): `ALTER TABLE models ADD COLUMN cost_ranking integer;`
- `chat.rs:load_routes`: add `ORDER BY m.cost_ranking ASC NULLS LAST, route.position`
- `router.rs:select_model_for_task`: add cost-awareness logging
- No UI changes required (cost_ranking is server-internal; UI already shows model name)

**Files**: `crates/gobrowse-server/migrations/0023_agent_retrieval_index.sql`,
`crates/gobrowse-server/src/chat.rs`, `crates/gobrowse-server/src/router.rs`.

### AD-7: Architecture map documentation

**Required by M24 spec**. Creates `docs/architecture-map.md` showing:
- Crate dependency graph (gobrowse-core → gobrowse-server, gobrowse-sandboxd, gobrowse-web)
- Module map within gobrowse-server
- Data flow: HTTP → router → run_api → chat/router/run_tools → providers/sandbox/MCP
- Context assembly flow: build_messages → build_context → model request
- Tool execution flow: execute_inner re-entrant loop
- Deployment topology: docker compose (app + pgvector)

**Files**: `docs/architecture-map.md` (new).

### AD-8: Capability regression matrix

**Required by M24 spec**. Creates `docs/m24-regression-matrix.md`:
- Every M24 optimization listed with the capabilities it MUST NOT degrade
- Before/after measurements for each
- Test commands to verify each capability is preserved

**Files**: `docs/m24-regression-matrix.md` (new).

## Implementation Checklist

### Batch 1: Tool schema optimization (highest token impact)
**Assignable to one cheaper-coder agent.**

1. **Lazy tool schemas per round** (`crates/gobrowse-server/src/run_api.rs:execute_inner`)
   - Track `sandbox_activated: bool` in the agent loop
   - Filter `tool_defs.to_vec()`: round 1 = base 3 tools only; after sandbox use = all tools
   - Gate: `sandbox_activated = true` when `sandbox_tool_kind(name).is_some()` succeeds

2. **Tool schema size reduction** (`crates/gobrowse-server/src/run_tools.rs`)
   - Strip `additionalProperties: false` from all tool definitions
   - Minimize property `description` strings to 1-3 words each
   - Remove redundant `maxLength` on properties where server-side validation already bounds
   - Measure before/after character count of `serde_json::to_string(&tool_definitions(...)).len()`

**Acceptance**: 
- `cargo test -p gobrowse-server -- run_tools` passes
- `cargo test -p gobrowse-server -- run_api::concurrency_tests` passes
- Before/after tool schema byte count recorded in batch output

### Batch 2: Container + Docker optimization
**Assignable to one cheaper-coder agent.**

1. **Remove curl + git from runtime image** (`Dockerfile`)
   - Runtime stage: `apt-get install -y --no-install-recommends ca-certificates` only
   - Remove `curl` and `git` packages

2. **Add `gobrowse health` subcommand** (`crates/gobrowse-server/src/main.rs`)
   - New subcommand: reads `GOBROWSE_PORT` env (default 8080), GETs `http://127.0.0.1:{port}/health/ready`
   - Exits 0 on 200, 1 otherwise. Timeout 5s.
   - Register in clap CLI as `health` subcommand

3. **Update docker-compose healthcheck** (`docker-compose.yml`)
   - Change from `curl --fail --silent http://127.0.0.1:8080/health/ready` to `gobrowse health`

**Acceptance**:
- `docker build -t gobrowse-os-app:m24 .` succeeds
- `docker compose up -d && docker compose ps` shows healthy app
- Image size measured: `docker images gobrowse-os-app:m24 --format '{{.Size}}'`
- `docker compose down` cleans up

### Batch 3: Sandbox client lazy connection
**Assignable to one cheaper-coder agent.**

1. **Change AppState.sandbox to lazy** (`crates/gobrowse-server/src/lib.rs`)
   - Replace `Option<SandboxClient>` with a lazy-initialized wrapper
   - Store `SandboxConfig` fields in `AppState`; connect on first use
   - `AppState::new` never fails for sandbox config (just stores config)

2. **Update execute_sandbox_op** (`crates/gobrowse-server/src/run_tools.rs`)
   - Check if client is initialized; if not, connect + cache
   - Return "sandbox unavailable" error if connection fails

**Acceptance**:
- Server starts without sandboxd running (no error)
- First sandbox tool call returns "sandbox unavailable" gracefully
- After sandboxd starts, subsequent tool calls succeed
- `cargo test -p gobrowse-server` passes

### Batch 4: Library search index + cheapest routing
**Assignable to one cheaper-coder agent.**

1. **Migration 0023** (`crates/gobrowse-server/migrations/0023_agent_retrieval_index.sql`)
   - `CREATE INDEX CONCURRENTLY books_agent_retrieval_idx ...` (partial index for agent retrieval)
   - `ALTER TABLE models ADD COLUMN IF NOT EXISTS cost_ranking integer;`
   - `UPDATE schema_metadata SET schema_version = 23`

2. **Update schema version assertions** in integration tests
   - `postgres_integration.rs:42`: `schema_version == 22` → `== 23`
   - Verify no other hardcoded `22` assertions exist

3. **Sort routes by cost_ranking** (`crates/gobrowse-server/src/chat.rs:load_routes`)
   - Change ORDER BY to `m.cost_ranking ASC NULLS LAST, route.position`

4. **select_model_for_task cost logging** (`crates/gobrowse-server/src/router.rs`)
   - When falling back to chain, log which model was selected and why

**Acceptance**:
- `cargo test -p gobrowse-server -- postgres_integration` passes with GOBROWSE_TEST_DATABASE_URL
- `cargo run -p gobrowse-server --bin gobrowse -- migrate` applies migration 0023
- Query plan shows index use: `EXPLAIN ANALYZE <the build_messages query>`

### Batch 5: Architecture map + regression matrix
**Assignable to one cheaper-coder agent.**

1. **Architecture map** (`docs/architecture-map.md`)
   - Crate dependency graph
   - Module map (gobrowse-server internal)
   - Data flow diagrams (context assembly, tool execution, model routing)
   - Deployment topology

2. **Capability regression matrix** (`docs/m24-regression-matrix.md`)
   - Table: Optimization × Capability × Before/After × Test command
   - Every tool (all ~18) listed with verification command
   - Context assembly: verify 500-book stress test passes
   - Model routing: verify task classification + model selection

**Acceptance**:
- Both docs are complete, readable, and accurate per code inspection
- Regression matrix has executable test commands for each row

### Batch 6: Final integration + benchmark table
**Assignable to one cheaper-coder agent. Runs AFTER all prior batches complete.**

1. **Run full CI suite**
   - `cargo fmt --check && cargo clippy --workspace --all-targets --all-features -- -D warnings`
   - `cargo clippy -p gobrowse-web --target wasm32-unknown-unknown -- -D warnings`
   - `cargo nextest run --workspace` with GOBROWSE_TEST_DATABASE_URL

2. **Compile benchmarks**
   - `docker build -t gobrowse-os-app:m24 .`
   - `trunk build index.html --release --dist ../../dist` (WASM size)
   - `wasm-opt -Oz` on output WASM (WASM optimized size)

3. **Populate before/after table** in `docs/m24-baselines.md`:
   | Metric | M23 baseline | M24 | Delta |
   |---|---|---|---|
   | Docker image size | 275 MB (69.9 MB compressed) | TBD | TBD |
   | WASM raw | 2,669,588 bytes | TBD | TBD |
   | WASM gzip | 798,996 bytes | TBD | TBD |
   | Tool schema bytes (per round) | TBD | TBD | TBD |
   | Container packages | ca-certificates, curl, git | ca-certificates | -2 |
   | Sandbox startup dependency | blocking | non-blocking | ✓ |
   | Context candidates limit | 12+10+20 (42 max) | same | - |
   | Tool defs sent round 1 | ~18 | 3 | -15 |
   | Stress test (500 books + 100 plugins) | TBD | TBD | TBD |

4. **Smoke test**
   - `docker compose up -d && sleep 15 && docker compose ps` (all healthy)
   - `curl http://127.0.0.1:8080/health/ready` → 200
   - `curl http://127.0.0.1:8080/api/v1/version` → schema_version 23
   - `docker compose down`

**Acceptance**: All metrics recorded, all tests green, docker compose smoke passes.

## Validation Commands (exact)

```bash
# ---- Pre-flight ----
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo clippy -p gobrowse-web --target wasm32-unknown-unknown -- -D warnings

# ---- Unit tests ----
cargo nextest run --workspace

# ---- Integration tests (requires postgres) ----
GOBROWSE_TEST_DATABASE_URL=postgres://gobrowse:test-only-password@localhost:5432/gobrowse_test \
  cargo nextest run -p gobrowse-server --test postgres_integration

GOBROWSE_TEST_DATABASE_URL=postgres://gobrowse:test-only-password@localhost:5432/gobrowse_test \
  cargo nextest run -p gobrowse-server --test m24_stress_integration

# ---- Migrations ----
GOBROWSE__DATABASE__URL=postgres://gobrowse:test-only-password@localhost:5432/gobrowse_test \
  cargo run -p gobrowse-server --bin gobrowse -- migrate

# ---- WASM build + size check ----
cd crates/gobrowse-web && trunk build index.html --release --dist ../../dist
ls -l ../../dist/*.wasm
wasm-opt -Oz -o ../../dist/optimized.wasm ../../dist/*.wasm
ls -l ../../dist/optimized.wasm
gzip -c ../../dist/optimized.wasm | wc -c

# ---- Docker build + smoke ----
docker build -t gobrowse-os-app:m24 .
docker images gobrowse-os-app:m24 --format '{{.Size}}'
cp .env.example .env  # if not exists
docker compose up -d
sleep 15
docker compose ps
curl --fail http://127.0.0.1:8080/health/ready
curl http://127.0.0.1:8080/api/v1/version
docker compose down

# ---- Tool schema measurement (run from repo root) ----
cargo test -p gobrowse-server -- run_tools::tests --nocapture  # if test added for schema size
```

## Open Questions / Risks

1. **Schema version 22 → 23**: Batch 4 bumps schema version. The RunningBaselineSlice is creating
   `m24_stress_integration.rs` — if it hardcodes schema_version=22, Batch 4 must also update it.
   **Coordinate with RuntimeBaselineSlice-3 before bumping.**

2. **Git removal from Docker**: Plugin `source_type = 'generic_git'` uses git. Current
   `plugin_github.rs` uses GitHub Release API (tarball download — no git needed). Verify no
   other code paths shell out to git before removing.

3. **WASM wasm-opt -Oz**: The Dockerfile currently has `wasm-opt -Oz ... || true`. WasmSizeSlice-3
   is removing the `|| true`. Batch 6 must use the updated Dockerfile.

4. **Sandbox lazy connection**: Changing `AppState.sandbox` type may affect downstream code.
   Full audit of all `.sandbox` field accesses before implementing.

5. **cost_ranking column**: NULL by default. Models without explicit cost_ranking sort last.
   This preserves backward compatibility but doesn't enforce "cheapest first" without admin
   configuration. Acceptable for M24 — the infrastructure is in place for operators to tune.

6. **Before/after benchmark tool**: The batch context mentions needing a benchmark table. The
   RuntimeBaselineSlice-3 is creating `docs/m24-baselines.md` with M23 baselines. Batch 6
   fills in M24 values. Ensure the template is compatible.