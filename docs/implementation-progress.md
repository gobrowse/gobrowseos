# Implementation Progress

This file is the durable handoff record for work after the `milestone-1` tag at commit `c49d3a6`.

## Current Continuation Checkpoint (2026-08-14)

- **Active milestone:** production hardening of the Tasks/Activity Ledger and webhook scheduler; this is not a release completion claim.
- **Completed in this checkpoint:** profile-scoped session rotation; MCP wire-version selection; fenced outbound-webhook delivery leases; workspace-scoped task CRUD/state transitions; durable activity replay; database-enforced task/agent/dependency tenancy; activity and repair-quarantine append-only triggers; restricted human-attributed activity notes.
- **Migrations:** `0009_webhook_delivery_fencing.sql` through `0012_webhook_lease_check.sql`; expected schema version is now **12**. The remote private deployment remains at schema version **3** and has not received these changes.
- **Validation:** `cargo fmt --all -- --check` and `CARGO_BUILD_JOBS=1 cargo clippy --workspace --all-targets --all-features -- -D warnings` pass locally. `cargo-nextest` is unavailable locally, and database-backed integration tests therefore remain pending CI/throwaway PostgreSQL execution. No release or deployment claim is made from mock/compile-only coverage.
- **Security/reliability review:** fixed cross-profile ADMIN/OWNER session rotation, stale webhook outcome writes, task/activity cross-workspace links, immutable-ledger forgery via public lifecycle events, activity cursor commit ordering, migration self-parent upgrade safety, and malformed non-running webhook leases. Full live-PostgreSQL validation remains required.
- **Remote storage checkpoint:** Docker `data-root` was safely migrated to `/mnt/volume_1786741375931/gobrowse/docker-data`; a SHA-256-verified pre-migration PostgreSQL dump is on the same persistent volume. The prior `/var/lib/docker` copy is retained for rollback. App health, PostgreSQL schema 3, `gobrowse doctor`, and `gobrowse security audit` were verified afterward. Expected vault/TLS configuration warnings remain. `/opt/gobrowse-os` is a deployment source snapshot without `.git`, so remote Git commit verification/deployment is pending a Git-backed checkout on the persistent volume.
- **Active worktrees:** integration `agent/orchestrator-mcp`; specialist branches under `/home/mateo/Downloads/gobrowseos-worktrees/` are retained as review evidence. Coding worktrees used isolated branches.
- **Next task:** run the complete CI matrix with real PostgreSQL, then implement the next reviewed release blocker (Skills revision/promotion API and database integrity) without enabling sandboxd or untrusted MCP execution.

## Release State

- Milestone 1 and Milestone 2 remain tagged and their backups are retained. Milestone 3 implementation commit `7ee7ff6` is deployed privately and verified.
- Public exposure remains disabled. The application binds to `127.0.0.1:8080` in the deployment.
- The pre-migration backup is `/opt/gobrowse-os/backups/pre-milestone-2.dump`; SHA-256 is `5977c99937699233650b91deb0c848baa44976f31f063a9b44a99138aa00ad26`.
- The retained Milestone 2 image digest is `sha256:c2c060cf38804a7c58d1c1400fb4118917acc6b16f449ed80d0468f5b845d606`; its released schema version was 2.
- The pre-Milestone 3 backup is `/opt/gobrowse-os/backups/pre-milestone-3.dump`; SHA-256 is `1f08fa30439e613ee34a6562a4bf83274e8afd32df0ca4c40d091609e44727ab`.
- The deployed Milestone 3 image digest is `sha256:448386209a27c1c700fd9c592de8325e45aea677d281497e12a6bbf6e277038a`; the live schema version is 3.

## Completed Milestone

Milestone 3 adds durable streamed chat: dynamic model routes, bounded neutral provider streams, idempotent atomic turns, leased multi-instance execution, fenced ordered events, cancellation and takeover, authorized context assembly, run-scoped realtime replay, and transcript/model controls in the operator UI.

## Decisions

- Store embeddings in `book_chunk_embeddings`, keyed by chunk and embedding model. Never overwrite vectors from one model with vectors from another model.
- Claim background work with unique expiring lease tokens. A process crash must make work reclaimable without operator intervention.
- Associate jobs with a Book revision. Workers cancel stale jobs instead of publishing vectors for obsolete content.
- Apply authorization predicates before lexical or vector ranking. Workspace search includes authorized profile/global/user Books rather than requiring an exact workspace match.
- Encrypt provider credentials before enabling authenticated provider calls. Each value uses AES-256-GCM with a random data-encryption key wrapped by an externally supplied master key, versioned AAD, and database-enforced wrapping-nonce uniqueness.
- Bind credentials to explicit provider hosts, pin validated DNS resolutions, and prohibit metadata/link-local destinations. Private endpoints require credential-free Ollama and the explicit local-embeddings feature.
- Rotate vault keys by configuring current and immediately previous key versions, then transactionally rewrapping profile credentials through the redacted API.
- Conversation Books are projections of durable messages and are deleted with their source conversation.
- Chat providers emit neutral incremental events from bounded Ollama NDJSON or OpenAI-compatible SSE streams. Fallbacks are explicit ordered routes and apply only before a provider stream is accepted.
- Conversation runs are claimed by bounded periodic workers with `FOR UPDATE SKIP LOCKED`, expiring execution tokens, persistent attempt counters, and token-fenced authoritative writes. Canceled abandoned work is reaped; graceful shutdown releases leases; takeover emits a mandatory durable output reset.
- Durable run-event inserts take a profile-scoped transaction advisory lock so sequence order also represents commit order and replay cursors cannot skip a late commit.
- Browser turns are one conversation-locked, idempotent transaction keyed by a client UUID and request fingerprint. Concurrent distinct turns reject before creating an orphan message.
- Personal conversations are creator-scoped. Workspace conversations, Books, runs, polling, and realtime delivery require explicit membership; execution and completion revalidate authorization before provider transmission and publication.
- Database lock order is event stream, conversation, run, projection Book, then embedding job, as recorded in ADR 0011.

