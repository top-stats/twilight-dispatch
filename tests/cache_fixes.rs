#![recursion_limit = "256"]

//! Regression tests for the state cache against a real Redis (set
//! TEST_REDIS_URL): an edit that removes a message's buttons, the reads of an
//! offline presence, the index entries a guild removal sends, the expiry of
//! keys with any number of index sets and the guild of chunk presences. Each
//! test uses its own database index.

mod common;

use common::*;
use redis::{
    aio::{ConnectionLike, ConnectionManager},
    Cmd, Pipeline, RedisFuture, Value as RedisValue,
};
use serde_json::{json, Value};
use twilight_dispatch::{
    cache::{self, batch::Batch, jobs, Outcome, UpdateContext},
    config::StateConfig,
};
use twilight_model::{gateway::event::Event, id::Id};

const GUILD: u64 = 1000;
/// `common::guild`'s first channel.
const CHANNEL: u64 = GUILD + 3;
/// Where message 1 of `CHANNEL` is stored.
const MESSAGE: &str = "message:1003:1";

// ---------------------------------------------------------------------------
// Helpers

fn context(state: &StateConfig) -> UpdateContext<'_> {
    UpdateContext {
        state,
        bot_id: Some(bot_id()),
        now_ms: now_ms(),
    }
}

/// Apply an event with the whole cache on.
async fn seed(conn: &mut ConnectionManager, kind: &str, d: Value) {
    apply(conn, &state_all(), kind, d).await;
}

async fn flush(conn: &mut ConnectionManager) {
    let _: () = redis::cmd("FLUSHDB").query_async(conn).await.unwrap();
}

/// Every key of the database, sorted.
async fn keys(conn: &mut ConnectionManager) -> Vec<String> {
    let mut keys: Vec<String> = redis::cmd("KEYS").arg("*").query_async(conn).await.unwrap();
    keys.sort();
    keys
}

fn args(cmd: &Cmd) -> Vec<String> {
    cmd.args_iter()
        .map(|arg| match arg {
            redis::Arg::Simple(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            _ => "<cursor>".to_owned(),
        })
        .collect()
}

/// Records every command and transaction, then passes it on to Redis.
struct Recording {
    inner: ConnectionManager,
    commands: Vec<Vec<String>>,
    transactions: Vec<Vec<Vec<String>>>,
}

impl Recording {
    fn new(conn: &ConnectionManager) -> Self {
        Self {
            inner: conn.clone(),
            commands: Vec::new(),
            transactions: Vec::new(),
        }
    }

    /// How many `name` commands were sent outside of transactions.
    fn sent(&self, name: &str) -> usize {
        self.commands.iter().filter(|cmd| cmd[0] == name).count()
    }

    /// Every `<set> <member>` an SREM of a transaction removed, sorted.
    fn unindexed(&self) -> Vec<String> {
        let mut removed = Vec::new();
        for cmd in self.transactions.iter().flatten() {
            if cmd[0] == "SREM" {
                removed.extend(cmd[2..].iter().map(|key| format!("{} {key}", cmd[1])));
            }
        }
        removed.sort();
        removed
    }
}

impl ConnectionLike for Recording {
    fn req_packed_command<'a>(&'a mut self, cmd: &'a Cmd) -> RedisFuture<'a, RedisValue> {
        self.commands.push(args(cmd));
        self.inner.req_packed_command(cmd)
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        pipeline: &'a Pipeline,
        offset: usize,
        count: usize,
    ) -> RedisFuture<'a, Vec<RedisValue>> {
        let commands: Vec<Vec<String>> = pipeline.cmd_iter().map(args).collect();
        self.transactions.push(commands);
        self.inner.req_packed_commands(pipeline, offset, count)
    }

    fn get_db(&self) -> i64 {
        self.inner.get_db()
    }
}

// ---------------------------------------------------------------------------
// MESSAGE_UPDATE that removes the buttons

/// Message `id` of `CHANNEL`.
fn text(id: u64, content: &str) -> Value {
    message(id, CHANNEL, Some(GUILD), content)
}

/// Message `id` of `CHANNEL` with one action row of one button.
fn with_button(id: u64, content: &str, custom_id: &str) -> Value {
    let mut sent = text(id, content);
    sent["components"] = json!([{
        "type": 1,
        "components": [{"type": 2, "style": 1, "custom_id": custom_id, "label": "A"}],
    }]);
    sent
}

/// The `custom_id` of a message's first button, `None` without one.
fn button(stored: &Value) -> Option<&str> {
    stored["components"][0]["components"][0]["custom_id"].as_str()
}

/// Apply a MESSAGE_UPDATE with the whole cache on, as an edit of the current
/// session (`cache::update`) or of a previous one (`cache::update_stale`).
async fn edit(conn: &mut ConnectionManager, stale: bool, d: Value) -> Outcome {
    let state = state_all();
    let ctx = context(&state);
    let mut update = event("MESSAGE_UPDATE", d);
    let outcome = if stale {
        cache::update_stale(conn, &mut update, &ctx).await
    } else {
        cache::update(conn, &mut update, &ctx).await
    };
    outcome.unwrap_or_else(|err| panic!("MESSAGE_UPDATE (stale: {stale}): {err}"))
}

