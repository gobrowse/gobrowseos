# Architecture Plan

## Product Boundary

Gobrowse OS is a modular monolith with one authoritative Rust application, one PostgreSQL database, a Rust/WASM browser client, and optional isolated services. The minimum deployment is `app + postgres`. Sandboxing is deliberately outside the app's privilege boundary.

## Workspace

- `gobrowse-core`: portable domain types, state machines, provider/tool traits, ranking, chunking, policies, and typed protocol contracts.
- `gobrowse-server`: Axum API, CLI, PostgreSQL adapters, authentication, provider adapters, realtime fanout, migrations, diagnostics, and static asset service.
- `gobrowse-web`: Leptos CSR operator interface. It depends on typed core contracts and HTTP/WebSocket APIs, never database structures.

Modules remain modules until an actual process, target, or dependency boundary justifies another crate. This avoids a dependency graph made of tiny crates while preserving interfaces that can later move behind subprocess RPC or WASI components.

## Runtime Plan

1. Milestone 1: typed config, server/CLI, secure owner setup and login, schema, Compose, CI, Rust/WASM shell.
2. Milestone 2: Books, immutable revisions, Autobiography proposals/rollback, chunks, PostgreSQL lexical and pgvector retrieval, background embedding jobs.
3. Milestone 3: model registry, neutral messages, streaming agent state machine, deterministic fake model, fallback classification, durable conversation events.
4. Milestone 4: tools, policy decisions, approvals, idempotency records, file/Git operations, checkpoints.
5. Milestone 5: separately deployed `sandboxd`, rootless runtime adapter, PTY streams, persistent workspace volumes, egress policy.
6. Milestone 6: tasks, Activity Ledger, worktrees, delegation, agent timelines and UI.
7. Milestone 7: Skills import/revisions/evaluation/promotion.
8. Milestone 8: dual-era MCP client, encrypted credential vault, OAuth, diagnostics and generator workflow.
9. Milestones 9-12: scheduler/webhooks, optional browser/connectors/plugins, operations commands, full E2E/security/performance release gate.

## Data Flow

Model requests are assembled from system policy, profile, active task, bounded recent messages, pinned Books, hybrid Library results, Autobiography excerpts, relevant Skill summaries, tools, and a token budget. Retrieved external content remains labelled data and cannot alter the instruction hierarchy.

All mutating actions produce application events. Security-sensitive events additionally produce append-only audit rows. PostgreSQL notifications are only wakeups; durable replay comes from monotonically ordered event rows.

## Release Gates

Every milestone must pass formatting, Clippy with warnings denied, nextest, and its integration tests. Features requiring an unimplemented security boundary remain disabled and visibly reported as unavailable rather than simulated.
