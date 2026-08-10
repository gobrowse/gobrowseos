# ADR 0006: Versioned MCP Client

Status: Accepted

Implement MCP from the normative specification with explicit wire-version adapters. Prefer the current `2026-07-28` stateless protocol while isolating session-era compatibility. Support stdio and Streamable HTTP, strict schema/size limits, cancellation, bounded reconnect, opaque cursors, and server capability negotiation. OAuth state and tokens live outside agent context. Generated integrations are untrusted until tested and approved.