## Verification Log

- 2026-08-09: Milestone 1 baseline passed `cargo fmt --all -- --check`, Clippy with warnings denied, and 26/26 nextest tests with PostgreSQL and pgvector.
- 2026-08-10: Milestone 2 passed native and WASM Clippy with warnings denied and 33/33 nextest tests. The suite includes dirty-v1 migration, vault encryption/rotation, scope/role denial, conversation projection/fork/delete, stale Autobiography proposals, leased embedding completion, SSRF denial, and semantic retrieval through a live fake provider.
- 2026-08-10: The release WASM bundle and a non-root, statically linked smoke image passed `/health/ready` and reported schema version 2. Local image digest: `sha256:c2c060cf38804a7c58d1c1400fb4118917acc6b16f449ed80d0468f5b845d606`.
- 2026-08-10: Private deployment passed health, version, backup integrity, non-root image, queue, pgvector, Git, and static-asset checks. Owner setup remains unclaimed; the vault warning is expected until an external master key is configured.
- 2026-08-10: Milestone 3 passed native and WASM Clippy with warnings denied, a release Trunk/WASM build, and 40/40 tests against fresh PostgreSQL 17 + pgvector. Coverage includes explicit fallback, Unicode-safe NDJSON/SSE parsing and event coalescing, simultaneous idempotent/distinct submissions, two-worker graceful takeover, stale fencing, mandatory output reset, cancellation/completion races, cancellation reaping, profile commit ordering, workspace collaborator visibility and personal authorization denials, restricted-context exclusion, Milestone 2 configuration compatibility, schema-2-to-3 upgrade, and cursor-based WebSocket reconnect.
- 2026-08-10: Local headless-Chrome validation passed owner login, conversation creation/opening, the transcript composer, provider registry, and responsive 1440px/390px layouts against an isolated schema-3 server. The deployed Milestone 2 service was not changed.
- 2026-08-10: Stabilized Milestone 3 repeated the 40/40 fresh-database suite, native/WASM Clippy, release Trunk build, desktop/mobile browser flow, `cargo deny --offline check`, and offline `cargo audit`. Deny reported allowed dependency-duplication warnings; audit reported the allowed transitive unmaintained `proc-macro-error2` warning and no unignored vulnerability.
- 2026-08-10: The update runbook now stops the previous app before schema migration and requires database restore before old-image rollback; this prevents Milestone 2 writes during the schema-3 transition.
- 2026-08-10: The reviewed Milestone 3 implementation was committed as `7ee7ff6`; closure review found no remaining high- or medium-severity application findings.
- 2026-08-11: The private schema-3 deployment passed backup verification, migration/backfill integrity, health/readiness, doctor, static UI, unauthenticated API denial, loopback-only publication, read-only root filesystem, non-root identity, dropped capabilities, and resource checks. The empty deployment remains unclaimed; authenticated chat/provider flows were verified locally without creating production identity or provider state.

## Skills/schema-15 acceptance closure (2026-08-15)

- The Skills/schema-15 candidate was accepted at commit `4d0b0b30ace33e9f3bd807883575742f6ae094da` (short SHA `4d0b0b3`) by successful CI run `31858799443`. The Rust job `94948272589`, Web/Trunk job `94948272573`, supply-chain deny/audit job `94948272638`, and container job `94948272578` all succeeded. Nextest reported 307 tests run, 307 passed, and 6 configured skipped; the migration CLI completed successfully with `migrations applied`.
- CI specifically passed `schema_v14_to_v15_repairs_skill_lifecycle_state` (0.443s), `deployed_schema_v3_upgrades_to_v15` (52.463s), `automatic_promotion_requires_recorded_non_regression` (0.773s), `duplicate_and_inaccessible_skill_sources_return_validation` (0.725s), `skills_reject_cross_profile_workspace_links_and_duplicate_globals` (0.146s), `workspace_skill_promotion_and_rollback_require_profile_admin` (0.757s), and `skill_database_enforces_evaluation_source_and_promotion_invariants` (15.259s).
- The release owner explicitly authorized the in-place pre-release repair of migration `0015`. The owner-declared private deployment at `root@178.128.179.216` is the sole known supported deployment. This authorization is bounded to that declared estate and does not claim that CI migrated production.
- Read-only SSH and SQL inventory on 2026-08-15 at `2026-08-15T02:26:06Z` observed healthy `gobrowse-os-app-1` and `gobrowse-os-postgres-1` containers (`pgvector/pgvector:0.8.1-pg17`), application database `gobrowse`, and PostgreSQL 17.8. `schema_metadata.schema_version` was 3; successful `_sqlx_migrations` versions were 1, 2, and 3; `max(version)` was 3; and rows where `version = 15` were 0.
- The live database had zero rows for version 15 under either the former or current migration checksum. For traceability, the former `0015` file SHA-256 was `b05865e0503b0d43836d622f7917f5972bba260a81e3009c50a334e13c7646b4` and SQLx SHA-384 was `bd6b27777a076a077e23763b87a1effec21c1a3a8dbadd714f958e7ef2ab8508bbb3ed6b37a92c041d4ab4aabfd89d40`; the current file SHA-256 is `be16df41b493d1a40f883d6abe5a460ffb50381bdf325fc9e60f658b5f3547dd` and SQLx SHA-384 is `f63824fd941dd823afccd7344809c4379de8085380bf58be16dbc24763b1af92c8e0ab0126e8ace9a6248fbae35e329e`. The only other connectable database, administrative `postgres`, has neither `public._sqlx_migrations` nor `public.schema_metadata`.
- This inventory was read-only: production was not migrated, and no live container restart, data mutation, migration-ledger write, or service-state change occurred. The live deployment remains at schema 3; schema 15 is proven only by the exact-SHA CI candidate and remains for a future deployment runbook execution.

