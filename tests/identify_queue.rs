#![recursion_limit = "256"]

//! Identifies take turns across processes (`gateway::queue::RedisQueue`),
//! against a real Redis (set TEST_REDIS_URL; the last test also runs the
//! whole service and needs TEST_AMQP_URL).
//!
//! Each `RedisQueue` here stands for one process of the bot. They take turns
//! with a short injected interval, so the tests need not wait Discord's 5 s
//! per identify; the local queue (`InMemoryQueue`) keeps its own 5 s, so only
//! the tests of one process' turns identify twice in one local bucket, with
//! Discord's interval.

mod common;
#[path = "support/fake_discord.rs"]
mod fake_discord;

use fake_discord::{FakeDiscord, Options};
use redis::aio::ConnectionManager;
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::oneshot::{self, error::TryRecvError},
    task::JoinHandle,
};
use twilight_dispatch::{
    app,
    config::Config,
    constants::identify_key,
    gateway::{
        self,
        queue::{RedisQueue, IDENTIFY_MARGIN, REDIS_TIMEOUT},
    },
    redis_conn,
};
use twilight_gateway::queue::{InMemoryQueue, Queue, IDENTIFY_DELAY};

/// The pause test holds back every client of the server, so the tests run
/// one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The injected interval between two identifies of one bucket.
const INTERVAL: Duration = Duration::from_millis(300);
/// How long a taken bucket stays taken.
const HOLD: Duration = INTERVAL.saturating_add(IDENTIFY_MARGIN);
/// For a loaded machine: how much later than due a grant may come.
const SLACK: Duration = Duration::from_millis(1500);
const DAY: Duration = Duration::from_secs(86_400);

/// One process' queue: a local queue as `gateway::create` builds it from
/// Discord's session start limit, taking turns through `redis`.
fn process(redis: &ConnectionManager, max_concurrency: u16) -> RedisQueue {
    let local = InMemoryQueue::new(max_concurrency, 1000, DAY, 1000);
    RedisQueue::with_interval(local, max_concurrency, redis.clone(), INTERVAL)
}

/// When the shard may identify. Panics if the queue gave up on the request
/// (which makes a shard enqueue again) or never answered.
async fn granted(rx: oneshot::Receiver<()>) -> Instant {
    tokio::time::timeout(Duration::from_secs(10), rx)
        .await
        .expect("granted within 10 s")
        .expect("the queue kept the request");
    Instant::now()
}

async fn granted_all(requests: Vec<oneshot::Receiver<()>>) -> Vec<Instant> {
    let mut times = futures_util::future::join_all(requests.into_iter().map(granted)).await;
    times.sort();
    times
}

async fn pttl(conn: &mut ConnectionManager, key: &str) -> i64 {
    redis::cmd("PTTL").arg(key).query_async(conn).await.unwrap()
}

async fn set(conn: &mut ConnectionManager, key: &str, value: &str, px: Option<Duration>) {
    let mut cmd = redis::cmd("SET");
    cmd.arg(key).arg(value);
    if let Some(px) = px {
        cmd.arg("PX").arg(px.as_millis() as u64);
    }
    cmd.query_async::<()>(conn).await.unwrap();
}

// ---------------------------------------------------------------------------
// Taking turns

/// Processes that start together identify one after the other: with
/// `max_concurrency` 1 every shard is in bucket 0, and no two of them get to
/// identify within the interval.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn processes_take_turns_in_one_bucket() {
    let _serial = SERIAL.lock().await;
    let Some(mut redis) = common::redis(1).await else {
        return;
    };
    let processes: Vec<RedisQueue> = (0..3).map(|_| process(&redis, 1)).collect();
    let started = Instant::now();
    let requests = processes
        .iter()
        .zip(0..)
        .map(|(queue, shard)| queue.enqueue(shard))
        .collect();
    let times = granted_all(requests).await;

    for pair in times.windows(2) {
        let apart = pair[1] - pair[0];
        assert!(apart >= INTERVAL, "two identifies {apart:?} apart");
    }
    let last = times[2] - started;
    assert!(last < HOLD * 2 + SLACK, "the third waited {last:?}");
    // The bucket's key is taken for one hold, by the last one to identify.
    let left = pttl(&mut redis, &identify_key(0)).await;
    assert!(
        left > 0 && left <= HOLD.as_millis() as i64,
        "gateway_identify:0 expires in {left} ms"
    );
}

