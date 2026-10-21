#![recursion_limit = "256"]

//! State cache behaviour against a real Redis (set TEST_REDIS_URL).
//! Each test uses its own database index.

mod common;

use common::*;
use redis::aio::ConnectionLike;
use serde_json::json;
use twilight_dispatch::{
    cache::{self, jobs, Outcome, UpdateContext},
    config::StateConfig,
    metrics,
};

#[tokio::test]
async fn guild_create_splits_the_guild_into_keys() {
    let Some(mut conn) = redis(1).await else {
        return;
    };
    let state = state_all();

    let outcome = apply(&mut conn, &state, "GUILD_CREATE", guild(1000, "g")).await;
    assert!(!outcome.guild_known, "a never-seen guild is a join");

    let blob = get(&mut conn, "guild:1000").await.unwrap();
    assert_eq!(blob["name"], "g");
    assert_eq!(blob["member_count"], 2);
    assert_eq!(
        blob["unavailable"], false,
        "kept for consumers of the old shape"
    );
    assert_eq!(blob["threads"][0]["id"], "1005", "threads stay in the blob");
    for field in [
        "channels",
        "roles",
        "emojis",
        "members",
        "presences",
        "voice_states",
    ] {
        assert_eq!(blob[field], json!([]), "{field} is stored separately");
    }

    // Channels (and threads) are stored without their guild, but indexed under it.
    assert_eq!(
        get(&mut conn, "channel:1003").await.unwrap()["guild_id"],
        "1000"
    );
    // Threads have their own keys (consumers of channel:* expect no threads).
    assert_eq!(
        get(&mut conn, "thread:1000:1005").await.unwrap()["type"],
        11
    );
    assert!(!exists(&mut conn, "channel:1005").await);
    assert_eq!(
        members(&mut conn, "thread_keys:1000").await,
        vec!["thread:1000:1005"]
    );
    assert!(get(&mut conn, "role:1000:1001").await.is_some());
    assert!(get(&mut conn, "emoji:1000:1002").await.is_some());
    assert!(get(&mut conn, "voice:1000:10").await.is_some());

    // Members keep guild_id (consumers rely on it) and expire, except the bot.
    let user = get(&mut conn, "member:1000:10").await.unwrap();
    assert_eq!(user["guild_id"], "1000");
    assert_eq!(user["user"]["discriminator"], "0");
    assert!(score(&mut conn, "member:1000:10").await.is_some());
    assert!(get(&mut conn, &format!("member:1000:{BOT_ID}"))
        .await
        .is_some());
    assert!(score(&mut conn, &format!("member:1000:{BOT_ID}"))
        .await
        .is_none());

    // Offline presences are not cached.
    assert!(get(&mut conn, "presence:1000:10").await.is_some());
    assert!(get(&mut conn, "presence:1000:11").await.is_none());

    let children = members(&mut conn, "guild_keys:1000").await;
    for key in [
        "channel:1003",
        "channel:1004",
        "thread:1000:1005",
        "role:1000:1000",
        "role:1000:1001",
        "emoji:1000:1002",
        "member:1000:10",
        "presence:1000:10",
        "voice:1000:10",
    ] {
        assert!(
            children.contains(&key.to_owned()),
            "{key} indexed: {children:?}"
        );
    }
    assert_eq!(members(&mut conn, "guild_keys").await, vec!["guild:1000"]);

    let again = apply(&mut conn, &state, "GUILD_CREATE", guild(1000, "g")).await;
    assert!(again.guild_known, "a re-sent GUILD_CREATE is not a join");
}

