FROM rust:1.86-bookworm AS builder

WORKDIR /usr/build

ENV RUSTFLAGS="-C target-cpu=haswell"

COPY src ./src
COPY Cargo.lock Cargo.toml ./

RUN apt update -y && apt upgrade -y && apt install -y g++ gcc cmake make

RUN cargo build --release

FROM debian:bookworm-slim

WORKDIR /usr/app

RUN apt update -y && apt install -y ca-certificates libssl3

ENV RUST_LOG=info

COPY --from=builder /usr/build/target/release/twilight-dispatch /usr/app/twilight-dispatch

ENTRYPOINT ["/usr/app/twilight-dispatch"]