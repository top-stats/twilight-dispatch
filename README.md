# THIS IS A FORK, WE ARE USING THIS TO IMPLEMENT FEATURES WE NEED BUT DO NOT INTEND ON MAKING A FEATURE REQUEST FOR. IF YOU HAVE A FEATURE REQUEST PLEASE GO TO THE ORIGINAL REPOSITORY.

## https://github.com/twilight-rs/twilight

## twilight-dispatch

Connects to the Discord gateway, keeps a state cache in Redis and publishes
every dispatch event to the `gateway` topic exchange of an AMQP broker
(LavinMQ / RabbitMQ) as `{"op", "t", "d", "old"?}`, with the event name as the
routing key. Gateway commands published to the `gateway.send` queue
(`{"op": 0, "shard": N, "data": {...}}` to send, `{"op": 1, "shard": N}` to
reconnect a shard) are sent to the matching shard.

Configuration is read from environment variables (or `.env`); see
[`.env.example`](.env.example).

### Operations

- `GET /metrics` (Prometheus) and `GET /healthcheck` on `PROMETHEUS_HOST:PROMETHEUS_PORT`.
  `/healthcheck` returns 503 only when restarting helps (a connected shard that
  stopped receiving anything). Broker outages and shards closed by Discord with
  a non-reconnectable code are reported as `warnings` with status 200, because
  a restart would re-identify every shard and burn the daily identify limit.
  The image's `HEALTHCHECK` runs `twilight-dispatch healthcheck`.
- On SIGTERM shards are closed resumably, queued events are flushed and the
  sessions are saved, so the next start resumes them. Set
  `stop_grace_period: 30s` in Swarm / Compose (Docker's default is 10s).
- Redis and the broker are reconnected automatically; while Redis is down
  events are still published (without cache updates or `old`).
- Events of one guild are cached and published in order; a consumer that reads
  the cache after receiving an event sees that event applied.
- Active threads are cached under `thread:<guild>:<thread>` (indexed in
  `thread_keys`, `thread_keys:<guild>` and `guild_keys:<guild>`), the live
  copy that thread events keep current; `channel:` keys never hold threads.
  The guild blob's `threads` is the list from the guild's last GUILD_CREATE,
  kept for compatibility as in earlier versions: thread events do not update it.
- With several processes (different `SHARDS_START`/`SHARDS_END`), every
  process consumes `gateway.send`, runs commands for its own shards and
  forwards the others to the `gateway.send.shards` direct exchange (routing key:
  shard id), where each process binds an exclusive queue for the shards it
  runs. Commands for shards no running process has are dropped by the broker.

### Tests

```bash
scripts/test.sh
```

Starts Redis and LavinMQ in Docker and runs `cargo fmt --check`, clippy and
all unit and integration tests. Plain `cargo test` runs the unit tests and
skips the integration tests unless `TEST_REDIS_URL` / `TEST_AMQP_URL` are set
(`REQUIRE_SERVICES=1` turns a missing service into a failure).

CI (`.github/workflows/docker.yml`) runs the same checks on every push and
pull request on GitHub-hosted runners, with Redis and LavinMQ as service
containers; the image is only built and published (self-hosted runner) for
pushes to `main` and tags, after the tests passed.

### Vendored crates

`vendor/twilight-model` is twilight-model 0.17.1 with a few patches (see
[`vendor/twilight-model/PATCHES.md`](vendor/twilight-model/PATCHES.md)).
