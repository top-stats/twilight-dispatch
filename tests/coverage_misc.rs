#![recursion_limit = "256"]

//! Behaviour outside the shard runner that the other tests do not reach:
//! - the worker's guild join / leave log, its circuit breaker and stale
//!   sessions (with a real Redis, and with fake connections that fail);
//! - the Discord log channels, against a fake Discord REST API;
//! - the status / session job, the metrics job and the /metrics server;
//! - `ApiError`, `FormattedDateTime`, `StatusInfo` and `Throttle`.
//!
//! Tests that need Redis or the broker are skipped unless TEST_REDIS_URL /
//! TEST_AMQP_URL are set.

mod common;

use bytes::Bytes;
use common::*;
use http_body_util::{BodyExt, Full};
use hyper::{body::Incoming, server::conn::http1, service::service_fn, Request, Response};
use hyper_util::rt::TokioIo;
use redis::aio::{ConnectionLike, ConnectionManager};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    convert::Infallible,
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio::{net::TcpListener, sync::watch, task::JoinHandle};
use twilight_dispatch::{
    amqp::Amqp,
    config::{ConfigError, StateConfig},
    constants::{JOIN_COLOR, LEAVE_COLOR},
    discord_log::DiscordLog,
    gateway::{
        registry::{Registry, ShardHandle, ShardSnapshot},
        runner::parse_event,
        sessions,
        worker::{self, Breaker, Job, Publisher, WorkerContext},
    },
    metrics::{self, Health},
    models::{ApiError, ApiResult, FormattedDateTime, StatusInfo},
    util::Throttle,
};
use twilight_http::Client;

/// Print warnings, so the log lines of the error paths are formatted too.
fn logs() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_max_level(tracing::Level::WARN)
        .try_init();
}

/// Tests that read or write process-wide state (Prometheus metrics, the
/// health state, the shared `gateway.recv` queue) run one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const GUILD_LOG: u64 = 22;
const SHARD_LOG: u64 = 11;

// ---------------------------------------------------------------------------
// Fake Discord REST API (DISCORD_API_PROXY points twilight-http at it)

#[derive(Clone, Debug)]
struct Posted {
    method: String,
    path: String,
    body: Value,
}

#[derive(Default)]
struct RestState {
    posted: Mutex<Vec<Posted>>,
    /// path -> (status, body); everything else answers 200 `{}`.
    responses: Mutex<HashMap<String, (u16, String)>>,
}

struct FakeRest {
    addr: SocketAddr,
    state: Arc<RestState>,
    /// While false, requests are recorded but not answered.
    open: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl FakeRest {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(RestState::default());
        let (open, rx) = watch::channel(true);
        let task = {
            let state = state.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    let (state, rx) = (state.clone(), rx.clone());
                    tokio::spawn(async move {
                        let service =
                            service_fn(move |req| respond(state.clone(), rx.clone(), req));
                        let _ = http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service)
                            .await;
                    });
                }
            })
        };
        Self {
            addr,
            state,
            open,
            task,
        }
    }

    /// A REST client like `app::http_client` builds with DISCORD_API_PROXY
    /// (without the ratelimiter, so hundreds of posts stay fast).
    fn client(&self) -> Arc<Client> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        Arc::new(
            Client::builder()
                .token("Bot test".to_owned())
                .proxy(self.addr.to_string(), true)
                .ratelimiter(None)
                .build(),
        )
    }

    fn respond_with(&self, channel: u64, status: u16, body: &str) {
        self.state
            .responses
            .lock()
            .unwrap()
            .insert(messages_path(channel), (status, body.to_owned()));
    }

    fn hold(&self) {
        self.open.send_replace(false);
    }

    fn release(&self) {
        self.open.send_replace(true);
    }

    fn posted(&self) -> Vec<Posted> {
        self.state.posted.lock().unwrap().clone()
    }

    /// Wait until at least `count` requests arrived.
    async fn wait_for(&self, count: usize) -> Vec<Posted> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let posted = self.posted();
            if posted.len() >= count {
                return posted;
            }
            assert!(
                Instant::now() < deadline,
                "expected {count} requests, got {posted:?}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Posts until (and without) a marker message sent last on the guild log.
    /// The log is one ordered queue, so once the marker arrived every earlier
    /// message was either posted or skipped.
    async fn until_marker(&self, log: &DiscordLog) -> Vec<Posted> {
        log.guild_join("marker", 1);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let posted = self.posted();
            if let Some(at) = posted.iter().position(|p| description(p) == "marker (1)") {
                return posted[..at].to_vec();
            }
            assert!(Instant::now() < deadline, "no marker in {posted:?}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

impl Drop for FakeRest {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn messages_path(channel: u64) -> String {
    format!("/api/v10/channels/{channel}/messages")
}

fn embed(posted: &Posted) -> &Value {
    &posted.body["embeds"][0]
}

fn description(posted: &Posted) -> &str {
    embed(posted)["description"].as_str().unwrap_or_default()
}

fn title(posted: &Posted) -> &str {
    embed(posted)["title"].as_str().unwrap_or_default()
}

async fn respond(
    state: Arc<RestState>,
    mut open: watch::Receiver<bool>,
    req: Request<Incoming>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let method = req.method().to_string();
    let path = req.uri().path().to_owned();
    let body = req
        .into_body()
        .collect()
        .await
        .map(|body| body.to_bytes())
        .unwrap_or_default();
    state.posted.lock().unwrap().push(Posted {
        method,
        path: path.clone(),
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
    });
    let _ = open.wait_for(|open| *open).await;
    let (status, body) = state
        .responses
        .lock()
        .unwrap()
        .get(&path)
        .cloned()
        .unwrap_or((200, "{}".to_owned()));
    Ok(Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body)))
        .unwrap())
}

// ---------------------------------------------------------------------------
// Fake Redis connections and publisher

#[derive(Clone, Copy, Debug)]
enum Failure {
    /// The socket broke (what a Redis restart looks like).
    BrokenPipe,
    Refused,
    /// Never answers.
    Hang,
}

/// A Redis connection that fails every command.
#[derive(Clone)]
struct FailingConn {
    failure: Failure,
    calls: Arc<AtomicUsize>,
}

impl FailingConn {
    fn new(failure: Failure) -> Self {
        Self {
            failure,
            calls: Arc::default(),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn fail<'a, T: Send + 'a>(&mut self) -> redis::RedisFuture<'a, T> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let failure = self.failure;
        Box::pin(async move {
            let kind = match failure {
                Failure::BrokenPipe => std::io::ErrorKind::BrokenPipe,
                Failure::Refused => std::io::ErrorKind::ConnectionRefused,
                Failure::Hang => std::future::pending().await,
            };
            Err(std::io::Error::new(kind, "fake failure").into())
        })
    }
}

impl ConnectionLike for FailingConn {
    fn req_packed_command<'a>(
        &'a mut self,
        _cmd: &'a redis::Cmd,
    ) -> redis::RedisFuture<'a, redis::Value> {
        self.fail()
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        _pipeline: &'a redis::Pipeline,
        _offset: usize,
        _count: usize,
    ) -> redis::RedisFuture<'a, Vec<redis::Value>> {
        self.fail()
    }

    fn get_db(&self) -> i64 {
        0
    }
}

