---

## Backend Batch (Product UX) — Streaming Speed, Default Embedding, Autobiography Auto-Update

**Status**: plan only — do not implement. Coder agent will implement from this plan.

**Schema version**: 25 (post-M25a migration). This batch adds no new migrations
(no schema change; see Item 5 durability verification below).

---

### Item 1: RUN_ID CONTEXT FIX — Verify Only (already landed)

**Finding**: The fix is already live in `crates/gobrowse-server/src/run_api.rs:426-434`
(`get_run_context`):

```rust
let mut value = snapshot;
if value.get("run_id").is_none() {
    value["run_id"] = serde_json::json!(id);
}
Ok(Json(value))
```

When the snapshot has no `run_id`, the endpoint merges the run_id from the URL path
before returning the JSON. The fallback `None` case also returns `{ "run_id": id }`.

**Verification**: Run existing test suite; no new failures. Optionally hit
`GET /runs/{id}/context` on a run whose `context_snapshot` column lacks a `run_id`
field — the response MUST include it.

**No plan needed.** This is confirmed done.

---

### Item 2: STREAMING SPEED — Audit & Speedup

#### Current Path (observed)

File: `crates/gobrowse-server/src/run_api.rs`

1. Model streams text deltas via `ModelEvent::TextDelta` in `execute_inner` (line ~870)
2. Each delta appends to `pending_delta: String`
3. `flush_text_events` is called on **every delta** with `force: false` (line ~888)
4. A `delta_flush` timer fires every **100 ms** with `force: true` (line ~860)
5. `flush_text_events` (line ~1210) splits `pending_delta` into **1 KB chunks** (`DELTA_EVENT_BYTES = 1024`)
6. Each chunk calls `append_event_owned` — a **single INSERT** per chunk

#### Concrete Issues

| Issue | Current | Impact |
|-------|---------|--------|
| Per-chunk DB round-trip | 1 INSERT per 1 KB | For a 4 KB response: 4 round-trips |
| Tiny chunk size | 1024 bytes | Amplifies INSERT count |
| No batching | Individual INSERTs | No amortization of DB protocol overhead |
| Double-flush window | Both delta-callback and 100ms timer trigger writes | Redundant flushes when stream is fast |

#### Speedup Patches

**A. Batch INSERT — `append_event_owned` → `append_events_batched`**

Add a new function in `run_api.rs` (adjacent to `append_event_owned`, line ~1904):

```rust
/// Batch-insert multiple run events in one round-trip.
async fn append_events_batched(
    pool: &PgPool,
    run_id: Uuid,
    lease: RunLease,
    events: &[(String, serde_json::Value)],
) -> Result<(), sqlx::Error> {
    // Multi-row INSERT using UNNEST or repeated VALUES
    sqlx::query(
        "INSERT INTO run_events (run_id, event_type, payload, profile_id, created_at) \
         SELECT $1, t.event_type, t.payload, $2, now() \
         FROM unnest($3::text[], $4::jsonb[]) AS t(event_type, payload)"
    )
    .bind(run_id)
    .bind(lease.profile_id)
    .bind(events.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>())
    .bind(events.iter().map(|(_, p)| p).collect::<Vec<_>>())
    .execute(pool)
    .await
    .map(|_| ())
}
```

Or simpler with multiple VALUES rows if UNNEST causes type issues:

```rust
// Build a dynamic VALUES clause
let mut query = String::from(
    "INSERT INTO run_events (run_id, event_type, payload, profile_id) VALUES "
);
let params: Vec<String> = events.iter().enumerate()
    .map(|(i, _)| format!("($1,$2{}::text,$2{}::jsonb,$3)", i*2+2, i*2+3))
    .collect::<Vec<_>>();
query.push_str(&params.join(","));
```

**B. Increase `DELTA_EVENT_BYTES`** — `run_api.rs:1217`

