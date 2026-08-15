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