#[tokio::test]
async fn guild_channel_reads_use_the_stored_key() {
    let Some(mut conn) = redis(2).await else {
        return;
    };
    let state = state_all();
    apply(&mut conn, &state, "GUILD_CREATE", guild(1000, "g")).await;

    // Previously GET used channel:<guild>:<channel> and always missed.
    apply(
        &mut conn,
        &state,
        "CHANNEL_PINS_UPDATE",
        json!({"guild_id": "1000", "channel_id": "1003", "last_pin_timestamp": "2026-10-20T12:00:00.000000+00:00"}),
    )
    .await;
    let pinned = get(&mut conn, "channel:1003").await.unwrap();
    assert_eq!(
        pinned["last_pin_timestamp"],
        "2026-10-20T12:00:00.000000+00:00"
    );

    let mut renamed = channel(1003, Some(1000), "renamed");
    renamed["guild_id"] = json!("1000");
    let outcome = apply(&mut conn, &state, "CHANNEL_UPDATE", renamed).await;
    assert_eq!(outcome.old.unwrap()["name"], "general");

    let outcome = apply(
        &mut conn,
        &state,
        "CHANNEL_DELETE",
        channel(1003, Some(1000), "renamed"),
    )
    .await;
    assert_eq!(outcome.old.unwrap()["name"], "renamed");
    assert!(!exists(&mut conn, "channel:1003").await);
    assert!(!members(&mut conn, "guild_keys:1000")
        .await
        .contains(&"channel:1003".to_owned()));
}

#[tokio::test]
async fn guild_update_merges_instead_of_replacing() {
    let Some(mut conn) = redis(3).await else {
        return;
    };
    let state = state_all();
    apply(&mut conn, &state, "GUILD_CREATE", guild(1000, "g")).await;

    let mut update = guild(1000, "renamed");
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
    update["roles"] = json!([role(1000, "@everyone"), role(1001, "admins")]);
    let outcome = apply(&mut conn, &state, "GUILD_UPDATE", update).await;
    assert_eq!(outcome.old.unwrap()["name"], "g");

    let blob = get(&mut conn, "guild:1000").await.unwrap();
    assert_eq!(blob["name"], "renamed");
    assert_eq!(blob["member_count"], 2, "GUILD_CREATE-only fields survive");
    assert_eq!(blob["joined_at"], "2024-01-01T00:00:00.000000+00:00");
    assert_eq!(blob["roles"], json!([]));
    assert_eq!(
        get(&mut conn, "role:1000:1001").await.unwrap()["name"],
        "admins"
    );
}

#[tokio::test]
async fn outages_keep_the_guild_and_are_not_joins() {
    let Some(mut conn) = redis(4).await else {
        return;
    };
    let state = state_all();
    apply(&mut conn, &state, "GUILD_CREATE", guild(1000, "g")).await;

    apply(
        &mut conn,
        &state,
        "GUILD_DELETE",
        json!({"id": "1000", "unavailable": true}),
    )
    .await;
    let blob = get(&mut conn, "guild:1000").await.unwrap();
    assert_eq!(blob["unavailable"], true);
    assert_eq!(blob["name"], "g");
    assert!(
        exists(&mut conn, "channel:1003").await,
        "children survive an outage"
    );

    let back = apply(&mut conn, &state, "GUILD_CREATE", guild(1000, "g")).await;
    assert!(back.guild_known, "recovering from an outage is not a join");

    // An unavailable GUILD_CREATE (outage at startup) is not a join either.
    let outage = apply(
        &mut conn,
        &state,
        "GUILD_CREATE",
        json!({"id": "2000", "unavailable": true}),
    )
    .await;
    assert!(!outage.guild_known);
    let later = apply(&mut conn, &state, "GUILD_CREATE", guild(2000, "h")).await;
    assert!(later.guild_known);
}

