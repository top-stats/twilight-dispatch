#![recursion_limit = "256"]

//! What reaches a running shard besides well-formed dispatches, through
//! `gateway::runner::run` against the fake Discord gateway of
//! `tests/support/fake_discord.rs`. No services are needed: the publisher
//! only records message ids, and Redis is a fake that never answers or fails
//! at once, which the workers never use for these events.
//!
//! Pinned here:
//! - a message twilight rejects (not JSON) is counted as
//!   `gateway_parse_errors{type="SHARD"}`, also when its warning is
//!   throttled, and skipped: the shard stays connected and keeps publishing;
//! - a dispatch whose `op`, `s` and `t` twilight's text scan finds but that
//!   is not JSON is counted the same way, is neither published nor counted as
//!   received, and the resume point moves past it;
//! - without a shutdown, READY's cache update is bounded by the smaller of
//!   the cache timeout and `READY_TIMEOUT`, and one that fails does not hold
//!   the shard up;
//! - `parse_event` keeps the plain error of a `GUILD_CREATE` that fails
//!   outside its guild.
//!
//! The two tests that read the process-wide `SHARD` label hold `SHARD_LABEL`;
//! every test runs its own shard id.

mod common;
#[path = "support/fake_discord.rs"]
mod fake_discord;

use fake_discord::{FakeDiscord, GatewayConnection};
use redis::aio::ConnectionLike;
use serde::de::DeserializeSeed;
use serde_json::json;
use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{sync::watch, task::JoinHandle};
use twilight_dispatch::{
    config::StateConfig,
    discord_log::DiscordLog,
    gateway::{
        registry::{Registry, ShardHandle},
        runner::{self, parse_event, RunnerContext, READY_TIMEOUT},
        worker::{self, Breaker, Publisher, WorkerContext},
    },
    metrics::{GATEWAY_EVENTS, GATEWAY_PARSE_ERRORS},
    models::{ApiResult, SessionInfo},
    util::Throttle,
};
use twilight_gateway::{queue::InMemoryQueue, ConfigBuilder, Intents, Shard, ShardId};
use twilight_model::gateway::event::GatewayEventDeserializer;

/// Shards of this many; every test runs its own shard id.
const TOTAL: u32 = 8;
/// A day: no heartbeat is due during a test, so the gateway only sees the
/// frames a test expects.
const QUIET: u64 = 86_400_000;
/// The bound of every wait; only a failing test waits this long.
const WAIT: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(5);

/// Held by the tests that read `gateway_parse_errors{type="SHARD"}`, which
/// both of them raise.
static SHARD_LABEL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A Redis that never answers (`Hang`) or fails every command at once
/// (`Down`).
#[derive(Clone, Copy)]
enum FakeRedis {
    Hang,
    Down,
}

impl FakeRedis {
    fn reply<'a, T: Send + 'a>(self) -> redis::RedisFuture<'a, T> {
        Box::pin(async move {
            match self {
                Self::Hang => std::future::pending().await,
                Self::Down => Err(std::io::Error::other("Redis is down").into()),
            }
        })
    }
}

impl ConnectionLike for FakeRedis {
    fn req_packed_command<'a>(
        &'a mut self,
        _cmd: &'a redis::Cmd,
    ) -> redis::RedisFuture<'a, redis::Value> {
        self.reply()
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        _pipeline: &'a redis::Pipeline,
        _offset: usize,
        _count: usize,
    ) -> redis::RedisFuture<'a, Vec<redis::Value>> {
        self.reply()
    }

    fn get_db(&self) -> i64 {
        0
    }
}

/// Records the message id of every published event, in order.
#[derive(Default)]
struct Ids(Mutex<Vec<Option<String>>>);

impl Ids {
    fn get(&self) -> Vec<Option<String>> {
        self.0.lock().unwrap().clone()
    }
}

impl Publisher for Ids {
    async fn publish(
        &self,
        _routing_key: &str,
        _payload: Vec<u8>,
        message_id: Option<String>,
    ) -> ApiResult<()> {
        self.0.lock().unwrap().push(message_id);
        Ok(())
    }
}

/// One shard's `run()` with its worker.
struct Running {
    handle: Arc<ShardHandle>,
    ids: Arc<Ids>,
    runner: JoinHandle<()>,
    /// Kept: `run()` takes a dropped sender for a shutdown.
    _shutdown: watch::Sender<bool>,
}

