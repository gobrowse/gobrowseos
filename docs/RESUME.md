# RESUME.md — Gobrowse OS (compact)

WHAT GOBROWSE IS: Self-hosted agent OS. Workspace = 4 crates (core, server `gobrowse`, web Leptos/WASM, sandboxd). Rootless-podman sandbox + retrieval-driven Library + plugin runtime + Custom UI System.

ACTIVE MILESTONE: M24b — Custom UI System (implementation + verification complete; committed HEAD 7ce7706; NOT yet deployed to prod — M24 remains the deployed baseline).

CURRENT ARCHITECTURE: books table = Library registry (kind: SOURCE/SKILL/MCP/PLUGIN/AUTOBIOGRAPHY/GOBROWSE_UI). Server → sandboxd Unix socket (token+peer-uid) → rootless podman. M23: TaskClass classification (rule-first + model fallback), capability ranking with task-aware signals, model_task_routes, book_usage_stats via trigger, per-category context budget, UI Context Inspector. M24 (PERMANENT baseline): lazy tool schemas, slim schemas 6.7K→4.5K, container = bookworm-slim + ca-certificates + `gobrowse health`, partial GIN index, lazy sandbox connection, cheapest-capable routing, bounded context, WASM -Oz baseline. M24b: ui package lifecycle (preview→install digest+approve→activate→rollback→delete), profile ui_permission_level (DENY/ASK/ALLOW_WORKSPACE/ALLOW_GLOBAL), CSP builder (no unsafe-inline), standalone recovery crate, capabilities_api, one-active-per-profile.

WHAT JUST CHANGED: M24b complete. HEAD 7ce7706 (29 files, +4443). Schema 24 (migration 0024_ui_packages.sql: ui_packages, ui_package_assets, ui_package_capabilities, profiles.ui_permission_level, books GOBROWSE_UI kind). New: docs/UI-SDK.md, schema/ui-package-manifest.json, examples/starter-ui, tools/ui-package. Web: UiPackages page + provider UX polish. Tests: ui_packages_integration 7/7.

WHAT WORKS (verified on local head): full gate suite — fmt clean; clippy workspace + wasm web + wasm recovery -D warnings clean; nextest 522/528 (6 pre-existing plugin_integration GitHub-mock 503 env failures); ui_packages_integration 7/7 (preview valid/invalid, digest 409, approve-gate 422, full lifecycle + CSP no-unsafe-inline + recovery no-auth, delete guards, schema 24).
PROD VERIFIED (2026-08-21, ssh root@178.128.179.216): M24 deployed; /health/ready 200; /api/v1/version schema 23; healthcheck healthy; UI assets 200; disk 9.0G free.
Rollback: DB dump backups/gobrowse-m24-pre-20260821-130148.dump (BACKUP_VERIFY_OK), image gobrowse-os-app:m21-final-rollback (cc8ef3648130), compose docker-compose.yml.pre-m24.

WHAT DOES NOT YET EXIST: M24b production deployment (migration 23→24 on prod, image rebuild, rollback preserved). Post-M24b backlog (roadmap M25+). Alt-checker omnibus audit report.

EXACT NEXT STEP: Option A — deploy M24b to prod per docs/deployment.md (backup DB + rollback image first, migrate 23→24, rebuild image, verify /health/version schema 24 + UI packages page + recovery). Option B — start post-M24b backlog. Option C — run alt-checker omnibus audit (everything ever done).

IMPORTANT INVARIANTS: no unsafe; clippy -D warnings; schema version 24 (new migrations only); Leptos: no reactive block reading static signals in list closures; server release binaries in rust:1.94-bookworm (glibc 2.36); tar deploys use `-C`; sandbox volumes owned container_uid:daemon_gid 2775 setgid; podman env = real daemon-user HOME + XDG_RUNTIME_DIR; canonical plugin versions + GitHub v-prefix fallback; CSP never unsafe-inline; activate sweeps stale 'previous' anchors BEFORE demoting current active (rollback anchor preservation).

RESUME_FROM: docs/CURRENT_CHECKPOINT.md (full detail). CI: gh run list. Prod: ssh root@178.128.179.216. Test app: localhost:8082 (m22test@example.com / TestPassword123!).