Change `const DELTA_EVENT_BYTES: usize = 1024` → `4096` (or `8192`).
Reduces event count proportionally. Safe because `content->>'text'` is plaintext
and `MAX_DELTA_EVENTS = 8192` already provides an upper bound.

**C. Flush in batches in `flush_text_events`** — `run_api.rs:1210-1242`

Rewrite `flush_text_events` to accumulate chunks then batch-insert:

```rust
async fn flush_text_events(
    pool: &PgPool,
    run_id: Uuid,
    lease: RunLease,
    pending: &mut String,
    event_count: &mut usize,
    force: bool,
) -> Result<(), (&'static str, &'static str)> {
    const DELTA_EVENT_BYTES: usize = 4096;
    const MAX_DELTA_EVENTS: usize = 8192;
    const BATCH_SIZE: usize = 8; // up to 8 events per INSERT

    let mut batch: Vec<(String, serde_json::Value)> = Vec::with_capacity(BATCH_SIZE);
    
    while pending.len() >= DELTA_EVENT_BYTES || (force && !pending.is_empty()) {
        if *event_count >= MAX_DELTA_EVENTS {
            return Err(("response_fragment_limit", "..."));
        }
        let text = take_text_chunk(pending, DELTA_EVENT_BYTES);
        batch.push(("model.text_delta".into(), serde_json::json!({"text": text})));
        *event_count += 1;
        if batch.len() >= BATCH_SIZE {
            append_events_batched(pool, run_id, lease, &batch)
                .await
                .map_err(database_failure)?;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        append_events_batched(pool, run_id, lease, &batch)
            .await
            .map_err(database_failure)?;
    }
    Ok(())
}
```

**D. Remove redundant delta callback flush** — `run_api.rs:~888`

Currently, every `TextDelta` calls `flush_text_events(force: false)`. Since the
100ms `delta_flush` timer already covers this, and `force: false` only flushes
when `pending.len() >= DELTA_EVENT_BYTES`, we can remove the inline call and rely
solely on the timer. Protects against double-write interleaving.

```rust
// BEFORE (inline call on every delta):
ModelEvent::TextDelta { text } => {
    output.push_str(&text);
    round_output.push_str(&text);
    pending_delta.push_str(&text);
    flush_text_events(... force: false).await?;  // REMOVE THIS
}

// AFTER:
ModelEvent::TextDelta { text } => {
    output.push_str(&text);
    round_output.push_str(&text);
    pending_delta.push_str(&text);
    // Flushed by delta_flush timer or Usage/Completed events
}
```

The `ModelEvent::Usage` and `ModelEvent::Completed` handlers already call
`flush_text_events(force: true)` — no text is lost.

**E. Prefetch conversation** — optional, low-priority

In `execute_inner`, the `build_messages` call happens before the model stream
opens. No further optimization needed; the prefetch is already done.

#### Verification

1. Run existing `concurrency_tests` in `run_api.rs` — no regressions.
2. Manual: start a run with a known response size (~2 KB), count `model.text_delta`
   events in `run_events` — expect fewer events with 4 KB chunking.
3. Benchmark: time from first delta to last delta write for a ~10 KB response.
   Target ≤ 50% of current round-trip count.

#### Files Modified
- `crates/gobrowse-server/src/run_api.rs`:
  - New `fn append_events_batched` (alongside `append_event_owned`, line ~1904)
  - `flush_text_events`: DELTA_EVENT_BYTES 1024→4096, add batch accumulation
  - `execute_inner`: remove inline `flush_text_events(force: false)` on `TextDelta` (line ~888)

---

### Item 3: DEFAULT EMBEDDING MODEL — qwen/qwen3-embedding-4b via OpenRouter

#### Current State

