#![recursion_limit = "256"]

//! Data contracts, pinned with insta snapshots (`tests/snapshots/contract__*.snap`).
//!
//! Everything here is something a consumer or an operator reads: the Redis
//! cache after every cache-relevant gateway event (every key, its type and its
//! value), the messages published to the broker, the session and status keys,
//! the /healthcheck bodies, the identify presence, the parsed `.env.example`
//! and the Prometheus metric names. A change to any of them fails this test
//! until the snapshot is updated on purpose:
//!
//! ```text
//! INSTA_UPDATE=unseen cargo test --test contract   # only writes new snapshots
//! INSTA_UPDATE=always cargo test --test contract   # also overwrites changed ones
//! git diff tests/snapshots   # review: this is what consumers will see
//! ```
//!
//! CI (`CI=true`) fails on a missing or different snapshot.
//!
//! Cache updates run with a fixed clock ([`NOW_MS`]), so expiry deadlines in
//! `expiry_zset` are shown relative to it (`now+60000ms`). Tests that need
//! Redis are skipped without TEST_REDIS_URL; each uses one of 15 database
//! indexes, guarded by a lock so tests sharing an index run one at a time.

mod common;

use common::*;
use redis::aio::ConnectionManager;
use serde_json::{json, Map, Value};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use twilight_dispatch::{
    cache::{self, Outcome, UpdateContext},
    config::{Config, StateConfig},
    constants::{JOIN_COLOR, LEAVE_COLOR},
    discord_log::{log_embed, DiscordLog},
    gateway::{
        self,
        payload::{self, Envelope},
        registry::{Registry, ShardHandle, ShardSnapshot},
        runner::parse_event,
        sessions,
        worker::{self, Breaker, Job, Publisher, WorkerContext},
    },
    metrics::{self, Health},
    models::{ApiResult, FormattedDateTime},
    util::Throttle,
};

/// The clock of every cache update in this file: 2026-10-20T12:00:00Z.
const NOW_MS: i64 = 1_792_497_600_000;
/// A `joined_at` one minute before [`NOW_MS`] (a new join).
const JUST_JOINED: &str = "2026-10-20T11:59:00.000000+00:00";
const PIN_TIME: &str = "2026-10-20T11:30:00.000000+00:00";

const G: u64 = 1000;

// ---------------------------------------------------------------------------
// Helpers

static DBS: [tokio::sync::Mutex<()>; 16] = [const { tokio::sync::Mutex::const_new(()) }; 16];

/// A clean Redis database that no other test of this file uses meanwhile.
async fn db(index: u8) -> Option<(tokio::sync::MutexGuard<'static, ()>, ConnectionManager)> {
    let guard = DBS[usize::from(index)].lock().await;
    let conn = redis(index).await?;
    Some((guard, conn))
}

async fn apply_at(
    conn: &mut ConnectionManager,
    state: &StateConfig,
    kind: &str,
    d: Value,
) -> Outcome {
    let mut event = event(kind, d);
    let ctx = UpdateContext {
        state,
        bot_id: Some(bot_id()),
        now_ms: NOW_MS,
    };
    cache::update(conn, &mut event, &ctx)
        .await
        .unwrap_or_else(|err| panic!("{kind} update failed: {err}"))
}

fn outcome_json(outcome: &Outcome) -> Value {
    json!({
        "old": outcome.old,
        "previous_guild": outcome.previous_guild,
        "guild_known": outcome.guild_known,
    })
}

/// An expiry deadline relative to the fixed clock.
fn deadline(score: f64) -> String {
    let offset = score as i64 - NOW_MS;
    if offset >= 0 {
        format!("now+{offset}ms")
    } else {
        format!("now-{}ms", -offset)
    }
}

/// Every key of the database with its type and value: strings parsed as JSON
/// (`raw` when they are not JSON), sets sorted, sorted-set scores as
/// deadlines relative to [`NOW_MS`], hashes sorted. Keys with a Redis TTL get
/// a rounded `ttl`.
async fn dump(conn: &mut ConnectionManager) -> Value {
    let mut keys: Vec<String> = redis::cmd("KEYS").arg("*").query_async(conn).await.unwrap();
    keys.sort();

    let mut out = Map::new();
    for key in keys {
        let kind: String = redis::cmd("TYPE")
            .arg(&key)
            .query_async(conn)
            .await
            .unwrap();
        let mut entry = Map::new();
        entry.insert("type".into(), json!(kind));
        match kind.as_str() {
            "string" => {
                let raw: String = redis::cmd("GET").arg(&key).query_async(conn).await.unwrap();
                match serde_json::from_str::<Value>(&raw) {
                    Ok(value) => entry.insert("value".into(), value),
                    Err(_) => entry.insert("raw".into(), json!(raw)),
                };
            }
            "set" => {
                let mut members: Vec<String> = redis::cmd("SMEMBERS")
                    .arg(&key)
                    .query_async(conn)
                    .await
                    .unwrap();
                members.sort();
                entry.insert("value".into(), json!(members));
            }
            "zset" => {
                let scored: Vec<(String, f64)> = redis::cmd("ZRANGE")
                    .arg(&key)
                    .arg(0)
                    .arg(-1)
                    .arg("WITHSCORES")
                    .query_async(conn)
                    .await
                    .unwrap();
                let members: BTreeMap<String, String> = scored
                    .into_iter()
                    .map(|(member, score)| (member, deadline(score)))
                    .collect();
                entry.insert("value".into(), json!(members));
            }
            "hash" => {
                let fields: BTreeMap<String, String> = redis::cmd("HGETALL")
                    .arg(&key)
                    .query_async(conn)
                    .await
                    .unwrap();
                entry.insert("value".into(), json!(fields));
            }
            "list" => {
                let items: Vec<String> = redis::cmd("LRANGE")
                    .arg(&key)
                    .arg(0)
                    .arg(-1)
                    .query_async(conn)
                    .await
                    .unwrap();
                entry.insert("value".into(), json!(items));
            }
            _ => {}
        }
        let pttl: i64 = redis::cmd("PTTL")
            .arg(&key)
            .query_async(conn)
            .await
            .unwrap();
        if pttl >= 0 {
            entry.insert(
                "ttl".into(),
                json!(format!("~{}min", (pttl + 59_999) / 60_000)),
            );
        }
        out.insert(key, Value::Object(entry));
    }
    Value::Object(out)
}

