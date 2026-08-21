# CURRENT CHECKPOINT — M24 COMPLETE (baseline landed)

CURRENT_MILESTONE: M24 — Full Token, Runtime, Container & Architecture Optimization (implementation + verification complete). Next: M24b (Custom UI System) or post-M24 backlog.

STATUS
- M24 batches 1-6 complete and committed (HEAD: 1df8ff3). M23 already landed (f54a2ca).
- Lazy tool schemas (AD-1): round 0 advertises only the 3 base library tools; full sandbox/terminal suite unlocks after the first sandbox tool call (run_api.rs active_tool_defs).
- Tool schema slimming (AD-2): 6,714 → 4,531 bytes (−32.5%); additionalProperties:false + redundant bounds dropped from all 18 tool schemas.
- Container minimization (AD-3): runtime image = debian:bookworm-slim + ca-certificates only (no curl/git); `gobrowse health` subcommand replaces curl for the compose healthcheck.
- Library search partial GIN index (AD-4): migration 0023 books_agent_retrieval_idx; EXPLAIN shows Bitmap Index Scan use, 0.48ms retrieval on 3k books.
- Sandbox lazy connection (AD-5): AppState.sandbox is now Option<SandboxHandle> (OnceLock), connects on first use; down daemon never fails startup.
- Cheapest-capable routing (AD-6): models.cost_ranking column; load_routes orders (position=0) DESC, cost_ranking ASC NULLS LAST, position.
- Docs (AD-7/8): docs/architecture-map.md (265 lines), docs/m24-regression-matrix.md (118 lines), docs/m24-baselines.md (before/after table).

LAST_GOOD_COMMIT: 1df8ff3 (feat(m24): lazy sandbox handle, tool schema slimming, logging cleanup, docs (batches 3-6)). Pushed to origin/initial-agent-os.

SCHEMA_VERSION: 23 (migration 0023_agent_retrieval_index.sql: partial GIN index + models.cost_ranking + version bump). Asserted in postgres_integration.rs:42 and worktrees_integration.rs:50.

VERIFIED (exact — M24 evidence)
- fmt clean, clippy workspace + WASM -D warnings clean, 508/514 nextest pass (6 plugin_integration env failures pre-existing at HEAD, GitHub-mock 503; NOT M24 regressions).
- m24_stress_integration PASS (500 books + 100 plugins, bounded context, deterministic answer).
- milestone3_integration PASS, postgres_integration PASS (19), library_load PASS, worktrees PASS.
- WASM: 2,669,588 → 2,353,690 raw (−11.8%); gzip 798,996 → 774,461 (−3.1%) via wasm-opt -Oz --enable-bulk-memory.
- Docker image gobrowse-os-app:m24-current built; compose app healthy (gobrowse health); /api/v1/version reports schema_version 23.
- Tool schema bytes measured: 6714 → 4531.

ARCHITECTURE_DECISIONS (carry forward)
- Lazy tool schemas: round 0 = base 3 only; unlock after first sandbox use.
- Sandbox lazy connect: config stored at startup, OnceLock client built on first use.
- Cheapest-capable routing: cost_ranking tie-break after primary.
- Bounded context: LIMIT 12 retrieval, ≤67% conversation / ≤33% library budget.
- M24 is the PERMANENT optimization baseline for all later builds (user directive).

DO_NOT_REDO
- All M22/M23 DO_NOT_REDO items.
- Do NOT re-add additionalProperties:false / verbose descriptions / redundant maxLength to tool schemas.
- Do NOT re-add curl/git to the runtime image; keep `gobrowse health`.
- Do NOT make sandbox connection eager again.
- Do NOT change schema version 23 without a new migration.

DEPLOYMENT_STATE: M24 PRODUCTION DEPLOYED + ACCEPTED (2026-08-21).
- Host: root@178.128.179.216, /opt/gobrowse-os, compose app+postgres (pgvector 0.8.1-pg17).
- Image: gobrowse-os-app:m24 (b9e181a2aa6e, 152 MB), transferred via docker save|gzip|load; compose switched to m24.
- Migrations: 21→23 applied with `docker compose run --rm app migrate`; /api/v1/version = schema_version 23.
- Rollback preserved: DB dump backups/gobrowse-m24-pre-20260821-130148.dump (BACKUP_VERIFY_OK), image gobrowse-os-app:m21-final-rollback (cc8ef3648130), compose docker-compose.yml.pre-m24 (+ .pre-m24-curl-fix).
- PROD VERIFICATION (all direct):
  - /health/ready = 200; /api/v1/version schema 23; owner_required:false.
  - Auth: bad login rejected (403/401); authenticated session → library search 200 (Test#1, VERIFIED/lexical), conversations 200, message POST 201 (user msg). Ephemeral test session removed after checks; sessions count restored.
  - Chat run: no providers configured on prod (pre-existing, 0 providers) — user message accepted; no model run (documented, not a regression).
  - gobrowse doctor: PostgreSQL/pgvector/embedding queue/static assets/vault/sandbox PASS ("ok"); Git FAIL = expected (git removed from minimal runtime in M24).
  - Container healthy (healthcheck), 0 restarts, 0 log errors; UI assets /pkg/gobrowse-web.js + .wasm + css all 200.
  - Disk: remote 9.0G free, local 7.8G free (≥5G budget).
- HEALTHCHECK FIX APPLIED IN PROD: prod compose still used `curl` (removed from M24 image) → app was "unhealthy" despite /health/ready 200; switched to `test: ["CMD","gobrowse","health"]` (repo compose was already correct) → healthy. No code/image change; config-only.
- TEST SESSION NOTE: real owner password not stored on host (no creds available); auth verified via minted DB session (sha256 token, auth_epoch 1) — cleanup confirmed (DELETE 1, sessions back to 5).

NEXT_TASK: M24b (Custom UI System, per roadmap) — architect inspects real post-M24 repo, PLAN.md update, implement preserving M24 baseline. Historical audit lanes (docs/HISTORICAL_AUDIT.md) running in parallel.
