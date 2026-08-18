# Roadmap and Release Gates

Each subsystem is marked `foundation`, `usable`, or `release-gated`. A foundation is not advertised as secure/complete until its integration and adversarial test gate passes.

| Area | Initial state | Release gate |
|---|---|---|
| Config, health, migrations | usable | Compose cold-start and upgrade tests |
| Owner/password auth | foundation | rate-limit, CSRF, rotation, WebAuthn/OIDC tests |
| Library | usable | PostgreSQL hybrid retrieval/access/concurrency suite passed in Milestone 2 |
| Agent runtime | foundation | deterministic fake-provider E2E and cancellation suite |
| Sandbox | release-gated/off | rootless runtime, PTY, egress and boundary tests |
| MCP | foundation | conformance, OAuth matrix and doctor matrix |
| Skills/worktrees/tasks | foundation | revision, isolation and concurrency suite |
| Scheduler/webhooks | release-gated/off | restart/replay/signature tests |
| Browser/connectors/media | release-gated/off | isolated adapter-specific security tests |

## M24 (defined, not started)

Full Token, Runtime, Container & Architecture Optimization — make Gobrowse dramatically
lighter without losing capability. Same-or-better capability, less context/RAM/disk/CPU/
latency/smaller containers/simpler code. Optimize architecture, not just compiler flags;
measure baselines first (token/context usage, image sizes, idle RSS, startup, WASM/binary
size, search/chat latency, build time). Non-negotiable: do NOT remove features. Pay for
capabilities only when needed (small core + unified searchable library + lazy book content
+ lazy tool schemas + dormant plugins + on-demand MCP/sandbox + cheapest-capable routing
+ bounded agent context + minimal containers). Keep boring, LLM-editable code; maintain an
architecture map; stress-test with many Books/plugins (context and RAM must not scale with
installed count). Requires before/after benchmark table and capability regression matrix.

## M24b — Custom UI System (defined, not started; after M22/M23 + M24)

Agent-editable UI with permission. THE UI IS CUSTOMIZABLE; THE SECURITY MODEL IS NOT.
UI permission levels: DENY / ASK (default) / ALLOW_WORKSPACE / ALLOW_GLOBAL (global requires
stronger approval). Safe change pipeline: request -> isolated change -> build -> automated
validation -> browser preview -> permission review -> user approval -> activate -> rollback;
never destructively edit the known-good active UI; preserve CURRENT + PREVIOUS + CANDIDATE.
Custom UI package format (kind: gobrowse-ui, api_version, entry, capabilities, permissions,
theme, source). Multiple full UI implementations over the SAME stable versioned backend APIs
+ GET /api/v1/capabilities capability discovery; graceful degradation. Plugin-provided UI
extensions load lazily; Plugin Book owns its UI components. Third-party UI = untrusted: no
raw secrets, no DB/sandbox creds, no internal tokens; CSP, no inline script by default,
hash-verified artifacts, HttpOnly secure cookies, server-side auth/RBAC/approval always
authoritative. THEME (visual tokens only) vs FULL UI PACKAGE distinction. Versioning
(UI_ID/NAME/VERSION/SOURCE/DIGEST/CREATED_BY/ACTIVATED_BY/API_VERSION/history; never remove
last known-good). Recovery UI: minimal built-in interface custom packages cannot overwrite;
auto fallback on broken UI (login + UI management + diagnostics). Dev kit: UI-SDK.md, API
docs, capability endpoint, manifest schema, minimal starter, build/package/validate/install
commands — LLM-friendly, boring explicit contracts, no server fork required. Agent uses the
same path as humans (clone -> edit -> build -> browser-test -> screenshots -> request
activation). Agent UI edit requires explicit user approval; silent activation prohibited.

Note (user directive): once M24 optimization lands, ALL subsequent builds (every later
milestone's Docker images, WASM, binaries, builds) MUST also be ultra-optimized to the same
standard — M24 optimizations are the permanent baseline, not a one-off.
