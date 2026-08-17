# PLAN.md — Context Features: Chat Pinning, Workspace Folder, Model Library Tools

Read-only planning batch. Authoritative for the three user-requested context features.
Prior M4/M7/M8 external blocker records remain unchanged and are out of scope for this batch.

## Goal

Implement three bounded context features:

1. **Library → chat pinning** — a user pins Library books to a specific conversation; every run
   of that conversation offers them as `ContextSource::PinnedBook` candidates.
2. **Workspace folder context** — a conversation bound to a workspace gains an ambient
   `ContextSource::Worktree` candidate describing the folders/files the workspace is working on.
3. **Model library search/add tools** — expose two bounded tools (`library_search`, `library_add`)
   to the chat model during a run so it can search the Library (never full load) and add a note
   to it.

## Current State (observed facts)

- `ContextSource::{PinnedBook, Worktree}` already exist in `crates/gobrowse-core/src/context.rs:6-19`;
  they are unused. `build_context` (`context.rs:41`) sorts `required` first, then `priority` desc,
  then `stable_id`; `required` candidates never get omitted.
- `run_api.rs:build_messages` (`:911`) currently emits only one optional candidate class:
  `ContextSource::LibraryRetrieval` at `priority: 300`, `required: false`, body truncated to
  `left(body,3000)`, from a websearch FTS query limited to 5 rows (`:958-992`). No pinned or
  worktree candidates are assembled.
- `run_api.rs:execute_inner` run loop (`:586`) **rejects** `ModelEvent::ToolCall` with
  `("tool_call_unsupported", …)` (`:806`). No tool definitions are attached: `chat::request`
  (`chat.rs:225`) hardcodes `tools: vec![]`.
- `chat.rs:HttpChatProvider::stream` (`:50`) flattens every message to `ChatMessage { role, content }`
  (`:23-52`); `ChatRequest` (`:25`) has **no `tools` field**. `parse_provider_line` (`:332`) parses
  only text/usage/completed — no `tool_calls` parsing.
- `gobrowse_core::model` already has the full surface needed for tools:
  `ToolDefinition { id, description, input_schema }`, `ModelRequest.tools`, `ModelEvent::ToolCall`,
  `ContentPart::{ToolCall, ToolResult}`, `ModelCapability::ToolCalls` (`serde snake_case` → stored as
  `'tool_calls'`), `ModelRoute { provider, identity }` (`model.rs:183`).
- `gobrowse_core::tools` already has the execution abstraction `Tool`, `ToolDescriptor`,
  `ToolContext { call_id, user_id, profile_id, workspace_id, run_id }`, `ToolError` — **zero
  server implementations exist** (grep for `impl Tool` returns nothing).
- `workspaces` table (`0001_initial.sql:47`) has **no root path/folder column** (title, description,
  model_preference, network_policy, sandbox_policy only). Folder data lives on `worktrees`
  (`0001_initial.sql:318`): `path`, `branch`, `base_commit`, `status`, `changed_files text[]`,
  `last_activity_at`, `UNIQUE (workspace_id, path)`.
- `conversations` table has `workspace_id uuid REFERENCES workspaces(id) ON DELETE SET NULL`.
- `books` table (`0001_initial.sql:81`) + `0002` add `owner_user_id`, `created_by_user_id`; scope
  enum includes `WORKSPACE`/`PROFILE`; CHECK `scope <> 'WORKSPACE' OR workspace_id IS NOT NULL`.
- `library_api::search_books` (`:196`) is the authoritative lexical+semantic search with full
  scope/security/workspace authorization predicates; `SearchQuery { q, workspace_id, limit }`
  (`:52`) caps limit at 100, candidates at 300.
- Routes are registered in `crates/gobrowse-server/src/lib.rs:78-161`. Conversation routes:
  `GET/POST /conversations`, `GET/DELETE /conversations/{id}`,
  `GET/POST /conversations/{id}/messages`, `POST /conversations/{id}/fork`,
  `GET /conversations/search`, `POST /conversations/{id}/turns`. Library:
  `GET/POST /library/books`, `GET /library/search`. Worktrees:
  `GET/POST /workspaces/{workspace_id}/worktrees`, `GET/PATCH/DELETE /worktrees/{id}`.