/// Discord's buckets are `shard % max_concurrency`: shards 0 and 16 of
/// different processes take turns with 16, shards 0 and 1 do not.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_shards_of_one_discord_bucket_take_turns() {
    let _serial = SERIAL.lock().await;
    let Some(mut redis) = common::redis(2).await else {
        return;
    };
    let (a, b) = (process(&redis, 16), process(&redis, 16));
    let times = granted_all(vec![a.enqueue(0), b.enqueue(16)]).await;
    let apart = times[1] - times[0];
    assert!(apart >= INTERVAL, "shards 0 and 16: {apart:?} apart");

    let _: () = redis::cmd("FLUSHDB").query_async(&mut redis).await.unwrap();
    let (a, b) = (process(&redis, 16), process(&redis, 16));
    // Relative to each other: setup (script loading) on a loaded machine
    // must not count.
    let times = granted_all(vec![a.enqueue(0), b.enqueue(1)]).await;
    let apart = times[0].max(times[1]) - times[0].min(times[1]);
    assert!(apart < INTERVAL, "shards 0 and 1: {apart:?} apart");
    assert!(pttl(&mut redis, &identify_key(0)).await > 0);
    assert!(pttl(&mut redis, &identify_key(1)).await > 0);
}

// ---------------------------------------------------------------------------
// Crash safety

/// A process that crashed right after taking a bucket never releases it; its
/// key expires and the next process identifies then.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crashed_holders_bucket_frees_when_its_key_expires() {
    let _serial = SERIAL.lock().await;
    let Some(mut redis) = common::redis(4).await else {
        return;
    };
    let key = identify_key(0);
    let started = Instant::now();
    set(&mut redis, &key, "crashed", Some(HOLD)).await;
    let at = granted(process(&redis, 1).enqueue(0)).await;

    let waited = at - started;
    assert!(
        waited >= INTERVAL,
        "identified while the holder's key lived ({waited:?})"
    );
    assert!(
        waited < HOLD + SLACK,
        "waited {waited:?} for a {HOLD:?} key"
    );
    let owner: String = redis::cmd("GET")
        .arg(&key)
        .query_async(&mut redis)
        .await
        .unwrap();
    assert_ne!(
        owner, "crashed",
        "the bucket is taken again by the identify"
    );
}

/// A key without an expiry (written by hand, not by the queue) would block
/// its bucket for good; it gets one hold.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_key_without_an_expiry_blocks_its_bucket_for_one_hold() {
    let _serial = SERIAL.lock().await;
    let Some(mut redis) = common::redis(5).await else {
        return;
    };
    let started = Instant::now();
    set(&mut redis, &identify_key(0), "by hand", None).await;
    let waited = granted(process(&redis, 1).enqueue(0)).await - started;
    assert!(waited < HOLD + SLACK, "waited {waited:?}");
}

/// A shard whose connection ended while it waited for the bucket does not
/// take it: the next shard (of any process) is not held back for an identify
/// that never happens.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shard_that_stops_waiting_leaves_the_bucket_free() {
    let _serial = SERIAL.lock().await;
    let Some(mut redis) = common::redis(6).await else {
        return;
    };
    let key = identify_key(0);
    set(&mut redis, &key, "holder", Some(HOLD)).await;
    let queue = process(&redis, 1);
    let request = queue.enqueue(0);
    // The local queue lets it through at once; then it waits for the bucket.
    tokio::time::sleep(INTERVAL / 3).await;
    drop(request);

    tokio::time::sleep(HOLD + Duration::from_millis(300)).await;
    assert_eq!(pttl(&mut redis, &key).await, -2, "the bucket was taken");
}

