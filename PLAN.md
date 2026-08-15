# PLAN.md — M7 Bounded Stdio JSON-RPC Framing

## Status, Decision, and External Prerequisite

The exhaustive response-correlation matrix is accepted at
`0724ebf8155229bc46e7320bf8745078e05baf48`. Its exact-SHA CI run
`31906615609` passed Rust, web, supply-chain, and container. That evidence
closes the response-schema matrix slice only; **M7 remains incomplete.**

**Decision: the smallest next production slice is caller-provided-pipe stdio
JSON-RPC line framing.** It adds only a bounded transport over Tokio async
pipes. It does not create a process, establish a session, or make an external
interoperability claim.

The real-peer CI gate remains `BLOCKED_EXTERNAL`. Before this gate can close,
CI must run a non-ignored integration test against a pinned, independent MCP
stdio peer that supports protocol version `2026-07-28`, `server/discover`, and
capability-backed `tools/list`. The test's peer provenance, version, and
content hash must be recorded and verified. A fixture, mock, loopback peer, or
in-repository substitute does not satisfy this requirement.

## Goal

Add `McpStdioTransport<R, W>` over caller-provided Tokio async pipes. It must:

- send a validated JSON-RPC request or notification with the existing `encode`,
  then write exactly one `LF` byte and flush;
- receive exactly one incrementally bounded `LF`- or `CRLF`-terminated
  JSON-RPC payload and validate it with the existing `decode`; and
- return distinct transport outcomes for I/O failure, clean EOF, partial-frame
  EOF, framing over-limit, and `WireError`.

`McpStdioTransport` is a framing boundary, not a protocol/session boundary. A
decoded response remains merely a `ValidatedMessage::Response`; this slice
MUST NOT infer its method or correlate it to a request.

## Exact Files and Symbols

| File | Production scope | Test-only scope |
|---|---|---|
| `PLAN.md` | Records this accepted checkpoint and the bounded stdio framing batch. | None. |
| `crates/gobrowse-core/Cargo.toml` | Promote the existing workspace-pinned `tokio` dependency from `[dev-dependencies]` to `[dependencies]` for production Tokio async I/O; no new crate/package or `Cargo.lock` change. | None. |
| `crates/gobrowse-core/src/mcp.rs` | Declare and export the new `stdio` module only. | None. |
| `crates/gobrowse-core/src/mcp/stdio.rs` | Add `McpStdioTransport<R, W>` and its bounded framing/error API over caller-provided Tokio async pipes. | Unit tests for framing behavior only. |
| `crates/gobrowse-core/src/mcp/wire.rs` | **None.** Reuse `encode`, `decode`, `ValidatedRequest`, `ValidatedNotification`, `ValidatedMessage`, `WireError`, and `MAX_FRAME_BYTES` unchanged. | None. |

Do not alter `server.rs`, `model.rs`, `capabilities.rs`, `lifecycle.rs`,
`validation.rs`, protocol eras, capability policy, or any existing transport
configuration. Do not change `docs/architecture-plan.md` in this batch.

## Transport Contract

`McpStdioTransport<R, W>` owns its caller-provided reader and writer; it does
not open them, spawn a child process, or retain any global/runtime state. Its
implementation is generic over Tokio `AsyncRead` and `AsyncWrite` pipes, with
the usual `Unpin` bounds needed for asynchronous I/O. Construction only wraps
those supplied pipes for bounded incremental reading.

The public sending surface accepts only validated requests and notifications,
not arbitrary JSON or responses. For each send:

1. construct the corresponding `ValidatedMessage`;
2. call the existing `encode` exactly once;
3. write the resulting bytes;
4. write exactly one `b'\n'`; and
5. flush the supplied writer before returning success.

It MUST NOT use raw `serde_json` serialization, add a `CR`, batch multiple
messages, or defer/flout the flush.

The receive surface reads one line incrementally. It recognizes `LF` and
`CRLF`, removes only the terminator, and passes the remaining payload
unchanged to the existing `decode`. It MUST NOT use an unbounded line-read API
or raw `serde_json` decoding. The payload buffer grows only as bytes arrive
and is capped: a payload longer than `MAX_FRAME_BYTES` is a framing
over-limit error, including a line without a terminator. A possible trailing
`CR` may be retained only long enough to distinguish a legal `CRLF`
terminator from payload data; it does not enlarge the permitted decoded
payload.

Define a transport error type that preserves the following distinct cases:

- `Io` for reader, writer, or flush failures;
- `CleanEof` when EOF occurs before any byte of the next frame;
- `PartialEof` when EOF occurs after any non-terminated frame byte;
- `FrameTooLarge` for a frame that exceeds the incremental framing limit; and
- `Wire(WireError)` when `encode` or `decode` rejects an otherwise completely
  framed message.

An empty terminated line reaches `decode` and is therefore a `WireError`; it
is not EOF. A completed `CRLF` frame is valid framing even when its payload
subsequently fails `decode`. Neither clean EOF nor partial EOF is converted
into I/O, malformed JSON, or an invented response.

## Test-only Framing Evidence

The unit tests in `stdio.rs` use in-memory caller-provided async pipes only.
They are test-only framing evidence and MUST NOT be described as external MCP
interoperability. Cover:

1. request and notification sends use existing `encode`, append exactly one
   `LF`, and flush;
2. a valid LF frame and an equivalent CRLF frame each decode through the
   existing `decode`;
3. a frame exactly at `MAX_FRAME_BYTES` is accepted when `decode` accepts it,
   while the first payload byte beyond that limit yields `FrameTooLarge`
   incrementally rather than an unbounded allocation/read;
4. EOF before a frame yields `CleanEof`, while EOF after one or more
   unterminated bytes yields `PartialEof`;
5. malformed or semantically invalid completed JSON-RPC frames preserve the
   underlying `WireError`; and
6. decoding a response neither infers a response method nor performs
   correlation.

Keep these tests deterministic, bounded, and free of process spawning,
listeners, background tasks, clocks, network I/O, or mutable global state.

## No-goals

- No subprocess spawning, child-process lifecycle, shell command, stdio
  endpoint configuration, or process supervision.
- No session driver, initialization/discovery choreography, pending-request
  map, response-method inference, response correlation, background task,
  cancellation, retry, reconnect, timeout, or concurrency policy.
- No Streamable HTTP, HTTP route/client, SSE, authentication, OAuth, JWKS,
  credentials, configuration, external integration, or conformance claim.
- No raw serde decode/encode path, unbounded line read, protocol-model change,
  wire change, capability change, or public-era expansion.
- No fixture/mock/loopback replacement for the real-peer CI prerequisite.

## Stop Conditions and Validation

Stop and open a separate plan if correct incremental framing requires changing
the wire boundary, protocol models, response correlation, session state,
process lifecycle, or Tokio/runtime ownership beyond caller-provided pipes.
Do not work around such a defect with raw serde, a larger/unbounded buffer, a
special-case response, or a hidden background task.

For this implementation, run only focused non-ignored unit tests covering the
new stdio framing module, subject to existing local artifact and disk
prerequisites. After focused proof and review, exact-SHA CI must pass the
existing Rust, web, supply-chain, and container jobs. This validation proves
only local framing behavior; it does not close M7 or the real-peer CI gate.

## Documentation Reconciliation Follow-up

`docs/architecture-plan.md` currently labels M8 as dual-era. Record that as a
documentation reconciliation follow-up after this slice. Do not retag
milestones, change that document now, or widen this bounded stdio framing
batch to resolve the discrepancy.