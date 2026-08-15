# PLAN.md — M7 Exhaustive Response-Correlation Core Matrix

## Status, Decision, and Prerequisite

The negotiation-handler failure-boundary correction is accepted at
`f1df7983d672703555e0afaec6c05fe0a6a489d2`. Its exact-SHA CI run
`31905108689` passed Rust (`95061580890`), web (`95061580770`),
supply-chain (`95061580835`), and container (`95061580892`). It proved only
that invalid handler-produced `DiscoverResult` and `InitializeResult` values
reach the redacted `-32603` fallback.

**Decision: the next blocker is remaining transport-neutral core-matrix
evidence, not a transport prerequisite.** Before an adapter is permitted to
accept a peer response, the core must prove that every supported request
method's successful result is accepted under precisely the correlation methods
whose public result schemas it satisfies. `ValidatedResponse::correlate` is
the sole receive-side method/result binding boundary, but its current
cross-method test covers only four of the eleven supported request methods.
The missing coverage includes the typed tool, resource, prompt, and
legacy-initialize result families.

This is the smallest safe next batch: test-only exhaustive evidence for that
existing boundary. It neither implements nor pretends to prove stdio or
Streamable HTTP. Real MCP/OAuth/JWKS infrastructure remains
`BLOCKED_EXTERNAL`; a fixture, loopback server, mock IdP, or mock JWKS client
must not be substituted for that gate.

## Goal

Add one deterministic, complete response-correlation matrix to the wire core.
For each currently supported request method, construct one valid success
result, pass it through the normal response construction and encode/decode
path, prove same-method correlation succeeds with its request ID preserved,
and prove every other supported method accepts or fails exactly according to
its public result schema. The empty-object `ping`/subscribe/unsubscribe and
empty pagination list families are intentionally schema-equivalent; their
cross-method acceptance is expected, while every other cross-method pairing
must fail closed with `WireError::InvalidValue`.

The matrix is specifically the contract an eventual stdio or Streamable HTTP
adapter will consume after it receives a JSON-RPC response. It establishes no
connection, framing, child-process, HTTP, OAuth, or live interoperability
claim.

## Exact Files and Symbols

| File | Production scope | Test-only scope |
|---|---|---|
| `PLAN.md` | Replace the accepted negotiation-correction plan with this bounded next-batch plan. | None. |
| `crates/gobrowse-core/src/mcp/wire.rs` | **None.** `ValidatedResponse::correlate`, `validate_success`, `validated_response_for_method`, `encode`, and `decode` must remain behaviorally and API-identical. | In `wire::tests`, add the exhaustive supported-response correlation matrix and any private test-only fixture/helper needed to keep the cases readable. |

Do not edit `server.rs`, `model.rs`, `capabilities.rs`, `lifecycle.rs`,
`validation.rs`, `mcp.rs`, server routes, or any transport/runtime code.

## Required Test Matrix

Use only the public wire construction/round-trip surface already exercised by
nearby tests: `validated_response_for_method`, `ValidatedMessage::Response`,
`encode`, `decode`, `ValidatedResponse::id`, and
`ValidatedResponse::correlate`. After `decode`, match
`ValidatedMessage::Response(response)` and fail the test on any other variant;
then call `response.id()` and `response.correlate(...)`. Do not call
`validate_success` directly and do not make the test pass through a private
duplicate of production validation.

Define the exact ordered supported-request list once in the test. It contains
all eleven request methods handled by `validate_success`:

1. `METHOD_DISCOVER` with a valid modern `DiscoverResult`: supported version
   `2026-07-28`, empty capabilities, and optional server info omitted.
2. `METHOD_INITIALIZE` with a valid legacy `InitializeResult`: protocol version
   `2025-11-25`, empty capabilities, and non-empty `serverInfo.name` and
   `serverInfo.version`.
3. `METHOD_PING` with exactly `{}`.
4. `METHOD_TOOLS_LIST` with a valid empty `Paginated<Tool>` result.
5. `METHOD_TOOLS_CALL` with a valid empty `ToolCallResult.content` result.
6. `METHOD_RESOURCES_LIST` with a valid empty `Paginated<Resource>` result.
7. `METHOD_RESOURCES_READ` with a valid empty `ResourceReadResult.contents`
   result.
8. `METHOD_RESOURCES_SUBSCRIBE` with exactly `{}`.
9. `METHOD_RESOURCES_UNSUBSCRIBE` with exactly `{}`.
10. `METHOD_PROMPTS_LIST` with a valid empty `Paginated<Prompt>` result.
11. `METHOD_PROMPTS_GET` with a valid empty `PromptGetResult.messages` result.

For every row, the test must:

- build the response with `validated_response_for_method` using a valid,
  bounded request ID;
- encode it and decode it back into a `ValidatedResponse`, asserting the
  response shape and exact request-ID preservation;
- show that `.correlate(row_method)` succeeds after the round trip; and
- iterate the other ten known request methods: cross-method correlation may
  succeed only within the exact bounded schema-equivalent fixture families
  `{}` among `METHOD_PING`, `METHOD_RESOURCES_SUBSCRIBE`, and
  `METHOD_RESOURCES_UNSUBSCRIBE`, and empty paginated `items: []` among
  `METHOD_TOOLS_LIST`, `METHOD_RESOURCES_LIST`, and `METHOD_PROMPTS_LIST`;
  every other cross-method pairing must be exactly
  `Err(WireError::InvalidValue)`.

Add one distinct error-response assertion: a bounded valid `RpcError` with
non-empty message and valid optional data must round-trip with its request ID
preserved and correlate successfully for every method in that same list.
This explicitly records the existing JSON-RPC rule that an error body has no
success-result schema to reinterpret; it must not be broadened into acceptance
of malformed error envelopes.

