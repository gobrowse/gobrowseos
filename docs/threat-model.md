# Threat Model

## Assets

- User identity, sessions, profiles, private Books and conversation content.
- Provider, MCP, webhook, repository, and infrastructure credentials.
- Workspace files, Git history, terminal streams, checkpoints, and backups.
- Tool approvals, agent run integrity, audit evidence, and provider billing authority.

## Trust Boundaries

1. Browser to Axum: authenticated same-origin HTTPS; all authorization is rechecked server-side.
2. Axum to PostgreSQL: private authenticated network; SQL parameters only; row access is scoped by application authorization and transaction checks.
3. Axum to providers/MCP: outbound untrusted network; bounded payloads, redirect restrictions, SSRF checks, redacted errors.
4. Axum to sandboxd: mutually authenticated narrow protocol; no Docker socket in app; sandboxd is not internet-exposed.
5. sandboxd to containers: unprivileged, capability-free, resource-limited, restricted network and explicit writable volumes.
6. Model boundary: prompts and tool schemas leave the host only for the configured provider. Credentials never enter model messages.

## Primary Threats and Controls

- Prompt injection: provenance/trust labels, fixed instruction hierarchy, no automatic elevation from retrieved content, tool policy outside the model.
- Credential disclosure: encrypted vault with external master key, secret references, response/log redaction, no raw prompt logs by default.
- Host escape: no host shell, no privileged container, no host namespaces/network, dropped capabilities, rootless runtime, seccomp and resource controls.
- SSRF: resolve and classify every outbound destination, deny loopback/private/link-local/metadata ranges by default, pin checked addresses through connection where practical, validate redirects independently.
- Account takeover: Argon2id, uniform login failures, bounded hashing concurrency, rate limits, opaque rotated sessions, secure cookie policy, optional WebAuthn/OIDC.
- CSRF/WebSocket hijacking: same-origin deployment, Origin and Fetch Metadata checks for mutations, exact Origin check before WebSocket upgrade.
- Cross-tenant access: resource-scoped authorization on every query/mutation; opaque IDs are not authorization.
- Tool replay: unique call IDs, durable execution state, explicit retry classification; external side effects are never blindly retried.
- Supply chain: locked dependencies, cargo-deny/audit, SBOM, non-root minimal image, protected CI.
- Malicious MCP: schema and size validation, no remote `$ref`, per-server permissions, approval policy, isolated generated servers, OAuth token audience/resource binding.
- Data loss: immutable revisions, Git/checkpoints, transactional migrations, encrypted optional secret backup, tested restore procedure.

## Explicit Non-Guarantees

Containers alone are not claimed to be a complete hostile-code boundary. Production sandbox hardening depends on the configured rootless runtime, kernel, LSM, seccomp policy, egress enforcement, and regular escape testing. A process cannot survive an infrastructure restart unless its runtime did; Gobrowse OS marks such terminal sessions terminated and preserves metadata/output honestly.
