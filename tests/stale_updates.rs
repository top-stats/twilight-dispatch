#![recursion_limit = "256"]

//! Edits of a previous session: a MESSAGE_UPDATE still queued when READY of
//! the next session arrives (`worker::only_removes`, `cache::update_stale`).
//! Nothing resends edits, so it is applied, exactly as a current one is, but
//! only while the message is still cached: READY's reconcile runs in the
//! shard runner at the same time and must not be undone (set TEST_REDIS_URL).

mod common;

use common::*;
use redis::{
    aio::{ConnectionLike, ConnectionManager},
    Cmd, Pipeline, RedisFuture, Value as RedisValue,
};
use serde_json::{json, Value};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
    time::Duration,
};
use twilight_dispatch::{
    cache::{self, UpdateContext},
    config::StateConfig,
    discord_log::DiscordLog,
    gateway::{
        registry::{Registry, ShardHandle},
        runner::parse_event,
        worker::{self, Breaker, Job, Publisher, WorkerContext},
    },
    models::ApiResult,
    util::Throttle,
};

const GUILD: u64 = 1000;
/// `common::guild`'s first channel.
const CHANNEL: u64 = GUILD + 3;
const MESSAGE: &str = "message:1003:1";
const CREATED_AT: i64 = 1_800_000_000_000;
const EDITED_AT: i64 = CREATED_AT + 1_000;

/// The cached message. Its `tts` is true, which MESSAGE_UPDATE (always
/// false) must not overwrite.
fn hello() -> Value {
    let mut hello = message(1, CHANNEL, Some(GUILD), "hello");
    hello["tts"] = json!(true);
    hello
}

fn edit(content: &str) -> Value {
    let mut edit = message(1, CHANNEL, Some(GUILD), content);
    edit["edited_timestamp"] = json!("2024-01-01T00:01:00.000000+00:00");
    edit
}

async fn flush(conn: &mut ConnectionManager) {
    let _: () = redis::cmd("FLUSHDB").query_async(conn).await.unwrap();
}

/// Every key with its value, its members, or its members and their scores
/// (expiry deadlines included, unlike most dumps in the other tests).
async fn dump(conn: &mut ConnectionManager) -> BTreeSet<String> {
    let keys: Vec<String> = redis::cmd("KEYS").arg("*").query_async(conn).await.unwrap();
    let mut entries = BTreeSet::new();
    for key in keys {
        let kind: String = redis::cmd("TYPE")
            .arg(&key)
            .query_async(conn)
            .await
            .unwrap();
        let values: Vec<String> = match kind.as_str() {
            "string" => vec![redis::cmd("GET").arg(&key).query_async(conn).await.unwrap()],
            "set" => redis::cmd("SMEMBERS")
                .arg(&key)
                .query_async(conn)
                .await
                .unwrap(),
            "zset" => {
                let scored: Vec<(String, f64)> = redis::cmd("ZRANGE")
                    .arg(&key)
                    .arg(0)
                    .arg(-1)
                    .arg("WITHSCORES")
                    .query_async(conn)
                    .await
                    .unwrap();
                scored
                    .into_iter()
                    .map(|(member, score)| format!("{member} @{score}"))
                    .collect()
            }
            other => panic!("{key} is a {other}"),
        };
        entries.extend(values.into_iter().map(|value| format!("{key} {value}")));
    }
    entries
}

/// The keys that are left (values, index sets and the expiry zset alike).
async fn keys(conn: &mut ConnectionManager) -> Vec<String> {
    let mut keys: Vec<String> = redis::cmd("KEYS").arg("*").query_async(conn).await.unwrap();
    keys.sort();
    keys
}

/// The stored message, the index sets that list it and its expiry deadline.
async fn cached_message(
    conn: &mut ConnectionManager,
) -> (Option<String>, Vec<String>, Option<f64>) {
    let value: Option<String> = redis::cmd("GET")
        .arg(MESSAGE)
        .query_async(conn)
        .await
        .unwrap();
    let mut indexes = Vec::new();
    for set in ["message_keys".to_owned(), format!("channel_keys:{CHANNEL}")] {
        if members(conn, &set).await.contains(&MESSAGE.to_owned()) {
            indexes.push(set);
        }
    }
    (value, indexes, score(conn, MESSAGE).await)
}

#[derive(Default)]
struct Recorder(Mutex<Vec<(String, Value)>>);

