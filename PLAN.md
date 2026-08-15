# PLAN.md — M7 Resource Payload Encoding Correction

## Goal

Correct the M7 resource-payload prerequisite before resuming the wire result, correlation, and fallback matrix: a resource payload must never carry both its text and base64 blob encodings.

## Invariant

- `EmbeddedResource` and `ResourceContent` reject a payload when both `text` and `blob` are present with `ValidationError::InvalidValue`.
- A resource payload with one valid encoding remains valid.
- The wire boundary maps this model rejection to the exact `WireError::InvalidValue` for all three result paths that can carry the resource: `resources/read`, `tools/call`, and `prompts/get`.

## Exact Files

| File | Change |
|---|---|
| `PLAN.md` | Replace the prior test-only matrix plan with this bounded correction plan. |
| `crates/gobrowse-core/src/mcp/model.rs` | Add the two fail-closed validation guards and focused model regressions. |
| `crates/gobrowse-core/src/mcp/wire.rs` | Retain the `resources/read` regression and add exact `tools/call` and `prompts/get` wire-boundary regressions. |

## Non-goals

- No transport, runtime, route, database, credential, frontend, dependency, migration, public-interface, or protocol-expansion work.
- No changes outside the three exact files above.
- No broader rewrites of existing result/correlation tests or resource validation.

## Verification Requirements

1. The local implementation task runs no formatter, linter, build, or test commands.
2. Exact-SHA CI MUST run the existing `cargo nextest run` workflow and all configured CI checks MUST pass.
3. The regression set MUST retain the `resources/read` failure case and assert exact `WireError::InvalidValue` for the new `tools/call` and `prompts/get` cases.

## Acceptance Criteria

- Both model validators reject simultaneous text and blob values with `ValidationError::InvalidValue`.
- Focused model tests cover both rejected structs and valid single-encoding controls.
- The three result paths reject simultaneous text and blob values at the wire boundary with exact `WireError::InvalidValue`.
