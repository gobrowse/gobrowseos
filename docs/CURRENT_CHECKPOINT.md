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
- nextest workspace

---

## 2026-08-21 Product-Audit Deploy (m25-final)
- Deployed: gobrowse-os-app:m25-hotfix @ 178.128.179.216 (schema 26, commit c7bf437)
- WARNING: m25-final is the KNOWN-BROKEN image (workspace SELECT crash loop, exit 139) — do NOT roll back to it
- Fixes: 8 delete flows, workspace PATCH+edit modal, session revocation, destructive tool risk, step-up on vault, SSRF guard, cross-profile UI close, webauthn RP-ID spec pairing, dup workspace picker, PRODUCT_AUDIT.md
- Gates: nextest 596/596, clippy clean, fmt clean
