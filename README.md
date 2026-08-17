# Gobrowse OS

> A self-hosted, model-agnostic AI agent operating environment with persistent sandboxed computers and a global semantic context system called the Library.

Gobrowse OS is a Rust-first agent harness for long-running work. The Library stores durable context as provenance-aware **Books**; the curated **Autobiography** captures stable user knowledge without becoming a transcript dump. Conversations, workspaces, tasks, Skills, MCP integrations, isolated Git worktrees, and sandboxed terminals share one inspectable operating environment.

## Status

**M20 PASS** — all 20 milestones accepted, zero release blockers. See [`docs/m20-release-report.md`](docs/m20-release-report.md) for the full release report and [`docs/implementation-progress.md`](docs/implementation-progress.md) for the acceptance ledger.

### Milestone Summary

| Milestone | Status |
|-----------|--------|
| M1-M3 | Foundation, Library, Runtime |
| M4 | Rootless Podman sandbox isolation |
| M5 | Worktree metadata lifecycle & tenant auth |
| M6 | Skill self-improvement lifecycle |
| M7 | Real MCP stdio interoperability |
| M8 | Auth vault, credential integrity, PKCE schema |
| M10 | Webhook release gate |
| M11-M19 | Auth matrix, backup, concurrency, security, performance, clean-install |
| M20 | **PASS** |

M13/M14 (browser/LSP, plugins/media) are out of scope for this release.

## Run

```bash
cp .env.example .env
docker compose up -d
```

Open `http://localhost:8080`. The first account is created through the one-time owner setup endpoint/wizard. Production deployments must terminate TLS before the app and set `GOBROWSE__HTTP__SECURE_COOKIES=true`.

### CLI Commands

```bash
gobrowse serve          # Start the HTTP/WebSocket server
gobrowse migrate        # Apply database migrations
gobrowse doctor         # Check runtime dependencies
gobrowse security audit # Inspect security configuration
gobrowse config         # Print effective configuration
```

## Architecture

- **gobrowse-core** — portable domain types, state machines, provider/tool traits, MCP protocol contracts
- **gobrowse-server** — Axum API, PostgreSQL adapters, authentication, migrations, diagnostics
- **gobrowse-web** — Leptos CSR operator interface (Chat, Library, Diagnostics, Models)
- **gobrowse-sandboxd** — standalone privileged daemon for rootless Podman terminals/workspaces

## Principles

- No Cloudflare or SaaS dependency.
- Chat models and embedding models are independently configured.
- PostgreSQL plus pgvector provides durable state, lexical retrieval, and vector retrieval.
- The web process never receives a Docker socket or an unsandboxed host shell.
- Tool policy is enforced outside model output.
- No external telemetry is enabled by default.
- MCP integrations use real stdio transport with independently maintained peers.
- Sandbox isolation uses real rootless Podman with user namespace mapping.

Architecture and security documentation start at [`docs/architecture.md`](docs/architecture.md) and [`docs/security.md`](docs/security.md).

## Development

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo clippy -p gobrowse-web --target wasm32-unknown-unknown -- -D warnings
cargo nextest run --workspace
cargo deny check
cargo audit
(cd crates/gobrowse-web && trunk build index.html --dist ../../dist)
```

See [`AGENTS.md`](AGENTS.md) for the full CI matrix and conventions.

## Recent Features

- **Provider catalog dropdown** — pick from OpenCode Go, OpenAI Codex, OpenRouter, OpenAI, Anthropic, Google, DeepSeek, Mistral, xAI, or Ollama; base URL, model, context window, and output limit auto-fill from the catalog.
- **Model cost & usage charts** — spend by model/provider/day, token totals, unpriced-run counts (`/api/v1/usage/summary`).
- **Library editing** — open any Book from the list, view its body, edit and save with revision checks.
- **Library → chat pinning** — pin Books to a conversation; the agent receives them as pinned context on every run.
- **Workspace context** — a selected workspace's worktrees/files are included in context assembly.
- **Model library tools** — agents can search and add Library context during a run via `library_search` / `library_add` tools.
- **Operator pages** — Skills, Autobiography, Workspaces, and MCP server management now have functional UI (list, create, delete).

## Built With

Gobrowse OS was built by [Oh My Pi](https://github.com/ohmyzsh/ohmyzsh) using multiple AI models across a 4-day autonomous development sprint. The entire M1→M20 milestone progression — from foundational infrastructure through security-critical migrations, sandbox isolation, MCP interoperability, and release qualification — was planned, implemented, reviewed, and verified by AI agents operating within the Pi coding harness.

Models used include OpenAI GPT-5.6 (Sol/Luna/Terra), DeepSeek V4 Flash, and Google Gemini, orchestrated through the Oh My Pi agent framework with cheapest-capable routing, Level 1-3 review gates, and automated CI validation.

## License

Apache-2.0. This repository is an original implementation and is not a fork of another agent harness.