- Schema version currently **18** (`0018_mcp_auth_states_vault_pkce.sql`).
- Frontend is Leptos (`crates/gobrowse-web/src/app.rs`, 2632 lines). `ChatPage` composer at
  `:959` (`<form class="composer" on:submit=send>`), conversation toolbar at `:926`, library
  search dialog at `:481-490` (currently a non-functional placeholder), `LibraryPage` books list
  at `:1278+`.

## Architectural Decisions

1. **PinnedBook priority = 400**, above auto `LibraryRetrieval` (300). Explicit user pinning is
   stronger intent than query-driven FTS retrieval. `required = false` for both, so a full context
   can still omit them rather than overflow.
2. **Worktree priority = 250**, below both. It is ambient structural context, always relevant but
   not query-selected. `required = false`; assembled as one aggregate candidate per workspace with a
   bounded file list.
3. **No `workspaces.root_path` column.** Workspaces do not model a root folder today; adding one is a
   new capability (folder management UI + path validation + storage) beyond the three asks. Folder
   context is derived from the workspace's `worktrees.path` + `changed_files`, which already exist.
4. **Pinning is a new join table** (`conversation_pinned_books`), not a JSON column on conversations,
   so pins get FK integrity, cascade delete, and idempotent upsert.
5. **Tools are native run tools, not MCP.** They reuse `gobrowse_core::tools::Tool`/`ToolContext`
   and are executed in-process by the run loop. MCP server CRUD is unrelated.
6. **`library_search` is lexical FTS only** (reuses `search_document @@ websearch_to_tsquery` +
   `ts_headline` snippets). Semantic/embedding retrieval is out of scope — bounded and dependency-free.
7. **`library_add` hardcodes safe metadata**: `book_type NOTE`, `provenance USER`,
   `trust USER_PROVIDED`, `security_classification INTERNAL`, `scope = WORKSPACE` when the
   conversation has a workspace else `PROFILE`, author = requester display name. The tool can never
   choose `AGENT`/`GLOBAL`/`CONVERSATION`/`AUTOBIOGRAPHY`/`RESTRICTED`.
8. **Tools are offered only when the selected model advertises `tool_calls` capability**; otherwise
   the run degrades gracefully to text-only (no tools). Requires a `supports_tools` flag on
   `ModelRoute`.
9. **Tool messages stay run-internal.** They are emitted as `run_events` + a `tool_calls` row, never
   persisted as durable `messages` rows. The durable transcript keeps only the final assistant text,
   so future `build_messages` reads remain clean text.

## Files To Modify

- `crates/gobrowse-server/migrations/0019_conversation_pinned_books.sql` (new)
- `crates/gobrowse-server/src/lib.rs` (register pin routes)
- `crates/gobrowse-server/src/conversation_api.rs` (pin handlers + access helpers)
- `crates/gobrowse-server/src/library_api.rs` (extract shared lexical search + note-insert helpers)
- `crates/gobrowse-server/src/run_api.rs` (pinned + worktree candidates; tool-call agent loop;
  select `requested_by` in the run row)
- `crates/gobrowse-server/src/run_tools.rs` (new — two `Tool` implementations + tool definitions)
- `crates/gobrowse-server/src/chat.rs` (tools in `ChatRequest`/`ChatMessage`, `tool_calls`
  parsing, `request()` signature, `load_routes` `supports_tools`)
- `crates/gobrowse-core/src/model.rs` (`ModelRoute` gains `supports_tools: bool`)
- `crates/gobrowse-core/src/fake_model.rs` (update 2 `ModelRoute` constructions with the new field)
- `crates/gobrowse-web/src/app.rs` (chat composer pin picker UI)
- Tests: `crates/gobrowse-server/tests/context_features_integration.rs` (new) plus inline
  `#[cfg(test)]` in `run_api.rs`, `chat.rs`, `conversation_api.rs`.

## Database/Migration Changes

`0019_conversation_pinned_books.sql` (single transaction, schema version 19):

```sql
CREATE TABLE conversation_pinned_books (
    conversation_id uuid NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    book_id uuid NOT NULL REFERENCES books(id) ON DELETE CASCADE,
    pinned_by uuid REFERENCES users(id) ON DELETE SET NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (conversation_id, book_id)
);
CREATE INDEX conversation_pinned_books_conversation_idx
    ON conversation_pinned_books (conversation_id, created_at);
UPDATE schema_metadata SET schema_version = 19, updated_at = now() WHERE singleton;
```

