# CURRENT CHECKPOINT — M22 COMPLETE (resume point)

CURRENT_MILESTONE: M22 — Unified Library + Sandbox & Plugin Runtime ✅ ACCEPTED 2026-08-20. Next: M23 — Adaptive Capability Router.

STATUS
- Complete: M22 architecture (PLAN.md), schema 21 migration + triggers, core plugin/manifest/marketplace types, sandboxd↔server integration (SandboxClient, ProvisionWorkspace, /sandbox/* routes, 15 agent tools), plugin install/upgrade/rollback/marketplace API, unified Library UI + plugin stepper + terminal UI, web fixes, prod sandboxd + app deployed with sandbox PASS. All chat journeys (B/D/E/J) verified. Security audit fixes applied (F1-F3).
- Not started: M23 implementation (design in PLAN.md). M24/M24b (recorded in roadmap only).

LAST_GOOD_COMMIT: d0009df (initial-agent-os, clean, pushed).

SCHEMA_VERSION: 21 (migration 0021 INSERT triggers; verified on both prod DBs).

VERIFIED (exact — M22 acceptance evidence)
- Build: cargo fmt clean, clippy -D warnings clean, 23/23 tests pass, trunk WASM build success, Docker image gobrowse-os-app:m22 built and deployed.
- Journey B (sandbox tools via chat): terminal_start → terminal_id → terminal_read_output → "hello from sandbox\r\n" state=Exited.
- Journey D (skill book load): library_search(q="skill") → hello-gobrowse → library_load → greeting + file-help skills.
- Journey E (MCP book load): library_search(q="MCP") → fixture-greeter → library_load → mcp-greeter tool, stdio transport.
- Journey J (retrieval + token metrics): library_search executed → token_metrics event emitted (book_searches=1).
- Pre-existing: A (terminal), C (source book), F (plugin install), I (upgrade/rollback) — all PASS.
- Doctor: PostgreSQL PASS, pgvector PASS, Embedding PASS, Git PASS, Static assets PASS, Credential vault PASS, Sandbox PASS.
- Production: main app 8080 health/ready 200, m22test 8082 health/ready 200, both running gobrowse-os-app:m22.

NOT_VERIFIED (non-blocking)
- G (plugin activation end-to-end), H (embedded MCP dedup) — lower priority, not required for M22 acceptance.
- DB-gated integration tests — run in CI only.
- Full CI green for latest commits — not blocking M22 acceptance.

ARCHITECTURE_DECISIONS (carry forward to M23)
- Library = books table registry with `kind` column; Skill/MCP/Plugin Books are companion rows.
- Sandbox: server → sandboxd Unix socket (token + peer-uid auth) → rootless podman.
- Plugin lifecycle: install → dormant; activation on demand; upgrade/rollback with permission diff + re-approval.
- Progressive loading: Phase-2 snippets only; library_load bounded (≤5/run, ≤24k chars, ≤16 MCP tools); token_metrics run event.
- UI: Leptos quirk — no reactive block closures reading static signals in list closures.

DEPLOYMENT_STATE: Prod main app + m22test BOTH run image gobrowse-os-app:m22. Schema 21 verified. sandboxd active + doctor sandbox PASS.

ROLLBACK_STATE: DB backup /opt/gobrowse-os/backups/pre-m22-20260818.dump (+sha256). Previous images tagged.

DO_NOT_REDO
- Book-card Leptos rendering quirk (see ARCHITECTURE_DECISIONS).
- Version normalization (canonical keys + v-prefix fallback).
- Volume ownership fix, podman env (HOME + XDG_RUNTIME_DIR).
- Server release builds MUST be in bookworm (glibc 2.36).

NEXT_TASK: M23 — Adaptive Capability Router. Design already in PLAN.md (M23 section). Implement routing pipeline: classify task → search Library → rank capabilities → select minimum required Books → select model → activate plugin/MCP → execute. Context budget breakdown UI, model routing explanation, activation status display.
