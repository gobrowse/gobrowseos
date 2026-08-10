# Backup and Restore

A named volume is not a backup. Use PostgreSQL's logical backup tools and copy exported Skills/workspace metadata according to policy.

```bash
docker compose exec -T postgres pg_dump -U gobrowse -d gobrowse --format=custom > gobrowse.dump
docker compose exec -T postgres pg_restore -U gobrowse -d gobrowse --clean --if-exists < gobrowse.dump
```

Stop the app or restore into a fresh database before restore. Validate with `gobrowse doctor`, row counts, a Library search, and login. Secret material is excluded from portable application exports by default; future encrypted secret export requires a separate backup key and explicit opt-in.
