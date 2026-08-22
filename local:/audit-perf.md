# Performance Audit - Gobrowse OS

## Baseline Metrics

### WASM Bundle Size
- Main WASM file: 2.9MB (gobrowse-web-c965fb8be8616ef_bg.wasm)
- JavaScript glue: 42.0KB (gobrowse-web-c965fb8be8616ef.js)
- Styles: 31.7KB (styles-7d68912902f1e39b.css)
- HTML: 1.2KB

### Container Image Analysis
**Dockerfile Multi-Stage Analysis:**
- Tools stage: rust:1.94-bookworm + cargo install trunk/wasm-opt
- Deps stage: Copies all cargo files, builds gobrowse-server
- Builder stage: Compiles web assets with trunk build and wasm-opt -Oz
- Runtime stage: debian:bookworm-slim with only compiled binary and assets

**Size Optimization Opportunities:**
- Runtime stage uses debian:bookworm-slim (good)
- Binary compiled with LTO thin, strip symbols, panic=abort (good)
- wasm-opt -Oz applied (good)

## Performance Issues Identified

### 1. Server-Side Polling Issues

**Embedding Worker (crates/gobrowse-server/src/embedding.rs)**
- Polling intervals: 1 second and 2 seconds sleeps
- `run_worker` function polls for jobs every 1-2 seconds
- Potential for reduced polling granularity

**Realtime WebSocket (crates/gobrowse-server/src/realtime.rs)**
- Heartbeat: every 25 seconds
- Replay: every 500ms (aggressive)
- Read patterns: SELECT with LIMIT 100 each cycle

**Web Frontend (crates/gobrowse-web/src/app.rs)**
- Terminal polling: TERMINAL_POLL_MS = 1000ms (1 second)
- Terminal reads: up to 32,768 bytes per poll

**Rate Limiter (crates/gobrowse-server/src/rate_limiter.rs)**
- 1,100ms sleeps for rate limiting

**Outbound HTTP (crates/gobrowse-server/src/outbound_http.rs)**
- 1 second sleep patterns for connection handling

### 2. N+1 Query Patterns Analysis

**Workspace Loading**
```rust
// Pattern found in lib.rs
let row = sqlx::query("...").bind(id).fetch_optional(&pool);
let assets = sqlx::query("...").bind(id).fetch_all(&pool);
```

**UI Package Loading**
```rust
// Multiple sequential queries for single UI package
- First: SELECT id, ui_kind, install_path FROM ui_packages
- Second: SELECT file_path, content_type, sha256_hash FROM ui_package_assets
```

**Realtime Delivery**
```rust
// In realtime.rs: SELECT with LIMIT 100, then WebSocket message processing
```

### 3. Reactive Re-rendering Risks

**Leptos Signal Patterns in app.rs**
- Multiple RwSignal updates for single operations
- Frequent reactivity in ChatPage, LibraryPage components
- Library search with query: `search_library(query, kind)` calls
- Book loading: `load_library_book(book_id)` calls

**UI Package Routes:**
- Multiple reactive signals for plugin state management
- Stepper state changes trigger UI updates

### 4. Large JSON Responses

**Chat Context Assembly**
- `/api/v1/runs/{run_id}/context` endpoint may return large context
- No size limits visible in code

**Library Books**
- Book bodies can be large (no size limits visible)
- Progressive loading exists but may still be large

### 5. Eager Loading Patterns

**Library Page Initial Load**
```rust
fn load_library_list(books, status, _query) {
    // Loads all books immediately
}
```

**Workspace Loading**
```rust
fn load_workspaces(workspaces) {
    // Loads all workspaces immediately
}
```

**UI Packages Loading**
```rust
// Loads UI packages during initialization
```

### 6. Container Size Analysis

**Dockerfile Size Components:**
- Base runtime: debian:bookworm-slim (~50MB)
- Binary: ~15MB (estimated)
- Web assets: 2.9MB + 42KB + 31.7KB + 1.2KB
- Recovery assets: Similar size to web assets

**Optimization Opportunities:**
- Runtime stage could be further minimized
- Consider multi-arch base image

## Key Performance Findings

### P1 - Core Performance
1. **Polling Frequency**: Multiple 1-second polling intervals across server and client
2. **WASM Size**: 2.9MB exceeds typical <2MB for good mobile performance
3. **N+1 Queries**: Visible in UI package and workspace loading patterns

### P2 - Correctness/Performance
1. **Eager Loading**: Library, workspaces, and UI packages load all data immediately
2. **Reactive Overkill**: Multiple signal updates for single operations
3. **Large JSON**: No response size limits on large endpoints

### P3 - Polish
1. **Container Size**: Runtime could be further optimized
2. **Polling Granularity**: Could be optimized for battery life

## Recommendations

### Immediate Actions
1. Reduce polling frequencies where appropriate
2. Optimize WASM bundle size through code splitting
3. Fix N+1 query patterns with proper joins
4. Implement response size limits

### Medium-term
1. Implement lazy loading for library and workspace data
2. Optimize reactive signal usage
3. Further container optimization

### Long-term
1. Implement polling batching where possible
2. Add response caching strategies
3. Consider WebAssembly code splitting