// ---------------------------------------------------------------------------
// Redis unreachable, slow or failing

/// A TCP proxy in front of the test Redis that can be taken down: it then
/// drops its connections and refuses new ones, like an unreachable Redis. It
/// can also go silent, like a network that loses every packet: what clients
/// send is dropped, so Redis neither runs nor answers it.
struct Proxy {
    addr: SocketAddr,
    accept: JoinHandle<()>,
    connections: Arc<Mutex<Vec<JoinHandle<()>>>>,
    silent: Arc<AtomicBool>,
}

impl Proxy {
    async fn start() -> Self {
        let url = std::env::var("TEST_REDIS_URL").expect("TEST_REDIS_URL");
        let target = url
            .trim_start_matches("redis://")
            .split('/')
            .next()
            .unwrap()
            .to_owned();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let connections = Arc::new(Mutex::new(Vec::new()));
        let silent = Arc::new(AtomicBool::new(false));
        let accept = {
            let connections = connections.clone();
            let silent = silent.clone();
            tokio::spawn(async move {
                while let Ok((client, _)) = listener.accept().await {
                    let target = target.clone();
                    let silent = silent.clone();
                    let connection = tokio::spawn(async move {
                        if let Ok(server) = TcpStream::connect(target).await {
                            forward(client, server, &silent).await;
                        }
                    });
                    connections.lock().unwrap().push(connection);
                }
            })
        };
        Self {
            addr,
            accept,
            connections,
            silent,
        }
    }

    async fn connect(&self, db: u8) -> ConnectionManager {
        redis_conn::connect(&format!("redis://{}/{db}", self.addr))
            .await
            .expect("connect through the proxy")
    }

    fn silence(&self, silent: bool) {
        self.silent.store(silent, Ordering::SeqCst);
    }

    /// Closes every connection but keeps accepting new ones, so clients
    /// reconnect, as after a network outage. Returns once they are closed.
    async fn reset(&self) {
        let connections = std::mem::take(&mut *self.connections.lock().unwrap());
        for connection in connections {
            connection.abort();
            let _ = connection.await;
        }
    }

    /// Returns once the listener and every connection are closed.
    async fn take_down(mut self) {
        self.accept.abort();
        let _ = (&mut self.accept).await;
        self.reset().await;
    }
}

/// Copies between `client` and `server` until either closes, dropping what
/// the client sends while `silent` is set.
async fn forward(client: TcpStream, server: TcpStream, silent: &AtomicBool) {
    let (mut from_client, mut to_client) = client.into_split();
    let (mut from_server, mut to_server) = server.into_split();
    let up = async {
        let mut buf = vec![0; 16 * 1024];
        while let Ok(read @ 1..) = from_client.read(&mut buf).await {
            if !silent.load(Ordering::SeqCst) && to_server.write_all(&buf[..read]).await.is_err() {
                break;
            }
        }
    };
    let down = tokio::io::copy(&mut from_server, &mut to_client);
    tokio::select! {
        () = up => {}
        _ = down => {}
    }
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

/// Identifies never wait for an unreachable Redis longer than
/// `REDIS_TIMEOUT`; they go ahead with the local queue only.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn identifies_go_ahead_while_redis_is_unreachable() {
    let _serial = SERIAL.lock().await;
    let Some(_) = common::redis(7).await else {
        return;
    };
    let proxy = Proxy::start().await;
    let redis = proxy.connect(7).await;
    proxy.take_down().await;

    // Twice: also once the connection is known to be gone and reconnecting.
    // New processes each time, since a local queue grants a second shard
    // only 5 s after the first.
    for round in 0..2 {
        let queues = [process(&redis, 1), process(&redis, 1)];
        let started = Instant::now();
        let requests = queues
            .iter()
            .zip(0..)
            .map(|(queue, shard)| queue.enqueue(shard))
            .collect();
        let times = granted_all(requests).await;
        let waited = times[1] - started;
        assert!(
            waited < REDIS_TIMEOUT + SLACK,
            "round {round}: waited {waited:?}"
        );
    }
}