No other schema change. No `workspaces.root_path` column.

## API Changes

New routes (auth via `require_user`; `VIEWER` → 403):

- `GET /conversations/{id}/pins` → `Json<Vec<PinnedBookResponse>>`
  `PinnedBookResponse { id, title, book_type, scope, trust, security_classification, updated_at }`
  Read access = conversation reader (member or owner). Ordered by `created_at ASC`.
- `PUT /conversations/{id}/pins/{book_id}` → `204 No Content` (idempotent upsert; re-pin is a no-op).
  Write access = `OWNER`/`EDITOR` on the conversation (reuse `start_turn` access predicate).
- `DELETE /conversations/{id}/pins/{book_id}` → `204 No Content` (idempotent; absent pin is a no-op).

Pin mutation validates the book with the same predicate as `library_api::get_book` (`:205`): same
`profile_id`, `RESTRICTED`/`AGENT` excluded for non-admin, `USER`/`PRIVATE` require `owner_user_id`,
`WORKSPACE`/`PROJECT` require membership, `CONVERSATION` requires conversation access. Foreign keys
cascade on conversation/book delete.

## Backend Changes

### Feature 1 — pinned candidates (`run_api.rs:build_messages`)

After the `workspace_id` lookup, run:

```sql
SELECT b.id, b.title, left(b.body, 3000) AS body, b.trust
FROM conversation_pinned_books p JOIN books b ON b.id = p.book_id
WHERE p.conversation_id = $1
ORDER BY p.created_at ASC LIMIT 20
```

Emit one `ContextCandidate` per row: `source: PinnedBook`, `stable_id: book id`,
`content: "Book: {title}\n{body}"`, `priority: 400`, `required: false`, `trust_label: trust`.
Push these into the same `candidates` vec as LibraryRetrieval before `build_context`.

### Feature 2 — worktree candidate (`run_api.rs:build_messages`)

When `workspace_id.is_some()`:

```sql
SELECT id, branch, base_commit, path, status, changed_files, last_activity_at
FROM worktrees WHERE workspace_id = $1 ORDER BY last_activity_at DESC LIMIT 10
```

Emit **one** aggregate candidate: `source: Worktree`,
`stable_id: format!("worktree-{workspace_id}")`, `priority: 250`, `required: false`,
`trust_label: "workspace"`, content = a compact text block listing each worktree
(`path`, `branch`, `base_commit`, `status`) plus `changed_files` truncated to the first 50 paths
(respecting `gobrowse_core::worktrees::MAX_CHANGED_FILES_BYTES`). Zero worktrees → no candidate.

### Feature 3 — tool loop

1. `run_api.rs:execute_inner` row query (`:599`) additionally selects `run.requested_by`.
2. Build `Vec<ToolDefinition>` from `run_tools::tool_definitions()`.
3. `chat::load_routes` (`chat.rs:138`) gains `supports_tools` per route from
   `'tool_calls'=ANY(m.capabilities)`; `ModelRoute.supports_tools: bool`.
4. `chat::request(messages, tools, max_output_tokens)` — signature change; populate
   `ModelRequest.tools` and `required_capabilities: BTreeSet::from([ModelCapability::ToolCalls])`
   when `tools` is non-empty.
5. In `execute_inner`, if any route supports tools, `routes.retain(|r| r.supports_tools)` and attach
   tools; if none, run text-only (empty tools, no tool loop).
6. Replace the `ToolCall => Err(tool_call_unsupported)` arm with a bounded agent loop:
   - maintain the in-flight `messages: Vec<NeutralMessage>` (system policy + retrieved + recent);
   - on `ToolCall { id, name, input }`: transition `running_tool`, validate the name against the two
     known tools, execute via `run_tools` with a 10 s timeout, append `run.tool_call` /
     `run.tool_result` events and a `tool_calls` row (existing table `0001_initial.sql:300`), append
     an assistant `ContentPart::ToolCall` + a `MessageRole::Tool` `ContentPart::ToolResult`, then
     re-invoke the stream with the extended messages under the same lease;
   - hard bounds: max 8 tool rounds per run, max 16 total tool calls, per-call 10 s timeout,
     64 KiB max tool output; exceeding any bound → `fail_run("tool_limit", …)`;
   - `check_canceled` + `check_execution_access` are already called between provider events
     (`:757-761`), so the loop stays cancellable.

