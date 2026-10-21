#![recursion_limit = "256"]

//! Cache maintenance jobs at scale: expiring more keys than one batch holds,
//! keys refreshed before their deadline, and migrating a legacy expiry hash
//! larger than one HSCAN page (set TEST_REDIS_URL). Each test uses its own
//! database index.

mod common;

use common::*;
use redis::aio::{ConnectionLike, ConnectionManager};
use serde_json::json;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use twilight_dispatch::{
    cache::{batch::Batch, jobs},
    constants::EXPIRY_BATCH,
    models::FormattedDateTime,
};

/// Every key of the database, sorted.
async fn keys(conn: &mut ConnectionManager) -> Vec<String> {
    let mut keys: Vec<String> = redis::cmd("KEYS").arg("*").query_async(conn).await.unwrap();
    keys.sort();
    keys
}

async fn zcard(conn: &mut ConnectionManager, key: &str) -> u64 {
    redis::cmd("ZCARD")
        .arg(key)
        .query_async(conn)
        .await
        .unwrap()
}

async fn scard(conn: &mut ConnectionManager, key: &str) -> u64 {
    redis::cmd("SCARD")
        .arg(key)
        .query_async(conn)
        .await
        .unwrap()
}

/// Write `count` messages of channel 7 that expire at `deadline`, the way the
/// cache does (value, index sets and expiry in one batch).
async fn messages_due_at(conn: &mut ConnectionManager, ids: std::ops::Range<u64>, deadline: i64) {
    let mut batch = Batch::new();
    for id in ids {
        let key = format!("message:7:{id}");
        batch.set(&key, &json!({"id": id.to_string()})).unwrap();
        // expire() takes a TTL relative to "now".
        batch.expire(&key, 1, deadline - 1);
    }
    batch.commit(conn).await.unwrap();
}

/// A legacy `expiry_keys` value: a JSON string holding a formatted timestamp.
fn legacy_value(unix_ms: i64) -> String {
    let at =
        time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(unix_ms) * 1_000_000).unwrap();
    serde_json::to_string(&FormattedDateTime::from_offset(at)).unwrap()
}

#[tokio::test]
async fn expiry_handles_more_keys_than_one_batch() {
    let Some(mut conn) = redis(1).await else {
        return;
    };
    let now = 1_000_000;
    let due = 2 * EXPIRY_BATCH as u64 + 7;
    messages_due_at(&mut conn, 0..due, now - 10).await;
    // Refreshed before the deadline: these stay.
    messages_due_at(&mut conn, due..due + 3, now + 60_000).await;
    assert_eq!(zcard(&mut conn, "expiry_zset").await, due + 3);

    assert_eq!(jobs::expire_due(&mut conn, now).await.unwrap(), due);

    assert_eq!(zcard(&mut conn, "expiry_zset").await, 3);
    assert_eq!(scard(&mut conn, "message_keys").await, 3);
    assert_eq!(scard(&mut conn, "channel_keys:7").await, 3);
    let left: Vec<String> = keys(&mut conn)
        .await
        .into_iter()
        .filter(|key| key.starts_with("message:"))
        .collect();
    let mut expected: Vec<String> = (due..due + 3).map(|id| format!("message:7:{id}")).collect();
    expected.sort();
    assert_eq!(left, expected);

    // Nothing is due any more.
    assert_eq!(jobs::expire_due(&mut conn, now).await.unwrap(), 0);
    // Their own deadline comes.
    assert_eq!(jobs::expire_due(&mut conn, now + 60_000).await.unwrap(), 3);
    assert!(
        keys(&mut conn).await.is_empty(),
        "{:?}",
        keys(&mut conn).await
    );
}

