# Roadmap and Release Gates

Each subsystem is marked `foundation`, `usable`, or `release-gated`. A foundation is not advertised as secure/complete until its integration and adversarial test gate passes.

| Area | Initial state | Release gate |
|---|---|---|
| Config, health, migrations | usable | Compose cold-start and upgrade tests |
| Owner/password auth | foundation | rate-limit, CSRF, rotation, WebAuthn/OIDC tests |
| Library | foundation | PostgreSQL hybrid retrieval/access/concurrency suite |
| Agent runtime | foundation | deterministic fake-provider E2E and cancellation suite |
| Sandbox | release-gated/off | rootless runtime, PTY, egress and boundary tests |
| MCP | foundation | conformance, OAuth matrix and doctor matrix |
| Skills/worktrees/tasks | foundation | revision, isolation and concurrency suite |
| Scheduler/webhooks | release-gated/off | restart/replay/signature tests |
| Browser/connectors/media | release-gated/off | isolated adapter-specific security tests |
