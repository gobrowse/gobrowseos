# MCP

Gobrowse OS targets the current `2026-07-28` MCP protocol and isolates compatibility with the older initialization/session era. Supported architecture includes stdio and Streamable HTTP, version-specific decoding, capability discovery, cancellation, bounded reconnect, tools/resources/prompts, subscriptions, schema limits, and opaque pagination.

`gobrowse mcp doctor` is a Gobrowse diagnostic, not a standardized MCP method. It must redact credentials and test transport, protocol, capabilities, auth discovery, token status, schemas, safe round trips, reconnect, latency, and permissions. See ADR 0006 and the roadmap for implementation status.
