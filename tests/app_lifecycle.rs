#![recursion_limit = "256"]

//! The whole service (`app::run_until`) against a fake Discord
//! (`tests/support/fake_discord.rs`), a real Redis and a real LavinMQ (set
//! TEST_REDIS_URL and TEST_AMQP_URL): what startup and shutdown log, saved
//! sessions without a shard range, and shutting down while Redis stalls or
//! refuses writes.
//!
//! The tests pause the whole Redis server (CLIENT PAUSE), change a Redis
//! user, share the `gateway.recv` queue, the process-wide metrics and one
//! log capture, so they run one at a time (`SERIAL`), each on its own
//! database.

mod common;
#[path = "support/fake_discord.rs"]
mod fake_discord;

use common::*;
use fake_discord::{FakeDiscord, GatewayConnection, TIMEOUT};
use lapin::{
    options::{QueueDeclareOptions, QueuePurgeOptions},
    types::FieldTable,
    Connection, ConnectionProperties,
};
use serde_json::json;
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};
use tokio::sync::oneshot;
use twilight_dispatch::{
    app,
    config::Config,
    constants::QUEUE_RECV,
    metrics::{GATEWAY_EVENTS, SHARD_EVENTS},
    models::ApiResult,
};

static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Discord's usual interval: no heartbeat is due during a test.
const HEARTBEAT_MS: u64 = 41_250;

// ---------------------------------------------------------------------------
// Log capture

/// Everything this binary logged at INFO and above.
static LOGS: Mutex<Vec<u8>> = Mutex::new(Vec::new());

struct LogWriter;

impl std::io::Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        LOGS.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The log lines written since `Logs::capture`.
struct Logs(usize);

impl Logs {
    /// Install the capture (the first call does) before the service starts,
    /// so that its INFO lines and spans are formatted too. Without colours,
    /// so a span reads `shard{id=0}:`.
    fn capture() -> Self {
        let _ = tracing_subscriber::fmt()
            .with_writer(|| LogWriter)
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .try_init();
        Self(LOGS.lock().unwrap().len())
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&LOGS.lock().unwrap()[self.0..]).into_owned()
    }
}

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

/// TEST_REDIS_URL with database `db`.
fn redis_url(db: u8) -> String {
    let base = std::env::var("TEST_REDIS_URL").expect("TEST_REDIS_URL");
    format!("{}/{db}", base.trim_end_matches('/'))
}

/// The service's environment: shard 0 of 1 on this fake Discord, Redis at
/// `redis_url`, a fresh metrics port.
fn vars(fake: &FakeDiscord, redis_url: String, amqp_url: &str) -> Vars {
    let mut vars: Vars = [
        ("BOT_TOKEN", "app-lifecycle-token"),
        ("SHARDS_START", "0"),
        ("SHARDS_END", "0"),
        ("SHARDS_TOTAL", "1"),
        ("INTENTS", "513"),
        ("LARGE_THRESHOLD", "100"),
        ("PROMETHEUS_HOST", "127.0.0.1"),
        ("WORKERS", "4"),
    ]
    .into_iter()
    .map(|(name, value)| (name, value.to_owned()))
    .collect();
    vars.extend(fake.env());
    vars.insert("REDIS_URL", redis_url);
    vars.insert("AMQP_URL", amqp_url.to_owned());
    vars.insert("PROMETHEUS_PORT", free_port().to_string());
    vars
}

/// The service, running `app::run_until` until `stop`.
struct App {
    stop: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<ApiResult<()>>,
}

impl App {
    fn start(vars: &Vars) -> Self {
        // main() installs it; twilight's rustls connectors panic without one.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let config = Config::from_lookup(&|name| vars.get(name).cloned()).expect("valid config");
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(app::run_until(config, async move {
            let _ = stopped.await;
        }));
        Self { stop, task }
    }