/// The same for a Redis that accepts commands but does not answer them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn identifies_go_ahead_while_redis_does_not_answer() {
    let _serial = SERIAL.lock().await;
    let Some(redis) = common::redis(8).await else {
        return;
    };
    let queue = process(&redis, 1);
    // Longer than the connection's own response timeout (5 s), which must
    // not be what ends the wait.
    let pause = Pause::all(REDIS_TIMEOUT + Duration::from_secs(4));
    let started = Instant::now();
    let waited = granted(queue.enqueue(0)).await - started;
    drop(pause);
    assert!(waited < REDIS_TIMEOUT + SLACK, "waited {waited:?}");
}

/// Makes the test Redis refuse every write until dropped, as a full Redis
/// with the default `noeviction` policy does (`maxmemory` of 1 byte).
struct Full {
    conn: redis::Connection,
    restore: Vec<(&'static str, String)>,
}

impl Full {
    fn new() -> Self {
        let url = std::env::var("TEST_REDIS_URL").expect("TEST_REDIS_URL");
        let mut conn = redis::Client::open(url)
            .and_then(|client| client.get_connection())
            .expect("connect to test redis");
        let mut restore = Vec::new();
        for (name, value) in [("maxmemory-policy", "noeviction"), ("maxmemory", "1")] {
            let current: Vec<String> = redis::cmd("CONFIG")
                .arg("GET")
                .arg(name)
                .query(&mut conn)
                .unwrap();
            restore.push((name, current[1].clone()));
            redis::cmd("CONFIG")
                .arg("SET")
                .arg(name)
                .arg(value)
                .query::<()>(&mut conn)
                .unwrap();
        }
        Self { conn, restore }
    }
}

impl Drop for Full {
    fn drop(&mut self) {
        for (name, value) in self.restore.iter().rev() {
            let restored = redis::cmd("CONFIG")
                .arg("SET")
                .arg(*name)
                .arg(value)
                .query::<()>(&mut self.conn);
            if let Err(err) = restored {
                eprintln!("CONFIG SET {name} {value} failed: {err}");
            }
        }
    }
}

/// A Redis that answers with an error (here: full, so it refuses to take the
/// bucket) is not asked again and again: the shard identifies at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn identifies_go_ahead_at_once_when_redis_answers_with_an_error() {
    let _serial = SERIAL.lock().await;
    let Some(redis) = common::redis(11).await else {
        return;
    };
    let queue = process(&redis, 1);
    let full = Full::new();
    let started = Instant::now();
    let waited = granted(queue.enqueue(0)).await - started;
    drop(full);
    assert!(waited < REDIS_TIMEOUT / 2, "waited {waited:?}");
}

// ---------------------------------------------------------------------------
// One process' turns, with Discord's interval
//
// The local queue lets the shards of a bucket through 5 s apart, counted from
// when it let the previous one through, before that one waited for Redis.

/// One process' queue with Discord's interval, `max_concurrency` 1.
fn process_with_discords_interval(redis: ConnectionManager) -> RedisQueue {
    RedisQueue::new(InMemoryQueue::new(1, 1000, DAY, 1000), 1, redis)
}

/// A shard that waited `REDIS_TIMEOUT` for a Redis that does not answer
/// identifies 2 s late. The next one is let through 3 s after that, and must
/// not identify then just because Redis answers again by now.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_process_identifies_apart_when_redis_stops_answering_for_a_while() {
    let _serial = SERIAL.lock().await;
    let Some(_) = common::redis(12).await else {
        return;
    };
    let proxy = Proxy::start().await;
    let queue = process_with_discords_interval(proxy.connect(12).await);
    proxy.silence(true);
    let started = Instant::now();
    let (first, second) = (queue.enqueue(0), queue.enqueue(1));
    let first = granted(first).await;
    proxy.silence(false);
    // Into a new connection: the old one waits for the answers Redis never
    // got the commands for.
    proxy.reset().await;
    let second = granted(second).await;

    let apart = second - first;
    assert!(apart >= IDENTIFY_DELAY, "identified {apart:?} apart");
    let waited = second - started;
    assert!(
        waited < REDIS_TIMEOUT + IDENTIFY_DELAY + IDENTIFY_MARGIN + SLACK,
        "the second waited {waited:?}"
    );
}

