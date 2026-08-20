# CURRENT CHECKPOINT — M23 IN PROGRESS (resume point)

CURRENT_MILESTONE: M23 — Adaptive Capability Router (implementation complete, verification in progress). Next: M24 — Full Token, Runtime, Container & Architecture Optimization.

STATUS
- Complete: M23 core types (TaskClass enum, extended RankingWeights, RunTokenMetrics), migration 0022_router.sql (model_task_routes, book_usage_stats, trigger), router module (classify_task, rank_capabilities, select_model_for_task, generate_routing_reason), API endpoints (GET /runs/:id/context, GET/PUT/DELETE /models/task-routes), UI Context Inspector panel (Budget/Why Loaded/Model tabs), deployed to m22test with routing verified.
- Verified: task_class correctly classified as "Coding" for "Write a Rust function to parse JSON"; context endpoint returns task_class + budget; sandbox PASS; doctor PASS.
- Not started: M24 implementation (defined in roadmap only). M24b (after M24).

LAST_GOOD_COMMIT: pending (M23 code not yet committed).

SCHEMA_VERSION: 22 (migration 0022_router.sql: model_task_routes + book_usage_stats tables + trigger).

VERIFIED (exact — M23 evidence)
- Build: cargo fmt clean, clippy -D warnings clean, 418+ tests pass, trunk WASM build success, Docker image gobrowse-os-app:m23 built and deployed.
- Routing: task_class "Coding" correctly classified via rule-based classification for coding request.
- Context: GET /runs/:id/context returns task_class + budget breakdown.
- Doctor: all checks PASS on m22test.
- Sandbox: PASS on m22test.

ARCHITECTURE_DECISIONS (carry forward to M24)
- Task classification: rule-first (80%+ cases), optional model fallback for ambiguous.
- Capability ranking: extended RRF with capability_match (0.05), past_success (0.002), token_cost (-0.001), permission_risk (-0.003).
- Model routing: model_task_routes table sits between primary model and fallback chain.
- Context budget: per-category with soft reservations (conversation ≤67%, library shares remaining 33%).
- Book usage: lazy stats via trigger on token_metrics events, in-memory cache per run.
- UI Context Inspector: slide-in panel with Budget/Why Loaded/Model tabs, progressive disclosure.
- Router module in gobrowse-server (not gobrowse-core) — server-side concern accessing DB/models.

DO_NOT_REDO
- All M22 DO_NOT_REDO items (book-card Leptos quirk, version normalization, volume ownership, podman env).
- TaskClass is in gobrowse-core (shared with WASM), router is in gobrowse-server (server-side only).
- Model_task_routes: single preferred model per task class (CHECK position = 0); fallback uses model_fallback_routes.

DEPLOYMENT_STATE: m22test runs gobrowse-os-app:m23 with schema 22. Main app still on gobrowse-os-app:m22 (schema 21). Deploy main app after M23 full verification.

NEXT_TASK: Commit M23 code, update docs, push. Then continue to M24.