    /// Trigger the shutdown and wait for `run_until` to return.
    async fn stop(self) -> ApiResult<()> {
        let _ = self.stop.send(());
        tokio::time::timeout(Duration::from_secs(15), self.task)
            .await
            .expect("run_until returned within 15 s")
            .expect("run_until did not panic")
    }
}

/// Poll `done` until it holds; panics after `TIMEOUT`.
async fn eventually(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// How many READYs the shards of this process applied.
fn ready_count() -> u64 {
    SHARD_EVENTS.with_label_values(&["Ready"]).get()
}

/// How many GUILD_CREATEs shard 0 received.
fn guild_creates() -> u64 {
    GATEWAY_EVENTS
        .with_label_values(&["GUILD_CREATE", "0"])
        .get()
}

/// Accept the shard's connection, answer its IDENTIFY with READY (seq 1) and
/// wait until the runner has applied READY to the cache (it counts READY
/// after that), so nothing of the startup still needs Redis.
async fn ready_shard(fake: &FakeDiscord, session: &str) -> GatewayConnection {
    let ready = ready_count();
    let mut conn = fake.gateway.accept().await;
    conn.hello(HEARTBEAT_MS).await;
    conn.expect_identify().await;
    conn.ready(1, session, [0, 1], &[]).await;
    eventually("READY is applied", || ready_count() > ready).await;
    conn
}

/// Empty `gateway.recv`, where the service published the test's events
/// (declared with the service's flags, as tests/amqp.rs does).
async fn purge_published(url: &str) {
    let connection = Connection::connect(url, ConnectionProperties::default())
        .await
        .unwrap();
    let channel = connection.create_channel().await.unwrap();
    channel
        .queue_declare(
            QUEUE_RECV.into(),
            QueueDeclareOptions {
                durable: true,
                ..Default::default()
            },
            FieldTable::default(),
        )
        .await
        .unwrap();
    channel
        .queue_purge(QUEUE_RECV.into(), QueuePurgeOptions::default())
        .await
        .unwrap();
    let _ = connection.close(200, "done".into()).await;
}

/// Pauses every client of the test Redis (CLIENT PAUSE ALL) until dropped,
/// like a Redis busy with a long command. Copied from tests/pipeline.rs: its
/// own blocking connection unpauses from `Drop`, which Redis postpones until
/// the pause is over.
struct Pause(redis::Connection);

impl Pause {
    fn all(duration: Duration) -> Self {
        let url = std::env::var("TEST_REDIS_URL").expect("TEST_REDIS_URL");
        let mut conn = redis::Client::open(url)
            .and_then(|client| client.get_connection())
            .expect("connect to test redis");
        conn.set_read_timeout(Some(duration + Duration::from_secs(5)))
            .unwrap();
        redis::cmd("CLIENT")
            .arg("PAUSE")
            .arg(duration.as_millis() as u64)
            .arg("ALL")
            .query::<()>(&mut conn)
            .expect("pause test redis");
        Self(conn)
    }
}

impl Drop for Pause {
    fn drop(&mut self) {
        let unpaused = redis::cmd("CLIENT").arg("UNPAUSE").query::<()>(&mut self.0);
        if let Err(err) = unpaused {
            eprintln!("CLIENT UNPAUSE failed: {err}");
        }
    }
}

/// A Redis user the service connects as, whose permissions a test changes
/// while the service runs. Deleted again when dropped, from its own blocking
/// connection (like `Pause`).
struct AclUser(redis::Connection);

impl AclUser {
    const NAME: &'static str = "twilight_dispatch_app_lifecycle";
    const PASSWORD: &'static str = "app-lifecycle";

    /// The user, allowed everything.
    fn create() -> Self {
        let url = std::env::var("TEST_REDIS_URL").expect("TEST_REDIS_URL");
        let conn = redis::Client::open(url)
            .and_then(|client| client.get_connection())
            .expect("connect to test redis");
        let mut user = Self(conn);
        let password = format!(">{}", Self::PASSWORD);
        user.set(&["reset", "on", &password, "~*", "&*", "+@all"]);
        user
    }

    /// Apply ACL rules to the user, such as `-@write`.
    fn set(&mut self, rules: &[&str]) {
        let mut command = redis::cmd("ACL");
        command.arg("SETUSER").arg(Self::NAME);
        for rule in rules {
            command.arg(*rule);
        }
        command.query::<()>(&mut self.0).expect("ACL SETUSER");
    }

    /// `REDIS_URL` for database `db`, as this user.
    fn url(db: u8) -> String {
        let base = std::env::var("TEST_REDIS_URL").expect("TEST_REDIS_URL");
        let (scheme, rest) = base.split_once("://").expect("TEST_REDIS_URL is a URL");
        // Without credentials TEST_REDIS_URL itself may carry.
        let host = rest.rsplit_once('@').map_or(rest, |(_, host)| host);
        let host = host.trim_end_matches('/');
        format!("{scheme}://{}:{}@{host}/{db}", Self::NAME, Self::PASSWORD)
    }
}

impl Drop for AclUser {
    fn drop(&mut self) {
        let deleted = redis::cmd("ACL")
            .arg("DELUSER")
            .arg(Self::NAME)
            .query::<()>(&mut self.0);
        if let Err(err) = deleted {
            eprintln!("ACL DELUSER failed: {err}");
        }
    }
}

// ---------------------------------------------------------------------------
// Tests

/// Every log line of a shard's runner task is in a `shard{id=N}` span:
/// twilight-gateway's own lines, which name no shard, as well as the
/// runner's. Startup logs the shard range, shutdown the saved sessions.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_log_lines_of_a_shard_name_it() {
    let _serial = SERIAL.lock().await;
    let Some(_redis) = redis(1).await else {
        return;
    };
    let Some(amqp) = amqp_url() else {
        return;
    };
    let logs = Logs::capture();
    let fake = FakeDiscord::start().await;
    let app = App::start(&vars(&fake, redis_url(1), &amqp));
    let mut conn = ready_shard(&fake, "sess-named").await;
    // twilight-gateway logs an opcode it does not know (at INFO).
    conn.send_text(r#"{"t":null,"s":null,"op":42,"d":null}"#)
        .await;
    eventually("twilight logged the unknown opcode", || {
        logs.text().contains("received an unknown opcode: 42")
    })
    .await;

    app.stop().await.expect("clean shutdown");
    assert_eq!(conn.expect_close().await, Some(4000), "closed resumably");

    let text = logs.text();
    let range = "Starting up 1 shards (0..=0 of 1)";
    assert!(text.contains(range), "{text}");
    for message in [
        "received an unknown opcode: 42",
        "[Shard 0] Hello",
        "[Shard 0] Ready (session: sess-named)",
        "[Shard 0] Shutting down",
    ] {
        let line = text
            .lines()
            .find(|line| line.contains(message))
            .unwrap_or_else(|| panic!("{message:?} was not logged: {text}"));
        assert!(line.contains("shard{id=0}:"), "{line}");
    }
    assert!(text.contains("Saved 1 resumable sessions"), "{text}");
    purge_published(&amqp).await;
}

/// Saved sessions are resumed only for a configured shard range: with
/// Discord's recommended shard count (no `SHARDS_*` variables) the shard
/// identifies, although the saved session would match.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_a_shard_range_saved_sessions_are_not_resumed() {
    let _serial = SERIAL.lock().await;
    let Some(mut store) = redis(2).await else {
        return;
    };
    let Some(amqp) = amqp_url() else {
        return;
    };
    let logs = Logs::capture();
    let fake = FakeDiscord::start().await;
    let saved =
        json!({"session_id": "saved", "sequence": 42, "resume_url": fake.gateway.resume_url()});
    let _: () = redis::pipe()
        .cmd("SET")
        .arg("gateway_shards")
        .arg("1")
        .cmd("SET")
        .arg("gateway_session:0")
        .arg(saved.to_string())
        .query_async(&mut store)
        .await
        .unwrap();

    let mut vars = vars(&fake, redis_url(2), &amqp);
    vars.retain(|name, _| !name.starts_with("SHARDS_"));
    let app = App::start(&vars);
    let mut conn = fake.gateway.accept().await;
    assert!(!conn.is_resume(), "connected to the resume URL");
    conn.hello(HEARTBEAT_MS).await;
    let identify = conn.expect_identify().await;
    assert_eq!(identify["shard"], json!([0, 1]), "{identify}");

    app.stop().await.expect("clean shutdown");
    let text = logs.text();
    assert!(
        text.contains("Using Discord's recommended shard count of 1"),
        "{text}"
    );
    assert!(text.contains("Resuming 0 sessions"), "{text}");
    purge_published(&amqp).await;
}

