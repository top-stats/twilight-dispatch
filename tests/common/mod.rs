//! Shared helpers for the integration tests.
//!
//! Tests that need services are skipped (they pass and print a notice) unless
//! the service URL is set:
//! - `TEST_REDIS_URL`, e.g. `redis://127.0.0.1:6379` (each test of a binary
//!   uses its own database index, so up to 16 run in parallel)
//! - `TEST_AMQP_URL`, e.g. `amqp://guest:guest@127.0.0.1:5672/%2f`
//!
//! `scripts/test.sh` starts Redis and LavinMQ in Docker and runs everything.
//!
//! The binaries are only isolated from each other because `cargo test` runs
//! them one at a time:
//! - database indexes are only unique within a binary (cache.rs uses 1-15,
//!   lifecycle.rs 1-6, pipeline.rs and sessions.rs 1-4, reconnect.rs 3-4...);
//! - some tests use server-wide commands (CLIENT PAUSE, CLIENT KILL, CONFIG
//!   RESETSTAT), and the broker tests share the durable `gateway.recv` and
//!   `gateway.send` queues. Each binary serializes those tests with a static
//!   mutex, which does not reach other processes.
//!
//! So do not run the suite with cargo-nextest (a process per test) unless
//! every test that uses a service is in one test group with
//! `max-threads = 1`, and do not point two runs at the same Redis or broker
//! at the same time.

#![allow(dead_code)]

use redis::aio::ConnectionManager;
use serde_json::{json, Value};
use std::io::Write;
use twilight_dispatch::{
    cache::{self, Outcome, UpdateContext},
    config::StateConfig,
    gateway::runner::parse_event,
    redis_conn,
};
use twilight_model::{
    gateway::event::Event,
    id::{marker::UserMarker, Id},
};

pub const BOT_ID: u64 = 900;

pub fn bot_id() -> Id<UserMarker> {
    Id::new(BOT_ID)
}

/// Missing services fail the test instead of skipping it when
/// REQUIRE_SERVICES is set (scripts/test.sh and CI set it).
///
/// The notice is written to the real stderr: libtest captures `eprintln!`
/// and only shows it for failing tests, so a skipped test looked like a pass.
pub fn skip(variable: &str) {
    if std::env::var_os("REQUIRE_SERVICES").is_some() {
        panic!("{variable} is not set but REQUIRE_SERVICES is");
    }
    // libtest names the thread of a test after it.
    let test = std::thread::current().name().unwrap_or("a test").to_owned();
    let _ = writeln!(std::io::stderr(), "SKIPPED {test}: {variable} is not set");
}

/// A clean Redis database, or `None` when TEST_REDIS_URL is unset.
///
/// The database is FLUSHED: never point TEST_REDIS_URL at a Redis with data.
pub async fn redis(db: u8) -> Option<ConnectionManager> {
    let Ok(base) = std::env::var("TEST_REDIS_URL") else {
        skip("TEST_REDIS_URL");
        return None;
    };
    let url = format!("{}/{}", base.trim_end_matches('/'), db);
    let mut conn = redis_conn::connect(&url)
        .await
        .expect("connect to test redis");
    redis::cmd("FLUSHDB")
        .query_async::<()>(&mut conn)
        .await
        .expect("flush test db");
    Some(conn)
}

pub fn amqp_url() -> Option<String> {
    let url = std::env::var("TEST_AMQP_URL").ok();
    if url.is_none() {
        skip("TEST_AMQP_URL");
    }
    url
}

pub fn state_all() -> StateConfig {
    StateConfig {
        enabled: true,
        member: true,
        member_ttl: 60_000,
        message: true,
        message_ttl: 60_000,
        presence: true,
        emoji: true,
        voice: true,
        old: true,
    }
}

/// Build a typed event from a dispatch type and its `d`, through the same
/// parser the shard runner uses.
pub fn event(kind: &str, d: Value) -> Event {
    let json = raw(kind, 1, d);
    parse_event(&json)
        .unwrap_or_else(|err| panic!("{kind} did not parse: {err}"))
        .unwrap_or_else(|| panic!("{kind} is not a gateway event"))
}

