# PLAN.md — M5 Inert Worktree Metadata CRUD

## Status, Decision, and Roadmap Boundary

**M5 is the active, implementation-ready plan.** Its sole scope is an
workspace-scoped, PostgreSQL-backed CRUD boundary for inert worktree metadata
associated with existing task and agent rows. It validates metadata purely,
derives the persisted worktree path on the server, and records durable,
actor-attributed lifecycle evidence. It does not create, inspect, check out,
remove, or otherwise operate on a Git worktree.

“M5” is the active worktree/task-subagent batch label requested for this plan.
The canonical architecture documentation assigns a broader worktree/delegation
area to another milestone number; this plan does not retag that roadmap. The
numbering discrepancy is a documentation-governance follow-up, not a reason to
expand or defer this bounded metadata slice.

## Preserved External Records

**M4 is `BLOCKED_EXTERNAL`.** The sandbox remains release-gated and off. This
plan records no successful runtime proof and authorizes no source or CI change
until the approved execution prerequisite below is available.

The caller-provided-pipe bounded stdio JSON-RPC framing slice is accepted at
`4c2bff3796ec7ac15b86a167d49828a1eb556b98`. Exact-SHA CI run
`31908353273` passed Rust `95069542559`, web `95069542603`,
supply-chain `95069542548`, and container `95069542574`. This accepts only
bounded stdio framing over caller-provided Tokio async pipes and its tests; it
does not create a process, establish a session, or prove real MCP
interoperability. **M7 remains incomplete.**

The sole remaining M7 gate is `BLOCKED_EXTERNAL`. It requires an approved,
immutable/pinned, independently implemented MCP stdio peer with recorded and
verified provenance, version, and content hash. Non-ignored CI must run a
real-peer integration test that performs `server/discover` and a
capability-authorized `tools/list` against that peer. A fixture, mock,
loopback peer, or in-repository substitute does not satisfy this requirement.

That independent pinned MCP peer prerequisite remains blocked; this M5 plan
does not alter, replace, or satisfy it.

### M4 External Prerequisite (Preserved)

The smallest honest M4 proof requires all of the following, none of which is
available in the current environment:

1. an approved ephemeral **local** runner executing as a non-root user with a
   real rootless Podman installation;
2. preconfigured `/etc/subuid` and `/etc/subgid` entries for that non-root
   runner user;
3. the exact immutable image reference preloaded locally by digest; and
4. a trusted, required, non-ignored CI job bound to that runner.

The runner and its Podman storage must be provisioned before the job starts.
The proof must neither pull nor install anything and must not use `sudo` or
make any host mutation. A Docker shim, remote Podman service, rootful Podman,
fixture, mock, or fake executable is not a substitute for this prerequisite.

Do not start the eventual test or CI work until every external prerequisite is
approved and present. Do not bypass a missing prerequisite with Docker,
rootful/remote Podman, a downloaded image, an install, `sudo`, a configuration
change, or a locally fabricated result. Until then, M4 remains
`BLOCKED_EXTERNAL`, the sandbox remains release-gated and off, and there is no
M4 runtime-proof success claim.

## Exact Product Boundary

Implement these five authenticated HTTP routes under the existing `/api/v1`
router:

| Route | Behavior |
|---|---|
| `GET /workspaces/{workspace_id}/worktrees` | List worktrees the caller may read in `last_activity_at DESC, id DESC` order; clamp `limit` to `1..=500`. |
| `POST /workspaces/{workspace_id}/worktrees` | Create inert metadata and return `201 Created`. |
| `GET /worktrees/{id}` | Return one authorized row. |
| `PATCH /worktrees/{id}` | Replace only the canonical bounded `changed_files` inventory and return `200 OK`. |
| `DELETE /worktrees/{id}` | Delete only the metadata row and return `204 No Content`. |

Create accepts exactly `task_id`, `owner_agent_id`, `repository_root`, optional
`branch`, and `base_commit`. `task_id` and `owner_agent_id` must name existing
rows in the route workspace. `repository_root` is UTF-8 metadata that is
absolute and lexically normalized; it is not canonicalized or inspected on the
filesystem. `base_commit` is a full 40- or 64-character hexadecimal object ID;
abbreviations, symbolic refs, options, whitespace, and non-hex values fail.

When no branch is supplied, derive it with `task_branch(task_id, task.title)`.
The client never supplies the final `path`: derive it with `worktree_path` as
`repository_root/worktrees/task-<first-12-simple-UUID>`. Creation fixes status
to `ACTIVE`. After creation, `task_id`, `owner_agent_id`, `branch`,
`base_commit`, and `path` are immutable. PATCH accepts a non-empty bounded list
of canonical repository-relative UTF-8 file paths only. It rejects absolute
paths, empty names or segments, `.`, `..`, controls/NUL, and backslash aliases;
its limits and deterministic duplicate policy are shared by core and database
validation. Identical PATCHes are `422` and append no event.

