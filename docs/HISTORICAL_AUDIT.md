# Historical Audit — Gobrowse OS

## Consolidated Historical Audit (2026-08-21) — Agent Audit

> **Scope:** Post-M24 repo at branch `initial-agent-os`, HEAD `0168513`.
> **Lanes:** Migration audit, Security audit, Architecture/dead-code audit, Tests audit.
> **Policy:** READ-ONLY. No source, test, migration, or dependency modifications.
> Every finding grounded in direct code inspection (file:line). PASS requires evidence beyond docs.

---

### Lane 1: Migration Audit

**Summary:** 23 migrations (0001–0023) form a clean, linear chain. Schema version 23 is asserted in `postgres_integration.rs:42`. Migration ordering is consistent; no gaps or duplicates. All migrations are non-destructive with proper CHECK constraints and indexes.

| Finding | Status | Severity | File:Line |
|---------|--------|----------|-----------|
| Migration chain is complete and ordered (0001→0023) | PASS | — | `migrations/` directory listing |
| Schema version assertion updated to 23 | PASS | — | `tests/postgres_integration.rs:42` |
| Migration 0023 adds `cost_ranking` column with safe default | PASS | — | `migrations/0023_agent_retrieval_index.sql:22` |
| Migration 0023 partial GIN index mirrors `build_messages` filters | PASS | — | `migrations/0023_agent_retrieval_index.sql:4-17` |
| Migration 0022 `model_task_routes` table with CHECK constraint | PASS | — | `migrations/0022_router.sql:1-24` |
| `db::migrate` uses advisory lock for safe concurrent startup | PASS | — | `src/db.rs:12-29` |
| `db::migrate` resolves migrations via `GOBROWSE_MIGRATIONS_DIR` or `CARGO_MANIFEST_DIR` | PASS | — | `src/db.rs:15-18` |

**No migration regressions found.**

---

### Lane 2: Security Audit

| Finding | Status | Severity | File:Line |
|---------|--------|----------|-----------|
| CSRF: `origin_guard` checks Origin + Sec-Fetch-Site for unsafe methods | PASS | — | `src/lib.rs:427-462` |
| CSRF: webhook deliveries correctly bypass origin check (HMAC auth) | PASS | — | `src/lib.rs:432-434` |
| Session: `__Host-` cookie prefix in production, `Secure` flag respected | PASS | — | `src/auth.rs:23-24, 430-440` |
| Session: Argon2id with configurable memory/iterations/parallelism | PASS | — | `src/auth.rs:36-81` |
| Session: login throttle (5 attempts / 5 min window) | PASS | — | `src/auth.rs:131-159` |
| Session: `auth_epoch` rotation invalidates all sessions atomically | PASS | — | `src/auth.rs:334-370` |
| Vault: AES-256-GCM envelope encryption with key versioning | PASS | — | `src/vault.rs:134-200` |
| Vault: key file permission validation (rejects world-readable keys) | PASS | — | `src/vault.rs:230-240` |
| Vault: MCP OAuth secrets require exactly one canonical authority host | PASS | — | `src/vault.rs:28-44` |
| SSRF: `validate_resolved_target` rejects private/link-local/metadata IPs | PASS | — | `src/outbound_http.rs:192-233` |
| SSRF: DNS bounded to 16 answers, 2s resolution timeout | PASS | — | `src/outbound_http.rs:10-12` |
| SSRF: redirect policy set to `none` (no follow) | PASS | — | `src/outbound_http.rs:290` |
| Webhook HMAC: constant-time comparison (`ct_eq`) | PASS | — | `src/webhooks.rs:89-91` |
| Webhook: 300s clock skew protection + idempotent delivery insert | PASS | — | `src/webhooks.rs:134-160` |
| Plugin: TOCTOU protection — digest re-verified at install time | PASS | — | `src/plugin_api.rs:394-420` |
| Plugin: zip bomb protection (512 MiB extracted, 10K entries) | PASS | — | `src/plugin_api.rs:48-49` |
| Plugin: path traversal rejection in `sanitize_archive_path` | PASS | — | `src/plugin_api.rs` |
| Plugin: permission scope validation (7 allowed domains, max 200 chars) | PASS | — | `src/plugin_api.rs` |
| MCP: schema/size validation, no remote `$ref`, per-server permissions | PASS | — | `src/mcp_client.rs:20-44` |
| MCP: OAuth token audience/resource binding | PASS | — | `src/mcp_client.rs` |
| Sandbox: lazy connection via `OnceLock` — down daemon never fails startup | PASS | — | `src/lib.rs:60-92, sandbox_client.rs:79-110` |
| Context isolation: `RESTRICTED`/`PRIVATE`/`AUTOBIOGRAPHY` never in implicit retrieval | PASS | — | `src/run_api.rs:1375-1390` |
| Context isolation: workspace-scoped books require membership check | PASS | — | `src/run_api.rs:1382-1384` |
| Run authorization: disabled users and VIEWERs blocked | PASS | — | `src/run_api.rs:666-680` |
| Network policy: workspace policy resolved server-side (never client-supplied) | PASS | — | `src/run_api.rs:692-702` |
| Error masking: database/internal errors return 500 with generic message | PASS | — | `src/error.rs:34-74` |
| Append-only audit: SQL triggers reject UPDATE/DELETE/TRUNCATE on audit tables | PASS | — | `migrations/0006_audit_append_only.sql` |
| Quarantine tables append-only with trigger protection | PASS | — | `migrations/0015_skill_lifecycle_hardening.sql:1-40` |
| Tool output truncation silently drops oversized results (64 KiB) | **INCOMPLETE** | **P1** | `src/run_api.rs:1097-1100` |
| Tool output truncation has no integration test | **INCOMPLETE** | **P2** | — |

