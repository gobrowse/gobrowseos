# Post-Release Backlog

Non-blocking improvements identified during M20 stabilization. None are release
blockers. Track as future work when capacity allows.

## M15 Operator UI — Missing Pages

The following pages render `EmptyOperationalPage` (stub shell):

| Page | Backend API Status | Effort |
|------|-------------------|--------|
| Tasks | Implemented (task_api) | Medium |
| Agents | No dedicated API | Low |
| Workspaces | No dedicated API | Low |
| Terminals | No dedicated API | Low |

Priority: Medium. The backend APIs exist for Tasks; Agents/Workspaces/Terminals
require new API endpoints before UI implementation.

## M12 Restore Rehearsal

Backup infrastructure is verified (checksum pg_dump, health/doctor/security-audit
checks). Actual restore validation is a deployment-runbook item:

1. Backup existing database
2. Restore into disposable PostgreSQL target
3. Run `gobrowse doctor`
4. Verify row counts
5. Test Library search and login
6. Confirm rollback viability

Schedule: during first production maintenance window.

## M8 Remaining Gaps (M7-dependent)

With M7 accepted, the following M8 sub-items are now theoretically unblocked:

- OAuth state/issuer/resource/audience/refresh contract
- Real-provider/JWKS rotation and interoperability proof
- Vault write-path integration for PKCE verifier lifecycle

These require real OAuth provider endpoints and remain future work. The current
M8 acceptance covers offline vault readiness, same-profile credential integrity,
and vault-backed PKCE schema only.

## M5 Schema Assertions

The worktrees integration test asserts `db::migrate()` schema version 18. This
is correct for current migrations but needs updating whenever a new migration
is added. Consider extracting the version into a shared constant.

## Test Infrastructure

- `cargo-nextest` is not installed in the CI environment; CI uses `cargo test`.
  Consider adding nextest for faster parallel test execution.
- `GOBROWSE_TEST_DATABASE_URL` is not configured locally; PostgreSQL integration
  tests skip. Local development workflow should document how to set this up.

## Code Quality

- The `mcp_raw_server.py` test server is a minimal raw JSON-RPC implementation.
  If MCP SDK v2.0.0 stabilizes its `tools/call` params validation, consider
  switching to the SDK server for richer protocol coverage.
- Several migration version assertions (schema_v14_to_v18, deployed_v3_to_v18)
  are tightly coupled. Consider a shared migration chain constant.

## M21 UI Backlog (P3)

- Models page: no delete button for model routes. Add route deletion (DELETE /api/v1/models/chat/{id}) + UI.
- Library page: books are creatable via the new Create Book form; also creatable via the chat library_add tool. No action needed.
- Mobile viewport session-cookie behavior: testing at 390x844 may show login instead of the dashboard when the browser context has no session cookie — normal auth behavior, verify with a persistent context in future mobile testing.