## M8A offline vault-readiness acceptance (2026-08-15)

- The bounded M8A slice was accepted at commit `5eb07151fb36c761d13b9d5943394f1b7a11e4f8` by successful CI run `31919742949`: Rust `95097548607`, web `95097548566`, supply-chain `95097548454`, and container `95097548439`.
- Acceptance is limited to the exact MCP OAuth vault-purpose and raw one-host metadata policy, redacted actual vault-key readiness, a bounded count-only read-only MCP metadata doctor, and router/PostgreSQL proof.
- This is not M8 closure. It does not prove OAuth, JWKS, discovery, refresh, same-profile reference migration, vault-backed PKCE remodeling, real-provider behavior, or M7 peer interoperability. M8 remains `IN_PROGRESS`; M4 and M7 remain independently `BLOCKED_EXTERNAL`.


## Active Milestone

- The honest final sweep is complete: only real-external-runtime gates remain and the user-excluded areas (browser, WebAuthn/OIDC) are out of scope. The integration suite is now DETERMINISTIC across the full workspace: all DB-touching tests serialize via a shared PostgreSQL advisory lock (`tests/common/mod.rs`), eliminating cross-process row races. 199 nextest pass, 5 skipped (opt-in docker-backed sandbox tests under `GOBROWSE_SANDBOX_DOCKER=1`). Only migrations left out of a fresh database are forward-only `0001`–`0008`. `cargo fmt`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo clippy -p gobrowse-web --target wasm32-unknown-unknown -- -D warnings`, `cargo nextest run --workspace`, `cargo deny check`, `cargo audit` (with the documented ignores), and `(cd crates/gobrowse-web && trunk build index.html --release --dist ../../dist)` are all green.
- Webhook delivery lease recovery (`recover_stuck_deliveries` in `webhook_scheduler.rs`): a crashed worker's in-flight `webhook_deliveries` row (status='running', expired `lease_expires_at`) is dead-lettered at max attempts or re-queued otherwise; mirrors `embedding.rs:340/358`. Schema bumped to 8 via `0008_webhook_delivery_lease.sql`.
- `TaskState::can_transition_to` (`activity.rs`) is now exhaustively covered: all 64 `(from→to)` pairs assert against a hand-transcribed allowed set, terminals reject everything, no self-transitions, non-terminals can cancel, `Failed→Ready` retry and `Running→Review/Done/Failed/Blocked` proceed.
- Outbound webhook scheduler (M6): `FOR UPDATE SKIP LOCKED` claim loop, HMAC-SHA256 over a canonical payload, exponential backoff, attempt counters, dead-letter after max; gated behind `features.webhook_scheduler_enabled` (default off, release-gated), schema v7 (`0007_webhook_scheduler.sql`).
- M4 runtime-backed adversarial boundary gate proven against a REAL container runtime via a `/tmp/podman`→`docker` shim (test infra, not committed): private PID namespace, read-only rootfs, `CapEff: 0000000000000000`, no host-mount escape, `--network=none` denies metadata/private egress. The 5 `docker_backed_*` tests are `#[ignore]` by default and run with `cargo nextest run -p gobrowse-sandboxd --run-ignored only` under `GOBROWSE_SANDBOX_DOCKER=1`.
- MCP doctor pure audience-binding validator: `validate_audience_binding(target, allowed_audiences)` rejects private/metadata/loopback targets and unlisted hosts; reuses `sandbox::is_public_destination` for IP rules; DNS names are allowlisted (real OAuth/JWKS remains release-gated per the mcp.rs banner).
- Earlier M5 hardening: skills evidence guard (`attempts > 0` before auto-promote); branch sanitizer (`git check-ref-format` parity); audit append-only DB trigger (schema v6); agent takeover-fence DB suite; argon2 anti-downgrade + session absolute-timeout.
- `audit_events` is append-only, so tests rely on FK cascades; no `DELETE` of audit rows.

## Remaining gates (genuinely NOT provable in this environment despite maximal simulation)
- **Real Podman-specific userns** (`--userns=keep-id` rootless): the docker shim strips it; only real Podman can prove the rootless UID mapping (and rootless setup requires persistent `/etc/subuid`+`/etc/subgid` config, which the no-permanent-changes constraint forbids). Sandbox HARDENING is proven via docker; the Podman-specific USERNS CLAIM is not.
- **MCP OAuth matrix / real-server conformance / JWKS token validation** — needs real OIDC IdPs (mcp.rs banner); building a mock-IdP + JWKS client would be net-new product dressed as evidence, excluded as dishonest.
- **OIDC/WebAuthn auth** — user-excluded; needs real providers.
- **Browser/connectors/media adapter tests** — user-excluded; needs real browser/adapter runtimes.

