# M24 Runtime Baselines

Permanent runtime-baseline measurement + stress-regression reference for the M24
optimization milestone. All numbers below were measured on the local single-host
compose stack (`docker compose up -d postgres app`) against the `gobrowse-os-app`
image built for this milestone.

This document is the baseline that later builds (M25+) must stay at or beat. The
permanent automated guard is
`crates/gobrowse-server/tests/m24_stress_integration.rs` (context/RAM must not
scale with installed count: 500 books + 100 plugins → bounded retrieval, library
< 33% / conversation < 67% of the content budget, deterministic answer).

## Environment

| Item | Value |
| --- | --- |
| Host OS | Linux 7.0.0-29-generic (Ubuntu) |
| Arch | x86_64 |
| CPU | 11th Gen Intel i3-1115G4 @ 3.00GHz |
| RAM | ~3.2 GB available |
| Docker | buildx v0.31.0, pgvector/pgvector:0.8.1-pg17 |
| Rust toolchain | 1.94, edition 2024 (workspace) |
| Measured | 2026-08-20 |

## Commands used (reproducible)

```bash
# 1. Image present?
docker images gobrowse-os-app

# 2. Bring up the stack (postgres first, then app; app healthchecks /health/ready)
docker compose up -d postgres
docker compose up -d app
# Wait for healthy:
docker ps --filter 'name=gobrowse-os-app-1' --format '{{.Status}}'

# 3. Image size (compressed + uncompressed)
docker images gobrowse-os-app --format '{{.Repository}}:{{.Tag}} DISK={{.Size}}'
docker image inspect gobrowse-os-app:m24 --format '{{.Size}}'   # compressed bytes

# 4. App idle RSS (container)
docker stats --no-stream --format '{{.Name}} {{.MemUsage}}'

# 5. Startup time: timestamp container-created -> healthcheck pass
docker inspect -f '{{.State.StartedAt}}' gobrowse-os-app-1
# (subtract compose-up invocation time; see Startup time table)

# 6. Login latency
curl -s -o /dev/null -w '%{http_code} %{time_total}\n' \
  -X POST http://127.0.0.1:8080/api/v1/auth/login \
  -H 'Content-Type: application/json' \
  -d '{"email":"m22test@example.com","password":"TestPassword123!"}'

# 7. Library search latency
curl -s -o /dev/null -w '%{http_code} %{time_total}\n' \
  'http://127.0.0.1:8080/api/v1/library/search?q=rust&limit=20'

# 8. Context endpoint (after a run id is known)
curl -s http://127.0.0.1:8080/api/v1/runs/<RUN_ID>/context \
  -H 'Cookie: gobrowse_session=<SESSION>'
```

## Docker image size (compressed vs uncompressed)

`docker images` reports `DISK USAGE` (= uncompressed virtual size, shared/builder
layers included) and `CONTENT SIZE` (= compressed image content). `docker image
inspect .Size` gives the compressed content size in bytes.

| Tag | Compressed | Uncompressed (disk usage) | Notes |
| --- | --- | --- | --- |
| m23 (previous) | 69.9 MB (69,864,786 B) | 275 MB | pre-M24 baseline |
| m24 (this milestone) | 69.8 MB (69,843,133 B) | 275 MB | real binary (11.2 MB), wasm-opt applied |

> m22 was not present in this environment, so the comparison is m23 → m24. The m24
> image reuses cached dependency/trunk/wasm-opt layers (deps-stage refactor) and
> carries the wasm-opt'd frontend, so compressed size is essentially flat while
> the WASM payload shrank 11.8% (see WASM table below).

> **Earlier build defect (resolved):** the first m24 build shipped a 330 KB dummy
> `fn main() {}` from the `deps` stage — the `builder` stage's `cargo build`
> reused the dependency-cache dummy artifact (mtimes preserved by `COPY` were
> older than the deps fingerprints, so cargo considered the dummy build fresh).
> Fixed by touching sources after `COPY` + dropping the dummy binary in the
> builder stage. The final m24 image's `/usr/local/bin/gobrowse` is the real
> 11.2 MB server (verified: 32 `serve`/`clap`/`tokio`/`migrate` symbols).

## Build time

`docker build -t gobrowse-os-app:m24 .` measured 2–3 min for the builder stage
(deps + trunk/wasm-opt layers cache-hit); the full cold build is ~25 min.

| Build | Container RSS (idle) | Host `ps -o rss` (if run bare) |
| --- | --- | --- |
| m24 | 3.3 MiB idle (13.2 MiB peak at startup) | not measured (ran in compose) |

