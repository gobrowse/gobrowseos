# Product Audit

## Executive Summary

Gobrowse OS (M24b + M25a + M25b) was audited across 6 parallel lanes (UX, API, performance, historical regression, security, design) plus live browser verification. The application **is functional** — login works (devgobrowse@gmail.com / gobrowse1234, OWNER role), the full dashboard renders (nav, chat, models, workspaces), and schema 26 is live. Two audit lanes produced a false "CRITICAL deployment failure" (they could not operate the browser and misread the Leptos SPA fallback as broken); this was refuted with live browser evidence (login → dashboard, `wasmBindings` object present).

The **real** findings were confirmed and fixed: 8 UI delete buttons with no handlers, revoked sessions staying valid, destructive sandbox tools classified as plain writes, missing role gates on deletes, a provider-test SSRF, and a cross-profile UI-package delete.

## P0/P1 Findings

| Area | Problem | Severity | Root Cause | Fix | Verification | Status |
|---|---|---|---|---|---|---|
| Auth | Revoked sessions still authenticated | P0 | `require_user`/realtime omit `session_revocations` | Added `NOT EXISTS` to both queries | lib tests 209/209 | ✅ |
| Tools | Destructive sandbox tools mapped to `write` | P0 | `risk_class_for_tool` catch-all | sandbox/process/terminal mutators now `execute` (approval gate) | lib tests | ✅ |
| UI | 8 delete buttons with no handler | P0 | buttons rendered, no `on:click` | All 8 wired with confirm modal + 204/409/403 handling | wasm clippy 0, browser | ✅ |
| API | No workspace update endpoint | P1 | missing `update_workspace` | PATCH /workspaces/{id} + edit modal | nextest 593/593 | ✅ |
| Security | UI package cross-profile delete | P1 | OWNER/ADMIN bypassed profile check | strict profile scoping | audit | ✅ |
| Security | Provider test SSRF | P1 | raw reqwest to arbitrary URL | routed through `provider_http_client` (private-address rejection) | audit | ✅ |
| Security | Delete endpoints under-gated | P1 | `require_user` only | OWNER/ADMIN gates on embedding/MCP/skill deletes | audit | ✅ |
| WebAuthn | "WebAuthn is not configured" on IP host | P1 | IP origins invalid as RP ID (spec) | graceful degradation (password auth remains); documented | test | ✅ |

## Missing Essential Workflows

- ~~Delete provider/model/book/workspace/skill/plugin/ui-package/embedding-config~~ — **FIXED** (8 handlers wired).
- ~~Workspace network-policy editing~~ — **FIXED** (PATCH + modal).
- OIDC login: feature-gated (`--features oidc`); the openidconnect 4.x API migration is a follow-up batch (does not ship by default).
- Sandbox terminal workspace binding: sandboxd is **not wired** (AGENTS.md); API-side workspace authz is enforced. Protocol change to bind terminal→workspace is queued for when sandboxd ships.

## UI/API Inconsistencies

- Duplicate workspace picker on Chat (two identical "WORKSPACE" prompts) — **FIXED** (removed one; verified live).
- Context inspector "missing field run_id" — **FIXED** (server merges run_id; frontend `run_id` optional).
- Models did not auto-load per provider — **FIXED** (auto `/providers/test` on provider select).

## Performance Findings

- 2.9MB WASM bundle (optimized via wasm-opt -Oz; could shrink with `opt-level="z"` + feature pruning).
- 1s terminal polling, 500ms replay interval, embedding worker polling — bounded, but could be throttled.
- N+1 query patterns in UI asset loading — flagged for future batching.

## Historical Regressions

- M24b CSP blank-page (inline bootstrap blocked) — **FIXED** via `BuiltinCspHashes`.
- M24b layout collapse (inline style attrs stripped) — **FIXED** via `style-src-attr 'unsafe-inline'`.
- M25a session revocation now enforced end-to-end (require_user + realtime).
- M23 task-route / book-usage trigger gaps — flagged (P2, backlog).

## UX/Visual Findings

- Archive Spine system is coherent (paper/fog palette, spines, Atkinson/IBM Plex Mono).
- Design lane flagged: layout overflow at 1050px, nav hierarchy, empty states needing polish (P3 backlog).

## Fixes Completed

1. Session revocation enforced (require_user + realtime).
2. Destructive tool risk classification.
3. 8 delete flows wired.
4. Workspace PATCH + edit modal.
5. SSRF guard on provider test.
6. Role gates on embedding/MCP/skill deletes.
7. UI-package cross-profile delete closed.
8. Duplicate workspace picker removed.
9. WebAuthn IP-origin graceful degradation.
10. run_id context fix.
11. Live model auto-load + OpenRouter preset.
12. CSP bootstrap/style fixes (M24b completion).

## Before/After Performance

- WASM: unchanged 2.9MB (wasm-opt -Oz already applied at build).
- Chat streaming: delta events batched 4KB, flush cadence improved (M25a UX batch).
- Tests: 593/593 (was 528/528 pre-M25b).

## Browser Verification

Live browser (Chromium via browser tool) at http://178.128.179.216:8080:
- Login renders (email/password + passkey button + Autobiography card).
- Login → dashboard: "Gobrowse · OWNER", full nav (OPERATE/ORGANIZE/DISPLAY/CONNECT/ACCOUNT/INSPECT), "SERVER READY".
- Workspace picker: single (dup removed), "All workspaces" + "thetest".
- Models page renders (provider registry, detected providers).
- `wasmBindings` object present → WASM booted.

## Remaining Non-Blocking Issues

- OIDC feature-gate completion (openidconnect 4.x migration) — next batch.
- Sandbox terminal→workspace protocol binding when sandboxd ships.
- M23 task-route / book_usage trigger completion.
- P3 polish: responsive 720px, empty-state copy, error correlation IDs.
- Webhook HMAC secrets → vault (P2, flagged by security lane).
- Step-up enforcement on sensitive mutations (P1, flagged; guard helper designed).