**P1 Detail — Tool output truncation (`run_api.rs:1097-1100`):**

When a tool result exceeds 64 KiB serialized, the code executes `continue`, which skips:
1. Appending the `tool.result` run event
2. Inserting the `tool_calls` row
3. Adding `ContentPart::ToolCall` and `ContentPart::ToolResult` to the in-flight message vectors

This creates a provider-level inconsistency: the assistant message references a `tool_call` that has no matching result in the conversation history. Providers that enforce tool_call/tool_result pairing may reject the next round's request or produce undefined behavior. The tool DID execute — only the result is dropped. No test covers this code path.

---

### Lane 3: Architecture / Dead-Code Audit

| Finding | Status | Severity | File:Line` |
|---------|--------|----------|-----------|
| Crate dependency graph matches architecture-map.md | PASS | — | `Cargo.toml` workspace |
| Module map in architecture-map.md matches `lib.rs` module declarations | PASS | — | `src/lib.rs:1-32` |
| M24 AD-1: Lazy tool schemas implemented via `active_tool_defs` | PASS | — | `src/run_api.rs:1183-1200` |
| M24 AD-1: `sandbox_activated` flips at first sandbox tool call | PASS | — | `src/run_api.rs:1065` |
| M24 AD-2: `tool_definitions` strips `additionalProperties` from base tools | PASS | — | `src/run_tools.rs:39-131`, test `:1387-1402` |
| M24 AD-2: `tool_descriptors` still retains `additionalProperties: false` | **INCOMPLETE** | **P3** | `src/run_tools.rs:288,309,340,388` |
| M24 AD-2: `sandbox_tool_definitions` still retains `additionalProperties: false` | **INCOMPLETE** | **P3** | `src/run_tools.rs` (sandbox fn) |
| M24 AD-3: `gobrowse health` subcommand implemented | PASS | — | `src/main.rs:198-218` |
| M24 AD-3: Dockerfile uses only `ca-certificates` (no curl/git) | PASS | — | `Dockerfile:34-52` |
| M24 AD-4: Partial GIN index `books_agent_retrieval_idx` created | PASS | — | `migrations/0023_agent_retrieval_index.sql:4-17` |
| M24 AD-5: `SandboxHandle` uses `OnceLock` for lazy init | PASS | — | `src/lib.rs:60-92` |
| M24 AD-5: `AppState::new` never fails for sandbox config | PASS | — | `src/lib.rs:112-128` |
| M24 AD-6: `cost_ranking` column added, routes ordered by cost | PASS | — | `src/chat.rs:272`, `migrations/0023:22` |
| M24 AD-6: `select_model_for_task` picks cheapest-capable fallback | PASS | — | `src/router.rs:355-380` |
| `run_tools.rs:tool_descriptors()` maintains `additionalProperties: false` for internal descriptors | **DRIFT** | **P3** | `src/run_tools.rs:288,309,340,388` |
| `docs/architecture-map.md` references schema version 22 | **DRIFT** | **P4** | `docs/architecture-map.md` (architecture doc) |
| `docs/m24-regression-matrix.md` claims schema version 22 is "last migration" | **DRIFT** | **P4** | `docs/m24-regression-matrix.md` |
| `test_release_lease` and `test_fail_run` are public but only for tests | **DRIFT** | **P4** | `src/run_api.rs:2075-2095` |

**P3 Detail — `tool_descriptors` retains `additionalProperties: false`:**

`tool_definitions()` (the compact wire-format schemas sent to providers) correctly strips `additionalProperties: false` per AD-2. However, `tool_descriptors()` (internal server-side descriptors used for policy/risk metadata) still emits `additionalProperties: false` at `run_tools.rs:288,309,340,388`. The regression matrix at row #2 claims the reduction applies to "ALL tool definitions" — this is inaccurate for `tool_descriptors`. If `tool_descriptors` is ever sent to a provider, the reduction is inconsistent. This is not a runtime bug today (descriptors are server-internal), but it is a maintenance hazard and the regression matrix claim is false.

---

### Lane 4: Tests Audit

**Test inventory:** 260+ tests in `postgres_integration.rs`, 153 in `skills_integration.rs`, 89 in `milestone3_integration.rs`, 84 in `plugin_integration.rs`, 55 in `outbound_http.rs`, 48 in `run_api.rs`, 42 in `sandbox_client.rs`, 38 in `takeover_fence_integration.rs`, 33 in `webhook_scheduler.rs`, 31 in `library_load_integration.rs`, plus tests in all major modules.

| Finding | Status | Severity | File:Line |
|---------|--------|----------|-----------|
| Schema version assertion: `== 23` | PASS | — | `tests/postgres_integration.rs:42` |
| `m24_stress_integration`: 500 books + 100 plugins bounded context | PASS | — | `tests/m24_stress_integration.rs:52-58` |
| `m24_stress_integration`: retrieval ≤ 12 candidates, library < 33%, conversation < 67% | PASS | — | `tests/m24_stress_integration.rs` |
| `run_api::concurrency_tests`: lease fence prevents stale writes | PASS | — | `src/run_api.rs:2100-2280` |
| `run_api::concurrency_tests`: cancellation + `finalize_expired_cancellations` | PASS | — | `src/run_api.rs:2280-2300` |
| `run_api::concurrency_tests`: implicit context excludes restricted/private books | PASS | — | `src/run_api.rs:2304-2490` |
| `router::tests`: `classify_task` determinism (5 task classes) | PASS | — | `src/router.rs:389-417` |
| `router::tests`: `select_model_for_task` cheapest-capable fallback | PASS | — | `src/router.rs:458-490` |
| `router::tests`: primary-first when costs equal | PASS | — | `src/router.rs:480-490` |
| `sandbox_client::tests`: `connect_to_missing_socket_returns_disconnected` | PASS | — | `src/sandbox_client.rs:1120-1129` |
| `sandbox_client::tests`: timeout on unresponsive daemon | PASS | — | `src/sandbox_client.rs:1132-1139` |
| `sandbox_client::tests`: rejects empty socket path and zero timeout | PASS | — | `src/sandbox_client.rs:1142-1160` |
| `outbound_http::tests`: SSRF validation comprehensive | PASS | — | `src/outbound_http.rs:305+` |
| `webhooks::tests`: HMAC verification | PASS | — | `src/webhooks.rs:214+` |
| `error::tests`: every variant maps to correct status/code | PASS | — | `src/error.rs:77-87` |
| `error::tests`: database error masked as 500 | PASS | — | `src/error.rs` |
| `error::tests`: internal error masked as 500 | PASS | — | `src/error.rs` |
| `security_guard_integration`: CSRF + WebSocket origin validation | PASS | — | `tests/security_guard_integration.rs` |
| `session_rotation_integration`: session invalidation after rotation | PASS | — | `tests/session_rotation_integration.rs` |
| `takeover_fence_integration`: run lease takeover semantics | PASS | — | `tests/takeover_fence_integration.rs` |
| `plugin_integration`: preview→install→activate pipeline | **ENV-FAIL** | **P3** | `tests/plugin_integration.rs` |
| `run_tools::tests`: library search/add/load schema assertions | PASS | — | `src/run_tools.rs:1360-1410` |
| `run_tools::tests`: sandbox tools conditional + high-risk | PASS | — | `src/run_tools.rs:1340-1355` |
| `run_api::concurrency_tests`: token metrics serialization | PASS | — | `src/run_api.rs:2100-2120` |
| `run_api::concurrency_tests`: unicode chunk boundaries | PASS | — | `src/run_api.rs:2120-2130` |
| Tool output truncation (64 KiB) path — **no test** | **INCOMPLETE** | **P2** | `src/run_api.rs:1097-1100` |
| `plugin_integration` fails locally with 503 (GitHub mock issue) | **ENV-FAIL** | **P3** | `tests/plugin_integration.rs` |
| `conversation_api` handlers — no unit tests | **INCOMPLETE** | **P3** | `src/conversation_api.rs` |
| `library_api` handlers — no unit tests | **INCOMPLETE** | **P3** | `src/library_api.rs` |
| `usage_api` handlers — no unit tests | **INCOMPLETE** | **P3** | `src/usage_api.rs` |
| `vault_api` handlers — no unit tests | **INCOMPLETE** | **P3** | `src/vault_api.rs` |
| `model_api` handlers — no unit tests | **INCOMPLETE** | **P3** | `src/model_api.rs` |
| `embedding_api` handlers — no unit tests | **INCOMPLETE** | **P3** | `src/embedding_api.rs` |
| `mcp_api` handlers — no unit tests | **INCOMPLETE** | **P3** | `src/mcp_api.rs` |
| `worktree_api` handlers — no unit tests | **INCOMPLETE** | **P3** | `src/worktree_api.rs` |
| `autobiography_api` handlers — no unit tests | **INCOMPLETE** | **P3** | `src/autobiography_api.rs` |
| `task_api` handlers — no unit tests (integration only) | **INCOMPLETE** | **P4** | `src/task_api.rs` |

---

### Milestone Regression Matrix

| # | M24 Optimization | Acceptance Claim | Current Evidence | Status | Issue |
|---|-----------------|------------------|-----------------|--------|-------|
| 1 | Lazy tool schemas | Round 0 sends 3 base tools only | `active_tool_defs` at `run_api.rs:1183-1200`; `sandbox_activated` flips at `:1065` | **PASS** | — |
| 2 | Tool schema size reduction | `additionalProperties` stripped, descriptions minimal | `tool_definitions` at `run_tools.rs:39-131` strips `additionalProperties`; tests `:1387-1402` verify. But `tool_descriptors` at `:288,309,340,388` retains it. | **INCOMPLETE** | P3: `tool_descriptors` retains `additionalProperties: false`; regression matrix claim is inaccurate |
| 3 | Container minimization | No curl/git in runtime image | `Dockerfile:34-52` installs only `ca-certificates`; healthcheck uses `gobrowse health` | **PASS** | — |
| 4 | Sandbox lazy connection | Server starts without sandboxd | `SandboxHandle` at `lib.rs:60-92` uses `OnceLock`; `AppState::new` at `:112-128` never fails for sandbox | **PASS** | — |
| 5 | Library search partial index | Retrieval bounded at LIMIT 12 | `migrations/0023_agent_retrieval_index.sql:4-17`; stress test guards at `m24_stress_integration.rs:58` | **PASS** | — |
| 6 | `cost_ranking` routing | Cheapest-capable fallback | `chat.rs:272` orders by cost; `router.rs:355-380` selects cheapest; tests at `:458-490` | **PASS** | — |
| 7 | WASM `wasm-opt -Oz` | Optimized frontend bundle | `Dockerfile:32` runs `wasm-opt`; CI `web` job in `ci.yml:78-99` | **PASS** | — |
| 8 | Parallel context assembly | Independent queries with `tokio::try_join!` | `run_api.rs:1291` (Phase 1), `:1368` (Phase 2); stress test guards | **PASS** | — |

---

### Top Findings (P0–P1)

**P1 — Tool output truncation creates provider-level inconsistency:**
`run_api.rs:1097-1100` — When a tool result exceeds 64 KiB, `continue` skips the event append, `tool_calls` row insert, and message history update. The assistant message references a `tool_call` with no matching result in the conversation. Providers enforcing tool_call/tool_result pairing may reject subsequent requests. No test covers this path. **Impact:** Correctness; agent loop integrity for tools producing large output (e.g., `sandbox_exec` with verbose output, `library_load` with large bodies).

**P2 — Tool output truncation has no integration test:**
The 64 KiB truncation path at `run_api.rs:1097-1100` is exercised by no test. A test should verify that (a) oversized tool results are handled gracefully, (b) the agent loop continues or terminates cleanly, and (c) the conversation state is consistent.

---

### P3 Findings

| # | Finding | File:Line |
|---|---------|-----------|
| 1 | `tool_descriptors()` retains `additionalProperties: false` despite AD-2 intent | `src/run_tools.rs:288,309,340,388` |
| 2 | `sandbox_tool_definitions()` retains `additionalProperties: false` | `src/run_tools.rs` (sandbox fn) |
| 3 | `plugin_integration` tests fail locally (GitHub mock connect error) — pre-existing | `tests/plugin_integration.rs` |
| 4 | `conversation_api` handlers have no unit tests | `src/conversation_api.rs` |
| 5 | `library_api` handlers have no unit tests | `src/library_api.rs` |
| 6 | `usage_api` handlers have no unit tests | `src/usage_api.rs` |
| 7 | `vault_api` handlers have no unit tests | `src/vault_api.rs` |
| 8 | `model_api` handlers have no unit tests | `src/model_api.rs` |
| 9 | `embedding_api` handlers have no unit tests | `src/embedding_api.rs` |
| 10 | `mcp_api` handlers have no unit tests | `src/mcp_api.rs` |
| 11 | `worktree_api` handlers have no unit tests | `src/worktree_api.rs` |
| 12 | `autobiography_api` handlers have no unit tests | `src/autobiography_api.rs` |

---

### P4 Findings

| # | Finding | File:Line |
|---|---------|-----------|
| 1 | `architecture-map.md` references schema version 22 (stale; actual is 23) | `docs/architecture-map.md` |
| 2 | `m24-regression-matrix.md` claims "last migration `0022_router.sql`" (stale) | `docs/m24-regression-matrix.md` |
| 3 | `test_release_lease` and `test_fail_run` are `pub` but test-only | `src/run_api.rs:2075-2095` |
| 4 | `task_api` handlers have integration tests but no unit tests | `src/task_api.rs` |

---

### Summary

**Total findings:** 37
- **P0:** 0
- **P1:** 1 (tool output truncation correctness)
- **P2:** 1 (missing truncation test)
- **P3:** 12 (schema drift, ENV-FAIL, handler test gaps)
- **P4:** 4 (doc drift, test-only pub functions)

**Migration lane:** Clean. No regressions. Schema version 23 correctly asserted.

**Security lane:** Solid. CSRF, session, vault, SSRF, webhook HMAC, plugin sandboxing, MCP security, context isolation all verified against code.

**Architecture lane:** M24 optimizations (AD-1 through AD-8) implemented and verified. One drift: `tool_descriptors` and `sandbox_tool_definitions` still carry `additionalProperties: false`, inconsistent with AD-2's stated intent for the wire-format schemas.

**Tests lane:** Core paths well-tested (260+ postgres integration, 48 run_api, 42 sandbox_client, 16 router). Significant coverage gaps in HTTP handler modules (conversation_api, library_api, usage_api, vault_api, model_api, embedding_api, mcp_api, worktree_api, autobiography_api). Tool output truncation path completely untested.

**Recommended next steps:**
1. **P1:** Fix `run_api.rs:1097-1100` to either (a) append a truncated result message instead of skipping, or (b) emit a `tool.result` event with `truncated: true` flag and include a truncated placeholder in the message history.
2. **P2:** Add integration test for oversized tool output handling.
3. **P3:** Either apply AD-2 reduction to `tool_descriptors`/`sandbox_tool_definitions` or update regression matrix row #2 to reflect the actual scope of the reduction.
4. **P4:** Update `architecture-map.md` and `m24-regression-matrix.md` to reference schema version 23 and migration 0023.
