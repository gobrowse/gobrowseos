# M24 Regression Matrix — Gobrowse OS

> **Baseline:** M24 (`8c54539`). M24 is the permanent performance/cost baseline.
> This matrix lists every M24 optimization, the capability it must **not**
> degrade, the before/after behavior, and the command that verifies it. It is
> grounded in code inspection (file:line) and the existing test suite.
>
> Companion doc: `docs/architecture-map.md`.

## 1. Optimization → capability regression table

| # | M24 optimization | Capability it must NOT degrade | Before (M23) | After (M24) | Verification command |
|---|------------------|-------------------------------|--------------|-------------|----------------------|
| 1 | **Lazy tool schemas** | Provider request payload size & first-token latency; tool-call correctness | All tool schemas (3 library + ~15 sandbox/terminal) sent every run from round 0 | Round 0 and every pre-sandbox round send only the 3 base library tools; full suite advertised only after the first sandbox tool call (`active_tool_defs`, `run_api.rs:1183-1200`; flip at `run_api.rs:1065`) | `cargo test -p gobrowse-server --test m24_stress_integration` (run completes with base-only advertisement pre-sandbox). No standalone unit test for `active_tool_defs`; behavior is covered by the run path + stress guard. |
| 2 | **Tool schema size reduction** | Per-tool-call provider token cost & payload bytes | Verbose tool JSON schemas | Compact schemas: bounded `maxLength`, `additionalProperties: false`, minimal required props, no extraneous fields (`run_tools.rs:39-320`) | `cargo test -p gobrowse-server --lib run_tools` (schema helpers) + smoke: assert `serde_json::to_string(&tool_definitions(false)).len()` stays under budget. |
| 3 | **Container minimization** | Container healthcheck + runtime start; image attack surface | Image relied on `curl` (healthcheck) and source tooling | `debian:bookworm-slim` with only `ca-certificates`; healthcheck uses the self-contained `gobrowse health` probe instead of `curl` (`Dockerfile:34-52`; `docker-compose.yml:29-34`) | `docker compose build && docker compose up -d && docker compose ps` (healthcheck passes) and `docker run --rm gobrowse-os:ci gobrowse health`. |
| 4 | **Sandbox lazy connection** | Server startup resilience / "down daemon never fails startup" | Startup required a live `gobrowse-sandboxd` | `SandboxClient::connect` stores config and returns `Ok` **without dialing**; failures surface lazily at call time (`sandbox_client.rs:78-94`; `AppState::new` `lib.rs:94-114`) | `cargo test -p gobrowse-server --lib sandbox_client::tests::connect_to_missing_socket_returns_disconnected` |
| 5 | **Library search partial index** (migration 0023) | Implicit retrieval latency & correctness at scale | Full-table scan of `books` for agent retrieval | Partial GIN index `books_agent_retrieval_idx` mirroring the `build_messages` filters (`migrations/0023_agent_retrieval_index.sql:11-17`) | `cargo test -p gobrowse-server --test m24_stress_integration` (asserts retrieval capped at `LIMIT 12`) + `EXPLAIN` smoke on the `build_messages` query against a seeded DB. |
| 6 | **`cost_ranking` routing** | Model-selection cost efficiency & determinism | Selection by `priority` only | `load_routes` orders by `(position=0) DESC, cost_ranking ASC NULLS LAST, position` (`chat.rs:272`); `select_model_for_task` picks cheapest-capable fallback (`router.rs:336-381`); `cost_ranking` column added in migration 0023 (`migrations/0023_agent_retrieval_index.sql:22`) | `cargo test -p gobrowse-server --lib router::tests::test_select_model_for_task_prefers_cheapest_capable` and `...::test_select_model_for_task_keeps_primary_when_costs_equal` |
| 7 | **WASM `wasm-opt` (`-Oz --enable-bulk-memory`)** | Frontend bundle size & load time | Unoptimized `.wasm` from `trunk build` | `wasm-opt -Oz --enable-bulk-memory` post-processes `dist/*.wasm` (`Dockerfile:32`; CI `web` job `ci.yml:78-99`) | CI `web` job; local smoke: `wasm-opt -Oz --enable-bulk-memory -o out.wasm dist/*.wasm && wc -c out.wasm` (must be smaller than the unoptimized input). |
| 8 | **Parallel context assembly** (M23 carryover) | Run latency (context-build time) | Sequential context queries | `build_messages` issues independent queries with `tokio::try_join!` (Phase 1 `run_api.rs:1291`, Phase 2 `run_api.rs:1368`) | `cargo test -p gobrowse-server --test m24_stress_integration` (guards context budget split + run completion). This M23 feature's regression is guarded by the M24 stress test. |

