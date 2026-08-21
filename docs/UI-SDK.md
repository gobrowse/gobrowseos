# UI-SDK.md — Gobrowse OS Custom UI System (M24b)

Machine-readable contract for building, validating, packaging, and installing UI packages (THEME or FULL_UI). The security model is not customizable: CSP is server-authoritative, auth/RBAC is server-authoritative, digest verification is mandatory, recovery UI is always available.

## 1. Architecture

```
Browser (WASM or THEME CSS)  ──►  Server (Axum)
  │ CSP enforced by browser         │ computes CSP hashes, injects CSP header
  │ fetches /api/v1/ui/*            │ serves /ui/packages/*, /recovery, CSP middleware
  │ reads /api/v1/capabilities      │ exposes capabilities endpoint
```

- **Backend APIs** are versioned `v1`, stable. UI packages may only call documented endpoints under `connect-src` (the server's `public_origin`). No raw secrets, no DB/sandbox creds, no internal tokens are ever exposed to the UI package.
- **CSP** is computed server-side from `ui_package_assets.sha256_hash` (hex SHA-256 of each file). The server injects `Content-Security-Policy` on every UI asset response. Manifest CSP declarations are advisory only and never override the server.
- **Recovery UI** at `GET /recovery` is a minimal built-in SPA compiled to `recovery_ui.wasm` and served from `/app/recovery/` in the Docker image. It bypasses the active UI package. If the active FULL_UI entry point is missing, `GET /` 302s to `/recovery`.

## 2. Manifest schema

Wire file: `schema/ui-package-manifest.json` (JSON Schema draft 2020-12). Every manifest MUST satisfy it.

Top-level shape (see `schema/ui-package-manifest.json` for normative constraints):

```json
{
  "kind": "gobrowse-ui",
  "manifest_version": 1,
  "name": "my-theme",
  "version": "1.0.0",
  "ui_kind": "THEME",
  "description": "Dark color scheme",
  "publisher": { "name": "Alice", "url": "https://example.com" },
  "license": "MIT",
  "api_version": "v1",
  "min_api_version": "v1",
  "entry": null,
  "capabilities": ["conversations", "library"],
  "permissions": { "ui_kind": "THEME" },
  "theme": {
    "variables": {
      "--ink": "#e0e0e0",
      "--canvas": "#1a1a2e"
    }
  },
  "source": {
    "repository": "https://github.com/alice/gobrowse-dark-theme",
    "revision": "abc123"
  }
}
```

### Field reference

| Field | Type | Required | Constraint |
|---|---|---|---|
| `kind` | string | yes | literal `"gobrowse-ui"` |
| `manifest_version` | integer | yes | `1` (current) |
| `name` | string | yes | `1..200` chars, DNS-like, unique per profile |
| `version` | string | yes | `1..64` chars, semver-ish `x.y.z` recommended |
| `ui_kind` | string | yes | `"THEME"` or `"FULL_UI"` |
| `description` | string | no | `0..2000` chars |
| `publisher.name` | string | no | human name |
| `publisher.url` | string | no | https URL |
| `license` | string | no | SPDX id, e.g. `"MIT"` |
| `api_version` | string | yes | `"v1"` |
| `min_api_version` | string | no | defaults to `api_version`; activation rejected if server `api_version` < `min_api_version` |
| `entry` | string \| null | conditional | `FULL_UI` requires relative path to `index.html` within package (e.g. `"index.html"`); `THEME` must be `null` |
| `capabilities` | string[] | no | free-form tags, e.g. `["conversations","library","sandbox"]`; informational |
| `permissions.ui_kind` | string | no | must match top-level `ui_kind` if present |
| `theme.variables` | object | conditional | `THEME` should provide `variables` as CSS custom property map `{"--ink":"#..."}`; `FULL_UI` should be `null` or absent |
| `source.repository` | string | no | URL |
| `source.revision` | string | no | commit or tag |

### Validation rules (server-side, `ui_api::validate_manifest`)

- `kind` must be `"gobrowse-ui"`, `manifest_version` must be `1`.
- `name` non-empty, `version` non-empty, `ui_kind` in `THEME|FULL_UI`.
- `FULL_UI`: `entry` non-null, ends with `.html`.
- `THEME`: `entry` null, `theme.variables` is an object with `--`-prefixed keys and non-empty string values.
- `api_version` must be `"v1"` (current).
- Unknown top-level fields are allowed but ignored; unknown `manifest_version` is rejected.

## 3. THEME tutorial

A THEME package only overrides CSS custom properties. The built-in WASM and HTML remain unchanged.

1. Copy the starter: `cp -r examples/starter-ui my-theme && cd my-theme`.
2. Edit `manifest.json`:
   ```json
   {
     "kind": "gobrowse-ui",
     "manifest_version": 1,
     "name": "my-theme",
     "version": "1.0.0",
     "ui_kind": "THEME",
     "description": "My first theme",
     "api_version": "v1",
     "theme": { "variables": { "--ink": "#e0e0e0", "--canvas": "#1a1a2e", "--surface": "#16213e", "--blue": "#7ea0ff" } }
   }
   ```
3. Edit `theme.css` if you prefer a file (optional — the server can generate it from `manifest.theme.variables`).
4. Validate: `tools/ui-package validate ./my-theme`.
5. Preview: `tools/ui-package preview ./my-theme` → prints digest + manifest.
6. Install (requires running server and a session cookie or `GOBROWSE_API_TOKEN`):
   `tools/ui-package install ./my-theme --approve` → returns `id` + `state`.
7. Activate: `curl -X POST http://127.0.0.1:8080/api/v1/ui/packages/{id}/activate -b cookies.txt` → `active`.
   The main UI fetches `/api/v1/ui/active-theme.css` and injects `<style id="gobrowse-theme">`.
8. Rollback: `curl -X POST http://127.0.0.1:8080/api/v1/ui/rollback -b cookies.txt`.
9. Delete: `curl -X DELETE http://127.0.0.1:8080/api/v1/ui/packages/{id} -b cookies.txt` (rejects `active`).

## 4. FULL_UI tutorial

A FULL_UI package replaces the entire frontend. It is a Leptos (or any) WASM app compiled to static assets with an `index.html` entry point.

1. Scaffold a Leptos app (same toolchain as `crates/gobrowse-web`):
   ```bash
   cargo new --lib my-ui --edition 2024
   # add leptos, gloo-net, serde, wasm-bindgen to [dependencies] (see gobrowse-web/Cargo.toml)
   trunk init  # creates Trunk.toml + index.html
   ```
2. `manifest.json`:
   ```json
   {
     "kind": "gobrowse-ui",
     "manifest_version": 1,
     "name": "custom-dashboard",
     "version": "1.0.0",
     "ui_kind": "FULL_UI",
     "description": "Custom dashboard",
     "api_version": "v1",
     "entry": "index.html",
     "capabilities": ["conversations","library","sandbox"]
   }
   ```
3. Build: `trunk build --release` → `dist/`.
4. Validate manifest: `tools/ui-package validate ./my-ui`.
5. Package: `tools/ui-package package ./my-ui --out ./my-ui.tar.gz` (creates a tarball with manifest + dist).
6. Preview/install/activate as for THEME, but FULL_UI activation shows a confirmation dialog in the UI ("This replaces the entire frontend. Continue?").
   After activation, `GET /` serves `install_path/index.html` and static assets under `install_path/`. CSP is computed from the package's asset hashes.

Constraints for FULL_UI:
- No inline `<script>` without hash. The server's CSP is `script-src 'sha256-…'`. Inline scripts without a matching hash will be blocked.
- No `unsafe-inline`, no `unsafe-eval`, no wildcard `connect-src`. `connect-src` is locked to `public_origin`.
- Must call only ` /api/v1/*` endpoints under `public_origin`.

## 5. Build / package / validate / install commands

All via `tools/ui-package`:

```bash
tools/ui-package validate ./path/to/package
# checks manifest.json against schema/ui-package-manifest.json, exits 0/1, prints errors

tools/ui-package preview ./path/to/package [--json]
# POST /api/v1/ui/preview (needs GOBROWSE_BASE_URL, session cookie)
# prints digest, ui_kind, capabilities; --json emits raw JSON

tools/ui-package package ./path/to/package [--out out.tar.gz]
# tar czf with manifest.json + assets; prints sha256 digest

tools/ui-package build ./path/to/full-ui
# wrapper around `trunk build --release`

tools/ui-package install ./path/to/package --approve [--base-url http://127.0.0.1:8080]
# preview → install with digest + approve; prints id/state
```

Environment:
- `GOBROWSE_BASE_URL` (default `http://127.0.0.1:8080`)
- `GOBROWSE_API_TOKEN` or a `cookies.txt` from login (`curl -c cookies.txt -X POST $BASE/api/v1/auth/login -H 'Content-Type: application/json' -d '{"email":"…","password":"…"}'`)

## 6. Capabilities endpoint

`GET /api/v1/capabilities` — unauthenticated, always available.

Response:

```json
{
  "api_version": "v1",
  "schema_version": 24,
  "server_version": "0.1.0",
  "auth": { "methods": ["session_cookie"], "setup_required": false },
  "features": { "sandbox": true, "browser": false, "messaging": false, "plugins": true, "ui_packages": true, "webhooks": true },
  "tools": [ { "name": "library_search", "kind": "library" }, { "name": "sandbox_exec", "kind": "sandbox" } ],
  "endpoints": { "conversations": { "base": "/api/v1/conversations" }, "library": { "base": "/api/v1/library" } },
  "ui": { "active_package_id": "uuid-or-null", "active_package_kind": "BUILT_IN", "permission_level": "ASK" }
}
```

- `tools` is derived from `AppState.tool_descriptors`.
- `features` from `settings.features`.
- `ui.active_package_kind` is `BUILT_IN|THEME|FULL_UI`.
- UI packages declare `min_api_version`; activation is rejected if `server api_version < min_api_version`.

## 7. Testing / validation workflow

1. `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings`
2. `cargo clippy -p gobrowse-web --target wasm32-unknown-unknown -- -D warnings`
3. `GOBROWSE_TEST_DATABASE_URL=postgres://gobrowse:test@127.0.0.1:5433/gobrowse_test cargo nextest run --workspace` (schema 24)
4. Manual: `curl -I http://127.0.0.1:8080/ | grep -i content-security-policy` — must not contain `unsafe-inline`.
5. Manual: `curl --fail http://127.0.0.1:8080/recovery` — recovery always 200.
6. Manual: `curl http://127.0.0.1:8080/api/v1/capabilities | jq .ui`

## 8. Agent workflow

Agent and human share the same HTTP endpoints. Agent auth is run-scoped; human auth is session cookie via `require_user`.

```
agent: POST /api/v1/ui/preview  {source_type:"local_package", source_uri:"inline:{...}", version:"1.0.0"}
server: 200 {digest, manifest, ui_kind}
agent: POST /api/v1/ui/install {source_type, source_uri, expected_digest: digest, approve:true, manifest}
server: 200 {id, state:"staged"}  (+ companion Book kind=GOBROWSE_UI)
agent: POST /api/v1/ui/packages/{id}/activate
server: if ui_permission_level == ASK and caller is agent → 202 {approval_required:true}
        else 200 {state:"active", previous_id}
human: POST /api/v1/ui/packages/{id}/approve  (user approves) → 200 active
human: POST /api/v1/ui/rollback → swaps active ↔ previous
```

- `ui_permission_level` on `profiles` (`DENY|ASK|ALLOW_WORKSPACE|ALLOW_GLOBAL`, default `ASK`). Changing to `ALLOW_GLOBAL` requires OWNER/ADMIN and is audit-logged.
- `DELETE /api/v1/ui/packages/{id}` rejects `active` or last-known-good (only remaining `active|previous|candidate|validated`).
- Digest verification: preview returns hex SHA-256 of the canonical manifest JSON; install must supply identical `expected_digest`; mismatch → 400.

## 9. Security boundaries (non-negotiable)

- UI package WASM runs in browser only, never on server.
- No access to DB creds, sandbox socket, vault secrets, HttpOnly session cookies, provider credentials.
- CSP `default-src 'none'; script-src 'sha256-…'; style-src 'sha256-…' or 'self'; img-src 'self' data:; connect-src $public_origin; font-src 'self'; frame-ancestors 'none'; base-uri 'self'; form-action 'self'` — no `unsafe-inline`/`unsafe-eval`/wildcard.
- All auth/RBAC decisions server-side. Manifest `permissions` is metadata only.
- Recovery routes (`/recovery`, `/recovery/*`) bypass the active UI package and cannot be overwritten.

## 10. File locations

- Server: `crates/gobrowse-server/src/ui_api.rs`, `csp.rs`, `capabilities_api.rs`, `lib.rs` (ActiveUiState, router, fallback).
- Migration: `crates/gobrowse-server/migrations/0024_ui_packages.sql` (schema 23→24).
- Web: `crates/gobrowse-web/src/app.rs` (Page::UiPackages, UiPackagesPage, ModelsPage polish), `styles.css`.
- Recovery: `crates/gobrowse-recovery/` (standalone Leptos app, `/app/recovery/` in Docker).
- Schema: `schema/ui-package-manifest.json`.
- Starter: `examples/starter-ui/`.
- Tool: `tools/ui-package`.

## 11. References

- `PLAN.md` M24b section is authoritative.
- `crates/gobrowse-server/tests/ui_packages_integration.rs` — integration test mirroring `plugin_integration.rs`.
