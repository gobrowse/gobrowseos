# MCP

Gobrowse OS targets the current `2026-07-28` MCP protocol and isolates compatibility with the older initialization/session era. Supported architecture includes stdio and Streamable HTTP, version-specific decoding, capability discovery, cancellation, bounded reconnect, tools/resources/prompts, subscriptions, schema limits, and opaque pagination.

`gobrowse mcp doctor` is a Gobrowse diagnostic, not a standardized MCP method. The accepted M8A slice is limited to a count-only, read-only metadata doctor with redacted vault-key readiness; it does not claim transport, protocol, capability, discovery, token, schema, round-trip, reconnect, latency, or permission diagnostics. Any broader diagnostic remains required to redact credentials. See ADR 0006 and the roadmap for implementation status.
