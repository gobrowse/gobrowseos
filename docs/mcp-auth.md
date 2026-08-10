# MCP Authentication

HTTP authorization follows protected-resource metadata and authorization-server discovery. PKCE S256, exact issuer validation, state binding, canonical resource parameters, refresh serialization, audience checks, and bounded scope escalation are mandatory. Dynamic client registration is optional fallback behavior, not the preferred path in the current protocol.

Access/refresh tokens, PKCE verifiers, and client secrets are encrypted or referenced by the Credential Vault. They never enter model context, browser diagnostics, or logs.
