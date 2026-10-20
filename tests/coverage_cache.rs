#![recursion_limit = "256"]

//! Every event arm and branch of `cache::update` that the other cache tests do
//! not reach: STATE_* switches turned off, STATE_OLD on and off, threads,
//! DM channels, stickers, unavailable guilds, uncached members and the READY
//! variants (set TEST_REDIS_URL). Each test uses its own database index.

mod common;

use common::*;
use redis::aio::ConnectionManager;
use serde_json::{json, Value};
use twilight_dispatch::{
    cache::{self, Outcome, UpdateContext},
    config::StateConfig,
    models::ApiError,
};
use twilight_model::{
    gateway::{event::Event, payload::incoming::UnavailableGuild},
    id::Id,
};

// ---------------------------------------------------------------------------
// Helpers

/// Every key of the database, sorted.
async fn keys(conn: &mut ConnectionManager) -> Vec<String> {
    let mut keys: Vec<String> = redis::cmd("KEYS").arg("*").query_async(conn).await.unwrap();
    keys.sort();
    keys
}

fn in_guild(mut value: Value, guild_id: u64) -> Value {
    value["guild_id"] = json!(guild_id.to_string());
    value
}

/// A GUILD_UPDATE `d`: a plain guild without the GUILD_CREATE extras.
fn guild_update(id: u64, name: &str) -> Value {
    let mut update = guild(id, name);
    for field in [
        "joined_at",
        "large",
        "member_count",
        "channels",
        "threads",
        "members",
        "presences",
        "voice_states",
        "unavailable",
    ] {
        update.as_object_mut().unwrap().remove(field);
    }
    update
}

fn sticker(id: u64, guild_id: u64) -> Value {
    json!({
        "id": id.to_string(), "name": format!("sticker{id}"), "tags": "tag",
        "type": 2, "format_type": 1, "description": null, "available": true,
        "guild_id": guild_id.to_string(),
    })
}

fn current_user(name: &str) -> Value {
    json!({
        "id": BOT_ID.to_string(), "username": name, "discriminator": "0",
        "global_name": null, "avatar": null, "bot": true, "mfa_enabled": false,
        "verified": true, "flags": 0,
    })
}

fn dm_channel(id: u64) -> Value {
    json!({
        "id": id.to_string(), "type": 1, "last_message_id": null,
        "recipients": [user(10, "user10")],
    })
}

/// Apply an event that was built directly (some events are never parsed from
/// a gateway payload, e.g. UNAVAILABLE_GUILD).
async fn apply_event(
    conn: &mut ConnectionManager,
    state: &StateConfig,
    mut event: Event,
) -> Outcome {
    let ctx = UpdateContext {
        state,
        bot_id: Some(bot_id()),
        now_ms: now_ms(),
    };
    cache::update(conn, &mut event, &ctx).await.unwrap()
}

fn unavailable_guild(id: u64) -> Event {
    Event::UnavailableGuild(UnavailableGuild { id: Id::new(id) })
}

fn old_off() -> StateConfig {
    StateConfig {
        old: false,
        ..state_all()
    }
}

// ---------------------------------------------------------------------------
// Tests

#[tokio::test]
async fn channels_threads_and_pins_use_their_own_keys() {
    let Some(mut conn) = redis(1).await else {
        return;
    };
    let state = state_all();

    // A guild channel is stored without its guild but indexed under it.
    apply(
        &mut conn,
        &state,
        "CHANNEL_CREATE",
        channel(2001, Some(2000), "general"),
    )
    .await;
    assert_eq!(
        get(&mut conn, "channel:2001").await.unwrap()["guild_id"],
        "2000"
    );
    assert_eq!(
        members(&mut conn, "guild_keys:2000").await,
        vec!["channel:2001"]
    );

    // A DM channel is only indexed by type.
    apply(&mut conn, &state, "CHANNEL_CREATE", dm_channel(2010)).await;
    assert_eq!(get(&mut conn, "channel:2010").await.unwrap()["type"], 1);
    assert_eq!(
        members(&mut conn, "channel_keys").await,
        vec!["channel:2001", "channel:2010"]
    );

    // THREAD_CREATE goes to thread:<guild>:<thread>, never channel:.
    apply(&mut conn, &state, "THREAD_CREATE", thread(2005, 2000, 2001)).await;
    assert!(exists(&mut conn, "thread:2000:2005").await);
    assert!(!exists(&mut conn, "channel:2005").await);
    for set in ["thread_keys", "thread_keys:2000"] {
        assert_eq!(members(&mut conn, set).await, vec!["thread:2000:2005"]);
    }
    assert!(members(&mut conn, "guild_keys:2000")
        .await
        .contains(&"thread:2000:2005".to_owned()));

    // An active THREAD_UPDATE replaces the thread and reports the old one.
    let mut renamed = thread(2005, 2000, 2001);
    renamed["name"] = json!("renamed");
    let outcome = apply(&mut conn, &state, "THREAD_UPDATE", renamed).await;
    assert_eq!(outcome.old.unwrap()["name"], "thread2005");
    assert_eq!(
        get(&mut conn, "thread:2000:2005").await.unwrap()["name"],
        "renamed"
    );

    // Pins of a thread: the channel: lookup misses, the thread key is updated.
    let pinned_at = "2026-10-20T12:00:00.000000+00:00";
    apply(
        &mut conn,
        &state,
        "CHANNEL_PINS_UPDATE",
        json!({"guild_id": "2000", "channel_id": "2005", "last_pin_timestamp": pinned_at}),
    )
    .await;
    assert_eq!(
        get(&mut conn, "thread:2000:2005").await.unwrap()["last_pin_timestamp"],
        pinned_at
    );
    assert!(!exists(&mut conn, "channel:2005").await, "no channel: copy");

    // Pins of a DM channel (no guild_id).
    apply(
        &mut conn,
        &state,
        "CHANNEL_PINS_UPDATE",
        json!({"channel_id": "2010", "last_pin_timestamp": pinned_at}),
    )
    .await;
    assert_eq!(
        get(&mut conn, "channel:2010").await.unwrap()["last_pin_timestamp"],
        pinned_at
    );

    // Pins of a channel that is not cached create nothing.
    let before = keys(&mut conn).await;
    apply(
        &mut conn,
        &state,
        "CHANNEL_PINS_UPDATE",
        json!({"guild_id": "2000", "channel_id": "2099", "last_pin_timestamp": null}),
    )
    .await;
    assert_eq!(keys(&mut conn).await, before);

    // THREAD_DELETE removes the thread, its messages and every index entry.
    apply(
        &mut conn,
        &state,
        "MESSAGE_CREATE",
        message(1, 2005, Some(2000), "in thread"),
    )
    .await;
    let outcome = apply(
        &mut conn,
        &state,
        "THREAD_DELETE",
        json!({"id": "2005", "guild_id": "2000", "parent_id": "2001", "type": 11}),
    )
    .await;
    assert_eq!(outcome.old.unwrap()["name"], "renamed");
    assert!(!exists(&mut conn, "thread:2000:2005").await);
    assert!(!exists(&mut conn, "message:2005:1").await);
    for set in [
        "thread_keys",
        "thread_keys:2000",
        "channel_keys:2005",
        "message_keys",
    ] {
        assert!(members(&mut conn, set).await.is_empty(), "{set}");
    }
    assert_eq!(score(&mut conn, "message:2005:1").await, None);
    assert_eq!(
        members(&mut conn, "guild_keys:2000").await,
        vec!["channel:2001"]
    );

    // CHANNEL_DELETE of a DM channel.
    let outcome = apply(&mut conn, &state, "CHANNEL_DELETE", dm_channel(2010)).await;
    assert_eq!(outcome.old.unwrap()["id"], "2010");
    assert!(!exists(&mut conn, "channel:2010").await);
    assert_eq!(
        members(&mut conn, "channel_keys").await,
        vec!["channel:2001"]
    );
}