/// twilight leaves an empty `components` out of a message, so an edit that
/// removed every button kept the cached ones (with STATE_MESSAGE_TTL=0 for
/// good), and the next edit's `old` still showed them. Current and stale
/// edits store the same.
#[tokio::test]
async fn an_edit_that_removes_the_buttons_removes_them_from_the_cache() {
    let Some(mut conn) = redis(1).await else {
        return;
    };
    for stale in [false, true] {
        let at = format!("stale: {stale}");
        flush(&mut conn).await;
        seed(&mut conn, "GUILD_CREATE", guild(GUILD, "g")).await;
        seed(&mut conn, "MESSAGE_CREATE", with_button(1, "pick", "a")).await;
        let stored = get(&mut conn, MESSAGE).await.unwrap();
        assert_eq!(button(&stored), Some("a"), "{at}");

        // An edit's buttons replace the cached ones.
        edit(&mut conn, stale, with_button(1, "pick again", "b")).await;
        let stored = get(&mut conn, MESSAGE).await.unwrap();
        assert_eq!(button(&stored), Some("b"), "{at}");

        // No buttons left: the update has no `components` at all.
        let outcome = edit(&mut conn, stale, text(1, "done")).await;
        assert_eq!(button(&outcome.old.unwrap()), Some("b"), "{at}");
        let stored = get(&mut conn, MESSAGE).await.unwrap();
        assert_eq!(stored["content"], "done", "{at}");
        assert!(stored.get("components").is_none(), "{at}: {stored}");

        // So the next edit's `old` has none either.
        let outcome = edit(&mut conn, stale, text(1, "done!")).await;
        let old = outcome.old.unwrap();
        assert_eq!(old["content"], "done", "{at}");
        assert!(old.get("components").is_none(), "{at}: {old}");

        // What is cached is what MESSAGE_CREATE caches for the edited message.
        let edited = get(&mut conn, MESSAGE).await;
        seed(&mut conn, "MESSAGE_CREATE", text(1, "done!")).await;
        assert_eq!(get(&mut conn, MESSAGE).await, edited, "{at}");

        // A message that never had buttons is edited without the key.
        seed(&mut conn, "MESSAGE_CREATE", text(2, "plain")).await;
        edit(&mut conn, stale, text(2, "plain!")).await;
        let stored = get(&mut conn, "message:1003:2").await.unwrap();
        assert_eq!(stored["content"], "plain!", "{at}");
        assert!(stored.get("components").is_none(), "{at}: {stored}");
    }
}

// ---------------------------------------------------------------------------
// Offline PRESENCE_UPDATE

/// An offline presence is only deleted, so it is only read for STATE_OLD: the
/// cached user that completes a partial one is not needed. It used to be read
/// whenever its user was partial, which is the usual shape.
#[tokio::test]
async fn an_offline_presence_is_only_read_for_state_old() {
    let Some(mut conn) = redis(2).await else {
        return;
    };
    for (old, gets) in [(false, 0), (true, 1)] {
        flush(&mut conn).await;
        let state = StateConfig { old, ..state_all() };
        let online = presence(GUILD, 10, "online");
        apply(&mut conn, &state, "PRESENCE_UPDATE", online).await;
        assert!(exists(&mut conn, "presence:1000:10").await);

        let mut recording = Recording::new(&conn);
        let mut offline = event("PRESENCE_UPDATE", presence(GUILD, 10, "offline"));
        let outcome = cache::update(&mut recording, &mut offline, &context(&state)).await;

        let at = format!("STATE_OLD={old}");
        assert_eq!(recording.sent("GET"), gets, "{at}");
        assert!(!exists(&mut conn, "presence:1000:10").await, "{at}");
        if old {
            assert_eq!(outcome.unwrap().old.unwrap()["status"], "online");
        } else {
            assert_eq!(outcome.unwrap(), Outcome::default());
        }
    }

    // A partial user that is stored is still completed from the cache.
    flush(&mut conn).await;
    let state = StateConfig {
        old: false,
        ..state_all()
    };
    let mut online = presence(GUILD, 10, "online");
    online["user"] = user(10, "ten");
    apply(&mut conn, &state, "PRESENCE_UPDATE", online).await;
    let mut recording = Recording::new(&conn);
    let mut idle = event("PRESENCE_UPDATE", presence(GUILD, 10, "idle"));
    cache::update(&mut recording, &mut idle, &context(&state))
        .await
        .unwrap();
    assert_eq!(recording.sent("GET"), 1);
    let stored = get(&mut conn, "presence:1000:10").await.unwrap();
    assert_eq!(stored["status"], "idle");
    assert_eq!(stored["user"]["username"], "ten");
}

// ---------------------------------------------------------------------------
// Removing a guild

