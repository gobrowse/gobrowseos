# AGENTS.md

Rust 1.94.0 (pinned in `rust-toolchain.toml`), edition 2024, workspace with 4 crates. No JS toolchain — the frontend is Rust/Leptos compiled to WASM. This is a security-sensitive clean-sheet agent OS; see `docs/security.md`, `docs/threat-model.md`, and `docs/roadmap.md` before adding capabilities.

## Commands

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo clippy -p gobrowse-web --target wasm32-unknown-unknown -- -D warnings
cargo nextest run --workspace        # use nextest, not cargo test
cargo deny check                     # supply chain
cargo audit --ignore RUSTSEC-2023-0071 --ignore RUSTSEC-2024-0436 --ignore RUSTSEC-2026-0173
(cd crates/gobrowse-web && trunk build index.html --dist ../../dist)   # wasm build
cargo run -p gobrowse-server --bin gobrowse -- serve
```

CI (`.github/workflows/ci.yml`) runs exactly: fmt → clippy → wasm clippy → nextest → migrations → web build → deny → audit → docker build. Match this locally before committing.

## Tests

- `postgres_integration.rs` and `milestone3_integration.rs` **silently skip** unless `GOBROWSE_TEST_DATABASE_URL` is set. Point it at a throwaway Postgres/pgvector DB to actually run them (see the CI service block for a working URL). Postgres must be reachable with migrations applied.
- The default `cargo nextest run` passes with those skipped; don't mistake a skip for coverage.

## Architecture

- `crates/gobrowse-core` — portable domain model and runtime contracts (no I/O). Everything domain-level lives here.
- `crates/gobrowse-server` — the app. Binary name is `gobrowse` (see `[[bin]]`), not `gobrowse-server`. Entrypoint `src/main.rs`; subcommands `serve`, `migrate`, `doctor`, `security`, `config`. DB migrations in `crates/gobrowse-server/migrations/`.
- `crates/gobrowse-web` — Leptos CSR frontend, built with Trunk into `dist/` (gitignored output).
- `crates/gobrowse-sandboxd` — standalone privileged daemon for rootless Podman terminals/workspaces. **Not yet wired into the app**; terminal execution is disabled until configured. Its protocol/interface is `gobrowse_core::sandbox`. Do not assume app↔sandboxd integration exists.

## Configuration

Precedence: typed defaults → `config.toml` → `GOBROWSE__` env vars (e.g. `GOBROWSE__HTTP__SECURE_COOKIES`) → CLI flags. `.env.example` and `config.example.toml` are the reference. The credential vault (`config.example.toml [vault]`) requires a base64 32-byte master key from a file or env before provider credentials can be stored.

## Conventions

- `unsafe_code` is **forbid** workspace-wide (`Cargo.toml [workspace.lints]`). No `unsafe` in any crate.
- Clippy: `all` warnings enabled; `module_name_repetitions` allowed. Keep `-D warnings` clean.
- Conventional Commits are used throughout history (`feat:`, `fix:`, `docs:`, `test:`, `build:`).
- Release gates tracked as tags `milestone-N`; `docs/roadmap.md` defines what each milestone must prove.
- Root is `repository = "https://github.com/gobrowse-os/gobrowse-os"` in Cargo.toml, but the repo lives at a different URL — do not change code to match.
- `deny.toml` explicitly ignores two advisories (RUSTSEC-2024-0436, RUSTSEC-2026-0173) via Leptos; do not "fix" them by bumping pinned deps without checking the ignore rationale.

## Pi Agent Resource and Plan Rules

- Keep this Pi setup and any agent-added runtime within a strict 300 MB budget. Do not add local databases, Docker, browser engines, persistent daemons, or heavyweight packages. Prefer the existing tools and lazily started services.
- Treat a user prompt beginning exactly with `[PLAN]` as **Read-Only Architecture Mode**.
- In `[PLAN]` mode, do not call `bash` or `edit`, and do not call `write` except for one final write to the repository-root `PLAN.md`. Do not modify source code, tests, configuration, dependencies, or any other file.
- In `[PLAN]` mode, inspect only with read-only tools as needed. Produce the complete architecture, dependency/impact analysis, implementation checklist, validation steps, and open questions in that one `PLAN.md` write.
- Immediately stop after writing `PLAN.md`: make no further tool calls and emit no implementation or follow-up response.
