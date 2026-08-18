# CURRENT CHECKPOINT — M22 (resume point)

CURRENT_MILESTONE: M22 — Unified Library + Sandbox & Plugin Runtime (in progress; browser/runtime journeys largely passing; final gates pending). M23 design not started.

STATUS
- Complete: M22 architecture (PLAN.md), schema 20 migration + triggers, core plugin/manifest/marketplace types, sandboxd↔server integration (SandboxClient, ProvisionWorkspace, /sandbox/* routes, 15 agent tools), plugin install/upgrade/rollback/marketplace API, unified Library UI + plugin stepper + terminal UI, web fixes (book cards render, stepper reactivity, terminal poll, marketplace URL), prod sandboxd + app deployed with sandbox PASS.
- Partial: browser journeys A (terminal) / C (source book) / F (plugin install) / I (upgrade/rollback) PASSED; B (agent sandbox tools) / D (skill book) / E (MCP book) / G (plugin activation) / H (embedded MCP dedup) / J (retrieval scale) partially verified or pending chat-model runs; M22 security acceptance audit in flight (cheaper-checker).
- Not started: M23 design doc; M24/M24b (recorded in roadmap only).

LAST_GOOD_COMMIT: 0fe0c1e (security fixes; working tree clean, pushed). Checker (CheaperCheckerM22AcceptanceB) audited c1f6007..29db56d: F1 HIGH (MCP create/update had no role gate → host command spawn by any user via library_load) FIXED (OWNER/ADMIN gate); F2 (no companion Books for NEW skills/MCP servers) FIXED (migration 0021 INSERT triggers, schema 21); F3 (self-asserted VERIFIED trust) FIXED (verify_artifact_signature always false until real publisher keys; PATCH rejects VERIFIED). F4 minor (terminal_id not bound to workspace) recorded, not release-blocking. Clippy all-targets + unit tests clean. Preceding: 70e271d, 2f0f5af, 62e89ee, 30d29c8, c5ce19f, e490e96, 36ba8ce, e327ca2, 6fd3728, 3ca3815, 62e89ee… (all pushed).

SCHEMA_VERSION: 21 (migration 0021 INSERT triggers; verified 21 on both prod DBs).

FILES_CHANGED_THIS_BATCH: crates/gobrowse-web/src/app.rs (book-card rendering via single-expression match + precomputed non-reactive conversation snapshot + always-rendered pin button; stepper `update()` reactivity; run_search preventDefault; terminal poll stops only on terminal state; TerminalReadResponse.output_complete removed), crates/gobrowse-server/src/plugin_github.rs (api_url query-string fix; v-prefix tag fallback), crates/gobrowse-sandboxd/src/runtime.rs + main.rs (volume ownership chown via podman unshare, container_uid/daemon_gid, real user home for podman env, .Subnets attestation, pty_command env), crates/gobrowse-server/src/run_api.rs (workspace network policy), crates/gobrowse-server/src/plugin_api.rs (canonical version keys), crates/gobrowse-server/src/sandbox_api.rs (policy resolution).

ARCHITECTURE_DECISIONS (affect continuation)
- Library = books table registry with `kind` column; Skill/MCP/Plugin Books are companion rows (skill_id/mcp_server_id/plugin_id in metadata). No duplicate MCP Book for plugin-embedded MCP.
- Sandbox: server → sandboxd Unix socket (token + peer-uid auth) → rootless podman. Workspace volumes chowned to `container_uid:daemon_gid` (2775 setgid) via `podman unshare`; container runs as image user (uid 1000 → host 1000 mapping with keep-id).
- Plugin lifecycle: install → dormant; activation on demand; upgrade/rollback with permission diff + re-approval; canonical version keys (manifest version, not raw tag) + GitHub v-prefix fallback for re-resolution.
- Progressive loading: Phase-2 implicit search returns snippets only; library_load bounded (≤5/run, ≤24k chars, ≤16 MCP tools); AUTOBIOGRAPHY/RESTRICTED/PRIVATE excluded; token_metrics run event.
- UI: Leptos quirk — `{move || {...}}` reactive blocks returning Vec<View> from view! DON'T render when they read a static signal (active_conversation) inside the map; use single-expression match + precompute non-reactive snapshots outside the closure.

IMPLEMENTED (concise)
- Unified Library search/filters/detail; Source/Skill/MCP/Plugin creation; plugin install stepper (source→preview→approve→install→dormant) + detail + marketplace; sandbox Terminal page (PTY poll/write/resize/interrupt/close, file manager); skills/mcp nav → Library redirect.
- /plugins/* + /sandbox/* + /library/books/{id}/load routes; token metrics; MCP client (mcp_client.rs).
- ProvisionWorkspace protocol op + daemon volume provisioning.

VERIFIED (exact)
- `cargo check -p gobrowse-web --target wasm32-unknown-unknown` + `cargo check -p gobrowse-server -p gobrowse-sandboxd` — clean (this batch).
- Browser (m22test at localhost:8082): login; Journey A terminal (start/echo/file create+cat/resize/interrupt/^C/fs read via API); Journey C source book create + search (snippet); Journey F plugin install via UI (preview→approve→INSTALLED·DORMANT); Library grid renders 3 cards (PLUGIN/SOURCE/auto) with actions; marketplace search returns 10 GitHub results; upgrade v1.0.0→v1.1.0 (diff) → activate → rollback via API.
- Prod: sandboxd systemd service active; app container sandbox PASS (doctor); terminal start + container↔fs coherence (container wrote file, fs_read returned it); schema 20; image gobrowse-os-app:m22 deployed to main app + m22test.
- Prior CI green: 32106629410-area runs through c5ce19f-era; latest CI for 29db56d running (not yet confirmed).

NOT_VERIFIED (deferred)
- Full CI green for 29db56d (in flight).
- Journeys B (agent sandbox tools via chat), D (skill book load), E (MCP book), G (plugin activation), H, J (retrieval scale + token metrics event) — need chat runs (model configured on m22test: Mistral open-mistral-nemo).
- DB-gated integration tests (plugin_integration, library_load_integration, sandbox_integration, migration tests) — run in CI only (no local Postgres).
- M22 security acceptance audit — cheaper-checker in flight (CheckerM22AcceptanceB).

NEXT_TASK: Collect the security-audit result; fix any REQUIRED FIXES; run the chat-based journeys (B/D/E/J) on m22test (Mistral model already configured); then M23 design (architect), full CI + wasm/trunk + nextest, deploy main app with the final UI, and the M22 acceptance report.

NEXT_FILES: crates/gobrowse-server/src/* (fixes from audit), crates/gobrowse-web/src/app.rs (chat journeys), docs/m22-report.md (new), PLAN.md (M23 section).

BLOCKERS: None hard. M4/M7 remain BLOCKED_EXTERNAL records (preserved). Primary architect/checker hit OpenRouter usage limit — use cheaper-architect/cheaper-checker until the limit resets.

MIGRATION_STATE: 0020 applied everywhere (schema 20). No pending migration.

DEPLOYMENT_STATE: Prod main app + m22test BOTH run image gobrowse-os-app:m22 (security-fixed binary + web dist + migrations through 0021). Schema 21 verified on gobrowse + gobrowse_m22. Health 200 on 8080 + 8082. Health 200 on 8080 + 8082; doctor sandbox PASS; library search 1 result; plugin hello-gobrowse dormant. sandboxd release binary deployed with all fixes; cgroup Delegate=yes; restricted network gobrowse-restricted-prod. sandboxd release binary deployed with all fixes; cgroup Delegate=yes; restricted network gobrowse-restricted-prod (public subnet 45.90.28.0/24).

ROLLBACK_STATE: DB backup /opt/gobrowse-os/backups/pre-m22-20260818.dump (+sha256). Previous images tagged (ui-fixed3..6, m22). Rollback = restore dump + deploy previous image.

KNOWN_BUGS
- P1: none known in committed code.
- P2/P3: "0 WORKSPACES" transient label before workspace list loads (cosmetic); `\x1b[6n` cursor-query escape shown in terminal output (cosmetic, PTY doesn't answer it); marketplace search returns broad GitHub repo matches (not filtered to gobrowse-plugin.json presence); main app UI not yet updated with final web fixes.

SECURITY_NOTES
- Sandbox: no raw Docker socket; token + peer-uid auth; no host fallback; workspace policy server-side; volume ownership fixed (container uid 1000 + daemon gid 995, 2775 setgid); cgroups delegated (Delegate=yes); Restricted network = attested public-subnet bridge; NONE = network none.
- Plugins: UNTRUSTED→USER_PROVIDED on approval; digest pinning + TOCTOU guard; permissions = references only; upgrades require diff + re-approval; rollback retained.
- The security audit result (CheckerM22AcceptanceB) is the authoritative gate — apply its REQUIRED FIXES before M22 acceptance.

DO_NOT_REDO
- The book-card Leptos rendering quirk (see ARCHITECTURE_DECISIONS) — do NOT reintroduce block closures reading static signals in the Library grid.
- Version normalization (canonical keys + v-prefix fallback) — do not "simplify" it back to raw tags.
- Volume ownership fix — do not revert to daemon-only ownership.
- Podman env: HOME must be the real daemon user home + XDG_RUNTIME_DIR set in BOTH command() and pty_command().
- Tar deploys need `-C` for path-correct extraction; server release builds MUST be in bookworm (glibc 2.36) not host (2.39).