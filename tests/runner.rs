#![recursion_limit = "256"]

//! The shard runner (`gateway::runner::run`) against the fake Discord gateway
//! of `tests/support/fake_discord.rs`, with the real ordered workers, a
//! recording publisher whose gate can be shut (so the worker queues fill up)
//! and a real Redis (set TEST_REDIS_URL).
//!
//! Pinned here:
//! - backpressure: a full worker queue closes the shard resumably (4000), the
//!   runner waits for room and the shard RESUMEs once from the last sequence
//!   it received; every dispatch is published exactly once, in order;
//! - the registry snapshot (`refresh()`): session, resume URL, status, close
//!   code, fatal flag and the resume point after READY;
//! - shutdown: a resumable close that still dispatches what arrives while
//!   closing, all within one close timeout (the gateway's answer, held events
//!   and READY's cache update), no new connection when the shard is already
//!   disconnected and no RESUME when the connection drops while closing;
//! - fatal close codes end `run()`; gateway commands reach the gateway; shard
//!   lifecycle metrics and Discord log messages.
//!
//! twilight waits 1 s before every connection (2 s after a failed one), so
//! most tests take one to four seconds. Tests asserting deltas of the
//! process-wide `gateway_shard_events` counter hold `METRICS` exclusively,
//! every other test holds it shared. Other counters are read per shard (each
//! test has its own shard id) or per event type only one test sends. With the
//! cache disabled the runner and the workers never touch Redis, so those tests
//! share `IDLE_DB` (and `the_cache_disabled_never_writes_to_redis` checks it).

mod common;
#[path = "support/fake_discord.rs"]
mod fake_discord;

use fake_discord::{ready_payload, FakeDiscord, Frame, GatewayConnection};
use futures_util::future::join_all;
use redis::aio::{ConnectionLike, ConnectionManager};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashSet},
    net::SocketAddr,
    ops::RangeInclusive,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{watch, RwLock},
    task::JoinHandle,
};
use twilight_dispatch::{
    config::StateConfig,
    constants::{CONNECT_COLOR, DISCONNECT_COLOR, READY_COLOR, RESUME_COLOR},
    discord_log::DiscordLog,
    gateway::{
        registry::{Registry, ShardHandle, ShardSnapshot},
        runner::{self, RunnerContext, CLOSE_TIMEOUT, FATAL_CLOSE_CODES},
        worker::{self, Breaker, Publisher, WorkerContext, QUEUE},
    },
    metrics::{GATEWAY_EVENTS, GATEWAY_PARSE_ERRORS, SHARD_EVENTS},
    models::{ApiResult, SessionInfo},
    util::Throttle,
};
use twilight_gateway::{
    queue::InMemoryQueue, CloseFrame, ConfigBuilder, Intents, Session, Shard, ShardId,
};
use twilight_http::Client;

/// Every test runs its own shard id of this many.
const TOTAL: u32 = 64;
const TOKEN: &str = "runner-token";
/// Tests with the cache disabled never write to Redis, so they share one.
const IDLE_DB: u8 = 0;
const PARSE_DB: u8 = 1;
const STALE_DB: u8 = 2;
const READY_DB: u8 = 3;
/// A day: no heartbeat is due during a test, so the gateway only sees the
/// frames a test expects.
const QUIET: u64 = 86_400_000;
/// One worker queue holds `QUEUE` jobs. With the only worker held at the gate
/// on READY, sequences 2..=QUEUE + 1 fill it and this one does not fit.
const FULL: u64 = QUEUE as u64 + 2;
const LOG_CHANNEL: u64 = 4242;
const WAIT: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(5);

static METRICS: RwLock<()> = RwLock::const_new(());

fn init() {
    // main() installs it; twilight's TLS connector panics without one.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

// ---------------------------------------------------------------------------
// Publisher

/// One published message.
#[derive(Clone, Debug)]
struct Published {
    key: String,
    /// The payload exactly as published.
    raw: String,
    body: Value,
    id: Option<String>,
}

impl Published {
    /// The gateway sequence, from the message id (`shard:session:seq`).
    fn seq(&self) -> u64 {
        seq_of(self.id.as_deref()).unwrap_or_else(|| panic!("no sequence in {:?}", self.id))
    }
}

fn seq_of(id: Option<&str>) -> Option<u64> {
    id?.rsplit(':').next()?.parse().ok()
}

type Delay = Box<dyn Fn(u64) -> Duration + Send + Sync>;

/// Records what the workers publish. While the gate is shut every publish
/// waits, so the worker stops taking jobs and its queue fills up.
struct Recorder {
    gate: watch::Sender<bool>,
    /// Extra time per publish, by sequence.
    delay: Option<Delay>,
    /// Publishes started, including the ones waiting at the gate.
    entered: AtomicUsize,
    published: Mutex<Vec<Published>>,
}

impl Recorder {
    fn new(open: bool, delay: Option<Delay>) -> Arc<Self> {
        Arc::new(Self {
            gate: watch::channel(open).0,
            delay,
            entered: AtomicUsize::new(0),
            published: Mutex::new(Vec::new()),
        })
    }

    fn open(&self) {
        self.gate.send_replace(true);
    }

    fn entered(&self) -> usize {
        self.entered.load(Ordering::SeqCst)
    }

    fn published(&self) -> Vec<Published> {
        self.published.lock().unwrap().clone()
    }

    fn count(&self) -> usize {
        self.published.lock().unwrap().len()
    }

    async fn wait_published(&self, count: usize) -> Vec<Published> {
        eventually(&format!("{count} published events"), || {
            let published = self.published();
            (published.len() >= count).then_some(published)
        })
        .await
    }

    async fn wait_entered(&self, count: usize) {
        eventually(&format!("{count} publishes to start"), || {
            (self.entered() >= count).then_some(())
        })
        .await;
    }
}

impl Publisher for Recorder {
    async fn publish(
        &self,
        routing_key: &str,
        payload: Vec<u8>,
        message_id: Option<String>,
    ) -> ApiResult<()> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        let mut gate = self.gate.subscribe();
        let _ = gate.wait_for(|open| *open).await;
        if let Some(delay) = &self.delay {
            let delay = delay(seq_of(message_id.as_deref()).unwrap_or(0));
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
        }
        let raw = String::from_utf8(payload).expect("payloads are UTF-8");
        let body = serde_json::from_str(&raw)?;
        self.published.lock().unwrap().push(Published {
            key: routing_key.to_owned(),
            raw,
            body,
            id: message_id,
        });
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Runner harness

struct Opts {
    state: StateConfig,
    workers: usize,
    /// Whether the publisher's gate starts open.
    open: bool,
    delay: Option<Delay>,
    /// A saved session to resume, as `app::start` restores it.
    restore: Option<SessionInfo>,
    discord: DiscordLog,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            state: StateConfig::default(),
            workers: 1,
            open: true,
            delay: None,
            restore: None,
            discord: DiscordLog::disabled(),
        }
    }
}

struct Running {
    id: u32,
    handle: Arc<ShardHandle>,
    recorder: Arc<Recorder>,
    shutdown: watch::Sender<bool>,
    runner: Option<JoinHandle<()>>,
    workers: Vec<JoinHandle<()>>,
}

fn new_shard(url: &str, id: u32, restore: Option<&SessionInfo>) -> Shard<InMemoryQueue> {
    // A max_concurrency of 0 grants every IDENTIFY at once.
    let queue = InMemoryQueue::new(0, 1000, Duration::from_secs(86_400), 1000);
    let mut builder = ConfigBuilder::new(TOKEN.to_owned(), Intents::GUILDS)
        .proxy_url(url.to_owned())
        .queue(queue);
    if let Some(session) = restore {
        builder = builder.session(Session::new(session.sequence, session.session_id.clone()));
        if let Some(resume_url) = &session.resume_url {
            builder = builder.resume_url(resume_url.clone());
        }
    }
    Shard::with_config(ShardId::new(id, TOTAL), builder.build())
}

/// Run shard `id` against the gateway at `url` the way `app::start` does.
fn start(url: &str, id: u32, redis: ConnectionManager, opts: Opts) -> Running {
    start_with(url, id, redis.clone(), redis, opts)
}