#[derive(Default)]
struct Recorder {
    published: Mutex<Vec<(String, Value, Option<String>)>>,
}

impl Recorder {
    fn published(&self) -> Vec<(String, Value, Option<String>)> {
        self.published.lock().unwrap().clone()
    }
}

impl Publisher for Recorder {
    async fn publish(
        &self,
        routing_key: &str,
        payload: Vec<u8>,
        message_id: Option<String>,
    ) -> ApiResult<()> {
        let value = serde_json::from_slice(&payload)?;
        self.published
            .lock()
            .unwrap()
            .push((routing_key.to_owned(), value, message_id));
        Ok(())
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
        cache_timeout: Duration::from_secs(5),
        discord,
        breaker: Breaker::default(),
        cache_errors: Throttle::default(),
        publish_errors: Throttle::default(),
    }
}

/// A job as the shard runner builds it. `handle` provides the sequence token.
fn job(handle: Option<&ShardHandle>, kind: &str, seq: u64, d: Value) -> Job {
    let json = raw(kind, seq, d);
    Job {
        shard: 0,
        seq: Some(seq),
        token: handle.and_then(|h| h.dispatch(seq)),
        session: Some(Arc::from("s1")),
        event: parse_event(&json).unwrap(),
        json,
    }
}

fn shard_registry() -> (Arc<ShardHandle>, Arc<Registry>) {
    let handle = Arc::new(ShardHandle::new(0, None, Some(1)));
    let registry = Arc::new(Registry::new([handle.clone()]));
    (handle, registry)
}

fn joins() -> u64 {
    metrics::GUILD_EVENTS.with_label_values(&["Join"]).get()
}

fn leaves() -> u64 {
    metrics::GUILD_EVENTS.with_label_values(&["Leave"]).get()
}

fn update_errors(reason: &str) -> u64 {
    metrics::STATE_UPDATE_ERRORS
        .with_label_values(&[reason])
        .get()
}

fn recent_guild(id: u64, name: &str) -> Value {
    let mut guild = guild(id, name);
    guild["joined_at"] = json!(now_iso());
    guild
}

// ---------------------------------------------------------------------------
// Worker: guild join / leave log

#[tokio::test]
async fn guild_joins_and_leaves_are_logged_once() {
    let _serial = SERIAL.lock().await;
    let Some(mut conn) = redis(1).await else {
        return;
    };
    let rest = FakeRest::start().await;
    let log = DiscordLog::new(rest.client(), 0, GUILD_LOG);
    let (handle, registry) = shard_registry();
    let recorder = Arc::new(Recorder::default());
    let ctx = context(
        conn.clone(),
        recorder.clone(),
        registry,
        state_all(),
        log.clone(),
    );
    let (joins_before, leaves_before) = (joins(), leaves());

    let jobs = [
        // New guild: a join.
        job(Some(&handle), "GUILD_CREATE", 2, guild(1000, "g")),
        // Re-sent after a reconnect: not a join.
        job(Some(&handle), "GUILD_CREATE", 3, guild(1000, "g")),
        // An outage is not a leave, and recovering from it is not a join.
        job(
            Some(&handle),
            "GUILD_DELETE",
            4,
            json!({"id": "1000", "unavailable": true}),
        ),
        job(Some(&handle), "GUILD_CREATE", 5, guild(1000, "g")),
        // Unavailable at startup, available later: not a join either.
        job(
            Some(&handle),
            "GUILD_CREATE",
            6,
            json!({"id": "3000", "unavailable": true}),
        ),
        job(Some(&handle), "GUILD_CREATE", 7, guild(3000, "h")),
        // Removed: a leave, named from the cache.
        job(Some(&handle), "GUILD_DELETE", 8, json!({"id": "1000"})),
        // Removed but never cached: a leave without a name.
        job(Some(&handle), "GUILD_DELETE", 9, json!({"id": "2000"})),
    ];
    for job in jobs {
        worker::process(&ctx, &mut conn, job).await;
    }

    assert_eq!(joins() - joins_before, 1);
    assert_eq!(leaves() - leaves_before, 2);
    let posted = rest.until_marker(&log).await;
    let lines: Vec<(&str, &str)> = posted.iter().map(|p| (title(p), description(p))).collect();
    assert_eq!(
        lines,
        [
            ("Guild Join", "g (1000)"),
            ("Guild Leave", "g (1000)"),
            ("Guild Leave", "Unknown (2000)"),
        ]
    );
    assert!(posted
        .iter()
        .all(|p| p.method == "POST" && p.path == messages_path(GUILD_LOG)));
    assert_eq!(embed(&posted[0])["color"], JOIN_COLOR);
    assert_eq!(embed(&posted[1])["color"], LEAVE_COLOR);
    assert_eq!(recorder.published().len(), 8, "everything is published");
    assert_eq!(handle.in_flight(), 0);
}

#[tokio::test]
async fn without_the_cache_joins_are_recognised_by_joined_at() {
    let _serial = SERIAL.lock().await;
    let rest = FakeRest::start().await;
    let log = DiscordLog::new(rest.client(), 0, GUILD_LOG);
    let (handle, registry) = shard_registry();
    let mut conn = FailingConn::new(Failure::BrokenPipe);
    let disabled = StateConfig {
        enabled: false,
        ..state_all()
    };
    let ctx = context(
        conn.clone(),
        Arc::new(Recorder::default()),
        registry,
        disabled,
        log.clone(),
    );
    let (joins_before, leaves_before) = (joins(), leaves());

    let jobs = [
        job(Some(&handle), "GUILD_CREATE", 2, recent_guild(1000, "new")),
        // joined_at long ago: a guild the bot was already in.
        job(Some(&handle), "GUILD_CREATE", 3, guild(2000, "old")),
        job(
            Some(&handle),
            "GUILD_DELETE",
            4,
            json!({"id": "2000", "unavailable": true}),
        ),
        job(Some(&handle), "GUILD_DELETE", 5, json!({"id": "2000"})),
    ];
    for job in jobs {
        worker::process(&ctx, &mut conn, job).await;
    }

    assert_eq!(conn.calls(), 0, "STATE_ENABLED=false never touches Redis");
    assert_eq!(joins() - joins_before, 1);
    assert_eq!(leaves() - leaves_before, 1);
    let posted = rest.until_marker(&log).await;
    let lines: Vec<&str> = posted.iter().map(description).collect();
    assert_eq!(lines, ["new (1000)", "Unknown (2000)"]);
}

// ---------------------------------------------------------------------------
// Worker: cache failures and the breaker

