# PLAN.md — M8A Offline MCP OAuth Vault Metadata and Doctor Readiness

## Status and boundary

**M8 remains `IN_PROGRESS`.** M5/M6 are accepted. This bounded M8A slice does
not complete M8 and does not reduce the milestone-gate blocker count. It
implements only offline MCP OAuth credential metadata policy and redacted vault
readiness diagnostics.

## Preserved External Records

**M4 is `BLOCKED_EXTERNAL`.** The sandbox remains release-gated and off. This
plan records no successful runtime proof and authorizes no source or CI change
until the approved execution prerequisite below is available.

The caller-provided-pipe bounded stdio JSON-RPC framing slice is accepted at
`4c2bff3796ec7ac15b86a167d49828a1eb556b98`. Exact-SHA CI run
`31908353273` passed Rust `95069542559`, web `95069542603`,
supply-chain `95069542548`, and container `95069542574`. This accepts only
bounded stdio framing over caller-provided Tokio async pipes and its tests; it
does not create a process, establish a session, or prove real MCP
interoperability. **M7 remains incomplete.**

The sole remaining M7 gate is `BLOCKED_EXTERNAL`. It requires an approved,
immutable/pinned, independently implemented MCP stdio peer with recorded and
verified provenance, version, and content hash. Non-ignored CI must run a
real-peer integration test that performs `server/discover` and a
capability-authorized `tools/list` against that peer. A fixture, mock,
loopback peer, or in-repository substitute does not satisfy this requirement.

That independent pinned MCP peer prerequisite remains blocked; this M5 plan
does not alter, replace, or satisfy it.

### M4 External Prerequisite (Preserved)

The smallest honest M4 proof requires all of the following, none of which is
available in the current environment:

1. an approved ephemeral **local** runner executing as a non-root user with a
   real rootless Podman installation;
2. preconfigured `/etc/subuid` and `/etc/subgid` entries for that non-root
   runner user;
3. the exact immutable image reference preloaded locally by digest; and
4. a trusted, required, non-ignored CI job bound to that runner.

The runner and its Podman storage must be provisioned before the job starts.
The proof must neither pull nor install anything and must not use `sudo` or
make any host mutation. A Docker shim, remote Podman service, rootful Podman,
fixture, mock, or fake executable is not a substitute for this prerequisite.

Do not start the eventual test or CI work until every external prerequisite is
approved and present. Do not bypass a missing prerequisite with Docker,
rootful/remote Podman, a downloaded image, an install, `sudo`, a configuration
change, or a locally fabricated result. Until then, M4 remains
`BLOCKED_EXTERNAL`, the sandbox remains release-gated and off, and there is no
M4 runtime-proof success claim.

## M8A implementation

### Exact policy

- Recognize exactly `mcp_oauth_access_token`, `mcp_oauth_refresh_token`,
  `mcp_oauth_client_secret`, and `mcp_oauth_pkce_verifier`.
- Reject every other purpose beginning with `mcp_`; preserve non-MCP purpose
  behavior.
- Every recognized MCP purpose requires exactly one canonical authority host.
  Canonicalization is shared by vault create, replace, and doctor: normalize
  case/trailing-dot and IDNA DNS names; reject empty/malformed labels, URLs,
  userinfo, ports, paths, queries, fragments, wildcards, and malformed
  authorities. Literal IPs are accepted only when
  `gobrowse_core::sandbox::is_public_destination` classifies them as public.
  Never resolve DNS in this slice.

### Doctor and security invariants

- Keep envelope encryption, AAD, rotation, fencing, profile scoping, and
  OWNER/ADMIN authorization unchanged. Do not decrypt or write during doctor.
- Construct `Vault::from_settings` exactly once per doctor run. A valid current
  key and optional valid previous key pass; absent configuration warns; invalid
  base64/length, unreadable or insecure files, and inconsistent sources fail
  with one generic redacted detail. No paths, raw errors, IDs, hosts, secrets,
  ciphertext, or envelope fields may appear in the new readiness output.
- Use one bounded read-only PostgreSQL metadata query selecting only `purpose`,
  `allowed_hosts`, `backend`, and `key_version` for `mcp_` rows. The fixed
  inspection cap is 1000 rows; query 1001 rows to detect overflow
  deterministically and fail without unbounded allocation. Fold to aggregate
  counts only. No decrypt, lock, rotation, or write.
- No rows warns. Valid recognized purpose/host/backend and current or valid
  previous key version passes. Unknown/stale purpose, invalid host or host
  cardinality, unsupported backend, stale key version, unavailable vault, or
  query failure fails with redacted count/generic detail.

### Files and tests

Modify only `PLAN.md`, `crates/gobrowse-server/src/vault.rs`,
`crates/gobrowse-server/src/vault_api.rs`, `crates/gobrowse-server/src/doctor.rs`,
and focused existing/new server tests. Add pure policy, key-readiness,
classifier, redaction, authenticated router, and real PostgreSQL tests using
the existing shared advisory lock convention. Do not add dependencies,
configuration, migrations, frontend changes, OAuth/JWKS/discovery/DNS
resolution, token refresh, MCP transport/server CRUD, or sandbox behavior.

Validation is focused formatting, native warnings-denied checks, and focused
unit/router/PostgreSQL tests; PostgreSQL tests must report their existing
`GOBROWSE_TEST_DATABASE_URL` skip exactly when unavailable. Do not claim stored
ciphertext decrypts, provider interoperability, real OAuth/JWKS behavior, or
M8 acceptance.

## Remaining M8 blockers

M8 still requires a separately reviewed same-profile composite integrity
migration for MCP server credential references, vault-backed PKCE state
remodeling, the complete OAuth state/issuer/resource/audience/refresh contract,
and real-provider/JWKS rotation and interoperability proof. M7 additionally
remains blocked on the independently pinned real MCP stdio peer above. M4
remains blocked on the approved rootless Podman runner and immutable image
prerequisites above. No item in this M8A slice retires those blockers.