/// `start` with another Redis for the runner (READY's cache update) than
/// for the workers.
fn start_with<C>(
    url: &str,
    id: u32,
    redis: ConnectionManager,
    runner_redis: C,
    opts: Opts,
) -> Running
where
    C: ConnectionLike + Clone + Send + Sync + 'static,
{
    init();
    let shard = new_shard(url, id, opts.restore.as_ref());
    let resumed = opts.restore.as_ref().map(|session| session.sequence);
    let handle = Arc::new(ShardHandle::new(id, Some(shard.sender()), resumed));
    if let Some(session) = &opts.restore {
        handle.update(|s| {
            s.session_id = Some(session.session_id.clone());
            s.resume_url = session.resume_url.clone();
        });
    }
    let registry = Arc::new(Registry::new([handle.clone()]));
    let recorder = Recorder::new(opts.open, opts.delay);
    let worker_ctx = Arc::new(WorkerContext {
        redis,
        publisher: recorder.clone(),
        registry,
        state: opts.state.clone(),
        bot_id: common::bot_id(),
        cache_timeout: Duration::from_secs(5),
        discord: opts.discord.clone(),
        breaker: Breaker::default(),
        cache_errors: Throttle::default(),
        publish_errors: Throttle::default(),
    });
    let (queues, workers) = worker::spawn(opts.workers, worker_ctx);
    // Only the runner holds the queues: once it returns the workers drain and stop.
    let runner_ctx = Arc::new(RunnerContext {
        state: opts.state,
        workers: queues,
        redis: runner_redis,
        discord: opts.discord,
        bot_id: common::bot_id(),
        cache_timeout: Duration::from_secs(5),
    });
    let (shutdown, shutdown_rx) = watch::channel(false);
    let runner = tokio::spawn(runner::run(shard, handle.clone(), runner_ctx, shutdown_rx));
    Running {
        id,
        handle,
        recorder,
        shutdown,
        runner: Some(runner),
        workers,
    }
}

impl Running {
    fn snapshot(&self) -> ShardSnapshot {
        self.handle.snapshot()
    }

    async fn wait_snapshot(
        &self,
        what: &str,
        check: impl Fn(&ShardSnapshot) -> bool,
    ) -> ShardSnapshot {
        let deadline = Instant::now() + WAIT;
        loop {
            let snapshot = self.handle.snapshot();
            if check(&snapshot) {
                return snapshot;
            }
            assert!(
                Instant::now() < deadline,
                "shard {}: no {what} within {WAIT:?}; snapshot: {snapshot:?}",
                self.id
            );
            tokio::time::sleep(POLL).await;
        }
    }

    async fn wait_session_info(&self, expected: SessionInfo) {
        let deadline = Instant::now() + WAIT;
        loop {
            let info = self.handle.session_info();
            if info.as_ref() == Some(&expected) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "shard {}: session {info:?}, expected {expected:?}",
                self.id
            );
            tokio::time::sleep(POLL).await;
        }
    }

    /// The message ids of `seqs` of `session`, as the workers publish them.
    fn ids(&self, session: &str, seqs: RangeInclusive<u64>) -> Vec<String> {
        seqs.map(|seq| format!("{}:{session}:{seq}", self.id))
            .collect()
    }

    fn finished(&self) -> bool {
        self.runner.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Wait for `run()` to return, then for the workers to drain.
    async fn join(&mut self, within: Duration) {
        let runner = self.runner.take().expect("joined once");
        tokio::time::timeout(within, runner)
            .await
            .unwrap_or_else(|_| panic!("shard {}: run() still running after {within:?}", self.id))
            .expect("run() panicked");
        for worker in std::mem::take(&mut self.workers) {
            tokio::time::timeout(WAIT, worker)
                .await
                .expect("workers drain once the runner is gone")
                .expect("worker panicked");
        }
    }

    /// Shut down through the watch channel, as `app` does on SIGTERM.
    async fn stop(&mut self) {
        self.shutdown.send_replace(true);
        self.join(CLOSE_TIMEOUT + Duration::from_secs(2)).await;
    }
}

// ---------------------------------------------------------------------------
// Helpers

async fn eventually<T>(what: &str, mut check: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(value) = check() {
            return value;
        }
        assert!(Instant::now() < deadline, "no {what} within {WAIT:?}");
        tokio::time::sleep(POLL).await;
    }
}

/// A TYPING_START: published, never cached.
fn typing(seq: u64) -> Value {
    json!({"channel_id": "5", "guild_id": "7", "user_id": "1", "timestamp": 1_700_000_000 + seq})
}

async fn send_typing(conn: &GatewayConnection, seqs: RangeInclusive<u64>) {
    for seq in seqs {
        conn.dispatch("TYPING_START", seq, &typing(seq)).await;
    }
}

/// `gateway_events{type, shard}`: dispatches the runner received.
fn received(shard: u32, kind: &str) -> u64 {
    GATEWAY_EVENTS
        .with_label_values(&[kind, shard.to_string().as_str()])
        .get()
}

fn shard_events(kind: &str) -> u64 {
    SHARD_EVENTS.with_label_values(&[kind]).get()
}

fn parse_errors(kind: &str) -> u64 {
    GATEWAY_PARSE_ERRORS.with_label_values(&[kind]).get()
}

fn resume_d(session_id: &str, seq: u64) -> Value {
    json!({"token": format!("Bot {TOKEN}"), "session_id": session_id, "seq": seq})
}

fn session(session_id: &str, sequence: u64, resume_url: &str) -> SessionInfo {
    SessionInfo {
        session_id: session_id.to_owned(),
        sequence,
        resume_url: Some(resume_url.to_owned()),
    }
}

fn has_close(conn: &GatewayConnection) -> bool {
    conn.frames()
        .iter()
        .any(|frame| matches!(frame, Frame::Close { .. }))
}

/// Accept the shard's first connection, HELLO, IDENTIFY and READY (seq 1).
async fn identified(
    fake: &FakeDiscord,
    shard: u32,
    session_id: &str,
    heartbeat_ms: u64,
) -> GatewayConnection {
    let mut conn = fake.gateway.accept().await;
    assert!(!conn.is_resume(), "a new session starts at the gateway URL");
    conn.hello(heartbeat_ms).await;
    let identify = conn.expect_identify().await;
    assert_eq!(identify["shard"], json!([shard, TOTAL]));
    assert_eq!(identify["token"], json!(format!("Bot {TOKEN}")));
    conn.ready(1, session_id, [shard, TOTAL], &[]).await;
    conn
}

/// Accept a reconnect at READY's resume URL, HELLO, and return the
/// connection with the `d` of its first frame (which must be a RESUME).
async fn resumed(fake: &FakeDiscord) -> (GatewayConnection, Value) {
    let mut conn = fake.gateway.accept().await;
    assert_eq!(conn.path, "/resume/", "resumes go to READY's resume URL");
    conn.hello(QUIET).await;
    let resume = conn.expect_resume().await;
    (conn, resume)
}

/// Exactly these message ids, in this order. Reports duplicates, gaps and the
/// first difference instead of dumping both lists.
fn assert_ids(published: &[Published], expected: &[String]) {
    let got: Vec<&str> = published
        .iter()
        .map(|p| p.id.as_deref().unwrap_or("<none>"))
        .collect();
    if got.len() == expected.len() && got.iter().zip(expected).all(|(a, b)| *a == b.as_str()) {
        return;
    }
    let mut seen = HashSet::new();
    let duplicates: Vec<&str> = got
        .iter()
        .filter(|id| !seen.insert(**id))
        .take(10)
        .copied()
        .collect();
    let missing: Vec<&String> = expected
        .iter()
        .filter(|id| !seen.contains(id.as_str()))
        .take(10)
        .collect();
    let first = got.iter().zip(expected).position(|(a, b)| *a != b.as_str());
    panic!(
        "published {} ids, expected {}; duplicates: {duplicates:?}; missing: {missing:?}; \
         first difference at {first:?}: {:?}",
        got.len(),
        expected.len(),
        first.map(|i| (got[i], &expected[i]))
    );
}

/// A port nothing listens on right now.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A Redis that never answers.
#[derive(Clone)]
struct Unresponsive;

impl ConnectionLike for Unresponsive {
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

// ---------------------------------------------------------------------------
// Relay

/// A TCP relay in front of the fake gateway that can hold back everything the
/// shard sends. While it holds, the gateway does not see (and so does not
/// answer) the shard's close frame, so a test can deliver events while the
/// shard is closing, or never answer at all. `cut` drops the connections
/// without a close frame.
struct Relay {
    addr: SocketAddr,
    hold: watch::Sender<bool>,
    cut: watch::Sender<bool>,
    /// Bytes received from the shard while holding.
    held: Arc<AtomicUsize>,
}

impl Relay {
    async fn start(gateway_url: &str) -> Self {
        let target: SocketAddr = gateway_url
            .trim_start_matches("ws://")
            .parse()
            .expect("ws://ip:port");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hold = watch::channel(false).0;
        let cut = watch::channel(false).0;
        let held = Arc::new(AtomicUsize::new(0));
        let (hold_rx, cut_rx, counter) = (hold.subscribe(), cut.subscribe(), held.clone());
        tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                let Ok(server) = TcpStream::connect(target).await else {
                    continue;
                };
                tokio::spawn(relay(
                    client,
                    server,
                    hold_rx.clone(),
                    cut_rx.clone(),
                    counter.clone(),
                ));
            }
        });
        Self {
            addr,
            hold,
            cut,
            held,
        }
    }

    /// The `proxy_url` for the shard.
    fn url(&self) -> String {
        format!("ws://{}", self.addr)
    }

    fn hold(&self, hold: bool) {
        self.hold.send_replace(hold);
    }

    fn cut(&self) {
        self.cut.send_replace(true);
    }

    async fn wait_held(&self) {
        eventually("the shard to send something while held", || {
            (self.held.load(Ordering::SeqCst) > 0).then_some(())
        })
        .await;
    }
}