impl Recorder {
    /// The `old` of every published event, by routing key.
    fn olds(&self) -> Vec<(String, Option<Value>)> {
        let published = self.0.lock().unwrap();
        published
            .iter()
            .map(|(kind, body)| (kind.clone(), body.get("old").cloned()))
            .collect()
    }
}

impl Publisher for Recorder {
    async fn publish(
        &self,
        routing_key: &str,
        payload: Vec<u8>,
        _: Option<String>,
    ) -> ApiResult<()> {
        let body = serde_json::from_slice(&payload)?;
        self.0.lock().unwrap().push((routing_key.to_owned(), body));
        Ok(())
    }
}

fn context<C>(
    redis: C,
    handle: &Arc<ShardHandle>,
    recorder: &Arc<Recorder>,
) -> WorkerContext<C, Recorder> {
    WorkerContext {
        redis,
        publisher: recorder.clone(),
        registry: Arc::new(Registry::new([handle.clone()])),
        state: state_all(),
        bot_id: bot_id(),
        cache_timeout: Duration::from_secs(5),
        discord: DiscordLog::disabled(),
        breaker: Breaker::default(),
        cache_errors: Throttle::default(),
        publish_errors: Throttle::default(),
    }
}

/// A job of the shard's current session (until `new_session`).
fn job(handle: &ShardHandle, kind: &str, seq: u64, d: Value) -> Job {
    let json = raw(kind, seq, d);
    let event = parse_event(&json).unwrap();
    Job {
        shard: 0,
        seq: Some(seq),
        token: handle.dispatch(seq),
        session: Some(Arc::from("A")),
        json,
        event,
    }
}

/// READY of the next session, which no longer lists the guild: its
/// reconcile removes the guild with its channels and their messages.
async fn ready_without_the_guild(conn: &mut ConnectionManager) {
    apply(conn, &state_all(), "READY", ready([0, 1], &[])).await;
}

// ---------------------------------------------------------------------------
// The same write as a current edit

/// `common::apply` at `CREATED_AT`, so the deadlines it sets are known.
async fn apply_at(conn: &mut ConnectionManager, state: &StateConfig, kind: &str, d: Value) {
    let ctx = UpdateContext {
        state,
        bot_id: Some(bot_id()),
        now_ms: CREATED_AT,
    };
    cache::update(conn, &mut event(kind, d), &ctx)
        .await
        .unwrap_or_else(|err| panic!("{kind}: {err}"));
}

/// `cache::update_stale` stores what `cache::update` stores for the same
/// edit at the same time: the merged message (with the cached `tts`), its
/// index entries and its refreshed expiry, and it returns the same `old`.
#[tokio::test]
async fn a_stale_edit_stores_what_a_current_one_stores() {
    let Some(mut conn) = redis(1).await else {
        return;
    };
    let states = [
        ("all", state_all(), true),
        (
            "TTL 0",
            StateConfig {
                message_ttl: 0,
                ..state_all()
            },
            true,
        ),
        (
            "no old",
            StateConfig {
                old: false,
                ..state_all()
            },
            true,
        ),
        // The message was cached before STATE_MESSAGE was turned off.
        (
            "no messages",
            StateConfig {
                message: false,
                ..state_all()
            },
            false,
        ),
    ];
    for (name, state, edits) in states {
        let mut results = Vec::new();
        for stale in [false, true] {
            flush(&mut conn).await;
            let seed = StateConfig {
                message: true,
                ..state.clone()
            };
            apply_at(&mut conn, &seed, "GUILD_CREATE", guild(GUILD, "g")).await;
            apply_at(&mut conn, &seed, "MESSAGE_CREATE", hello()).await;
            let seeded = dump(&mut conn).await;

            let ctx = UpdateContext {
                state: &state,
                bot_id: Some(bot_id()),
                now_ms: EDITED_AT,
            };
            let mut update = event("MESSAGE_UPDATE", edit("edited"));
            let outcome = if stale {
                cache::update_stale(&mut conn, &mut update, &ctx).await
            } else {
                cache::update(&mut conn, &mut update, &ctx).await
            }
            .unwrap_or_else(|err| panic!("{name} (stale: {stale}): {err}"));
            let after = dump(&mut conn).await;
            assert_eq!(after != seeded, edits, "{name} (stale: {stale}) edits");
            results.push((outcome, after, cached_message(&mut conn).await));
        }
        assert_eq!(results[0], results[1], "{name}: current, then stale");

        let (outcome, _, (value, indexes, deadline)) = &results[1];
        let value: Value = serde_json::from_str(value.as_deref().unwrap()).unwrap();
        let content = if edits { "edited" } else { "hello" };
        assert_eq!(value["content"], content, "{name}");
        assert_eq!(value["tts"], true, "{name}");
        assert_eq!(indexes.len(), 2, "{name}");
        let expiry = (state.message_ttl > 0).then(|| {
            let at = if edits { EDITED_AT } else { CREATED_AT };
            (at + state.message_ttl as i64) as f64
        });
        assert_eq!(*deadline, expiry, "{name}");
        let old = (edits && state.old).then(|| hello()["content"].clone());
        assert_eq!(
            outcome.old.as_ref().map(|old| old["content"].clone()),
            old,
            "{name}"
        );
    }
}

