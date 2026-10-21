#![recursion_limit = "256"]

//! The status job (`sessions::run_job`) and `gateway_statuses`, which every
//! process merges its own shards into: the entries of other processes' shards
//! are kept and a value the job cannot read is replaced (against a real
//! Redis, set TEST_REDIS_URL), and while Redis fails the job carries on and
//! throttles its warnings.

mod common;

use common::*;
use redis::aio::{ConnectionLike, ConnectionManager};
use serde_json::{json, Value};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use twilight_dispatch::gateway::{
    registry::{Registry, ShardHandle},
    sessions,
};

const STATUSES: &str = "gateway_statuses";

/// A `last_ack` exactly as `gateway_statuses` stores it, so an entry that is
/// read and written back stays byte for byte the same.
const LAST_ACK: &str = "2026-10-20T12:30:04.836086";

/// What other processes left in the list (shards 5 and 2), with a stale entry
/// of shard 0, unsorted.
const SEEDED: &str = r#"[
    {"shard": 5, "status": "Connected", "latency": 7, "last_ack": "2026-10-20T12:30:04.836086"},
    {"shard": 0, "status": "Resuming", "latency": 7, "last_ack": "2026-10-20T12:30:04.836086"},
    {"shard": 2, "status": "Identifying", "latency": 7, "last_ack": "2026-10-20T12:30:04.836086"}
]"#;

/// An entry of another process, as `SEEDED` holds it.
fn other(shard: u64, status: &str) -> Value {
    json!({"shard": shard, "status": status, "latency": 7, "last_ack": LAST_ACK})
}

async fn set(conn: &mut ConnectionManager, key: &str, value: &str) {
    let _: () = redis::cmd("SET")
        .arg(key)
        .arg(value)
        .query_async(conn)
        .await
        .unwrap();
}