- `profiles.active_embedding_model_id` (`TEXT REFERENCES embedding_models(id)`) — nullable
- `embedding_models` rows reference a `provider_id` in `providers`
- `providers.provider_type` expected: `"openai_compatible"` (OpenRouter uses OpenAI-compatible embeddings endpoint at `https://openrouter.ai/api/v1/embeddings`)
- Provider auto-detection in `model_api.rs::detect_providers_from_env` already reports `openrouter` as available when `OPENROUTER_API_KEY` is present (line ~443-444)
- But no embedding model is auto-created — the profile stays with `active_embedding_model_id = NULL`
- `embedding.rs::load_provider` maps `provider_type == "openai_compatible"` → `POST {base}/embeddings` with OpenAI-format request/response (line 157-165)
- `pgvector` column `book_chunk_embeddings.embedding vector` is variable-dimension; validated per-row via `CHECK (vector_dims(embedding) = dimensions)`. No dimension constraint in schema — any dimension works

#### qwen/qwen3-embedding-4b (OpenRouter)

| Property | Value |
|----------|-------|
| Model ID (OpenRouter) | `qwen/qwen3-embedding-4b` |
| Provider type | `openai_compatible` |
| Base URL | `https://openrouter.ai/api/v1` |
| Dimensions | **2048** |
| API key | `OPENROUTER_API_KEY` env var (already detected) |

The embedding endpoint is `POST https://openrouter.ai/api/v1/embeddings` with
standard OpenAI-compatible JSON body. The current `embedding.rs` code path for
`provider_type == "openai_compatible"` handles this directly.

#### Implementation

**Step 1: Add auto-seed logic** — new function in `crates/gobrowse-server/src/embedding.rs`:

```rust
/// Create a default OpenRouter embedding configuration (qwen3-embedding-4b)
/// when OPENROUTER_API_KEY is available and no embedding model exists yet.
/// Idempotent: skips if an active_embedding_model_id is already set or the
/// provider+model rows already exist.
pub async fn seed_default_embedding_model(
    pool: &PgPool,
    profile_id: Uuid,
) -> Result<(), sqlx::Error> {
    const DEFAULT_PROVIDER_ID: &str = "provider_openrouter_default";
    const DEFAULT_MODEL_ID: &str = "model_qwen3_embedding_4b";
    const DEFAULT_MODEL_REF: &str = "qwen/qwen3-embedding-4b";
    const DEFAULT_DIMENSIONS: i32 = 2048;

    // Skip if OpenRouter key is not in environment
    if std::env::var("OPENROUTER_API_KEY").is_err() {
        return Ok(());
    }

    // Skip if active embedding model already set
    let has_active: bool = sqlx::query_scalar(
        "SELECT active_embedding_model_id IS NOT NULL FROM profiles WHERE id=$1"
    )
    .bind(profile_id)
    .fetch_one(pool)
    .await?;
    if has_active {
        return Ok(());
    }

    // Upsert provider row
    sqlx::query(
        "INSERT INTO providers (id, profile_id, provider_type, display_name, base_url, enabled) \
         VALUES ($1, $2, 'openai_compatible', 'OpenRouter Embeddings', 'https://openrouter.ai/api/v1', true) \
         ON CONFLICT (id) DO UPDATE SET enabled=true, updated_at=now()"
    )
    .bind(DEFAULT_PROVIDER_ID)
    .bind(profile_id)
    .execute(pool)
    .await?;

    // Upsert embedding_model row
    sqlx::query(
        "INSERT INTO embedding_models (id, provider_id, model_reference, dimensions, enabled) \
         VALUES ($1, $2, $3, $4, true) \
         ON CONFLICT (provider_id, model_reference) DO UPDATE SET enabled=true"
    )
    .bind(DEFAULT_MODEL_ID)
    .bind(DEFAULT_PROVIDER_ID)
    .bind(DEFAULT_MODEL_REF)
    .bind(DEFAULT_DIMENSIONS)
    .execute(pool)
    .await?;

    // Activate
    sqlx::query("UPDATE profiles SET active_embedding_model_id=$1, updated_at=now() WHERE id=$2")
        .bind(DEFAULT_MODEL_ID)
        .bind(profile_id)
        .execute(pool)
        .await?;

    Ok(())
}
```

