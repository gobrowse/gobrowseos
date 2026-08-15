# PLAN.md — M7 Negotiation-Handler Failure Boundary

## Status and Prerequisite

The resource-payload encoding correction is accepted at
`a9b603735b720ee4dfc7e26278f0246f309d9ee2`; its exact-SHA CI run
`31904514287` passed Rust, web, supply-chain, and container jobs.  That
correction remains the baseline: resource payloads with both `text` and
`blob` are already rejected at the model and wire-result boundaries.

M7 is **not** complete.  In particular, this batch is a narrow core
correction before transport work, not evidence of stdio or Streamable HTTP
interoperability.  Real MCP/OAuth/JWKS infrastructure requirements remain
`BLOCKED_EXTERNAL` and must not be replaced with mocks.

## Goal

Make the transport-neutral dispatcher classify invalid **handler-produced
negotiation results** as a redacted server fault, never as client-invalid
parameters.  `invalid parameters` (`-32602`) is reserved for malformed or
invalid caller input; a handler that returns a `DiscoverResult` or
`InitializeResult` failing `ValidateMcp` is an internal fault and must follow
the existing bounded `-32603` fallback path.

This is the smallest missing member of the existing handler-result fallback
family: `validated_result` already protects the typed operational handlers,
and `McpServerDispatcher::dispatch` already provides the bounded fallback
when response validation fails.  It must be corrected before any adapter is
allowed to expose the dispatcher to an untrusted peer.

## Exact Files and Symbols

| File | Production scope | Test-only scope |
|---|---|---|
| `PLAN.md` | Replace the prior accepted correction plan with this one. | None. |
| `crates/gobrowse-core/src/mcp/server.rs` | In `McpServerDispatcher::dispatch_request`, at the `METHOD_DISCOVER` and `METHOD_INITIALIZE` result-validation sites, map a handler result that fails `ValidateMcp` through the existing private `internal_handler_error()` (`-32603`) rather than `invalid_params()` (`-32602`). Do not add a public abstraction, API, or transport type. | Extend `server::tests` with deterministic handlers and focused async regressions for the two negotiation branches. |

`crates/gobrowse-core/src/mcp/wire.rs` is deliberately **not** changed in this
batch.  Its existing `validated_response_for_method` and
`safe_internal_error_response` remain the sole JSON-RPC envelope/fallback
boundary; the dispatcher correction must use that boundary rather than
constructing a second response path.

## Required Behavior and Acceptance Tests

Add focused tests in `server::tests`; use `try_request`,
`McpServerDispatcher::dispatch`, `ResponseBody`, and `encode` rather than
calling private validation helpers directly.

1. A handler whose `discover()` returns a model-invalid `DiscoverResult`
   (for example, duplicate supported versions) yields exactly a JSON-RPC
   error with the request ID preserved, code `-32603`, message
   `internal MCP handler error`, and `data: None`.  The response encodes below
   `MAX_FRAME_BYTES`; `initialized()`, `era()`, and `capabilities()` remain
   pristine.  A subsequent valid negotiation request on the same dispatcher
   must still be permitted, proving rollback rather than poisoned state.
2. A handler whose `initialize()` returns a model-invalid `InitializeResult`
   (for example, an empty server-info identifier) yields that same exact
   redacted `-32603` response and preserves pristine negotiation state.  A
   subsequent valid legacy initialize request must still be permitted.
3. A deliberately returned, already valid handler `RpcError` from each of the
   two negotiation callbacks remains unchanged, including its code, message,
   optional data, and request ID.  This proves the correction does not redact
   intentional protocol errors.
4. Keep the existing semantic negotiation behavior distinct: a structurally
   valid initialization response whose protocol version does not match the
   request remains the existing negotiation error (`-32003`), not an internal
   fallback.  The test must assert no readiness/capability binding.
5. Existing operational-handler fallback tests, including
   `semantically_invalid_typed_handler_result_uses_internal_fallback` and the
   bounded ping/subscription fallback test, remain unchanged in intent and
   pass.  Together with the new two endpoint regressions they cover every
   dispatcher handler-result category: negotiation typed result, operational
   typed result, and `Value` result revalidated at the wire boundary.

## Security Invariants

- Never label handler-controlled output as a caller parameter error.  That
  distinction prevents a remote peer from being blamed for server defects and
  keeps malformed server output on the redacted internal-fault path.
- The fallback must disclose no validation cause, payload, capability set,
  handler error data, or negotiated state; it must retain only the safe
  JSON-RPC request ID and fixed `-32603` message.
- A failed handler result must not bind an era or capabilities, advance the
  session to ready, or poison a pristine dispatcher.  The existing
  `dispatch` rollback remains the atomicity mechanism.
- Preserve intentional, valid `RpcError` responses and valid protocol
  mismatch handling exactly; broad catch-all redaction would be a protocol
  regression.

## Explicit Scope Boundary

### Production change

Only the two `ValidateMcp` error mappings in `server.rs` may change
production behavior.  No changes are permitted to model schemas, wire
formatting, JSON-RPC envelopes, lifecycle state types, capability policy,
server routes, persistence, credentials, HTTP clients, process spawning, or
feature flags.

### Test-only evidence

All other edits in `server.rs` are confined to its `#[cfg(test)]` module.
The tests exercise the transport-neutral core in memory; they are not stdio,
HTTP, OAuth, JWKS, conformance-server, or runtime evidence and must not be
reported as such.

## Non-goals

- No stdio framing/process lifecycle or Streamable HTTP client/server work.
- No local fake transport, mock OIDC provider, mock JWKS client, or simulated
  conformance claim.
- No changes to `gobrowse-server`, `gobrowse-web`, sandbox code, database
  schema/migrations, configuration, dependencies, routes, credentials, or
  documentation.
- No expansion of protocol eras, capabilities, request methods, result
  models, retry policy, or public API.
- No broad rewrite of the core response/correlation matrix.  That remaining
  evidence work is separately planned after this specific failure-class
  correction passes review.

## Stop-to-Correct Conditions

Stop this batch and open a new bounded correction plan instead of widening
scope if any of the following occurs:

1. The two focused tests show that `safe_internal_error_response` cannot
   encode within `MAX_FRAME_BYTES`, cannot preserve a valid request ID, or
   exposes handler-derived data.  That is a wire-boundary defect, not a reason
   to add a second fallback in the dispatcher.
2. Fixing either mapping changes a valid handler `RpcError`, a valid
   mismatched-negotiation `-32003`, or post-failure pristine-state behavior.
   Diagnose and correct the specific state/error boundary before continuing.
3. The change requires a transport, dependency, database, configuration,
   credential, public-interface, or protocol-model edit.  Do not smuggle that
   work into this core batch.
4. A test exposes an invalid operational handler result that bypasses the
   existing `validated_result`/wire fallback family.  Record the exact method
   and stop for a separate exhaustive core-matrix plan; do not generalize
   speculative behavior here.

## Disk-Safe Validation

The implementation task runs no dependency installation, Docker, browser,
permanent daemon, formatter, linter, full workspace suite, or local build.
It must run only the focused existing Rust test target covering
`mcp::server::tests` with one build job and no ignored tests, provided the
repository's existing build artifacts are available and free disk remains
above 5 GiB.  If that prerequisite is not met, stop local validation rather
than consuming cache space.

After focused proof and independent review, exact-SHA CI must run the
repository's existing Rust, web, supply-chain, and container jobs.  All must
pass before accepting this bounded correction.  CI success proves only this
core boundary; M7 remains open pending the later exhaustive core matrix and
real stdio/Streamable HTTP interoperability evidence.