/// Resolves once the relay is cut (or dropped).
async fn until_cut(cut: &mut watch::Receiver<bool>) {
    let _ = cut.wait_for(|cut| *cut).await;
}

async fn relay(
    client: TcpStream,
    server: TcpStream,
    mut hold: watch::Receiver<bool>,
    mut cut: watch::Receiver<bool>,
    held: Arc<AtomicUsize>,
) {
    let (mut from_client, mut to_client) = client.into_split();
    let (mut from_server, mut to_server) = server.into_split();
    let mut down_cut = cut.clone();
    let down = tokio::spawn(async move {
        tokio::select! {
            _ = tokio::io::copy(&mut from_server, &mut to_client) => {
                let _ = to_client.shutdown().await;
            }
            _ = until_cut(&mut down_cut) => {}
        }
    });
    let mut pending = Vec::new();
    let mut buf = vec![0; 16 * 1024];
    let mut eof = false;
    let cut_off = loop {
        let holding = *hold.borrow_and_update();
        if !holding && !pending.is_empty() {
            if to_server.write_all(&pending).await.is_err() {
                break false;
            }
            pending.clear();
        }
        if eof && !holding {
            let _ = to_server.shutdown().await;
            break false;
        }
        tokio::select! {
            read = from_client.read(&mut buf), if !eof => match read {
                Ok(0) | Err(_) => eof = true,
                Ok(n) => {
                    pending.extend_from_slice(&buf[..n]);
                    if *hold.borrow() {
                        held.fetch_add(n, Ordering::SeqCst);
                    }
                }
            },
            changed = hold.changed() => if changed.is_err() {
                break true;
            },
            _ = until_cut(&mut cut) => break true,
        }
    };
    if cut_off {
        // Both halves of each socket are dropped: the shard sees the
        // connection end without a close frame.
        down.abort();
    }
}

// ---------------------------------------------------------------------------
// Backpressure

/// The pause closes the shard resumably, opens no connection while the
/// workers are stuck, and costs exactly one RESUME from the last received
/// sequence; nothing is lost or published twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_full_queue_pauses_the_shard_and_resumes_it_once() {
    let _metrics = METRICS.read().await;
    let Some(redis) = common::redis(IDLE_DB).await else {
        return;
    };
    let fake = FakeDiscord::start().await;
    let shard = 1;
    let base = received(shard, "TYPING_START");
    let opts = Opts {
        open: false,
        ..Opts::default()
    };
    let mut running = start(&fake.gateway.url(), shard, redis, opts);

    let mut conn = identified(&fake, shard, "A", QUIET).await;
    running.recorder.wait_entered(1).await;
    send_typing(&conn, 2..=FULL).await;

    assert_eq!(conn.expect_close().await, Some(4000), "closed resumably");
    let paused = running
        .wait_snapshot("the pause", |s| {
            s.status == "Disconnected" && s.close_code == Some(4000)
        })
        .await;
    assert!(!paused.active);
    assert_eq!(paused.session_id.as_deref(), Some("A"));
    assert!(
        fake.gateway
            .try_accept(Duration::from_millis(1500))
            .await
            .is_none(),
        "reconnected while the workers were still full"
    );
    assert_eq!(running.recorder.count(), 0);
    assert_eq!(received(shard, "TYPING_START") - base, FULL - 1);

    running.recorder.open();
    // The held events are queued; until the next message the runner reports
    // that it waits for the old connection (for /healthcheck).
    running
        .wait_snapshot("the wait for the old connection", |s| {
            s.close_wait_since.is_some()
        })
        .await;
    let (mut conn, resume) = resumed(&fake).await;
    assert_eq!(
        resume,
        resume_d("A", FULL),
        "resumes after the last received"
    );
    conn.resumed(FULL + 1).await;
    send_typing(&conn, FULL + 2..=FULL + 4).await;

    let published = running.recorder.wait_published(FULL as usize + 4).await;
    assert_ids(&published, &running.ids("A", 1..=FULL + 4));
    assert_eq!(published[0].key, "READY");
    assert_eq!(published[FULL as usize].key, "RESUMED");
    assert_eq!(received(shard, "TYPING_START") - base, FULL - 1 + 3);
    let connected = running
        .wait_snapshot("the shard to be connected", |s| s.status == "Connected")
        .await;
    assert!(connected.active && connected.close_wait_since.is_none());
    running
        .wait_session_info(session("A", FULL + 4, &fake.gateway.resume_url()))
        .await;

    // Exactly one reconnect: the resumed connection stays open.
    assert_eq!(conn.try_next_frame(Duration::from_millis(300)).await, None);
    assert_eq!(fake.gateway.connections(), 2);
    running.stop().await;
    assert_eq!(conn.expect_close().await, Some(4000));
}

/// Workers that cannot publish do not stop the shard from reading: it keeps
/// heartbeating with the latest sequence on time.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_workers_do_not_hold_up_heartbeats() {
    let _metrics = METRICS.read().await;
    let Some(redis) = common::redis(IDLE_DB).await else {
        return;
    };
    let fake = FakeDiscord::start().await;
    let shard = 2;
    let base = received(shard, "TYPING_START");
    let opts = Opts {
        open: false,
        ..Opts::default()
    };
    let mut running = start(&fake.gateway.url(), shard, redis, opts);

    let mut conn = identified(&fake, shard, "A", 200).await;
    send_typing(&conn, 2..=300).await;
    eventually("every event to be received", || {
        (received(shard, "TYPING_START") - base == 299).then_some(())
    })
    .await;
    let started = Instant::now();
    for _ in 0..3 {
        conn.wait_for("a heartbeat with sequence 300", |frame| {
            frame.is_heartbeat() && frame.d() == Some(&json!(300))
        })
        .await;
    }
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "3 heartbeats at 200 ms took {:?}",
        started.elapsed()
    );
    assert!(!has_close(&conn), "{:?}", conn.frames());
    let snapshot = running
        .wait_snapshot("a heartbeat ACK", |s| s.last_ack.is_some())
        .await;
    assert_eq!(snapshot.status, "Connected");
    assert!(snapshot.active);
    assert!(snapshot
        .silence()
        .is_some_and(|s| s < Duration::from_secs(1)));
    assert_eq!(running.recorder.count(), 0, "the gate is shut");

    running.recorder.open();
    let published = running.recorder.wait_published(300).await;
    assert_ids(&published, &running.ids("A", 1..=300));
    running.stop().await;
    assert_eq!(conn.expect_close().await, Some(4000));
}

/// A queue that fills up after the shard already closed (here: on RECONNECT)
/// must not queue another close: it would close the next connection right
/// after it opened.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_full_queue_after_the_shard_closed_does_not_close_the_next_connection() {
    let _metrics = METRICS.read().await;
    let Some(redis) = common::redis(IDLE_DB).await else {
        return;
    };
    let fake = FakeDiscord::start().await;
    let relay = Relay::start(&fake.gateway.url()).await;
    let shard = 3;
    let base = received(shard, "TYPING_START");
    let opts = Opts {
        open: false,
        ..Opts::default()
    };
    let mut running = start(&relay.url(), shard, redis, opts);

    let mut conn = identified(&fake, shard, "A", QUIET).await;
    running.recorder.wait_entered(1).await;
    // Exactly fills the queue.
    send_typing(&conn, 2..=FULL - 1).await;
    eventually("the queue to be full", || {
        (received(shard, "TYPING_START") - base == FULL - 2).then_some(())
    })
    .await;
    relay.hold(true);
    conn.reconnect().await;
    relay.wait_held().await;
    // In flight when the shard closed: FULL is received while the shard is
    // disconnected and does not fit.
    send_typing(&conn, FULL..=FULL + 4).await;
    eventually("the event that does not fit", || {
        (received(shard, "TYPING_START") - base == FULL - 1).then_some(())
    })
    .await;
    relay.hold(false);
    assert_eq!(conn.expect_close().await, Some(4000));

    running.recorder.open();
    // The first frame of the next connection is its RESUME, not a stale close.
    let (mut conn, resume) = resumed(&fake).await;
    assert_eq!(resume, resume_d("A", FULL + 4));
    conn.resumed(FULL + 5).await;
    let published = running.recorder.wait_published(FULL as usize + 5).await;
    assert_ids(&published, &running.ids("A", 1..=FULL + 5));
    assert_eq!(conn.try_next_frame(Duration::from_millis(300)).await, None);
    assert_eq!(fake.gateway.connections(), 2);
    running.stop().await;
}