// ---------------------------------------------------------------------------
// The worker applies stale edits

/// An edit queued in session A and handled after READY of session B is
/// applied as if it were current: nothing resends it, so skipping it kept
/// the old content cached (with STATE_MESSAGE_TTL=0 for good), and the next
/// edit's `old` was wrong.
#[tokio::test]
async fn an_edit_of_a_previous_session_is_applied() {
    let Some(mut conn) = redis(2).await else {
        return;
    };
    let ttl = state_all().message_ttl as i64;
    let mut runs = Vec::new();
    for stale in [false, true] {
        flush(&mut conn).await;
        let handle = Arc::new(ShardHandle::new(0, None, Some(1)));
        let recorder = Arc::new(Recorder::default());
        let ctx = context(conn.clone(), &handle, &recorder);
        for job in [
            job(&handle, "GUILD_CREATE", 2, guild(GUILD, "g")),
            job(&handle, "MESSAGE_CREATE", 3, hello()),
        ] {
            worker::process(&ctx, &mut conn, job).await;
        }

        let edited = job(&handle, "MESSAGE_UPDATE", 4, edit("edited"));
        if stale {
            handle.new_session(1);
        }
        // So that MESSAGE_CREATE's deadline is earlier than any the edit sets.
        tokio::time::sleep(Duration::from_millis(5)).await;
        let started = now_ms();
        worker::process(&ctx, &mut conn, edited).await;
        let finished = now_ms();
        // The edit of the new session sees the stale one in its `old`.
        let next = job(&handle, "MESSAGE_UPDATE", 5, edit("edited again"));
        let (value, indexes, deadline) = cached_message(&mut conn).await;
        worker::process(&ctx, &mut conn, next).await;

        let deadline = deadline.expect("an expiry") as i64;
        assert!(
            (started + ttl..=finished + ttl).contains(&deadline),
            "stale: {stale}: the edit refreshes the expiry"
        );
        assert_eq!(handle.in_flight(), 0);
        runs.push((value, indexes, recorder.olds()));
    }
    assert_eq!(runs[0], runs[1], "current, then stale");

    let (value, indexes, olds) = &runs[1];
    let value: Value = serde_json::from_str(value.as_deref().unwrap()).unwrap();
    assert_eq!(value["content"], "edited");
    assert_eq!(value["tts"], true);
    assert_eq!(indexes.len(), 2);
    let contents: Vec<(&str, Option<&Value>)> = olds
        .iter()
        .map(|(kind, old)| (kind.as_str(), old.as_ref().map(|old| &old["content"])))
        .collect();
    assert_eq!(
        contents[2..],
        [
            ("MESSAGE_UPDATE", Some(&json!("hello"))),
            ("MESSAGE_UPDATE", Some(&json!("edited"))),
        ]
    );
}

/// A stale edit of a message that is not cached writes nothing: neither when
/// it never was, nor when READY of the new session removed its guild before
/// the edit's turn came.
#[tokio::test]
async fn a_stale_edit_does_not_cache_a_message() {
    let Some(mut conn) = redis(3).await else {
        return;
    };
    for removed_by_ready in [false, true] {
        flush(&mut conn).await;
        let handle = Arc::new(ShardHandle::new(0, None, Some(1)));
        let recorder = Arc::new(Recorder::default());
        let ctx = context(conn.clone(), &handle, &recorder);
        worker::process(
            &ctx,
            &mut conn,
            job(&handle, "GUILD_CREATE", 2, guild(GUILD, "g")),
        )
        .await;
        if removed_by_ready {
            worker::process(&ctx, &mut conn, job(&handle, "MESSAGE_CREATE", 3, hello())).await;
        }

        let edited = job(&handle, "MESSAGE_UPDATE", 4, edit("edited"));
        handle.new_session(1);
        if removed_by_ready {
            ready_without_the_guild(&mut conn).await;
            assert!(!exists(&mut conn, MESSAGE).await);
        }
        let before = dump(&mut conn).await;
        worker::process(&ctx, &mut conn, edited).await;

        let at = format!("removed by READY: {removed_by_ready}");
        assert_eq!(dump(&mut conn).await, before, "{at}");
        let olds = recorder.olds();
        assert_eq!(
            olds.last(),
            Some(&("MESSAGE_UPDATE".to_owned(), None)),
            "{at}"
        );
    }
}