#[tokio::test]
async fn leaving_a_guild_removes_everything() {
    let Some(mut conn) = redis(5).await else {
        return;
    };
    let state = state_all();
    apply(&mut conn, &state, "GUILD_CREATE", guild(1000, "g")).await;
    apply(
        &mut conn,
        &state,
        "MESSAGE_CREATE",
        message(1, 1003, Some(1000), "hi"),
    )
    .await;

    let outcome = apply(&mut conn, &state, "GUILD_DELETE", json!({"id": "1000"})).await;
    assert_eq!(outcome.previous_guild.unwrap()["name"], "g");

    let keys: Vec<String> = redis::cmd("KEYS")
        .arg("*")
        .query_async(&mut conn)
        .await
        .unwrap();
    let mut leftover: Vec<String> = Vec::new();
    for key in keys {
        let kind: String = redis::cmd("TYPE")
            .arg(&key)
            .query_async(&mut conn)
            .await
            .unwrap();
        let size: u64 = match kind.as_str() {
            "set" => redis::cmd("SCARD")
                .arg(&key)
                .query_async(&mut conn)
                .await
                .unwrap(),
            "zset" => redis::cmd("ZCARD")
                .arg(&key)
                .query_async(&mut conn)
                .await
                .unwrap(),
            _ => 1,
        };
        if size > 0 {
            leftover.push(key);
        }
    }
    assert!(leftover.is_empty(), "nothing is left behind: {leftover:?}");
}

#[tokio::test]
async fn member_count_follows_joins_and_leaves() {
    let Some(mut conn) = redis(6).await else {
        return;
    };
    let state = state_all();
    apply(&mut conn, &state, "GUILD_CREATE", guild(1000, "g")).await;

    let mut add = member(20, &[]);
    add["guild_id"] = json!("1000");
    add["joined_at"] = json!(now_iso());
    apply(&mut conn, &state, "GUILD_MEMBER_ADD", add.clone()).await;
    // Discord may resend MEMBER_ADD for an existing member.
    apply(&mut conn, &state, "GUILD_MEMBER_ADD", add).await;
    assert_eq!(
        get(&mut conn, "guild:1000").await.unwrap()["member_count"],
        3
    );
    assert_eq!(
        get(&mut conn, "member:1000:20").await.unwrap()["guild_id"],
        "1000"
    );
    // A re-sent MEMBER_ADD for a long-time member is not a join either, even
    // when that member is not cached (its TTL expired).
    let mut old_member = member(21, &[]);
    old_member["guild_id"] = json!("1000");
    apply(&mut conn, &state, "GUILD_MEMBER_ADD", old_member).await;
    assert_eq!(
        get(&mut conn, "guild:1000").await.unwrap()["member_count"],
        3
    );

    let outcome = apply(
        &mut conn,
        &state,
        "GUILD_MEMBER_REMOVE",
        json!({"guild_id": "1000", "user": user(10, "user10")}),
    )
    .await;
    assert_eq!(outcome.old.unwrap()["user"]["id"], "10");
    assert_eq!(
        get(&mut conn, "guild:1000").await.unwrap()["member_count"],
        2
    );
    assert!(!exists(&mut conn, "member:1000:10").await);
    assert!(!exists(&mut conn, "presence:1000:10").await);
    assert!(
        score(&mut conn, "member:1000:10").await.is_none(),
        "expiry entry removed too"
    );

    // Without STATE_MEMBER the count is still maintained.
    let counts_only = StateConfig {
        member: false,
        ..state_all()
    };
    let mut add = member(30, &[]);
    add["guild_id"] = json!("1000");
    add["joined_at"] = json!(now_iso());
    apply(&mut conn, &counts_only, "GUILD_MEMBER_ADD", add).await;
    assert_eq!(
        get(&mut conn, "guild:1000").await.unwrap()["member_count"],
        3
    );
    assert!(!exists(&mut conn, "member:1000:30").await);
}

