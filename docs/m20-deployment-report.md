# M20 Deployment Report

## Upgrade Summary

| Field | Value |
|-------|-------|
| PREVIOUS_SHA | `7ee7ff6` (Milestone 3) |
| NEW_SHA | `70e55ce` (M20 post-audit cleanup) |
| PREVIOUS_IMAGE | `sha256:448386209a27c1c700fd9c592de8325e45aea677d281497e12a6bbf6e277038a` |
| NEW_IMAGE_DIGEST | `sha256:057d64c990651fa0dfcce9eac0a2804534625a9afbc9b5029ca55eb13b5f08b3` |
| SCHEMA_BEFORE | 3 |
| SCHEMA_AFTER | 18 |
| BACKUP_PATH | `/opt/gobrowse-os/backups/pre-m20-upgrade-20260816-213718.dump` |
| BACKUP_CHECKSUM | `f5d71c4324e7b256d1a3fa61f790b44ead67f048941fa1be01113fa9ca5879b2` |
| HEALTH_LIVE | `{"status":"live","version":"0.1.0"}` |
| HEALTH_READY | `{"status":"ready","version":"0.1.0"}` |
| DOCTOR | PostgreSQL Pass, pgvector Pass, Git Pass, Static assets Pass |
| SECURITY_AUDIT | Sandbox boundary Pass, Telemetry Pass |
| ROLLBACK_READY | Yes — image tagged `gobrowse-os-app:rollback-schema3`, backup verified |
| DEPLOYMENT_STATUS | SUCCESS |

## Verification Results

| Check | Result |
|-------|--------|
| Schema version = 18 | ✅ |
| Migrations applied (18 total) | ✅ |
| /health/live | ✅ 200 |
| /health/ready | ✅ 200 |
| /api/v1/auth/me (unauth) | ✅ 401 |
| /api/v1/conversations (unauth) | ✅ 401 |
| /api/v1/library/books (unauth) | ✅ 401 |
| /api/v1/models | ✅ 200 |
| Static assets (/) | ✅ 200 |
| gobrowse doctor | ✅ Pass |
| gobrowse security audit | ✅ Pass |
| App image = M20 candidate | ✅ sha256:057d64c9... |
| No public exposure (127.0.0.1:8080 only) | ✅ |
| Container logs clean | ✅ |
| Disk healthy (7.3G free) | ✅ |

## Rollback Procedure

If issues are discovered:

```bash
cd /opt/gobrowse-os
# Stop current app
docker compose stop app
# Restore compose file
cp backups/docker-compose.rollback-schema3.yml docker-compose.yml
# Restart with old image
docker compose up -d app
# Restore database if needed
docker exec -i gobrowse-os-postgres-1 pg_restore -U gobrowse -d gobrowse < backups/pre-m20-upgrade-20260816-213718.dump
```

## Deployment Date

2026-08-16T22:04:27Z
