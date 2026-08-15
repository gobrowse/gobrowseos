# PLAN.md — M7 Typed-Handler Fallback Correction

## Goal

Correct the M7 MCP dispatcher so a semantically invalid *typed handler result* is emitted as the same redacted internal fallback as every other invalid handler output. This is a bounded correction triggered by the regression in `server.rs`; it is not a continuation of the deferred M7 matrix expansion.

## Evidence and Defect

`McpServerDispatcher::dispatch_request` calls `validated_result` for typed method outputs. `validated_result` currently translates a `ValidateMcp` failure into `invalid_params()` (`-32602`, `invalid parameters`). That category is reserved for malformed client parameters; a handler returning duplicate `Tool` identities is an internal handler failure.

The regression test `semantically_invalid_typed_handler_result_uses_internal_fallback` constructs exactly that output after successful modern negotiation. It requires:

- `-32603`;
- message `internal MCP handler error`;
- no error data;
- preserved JSON-RPC request ID;
- an encodable response smaller than `MAX_FRAME_BYTES`.

The current implementation is expected to fail that assertion with `-32602`. No further matrix work may be added until this correction is accepted.

## Scope

### Modify

| File | Change |
|---|---|
| `crates/gobrowse-core/src/mcp/server.rs` | Keep the new regression fixture/test. Map a typed handler result validation failure to the redacted internal-handler error. Add only directly necessary focused coverage. |

### Do not modify

- Any MCP transport, runtime, process, server route, credential, database, frontend, or configuration code.
- `validation.rs`, `wire.rs`, `lifecycle.rs`, `model.rs`, or `capabilities.rs`.
- Dependencies, migrations, public API, docs other than this plan, or the user’s unrelated working-tree changes.

## Design

1. Preserve `Result<T, RpcError>` handler errors unchanged: protocol/client errors returned intentionally by the handler must retain their defined code and shape.
2. Change only the `value.validate_mcp()` failure branch in `validated_result` to return the exact redacted internal-handler `RpcError` used by `safe_internal_error_response`.
3. Keep response serialization failures routed through the existing dispatcher fallback path; do not create a second error protocol.
4. The dispatcher must preserve its negotiation state for this non-negotiation method failure, as it does for the existing fallback path.

## Security and Reliability Invariants

- Handler-invalid output never becomes a client input error or exposes validation details.
- Error responses contain no `data` field and preserve only the request ID.
- Negotiated capabilities remain usable after a failed method result.
- No network, process, credential, database, filesystem, or concurrency behavior is introduced.

## Verification

1. `cargo fmt --check`.
2. `cargo check -p gobrowse-core --tests`.
3. Run the focused regression and the complete workspace suite with `cargo nextest` when the mandated runner is available; do not install `cargo-nextest` locally because of the resource constraint.
4. Require exact-SHA CI covering format, native/WASM Clippy, Nextest, migrations, web build, deny/audit, and container build before accepting this correction.
5. Run independent checker review. Resume the M7 matrix plan only after this correction passes.

## Acceptance Criteria

- The regression fails before and passes after the one-branch classification correction.
- Typed handler validation failures return exactly `-32603`, `internal MCP handler error`, no data, preserved ID, and a bounded encodable frame.
- Intentional handler `RpcError`s retain their existing behavior.
- No out-of-scope file or behavior changes.
