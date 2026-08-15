# PLAN.md — Skills Schema-15 Acceptance Attestation and Documentation Closure

## Goal

Close the documentation gap left by commit `8823dac` by committing this authoritative PLAN attestation on its own. The release-owner authorization, live inventory, and implementation-progress evidence are already recorded; no application content changes remain.

No application source, tests, migrations, schema, APIs, frontend, sandbox, dependencies, or deployment state may change in this batch.

## Current State

### Green exact-SHA CI

Observed GitHub Actions evidence for commit `4d0b0b30ace33e9f3bd807883575742f6ae094da`, run `31858799443`:

- Overall workflow conclusion: success.
- Rust job `94948272589`: success.
- Nextest summary: 307 tests run, 307 passed, 6 configured skipped, duration 55.070s.
- `schema_v14_to_v15_repairs_skill_lifecycle_state`: passed in 0.443s.
- `deployed_schema_v3_upgrades_to_v15`: passed in 52.463s.
- `automatic_promotion_requires_recorded_non_regression`: passed in 0.773s.
- `duplicate_and_inaccessible_skill_sources_return_validation`: passed in 0.725s.
- `skills_reject_cross_profile_workspace_links_and_duplicate_globals`: passed in 0.146s.
- `workspace_skill_promotion_and_rollback_require_profile_admin`: passed in 0.757s.
- `skill_database_enforces_evaluation_source_and_promotion_invariants`: passed in 15.259s.
- Migration CLI step completed successfully and logged `migrations applied`.
- Web/Trunk job `94948272573`: success.
- Supply-chain deny/audit job `94948272638`: success.
- Container job `94948272578`: success.

All previously reported code, fixture, migration, authorization, audit, provenance, and release-tool blockers are closed for this exact SHA.

Commit `8823dac3ea793373d5c466692bea1097232562da` (`docs: record skills schema15 acceptance evidence`) committed the required nine-line `docs/implementation-progress.md` closure record but omitted the already-prepared `PLAN.md` attestation. The current PLAN diff is therefore the sole remaining closure artifact.

### Release-owner authorization

The user/release owner explicitly authorized the in-place pre-release repair of migration 0015. Repository tags stop at `milestone-7`; the schema-15 repair commit postdates that release line. `docs/implementation-progress.md` identifies the private deployment at `root@178.128.179.216` as the sole known supported deployment and records it at schema 3.

### Read-only live inventory attestation

Read-only SSH and SQL inventory was executed on 2026-08-15 without changing containers, data, migration rows, or service state.

Observed deployment identity:

```text
host = gobrowseos (178.128.179.216)
SSH user = root
inventory UTC = 2026-08-15T02:26:06Z
app container = gobrowse-os-app-1, healthy
PostgreSQL container = gobrowse-os-postgres-1, pgvector/pgvector:0.8.1-pg17, healthy
application database = gobrowse
PostgreSQL server = 17.8
```

Observed application schema:

```text
schema_metadata.schema_version = 3
schema_metadata.updated_at = 2026-08-11T06:59:24.853223Z
_sqlx_migrations successful versions = 1, 2, 3
_sqlx_migrations max(version) = 3
_sqlx_migrations rows where version=15 = 0
```

The only other connectable database is the standard administrative `postgres` database; read-only catalog checks show it has neither `public._sqlx_migrations` nor `public.schema_metadata`.

Checksum evidence:

```text
former 0015 file SHA-256 = b05865e0503b0d43836d622f7917f5972bba260a81e3009c50a334e13c7646b4
former SQLx SHA-384 checksum = bd6b27777a076a077e23763b87a1effec21c1a3a8dbadd714f958e7ef2ab8508bbb3ed6b37a92c041d4ab4aabfd89d40
current 0015 file SHA-256 = be16df41b493d1a40f883d6abe5a460ffb50381bdf325fc9e60f658b5f3547dd
current SQLx SHA-384 checksum = f63824fd941dd823afccd7344809c4379de8085380bf58be16dbc24763b1af92c8e0ab0126e8ace9a6248fbae35e329e
live version-15 rows = 0
live former-checksum version-15 rows = 0
live current-checksum version-15 rows = 0
```

The absence of any version-15 ledger row is the controlling evidence: neither the former nor current 0015 was applied. Combined with the owner's supported-estate declaration and explicit authorization, the in-place pre-release repair is safe for the declared supported estate. The prior release-attestation blocker is closed.