/// A raw dispatch payload as Discord sends it (`t`, `s`, `op`, `d`).
pub fn raw(kind: &str, seq: u64, d: Value) -> String {
    format!(r#"{{"t":"{kind}","s":{seq},"op":0,"d":{d}}}"#)
}

pub async fn apply(
    conn: &mut ConnectionManager,
    state: &StateConfig,
    kind: &str,
    d: Value,
) -> Outcome {
    let mut event = event(kind, d);
    let ctx = UpdateContext {
        state,
        bot_id: Some(bot_id()),
        now_ms: now_ms(),
    };
    cache::update(conn, &mut event, &ctx)
        .await
        .unwrap_or_else(|err| panic!("{kind} update failed: {err}"))
}

/// The current time as a Discord timestamp.
pub fn now_iso() -> String {
    let format = time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:6]+00:00"
    );
    time::OffsetDateTime::now_utc().format(&format).unwrap()
}

pub fn now_ms() -> i64 {
    twilight_dispatch::models::FormattedDateTime::now().unix_millis()
}

pub async fn get(conn: &mut ConnectionManager, key: &str) -> Option<Value> {
    let raw: Option<String> = redis::cmd("GET").arg(key).query_async(conn).await.unwrap();
    raw.map(|raw| serde_json::from_str(&raw).unwrap())
}

pub async fn members(conn: &mut ConnectionManager, set: &str) -> Vec<String> {
    let mut members: Vec<String> = redis::cmd("SMEMBERS")
        .arg(set)
        .query_async(conn)
        .await
        .unwrap();
    members.sort();
    members
}

pub async fn exists(conn: &mut ConnectionManager, key: &str) -> bool {
    let n: u64 = redis::cmd("EXISTS")
        .arg(key)
        .query_async(conn)
        .await
        .unwrap();
    n > 0
}

pub async fn score(conn: &mut ConnectionManager, key: &str) -> Option<f64> {
    redis::cmd("ZSCORE")
        .arg("expiry_zset")
        .arg(key)
        .query_async(conn)
        .await
        .unwrap()
}

// ---------------------------------------------------------------------------
// Fixtures (shapes follow https://discord.com/developers/docs)

pub fn user(id: u64, name: &str) -> Value {
    json!({
        "id": id.to_string(), "username": name, "global_name": name.to_uppercase(),
        "discriminator": "0", "avatar": null, "bot": id == BOT_ID, "public_flags": 0,
    })
}

pub fn member(user_id: u64, roles: &[u64]) -> Value {
    json!({
        "user": user(user_id, &format!("user{user_id}")),
        "nick": null, "avatar": null, "banner": null,
        "roles": roles.iter().map(u64::to_string).collect::<Vec<_>>(),
        "joined_at": "2024-01-01T00:00:00.000000+00:00", "premium_since": null,
        "deaf": false, "mute": false, "flags": 0, "pending": false,
        "communication_disabled_until": null,
    })
}

pub fn role(id: u64, name: &str) -> Value {
    json!({
        "id": id.to_string(), "name": name, "color": 0,
        "colors": {"primary_color": 0, "secondary_color": null, "tertiary_color": null},
        "hoist": false, "icon": null, "unicode_emoji": null, "position": 1,
        "permissions": "2251799813685248", "managed": false, "mentionable": false, "flags": 0,
    })
}

pub fn channel(id: u64, guild_id: Option<u64>, name: &str) -> Value {
    let mut channel = json!({
        "id": id.to_string(), "type": 0, "name": name, "position": 0,
        "permission_overwrites": [], "nsfw": false, "topic": null,
        "last_message_id": null, "parent_id": null, "rate_limit_per_user": 0, "flags": 0,
    });
    if let Some(guild_id) = guild_id {
        channel["guild_id"] = json!(guild_id.to_string());
    }
    channel
}

pub fn thread(id: u64, guild_id: u64, parent: u64) -> Value {
    json!({
        "id": id.to_string(), "guild_id": guild_id.to_string(), "parent_id": parent.to_string(),
        "type": 11, "name": format!("thread{id}"), "owner_id": "1", "flags": 0,
        "thread_metadata": {"archived": false, "auto_archive_duration": 1440,
            "archive_timestamp": "2024-01-01T00:00:00.000000+00:00", "locked": false},
        "message_count": 0, "member_count": 1, "rate_limit_per_user": 0,
    })
}