/// The first shard waits for another process' key until after the local
/// queue let the second one through, and Redis starts refusing to take the
/// bucket meanwhile: the second shard still identifies after the first, one
/// interval later, not at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_process_identifies_in_turn_when_redis_fails_while_they_wait() {
    let _serial = SERIAL.lock().await;
    let Some(mut redis) = common::redis(13).await else {
        return;
    };
    let hold = IDENTIFY_DELAY + IDENTIFY_MARGIN;
    set(&mut redis, &identify_key(0), "another process", Some(hold)).await;
    let queue = process_with_discords_interval(redis);
    let (first, mut second) = (queue.enqueue(0), queue.enqueue(1));
    tokio::time::sleep(IDENTIFY_DELAY / 2).await;
    let full = Full::new();
    let first = granted(first).await;
    let early = second.try_recv();
    let second = match early {
        Err(TryRecvError::Empty) => granted(second).await,
        _ => panic!("the second shard identified first ({early:?})"),
    };
    drop(full);

    let apart = second - first;
    assert!(apart >= IDENTIFY_DELAY, "identified {apart:?} apart");
}

// ---------------------------------------------------------------------------
// The local queue

/// The wrapped local queue still keeps Discord's daily session start limit:
/// with one identify left, the second shard waits for the reset although its
/// bucket in Redis is free.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_daily_limit_still_holds() {
    let _serial = SERIAL.lock().await;
    let Some(mut redis) = common::redis(9).await else {
        return;
    };
    let local = InMemoryQueue::new(2, 1, DAY, 1);
    let queue = RedisQueue::with_interval(local, 2, redis.clone(), INTERVAL);
    let first = queue.enqueue(0);
    let mut second = queue.enqueue(1);
    granted(first).await;

    let early = tokio::time::timeout(HOLD + Duration::from_millis(500), &mut second).await;
    assert!(early.is_err(), "identified past the daily limit");
    assert_eq!(pttl(&mut redis, &identify_key(1)).await, -2);
}

/// A shard whose connection ended while it waited for the local queue gives
/// its turn and its daily permit to the next shard, as with the local queue
/// alone: with two identifies left, the third shard identifies at the second
/// shard's turn (the local queue's next 5 s tick).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shard_that_stops_waiting_gives_its_local_turn_to_the_next() {
    let _serial = SERIAL.lock().await;
    let Some(redis) = common::redis(3).await else {
        return;
    };
    let local = InMemoryQueue::new(1, 2, DAY, 2);
    let queue = RedisQueue::with_interval(local, 1, redis.clone(), INTERVAL);
    let started = Instant::now();
    granted(queue.enqueue(0)).await;
    drop(queue.enqueue(1));
    let third = queue.enqueue(2);

    let waited = granted(third).await - started;
    assert!(waited < IDENTIFY_DELAY + SLACK, "waited {waited:?}");
}

// ---------------------------------------------------------------------------
// The service

/// The environment of a process running shards 0 and 1 of 2, plus `overrides`.
fn service_vars(
    fake: &FakeDiscord,
    overrides: &[(&'static str, &str)],
) -> HashMap<&'static str, String> {
    let mut vars: HashMap<&'static str, String> = [
        ("BOT_TOKEN", "identify-token"),
        ("SHARDS_START", "0"),
        ("SHARDS_END", "1"),
        ("SHARDS_TOTAL", "2"),
        ("INTENTS", "513"),
        ("PROMETHEUS_HOST", "127.0.0.1"),
        ("PROMETHEUS_PORT", "0"),
    ]
    .into_iter()
    .map(|(name, value)| (name, value.to_owned()))
    .collect();
    vars.extend(fake.env());
    for &(name, value) in overrides {
        vars.insert(name, value.to_owned());
    }
    vars
}