## Relevant Existing Code

- `crates/gobrowse-server/migrations/0015_skill_lifecycle_hardening.sql`: accepted current migration, unchanged in this batch.
- `crates/gobrowse-server/src/db.rs:14-37`: SQLx migration ledger/checksum enforcement and advisory lock.
- `docs/implementation-progress.md`: authoritative deployment and validation handoff record.
- `docs/deployment.md:23-32`: backup/stop/migrate/restore procedure for any future live schema upgrade.
- `.github/workflows/ci.yml`: exact CI acceptance workflow.

## Architectural Decisions

1. Treat the user's explicit authorization plus the sole-supported-deployment inventory as the release-owner attestation.
2. Use `version=15` ledger absence as primary proof; SHA-256 identifies repository files while SQLx stores SHA-384 migration checksums.
3. Record evidence in durable repository documentation without altering code or migration content.
4. Do not migrate the live schema-3 deployment as part of documentation closure.
5. Do not claim that CI deploys production; it proves the candidate against ephemeral PostgreSQL 17+pgvector.
6. Preserve unrelated working-tree drift and commit only `PLAN.md` after review.

## Files To Modify

| File | Exact scope |
|---|---|
| `PLAN.md` | Commit the evidence-backed attestation and closure checklist omitted from `8823dac`. |

`docs/implementation-progress.md` is already committed in `8823dac`; it must not be amended in this follow-up. No other file is in scope.

## Database/Migration Changes

None. The SSH/SQL inventory was read-only. Live schema remains 3; candidate schema remains 15.

## API Changes

None.

## Backend Changes

None.

## Frontend Changes

None.

## Security Requirements

- Do not commit credentials, environment values, SSH keys, database URLs, or container secrets.
- Record only host identity, non-secret container/image names, schema/ledger metadata, checksums, and CI identifiers.
- Preserve the live service and database without writes or restarts.
- Require reviewer confirmation that the attestation is bounded to the owner-declared supported estate.

## Concurrency Requirements

None. No runtime or database write occurs in this documentation batch.

## Tests Required

No new tests. Existing acceptance evidence is the green exact-SHA CI run `31858799443`.

Documentation validation:

```bash
cargo fmt --all -- --check
git diff --check
git diff --name-only
git diff --cached --name-only
git status --short
```

Before committing, verify the staged set contains only:

```text
PLAN.md
```

No need to rerun CI for a plan-only evidence commit unless repository policy or reviewer requests it.

## Deployment Considerations

- The private deployment remains healthy at schema 3; no schema-15 deployment occurred during attestation.
- A future deployment must independently follow backup → stop old app → migrate with target image → verify schema/quarantine → start → readiness checks.
- The inventory establishes checksum compatibility for that future 3→15 path; it is not authorization to bypass the deployment runbook.
- Any newly discovered supported database outside the owner-declared inventory must be checked before migration.

## Ordered Implementation Steps

1. Preserve `.gitignore`, `AGENTS.md`, deleted setup script, and untracked output/session drift.
2. Verify `8823dac` contains only the intended `docs/implementation-progress.md` acceptance record.
3. Review the existing PLAN attestation for consistency with committed progress evidence; make no unrelated content changes.
4. Run documentation diff/whitespace checks and inspect for secrets.
5. Obtain reviewer approval of the PLAN-only closure diff.
6. Stage only `PLAN.md` and verify the cached name/status and diff.
7. Commit with a documentation-only conventional message such as `docs: commit skills schema15 closure plan`.
8. Confirm the new commit contains only `PLAN.md`, the index is empty, and unrelated drift is unchanged.

## Acceptance Criteria

- Committed `docs/implementation-progress.md` at `8823dac` contains the exact green CI SHA/run/jobs and test/migration results.
- The committed progress record and PLAN record the owner's explicit authorization and declared sole supported deployment.
- PLAN records live schema 3, successful migration ledger versions 1–3, and zero version-15 rows.
- PLAN distinguishes file SHA-256 values from SQLx SHA-384 checksums.
- PLAN states that the inventory was read-only and no deployment occurred.
- No application source, test, migration, schema, configuration, dependency, or unrelated documentation changes are included.
- Reviewers confirm no remaining blocker or high-severity finding.
- The final plan-closure commit contains only `PLAN.md`; `8823dac` remains the separate progress-documentation commit.