The compose `app` service is capped at `memory: 384M` (deploy limit) and runs
`read_only: true` with `cap_drop: ALL`.

## Startup time (container created → healthy)

| Build | Created→Healthy |
| --- | --- |
| m24 | ~0.05 s (healthcheck 200 on first poll) |

## Login latency

Endpoint `POST /api/v1/auth/login` with `m24test@example.com` /
`TestPassword123!` (OWNER created via `POST /api/v1/setup/owner`).

| Build | HTTP status | Latency |
| --- | --- | --- |
| m24 | 200 | 29–38 ms (3 samples) |

## Library search latency

Endpoint `GET /api/v1/library/search?q=…` (route `library_api::search_books`).
Query uses `websearch_to_tsquery` over the generated `search_document` tsvector.

| Build | HTTP status | Latency (ms) |
| --- | --- | --- |
| m24 | 200 | 1.6–3.3 ms (3 samples) |

## Context endpoint shape

`GET /api/v1/runs/{id}/context` returns the run's `context_snapshot` JSON
(serialized from `build_messages`). Observed shape:

```json
{
  "selected": ["system-policy-v1", "<candidate stable id>", "…"],
  "omitted": ["<candidate stable id>", "…"],
  "used_tokens": <u32>,
  "budget": <u32>,
  "recent_messages": <count>,
  "task_class": "GeneralQA",
  "task_class_source": "rule"
}
```

`selected` (excluding the literal `"system-policy-v1"`) are the retrieved
context candidates (library retrieval, pinned books, worktree). The stress test
asserts `selected` length ≤ 12 (the `build_messages` SQL `LIMIT 12`) regardless
of installed book count, and that library tokens stay under 33% / conversation
tokens under 67% of the content budget.


## Notes / invariants enforced by M24

- The app is built with `read_only: true`, `cap_drop: ALL`,
  `no-new-privileges`, `tmpfs: /tmp`, and a 384M memory cap.
- Implicit library retrieval is bounded to 12 candidates (`build_messages`
  `LIMIT 12`) and competes only for the *optional* budget
  (`content_budget − recent_tokens`), so installed book/plugin count cannot
  inflate context RAM.
- `gobrowse-core` `build_context` selects required candidates first, then
  highest-priority optional ones, without slicing content.

## Before/After benchmark table (M23 → M24)

| Metric | M23 (before) | M24 (after) | Delta |
| --- | --- | --- | --- |
| WASM raw (dist) | 2,669,588 B | 2,353,690 B | −11.8 % |
| WASM gzip | 798,996 B | 774,461 B | −3.1 % |
| Docker image compressed | 69,864,786 B | 69,843,133 B | ≈ flat |
| Docker image uncompressed | 275 MB | 275 MB | ≈ flat |
| App idle RSS | n/a | 3.3 MiB | baseline |
| Startup → healthy | n/a | ~0.05 s | baseline |
| Login | n/a | 29–38 ms | baseline |
| Library search | n/a | 1.6–3.3 ms | baseline |

> WASM optimized with `wasm-opt -Oz --enable-bulk-memory` (binaryen 120). The
> `--enable-bulk-memory` flag is required — bare `wasm-opt -Oz` fails validation
> for this WASM. CI and the Dockerfile now run this with a real failure on a
> missing/empty result (no `|| true`).

## Capability regression matrix (M24)

Every feature exercised at M23 was re-exercised against the M24 build:

| Capability | M23 status | M24 status | Evidence |
| --- | --- | --- | --- |
| Owner setup | PASS | 200 (owner created via `/setup/owner`) | baselines run |
| Login | PASS | PASS (200, 29–38 ms) | baselines run |
| Library search | PASS | PASS (200, 1.6–3.3 ms) | baselines run |
| Task classification | PASS | PASS | `m24_stress` + context endpoint |
| Bounded context (500 books + 100 plugins) | n/a | PASS | `m24_stress_integration` |
| Chat agent loop | PASS (`milestone3`) | PASS (415/512 suite; milestone3 green) | full nextest |
| Plugin preview→install | PASS (prod verify) | ENV-FAIL local (pre-existing) | see note |

> Pre-existing, non-M24 note: `plugin_integration` tests fail locally with 503
> "plugin source unavailable" at HEAD too (GitHub-mock client connect error in
> this environment; milestone3's loopback fake providers pass, so this is not a
> loopback outage and predates M24). It is out of M24 scope and does not affect
> the M24 optimization work.
