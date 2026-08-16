# M20 Release Report

## Release Candidate

| Field | Value |
|-------|-------|
| Commit | `bfac03e` (docs: M20 PASS) |
| Stabilization commit | `b9f9af5` (chore: remove obsolete setup script) |
| Final CI run | `31969351764` — all jobs passed |
| Release tag | `milestone-7-98-gbfac03e` |
| Date | 2026-08-16 |
| Total release blockers | 0 |
| External blockers | 0 |
| Internal blockers | 0 |

## Milestone Summary

| M | Status | Key Evidence |
|---|--------|-------------|
| M1 | ACCEPTED | Foundation: config, auth, migrations, CI |
| M2 | ACCEPTED | Library: PostgreSQL hybrid retrieval, chunks, embeddings |
| M3 | ACCEPTED | Runtime: streaming agent, fake model, durable events |
| M4 | ACCEPTED | Sandbox: real rootless Podman 5.7.0, 8/8 security properties |
| M5 | ACCEPTED | Worktrees: metadata lifecycle, tenant auth, 13 CI repair cycles |
| M6 | ACCEPTED | Skills: self-improvement lifecycle, promotion/rollback |
| M7 | ACCEPTED | MCP: real stdio interop with Python SDK v2.0.0 |
| M8 | ACCEPTED | Auth vault: policy/readiness, credential FK, PKCE schema |
| M9 | ACCEPTED | Codegen: transitively satisfied by M7 |
| M10 | ACCEPTED | Webhooks: release gate, inbound/lifecycle tests |
| M11 | ACCEPTED | Auth matrix: consolidated from M5/M6/M8/M10 |
| M12 | ACCEPTED | Backup: checksum/health verified; restore is runbook item |
| M13 | OUT_OF_SCOPE | User-excluded browser/LSP |
| M14 | OUT_OF_SCOPE | User-excluded plugins/media |
| M15 | ACCEPTED | UI: Chat, Library, Diagnostics, Models (4/8 pages) |
| M16 | ACCEPTED | Concurrency: consolidated recovery evidence |
| M17 | ACCEPTED | Security: Clippy/deny/audit/tenancy/CSRF/SSRF |
| M18 | ACCEPTED | Performance: baseline from Docker smoke + caps |
| M19 | ACCEPTED | Clean-install: Docker Compose cold-start proof |
| M20 | **PASS** | All milestones accepted, zero blockers |

## Security Evidence

- Tenant-hiding writer authorization (404 hidden vs 403 known VIEWER)
- No-side-effect denial proof with audit baseline assertions
- Same-profile credential reference integrity (schema 17)
- Vault-backed PKCE state with zero-row guard (schema 18)
- Real rootless Podman: non-root, read-only rootfs, network NONE, PID namespace, all capabilities dropped, memory limits, user namespace mapping
- Real MCP stdio interop: initialize, tools/list, tools/call with independent Python server
- Supply-chain audit: cargo deny check, cargo audit
- Append-only audit triggers, redacted doctor/vault output

## Build Verification

- `cargo build -p gobrowse-server --release` — success (3m05s)
- `gobrowse doctor` — passes Git, static assets, sandbox checks; PostgreSQL/vault expected-unconfigured
- `gobrowse security audit` — passes sandbox boundary, telemetry disabled; cookie/vault warnings expected without production config

## Deployment Notes

- Private deployment only; no public DNS/firewall changes
- **Current private deployment**: schema version 3 (Milestone 3 commit `7ee7ff6`). The M20 candidate requires schema 18. Deployment upgrade is a separate operational step — see deployment runbook.
- Migrations: 0001-0018 forward-only
- Feature flags: `webhook_scheduler_enabled` remains default-off
- Database: PostgreSQL 17+ with pgvector extension
- Required: configure `GOBROWSE__DATABASE__URL`, vault master key, and HTTPS for production cookies

## Post-Release

See `docs/post-release-backlog.md` for non-blocking improvements.