/// The gateway drops the connection instead of answering the pause's close.
/// twilight reconnects on its own; while the workers are still stuck the
/// runner closes that connection again right after RESUME rather than leave
/// it open without heartbeats, then waits. Nothing is lost or duplicated.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pause_whose_close_is_never_answered_does_not_stay_connected() {
    let _metrics = METRICS.read().await;
    let Some(redis) = common::redis(IDLE_DB).await else {
        return;
    };
    let fake = FakeDiscord::start().await;
    let relay = Relay::start(&fake.gateway.url()).await;
    let shard = 4;
    let opts = Opts {
        open: false,
        ..Opts::default()
    };
    let mut running = start(&relay.url(), shard, redis, opts);

    let mut first = identified(&fake, shard, "A", QUIET).await;
    running.recorder.wait_entered(1).await;
    relay.hold(true);
    send_typing(&first, 2..=FULL).await;
    relay.wait_held().await;
    relay.cut();
    first.expect_gone().await;

    let (mut second, resume) = resumed(&fake).await;
    assert_eq!(resume, resume_d("A", FULL));
    assert_eq!(second.expect_close().await, Some(4000));
    assert!(
        fake.gateway
            .try_accept(Duration::from_millis(1500))
            .await
            .is_none(),
        "reconnected while the workers were still full"
    );
    assert_eq!(running.recorder.count(), 0);

    running.recorder.open();
    let (third, resume) = resumed(&fake).await;
    assert_eq!(resume, resume_d("A", FULL));
    third.resumed(FULL + 1).await;
    let published = running.recorder.wait_published(FULL as usize + 1).await;
    assert_ids(&published, &running.ids("A", 1..=FULL + 1));
    assert_eq!(fake.gateway.connections(), 3);
    running.stop().await;
}

/// A shutdown while the runner waits for the workers keeps every held event
/// (the wait it cuts short drops none): `run()` returns once they are queued,
/// opens no new connection and keeps the session at the last received
/// sequence.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_during_a_pause_waits_for_the_held_events() {
    let _metrics = METRICS.read().await;
    let Some(redis) = common::redis(IDLE_DB).await else {
        return;
    };
    let fake = FakeDiscord::start().await;
    let shard = 22;
    let opts = Opts {
        open: false,
        ..Opts::default()
    };
    let mut running = start(&fake.gateway.url(), shard, redis, opts);

    let mut conn = identified(&fake, shard, "A", QUIET).await;
    running.recorder.wait_entered(1).await;
    send_typing(&conn, 2..=FULL).await;
    assert_eq!(conn.expect_close().await, Some(4000));
    running
        .wait_snapshot("the pause", |s| s.status == "Disconnected")
        .await;

    running.shutdown.send_replace(true);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!running.finished(), "returned with events still held");
    running.recorder.open();
    running.join(Duration::from_secs(3)).await;

    assert_ids(&running.recorder.published(), &running.ids("A", 1..=FULL));
    assert_eq!(fake.gateway.connections(), 1, "opened a connection");
    assert!(!running.snapshot().active);
    assert_eq!(
        running.handle.session_info(),
        Some(session("A", FULL, &fake.gateway.resume_url()))
    );
}

/// As above with workers that stay stuck: the held events get the close
/// timeout, not until `app` aborts the runner. What was not queued is
/// replayed on resume; nothing is published twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_during_a_pause_waits_for_stuck_workers_once() {
    let _metrics = METRICS.read().await;
    let Some(redis) = common::redis(IDLE_DB).await else {
        return;
    };
    let fake = FakeDiscord::start().await;
    let shard = 33;
    let opts = Opts {
        open: false,
        ..Opts::default()
    };
    let mut running = start(&fake.gateway.url(), shard, redis, opts);

    let mut conn = identified(&fake, shard, "A", QUIET).await;
    running.recorder.wait_entered(1).await;
    send_typing(&conn, 2..=FULL).await;
    assert_eq!(conn.expect_close().await, Some(4000));
    running
        .wait_snapshot("the pause", |s| s.status == "Disconnected")
        .await;

    let stopping = Instant::now();
    running.shutdown.send_replace(true);
    eventually("run() to return", || running.finished().then_some(())).await;
    let took = stopping.elapsed();
    assert!(
        took >= CLOSE_TIMEOUT - Duration::from_millis(500)
            && took < CLOSE_TIMEOUT + Duration::from_secs(1),
        "shutdown took {took:?}"
    );
    assert!(!running.snapshot().active);

    running.recorder.open();
    running.join(WAIT).await;
    // READY waited at the gate and 2..=FULL - 1 in the queue; FULL was held.
    assert_ids(
        &running.recorder.published(),
        &running.ids("A", 1..=FULL - 1),
    );
    assert_eq!(fake.gateway.connections(), 1, "opened a connection");
    assert_eq!(
        running.handle.session_info(),
        Some(session("A", FULL - 1, &fake.gateway.resume_url()))
    );
}

// ---------------------------------------------------------------------------
// READY, RESUMED and the snapshot

/// A restored session that Discord rejects is dropped from the snapshot, and
/// READY of the new session replaces it together with its resume point.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ready_replaces_a_restored_session_and_its_resume_point() {
    let _metrics = METRICS.read().await;
    let Some(redis) = common::redis(IDLE_DB).await else {
        return;
    };
    let fake = FakeDiscord::start().await;
    let shard = 5;
    let restored = session("OLD", 500, &fake.gateway.resume_url());
    let opts = Opts {
        restore: Some(restored.clone()),
        ..Opts::default()
    };
    let mut running = start(&fake.gateway.url(), shard, redis, opts);
    assert_eq!(running.handle.session_info(), Some(restored));

    let (mut conn, resume) = resumed(&fake).await;
    assert_eq!(resume, resume_d("OLD", 500));
    conn.invalid_session(false).await;
    assert_eq!(conn.expect_close().await, Some(1000), "not resumable");
    let dropped = running
        .wait_snapshot("the session to be dropped", |s| s.session_id.is_none())
        .await;
    assert_eq!(dropped.resume_url, None);
    assert_eq!(running.handle.session_info(), None);

    let mut conn = fake.gateway.accept().await;
    assert!(!conn.is_resume());
    conn.hello(QUIET).await;
    conn.expect_identify().await;
    conn.ready(1, "NEW", [shard, TOTAL], &[]).await;
    let ready = running
        .wait_snapshot("session NEW", |s| {
            s.session_id.as_deref() == Some("NEW") && s.status == "Connected"
        })
        .await;
    assert_eq!(ready.resume_url, Some(fake.gateway.resume_url()));
    assert!(!ready.fatal && ready.active);
    // The resume point is READY's sequence, not the restored 500.
    running
        .wait_session_info(session("NEW", 1, &fake.gateway.resume_url()))
        .await;

    send_typing(&conn, 2..=4).await;
    let published = running.recorder.wait_published(4).await;
    assert_ids(&published, &running.ids("NEW", 1..=4));
    running
        .wait_session_info(session("NEW", 4, &fake.gateway.resume_url()))
        .await;
    running.stop().await;
}

/// twilight forgets the resume URL when connecting to it fails but keeps the
/// session; the snapshot (and so the saved session) keeps READY's URL.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_resume_url_from_ready_survives_a_failed_reconnect() {
    let _metrics = METRICS.read().await;
    let Some(redis) = common::redis(IDLE_DB).await else {
        return;
    };
    let fake = FakeDiscord::start().await;
    let shard = 8;
    let mut running = start(&fake.gateway.url(), shard, redis, Opts::default());
    let dead = format!("ws://127.0.0.1:{}", free_port());

    let mut conn = fake.gateway.accept().await;
    conn.hello(QUIET).await;
    conn.expect_identify().await;
    let ready = ready_payload(common::BOT_ID, "A", &dead, [shard, TOTAL], &[]);
    conn.dispatch("READY", 1, &ready).await;
    send_typing(&conn, 2..=3).await;
    running.recorder.wait_published(3).await;
    running
        .wait_snapshot("READY's resume URL", |s| {
            s.resume_url.as_deref() == Some(dead.as_str())
        })
        .await;

    conn.reconnect().await;
    assert_eq!(conn.expect_close().await, Some(4000));
    // The resume URL refuses the connection; twilight falls back to the
    // gateway URL with the same session.
    let mut conn = fake.gateway.accept().await;
    assert!(!conn.is_resume());
    let snapshot = running.snapshot();
    assert_eq!(snapshot.session_id.as_deref(), Some("A"));
    assert_eq!(snapshot.resume_url.as_deref(), Some(dead.as_str()));
    conn.hello(QUIET).await;
    assert_eq!(conn.expect_resume().await, resume_d("A", 3));
    conn.resumed(4).await;
    running.recorder.wait_published(4).await;
    running.wait_session_info(session("A", 4, &dead)).await;
    running.stop().await;
}