**Step 2: Call from profile bootstrap** — `crates/gobrowse-server/src/auth.rs` (or wherever
the first profile is created). The current bootstrap path is in `auth.rs` around line ~248
where the first profile + autobiography book are created:

```rust
// After creating the first profile and autobiography book (auth.rs:~248):
embedding::seed_default_embedding_model(&state.pool, profile_id).await?;
```

**Step 3: Also call on `/api/v1/embedding/configurations` GET** — `embedding_api.rs`.
Add auto-seed to `list_configurations` so that any profile querying embedding configs
gets the default created on first access (lazy fallback):

```rust
// In list_configurations, before the SELECT:
embedding::seed_default_embedding_model(&state.pool, user.profile_id).await?;
```

#### Verification

1. Unit test: `seed_default_embedding_model` called with no `OPENROUTER_API_KEY` →
   no-op, no error.
2. Integration test: with `OPENROUTER_API_KEY=sk-test` set:
   - Call `seed_default_embedding_model` → provider + embedding_model rows exist
   - `profiles.active_embedding_model_id` = `"model_qwen3_embedding_4b"`
   - Dimensions = 2048
   - `embed_query` succeeds (lexical fallback if network unavailable)
   - Second call is idempotent (no duplicates)
3. `embedding.rs::load_provider` with the seeded model → `provider_type = "openai_compatible"`,
   `model_reference = "qwen/qwen3-embedding-4b"`, `dimensions = 2048` — all match.

#### Files Modified
- `crates/gobrowse-server/src/embedding.rs`: add `pub async fn seed_default_embedding_model`
- `crates/gobrowse-server/src/auth.rs`: call `seed_default_embedding_model` after bootstrap
- `crates/gobrowse-server/src/embedding_api.rs`: call in `list_configurations` as lazy seed

#### No Migration Needed
- `pgvector` `vector` type supports any dimension; `dimensions` column already matched per-row
- No constraint changes; 2048-dimensional vectors work with existing schema

---

### Item 4: AUTOBIOGRAPHY AUTO-UPDATE — Post-Run Summarization

#### Design

After each completed conversation run (policy `'automatic'`), the system summarizes
new facts/decisions into the user's Autobiography book. The update is **async fire-and-forget**
— it never blocks or fails the run.

#### Trigger Point

In `run_api.rs::complete_run`, after `tx.commit()` succeeds (line ~1685).
Add at the end of the function, before `Ok(())`:

```rust
// After tx.commit().await? and rebuild_projection completes:
if let Err(error) = autobiography::auto_update_after_run(
    state,
    lease.profile_id,
    requested_by,
    conversation_id,
    &output,
).await {
    warn!(%run_id, %error, "autobiography auto-update failed");
}
```

Must NOT be inside the transaction — it starts its own transaction after the run
transaction commits.

#### New Module: `crates/gobrowse-server/src/autobiography_update.rs`