/// A Redis that stops answering (paused here for longer than the whole
/// shutdown) holds up neither the workers' drain nor the final save past
/// their budgets. The shards still close resumably and in time, and the
/// shutdown ends well within Docker's 10 s grace period.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stalled_redis_cannot_hold_up_the_shutdown() {
    let _serial = SERIAL.lock().await;
    let Some(_redis) = redis(3).await else {
        return;
    };
    let Some(amqp) = amqp_url() else {
        return;
    };
    let logs = Logs::capture();
    let fake = FakeDiscord::start().await;
    let app = App::start(&vars(&fake, redis_url(3), &amqp));
    let mut conn = ready_shard(&fake, "sess-stalled").await;

    let pause = Pause::all(Duration::from_secs(8));
    // A worker takes this guild to the cache, where it waits for Redis. The
    // runner queues it right after counting it.
    let before = guild_creates();
    conn.dispatch("GUILD_CREATE", 2, &guild(2000, "stalled"))
        .await;
    eventually("GUILD_CREATE is received", || guild_creates() > before).await;

    let stopping = Instant::now();
    // Not an error: the shutdown goes on without the drain and the save.
    app.stop().await.expect("clean shutdown");
    let took = stopping.elapsed();
    let text = logs.text();
    let undrained = "Workers did not drain in time; unpublished events will be replayed on resume";
    assert!(text.contains(undrained), "{text}");
    assert!(text.contains("Timed out saving sessions"), "{text}");
    assert!(
        !text.contains("Some shards did not close in time"),
        "{text}"
    );
    assert!(took < Duration::from_secs(10), "shutdown took {took:?}");
    assert_eq!(conn.expect_close().await, Some(4000), "closed resumably");

    drop(pause);
    purge_published(&amqp).await;
}

/// When Redis refuses the final save (here the service's Redis user loses
/// its write permissions), the shutdown logs that and still ends normally.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_final_save_is_logged_and_the_shutdown_ends() {
    let _serial = SERIAL.lock().await;
    let Some(_redis) = redis(4).await else {
        return;
    };
    let Some(amqp) = amqp_url() else {
        return;
    };
    let mut user = AclUser::create();
    let logs = Logs::capture();
    let fake = FakeDiscord::start().await;
    let app = App::start(&vars(&fake, AclUser::url(4), &amqp));
    let mut conn = ready_shard(&fake, "sess-refused").await;

    // From now on Redis answers every write of the service with NOPERM.
    user.set(&["-@write"]);
    // Not an error: the shutdown goes on without the save.
    app.stop().await.expect("clean shutdown");
    let text = logs.text();
    assert!(text.contains("Failed to save sessions: "), "{text}");
    assert!(!text.contains("Timed out saving sessions"), "{text}");
    assert_eq!(conn.expect_close().await, Some(4000), "closed resumably");

    drop(user);
    purge_published(&amqp).await;
}