/// When twilight drops the session (INVALID_SESSION false, or a 4009 close
/// through the vendored patch) the snapshot drops it too, so it is never
/// saved, and the next connection identifies.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_session_is_cleared_when_twilight_drops_it() {
    let _metrics = METRICS.read().await;
    let Some(redis) = common::redis(IDLE_DB).await else {
        return;
    };
    let fake = FakeDiscord::start().await;
    let shard = 9;
    let mut running = start(&fake.gateway.url(), shard, redis, Opts::default());

    let mut conn = identified(&fake, shard, "A", QUIET).await;
    send_typing(&conn, 2..=2).await;
    running.recorder.wait_published(2).await;
    conn.invalid_session(false).await;
    assert_eq!(conn.expect_close().await, Some(1000));
    let dropped = running
        .wait_snapshot("session A to be dropped", |s| s.session_id.is_none())
        .await;
    assert_eq!(dropped.resume_url, None);
    assert_eq!(running.handle.session_info(), None);

    let mut conn = fake.gateway.accept().await;
    assert!(!conn.is_resume());
    conn.hello(QUIET).await;
    conn.expect_identify().await;
    conn.ready(1, "B", [shard, TOTAL], &[]).await;
    running
        .wait_snapshot("session B", |s| {
            s.session_id.as_deref() == Some("B")
                && s.resume_url.as_deref() == Some(fake.gateway.resume_url().as_str())
        })
        .await;
    send_typing(&conn, 2..=2).await;
    running.recorder.wait_published(4).await;

    conn.close(4009, "Session timed out").await;
    let dropped = running
        .wait_snapshot("session B to be dropped", |s| {
            s.session_id.is_none() && s.close_code == Some(4009)
        })
        .await;
    assert_eq!(dropped.resume_url, None);
    assert!(!dropped.fatal);
    assert_eq!(running.handle.session_info(), None);

    let mut conn = fake.gateway.accept().await;
    assert!(!conn.is_resume());
    conn.hello(QUIET).await;
    conn.expect_identify().await;
    conn.ready(1, "C", [shard, TOTAL], &[]).await;
    let published = running.recorder.wait_published(5).await;
    let mut expected = running.ids("A", 1..=2);
    expected.extend(running.ids("B", 1..=2));
    expected.extend(running.ids("C", 1..=1));
    assert_ids(&published, &expected);
    running.stop().await;
}

/// Events of the cache's types that the model cannot parse, a READY the model
/// rejects and unknown dispatch types are published anyway, `d` byte for
/// byte; the parsed ones reach the cache.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn events_the_cache_cannot_parse_are_still_published() {
    let _metrics = METRICS.read().await;
    let Some(mut cache) = common::redis(PARSE_DB).await else {
        return;
    };
    let fake = FakeDiscord::start().await;
    let shard = 7;
    let guild = (7u64 << 22) + 70;
    let (ready_errors, role_errors, guild_errors) = (
        parse_errors("READY"),
        parse_errors("GUILD_ROLE_DELETE"),
        parse_errors("GUILD_CREATE"),
    );
    let opts = Opts {
        state: common::state_all(),
        ..Opts::default()
    };
    let mut running = start(&fake.gateway.url(), shard, cache.clone(), opts);

    let mut conn = fake.gateway.accept().await;
    conn.hello(QUIET).await;
    conn.expect_identify().await;
    // Only what twilight itself needs from READY.
    let ready = json!({"session_id": "A", "resume_gateway_url": fake.gateway.resume_url()});
    conn.dispatch("READY", 1, &ready).await;
    conn.dispatch("GUILD_CREATE", 2, &common::guild(guild, "seven"))
        .await;
    let role_delete = format!(r#"{{"guild_id":"{guild}"}}"#);
    conn.dispatch_raw("GUILD_ROLE_DELETE", 3, &role_delete)
        .await;
    let future = r#"{"x": [1, 2.50],  "y":null}"#;
    conn.dispatch_raw("SOME_FUTURE_EVENT", 4, future).await;

    let published = running.recorder.wait_published(4).await;
    assert_ids(&published, &running.ids("A", 1..=4));
    let keys: Vec<&str> = published.iter().map(|p| p.key.as_str()).collect();
    assert_eq!(
        keys,
        [
            "READY",
            "GUILD_CREATE",
            "GUILD_ROLE_DELETE",
            "SOME_FUTURE_EVENT"
        ]
    );
    assert_eq!(
        published[0].body,
        json!({"op": 0, "t": "READY", "d": ready})
    );
    assert_eq!(
        published[2].raw,
        format!(r#"{{"op":0,"t":"GUILD_ROLE_DELETE","d":{role_delete}}}"#)
    );
    assert_eq!(
        published[3].raw,
        format!(r#"{{"op":0,"t":"SOME_FUTURE_EVENT","d":{future}}}"#)
    );
    // READY still started the session.
    let snapshot = running.snapshot();
    assert_eq!(snapshot.session_id.as_deref(), Some("A"));
    assert_eq!(snapshot.resume_url, Some(fake.gateway.resume_url()));

    assert_eq!(parse_errors("READY") - ready_errors, 1);
    assert_eq!(parse_errors("GUILD_ROLE_DELETE") - role_errors, 1);
    assert_eq!(parse_errors("GUILD_CREATE") - guild_errors, 0);
    assert!(common::exists(&mut cache, &format!("guild:{guild}")).await);
    for kind in [
        "READY",
        "GUILD_CREATE",
        "GUILD_ROLE_DELETE",
        "SOME_FUTURE_EVENT",
    ] {
        assert_eq!(received(shard, kind), 1, "{kind}");
    }
    running.stop().await;
}

/// Jobs of a previous session still queued when READY of the next session
/// arrives must not undo what READY reconciled: session A's GUILD_CREATE of
/// a guild the bot has left since does not bring it back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn queued_events_of_a_previous_session_do_not_undo_ready() {
    let _metrics = METRICS.read().await;
    let Some(mut cache) = common::redis(STALE_DB).await else {
        return;
    };
    let fake = FakeDiscord::start().await;
    let shard = 20;
    // Guilds of this shard: (id >> 22) % TOTAL == shard.
    let (left, kept) = ((20u64 << 22) + 70, (20u64 << 22) + 700);
    let opts = Opts {
        state: common::state_all(),
        open: false,
        ..Opts::default()
    };
    let mut running = start(&fake.gateway.url(), shard, cache.clone(), opts);

    let mut conn = fake.gateway.accept().await;
    conn.hello(QUIET).await;
    conn.expect_identify().await;
    conn.ready(1, "A", [shard, TOTAL], &[left]).await;
    running.recorder.wait_entered(1).await;
    conn.dispatch("GUILD_CREATE", 2, &common::guild(left, "left"))
        .await;
    eventually("GUILD_CREATE to be received", || {
        (received(shard, "GUILD_CREATE") == 1).then_some(())
    })
    .await;
    conn.invalid_session(false).await;

    let mut conn = fake.gateway.accept().await;
    conn.hello(QUIET).await;
    conn.expect_identify().await;
    conn.ready(1, "B", [shard, TOTAL], &[kept]).await;
    conn.dispatch("GUILD_CREATE", 2, &common::guild(kept, "kept"))
        .await;
    // Received after READY B (and its cache update) was handled.
    eventually("session B's GUILD_CREATE to be received", || {
        (received(shard, "GUILD_CREATE") == 2).then_some(())
    })
    .await;
    assert_eq!(running.snapshot().session_id.as_deref(), Some("B"));
    assert!(
        !common::exists(&mut cache, &format!("guild:{left}")).await,
        "READY B removed the guild"
    );

    running.recorder.open();
    let published = running.recorder.wait_published(4).await;
    let mut expected = running.ids("A", 1..=2);
    expected.extend(running.ids("B", 1..=2));
    assert_ids(&published, &expected);
    assert!(
        !common::exists(&mut cache, &format!("guild:{left}")).await,
        "session A's GUILD_CREATE brought the guild back"
    );
    assert!(!common::exists(&mut cache, &format!("channel:{}", left + 3)).await);
    let index = common::members(&mut cache, "guild_keys").await;
    assert!(!index.contains(&format!("guild:{left}")), "{index:?}");
    assert!(common::exists(&mut cache, &format!("guild:{kept}")).await);
    running.stop().await;
}

/// With several workers, the events of one guild (or DM channel) are still
/// published in gateway order, each exactly once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn events_of_one_guild_stay_in_order_across_workers() {
    let _metrics = METRICS.read().await;
    let Some(redis) = common::redis(IDLE_DB).await else {
        return;
    };
    let fake = FakeDiscord::start().await;
    let shard = 21;
    let opts = Opts {
        workers: 4,
        // Uneven publish times, so workers overtake each other.
        delay: Some(Box::new(|seq| Duration::from_millis(seq * 7 % 3))),
        ..Opts::default()
    };
    let mut running = start(&fake.gateway.url(), shard, redis, opts);
    let conn = identified(&fake, shard, "A", QUIET).await;
    for seq in 2..=401u64 {
        match seq % 10 {
            n @ 0..=7 => {
                let d = json!({"guild_id": (1000 + n).to_string(), "channel_id": "5",
                    "user_id": "1", "timestamp": seq});
                conn.dispatch("TYPING_START", seq, &d).await;
            }
            8 => {
                let d = json!({"channel_id": "77", "user_id": "1", "timestamp": seq});
                conn.dispatch("TYPING_START", seq, &d).await;
            }
            _ => conn.dispatch("X_UNROUTED", seq, &json!({"n": seq})).await,
        }
    }

    let published = running.recorder.wait_published(401).await;
    let mut seqs: Vec<u64> = published.iter().map(Published::seq).collect();
    let mut routes: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    for p in &published {
        let d = &p.body["d"];
        let route = match (d.get("guild_id"), d.get("channel_id")) {
            (Some(guild), _) => format!("guild {guild}"),
            (None, Some(channel)) => format!("channel {channel}"),
            _ => "unrouted".to_owned(),
        };
        routes.entry(route).or_default().push(p.seq());
    }
    assert_eq!(routes.len(), 10, "{:?}", routes.keys());
    for (route, order) in &routes {
        assert!(
            order.windows(2).all(|w| w[0] < w[1]),
            "{route} published out of order: {order:?}"
        );
    }
    seqs.sort_unstable();
    assert_eq!(seqs, (1..=401).collect::<Vec<_>>(), "exactly once");
    running
        .wait_session_info(session("A", 401, &fake.gateway.resume_url()))
        .await;
    running.stop().await;
}

