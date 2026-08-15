# PLAN.md — Final Skills Assertions and External Validation

## Goal

Close the last two static acceptance-test gaps on committed HEAD, then obtain real PostgreSQL 17+pgvector and release-tool evidence for schema-15 Skills.

This is a test/evidence-only batch. Production source, migrations, schema, routes, core, contract docs, dependencies, frontend, and unrelated subsystems remain frozen unless real execution reveals a new defect and a reviewer re-triages scope.

## Current State

### Committed baseline

- Root HEAD is `7496c8d`; it is a test-only commit modifying `crates/gobrowse-server/tests/skills_integration.rs` and `crates/gobrowse-server/tests/postgres_integration.rs`.
- Migrations 0013–0015 and production source are unchanged from `77ed260`; schema remains 15.
- Static review confirms prior authorization, promotion-response, provenance-fixture, denial-audit, and deterministic-race fixes are present.
- Formatting, native/workspace/WASM Clippy, test compilation/discovery, and diff checks pass. No files are staged.
- Checker round five reports **FAIL** for exactly two remaining assertion/numbering gaps plus unavailable external validation. No new production defect was found.

### Remaining gap 1 — privileged success assertions

In `skills_integration.rs:779-839`, workspace OWNER/EDITOR evaluation calls assert HTTP 200 and audit rows but do not assert that evaluation JSON was persisted on each revision.

The same test performs profile OWNER promotion and profile ADMIN rollback but asserts status only. Required exact success audits are missing:

- OWNER promote: action `skill.promoted`, resource type `skill_revision`, resource ID `{skill_id}:2`;
- ADMIN rollback: action `skill.rolled_back`, resource type `skill`, resource ID `{skill_id}`.

### Remaining gap 2 — globally distinct attempted revision numbers

The six-case raw-SQL source matrix uses revision 100 for both nil and wrong-workspace attempts because they target different Skills. The schema-14 post-upgrade matrix uses 10–14, then revision 1 for wrong-workspace on another Skill.

Although different Skills prevent direct uniqueness masking, the required evidence calls for six globally distinct numbers so each logged/asserted case is unambiguous:

- raw-SQL matrix: 100, 101, 102, 103, 104, 105;
- post-upgrade matrix: 10, 11, 12, 13, 14, 15.

All cases must retain exact `skill_revisions_sources_valid` constraint assertions.

### External environment blocker

- `GOBROWSE_TEST_DATABASE_URL` is unset; DB-facing tests return at guards in 0.00s.
- `cargo-nextest`, PostgreSQL client tools, Trunk, cargo-deny, and cargo-audit are unavailable locally. Docker exists but the image build has not run.
- Therefore authorization, provenance, migration repair, audit, concurrency, migration CLI, release web, supply-chain, and container proof remain not run. Guarded test success is not acceptance evidence.

## Relevant Existing Code

- `crates/gobrowse-server/tests/skills_integration.rs::workspace_skill_promotion_and_rollback_require_profile_admin`.
- `skills_integration.rs::skill_database_enforces_evaluation_source_and_promotion_invariants`.
- Test helper `audit_count`, which already supports actor/profile/action/resource-type/exact-resource assertions.
- `crates/gobrowse-server/tests/postgres_integration.rs::schema_v14_to_v15_repairs_skill_lifecycle_state` post-upgrade source matrix.
- Existing focused/full validation commands in `AGENTS.md` and CI.

## Architectural Decisions

1. Modify assertions/fixtures only; do not alter behavior under test.
2. Persisted evaluation proof compares stored JSON with the exact submitted `valid_evaluation(1)` value, not merely `evaluation IS NOT NULL`.
3. Successful privileged audits are matched by actor, profile, action, resource type, exact resource ID, and outcome through `audit_count`.
4. Source-case revision numbers are globally distinct within each six-case test matrix, including the wrong-workspace case on another Skill.
5. Every failed source insert continues to assert the database constraint name, not generic failure.
6. If real PostgreSQL reveals a production or migration defect, stop and obtain reviewer scope approval before touching frozen code.

