# THIS IS A FORK, WE ARE USING THIS TO IMPLEMENT FEATURES WE NEED BUT DO NOT INTEND ON MAKING A FEATURE REQUEST FOR. IF YOU HAVE A FEATURE REQUEST PLEASE GO TO THE ORIGINAL REPOSITORY.

## https://github.com/chamburr/twilight-dispatch

## twilight-dispatch

Connects to the Discord gateway (with the
[twilight](https://github.com/twilight-rs/twilight) library), keeps a state
cache in Redis and publishes every dispatch event to the `gateway` topic
exchange of an AMQP broker (LavinMQ / RabbitMQ) as `{"op", "t", "d", "old"?}`,
with the event name as the routing key. Gateway commands published to the
`gateway.send` queue (`{"op": 0, "shard": N, "data": {...}}` to send,
`{"op": 1, "shard": N}` to reconnect a shard) are sent to the matching shard.
Only the gateway opcodes 3, 4, 8, 31 and 43 may be sent, and `data` (as
compact JSON) may be at most 4096 bytes; other commands, and commands for a
shard the bot does not have, are dropped and counted in
`gateway_commands{result="invalid"}`.

Configuration is read from environment variables (or `.env`); see
[`.env.example`](.env.example).

### Operations

- `GET /metrics` (Prometheus) and `GET /healthcheck` on `PROMETHEUS_HOST:PROMETHEUS_PORT`.
  `/healthcheck` returns 503 only when restarting helps: a connected shard
  that received nothing for longer than `SHARD_SILENCE_SECS`, or a shard whose
  closed connection has not finished closing after 2 minutes (twilight would
  wait for it forever). Broker outages and shards closed by Discord with
  a non-reconnectable code are reported as `warnings` with status 200, because
  a restart would re-identify every shard and burn the daily identify limit.
  The image's `HEALTHCHECK` runs `twilight-dispatch healthcheck`.
- The binary takes no argument (run the service) or `healthcheck`; any other
  argument exits with status 2 without starting anything. A `.env` that
  cannot be parsed is reported as `Failed to load .env: ...` (dotenvy applies
  only the lines before the bad one).
- Every log line of a shard, twilight's own included, is prefixed with
  `shard{id=N}:`. Logs have ANSI colours only when stdout is a terminal and
  `NO_COLOR` is not set.
- On SIGTERM shards are closed resumably, queued events are flushed and the
  sessions are saved, so the next start resumes them. Set
  `stop_grace_period: 30s` in Swarm / Compose (Docker's default is 10s).
- Redis and the broker are reconnected automatically; while Redis is down
  events are still published (without cache updates or `old`).
- By default an event counts as published once it is written to the broker
  connection, so a broker crash or a connection drop right after the write
  can lose it. With `AMQP_CONFIRMS=true` it counts as published only once the
  broker confirmed it (publisher confirms); only then may the saved resume
  point pass it. An event the broker nacked, did not confirm within 5 s, or
  whose connection dropped first is retried and, failing that, dropped. The
  saved resume point stays before it for 15 minutes (`seq::FAILED_RETENTION`),
  so a restart within that time that resumes the saved sessions (`RESUME` on,
  `SHARDS_*` set, `SHARDS_TOTAL` unchanged) makes Discord replay it. A
  reconnect of the running process resumes from the shard's own sequence and
  does not replay it. Delivery is at-least-once: when the connection drops
  after the broker stored an event but before its confirm arrived, the event
  is published again (as after a restart), so consumers should dedupe by the
  `message_id` property (`shard:session:seq`). Each worker waits for its
  confirm before the next event, and LavinMQ confirms only after syncing its
  data directory, so the broker's disk bounds the rate per worker.
- With confirms, `WORKERS` is the setting for the publish rate: with one
  event in flight per worker, the confirmed rate of a process grows with the
  number of workers.
  Measured on a dev machine (LavinMQ 2.10 with its default disk sync, 1 KiB
  events): about 200 events/s from one worker, 2.5k/s from 32 and 10k/s from
  128 (about 60k/s from 32 before confirms). Raise it (up to 1024) when shards
  log `Event queue is full; pausing the shard until it drains`. Each worker
  buffers up to 1024 events, so memory for queued events grows with `WORKERS`
  (about a million payloads at 1024). It does not help one busy guild: a
  guild's events always use the same worker, to stay in order, so a guild is
  capped at one worker's rate (presence updates of a large guild, which the
  `INTENTS` of `.env.example` include, can exceed it), and while its worker's
  queue is full the whole shard pauses. Only faster confirms (the broker's
  disk) lift that cap. The numbers depend on the disk: compare the production
  peak of `gateway_events` with a measurement on the production broker.
- Events of one guild are cached and published in order; a consumer that reads
  the cache after receiving an event sees that event applied if its cache
  update succeeded (an update that failed or was skipped while Redis is down
  is not applied, and one that timed out may still be applied after the event
  was published).
- Active threads are cached under `thread:<guild>:<thread>` (indexed in
  `thread_keys`, `thread_keys:<guild>` and `guild_keys:<guild>`), the live
  copy that thread events keep current; `channel:` keys never hold threads.
  The guild blob's `threads` is the list from the guild's last GUILD_CREATE,
  kept for compatibility as in earlier versions: thread events do not update it.
- After every new session (a fresh identify, not a resume) Discord sends each
  guild's GUILD_CREATE again, and it lists only some members of a large guild:
  none that are offline above `LARGE_THRESHOLD` members, and only the bot and
  the members in voice channels over 75k members. This re-sync replaces what is
  cached for the guild (channels, threads, roles, emojis, presences, voice
  states) but, with `STATE_MEMBER`, keeps the cached members, still indexed in
  `guild_keys:<guild>` and `member_keys`. With `STATE_MEMBER_TTL=0` the members
  it does not list get a deadline 24 hours later in `expiry_zset`
  (`cache::MEMBER_RESYNC_GRACE_MS`), and a member that already has one keeps
  it, so another re-sync does not postpone it. Anything that writes a member
  again drops its deadline: a GUILD_MEMBERS_CHUNK (the bot requests every
  guild's members hourly), GUILD_MEMBER_ADD or GUILD_MEMBER_UPDATE. So members
  still in the guild stay cached, and members who left while the bot was
  disconnected (whose GUILD_MEMBER_REMOVE was missed) are removed when the
  grace ends. A GUILD_CREATE that lists every member (as many as its
  `member_count`, as for guilds below `LARGE_THRESHOLD`) removes the members it
  does not list at once. With a member TTL the kept members keep the deadline
  they had, and one without a deadline (cached while the TTL was 0) gets the
  TTL.
  Leaving a guild (GUILD_DELETE without `unavailable`) and READY no longer
  listing it still remove all of its members at once.
- With several processes (different `SHARDS_START`/`SHARDS_END`), every
  process consumes `gateway.send`, runs commands for its own shards and
  forwards the others to the `gateway.send.shards` direct exchange (routing key:
  shard id), where each process binds an exclusive queue for the shards it
  runs. Commands for shards no running process has are dropped by the broker.
  Commands for one shard are not guaranteed to run in the order they were
  published: one that the shard's own process consumes runs at once, while an
  earlier one that another process consumed is still being forwarded.
- Every second each process writes the status of its shards into
  `gateway_statuses` (a JSON array of `{"shard", "status", "latency",
  "last_ack"}`) and keeps the entries of the shards it does not run, so the
  key holds the shards of every process. An entry of a shard that no running
  process has (for example after its process stopped) keeps its last status
  until a process running that shard writes it again. A process that starts
  with a different shard total clears the list.
- Identifies take turns across processes through Redis, so every process of
  the bot must use the same Redis database. Discord allows one identify per
  5 s in each bucket `shard % max_concurrency` (Discord's `max_concurrency`,
  also when `SHARDS_CONCURRENCY` is lower), whichever process sends it. Before
  a shard identifies, its process takes the bucket's key
  `gateway_identify:<bucket>` (`SET NX PX 5500`: the 5 s plus a 500 ms margin;
  the value names the process) or waits until the holder's key expires. The
  key is never deleted, so a process that crashed holding it delays the
  bucket by no more than its own identify would have. A process also keeps
  its own identifies of a bucket 5.5 s apart and in order, counted from when
  they happen, whatever Redis does. If Redis answers with an error or not
  within 2 s, the shard identifies with that spacing only (logged once until
  Redis answers again). Each process still keeps the daily session start
  limit for its own shards.

### Tests

```bash
scripts/test.sh
```

Starts Redis and LavinMQ in Docker and runs `cargo fmt --check`, clippy,
rustdoc (warnings are errors) and all unit and integration tests. Plain
`cargo test` runs the unit tests and skips the integration tests unless
`TEST_REDIS_URL` / `TEST_AMQP_URL` are set (`REQUIRE_SERVICES=1` turns a
missing service into a failure). The broker outage tests also need
`TEST_AMQP_HTTP`, the address of LavinMQ's management API (e.g.
`127.0.0.1:15672`).

`tests/repo_files.rs` reads the repository's own files and fails on a change
no build step catches: a vendored twilight crate that is no longer the one
built (an unused `[patch.crates-io]` entry only warns), a patch hunk without
its marker or a `PATCHES.md` naming something that does not exist, a second
rustls in `Cargo.lock`, and in CI an action not pinned to a commit, a
checkout that keeps the token or a job without a timeout.

CI (`.github/workflows/docker.yml`) runs the same checks on every push and
pull request on GitHub-hosted runners, as separate jobs (format; clippy and
rustdoc; tests), with Redis and LavinMQ as service containers. A coverage job
runs the tests under `cargo llvm-cov`, adds the per-file line coverage of
`src/` to the run's summary, lists the lines no test runs in its log,
uploads `lcov.info` as an artifact and fails when the line coverage of `src/`
drops below 97%. The image is only built and published
(self-hosted runner) for pushes to `main` and tags, after the format, clippy
and test jobs passed. Actions are pinned to commits and kept current by
Dependabot (`.github/dependabot.yml`), which also proposes crate updates
except for the twilight crates.

### Vendored crates

Both are used through `[patch.crates-io]` in `Cargo.toml`; to upgrade twilight,
re-apply the patches each `PATCHES.md` lists.

- `vendor/twilight-model` is twilight-model 0.17.1 with a few patches (see
  [`vendor/twilight-model/PATCHES.md`](vendor/twilight-model/PATCHES.md)).
- `vendor/twilight-gateway` is twilight-gateway 0.17.1 with one patch: a
  4007/4009 close drops the session, so the shard identifies anew instead of
  resuming the dead session forever (see
  [`vendor/twilight-gateway/PATCHES.md`](vendor/twilight-gateway/PATCHES.md)).