#[tokio::test]
async fn member_update_merges_every_field() {
    let Some(mut conn) = redis(7).await else {
        return;
    };
    let state = state_all();
    apply(&mut conn, &state, "GUILD_CREATE", guild(1000, "g")).await;

    let mut update = member(10, &[1001, 1000]);
    update["guild_id"] = json!("1000");
    update["nick"] = json!("nick");
    update["communication_disabled_until"] = json!("2030-01-01T00:00:00.000000+00:00");
    update["deaf"] = json!(null);
    update["pending"] = json!(true);
    let outcome = apply(&mut conn, &state, "GUILD_MEMBER_UPDATE", update).await;
    assert_eq!(outcome.old.unwrap()["nick"], json!(null));

    let cached = get(&mut conn, "member:1000:10").await.unwrap();
    assert_eq!(cached["nick"], "nick");
    assert_eq!(
        cached["communication_disabled_until"],
        "2030-01-01T00:00:00.000000+00:00"
    );
    assert_eq!(cached["pending"], true);
    assert_eq!(cached["deaf"], false, "null deaf keeps the cached value");
    assert_eq!(cached["roles"], json!(["1001", "1000"]));
    assert_eq!(cached["guild_id"], "1000");

    // A member written by the previous version (no flags, padded
    // discriminator) is still updated.
    let legacy = json!({
        "guild_id": "1000", "user": {"id": "40", "username": "old", "discriminator": "0000", "avatar": null, "bot": false},
        "nick": null, "roles": [], "joined_at": "2020-01-01T00:00:00.000000+00:00",
        "deaf": true, "mute": false, "pending": false, "communication_disabled_until": null,
    });
    redis::cmd("SET")
        .arg("member:1000:40")
        .arg(legacy.to_string())
        .query_async::<()>(&mut conn)
        .await
        .unwrap();
    let mut update = member(40, &[]);
    update["guild_id"] = json!("1000");
    update["nick"] = json!("renamed");
    update["deaf"] = json!(null);
    apply(&mut conn, &state, "GUILD_MEMBER_UPDATE", update).await;
    let cached = get(&mut conn, "member:1000:40").await.unwrap();
    assert_eq!(cached["nick"], "renamed");
    assert_eq!(cached["deaf"], true);

    // The bot's own member never expires, even without STATE_MEMBER.
    let minimal = StateConfig {
        member: false,
        ..state_all()
    };
    let mut bot = member(BOT_ID, &[1001]);
    bot["guild_id"] = json!("1000");
    apply(&mut conn, &minimal, "GUILD_MEMBER_UPDATE", bot).await;
    assert_eq!(
        get(&mut conn, &format!("member:1000:{BOT_ID}"))
            .await
            .unwrap()["roles"],
        json!(["1001"])
    );
    assert!(score(&mut conn, &format!("member:1000:{BOT_ID}"))
        .await
        .is_none());
}

#[tokio::test]
async fn emoji_update_with_one_cached_emoji() {
    let Some(mut conn) = redis(8).await else {
        return;
    };
    let state = state_all();
    apply(&mut conn, &state, "GUILD_CREATE", guild(1000, "g")).await;

    // Exactly one emoji cached: this used to send GET instead of MGET and fail.
    let outcome = apply(
        &mut conn,
        &state,
        "GUILD_EMOJIS_UPDATE",
        json!({"guild_id": "1000", "emojis": [emoji(1100, "new"), emoji(1101, "newer")]}),
    )
    .await;
    assert_eq!(outcome.old.unwrap().as_array().unwrap().len(), 1);
    assert!(!exists(&mut conn, "emoji:1000:1002").await);
    assert!(exists(&mut conn, "emoji:1000:1100").await);
    assert!(exists(&mut conn, "emoji:1000:1101").await);
}

#[tokio::test]
async fn message_lifecycle() {
    let Some(mut conn) = redis(9).await else {
        return;
    };
    let state = state_all();

    let mut tts = message(1, 3, Some(1000), "hello");
    tts["tts"] = json!(true);
    apply(&mut conn, &state, "MESSAGE_CREATE", tts).await;
    assert!(score(&mut conn, "message:3:1").await.is_some());

    // MESSAGE_UPDATE is a full message whose tts is always false.
    let outcome = apply(
        &mut conn,
        &state,
        "MESSAGE_UPDATE",
        message(1, 3, Some(1000), "edited"),
    )
    .await;
    assert_eq!(outcome.old.unwrap()["content"], "hello");
    let cached = get(&mut conn, "message:3:1").await.unwrap();
    assert_eq!(cached["content"], "edited");
    assert_eq!(cached["tts"], true);

    // Bulk delete with a single id used to fail (GET instead of MGET).
    let outcome = apply(
        &mut conn,
        &state,
        "MESSAGE_DELETE_BULK",
        json!({"ids": ["1"], "channel_id": "3", "guild_id": "1000"}),
    )
    .await;
    assert_eq!(outcome.old.unwrap()[0]["content"], "edited");
    assert!(!exists(&mut conn, "message:3:1").await);
    assert!(members(&mut conn, "channel_keys:3").await.is_empty());
    assert!(score(&mut conn, "message:3:1").await.is_none());
}