## 2. Tool inventory — 18 tool ids and their verification

All ids are defined in `run_tools.rs` (`tool_definitions` `:39-131` and
`sandbox_tool_definitions` `:135-321`). Base set = 3 library tools; sandbox
suite = 15 sandbox/terminal/process tools. Dispatch is in `execute_inner`
(`run_api.rs:1001-1089`); sandbox names resolve via `sandbox_tool_kind`
(`run_tools.rs:573-592`) to a `SandboxToolKind` (`run_tools.rs:597-613`).

### Base library tools

| Tool id | Definition | Verification command | Notes |
|---------|-----------|----------------------|-------|
| `library_search` | `run_tools.rs:42` | `cargo test -p gobrowse-server --lib run_tools` (covers `lexical_search`) | Also reachable via `GET /api/v1/library/search`. |
| `library_add` | `run_tools.rs:71` | `cargo test -p gobrowse-server --lib run_tools` (covers `create_library_note`) | Smoke: `POST /api/v1/library/books`. |
| `library_load` | `run_tools.rs:102` | `cargo test -p gobrowse-server --test library_load_integration` | Smoke: `POST /api/v1/library/books/{id}/load`. |

### Sandbox / terminal / process tools (require `gobrowse-sandboxd`)

These are exercised end-to-end through `sandbox_integration.rs` with a live
daemon (`GOBROWSE_SANDBOX_SOCKET_PATH` + `GOBROWSE_SANDBOX_AUTH_TOKEN`). There is
**no standalone unit test per tool id**; verification is the integration test
plus a direct API smoke.

| Tool id | Definition | Verification command | Smoke |
|---------|-----------|----------------------|-------|
| `sandbox_exec` | `run_tools.rs:152` | `cargo test -p gobrowse-server --test sandbox_integration` | `POST /api/v1/sandbox/exec` |
| `sandbox_read_file` | `run_tools.rs:182` | `cargo test -p gobrowse-server --test sandbox_integration` | `POST /api/v1/sandbox/files/read` |
| `sandbox_write_file` | `run_tools.rs:189` | `cargo test -p gobrowse-server --test sandbox_integration` | `POST /api/v1/sandbox/files/write` |
| `sandbox_list_files` | `run_tools.rs:204` | `cargo test -p gobrowse-server --test sandbox_integration` | `POST /api/v1/sandbox/files/list` |
| `sandbox_stat` | `run_tools.rs:209` | `cargo test -p gobrowse-server --test sandbox_integration` | `POST /api/v1/sandbox/files/stat` |
| `sandbox_mkdir` | `run_tools.rs:216` | `cargo test -p gobrowse-server --test sandbox_integration` | `POST /api/v1/sandbox/files/mkdir` |
| `sandbox_remove` | `run_tools.rs:221` | `cargo test -p gobrowse-server --test sandbox_integration` | `POST /api/v1/sandbox/files/remove` |
| `terminal_start` | `run_tools.rs:226` | `cargo test -p gobrowse-server --test sandbox_integration` | `POST /api/v1/sandbox/terminal/start` |
| `terminal_input` | `run_tools.rs:249` | `cargo test -p gobrowse-server --test sandbox_integration` | `POST /api/v1/sandbox/terminal/{id}/write` |
| `terminal_read_output` | `run_tools.rs:262` | `cargo test -p gobrowse-server --test sandbox_integration` | `POST /api/v1/sandbox/terminal/{id}/read` |
| `terminal_resize` | `run_tools.rs:279` | `cargo test -p gobrowse-server --test sandbox_integration` | `POST /api/v1/sandbox/terminal/{id}/resize` |
| `terminal_interrupt` | `run_tools.rs:293` | `cargo test -p gobrowse-server --test sandbox_integration` | `POST /api/v1/sandbox/terminal/{id}/interrupt` |
| `terminal_close` | `run_tools.rs:298` | `cargo test -p gobrowse-server --test sandbox_integration` | `POST /api/v1/sandbox/terminal/{id}/close` |
| `process_list` | `run_tools.rs:303` | `cargo test -p gobrowse-server --test sandbox_integration` | `POST /api/v1/sandbox/processes` |
| `process_kill` | `run_tools.rs:308` | `cargo test -p gobrowse-server --test sandbox_integration` | `POST /api/v1/sandbox/processes/{pid}/kill` |