// ---------------------------------------------------------------------------
// Shutdown

/// Shutdown closes resumably (4000) and keeps dispatching what arrives until
/// the gateway answers; the session is kept with the last sequence.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_closes_resumably_and_dispatches_what_arrives_while_closing() {
    let _metrics = METRICS.read().await;
    let Some(redis) = common::redis(IDLE_DB).await else {
        return;
    };
    let fake = FakeDiscord::start().await;
    let relay = Relay::start(&fake.gateway.url()).await;
    let shard = 10;
    let mut running = start(&relay.url(), shard, redis, Opts::default());

    let mut conn = identified(&fake, shard, "A", QUIET).await;
    send_typing(&conn, 2..=5).await;
    running.recorder.wait_published(5).await;

    relay.hold(true);
    running.shutdown.send_replace(true);
    relay.wait_held().await;
    // Sent before the gateway saw the close.
    send_typing(&conn, 6..=10).await;
    running.recorder.wait_published(10).await;
    assert!(!running.finished(), "returned before the gateway answered");

    relay.hold(false);
    assert_eq!(conn.expect_close().await, Some(4000), "closed resumably");
    running.join(Duration::from_secs(3)).await;
    assert_ids(&running.recorder.published(), &running.ids("A", 1..=10));
    let snapshot = running.snapshot();
    assert!(!snapshot.active);
    assert_eq!(snapshot.session_id.as_deref(), Some("A"));
    assert_eq!(
        running.handle.session_info(),
        Some(session("A", 10, &fake.gateway.resume_url()))
    );
    assert_eq!(fake.gateway.connections(), 1);
}

/// A gateway that never answers the close does not hold up the shutdown for
/// longer than the close timeout; what arrived meanwhile is still dispatched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_gives_up_on_a_gateway_that_never_answers_the_close() {
    let _metrics = METRICS.read().await;
    let Some(redis) = common::redis(IDLE_DB).await else {
        return;
    };
    let fake = FakeDiscord::start().await;
    let relay = Relay::start(&fake.gateway.url()).await;
    let shard = 11;
    let mut running = start(&relay.url(), shard, redis, Opts::default());

    let conn = identified(&fake, shard, "A", QUIET).await;
    send_typing(&conn, 2..=3).await;
    running.recorder.wait_published(3).await;

    relay.hold(true);
    let stopping = Instant::now();
    running.shutdown.send_replace(true);
    relay.wait_held().await;
    send_typing(&conn, 4..=6).await;
    running.join(CLOSE_TIMEOUT * 2).await;
    let took = stopping.elapsed();
    assert!(
        took >= CLOSE_TIMEOUT - Duration::from_millis(500)
            && took < CLOSE_TIMEOUT + Duration::from_secs(2),
        "shutdown took {took:?}"
    );
    assert_ids(&running.recorder.published(), &running.ids("A", 1..=6));
    assert_eq!(
        running.handle.session_info(),
        Some(session("A", 6, &fake.gateway.resume_url()))
    );
}

/// A shard that is already disconnected (waiting to reconnect) has nothing to
/// close: shutdown returns at once without opening a connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_while_disconnected_opens_no_connection() {
    let _metrics = METRICS.read().await;
    let Some(redis) = common::redis(IDLE_DB).await else {
        return;
    };
    let fake = FakeDiscord::start().await;
    let shard = 12;
    let mut running = start(&fake.gateway.url(), shard, redis, Opts::default());

    let conn = identified(&fake, shard, "A", QUIET).await;
    send_typing(&conn, 2..=3).await;
    running.recorder.wait_published(3).await;
    conn.close(4000, "").await;
    let closed = running
        .wait_snapshot("the gateway's close", |s| {
            s.status == "Disconnected" && s.close_code == Some(4000)
        })
        .await;
    assert!(!closed.fatal && !closed.active);

    let stopping = Instant::now();
    running.shutdown.send_replace(true);
    running.join(Duration::from_secs(1)).await;
    assert!(stopping.elapsed() < Duration::from_millis(500));
    assert!(
        fake.gateway
            .try_accept(Duration::from_millis(1500))
            .await
            .is_none(),
        "connected only to close again"
    );
    assert_eq!(fake.gateway.connections(), 1);
    assert_eq!(
        running.handle.session_info(),
        Some(session("A", 3, &fake.gateway.resume_url()))
    );
}

/// One deadline for the whole shutdown: a gateway that never answers the
/// close and workers that never take what arrived meanwhile cost the close
/// timeout once, not once each. What was not queued is replayed on resume.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_waits_once_for_the_gateway_and_the_held_events() {
    let _metrics = METRICS.read().await;
    let Some(redis) = common::redis(IDLE_DB).await else {
        return;
    };
    let fake = FakeDiscord::start().await;
    let relay = Relay::start(&fake.gateway.url()).await;
    let shard = 27;
    let opts = Opts {
        open: false,
        ..Opts::default()
    };
    let mut running = start(&relay.url(), shard, redis, opts);

    let conn = identified(&fake, shard, "A", QUIET).await;
    running.recorder.wait_entered(1).await;
    relay.hold(true);
    let stopping = Instant::now();
    running.shutdown.send_replace(true);
    relay.wait_held().await;
    // READY waits at the gate, 2..=FULL - 1 fill the queue and FULL is held.
    send_typing(&conn, 2..=FULL).await;
    eventually("every event received", || {
        (received(shard, "TYPING_START") == FULL - 1).then_some(())
    })
    .await;
    eventually("run() to return", || running.finished().then_some(())).await;
    let took = stopping.elapsed();
    assert!(
        took >= CLOSE_TIMEOUT - Duration::from_millis(500)
            && took < CLOSE_TIMEOUT + Duration::from_secs(1),
        "shutdown took {took:?}"
    );

    running.recorder.open();
    running.join(WAIT).await;
    assert_ids(
        &running.recorder.published(),
        &running.ids("A", 1..=FULL - 1),
    );
    assert_eq!(
        running.handle.session_info(),
        Some(session("A", FULL - 1, &fake.gateway.resume_url()))
    );
}