#[tokio::test]
async fn thread_list_sync_without_channel_ids_covers_the_whole_guild() {
    let Some(mut conn) = redis(2).await else {
        return;
    };
    let state = state_all();
    apply(&mut conn, &state, "GUILD_CREATE", guild(1000, "g")).await;
    apply(&mut conn, &state, "THREAD_CREATE", thread(1007, 1000, 1004)).await;

    // No channel_ids: the list is every active thread of the guild. Threads
    // in it may lack guild_id; they are stored with it.
    let mut listed = thread(1008, 1000, 1004);
    listed.as_object_mut().unwrap().remove("guild_id");
    apply(
        &mut conn,
        &state,
        "THREAD_LIST_SYNC",
        json!({"guild_id": "1000", "threads": [listed], "members": []}),
    )
    .await;

    assert!(!exists(&mut conn, "thread:1000:1005").await);
    assert!(!exists(&mut conn, "thread:1000:1007").await);
    assert_eq!(
        get(&mut conn, "thread:1000:1008").await.unwrap()["guild_id"],
        "1000"
    );
    assert_eq!(
        members(&mut conn, "thread_keys:1000").await,
        vec!["thread:1000:1008"]
    );
}

#[tokio::test]
async fn unavailable_guilds_keep_what_is_cached() {
    let Some(mut conn) = redis(3).await else {
        return;
    };
    let state = state_all();

    // Never seen: a stub, indexed like a guild.
    let outcome = apply_event(&mut conn, &state, unavailable_guild(3000)).await;
    assert_eq!(outcome.old, None);
    assert_eq!(
        get(&mut conn, "guild:3000").await.unwrap(),
        json!({"id": "3000", "unavailable": true})
    );
    assert_eq!(members(&mut conn, "guild_keys").await, vec!["guild:3000"]);

    // Cached: flagged, nothing else changes.
    apply(&mut conn, &state, "GUILD_CREATE", guild(1000, "g")).await;
    let outcome = apply_event(&mut conn, &state, unavailable_guild(1000)).await;
    assert_eq!(outcome.old.unwrap()["name"], "g");
    let blob = get(&mut conn, "guild:1000").await.unwrap();
    assert_eq!(blob["unavailable"], true);
    assert_eq!(blob["name"], "g");
    assert!(exists(&mut conn, "channel:1003").await);
    assert!(exists(&mut conn, "member:1000:10").await);

    // An unavailable GUILD_CREATE for a cached guild (outage during a
    // reconnect): the guild is known, the blob is kept and flagged.
    apply(&mut conn, &state, "GUILD_CREATE", guild(1000, "g")).await;
    let outcome = apply(
        &mut conn,
        &state,
        "GUILD_CREATE",
        json!({"id": "1000", "unavailable": true}),
    )
    .await;
    assert!(outcome.guild_known);
    assert_eq!(outcome.old.as_ref().unwrap()["unavailable"], false);
    assert_eq!(outcome.previous_guild.unwrap()["name"], "g");
    let blob = get(&mut conn, "guild:1000").await.unwrap();
    assert_eq!(blob["unavailable"], true);
    assert_eq!(blob["member_count"], 2);
    assert!(exists(&mut conn, "role:1000:1001").await);

    // STATE_OLD off: still written, no old value.
    let outcome = apply_event(&mut conn, &old_off(), unavailable_guild(3001)).await;
    assert_eq!(outcome.old, None);
    assert!(exists(&mut conn, "guild:3001").await);
}