```rust
pub async fn auto_update_after_run(
    state: &AppState,
    profile_id: Uuid,
    user_id: Uuid,
    conversation_id: Uuid,
    assistant_output: &str,
) -> Result<(), AppError> {
    // 1. Check policy
    let policy: String = sqlx::query_scalar(
        "SELECT autobiography_update_policy FROM profiles WHERE id=$1"
    )
    .bind(profile_id)
    .fetch_one(&state.pool)
    .await?;
    if policy != "automatic" {
        return Ok(());
    }

    // 2. Fetch existing autobiography
    let book: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT id, body FROM books WHERE profile_id=$1 AND book_type='AUTOBIOGRAPHY'"
    )
    .bind(profile_id)
    .fetch_optional(&state.pool)
    .await?;
    let (book_id, current_body) = match book {
        Some(b) => b,
        None => return Ok(()),
    };

    // 3. Fetch recent user message for context
    let user_message: Option<String> = sqlx::query_scalar(
        "SELECT content->>'text' FROM messages \
         WHERE conversation_id=$1 AND role='user' \
         ORDER BY ordinal DESC LIMIT 1"
    )
    .bind(conversation_id)
    .fetch_optional(&state.pool)
    .await?
    .flatten();
    let user_message = user_message.unwrap_or_default();

    // 4. Build prompt
    let prompt = build_auto_update_prompt(&current_body, &user_message, assistant_output);

    // 5. Call cheapest chat model
    let summary = match summarize_with_model(state, profile_id, &prompt).await {
        Ok(s) => s,
        Err(e) => {
            warn!(%profile_id, %e, "autobiography summarization failed");
            return Ok(());
        }
    };

    // 6. Guardrails
    let summary = sanitize_secrets(&summary);
    let summary = deduplicate_content(&current_body, &summary);
    if summary.is_empty() {
        return Ok(());
    }

    // 7. Merge into body
    let max_body = 99_000; // leave headroom below 99999 CHECK constraint
    let new_body = merge_body(&current_body, &summary, max_body);

    // 8. Write in transaction
    let mut tx = state.pool.begin().await?;
    let reason = format!("Autobiography auto-update from conversation {}", conversation_id);
    library_api::replace_book_body(&mut tx, book_id, &new_body, Some(user_id), &reason).await?;
    tx.commit().await?;

    Ok(())
}
```

#### Prompt Design

```text
You maintain a concise Autobiography for the user. Below is the current
Autobiography plus the most recent conversation. Extract ONLY genuinely new facts
or decisions about the user — preferences, important context, decisions made,
explicit instructions, identity details. Do NOT include:

- Restatements of existing autobiography content
- Transient chat details (small talk, greetings, task-specific trivia)
- System instructions, prompts, or tool outputs
- Any content resembling API keys, passwords, tokens, or secrets
- Code snippets or technical implementation details

Output ONLY the new facts as 1-3 bullet points. If nothing is genuinely new,
output the single word: NOTHING.

=== CURRENT AUTOBIOGRAPHY ===
{current_body}

=== USER MESSAGE ===
{user_message}

=== ASSISTANT RESPONSE ===
{assistant_output}
```

#### Guardrails

| Guardrail | Implementation |
|-----------|---------------|
| **Size cap** | `max_body = 99_000` chars; if merged body exceeds this, truncate oldest content with `…(trimmed)…` marker at the boundary |
| **Secrets** | `sanitize_secrets()` regex: redact patterns matching `sk-[a-zA-Z0-9]{20,}`, `Bearer\s+\S+`, `-----BEGIN.*-----`, any line matching known vault reference patterns |
| **Dedupe** | `deduplicate_content()`: split current body into sentences, compute Jaccard similarity on word trigrams. If summary sentences have >0.7 overlap with existing, drop them |
| **No-op** | If summary is `"NOTHING"` or empty after sanitization, return without writing |
| **Cost** | Use cheapest available chat model (lowest `cost_ranking` with `text` capability) |
| **Retry** | No retry — single best-effort attempt per run. Failures are logged, never surfaced |

#### Transaction Design

- `auto_update_after_run` runs OUTSIDE the run completion transaction
- It opens its own transaction for the book update
- Uses `library_api::replace_book_body` which:
  - Bumps `books.revision`
  - Appends `book_revisions` row (immutable via trigger)
  - Re-chunks and enqueues embedding
  - Uses `FOR UPDATE` row lock on the book

#### Tests

1. **Policy gating**: call `auto_update_after_run` with policy=`"propose"` → no update, no error
2. **Empty summary**: model returns `"NOTHING"` → no update, no error
3. **Secret redaction**: summary contains `sk-abc123def456` → stripped before merge
4. **Dedupe**: summary duplicates existing content → stripped, returns empty → no update
5. **Size cap**: body at 99,500 chars after merge → truncated to 99,000 with marker
6. **Revision bump**: successful update → book revision increments, book_revisions row added, embedding job enqueued
7. **No autobiography book**: profile has no AUTOBIOGRAPHY book → no-op, no error