#[tokio::test]
async fn presences() {
    let Some(mut conn) = redis(10).await else {
        return;
    };
    let state = state_all();
    apply(&mut conn, &state, "GUILD_CREATE", guild(1000, "g")).await;
    let mut full = presence(1000, 50, "online");
    full["user"] = user(50, "fifty");
    apply(&mut conn, &state, "PRESENCE_UPDATE", full).await;

    // A partial user keeps the cached one.
    apply(
        &mut conn,
        &state,
        "PRESENCE_UPDATE",
        presence(1000, 50, "idle"),
    )
    .await;
    let cached = get(&mut conn, "presence:1000:50").await.unwrap();
    assert_eq!(cached["status"], "idle");
    assert_eq!(cached["user"]["username"], "fifty");

    // Going offline removes the presence instead of keeping it forever.
    apply(
        &mut conn,
        &state,
        "PRESENCE_UPDATE",
        presence(1000, 50, "offline"),
    )
    .await;
    assert!(!exists(&mut conn, "presence:1000:50").await);
    assert!(!members(&mut conn, "presence_keys")
        .await
        .contains(&"presence:1000:50".to_owned()));
}

#[tokio::test]
async fn voice_states() {
    let Some(mut conn) = redis(11).await else {
        return;
    };
    let state = state_all();
    let mut joined = voice_state(Some(1000), 60, Some(1003));
    joined["member"] = member(60, &[]);
    apply(&mut conn, &state, "VOICE_STATE_UPDATE", joined).await;
    let cached = get(&mut conn, "voice:1000:60").await.unwrap();
    assert_eq!(cached["guild_id"], "1000");
    assert_eq!(
        cached["member"]["guild_id"], "1000",
        "nested member keeps guild_id"
    );
    let outcome = apply(
        &mut conn,
        &state,
        "VOICE_STATE_UPDATE",
        voice_state(Some(1000), 60, None),
    )
    .await;
    assert_eq!(outcome.old.unwrap()["channel_id"], "1003");
    assert!(!exists(&mut conn, "voice:1000:60").await);
}

#[tokio::test]
async fn ready_keeps_full_guilds_and_drops_left_ones() {
    let Some(mut conn) = redis(12).await else {
        return;
    };
    let state = state_all();
    // Guild ids that land on shard 0 and shard 1 of 2.
    let on = |shard: u64| {
        (1u64..)
            .map(|n| n << 22)
            .filter(move |id| (id >> 22) % 2 == shard)
    };
    let (kept, left) = {
        let mut s0 = on(0);
        (s0.next().unwrap(), s0.next().unwrap())
    };
    let other = on(1).next().unwrap();
    for id in [kept, left, other] {
        apply(
            &mut conn,
            &state,
            "GUILD_CREATE",
            guild(id, &format!("g{id}")),
        )
        .await;
    }

    apply(&mut conn, &state, "READY", ready([0, 2], &[kept, 77 << 22])).await;

    assert_eq!(
        get(&mut conn, &format!("guild:{kept}")).await.unwrap()["name"],
        format!("g{kept}"),
        "full guild kept"
    );
    assert!(
        !exists(&mut conn, &format!("guild:{left}")).await,
        "left while offline"
    );
    assert!(members(&mut conn, &format!("guild_keys:{left}"))
        .await
        .is_empty());
    assert!(
        exists(&mut conn, &format!("guild:{other}")).await,
        "other shard untouched"
    );
    let stub = get(&mut conn, &format!("guild:{}", 77u64 << 22))
        .await
        .unwrap();
    assert_eq!(stub["unavailable"], true);
    assert_eq!(
        get(&mut conn, "bot_user").await.unwrap()["id"],
        BOT_ID.to_string()
    );

    // After READY, a listed guild's GUILD_CREATE is not a join.
    let outcome = apply(&mut conn, &state, "GUILD_CREATE", guild(77 << 22, "late")).await;
    assert!(outcome.guild_known);
}

