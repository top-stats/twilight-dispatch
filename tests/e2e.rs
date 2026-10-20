#![recursion_limit = "256"]

//! End-to-end tests: the whole service (`app::run_until`) against a fake
//! Discord (`tests/support/fake_discord.rs`), a real Redis and a real
//! LavinMQ (set TEST_REDIS_URL and TEST_AMQP_URL).
//!
//! Each test uses its own Redis database (the two startup-failure tests,
//! which never write, share one). Tests that run the gateway share the
//! `gateway.recv` / `gateway.send` queues and the process-wide metrics and
//! health state, so they run one at a time (`SERIAL`); every one ends with a
//! graceful shutdown.
//!
//! The shard always waits 1 s before connecting, and a second IDENTIFY of
//! one process waits up to 5 s for twilight's identify queue, so these tests
//! take a few seconds each.

mod common;
#[path = "support/fake_discord.rs"]
mod fake_discord;

use common::*;
use fake_discord::{FakeDiscord, Frame, GatewayConnection, Options, TIMEOUT};
use futures_util::StreamExt;
use lapin::{
    options::{BasicConsumeOptions, BasicPublishOptions, QueueDeclareOptions, QueuePurgeOptions},
    types::FieldTable,
    BasicProperties, Channel, Connection, ConnectionProperties, Consumer,
};
use redis::aio::ConnectionManager;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::oneshot,
};
use twilight_dispatch::{
    app,
    config::Config,
    constants::{QUEUE_RECV, QUEUE_SEND},
    metrics::{self, GATEWAY_COMMANDS},
    models::{ApiError, ApiResult, FormattedDateTime},
};

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const TOKEN: &str = "e2e-token";
/// Discord's usual interval: no heartbeat is due during most tests.
const HEARTBEAT_MS: u64 = 41_250;
/// The guild of `common::guild(GUILD, ..)`: roles 1000/1001, emoji 1002,
/// channels 1003/1004, thread 1005, members 900 (the bot) and 10.
const GUILD: u64 = 1000;
const QUERY: &str = "v=10&encoding=json&compress=zstd-stream";

// ---------------------------------------------------------------------------
// Harness

type Vars = HashMap<&'static str, String>;

/// A port nothing listens on right now.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn base_vars(fake: &FakeDiscord, redis_url: String, amqp_url: String, port: u16) -> Vars {
    let mut vars: Vars = [
        ("BOT_TOKEN", TOKEN),
        ("SHARDS_START", "0"),
        ("SHARDS_END", "0"),
        ("SHARDS_TOTAL", "1"),
        ("INTENTS", "513"),
        ("LARGE_THRESHOLD", "100"),
        ("STATUS", "idle"),
        ("ACTIVITY_TYPE", "3"),
        ("ACTIVITY_NAME", "the tests"),
        ("PROMETHEUS_HOST", "127.0.0.1"),
        ("STATE_MEMBER", "true"),
        ("STATE_MESSAGE", "true"),
        ("STATE_PRESENCE", "true"),
        ("STATE_EMOJI", "true"),
        ("STATE_VOICE", "true"),
        ("STATE_OLD", "false"),
        ("WORKERS", "4"),
    ]
    .into_iter()
    .map(|(name, value)| (name, value.to_owned()))
    .collect();
    vars.extend(fake.env());
    vars.insert("REDIS_URL", redis_url);
    vars.insert("AMQP_URL", amqp_url);
    vars.insert("PROMETHEUS_PORT", port.to_string());
    vars
}

/// The service's environment for Redis database `db`, on a fresh metrics port.
fn vars(fake: &FakeDiscord, db: u8) -> Vars {
    let redis = std::env::var("TEST_REDIS_URL").unwrap();
    let amqp = std::env::var("TEST_AMQP_URL").unwrap();
    base_vars(
        fake,
        format!("{}/{db}", redis.trim_end_matches('/')),
        amqp,
        free_port(),
    )
}

