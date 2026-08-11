# ADR 0011: Run leases and lock order

## Status

Accepted for Milestone 3.

## Decision

Conversation turns are durable database work. HTTP handlers enqueue rows and never own execution. Every server runs a bounded scanner that claims available or expired rows with `FOR UPDATE SKIP LOCKED` and writes a new random execution token. The token fences every execution-owned state, event, error, cancellation, and completion write.

Leases are renewed for the entire execution, including context construction and provider connection. Losing a lease stops authoritative publication; it is not itself a run failure. Multiple provider calls may occur after a crash, but only the current token may publish.

Database locks are acquired in this order:

1. Run-event advisory lock.
2. Conversation row.
3. Agent-run rows in UUID order.
4. Conversation projection Book.
5. Embedding-job rows in UUID order.

Callers may begin later in the order when earlier resources are unnecessary, but never acquire an earlier lock after a later one. Async in-memory locks only protect the process-local cancellation registry. They are released before database, network, sleep, WebSocket, or provider awaits.

Browser submissions use a client-generated UUID and a server fingerprint. One conversation transaction either returns the existing matching turn, creates one user message and one run, or rejects a distinct concurrent turn without persisting an orphan message.

## Consequences

- A replacement instance can reclaim expired work without startup coordination.
- A stale instance cannot finalize after takeover.
- Run execution is externally at-least-once and database publication is fenced exactly-once.
- Cancellation and terminal state/event changes are transactionally ordered.
- Conversation deletion and completion must preserve the documented lock order.
