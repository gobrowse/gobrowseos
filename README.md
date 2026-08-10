# Gobrowse OS

> A self-hosted, model-agnostic AI agent operating environment with persistent sandboxed computers and a global semantic context system called the Library.

Gobrowse OS is a Rust-first agent harness for long-running work. The Library stores durable context as provenance-aware **Books**; the curated **Autobiography** captures stable user knowledge without becoming a transcript dump. Conversations, workspaces, tasks, Skills, MCP integrations, isolated Git worktrees, and sandboxed terminals share one inspectable operating environment.

## Status

Gobrowse OS is under active clean-sheet development. Milestone 2 provides the secure modular-monolith foundation plus scoped hybrid Library retrieval, durable conversation projections, governed Autobiography revisions, leased embedding workers, OpenAI-compatible/Ollama embedding adapters, and an envelope-encrypted credential vault. See [`docs/roadmap.md`](docs/roadmap.md) for the release gate of each subsystem.

## Run

```bash
cp .env.example .env
docker compose up -d
```

Open `http://localhost:8080`. The first account is created through the one-time owner setup endpoint/wizard. Production deployments must terminate TLS before the app and set `GOBROWSE__HTTP__SECURE_COOKIES=true`.

## Principles

- No Cloudflare or SaaS dependency.
- Chat models and embedding models are independently configured.
- PostgreSQL plus pgvector provides durable state, lexical retrieval, and vector retrieval.
- The web process never receives a Docker socket or an unsandboxed host shell.
- Tool policy is enforced outside model output.
- No external telemetry is enabled by default.

Architecture and security documentation start at [`docs/architecture.md`](docs/architecture.md) and [`docs/security.md`](docs/security.md).

## License

Apache-2.0. This repository is an original implementation and is not a fork of another agent harness.
