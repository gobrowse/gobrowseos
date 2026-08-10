# Worktrees and Collaboration

Coding tasks receive isolated Git worktrees keyed by workspace, branch, base commit, task, and owner agent. A uniqueness constraint prevents duplicate path/branch ownership. Agents query the durable Activity Ledger before shared changes and can inspect sibling worktrees without editing them.

Events are committed before WebSocket publication. Clients reconnect with the last durable sequence and replay missed events.
