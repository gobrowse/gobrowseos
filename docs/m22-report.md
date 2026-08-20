# M22 Acceptance Report

**Milestone:** M22 — Unified Library + Sandbox & Plugin Runtime  
**Date:** 2026-08-20  
**Commit:** d0009df (initial-agent-os)  
**Image:** gobrowse-os-app:m22  

## Build Gates

| Gate | Result | Evidence |
|------|--------|----------|
| `cargo fmt --check` | PASS | No output (clean) |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | PASS | `Finished dev profile` |
| `cargo test --workspace` | PASS | 23 passed, 0 failed, 1 ignored |
| `trunk build` (WASM) | PASS | dist/gobrowse-web-*.wasm + index.html + JS + CSS |
| Docker build (bookworm) | PASS | gobrowse-os-app:m22-final (66MB gzipped) |

## Chat Journeys (m22test @ localhost:8082, Mistral open-mistral-nemo)

| Journey | Description | Result | Evidence |
|---------|-------------|--------|----------|
| B | Agent sandbox tools via chat | PASS | terminal_start → terminal_id → terminal_read_output → `"hello from sandbox\r\n"` state=Exited |
| D | Skill book load via chat | PASS | library_search(q="skill") → hello-gobrowse → library_load → components: greeting, file-help |
| E | MCP book load via chat | PASS | library_search(q="MCP") → fixture-greeter → library_load → mcp-greeter tool, stdio transport |
| J | Retrieval scale + token metrics | PASS | library_search executed → token_metrics event emitted: book_searches=1, all counters present |

## Pre-existing Browser Journeys (from prior verification)

| Journey | Description | Result |
|---------|-------------|--------|
| A | Terminal start/echo/file/resize/interrupt | PASS |
| C | Source book create + search | PASS |
| F | Plugin install via UI stepper | PASS |
| I | Plugin upgrade v1→v2 + rollback | PASS |

## Production Verification

| Check | Main (8080) | m22test (8082) |
|-------|-------------|-----------------|
| health/ready | `{"status":"ready","version":"0.1.0"}` | `{"status":"ready","version":"0.1.0"}` |
| Doctor PostgreSQL | PASS | PASS |
| Doctor pgvector | PASS | PASS |
| Doctor Sandbox | PASS | PASS |
| Doctor Static assets | PASS | PASS |
| Doctor Credential vault | PASS | PASS |

- **Containers:** gobrowse-os-app-1 (healthy), gobrowse-m22test (running), gobrowse-os-postgres-1 (healthy)
- **Schema version:** 21 (migration 0021)
- **sandboxd:** active, health check OK, socket accessible from containers

## Security Gates

- F1 (MCP create/update role gate): FIXED — OWNER/ADMIN gate on MCP create/update
- F2 (Companion Book INSERT triggers): FIXED — migration 0021 triggers
- F3 (Self-asserted VERIFIED trust): FIXED — verify_artifact_signature always false until real publisher keys
- F4 (terminal_id not bound to workspace): recorded, not release-blocking

## Known Non-blocking Issues

- P3: "0 WORKSPACES" transient label before workspace list loads (cosmetic)
- P3: `\x1b[6n` cursor-query escape shown in terminal output (PTY doesn't answer it)
- P3: Marketplace search returns broad GitHub matches (not filtered to gobrowse-plugin.json)
- MCP OAuth vault: Warn (credentials=0, expected — no MCP OAuth providers configured)

## Commits in This Batch

```
d0009df fix: add no_proxy to GitHub API client; update PLAN.md
380cbe0 fix(run): align tool risk_class taxonomy with the tool registry
ba41d3a fix(sandboxd): make nested workspace dirs two-way writable
d50da86 fix(chat): fan out one wire tool-result message per parallel call
d22a4e8 fix(sandboxd): daemon-created workspace files readable by the container
0bfb431 fix(sandboxd): tolerate exit between readiness probe and resize
f7bfed9 fix(sandboxd): accept one-shot execs that exit before readiness
191ecff fix(run): persist tool_calls with schema columns; default chat models to tool_calls
364fbd7 fix(sandboxd): drop keep-id userns; map container user to subuid range
d28385d fix(ui): auto-store pasted provider API key in vault when creating a chat model
6b14a21 test(library): fixture body long enough to exercise snippet truncation
ee75b88 docs: checkpoint after security fixes + deploy
0fe0c1e fix(security): MCP role gate, no self-asserted VERIFIED trust, companion-book INSERT triggers (migration 21)
```

## Conclusion

M22 is **ACCEPTED**. All build gates pass, all required chat journeys pass, production is deployed and healthy with sandbox integration verified. No blocking issues remain.