#[tokio::test]
async fn failed_cache_updates_skip_the_guild_log_and_keep_the_breaker_closed() {
    logs();
    let _serial = SERIAL.lock().await;
    let Some(mut conn) = redis(2).await else {
        return;
    };
    let rest = FakeRest::start().await;
    let log = DiscordLog::new(rest.client(), 0, GUILD_LOG);
    let (handle, registry) = shard_registry();
    let recorder = Arc::new(Recorder::default());
    let ctx = context(
        conn.clone(),
        recorder.clone(),
        registry,
        state_all(),
        log.clone(),
    );
    // A key of the wrong type and a value that is not JSON: problems with one
    // event, not with Redis.
    let _: () = redis::cmd("SADD")
        .arg("guild:1000")
        .arg("x")
        .query_async(&mut conn)
        .await
        .unwrap();
    let _: () = redis::cmd("SET")
        .arg("guild:1001")
        .arg("not json")
        .query_async(&mut conn)
        .await
        .unwrap();
    let (joins_before, leaves_before, errors_before) = (joins(), leaves(), update_errors("error"));

    let mut update = guild(1001, "g");
    update.as_object_mut().unwrap().remove("channels");
    let jobs = [
        job(Some(&handle), "GUILD_CREATE", 2, guild(1000, "g")),
        job(Some(&handle), "GUILD_DELETE", 3, json!({"id": "1000"})),
        job(Some(&handle), "GUILD_UPDATE", 4, update),
    ];
    for job in jobs {
        worker::process(&ctx, &mut conn, job).await;
        assert!(
            ctx.breaker.allow(),
            "a server error must not open the breaker"
        );
    }

    assert_eq!(update_errors("error") - errors_before, 3);
    // Without a cache result, a GUILD_CREATE is not a join and a GUILD_DELETE
    // is not reported as a leave.
    assert_eq!(joins() - joins_before, 0);
    assert_eq!(leaves() - leaves_before, 0);
    assert!(rest.until_marker(&log).await.is_empty());
    // Still published, without `old`.
    let published = recorder.published();
    assert_eq!(published.len(), 3);
    assert!(published
        .iter()
        .all(|(_, body, _)| body.get("old").is_none()));
    assert_eq!(handle.in_flight(), 0);
}

#[tokio::test]
async fn connection_errors_and_timeouts_open_the_breaker() {
    logs();
    let _serial = SERIAL.lock().await;
    let (handle, registry) = shard_registry();

    for failure in [Failure::BrokenPipe, Failure::Refused] {
        let mut conn = FailingConn::new(failure);
        let recorder = Arc::new(Recorder::default());
        let ctx = context(
            conn.clone(),
            recorder.clone(),
            registry.clone(),
            state_all(),
            DiscordLog::disabled(),
        );
        let (errors, skipped) = (update_errors("error"), update_errors("skipped"));

        worker::process(
            &ctx,
            &mut conn,
            job(Some(&handle), "GUILD_CREATE", 2, guild(1000, "g")),
        )
        .await;
        assert!(!ctx.breaker.allow(), "{failure:?} opens the breaker");
        assert_eq!(update_errors("error") - errors, 1);
        let calls = conn.calls();

        // While it is open, updates are skipped without touching Redis.
        worker::process(
            &ctx,
            &mut conn,
            job(
                Some(&handle),
                "MESSAGE_CREATE",
                3,
                message(1, 1003, Some(1000), "x"),
            ),
        )
        .await;
        assert_eq!(conn.calls(), calls);
        assert_eq!(update_errors("skipped") - skipped, 1);
        assert_eq!(recorder.published().len(), 2, "both are published");
    }

    // A Redis that never answers: the update times out and opens the breaker.
    let mut conn = FailingConn::new(Failure::Hang);
    let mut ctx = context(
        conn.clone(),
        Arc::new(Recorder::default()),
        registry,
        state_all(),
        DiscordLog::disabled(),
    );
    ctx.cache_timeout = Duration::from_millis(50);
    let timeouts = update_errors("timeout");
    let started = Instant::now();
    worker::process(
        &ctx,
        &mut conn,
        job(Some(&handle), "GUILD_CREATE", 4, guild(1000, "g")),
    )
    .await;
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(!ctx.breaker.allow());
    assert_eq!(update_errors("timeout") - timeouts, 1);
    assert_eq!(handle.in_flight(), 0);
}

// ---------------------------------------------------------------------------
// Worker: jobs of a previous session

#[tokio::test]
async fn jobs_of_a_previous_session_only_apply_removals() {
    let _serial = SERIAL.lock().await;
    let Some(mut conn) = redis(3).await else {
        return;
    };
    let rest = FakeRest::start().await;
    let log = DiscordLog::new(rest.client(), 0, GUILD_LOG);
    let (handle, registry) = shard_registry();
    let recorder = Arc::new(Recorder::default());
    let ctx = context(
        conn.clone(),
        recorder.clone(),
        registry,
        state_all(),
        log.clone(),
    );

    let setup = [
        job(Some(&handle), "GUILD_CREATE", 2, guild(1000, "g")),
        job(Some(&handle), "GUILD_CREATE", 3, guild(2000, "two")),
        job(Some(&handle), "GUILD_CREATE", 4, guild(3000, "three")),
        job(
            Some(&handle),
            "MESSAGE_CREATE",
            5,
            message(1, 1003, Some(1000), "a"),
        ),
        job(
            Some(&handle),
            "MESSAGE_CREATE",
            6,
            message(2, 1003, Some(1000), "b"),
        ),
    ];
    for job in setup {
        worker::process(&ctx, &mut conn, job).await;
    }
    let (joins_before, leaves_before) = (joins(), leaves());

    // Queued in the old session, processed after the new READY.
    let stale = [
        job(
            Some(&handle),
            "MESSAGE_DELETE_BULK",
            7,
            json!({"ids": ["1"], "channel_id": "1003", "guild_id": "1000"}),
        ),
        job(
            Some(&handle),
            "MESSAGE_DELETE",
            8,
            json!({"id": "2", "channel_id": "1003", "guild_id": "1000"}),
        ),
        job(
            Some(&handle),
            "CHANNEL_DELETE",
            9,
            channel(1004, Some(1000), "random"),
        ),
        job(
            Some(&handle),
            "THREAD_DELETE",
            10,
            json!({"id": "1005", "guild_id": "1000", "parent_id": "1003", "type": 11}),
        ),
        job(
            Some(&handle),
            "GUILD_ROLE_DELETE",
            11,
            json!({"guild_id": "1000", "role_id": "1001"}),
        ),
        job(Some(&handle), "GUILD_DELETE", 12, json!({"id": "2000"})),
        // Not removals: they would bring back state the new session replays.
        job(
            Some(&handle),
            "GUILD_DELETE",
            13,
            json!({"id": "3000", "unavailable": true}),
        ),
        job(
            Some(&handle),
            "GUILD_ROLE_CREATE",
            14,
            json!({"guild_id": "1000", "role": role(1100, "new")}),
        ),
        job(
            Some(&handle),
            "MESSAGE_CREATE",
            15,
            message(3, 1003, Some(1000), "c"),
        ),
        job(Some(&handle), "GUILD_CREATE", 16, guild(4000, "four")),
    ];
    handle.new_session(1);
    for job in stale {
        worker::process(&ctx, &mut conn, job).await;
    }

    for gone in [
        "message:1003:1",
        "message:1003:2",
        "channel:1004",
        "thread:1000:1005",
        "role:1000:1001",
        "guild:2000",
    ] {
        assert!(!exists(&mut conn, gone).await, "{gone} was removed");
    }
    assert_eq!(
        get(&mut conn, "guild:3000").await.unwrap()["unavailable"],
        false,
        "a stale outage is not applied"
    );
    for absent in ["role:1000:1100", "message:1003:3", "guild:4000"] {
        assert!(!exists(&mut conn, absent).await, "{absent} was not written");
    }
    assert_eq!(joins() - joins_before, 0, "a stale GUILD_CREATE is no join");
    assert_eq!(leaves() - leaves_before, 1);
    let posted = rest.until_marker(&log).await;
    let lines: Vec<&str> = posted.iter().map(description).collect();
    assert_eq!(
        lines,
        ["g (1000)", "two (2000)", "three (3000)", "two (2000)"]
    );
    assert_eq!(recorder.published().len(), 15, "everything is published");
    assert_eq!(handle.in_flight(), 0);
}

