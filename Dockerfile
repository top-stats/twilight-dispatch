FROM rust:1.98.1-trixie AS builder

WORKDIR /usr/build

# Build the dependencies first so code changes don't rebuild every crate.
COPY Cargo.toml Cargo.lock ./
COPY vendor ./vendor
RUN mkdir src \
    && echo 'fn main() {}' > src/main.rs \
    && touch src/lib.rs \
    && cargo build --release --locked \
    && rm -rf src

COPY src ./src
RUN touch src/main.rs src/lib.rs && cargo build --release --locked

FROM debian:trixie-slim

RUN apt-get update -y \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /usr/app

ENV RUST_LOG=info

COPY --from=builder /usr/build/target/release/twilight-dispatch /usr/app/twilight-dispatch

# SIGTERM closes the shards resumably and saves their sessions. Give it time
# (stop_grace_period in Swarm / Compose; Docker's default is 10s).
STOPSIGNAL SIGTERM

# Unhealthy only when a connected shard is stuck (see /healthcheck).
HEALTHCHECK --interval=30s --timeout=10s --start-period=5m --retries=3 \
    CMD ["/usr/app/twilight-dispatch", "healthcheck"]

ENTRYPOINT ["/usr/app/twilight-dispatch"]
