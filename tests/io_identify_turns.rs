#![recursion_limit = "256"]

//! More of `gateway::queue::RedisQueue` against a real Redis (set
//! TEST_REDIS_URL): a `max_concurrency` of 0, a holder's key that outlives
//! several holds, the fallback and recovery logs, and the turns of shards
//! that stop waiting or that the local queue gives up on. The constants and
//! helpers are copies of those in tests/identify_queue.rs.

mod common;

use redis::aio::ConnectionManager;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use twilight_dispatch::{
    constants::identify_key,
    gateway::queue::{RedisQueue, IDENTIFY_MARGIN},
};
use twilight_gateway::queue::{InMemoryQueue, Queue, IDENTIFY_DELAY};

/// The fallback test makes the whole server refuse writes and the others
/// measure time, so the tests run one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The injected interval between two identifies of one bucket.
const INTERVAL: Duration = Duration::from_millis(300);
/// How long a taken bucket stays taken.
const HOLD: Duration = INTERVAL.saturating_add(IDENTIFY_MARGIN);
/// For a loaded machine: how much later than due a grant may come.
const SLACK: Duration = Duration::from_millis(1500);
const DAY: Duration = Duration::from_secs(86_400);

/// When the shard may identify. Panics if the queue gave up on the request
/// (which makes a shard enqueue again) or did not answer within `limit`.
async fn granted(rx: oneshot::Receiver<()>, limit: Duration) -> Instant {
    tokio::time::timeout(limit, rx)
        .await
        .unwrap_or_else(|_| panic!("not granted within {limit:?}"))
        .expect("the queue kept the request");
    Instant::now()
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

/// The value of the bucket's key: who took it last.
async fn owner(conn: &mut ConnectionManager, key: &str) -> Option<String> {
    redis::cmd("GET").arg(key).query_async(conn).await.unwrap()
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

/// What this test binary logged, from INFO up.
static LOGS: std::sync::Mutex<Vec<u8>> = std::sync::Mutex::new(Vec::new());

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

/// Collect logs in [`LOGS`]; returns how many bytes there already are, so
/// a test can read only its own.
fn capture_logs() -> usize {
    let _ = tracing_subscriber::fmt()
        .with_writer(|| LogWriter)
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .try_init();
    LOGS.lock().unwrap().len()
}

fn logs_since(start: usize) -> String {
    String::from_utf8_lossy(&LOGS.lock().unwrap()[start..]).into_owned()
}

/// Discord's `max_concurrency` is at least 1, and a 0 (from a proxy in
/// front of Discord, say) counts as 1. Taken as it is, the bucket of every
/// shard would be `shard % 0`, and the queue would never let one through.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_max_concurrency_of_zero_counts_as_one() {
    let _serial = SERIAL.lock().await;
    let Some(mut redis) = common::redis(1).await else {
        return;
    };
    let local = InMemoryQueue::new(1, 1000, DAY, 1000);
    let queue = RedisQueue::with_interval(local, 0, redis.clone(), INTERVAL);
    granted(queue.enqueue(7), Duration::from_secs(5)).await;

    // Shard 7 is in bucket 0, the only one, and took it for one hold.
    let left = pttl(&mut redis, &identify_key(0)).await;
    assert!(
        left > 0 && left <= HOLD.as_millis() as i64,
        "gateway_identify:0 expires in {left} ms"
    );
}

/// The holder's key decides how long its bucket stays taken, also when it
/// lives for several of this process' holds (written by a version with a
/// longer interval): the shard asks again at most one hold apart until the
/// key expired, rather than identifying after the first hold.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_holder_key_longer_than_one_hold_is_waited_out() {
    let _serial = SERIAL.lock().await;
    let Some(mut redis) = common::redis(2).await else {
        return;
    };
    let lives = HOLD * 3;
    let key = identify_key(0);
    let started = Instant::now();
    set(&mut redis, &key, "longer hold", Some(lives)).await;
    let local = InMemoryQueue::new(1, 1000, DAY, 1000);
    let queue = RedisQueue::with_interval(local, 1, redis.clone(), INTERVAL);
    let waited = granted(queue.enqueue(0), Duration::from_secs(10)).await - started;

    assert!(
        waited >= lives - Duration::from_millis(50),
        "identified {waited:?} after a {lives:?} key was set"
    );
    assert!(
        waited < lives + SLACK,
        "waited {waited:?} for a {lives:?} key"
    );
    assert_ne!(
        owner(&mut redis, &key).await.as_deref(),
        Some("longer hold"),
        "the bucket is taken again by the identify"
    );
}

