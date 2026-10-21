#![recursion_limit = "256"]

//! Workers: which `GUILD_CREATE` is a guild join once READY of a new session
//! has cached every guild it lists as a stub (set TEST_REDIS_URL), and that
//! `worker::spawn` always starts a worker, which drains its queue on drop.

mod common;
#[path = "support/fake_discord.rs"]
mod fake_discord;

use common::*;
use fake_discord::FakeDiscord;
use redis::aio::ConnectionLike;
use serde_json::{json, Value};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use twilight_dispatch::{
    config::StateConfig,
    discord_log::DiscordLog,
    gateway::{
        registry::{Registry, ShardHandle},
        runner::parse_event,
        worker::{self, Breaker, Job, Publisher, WorkerContext},
    },
    metrics::GUILD_EVENTS,
    models::ApiResult,
    util::Throttle,
};
use twilight_http::Client;

/// The guild join tests compare the process-wide `GUILD_EVENTS` counter, so
/// they run one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The `LOG_GUILD_CHANNEL` on the fake Discord.
const GUILD_LOG: u64 = 22;

/// The routing key and message id of every published event.
#[derive(Default)]
struct Recorder(Mutex<Vec<(String, Option<String>)>>);

impl Recorder {
    fn published(&self) -> Vec<(String, Option<String>)> {
        self.0.lock().unwrap().clone()
    }
}

impl Publisher for Recorder {
    async fn publish(
        &self,
        routing_key: &str,
        _payload: Vec<u8>,
        message_id: Option<String>,
    ) -> ApiResult<()> {
        let mut published = self.0.lock().unwrap();
        published.push((routing_key.to_owned(), message_id));
        Ok(())
    }
}

/// The Redis connection of workers with the cache off, which never use it.
#[derive(Clone)]
struct NoRedis;

impl ConnectionLike for NoRedis {
    fn req_packed_command<'a>(
        &'a mut self,
        _cmd: &'a redis::Cmd,
    ) -> redis::RedisFuture<'a, redis::Value> {
        unreachable!("the cache is disabled")
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        _pipeline: &'a redis::Pipeline,
        _offset: usize,
        _count: usize,
    ) -> redis::RedisFuture<'a, Vec<redis::Value>> {
        unreachable!("the cache is disabled")
    }

    fn get_db(&self) -> i64 {
        0
    }
}

fn context<C, P>(
    redis: C,
    publisher: Arc<P>,
    registry: Arc<Registry>,
    state: StateConfig,
    discord: DiscordLog,
) -> WorkerContext<C, P> {
    WorkerContext {
        redis,
        publisher,
        registry,
        state,
        bot_id: bot_id(),
        cache_timeout: Duration::from_secs(10),
        discord,
        breaker: Breaker::default(),
        cache_errors: Throttle::default(),
        publish_errors: Throttle::default(),
    }
}

fn registry(handle: &Arc<ShardHandle>) -> Arc<Registry> {
    Arc::new(Registry::new([handle.clone()]))
}

/// A job of shard 0 in session `s1`, as the shard runner builds it.
fn job(handle: &ShardHandle, kind: &str, seq: u64, d: Value) -> Job {
    let json = raw(kind, seq, d);
    Job {
        shard: 0,
        seq: Some(seq),
        token: handle.dispatch(seq),
        session: Some(Arc::from("s1")),
        event: parse_event(&json).unwrap(),
        json,
    }
}

/// A `GUILD_CREATE` of a guild the bot joined just now.
fn recent_guild(id: u64, name: &str) -> Value {
    let mut guild = guild(id, name);
    guild["joined_at"] = json!(now_iso());
    guild
}

fn joins() -> u64 {
    GUILD_EVENTS.with_label_values(&["Join"]).get()
}

/// A guild log posting to `GUILD_LOG` on the fake Discord.
fn guild_log(fake: &FakeDiscord) -> DiscordLog {
    // main() installs it; twilight's rustls connectors panic without one.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let proxy = fake.api.proxy();
    let address = proxy.trim_start_matches("http://").to_owned();
    let client = Client::builder()
        .token("Bot test".to_owned())
        .proxy(address, true)
        .ratelimiter(None)
        .build();
    DiscordLog::new(Arc::new(client), 0, GUILD_LOG)
}