#[tokio::test]
async fn state_old_off_reports_no_old_values_but_still_writes() {
    let Some(mut conn) = redis(4).await else {
        return;
    };
    let state = old_off();
    let mut olds: Vec<(&str, Outcome)> = Vec::new();

    let first = apply(&mut conn, &state, "GUILD_CREATE", guild(1000, "g")).await;
    assert!(!first.guild_known);
    let again = apply(&mut conn, &state, "GUILD_CREATE", guild(1000, "g")).await;
    assert!(again.guild_known, "join detection does not need STATE_OLD");
    assert_eq!(again.previous_guild.as_ref().unwrap()["name"], "g");
    olds.push(("GUILD_CREATE", again));

    let mut cases: Vec<(&str, Value)> = vec![
        ("CHANNEL_UPDATE", channel(1003, Some(1000), "renamed")),
        ("THREAD_UPDATE", {
            let mut t = thread(1005, 1000, 1003);
            t["name"] = json!("renamed-thread");
            t
        }),
        ("GUILD_UPDATE", guild_update(1000, "renamed-guild")),
        (
            "GUILD_ROLE_UPDATE",
            json!({"guild_id": "1000", "role": role(1001, "renamed-role")}),
        ),
        (
            "GUILD_EMOJIS_UPDATE",
            json!({"guild_id": "1000", "emojis": [emoji(1100, "new")]}),
        ),
        (
            "GUILD_STICKERS_UPDATE",
            json!({"guild_id": "1000", "stickers": [sticker(1200, 1000)]}),
        ),
        ("GUILD_MEMBER_UPDATE", {
            let mut m = in_guild(member(10, &[1001]), 1000);
            m["nick"] = json!("nick");
            m
        }),
        ("PRESENCE_UPDATE", presence(1000, 10, "idle")),
        ("MESSAGE_CREATE", message(1, 1003, Some(1000), "a")),
        ("MESSAGE_UPDATE", message(1, 1003, Some(1000), "b")),
        ("MESSAGE_CREATE", message(2, 1003, Some(1000), "c")),
        ("MESSAGE_CREATE", message(3, 1003, Some(1000), "d")),
        (
            "MESSAGE_DELETE",
            json!({"id": "2", "channel_id": "1003", "guild_id": "1000"}),
        ),
        (
            "MESSAGE_DELETE_BULK",
            json!({"ids": ["1", "3"], "channel_id": "1003", "guild_id": "1000"}),
        ),
        ("VOICE_STATE_UPDATE", voice_state(Some(1000), 10, None)),
        ("USER_UPDATE", current_user("bot1")),
        ("USER_UPDATE", current_user("bot2")),
        (
            "GUILD_ROLE_DELETE",
            json!({"guild_id": "1000", "role_id": "1001"}),
        ),
        (
            "THREAD_DELETE",
            json!({"id": "1005", "guild_id": "1000", "parent_id": "1003", "type": 11}),
        ),
        ("CHANNEL_DELETE", channel(1004, Some(1000), "random")),
        (
            "GUILD_MEMBER_REMOVE",
            json!({"guild_id": "1000", "user": user(10, "user10")}),
        ),
    ];
    for (kind, d) in cases.drain(..) {
        let outcome = apply(&mut conn, &state, kind, d).await;
        olds.push((kind, outcome));
    }

    // Every write happened.
    assert_eq!(
        get(&mut conn, "channel:1003").await.unwrap()["name"],
        "renamed"
    );
    let blob = get(&mut conn, "guild:1000").await.unwrap();
    assert_eq!(blob["name"], "renamed-guild");
    assert_eq!(blob["stickers"][0]["id"], "1200");
    assert_eq!(blob["member_count"], 1, "the removal was counted");
    assert!(exists(&mut conn, "emoji:1000:1100").await);
    assert!(!exists(&mut conn, "emoji:1000:1002").await);
    assert!(!exists(&mut conn, "role:1000:1001").await);
    assert!(!exists(&mut conn, "thread:1000:1005").await);
    assert!(!exists(&mut conn, "channel:1004").await);
    assert!(!exists(&mut conn, "member:1000:10").await);
    assert!(!exists(&mut conn, "presence:1000:10").await);
    assert!(!exists(&mut conn, "voice:1000:10").await);
    assert!(members(&mut conn, "channel_keys:1003").await.is_empty());
    assert_eq!(
        get(&mut conn, "bot_user").await.unwrap()["username"],
        "bot2"
    );

    let outage = apply_event(&mut conn, &state, unavailable_guild(1000)).await;
    olds.push(("UNAVAILABLE_GUILD", outage));
    let outage = apply(
        &mut conn,
        &state,
        "GUILD_DELETE",
        json!({"id": "1000", "unavailable": true}),
    )
    .await;
    assert_eq!(
        outage.previous_guild.as_ref().unwrap()["name"],
        "renamed-guild"
    );
    olds.push(("GUILD_DELETE (outage)", outage));
    let left = apply(&mut conn, &state, "GUILD_DELETE", json!({"id": "1000"})).await;
    assert_eq!(
        left.previous_guild.as_ref().unwrap()["name"],
        "renamed-guild",
        "the leave log still gets the guild name"
    );
    olds.push(("GUILD_DELETE", left));
    assert!(!exists(&mut conn, "guild:1000").await);

    for (kind, outcome) in &olds {
        assert_eq!(
            outcome.old, None,
            "{kind} must not report old without STATE_OLD"
        );
    }
}

