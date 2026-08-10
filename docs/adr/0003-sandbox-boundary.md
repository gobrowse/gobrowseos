# ADR 0003: Isolated Sandbox Manager

Status: Accepted

The main application never mounts a container runtime socket. An optional, narrowly scoped sandbox manager owns rootless runtime operations and accepts authenticated workspace/process requests only. Containers run unprivileged with all capabilities dropped, bounded CPU/memory/PIDs, explicit writable workspace storage, and restricted networking by default.