/// Run shard `id` against the gateway at `url` the way `app::start` does,
/// with one worker (whose Redis never answers), `state` and `cache_timeout`,
/// and `redis` for READY's cache update.
fn start(
    url: &str,
    id: u32,
    state: StateConfig,
    cache_timeout: Duration,
    redis: FakeRedis,
) -> Running {
    // main() installs it; twilight's TLS connector panics without one.
    let _ = rustls::crypto::ring::default_provider().install_default();
    // A max_concurrency of 0 grants every IDENTIFY at once.
    let queue = InMemoryQueue::new(0, 1000, Duration::from_secs(86_400), 1000);
    let config = ConfigBuilder::new("runner-token".to_owned(), Intents::GUILDS)
        .proxy_url(url.to_owned())
        .queue(queue)
        .build();
    let shard = Shard::with_config(ShardId::new(id, TOTAL), config);
    let handle = Arc::new(ShardHandle::new(id, Some(shard.sender()), None));
    let ids = Arc::new(Ids::default());
    let worker_ctx = Arc::new(WorkerContext {
        redis: FakeRedis::Hang,
        publisher: ids.clone(),
        registry: Arc::new(Registry::new([handle.clone()])),
        state: state.clone(),
        bot_id: common::bot_id(),
        cache_timeout,
        discord: DiscordLog::disabled(),
        breaker: Breaker::default(),
        cache_errors: Throttle::default(),
        publish_errors: Throttle::default(),
    });
    let (workers, _) = worker::spawn(1, worker_ctx);
    let runner_ctx = Arc::new(RunnerContext {
        state,
        workers,
        redis,
        discord: DiscordLog::disabled(),
        bot_id: common::bot_id(),
        cache_timeout,
        max_held_bytes: runner::MAX_HELD_BYTES,
    });
    let (shutdown, shutdown_rx) = watch::channel(false);
    let runner = tokio::spawn(runner::run(shard, handle.clone(), runner_ctx, shutdown_rx));
    Running {
        handle,
        ids,
        runner,
        _shutdown: shutdown,
    }
}

impl Running {
    /// The message ids published so far, once there are at least `count`.
    async fn wait_ids(&self, count: usize) -> Vec<Option<String>> {
        let deadline = Instant::now() + WAIT;
        loop {
            let ids = self.ids.get();
            if ids.len() >= count {
                return ids;
            }
            assert!(
                Instant::now() < deadline,
                "no {count} published events within {WAIT:?}: {ids:?}"
            );
            tokio::time::sleep(POLL).await;
        }
    }

    /// Wait until the session to save for a resume is `expected`.
    async fn wait_session_info(&self, expected: &SessionInfo) {
        let deadline = Instant::now() + WAIT;
        loop {
            let info = self.handle.session_info();
            if info.as_ref() == Some(expected) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "session {info:?}, expected {expected:?}"
            );
            tokio::time::sleep(POLL).await;
        }
    }
}

/// The cache on, without any of the optional parts.
fn cached() -> StateConfig {
    StateConfig {
        enabled: true,
        ..StateConfig::default()
    }
}

/// Accept the shard's connection, send HELLO and read its IDENTIFY.
async fn identify(fake: &FakeDiscord) -> GatewayConnection {
    let mut conn = fake.gateway.accept().await;
    conn.hello(QUIET).await;
    conn.expect_identify().await;
    conn
}

/// A TYPING_START (published, never cached) with sequence `seq`.
async fn typing(conn: &GatewayConnection, seq: u64) {
    let d = json!({"channel_id": "5", "guild_id": "7", "user_id": "1", "timestamp": seq});
    conn.dispatch("TYPING_START", seq, &d).await;
}

/// The message ids of session `A`'s `seqs` on `shard`, as the worker
/// publishes them.
fn ids(shard: u32, seqs: &[u64]) -> Vec<Option<String>> {
    seqs.iter()
        .map(|seq| Some(format!("{shard}:A:{seq}")))
        .collect()
}

/// `gateway_parse_errors{type="SHARD"}`: messages the shard could not use.
fn shard_parse_errors() -> u64 {
    GATEWAY_PARSE_ERRORS.with_label_values(&["SHARD"]).get()
}

/// `gateway_events{type, shard}`: dispatches the runner received.
fn received(shard: u32, kind: &str) -> u64 {
    GATEWAY_EVENTS
        .with_label_values(&[kind, shard.to_string().as_str()])
        .get()
}

/// Messages twilight rejects while the shard runs (here: not JSON, and
/// longer than what is logged of them) are counted as `SHARD` parse errors,
/// the second one too although its warning is throttled, and skipped: the
/// shard stays connected and the events after them are published.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_twilight_rejects_is_counted_and_skipped() {
    let _shard_label = SHARD_LABEL.lock().await;
    let fake = FakeDiscord::start().await;
    let shard = 1;
    let running = start(
        &fake.gateway.url(),
        shard,
        StateConfig::default(),
        Duration::from_secs(5),
        FakeRedis::Hang,
    );
    let conn = identify(&fake).await;
    conn.ready(1, "A", [shard, TOTAL], &[]).await;
    running.wait_ids(1).await;
    let before = shard_parse_errors();

    let rejected = format!("not json {}", "x".repeat(1000));
    conn.send_text(rejected.clone()).await;
    conn.send_text(rejected).await;
    typing(&conn, 2).await;
    assert_eq!(running.wait_ids(2).await, ids(shard, &[1, 2]));
    // Both were handled before the TYPING_START that follows them.
    assert_eq!(shard_parse_errors() - before, 2);
    let snapshot = running.handle.snapshot();
    assert_eq!(snapshot.status, "Connected");
    assert!(snapshot.active);
    assert!(!running.runner.is_finished());
}