// ---------------------------------------------------------------------------
// Worker: publishing

#[tokio::test]
async fn payloads_that_cannot_be_published_do_not_hold_the_resume_point() {
    logs();
    let handle = Arc::new(ShardHandle::new(0, None, Some(1)));
    handle.update(|s| s.session_id = Some("abc".into()));
    let registry = Arc::new(Registry::new([handle.clone()]));
    let recorder = Arc::new(Recorder::default());
    let mut conn = FailingConn::new(Failure::BrokenPipe);
    let ctx = context(
        conn.clone(),
        recorder.clone(),
        registry,
        state_all(),
        DiscordLog::disabled(),
    );

    let raw_job = |seq: Option<u64>, session: Option<&str>, json: &str| Job {
        shard: 0,
        seq,
        token: seq.and_then(|seq| handle.dispatch(seq)),
        session: session.map(Arc::from),
        json: json.to_owned(),
        event: None,
    };
    for job in [
        // Not JSON, and no `t`: logged and skipped.
        raw_job(Some(2), Some("s1"), "not json"),
        raw_job(Some(3), Some("s1"), r#"{"op":0,"s":3,"d":{}}"#),
        // No session id yet (e.g. replayed before READY): `<shard>::<seq>`.
        raw_job(Some(4), None, r#"{"op":0,"t":"A","s":4,"d":{"x":1}}"#),
        // No `d`: published with `"d": null`.
        raw_job(Some(5), Some("s1"), r#"{"op":0,"t":"B","s":5}"#),
        // No sequence: no message id, no token.
        raw_job(None, Some("s1"), r#"{"op":0,"t":"C","d":[]}"#),
    ] {
        worker::process(&ctx, &mut conn, job).await;
    }

    assert_eq!(
        recorder.published(),
        [
            (
                "A".to_owned(),
                json!({"op": 0, "t": "A", "d": {"x": 1}}),
                Some("0::4".to_owned())
            ),
            (
                "B".to_owned(),
                json!({"op": 0, "t": "B", "d": null}),
                Some("0:s1:5".to_owned())
            ),
            ("C".to_owned(), json!({"op": 0, "t": "C", "d": []}), None),
        ]
    );
    assert_eq!(conn.calls(), 0, "untyped payloads never touch the cache");
    assert_eq!(handle.in_flight(), 0);
    assert_eq!(
        handle.session_info().unwrap().sequence,
        5,
        "skipped payloads do not make Discord replay them"
    );
}

#[tokio::test]
async fn the_broker_publisher_sets_the_message_id() {
    use futures_util::StreamExt;
    use lapin::{
        options::{BasicAckOptions, BasicConsumeOptions, QueuePurgeOptions},
        types::FieldTable,
        Connection, ConnectionProperties,
    };

    let Some(url) = amqp_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let amqp = Amqp::connect(url.clone(), true).await.unwrap();
    let connection = Connection::connect(&url, ConnectionProperties::default())
        .await
        .unwrap();
    let channel = connection.create_channel().await.unwrap();
    channel
        .queue_purge("gateway.recv".into(), QueuePurgeOptions::default())
        .await
        .unwrap();

    let (handle, registry) = shard_registry();
    let mut conn = FailingConn::new(Failure::BrokenPipe);
    let ctx = context(
        conn.clone(),
        amqp.clone(),
        registry,
        StateConfig::default(),
        DiscordLog::disabled(),
    );
    worker::process(
        &ctx,
        &mut conn,
        job(
            Some(&handle),
            "MESSAGE_CREATE",
            5,
            message(1, 3, None, "hi"),
        ),
    )
    .await;
    let mut unnumbered = job(
        None,
        "TYPING_START",
        6,
        json!({"channel_id": "3", "user_id": "1", "timestamp": 1}),
    );
    unnumbered.seq = None;
    worker::process(&ctx, &mut conn, unnumbered).await;

    let mut consumer = channel
        .basic_consume(
            "gateway.recv".into(),
            "coverage".into(),
            BasicConsumeOptions::default(),
            FieldTable::default(),
        )
        .await
        .unwrap();
    let mut received = Vec::new();
    for _ in 0..2 {
        let delivery = tokio::time::timeout(Duration::from_secs(10), consumer.next())
            .await
            .expect("a message within 10s")
            .unwrap()
            .unwrap();
        delivery
            .acker
            .ack(BasicAckOptions::default())
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&delivery.data).unwrap();
        let id = delivery
            .properties
            .message_id()
            .as_ref()
            .map(|id| id.as_str().to_owned());
        received.push((delivery.routing_key.to_string(), body, id));
    }
    assert_eq!(received[0].0, "MESSAGE_CREATE");
    assert_eq!(received[0].1["op"], 0);
    assert_eq!(received[0].1["t"], "MESSAGE_CREATE");
    assert_eq!(received[0].1["d"]["content"], "hi");
    assert_eq!(received[0].2.as_deref(), Some("0:s1:5"));
    assert_eq!(received[1].0, "TYPING_START");
    assert_eq!(received[1].2, None);
    assert_eq!(handle.in_flight(), 0);

    let _ = connection.close(200, "done".into()).await;
    amqp.close().await;
}

// ---------------------------------------------------------------------------
// Discord log channels

#[tokio::test]
async fn the_discord_log_posts_rich_embeds_to_each_channel() {
    let rest = FakeRest::start().await;
    let log = DiscordLog::new(rest.client(), SHARD_LOG, GUILD_LOG);
    log.shard(0x1E90FF, "Shard 3 Resumed");
    log.guild_join("Guild", 1000);
    log.guild_leave("Gone", 2000);

    let posted = rest.wait_for(3).await;
    assert_eq!(posted[0].method, "POST");
    assert_eq!(posted[0].path, messages_path(SHARD_LOG));
    assert_eq!(posted[1].path, messages_path(GUILD_LOG));
    assert_eq!(posted[2].path, messages_path(GUILD_LOG));

    let shard = embed(&posted[0]);
    assert_eq!(shard["type"], "rich");
    assert_eq!(shard["title"], "Shard 3 Resumed");
    assert_eq!(shard["color"], 0x1E90FF);
    assert!(shard.get("description").is_none());
    let timestamp = shard["timestamp"].as_str().unwrap();
    assert!(
        timestamp.starts_with("20") && timestamp.contains('T'),
        "{timestamp}"
    );
    assert_eq!(posted[0].body["embeds"].as_array().unwrap().len(), 1);

    assert_eq!(title(&posted[1]), "Guild Join");
    assert_eq!(description(&posted[1]), "Guild (1000)");
    assert_eq!(embed(&posted[1])["color"], JOIN_COLOR);
    assert_eq!(title(&posted[2]), "Guild Leave");
    assert_eq!(description(&posted[2]), "Gone (2000)");
    assert_eq!(embed(&posted[2])["color"], LEAVE_COLOR);
}

#[tokio::test]
async fn unset_log_channels_get_nothing() {
    let rest = FakeRest::start().await;

    // Only the guild channel: shard messages are dropped.
    let guilds_only = DiscordLog::new(rest.client(), 0, GUILD_LOG);
    guilds_only.shard(0, "dropped");
    assert!(rest.until_marker(&guilds_only).await.is_empty());

    // Only the shard channel: guild messages are dropped.
    let shards_only = DiscordLog::new(rest.client(), SHARD_LOG, 0);
    shards_only.guild_join("dropped", 5);
    shards_only.shard(0, "kept");
    let posted = rest.wait_for(2).await;
    assert_eq!(posted.len(), 2);
    assert_eq!(title(&posted[1]), "kept");
    assert_eq!(posted[1].path, messages_path(SHARD_LOG));

    // Neither: nothing is ever sent.
    let none = DiscordLog::new(rest.client(), 0, 0);
    none.shard(0, "dropped");
    none.guild_join("dropped", 1);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(rest.posted().len(), 2);
}

#[tokio::test]
async fn permanent_errors_disable_only_that_channel() {
    logs();
    let rest = FakeRest::start().await;
    rest.respond_with(
        SHARD_LOG,
        403,
        r#"{"code": 50013, "message": "Missing Permissions"}"#,
    );
    let log = DiscordLog::new(rest.client(), SHARD_LOG, GUILD_LOG);
    log.shard(0, "rejected");
    log.shard(0, "not sent");
    log.guild_join("Guild", 1000);
    log.shard(0, "not sent either");

    let posted = rest.until_marker(&log).await;
    let lines: Vec<(&str, &str)> = posted.iter().map(|p| (p.path.as_str(), title(p))).collect();
    let (shards, guilds) = (messages_path(SHARD_LOG), messages_path(GUILD_LOG));
    assert_eq!(
        lines,
        [
            (shards.as_str(), "rejected"),
            (guilds.as_str(), "Guild Join")
        ]
    );

    // 404 (channel deleted) is permanent too.
    let rest = FakeRest::start().await;
    rest.respond_with(
        GUILD_LOG,
        404,
        r#"{"code": 10003, "message": "Unknown Channel"}"#,
    );
    let log = DiscordLog::new(rest.client(), SHARD_LOG, GUILD_LOG);
    log.guild_join("a", 1);
    rest.wait_for(1).await;
    log.guild_join("b", 2);
    log.shard(0, "shards still work");
    let posted = rest.wait_for(2).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let posted_now = rest.posted();
    assert_eq!(posted_now.len(), 2, "{posted_now:?}");
    assert_eq!(title(&posted[1]), "shards still work");
}

#[tokio::test]
async fn transient_errors_keep_the_channel() {
    logs();
    let rest = FakeRest::start().await;
    rest.respond_with(
        SHARD_LOG,
        500,
        r#"{"code": 0, "message": "Internal Server Error"}"#,
    );
    let log = DiscordLog::new(rest.client(), SHARD_LOG, GUILD_LOG);
    log.shard(0, "first");
    log.shard(0, "second");
    let posted = rest.until_marker(&log).await;
    let titles: Vec<&str> = posted.iter().map(title).collect();
    assert_eq!(titles, ["first", "second"]);
}

#[tokio::test]
async fn an_invalid_token_disables_every_channel() {
    let rest = FakeRest::start().await;
    rest.respond_with(
        SHARD_LOG,
        401,
        r#"{"code": 0, "message": "401: Unauthorized"}"#,
    );
    let log = DiscordLog::new(rest.client(), SHARD_LOG, GUILD_LOG);
    log.shard(0, "unauthorized");
    rest.wait_for(1).await;
    // twilight refuses every later request with this token, so the other
    // channel is disabled on its first message without another request.
    log.guild_join("a", 1);
    log.guild_join("b", 2);
    log.shard(0, "c");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(rest.posted().len(), 1, "{:?}", rest.posted());
}

#[tokio::test]
#[ignore = "BUG: a 401/403/404 whose body is not Discord's JSON error (e.g. a Cloudflare or proxy HTML page) does not disable the log channel"]
async fn permanent_errors_without_a_json_body_disable_the_channel() {
    let rest = FakeRest::start().await;
    rest.respond_with(SHARD_LOG, 403, "<html>Access denied</html>");
    let log = DiscordLog::new(rest.client(), SHARD_LOG, GUILD_LOG);
    log.shard(0, "rejected");
    log.shard(0, "not sent");

    let posted = rest.until_marker(&log).await;
    let titles: Vec<&str> = posted.iter().map(title).collect();
    assert_eq!(titles, ["rejected"], "a 403 disables the channel");
}

#[tokio::test]
async fn a_full_log_queue_drops_messages() {
    let rest = FakeRest::start().await;
    let log = DiscordLog::new(rest.client(), SHARD_LOG, GUILD_LOG);
    rest.hold();
    log.shard(0, "in flight");
    rest.wait_for(1).await;

    // One message is being posted; the queue holds 256 more.
    for i in 0..300 {
        log.shard(0, format!("m{i}"));
    }
    rest.release();
    let posted = rest.wait_for(257).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(rest.posted().len(), 257, "the rest was dropped");
    let titles: Vec<&str> = posted.iter().map(title).collect();
    assert_eq!(titles[0], "in flight");
    assert_eq!(titles[1], "m0");
    assert_eq!(
        titles[256], "m255",
        "the oldest messages are kept, in order"
    );

    // Once drained, new messages are posted again.
    log.shard(0, "later");
    let posted = rest.wait_for(258).await;
    assert_eq!(title(&posted[257]), "later");
}

// ---------------------------------------------------------------------------
// Registry and the status / session job

#[test]
fn status_info_reports_status_latency_and_last_ack() {
    let handle = ShardHandle::new(4, None, None);
    let now = FormattedDateTime::now();
    let info = handle.status_info(now);
    assert_eq!(info.shard, 4);
    assert_eq!(info.status, "Disconnected");
    assert_eq!(info.latency, 0);
    assert_eq!(info.last_ack, now, "no heartbeat ACK yet: now");

    handle.update(|s| {
        s.status = "Connected";
        s.latency_ms = 42;
        s.last_ack = Instant::now().checked_sub(Duration::from_secs(2));
    });
    let now = FormattedDateTime::now();
    let info = handle.status_info(now);
    assert_eq!(info.status, "Connected");
    assert_eq!(info.latency, 42);
    let ago = (now - info.last_ack).whole_milliseconds();
    assert!((2_000..2_500).contains(&ago), "{ago}");

    // The JSON stored in gateway_statuses keeps its shape.
    let json = serde_json::to_value(&info).unwrap();
    let mut fields: Vec<&str> = json
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    fields.sort_unstable();
    assert_eq!(fields, ["last_ack", "latency", "shard", "status"]);
    assert_eq!(json["shard"], 4);
    assert_eq!(json["latency"], 42);
    assert!(FormattedDateTime::parse(json["last_ack"].as_str().unwrap()).is_some());
    let back: StatusInfo = serde_json::from_value(json).unwrap();
    assert_eq!(back.last_ack, info.last_ack);
}

#[test]
fn the_registry_lists_shards_in_order() {
    let handles: Vec<Arc<ShardHandle>> = [3, 0, 1]
        .into_iter()
        .map(|id| Arc::new(ShardHandle::new(id, None, Some(1))))
        .collect();
    handles[0].update(|s| s.status = "Resuming");
    let registry = Registry::new(handles);
    assert_eq!(registry.len(), 3);
    assert!(!registry.is_empty());
    assert!(registry.get(2).is_none());
    assert_eq!(registry.get(3).unwrap().id, 3);

    let snapshots = registry.snapshots();
    let ids: Vec<u32> = snapshots.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, [0, 1, 3]);
    assert_eq!(snapshots[2].1.status, "Resuming");
    assert_eq!(snapshots[0].1.status, "Disconnected");

    let statuses = sessions::statuses(&registry);
    let shards: Vec<u32> = statuses.iter().map(|s| s.shard).collect();
    assert_eq!(shards, [0, 1, 3]);
    assert_eq!(statuses[2].status, "Resuming");

    assert!(Registry::default().is_empty());
    assert!(sessions::statuses(&Registry::default()).is_empty());

    // A fresh shard (nothing dispatched yet) has nothing to resume.
    let fresh = ShardHandle::new(9, None, None);
    fresh.update(|s| s.session_id = Some("abc".into()));
    assert_eq!(fresh.session_info(), None);
    let snapshot = ShardSnapshot::default();
    assert_eq!(snapshot.status, "Disconnected");
    assert_eq!(snapshot.silence(), None);
}

async fn wait_for_value(
    conn: &mut ConnectionManager,
    key: &str,
    check: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(value) = get(conn, key).await {
            if check(&value) {
                return value;
            }
        }
        assert!(Instant::now() < deadline, "{key} never matched");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn the_status_job_writes_statuses_and_sessions_every_second() {
    let Some(mut conn) = redis(4).await else {
        return;
    };
    let connected = Arc::new(ShardHandle::new(0, None, Some(42)));
    connected.update(|s| {
        s.status = "Connected";
        s.latency_ms = 12;
        s.session_id = Some("abc".into());
        s.resume_url = Some("wss://resume.discord.gg".into());
    });
    let resuming = Arc::new(ShardHandle::new(1, None, None));
    resuming.update(|s| s.status = "Resuming");
    let registry = Arc::new(Registry::new([resuming.clone(), connected]));
    let job = tokio::spawn(sessions::run_job(conn.clone(), registry));

    let statuses = wait_for_value(&mut conn, "gateway_statuses", |_| true).await;
    let statuses = statuses.as_array().unwrap();
    assert_eq!(statuses.len(), 2);
    assert_eq!(statuses[0]["shard"], 0);
    assert_eq!(statuses[0]["status"], "Connected");
    assert_eq!(statuses[0]["latency"], 12);
    assert!(statuses[0]["last_ack"].is_string());
    assert_eq!(statuses[1]["shard"], 1);
    assert_eq!(statuses[1]["status"], "Resuming");

    let session = wait_for_value(&mut conn, "gateway_session:0", |_| true).await;
    assert_eq!(
        session,
        json!({"session_id": "abc", "sequence": 42, "resume_url": "wss://resume.discord.gg"})
    );
    assert!(!exists(&mut conn, "gateway_session:1").await);

    // The next run picks up changes.
    resuming.update(|s| s.status = "Connected");
    wait_for_value(&mut conn, "gateway_statuses", |v| {
        v[1]["status"] == "Connected"
    })
    .await;
    job.abort();
}

#[tokio::test]
async fn the_status_job_keeps_running_while_redis_fails() {
    logs();
    let conn = FailingConn::new(Failure::BrokenPipe);
    let handle = Arc::new(ShardHandle::new(0, None, Some(1)));
    let job = tokio::spawn(sessions::run_job(
        conn.clone(),
        Arc::new(Registry::new([handle])),
    ));
    // Each run sends the statuses and the sessions; a third call means the
    // job went on after both failed.
    let deadline = Instant::now() + Duration::from_secs(5);
    while conn.calls() < 3 {
        assert!(Instant::now() < deadline, "{} calls", conn.calls());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!job.is_finished());
    job.abort();
}

// ---------------------------------------------------------------------------
// Metrics job and server

/// GET a path from the metrics server: (status, headers, body).
async fn http_get(port: u16, method: &str, path: &str) -> (u16, String, String) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    let request = format!("{method} {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap_or((&response, ""));
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    (status, head.to_lowercase(), body.to_owned())
}

async fn metrics_server() -> (u16, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    (port, tokio::spawn(metrics::run_server(listener)))
}

async fn wait_until(what: &str, check: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn snapshot_handle(id: u32, f: impl FnOnce(&mut ShardSnapshot)) -> Arc<ShardHandle> {
    let handle = Arc::new(ShardHandle::new(id, None, Some(1)));
    handle.update(f);
    handle
}

#[tokio::test]
async fn the_metrics_job_reports_shards_cache_and_health() {
    let Some(url) = amqp_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let Some(mut conn) = redis(5).await else {
        return;
    };
    let state = state_all();
    apply(&mut conn, &state, "GUILD_CREATE", guild(1000, "g")).await;
    apply(
        &mut conn,
        &state,
        "MESSAGE_CREATE",
        message(1, 1003, Some(1000), "x"),
    )
    .await;

    let active = snapshot_handle(0, |s| {
        s.status = "Connected";
        s.active = true;
        s.latency_ms = 42;
        s.last_message = Some(Instant::now());
    });
    let fatal = snapshot_handle(1, |s| {
        s.status = "FatallyClosed";
        s.fatal = true;
        s.close_code = Some(4014);
    });
    let disconnected = snapshot_handle(2, |_| {});
    let registry = Arc::new(Registry::new([active.clone(), fatal, disconnected]));
    let amqp = Amqp::connect(url, false).await.unwrap();
    let (port, server) = metrics_server().await;

    for gauge in [
        &*metrics::STATE_GUILDS,
        &*metrics::STATE_CHANNELS,
        &*metrics::STATE_MESSAGES,
        &*metrics::STATE_ROLES,
        &*metrics::STATE_EMOJIS,
        &*metrics::STATE_MEMBERS,
        &*metrics::STATE_PRESENCES,
        &*metrics::STATE_VOICES,
    ] {
        gauge.set(-1);
    }
    let job = tokio::spawn(metrics::run_jobs(
        conn.clone(),
        registry,
        amqp.clone(),
        Duration::from_secs(180),
    ));
    wait_until("state counts", || metrics::STATE_GUILDS.get() == 1).await;

    // Cache sizes come from the type index sets (threads are not channels).
    let counts: Vec<i64> = [
        &*metrics::STATE_GUILDS,
        &*metrics::STATE_CHANNELS,
        &*metrics::STATE_MESSAGES,
        &*metrics::STATE_ROLES,
        &*metrics::STATE_EMOJIS,
        &*metrics::STATE_MEMBERS,
        &*metrics::STATE_PRESENCES,
        &*metrics::STATE_VOICES,
    ]
    .iter()
    .map(|gauge| gauge.get())
    .collect();
    assert_eq!(counts, [1, 2, 1, 2, 1, 2, 1, 1]);

    assert_eq!(metrics::GATEWAY_SHARDS.get(), 3);
    let status = |label: &str| metrics::GATEWAY_STATUSES.with_label_values(&[label]).get();
    assert_eq!(status("Connected"), 1);
    assert_eq!(status("FatallyClosed"), 1);
    assert_eq!(status("Disconnected"), 1);
    for label in ["Handshaking", "Identifying", "Resuming"] {
        assert_eq!(status(label), 0, "{label} is reported as 0");
    }
    assert_eq!(
        metrics::GATEWAY_LATENCIES.with_label_values(&["0"]).get(),
        42
    );
    assert_eq!(metrics::SHARD_FATAL.with_label_values(&["0"]).get(), 0);
    assert_eq!(metrics::SHARD_FATAL.with_label_values(&["1"]).get(), 1);
    assert_eq!(metrics::SHARD_SILENCE.with_label_values(&["0"]).get(), 0);
    assert_eq!(metrics::AMQP_CONNECTED.get(), 1);

    // A fatal close is a warning: the healthcheck still passes.
    let (code, head, body) = http_get(port, "GET", "/healthcheck").await;
    assert_eq!(code, 200, "{body}");
    assert!(head.contains("content-type: application/json"), "{head}");
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["status"], "OK");
    assert_eq!(
        body["warnings"],
        json!(["shard 1 was closed by Discord with a non-reconnectable code (4014)"])
    );
    let (code, head, body) = http_get(port, "GET", "/metrics").await;
    assert_eq!(code, 200);
    assert!(
        head.contains("content-type: text/plain; version=0.0.4"),
        "{head}"
    );
    for line in [
        "state_guilds 1",
        "gateway_shards 3",
        "gateway_statuses{type=\"Connected\"} 1",
        "gateway_latencies{shard=\"0\"} 42",
        "gateway_shard_fatal{shard=\"1\"} 1",
        "amqp_connected 1",
    ] {
        assert!(body.lines().any(|l| l == line), "{line} in\n{body}");
    }

    // A connected shard that went silent fails the healthcheck. The silence
    // gauge is set in the same step as the health state.
    active.update(|s| s.last_message = Instant::now().checked_sub(Duration::from_secs(600)));
    let silence = || metrics::SHARD_SILENCE.with_label_values(&["0"]).get();
    wait_until("the next run", || silence() >= 600).await;
    assert!(silence() <= 602, "{}", silence());
    let (code, _, body) = http_get(port, "GET", "/healthcheck").await;
    assert_eq!(code, 503, "{body}");
    let body: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(body["status"], "UNHEALTHY");
    let problems = body["problems"].as_array().unwrap();
    assert_eq!(problems.len(), 1, "{problems:?}");
    let problem = problems[0].as_str().unwrap();
    assert!(
        problem.starts_with("shard 0 is connected but received nothing for 60")
            && problem.ends_with('s'),
        "{problem}"
    );

    // A broker outage is a warning.
    active.update(|s| s.last_message = Some(Instant::now()));
    amqp.close().await;
    wait_until("the next run", || metrics::AMQP_CONNECTED.get() == 0).await;
    let (code, _, body) = http_get(port, "GET", "/healthcheck").await;
    assert_eq!(code, 200, "{body}");
    let body: Value = serde_json::from_str(&body).unwrap();
    assert!(body["warnings"]
        .as_array()
        .unwrap()
        .contains(&json!("not connected to the AMQP broker")));

    job.abort();
    server.abort();
    metrics::set_health(Health::default());
}

#[tokio::test]
async fn the_metrics_job_keeps_reporting_health_while_redis_fails() {
    logs();
    let Some(url) = amqp_url() else {
        return;
    };
    let _serial = SERIAL.lock().await;
    let conn = FailingConn::new(Failure::BrokenPipe);
    let stuck = snapshot_handle(7, |s| {
        s.status = "Connected";
        s.active = true;
        s.last_message = Instant::now().checked_sub(Duration::from_secs(600));
    });
    let amqp = Amqp::connect(url, false).await.unwrap();
    metrics::STATE_GUILDS.set(-7);
    metrics::GATEWAY_SHARDS.set(-1);
    let job = tokio::spawn(metrics::run_jobs(
        conn.clone(),
        Arc::new(Registry::new([stuck])),
        amqp.clone(),
        Duration::from_secs(180),
    ));
    wait_until("a run", || metrics::GATEWAY_SHARDS.get() == 1).await;
    let (port, server) = metrics_server().await;
    let (code, _, body) = http_get(port, "GET", "/healthcheck").await;
    assert_eq!(code, 503, "{body}");
    assert!(body.contains("shard 7 is connected but received nothing"));

    // The job goes on; the last cache counts are kept.
    wait_until("a second run", || conn.calls() >= 2).await;
    assert!(!job.is_finished());
    assert_eq!(metrics::STATE_GUILDS.get(), -7);

    job.abort();
    server.abort();
    amqp.close().await;
    metrics::set_health(Health::default());
}

#[tokio::test]
async fn the_metrics_server_routes_and_survives_bad_requests() {
    logs();
    let _serial = SERIAL.lock().await;
    metrics::set_health(Health::default());
    let (port, server) = metrics_server().await;

    let (code, head, body) = http_get(port, "GET", "/healthcheck").await;
    assert_eq!((code, body.as_str()), (200, r#"{"status":"OK"}"#));
    assert!(head.contains("content-type: application/json"));

    for (method, path) in [
        ("GET", "/"),
        ("GET", "/metrics/x"),
        ("GET", "/health"),
        ("POST", "/metrics"),
        ("DELETE", "/healthcheck"),
    ] {
        let (code, _, body) = http_get(port, method, path).await;
        assert_eq!((code, body.as_str()), (404, ""), "{method} {path}");
    }

    // Garbage on one connection does not take the server down.
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        stream
            .write_all(b"\x16\x03\x01garbage\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        let _ =
            tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut response)).await;
    }
    // And neither does a client that leaves without sending anything.
    drop(
        tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap(),
    );
    let (code, _, _) = http_get(port, "GET", "/metrics").await;
    assert_eq!(code, 200);
    assert!(metrics::probe(std::net::IpAddr::from([127, 0, 0, 1]), port).await);

    server.abort();
}

#[tokio::test]
async fn the_probe_fails_without_a_healthy_answer() {
    let local = std::net::IpAddr::from([127, 0, 0, 1]);

    // Nothing listening.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    assert!(!metrics::probe(local, port).await);

    // Something that closes the connection without answering.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let closer = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            drop(stream);
        }
    });
    assert!(!metrics::probe(local, port).await);

    // An HTTP server that answers something other than 200.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let teapot = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut buffer = [0; 1024];
            let _ = stream.read(&mut buffer).await;
            let _ = stream
                .write_all(b"HTTP/1.1 418 I'm a teapot\r\nContent-Length: 0\r\n\r\n")
                .await;
        }
    });
    assert!(!metrics::probe(local, port).await);

    closer.abort();
    teapot.abort();
}

