# MCP Authentication

HTTP authorization follows protected-resource metadata and authorization-server discovery. PKCE S256, exact issuer validation, state binding, canonical resource parameters, refresh serialization, audience checks, and bounded scope escalation are mandatory. Dynamic client registration is optional fallback behavior, not the preferred path in the current protocol.

Access/refresh tokens, PKCE verifiers, and client secrets are encrypted or referenced by the Credential Vault. They never enter model context, browser diagnostics, or logs.

The accepted M8A offline policy narrows this boundary further: MCP OAuth vault records carry an explicit vault purpose, and raw MCP metadata is accepted only from one configured host. Vault-key readiness may be reported only in redacted form. This does not establish OAuth discovery, JWKS validation, refresh, PKCE remodeling, same-profile reference migration, or real-provider behavior.
