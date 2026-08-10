# Troubleshooting

- `/health/live` proves the process is serving HTTP; `/health/ready` also checks PostgreSQL.
- Run `gobrowse doctor --json` for machine-readable dependency checks.
- A correlation ID in an API error maps to structured server logs without exposing SQL details.
- Login cookies require the exact configured public origin. HTTPS deployments must enable secure cookies.
- If migrations cannot create pgvector, enable the extension as a database administrator and rerun `gobrowse migrate`.
- Sandbox terminal controls remain unavailable while `FEATURE_SANDBOX=false`.
