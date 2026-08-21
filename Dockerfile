FROM rust:1.94-bookworm AS tools
RUN cargo install trunk --version 0.21.14 --locked
RUN cargo install wasm-opt --locked

FROM tools AS deps
WORKDIR /workspace
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates/gobrowse-core/Cargo.toml crates/gobrowse-core/
COPY crates/gobrowse-core/benches crates/gobrowse-core/benches
COPY crates/gobrowse-server/Cargo.toml crates/gobrowse-server/
COPY crates/gobrowse-web/Cargo.toml crates/gobrowse-web/
COPY crates/gobrowse-sandboxd/Cargo.toml crates/gobrowse-sandboxd/
COPY crates/gobrowse-recovery/Cargo.toml crates/gobrowse-recovery/
RUN mkdir -p crates/gobrowse-core/src crates/gobrowse-server/src crates/gobrowse-web/src crates/gobrowse-sandboxd/src crates/gobrowse-recovery/src
RUN echo "// dummy" > crates/gobrowse-core/src/lib.rs
RUN echo "// dummy" > crates/gobrowse-server/src/lib.rs
RUN echo "fn main() {}" > crates/gobrowse-server/src/main.rs
RUN echo "// dummy" > crates/gobrowse-web/src/lib.rs
RUN echo "// dummy" > crates/gobrowse-sandboxd/src/lib.rs
RUN echo "fn main() {}" > crates/gobrowse-recovery/src/main.rs
RUN rustup target add wasm32-unknown-unknown
RUN cargo build --locked --release -p gobrowse-server

FROM deps AS builder
COPY crates ./crates
# deps-stage fingerprints are newer than COPY'd sources (mtimes preserved
# from the build context), so cargo would consider the dummy deps build fresh.
# Touch sources + drop the dummy bin to force a real rebuild.
RUN find /workspace/crates -name '*.rs' -exec touch {} + \
    && rm -f /workspace/target/release/gobrowse \
    && cargo build --locked --release -p gobrowse-server
WORKDIR /workspace/crates/gobrowse-web
RUN trunk build index.html --release --dist /workspace/dist
RUN wasm-opt -Oz --enable-bulk-memory -o /workspace/dist/*.wasm /workspace/dist/*.wasm
WORKDIR /workspace/crates/gobrowse-recovery
RUN trunk build index.html --release --dist /workspace/dist-recovery
RUN wasm-opt -Oz --enable-bulk-memory -o /workspace/dist-recovery/*.wasm /workspace/dist-recovery/*.wasm 2>/dev/null || true

FROM debian:bookworm-slim AS runtime
# Runtime needs no git or curl: the container healthcheck uses `gobrowse health`
# (a self-contained HTTP probe against /health/ready) instead of curl, and the
# binary is built in the builder stage, so source tooling is not required here.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 gobrowse \
    && useradd --system --uid 10001 --gid gobrowse --home-dir /app --shell /usr/sbin/nologin gobrowse
WORKDIR /app
COPY --from=builder /workspace/target/release/gobrowse /usr/local/bin/gobrowse
COPY --from=builder /workspace/dist /app/dist
COPY --from=builder /workspace/dist-recovery /app/recovery
COPY --from=builder /workspace/crates/gobrowse-server/migrations /app/migrations
RUN mkdir -p /app/data && chown 10001:10001 /app/data
ENV GOBROWSE_MIGRATIONS_DIR=/app/migrations
USER 10001:10001
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/gobrowse"]
CMD ["serve"]