/// Guild channels are stored without their guild. Removing a guild used to
/// add it back to also remove them from `guild_keys:<guild>`: an SREM of the
/// set the same transaction deletes. No index may be left listing them.
#[tokio::test]
async fn removing_a_guild_sends_no_srem_of_the_set_it_deletes() {
    let Some(mut conn) = redis(3).await else {
        return;
    };
    let state = state_all();
    let removals = [
        ("GUILD_DELETE", json!({"id": "1000"})),
        ("READY", ready([0, 1], &[])),
    ];
    for (kind, d) in removals {
        flush(&mut conn).await;
        seed(&mut conn, "GUILD_CREATE", guild(GUILD, "g")).await;
        seed(&mut conn, "MESSAGE_CREATE", text(1, "hi")).await;

        let mut recording = Recording::new(&conn);
        let mut removal = event(kind, d);
        cache::update(&mut recording, &mut removal, &context(&state))
            .await
            .unwrap();

        let mut channels = recording.unindexed();
        channels.retain(|removed| removed.contains(" channel:"));
        assert_eq!(
            channels,
            ["channel_keys channel:1003", "channel_keys channel:1004"],
            "{kind}"
        );
        assert!(!exists(&mut conn, "guild_keys:1000").await, "{kind}");
        let listed = members(&mut conn, "channel_keys").await;
        assert!(listed.is_empty(), "{kind}: {listed:?}");
        assert!(!exists(&mut conn, MESSAGE).await, "{kind}");
    }

    // A GUILD_CREATE that no longer lists a cached channel removes it from
    // `channel_keys` and, since a re-sync with STATE_MEMBER keeps
    // `guild_keys:<guild>`, from the guild's index; the channels it lists are
    // indexed again.
    flush(&mut conn).await;
    seed(&mut conn, "GUILD_CREATE", guild(GUILD, "g")).await;
    let mut resync = guild(GUILD, "g");
    resync["channels"] = json!([channel(CHANNEL, None, "general")]);
    let mut recording = Recording::new(&conn);
    let mut create = event("GUILD_CREATE", resync);
    cache::update(&mut recording, &mut create, &context(&state))
        .await
        .unwrap();

    let mut channels = recording.unindexed();
    channels.retain(|removed| removed.contains(" channel:"));
    assert_eq!(
        channels,
        ["channel_keys channel:1004", "guild_keys:1000 channel:1004"]
    );
    assert!(!exists(&mut conn, "channel:1004").await);
    assert_eq!(members(&mut conn, "channel_keys").await, ["channel:1003"]);
    let mut children = members(&mut conn, "guild_keys:1000").await;
    children.retain(|child| child.starts_with("channel:"));
    assert_eq!(children, ["channel:1003"]);
}

// ---------------------------------------------------------------------------
// Expiry

/// The expiry script steps through its arguments by each key's own number of
/// index sets. Keys with 0, 1, 2 and 3 of them are removed with their index
/// entries; the key that is not due yet keeps its own.
#[tokio::test]
async fn expiry_walks_keys_with_any_number_of_index_sets() {
    let Some(mut conn) = redis(4).await else {
        return;
    };
    let mut batch = Batch::new();
    for key in [
        "bot_user",
        "guild:5",
        "member:1:2",
        "thread:1:9",
        "message:3:4",
    ] {
        batch.set(key, &json!({})).unwrap();
        // expire() takes a TTL relative to "now": due at 100.
        batch.expire(key, 1, 99);
    }
    batch.set("member:1:3", &json!({})).unwrap();
    batch.expire("member:1:3", 1, 9_999);
    batch.commit(&mut conn).await.unwrap();

    assert_eq!(jobs::expire_due(&mut conn, 100).await.unwrap(), 5);
    assert_eq!(
        keys(&mut conn).await,
        ["expiry_zset", "guild_keys:1", "member:1:3", "member_keys"]
    );
    assert_eq!(members(&mut conn, "guild_keys:1").await, ["member:1:3"]);
    assert_eq!(members(&mut conn, "member_keys").await, ["member:1:3"]);
    assert_eq!(score(&mut conn, "member:1:3").await, Some(10_000.0));
}

// ---------------------------------------------------------------------------
// GUILD_MEMBERS_CHUNK

/// Chunk presences are stored with the chunk's guild, also when a presence
/// names another one (twilight's deserializer sets it, a chunk built in code
/// may not).
#[tokio::test]
async fn chunk_presences_are_stored_with_the_guild_of_the_chunk() {
    let Some(mut conn) = redis(5).await else {
        return;
    };
    let mut chunk = event(
        "GUILD_MEMBERS_CHUNK",
        json!({
            "guild_id": "1000", "members": [member(10, &[])], "chunk_index": 0,
            "chunk_count": 1, "not_found": [],
            "presences": [presence(GUILD, 10, "online"), presence(GUILD, 11, "offline")],
        }),
    );
    let Event::MemberChunk(data) = &mut chunk else {
        panic!("not a member chunk");
    };
    for listed in &mut data.presences {
        listed.guild_id = Id::new(1);
    }
    let state = state_all();
    cache::update(&mut conn, &mut chunk, &context(&state))
        .await
        .unwrap();

    let stored = get(&mut conn, "presence:1000:10").await.unwrap();
    assert_eq!(stored["guild_id"], "1000");
    assert!(!exists(&mut conn, "presence:1000:11").await);
}