pub fn emoji(id: u64, name: &str) -> Value {
    json!({
        "id": id.to_string(), "name": name, "roles": [], "require_colons": true,
        "managed": false, "animated": false, "available": true,
    })
}

pub fn presence(guild_id: u64, user_id: u64, status: &str) -> Value {
    json!({
        "user": {"id": user_id.to_string()}, "guild_id": guild_id.to_string(),
        "status": status, "activities": [],
        "client_status": {"desktop": status},
    })
}

pub fn voice_state(guild_id: Option<u64>, user_id: u64, channel_id: Option<u64>) -> Value {
    let mut voice = json!({
        "channel_id": channel_id.map(|c| c.to_string()), "user_id": user_id.to_string(),
        "session_id": "abc", "deaf": false, "mute": false, "self_deaf": false,
        "self_mute": false, "self_video": false, "suppress": false,
        "request_to_speak_timestamp": null,
    });
    if let Some(guild_id) = guild_id {
        voice["guild_id"] = json!(guild_id.to_string());
    }
    voice
}

pub fn message(id: u64, channel_id: u64, guild_id: Option<u64>, content: &str) -> Value {
    let mut message = json!({
        "id": id.to_string(), "channel_id": channel_id.to_string(), "type": 0,
        "author": user(1, "author"), "content": content,
        "timestamp": "2024-01-01T00:00:00.000000+00:00", "edited_timestamp": null,
        "tts": false, "mention_everyone": false, "mentions": [], "mention_roles": [],
        "attachments": [], "embeds": [], "pinned": false, "components": [],
    });
    if let Some(guild_id) = guild_id {
        message["guild_id"] = json!(guild_id.to_string());
    }
    message
}

/// A GUILD_CREATE `d` with two channels, a thread, two roles, an emoji, the bot
/// and one user as members, presences (one offline) and a voice state.
pub fn guild(id: u64, name: &str) -> Value {
    json!({
        "id": id.to_string(), "name": name, "icon": null, "splash": null,
        "discovery_splash": null, "owner_id": "1", "afk_channel_id": null,
        "afk_timeout": 300, "verification_level": 1, "default_message_notifications": 0,
        "explicit_content_filter": 0, "features": [], "mfa_level": 0,
        "application_id": null, "system_channel_id": null, "system_channel_flags": 0,
        "rules_channel_id": null, "vanity_url_code": null, "description": null,
        "banner": null, "premium_tier": 0, "premium_subscription_count": 0,
        "preferred_locale": "en-US", "public_updates_channel_id": null,
        "nsfw_level": 0, "premium_progress_bar_enabled": false,
        "safety_alerts_channel_id": null,
        "joined_at": "2024-01-01T00:00:00.000000+00:00", "large": false,
        "unavailable": false, "member_count": 2,
        "roles": [role(id, "@everyone"), role(id + 1, "mods")],
        "emojis": [emoji(id + 2, "pog")],
        "channels": [channel(id + 3, None, "general"), channel(id + 4, None, "random")],
        "threads": [thread(id + 5, id, id + 3)],
        "members": [member(BOT_ID, &[]), member(10, &[id + 1])],
        "presences": [presence(id, 10, "online"), presence(id, 11, "offline")],
        "voice_states": [voice_state(None, 10, Some(id + 3))],
        "stickers": [], "stage_instances": [], "guild_scheduled_events": [],
        "soundboard_sounds": [],
    })
}

pub fn ready(shard: [u32; 2], guilds: &[u64]) -> Value {
    json!({
        "v": 10, "user": {
            "id": BOT_ID.to_string(), "username": "bot", "discriminator": "0",
            "global_name": null, "avatar": null, "bot": true, "mfa_enabled": false,
            "verified": true, "flags": 0,
        },
        "guilds": guilds.iter().map(|g| json!({"id": g.to_string(), "unavailable": true})).collect::<Vec<_>>(),
        "session_id": "session", "resume_gateway_url": "wss://gateway-us-east1-b.discord.gg",
        "shard": shard, "application": {"id": BOT_ID.to_string(), "flags": 0},
    })
}
