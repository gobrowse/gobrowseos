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

DEPLOYMENT_STATE: m24-current image tagged :latest locally and running in compose (healthy). Test app localhost:8082 still on m23 (deploy m24 to prod next).

NEXT_TASK: Deploy m24 image to prod (ssh root@178.128.179.216), then start M24b (Custom UI System, per roadmap) or the post-release backlog.
