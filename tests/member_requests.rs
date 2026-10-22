#![recursion_limit = "256"]

//! Guild member requests (`REQUEST_MEMBERS`) of a running shard, through
//! `gateway::runner::run` against the fake Discord gateway of
//! `tests/support/fake_discord.rs`. No services are needed: the publisher
//! records what the workers publish, and with the cache off nothing uses
//! Redis.
//!
//! Pinned here:
//! - READY and a GUILD_CREATE that lists every member request nothing;
//! - a GUILD_CREATE that leaves members out makes the shard send one op 8
//!   (every member, no presences, a nonce of at most 32 bytes);
//! - one request at a time: the next guild once the last chunk with the
//!   nonce of the one in flight arrived, not another client's;
//! - a RATE_LIMITED for the request in flight is retried with a new nonce
//!   after `retry_after` and the margin;
//! - chunks and RATE_LIMITED are published as before, and
//!   `gateway_member_requests` counts the requests;
//! - without `REQUEST_MEMBERS` nothing is requested.
//!
//! Only `a_guild_that_leaves_members_out_is_requested` makes requests, so
//! the deltas of the process-wide counter it reads are its own.

mod common;
#[path = "support/fake_discord.rs"]
mod fake_discord;

use fake_discord::{FakeDiscord, GatewayConnection};
use redis::aio::ConnectionLike;
use serde_json::{json, Value};
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{sync::watch, task::JoinHandle};
use twilight_dispatch::{
    config::{MemberConfig, StateConfig},
    discord_log::DiscordLog,
    gateway::{
        members::RATE_LIMIT_MARGIN,
        registry::{Registry, ShardHandle},
        runner::{self, RunnerContext},
        worker::{self, Breaker, Publisher, WorkerContext},
    },
    metrics::MEMBER_REQUESTS,
    models::ApiResult,
    util::Throttle,
};
use twilight_gateway::{queue::InMemoryQueue, ConfigBuilder, Intents, Shard, ShardId};

/// Shards of this many; every test runs its own shard id.
const TOTAL: u32 = 4;
/// A day: no heartbeat is due during a test, so the gateway only sees the
/// frames a test expects.
const QUIET: u64 = 86_400_000;
/// The bound of every wait; only a failing test waits this long.
const WAIT: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(5);
/// How long a test waits to see that the shard sends nothing.
const SILENCE: Duration = Duration::from_millis(300);
const CHUNK: &str = "GUILD_MEMBERS_CHUNK";

/// A Redis that never answers; with the cache off nothing asks it.
#[derive(Clone)]
struct NoRedis;

impl ConnectionLike for NoRedis {
    fn req_packed_command<'a>(
        &'a mut self,
        _cmd: &'a redis::Cmd,
    ) -> redis::RedisFuture<'a, redis::Value> {
        Box::pin(std::future::pending())
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        _pipeline: &'a redis::Pipeline,
        _offset: usize,
        _count: usize,
    ) -> redis::RedisFuture<'a, Vec<redis::Value>> {
        Box::pin(std::future::pending())
    }

    fn get_db(&self) -> i64 {
        0
    }
}

/// The routing key and `d` of every published event, in order.
#[derive(Default)]
struct Published(Mutex<Vec<(String, Value)>>);

impl Published {
    fn get(&self) -> Vec<(String, Value)> {
        self.0.lock().unwrap().clone()
    }

    /// The `d` of every published event of `kind`.
    fn of(&self, kind: &str) -> Vec<Value> {
        self.get()
            .into_iter()
            .filter(|(key, _)| key == kind)
            .map(|(_, d)| d)
            .collect()
    }

    /// Wait until at least `count` events of `kind` were published.
    async fn wait(&self, kind: &str, count: usize) -> Vec<Value> {
        let deadline = Instant::now() + WAIT;
        loop {
            let found = self.of(kind);
            if found.len() >= count {
                return found;
            }
            assert!(
                Instant::now() < deadline,
                "no {count} {kind} within {WAIT:?}: {:?}",
                self.get()
            );
            tokio::time::sleep(POLL).await;
        }
    }
}