#[tokio::test]
async fn expiry_job_deletes_due_keys_only() {
    let Some(mut conn) = redis(13).await else {
        return;
    };
    let state = StateConfig {
        member_ttl: 1,
        message_ttl: 1,
        ..state_all()
    };
    apply(&mut conn, &state, "GUILD_CREATE", guild(1000, "g")).await;
    apply(
        &mut conn,
        &state,
        "MESSAGE_CREATE",
        message(1, 1003, Some(1000), "hi"),
    )
    .await;

    // Refresh one member far into the future.
    let mut keep = member(10, &[]);
    keep["guild_id"] = json!("1000");
    let long = StateConfig {
        member_ttl: 3_600_000,
        ..state_all()
    };
    apply(&mut conn, &long, "GUILD_MEMBER_UPDATE", keep).await;

    let removed = jobs::expire_due(&mut conn, now_ms() + 10).await.unwrap();
    assert_eq!(removed, 1, "only the message was due");
    assert!(!exists(&mut conn, "message:1003:1").await);
    assert!(members(&mut conn, "message_keys").await.is_empty());
    assert!(exists(&mut conn, "member:1000:10").await);
    assert!(
        exists(&mut conn, &format!("member:1000:{BOT_ID}")).await,
        "the bot never expires"
    );
}

#[tokio::test]
async fn legacy_expiry_hash_is_migrated() {
    let Some(mut conn) = redis(14).await else {
        return;
    };
    redis::cmd("HSET")
        .arg("expiry_keys")
        .arg("member:1:2")
        .arg(r#""1970-01-01T00:00:01.5""#)
        .arg("member:1:3")
        .arg("garbage")
        .query_async::<()>(&mut conn)
        .await
        .unwrap();
    assert_eq!(jobs::migrate_legacy_expiry(&mut conn).await.unwrap(), 1);
    assert_eq!(score(&mut conn, "member:1:2").await, Some(1500.0));

    // An older instance refreshed an entry that was already migrated: the
    // later deadline wins (ZADD GT on Redis >= 6.2).
    let _: () = redis::cmd("HSET")
        .arg("expiry_keys")
        .arg("member:1:2")
        .arg(r#""1970-01-01T00:00:02.5""#)
        .query_async(&mut conn)
        .await
        .unwrap();
    assert_eq!(jobs::migrate_legacy_expiry(&mut conn).await.unwrap(), 1);
    assert_eq!(score(&mut conn, "member:1:2").await, Some(2500.0));
    assert!(!exists(&mut conn, "expiry_keys").await);
    // Running it again is a no-op.
    assert_eq!(jobs::migrate_legacy_expiry(&mut conn).await.unwrap(), 0);
}

#[tokio::test]
async fn disabled_features_write_nothing() {
    let Some(mut conn) = redis(15).await else {
        return;
    };
    let off = StateConfig {
        enabled: true,
        ..StateConfig::default()
    };
    apply(&mut conn, &off, "MESSAGE_CREATE", message(1, 3, None, "hi")).await;
    apply(
        &mut conn,
        &off,
        "PRESENCE_UPDATE",
        presence(1000, 50, "online"),
    )
    .await;
    apply(
        &mut conn,
        &off,
        "VOICE_STATE_UPDATE",
        voice_state(Some(1000), 60, Some(1)),
    )
    .await;
    let keys: Vec<String> = redis::cmd("KEYS")
        .arg("*")
        .query_async(&mut conn)
        .await
        .unwrap();
    assert!(keys.is_empty(), "{keys:?}");
}

/// A Redis connection that fails (and counts) every command.
#[derive(Default)]
struct Untouchable {
    calls: usize,
}

impl Untouchable {
    fn fail<'a, T: Send + 'a>(&mut self) -> redis::RedisFuture<'a, T> {
        self.calls += 1;
        Box::pin(async { Err(std::io::Error::other("Redis must not be used").into()) })
    }
}

impl ConnectionLike for Untouchable {
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
        self.fail()
    }

    fn get_db(&self) -> i64 {
        0
    }
}

