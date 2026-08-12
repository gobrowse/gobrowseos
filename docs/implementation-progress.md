# Implementation Progress

This file is the durable handoff record for work after the `milestone-1` tag at commit `c49d3a6`.

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
- **Real Podman-specific userns** (`--userns=keep-id` rootless): the docker shim strips it; only real Podman can prove the rootless UID mapping. Sandbox HARDENING is proven via docker; the Podman-specific USERNS CLAIM is not.
- **MCP OAuth matrix / real-server conformance / JWKS token validation** — needs real OIDC IdPs (mcp.rs banner).
- **OIDC/WebAuthn auth** — user-excluded; needs real providers.
- **Browser/connectors/media adapter tests** — user-excluded; needs real browser/adapter runtimes.
- **Compose cold-start smoke** — technically possible against the running docker daemon but costs ~10 min + a pgvector pull for marginal coverage beyond `doctor` + migration gates; intentionally deferred.

## Completed Milestone (auth/webhook/MCP subset)

- Login and owner-setup throttling persists attempts with a uniform Unauthorized response (per `docs/threat-model.md` "uniform login failures"); schema bumped to 4 via `0004_login_attempts.sql`.
- Webhook HMAC-SHA256 signature verification (manual RFC-2104, no `hmac` dependency) and replay idempotency via `webhook_deliveries` PK; webhooks excluded from the CSRF origin guard because they use signature auth; schema bumped to 5 via `0005_webhooks.sql`.
- Session rotation invalidates all prior sessions through `users.auth_epoch + 1` (column and enforcement present since `0001_initial.sql`); disabled users cannot log in; non-admins cannot rotate others.
- CSRF closure: `origin_guard` now rejects missing Origin on state-changing methods and `Sec-Fetch-Site: cross-site`; WebSocket `validate_origin` rejects empty/mismatched Origin and wrong-scheme; latent no-Origin requests in two existing test helpers were corrected.
- MCP doctor pure-logic validator flags remote/dynamic `$ref`, oversized (>256 KiB), and depth-bombed tool schemas in `gobrowse-core/src/mcp.rs`, reusing the existing `McpDoctorReport`/`DiagnosticStatus`. OAuth matrix and real-server conformance remain release-gated.