Responses expose only `id`, `workspace_id`, `task_id`, `owner_agent_id`,
`branch`, `base_commit`, `path`, `status`, `changed_files`, and
`last_activity_at`. This is a metadata boundary: a later executor must obtain a
trusted configured repository root and rederive/revalidate; it must never trust
the database path or a ledger event as a host capability.

Use existing authentication semantics without exceptions: unauthenticated
requests are `401`; existing globally read-only roles are `403` under
`require_writer`; inaccessible workspaces or worktrees are tenant-hiding `404`;
unsafe or invalid input, empty/identical update, and same-workspace task/agent
failure are `422`; and branch/path ownership conflicts, including races, are
`409` without raw database error disclosure.

## Required Implementation

1. In `crates/gobrowse-core/src/worktrees.rs`, retain process-free validation
   and strengthen it. `validate_branch` must reject Git-ref hazards including
   `@`, `@{`, repeated slash, controls and forbidden characters, `refs/`,
   trailing dot or `.lock`, and dot-leading/dot-only components. Make
   `worktree_path` reject relative or lexically non-normal roots. Add distinct
   pure validation for base commits and bounded changed files; represent unsafe
   repository-root, base-commit, and changed-file outcomes distinctly. No Git
   command, shell, filesystem lookup, or repository abstraction is permitted.
2. In `crates/gobrowse-core/src/activity.rs`, add
   `ActivityKind::WorktreeDeleted`; do not overload agent, merge, or commit
   lifecycle kinds.
3. In `crates/gobrowse-server/src/task_api.rs`, expose only the necessary
   existing helpers as `pub(crate)`: `authorize_workspace`,
   `authorize_workspace_in_transaction`, `append_activity`, and
   `require_writer`. Reuse them; do not create a second authorization,
   locking, cursor, or event-insertion convention.
4. Add `crates/gobrowse-server/src/worktree_api.rs` with direct SQLx matching
   `task_api`: request/query/response types, list/create/get/update/delete,
   authorized-worktree helpers, row mapping, bounded validation, and stable
   conflict mapping. Add `pub mod worktree_api` and wire exactly the five
   routes in `crates/gobrowse-server/src/lib.rs`.
5. Add `crates/gobrowse-server/migrations/0016_worktree_integrity.sql`; make
   it set `schema_metadata.schema_version = 16`.
6. Add real PostgreSQL/router proof in
   `crates/gobrowse-server/tests/worktrees_integration.rs`, using
   `tests/common::acquire_test_lock`. Update
   `crates/gobrowse-server/tests/postgres_integration.rs` to expect fresh
   schema 16, extend the deployed schema-3 upgrade through 0016, and add a
   focused schema-15-to-16 integrity-repair upgrade test.
7. After exact-SHA acceptance only, update `docs/worktrees.md` to describe
   authorized inert metadata and the future trusted-root requirement, and
   `docs/implementation-progress.md` only to claim metadata/database proof—
   never worktree execution, sandbox isolation, delegation, or release-gate
   completion.

## Schema-16 Integrity Migration

Before adding new constraints, identify legacy `worktrees` whose task or owner
agent is outside the workspace, whose branch/path/base commit is unsafe, or
whose `changed_files` is unsafe. Preserve every rejected row, all original
columns, and explicit reasons in `worktree_integrity_quarantine`. Protect that
table with rejecting `UPDATE`, `DELETE`, and `TRUNCATE` triggers equivalent to
the existing task-integrity quarantine protections; then remove the unsafe
metadata row. Never silently repair/reassign a workspace, task, or agent.

Drop `worktrees_task_id_fkey` and `worktrees_owner_agent_id_fkey`. Add:

- `worktrees_task_same_workspace_fk (workspace_id, task_id)` referencing
  `tasks(workspace_id, id) ON DELETE RESTRICT`;
- `worktrees_owner_same_workspace_fk (workspace_id, owner_agent_id)`
  referencing `agents(workspace_id, id) ON DELETE RESTRICT`.

Reuse composite keys installed by migration 0010. Retain existing
workspace-scoped unique branch/path ownership. Install database functions and
checks that mirror pure validation for branch, task-derived absolute path, full
base commit, and bounded normalized `changed_files`, so raw SQL cannot bypass
the API. Do not modify task/agent semantics, activity cursor ordering, audit
append-only behavior, or prior migrations.

Migration risk is material: the known private deployment is schema 3 and must
upgrade through every intermediate migration to 16. Fresh schema, schema-15
repair, and deployed-schema-3 upgrade paths must be proven. Rust and SQL
validators can drift and must exercise identical accepted/rejected fixtures.
Before deployment, take and verify a backup/restore point and ensure adequate
disk for bounded metadata/evidence. If product ownership has not explicitly
accepted quarantine-and-remove treatment for unsafe legacy rows, stop before
authoring or applying the migration.

## Authorization, Ledger, and Concurrency

All reads require the existing authenticated profile/workspace-membership
predicate. All mutations use `require_writer`. Within the transaction,
reauthorize after acquiring the workspace guard; retain existing profile
OWNER/ADMIN and workspace OWNER/EDITOR rules, and deny VIEWER mutation.

Acquire locks in this order for every mutation:

