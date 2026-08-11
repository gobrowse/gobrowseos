# ADR 0012: Sandbox Daemon Boundary

## Status

Accepted as the Milestone 4 foundation; feature remains disabled.

## Decision

User-controlled execution is owned only by a separate, non-root `gobrowse-sandboxd` process. The application receives neither a host shell nor a container-runtime socket. App-to-daemon requests use a versioned, bounded newline-delimited JSON protocol over a private Unix socket with peer UID and opaque-token authentication.

The daemon invokes rootless Podman with structured arguments and a fixed isolation baseline. It clears inherited environment variables, drops all capabilities, prohibits host namespaces and privileged mode, uses a read-only base image, and applies CPU, memory, PID, execution-time, writable-storage, global-session, and workspace-session ceilings. There is no host-execution fallback.

`NONE` egress uses no network. `RESTRICTED` requires a separately provisioned network named with the `gobrowse-restricted-` prefix; reserved Podman network modes are rejected. `FULL` remains an explicit daemon-level opt-in. Naming is not proof of enforcement, so runtime egress tests remain a release gate.

Non-idempotent protocol requests have bounded request-ID replay records. A duplicate fingerprint returns the exact prior response; conflicting reuse rejects. Workspace storage must be quota-managed and preprovisioned before a terminal starts.

## Consequences

The daemon can evolve and be tested without increasing the application's privileges. Sandbox endpoints stay unavailable until filesystem operations are descriptor-relative and race-safe, PTY output and lifecycle are durable, restart reconciliation is honest, and rootless runtime security tests pass on the deployment kernel.