type Step = (&'static str, Value);

/// Apply `setup` (not shown in the snapshot), then `events`, and snapshot the
/// outcome of every event plus the whole database.
async fn cache_contract(
    name: &str,
    index: u8,
    state: &StateConfig,
    setup: Vec<Step>,
    events: Vec<Step>,
) {
    let Some((_guard, mut conn)) = db(index).await else {
        return;
    };
    for (kind, d) in setup {
        apply_at(&mut conn, state, kind, d).await;
    }
    let mut outcomes = Vec::new();
    for (kind, d) in events {
        let outcome = apply_at(&mut conn, state, kind, d).await;
        outcomes.push(json!({"event": kind, "outcome": outcome_json(&outcome)}));
    }
    let contract = json!({"events": outcomes, "redis": dump(&mut conn).await});
    insta::assert_json_snapshot!(name, contract);
}

fn guild_create() -> Step {
    ("GUILD_CREATE", guild(G, "g"))
}

fn message_create(id: u64, channel_id: u64, content: &str) -> Step {
    ("MESSAGE_CREATE", message(id, channel_id, Some(G), content))
}

/// GUILD_UPDATE's `d`: a guild without the GUILD_CREATE extra fields.
fn guild_update_payload() -> Value {
    let mut update = guild(G, "renamed");
    let object = update.as_object_mut().unwrap();
    for field in [
        "channels",
        "threads",
        "members",
        "presences",
        "voice_states",
        "joined_at",
        "large",
        "member_count",
        "unavailable",
        "stage_instances",
        "guild_scheduled_events",
        "soundboard_sounds",
    ] {
        object.remove(field);
    }
    object.insert(
        "roles".into(),
        json!([role(G, "@everyone"), role(G + 6, "new role")]),
    );
    object.insert("emojis".into(), json!([emoji(G + 2, "pog2")]));
    object.insert("icon".into(), json!("a_0123456789abcdef0123456789abcdef"));
    update
}

fn member_update(user_id: u64, nick: &str, joined_at: Option<&str>) -> Value {
    json!({
        "guild_id": G.to_string(), "roles": [], "user": user(user_id, &format!("user{user_id}")),
        "nick": nick, "avatar": null, "joined_at": joined_at, "premium_since": null,
        "pending": false, "communication_disabled_until": null,
    })
}

// ---------------------------------------------------------------------------
// Cache: guilds

#[test]
fn fixed_clock_matches_its_timestamps() {
    let now = time::macros::datetime!(2026-10-20 12:00 UTC);
    assert_eq!(now.unix_timestamp() * 1000, NOW_MS);
    let joined =
        time::OffsetDateTime::parse(JUST_JOINED, &time::format_description::well_known::Rfc3339)
            .unwrap();
    assert_eq!((now - joined).whole_seconds(), 60);
}

#[tokio::test]
async fn cache_guild_create() {
    cache_contract(
        "cache_guild_create",
        1,
        &state_all(),
        vec![],
        vec![guild_create()],
    )
    .await;
}

#[tokio::test]
async fn cache_guild_create_minimal_state() {
    // STATE_ENABLED only: no members (except the bot), presences, emojis,
    // voice states or `old`.
    let state = StateConfig {
        enabled: true,
        ..StateConfig::default()
    };
    cache_contract(
        "cache_guild_create_minimal_state",
        2,
        &state,
        vec![],
        vec![guild_create()],
    )
    .await;
}

#[tokio::test]
async fn cache_guild_create_resent() {
    // A re-sent GUILD_CREATE (e.g. after a new session) replaces the guild;
    // channels it no longer lists go with their messages, the others keep them.
    let mut resent = guild(G, "g2");
    resent["channels"] = json!([channel(G + 3, None, "general")]);
    cache_contract(
        "cache_guild_create_resent",
        3,
        &state_all(),
        vec![
            guild_create(),
            message_create(2001, G + 3, "in general"),
            message_create(2002, G + 4, "in random"),
            message_create(2003, G + 5, "in the thread"),
        ],
        vec![("GUILD_CREATE", resent)],
    )
    .await;
}

#[tokio::test]
async fn cache_guild_create_unavailable() {
    cache_contract(
        "cache_guild_create_unavailable",
        4,
        &state_all(),
        vec![],
        vec![("GUILD_CREATE", json!({"id": "2000", "unavailable": true}))],
    )
    .await;
}

#[tokio::test]
async fn cache_guild_create_unavailable_known() {
    cache_contract(
        "cache_guild_create_unavailable_known",
        5,
        &state_all(),
        vec![guild_create()],
        vec![(
            "GUILD_CREATE",
            json!({"id": G.to_string(), "unavailable": true}),
        )],
    )
    .await;
}

#[tokio::test]
async fn cache_guild_update() {
    cache_contract(
        "cache_guild_update",
        6,
        &state_all(),
        vec![guild_create()],
        vec![("GUILD_UPDATE", guild_update_payload())],
    )
    .await;
}

#[tokio::test]
async fn cache_guild_delete_outage() {
    cache_contract(
        "cache_guild_delete_outage",
        7,
        &state_all(),
        vec![guild_create(), message_create(2001, G + 3, "kept")],
        vec![(
            "GUILD_DELETE",
            json!({"id": G.to_string(), "unavailable": true}),
        )],
    )
    .await;
}

#[tokio::test]
async fn cache_guild_delete_removal() {
    // The bot left or was removed: nothing of the guild may remain.
    cache_contract(
        "cache_guild_delete_removal",
        8,
        &state_all(),
        vec![
            guild_create(),
            message_create(2001, G + 3, "in general"),
            message_create(2003, G + 5, "in the thread"),
        ],
        vec![("GUILD_DELETE", json!({"id": G.to_string()}))],
    )
    .await;
}

// ---------------------------------------------------------------------------
// Cache: channels and threads

#[tokio::test]
async fn cache_channel_create() {
    cache_contract(
        "cache_channel_create",
        9,
        &state_all(),
        vec![guild_create()],
        vec![
            ("CHANNEL_CREATE", channel(1010, Some(G), "new")),
            (
                "CHANNEL_CREATE",
                json!({"id": "3000", "type": 1, "last_message_id": null,
                    "recipients": [user(20, "friend")]}),
            ),
        ],
    )
    .await;
}

#[tokio::test]
async fn cache_channel_update() {
    let mut renamed = channel(G + 3, Some(G), "renamed");
    renamed["topic"] = json!("a topic");
    cache_contract(
        "cache_channel_update",
        10,
        &state_all(),
        vec![guild_create()],
        vec![("CHANNEL_UPDATE", renamed)],
    )
    .await;
}

#[tokio::test]
async fn cache_channel_delete() {
    // The channel goes with its messages and its threads (and their messages).
    cache_contract(
        "cache_channel_delete",
        11,
        &state_all(),
        vec![
            guild_create(),
            message_create(2001, G + 3, "in general"),
            message_create(2002, G + 4, "in random"),
            message_create(2003, G + 5, "in the thread"),
        ],
        vec![("CHANNEL_DELETE", channel(G + 3, Some(G), "general"))],
    )
    .await;
}

#[tokio::test]
async fn cache_channel_pins_update() {
    let pins = |channel_id: u64| {
        json!({"guild_id": G.to_string(), "channel_id": channel_id.to_string(),
            "last_pin_timestamp": PIN_TIME})
    };
    cache_contract(
        "cache_channel_pins_update",
        12,
        &state_all(),
        vec![guild_create()],
        vec![
            ("CHANNEL_PINS_UPDATE", pins(G + 3)),
            // A thread: stored under thread:<guild>:<thread>.
            ("CHANNEL_PINS_UPDATE", pins(G + 5)),
            // Not cached: nothing is written.
            ("CHANNEL_PINS_UPDATE", pins(1099)),
        ],
    )
    .await;
}

#[tokio::test]
async fn cache_thread_create() {
    let mut created = thread(1011, G, G + 4);
    created["newly_created"] = json!(true);
    cache_contract(
        "cache_thread_create",
        13,
        &state_all(),
        vec![guild_create()],
        vec![("THREAD_CREATE", created)],
    )
    .await;
}

#[tokio::test]
async fn cache_thread_update() {
    let mut updated = thread(G + 5, G, G + 3);
    updated["name"] = json!("renamed thread");
    updated["message_count"] = json!(5);
    cache_contract(
        "cache_thread_update",
        14,
        &state_all(),
        vec![guild_create()],
        vec![("THREAD_UPDATE", updated)],
    )
    .await;
}

#[tokio::test]
async fn cache_thread_update_archived() {
    // Only active threads are cached: archiving removes it and its messages.
    let mut archived = thread(G + 5, G, G + 3);
    archived["thread_metadata"]["archived"] = json!(true);
    cache_contract(
        "cache_thread_update_archived",
        15,
        &state_all(),
        vec![guild_create(), message_create(2003, G + 5, "in the thread")],
        vec![("THREAD_UPDATE", archived)],
    )
    .await;
}

#[tokio::test]
async fn cache_thread_delete() {
    cache_contract(
        "cache_thread_delete",
        1,
        &state_all(),
        vec![guild_create(), message_create(2003, G + 5, "in the thread")],
        vec![(
            "THREAD_DELETE",
            json!({"id": (G + 5).to_string(), "guild_id": G.to_string(),
                "parent_id": (G + 3).to_string(), "type": 11}),
        )],
    )
    .await;
}

/// A THREAD_LIST_SYNC thread without `guild_id` (filled in from the event).
fn synced_thread(id: u64, parent: u64) -> Value {
    let mut synced = thread(id, G, parent);
    synced.as_object_mut().unwrap().remove("guild_id");
    synced
}

#[tokio::test]
async fn cache_thread_list_sync_guild() {
    // No channel_ids: the list is every active thread of the guild.
    cache_contract(
        "cache_thread_list_sync_guild",
        2,
        &state_all(),
        vec![guild_create()],
        vec![(
            "THREAD_LIST_SYNC",
            json!({"guild_id": G.to_string(), "threads": [synced_thread(1012, G + 4)],
                "members": []}),
        )],
    )
    .await;
}

#[tokio::test]
async fn cache_thread_list_sync_channels() {
    // channel_ids: only threads under those channels are synced; thread 1005
    // (under 1003) is kept.
    cache_contract(
        "cache_thread_list_sync_channels",
        3,
        &state_all(),
        vec![guild_create()],
        vec![(
            "THREAD_LIST_SYNC",
            json!({"guild_id": G.to_string(), "channel_ids": [(G + 4).to_string()],
                "threads": [synced_thread(1012, G + 4)], "members": []}),
        )],
    )
    .await;
}

// ---------------------------------------------------------------------------
// Cache: roles, emojis, stickers

#[tokio::test]
async fn cache_guild_roles() {
    let guild_id = G.to_string();
    cache_contract(
        "cache_guild_roles",
        4,
        &state_all(),
        vec![guild_create()],
        vec![
            (
                "GUILD_ROLE_CREATE",
                json!({"guild_id": guild_id, "role": role(1006, "new")}),
            ),
            (
                "GUILD_ROLE_UPDATE",
                json!({"guild_id": guild_id, "role": role(G + 1, "admins")}),
            ),
            (
                "GUILD_ROLE_DELETE",
                json!({"guild_id": guild_id, "role_id": "1006"}),
            ),
        ],
    )
    .await;
}

#[tokio::test]
async fn cache_guild_emojis_update() {
    // The list replaces the cached emojis: 1002 goes, 1020 comes.
    cache_contract(
        "cache_guild_emojis_update",
        5,
        &state_all(),
        vec![guild_create()],
        vec![(
            "GUILD_EMOJIS_UPDATE",
            json!({"guild_id": G.to_string(), "emojis": [emoji(1020, "new")]}),
        )],
    )
    .await;
}

#[tokio::test]
async fn cache_guild_stickers_update() {
    let sticker = json!({
        "id": "1030", "name": "wave", "description": "hello", "tags": "wave",
        "type": 2, "format_type": 1, "available": true, "guild_id": G.to_string(),
    });
    cache_contract(
        "cache_guild_stickers_update",
        6,
        &state_all(),
        vec![guild_create()],
        vec![(
            "GUILD_STICKERS_UPDATE",
            json!({"guild_id": G.to_string(), "stickers": [sticker]}),
        )],
    )
    .await;
}

// ---------------------------------------------------------------------------
// Cache: members

#[tokio::test]
async fn cache_guild_member_add() {
    // A new join: cached with an expiry, and member_count goes up.
    let mut added = member(20, &[G + 1]);
    added["guild_id"] = json!(G.to_string());
    added["joined_at"] = json!(JUST_JOINED);
    cache_contract(
        "cache_guild_member_add",
        7,
        &state_all(),
        vec![guild_create()],
        vec![("GUILD_MEMBER_ADD", added)],
    )
    .await;
}

#[tokio::test]
async fn cache_guild_member_add_resent() {
    // Discord re-sends MEMBER_ADD for existing members (old joined_at): the
    // member is rewritten but member_count does not change.
    let mut resent = member(10, &[G + 1]);
    resent["guild_id"] = json!(G.to_string());
    resent["nick"] = json!("resent");
    cache_contract(
        "cache_guild_member_add_resent",
        8,
        &state_all(),
        vec![guild_create()],
        vec![("GUILD_MEMBER_ADD", resent)],
    )
    .await;
}

#[tokio::test]
async fn cache_guild_member_update() {
    cache_contract(
        "cache_guild_member_update",
        9,
        &state_all(),
        vec![guild_create()],
        vec![
            // Cached: merged; deaf / mute / flags / joined_at survive nulls.
            (
                "GUILD_MEMBER_UPDATE",
                member_update(10, "nick", Some("2024-01-01T00:00:00.000000+00:00")),
            ),
            // Not cached: inserted with defaults for what MEMBER_UPDATE lacks.
            ("GUILD_MEMBER_UPDATE", member_update(30, "new", None)),
            // The bot never expires.
            (
                "GUILD_MEMBER_UPDATE",
                member_update(BOT_ID, "botnick", Some("2024-01-01T00:00:00.000000+00:00")),
            ),
        ],
    )
    .await;
}

#[tokio::test]
async fn cache_guild_member_remove() {
    cache_contract(
        "cache_guild_member_remove",
        10,
        &state_all(),
        vec![guild_create()],
        vec![(
            "GUILD_MEMBER_REMOVE",
            json!({"guild_id": G.to_string(), "user": user(10, "user10")}),
        )],
    )
    .await;
}

#[tokio::test]
async fn cache_guild_members_chunk() {
    cache_contract(
        "cache_guild_members_chunk",
        11,
        &state_all(),
        vec![guild_create()],
        vec![(
            "GUILD_MEMBERS_CHUNK",
            json!({
                "guild_id": G.to_string(), "members": [member(10, &[]), member(40, &[])],
                "chunk_index": 0, "chunk_count": 1, "not_found": [],
                "presences": [presence(G, 40, "idle"), presence(G, 10, "offline")],
                "nonce": "n1",
            }),
        )],
    )
    .await;
}

// ---------------------------------------------------------------------------
// Cache: messages

#[tokio::test]
async fn cache_message_create() {
    cache_contract(
        "cache_message_create",
        12,
        &state_all(),
        vec![guild_create()],
        vec![
            message_create(2001, G + 3, "hello"),
            ("MESSAGE_CREATE", message(2002, 3000, None, "a DM")),
        ],
    )
    .await;
}

#[tokio::test]
async fn cache_message_update() {
    let mut original = message(2001, G + 3, Some(G), "hello");
    original["tts"] = json!(true);
    let mut edited = message(2001, G + 3, Some(G), "edited");
    edited["edited_timestamp"] = json!(JUST_JOINED);
    cache_contract(
        "cache_message_update",
        13,
        &state_all(),
        vec![guild_create(), ("MESSAGE_CREATE", original)],
        vec![
            // MESSAGE_UPDATE's tts is always false; the cached one is kept.
            ("MESSAGE_UPDATE", edited),
            // Not cached: ignored.
            (
                "MESSAGE_UPDATE",
                message(2009, G + 3, Some(G), "never seen"),
            ),
        ],
    )
    .await;
}

#[tokio::test]
async fn cache_message_delete() {
    cache_contract(
        "cache_message_delete",
        14,
        &state_all(),
        vec![
            guild_create(),
            message_create(2001, G + 3, "first"),
            message_create(2002, G + 3, "second"),
        ],
        vec![(
            "MESSAGE_DELETE",
            json!({"id": "2001", "channel_id": (G + 3).to_string(), "guild_id": G.to_string()}),
        )],
    )
    .await;
}

#[tokio::test]
async fn cache_message_delete_bulk() {
    cache_contract(
        "cache_message_delete_bulk",
        15,
        &state_all(),
        vec![
            guild_create(),
            message_create(2001, G + 3, "first"),
            message_create(2002, G + 3, "second"),
            message_create(2003, G + 3, "third"),
        ],
        vec![(
            "MESSAGE_DELETE_BULK",
            json!({"ids": ["2001", "2002", "2099"], "channel_id": (G + 3).to_string(),
                "guild_id": G.to_string()}),
        )],
    )
    .await;
}

// ---------------------------------------------------------------------------
// Cache: presences and voice states

#[tokio::test]
async fn cache_presence_update() {
    let mut full = presence(G, 10, "idle");
    full["user"] = user(10, "user10");
    cache_contract(
        "cache_presence_update",
        1,
        &state_all(),
        vec![guild_create()],
        vec![
            ("PRESENCE_UPDATE", full),
            // A partial user ({"id"}) keeps the cached full user.
            ("PRESENCE_UPDATE", presence(G, 10, "dnd")),
        ],
    )
    .await;
}

#[tokio::test]
async fn cache_presence_update_offline() {
    cache_contract(
        "cache_presence_update_offline",
        2,
        &state_all(),
        vec![guild_create()],
        vec![("PRESENCE_UPDATE", presence(G, 10, "offline"))],
    )
    .await;
}

#[tokio::test]
async fn cache_voice_state_update() {
    let mut joined = voice_state(Some(G), 20, Some(G + 4));
    joined["member"] = member(20, &[]);
    cache_contract(
        "cache_voice_state_update",
        3,
        &state_all(),
        vec![guild_create()],
        vec![
            ("VOICE_STATE_UPDATE", joined),
            // Leaving (no channel) removes the voice state.
            ("VOICE_STATE_UPDATE", voice_state(Some(G), 10, None)),
            // Not in a guild: not cached.
            ("VOICE_STATE_UPDATE", voice_state(None, 21, Some(3000))),
        ],
    )
    .await;
}

// ---------------------------------------------------------------------------
// Cache: READY and the bot user

#[tokio::test]
async fn cache_ready() {
    cache_contract(
        "cache_ready",
        4,
        &state_all(),
        vec![],
        vec![("READY", ready([0, 1], &[G, 2000]))],
    )
    .await;
}

#[tokio::test]
async fn cache_ready_reconciles() {
    // Shard 0 of 2. Guild ids >> 22: 1000 -> 0, 8388608 -> 2 and 16777216 -> 4
    // are on shard 0; 4194304 -> 1 is on shard 1.
    // - 1000: listed and cached in full -> kept as it is (no stub over it);
    // - 8388608: cached, on this shard, not listed -> removed with its keys;
    // - 4194304: another shard's guild -> untouched;
    // - 16777216: listed, not cached -> stub.
    let stale = 8_388_608u64;
    cache_contract(
        "cache_ready_reconciles",
        5,
        &state_all(),
        vec![
            guild_create(),
            ("GUILD_CREATE", guild(stale, "stale")),
            (
                "GUILD_CREATE",
                json!({"id": "4194304", "unavailable": true}),
            ),
        ],
        vec![("READY", ready([0, 2], &[G, 16_777_216]))],
    )
    .await;
}

#[tokio::test]
async fn cache_user_update() {
    cache_contract(
        "cache_user_update",
        6,
        &state_all(),
        vec![("READY", ready([0, 1], &[]))],
        vec![(
            "USER_UPDATE",
            json!({
                "id": BOT_ID.to_string(), "username": "renamed-bot", "discriminator": "0",
                "global_name": "Bot", "avatar": null, "bot": true, "mfa_enabled": true,
                "verified": true, "flags": 0, "locale": "en-US", "premium_type": 0,
                "public_flags": 0,
            }),
        )],
    )
    .await;
}

#[tokio::test]
async fn cache_without_old() {
    // STATE_OLD=false: no `old` in any outcome (so none is published), but
    // GUILD_CREATE still reports the cached guild for the join / leave logic.
    let state = StateConfig {
        old: false,
        ..state_all()
    };
    let mut resent = guild(G, "g2");
    resent["channels"] = json!([channel(G + 3, None, "general")]);
    cache_contract(
        "cache_without_old",
        7,
        &state,
        vec![guild_create(), message_create(2001, G + 3, "hello")],
        vec![
            ("CHANNEL_UPDATE", channel(G + 3, Some(G), "renamed")),
            (
                "GUILD_ROLE_UPDATE",
                json!({"guild_id": G.to_string(), "role": role(G + 1, "admins")}),
            ),
            (
                "MESSAGE_DELETE",
                json!({"id": "2001", "channel_id": (G + 3).to_string(), "guild_id": G.to_string()}),
            ),
            ("PRESENCE_UPDATE", presence(G, 10, "idle")),
            ("GUILD_CREATE", resent),
        ],
    )
    .await;
}

// ---------------------------------------------------------------------------
// Cache: expiry

#[tokio::test]
async fn cache_expiry() {
    // Members and messages expire at their deadline, with their index entries;
    // the bot's member and everything else stay.
    let Some((_guard, mut conn)) = db(8).await else {
        return;
    };
    let state = state_all();
    for (kind, d) in [guild_create(), message_create(2001, G + 3, "hello")] {
        apply_at(&mut conn, &state, kind, d).await;
    }
    let early = cache::jobs::expire_due(&mut conn, NOW_MS + 59_999)
        .await
        .unwrap();
    let removed = cache::jobs::expire_due(&mut conn, NOW_MS + 60_000)
        .await
        .unwrap();
    insta::assert_json_snapshot!(
        "cache_expiry",
        json!({
            "removed_before_deadline": early,
            "removed_at_deadline": removed,
            "redis": dump(&mut conn).await,
        })
    );
}

#[tokio::test]
async fn cache_legacy_expiry_migration() {
    // Older versions kept deadlines in the `expiry_keys` hash as JSON strings
    // of a formatted timestamp; they move to `expiry_zset` (unparseable
    // entries are dropped).
    let Some((_guard, mut conn)) = db(9).await else {
        return;
    };
    let () = redis::cmd("HSET")
        .arg("expiry_keys")
        .arg("member:1000:10")
        .arg(r#""2026-10-20T12:01:00.000000""#)
        .arg("message:1003:2001")
        .arg(r#""2026-10-20T12:00:30.5""#)
        .arg("member:1000:11")
        .arg("garbage")
        .query_async(&mut conn)
        .await
        .unwrap();
    let migrated = cache::jobs::migrate_legacy_expiry(&mut conn).await.unwrap();
    insta::assert_json_snapshot!(
        "cache_legacy_expiry_migration",
        json!({"migrated": migrated, "redis": dump(&mut conn).await})
    );
}

// ---------------------------------------------------------------------------
// Published messages

/// One published message: routing key, body, message id.
type Published = (String, Vec<u8>, Option<String>);

#[derive(Default)]
struct Recorder {
    published: Mutex<Vec<Published>>,
}

impl Publisher for Recorder {
    async fn publish(
        &self,
        routing_key: &str,
        payload: Vec<u8>,
        message_id: Option<String>,
    ) -> ApiResult<()> {
        self.published
            .lock()
            .unwrap()
            .push((routing_key.to_owned(), payload, message_id));
        Ok(())
    }
}

/// A published body: the raw prefix up to `d` (field order), whether `d` is
/// Discord's bytes unchanged, and the parsed body.
fn published_json(body: &[u8], d: &str) -> Value {
    let text = std::str::from_utf8(body).expect("published bodies are UTF-8");
    let d_at = text.find(r#""d":"#).expect("a d field") + 4;
    json!({
        "prefix": &text[..d_at],
        "d_is_verbatim": text[d_at..].starts_with(d),
        "body": serde_json::from_str::<Value>(text).expect("published bodies are JSON"),
    })
}

#[tokio::test]
async fn published_messages() {
    // What the `gateway` exchange receives from the workers: routing key,
    // message id (shard:session:seq, the dedup key) and body with `old`.
    let Some((_guard, mut conn)) = db(7).await else {
        return;
    };
    let state = state_all();
    apply_at(&mut conn, &state, "GUILD_CREATE", guild(G, "g")).await;

    let handle = Arc::new(ShardHandle::new(0, None, Some(1)));
    let registry = Arc::new(Registry::new([handle.clone()]));
    let recorder = Arc::new(Recorder::default());
    let ctx = WorkerContext {
        redis: conn.clone(),
        publisher: recorder.clone(),
        registry,
        state,
        bot_id: bot_id(),
        cache_timeout: Duration::from_secs(5),
        discord: DiscordLog::disabled(),
        breaker: Breaker::default(),
        cache_errors: Throttle::default(),
        publish_errors: Throttle::default(),
    };

    let mut edited = message(2001, G + 3, Some(G), "edited");
    edited["edited_timestamp"] = json!(JUST_JOINED);
    let jobs: Vec<(&str, Value, bool, Option<&str>)> = vec![
        (
            "MESSAGE_CREATE",
            message(2001, G + 3, Some(G), "hello"),
            true,
            Some("s1"),
        ),
        ("MESSAGE_UPDATE", edited, true, Some("s1")),
        (
            "CHANNEL_UPDATE",
            channel(G + 3, Some(G), "renamed"),
            true,
            Some("s1"),
        ),
        (
            "GUILD_ROLE_DELETE",
            json!({"guild_id": G.to_string(), "role_id": (G + 1).to_string()}),
            true,
            Some("s1"),
        ),
        // Not cached, and no session known (e.g. right after a restart).
        (
            "TYPING_START",
            json!({"channel_id": (G + 3).to_string(), "guild_id": G.to_string(),
                "user_id": "10", "timestamp": NOW_MS / 1000}),
            false,
            None,
        ),
    ];

    let mut sent = Vec::new();
    for (index, (kind, d, typed, session)) in jobs.into_iter().enumerate() {
        let seq = index as u64 + 2;
        let json = raw(kind, seq, d.clone());
        let event = if typed {
            parse_event(&json).unwrap()
        } else {
            None
        };
        worker::process(
            &ctx,
            &mut conn,
            Job {
                shard: 0,
                seq: Some(seq),
                token: handle.dispatch(seq),
                session: session.map(Arc::from),
                json,
                event,
            },
        )
        .await;
        sent.push(d.to_string());
    }

    let published = recorder.published.lock().unwrap();
    assert_eq!(published.len(), sent.len(), "every job is published");
    let messages: Vec<Value> = published
        .iter()
        .zip(&sent)
        .map(|((routing_key, body, message_id), d)| {
            json!({
                "routing_key": routing_key,
                "message_id": message_id,
                "payload": published_json(body, d),
            })
        })
        .collect();
    insta::assert_json_snapshot!("published_messages", messages);
}

#[test]
fn payload_outgoing() {
    // `d` is passed through byte for byte (spacing, key order and escapes
    // included); only op, t, d and old are published (never s).
    let raw = r#"{"t":"MESSAGE_CREATE","s":42,"op":0,"d":{"id":"5", "content":"caf\u00e9 \"q\"","channel_id":"3","guild_id":"7"}}"#;
    let envelope = Envelope::parse(raw).unwrap();
    let without_old = String::from_utf8(payload::outgoing(&envelope, None).unwrap()).unwrap();
    let old = json!({"id": "5", "content": "before", "channel_id": "3"});
    let with_old = String::from_utf8(payload::outgoing(&envelope, Some(&old)).unwrap()).unwrap();

    insta::assert_snapshot!(
        "payload_outgoing_raw",
        format!("without old:\n{without_old}\nwith old:\n{with_old}\n")
    );
    let parse = |text: &str| serde_json::from_str::<Value>(text).unwrap();
    insta::assert_json_snapshot!(
        "payload_outgoing_parsed",
        json!({"without_old": parse(&without_old), "with_old": parse(&with_old)})
    );
}

// ---------------------------------------------------------------------------
// Sessions and statuses

fn session_handle(id: u32, session: Option<&str>, seq: u64, resume_url: bool) -> Arc<ShardHandle> {
    let handle = ShardHandle::new(id, None, Some(seq));
    if let Some(session) = session {
        handle.update(|s| {
            s.session_id = Some(session.to_owned());
            if resume_url {
                s.resume_url = Some(format!("wss://gateway-us-east1-{id}.discord.gg"));
            }
        });
    }
    Arc::new(handle)
}

#[tokio::test]
async fn sessions_saved() {
    // gateway_session:<shard> (with a TTL) for resumable shards, and the
    // legacy gateway_sessions blob merged with other processes' shards.
    let Some((_guard, mut conn)) = db(8).await else {
        return;
    };
    let () = redis::cmd("SET")
        .arg("gateway_sessions")
        .arg(
            r#"{"5":{"session_id":"other-process","sequence":9},"1":{"session_id":"stale","sequence":3}}"#,
        )
        .query_async(&mut conn)
        .await
        .unwrap();
    let registry = Registry::new([
        session_handle(0, Some("abc"), 42, true),
        session_handle(1, None, 0, false),
        session_handle(2, Some("def"), 7, false),
    ]);
    let stored = sessions::save(&mut conn, &registry).await.unwrap();
    insta::assert_json_snapshot!(
        "sessions_saved",
        json!({"stored": stored, "redis": dump(&mut conn).await})
    );
}

#[test]
fn gateway_statuses() {
    // The JSON written to gateway_statuses every second, sorted by shard.
    let connected = ShardHandle::new(0, None, Some(1));
    connected.update(|s| {
        s.status = "Connected";
        s.active = true;
        s.latency_ms = 42;
        s.last_ack = Some(Instant::now());
    });
    let resuming = ShardHandle::new(2, None, None);
    resuming.update(|s| s.status = "Resuming");
    let fresh = ShardHandle::new(1, None, None);
    let registry = Registry::new([Arc::new(resuming), Arc::new(connected), Arc::new(fresh)]);

    let statuses = serde_json::to_value(sessions::statuses(&registry)).unwrap();
    insta::assert_json_snapshot!("gateway_statuses", statuses, {
        "[].last_ack" => insta::dynamic_redaction(|value, _path| {
            let text = value.as_str().expect("last_ack is a string").to_owned();
            assert!(
                FormattedDateTime::parse(&text).is_some(),
                "last_ack {text:?} is not YYYY-MM-DDTHH:MM:SS.fffffffff"
            );
            "[YYYY-MM-DDTHH:MM:SS.f]"
        }),
    });
}

// ---------------------------------------------------------------------------
// /healthcheck

#[test]
fn healthcheck_bodies() {
    let cases = [
        ("ok", Health::default()),
        (
            "warnings",
            Health {
                problems: vec![],
                warnings: vec!["not connected to the AMQP broker".into()],
            },
        ),
        (
            "problems",
            Health {
                problems: vec!["shard 3 is connected but received nothing for 600s".into()],
                warnings: vec![
                    "shard 1 was closed by Discord with a non-reconnectable code (4014)".into(),
                ],
            },
        ),
    ];
    let bodies: Vec<Value> = cases
        .iter()
        .map(|(case, health)| {
            let (status, body) = metrics::health_body(health);
            json!({
                "case": case,
                "status": status.as_u16(),
                "raw": body,
                "body": serde_json::from_str::<Value>(&body).unwrap(),
            })
        })
        .collect();
    insta::assert_json_snapshot!("healthcheck_bodies", bodies);
}

#[test]
fn health_messages() {
    // The texts operators see in /healthcheck (and alerts are written for).
    let now = Instant::now();
    let ago = |secs: u64| {
        Some(
            now.checked_sub(Duration::from_secs(secs))
                .expect("the monotonic clock started minutes ago"),
        )
    };
    let snapshots = vec![
        (
            0,
            ShardSnapshot {
                status: "Connected",
                active: true,
                last_message: ago(10),
                ..ShardSnapshot::default()
            },
        ),
        (
            1,
            ShardSnapshot {
                status: "FatallyClosed",
                fatal: true,
                close_code: Some(4014),
                ..ShardSnapshot::default()
            },
        ),
        (
            2,
            ShardSnapshot {
                status: "FatallyClosed",
                fatal: true,
                ..ShardSnapshot::default()
            },
        ),
        (
            3,
            ShardSnapshot {
                status: "Connected",
                active: true,
                last_message: ago(240),
                ..ShardSnapshot::default()
            },
        ),
        (
            4,
            ShardSnapshot {
                close_wait_since: ago(150),
                ..ShardSnapshot::default()
            },
        ),
        (
            5,
            ShardSnapshot {
                last_message: ago(240),
                ..ShardSnapshot::default()
            },
        ),
    ];
    let mut health = metrics::compute_health(&snapshots, false, Duration::from_secs(180));
    // The silence is measured while computing; pin the format, not the clock.
    for problem in &mut health.problems {
        if let Some(rest) = problem.strip_prefix("shard 3 is connected but received nothing for ") {
            let secs: u64 = rest.trim_end_matches('s').parse().unwrap();
            assert!((240..245).contains(&secs), "{problem}");
            *problem = problem.replace(&format!("for {secs}s"), "for 240s");
        }
    }
    insta::assert_json_snapshot!(
        "health_messages",
        json!({"problems": health.problems, "warnings": health.warnings})
    );
}

// ---------------------------------------------------------------------------
// Configuration and identify

fn env_example() -> HashMap<String, String> {
    include_str!("../.env.example")
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .collect()
}

/// The shipped `.env.example` (with a bot token) plus `overrides`.
fn config_with(overrides: &[(&str, &str)]) -> Config {
    let mut env = env_example();
    env.insert("BOT_TOKEN".into(), "token".into());
    for (name, value) in overrides {
        env.insert((*name).into(), (*value).into());
    }
    Config::from_lookup(&|name| env.get(name).cloned()).expect("config loads")
}

#[test]
fn config_env_example() {
    insta::assert_debug_snapshot!("config_env_example", config_with(&[]));
}

#[test]
fn identify_presence() {
    let cases: [(&str, &[(&str, &str)]); 6] = [
        ("env_example_playing", &[]),
        (
            "custom_status",
            &[
                ("ACTIVITY_TYPE", "4"),
                ("ACTIVITY_NAME", "Counting servers"),
            ],
        ),
        (
            "custom_status_with_state",
            &[
                ("ACTIVITY_TYPE", "4"),
                ("ACTIVITY_NAME", "Custom Status"),
                ("ACTIVITY_STATE", "Counting servers"),
            ],
        ),
        (
            "streaming",
            &[
                ("ACTIVITY_TYPE", "1"),
                ("ACTIVITY_NAME", "Live"),
                ("ACTIVITY_URL", "https://twitch.tv/topstats"),
            ],
        ),
        (
            "watching_dnd",
            &[
                ("ACTIVITY_TYPE", "3"),
                ("ACTIVITY_NAME", "servers"),
                ("STATUS", "dnd"),
            ],
        ),
        (
            "listening_idle",
            &[
                ("ACTIVITY_TYPE", "2"),
                ("ACTIVITY_NAME", "music"),
                ("STATUS", "idle"),
            ],
        ),
    ];
    let presences: BTreeMap<&str, Value> = cases
        .iter()
        .map(|(case, overrides)| {
            let presence = gateway::presence(&config_with(overrides)).expect("valid presence");
            (*case, serde_json::to_value(presence).unwrap())
        })
        .collect();
    insta::assert_json_snapshot!("identify_presence", presences);
}

// ---------------------------------------------------------------------------
// Prometheus and Discord log channel

#[test]
fn prometheus_metric_names() {
    use metrics::*;

    // Vectors only show up once they have a child.
    GATEWAY_EVENTS.with_label_values(&["READY", "0"]).inc_by(0);
    SHARD_EVENTS.with_label_values(&["Connected"]).inc_by(0);
    GUILD_EVENTS.with_label_values(&["Join"]).inc_by(0);
    GATEWAY_SHARDS.add(0);
    GATEWAY_STATUSES.with_label_values(&["Connected"]).add(0);
    GATEWAY_LATENCIES.with_label_values(&["0"]).add(0);
    for gauge in [
        &*STATE_GUILDS,
        &*STATE_CHANNELS,
        &*STATE_MESSAGES,
        &*STATE_ROLES,
        &*STATE_EMOJIS,
        &*STATE_MEMBERS,
        &*STATE_PRESENCES,
        &*STATE_VOICES,
    ] {
        gauge.add(0);
    }
    GATEWAY_PARSE_ERRORS.with_label_values(&["READY"]).inc_by(0);
    STATE_UPDATE_ERRORS.with_label_values(&["error"]).inc_by(0);
    AMQP_PUBLISH_FAILURES.inc_by(0);
    AMQP_RECONNECTS.inc_by(0);
    GATEWAY_COMMANDS.with_label_values(&["0", "sent"]).inc_by(0);
    AMQP_CONNECTED.add(0);
    SHARD_FATAL.with_label_values(&["0"]).add(0);
    SHARD_SILENCE.with_label_values(&["0"]).add(0);

    // process_* come from the prometheus crate (Linux only), not from us.
    let families: Vec<Value> = prometheus::gather()
        .iter()
        .filter(|family| !family.name().starts_with("process_"))
        .map(|family| {
            let labels: Vec<String> = family
                .get_metric()
                .first()
                .map(|metric| {
                    metric
                        .get_label()
                        .iter()
                        .map(|label| label.name().to_owned())
                        .collect()
                })
                .unwrap_or_default();
            json!({
                "name": family.name(),
                "type": format!("{:?}", family.get_field_type()),
                "labels": labels,
            })
        })
        .collect();
    insta::assert_json_snapshot!("prometheus_metric_names", families);
}

#[test]
fn discord_log_embeds() {
    let at = twilight_model::util::Timestamp::from_secs(NOW_MS / 1000).ok();
    let embeds = [
        log_embed(
            JOIN_COLOR,
            Some("Guild Join".into()),
            Some("g (1000)".into()),
            at,
        ),
        log_embed(
            LEAVE_COLOR,
            Some("Guild Leave".into()),
            Some("Unknown (1000)".into()),
            at,
        ),
    ];
    insta::assert_json_snapshot!("discord_log_embeds", embeds);
}
