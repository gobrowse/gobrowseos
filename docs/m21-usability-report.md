# M21 Usability & UI Quality Report

Date: 2026-08-18. Verification performed by browser automation (Firefox devtools MCP + mimo-v2.5 vision) against an isolated test instance, then production smoke test.

## Button & Form Audit

| Metric | Value |
|---|---|
| BUTTONS_TOTAL | 40 interactive controls audited |
| BUTTONS_PASS | 40 (after fixes) |
| BUTTONS_FIXED | 5 (inert CTA, dead search dialog, cross-section feedback, silent validation, unused import) |
| BUTTONS_REMAINING | 0 |
| FORMS_TOTAL | 12 |
| FORMS_PASS | 12 |

Fixed defects (commits):
- `8c86352` — "Add + activate" model form: populate model select on load (was silently blocked by native validation)
- `2f3f4e9` — Lane A audit: removed inert EmptyOperationalPage CTA, removed dead search dialog, added chat-status feedback beside the model form, removed unused import, surfaced silent-validation messages
- `1c87707` — accept catalog provider types end-to-end (backend previously rejected mistral/openai/etc.)
- `e4e9862` — Mistral catalog base URL needs `/v1`
- `2816382` — add Create Book form to Library page
- `5a7c963` — bump schema version assertions to 19

## UI Verification

| Check | Result |
|---|---|
| DESKTOP_UI (1440x900) | PASS |
| MOBILE_UI (390x844) | PASS — layout adapts, no overflow |
| CONSOLE_ERRORS | NONE |
| FAILED_NETWORK_REQUESTS | NONE (one transient invalid_credentials on misconfigured route, resolved) |

## Core User Journeys (browser-verified)

| Journey | Result |
|---|---|
| MODEL_ADD (provider dropdown + autofill + Add+activate) | PASS |
| MODEL_ACTIVATE | PASS |
| CHAT_REAL_RESPONSE (Mistral: "GOBROWSE_CHAT_OK") | PASS |
| CHAT_PERSISTENCE (reload) | PASS |
| LIBRARY_CREATE | PASS |
| LIBRARY_EDIT | PASS |
| LIBRARY_SEARCH | PASS |
| CONTEXT_PIN (book → model returns 739251) | PASS |
| CONTEXT_UNPIN (model no longer knows the code) | PASS |
| CUSTOM_CONTEXT (pinned library context reaches model) | PASS |
| WORKSPACE_CONTEXT | PASS |
| MODEL_LIBRARY_SEARCH/ADD (tools) | Implemented (run_tools.rs), gated on tool_calls capability |
| WORKSPACE_CREATE | PASS |
| MCP_ADD / MCP_DELETE | PASS |
| USAGE_STATS | PASS (10 runs, tokens tracked) |
| COST_CHARTS | PASS (3 charts, no NaN/negative) |
| AUTH (setup/login/logout/reload) | PASS |

## Release State

| Field | Value |
|---|---|
| FULL_CI | PASS (run 32092622585) |
| DEPLOYED_SHA | `5a7c963` |
| DEPLOYED_IMAGE_DIGEST | `sha256:8a00b1b0c414e6d28ab9e4fc0142258ba63f1dac8fab8496759f9e93929758ac` |
| SCHEMA_VERSION | 19 |
| HEALTH_LIVE | PASS |
| HEALTH_READY | PASS |
| DOCTOR | Pass (PostgreSQL, pgvector, Git, static assets, vault) |
| SECURITY_AUDIT | Pass (secure-cookies warns pending HTTPS; sandbox/telemetry pass) |
| ROLLBACK_READY | PASS (image `ui-fixed` + backup retained) |

## Remaining Classification

| Class | Count | Notes |
|---|---|---|
| P0_REMAINING | 0 | |
| P1_REMAINING | 0 | |
| P2_REMAINING | 0 | |
| P3_BACKLOG | 3 | (a) no delete button for model routes in Models UI; (b) books are also creatable via chat tool rather than only UI form; (c) mobile viewport session-cookie behavior — see docs/post-release-backlog.md |

## Production Deployment

- Production upgraded to image `8a00b1b0` (schema 19, all UI fixes).
- Real chat verified against the test instance with Mistral `mistral-medium-3-5` (live API key).
- Production smoke test: login page loads, authenticated owner UI loads, Models/Library/Workspaces/MCP/Diagnostics pages load.
- No new console/runtime errors.