1. workspace Activity Ledger advisory lock;
2. workspace row;
3. membership row when needed;
4. worktree row for PATCH/DELETE.

For create, authorize first under that order, then validate the task and owner
agent in the route workspace. Never lock a worktree before the workspace guard.
The workspace lock serializes membership revocation, uniqueness claims,
metadata mutation/deletion, and event cursor allocation. Database unique
constraints remain final authority. Create uses `INSERT ... ON CONFLICT DO
NOTHING RETURNING` (or equivalent explicit constraint mapping): no returned row
is `409`; exactly one concurrent same-branch/path claimant may append evidence.
PATCH and DELETE lock the authorized row `FOR UPDATE`. Do not add an in-memory
mutex or publish before commit.

Each successful mutation appends its lifecycle Activity Ledger event and
append-only audit row in the same transaction as its metadata change. HTTP
mutations attribute `actor_user_id` to the authenticated user and set
`agent_id = NULL`; `owner_agent_id` is payload metadata, not proof that the
agent acted. Suggested audit actions are `worktree.created`,
`worktree.files_changed`, and `worktree.deleted`.

- `WORKTREE_CREATED`: `worktree_id`, `branch`, `base_commit`, `owner_agent_id`.
- `FILES_CHANGED`: `worktree_id`, bounded `changed_files`.
- `WORKTREE_DELETED`: `worktree_id`, `branch`, `owner_agent_id`.

Never put an absolute host path in the ledger. `append_activity` and its
database trigger retain commit-ordered workspace cursors. The public activity
endpoint must reject `WORKTREE_DELETED`, as it rejects all reserved lifecycle
kinds.

## Required Proof

Core unit tests must prove safe generated/explicit branch acceptance and
rejection of traversal/ref syntax, `@`, `@{`, repeated slash, controls,
forbidden characters, trailing dot/`.lock`, `refs/`, and dot components. They
must prove exact path derivation from normalized absolute roots, reject relative
or `CurDir`/`ParentDir` roots, accept exactly 40/64 hex commits, and cover
changed-file normalization, rejection cases, bounds, aggregate size, and
deterministic duplicates.

Real PostgreSQL/router tests must cover owner/editor create; member/viewer
reads; foreign-tenant `404`; viewer denial; full list/get/update/delete
responses; server default branch/path derivation; prohibition on client
path/status input; immutable identity fields; and stable `422`/`409` mapping.
They must prove same-workspace task/agent enforcement through both API and raw
SQL, database rejection of unsafe values, actor attribution and exactly one
event/audit per successful mutation, metadata-only delete, reserved-event
anti-forgery, and existing append-only event behavior.

Race tests must prove two identical creates return one `201` and one `409`,
with one row, one creation event, and one successful audit; membership
revocation racing create/PATCH/DELETE must prevent commit, following the
existing task-create race pattern. Migration tests must prove unsafe schema-15
rows are fully quarantined before removal, safe rows survive, recurrence is
blocked, fresh schema is 16, and isolated schema-3 upgrade reaches 16.

After focused core, PostgreSQL/router, and migration proof, run repository
standard formatting, native/WASM warnings-denied Clippy, full PostgreSQL
Nextest, migrations CLI, web/Trunk, supply-chain, and container CI at the exact
candidate SHA. Acceptance language must state only metadata CRUD and database
isolation—not Git worktree creation, subagent execution, sandbox isolation, or
completed M5/delegation release gates.

## Non-goals and Stop Conditions

This plan excludes Git commands (`worktree add/remove/prune` and
`check-ref-format`), shells/processes, filesystem checks/canonicalization,
checkout/diff/commit/merge/cleanup, quotas/disk reservation, sandboxd, MCP,
Skills, UI, WebSockets, checkpoints, tools/approval policy, and all M4/M7
changes. It creates no agent/run, starts no subagent, assigns no task,
transitions no task state, adds no status state machine, branch rename/rebase,
head tracking, merge lifecycle, or runtime attribution. It does not accept a
client final path or treat metadata as a host capability.

Stop and open a separate plan if correct behavior needs a trusted repository
registry/root, filesystem inspection/canonicalization, Git/shell/process use,
worktree cleanup, quota enforcement, agent/run creation, delegation/task-state
operation, or agent-runtime attribution. Stop and split if status transitions,
branch rename/rebase, head tracking, merge/checkpoint lifecycle, or WebSocket
fanout becomes necessary. Stop rather than bypass pure validation, transaction
reauthorization, database uniqueness, append-only triggers, or commit-before-
publish ordering.

No production deployment is part of this coding slice. Deployment requires a
verified backup, schema-3-to-16 migration, quarantine inspection,
health/readiness checks, and a rollback/restore procedure; do not deploy an
old binary after schema 16 without explicit backward-compatibility proof.
Real PostgreSQL is required for authoritative migration/integration proof, but
no Git binary, shell, Docker, sandboxd, network, or external peer is a
build/runtime prerequisite. Existing workspace/task/owner-agent rows are a
precondition; because no server agent-provisioning API exists, tests may seed
agents directly and the result must not be advertised as a complete end-user
delegation workflow.