### `run_tools.rs` (new module)

Two `impl Tool` structs using existing `ToolDescriptor`/`ToolContext`/`ToolError`. Map
`ToolDescriptor → model::ToolDefinition` (id, description, input_schema).

- `library_search`
  - description: "Search the profile Library for books matching a query. Returns bounded summaries
    (id, title, snippet, scope, trust). Never returns full book content."
  - input_schema: `{"type":"object","properties":{"q":{"type":"string","minLength":1,
    "maxLength":1000},"workspace_id":{"type":["string","null"],"format":"uuid"},
    "limit":{"type":"integer","minimum":1,"maximum":20,"default":10}},"required":["q"],
    "additionalProperties":false}`
  - execution: reuse a new `pub(crate) library_api::lexical_search(...)` extracted from the lexical
    half of `search_books` (`:210-250`) parameterized by `(profile_id, workspace_id, role, user_id,
    q, limit)`, returning `ts_headline` snippets only (no `body`). Bounded to `limit ≤ 20`.
- `library_add`
  - description: "Create a NOTE in the Library. The note is user-authored and scoped to the current
    workspace (or profile). Returns the new book id."
  - input_schema: `{"type":"object","properties":{"title":{"type":"string","minLength":1,
    "maxLength":512},"body":{"type":"string","minLength":1,"maxLength":100000},
    "tags":{"type":"array","items":{"type":"string","maxLength":100},"maxItems":32}},
    "required":["title","body"],"additionalProperties":false}`
  - execution: new `pub(crate) library_api::create_library_note(pool, profile_id, workspace_id,
    author_user_id, author_name, title, body, tags)` that performs the same INSERT/chunks/revision/
    `embedding::enqueue_book`/audit as `create_book` (`:140-193`) but with hardcoded safe
    `book_type/scope/trust/provenance/classification` per Decision 7. Rejects empty/oversized input
    as `ToolError::InvalidInput`; database failure as `ToolError::Execution`.

## Frontend Changes (`crates/gobrowse-web/src/app.rs`)

- In `ChatPage`, add a `pinned_books: RwSignal<Vec<PinnedBookSummary>>` loaded on
  `open_conversation` via `GET /conversations/{id}/pins`.
- Add a "Pin books" button in the conversation toolbar (`:926`) opening a picker `<dialog>` (reuse
  the existing search-dialog pattern at `:481`); the picker lists Library books via
  `GET /library/search` and toggles `PUT/DELETE /conversations/{id}/pins/{book_id}`.
- Render pinned-book chips above the composer (`:959`) showing each pinned title, so the user sees
  what will be offered as PinnedBook context. Reuse `BookSummary` fields already defined at `:72`.

## Security Requirements

- Pin reads/mutations and both tools inherit the existing conversation/workspace access predicates;
  no new bypass. `VIEWER`/non-member → 403.
- Pinned books and `library_search` must never surface `RESTRICTED`/`AGENT`/private books to
  unauthorized users — reuse the exact `get_book`/`search_books` predicates.
- `library_search` returns snippets only; never `body`. FTS `websearch_to_tsquery` is already
  parameterized (no SQL injection).
- `library_add` cannot set provenance/trust/scope/classification or book_type from the model — they
  are server-hardcoded. Output is capped at 64 KiB.
- Tool input is schema-validated (deny unknown fields) before execution; invalid → `ToolError`.
- Tool execution runs with the requester's identity (`requested_by`), never an ambient privilege.

## Concurrency Requirements

- The tool loop runs inside the existing run lease and single active-run-per-conversation invariant
  (`one_active_conversation_turn`, `0003_chat_runs.sql`). No new locks.
- Re-entrant model calls reuse the same `RunLease`/`execution_token`; `check_canceled` and
  `check_execution_access` gate every round and are already wired into the event select loop.
- `tool_calls` rows use unique `idempotency_key` (existing table constraint) to avoid double-insert
  on retry.

## Tests Required

- Migration (`context_features_integration.rs`): FK cascade on conversation/book delete, PK
  dedupe on re-pin, schema version 19, `GOBROWSE_TEST_DATABASE_URL` skip behavior.