fn with(mut vars: Vars, overrides: &[(&'static str, &str)]) -> Vars {
    for &(name, value) in overrides {
        vars.insert(name, value.to_owned());
    }
    vars
}

fn without(mut vars: Vars, names: &[&str]) -> Vars {
    vars.retain(|name, _| !names.contains(name));
    vars
}

/// The service, running `app::run_until` until `stop`.
struct App {
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<ApiResult<()>>,
    metrics_port: u16,
}

impl App {
    fn start(vars: &Vars) -> Self {
        // main() installs it; twilight's rustls connectors panic without one.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let config = Config::from_lookup(&|name| vars.get(name).cloned()).expect("valid config");
        let metrics_port = config.prometheus_port;
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(app::run_until(config, async move {
            let _ = stopped.await;
        }));
        Self {
            stop: Some(stop),
            task,
            metrics_port,
        }
    }

    /// Trigger the shutdown and wait for `run_until` to return.
    async fn stop(mut self) -> ApiResult<()> {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.finished().await
    }

    /// Wait for `run_until` to return on its own.
    async fn finished(&mut self) -> ApiResult<()> {
        tokio::time::timeout(Duration::from_secs(15), &mut self.task)
            .await
            .expect("run_until returned within 15s")
            .expect("run_until did not panic")
    }
}

/// One message the service published to the `gateway` exchange.
#[derive(Debug)]
struct Published {
    key: String,
    raw: String,
    body: Value,
    message_id: Option<String>,
    /// Arrival order.
    index: usize,
}

/// A consumer of `gateway.recv` and a producer for `gateway.send`.
struct Broker {
    connection: Connection,
    channel: Channel,
    consumer: Consumer,
    buffered: Vec<Published>,
    received: usize,
}

impl Broker {
    /// Purge both queues (left-overs of earlier runs) and start consuming.
    async fn connect(url: &str) -> Self {
        let connection = Connection::connect(url, ConnectionProperties::default())
            .await
            .unwrap();
        let channel = connection.create_channel().await.unwrap();
        for queue in [QUEUE_RECV, QUEUE_SEND] {
            // The service's flags; it binds gateway.recv itself on connect.
            channel
                .queue_declare(
                    queue.into(),
                    QueueDeclareOptions {
                        durable: true,
                        ..Default::default()
                    },
                    FieldTable::default(),
                )
                .await
                .unwrap();
            channel
                .queue_purge(queue.into(), QueuePurgeOptions::default())
                .await
                .unwrap();
        }
        let consumer = channel
            .basic_consume(
                QUEUE_RECV.into(),
                "e2e".into(),
                BasicConsumeOptions {
                    no_ack: true,
                    ..Default::default()
                },
                FieldTable::default(),
            )
            .await
            .unwrap();
        Self {
            connection,
            channel,
            consumer,
            buffered: Vec::new(),
            received: 0,
        }
    }

    /// The first published message with this routing key not taken yet.
    async fn take(&mut self, key: &str) -> Published {
        if let Some(position) = self.buffered.iter().position(|p| p.key == key) {
            return self.buffered.remove(position);
        }
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        loop {
            let delivery = tokio::time::timeout_at(deadline, self.consumer.next())
                .await
                .unwrap_or_else(|_| {
                    let keys: Vec<&str> = self.buffered.iter().map(|p| p.key.as_str()).collect();
                    panic!("no {key} published within {TIMEOUT:?}; other messages: {keys:?}")
                })
                .expect("consumer open")
                .expect("delivery");
            let raw = String::from_utf8(delivery.data.clone()).expect("UTF-8 body");
            let published = Published {
                key: delivery.routing_key.to_string(),
                body: serde_json::from_str(&raw).expect("JSON body"),
                raw,
                message_id: delivery
                    .properties
                    .message_id()
                    .as_ref()
                    .map(|id| id.to_string()),
                index: self.received,
            };
            self.received += 1;
            if published.key == key {
                return published;
            }
            self.buffered.push(published);
        }
    }

    /// Publish a gateway command to `gateway.send`.
    async fn command(&self, body: &[u8]) {
        self.channel
            .basic_publish(
                "".into(),
                QUEUE_SEND.into(),
                BasicPublishOptions::default(),
                body,
                BasicProperties::default(),
            )
            .await
            .unwrap();
    }

    async fn close(self) {
        let _ = self.connection.close(200, "done".into()).await;
    }
}

struct Setup {
    _serial: tokio::sync::MutexGuard<'static, ()>,
    redis: ConnectionManager,
    broker: Broker,
}

/// Wait for the other gateway tests, then a clean Redis database `db` and
/// empty queues. `None` when the services are not configured.
async fn setup(db: u8) -> Option<Setup> {
    let serial = SERIAL.lock().await;
    let redis = common::redis(db).await?;
    let url = common::amqp_url()?;
    let broker = Broker::connect(&url).await;
    Some(Setup {
        _serial: serial,
        redis,
        broker,
    })
}

/// Poll `check` until it returns something (for the once-a-second jobs).
async fn eventually<T>(what: &str, mut check: impl AsyncFnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(value) = check().await {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// GET from the metrics server: (status, body).
async fn http_get(port: u16, path: &str) -> (u16, String) {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    stream
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    let status = response
        .get(9..12)
        .and_then(|status| status.parse().ok())
        .unwrap_or_else(|| panic!("not an HTTP response: {response:?}"));
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_owned())
        .unwrap_or_default();
    (status, body)
}

async fn get_string(conn: &mut ConnectionManager, key: &str) -> Option<String> {
    redis::cmd("GET").arg(key).query_async(conn).await.unwrap()
}

async fn ttl(conn: &mut ConnectionManager, key: &str) -> i64 {
    redis::cmd("TTL").arg(key).query_async(conn).await.unwrap()
}

/// The saved session of shard 0 once it has `sequence`.
async fn saved_session(conn: &mut ConnectionManager, session_id: &str, sequence: u64) -> Value {
    eventually(
        &format!("gateway_session:0 is {session_id} at {sequence}"),
        async || {
            let session = get(conn, "gateway_session:0").await?;
            (session["session_id"] == session_id && session["sequence"] == sequence)
                .then_some(session)
        },
    )
    .await
}

fn commands(shard: &str, result: &str) -> u64 {
    GATEWAY_COMMANDS.with_label_values(&[shard, result]).get()
}

/// A TYPING_START of guild 1000 (published, never cached).
fn typing(seq: u64) -> Value {
    json!({"channel_id": "1003", "guild_id": "1000", "user_id": "10", "timestamp": 1_700_000_000 + seq})
}

/// Accept the shard's first connection, HELLO, IDENTIFY and READY (seq 1).
async fn ready_shard(fake: &FakeDiscord, session: &str, guilds: &[u64]) -> GatewayConnection {
    let mut conn = fake.gateway.accept().await;
    assert!(
        !conn.is_resume(),
        "a new session starts at DISCORD_GATEWAY_URL"
    );
    conn.hello(HEARTBEAT_MS).await;
    conn.expect_identify().await;
    conn.ready(1, session, [0, 1], guilds).await;
    conn
}

/// After a resumable close: the shard reconnects to READY's resume URL and
/// RESUMEs. Returns the new connection and the RESUME's `d`.
async fn expect_resume(fake: &FakeDiscord) -> (GatewayConnection, Value) {
    let mut conn = fake.gateway.accept().await;
    assert_eq!(
        conn.path, "/resume/",
        "resumes go to READY's resume_gateway_url"
    );
    assert_eq!(conn.query, QUERY);
    conn.hello(HEARTBEAT_MS).await;
    let resume = conn.expect_resume().await;
    (conn, resume)
}

fn resume_payload(session_id: &str, seq: u64) -> Value {
    json!({"token": format!("Bot {TOKEN}"), "session_id": session_id, "seq": seq})
}

// ---------------------------------------------------------------------------
// Startup and the published / cached data

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn startup_identifies_caches_and_publishes() {
    let Some(mut s) = setup(1).await else {
        return;
    };
    // Expiry deadlines in the hash older versions wrote: one past, one future.
    for (key, deadline) in [
        ("message:5:6", "2020-01-01T00:00:00.0"),
        ("member:7:8", "2100-01-01T00:00:00.0"),
    ] {
        let _: () = redis::pipe()
            .cmd("SET")
            .arg(key)
            .arg("{}")
            .cmd("HSET")
            .arg("expiry_keys")
            .arg(key)
            .arg(json!(deadline).to_string())
            .query_async(&mut s.redis)
            .await
            .unwrap();
    }
    let _: () = redis::pipe()
        .cmd("SADD")
        .arg("message_keys")
        .arg("message:5:6")
        .cmd("SADD")
        .arg("channel_keys:5")
        .arg("message:5:6")
        .query_async(&mut s.redis)
        .await
        .unwrap();

    let fake = FakeDiscord::start().await;
    let app = App::start(&vars(&fake, 1));

    let mut conn = fake.gateway.accept().await;
    assert_eq!(conn.path, "/");
    // zstd-stream: Discord compresses (the fake sends plain text, which
    // twilight accepts too).
    assert_eq!(conn.query, QUERY);
    conn.hello(HEARTBEAT_MS).await;
    let identify = conn.expect_identify().await;
    assert_eq!(identify["token"], format!("Bot {TOKEN}"));
    assert_eq!(identify["intents"], 513);
    assert_eq!(identify["shard"], json!([0, 1]));
    assert_eq!(identify["large_threshold"], 100);
    assert_eq!(identify["compress"], false);
    let presence = &identify["presence"];
    assert_eq!(presence["status"], "idle");
    assert_eq!(presence["afk"], false);
    assert_eq!(presence["since"], Value::Null);
    let activity = &presence["activities"][0];
    assert_eq!(
        (&activity["name"], &activity["type"]),
        (&json!("the tests"), &json!(3))
    );
    assert!(activity.get("state").is_none(), "{activity}");

    // Logged in, then asked Discord for the gateway, with the bot token.
    let requests = fake.api.requests();
    let calls: Vec<String> = requests
        .iter()
        .map(|r| format!("{} {}", r.method, r.path))
        .collect();
    assert_eq!(calls, ["GET /users/@me", "GET /gateway/bot"]);
    for request in &requests {
        assert_eq!(request.authorization.as_deref(), Some("Bot e2e-token"));
    }

    let ready = conn.ready(1, "sess-1", [0, 1], &[GUILD]).await;
    // The name as a JSON escape (backslash, "u00e9"), which re-serializing
    // would turn into "é".
    let escaped = format!("caf{}u00e9", char::from(92u8));
    let guild_d = guild(GUILD, "e2e guild")
        .to_string()
        .replace("e2e guild", &escaped);
    assert!(guild_d.contains(r#""name":"caf"#) && !guild_d.contains('é'));
    conn.dispatch_raw("GUILD_CREATE", 2, &guild_d).await;

    // Published as {"op","t","d"} with `d` passed through byte for byte,
    // routed by event name, with "<shard>:<session>:<seq>" as message id.
    let published = s.broker.take("READY").await;
    assert_eq!(
        published.raw,
        format!(r#"{{"op":0,"t":"READY","d":{ready}}}"#)
    );
    assert_eq!(published.message_id.as_deref(), Some("0:sess-1:1"));
    let published = s.broker.take("GUILD_CREATE").await;
    assert_eq!(
        published.raw,
        format!(r#"{{"op":0,"t":"GUILD_CREATE","d":{guild_d}}}"#)
    );
    assert_eq!(published.message_id.as_deref(), Some("0:sess-1:2"));

    // A consumer that reads the cache after receiving the event sees it.
    let redis = &mut s.redis;
    let blob = get(redis, "guild:1000").await.expect("guild cached");
    assert_eq!(blob["name"], "café");
    assert_eq!(blob["id"], "1000");
    assert_eq!(blob["unavailable"], false);
    for key in [
        "channel:1003",
        "channel:1004",
        "thread:1000:1005",
        "role:1000:1000",
        "role:1000:1001",
        "emoji:1000:1002",
        "member:1000:900",
        "member:1000:10",
        "presence:1000:10",
        "voice:1000:10",
    ] {
        assert!(exists(redis, key).await, "{key} is cached");
    }
    assert!(
        !exists(redis, "presence:1000:11").await,
        "offline presences are not"
    );
    assert!(members(redis, "guild_keys")
        .await
        .contains(&"guild:1000".to_owned()));
    assert_eq!(get(redis, "bot_user").await.unwrap()["id"], "900");
    assert_eq!(
        get_string(redis, "gateway_shards").await.as_deref(),
        Some("1")
    );
    let started = get_string(redis, "gateway_started")
        .await
        .expect("gateway_started");
    assert!(
        serde_json::from_str::<FormattedDateTime>(&started).is_ok(),
        "{started}"
    );

    // The status and session jobs (every second).
    let statuses = eventually("gateway_statuses shows shard 0 connected", async || {
        let statuses = get(redis, "gateway_statuses").await?;
        (statuses[0]["status"] == "Connected").then_some(statuses)
    })
    .await;
    let status = statuses[0].as_object().unwrap();
    let mut fields: Vec<&str> = status.keys().map(String::as_str).collect();
    fields.sort_unstable();
    assert_eq!(fields, ["last_ack", "latency", "shard", "status"]);
    assert_eq!(status["shard"], 0);
    assert!(FormattedDateTime::parse(status["last_ack"].as_str().unwrap()).is_some());
    let session = saved_session(redis, "sess-1", 2).await;
    assert_eq!(
        session,
        json!({"session_id": "sess-1", "sequence": 2, "resume_url": fake.gateway.resume_url()})
    );
    let expires = ttl(redis, "gateway_session:0").await;
    assert!((1..=600).contains(&expires), "TTL {expires}");
    assert_eq!(
        get(redis, "gateway_sessions").await,
        Some(json!({"0": session}))
    );

    // Legacy expiry entries were migrated on startup and the expiry job
    // deleted the overdue key with its index entries.
    eventually("the overdue message expired", async || {
        (!exists(redis, "message:5:6").await).then_some(())
    })
    .await;
    assert!(members(redis, "message_keys").await.is_empty());
    assert!(members(redis, "channel_keys:5").await.is_empty());
    let legacy: u64 = redis::cmd("HLEN")
        .arg("expiry_keys")
        .query_async(redis)
        .await
        .unwrap();
    assert_eq!(legacy, 0, "the legacy hash is emptied");
    assert!(exists(redis, "member:7:8").await);
    assert_eq!(score(redis, "member:7:8").await, Some(4_102_444_800_000.0));

    // Metrics server.
    let port = app.metrics_port;
    let (status, body) = http_get(port, "/healthcheck").await;
    assert_eq!((status, body.as_str()), (200, r#"{"status":"OK"}"#));
    let (status, metrics) = http_get(port, "/metrics").await;
    assert_eq!(status, 200);
    assert!(
        metrics
            .lines()
            .any(|line| line.starts_with("gateway_events{")
                && line.contains(r#"shard="0""#)
                && line.contains(r#"type="GUILD_CREATE""#)),
        "{metrics}"
    );
    assert!(
        metrics.lines().any(|line| line == "amqp_connected 1"),
        "{metrics}"
    );
    assert_eq!(http_get(port, "/nothing").await.0, 404);

    app.stop().await.expect("clean shutdown");
    assert_eq!(conn.expect_close().await, Some(4000), "closed resumably");
    s.broker.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn message_events_carry_the_previous_cached_value_as_old() {
    let Some(mut s) = setup(2).await else {
        return;
    };
    let fake = FakeDiscord::start().await;
    let app = App::start(&with(vars(&fake, 2), &[("STATE_OLD", "true")]));
    let conn = ready_shard(&fake, "sess-2", &[GUILD]).await;

    conn.dispatch("GUILD_CREATE", 2, &guild(GUILD, "g")).await;
    let created = s.broker.take("GUILD_CREATE").await;
    // `old` of a GUILD_CREATE is what was cached before: READY's stub.
    assert_eq!(
        created.body["old"],
        json!({"id": "1000", "unavailable": true})
    );

    conn.dispatch("MESSAGE_CREATE", 3, &message(1, 1003, Some(GUILD), "first"))
        .await;
    let mut edited = message(1, 1003, Some(GUILD), "edited");
    edited["edited_timestamp"] = json!("2024-01-01T00:01:00.000000+00:00");
    conn.dispatch("MESSAGE_UPDATE", 4, &edited).await;
    conn.dispatch(
        "MESSAGE_DELETE",
        5,
        &json!({"id": "1", "channel_id": "1003", "guild_id": "1000"}),
    )
    .await;

    let create = s.broker.take("MESSAGE_CREATE").await;
    assert!(create.body.get("old").is_none(), "{}", create.raw);
    let update = s.broker.take("MESSAGE_UPDATE").await;
    assert_eq!(update.body["d"]["content"], "edited");
    assert_eq!(update.body["old"]["content"], "first");
    let delete = s.broker.take("MESSAGE_DELETE").await;
    assert_eq!(delete.body["old"]["id"], "1");
    assert_eq!(
        delete.body["old"]["content"], "edited",
        "old is the updated message"
    );
    assert!(
        created.index < create.index && create.index < update.index && update.index < delete.index,
        "one guild's events are published in gateway order"
    );
    assert!(!exists(&mut s.redis, "message:1003:1").await);

    conn.dispatch("MESSAGE_CREATE", 6, &message(2, 1003, Some(GUILD), "two"))
        .await;
    conn.dispatch("MESSAGE_CREATE", 7, &message(3, 1003, Some(GUILD), "three"))
        .await;
    conn.dispatch(
        "MESSAGE_DELETE_BULK",
        8,
        &json!({"ids": ["2", "3", "4"], "channel_id": "1003", "guild_id": "1000"}),
    )
    .await;
    let bulk = s.broker.take("MESSAGE_DELETE_BULK").await;
    let old: Vec<&Value> = bulk.body["old"]
        .as_array()
        .expect("old is an array")
        .iter()
        .map(|message| &message["content"])
        .collect();
    assert_eq!(
        old,
        [&json!("two"), &json!("three")],
        "unknown ids are skipped"
    );
    assert!(!exists(&mut s.redis, "message:1003:2").await);

    app.stop().await.expect("clean shutdown");
    s.broker.close().await;
}

// ---------------------------------------------------------------------------
// Gateway commands (gateway.send)

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gateway_commands_reach_the_shard_and_invalid_ones_never_do() {
    let Some(mut s) = setup(3).await else {
        return;
    };
    let fake = FakeDiscord::start().await;
    let app = App::start(&vars(&fake, 3));
    let mut conn = ready_shard(&fake, "sess-3", &[]).await;
    s.broker.take("READY").await;

    let invalid = commands("", "invalid");
    let sent = commands("0", "sent");
    // An injected IDENTIFY, a shard that does not exist, garbage.
    s.broker
        .command(br#"{"op":0,"shard":0,"data":{"op":2,"d":{"token":"injected"}}}"#)
        .await;
    s.broker
        .command(br#"{"op":0,"shard":5,"data":{"op":3,"d":{"status":"dnd","afk":false,"since":null,"activities":[]}}}"#)
        .await;
    s.broker.command(b"not json").await;
    let presence =
        json!({"op": 3, "d": {"status": "dnd", "afk": false, "since": null, "activities": []}});
    s.broker
        .command(
            json!({"op": 0, "shard": 0, "data": presence})
                .to_string()
                .as_bytes(),
        )
        .await;
    let members = json!({"op": 8, "d": {"guild_id": "1000", "query": "", "limit": 0}});
    s.broker
        .command(
            json!({"op": 0, "shard": 0, "data": members})
                .to_string()
                .as_bytes(),
        )
        .await;

    // Commands are handled in order, so the rejected ones would have arrived
    // first; expect_op fails on any other frame.
    assert_eq!(conn.expect_op(3).await, presence);
    assert_eq!(conn.expect_op(8).await, members);
    assert_eq!(commands("", "invalid") - invalid, 3);
    assert_eq!(commands("0", "sent") - sent, 2);

    // Reconnect: closed resumably, then resumed at the resume URL.
    s.broker.command(br#"{"op":1,"shard":0}"#).await;
    assert_eq!(conn.expect_close().await, Some(4000));
    conn.expect_gone().await;
    let (mut conn, resume) = expect_resume(&fake).await;
    assert_eq!(resume, resume_payload("sess-3", 1));
    conn.resumed(2).await;

    // Commands follow the shard to its new connection.
    s.broker
        .command(
            json!({"op": 0, "shard": 0, "data": presence})
                .to_string()
                .as_bytes(),
        )
        .await;
    assert_eq!(conn.expect_op(3).await, presence);

    app.stop().await.expect("clean shutdown");
    s.broker.close().await;
}

/// Accept `count` connections, HELLO them and return them by the shard
/// their IDENTIFY names.
async fn identified_shards(fake: &FakeDiscord, count: usize) -> Vec<(Value, GatewayConnection)> {
    let mut connections = Vec::new();
    for _ in 0..count {
        let conn = fake.gateway.accept().await;
        conn.hello(HEARTBEAT_MS).await;
        connections.push(conn);
    }
    let mut shards = Vec::new();
    for mut conn in connections {
        // A second identify may wait for the identify queue (5 s).
        shards.push((conn.expect_identify().await["shard"].clone(), conn));
    }
    shards.sort_by_key(|(shard, _)| shard[0].as_u64());
    shards
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shards_follow_discord_and_commands_reach_the_process_running_them() {
    let Some(mut s) = setup(14).await else {
        return;
    };
    let mut options = Options {
        shards: 2,
        ..Options::default()
    };
    options.session_start_limit.max_concurrency = 2;
    let fake = FakeDiscord::with_options(options).await;
    let presence =
        json!({"op": 3, "d": {"status": "dnd", "afk": false, "since": null, "activities": []}});
    let command = |shard: u32| {
        json!({"op": 0, "shard": shard, "data": presence})
            .to_string()
            .into_bytes()
    };

    // No SHARDS_*: every shard of Discord's recommended count.
    let app = App::start(&without(
        vars(&fake, 14),
        &["SHARDS_START", "SHARDS_END", "SHARDS_TOTAL"],
    ));
    let mut shards = identified_shards(&fake, 2).await;
    let ids: Vec<&Value> = shards.iter().map(|(shard, _)| shard).collect();
    assert_eq!(ids, [&json!([0, 2]), &json!([1, 2])]);
    shards[0].1.ready(1, "sess-a", [0, 2], &[]).await;
    shards[1].1.ready(1, "sess-b", [1, 2], &[]).await;
    let mut ids = vec![
        s.broker.take("READY").await.message_id.unwrap(),
        s.broker.take("READY").await.message_id.unwrap(),
    ];
    ids.sort();
    assert_eq!(ids, ["0:sess-a:1", "1:sess-b:1"]);
    assert_eq!(
        get_string(&mut s.redis, "gateway_shards").await.as_deref(),
        Some("2")
    );
    // Each command goes to its own shard only (expect_op fails on another
    // frame, so shard 0 did not get shard 1's command).
    s.broker.command(&command(1)).await;
    s.broker.command(&command(0)).await;
    assert_eq!(shards[1].1.expect_op(3).await, presence);
    assert_eq!(shards[0].1.expect_op(3).await, presence);
    app.stop().await.expect("clean shutdown");

    // A process running only shard 1 of 2 forwards shard 0's commands to the
    // process running it (none here: the broker drops them).
    let _: () = redis::cmd("FLUSHDB")
        .query_async(&mut s.redis)
        .await
        .unwrap();
    let app = App::start(&with(
        vars(&fake, 14),
        &[
            ("SHARDS_START", "1"),
            ("SHARDS_END", "1"),
            ("SHARDS_TOTAL", "2"),
        ],
    ));
    let mut shards = identified_shards(&fake, 1).await;
    assert_eq!(shards[0].0, json!([1, 2]));
    let (_, conn) = &mut shards[0];
    conn.ready(1, "sess-c", [1, 2], &[]).await;
    s.broker.take("READY").await;
    let forwarded = commands("0", "forwarded");
    s.broker.command(&command(0)).await;
    s.broker.command(&command(1)).await;
    assert_eq!(conn.expect_op(3).await, presence);
    assert_eq!(commands("0", "forwarded") - forwarded, 1);
    // Two connections of the first run and shard 1's; shard 0 would have
    // connected together with shard 1.
    assert_eq!(fake.gateway.connections(), 3, "shard 0 is not run here");

    app.stop().await.expect("clean shutdown");
    s.broker.close().await;
}

// ---------------------------------------------------------------------------
// Session handling

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconnect_request_resumes_at_the_resume_url() {
    let Some(mut s) = setup(4).await else {
        return;
    };
    let fake = FakeDiscord::start().await;
    let app = App::start(&vars(&fake, 4));
    let mut conn = ready_shard(&fake, "sess-4", &[GUILD]).await;
    conn.dispatch("GUILD_CREATE", 2, &guild(GUILD, "g")).await;
    conn.dispatch("TYPING_START", 3, &typing(3)).await;

    conn.reconnect().await;
    assert_eq!(conn.expect_close().await, Some(4000), "a resumable close");
    conn.expect_gone().await;
    let (conn, resume) = expect_resume(&fake).await;
    assert_eq!(resume, resume_payload("sess-4", 3));

    conn.resumed(4).await;
    conn.dispatch("TYPING_START", 5, &typing(5)).await;
    let resumed = s.broker.take("RESUMED").await;
    assert_eq!(resumed.message_id.as_deref(), Some("0:sess-4:4"));
    s.broker.take("TYPING_START").await;
    let replayed = s.broker.take("TYPING_START").await;
    assert_eq!(replayed.message_id.as_deref(), Some("0:sess-4:5"));
    let session = saved_session(&mut s.redis, "sess-4", 5).await;
    assert_eq!(session["resume_url"], fake.gateway.resume_url());

    app.stop().await.expect("clean shutdown");
    s.broker.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invalid_session_that_is_not_resumable_identifies_again() {
    let Some(mut s) = setup(5).await else {
        return;
    };
    let fake = FakeDiscord::start().await;
    let app = App::start(&vars(&fake, 5));
    let mut conn = ready_shard(&fake, "sess-5", &[]).await;
    conn.dispatch("TYPING_START", 2, &typing(2)).await;

    conn.invalid_session(false).await;
    assert_eq!(
        conn.expect_close().await,
        Some(1000),
        "the session is given up"
    );
    conn.expect_gone().await;

    let mut conn = fake.gateway.accept().await;
    assert_eq!(
        conn.path, "/",
        "a new session starts at DISCORD_GATEWAY_URL"
    );
    conn.hello(HEARTBEAT_MS).await;
    let identify = conn.expect_identify().await;
    assert_eq!(identify["shard"], json!([0, 1]));
    conn.ready(1, "sess-5b", [0, 1], &[]).await;

    let first = s.broker.take("READY").await;
    let second = s.broker.take("READY").await;
    assert_eq!(first.message_id.as_deref(), Some("0:sess-5:1"));
    assert_eq!(second.message_id.as_deref(), Some("0:sess-5b:1"));
    saved_session(&mut s.redis, "sess-5b", 1).await;

    app.stop().await.expect("clean shutdown");
    s.broker.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn invalid_session_that_is_resumable_resumes() {
    let Some(s) = setup(6).await else {
        return;
    };
    let fake = FakeDiscord::start().await;
    let app = App::start(&vars(&fake, 6));
    let mut conn = ready_shard(&fake, "sess-6", &[]).await;
    conn.dispatch("TYPING_START", 2, &typing(2)).await;

    conn.invalid_session(true).await;
    assert_eq!(conn.expect_close().await, Some(4000));
    conn.expect_gone().await;
    let (_conn, resume) = expect_resume(&fake).await;
    assert_eq!(resume, resume_payload("sess-6", 2));

    app.stop().await.expect("clean shutdown");
    s.broker.close().await;
}

/// Discord closes with a code that means "reconnect and start a new
/// session" (the vendored twilight-gateway patch): IDENTIFY, never RESUME.
async fn close_starts_a_new_session(code: u16, db: u8) {
    let Some(mut s) = setup(db).await else {
        return;
    };
    let fake = FakeDiscord::start().await;
    let app = App::start(&vars(&fake, db));
    let session = format!("sess-{code}");
    let mut conn = ready_shard(&fake, &session, &[]).await;
    conn.dispatch("TYPING_START", 2, &typing(2)).await;
    saved_session(&mut s.redis, &session, 2).await;

    conn.close(code, "session gone").await;
    assert_eq!(conn.expect_close().await, Some(code), "the client's echo");
    conn.expect_gone().await;

    let mut conn = fake.gateway.accept().await;
    assert_eq!(conn.path, "/", "not the dead session's resume URL");
    conn.hello(HEARTBEAT_MS).await;
    // expect_identify fails on a RESUME.
    assert_eq!(conn.expect_identify().await["shard"], json!([0, 1]));
    conn.ready(1, "sess-new", [0, 1], &[]).await;
    saved_session(&mut s.redis, "sess-new", 1).await;

    app.stop().await.expect("clean shutdown");
    s.broker.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_4009_session_timed_out_starts_a_new_session() {
    close_starts_a_new_session(4009, 7).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_4007_invalid_seq_starts_a_new_session() {
    close_starts_a_new_session(4007, 8).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resumable_closes_and_dropped_connections_resume() {
    let Some(s) = setup(15).await else {
        return;
    };
    let fake = FakeDiscord::start().await;
    let app = App::start(&vars(&fake, 15));
    let mut conn = ready_shard(&fake, "sess-15", &[]).await;
    conn.dispatch("TYPING_START", 2, &typing(2)).await;

    // 4000 "unknown error": Discord allows resuming.
    conn.close(4000, "unknown error").await;
    assert_eq!(conn.expect_close().await, Some(4000));
    conn.expect_gone().await;
    let (conn, resume) = expect_resume(&fake).await;
    assert_eq!(resume, resume_payload("sess-15", 2));
    conn.resumed(3).await;

    // The connection drops without a close frame.
    conn.drop_connection().await;
    let (_conn, resume) = expect_resume(&fake).await;
    assert_eq!(resume, resume_payload("sess-15", 3));

    app.stop().await.expect("clean shutdown");
    s.broker.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn heartbeats_carry_the_sequence_and_a_zombied_connection_is_resumed() {
    let Some(s) = setup(12).await else {
        return;
    };
    let fake = FakeDiscord::start().await;
    let app = App::start(&vars(&fake, 12));
    let mut conn = fake.gateway.accept().await;
    conn.hello(250).await;
    conn.expect_identify().await;
    conn.ready(1, "sess-12", [0, 1], &[]).await;
    conn.dispatch("TYPING_START", 2, &typing(2)).await;

    let beat = conn
        .wait_for("a heartbeat after seq 2", |frame| {
            frame.is_heartbeat() && frame.d() == Some(&json!(2))
        })
        .await;
    assert_eq!(beat, Frame::Json(json!({"op": 1, "d": 2})));

    // No more ACKs: the shard gives up on the connection and resumes.
    conn.set_auto_ack(false);
    conn.expect_gone().await;
    let (_conn, resume) = expect_resume(&fake).await;
    assert_eq!(resume, resume_payload("sess-12", 2));

    app.stop().await.expect("clean shutdown");
    s.broker.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_closes_resumably_saves_the_session_and_a_restart_resumes_it() {
    let Some(mut s) = setup(9).await else {
        return;
    };
    let fake = FakeDiscord::start().await;
    let app = App::start(&vars(&fake, 9));
    let mut conn = ready_shard(&fake, "sess-9", &[]).await;
    for seq in 2..=6 {
        conn.dispatch("TYPING_START", seq, &typing(seq)).await;
    }

    // Stop right away: what was received is still published and counted.
    let stopping = Instant::now();
    app.stop().await.expect("clean shutdown");
    assert!(
        stopping.elapsed() < Duration::from_secs(5),
        "shutdown took {:?}",
        stopping.elapsed()
    );
    assert_eq!(conn.expect_close().await, Some(4000), "resumable, not 1000");
    let mut ids = Vec::new();
    for _ in 2..=6 {
        ids.push(s.broker.take("TYPING_START").await.message_id.unwrap());
    }
    ids.sort();
    assert_eq!(
        ids,
        (2..=6)
            .map(|seq| format!("0:sess-9:{seq}"))
            .collect::<Vec<_>>()
    );

    let session = get(&mut s.redis, "gateway_session:0")
        .await
        .expect("session saved on shutdown");
    assert_eq!(
        session,
        json!({"session_id": "sess-9", "sequence": 6, "resume_url": fake.gateway.resume_url()})
    );
    let expires = ttl(&mut s.redis, "gateway_session:0").await;
    assert!((1..=600).contains(&expires), "TTL {expires}");
    // Older versions (a rollback) read the legacy blob.
    assert_eq!(
        get(&mut s.redis, "gateway_sessions").await,
        Some(json!({"0": session}))
    );

    // The next start resumes the session instead of identifying.
    let app = App::start(&vars(&fake, 9));
    let (conn, resume) = expect_resume(&fake).await;
    assert_eq!(resume, resume_payload("sess-9", 6));
    conn.resumed(7).await;
    let resumed = s.broker.take("RESUMED").await;
    assert_eq!(resumed.message_id.as_deref(), Some("0:sess-9:7"));

    app.stop().await.expect("clean shutdown");
    s.broker.close().await;
}

/// Start the service, answer HELLO and return the path of its connection and
/// its first IDENTIFY or RESUME.
async fn first_contact(fake: &FakeDiscord, vars: &Vars) -> (String, Value) {
    let app = App::start(vars);
    let mut conn = fake.gateway.accept().await;
    conn.hello(HEARTBEAT_MS).await;
    let frame = conn
        .wait_for("IDENTIFY or RESUME", |frame| {
            matches!(frame.op(), Some(2 | 6))
        })
        .await;
    app.stop().await.expect("clean shutdown");
    (conn.path.clone(), frame.json().unwrap().clone())
}

async fn seed(conn: &mut ConnectionManager, pairs: &[(&str, String)]) {
    let _: () = redis::cmd("FLUSHDB").query_async(conn).await.unwrap();
    for (key, value) in pairs {
        let _: () = redis::cmd("SET")
            .arg(key)
            .arg(value)
            .query_async(conn)
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn startup_resumes_only_usable_saved_sessions() {
    let Some(mut s) = setup(0).await else {
        return;
    };
    let fake = FakeDiscord::start().await;
    let saved =
        json!({"session_id": "saved", "sequence": 42, "resume_url": fake.gateway.resume_url()})
            .to_string();
    let legacy = json!({"0": {"session_id": "legacy", "sequence": 7}}).to_string();

    // RESUME=false always identifies.
    seed(
        &mut s.redis,
        &[
            ("gateway_shards", "1".into()),
            ("gateway_session:0", saved.clone()),
        ],
    )
    .await;
    let (path, first) = first_contact(&fake, &with(vars(&fake, 0), &[("RESUME", "false")])).await;
    assert_eq!((path.as_str(), &first["op"]), ("/", &json!(2)));

    // Sessions are only valid for the shard count they were created with.
    seed(
        &mut s.redis,
        &[
            ("gateway_shards", "2".into()),
            ("gateway_session:0", saved.clone()),
        ],
    )
    .await;
    let (path, first) = first_contact(&fake, &vars(&fake, 0)).await;
    assert_eq!((path.as_str(), &first["op"]), ("/", &json!(2)));

    // Only the legacy blob (an upgrade): resumed, at DISCORD_GATEWAY_URL since
    // it has no resume URL.
    seed(
        &mut s.redis,
        &[
            ("gateway_shards", "1".into()),
            ("gateway_sessions", legacy.clone()),
        ],
    )
    .await;
    let (path, first) = first_contact(&fake, &vars(&fake, 0)).await;
    assert_eq!(path, "/");
    assert_eq!(first["d"], resume_payload("legacy", 7));

    // The per-shard key wins over the legacy blob.
    seed(
        &mut s.redis,
        &[
            ("gateway_shards", "1".into()),
            ("gateway_session:0", saved),
            ("gateway_sessions", legacy),
        ],
    )
    .await;
    let (path, first) = first_contact(&fake, &vars(&fake, 0)).await;
    assert_eq!(path, "/resume/");
    assert_eq!(first["d"], resume_payload("saved", 42));

    s.broker.close().await;
}

// ---------------------------------------------------------------------------
// Health and Discord logs

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fatal_close_is_a_healthcheck_warning_not_a_restart() {
    let Some(mut s) = setup(10).await else {
        return;
    };
    let fake = FakeDiscord::start().await;
    let app = App::start(&vars(&fake, 10));
    let mut conn = ready_shard(&fake, "sess-10", &[]).await;

    conn.close(4014, "Disallowed intent(s).").await;
    assert_eq!(conn.expect_close().await, Some(4014));
    conn.expect_gone().await;

    let port = app.metrics_port;
    let body = eventually("/healthcheck reports the fatal close", async || {
        let (status, body) = http_get(port, "/healthcheck").await;
        assert_eq!(status, 200, "a restart would only burn identifies: {body}");
        body.contains("warnings").then_some(body)
    })
    .await;
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap(),
        json!({
            "status": "OK",
            "warnings": ["shard 0 was closed by Discord with a non-reconnectable code (4014)"],
        })
    );
    // The Docker HEALTHCHECK passes.
    assert!(metrics::probe(std::net::Ipv4Addr::LOCALHOST.into(), port).await);
    let (_, metrics) = http_get(port, "/metrics").await;
    assert!(
        metrics
            .lines()
            .any(|line| line == r#"gateway_shard_fatal{shard="0"} 1"#),
        "{metrics}"
    );
    eventually("gateway_statuses shows FatallyClosed", async || {
        let statuses = get(&mut s.redis, "gateway_statuses").await?;
        (statuses[0]["status"] == "FatallyClosed").then_some(())
    })
    .await;

    // Commands for the closed shard are dropped, not retried forever.
    let dropped = commands("0", "dropped");
    s.broker
        .command(br#"{"op":0,"shard":0,"data":{"op":3,"d":{"status":"online","afk":false,"since":null,"activities":[]}}}"#)
        .await;
    eventually("the command is dropped", async || {
        (commands("0", "dropped") > dropped).then_some(())
    })
    .await;
    // It does not reconnect (it would be closed with 4014 again).
    assert!(fake
        .gateway
        .try_accept(Duration::from_millis(2500))
        .await
        .is_none());

    app.stop().await.expect("clean shutdown");
    s.broker.close().await;
}

/// The titles of posted log embeds; every one is a "rich" embed.
fn titles(posts: &[Value]) -> Vec<String> {
    posts
        .iter()
        .map(|post| {
            let embeds = post["embeds"].as_array().expect("embeds");
            assert_eq!(embeds.len(), 1, "{post}");
            assert_eq!(embeds[0]["type"], "rich", "{post}");
            assert!(embeds[0]["timestamp"].is_string(), "{post}");
            embeds[0]["title"].as_str().unwrap_or_default().to_owned()
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn log_channel_posts_the_shard_lifecycle_and_stops_after_a_403() {
    let Some(s) = setup(11).await else {
        return;
    };
    let fake = FakeDiscord::start().await;
    let app = App::start(&with(
        vars(&fake, 11),
        &[("LOG_CHANNEL", "111"), ("LOG_GUILD_CHANNEL", "222")],
    ));
    let mut conn = ready_shard(&fake, "sess-11", &[]).await;

    let posts = fake.api.wait_posts(111, 1).await;
    assert_eq!(titles(&posts), ["[Shard 0] Ready"]);
    assert_eq!(posts[0]["embeds"][0]["color"], 0x00FF00);

    conn.reconnect().await;
    conn.expect_close().await;
    let (conn, _) = expect_resume(&fake).await;
    conn.resumed(2).await;
    let posts = fake.api.wait_posts(111, 4).await;
    assert_eq!(
        titles(&posts),
        [
            "[Shard 0] Ready",
            "[Shard 0] Disconnected",
            "[Shard 0] Resumed",
            "[Shard 0] Connected"
        ]
    );
    let colors: Vec<&Value> = posts.iter().map(|p| &p["embeds"][0]["color"]).collect();
    assert_eq!(
        colors,
        [
            &json!(0x00FF00),
            &json!(0xFF0000),
            &json!(0x1E90FF),
            &json!(0x00FF00)
        ]
    );
    let requests = fake.api.requests();
    let post = requests
        .iter()
        .find(|r| r.path == "/channels/111/messages")
        .unwrap();
    assert_eq!(post.authorization.as_deref(), Some("Bot e2e-token"));

    // Discord rejects the channel: one more attempt, then the channel is
    // disabled (repeated invalid requests count towards a Cloudflare ban).
    fake.api.set_channel_status(111, 403);
    conn.reconnect().await;
    let (conn, _) = expect_resume(&fake).await;
    conn.resumed(3).await;
    // A new guild: its join is posted to LOG_GUILD_CHANNEL through the same
    // queue, after the shard messages before it.
    conn.dispatch("GUILD_CREATE", 4, &guild(2000, "joined guild"))
        .await;
    let joins = fake.api.wait_posts(222, 1).await;
    assert_eq!(titles(&joins), ["Guild Join"]);
    assert_eq!(joins[0]["embeds"][0]["description"], "joined guild (2000)");

    let titles = titles(&fake.api.posts(111));
    assert_eq!(
        titles.len(),
        5,
        "nothing is posted after the 403: {titles:?}"
    );
    assert_eq!(titles[4], "[Shard 0] Disconnected");

    app.stop().await.expect("clean shutdown");
    s.broker.close().await;
}

// ---------------------------------------------------------------------------
// Startup failures and early shutdown. No SERIAL (no gateway, no consumer)
// and both stop before writing anything, so they share database 13.

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stopping_while_discord_does_not_answer_returns_promptly() {
    let Some(_redis) = common::redis(13).await else {
        return;
    };
    if common::amqp_url().is_none() {
        return;
    }
    let fake = FakeDiscord::start().await;
    fake.api.set_stall(true);
    let app = App::start(&vars(&fake, 13));
    fake.api
        .wait_until("the login request", |requests| {
            (!requests.is_empty()).then_some(())
        })
        .await;

    let stopping = Instant::now();
    app.stop()
        .await
        .expect("stopping during startup is not an error");
    assert!(
        stopping.elapsed() < Duration::from_secs(2),
        "{:?}",
        stopping.elapsed()
    );
    assert_eq!(fake.gateway.connections(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rejected_token_fails_startup() {
    let Some(_redis) = common::redis(13).await else {
        return;
    };
    if common::amqp_url().is_none() {
        return;
    }
    let fake = FakeDiscord::start().await;
    fake.api
        .set_response("GET", "/users/@me", 401, fake_discord::error_body(401));
    let mut app = App::start(&vars(&fake, 13));

    let err = app.finished().await.expect_err("startup fails");
    assert!(matches!(err, ApiError::TwilightHttp(_)), "{err}");
    assert!(err.to_string().contains("401"), "{err}");
    let calls: Vec<String> = fake
        .api
        .requests()
        .iter()
        .map(|r| format!("{} {}", r.method, r.path))
        .collect();
    assert_eq!(calls, ["GET /users/@me"]);
    assert_eq!(fake.gateway.connections(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_busy_metrics_port_fails_before_contacting_anything() {
    let fake = FakeDiscord::start().await;
    let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = busy.local_addr().unwrap().port();
    // Nothing listens on port 1: contacting Redis or the broker would fail
    // differently (and slowly).
    let vars = base_vars(
        &fake,
        "redis://127.0.0.1:1/0".into(),
        "amqp://127.0.0.1:1/%2f".into(),
        port,
    );
    let mut app = App::start(&vars);

    let err = app.finished().await.expect_err("the port is taken");
    assert!(matches!(err, ApiError::Io(_)), "{err}");
    assert!(fake.api.requests().is_empty());
    drop(busy);
}
