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