/// STATE_ENABLED=false must leave Redis alone even for a caller that does not
/// check it itself. Every other flag is on, so anything would be written.
#[tokio::test]
async fn a_disabled_cache_sends_nothing_to_redis() {
    let disabled = StateConfig {
        enabled: false,
        ..state_all()
    };
    let ctx = UpdateContext {
        state: &disabled,
        bot_id: Some(bot_id()),
        now_ms: now_ms(),
    };
    let mut added = member(30, &[]);
    added["guild_id"] = json!("1000");
    added["joined_at"] = json!(now_iso());
    let events = [
        ("READY", ready([0, 1], &[1000, 2000])),
        ("GUILD_CREATE", guild(1000, "g")),
        ("CHANNEL_DELETE", channel(1003, Some(1000), "general")),
        ("MESSAGE_CREATE", message(1, 1003, Some(1000), "hi")),
        ("GUILD_MEMBER_ADD", added),
        ("PRESENCE_UPDATE", presence(1000, 10, "online")),
        ("USER_UPDATE", ready([0, 1], &[])["user"].clone()),
        ("GUILD_DELETE", json!({"id": "1000"})),
    ];
    let mut conn = Untouchable::default();
    for (kind, d) in events {
        let mut typed = event(kind, d);
        let outcome = cache::update(&mut conn, &mut typed, &ctx).await;
        assert_eq!(outcome.unwrap(), Outcome::default(), "{kind}");
    }
    assert_eq!(conn.calls, 0);
}

/// Each state gauge counts its own type index: every index gets a different
/// size, so a gauge that reads another index's count shows up.
#[tokio::test]
async fn state_gauges_count_their_own_type() {
    let Some(mut conn) = redis(0).await else {
        return;
    };
    let gauges = [
        ("guild_keys", &metrics::STATE_GUILDS),
        ("channel_keys", &metrics::STATE_CHANNELS),
        ("message_keys", &metrics::STATE_MESSAGES),
        ("role_keys", &metrics::STATE_ROLES),
        ("emoji_keys", &metrics::STATE_EMOJIS),
        ("member_keys", &metrics::STATE_MEMBERS),
        ("presence_keys", &metrics::STATE_PRESENCES),
        ("voice_keys", &metrics::STATE_VOICES),
        ("thread_keys", &metrics::STATE_THREADS),
    ];
    assert_eq!(
        gauges.len(),
        metrics::STATE_GAUGES.len(),
        "a gauge is untested"
    );
    for (count, (set, _)) in (1..).zip(&gauges) {
        let members: Vec<String> = (0..count).map(|n| format!("{set}:{n}")).collect();
        redis::cmd("SADD")
            .arg(set)
            .arg(members)
            .query_async::<()>(&mut conn)
            .await
            .unwrap();
    }
    // Per-guild indexes are not types.
    redis::cmd("SADD")
        .arg("thread_keys:1000")
        .arg((0..20).map(|n| n.to_string()).collect::<Vec<_>>())
        .query_async::<()>(&mut conn)
        .await
        .unwrap();

    metrics::update_state_gauges(&mut conn).await.unwrap();
    let counted: Vec<(&str, i64)> = gauges.iter().map(|(set, g)| (*set, g.get())).collect();
    let expected: Vec<(&str, i64)> = (1..).zip(&gauges).map(|(n, (set, _))| (*set, n)).collect();
    assert_eq!(counted, expected);
}
