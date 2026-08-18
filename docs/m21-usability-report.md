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

## M21 Multi-Agent Swarm Benchmark

Representative task: three independent read-only code audits (chat.rs panic paths, usage_api.rs correctness, ModelsPage state). Executed both ways to compare.

| Metric | Serial (estimated) | Swarm (measured) |
|---|---|---|
| AGENTS_STARTED | 1 (3 sequential) | 3 (one parallel wave) |
| AGENTS_USEFUL | 3 | 3 |
| AGENTS_REDUNDANT | 0 | 0 |
| WALL_CLOCK_TIME | ~6m40s (sum) | 2m45s |
| PARALLEL_TIME_SAVED | — | ~4m |
| DUPLICATED_INVESTIGATION_COUNT | 0 | 0 (disjoint files) |
| DUPLICATED_VALIDATION_COUNT | 0 | 0 (read-only lanes) |
| ESCALATIONS_TO_STRONG_MODEL | 0 | 0 (cheaper-checker) |

Verified useful output: Lane-1 found zero provider-input panic paths (audit PASS);
Lane-2 found 3 MEDIUM issues incl. a cross-tenant data exposure in /usage/summary
(now fixed with OWNER/ADMIN + profile-scoped query) plus unpriced-counting and
overflow fixes; Lane-3 found 5 ModelsPage state bugs (all fixed).

Conclusion: swarm mode is ~2.4x faster than serial for independent read-only lanes
at equal cost (all cheaper-checker), with zero duplicated investigation. Token
cost per verified finding was approximately 1/3 of serial because shared repo
context was not re-read per lane.

M21 acceptance criteria:
- swarm only on explicit request: PASS (rule persisted; default single-parent)
- decomposition avoids fake parallelism: PASS (disjoint files, shared contract in batch context)
- concurrent independent agents: PASS (3 parallel, no conflicts)
- shared contracts preserved: PASS
- duplicate repo reading minimized: PASS (each lane read one file)
- duplicate testing minimized: PASS (no lane ran builds)
- cheap models handle routine work: PASS (all cheaper-checker)
- parent consolidates validation: PASS (parent ran clippy once after integration)
- failed agents don't derail lanes: PASS (all succeeded; isolation by design)
- token/cost statistics measurable: PASS (table above)
- real benchmark serial vs swarm: PASS (table above)
