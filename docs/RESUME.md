# RESUME.md — Gobrowse OS (compact)

WHAT GOBROWSE IS: Self-hosted agent OS. Workspace = 4 crates (core, server `gobrowse`, web Leptos/WASM, sandboxd). Rootless-podman sandbox + retrieval-driven Library + plugin runtime.

ACTIVE MILESTONE: M23 — Adaptive Capability Router (implementation complete, deployed to m22test). Next: M24 — Full Token, Runtime, Container & Architecture Optimization.

CURRENT ARCHITECTURE: books table = Library registry (kind: SOURCE/SKILL/MCP/PLUGIN/AUTOBIOGRAPHY). Server → sandboxd Unix socket (token+peer-uid) → rootless podman. M23 adds: TaskClass classification (rule-first + model fallback), capability ranking with task-aware signals, model_task_routes for task-specific model overrides, book_usage_stats via trigger, per-category context budget, UI Context Inspector panel (Budget/Why Loaded/Model tabs).

WHAT JUST CHANGED: M23 implementation complete. Docker image gobrowse-os-app:m23 deployed to m22test (8082). Schema 22 (model_task_routes + book_usage_stats). task_class "Coding" verified for coding request. Code not yet committed.

WHAT WORKS (verified on prod m22test @ localhost:8082): login; sandbox terminal; source-book create+search; plugin install/upgrade/rollback; Library grid; marketplace search; schema 22; doctor sandbox PASS; M23 task classification (Coding for coding requests); context endpoint with budget breakdown.

WHAT DOES NOT YET EXIST: M24 implementation (defined in roadmap only). M24b (after M24). Full routing verification with capability ranking and model selection (needs model_task_routes configured).

EXACT NEXT STEP: Commit M23, push. Then M24 — optimize architecture: measure baselines (token/context, image sizes, idle RSS, startup, WASM size, search/chat latency, build time), optimize architecture (small core + unified library + lazy content + lazy tool schemas + dormant plugins + on-demand MCP/sandbox + cheapest routing + bounded context + minimal containers), verify same-or-better capability.

IMPORTANT INVARIANTS: no unsafe; clippy -D warnings; schema version 22; Leptos: no reactive block reading static signals in list closures; server release binaries in rust:1.94-bookworm (glibc 2.36); tar deploys use `-C`; sandbox volumes owned container_uid:daemon_gid 2775 setgid; podman env = real daemon-user HOME + XDG_RUNTIME_DIR; canonical plugin versions + GitHub v-prefix fallback.

RESUME_FROM: docs/CURRENT_CHECKPOINT.md (full detail). CI: gh run list. Prod: ssh root@178.128.179.216. Test app: localhost:8082 (m22test@example.com / TestPassword123!).
