# Implementation Progress

This file is the durable handoff record for work after the `milestone-1` tag at commit `c49d3a6`.

## Release State

- Milestone 1 is tagged, deployed privately, backed up, and verified.
- Public exposure remains disabled. The application binds to `127.0.0.1:8080` in the deployment.
- The remote PostgreSQL backup is `/opt/gobrowse-os/backups/milestone-1.dump` with a colocated SHA-256 file.
- The deployed Milestone 1 image digest is `sha256:864a29e45693916183cedec126a9c59c43bc0b2a4df4b525bd11414cccb58648`.

## Completed Milestone

Milestone 2 makes the Library complete: enforceable Book scopes, immutable revision snapshots and rollback, conversation projections, Autobiography proposals, durable embedding jobs, provider-neutral embedding adapters, and authorized hybrid retrieval.

## Decisions

- Store embeddings in `book_chunk_embeddings`, keyed by chunk and embedding model. Never overwrite vectors from one model with vectors from another model.
- Claim background work with unique expiring lease tokens. A process crash must make work reclaimable without operator intervention.
- Associate jobs with a Book revision. Workers cancel stale jobs instead of publishing vectors for obsolete content.
- Apply authorization predicates before lexical or vector ranking. Workspace search includes authorized profile/global/user Books rather than requiring an exact workspace match.
- Encrypt provider credentials before enabling authenticated provider calls. Each value uses AES-256-GCM with a random data-encryption key wrapped by an externally supplied master key, versioned AAD, and database-enforced wrapping-nonce uniqueness.
- Bind credentials to explicit provider hosts, pin validated DNS resolutions, and prohibit metadata/link-local destinations. Private endpoints require credential-free Ollama and the explicit local-embeddings feature.
- Rotate vault keys by configuring current and immediately previous key versions, then transactionally rewrapping profile credentials through the redacted API.
- Conversation Books are projections of durable messages and are deleted with their source conversation.

## Verification Log

- 2026-08-09: Milestone 1 baseline passed `cargo fmt --all -- --check`, Clippy with warnings denied, and 26/26 nextest tests with PostgreSQL and pgvector.
- 2026-08-10: Milestone 2 passed native and WASM Clippy with warnings denied and 33/33 nextest tests. The suite includes dirty-v1 migration, vault encryption/rotation, scope/role denial, conversation projection/fork/delete, stale Autobiography proposals, leased embedding completion, SSRF denial, and semantic retrieval through a live fake provider.
- 2026-08-10: The release WASM bundle and a non-root, statically linked smoke image passed `/health/ready` and reported schema version 2. Local image digest: `sha256:c2c060cf38804a7c58d1c1400fb4118917acc6b16f449ed80d0468f5b845d606`.

## Next Milestone

- Milestone 3: dynamic chat provider registry, neutral streaming conversations, durable run events, context assembly, fallback classification, cancellation, and deterministic provider E2E tests.