/// The title and description of a posted log message's embed.
fn line(body: &Value) -> (String, String) {
    let embed = &body["embeds"][0];
    let text = |field: &str| embed[field].as_str().unwrap_or_default().to_owned();
    (text("title"), text("description"))
}

/// The guild log lines posted before a marker logged now. One task posts them
/// in order, so once the marker is there everything logged before it is too.
async fn guild_log_lines(fake: &FakeDiscord, log: &DiscordLog) -> Vec<(String, String)> {
    log.guild_leave("marker", 1);
    let path = format!("/channels/{GUILD_LOG}/messages");
    let api = &fake.api;
    api.wait_until("the marker in the guild log", |requests| {
        let mut lines: Vec<(String, String)> = requests
            .iter()
            .filter(|request| request.method == "POST" && request.path == path)
            .map(|request| line(&request.body))
            .collect();
        let marker = lines
            .iter()
            .position(|(_, description)| description == "marker (1)")?;
        lines.truncate(marker);
        Some(lines)
    })
    .await
}

/// A guild joined while no session received its `GUILD_CREATE` (while the
/// bot was disconnected, or queued in the session that ended and so not
/// applied) is listed by READY of the new session, which caches it as a stub
/// without a name. The new session's `GUILD_CREATE` is the join: counted and
/// posted once.
#[tokio::test]
async fn a_join_no_session_reported_is_logged_after_ready() {
    let _serial = SERIAL.lock().await;
    let Some(mut conn) = redis(1).await else {
        return;
    };
    let fake = FakeDiscord::start().await;
    let log = guild_log(&fake);
    let handle = Arc::new(ShardHandle::new(0, None, Some(1)));
    let recorder = Arc::new(Recorder::default());
    let ctx = context(
        conn.clone(),
        recorder.clone(),
        registry(&handle),
        state_all(),
        log.clone(),
    );
    let before = joins();

    let stale = job(&handle, "GUILD_CREATE", 2, recent_guild(1000, "g"));
    handle.new_session(1);
    let state = state_all();
    apply(&mut conn, &state, "READY", ready([0, 1], &[1000, 2000])).await;
    let stub = get(&mut conn, "guild:1000").await.expect("a stub");
    assert!(stub.get("name").is_none(), "{stub}");

    worker::process(&ctx, &mut conn, stale).await;
    assert_eq!(joins() - before, 0, "the stale GUILD_CREATE is no join");

    let current = job(&handle, "GUILD_CREATE", 2, recent_guild(1000, "g"));
    worker::process(&ctx, &mut conn, current).await;
    assert_eq!(joins() - before, 1, "a join of the new session");
    assert_eq!(get(&mut conn, "guild:1000").await.unwrap()["name"], "g");

    // Also listed by READY, but the bot joined it long ago.
    let old = job(&handle, "GUILD_CREATE", 3, guild(2000, "h"));
    worker::process(&ctx, &mut conn, old).await;
    assert_eq!(joins() - before, 1, "a guild joined long ago is no join");

    let lines = guild_log_lines(&fake, &log).await;
    assert_eq!(lines, [("Guild Join".to_owned(), "g (1000)".to_owned())]);
    assert_eq!(recorder.published().len(), 3, "everything is published");
    assert_eq!(handle.in_flight(), 0);
}