/// A READY that arrives while closing is still published and starts the
/// session, but its cache update does not outlast the close timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_does_not_wait_for_readys_cache_update_past_the_close_timeout() {
    let _metrics = METRICS.read().await;
    let Some(redis) = common::redis(READY_DB).await else {
        return;
    };
    let fake = FakeDiscord::start().await;
    let relay = Relay::start(&fake.gateway.url()).await;
    let shard = 29;
    let opts = Opts {
        state: StateConfig {
            enabled: true,
            ..StateConfig::default()
        },
        ..Opts::default()
    };
    let mut running = start_with(&relay.url(), shard, redis, Unresponsive, opts);

    let mut conn = fake.gateway.accept().await;
    conn.hello(QUIET).await;
    conn.expect_identify().await;
    relay.hold(true);
    let stopping = Instant::now();
    running.shutdown.send_replace(true);
    relay.wait_held().await;
    conn.ready(1, "A", [shard, TOTAL], &[]).await;
    running.recorder.wait_published(1).await;
    eventually("run() to return", || running.finished().then_some(())).await;
    let took = stopping.elapsed();
    assert!(
        took >= CLOSE_TIMEOUT - Duration::from_millis(500)
            && took < CLOSE_TIMEOUT + Duration::from_millis(700),
        "shutdown took {took:?}"
    );
    running.join(WAIT).await;
    assert_ids(&running.recorder.published(), &running.ids("A", 1..=1));
    assert_eq!(
        running.handle.session_info(),
        Some(session("A", 1, &fake.gateway.resume_url()))
    );
}

/// The gateway drops the connection instead of answering the shutdown's
/// close: nothing is left to wait for, so `run()` returns promptly and the
/// shard does not resume a new connection during the shutdown, nor counts
/// one it abandons.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_does_not_resume_when_the_gateway_drops_the_connection() {
    let _metrics = METRICS.write().await;
    let Some(redis) = common::redis(IDLE_DB).await else {
        return;
    };
    let fake = FakeDiscord::start().await;
    let relay = Relay::start(&fake.gateway.url()).await;
    let shard = 26;
    let mut running = start(&relay.url(), shard, redis, Opts::default());

    let mut conn = identified(&fake, shard, "A", QUIET).await;
    send_typing(&conn, 2..=3).await;
    running.recorder.wait_published(3).await;
    let kinds = ["Connecting", "Identifying", "Resuming"];
    let before = kinds.map(shard_events);

    relay.hold(true);
    let stopping = Instant::now();
    running.shutdown.send_replace(true);
    relay.wait_held().await;
    relay.cut();
    conn.expect_gone().await;
    // Should twilight connect again, answer like Discord and see what the
    // shard sends.
    let mut reconnected = None;
    if let Some(mut second) = fake.gateway.try_accept(Duration::from_secs(3)).await {
        let hello = json!({"op": 10, "s": null, "t": null, "d": {"heartbeat_interval": QUIET}});
        if second.try_send_text(hello.to_string()).await {
            reconnected = second.try_next_frame(Duration::from_secs(1)).await;
        }
    }
    running.join(CLOSE_TIMEOUT * 2).await;
    let took = stopping.elapsed();
    assert!(
        !matches!(reconnected.as_ref().and_then(Frame::op), Some(2 | 6)),
        "identified or resumed a new connection during the shutdown: {reconnected:?}"
    );
    // twilight 0.17 does reconnect; the runner drops that connection at HELLO.
    assert_eq!(reconnected, Some(Frame::Gone));
    // twilight calls it Resuming by then, but nothing is resumed.
    assert_eq!(kinds.map(shard_events), before, "{kinds:?}");
    assert!(took < Duration::from_secs(3), "shutdown took {took:?}");
    assert_eq!(
        running.handle.session_info(),
        Some(session("A", 3, &fake.gateway.resume_url()))
    );
}

/// As above, but connecting again fails: `run()` returns at the failed
/// attempt instead of retrying until the close timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_does_not_retry_when_the_gateway_drops_the_connection() {
    let _metrics = METRICS.read().await;
    let Some(redis) = common::redis(IDLE_DB).await else {
        return;
    };
    let fake = FakeDiscord::start().await;
    let relay = Relay::start(&fake.gateway.url()).await;
    let shard = 28;
    let mut running = start(&relay.url(), shard, redis, Opts::default());
    // Resuming goes to a port nothing listens on, then (cut) to the relay.
    let dead = format!("ws://127.0.0.1:{}", free_port());

    let mut conn = fake.gateway.accept().await;
    conn.hello(QUIET).await;
    conn.expect_identify().await;
    let ready = ready_payload(common::BOT_ID, "A", &dead, [shard, TOTAL], &[]);
    conn.dispatch("READY", 1, &ready).await;
    send_typing(&conn, 2..=3).await;
    running.recorder.wait_published(3).await;

    relay.hold(true);
    let stopping = Instant::now();
    running.shutdown.send_replace(true);
    relay.wait_held().await;
    relay.cut();
    conn.expect_gone().await;
    running.join(CLOSE_TIMEOUT * 2).await;
    let took = stopping.elapsed();
    // twilight waits 1 s before connecting again, then 2 s after a failure.
    assert!(took < Duration::from_millis(2500), "shutdown took {took:?}");
    assert_eq!(fake.gateway.connections(), 1);
    assert_eq!(running.handle.session_info(), Some(session("A", 3, &dead)));
}

/// Shutting down before the first connection does not connect at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_before_connecting_opens_no_connection() {
    let _metrics = METRICS.read().await;
    let Some(redis) = common::redis(IDLE_DB).await else {
        return;
    };
    let fake = FakeDiscord::start().await;
    let mut running = start(&fake.gateway.url(), 24, redis, Opts::default());
    running.shutdown.send_replace(true);
    running.join(Duration::from_millis(500)).await;
    assert!(fake
        .gateway
        .try_accept(Duration::from_millis(1500))
        .await
        .is_none());
    assert_eq!(running.handle.session_info(), None);
}

// ---------------------------------------------------------------------------
// Fatal closes, commands, metrics and logs

/// Every close code the runner treats as fatal ends `run()` on its own,
/// marks the snapshot fatal and never reconnects (twilight agrees on the set).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fatal_close_codes_end_the_runner_without_reconnecting() {
    let _metrics = METRICS.write().await;
    let Some(redis) = common::redis(IDLE_DB).await else {
        return;
    };
    let before = shard_events("FatallyClosed");
    let cases = FATAL_CLOSE_CODES.iter().enumerate().map(|(i, &code)| {
        let redis = redis.clone();
        async move {
            let shard = 13 + i as u32;
            let fake = FakeDiscord::start().await;
            let mut running = start(&fake.gateway.url(), shard, redis, Opts::default());
            let conn = identified(&fake, shard, "A", QUIET).await;
            running.recorder.wait_published(1).await;
            conn.close(code, "fatal").await;
            running.join(Duration::from_secs(5)).await;
            let snapshot = running.snapshot();
            assert!(snapshot.fatal, "{code}: {snapshot:?}");
            assert!(!snapshot.active, "{code}");
            assert_eq!(snapshot.status, "FatallyClosed", "{code}");
            assert_eq!(snapshot.close_code, Some(code));
            assert_eq!(fake.gateway.connections(), 1, "{code}: reconnected");
        }
    });
    join_all(cases).await;
    assert_eq!(
        shard_events("FatallyClosed") - before,
        FATAL_CLOSE_CODES.len() as u64
    );
}

/// Commands sent through the shard's `MessageSender` (what the gateway.send
/// consumer uses) reach the gateway: queued until the session is identified,
/// then in order; a reconnect command closes resumably and the shard resumes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gateway_commands_reach_the_gateway() {
    let _metrics = METRICS.read().await;
    let Some(redis) = common::redis(IDLE_DB).await else {
        return;
    };
    let fake = FakeDiscord::start().await;
    let shard = 19;
    let mut running = start(&fake.gateway.url(), shard, redis, Opts::default());
    let sender = running.handle.sender.clone().expect("a sender");

    let members = json!({"op": 8, "d": {"guild_id": "7", "query": "", "limit": 0}});
    sender.send(members.to_string()).unwrap();
    let mut conn = fake.gateway.accept().await;
    conn.hello(QUIET).await;
    // The command waits for the session.
    conn.expect_identify().await;
    conn.ready(1, "A", [shard, TOTAL], &[]).await;
    assert_eq!(conn.expect_op(8).await, members);

    let presence = json!({"op": 3, "d": {"since": null, "activities": [], "status": "idle",
        "afk": false}});
    let voice = json!({"op": 4, "d": {"guild_id": "7", "channel_id": null,
        "self_mute": false, "self_deaf": false}});
    sender.send(presence.to_string()).unwrap();
    sender.send(voice.to_string()).unwrap();
    assert_eq!(conn.expect_op(3).await, presence);
    assert_eq!(conn.expect_op(4).await, voice);

    sender.close(CloseFrame::RESUME).unwrap();
    assert_eq!(conn.expect_close().await, Some(4000));
    let (_conn, resume) = resumed(&fake).await;
    assert_eq!(resume, resume_d("A", 1));
    assert_eq!(running.snapshot().session_id.as_deref(), Some("A"));
    running.stop().await;
}

