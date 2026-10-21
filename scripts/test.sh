#!/usr/bin/env bash
# Run the full test suite (unit + integration) in Docker:
# Redis and LavinMQ (the production broker) are started on a private network,
# then cargo fmt --check, clippy and cargo test run in the Rust image.
#
#   scripts/test.sh                 # everything
#   scripts/test.sh --test cache    # only tests/cache.rs (any cargo test arguments work)
#
# The integration tests FLUSH their Redis databases; this script always uses
# throwaway containers.
set -euo pipefail

RUST_IMAGE="${RUST_IMAGE:-rust:1.98.1-trixie}"
REDIS_IMAGE="${REDIS_IMAGE:-redis:7.4-alpine}"
LAVINMQ_IMAGE="${LAVINMQ_IMAGE:-cloudamqp/lavinmq:2.10.1}"
NAME="twilight-dispatch-test-$$"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

cleanup() {
    docker rm -f "$NAME-redis" "$NAME-lavinmq" >/dev/null 2>&1 || true
    docker network rm "$NAME" >/dev/null 2>&1 || true
}
trap cleanup EXIT

docker network create "$NAME" >/dev/null
docker run -d --rm --name "$NAME-redis" --network "$NAME" "$REDIS_IMAGE" >/dev/null
docker run -d --rm --name "$NAME-lavinmq" --network "$NAME" "$LAVINMQ_IMAGE" >/dev/null

# Wait for the broker to accept connections.
for _ in $(seq 1 30); do
    if docker logs "$NAME-lavinmq" 2>&1 | grep -q "Finished startup"; then break; fi
    sleep 1
done

# MSYS_NO_PATHCONV keeps Git Bash on Windows from rewriting container paths.
MSYS_NO_PATHCONV=1 docker run --rm --network "$NAME" \
    -v "$ROOT:/src" -w /src \
    -v twilight-dispatch-cargo:/usr/local/cargo/registry \
    -v twilight-dispatch-target:/target -e CARGO_TARGET_DIR=/target \
    -e TEST_REDIS_URL="redis://$NAME-redis:6379" \
    -e TEST_AMQP_URL="amqp://guest:guest@$NAME-lavinmq:5672/%2f" \
    -e TEST_AMQP_HTTP="$NAME-lavinmq:15672" \
    -e REQUIRE_SERVICES=1 \
    "$RUST_IMAGE" bash -c "
        set -e
        rustup component add rustfmt clippy >/dev/null 2>&1
        cargo fmt --check
        cargo clippy --locked --all-targets -- -D warnings
        cargo test --locked --no-fail-fast $*
    "