/// The queue gets Discord's `max_concurrency`, not the local queue's one that
/// SHARDS_CONCURRENCY lowered: Discord's buckets are the same for every
/// process, so shard 8 of a process that lowered it to 8 (its local bucket 0)
/// still takes turns with shard 8 of one that did not.
#[tokio::test]
async fn the_queue_gets_discords_max_concurrency() {
    // main() installs it; twilight's rustls connectors panic without one.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut options = Options::default();
    options.session_start_limit.max_concurrency = 16;
    let fake = FakeDiscord::with_options(options).await;
    let vars = service_vars(
        &fake,
        &[
            ("SHARDS_CONCURRENCY", "8"),
            ("REDIS_URL", "redis://127.0.0.1:6379"),
            ("AMQP_URL", "amqp://127.0.0.1:5672/%2f"),
        ],
    );
    let config = Config::from_lookup(&|name| vars.get(name).cloned()).expect("valid config");
    let http = app::http_client(&config);
    let mut given = None;
    // Nothing connects: shards only do once they are polled.
    let created = gateway::create_with_queue(&config, &http, HashMap::new(), |local, max| {
        given = Some(max);
        local
    })
    .await
    .expect("create the shards");
    assert_eq!(given, Some(16));
    assert_eq!(created.shards.len(), 2);
}

/// Two processes of the bot (shards 0 and 1 of 2, `max_concurrency` 1)
/// started together identify Discord's 5 s apart. Each used to identify at
/// once with its own queue, and Discord answered one of them with an invalid
/// session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn processes_of_the_service_identify_one_after_the_other() {
    let _serial = SERIAL.lock().await;
    let Some(_) = common::redis(10).await else {
        return;
    };
    let Some(amqp) = common::amqp_url() else {
        return;
    };
    // main() installs it; twilight's rustls connectors panic without one.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let fake = FakeDiscord::with_options(Options {
        shards: 2,
        ..Options::default()
    })
    .await;
    let redis = format!(
        "{}/10",
        std::env::var("TEST_REDIS_URL")
            .unwrap()
            .trim_end_matches('/')
    );
    let mut apps = Vec::new();
    for shard in ["0", "1"] {
        let vars = service_vars(
            &fake,
            &[
                ("SHARDS_START", shard),
                ("SHARDS_END", shard),
                ("REDIS_URL", &redis),
                ("AMQP_URL", &amqp),
            ],
        );
        let config = Config::from_lookup(&|name| vars.get(name).cloned()).expect("valid config");
        let (stop, stopped) = oneshot::channel::<()>();
        let task = tokio::spawn(app::run_until(config, async move {
            let _ = stopped.await;
        }));
        apps.push((stop, task));
    }

    let mut connections = Vec::new();
    for _ in 0..2 {
        let conn = fake.gateway.accept().await;
        conn.hello(41_250).await;
        connections.push(conn);
    }
    let identified =
        futures_util::future::join_all(connections.iter_mut().map(|conn| async move {
            let identify = conn.expect_identify().await;
            (Instant::now(), identify["shard"][0].as_u64().unwrap())
        }))
        .await;
    let mut shards: Vec<u64> = identified.iter().map(|(_, shard)| *shard).collect();
    shards.sort();
    assert_eq!(shards, [0, 1]);
    let apart = identified[0].0.max(identified[1].0) - identified[0].0.min(identified[1].0);
    assert!(
        apart >= IDENTIFY_DELAY,
        "the processes identified {apart:?} apart"
    );

    for (stop, task) in apps {
        let _ = stop.send(());
        tokio::time::timeout(Duration::from_secs(15), task)
            .await
            .expect("run_until returned within 15 s")
            .expect("run_until did not panic")
            .expect("clean shutdown");
    }
}