#[tokio::test]
async fn state_old_on_reports_previous_values() {
    let Some(mut conn) = redis(5).await else {
        return;
    };
    let state = state_all();
    apply(&mut conn, &state, "GUILD_CREATE", guild(1000, "g")).await;

    // Roles.
    let created = apply(
        &mut conn,
        &state,
        "GUILD_ROLE_CREATE",
        json!({"guild_id": "1000", "role": role(1100, "new")}),
    )
    .await;
    assert_eq!(created.old, None);
    assert!(members(&mut conn, "role_keys")
        .await
        .contains(&"role:1000:1100".to_owned()));
    assert!(members(&mut conn, "guild_keys:1000")
        .await
        .contains(&"role:1000:1100".to_owned()));
    let updated = apply(
        &mut conn,
        &state,
        "GUILD_ROLE_UPDATE",
        json!({"guild_id": "1000", "role": role(1100, "renamed")}),
    )
    .await;
    assert_eq!(updated.old.unwrap()["name"], "new");
    let deleted = apply(
        &mut conn,
        &state,
        "GUILD_ROLE_DELETE",
        json!({"guild_id": "1000", "role_id": "1100"}),
    )
    .await;
    assert_eq!(deleted.old.unwrap()["name"], "renamed");
    assert!(!exists(&mut conn, "role:1000:1100").await);
    assert!(!members(&mut conn, "role_keys")
        .await
        .contains(&"role:1000:1100".to_owned()));

    // The bot user.
    let first = apply(&mut conn, &state, "USER_UPDATE", current_user("bot1")).await;
    assert_eq!(first.old, None);
    let second = apply(&mut conn, &state, "USER_UPDATE", current_user("bot2")).await;
    assert_eq!(second.old.unwrap()["username"], "bot1");
    let bot_user = get(&mut conn, "bot_user").await.unwrap();
    assert_eq!(bot_user["username"], "bot2");
    assert_eq!(bot_user["id"], BOT_ID.to_string());

    // Stickers live in the guild blob; old is the previous list. twilight
    // omits an empty `stickers` from the blob, so there is no old list yet.
    assert!(get(&mut conn, "guild:1000")
        .await
        .unwrap()
        .get("stickers")
        .is_none());
    let first = apply(
        &mut conn,
        &state,
        "GUILD_STICKERS_UPDATE",
        json!({"guild_id": "1000", "stickers": [sticker(1200, 1000)]}),
    )
    .await;
    assert_eq!(first.old, None);
    let second = apply(
        &mut conn,
        &state,
        "GUILD_STICKERS_UPDATE",
        json!({"guild_id": "1000", "stickers": []}),
    )
    .await;
    assert_eq!(second.old.unwrap()[0]["name"], "sticker1200");

    // A single message delete.
    apply(
        &mut conn,
        &state,
        "MESSAGE_CREATE",
        message(1, 1003, Some(1000), "hello"),
    )
    .await;
    let deleted = apply(
        &mut conn,
        &state,
        "MESSAGE_DELETE",
        json!({"id": "1", "channel_id": "1003", "guild_id": "1000"}),
    )
    .await;
    assert_eq!(deleted.old.unwrap()["content"], "hello");
    assert!(members(&mut conn, "message_keys").await.is_empty());

    // Going offline reports the presence it removed.
    let offline = apply(
        &mut conn,
        &state,
        "PRESENCE_UPDATE",
        presence(1000, 10, "offline"),
    )
    .await;
    assert_eq!(offline.old.unwrap()["status"], "online");
    assert!(!exists(&mut conn, "presence:1000:10").await);

    // Thread update / delete with STATE_OLD.
    let mut archived = thread(1005, 1000, 1003);
    archived["thread_metadata"]["archived"] = json!(true);
    let outcome = apply(&mut conn, &state, "THREAD_UPDATE", archived).await;
    assert_eq!(
        outcome.old.unwrap()["thread_metadata"]["archived"],
        false,
        "the cached (still active) thread"
    );

    // An outage reports the blob it flagged.
    let outage = apply(
        &mut conn,
        &state,
        "GUILD_DELETE",
        json!({"id": "1000", "unavailable": true}),
    )
    .await;
    assert_eq!(outage.old.unwrap()["unavailable"], false);
    assert_eq!(outage.previous_guild.unwrap()["name"], "g");
}