#[tokio::test]
async fn expiry_of_exactly_one_batch() {
    let Some(mut conn) = redis(2).await else {
        return;
    };
    // A full batch makes the job look for more; there is nothing else.
    messages_due_at(&mut conn, 0..EXPIRY_BATCH as u64, 500).await;
    assert_eq!(
        jobs::expire_due(&mut conn, 1_000).await.unwrap(),
        EXPIRY_BATCH as u64
    );
    assert!(keys(&mut conn).await.is_empty());
    // An empty schedule is fine.
    assert_eq!(jobs::expire_due(&mut conn, 1_000).await.unwrap(), 0);
}

#[tokio::test]
async fn deadlines_are_inclusive_and_refreshes_win() {
    let Some(mut conn) = redis(3).await else {
        return;
    };
    messages_due_at(&mut conn, 0..1, 1_000).await;
    messages_due_at(&mut conn, 1..2, 1_001).await;
    assert_eq!(jobs::expire_due(&mut conn, 999).await.unwrap(), 0);
    assert_eq!(
        jobs::expire_due(&mut conn, 1_000).await.unwrap(),
        1,
        "a key is due at its deadline"
    );
    assert!(!exists(&mut conn, "message:7:0").await);
    assert!(exists(&mut conn, "message:7:1").await);

    // A refresh moves the deadline; the old one no longer applies.
    messages_due_at(&mut conn, 1..2, 5_000).await;
    assert_eq!(jobs::expire_due(&mut conn, 2_000).await.unwrap(), 0);
    assert!(exists(&mut conn, "message:7:1").await);

    // Members expire with their guild index entries.
    let mut batch = Batch::new();
    batch.set("member:9:1", &json!({"guild_id": "9"})).unwrap();
    batch.expire("member:9:1", 10, 0);
    batch.commit(&mut conn).await.unwrap();
    assert_eq!(jobs::expire_due(&mut conn, 10).await.unwrap(), 1);
    assert!(!exists(&mut conn, "member:9:1").await);
    assert!(members(&mut conn, "member_keys").await.is_empty());
    assert!(members(&mut conn, "guild_keys:9").await.is_empty());
}

#[tokio::test]
async fn entries_of_keys_deleted_elsewhere_are_cleaned_up() {
    let Some(mut conn) = redis(4).await else {
        return;
    };
    messages_due_at(&mut conn, 0..3, 100).await;
    // Deleted without its schedule entry (e.g. by hand or an older version).
    let _: () = redis::cmd("DEL")
        .arg("message:7:1")
        .query_async(&mut conn)
        .await
        .unwrap();

    assert_eq!(jobs::expire_due(&mut conn, 100).await.unwrap(), 3);
    assert!(
        keys(&mut conn).await.is_empty(),
        "{:?}",
        keys(&mut conn).await
    );
}

#[tokio::test]
async fn legacy_hash_larger_than_one_scan_page_is_migrated() {
    let Some(mut conn) = redis(5).await else {
        return;
    };
    let valid = 2 * EXPIRY_BATCH + 37;
    let garbage = 150;
    let mut entries: Vec<(String, String)> = (0..valid)
        .map(|i| {
            (
                format!("member:1:{i}"),
                legacy_value(1_700_000_000_000 + i as i64),
            )
        })
        .collect();
    entries.extend((0..garbage).map(|i| (format!("member:2:{i}"), format!("garbage{i}"))));
    entries.push(("member:3:1".to_owned(), "12345".to_owned()));
    for chunk in entries.chunks(200) {
        let mut cmd = redis::cmd("HSET");
        cmd.arg("expiry_keys");
        for (key, value) in chunk {
            cmd.arg(key).arg(value);
        }
        let _: () = cmd.query_async(&mut conn).await.unwrap();
    }

    let migrated = jobs::migrate_legacy_expiry(&mut conn).await.unwrap();
    assert_eq!(migrated, valid as u64);

    // Every valid entry moved with its deadline; garbage was dropped.
    assert!(!exists(&mut conn, "expiry_keys").await);
    assert_eq!(zcard(&mut conn, "expiry_zset").await, valid as u64);
    for i in [0, 1, EXPIRY_BATCH, valid - 1] {
        assert_eq!(
            score(&mut conn, &format!("member:1:{i}")).await,
            Some((1_700_000_000_000 + i as i64) as f64),
            "member:1:{i}"
        );
    }
    assert_eq!(score(&mut conn, "member:2:0").await, None);
    assert_eq!(score(&mut conn, "member:3:1").await, None);
    // The ZADD GT probe leaves nothing behind.
    assert_eq!(keys(&mut conn).await, ["expiry_zset"]);

    // Migrated deadlines are honoured by the expiry job.
    let removed = jobs::expire_due(&mut conn, 1_700_000_000_000 + 9)
        .await
        .unwrap();
    assert_eq!(removed, 10);
}