## Compose cold-start smoke (proven ephemerally 2026-08-12)
- `docker compose -p gobrowse-smoke up --build -d` with `GOBROWSE_PORT=8181` (isolated project name + port override) on the running docker daemon: the full Dockerfile (Trunk WASM release build + `cargo build --release -p gobrowse-server`) succeeded, both containers started, `/health/ready` returned `{"status":"ready","version":"0.1.0"}`, and `/api/v1/auth/me` returned `401` (unauthenticated API denial). The smoke project was then `down -v`'d (volume + networks removed); the pre-existing deployment on port 8080 was untouched throughout. The smoke image + build cache were pruned afterward (~7.5 GB reclaimed, disk returned to 21G free). This closes the "Config/health/migrations: Compose cold-start" gate row.

## Completed Milestone (auth/webhook/MCP subset)

- Login and owner-setup throttling persists attempts with a uniform Unauthorized response (per `docs/threat-model.md` "uniform login failures"); schema bumped to 4 via `0004_login_attempts.sql`.
- Webhook HMAC-SHA256 signature verification (manual RFC-2104, no `hmac` dependency) and replay idempotency via `webhook_deliveries` PK; webhooks excluded from the CSRF origin guard because they use signature auth; schema bumped to 5 via `0005_webhooks.sql`.
- Session rotation invalidates all prior sessions through `users.auth_epoch + 1` (column and enforcement present since `0001_initial.sql`); disabled users cannot log in; non-admins cannot rotate others.
- CSRF closure: `origin_guard` now rejects missing Origin on state-changing methods and `Sec-Fetch-Site: cross-site`; WebSocket `validate_origin` rejects empty/mismatched Origin and wrong-scheme; latent no-Origin requests in two existing test helpers were corrected.
- MCP doctor pure-logic validator flags remote/dynamic `$ref`, oversized (>256 KiB), and depth-bombed tool schemas in `gobrowse-core/src/mcp.rs`, reusing the existing `McpDoctorReport`/`DiagnosticStatus`. OAuth matrix and real-server conformance remain release-gated.

## Webhook SSRF proof acceptance closure (2026-08-15)

- The final webhook SSRF proof assertions were accepted at commit `dfbbaa5b8e6e884010cbd4f2e57b60a8253c9edc` (short SHA `dfbbaa5`) by successful exact-SHA CI run `31864560017`. Every CI job succeeded: Rust `94963435928`, Web/Trunk `94963435902`, supply-chain `94963435920`, and container `94963435897`.
- The Rust job passed format, native Clippy, WASM Clippy, the real-PostgreSQL Nextest suite, and migrations. Nextest reported **324/324 tests passed** (324 run, 6 configured skipped); the migration CLI completed with `migrations applied`.
- Focused proof tests passed: `outbound_http::tests::trust_material_verifies_now_and_five_years_forward` (current and five-calendar-year TLS verification), `outbound_http::tests::wrong_hostname_certificate_is_a_redacted_transport_failure` (exact `TransportError::Connection`, stable code `target_connection_failed`, and no hostname disclosure), and `webhook_scheduler_integration::stale_worker_cannot_persist_after_recovery_and_reclaim` (current-owner success clears both `lease_token` and `lease_expires_at`). `webhook_scheduler_integration::webhook_scheduler_is_disabled_by_default` also passed.
- Supply-chain checker outcome was clean: `cargo deny check` reported `advisories ok, bans ok, licenses ok, sources ok`; its only output was the existing dependency-duplication warnings. `cargo audit --ignore RUSTSEC-2023-0071 --ignore RUSTSEC-2024-0436 --ignore RUSTSEC-2026-0173` succeeded with no unignored vulnerability. Web/Trunk and the container build also succeeded.
- `features.webhook_scheduler_enabled` remains default-off, as proved by the focused default-off test. This acceptance run was CI validation only: no production deployment, live migration, restart, or production state change occurred; scheduler release remains separately gated.

## M7 typed-handler fallback correction (2026-08-15)

- Commit `ca1fe7360c05ef99f4f73b7273999c942e8d3046` (`fix(mcp): redact invalid typed handler results`) corrects the MCP dispatcher’s classification of a semantically invalid typed handler result. Duplicate typed `Tool` identities now yield the redacted internal handler fallback: JSON-RPC `-32603`, `internal MCP handler error`, no error data, preserved request ID, and a bounded encodable response.
- The correction added the focused `semantically_invalid_typed_handler_result_uses_internal_fallback` regression and changed only `crates/gobrowse-core/src/mcp/server.rs` plus its current correction plan. It preserves intentionally returned handler `RpcError` values and adds no transport, runtime, credential, database, route, migration, frontend, or deployment behavior.
- Independent review reported PASS with no blocker. Exact-SHA CI run `31903353191` passed: Rust (`95057374683`, including format, native/WASM Clippy, Nextest, and migrations), web/Trunk (`95057374702`), supply-chain (`95057374705`), and container (`95057374736`). The workflow emitted only GitHub’s Node.js 20 action-runtime deprecation notices.
- This accepts the bounded correction only. M7 remains active: the deferred exhaustive core matrix and real stdio/Streamable HTTP interoperability gate are not complete.

## M7 resource-payload correction acceptance (2026-08-15)

- Commit `a9b603735b720ee4dfc7e26278f0246f309d9ee2` accepts the MCP resource-payload correction: resource payload variants are mutually exclusive, with three wire-regression tests covering the corrected encoding.
- Independent checker outcome: PASS. Exact-SHA CI run `31904514287` passed all required jobs: Rust `95060146421`, supply-chain `95060146448`, web `95060146474`, and container `95060146476`.
- This accepts only the bounded resource-payload correction. M7 remains incomplete pending exhaustive core evidence and real stdio plus Streamable HTTP interoperability.

## M7 negotiation-handler failure-boundary correction (2026-08-15)