#[tokio::test]
async fn guild_stickers_update_only_touches_cached_guilds() {
    let Some(mut conn) = redis(6).await else {
        return;
    };
    let state = state_all();

    let outcome = apply(
        &mut conn,
        &state,
        "GUILD_STICKERS_UPDATE",
        json!({"guild_id": "1000", "stickers": [sticker(1200, 1000)]}),
    )
    .await;
    assert_eq!(outcome, Outcome::default());
    assert!(
        keys(&mut conn).await.is_empty(),
        "no stub for an uncached guild"
    );

    apply(&mut conn, &state, "GUILD_CREATE", guild(1000, "g")).await;
    apply(
        &mut conn,
        &state,
        "GUILD_STICKERS_UPDATE",
        json!({"guild_id": "1000", "stickers": [sticker(1200, 1000), sticker(1201, 1000)]}),
    )
    .await;
    let blob = get(&mut conn, "guild:1000").await.unwrap();
    assert_eq!(blob["name"], "g", "the rest of the blob is kept");
    assert_eq!(blob["member_count"], 2);
    let stickers: Vec<&str> = blob["stickers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap())
        .collect();
    assert_eq!(stickers, ["1200", "1201"]);
    assert_eq!(blob["stickers"][0]["format_type"], 1);
}

#[tokio::test]
async fn guild_create_with_optional_state_off_writes_the_minimal_layout() {
    let Some(mut conn) = redis(7).await else {
        return;
    };
    let minimal = StateConfig {
        enabled: true,
        member: false,
        member_ttl: 60_000,
        message: false,
        message_ttl: 60_000,
        presence: false,
        emoji: false,
        voice: false,
        old: false,
    };

    // Discord may omit `unavailable` for available guilds.
    let mut create = guild(1000, "g");
    create.as_object_mut().unwrap().remove("unavailable");
    apply(&mut conn, &minimal, "GUILD_CREATE", create).await;

    // The exact key layout is a contract with the cache's consumers.
    assert_eq!(
        keys(&mut conn).await,
        [
            "channel:1003",
            "channel:1004",
            "channel_keys",
            "guild:1000",
            "guild_keys",
            "guild_keys:1000",
            "member:1000:900",
            "member_keys",
            "role:1000:1000",
            "role:1000:1001",
            "role_keys",
            "thread:1000:1005",
            "thread_keys",
            "thread_keys:1000",
        ]
    );
    assert_eq!(
        members(&mut conn, "guild_keys:1000").await,
        [
            "channel:1003",
            "channel:1004",
            "member:1000:900",
            "role:1000:1000",
            "role:1000:1001",
            "thread:1000:1005",
        ]
    );
    // The bot's own member is always kept and never expires.
    assert_eq!(score(&mut conn, "member:1000:900").await, None);
    let blob = get(&mut conn, "guild:1000").await.unwrap();
    for field in [
        "channels",
        "roles",
        "emojis",
        "members",
        "presences",
        "voice_states",
    ] {
        assert_eq!(blob[field], json!([]), "{field}");
    }
    assert_eq!(blob["unavailable"], false, "always stored, as before");

    // GUILD_UPDATE writes roles but no emojis without STATE_EMOJI.
    let mut update = guild_update(1000, "renamed");
    update["roles"] = json!([role(1000, "@everyone"), role(1010, "new")]);
    update["emojis"] = json!([emoji(1100, "e")]);
    apply(&mut conn, &minimal, "GUILD_UPDATE", update).await;
    assert!(exists(&mut conn, "role:1000:1010").await);
    assert!(!exists(&mut conn, "emoji:1000:1100").await);

    // GUILD_EMOJIS_UPDATE is ignored entirely without STATE_EMOJI.
    let outcome = apply(
        &mut conn,
        &minimal,
        "GUILD_EMOJIS_UPDATE",
        json!({"guild_id": "1000", "emojis": [emoji(1101, "x")]}),
    )
    .await;
    assert_eq!(outcome, Outcome::default());
    assert!(!members(&mut conn, "guild_keys:1000")
        .await
        .iter()
        .any(|key| key.starts_with("emoji:")));

    // With STATE_EMOJI, GUILD_UPDATE writes its emojis.
    let mut update = guild_update(1000, "renamed");
    update["emojis"] = json!([emoji(1102, "e")]);
    apply(&mut conn, &state_all(), "GUILD_UPDATE", update).await;
    assert!(exists(&mut conn, "emoji:1000:1102").await);
}

#[tokio::test]
async fn members_without_state_member_keep_only_the_bot() {
    let Some(mut conn) = redis(8).await else {
        return;
    };
    let state = StateConfig {
        member: false,
        ..state_all()
    };
    apply(&mut conn, &state, "GUILD_CREATE", guild(1000, "g")).await;
    assert!(exists(&mut conn, "member:1000:900").await);
    assert!(!exists(&mut conn, "member:1000:10").await);
    assert!(exists(&mut conn, "presence:1000:10").await);

    // Updates of other members are not cached.
    let outcome = apply(
        &mut conn,
        &state,
        "GUILD_MEMBER_UPDATE",
        in_guild(member(10, &[1001]), 1000),
    )
    .await;
    assert_eq!(outcome.old, None);
    assert!(!exists(&mut conn, "member:1000:10").await);

    // The bot's own MEMBER_ADD is cached without expiry.
    let bot = in_guild(member(BOT_ID, &[1001]), 1000);
    apply(&mut conn, &state, "GUILD_MEMBER_ADD", bot).await;
    assert_eq!(
        get(&mut conn, "member:1000:900").await.unwrap()["roles"],
        json!(["1001"])
    );
    assert_eq!(score(&mut conn, "member:1000:900").await, None);

    // Chunks: only the bot is cached; presences (which Discord sends without
    // guild_id) are stored with it.
    let mut partial = presence(1000, 12, "dnd");
    partial.as_object_mut().unwrap().remove("guild_id");
    apply(
        &mut conn,
        &state,
        "GUILD_MEMBERS_CHUNK",
        json!({
            "guild_id": "1000", "members": [member(12, &[]), member(BOT_ID, &[])],
            "chunk_index": 0, "chunk_count": 1, "not_found": [], "presences": [partial],
        }),
    )
    .await;
    assert!(!exists(&mut conn, "member:1000:12").await);
    assert_eq!(
        get(&mut conn, "member:1000:900").await.unwrap()["roles"],
        json!([])
    );
    assert_eq!(score(&mut conn, "member:1000:900").await, None);
    let cached = get(&mut conn, "presence:1000:12").await.unwrap();
    assert_eq!(cached["guild_id"], "1000");
    assert_eq!(cached["status"], "dnd");

    // A removal without STATE_MEMBER still drops the presence and the count.
    apply(
        &mut conn,
        &state,
        "GUILD_MEMBER_REMOVE",
        json!({"guild_id": "1000", "user": user(12, "user12")}),
    )
    .await;
    assert!(!exists(&mut conn, "presence:1000:12").await);
    assert_eq!(
        get(&mut conn, "guild:1000").await.unwrap()["member_count"],
        1
    );

    // Without STATE_PRESENCE, chunk presences and removals leave presences alone.
    let no_presence = StateConfig {
        member: false,
        presence: false,
        ..state_all()
    };
    apply(
        &mut conn,
        &no_presence,
        "GUILD_MEMBERS_CHUNK",
        json!({
            "guild_id": "1000", "members": [member(13, &[])], "chunk_index": 0,
            "chunk_count": 1, "not_found": [], "presences": [presence(1000, 13, "online")],
        }),
    )
    .await;
    assert!(!exists(&mut conn, "presence:1000:13").await);
    apply(
        &mut conn,
        &no_presence,
        "GUILD_MEMBER_REMOVE",
        json!({"guild_id": "1000", "user": user(10, "user10")}),
    )
    .await;
    assert!(
        exists(&mut conn, "presence:1000:10").await,
        "presences are not touched without STATE_PRESENCE"
    );

    // With STATE_MEMBER, chunk members expire and the bot does not.
    apply(
        &mut conn,
        &state_all(),
        "GUILD_MEMBERS_CHUNK",
        json!({
            "guild_id": "1000", "members": [member(14, &[]), member(BOT_ID, &[])],
            "chunk_index": 0, "chunk_count": 1, "not_found": [],
        }),
    )
    .await;
    assert_eq!(
        get(&mut conn, "member:1000:14").await.unwrap()["guild_id"],
        "1000"
    );
    assert!(score(&mut conn, "member:1000:14").await.is_some());
    assert_eq!(score(&mut conn, "member:1000:900").await, None);
}

#[tokio::test]
async fn member_update_inserts_uncached_members_with_defaults() {
    let Some(mut conn) = redis(9).await else {
        return;
    };
    let state = state_all();
    let before = now_ms();

    // MEMBER_UPDATE may lack deaf, mute and flags; an uncached member is
    // inserted with the defaults consumers expect.
    let mut update = in_guild(member(50, &[7]), 1000);
    update["deaf"] = json!(null);
    update["mute"] = json!(null);
    update.as_object_mut().unwrap().remove("flags");
    let outcome = apply(&mut conn, &state, "GUILD_MEMBER_UPDATE", update).await;
    assert_eq!(outcome.old, None);
    let cached = get(&mut conn, "member:1000:50").await.unwrap();
    assert_eq!(cached["deaf"], false);
    assert_eq!(cached["mute"], false);
    assert_eq!(cached["flags"], 0);
    assert_eq!(cached["guild_id"], "1000");
    assert_eq!(cached["roles"], json!(["7"]));
    assert_eq!(cached["joined_at"], "2024-01-01T00:00:00.000000+00:00");
    assert_eq!(cached["user"]["id"], "50");
    let deadline = score(&mut conn, "member:1000:50").await.unwrap() as i64;
    assert!(
        (before + 60_000..=now_ms() + 60_000).contains(&deadline),
        "expires member_ttl after the update: {deadline}"
    );
    assert!(members(&mut conn, "guild_keys:1000")
        .await
        .contains(&"member:1000:50".to_owned()));

    // A later update without joined_at keeps the cached one; explicit values win.
    let mut update = in_guild(member(50, &[]), 1000);
    update["joined_at"] = json!(null);
    update["deaf"] = json!(true);
    update["flags"] = json!(2);
    let outcome = apply(&mut conn, &state, "GUILD_MEMBER_UPDATE", update).await;
    assert_eq!(outcome.old.unwrap()["deaf"], false);
    let cached = get(&mut conn, "member:1000:50").await.unwrap();
    assert_eq!(cached["joined_at"], "2024-01-01T00:00:00.000000+00:00");
    assert_eq!(cached["deaf"], true);
    assert_eq!(cached["flags"], 2);
    assert_eq!(cached["roles"], json!([]));

    // MEMBER_TTL 0 means members never expire.
    let forever = StateConfig {
        member_ttl: 0,
        ..state_all()
    };
    let mut add = in_guild(member(51, &[]), 1000);
    add["joined_at"] = json!(now_iso());
    apply(&mut conn, &forever, "GUILD_MEMBER_ADD", add).await;
    assert!(exists(&mut conn, "member:1000:51").await);
    assert_eq!(score(&mut conn, "member:1000:51").await, None);
}

#[tokio::test]
async fn presence_updates_merge_partial_users_without_state_old() {
    let Some(mut conn) = redis(10).await else {
        return;
    };
    let state = old_off();

    // Not cached yet: a partial user is stored as it is.
    apply(
        &mut conn,
        &state,
        "PRESENCE_UPDATE",
        presence(1000, 50, "online"),
    )
    .await;
    assert_eq!(
        get(&mut conn, "presence:1000:50").await.unwrap()["user"],
        json!({"id": "50"})
    );

    let mut full = presence(1000, 50, "online");
    full["user"] = user(50, "fifty");
    apply(&mut conn, &state, "PRESENCE_UPDATE", full).await;

    // The cached user is still read for a partial update, even without STATE_OLD.
    let outcome = apply(
        &mut conn,
        &state,
        "PRESENCE_UPDATE",
        presence(1000, 50, "idle"),
    )
    .await;
    assert_eq!(outcome.old, None);
    let cached = get(&mut conn, "presence:1000:50").await.unwrap();
    assert_eq!(cached["status"], "idle");
    assert_eq!(cached["user"]["username"], "fifty");
    assert_eq!(cached["guild_id"], "1000");
    assert!(members(&mut conn, "guild_keys:1000")
        .await
        .contains(&"presence:1000:50".to_owned()));
}

#[tokio::test]
async fn ready_without_a_shard_keeps_every_cached_guild() {
    let Some(mut conn) = redis(11).await else {
        return;
    };
    let state = state_all();
    let (cached, listed) = (1u64 << 22, 2u64 << 22);
    apply(&mut conn, &state, "GUILD_CREATE", guild(cached, "kept")).await;

    let mut payload = ready([0, 1], &[listed]);
    payload.as_object_mut().unwrap().remove("shard");
    let outcome = apply(&mut conn, &state, "READY", payload).await;
    assert_eq!(outcome, Outcome::default());

    // Without the shard it cannot tell which cached guilds are stale.
    assert_eq!(
        get(&mut conn, &format!("guild:{cached}")).await.unwrap()["name"],
        "kept"
    );
    assert_eq!(
        get(&mut conn, &format!("guild:{listed}")).await.unwrap(),
        json!({"id": listed.to_string(), "unavailable": true})
    );
    assert_eq!(
        members(&mut conn, "guild_keys").await,
        [format!("guild:{cached}"), format!("guild:{listed}")]
    );
    assert_eq!(get(&mut conn, "bot_user").await.unwrap()["username"], "bot");

    // With the shard, the unlisted guild of shard 0/1 is dropped.
    apply(&mut conn, &state, "READY", ready([0, 1], &[listed])).await;
    assert!(!exists(&mut conn, &format!("guild:{cached}")).await);
    assert!(members(&mut conn, &format!("guild_keys:{cached}"))
        .await
        .is_empty());
}

#[tokio::test]
async fn messages_are_only_updated_while_cached() {
    let Some(mut conn) = redis(12).await else {
        return;
    };
    let state = state_all();

    // An edit of a message that is not cached does not insert it.
    let outcome = apply(
        &mut conn,
        &state,
        "MESSAGE_UPDATE",
        message(1, 3, Some(1000), "edited"),
    )
    .await;
    assert_eq!(outcome.old, None);
    assert!(keys(&mut conn).await.is_empty());

    // MESSAGE_TTL 0: cached without expiry.
    let forever = StateConfig {
        message_ttl: 0,
        ..state_all()
    };
    apply(
        &mut conn,
        &forever,
        "MESSAGE_CREATE",
        message(1, 3, None, "dm"),
    )
    .await;
    assert!(exists(&mut conn, "message:3:1").await);
    assert_eq!(score(&mut conn, "message:3:1").await, None);
    assert_eq!(
        members(&mut conn, "channel_keys:3").await,
        vec!["message:3:1"]
    );

    // An edit refreshes the expiry.
    apply(
        &mut conn,
        &state,
        "MESSAGE_UPDATE",
        message(1, 3, None, "edited"),
    )
    .await;
    assert!(score(&mut conn, "message:3:1").await.is_some());

    // Without STATE_MESSAGE, message events touch nothing.
    let off = StateConfig {
        message: false,
        ..state_all()
    };
    for (kind, d) in [
        ("MESSAGE_CREATE", message(2, 3, None, "x")),
        ("MESSAGE_UPDATE", message(1, 3, None, "ignored")),
        ("MESSAGE_DELETE", json!({"id": "1", "channel_id": "3"})),
        (
            "MESSAGE_DELETE_BULK",
            json!({"ids": ["1"], "channel_id": "3"}),
        ),
    ] {
        let outcome = apply(&mut conn, &off, kind, d).await;
        assert_eq!(outcome, Outcome::default(), "{kind}");
    }
    assert!(!exists(&mut conn, "message:3:2").await);
    assert_eq!(
        get(&mut conn, "message:3:1").await.unwrap()["content"],
        "edited"
    );

    // Bulk delete without STATE_OLD still deletes.
    let outcome = apply(
        &mut conn,
        &old_off(),
        "MESSAGE_DELETE_BULK",
        json!({"ids": ["1", "99"], "channel_id": "3"}),
    )
    .await;
    assert_eq!(outcome.old, None);
    assert!(
        keys(&mut conn).await.is_empty(),
        "{:?}",
        keys(&mut conn).await
    );
}

#[tokio::test]
async fn voice_states_outside_guilds_are_ignored() {
    let Some(mut conn) = redis(13).await else {
        return;
    };
    let state = state_all();
    let outcome = apply(
        &mut conn,
        &state,
        "VOICE_STATE_UPDATE",
        voice_state(None, 60, Some(5)),
    )
    .await;
    assert_eq!(outcome, Outcome::default());
    assert!(keys(&mut conn).await.is_empty());

    // Without STATE_OLD a leave still deletes.
    apply(
        &mut conn,
        &state,
        "VOICE_STATE_UPDATE",
        voice_state(Some(1000), 60, Some(5)),
    )
    .await;
    let outcome = apply(
        &mut conn,
        &old_off(),
        "VOICE_STATE_UPDATE",
        voice_state(Some(1000), 60, None),
    )
    .await;
    assert_eq!(outcome.old, None);
    assert!(keys(&mut conn).await.is_empty());
}

#[tokio::test]
async fn reads_and_failed_updates() {
    let Some(mut conn) = redis(14).await else {
        return;
    };
    let state = state_all();

    assert!(cache::get_all(&mut conn, &[]).await.unwrap().is_empty());
    assert_eq!(cache::get(&mut conn, "guild:1").await.unwrap(), None);
    assert!(!cache::exists(&mut conn, "guild:1").await.unwrap());
    assert!(cache::members(&mut conn, "guild_keys")
        .await
        .unwrap()
        .is_empty());

    // Logical guild channel keys are read from the stored key.
    apply(
        &mut conn,
        &state,
        "CHANNEL_CREATE",
        channel(2001, Some(2000), "general"),
    )
    .await;
    assert_eq!(
        cache::get(&mut conn, "channel:2000:2001")
            .await
            .unwrap()
            .unwrap()["name"],
        "general"
    );
    assert!(cache::exists(&mut conn, "channel:2000:2001").await.unwrap());
    let all = cache::get_all(
        &mut conn,
        &["channel:2000:2001".to_owned(), "channel:2000:9".to_owned()],
    )
    .await
    .unwrap();
    assert_eq!(all[0].as_ref().unwrap()["id"], "2001");
    assert_eq!(all[1], None);

    // Unhandled events do nothing.
    let outcome = apply(
        &mut conn,
        &state,
        "TYPING_START",
        json!({"channel_id": "1", "user_id": "2", "timestamp": 1}),
    )
    .await;
    assert_eq!(outcome, Outcome::default());

    // A cached value that is not JSON fails the update before anything is
    // written: the event's other keys are not half-applied.
    let _: () = redis::cmd("SET")
        .arg("guild:1000")
        .arg("not json")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert!(matches!(
        cache::get(&mut conn, "guild:1000").await,
        Err(ApiError::Json(_))
    ));
    let before = keys(&mut conn).await;
    let mut create = event("GUILD_CREATE", guild(1000, "g"));
    let ctx = UpdateContext {
        state: &state,
        bot_id: Some(bot_id()),
        now_ms: now_ms(),
    };
    let result = cache::update(&mut conn, &mut create, &ctx).await;
    assert!(matches!(result, Err(ApiError::Json(_))), "{result:?}");
    assert_eq!(keys(&mut conn).await, before);

    // A key of the wrong type is a Redis (server) error.
    let _: () = redis::cmd("DEL")
        .arg("guild:1000")
        .query_async(&mut conn)
        .await
        .unwrap();
    let _: () = redis::cmd("SADD")
        .arg("guild:1000")
        .arg("x")
        .query_async(&mut conn)
        .await
        .unwrap();
    let mut update = event("GUILD_UPDATE", guild_update(1000, "g"));
    let result = cache::update(&mut conn, &mut update, &ctx).await;
    match result {
        Err(ApiError::Redis(err)) => assert!(!err.is_io_error(), "{err}"),
        other => panic!("expected a Redis error, got {other:?}"),
    }
}

#[tokio::test]
async fn events_for_uncached_guilds_create_no_guild() {
    let Some(mut conn) = redis(15).await else {
        return;
    };
    let state = state_all();

    let outcome = apply(&mut conn, &state, "GUILD_DELETE", json!({"id": "4000"})).await;
    assert_eq!(outcome, Outcome::default());
    assert!(keys(&mut conn).await.is_empty());

    // Member events adjust the count of a cached guild only.
    let mut add = in_guild(member(20, &[]), 4000);
    add["joined_at"] = json!(now_iso());
    apply(&mut conn, &state, "GUILD_MEMBER_ADD", add).await;
    apply(
        &mut conn,
        &state,
        "GUILD_MEMBER_REMOVE",
        json!({"guild_id": "4000", "user": user(21, "user21")}),
    )
    .await;
    assert!(!exists(&mut conn, "guild:4000").await);
    assert!(exists(&mut conn, "member:4000:20").await);

    // A stub (READY) has no member_count to adjust.
    apply(&mut conn, &state, "READY", ready([0, 1], &[4000 << 22])).await;
    let mut add = in_guild(member(22, &[]), 4000 << 22);
    add["joined_at"] = json!(now_iso());
    apply(&mut conn, &state, "GUILD_MEMBER_ADD", add).await;
    assert_eq!(
        get(&mut conn, &format!("guild:{}", 4000u64 << 22))
            .await
            .unwrap(),
        json!({"id": (4000u64 << 22).to_string(), "unavailable": true})
    );
}

#[tokio::test]
async fn guild_delete_removes_threads_and_their_messages() {
    let Some(mut conn) = redis(0).await else {
        return;
    };
    let state = state_all();
    apply(&mut conn, &state, "GUILD_CREATE", guild(1000, "g")).await;
    apply(
        &mut conn,
        &state,
        "MESSAGE_CREATE",
        message(1, 1005, Some(1000), "in thread"),
    )
    .await;
    apply(
        &mut conn,
        &state,
        "MESSAGE_CREATE",
        message(2, 1003, Some(1000), "in channel"),
    )
    .await;
    // A key of another guild survives.
    apply(&mut conn, &state, "GUILD_CREATE", guild(5000, "other")).await;

    apply(&mut conn, &state, "GUILD_DELETE", json!({"id": "1000"})).await;
    let left: Vec<String> = keys(&mut conn)
        .await
        .into_iter()
        .filter(|key| key.contains("1000") || key.contains(":100"))
        .collect();
    assert!(left.is_empty(), "{left:?}");
    assert!(members(&mut conn, "message_keys").await.is_empty());
    assert!(members(&mut conn, "thread_keys")
        .await
        .iter()
        .all(|key| key.starts_with("thread:5000:")));
    assert_eq!(members(&mut conn, "guild_keys").await, vec!["guild:5000"]);
    let zset: Vec<String> = redis::cmd("ZRANGE")
        .arg("expiry_zset")
        .arg(0)
        .arg(-1)
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(zset, vec!["member:5000:10"]);
}