Keep the existing focused tests
`response_size_and_correlation_are_bounded`,
`all_valid_success_results_reject_cross_method_correlation`, and the three
resource-payload regressions. The new matrix supersedes no test: the former
retains size/fallback boundaries, while the latter retain concise targeted
regressions and failure diagnostics.

## Security and Concurrency Invariants

- Cross-method success is permitted only when equivalence is proven by the
  exact bounded schema-equivalent fixture families: `{}` among
  `METHOD_PING`, `METHOD_RESOURCES_SUBSCRIBE`, and
  `METHOD_RESOURCES_UNSUBSCRIBE`, or empty paginated `items: []` among
  `METHOD_TOOLS_LIST`, `METHOD_RESOURCES_LIST`, and `METHOD_PROMPTS_LIST`.
  Every other cross-method pairing at the untrusted peer boundary MUST fail
  closed with `WireError::InvalidValue`, with no coercion, fallback
  deserialization, or panic.
- The successful response's valid JSON-RPC request ID MUST survive
  `validated_response_for_method` → `encode` → `decode` unchanged. No test
  may bypass the real envelope path or use an unchecked response constructor.
- The matrix must keep all values below `MAX_FRAME_BYTES`; it is evidence for
  existing bounded validation, not permission to allocate boundary-sized
  fixtures.
- An existing valid `RpcError` remains a protocol error independent of the
  request's success schema. Its code, message, optional data, and request ID
  must remain intact; malformed/oversized errors remain covered by the
  existing fail-closed tests.
- This boundary is pure and has no shared dispatcher, network, process, clock,
  filesystem, or task state. The new tests MUST be hermetic and safe to run
  concurrently: no mutable static state, randomized values, test ordering,
  background task, listener, or environment mutation.

## Production/Test Boundary

This batch adds evidence only. It MUST NOT modify production code, public
interfaces, protocol-model schemas, version selection, state transitions,
capability policy, JSON limits, response formatting, or error handling. If the
matrix exposes a behavior defect, stop rather than changing production code in
this test-only batch; the defect requires its own reviewed, narrowly scoped
correction plan.

The evidence is transport-neutral. It cannot establish line framing, child
process lifecycle, EOF/error handling, HTTP request/response or SSE behavior,
redirect/SSRF policy, session headers, authentication, OAuth discovery, JWKS
key validation, cancellation delivery, reconnect I/O, conformance-server
compatibility, or actual stdio/Streamable HTTP interoperability.

## No-goals

- No stdio transport, subprocess spawning, Streamable HTTP client/server,
  HTTP route, SSE parser, conformance harness, daemon, or service.
- No mock transport, loopback TCP/HTTP fixture, fake MCP server, mock OIDC
  provider, fake JWKS client, or simulated external-conformance claim.
- No `gobrowse-server`, `gobrowse-web`, sandbox, database, migration,
  configuration, dependency, credential, routing, feature-flag, or deployment
  change.
- No expansion of supported protocol eras, methods, capabilities, retry
  policy, models, limits, or public API.
- No cleanup/refactor of existing wire or model tests beyond the minimal local
  helper necessary for this matrix.

## Per-part Stop Conditions

### Part 1 — matrix fixture and known-method inventory

Stop and open a new plan if the inventory discovers a request method accepted
by `validate_success` that is absent from the wire constants/list, or a listed
method cannot be represented by a valid bounded model result. Do not silently
omit it, invent a wire shape, or alter a model/schema to make the test pass.
The next plan must identify the exact divergent symbol and normative contract
needed to resolve it.

### Part 2 — normal construction and round trip

Stop if a valid model result accepted by `validated_response_for_method` fails
`encode` or `decode`, changes its ID, or exceeds `MAX_FRAME_BYTES`. That is an
existing wire/envelope defect, not a reason to use `response`, hand-build JSON,
or weaken the matrix. Preserve the fixture and plan a separate production
correction at the failing boundary.

### Part 3 — exhaustive correlation rejection

Stop if a response correlates successfully under a different request method outside
the two explicitly permitted schema-equivalent fixture families: empty `{}` among
`ping`/`resources-subscribe`/`resources-unsubscribe`, or empty `items: []` among
`tools-list`/`resources-list`/`prompts-list`; stop if rejection is not
`WireError::InvalidValue`, or if a valid error response is reinterpreted as a
successful typed result. Do not add an adapter, special-case
the test, or broaden a result schema. Record the exact source and destination
methods and isolate the wire validation defect first.

### Part 4 — scope and external gates

Stop if proving any case requires a dispatcher change, lifecycle/capability
state, network I/O, process spawning, dependency installation, a service, or
external identity infrastructure. Those requirements are out of this test-only
batch. Real stdio and Streamable HTTP remain subsequent work only after this
core evidence is accepted; OAuth/JWKS and real conformance remain
`BLOCKED_EXTERNAL` until live infrastructure is available.

## Disk-safe Validation

The implementation task MUST run no installation, Docker, browser, service,
daemon, formatter, linter, full workspace suite, benchmark, or build-cache
cleanup. With existing artifacts available and free disk strictly above 5 GiB,
run only the focused existing `gobrowse-core` wire test target containing the
new exhaustive matrix, with one build job and no ignored tests. The targeted
run must exercise the added test and the adjacent wire tests it relies on. If
the artifact or disk prerequisite is not met, stop local validation rather than
expanding the cache.

After focused proof and independent review, exact-SHA CI must run the existing
Rust, web, supply-chain, and container jobs. Every job must pass before
accepting this evidence. Passing it closes only this response-correlation
slice; it does not close M7 or permit a claim of real stdio/Streamable HTTP,
OAuth/JWKS, or conformance interoperability.