- Commit `f1df7983d672703555e0afaec6c05fe0a6a489d2` corrects invalid handler-produced `DiscoverResult` and `InitializeResult` classification: they now produce the redacted internal JSON-RPC fallback (`-32603`, `internal MCP handler error`, no data) rather than caller-invalid-parameters (`-32602`), while preserving the request ID and existing handler/protocol error behavior.
- Independent checker outcome: PASS. Exact-SHA CI run `31905108689` passed all required jobs: Rust `95061580890`, web `95061580770`, supply-chain `95061580835`, and container `95061580892`.
- This accepts only the negotiation-handler failure-boundary correction. M7 remains open pending the exhaustive core matrix and real stdio/Streamable HTTP interoperability proof.

## M7 bounded stdio framing acceptance (2026-08-15)

- Commit `4c2bff3796ec7ac15b86a167d49828a1eb556b98` accepts bounded JSON-RPC stdio line framing over caller-provided Tokio async pipes. Exact-SHA CI run `31908353273` passed Rust `95069542559`, web `95069542603`, supply-chain `95069542548`, and container `95069542574`.
- This is framing-only evidence and its bounded tests, not a claim of process management, session establishment, or real MCP interoperability. M7 remains incomplete.
- The sole remaining M7 gate is `BLOCKED_EXTERNAL`: an approved immutable/pinned independently implemented MCP stdio peer, with provenance, version, and content hash recorded and verified, plus a non-ignored CI real-peer test that performs `server/discover` and capability-authorized `tools/list`. Fixtures, mocks, loopback peers, and in-repository substitutes do not satisfy the gate.

## M4 rootless Podman `keep-id` external gate (2026-08-15)

- **`BLOCKED_EXTERNAL`:** M4 has no real runtime-proof success claim. The required approved ephemeral local runner is absent: it must run real rootless Podman as a non-root user, already have that user's `/etc/subuid` and `/etc/subgid` entries configured, hold the approved immutable digest image locally, and back a trusted required non-ignored CI job.
- No Docker shim, remote or rootful Podman fallback, image pull, installation, `sudo`, or host mutation is permitted. The smallest eventual scope is one deterministic test-only real-local-Podman `--userns=keep-id` UID-identity proof against the preloaded digest image and its trusted required CI invocation.
- The sandbox remains release-gated and off. Existing fake and Docker-backed tests do not prove the rootless Podman `keep-id` user-namespace mapping.

## M5 worktree metadata acceptance (2026-08-15)

- **`ACCEPTED`:** Exact implementation candidate commit `5f9488b36ff1f6f762fc2e56d68173ca6de4ac3d` passed CI run `31916585665`: Rust `95089262788`, web `95089262756`, supply-chain `95089262752`, and container `95089262753`.
- Acceptance is limited to the workspace-scoped inert worktree metadata lifecycle, legacy migration repair/quarantine proof, tenant-hiding writer authorization (`404` hidden versus `403` known `VIEWER`), no-side-effect denial proof, and existing task/activity route registration required by real integration coverage. It is not a claim of Git worktree execution, subagent execution, sandbox isolation, production deployment, or a live migration.
- M4 and M7 remain independently `BLOCKED_EXTERNAL`; their recorded prerequisites are unchanged. The next M20 priority is M8: define and accept its exact completion matrix without treating M2 vault or pure-validator evidence as real OAuth/JWKS interoperability.

## M20 Readiness Ledger

<!-- m20-readiness-ledger:v1; update this block in place; never append a second ledger -->