/// A guild joined minutes ago is cached in full by its first `GUILD_CREATE`.
/// Neither the next session's `GUILD_CREATE` (READY keeps a cached guild) nor
/// the one after an outage (which only marks it unavailable) is a second
/// join, although its `joined_at` is still recent.
#[tokio::test]
async fn a_recent_join_cached_in_full_is_not_logged_again() {
    let _serial = SERIAL.lock().await;
    let Some(mut conn) = redis(2).await else {
        return;
    };
    let handle = Arc::new(ShardHandle::new(0, None, Some(1)));
    let ctx = context(
        conn.clone(),
        Arc::new(Recorder::default()),
        registry(&handle),
        state_all(),
        DiscordLog::disabled(),
    );
    let before = joins();

    let first = job(&handle, "GUILD_CREATE", 2, recent_guild(1000, "g"));
    worker::process(&ctx, &mut conn, first).await;
    assert_eq!(joins() - before, 1, "an unknown guild is a join");

    handle.new_session(1);
    let state = state_all();
    apply(&mut conn, &state, "READY", ready([0, 1], &[1000])).await;
    assert_eq!(get(&mut conn, "guild:1000").await.unwrap()["name"], "g");
    let resent = job(&handle, "GUILD_CREATE", 2, recent_guild(1000, "g"));
    worker::process(&ctx, &mut conn, resent).await;
    assert_eq!(joins() - before, 1, "re-sent by the next session");

    let outage = json!({"id": "1000", "unavailable": true});
    let delete = job(&handle, "GUILD_DELETE", 3, outage);
    worker::process(&ctx, &mut conn, delete).await;
    let cached = get(&mut conn, "guild:1000").await.unwrap();
    assert_eq!(cached["name"], "g", "{cached}");
    assert_eq!(cached["unavailable"], true, "{cached}");
    let back = job(&handle, "GUILD_CREATE", 4, recent_guild(1000, "g"));
    worker::process(&ctx, &mut conn, back).await;
    assert_eq!(joins() - before, 1, "back after an outage");
}

/// A guild that was never cached and is first seen unavailable (an outage's
/// `GUILD_DELETE`, or an unavailable `GUILD_CREATE`) is cached as a stub
/// without a name as well, so a `GUILD_CREATE` after it with a recent
/// `joined_at` is a join.
#[tokio::test]
async fn a_recent_join_known_only_from_an_unavailable_stub_is_logged() {
    let _serial = SERIAL.lock().await;
    let Some(mut conn) = redis(3).await else {
        return;
    };
    let handle = Arc::new(ShardHandle::new(0, None, Some(1)));
    let ctx = context(
        conn.clone(),
        Arc::new(Recorder::default()),
        registry(&handle),
        state_all(),
        DiscordLog::disabled(),
    );
    let before = joins();

    let outage = json!({"id": "3000", "unavailable": true});
    let delete = job(&handle, "GUILD_DELETE", 2, outage);
    worker::process(&ctx, &mut conn, delete).await;
    let unavailable = json!({"id": "4000", "unavailable": true});
    let create = job(&handle, "GUILD_CREATE", 3, unavailable);
    worker::process(&ctx, &mut conn, create).await;
    for key in ["guild:3000", "guild:4000"] {
        let stub = get(&mut conn, key).await.expect("a stub");
        assert!(stub.get("name").is_none(), "{key}: {stub}");
    }
    assert_eq!(joins() - before, 0);

    let joined = job(&handle, "GUILD_CREATE", 4, recent_guild(3000, "k"));
    worker::process(&ctx, &mut conn, joined).await;
    assert_eq!(joins() - before, 1, "after an outage's stub");
    let joined = job(&handle, "GUILD_CREATE", 5, recent_guild(4000, "m"));
    worker::process(&ctx, &mut conn, joined).await;
    assert_eq!(joins() - before, 2, "after an unavailable GUILD_CREATE");
}

/// `spawn` starts at least one worker even for a count of 0 (the runner
/// indexes into the queues it returns), and a worker publishes what is still
/// queued and stops once every sender is dropped. The cache is off, so Redis
/// is never used.
#[tokio::test]
async fn spawning_no_workers_still_starts_one_that_drains_its_queue() {
    let recorder = Arc::new(Recorder::default());
    let ctx = Arc::new(context(
        NoRedis,
        recorder.clone(),
        Arc::new(Registry::default()),
        StateConfig::default(),
        DiscordLog::disabled(),
    ));
    let (queues, workers) = worker::spawn(0, ctx);
    assert_eq!((queues.len(), workers.len()), (1, 1));

    let queued = Job {
        shard: 0,
        seq: Some(2),
        token: None,
        session: None,
        json: r#"{"op":0,"t":"A","s":2,"d":{}}"#.to_owned(),
        event: None,
    };
    queues[0].send(queued).await.unwrap();
    drop(queues);
    for worker in workers {
        tokio::time::timeout(Duration::from_secs(30), worker)
            .await
            .expect("the worker drains its queue and stops")
            .unwrap();
    }
    let published = recorder.published();
    assert_eq!(published, [("A".to_owned(), Some("0::2".to_owned()))]);
}
