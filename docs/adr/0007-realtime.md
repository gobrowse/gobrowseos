# ADR 0007: Durable Events with WebSocket Delivery

Status: Accepted

Persist authoritative activity/run/tool events before publishing them. WebSockets deliver bounded incremental updates with sequence cursors and replay after reconnect. In one process Tokio channels provide wakeups; PostgreSQL LISTEN/NOTIFY may wake multiple instances but never replaces durable event rows.