impl Publisher for Published {
    async fn publish(
        &self,
        routing_key: &str,
        payload: Vec<u8>,
        _message_id: Option<String>,
    ) -> ApiResult<()> {
        let body: Value = serde_json::from_slice(&payload)?;
        let d = body["d"].clone();
        let mut published = self.0.lock().unwrap();
        published.push((routing_key.to_owned(), d));
        Ok(())
    }
}

struct Running {
    published: Arc<Published>,
    runner: JoinHandle<()>,
    shutdown: watch::Sender<bool>,
}

impl Running {
    /// Shut down as `app` does, and wait for `run()` to return.
    async fn stop(self) {
        self.shutdown.send_replace(true);
        let stopped = tokio::time::timeout(WAIT, self.runner).await;
        assert!(stopped.is_ok(), "run() did not return");
    }
}

/// Run shard `id` against the gateway at `url` the way `app::start` does,
/// with the cache off and these member requests.
fn start(url: &str, id: u32, members: MemberConfig) -> Running {
    // main() installs it; twilight's TLS connector panics without one.
    let _ = rustls::crypto::ring::default_provider().install_default();
    // A max_concurrency of 0 grants every IDENTIFY at once.
    let queue = InMemoryQueue::new(0, 1000, Duration::from_secs(86_400), 1000);
    let intents = Intents::GUILDS | Intents::GUILD_MEMBERS;
    let config = ConfigBuilder::new("member-token".to_owned(), intents)
        .proxy_url(url.to_owned())
        .queue(queue)
        .build();
    let shard = Shard::with_config(ShardId::new(id, TOTAL), config);
    let handle = Arc::new(ShardHandle::new(id, Some(shard.sender()), None));
    let published = Arc::new(Published::default());
    let worker_ctx = Arc::new(WorkerContext {
        redis: NoRedis,
        publisher: published.clone(),
        registry: Arc::new(Registry::new([handle.clone()])),
        state: StateConfig::default(),
        bot_id: common::bot_id(),
        cache_timeout: Duration::from_secs(5),
        discord: DiscordLog::disabled(),
        breaker: Breaker::default(),
        cache_errors: Throttle::default(),
        publish_errors: Throttle::default(),
    });
    let (workers, _) = worker::spawn(1, worker_ctx);
    let runner_ctx = Arc::new(RunnerContext {
        state: StateConfig::default(),
        workers,
        redis: NoRedis,
        discord: DiscordLog::disabled(),
        bot_id: common::bot_id(),
        cache_timeout: Duration::from_secs(5),
        max_held_bytes: runner::MAX_HELD_BYTES,
        members,
    });
    let (shutdown, shutdown_rx) = watch::channel(false);
    let runner = tokio::spawn(runner::run(shard, handle, runner_ctx, shutdown_rx));
    Running {
        published,
        runner,
        shutdown,
    }
}

/// Accept the shard's connection, send HELLO, read its IDENTIFY and send
/// READY (sequence 1) listing these guilds.
async fn identified(fake: &FakeDiscord, shard: u32, guilds: &[u64]) -> GatewayConnection {
    let mut conn = fake.gateway.accept().await;
    conn.hello(QUIET).await;
    let identify = conn.expect_identify().await;
    assert_eq!(identify["shard"], json!([shard, TOTAL]));
    conn.ready(1, "S", [shard, TOTAL], guilds).await;
    conn
}

/// The `d` of a GUILD_CREATE that lists 2 of its `member_count` members.
fn partial_guild(id: u64, member_count: u64) -> Value {
    let mut guild = common::guild(id, "partial");
    guild["member_count"] = json!(member_count);
    guild
}

/// The `d` of a GUILD_MEMBERS_CHUNK.
fn chunk(guild: u64, index: u32, count: u32, nonce: &str) -> Value {
    json!({
        "guild_id": guild.to_string(), "members": [common::member(10, &[])],
        "chunk_index": index, "chunk_count": count, "not_found": [], "nonce": nonce,
    })
}

/// The next frame, which must request every member of `guild` without
/// presences (op 8); its nonce.
async fn expect_request(conn: &mut GatewayConnection, guild: u64) -> String {
    let d = conn.expect_op(8).await["d"].clone();
    assert_eq!(d["guild_id"], guild.to_string(), "{d}");
    assert_eq!(d["query"], "", "{d}");
    assert_eq!(d["limit"], 0, "{d}");
    assert_eq!(d["presences"], json!(false), "{d}");
    let nonce = d["nonce"].as_str().expect("a nonce").to_owned();
    assert!(nonce.len() <= 32, "{nonce}");
    nonce
}