## Files To Modify

| File | Exact scope |
|---|---|
| `crates/gobrowse-server/tests/skills_integration.rs` | Add persisted evaluation and privileged success-audit assertions; assign raw provenance revisions 100–105. |
| `crates/gobrowse-server/tests/postgres_integration.rs` | Assign post-upgrade provenance revisions 10–15. |
| `docs/implementation-progress.md` | Record results only after real external validation passes. |

No production source or migration change is planned.

## Database/Migration Changes

None. Schema remains 15 and migrations remain unchanged.

## API Changes

None.

## Backend Changes

None.

## Frontend Changes

None.

## Security Requirements

- Workspace OWNER/EDITOR evaluation success must be proven in durable rows, not only responses/audits.
- Profile OWNER promote and profile ADMIN rollback success must have exact attributable audit evidence.
- Provenance failures must be individually identifiable by revision number and exact source constraint.
- No weakened assertion, generic `is_err()`, or catalog-only substitute is accepted.

## Concurrency Requirements

No synchronization changes. Retain all existing Barrier, lock-wait polling, timeout, final-state, and audit assertions.

Real PostgreSQL must execute the five race tests before acceptance.

## Tests Required

### 1. Persist both workspace evaluations

Inside `workspace_skill_promotion_and_rollback_require_profile_admin`, after each successful evaluate response:

1. Parse `evaluated["id"]` as `Uuid`.
2. Query `SELECT evaluation FROM skill_revisions WHERE id=$1`.
3. Assert the returned `serde_json::Value` equals `valid_evaluation(1)` exactly.
4. Retain the existing exact `skill.evaluated` audit assertion for that revision ID.

This must run once for the MEMBER+workspace OWNER revision and once for the MEMBER+workspace EDITOR revision.

### 2. Assert privileged success audits

After profile OWNER promotes revision 2:

```text
actor = profile owner user ID
profile = fixture profile ID
action = skill.promoted
resource_type = skill_revision
resource_id = "{skill_id}:2"
expected count = 1
```

After profile ADMIN rolls back to revision 1:

```text
actor = profile admin user ID
profile = fixture profile ID
action = skill.rolled_back
resource_type = skill
resource_id = skill_id string
expected count = 1
```

Keep the successful response assertions and final active/promoted state checks. If no final state check exists, add `(active_revision,promoted_revision) == (1,1)` after rollback.

### 3. Raw-SQL six-case numbering

In `skill_database_enforces_evaluation_source_and_promotion_invariants`, use exactly:

| Revision | Invalid source array |
|---:|---|
| 100 | `[Uuid::nil()]` |
| 101 | `[valid, valid]` |
| 102 | `[missing]` |
| 103 | `[deleted]` |
| 104 | `[cross_profile]` |
| 105 | `[wrong_workspace_conversation]` on the scoped Skill |

Do not reuse 100 for the wrong-workspace Skill. Each result must assert constraint `skill_revisions_sources_valid`.

### 4. Schema-14 post-upgrade six-case numbering

After applying migration 0015, use exactly:

| Revision | Invalid source array |
|---:|---|
| 10 | nil |
| 11 | non-nil duplicate |
| 12 | missing |
| 13 | deleted |
| 14 | cross-profile |
| 15 | wrong-workspace on the post-upgrade scoped Skill |

Retain exact `skill_revisions_sources_valid` assertions for all six.

### 5. Focused real execution

Run these first with a real PostgreSQL 17+pgvector URL:

```bash
GOBROWSE_TEST_DATABASE_URL=postgres://gobrowse:test-only-password@localhost:5432/gobrowse_test CARGO_BUILD_JOBS=1 cargo nextest run -p gobrowse-server --test skills_integration -E 'test(workspace_skill_promotion_and_rollback_require_profile_admin) or test(skill_database_enforces_evaluation_source_and_promotion_invariants)'
GOBROWSE_TEST_DATABASE_URL=postgres://gobrowse:test-only-password@localhost:5432/gobrowse_test CARGO_BUILD_JOBS=1 cargo nextest run -p gobrowse-server --test postgres_integration -E 'test(schema_v14_to_v15_repairs_skill_lifecycle_state)'
```