/// `gateway_shard_events` and the shard log channel over a lifecycle:
/// identify, READY, a resumable close by the gateway, RESUME, RESUMED and a
/// fatal close.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_shard_lifecycle_is_counted_and_logged() {
    let _metrics = METRICS.write().await;
    let Some(redis) = common::redis(IDLE_DB).await else {
        return;
    };
    let fake = FakeDiscord::start().await;
    let shard = 6;
    let proxy = fake.api.proxy().trim_start_matches("http://").to_owned();
    let http = Client::builder()
        .token(TOKEN.to_owned())
        .proxy(proxy, true)
        .build();
    let discord = DiscordLog::new(Arc::new(http), LOG_CHANNEL, 0);
    let kinds = [
        "Connecting",
        "Identifying",
        "Ready",
        "Connected",
        "Disconnected",
        "Reconnecting",
        "Resuming",
        "Resumed",
        "FatallyClosed",
    ];
    let before: Vec<u64> = kinds.iter().map(|kind| shard_events(kind)).collect();
    let opts = Opts {
        discord,
        ..Opts::default()
    };
    let mut running = start(&fake.gateway.url(), shard, redis, opts);

    let conn = identified(&fake, shard, "A", QUIET).await;
    running.recorder.wait_published(1).await;
    conn.close(4000, "").await;
    let (conn, resume) = resumed(&fake).await;
    assert_eq!(resume, resume_d("A", 1));
    conn.resumed(2).await;
    running.recorder.wait_published(2).await;
    conn.close(4014, "Disallowed intent(s)").await;
    running.join(Duration::from_secs(5)).await;

    let deltas: BTreeMap<&str, u64> = kinds
        .iter()
        .zip(&before)
        .map(|(kind, before)| (*kind, shard_events(kind) - before))
        .collect();
    let expected: BTreeMap<&str, u64> = [
        ("Connecting", 2),
        ("Identifying", 1),
        ("Ready", 1),
        ("Connected", 2),
        ("Disconnected", 2),
        ("Reconnecting", 1),
        ("Resuming", 1),
        ("Resumed", 1),
        ("FatallyClosed", 1),
    ]
    .into_iter()
    .collect();
    assert_eq!(deltas, expected);

    let posts = fake.api.wait_posts(LOG_CHANNEL, 6).await;
    let logged: Vec<(String, u64)> = posts
        .iter()
        .map(|post| {
            let embed = &post["embeds"][0];
            (
                embed["title"].as_str().unwrap_or_default().to_owned(),
                embed["color"].as_u64().unwrap_or_default(),
            )
        })
        .collect();
    let title = |what: &str| format!("[Shard {shard}] {what}");
    assert_eq!(
        logged,
        [
            (title("Ready"), u64::from(READY_COLOR)),
            (title("Disconnected"), u64::from(DISCONNECT_COLOR)),
            (title("Resumed"), u64::from(RESUME_COLOR)),
            (title("Connected"), u64::from(CONNECT_COLOR)),
            (title("Disconnected"), u64::from(DISCONNECT_COLOR)),
            (title("Fatally closed"), u64::from(DISCONNECT_COLOR)),
        ]
    );
}

/// A RECONNECT (op 7) or an INVALID_SESSION (op 9) is one reconnect, like a
/// resumable close by the gateway (and like earlier versions counted it).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reconnect_request_counts_one_reconnect() {
    let _metrics = METRICS.write().await;
    let Some(redis) = common::redis(IDLE_DB).await else {
        return;
    };
    let fake = FakeDiscord::start().await;
    let shard = 23;
    let mut running = start(&fake.gateway.url(), shard, redis, Opts::default());
    let mut conn = identified(&fake, shard, "A", QUIET).await;
    running.recorder.wait_published(1).await;
    let (reconnecting, disconnected) = (shard_events("Reconnecting"), shard_events("Disconnected"));

    conn.reconnect().await;
    assert_eq!(conn.expect_close().await, Some(4000));
    let (mut conn, resume) = resumed(&fake).await;
    assert_eq!(resume, resume_d("A", 1));
    conn.resumed(2).await;
    running.recorder.wait_published(2).await;
    assert_eq!(shard_events("Disconnected") - disconnected, 1);
    assert_eq!(shard_events("Reconnecting") - reconnecting, 1);

    conn.invalid_session(false).await;
    assert_eq!(conn.expect_close().await, Some(1000));
    let _conn = identified(&fake, shard, "B", QUIET).await;
    running.recorder.wait_published(3).await;
    assert_eq!(shard_events("Disconnected") - disconnected, 2);
    assert_eq!(shard_events("Reconnecting") - reconnecting, 2);
    running.stop().await;
}

/// As above when the gateway drops the connection instead of echoing
/// twilight's close, which leaves the runner no close to count: the request
/// itself is the one reconnect, and it is not mistaken for the next close.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reconnect_request_counts_one_reconnect_when_the_connection_drops() {
    let _metrics = METRICS.write().await;
    let Some(redis) = common::redis(IDLE_DB).await else {
        return;
    };
    for (shard, op) in [(30, 7), (31, 9)] {
        let fake = FakeDiscord::start().await;
        let relay = Relay::start(&fake.gateway.url()).await;
        let mut running = start(&relay.url(), shard, redis.clone(), Opts::default());
        let mut conn = identified(&fake, shard, "A", QUIET).await;
        running.recorder.wait_published(1).await;
        let reconnecting = shard_events("Reconnecting");

        relay.hold(true);
        if op == 7 {
            conn.reconnect().await;
        } else {
            conn.invalid_session(true).await;
        }
        relay.wait_held().await;
        relay.cut();
        conn.expect_gone().await;
        // READY's resume URL is the gateway's own, not the cut relay.
        let (conn, resume) = resumed(&fake).await;
        assert_eq!(resume, resume_d("A", 1), "op {op}");
        conn.resumed(2).await;
        running.recorder.wait_published(2).await;
        assert_eq!(shard_events("Reconnecting") - reconnecting, 1, "op {op}");
        conn.close(4000, "").await;
        let (_conn, resume) = resumed(&fake).await;
        assert_eq!(resume, resume_d("A", 2), "op {op}");
        assert_eq!(shard_events("Reconnecting") - reconnecting, 2, "op {op}");
        running.stop().await;
    }
}

/// A RECONNECT that arrives while shutting down is no reconnect: the shard
/// stops instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reconnect_request_while_shutting_down_is_not_counted() {
    let _metrics = METRICS.write().await;
    let Some(redis) = common::redis(IDLE_DB).await else {
        return;
    };
    let fake = FakeDiscord::start().await;
    let relay = Relay::start(&fake.gateway.url()).await;
    let shard = 32;
    let mut running = start(&relay.url(), shard, redis, Opts::default());
    let mut conn = identified(&fake, shard, "A", QUIET).await;
    running.recorder.wait_published(1).await;
    let reconnecting = shard_events("Reconnecting");

    relay.hold(true);
    running.shutdown.send_replace(true);
    relay.wait_held().await;
    // Reaches the shard before the answer to its close can.
    conn.reconnect().await;
    relay.hold(false);
    assert_eq!(conn.expect_close().await, Some(4000), "closed resumably");
    running.join(CLOSE_TIMEOUT + Duration::from_secs(2)).await;
    assert_eq!(shard_events("Reconnecting") - reconnecting, 0);
    assert_eq!(fake.gateway.connections(), 1);
}

/// With STATE_ENABLED=false nothing is ever written to Redis: every test on
/// `IDLE_DB` leaves it empty.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_cache_disabled_never_writes_to_redis() {
    let _metrics = METRICS.read().await;
    let Some(mut redis) = common::redis(IDLE_DB).await else {
        return;
    };
    let fake = FakeDiscord::start().await;
    let shard = 25;
    let mut running = start(&fake.gateway.url(), shard, redis.clone(), Opts::default());
    let mut conn = fake.gateway.accept().await;
    conn.hello(QUIET).await;
    conn.expect_identify().await;
    conn.ready(1, "A", [shard, TOTAL], &[(25u64 << 22) + 1])
        .await;
    conn.dispatch("GUILD_CREATE", 2, &common::guild((25u64 << 22) + 1, "g"))
        .await;
    conn.dispatch("MESSAGE_CREATE", 3, &common::message(9, 5, Some(7), "hi"))
        .await;
    running.recorder.wait_published(3).await;
    running.stop().await;
    let keys: u64 = redis::cmd("DBSIZE").query_async(&mut redis).await.unwrap();
    assert_eq!(keys, 0);
}