/// `gateway_member_requests{result}`.
fn requests(result: &str) -> u64 {
    MEMBER_REQUESTS.with_label_values(&[result]).get()
}

/// A GUILD_CREATE that leaves members out makes the shard request them with
/// one op 8, one guild at a time; the last chunk with its nonce completes
/// the request, and a RATE_LIMITED for it is retried after `retry_after`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_guild_that_leaves_members_out_is_requested() {
    let fake = FakeDiscord::start().await;
    let shard = 1;
    let base = ["sent", "done", "rate_limited"].map(requests);
    let members = MemberConfig {
        request: true,
        refresh_hours: 0,
    };
    let running = start(&fake.gateway.url(), shard, members);
    let mut conn = identified(&fake, shard, &[1000, 2000, 3000]).await;

    // READY, and a GUILD_CREATE that lists every member, request nothing.
    let complete = common::guild(1000, "complete");
    conn.dispatch("GUILD_CREATE", 2, &complete).await;
    assert_eq!(conn.try_next_frame(SILENCE).await, None);

    let partial = partial_guild(2000, 500);
    conn.dispatch("GUILD_CREATE", 3, &partial).await;
    let first = expect_request(&mut conn, 2000).await;
    // Another one waits for the request in flight.
    let next = partial_guild(3000, 300);
    conn.dispatch("GUILD_CREATE", 4, &next).await;
    assert_eq!(conn.try_next_frame(SILENCE).await, None);

    // The last chunk of another client's request (another nonce) does not
    // complete it; its own last chunk does.
    let own = chunk(2000, 0, 2, &first);
    conn.dispatch(CHUNK, 5, &own).await;
    let other = chunk(2000, 1, 2, "bot.1");
    conn.dispatch(CHUNK, 6, &other).await;
    assert_eq!(conn.try_next_frame(SILENCE).await, None);
    let last = chunk(2000, 1, 2, &first);
    conn.dispatch(CHUNK, 7, &last).await;
    let limited = expect_request(&mut conn, 3000).await;
    assert_ne!(limited, first);

    // Rate limited: requested again after retry_after and the margin.
    let retry_after = Duration::from_millis(300);
    let rate_limited = json!({
        "opcode": 8, "retry_after": retry_after.as_secs_f64(),
        "meta": {"guild_id": "3000", "nonce": limited},
    });
    let at = Instant::now();
    conn.dispatch("RATE_LIMITED", 8, &rate_limited).await;
    let retry = expect_request(&mut conn, 3000).await;
    let waited = at.elapsed();
    let least = retry_after + RATE_LIMIT_MARGIN - Duration::from_millis(100);
    assert!(waited >= least, "retried after {waited:?}");
    assert_ne!(retry, limited);
    let only = chunk(3000, 0, 1, &retry);
    conn.dispatch(CHUNK, 9, &only).await;
    assert_eq!(conn.try_next_frame(SILENCE).await, None, "nothing left");

    // Chunks and RATE_LIMITED are published as before, the other client's
    // chunk too.
    let chunks = running.published.wait(CHUNK, 4).await;
    assert_eq!(chunks[1]["nonce"], "bot.1");
    let published = running.published.wait("RATE_LIMITED", 1).await;
    assert_eq!(published, [rate_limited]);
    let counted = ["sent", "done", "rate_limited"].map(requests);
    assert_eq!(counted, [base[0] + 3, base[1] + 2, base[2] + 1]);
    running.stop().await;
}

/// Without `REQUEST_MEMBERS` a GUILD_CREATE that leaves members out is
/// published, and nothing is requested.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_request_members_nothing_is_requested() {
    let fake = FakeDiscord::start().await;
    let shard = 2;
    let running = start(&fake.gateway.url(), shard, MemberConfig::default());
    let mut conn = identified(&fake, shard, &[2000]).await;
    let partial = partial_guild(2000, 500);
    conn.dispatch("GUILD_CREATE", 2, &partial).await;
    running.published.wait("GUILD_CREATE", 1).await;
    assert_eq!(conn.try_next_frame(SILENCE).await, None);
    running.stop().await;
}