/// While Redis fails, the fallback is logged once, not once per identify;
/// once Redis answers again, that is logged once too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_fallback_is_logged_once_per_outage_and_the_recovery_once() {
    let _serial = SERIAL.lock().await;
    let logged = capture_logs();
    let Some(redis) = common::redis(3).await else {
        return;
    };
    // Three buckets, locally and at Discord, so no shard waits for another's
    // turn. The local queue lets shards 0 and 1 through at once, and shard 2,
    // enqueued after that, at its next tick 5 s later (a shard enqueued just
    // after a tick waits for the next one too, hence the limits).
    let local = InMemoryQueue::new(3, 1000, DAY, 1000);
    let queue = RedisQueue::with_interval(local, 3, redis, INTERVAL);
    let full = Full::new();
    let failing = [queue.enqueue(0), queue.enqueue(1)];
    for request in failing {
        granted(request, IDENTIFY_DELAY * 2 + SLACK).await;
    }
    drop(full);
    granted(queue.enqueue(2), IDENTIFY_DELAY * 2 + SLACK).await;

    let logs = logs_since(logged);
    let count = |message: &str| logs.matches(message).count();
    assert_eq!(
        count("Cannot take turns identifying with other processes through Redis"),
        1,
        "{logs}"
    );
    assert_eq!(
        count("Identifies take turns with other processes through Redis again"),
        1,
        "{logs}"
    );
}

/// A shard that stops waiting (its connection ended) while it waits for
/// another process' key hands its turn to the next shard of the bucket in
/// this process unused: the next one identifies once the key expired, not a
/// hold after a turn the stopped shard never took.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shard_that_stops_waiting_hands_its_turn_to_the_next() {
    let _serial = SERIAL.lock().await;
    let Some(mut redis) = common::redis(4).await else {
        return;
    };
    // The hold (9.5 s) is longer than the key lives (6 s): a turn that the
    // stopped shard took, or one counted from before it waited, would keep
    // the next shard waiting well after the key expired.
    let lives = Duration::from_secs(6);
    let key = identify_key(0);
    let started = Instant::now();
    set(&mut redis, &key, "another process", Some(lives)).await;
    let local = InMemoryQueue::new(1, 1000, DAY, 1000);
    let queue = RedisQueue::with_interval(local, 1, redis.clone(), Duration::from_secs(9));
    // The local queue lets the first through at once (it then waits for the
    // key) and the second 5 s later (it then waits for the first's turn).
    let first = queue.enqueue(0);
    let second = queue.enqueue(1);
    tokio::time::sleep(IDENTIFY_DELAY + Duration::from_millis(500)).await;
    drop(first);

    let waited = granted(second, Duration::from_secs(15)).await - started;
    assert!(
        waited >= lives - Duration::from_millis(50),
        "identified {waited:?} after a {lives:?} key was set"
    );
    assert!(
        waited < lives + SLACK,
        "waited {waited:?}: the turn was not handed over"
    );
    assert_ne!(
        owner(&mut redis, &key).await.as_deref(),
        Some("another process"),
        "the bucket is taken again by the identify"
    );
}

/// A shard the local queue gives up on (here: the queue was dropped before
/// it let the shard through) is not let through without it either: it is
/// answered with an error at once, which makes the shard enqueue again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shard_the_local_queue_gives_up_on_is_not_let_through() {
    let _serial = SERIAL.lock().await;
    let Some(redis) = common::redis(5).await else {
        return;
    };
    let local = InMemoryQueue::new(1, 1000, DAY, 1000);
    let queue = RedisQueue::with_interval(local, 1, redis, INTERVAL);
    let first = queue.enqueue(0);
    let second = queue.enqueue(1);
    granted(first, Duration::from_secs(5)).await;
    // The local queue would let the second through 5 s after the first. It
    // stops with its last handle, dropping the requests it holds.
    drop(queue);

    let answer = tokio::time::timeout(Duration::from_secs(2), second)
        .await
        .expect("answered within 2 s, not at the local queue's next turn");
    assert!(answer.is_err(), "let through without the local queue");
}
