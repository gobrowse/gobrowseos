# Development

Required: Rust 1.94+, PostgreSQL 17 with pgvector, Docker Compose, Trunk, and cargo-nextest.

```bash
cargo fmt --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo nextest run --workspace
(cd crates/gobrowse-web && trunk build index.html --dist ../../dist)
```

Run PostgreSQL from Compose and start the app with `cargo run -p gobrowse-server --bin gobrowse -- serve`. Configuration precedence is typed defaults, optional `config.toml`, `GOBROWSE__...` environment, then explicit CLI flags where supported.
