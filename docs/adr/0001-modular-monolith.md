# ADR 0001: Modular Monolith

Status: Accepted

Use three initial Rust crates: portable core, native server/CLI, and WASM web. Keep domain boundaries as modules and traits. Add a process only for a privilege or operational boundary, notably sandboxd and optional browser automation. This keeps the minimum deployment small and transactional while allowing later extraction.
