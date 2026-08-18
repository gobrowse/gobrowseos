# RESUME.md — Gobrowse OS (compact)

WHAT GOBROWSE IS: Self-hosted agent OS. Workspace = 4 crates (core, server `gobrowse`, web Leptos/WASM, sandboxd). Rootless-podman sandbox + retrieval-driven Library + plugin runtime.

ACTIVE MILESTONE: M22 — Unified Library + Sandbox & Plugin Runtime. M23 (adaptive router) design after. M24/M24b defined only.

CURRENT ARCHITECTURE: books table = Library registry (kind: SOURCE/SKILL/MCP/PLUGIN/AUTOBIOGRAPHY; companion rows for skills/mcp/plugins). Server → sandboxd Unix socket (token+peer-uid) → rootless podman. Plugins: normalized tables + PLUGIN Book, install→dormant, upgrade/rollback with diff. Progressive loading: snippets in implicit context, bounded `library_load`, token_metrics event.

WHAT JUST CHANGED: Last batch = web fixes (book cards render, stepper reactivity, search form submit, terminal poll, marketplace URL) + plugin GitHub URL query fix + sandboxd volume-ownership/podman-env fixes. Committed: 29db56d (HEAD, clean).

WHAT WORKS (verified on prod m22test @ localhost:8082): login; sandbox terminal (start/echo/write/cat/resize/interrupt/^C/reconnect); source-book create+search; plugin install via UI stepper (preview→approve→INSTALLED·DORMANT); Library grid (3 cards w/ actions); marketplace search; plugin upgrade v1→v2 (diff) + activate + rollback; schema 20; prod sandboxd active + doctor sandbox PASS; container↔fs coherence.

WHAT DOES NOT YET EXIST: M23 design doc; chat-based journeys B/D/E/J verified (Mistral model configured on m22test: open-mistral-nemo); final security-audit gate (cheaper-checker in flight); full CI green for 29db56d (running); main-app dist not yet updated with final web fixes; DB-gated integration tests only in CI.

EXACT NEXT STEP: 1) Collect CheckerM22AcceptanceB result → apply REQUIRED FIXES. 2) Run chat journeys on m22test (conv + user msg + run → library_search → sandbox tools). 3) M23 design (cheaper-architect → PLAN.md). 4) Full CI + trunk + deploy main app. 5) M22 acceptance report.

IMPORTANT INVARIANTS: no unsafe; clippy -D warnings; schema version 20 (bump tests if changed); Leptos: no reactive block reading static signals in list closures (see CURRENT_CHECKPOINT.md ARCHITECTURE_DECISIONS); server release binaries must build in rust:1.94-bookworm (glibc 2.36); tar deploys use `-C`; sandbox volumes owned container_uid:daemon_gid 2775 setgid; podman env = real daemon-user HOME + XDG_RUNTIME_DIR (both command() and pty_command()); canonical plugin versions + GitHub v-prefix fallback.

RESUME_FROM: docs/CURRENT_CHECKPOINT.md (full detail). CI: gh run list. Prod: ssh root@178.128.179.216. Test app: localhost:8082 (m22test@example.com / TestPassword123!).