Then run the full acceptance matrix:

```bash
cargo fmt --all -- --check
CARGO_BUILD_JOBS=1 cargo clippy -p gobrowse-server --all-targets --all-features -- -D warnings
CARGO_BUILD_JOBS=1 cargo clippy --workspace --all-targets --all-features -- -D warnings
CARGO_BUILD_JOBS=1 cargo clippy -p gobrowse-web --target wasm32-unknown-unknown -- -D warnings
GOBROWSE_TEST_DATABASE_URL=postgres://gobrowse:test-only-password@localhost:5432/gobrowse_test CARGO_BUILD_JOBS=1 cargo nextest run -p gobrowse-server --test skills_integration
GOBROWSE_TEST_DATABASE_URL=postgres://gobrowse:test-only-password@localhost:5432/gobrowse_test CARGO_BUILD_JOBS=1 cargo nextest run -p gobrowse-server --test postgres_integration
GOBROWSE_TEST_DATABASE_URL=postgres://gobrowse:test-only-password@localhost:5432/gobrowse_test CARGO_BUILD_JOBS=1 cargo nextest run --workspace
GOBROWSE__DATABASE__URL=postgres://gobrowse:test-only-password@localhost:5432/gobrowse_test CARGO_BUILD_JOBS=1 cargo run -p gobrowse-server --bin gobrowse -- migrate
(cd crates/gobrowse-web && trunk build index.html --release --dist ../../dist)
cargo deny check
cargo audit --ignore RUSTSEC-2023-0071 --ignore RUSTSEC-2024-0436 --ignore RUSTSEC-2026-0173
docker build -t gobrowse-os:skills-final-evidence .
git diff --check 7496c8d..HEAD
git diff --cached --name-only
git status --short
```

Record focused/full nextest summaries, test durations, schema 15, migration CLI, Trunk, deny, audit, and Docker results. Guarded 0.00s tests are explicitly rejected.

## Deployment Considerations

- No production or schema change is planned.
- Do not deploy without real database and release-tool evidence.
- Follow existing backup/stop/migrate/start/restore procedures.
- Exclude unrelated unstaged drift from release packaging.

## Ordered Implementation Steps

1. Preserve unrelated drift and confirm the index is empty.
2. Add exact persisted evaluation assertions for workspace OWNER/EDITOR.
3. Add exact profile OWNER promote and profile ADMIN rollback audit assertions plus final `(1,1)` state.
4. Change wrong-workspace raw-SQL attempted revision from 100 to 105.
5. Change wrong-workspace post-upgrade attempted revision from 1 to 15.
6. Run the three focused tests against real PostgreSQL and fix test-only failures.
7. Run focused binaries, full workspace nextest, migration CLI, Trunk, deny, audit, and Docker build.
8. Update implementation progress with observed totals/durations only.
9. Obtain required security, migration, concurrency/reliability, QA, and API review; rerun after findings.

## Acceptance Criteria

- Both workspace OWNER/EDITOR evaluation calls persist exactly the submitted evaluation JSON and retain exact evaluation audits.
- Profile OWNER promote and profile ADMIN rollback each produce exactly one expected success audit with exact resource ID.
- Final rollback state is active/promoted revision 1.
- Raw-SQL provenance cases use revisions 100–105 exactly once each and assert the source constraint.
- Post-upgrade provenance cases use revisions 10–15 exactly once each and assert the source constraint.
- No production source, migration, schema, route, dependency, frontend, or unrelated subsystem changes are included.
- Focused/full nextest, schema-15 migration CLI, Trunk release build, deny, audit, and Docker build pass in an approved environment with recorded non-guarded durations.
- Required reviewers report no unresolved blocker or high-severity finding.
