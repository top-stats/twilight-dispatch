#![recursion_limit = "256"]

//! What the state cache sends to Redis, against a scripted connection (no
//! services needed): a batch commits exactly the commands it plans, in one
//! transaction; reads and expiries use stored keys; an edit of a previous
//! session sends nothing with the cache or messages off. Also key planning
//! for keys the cache never writes, and READY's stale guilds with any hasher.

mod common;

use common::*;
use redis::{aio::ConnectionLike, Cmd, Pipeline, RedisFuture, Value as RedisValue};
use serde_json::json;
use std::{
    collections::HashSet,
    hash::{BuildHasherDefault, DefaultHasher},
};
use twilight_dispatch::{
    cache::{self, batch::Batch, keys, Outcome, UpdateContext},
    config::StateConfig,
};

fn context(state: &StateConfig) -> UpdateContext<'_> {
    UpdateContext {
        state,
        bot_id: Some(bot_id()),
        now_ms: now_ms(),
    }
}

fn args(cmd: &Cmd) -> Vec<String> {
    cmd.args_iter()
        .map(|arg| match arg {
            redis::Arg::Simple(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            _ => "<cursor>".to_owned(),
        })
        .collect()
}

/// What the scripted Redis holds under a stored key: only `channel:2`.
fn cached(key: &str) -> RedisValue {
    if key == "channel:2" {
        RedisValue::BulkString(br#"{"id":"2"}"#.to_vec())
    } else {
        RedisValue::Nil
    }
}

/// A scripted Redis that records every command and transaction. GET finds
/// nothing, MGET finds `cached` values, EXISTS finds every key and every
/// transaction succeeds.
#[derive(Default)]
struct Scripted {
    commands: Vec<Vec<String>>,
    transactions: Vec<Vec<Vec<String>>>,
}

impl Scripted {
    fn sent_nothing(&self) -> bool {
        self.commands.is_empty() && self.transactions.is_empty()
    }
}

impl ConnectionLike for Scripted {
    fn req_packed_command<'a>(&'a mut self, cmd: &'a Cmd) -> RedisFuture<'a, RedisValue> {
        let args = args(cmd);
        let reply = match args[0].as_str() {
            "GET" => RedisValue::Nil,
            "EXISTS" => RedisValue::Int(1),
            "MGET" => RedisValue::Array(args[1..].iter().map(|key| cached(key)).collect()),
            other => panic!("unexpected command {other}"),
        };
        self.commands.push(args);
        Box::pin(async move { Ok(reply) })
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        pipeline: &'a Pipeline,
        _offset: usize,
        _count: usize,
    ) -> RedisFuture<'a, Vec<RedisValue>> {
        let commands: Vec<Vec<String>> = pipeline.cmd_iter().map(args).collect();
        let replies = vec![RedisValue::Int(1); commands.len()];
        self.transactions.push(commands);
        // One transaction: the EXEC reply holds every command's reply.
        Box::pin(async move { Ok(vec![RedisValue::Array(replies)]) })
    }

    fn get_db(&self) -> i64 {
        0
    }
}

// ---------------------------------------------------------------------------
// Batch::commit

/// An empty batch sends nothing, not even an empty transaction.
#[tokio::test]
async fn an_empty_batch_sends_nothing() {
    let mut conn = Scripted::default();
    Batch::new().commit(&mut conn).await.unwrap();
    assert!(conn.sent_nothing());
}

/// `commit` sends exactly what `commands` plans, as one transaction: every
/// delete and removal first, whatever order they were planned in.
#[tokio::test]
async fn a_batch_commits_what_it_plans_in_one_transaction() {
    let mut batch = Batch::new();
    batch.set("channel:1:2", &json!({"id": "2"})).unwrap();
    batch.del("member:1:3");
    batch.expire("message:5:6", 60_000, 1_000);
    batch.persist("member:1:4");
    batch.del_raw("channel_keys:9");
    let planned = batch.commands();
    assert_eq!(
        planned,
        [
            vec!["DEL", "member:1:3", "channel_keys:9"],
            vec!["SREM", "guild_keys:1", "member:1:3"],
            vec!["SREM", "member_keys", "member:1:3"],
            vec!["ZREM", "expiry_zset", "member:1:3", "member:1:4"],
            vec!["MSET", "channel:2", r#"{"id":"2"}"#],
            vec!["SADD", "channel_keys", "channel:2"],
            vec!["SADD", "guild_keys:1", "channel:2"],
            vec!["ZADD", "expiry_zset", "61000", "message:5:6"],
        ]
    );

    let mut conn = Scripted::default();
    batch.commit(&mut conn).await.unwrap();
    assert!(conn.commands.is_empty(), "{:?}", conn.commands);
    assert_eq!(conn.transactions, [planned]);
}

/// More keys than one command takes are still one transaction, of the
/// chunked commands `commands` plans.
#[tokio::test]
async fn a_large_batch_is_one_transaction_of_chunked_commands() {
    let mut batch = Batch::new();
    for id in 0..=cache::batch::CHUNK {
        let key = format!("member:1:{id}");
        batch.set_json(&key, "{}".into());
        batch.expire(&key, 1, 0);
    }
    let planned = batch.commands();
    let count = |name: &str| planned.iter().filter(|cmd| cmd[0] == name).count();
    // `member_keys` and `guild_keys:1` take two SADDs each.
    assert_eq!((count("MSET"), count("SADD"), count("ZADD")), (2, 4, 2));

    let mut conn = Scripted::default();
    batch.commit(&mut conn).await.unwrap();
    assert_eq!(conn.transactions, [planned]);
}

// ---------------------------------------------------------------------------
// Stored keys

/// A guild channel is stored without its guild: reads, expiries and their
/// removal use the stored key, and other keys are used as they are.
#[tokio::test]
async fn reads_and_expiries_use_the_stored_key() {
    let mut conn = Scripted::default();
    let value = cache::get(&mut conn, "channel:1:2").await.unwrap();
    assert!(value.is_none());
    assert!(cache::exists(&mut conn, "channel:1:3").await.unwrap());
    let logical = ["channel:1:2".to_owned(), "member:1:3".to_owned()];
    let values = cache::get_all(&mut conn, &logical).await.unwrap();
    assert_eq!(values, [Some(json!({"id": "2"})), None]);
    // No keys, no MGET.
    assert!(cache::get_all(&mut conn, &[]).await.unwrap().is_empty());
    assert_eq!(
        conn.commands,
        [
            vec!["GET", "channel:2"],
            vec!["EXISTS", "channel:3"],
            vec!["MGET", "channel:2", "member:1:3"],
        ]
    );

    let mut batch = Batch::new();
    batch.expire("channel:1:2", 10, 0);
    batch.persist("channel:1:3");
    assert_eq!(
        batch.commands(),
        [
            vec!["ZREM", "expiry_zset", "channel:3"],
            vec!["ZADD", "expiry_zset", "10", "channel:2"],
        ]
    );
}

/// Keys of a prefix without a parent never have a parent segment in the
/// cache. Planning one anyway indexes it under that segment as a guild, as
/// in earlier versions, so planning stays total.
#[test]
fn global_keys_with_a_parent_segment_keep_the_old_guild_rule() {
    for (key, own) in [
        ("guild:1:2", "guild_keys"),
        ("bot_user:1:2", "bot_user_keys"),
    ] {
        let sets: Vec<String> = keys::plan(key)
            .indexes
            .into_iter()
            .map(|(set, _)| set)
            .collect();
        assert_eq!(sets, [own, "guild_keys:1"], "{key}");
    }
}

// ---------------------------------------------------------------------------
// Stale edits with the cache or messages off

/// `cache::update_stale` checks STATE_ENABLED and STATE_MESSAGE itself for
/// edits and hands every other event to `cache::update`: with the cache or
/// messages off neither reads or writes anything.
#[tokio::test]
async fn a_stale_edit_with_the_cache_or_messages_off_sends_nothing() {
    let states = [
        StateConfig {
            enabled: false,
            ..state_all()
        },
        StateConfig {
            message: false,
            ..state_all()
        },
    ];
    for state in &states {
        let mut conn = Scripted::default();
        let deleted = json!({"id": "1", "channel_id": "1003", "guild_id": "1000"});
        let events = [
            ("MESSAGE_UPDATE", message(1, 1003, Some(1000), "edited")),
            ("MESSAGE_DELETE", deleted),
        ];
        for (kind, d) in events {
            let mut typed = event(kind, d);
            let outcome = cache::update_stale(&mut conn, &mut typed, &context(state)).await;
            assert_eq!(outcome.unwrap(), Outcome::default(), "{kind}");
        }
        assert!(conn.sent_nothing(), "{state:?}: {:?}", conn.commands);
    }
}

// ---------------------------------------------------------------------------
// READY

/// READY's guilds may come in a set with any hasher.
#[test]
fn stale_guilds_take_a_set_with_any_hasher() {
    // On shards 1, 0 and 1 of 2 (`(id >> 22) % 2`).
    let ids: [u64; 3] = [1 << 22, 2 << 22, 3 << 22];
    let listed: HashSet<u64, BuildHasherDefault<DefaultHasher>> = [ids[0]].into_iter().collect();
    assert_eq!(keys::stale_guilds(ids, &listed, 1, 2), [ids[2]]);
    assert_eq!(keys::stale_guilds(ids, &listed, 0, 2), [ids[1]]);
}