#### Files Modified
- `crates/gobrowse-server/src/autobiography_update.rs` — new module
- `crates/gobrowse-server/src/lib.rs` — register `mod autobiography_update`
- `crates/gobrowse-server/src/run_api.rs` — call `auto_update_after_run` after `tx.commit()` in `complete_run` (line ~1685)

---

### Item 5: CHAT PERSISTENCE — Confirm Durability (No Change)

**Finding**: All conversation data is durably stored in PostgreSQL. Confirmed:

| Data | Storage | Survives Restart |
|------|---------|------------------|
| Conversation metadata | `conversations` table (title, workspace_id, status, timestamps) | ✅ PK `id` persisted in PostgreSQL |
| Messages | `messages` table (id, conversation_id, ordinal, role, content jsonb, provider, model, usage, agent_run_id) | ✅ UNIQUE `(conversation_id, ordinal)`, foreign keys to conversations |
| Agent runs | `agent_runs` table (id, conversation_id, state, step, context_snapshot, timestamps) | ✅ indexed by profile_id, conversation_id |
| Run events (deltas, tool calls, etc.) | `run_events` table (sequence bigserial, run_id, event_type, payload, created_at) | ✅ `run_events_replay_idx (run_id, sequence)` for replay |

**Replay path** (confirmed):
- `GET /runs/{id}/events?after={cursor}` → `run_api.rs::list_run_events` (line ~470)
- Queries `SELECT sequence, event_type, payload, created_at FROM run_events WHERE run_id=$1 AND sequence>$2 ORDER BY sequence LIMIT $3`
- Frontend replays complete event stream from any cursor position
- Agent runs persist across app restarts (no in-memory-only state)

**No backend change required.** This is a verification-only item.

---

### Implementation Order (Recommended)

1. **Item 2 (streaming speed)** — purely internal optimization, no API change
2. **Item 3 (default embedding)** — new feature, adds seed function; backward-compatible
3. **Item 4 (autobiography auto-update)** — new feature, new module; depends on Item 3 for cheapest-model selection
4. **Items 1 and 5** — verify only, last

### Acceptance Criteria (per item)

#### Item 2
- [ ] `DELTA_EVENT_BYTES` 1024→4096
- [ ] Multi-row event INSERT (batched in groups of 8)
- [ ] Inline `flush_text_events(force: false)` removed from `TextDelta` handler
- [ ] All existing tests pass
- [ ] Manual: fewer run_events rows for same response length

#### Item 3
- [ ] `seed_default_embedding_model` creates provider `provider_openrouter_default` + model `model_qwen3_embedding_4b` when `OPENROUTER_API_KEY` is set
- [ ] Idempotent: second call is no-op
- [ ] Dimensions = 2048; model_reference = `"qwen/qwen3-embedding-4b"`
- [ ] `active_embedding_model_id` set on profiles
- [ ] Embedding queries work (lexical fallback if network unavailable)
- [ ] No migration required; existing pgvector vectors unaffected

#### Item 4
- [ ] Policy `"automatic"` triggers post-run update; `"propose"`/`"manual"` do not
- [ ] New content merged into autobiography body
- [ ] Revision bumped, book_revisions row appended, embedding re-enqueued
- [ ] Size cap at 99,000 chars
- [ ] Secret patterns redacted
- [ ] High-similarity content deduplicated
- [ ] "NOTHING" response → no-op
- [ ] Run never fails due to autobiography update failure

#### Items 1 & 5
- [ ] `get_run_context` returns `run_id` when snapshot lacks it
- [ ] Conversations/messages/run_events survive app restart (verified via manual container restart)