- Pins API: pin/unpin idempotency; list ordering; 403 for VIEWER/non-member; cross-profile or
  `RESTRICTED` book rejected on pin; cascade cleanup.
- Context assembly (inline in `run_api.rs`): PinnedBook candidates appear with `source=PinnedBook`
  and rank above `LibraryRetrieval` (priority 400 > 300); Worktree candidate present only when
  workspace has worktrees; omitted (not sliced) when over budget.
- Tools (`run_tools.rs`): schema rejects unknown fields/oversized body; `library_search` returns only
  summaries (assert no body field) and respects scope/security; `library_add` persists a NOTE with
  correct provenance/trust/scope/classification and rejects `AGENT`/`RESTRICTED`.
- `chat.rs`: OpenAI-compatible `tool_calls` delta aggregation into `ModelEvent::ToolCall`; Ollama
  `message.tool_calls` parsing; `tool`-role/assistant-tool-call message serialization; tools omitted
  from wire when empty.
- Run loop: bounded rounds (8) and total calls (16) enforced; cancellation honored between rounds;
  graceful text-only degradation when no route supports `tool_calls`.

## Deployment Considerations

- One additive migration (19). No destructive column/constraint changes; `ACCESS EXCLUSIVE` not
  required beyond the new table DDL.
- `ModelRoute` gains a field → `gobrowse-core` public API change; update `fake_model.rs` and any
  `ModelRoute` constructors in one commit.
- Tool-capable models must advertise `tool_calls` in `models.capabilities` (`model_api.rs`); models
  created without it simply run text-only — no forced reconfiguration.
- No new dependencies.

## Ordered Implementation Steps

1. Migration `0019` + schema-version test.
2. `conversation_api.rs` pin handlers + `lib.rs` routes + tests.
3. `run_api.rs` pinned + worktree candidates + tests.
4. `model.rs` `ModelRoute.supports_tools` + `fake_model.rs` + `chat.rs` `load_routes`/`request`
   signature (tools param) — compile clean first.
5. `chat.rs` wire-format: `ChatRequest.tools`, `ChatMessage` tool fields, `tool_calls` parsing + tests.
6. `run_tools.rs` two tools + extracted `library_api` helpers + tests.
7. `run_api.rs` agent tool loop + tests.
8. `app.rs` pin picker UI.
9. Full `cargo test` (server + core) with warnings denied; smoke-test a pinned conversation and a
   tool-capable model run end-to-end.

## Acceptance Criteria

- Pinning a book to a conversation makes it appear as a `PinnedBook` candidate (priority 400) in
  every run of that conversation, ranked above auto retrieval; unpinning removes it.
- A workspace-bound conversation gains a `Worktree` candidate listing its worktrees + changed files,
  omitted cleanly when it would overflow the budget.
- `library_search` returns bounded snippets (≤20, no body) and `library_add` creates a user-scoped
  NOTE, both invoked only when the selected model advertises `tool_calls`; non-tool models run
  text-only with zero regression.
- All existing run/lease/cancellation invariants hold; no new migration beyond 19; no new deps.

## Risks / Stop Conditions

- **Provider tool wire-format divergence**: OpenAI-compatible (`delta.tool_calls[]` accumulation +
  `finish_reason: "tool_calls"`) vs Ollama (`message.tool_calls[]`) differ. Implement
  OpenAI-compatible first (primary provider); if Ollama's native tool format cannot be reconciled
  within the batch, gate tools to OpenAI-compatible routes and record Ollama as a documented gap —
  do not fabricate an Ollama mapping.
- **`content_text` flattening loses tool structure**: current `ChatMessage` is flat text. The
  serialization path must be structural (assistant `tool_calls` + `tool`-role messages); a flat
  fallback silently drops tool context. Blocked here → stop and report.
- **Model capability casing**: `ToolCalls` serde is `snake_case` → DB string must be `tool_calls`;
  verify against `model_api.rs` inserts before gating.
- **Unbounded agent loop**: enforced round/total/timeout/output caps are mandatory; a missing cap is a
  stop condition.
- **Semantic search scope creep**: do not add embedding retrieval to `library_search`; lexical only.
- **Root-path scope creep**: do not add `workspaces.root_path`; worktree-derived folder context only.