// ---------------------------------------------------------------------------
// READY's reconcile between the read and the write

/// Runs READY's reconcile of the next session, on a connection of its own
/// as the shard runner does, right after the first GET of the message was
/// answered: between the read and the write of an edit.
struct RemovedAfterRead {
    inner: ConnectionManager,
    /// The runner's connection, until the GET.
    runner: Option<ConnectionManager>,
}

fn reads_the_message(cmd: &Cmd) -> bool {
    let mut args = cmd.args_iter();
    match (args.next(), args.next()) {
        (Some(redis::Arg::Simple(name)), Some(redis::Arg::Simple(key))) => {
            name == b"GET" && key == MESSAGE.as_bytes()
        }
        _ => false,
    }
}

impl ConnectionLike for RemovedAfterRead {
    fn req_packed_command<'a>(&'a mut self, cmd: &'a Cmd) -> RedisFuture<'a, RedisValue> {
        Box::pin(async move {
            let reply = self.inner.req_packed_command(cmd).await;
            if reads_the_message(cmd) {
                if let Some(mut runner) = self.runner.take() {
                    ready_without_the_guild(&mut runner).await;
                }
            }
            reply
        })
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        pipeline: &'a Pipeline,
        offset: usize,
        count: usize,
    ) -> RedisFuture<'a, Vec<RedisValue>> {
        self.inner.req_packed_commands(pipeline, offset, count)
    }

    fn get_db(&self) -> i64 {
        self.inner.get_db()
    }
}

/// READY removes the guild while a stale edit of one of its messages is
/// between reading the message and writing it back. Nothing of the message
/// comes back: no key, no index entry, no expiry entry (with a TTL of 0 a
/// message brought back would never expire).
#[tokio::test]
async fn a_message_removed_between_the_read_and_the_write_stays_removed() {
    let Some(mut conn) = redis(4).await else {
        return;
    };
    for ttl in [state_all().message_ttl, 0] {
        flush(&mut conn).await;
        let handle = Arc::new(ShardHandle::new(0, None, Some(1)));
        let recorder = Arc::new(Recorder::default());
        let mut ctx = context(conn.clone(), &handle, &recorder);
        ctx.state.message_ttl = ttl;
        for job in [
            job(&handle, "GUILD_CREATE", 2, guild(GUILD, "g")),
            job(&handle, "MESSAGE_CREATE", 3, hello()),
        ] {
            worker::process(&ctx, &mut conn, job).await;
        }
        assert!(exists(&mut conn, MESSAGE).await);

        let edited = job(&handle, "MESSAGE_UPDATE", 4, edit("edited"));
        handle.new_session(1);
        let mut racing = RemovedAfterRead {
            inner: conn.clone(),
            runner: Some(conn.clone()),
        };
        // `process` uses the connection it is given; `redis` is the one
        // `worker::spawn` would clone for each worker.
        let spawned = RemovedAfterRead {
            inner: conn.clone(),
            runner: None,
        };
        let mut racing_ctx = context(spawned, &handle, &recorder);
        racing_ctx.state = ctx.state.clone();
        worker::process(&racing_ctx, &mut racing, edited).await;

        assert!(
            racing.runner.is_none(),
            "TTL {ttl}: the edit read the message"
        );
        assert_eq!(
            cached_message(&mut conn).await,
            (None, Vec::new(), None),
            "TTL {ttl}"
        );
        // READY left only the bot user.
        assert_eq!(keys(&mut conn).await, ["bot_user"], "TTL {ttl}");
        let olds = recorder.olds();
        assert_eq!(
            olds.last(),
            Some(&("MESSAGE_UPDATE".to_owned(), None)),
            "TTL {ttl}"
        );
    }
}