/// Dispatches whose `op`, `s` and `t` twilight's text scan finds but that are
/// not JSON (one broken inside `d`, one cut off): twilight yields them and
/// moves its sequence past them. They cannot be published, so each is
/// counted as a `SHARD` parse error and not as a received event, and the
/// resume point moves past them with the next event.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dispatch_that_is_not_json_is_counted_and_skipped() {
    let _shard_label = SHARD_LABEL.lock().await;
    let fake = FakeDiscord::start().await;
    let shard = 2;
    let running = start(
        &fake.gateway.url(),
        shard,
        StateConfig::default(),
        Duration::from_secs(5),
        FakeRedis::Hang,
    );
    let conn = identify(&fake).await;
    conn.ready(1, "A", [shard, TOTAL], &[]).await;
    running.wait_ids(1).await;
    let before = shard_parse_errors();

    for broken in [
        r#"{"t":"TYPING_START","s":2,"op":0,"d":{"channel_id":}}"#,
        r#"{"t":"TYPING_START","s":3,"op":0,"d":{"channel_id":"5""#,
    ] {
        conn.send_text(broken).await;
    }
    typing(&conn, 4).await;
    assert_eq!(running.wait_ids(2).await, ids(shard, &[1, 4]));
    assert_eq!(shard_parse_errors() - before, 2);
    assert_eq!(received(shard, "TYPING_START"), 1);
    let expected = SessionInfo {
        session_id: "A".to_owned(),
        sequence: 4,
        resume_url: Some(fake.gateway.resume_url()),
    };
    running.wait_session_info(&expected).await;
    assert!(!running.runner.is_finished());
}

/// Without a shutdown, READY's cache update waits for a Redis that never
/// answers for the smaller of the cache timeout and `READY_TIMEOUT` (which is
/// the smaller one in production), then the shard goes on: READY starts the
/// session and is published, and the events after it flow.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn readys_cache_update_is_bounded_without_a_shutdown() {
    for (shard, cache_timeout, bound) in [
        (3, Duration::from_millis(300), Duration::from_millis(300)),
        (4, Duration::from_secs(60), READY_TIMEOUT),
    ] {
        let fake = FakeDiscord::start().await;
        let running = start(
            &fake.gateway.url(),
            shard,
            cached(),
            cache_timeout,
            FakeRedis::Hang,
        );
        let conn = identify(&fake).await;
        let sent = Instant::now();
        conn.ready(1, "A", [shard, TOTAL], &[]).await;
        typing(&conn, 2).await;
        assert_eq!(
            running.wait_ids(2).await,
            ids(shard, &[1, 2]),
            "shard {shard}"
        );
        // The bound starts once READY is received, after `sent`.
        let took = sent.elapsed();
        assert!(
            took >= bound && took < bound + Duration::from_secs(2),
            "shard {shard}: READY held the shard up for {took:?}, expected {bound:?}"
        );
        let snapshot = running.handle.snapshot();
        assert_eq!(snapshot.session_id.as_deref(), Some("A"));
        assert!(!running.runner.is_finished());
    }
}

/// A READY cache update that fails (Redis refuses every command) is given up
/// at once: the shard does not wait out `READY_TIMEOUT`, and READY still
/// starts the session and is published before the events after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_ready_cache_update_does_not_hold_up_the_shard() {
    let fake = FakeDiscord::start().await;
    let shard = 5;
    let running = start(
        &fake.gateway.url(),
        shard,
        cached(),
        Duration::from_secs(60),
        FakeRedis::Down,
    );
    let conn = identify(&fake).await;
    let sent = Instant::now();
    conn.ready(1, "A", [shard, TOTAL], &[]).await;
    typing(&conn, 2).await;
    assert_eq!(running.wait_ids(2).await, ids(shard, &[1, 2]));
    let took = sent.elapsed();
    assert!(
        took < READY_TIMEOUT / 2,
        "READY held the shard up for {took:?}"
    );
    let snapshot = running.handle.snapshot();
    assert_eq!(snapshot.session_id.as_deref(), Some("A"));
    assert_eq!(snapshot.status, "Connected");
    assert!(!running.runner.is_finished());
}

/// A GUILD_CREATE that fails outside its guild (here: it has no `s`) keeps
/// the plain error: the guild is parsed again only to say which of its
/// fields fails, and here none does.
#[test]
fn a_guild_create_that_fails_outside_its_guild_keeps_the_plain_error() {
    let guild = common::guild(1 << 22, "g");
    let json = format!(r#"{{"t":"GUILD_CREATE","op":0,"d":{guild}}}"#);
    let mut deserializer = serde_json::Deserializer::from_str(&json);
    let plain = GatewayEventDeserializer::new(0, Some("GUILD_CREATE"))
        .deserialize(&mut deserializer)
        .expect_err("a dispatch without `s`")
        .to_string();
    assert!(plain.starts_with("missing field `s`"), "{plain}");
    assert_eq!(parse_event(&json).expect_err("no `s`"), plain);
}