/// The JSON stored at `key` once `check` accepts it. A missing key, a value
/// that is not JSON and a key of another type (GET fails) count as not yet.
async fn wait_for(
    conn: &mut ConnectionManager,
    key: &str,
    check: impl Fn(&Value) -> bool,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let raw: Option<String> = redis::cmd("GET")
            .arg(key)
            .query_async(conn)
            .await
            .unwrap_or_default();
        if let Some(value) = raw.and_then(|raw| serde_json::from_str(&raw).ok()) {
            if check(&value) {
                return value;
            }
        }
        assert!(Instant::now() < deadline, "{key} never matched");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The entry of `shard` in a `gateway_statuses` list.
fn status_of(statuses: &Value, shard: u64) -> Option<&Value> {
    statuses
        .as_array()?
        .iter()
        .find(|status| status["shard"] == shard)
}

/// The shards of a `gateway_statuses` list, in stored order.
fn shards(statuses: &Value) -> Vec<u64> {
    statuses
        .as_array()
        .expect("a list")
        .iter()
        .map(|status| status["shard"].as_u64().expect("a shard id"))
        .collect()
}

/// Processes that run different shard ranges share `gateway_statuses`: every
/// run replaces this process' entries only, keeps the other processes' ones
/// exactly as they were and keeps the list sorted by shard.
#[tokio::test]
async fn the_status_job_keeps_other_processes_shards() {
    let Some(mut conn) = redis(1).await else {
        return;
    };
    set(&mut conn, STATUSES, SEEDED).await;

    let handle = Arc::new(ShardHandle::new(0, None, Some(1)));
    handle.update(|s| {
        s.status = "Connected";
        s.latency_ms = 12;
    });
    let job = tokio::spawn(sessions::run_job(
        conn.clone(),
        Arc::new(Registry::new([handle.clone()])),
    ));

    let statuses = wait_for(&mut conn, STATUSES, |v| {
        status_of(v, 0).is_some_and(|s| s["status"] == "Connected")
    })
    .await;
    assert_eq!(shards(&statuses), [0, 2, 5], "{statuses}");
    assert_eq!(statuses[0]["latency"], 12);
    assert_eq!(status_of(&statuses, 2), Some(&other(2, "Identifying")));
    assert_eq!(status_of(&statuses, 5), Some(&other(5, "Connected")));

    // The next run merges again, and the other entries still stay as they were.
    handle.update(|s| s.status = "Disconnected");
    let statuses = wait_for(&mut conn, STATUSES, |v| {
        status_of(v, 0).is_some_and(|s| s["status"] == "Disconnected")
    })
    .await;
    assert_eq!(shards(&statuses), [0, 2, 5], "{statuses}");
    assert_eq!(status_of(&statuses, 2), Some(&other(2, "Identifying")));
    assert_eq!(status_of(&statuses, 5), Some(&other(5, "Connected")));
    job.abort();
}

/// Run the status job for shard 0 until `gateway_statuses` lists shard 0, and
/// check that it lists nothing else.
async fn assert_replaced(conn: &mut ConnectionManager, case: &str) {
    let handle = Arc::new(ShardHandle::new(0, None, Some(1)));
    handle.update(|s| {
        s.status = "Connected";
        s.latency_ms = 4;
    });
    let job = tokio::spawn(sessions::run_job(
        conn.clone(),
        Arc::new(Registry::new([handle])),
    ));
    let statuses = wait_for(conn, STATUSES, |v| status_of(v, 0).is_some()).await;
    job.abort();
    assert_eq!(shards(&statuses), [0], "{case}: {statuses}");
    assert_eq!(statuses[0]["status"], "Connected", "{case}");
    assert_eq!(statuses[0]["latency"], 4, "{case}");
}

/// A stored value that is not a list of statuses (not JSON, not a list, a
/// list with an incomplete entry, a key of another type) does not stop the
/// job: it is replaced by this process' shards.
#[tokio::test]
async fn an_unreadable_statuses_value_is_replaced() {
    let cases: [(u8, &str); 3] = [
        (2, "not json"),
        (3, r#"{"0":{"shard":0,"status":"Connected"}}"#),
        (4, r#"[{"shard":3,"status":"Connected"}]"#),
    ];
    for (db, stored) in cases {
        let Some(mut conn) = redis(db).await else {
            return;
        };
        set(&mut conn, STATUSES, stored).await;
        assert_replaced(&mut conn, stored).await;
    }

    // GET fails on a hash; SET replaces it.
    let Some(mut conn) = redis(5).await else {
        return;
    };
    let _: i64 = redis::cmd("HSET")
        .arg(STATUSES)
        .arg("0")
        .arg("x")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_replaced(&mut conn, "a hash").await;
}

// ---------------------------------------------------------------------------
// While Redis fails

/// What `tracing` logs on this thread while the guard lives.
#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Logs {
    fn capture() -> (Self, tracing::subscriber::DefaultGuard) {
        let logs = Self::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || writer.clone())
            .finish();
        (logs, tracing::subscriber::set_default(subscriber))
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

/// A Redis connection that fails every command. It counts the pipelines it
/// is sent: each run of the status job ends with one (the sessions' writes).
#[derive(Clone, Default)]
struct FailingConn {
    pipelines: Arc<AtomicUsize>,
}

impl FailingConn {
    fn pipelines(&self) -> usize {
        self.pipelines.load(Ordering::SeqCst)
    }

    fn fail<'a, T: Send + 'a>(&self) -> redis::RedisFuture<'a, T> {
        let err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "fake failure");
        Box::pin(async move { Err(err.into()) })
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
        self.pipelines.fetch_add(1, Ordering::SeqCst);
        self.fail()
    }

    fn get_db(&self) -> i64 {
        0
    }
}

/// While every write fails, the job keeps running once a second, and each of
/// its two warnings is logged at most once per 5 s, with the number of
/// suppressed ones.
#[tokio::test]
async fn the_status_job_throttles_its_warnings_while_redis_fails() {
    // A `tokio::test` runtime runs the job on this thread, so it logs here.
    let (logs, _guard) = Logs::capture();
    let conn = FailingConn::default();
    let handle = Arc::new(ShardHandle::new(0, None, Some(1)));
    let start = Instant::now();
    let job = tokio::spawn(sessions::run_job(
        conn.clone(),
        Arc::new(Registry::new([handle])),
    ));

    // Three runs, about 2 s.
    let deadline = Instant::now() + Duration::from_secs(30);
    while conn.pipelines() < 3 {
        assert!(Instant::now() < deadline, "{} runs", conn.pipelines());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!job.is_finished(), "the job stopped after a failure");
    job.abort();
    let _ = job.await;
    let runs = conn.pipelines();
    // The first warning is let through, then at most one per 5 s.
    let allowed = 1 + start.elapsed().as_secs() as usize / 5;

    let text = logs.text();
    for warning in [
        "Failed to dump gateway statuses: ",
        "Failed to dump gateway sessions: ",
    ] {
        let lines: Vec<&str> = text.lines().filter(|l| l.contains(warning)).collect();
        assert!(!lines.is_empty(), "{warning}not logged: {text}");
        assert!(lines.len() <= allowed, "{runs} runs: {text}");
        assert!(lines[0].contains(" (0 similar suppressed)"), "{text}");
    }
}
