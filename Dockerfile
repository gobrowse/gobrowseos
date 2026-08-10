FROM rust:1.94-bookworm AS tools
RUN cargo install trunk --version 0.21.14 --locked

FROM tools AS builder
WORKDIR /workspace
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
RUN rustup target add wasm32-unknown-unknown
WORKDIR /workspace/crates/gobrowse-web
RUN trunk build index.html --release --dist /workspace/dist
WORKDIR /workspace
RUN cargo build --locked --release -p gobrowse-server

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl git \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 gobrowse \
    && useradd --system --uid 10001 --gid gobrowse --home-dir /app --shell /usr/sbin/nologin gobrowse
WORKDIR /app
COPY --from=builder /workspace/target/release/gobrowse /usr/local/bin/gobrowse
COPY --from=builder /workspace/dist /app/dist
USER 10001:10001
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/gobrowse"]
CMD ["serve"]