#[tokio::test]
async fn legacy_migration_keeps_the_later_deadline_and_handles_odd_input() {
    let Some(mut conn) = redis(6).await else {
        return;
    };
    // Nothing to migrate.
    assert_eq!(jobs::migrate_legacy_expiry(&mut conn).await.unwrap(), 0);
    assert!(keys(&mut conn).await.is_empty(), "no probe without entries");

    // Only garbage: removed, nothing scheduled.
    let _: () = redis::cmd("HSET")
        .arg("expiry_keys")
        .arg("member:1:1")
        .arg("not json")
        .arg("member:1:2")
        .arg(r#""2026-13-45T99:99:99""#)
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(jobs::migrate_legacy_expiry(&mut conn).await.unwrap(), 0);
    assert!(
        keys(&mut conn).await.is_empty(),
        "{:?}",
        keys(&mut conn).await
    );

    // A key that is already scheduled later keeps its later deadline (an
    // older instance wrote a stale value to the hash).
    let mut batch = Batch::new();
    batch.set("member:1:3", &json!({})).unwrap();
    batch.expire("member:1:3", 1, 9_999);
    batch.commit(&mut conn).await.unwrap();
    let _: () = redis::cmd("HSET")
        .arg("expiry_keys")
        .arg("member:1:3")
        .arg(legacy_value(5_000))
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(jobs::migrate_legacy_expiry(&mut conn).await.unwrap(), 1);
    assert_eq!(score(&mut conn, "member:1:3").await, Some(10_000.0));
    assert!(!exists(&mut conn, "expiry_keys").await);

    // Values with every subsecond precision older versions wrote parse.
    let _: () = redis::cmd("HSET")
        .arg("expiry_keys")
        .arg("member:1:4")
        .arg(r#""1970-01-01T00:00:02.123456789""#)
        .arg("member:1:5")
        .arg(r#""1970-01-01T00:00:03.0""#)
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(jobs::migrate_legacy_expiry(&mut conn).await.unwrap(), 2);
    assert_eq!(score(&mut conn, "member:1:4").await, Some(2_123.0));
    assert_eq!(score(&mut conn, "member:1:5").await, Some(3_000.0));

    // A legacy key of the wrong type is an error and is left alone.
    let _: () = redis::cmd("SET")
        .arg("expiry_keys")
        .arg("x")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert!(jobs::migrate_legacy_expiry(&mut conn).await.is_err());
    assert!(exists(&mut conn, "expiry_keys").await);
}

// ---------------------------------------------------------------------------
// Redis before 6.2 (no `ZADD ... GT`), scripted

#[derive(Clone, Copy)]
enum Probe {
    /// `ERR syntax error`, what Redis < 6.2 answers to `ZADD ... GT`.
    Unsupported,
    /// The connection broke during the probe.
    Broken,
}

/// One HSCAN reply: the next cursor and the fields with their values.
type Page = (u64, Vec<(String, String)>);

/// A scripted Redis that serves `expiry_keys` in HSCAN pages and records
/// every command and transaction it receives.
#[derive(Clone)]
struct ScriptedRedis {
    probe: Probe,
    pages: Arc<Mutex<VecDeque<Page>>>,
    commands: Arc<Mutex<Vec<Vec<String>>>>,
    transactions: Arc<Mutex<Vec<Vec<Vec<String>>>>>,
}

impl ScriptedRedis {
    fn new(probe: Probe, pages: Vec<Page>) -> Self {
        Self {
            probe,
            pages: Arc::new(Mutex::new(pages.into())),
            commands: Arc::default(),
            transactions: Arc::default(),
        }
    }

    fn commands(&self) -> Vec<Vec<String>> {
        self.commands.lock().unwrap().clone()
    }

    fn transactions(&self) -> Vec<Vec<Vec<String>>> {
        self.transactions.lock().unwrap().clone()
    }
}

fn args(cmd: &redis::Cmd) -> Vec<String> {
    cmd.args_iter()
        .map(|arg| match arg {
            redis::Arg::Simple(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            _ => "<cursor>".to_owned(),
        })
        .collect()
}

fn bulk(value: &str) -> redis::Value {
    redis::Value::BulkString(value.as_bytes().to_vec())
}

impl ConnectionLike for ScriptedRedis {
    fn req_packed_command<'a>(
        &'a mut self,
        cmd: &'a redis::Cmd,
    ) -> redis::RedisFuture<'a, redis::Value> {
        let args = args(cmd);
        self.commands.lock().unwrap().push(args.clone());
        let reply = match args[0].as_str() {
            "HSCAN" => {
                let (cursor, entries) = self.pages.lock().unwrap().pop_front().unwrap_or_default();
                Ok(redis::Value::Array(vec![
                    bulk(&cursor.to_string()),
                    redis::Value::Array(
                        entries
                            .iter()
                            .flat_map(|(key, value)| [bulk(key), bulk(value)])
                            .collect(),
                    ),
                ]))
            }
            "ZADD" => Err(match self.probe {
                Probe::Unsupported => redis::RedisError::from((
                    redis::ErrorKind::Server(redis::ServerErrorKind::ResponseError),
                    "ERR syntax error",
                )),
                Probe::Broken => {
                    std::io::Error::new(std::io::ErrorKind::BrokenPipe, "broken pipe").into()
                }
            }),
            "DEL" => Ok(redis::Value::Int(1)),
            other => panic!("unexpected command {other}"),
        };
        Box::pin(async move { reply })
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        pipeline: &'a redis::Pipeline,
        _offset: usize,
        _count: usize,
    ) -> redis::RedisFuture<'a, Vec<redis::Value>> {
        let commands: Vec<Vec<String>> = pipeline.cmd_iter().map(args).collect();
        let replies = vec![redis::Value::Int(1); commands.len()];
        self.transactions.lock().unwrap().push(commands);
        // One transaction: the EXEC reply holds every command's reply.
        Box::pin(async move { Ok(vec![redis::Value::Array(replies)]) })
    }

    fn get_db(&self) -> i64 {
        0
    }
}

fn entry(key: &str, unix_ms: i64) -> (String, String) {
    (key.to_owned(), legacy_value(unix_ms))
}

#[tokio::test]
async fn legacy_migration_falls_back_to_nx_before_redis_6_2() {
    let mut redis = ScriptedRedis::new(
        Probe::Unsupported,
        vec![
            (
                17,
                vec![
                    entry("member:1:1", 1_500),
                    ("member:1:2".to_owned(), "garbage".to_owned()),
                ],
            ),
            (0, vec![entry("message:2:3", 2_500)]),
        ],
    );

    assert_eq!(jobs::migrate_legacy_expiry(&mut redis).await.unwrap(), 2);

    // The scan follows the cursor; the probe runs once, before any write.
    let commands = redis.commands();
    let names: Vec<&str> = commands.iter().map(|c| c[0].as_str()).collect();
    assert_eq!(names, ["HSCAN", "ZADD", "DEL", "HSCAN"]);
    assert_eq!(commands[0][..3], ["HSCAN", "expiry_keys", "0"]);
    assert_eq!(commands[3][..3], ["HSCAN", "expiry_keys", "17"]);
    assert_eq!(commands[1][..3], ["ZADD", "expiry_zset_probe", "GT"]);
    assert_eq!(commands[2], ["DEL", "expiry_zset_probe"]);

    // One transaction per page: ZADD NX (never lowers a deadline written
    // meanwhile) and HDEL of every scanned field, garbage included.
    assert_eq!(
        redis.transactions(),
        [
            vec![
                vec!["ZADD", "expiry_zset", "NX", "1500", "member:1:1"],
                vec!["HDEL", "expiry_keys", "member:1:1", "member:1:2"],
            ],
            vec![
                vec!["ZADD", "expiry_zset", "NX", "2500", "message:2:3"],
                vec!["HDEL", "expiry_keys", "message:2:3"],
            ],
        ]
    );
}

#[tokio::test]
async fn legacy_entries_survive_a_failed_probe() {
    let mut redis = ScriptedRedis::new(Probe::Broken, vec![(0, vec![entry("member:1:1", 1_500)])]);

    assert!(jobs::migrate_legacy_expiry(&mut redis).await.is_err());
    // Nothing was deleted: the next run migrates them.
    assert!(redis.transactions().is_empty());
}

// ---------------------------------------------------------------------------
// When an expiry run stops, scripted

/// A scripted Redis for the expiry job: every ZRANGEBYSCORE finds the same
/// `due` keys and every script run removes `removed` of them.
struct DueEveryTime {
    due: Vec<String>,
    removed: u64,
    queries: usize,
}

impl DueEveryTime {
    fn new(due: usize, removed: u64) -> Self {
        Self {
            due: (0..due).map(|id| format!("message:7:{id}")).collect(),
            removed,
            queries: 0,
        }
    }
}

impl ConnectionLike for DueEveryTime {
    fn req_packed_command<'a>(
        &'a mut self,
        cmd: &'a redis::Cmd,
    ) -> redis::RedisFuture<'a, redis::Value> {
        let reply = match args(cmd)[0].as_str() {
            "ZRANGEBYSCORE" => {
                self.queries += 1;
                // Nothing is due after a few rounds, so a run that does not
                // stop fails the test instead of hanging it.
                let due = self.due.iter().filter(|_| self.queries <= 3);
                redis::Value::Array(due.map(|key| bulk(key)).collect())
            }
            "EVALSHA" => redis::Value::Int(self.removed as i64),
            other => panic!("unexpected command {other}"),
        };
        Box::pin(async move { Ok(reply) })
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        _pipeline: &'a redis::Pipeline,
        _offset: usize,
        _count: usize,
    ) -> redis::RedisFuture<'a, Vec<redis::Value>> {
        panic!("the expiry job sends no pipelines")
    }

    fn get_db(&self) -> i64 {
        0
    }
}

/// The job runs every second on Redis' single thread: fewer due keys than a
/// batch means there is nothing left, so it does not ask again.
#[tokio::test]
async fn an_expiry_run_ends_with_a_batch_that_is_not_full() {
    let mut redis = DueEveryTime::new(3, 3);
    assert_eq!(jobs::expire_due(&mut redis, 1_000).await.unwrap(), 3);
    assert_eq!(redis.queries, 1);
}

/// A full batch of which the script removes nothing (refreshed after they
/// were read, or not removable at all) ends the run. Keys that stay due but
/// are never removed would otherwise keep the job looping on Redis.
#[tokio::test]
async fn an_expiry_run_ends_when_a_full_batch_removes_nothing() {
    let mut redis = DueEveryTime::new(EXPIRY_BATCH, 0);
    assert_eq!(jobs::expire_due(&mut redis, 1_000).await.unwrap(), 0);
    assert_eq!(redis.queries, 1);

    // Removing some of a full batch goes on with the next one.
    let mut redis = DueEveryTime::new(EXPIRY_BATCH, 1);
    assert_eq!(jobs::expire_due(&mut redis, 1_000).await.unwrap(), 3);
    assert_eq!(redis.queries, 4);
}
