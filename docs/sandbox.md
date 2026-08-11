# Sandbox

Terminal execution is disabled until sandboxd is configured. The app has no host-shell tool and receives no container socket. sandboxd will own rootless runtime operations through a narrow authenticated protocol.

Default policy is restricted egress, unprivileged user, dropped capabilities, no host PID/IPC/network namespace, read-only base, explicit workspace volume, CPU/memory/PID limits, timeout cleanup, seccomp, and LSM compatibility. Cloud metadata, loopback, private ranges, runtime daemons, and host services are denied unless explicitly authorized.

Terminal metadata and output cursors persist. If infrastructure restart kills a process, its state becomes terminated; reconnect does not claim process survival.

## Release Gate

The standalone `gobrowse-sandboxd` protocol and rootless Podman adapter are under active implementation and are not wired to the app. The daemon rejects root execution, host execution fallback, unprovisioned storage, unbounded sessions, reserved Podman network modes, unknown protocol fields, unauthenticated peers, and replay-conflicting request IDs.

Sandbox routes and deployment remain disabled until descriptor-relative filesystem resolution removes symlink races, a preprovisioned restricted network passes metadata/private-range tests, PTY output is durably replayable, daemon restart reconciliation is implemented, and runtime-backed resource/escape tests pass. Unit-level policy checks are not treated as proof of the kernel/runtime boundary.