```json
{
  "SCHEMA_VERSION": 1,
  "AS_OF": "2026-08-15",
  "STATUS_ENUM": [
    "ACCEPTED",
    "IN_PROGRESS",
    "BLOCKED_EXTERNAL",
    "NOT_STARTED",
    "UNASSESSED",
    "DEFERRED"
  ],
  "COUNTING_RULE": "Count one release blocker for each milestone whose acceptance gate is not ACCEPTED. This is a milestone-gate count, not a count of implementation subtasks or transitive dependency failures. EXTERNAL_BLOCKERS counts only records explicitly established as BLOCKED_EXTERNAL; every other unsatisfied gate is INTERNAL_BLOCKERS until an accepted review reclassifies it.",
  "TOTAL_RELEASE_BLOCKERS": 15,
  "EXTERNAL_BLOCKERS": 2,
  "INTERNAL_BLOCKERS": 13,
  "MILESTONES": [
    {
      "ID": "M1",
      "NAME": "foundation",
      "STATUS": "ACCEPTED",
      "REMAINING_RELEASE_BLOCKERS": 0,
      "DEPENDENCIES": [],
      "EVIDENCE": [
        "docs/implementation-progress.md § Verification Log: 2026-08-09 Milestone 1 baseline passed formatting, Clippy with warnings denied, and 26/26 Nextest tests with PostgreSQL and pgvector.",
        "docs/implementation-progress.md § Release State: Milestone 1 remains tagged and its backup is retained."
      ],
      "CI_RUN": [],
      "NEXT_ACTION": "Preserve the accepted evidence; reopen only for a demonstrated regression."
    },
    {
      "ID": "M2",
      "NAME": "library",
      "STATUS": "ACCEPTED",
      "REMAINING_RELEASE_BLOCKERS": 0,
      "DEPENDENCIES": null,
      "EVIDENCE": [
        "docs/implementation-progress.md § Verification Log: 2026-08-10 Milestone 2 passed native/WASM Clippy and 33/33 Nextest tests, including live fake-provider semantic retrieval.",
        "docs/implementation-progress.md § Release State records the retained Milestone 2 image digest and released schema version 2."
      ],
      "CI_RUN": [],
      "NEXT_ACTION": "Preserve the accepted evidence; reopen only for a demonstrated regression."
    },
    {
      "ID": "M3",
      "NAME": "runtime",
      "STATUS": "ACCEPTED",
      "REMAINING_RELEASE_BLOCKERS": 0,
      "DEPENDENCIES": null,
      "EVIDENCE": [
        "docs/implementation-progress.md § Verification Log: Milestone 3 passed 40/40 fresh-PostgreSQL tests, native/WASM Clippy, release Trunk build, browser flow, supply-chain checks, and later closure review.",
        "docs/implementation-progress.md § Release State and § Verification Log: reviewed implementation commit `7ee7ff6` was deployed privately; live schema version is 3."
      ],
      "CI_RUN": [],
      "NEXT_ACTION": "Preserve the accepted implementation/deployment evidence; later schema releases are tracked by their own milestones and M20."
    },
    {
      "ID": "M4",
      "NAME": "sandbox runtime",
      "STATUS": "BLOCKED_EXTERNAL",
      "REMAINING_RELEASE_BLOCKERS": 1,
      "DEPENDENCIES": null,
      "EVIDENCE": [
        "PLAN.md § Status, Decision, and Preserved M7 External Record: M4 is `BLOCKED_EXTERNAL`; no successful runtime proof is claimed.",
        "docs/implementation-progress.md § M4 rootless Podman `keep-id` external gate: fake and Docker-backed tests do not prove the required real rootless Podman UID mapping."
      ],
      "CI_RUN": [],
      "NEXT_ACTION": "Provision the approved non-root local runner with real rootless Podman, preconfigured subuid/subgid ranges, the approved digest-pinned image preloaded, and a trusted required non-ignored CI job; then run the single `--pull=never --userns=keep-id` UID-identity proof."
    },
    {
      "ID": "M5",
      "NAME": "worktree task-subagent workflows",
      "STATUS": "ACCEPTED",
      "REMAINING_RELEASE_BLOCKERS": 0,
      "DEPENDENCIES": [
        "M1",
        "M3",
        "M6"
      ],
      "EVIDENCE": [
        "docs/implementation-progress.md § M5 worktree metadata acceptance: exact implementation candidate `5f9488b36ff1f6f762fc2e56d68173ca6de4ac3d` accepts workspace-scoped inert metadata lifecycle, legacy migration repair/quarantine, tenant-hiding writer authorization, no-side-effect denial, and task/activity route registration for integration coverage.",
        "Acceptance explicitly excludes Git worktree execution, subagent execution, sandbox isolation, production deployment, and live migration claims."
      ],
      "CI_RUN": [
        {
          "RUN_ID": "31916585665",
          "COMMIT": "5f9488b36ff1f6f762fc2e56d68173ca6de4ac3d",
          "RUST_JOB": "95089262788",
          "WEB_JOB": "95089262756",
          "SUPPLY_CHAIN_JOB": "95089262752",
          "CONTAINER_JOB": "95089262753",
          "SCOPE": "M5 worktree metadata candidate only"
        }
      ],
      "NEXT_ACTION": "Preserve M5 acceptance evidence. Do not revisit M4 or M7; the next M20 priority is M8's exact completion matrix."
    },
    {
      "ID": "M6",
      "NAME": "skill self-improvement",
      "STATUS": "ACCEPTED",
      "REMAINING_RELEASE_BLOCKERS": 0,
      "DEPENDENCIES": null,
      "EVIDENCE": [
        "docs/implementation-progress.md § Skills/schema-15 acceptance closure: commit `4d0b0b30ace33e9f3bd807883575742f6ae094da` was accepted by exact-SHA CI; Nextest ran 307 tests and passed all 307, with six configured skips.",
        "docs/skills.md records the accepted revision, evidence, promotion, rollback, authorization, concurrency, provenance, and schema-15 integrity contract.",
        "The live deployment remains schema 3; this is implementation-candidate acceptance, not a deployment claim and not acceptance of the old combined Skills/worktrees/tasks roadmap row."
      ],
      "CI_RUN": [
        {
          "RUN_ID": "31858799443",
          "COMMIT": "4d0b0b30ace33e9f3bd807883575742f6ae094da",
          "SCOPE": "Skills/schema-15 candidate only"
        }
      ],
      "NEXT_ACTION": "Preserve acceptance evidence. Deploy only through the later release runbook after dependent release gates pass; track deployment under M20."
    },
    {
      "ID": "M7",
      "NAME": "real MCP transport",
      "STATUS": "BLOCKED_EXTERNAL",
      "REMAINING_RELEASE_BLOCKERS": 1,
      "DEPENDENCIES": null,
      "EVIDENCE": [
        "docs/implementation-progress.md § M7 bounded stdio framing acceptance: commit `4c2bff3796ec7ac15b86a167d49828a1eb556b98` accepts bounded JSON-RPC line framing over caller-provided Tokio pipes only.",
        "The same section states the sole remaining M7 gate is `BLOCKED_EXTERNAL`: an approved, immutable, independently implemented MCP stdio peer and a non-ignored real-peer CI test for `server/discover` and capability-authorized `tools/list`."
      ],
      "CI_RUN": [
        {
          "RUN_ID": "31908353273",
          "COMMIT": "4c2bff3796ec7ac15b86a167d49828a1eb556b98",
          "SCOPE": "Bounded stdio framing only"
        }
      ],
      "NEXT_ACTION": "Approve and pin an independently implemented MCP stdio peer with provenance, version, and content hash; add the required non-ignored CI interoperability test. Do not substitute a fixture, mock, loopback, or in-repository peer."
    },
    {
      "ID": "M8",
      "NAME": "MCP authentication vault doctor",
      "STATUS": "IN_PROGRESS",
      "REMAINING_RELEASE_BLOCKERS": 1,
      "DEPENDENCIES": null,
      "EVIDENCE": [
        "Accepted M8A commit 5eb07151fb36c761d13b9d5943394f1b7a11e4f8 proves only exact MCP OAuth vault-purpose and raw one-host metadata policy, redacted actual vault-key readiness, a bounded count-only read-only MCP metadata doctor, and router/PostgreSQL coverage.",
        "Prior credential-vault encryption/rotation coverage and pure MCP doctor audience/schema validators remain supporting evidence, not real OAuth/JWKS interoperability proof.",
        "OAuth, JWKS, discovery, refresh, same-profile reference migration, vault-backed PKCE remodeling, real-provider proof, and M7 peer interoperability remain outside M8A acceptance."
      ],
      "CI_RUN": [
        {
          "RUN_ID": "31919742949",
          "COMMIT": "5eb07151fb36c761d13b9d5943394f1b7a11e4f8",
          "SCOPE": "M8A offline vault metadata/readiness: vault purpose, one-host raw metadata, redacted key readiness, count-only read-only doctor, and router/PostgreSQL proof"
        }
      ],
      "NEXT_ACTION": "Keep M8 IN_PROGRESS until its retained OAuth/JWKS/discovery/refresh, same-profile reference migration, vault-backed PKCE remodeling, and real-provider requirements are separately proven or its scope is formally changed; M7 peer interoperability remains independently BLOCKED_EXTERNAL."
    },
    {
      "ID": "M9",
      "NAME": "generated MCP integration pipeline",
      "STATUS": "UNASSESSED",
      "REMAINING_RELEASE_BLOCKERS": 1,
      "DEPENDENCIES": null,
      "EVIDENCE": [
        "docs/mcp.md names a generator workflow as intended architecture, but current implementation-progress contains no milestone-scoped M9 acceptance, exact CI run, or completion claim."
      ],
      "CI_RUN": [],
      "NEXT_ACTION": "Inventory generator code, generated artifacts, drift checks, security boundaries, and CI; write a bounded milestone plan before assigning implementation status."
    },
    {
      "ID": "M10",
      "NAME": "scheduler webhook release",
      "STATUS": "IN_PROGRESS",
      "REMAINING_RELEASE_BLOCKERS": 1,
      "DEPENDENCIES": null,
      "EVIDENCE": [
        "docs/implementation-progress.md § Webhook SSRF proof acceptance closure: commit `dfbbaa5b8e6e884010cbd4f2e57b60a8253c9edc` passed exact-SHA CI with 324/324 tests.",
        "The same section proves `features.webhook_scheduler_enabled` remains default-off and explicitly says scheduler release remains separately gated; docs/roadmap.md still marks scheduler/webhooks release-gated/off."
      ],
      "CI_RUN": [
        {
          "RUN_ID": "31864560017",
          "COMMIT": "dfbbaa5b8e6e884010cbd4f2e57b60a8253c9edc",
          "SCOPE": "Webhook SSRF/fencing proof only; not scheduler release"
        }
      ],
      "NEXT_ACTION": "Keep the scheduler off until the complete restart/replay/signature/recovery release gate is explicitly accepted and recorded; do not promote the bounded SSRF CI run into M10 completion."
    },
    {
      "ID": "M11",
      "NAME": "authorization matrix",
      "STATUS": "IN_PROGRESS",
      "REMAINING_RELEASE_BLOCKERS": 1,
      "DEPENDENCIES": null,
      "EVIDENCE": [
        "docs/implementation-progress.md records accepted bounded authorization evidence across Milestones 1–3, task/activity tenancy hardening, and the Skills/schema-15 lifecycle.",
        "No consolidated M11 authorization-matrix definition, exact acceptance run, or completion statement exists in the inspected repository docs."
      ],
      "CI_RUN": [],
      "NEXT_ACTION": "Define the cross-surface subject/role/resource/action matrix, map existing tests to every cell and revocation race, add only missing behavioral coverage, and record a milestone-scoped acceptance run."
    },
    {
      "ID": "M12",
      "NAME": "backup restore diagnostics",
      "STATUS": "IN_PROGRESS",
      "REMAINING_RELEASE_BLOCKERS": 1,
      "DEPENDENCIES": null,
      "EVIDENCE": [
        "docs/implementation-progress.md records checksum-verified backups and post-storage-move health, doctor, and security-audit checks.",
        "docs/backups.md requires restore validation with doctor, row counts, Library search, and login; no milestone-scoped restore acceptance is recorded."
      ],
      "CI_RUN": [],
      "NEXT_ACTION": "Run and record a destructive-safe restore rehearsal against an isolated target, including backup checksum, row counts, login, Library search, doctor, and rollback viability; do not treat backup creation as restore proof."
    },
    {
      "ID": "M13",
      "NAME": "sandboxed browser web LSP",
      "STATUS": "DEFERRED",
      "REMAINING_RELEASE_BLOCKERS": 1,
      "DEPENDENCIES": null,
      "EVIDENCE": [
        "docs/implementation-progress.md § Remaining gates lists browser/connectors/media adapter tests as user-excluded and requiring real adapter runtimes.",
        "No M13 acceptance or CI record exists."
      ],
      "CI_RUN": [],
      "NEXT_ACTION": "Obtain an explicit M20 scope decision. If M13 remains required, plan and prove the sandbox/browser/Web/LSP boundary against real runtimes; if removed, record the authoritative scope change rather than marking it accepted."
    },
    {
      "ID": "M14",
      "NAME": "plugin messaging media boundaries",
      "STATUS": "DEFERRED",
      "REMAINING_RELEASE_BLOCKERS": 1,
      "DEPENDENCIES": null,
      "EVIDENCE": [
        "docs/roadmap.md marks browser/connectors/media release-gated/off pending isolated adapter-specific security tests.",
        "docs/implementation-progress.md says browser/connectors/media adapter tests were user-excluded; no M14 acceptance or CI record exists."
      ],
      "CI_RUN": [],
      "NEXT_ACTION": "Obtain an explicit M20 scope decision. If retained, define and prove plugin, messaging, connector, and media trust boundaries with isolated real-adapter tests; exclusion is not completion."
    },
    {
      "ID": "M15",
      "NAME": "operator UI surfaces",
      "STATUS": "IN_PROGRESS",
      "REMAINING_RELEASE_BLOCKERS": 1,
      "DEPENDENCIES": null,
      "EVIDENCE": [
        "docs/implementation-progress.md records accepted M3 browser validation for owner login, conversation creation/opening, transcript composer, provider registry, and responsive layouts.",
        "docs/skills.md has an accepted backend contract while the inspected architecture evidence identifies the Skills operator page as non-functional; no complete M15 surface inventory or acceptance exists."
      ],
      "CI_RUN": [],
      "NEXT_ACTION": "Inventory every required operator surface against accepted APIs, implement missing surfaces only after their backend gates, and run desktop/mobile authenticated browser acceptance for the complete inventory."
    },
    {
      "ID": "M16",
      "NAME": "concurrency recovery gates",
      "STATUS": "IN_PROGRESS",
      "REMAINING_RELEASE_BLOCKERS": 1,
      "DEPENDENCIES": null,
      "EVIDENCE": [
        "docs/implementation-progress.md records bounded accepted concurrency/recovery evidence for M3 run leases and takeover, schema-15 Skills lifecycle races, task/activity ordering, and webhook stale-worker fencing.",
        "No repository record consolidates these slices into a complete M16 gate or proves every enabled subsystem."
      ],
      "CI_RUN": [],
      "NEXT_ACTION": "Define the enabled-subsystem crash/race/restart matrix, reuse existing accepted cases, add only uncovered observable recovery cases, and accept one milestone-scoped real-PostgreSQL CI run."
    },
    {
      "ID": "M17",
      "NAME": "security regression gates",
      "STATUS": "IN_PROGRESS",
      "REMAINING_RELEASE_BLOCKERS": 1,
      "DEPENDENCIES": null,
      "EVIDENCE": [
        "docs/implementation-progress.md records repeated Clippy, deny, audit, tenancy, CSRF, SSRF, append-only, and redaction checks, including clean supply-chain outcome in CI `31864560017`.",
        "docs/security.md retains documented advisory exceptions; no repository record declares a complete M17 adversarial regression gate accepted."
      ],
      "CI_RUN": [],
      "NEXT_ACTION": "Create a traceable security-requirement-to-test matrix, include accepted CI evidence without relabeling bounded runs, resolve uncovered enabled-surface threats, and record independent milestone acceptance."
    },
    {
      "ID": "M18",
      "NAME": "performance resource evidence",
      "STATUS": "UNASSESSED",
      "REMAINING_RELEASE_BLOCKERS": 1,
      "DEPENDENCIES": null,
      "EVIDENCE": [
        "docs/deployment.md documents example CPU/memory caps and states production sizing depends on workload.",
        "No milestone-scoped load, latency, memory, CPU, queue, database, or resource-exhaustion acceptance evidence is recorded."
      ],
      "CI_RUN": [],
      "NEXT_ACTION": "Define measurable release budgets and representative workloads first, then run repeatable resource/performance experiments and record raw results and thresholds."
    },
    {
      "ID": "M19",
      "NAME": "clean-install qualification",
      "STATUS": "IN_PROGRESS",
      "REMAINING_RELEASE_BLOCKERS": 1,
      "DEPENDENCIES": null,
      "EVIDENCE": [
        "docs/implementation-progress.md § Compose cold-start smoke records an ephemeral build/start/readiness/401 proof on 2026-08-12 and cleanup afterward.",
        "No complete M19 clean-host qualification, immutable artifact provenance, setup/upgrade matrix, or milestone acceptance record exists."
      ],
      "CI_RUN": [],
      "NEXT_ACTION": "Define the clean-host matrix and qualify the release candidate from immutable artifacts, including fresh install, migration, readiness, authentication denial, diagnostics, restart, and cleanup with exact evidence."
    },
    {
      "ID": "M20",
      "NAME": "private production release",
      "STATUS": "NOT_STARTED",
      "REMAINING_RELEASE_BLOCKERS": 1,
      "DEPENDENCIES": [
        "M1",
        "M2",
        "M3",
        "M4",
        "M5",
        "M6",
        "M7",
        "M8",
        "M9",
        "M10",
        "M11",
        "M12",
        "M13",
        "M14",
        "M15",
        "M16",
        "M17",
        "M18",
        "M19"
      ],
      "EVIDENCE": [
        "docs/implementation-progress.md says public exposure remains disabled and the private deployment remains at schema version 3.",
        "docs/implementation-progress.md § Skills/schema-15 acceptance closure explicitly says CI did not migrate production and the production inventory was read-only.",
        "No M20 release-candidate deployment, post-migration smoke, or release acceptance exists."
      ],
      "CI_RUN": [],
      "NEXT_ACTION": "Do not deploy or claim M20 until every dependency is ACCEPTED or formally removed from scope. Then execute the backup-first maintenance-mode schema upgrade from the Git-backed immutable candidate, verify health/schema/auth/diagnostics/security/functional flows and rollback evidence, and record exact deployment identity and acceptance."
    }
  ]
}
```
