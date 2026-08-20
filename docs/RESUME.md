# RESUME.md — Gobrowse OS (compact)

WHAT GOBROWSE IS: Self-hosted agent OS. Workspace = 4 crates (core, server `gobrowse`, web Leptos/WASM, sandboxd). Rootless-podman sandbox + retrieval-driven Library + plugin runtime.

ACTIVE MILESTONE: M22 ✅ ACCEPTED (2026-08-20). Next: M23 — Adaptive Capability Router (design in PLAN.md). M24/M24b defined only.

CURRENT ARCHITECTURE: books table = Library registry (kind: SOURCE/SKILL/MCP/PLUGIN/AUTOBIOGRAPHY; companion rows for skills/mcp/plugins). Server → sandboxd Unix socket (token+peer-uid) → rootless podman. Plugins: normalized tables + PLUGIN Book, install→dormant, upgrade/rollback with diff. Progressive loading: snippets in implicit context, bounded `library_load`, token_metrics event.

WHAT JUST CHANGED: M22 fully accepted. Docker image gobrowse-os-app:m22 deployed to main app (8080) + m22test (8082). All build gates pass, all chat journeys (B/D/E/J) verified, sandbox doctor PASS. Commit: d0009df (HEAD, clean).

WHAT WORKS (verified on prod m22test @ localhost:8082): login; sandbox terminal (start/echo/write/cat/resize/interrupt/^C/reconnect); source-book create+search; plugin install via UI stepper (preview→approve→INSTALLED·DORMANT); Library grid with actions; marketplace search; plugin upgrade v1→v2 (diff) + activate + rollback; schema 21; prod sandboxd active + doctor sandbox PASS; container↔fs coherence; chat journeys B/D/E/J (sandbox tools, skill book load, MCP book load, retrieval + token metrics).

WHAT DOES NOT YET EXIST: M23 implementation (design complete in PLAN.md); M24/M24b (recorded in roadmap only).

EXACT NEXT STEP: M23 — implement adaptive capability router per PLAN.md: task classification → library search → capability ranking → minimum book selection → model routing → plugin/MCP activation → execution. Context budget breakdown UI + model routing explanation.

IMPORTANT INVARIANTS: no unsafe; clippy -D warnings; schema version 21; Leptos: no reactive block reading static signals in list closures (see CURRENT_CHECKPOINT.md ARCHITECTURE_DECISIONS); server release binaries must build in rust:1.94-bookworm (glibc 2.36); tar deploys use `-C`; sandbox volumes owned container_uid:daemon_gid 2775 setgid; podman env = real daemon-user HOME + XDG_RUNTIME_DIR (both command() and pty_command()); canonical plugin versions + GitHub v-prefix fallback.

RESUME_FROM: docs/CURRENT_CHECKPOINT.md (full detail). CI: gh run list. Prod: ssh root@178.128.179.216. Test app: localhost:8082 (m22test@example.com / TestPassword123!).
