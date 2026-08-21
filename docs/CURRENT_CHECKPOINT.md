# CURRENT CHECKPOINT — M24b COMPLETE (Custom UI System landed)

CURRENT_MILESTONE: M24b — Custom UI System (implementation + verification complete). M24 optimization baseline preserved. Next: post-M24 backlog / M25.

STATUS
- M24b batches 1-9 complete and committed (HEAD: 7ce7706). M24 already landed (1df8ff3 tracked as baseline).
- Custom UI System: ui packages (THEME/FULL_UI) with full lifecycle — preview → install (digest-verified, explicit approve) → activate → rollback → delete; per-profile permission level DENY/ASK/ALLOW_WORKSPACE/ALLOW_GLOBAL; companion book per install; one-active-per-profile invariant.
- capabilities_api.rs: capability exposure with ui_packages feature flag; recovery UI (standalone crate gobrowse-recovery) served without auth at /recovery.
- csp.rs: content-security-policy builder; CSP on / and /recovery, no unsafe-inline anywhere.
- migration 0024: ui_packages, ui_package_assets, ui_package_capabilities tables; profiles.ui_permission_level; books GOBROWSE_UI kind; schema 23→24.
- UI-SDK: docs/UI-SDK.md (12.4 KB), schema/ui-package-manifest.json, examples/starter-ui (manifest + theme.css + README), tools/ui-package packaging script.
- gobrowse-web: UiPackages specimen-wall page (kind badges, state/trust/source badges, expandable detail, install form with preview + approve), provider registry polish (empty-state CTA, curated presets, live Test button with spinner states). Design continues the existing brutalist OS language (1px --rule borders, IBM Plex Mono, ink spines); no second visual system.

LAST_GOOD_COMMIT: 7ce7706 (feat: M24b Custom UI System — ui packages, capabilities, CSP, recovery). Pushed to origin/initial-agent-os.

SCHEMA_VERSION: 24 (migration 0024_ui_packages.sql). Asserted in ui_packages_integration.rs (schema_24_migration_applies), postgres_integration.rs:42, worktrees_integration.rs:50.

VERIFIED (exact — M24b evidence)
- fmt clean; clippy workspace + wasm32 web + wasm32 recovery all -D warnings clean.
- nextest workspace: 522/528 pass; 6 failures ALL in plugin_integration (pre-existing, GitHub-mock 503 "plugin source is unavailable or timed out" — env, not code; they failed identically at M24 HEAD).
- ui_packages_integration 7/7 PASS (new): preview valid/invalid, install digest-mismatch 409 + approve-gate 422, full lifecycle (install→activate→rollback→delete + CSP headers no-unsafe-inline + recovery accessible), delete-last-known-good 422, capabilities/recovery without auth, schema 24.
- Real server bug found & fixed during verification: activate handler swept the just-demoted rollback anchor (state='previous') to rolled_back, so rollback could never find a row; fixed by sweeping stale anchors BEFORE demoting the current active. Covered by ui_packages full_lifecycle (A stays previous, rollback reactivates A).
- Test harness fixes (vs designer's initial): cookie name gobrowse_session (DEV, secure_cookies=false default), Origin http://127.0.0.1:8080 matches test_settings.public_origin, digest mismatch → 409 (Conflict), approve=false → 422 (Validation), install state candidate, delete-active → 422.

ARCHITECTURE_DECISIONS (carry forward)
- M24 is the PERMANENT optimization baseline (user directive) — M24b added no regression: lazy sandbox, slim tool schemas, gobrowse health, bounded context all preserved.
- UI packages mirror the plugin model where sensible, but single-active-per-profile (unique partial index) with explicit previous anchor; activate sweeps stale anchors before demoting (rollback-anchor preservation).
- CSP builder is the single source of truth for / and /recovery; no unsafe-inline permitted — asserted in tests.
- Recovery UI is a static no-auth surface (account recovery path) — auth-free by design, documented.

DO_NOT_REDO
- All M22/M23/M24 DO_NOT_REDO items.
- Do NOT re-add additionalProperties:false / verbose descriptions / redundant maxLength to tool schemas.
- Do NOT re-add curl/git to the runtime image; keep `gobrowse health`.
- Do NOT make sandbox connection eager again.
- Do NOT re-break the rollback anchor (activate must sweep stale 'previous' BEFORE demoting current active).
- Do NOT add unsafe-inline to CSP; new UI packages must go through the manifest + digest + approve flow.

DEPLOYMENT_STATE: M24 still the deployed production state (M24b NOT yet deployed; per process, this checkpoint records local acceptance of M24b; deploy M24b artifact + migration 24 following docs/deployment.md when scheduled).

NEXT_TASK: Post-M24b backlog (roadmap M25+) — OR deploy M24b to production (migration 23→24, image rebuild, rollback preserved). Historical audit lanes (docs/HISTORICAL_AUDIT.md) + alt-checker omnibus audit available.