// ---------------------------------------------------------------------------
// Models and utilities

#[test]
fn api_errors_name_their_source() {
    let other: ApiError = "the AMQP broker is unavailable".into();
    assert!(matches!(other, ApiError::Other(_)));
    assert_eq!(other.to_string(), "Other: the AMQP broker is unavailable");

    let config: ApiError = ConfigError::Missing("BOT_TOKEN").into();
    assert_eq!(
        config.to_string(),
        "Config: missing environment variable BOT_TOKEN"
    );

    let json: ApiError = serde_json::from_str::<Value>("{").unwrap_err().into();
    assert!(matches!(json, ApiError::Json(_)));
    assert!(json.to_string().starts_with("Json: "), "{json}");

    let io: ApiError = std::io::Error::other("boom").into();
    assert_eq!(io.to_string(), "Io: boom");

    let redis: ApiError =
        redis::RedisError::from(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pipe")).into();
    assert!(matches!(redis, ApiError::Redis(_)));
    assert!(redis.to_string().starts_with("Redis: "), "{redis}");

    let boxed: Box<dyn std::error::Error> = Box::new(ApiError::Other("x".into()));
    assert_eq!(boxed.to_string(), "Other: x");
}

#[test]
fn formatted_timestamps_do_arithmetic() {
    use time::{macros::datetime, Duration as TimeDuration};
    let start = FormattedDateTime::from_offset(datetime!(2026-10-20 12:00:00 UTC));
    let later = start + TimeDuration::milliseconds(1_500);
    assert_eq!(later.format(), "2026-10-20T12:00:01.5");
    assert_eq!(later - start, TimeDuration::milliseconds(1_500));
    assert_eq!(later - TimeDuration::milliseconds(1_500), start);
    assert!(start < later);
    assert_eq!(later.unix_millis() - start.unix_millis(), 1_500);
    assert_eq!(
        FormattedDateTime::parse("2026-10-20T12:00:01.5"),
        Some(later)
    );
    assert_eq!(FormattedDateTime::parse("2026-10-20 12:00:01"), None);
    assert_eq!(
        FormattedDateTime::parse("2026-10-20T12:00:01.5+00:00"),
        None
    );
}

#[tokio::test]
async fn throttle_allows_one_message_per_interval() {
    let throttle = Throttle::new(Duration::from_millis(300));
    assert_eq!(throttle.check(), Some(0));
    assert_eq!(throttle.check(), None);
    assert_eq!(throttle.check(), None);
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert_eq!(throttle.check(), Some(2), "reports what it suppressed");
    assert_eq!(throttle.check(), None);

    let default = Throttle::default();
    assert_eq!(default.check(), Some(0));
    assert_eq!(
        default.check(),
        None,
        "the default interval is seconds long"
    );
}

// ---------------------------------------------------------------------------
// Publish failures and empty registries

/// A broker that rejects everything.
struct Rejecting;

impl Publisher for Rejecting {
    async fn publish(
        &self,
        _routing_key: &str,
        _payload: Vec<u8>,
        _message_id: Option<String>,
    ) -> ApiResult<()> {
        Err("the AMQP broker is unavailable".into())
    }
}

#[tokio::test]
async fn repeated_publish_failures_resume_before_the_first_one() {
    logs();
    let handle = Arc::new(ShardHandle::new(0, None, Some(1)));
    handle.update(|s| s.session_id = Some("abc".into()));
    let registry = Arc::new(Registry::new([handle.clone()]));
    let mut conn = FailingConn::new(Failure::BrokenPipe);
    let ctx = context(
        conn.clone(),
        Arc::new(Rejecting),
        registry,
        StateConfig::default(),
        DiscordLog::disabled(),
    );
    // The second and third failures are within the warning throttle.
    for seq in 2..=4 {
        worker::process(
            &ctx,
            &mut conn,
            job(
                Some(&handle),
                "TYPING_START",
                seq,
                json!({"channel_id": "3", "user_id": "1", "timestamp": 1}),
            ),
        )
        .await;
    }
    assert_eq!(handle.in_flight(), 0);
    assert_eq!(
        handle.session_info().unwrap().sequence,
        1,
        "a restart replays every lost payload"
    );
}

#[tokio::test]
async fn sessions_of_no_shards_need_no_redis() {
    let mut conn = FailingConn::new(Failure::BrokenPipe);
    assert!(sessions::load(&mut conn, std::iter::empty())
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        sessions::save(&mut conn, &Registry::default())
            .await
            .unwrap(),
        0
    );
    assert_eq!(conn.calls(), 0);

    // With shards, a broken Redis is an error (startup fails instead of
    // identifying every shard from scratch).
    assert!(sessions::load(&mut conn, [0]).await.is_err());
    let registry = Registry::new([Arc::new(ShardHandle::new(0, None, Some(1)))]);
    assert!(sessions::save(&mut conn, &registry).await.is_err());
}
