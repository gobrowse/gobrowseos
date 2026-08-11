# Deployment

## Local

```bash
cp .env.example .env
docker compose up -d --build
```

The published port binds to loopback by default. Open `http://localhost:8080` and create the one-time owner. Change every example password before binding publicly.

## Production

1. Put a TLS reverse proxy in front of `127.0.0.1:8080`.
2. Set `GOBROWSE__HTTP__PUBLIC_ORIGIN` to the exact HTTPS origin and enable secure cookies.
3. Use Docker/Podman secrets or an environment file readable only by the service account.
4. Keep PostgreSQL on the internal Compose network; never publish port 5432.
5. Run `gobrowse doctor` and `gobrowse security audit` before accepting traffic.
6. Use rootless Docker or Podman for the optional sandbox deployment. Never mount a runtime socket into the app.

The example Compose file caps the app and PostgreSQL at 384 MB each and uses fractional CPU limits so a quiet core installation can run on a single-core, 1 GB host with swap. Production sizing depends on connection count, Library indexes, concurrent agents, and embedding workloads; monitor resources and raise limits deliberately rather than removing them.

## Update

1. Create and verify a database backup.
2. Pull/build the target image by immutable tag or digest.
3. Put the service in maintenance mode and stop the existing app so an older binary cannot write through a newer schema.
4. Run migrations with the target image: `docker compose run --rm app migrate`.
5. Start the target app and wait for `/health/ready` before removing maintenance mode.
6. Keep the previous image digest and verified backup until functional checks pass.

Treat migrations as incompatible with the previous app unless the release notes explicitly document an expand-compatible schema. If migration or startup fails, stop the target app, restore the pre-update database backup, restore the previous image digest, and only then restart the previous app. Never run the previous binary against a schema it was not released to accept.