**Total: 18 tool ids** (3 base + 15 sandbox/terminal/process). The lazy
advertisement (optimization #1) means a run that never calls a sandbox tool
ships only the 3 base schemas.

## 3. Context-assembly guard — `m24_stress_integration`

`crates/gobrowse-server/tests/m24_stress_integration.rs` is the permanent
"context/RAM must not scale with installed count" guard. It seeds **500 books**
(`SEED_BOOKS = 500`, `:52`) and **100 plugins** (`SEED_PLUGINS = 100`, `:54`)
into one profile, runs a single chat turn against a deterministic fake Ollama
chat+embedding server, and asserts:

- implicit library retrieval selects **at most `LIMIT 12`** candidates
  (`LIBRARY_RETRIEVAL_LIMIT = 12`, `:58`) regardless of 500 installed books;
- the library (retrieval) portion stays **under 33%** of the content budget;
- the recent-messages portion stays **under 67%**;
- the run completes with the deterministic assistant answer.

Command: `cargo test -p gobrowse-server --test m24_stress_integration`
(guarded by `GOBROWSE_TEST_DATABASE_URL`; skipped if unset — `:70-73`).

## 4. Model-routing guard — `classify_task` + `select_model_for_task`

Unit tests in `crates/gobrowse-server/src/router.rs`:

- `classify_task` determinism:
  `test_classify_task_coding` (`:389`), `test_classify_task_research` (`:396`),
  `test_classify_task_general_qa` (`:403`),
  `test_classify_task_shell_automation` (`:410`),
  `test_classify_task_ecommerce` (`:417`), `test_generate_routing_reason` (`:424`).
- `select_model_for_task` routing + `cost_ranking` fallback:
  `test_select_model_for_task_prefers_cheapest_capable` (`:458`),
  `test_select_model_for_task_keeps_primary_when_costs_equal` (`:480`).

Command:
`cargo test -p gobrowse-server --lib router::tests`

## 5. Canonical validation command list

Run in this order (mirrors `.github/workflows/ci.yml`):

1. **Format** — `cargo fmt --check` (`ci.yml:40`)
2. **Clippy (workspace)** — `cargo clippy --workspace --all-targets --all-features -- -D warnings` (`ci.yml:42`)
3. **Clippy (WASM)** — `cargo clippy -p gobrowse-web --target wasm32-unknown-unknown -- -D warnings` (`ci.yml:44`)
4. **Unit + integration tests (nextest)** —
   `cargo nextest run --workspace` (`ci.yml:46`); integration tests require
   `GOBROWSE_TEST_DATABASE_URL` (set in CI `ci.yml:30`):
   `GOBROWSE_TEST_DATABASE_URL=postgres://gobrowse:test-only-password@localhost:5432/gobrowse_test cargo nextest run -p gobrowse-server --test m24_stress_integration`
5. **Migrations** —
   `cargo run -p gobrowse-server --bin gobrowse -- migrate`
   (with `GOBROWSE__DATABASE__URL` set; `ci.yml:47-50`)
6. **Docker compose smoke** —
   `cp .env.example .env && docker compose config --quiet && docker compose build && docker compose up -d`
   then confirm the `app` healthcheck (`gobrowse health`) reaches `healthy`
   (`ci.yml:111-124`; `docker-compose.yml:29-34`).
