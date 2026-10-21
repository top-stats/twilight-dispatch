#![recursion_limit = "256"]

//! Guards for feature work: tests that fail when a change in one place misses
//! another place that has to agree with it. A new gateway event, cache arm,
//! key type, removal, queue flag or twilight upgrade that forgets a related
//! place fails here until that place is updated (or the decision is recorded
//! in one of the tables below).
//!
//! Most guards run `universe()`, one fixture per dispatch event twilight knows
//! plus variants, through `runner::prepare` like the shard runner does.
//! `Recording` shows what `cache::update` sends for each one, either to a
//! `Fake` Redis (everything or nothing cached) or to a real one.
//!
//! Redis tests (TEST_REDIS_URL) each use their own database index; the broker
//! test needs TEST_AMQP_URL. Everything else needs no services.

mod common;

use common::*;
use lapin::{
    options::{ExchangeDeclareOptions, QueueDeclareOptions},
    types::FieldTable,
    Connection, ConnectionProperties, ExchangeKind,
};
use redis::{
    aio::{ConnectionLike, ConnectionManager},
    Cmd, Pipeline, RedisFuture, Value as RedisValue,
};
use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use twilight_dispatch::{
    amqp::{declare_consume, Amqp},
    cache::{self, merge::SPLIT_GUILD_FIELDS, CachedMember, UpdateContext},
    commands::ALLOWED_OPCODES,
    config::StateConfig,
    discord_log::DiscordLog,
    gateway::{
        payload::Route,
        registry::{Registry, ShardHandle},
        runner::{is_fatal_close, parse_event, prepare, Prepared},
        worker::{self, Breaker, Job, Publisher, WorkerContext},
    },
    models::ApiResult,
    util::Throttle,
};
use twilight_model::{
    gateway::{
        event::{Event, EventType},
        payload::incoming::GuildCreate,
        CloseCode, OpCode,
    },
    guild::Member,
    id::Id,
};

/// The guild of every guild event in the universe (children as created by
/// `common::guild`: roles 1000/1001, emoji 1002, channels 1003/1004, thread
/// 1005, members 900 (the bot) and 10).
const G: u64 = 1000;
/// Another guild on the same shard, which READY of `G`'s shard does not list.
const OTHER: u64 = 2000;
/// A DM channel.
const DM: u64 = 77;

// ---------------------------------------------------------------------------
// The universe: every dispatch event

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Scope {
    /// An event of guild `G`.
    Guild,
    /// An event of the DM channel `DM`.
    Dm,
    /// About the bot or its session.
    Bot,
}

struct Fixture {
    /// The event type, followed by the variant in parentheses if there are
    /// several fixtures of the type.
    label: &'static str,
    kind: &'static str,
    scope: Scope,
    d: Value,
}

impl Fixture {
    fn raw(&self, seq: u64) -> String {
        raw(self.kind, seq, self.d.clone())
    }
}

fn new_fixture(scope: Scope, label: &'static str, d: Value) -> Fixture {
    let kind = label.split(" (").next().unwrap_or(label);
    Fixture {
        label,
        kind,
        scope,
        d,
    }
}

fn in_guild(label: &'static str, d: Value) -> Fixture {
    new_fixture(Scope::Guild, label, d)
}

fn in_dm(label: &'static str, d: Value) -> Fixture {
    new_fixture(Scope::Dm, label, d)
}

fn of_bot(label: &'static str, d: Value) -> Fixture {
    new_fixture(Scope::Bot, label, d)
}

fn find<'a>(fixtures: &'a [Fixture], label: &str) -> &'a Fixture {
    fixtures
        .iter()
        .find(|f| f.label == label)
        .unwrap_or_else(|| panic!("no fixture {label}"))
}

fn gid() -> String {
    G.to_string()
}

fn sticker(id: u64) -> Value {
    json!({
        "id": id.to_string(), "name": "s", "description": null, "tags": "t", "type": 2,
        "format_type": 1, "available": true, "guild_id": gid(), "sort_value": null,
    })
}

fn automod_rule() -> Value {
    json!({
        "id": "70", "guild_id": gid(), "name": "r", "creator_id": "1", "event_type": 1,
        "trigger_type": 1, "trigger_metadata": {"keyword_filter": ["x"]}, "actions": [],
        "enabled": true, "exempt_roles": [], "exempt_channels": [],
    })
}

fn scheduled_event() -> Value {
    json!({
        "id": "80", "guild_id": gid(), "channel_id": "1003", "creator_id": "1", "name": "e",
        "description": null, "scheduled_start_time": "2030-01-01T00:00:00.000000+00:00",
        "scheduled_end_time": null, "privacy_level": 2, "status": 1, "entity_type": 2,
        "entity_id": null, "entity_metadata": null, "image": null,
    })
}

fn stage_instance() -> Value {
    json!({
        "id": "90", "guild_id": gid(), "channel_id": "1003", "topic": "t",
        "privacy_level": 2, "guild_scheduled_event_id": null,
    })
}

fn integration() -> Value {
    json!({
        "id": "60", "name": "i", "type": "discord", "enabled": true,
        "account": {"id": "1", "name": "a"}, "guild_id": gid(),
    })
}

fn current_user() -> Value {
    json!({
        "id": BOT_ID.to_string(), "username": "bot", "discriminator": "0",
        "global_name": null, "avatar": null, "bot": true, "mfa_enabled": false,
        "verified": true, "flags": 0,
    })
}

fn reaction(with_member: bool) -> Value {
    let mut d = json!({
        "user_id": "10", "channel_id": "1003", "message_id": "1", "guild_id": gid(),
        "emoji": {"id": null, "name": "x"}, "burst": false, "type": 0,
    });
    if with_member {
        d["member"] = member(10, &[]);
    }
    d
}

fn dm_channel() -> Value {
    json!({
        "id": DM.to_string(), "type": 1, "last_message_id": null, "flags": 0,
        "recipients": [user(10, "user10")],
    })
}

/// `common::guild` with every list twilight models filled in.
fn full_guild() -> Value {
    let mut guild = guild(G, "g");
    guild["stickers"] = json!([sticker(1200)]);
    guild["stage_instances"] = json!([stage_instance()]);
    guild["guild_scheduled_events"] = json!([scheduled_event()]);
    guild["features"] = json!(["COMMUNITY"]);
    guild
}

fn member_update(user_id: u64) -> Value {
    let mut update = member(user_id, &[1001]);
    update["guild_id"] = json!(gid());
    update["nick"] = json!("n");
    update
}

/// One fixture per dispatch event of twilight-model (see
/// `universe_has_every_dispatch_event`), plus variants for the branches the
/// cache treats differently. Guild events are about guild `G` as
/// `full_guild()` creates it.
fn universe() -> Vec<Fixture> {
    let mut member_add = member(20, &[]);
    member_add["guild_id"] = json!(gid());
    member_add["joined_at"] = json!(now_iso());
    let mut guild_update = full_guild();
    guild_update["name"] = json!("renamed");
    // GUILD_UPDATE carries a guild without the GUILD_CREATE extra fields.
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
        guild_update.as_object_mut().unwrap().remove(field);
    }
    let mut archived = thread(1005, G, 1003);
    archived["thread_metadata"]["archived"] = json!(true);
    let pins = |channel: u64| {
        json!({"guild_id": gid(), "channel_id": channel.to_string(),
            "last_pin_timestamp": "2026-10-20T12:00:00.000000+00:00"})
    };

    vec![
        in_guild(
            "AUTO_MODERATION_ACTION_EXECUTION",
            json!({
                "guild_id": gid(), "action": {"type": 1}, "rule_id": "70",
                "rule_trigger_type": 1, "user_id": "10", "channel_id": "1003",
                "message_id": "1", "content": "x", "matched_keyword": null,
                "matched_content": null,
            }),
        ),
        in_guild("AUTO_MODERATION_RULE_CREATE", automod_rule()),
        in_guild("AUTO_MODERATION_RULE_UPDATE", automod_rule()),
        in_guild("AUTO_MODERATION_RULE_DELETE", automod_rule()),
        in_guild("CHANNEL_CREATE", channel(1006, Some(G), "new")),
        in_dm("CHANNEL_CREATE (DM)", dm_channel()),
        in_guild("CHANNEL_UPDATE", channel(1003, Some(G), "renamed")),
        in_guild("CHANNEL_DELETE", channel(1004, Some(G), "random")),
        in_dm("CHANNEL_DELETE (DM)", dm_channel()),
        in_guild("CHANNEL_PINS_UPDATE", pins(1003)),
        in_guild("CHANNEL_PINS_UPDATE (thread)", pins(1005)),
        in_guild(
            "APPLICATION_COMMAND_PERMISSIONS_UPDATE",
            json!({"id": "1", "application_id": BOT_ID.to_string(), "guild_id": gid(),
                "permissions": []}),
        ),
        in_guild(
            "ENTITLEMENT_CREATE",
            json!({"id": "2", "sku_id": "3", "application_id": BOT_ID.to_string(),
                "user_id": "10", "type": 8, "deleted": false, "starts_at": null,
                "ends_at": null, "guild_id": gid(), "consumed": false}),
        ),
        in_guild(
            "GUILD_AUDIT_LOG_ENTRY_CREATE",
            json!({"guild_id": gid(), "id": "4", "action_type": 1, "user_id": "10",
                "target_id": gid(), "changes": [], "reason": null}),
        ),
        in_guild(
            "GUILD_BAN_ADD",
            json!({"guild_id": gid(), "user": user(30, "banned")}),
        ),
        in_guild(
            "GUILD_BAN_REMOVE",
            json!({"guild_id": gid(), "user": user(30, "banned")}),
        ),
        in_guild("GUILD_CREATE", full_guild()),
        in_guild(
            "GUILD_CREATE (unavailable)",
            json!({"id": gid(), "unavailable": true}),
        ),
        in_guild("GUILD_DELETE", json!({"id": gid()})),
        in_guild(
            "GUILD_DELETE (outage)",
            json!({"id": gid(), "unavailable": true}),
        ),
        in_guild(
            "GUILD_EMOJIS_UPDATE",
            json!({"guild_id": gid(), "emojis": [emoji(1100, "new")]}),
        ),
        in_guild("GUILD_INTEGRATIONS_UPDATE", json!({"guild_id": gid()})),
        in_guild("GUILD_SCHEDULED_EVENT_CREATE", scheduled_event()),
        in_guild("GUILD_SCHEDULED_EVENT_UPDATE", scheduled_event()),
        in_guild("GUILD_SCHEDULED_EVENT_DELETE", scheduled_event()),
        in_guild(
            "GUILD_SCHEDULED_EVENT_USER_ADD",
            json!({"guild_scheduled_event_id": "80", "user_id": "10", "guild_id": gid()}),
        ),
        in_guild(
            "GUILD_SCHEDULED_EVENT_USER_REMOVE",
            json!({"guild_scheduled_event_id": "80", "user_id": "10", "guild_id": gid()}),
        ),
        in_guild(
            "GUILD_MEMBERS_CHUNK",
            json!({
                "guild_id": gid(), "members": [member(10, &[]), member(BOT_ID, &[])],
                "chunk_index": 0, "chunk_count": 1, "not_found": [],
                "presences": [presence(G, 10, "idle")],
            }),
        ),
        in_guild("GUILD_MEMBER_ADD", member_add),
        in_guild(
            "GUILD_MEMBER_REMOVE",
            json!({"guild_id": gid(), "user": user(10, "user10")}),
        ),
        in_guild("GUILD_MEMBER_UPDATE", member_update(10)),
        // The bot's own member is cached without STATE_MEMBER.
        in_guild("GUILD_MEMBER_UPDATE (bot)", member_update(BOT_ID)),
        in_guild(
            "GUILD_ROLE_CREATE",
            json!({"guild_id": gid(), "role": role(1010, "new")}),
        ),
        in_guild(
            "GUILD_ROLE_UPDATE",
            json!({"guild_id": gid(), "role": role(1001, "admins")}),
        ),
        in_guild(
            "GUILD_ROLE_DELETE",
            json!({"guild_id": gid(), "role_id": "1001"}),
        ),
        in_guild(
            "GUILD_STICKERS_UPDATE",
            json!({"guild_id": gid(), "stickers": [sticker(1201)]}),
        ),
        in_guild("GUILD_UPDATE", guild_update),
        in_guild("INTEGRATION_CREATE", integration()),
        in_guild("INTEGRATION_UPDATE", integration()),
        in_guild("INTEGRATION_DELETE", json!({"id": "60", "guild_id": gid()})),
        in_guild(
            "INTERACTION_CREATE",
            json!({"id": "5", "application_id": BOT_ID.to_string(), "type": 1,
                "token": "t", "version": 1, "guild_id": gid(),
                "authorizing_integration_owners": {}, "entitlements": []}),
        ),
        in_guild(
            "INVITE_CREATE",
            json!({"channel_id": "1003", "code": "abc", "guild_id": gid(),
                "created_at": "2026-10-20T12:00:00.000000+00:00", "max_age": 0,
                "max_uses": 0, "temporary": false, "uses": 0}),
        ),
        in_guild(
            "INVITE_DELETE",
            json!({"channel_id": "1003", "code": "abc", "guild_id": gid()}),
        ),
        in_guild("MESSAGE_CREATE", message(4, 1003, Some(G), "hello")),
        in_dm("MESSAGE_CREATE (DM)", message(5, DM, None, "dm")),
        in_guild("MESSAGE_UPDATE", message(1, 1003, Some(G), "edited")),
        in_guild(
            "MESSAGE_DELETE",
            json!({"id": "1", "channel_id": "1003", "guild_id": gid()}),
        ),
        in_guild(
            "MESSAGE_DELETE_BULK",
            json!({"ids": ["1"], "channel_id": "1003", "guild_id": gid()}),
        ),
        in_guild("MESSAGE_REACTION_ADD", reaction(true)),
        in_guild("MESSAGE_REACTION_REMOVE", reaction(false)),
        in_guild(
            "MESSAGE_REACTION_REMOVE_EMOJI",
            json!({"channel_id": "1003", "guild_id": gid(), "message_id": "1",
                "emoji": {"id": null, "name": "x"}}),
        ),
        in_guild(
            "MESSAGE_REACTION_REMOVE_ALL",
            json!({"channel_id": "1003", "message_id": "1", "guild_id": gid()}),
        ),
        in_guild(
            "MESSAGE_POLL_VOTE_ADD",
            json!({"user_id": "10", "channel_id": "1003", "message_id": "1",
                "guild_id": gid(), "answer_id": 1}),
        ),
        in_guild(
            "MESSAGE_POLL_VOTE_REMOVE",
            json!({"user_id": "10", "channel_id": "1003", "message_id": "1",
                "guild_id": gid(), "answer_id": 1}),
        ),
        // Another user than the variants, so both keys are around at the end.
        in_guild("PRESENCE_UPDATE", presence(G, 12, "idle")),
        in_guild("PRESENCE_UPDATE (offline)", presence(G, 10, "offline")),
        // As Discord sends it; see rate_limited_is_published_without_parsing.
        in_guild(
            "RATE_LIMITED",
            json!({"opcode": 8, "retry_after": 1.5, "meta": {"guild_id": gid(), "nonce": "n"}}),
        ),
        of_bot("READY", ready([0, 1], &[G])),
        of_bot("RESUMED", json!(null)),
        in_guild("STAGE_INSTANCE_CREATE", stage_instance()),
        in_guild("STAGE_INSTANCE_UPDATE", stage_instance()),
        in_guild("STAGE_INSTANCE_DELETE", stage_instance()),
        in_guild("THREAD_CREATE", thread(1007, G, 1003)),
        in_guild("THREAD_UPDATE", thread(1005, G, 1003)),
        in_guild("THREAD_UPDATE (archived)", archived),
        in_guild(
            "THREAD_DELETE",
            json!({"id": "1005", "guild_id": gid(), "parent_id": "1003", "type": 11}),
        ),
        in_guild(
            "THREAD_LIST_SYNC",
            json!({"guild_id": gid(), "channel_ids": ["1003"],
                "threads": [thread(1008, G, 1003)], "members": []}),
        ),
        in_guild(
            "THREAD_MEMBER_UPDATE",
            json!({"id": "1005", "user_id": "10", "flags": 0, "guild_id": gid(),
                "join_timestamp": "2026-10-20T12:00:00.000000+00:00"}),
        ),
        in_guild(
            "THREAD_MEMBERS_UPDATE",
            json!({"id": "1005", "guild_id": gid(), "member_count": 1,
                "added_members": [], "removed_member_ids": ["10"]}),
        ),
        in_guild(
            "TYPING_START",
            json!({"channel_id": "1003", "guild_id": gid(), "user_id": "10",
                "timestamp": 1}),
        ),
        of_bot("USER_UPDATE", current_user()),
        in_guild(
            "VOICE_SERVER_UPDATE",
            json!({"token": "t", "guild_id": gid(), "endpoint": "e"}),
        ),
        in_guild("VOICE_STATE_UPDATE", voice_state(Some(G), 12, Some(1004))),
        in_guild("VOICE_STATE_UPDATE (left)", voice_state(Some(G), 10, None)),
        in_guild(
            "WEBHOOKS_UPDATE",
            json!({"guild_id": gid(), "channel_id": "1003"}),
        ),
    ]
}

/// Every event name twilight's dispatch deserializer accepts, read from the
/// vendored source (the arms of `DispatchEventWithTypeDeserializer`).
fn twilight_dispatch_events() -> BTreeSet<String> {
    let source = read_repo_file("vendor/twilight-model/src/gateway/event/dispatch.rs");
    let arms = block_after(
        &source,
        "DeserializeSeed<'de> for DispatchEventWithTypeDeserializer",
    )
    .expect("DispatchEventWithTypeDeserializer moved: update this guard");
    arms.lines()
        .filter_map(|line| {
            let (name, rest) = line.trim().strip_prefix('"')?.split_once('"')?;
            rest.trim_start().starts_with("=>").then(|| name.to_owned())
        })
        .collect()
}

/// A twilight upgrade that adds dispatch events fails here until they have a
/// fixture, so every guard below also covers them.
#[test]
fn universe_has_every_dispatch_event() {
    let twilight = twilight_dispatch_events();
    for name in &twilight {
        // The source parse matches the compiled crate.
        let kind = EventType::deserialize(Value::String(name.clone()))
            .unwrap_or_else(|err| panic!("{name} is not a twilight EventType: {err}"));
        assert_eq!(kind.name(), Some(name.as_str()));
    }
    assert!(
        twilight.len() > 60,
        "only {twilight:?}: parser out of date?"
    );

    let fixtures = universe();
    let mut labels = BTreeSet::new();
    for fixture in &fixtures {
        assert!(labels.insert(fixture.label), "{} twice", fixture.label);
    }
    let kinds: BTreeSet<String> = fixtures.iter().map(|f| f.kind.to_owned()).collect();
    assert_eq!(
        kinds.difference(&twilight).collect::<Vec<_>>(),
        Vec::<&String>::new(),
        "fixtures of events twilight does not dispatch"
    );
    assert_eq!(
        twilight.difference(&kinds).collect::<Vec<_>>(),
        Vec::<&String>::new(),
        "dispatch events without a fixture in universe()"
    );
}

/// Fixtures that would not parse would make every guard skip them.
#[test]
fn every_fixture_parses_as_its_event() {
    for fixture in universe() {
        if fixture.kind == "RATE_LIMITED" {
            continue;
        }
        match parse_event(&fixture.raw(1)) {
            Ok(Some(event)) => assert_eq!(event.kind().name(), Some(fixture.kind)),
            other => panic!("{}: {other:?}", fixture.label),
        }
    }
}

/// twilight 0.17 models RATE_LIMITED's `meta` as an externally tagged enum
/// (`{"RequestGuildMembers": {...}}`), while Discord sends the fields
/// themselves. Harmless as long as nothing parses it; a feature that uses
/// RATE_LIMITED needs a vendor patch first, and then this test changes.
#[test]
fn rate_limited_is_published_without_parsing() {
    let fixtures = universe();
    let raw = find(&fixtures, "RATE_LIMITED").raw(1);
    let err = parse_event(&raw).expect_err("twilight parses RATE_LIMITED now");
    assert!(err.contains("unknown variant `guild_id`"), "{err}");
    for (name, state) in states() {
        match prepare(&raw, &state) {
            Prepared::Dispatch {
                kind,
                event: None,
                parse_error: None,
                ..
            } => assert_eq!(kind, "RATE_LIMITED"),
            other => panic!("with {name}: {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// A connection that records what the cache sends

/// What a connection was asked to send.
#[derive(Clone, Debug)]
enum Sent {
    Command(Vec<String>),
    Pipeline {
        /// Sent as one MULTI/EXEC transaction.
        atomic: bool,
        commands: Vec<Vec<String>>,
    },
}

/// Commands that only read.
const READS: [&str; 16] = [
    "EXISTS",
    "GET",
    "HEXISTS",
    "HGET",
    "HGETALL",
    "HMGET",
    "MGET",
    "PTTL",
    "SCARD",
    "SISMEMBER",
    "SMEMBERS",
    "TTL",
    "TYPE",
    "ZRANGE",
    "ZRANGEBYSCORE",
    "ZSCORE",
];

/// Every command that changes something, in order.
fn writes(sent: &[Sent]) -> Vec<&[String]> {
    sent.iter()
        .flat_map(|sent| match sent {
            Sent::Command(command) => std::slice::from_ref(command),
            Sent::Pipeline { commands, .. } => commands.as_slice(),
        })
        .filter(|command| !READS.contains(&command[0].as_str()))
        .map(Vec::as_slice)
        .collect()
}

fn names(commands: &[&[String]]) -> Vec<String> {
    commands.iter().map(|c| c[0].clone()).collect()
}

fn args(cmd: &Cmd) -> Vec<String> {
    cmd.args_iter()
        .map(|arg| match arg {
            redis::Arg::Simple(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            _ => "<cursor>".to_owned(),
        })
        .collect()
}

/// Records every command, then passes it on to `inner`.
struct Recording<C> {
    inner: C,
    sent: Vec<Sent>,
    /// Commands refused with NOSCRIPT: a script Redis did not have yet, so it
    /// did not run. redis-rs then sends SCRIPT LOAD and the EVALSHA again.
    scripts_not_run: usize,
}

impl<C> Recording<C> {
    fn new(inner: C) -> Self {
        Self {
            inner,
            sent: Vec::new(),
            scripts_not_run: 0,
        }
    }
}

impl<C: ConnectionLike + Send> ConnectionLike for Recording<C> {
    fn req_packed_command<'a>(&'a mut self, cmd: &'a Cmd) -> RedisFuture<'a, RedisValue> {
        self.sent.push(Sent::Command(args(cmd)));
        Box::pin(async move {
            let reply = self.inner.req_packed_command(cmd).await;
            // Error replies are values here; `Cmd::query_async` makes them errors.
            if matches!(&reply, Ok(RedisValue::ServerError(err)) if err.code() == "NOSCRIPT") {
                self.scripts_not_run += 1;
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
        self.sent.push(Sent::Pipeline {
            atomic: pipeline.is_transaction(),
            commands: pipeline.cmd_iter().map(args).collect(),
        });
        self.inner.req_packed_commands(pipeline, offset, count)
    }

    fn get_db(&self) -> i64 {
        self.inner.get_db()
    }
}

/// What the fake Redis holds.
#[derive(Clone, Copy, Debug)]
enum Cache {
    /// Every key has a value and every index set has members, so every
    /// read-modify-write of the cache goes ahead.
    Everything,
    /// Nothing, so every "only if cached" branch is skipped and every
    /// "insert if missing" branch runs.
    Nothing,
}

const CACHES: [Cache; 2] = [Cache::Everything, Cache::Nothing];

/// A Redis that answers reads as `Cache` says, SCRIPT LOAD with the script's
/// SHA1 and every other write with OK.
struct Fake(Cache);

/// A cached value for any key, with what every read-modify-write looks at.
fn cached_value(key: &str) -> String {
    json!({
        "id": key.rsplit(':').next().unwrap_or("1"), "guild_id": gid(), "name": "cached",
        "type": 0, "parent_id": "1003", "member_count": 5, "tts": false,
        "content": "cached", "user": {"id": "10", "username": "cached"}, "stickers": [],
    })
    .to_string()
}

/// Index set members when everything is cached (layout: `cache::keys`).
fn index_members(set: &str) -> Vec<String> {
    match set.split_once(':') {
        None if set == "guild_keys" => vec![format!("guild:{G}"), format!("guild:{OTHER}")],
        Some(("guild_keys", g)) => {
            let mut keys = vec!["channel:1003".to_owned(), "channel:1004".to_owned()];
            for (prefix, id) in [
                ("thread", 1005),
                ("role", 1001),
                ("emoji", 1002),
                ("member", 10),
                ("presence", 10),
                ("voice", 10),
            ] {
                keys.push(format!("{prefix}:{g}:{id}"));
            }
            keys
        }
        Some(("thread_keys", g)) => vec![format!("thread:{g}:1005")],
        Some(("channel_keys", channel)) => vec![format!("message:{channel}:1")],
        _ => Vec::new(),
    }
}

impl Fake {
    fn reply(&self, args: &[String]) -> RedisValue {
        let everything = matches!(self.0, Cache::Everything);
        let bulk = |value: String| RedisValue::BulkString(value.into_bytes());
        match args[0].as_str() {
            "GET" if everything => bulk(cached_value(&args[1])),
            "GET" => RedisValue::Nil,
            "MGET" => RedisValue::Array(
                args[1..]
                    .iter()
                    .map(|key| {
                        if everything {
                            bulk(cached_value(key))
                        } else {
                            RedisValue::Nil
                        }
                    })
                    .collect(),
            ),
            "SMEMBERS" if everything => {
                RedisValue::Array(index_members(&args[1]).into_iter().map(bulk).collect())
            }
            "SMEMBERS" => RedisValue::Array(Vec::new()),
            "EXISTS" => RedisValue::Int(i64::from(everything)),
            // redis-rs checks the SHA1 that SCRIPT LOAD answers with.
            "SCRIPT" if args[1] == "LOAD" => {
                bulk(redis::Script::new(&args[2]).get_hash().to_owned())
            }
            _ => RedisValue::Okay,
        }
    }
}

impl ConnectionLike for Fake {
    fn req_packed_command<'a>(&'a mut self, cmd: &'a Cmd) -> RedisFuture<'a, RedisValue> {
        let reply = self.reply(&args(cmd));
        Box::pin(async move { Ok(reply) })
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        pipeline: &'a Pipeline,
        _offset: usize,
        _count: usize,
    ) -> RedisFuture<'a, Vec<RedisValue>> {
        let replies: Vec<RedisValue> = pipeline
            .cmd_iter()
            .map(|cmd| self.reply(&args(cmd)))
            .collect();
        // EXEC answers with one array of every reply.
        let replies = if pipeline.is_transaction() {
            vec![RedisValue::Array(replies)]
        } else {
            replies
        };
        Box::pin(async move { Ok(replies) })
    }

    fn get_db(&self) -> i64 {
        0
    }
}

/// `Script::load_async` asserts (in debug builds) that SCRIPT LOAD answers
/// with the script's SHA1, so a cache arm that loads a script would panic in
/// every guard that runs it against the fake.
#[tokio::test]
async fn the_fake_loads_scripts_as_redis_does() {
    let script = redis::Script::new("return 1");
    for cache in CACHES {
        let hash = script.load_async(&mut Fake(cache)).await.unwrap();
        assert_eq!(hash, script.get_hash(), "{cache:?}");
    }
}

// ---------------------------------------------------------------------------
// Helpers

fn update_context(state: &StateConfig) -> UpdateContext<'_> {
    UpdateContext {
        state,
        bot_id: Some(bot_id()),
        now_ms: now_ms(),
    }
}

/// STATE_* combinations: everything, the cache with no optional part, the
/// cache off, and each switch on its own and off on its own.
fn states() -> Vec<(String, StateConfig)> {
    let all = state_all();
    let minimal = StateConfig {
        enabled: true,
        member_ttl: all.member_ttl,
        message_ttl: all.message_ttl,
        ..StateConfig::default()
    };
    let mut states = vec![
        ("all".to_owned(), all.clone()),
        ("minimal".to_owned(), minimal.clone()),
        ("disabled".to_owned(), StateConfig::default()),
    ];
    type Switch = fn(&mut StateConfig) -> &mut bool;
    let switches: [(&str, Switch); 6] = [
        ("member", |s| &mut s.member),
        ("message", |s| &mut s.message),
        ("presence", |s| &mut s.presence),
        ("emoji", |s| &mut s.emoji),
        ("voice", |s| &mut s.voice),
        ("old", |s| &mut s.old),
    ];
    // A new STATE_* switch has to be added above.
    let debug = format!("{:?}", StateConfig::default());
    let flags: BTreeSet<&str> = debug
        .split([',', '{', '}'])
        .filter_map(|field| field.trim().strip_suffix(": false"))
        .collect();
    let known: BTreeSet<&str> = switches
        .iter()
        .map(|(name, _)| *name)
        .chain(["enabled"])
        .collect();
    assert_eq!(flags, known, "StateConfig switches changed");

    for (name, switch) in switches {
        let mut only = minimal.clone();
        *switch(&mut only) = true;
        states.push((format!("only {name}"), only));
        let mut without = all.clone();
        *switch(&mut without) = false;
        states.push((format!("all but {name}"), without));
    }
    states
}

/// The states in which the worker applies events to the cache.
fn cache_states() -> Vec<(String, StateConfig)> {
    states().into_iter().filter(|(_, s)| s.enabled).collect()
}

/// The typed event the shard runner hands to the workers.
fn runner_event(fixture: &Fixture, state: &StateConfig) -> Option<Event> {
    match prepare(&fixture.raw(2), state) {
        Prepared::Dispatch { event, .. } => event,
        other => panic!("{} is no dispatch: {other:?}", fixture.label),
    }
}

/// What `cache::update` sends for `fixture` when Redis holds what `cache`
/// says, whether or not the shard runner parses the event (`None`: twilight
/// cannot parse it at all).
async fn cache_commands(fixture: &Fixture, state: &StateConfig, cache: Cache) -> Option<Vec<Sent>> {
    let mut event = parse_event(&fixture.raw(2)).ok()??;
    let mut conn = Recording::new(Fake(cache));
    cache::update(&mut conn, &mut event, &update_context(state))
        .await
        .unwrap_or_else(|err| panic!("{} with {cache:?} cached: {err}", fixture.label));
    Some(conn.sent)
}

/// Apply a fixture as production does: only if the shard runner parses it.
async fn apply_as_runner(conn: &mut ConnectionManager, state: &StateConfig, fixture: &Fixture) {
    if let Some(mut event) = runner_event(fixture, state) {
        cache::update(conn, &mut event, &update_context(state))
            .await
            .unwrap_or_else(|err| panic!("{}: {err}", fixture.label));
    }
}

async fn flush(conn: &mut ConnectionManager) {
    let _: () = redis::cmd("FLUSHDB").query_async(conn).await.unwrap();
}

/// Guild `G` cached by the universe's GUILD_CREATE, with a message in both
/// channels and in the thread.
async fn seed_guild(conn: &mut ConnectionManager, fixtures: &[Fixture]) {
    flush(conn).await;
    let state = state_all();
    apply_as_runner(conn, &state, find(fixtures, "GUILD_CREATE")).await;
    for (id, channel) in [(1, 1003), (2, 1004), (3, 1005)] {
        let d = message(id, channel, Some(G), "seeded");
        apply(conn, &state, "MESSAGE_CREATE", d).await;
    }
}

/// `seed_guild` plus the DM channel with a message.
async fn seed_guild_and_dm(conn: &mut ConnectionManager, fixtures: &[Fixture]) {
    seed_guild(conn, fixtures).await;
    let state = state_all();
    apply_as_runner(conn, &state, find(fixtures, "CHANNEL_CREATE (DM)")).await;
    apply(
        conn,
        &state,
        "MESSAGE_CREATE",
        message(6, DM, None, "seeded"),
    )
    .await;
}

/// Every key with its value (strings) or each member (sets and sorted sets;
/// expiry deadlines depend on the clock and are left out).
async fn dump(conn: &mut ConnectionManager) -> BTreeSet<String> {
    let keys: Vec<String> = redis::cmd("KEYS").arg("*").query_async(conn).await.unwrap();
    // redis-rs refuses an empty pipeline.
    if keys.is_empty() {
        return BTreeSet::new();
    }
    // Pipelined: every_event_is_one_transaction dumps before and after each
    // event.
    let mut types = redis::pipe();
    for key in &keys {
        types.cmd("TYPE").arg(key);
    }
    let types: Vec<String> = types.query_async(conn).await.unwrap();
    let mut reads = redis::pipe();
    for (key, kind) in keys.iter().zip(&types) {
        match kind.as_str() {
            "string" => reads.cmd("GET").arg(key),
            "set" => reads.cmd("SMEMBERS").arg(key),
            "zset" => reads.cmd("ZRANGE").arg(key).arg(0).arg(-1),
            // Only the type of anything else.
            _ => reads.cmd("TYPE").arg(key),
        };
    }
    let values: Vec<RedisValue> = reads.query_async(conn).await.unwrap();
    let mut entries = BTreeSet::new();
    for (key, value) in keys.iter().zip(values) {
        let values = match value {
            RedisValue::Array(members) | RedisValue::Set(members) => members,
            value => vec![value],
        };
        for value in values {
            let value = match value {
                RedisValue::BulkString(bytes) => String::from_utf8(bytes).unwrap(),
                RedisValue::SimpleString(kind) => format!("<{kind}>"),
                other => panic!("{key}: {other:?}"),
            };
            entries.insert(format!("{key} {value}"));
        }
    }
    entries
}

fn read_repo_file(path: impl AsRef<Path>) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(path);
    std::fs::read_to_string(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The `{ ... }` block after the first `needle` in `source`.
fn block_after<'a>(source: &'a str, needle: &str) -> Option<&'a str> {
    let after = source.find(needle)? + needle.len();
    let open = after + source[after..].find('{')?;
    let mut depth = 0;
    for (i, c) in source[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&source[open..=open + i]);
                }
            }
            _ => {}
        }
    }
    None
}

// ---------------------------------------------------------------------------
// runner::wants_typed and cache::update decide the same thing

/// Events the cache could use but the runner deliberately does not parse.
fn deliberately_unparsed(kind: &str, state: &StateConfig) -> bool {
    // A chunk holds up to 1000 members. Without STATE_MEMBER and
    // STATE_PRESENCE only the bot's own member in it would be kept, and
    // GUILD_CREATE and GUILD_MEMBER_UPDATE (always parsed) keep that anyway.
    kind == "GUILD_MEMBERS_CHUNK" && !state.member && !state.presence
}

/// The worker only caches events the shard runner parsed, and the runner
/// only parses what `runner::wants_typed` lists. A cache arm whose event (or
/// STATE_* switch) is missing there never runs in production, while tests
/// that call `cache::update` directly pass.
#[tokio::test]
async fn everything_the_cache_does_is_reachable_through_the_runner() {
    let fixtures = universe();
    // What the cache sends -> the STATE_* combinations it is unreachable in.
    let mut dead: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, state) in cache_states() {
        // Events the runner skips in this state whose cache update was run.
        let mut checked = 0;
        for fixture in &fixtures {
            if runner_event(fixture, &state).is_some()
                || deliberately_unparsed(fixture.kind, &state)
            {
                continue;
            }
            for cache in CACHES {
                let Some(sent) = cache_commands(fixture, &state, cache).await else {
                    continue;
                };
                checked += 1;
                let changes = writes(&sent);
                if !changes.is_empty() {
                    let sends = format!(
                        "{} ({cache:?} cached): {:?}",
                        fixture.label,
                        names(&changes)
                    );
                    dead.entry(sends).or_default().push(name.clone());
                }
            }
        }
        assert!(checked > 0, "with {name} no event was checked");
    }
    assert!(
        dead.is_empty(),
        "the cache does this, but the runner does not parse the event: {dead:#?}"
    );
}

/// Parsed with the cache off. GUILD_CREATE and GUILD_DELETE are for the
/// worker's guild join / leave log; READY's typed event is only applied to
/// the cache, so it is not used then (its session is read separately).
const PARSED_WITHOUT_THE_CACHE: [&str; 3] = ["GUILD_CREATE", "GUILD_DELETE", "READY"];

/// The other direction: what the runner parses for the cache is used there
/// (in some state of the cache), so payloads are not parsed for nothing and
/// their parse errors are not counted as cache problems.
#[tokio::test]
async fn everything_the_runner_parses_is_used() {
    let fixtures = universe();
    // Event type -> the STATE_* combinations it is parsed for nothing in.
    let mut unused: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for (name, state) in states() {
        let parsed: BTreeSet<&str> = fixtures
            .iter()
            .filter(|f| runner_event(f, &state).is_some())
            .map(|f| f.kind)
            .collect();
        assert!(!parsed.is_empty(), "with {name} the runner parses nothing");
        for kind in parsed {
            if !state.enabled {
                if !PARSED_WITHOUT_THE_CACHE.contains(&kind) {
                    unused.entry(kind).or_default().push(name.clone());
                }
                continue;
            }
            let mut used = false;
            for fixture in fixtures.iter().filter(|f| f.kind == kind) {
                for cache in CACHES {
                    let sent = cache_commands(fixture, &state, cache).await;
                    used |= sent.is_some_and(|sent| !writes(&sent).is_empty());
                }
            }
            if !used {
                unused.entry(kind).or_default().push(name.clone());
            }
        }
    }
    assert!(
        unused.is_empty(),
        "parsed, but the cache does nothing with it: {unused:#?}"
    );
}

// ---------------------------------------------------------------------------
// GUILD_CREATE / GUILD_UPDATE and merge::SPLIT_GUILD_FIELDS

/// The JSON values recorded MSET / SET commands store, by stored key.
fn stored_values(writes: &[&[String]]) -> Vec<(String, String)> {
    let mut values = Vec::new();
    for command in writes {
        let pairs = match command[0].as_str() {
            "MSET" => &command[1..],
            "SET" => &command[1..3],
            _ => continue,
        };
        for pair in pairs.chunks(2) {
            values.push((pair[0].clone(), pair[1].clone()));
        }
    }
    values
}

/// The list fields of twilight's `Guild`, read from the vendored source.
fn guild_lists() -> BTreeSet<String> {
    let source = read_repo_file("vendor/twilight-model/src/guild/mod.rs");
    let fields = block_after(&source, "pub struct Guild ").expect("Guild moved: update this guard");
    fields
        .lines()
        .filter_map(|line| {
            let (name, kind) = line.trim().strip_prefix("pub ")?.split_once(':')?;
            let kind = kind.trim();
            let kind = kind.strip_prefix("Option<").unwrap_or(kind);
            kind.starts_with("Vec<").then(|| name.to_owned())
        })
        .collect()
}

/// GUILD_UPDATE copies everything but SPLIT_GUILD_FIELDS into the guild blob,
/// so a list GUILD_CREATE moves into its own keys but that is missing from
/// SPLIT_GUILD_FIELDS comes back into the blob (and goes stale there).
#[tokio::test]
async fn split_guild_fields_are_what_guild_create_moves_out() {
    let fixtures = universe();
    let create = find(&fixtures, "GUILD_CREATE");
    let Some(Event::GuildCreate(parsed)) = parse_event(&create.raw(1)).unwrap() else {
        panic!("GUILD_CREATE");
    };
    let GuildCreate::Available(parsed) = *parsed else {
        panic!("an available guild");
    };
    let parsed = serde_json::to_value(&parsed).unwrap();

    // Only a list the fixture fills shows whether it is moved out, so a list
    // a twilight upgrade adds to `Guild` has to be filled in `full_guild()`.
    let lists = guild_lists();
    assert!(lists.len() > 5, "only {lists:?}: parser out of date?");
    let unfilled: Vec<&String> = lists
        .iter()
        .filter(|field| {
            parsed
                .get(field.as_str())
                .and_then(Value::as_array)
                .is_none_or(Vec::is_empty)
        })
        .collect();
    assert_eq!(
        unfilled,
        Vec::<&String>::new(),
        "lists full_guild() leaves empty"
    );

    let sent = cache_commands(create, &state_all(), Cache::Nothing)
        .await
        .unwrap();
    let values = stored_values(&writes(&sent));
    let (_, blob) = values
        .iter()
        .find(|(key, _)| *key == format!("guild:{G}"))
        .expect("the guild blob is written");
    let blob: Value = serde_json::from_str(blob).unwrap();

    let moved: BTreeSet<&str> = parsed
        .as_object()
        .unwrap()
        .iter()
        .filter(|(_, value)| value.as_array().is_some_and(|list| !list.is_empty()))
        // Emptied, or left out (twilight skips some empty lists).
        .filter(|(field, _)| {
            blob.get(field.as_str())
                .is_none_or(|value| value == &json!([]))
        })
        .map(|(field, _)| field.as_str())
        .collect();
    assert_eq!(
        moved,
        SPLIT_GUILD_FIELDS.into_iter().collect::<BTreeSet<_>>(),
        "blob: {blob:#}"
    );
}

/// GUILD_UPDATE carries a guild without GUILD_CREATE's extra fields; the
/// cached blob keeps every field the update does not carry, takes every one
/// it does, and keeps the split lists empty.
#[tokio::test]
async fn guild_update_keeps_every_field_it_does_not_split() {
    let Some(mut conn) = redis(4).await else {
        return;
    };
    let state = state_all();
    let fixtures = universe();
    let update = find(&fixtures, "GUILD_UPDATE");
    let Some(Event::GuildUpdate(parsed)) = runner_event(update, &state) else {
        panic!("GUILD_UPDATE is parsed");
    };
    let carried = serde_json::to_value(&parsed.0).unwrap();
    let carried = carried.as_object().unwrap();

    apply_as_runner(&mut conn, &state, find(&fixtures, "GUILD_CREATE")).await;
    let key = format!("guild:{G}");
    let before = get(&mut conn, &key).await.unwrap();
    apply_as_runner(&mut conn, &state, update).await;
    let after = get(&mut conn, &key).await.unwrap();

    let split = |field: &str| SPLIT_GUILD_FIELDS.contains(&field);
    let before = before.as_object().unwrap();
    // Each of the three properties is checked on at least one field.
    assert!(
        before
            .keys()
            .any(|field| !split(field) && !carried.contains_key(field)),
        "GUILD_UPDATE carries every field of the blob: nothing to keep"
    );
    assert!(
        carried
            .iter()
            .any(|(field, value)| !split(field) && before.get(field) != Some(value)),
        "GUILD_UPDATE changes no field of the blob"
    );
    // An empty list copied into the blob still reads `[]` there.
    assert!(
        before.keys().any(|field| split(field)
            && carried
                .get(field)
                .and_then(Value::as_array)
                .is_some_and(|list| !list.is_empty())),
        "GUILD_UPDATE carries no split list of the blob: nothing to keep out"
    );
    let empty = json!([]);
    for (field, value) in before {
        let expected = if split(field) {
            &empty
        } else {
            carried.get(field).unwrap_or(value)
        };
        assert_eq!(after.get(field), Some(expected), "{field}");
    }
    for (field, value) in carried {
        if !split(field) {
            assert_eq!(after.get(field), Some(value), "{field}");
        }
    }
}

// ---------------------------------------------------------------------------
// Removals of a previous session (worker::only_removes)

/// Whether a job queued in a previous session (before a new READY) whose
/// update only removes cached state is still applied (`worker::only_removes`).
/// Those that are not would be undone anyway: the new session's GUILD_CREATE
/// replaces the guild's presences, voice states and (active) threads.
const STALE_REMOVALS: [(&str, bool); 10] = [
    ("CHANNEL_DELETE", true),
    ("CHANNEL_DELETE (DM)", true),
    ("GUILD_DELETE", true),
    ("GUILD_ROLE_DELETE", true),
    ("MESSAGE_DELETE", true),
    ("MESSAGE_DELETE_BULK", true),
    ("PRESENCE_UPDATE (offline)", false),
    ("THREAD_DELETE", true),
    ("THREAD_UPDATE (archived)", false),
    ("VOICE_STATE_UPDATE (left)", false),
];

/// Every event whose cache update only deletes has a decision above. Removals
/// that are skipped after a reconnect stay cached for good unless the new
/// session sends that state again, so a new removal has to be decided on.
#[tokio::test]
async fn every_removal_has_a_stale_session_decision() {
    let mut removals = BTreeSet::new();
    for fixture in universe() {
        let (mut deletes, mut writes_other) = (false, false);
        for (_, state) in cache_states() {
            for cache in CACHES {
                let Some(sent) = cache_commands(&fixture, &state, cache).await else {
                    continue;
                };
                for command in writes(&sent) {
                    match command[0].as_str() {
                        "DEL" => deletes = true,
                        // Index and expiry entries of the deleted keys.
                        "SREM" | "ZREM" => {}
                        _ => writes_other = true,
                    }
                }
            }
        }
        if deletes && !writes_other {
            removals.insert(fixture.label);
        }
    }
    let decided: BTreeSet<&str> = STALE_REMOVALS.iter().map(|(label, _)| *label).collect();
    let undecided: Vec<&&str> = removals.difference(&decided).collect();
    let no_removals: Vec<&&str> = decided.difference(&removals).collect();
    assert!(
        undecided.is_empty() && no_removals.is_empty(),
        "removals without a decision in STALE_REMOVALS: {undecided:?}; decided but no (pure) \
         removals: {no_removals:?}"
    );
}

struct Discard;

impl Publisher for Discard {
    async fn publish(&self, _: &str, _: Vec<u8>, _: Option<String>) -> ApiResult<()> {
        Ok(())
    }
}

/// Process a fixture as a job dispatched in the shard's current session, or
/// in one that a new READY replaced while the job was queued.
async fn process(
    ctx: &WorkerContext<ConnectionManager, Discard>,
    conn: &mut ConnectionManager,
    handle: &ShardHandle,
    fixture: &Fixture,
    new_session: bool,
) {
    let json = fixture.raw(2);
    let Prepared::Dispatch { event, .. } = prepare(&json, &ctx.state) else {
        panic!("{}", fixture.label);
    };
    let job = Job {
        shard: 0,
        seq: Some(2),
        token: handle.dispatch(2),
        session: Some(Arc::from("old")),
        json,
        event,
    };
    if new_session {
        handle.new_session(1);
    }
    worker::process(ctx, conn, job).await;
}

#[tokio::test]
async fn stale_removals_follow_their_decision() {
    let Some(mut conn) = redis(2).await else {
        return;
    };
    let handle = Arc::new(ShardHandle::new(0, None, Some(1)));
    let ctx = WorkerContext {
        redis: conn.clone(),
        publisher: Arc::new(Discard),
        registry: Arc::new(Registry::new([handle.clone()])),
        state: state_all(),
        bot_id: bot_id(),
        cache_timeout: Duration::from_secs(5),
        discord: DiscordLog::disabled(),
        breaker: Breaker::default(),
        cache_errors: Throttle::default(),
        publish_errors: Throttle::default(),
    };
    let fixtures = universe();
    for (label, applied) in STALE_REMOVALS {
        let fixture = find(&fixtures, label);
        seed_guild_and_dm(&mut conn, &fixtures).await;
        let seeded = dump(&mut conn).await;
        process(&ctx, &mut conn, &handle, fixture, false).await;
        let current = dump(&mut conn).await;
        assert_ne!(current, seeded, "{label} removes nothing that is seeded");

        seed_guild_and_dm(&mut conn, &fixtures).await;
        process(&ctx, &mut conn, &handle, fixture, true).await;
        let stale = dump(&mut conn).await;
        let expected = if applied { &current } else { &seeded };
        let missing: Vec<&String> = expected.difference(&stale).collect();
        let unexpected: Vec<&String> = stale.difference(expected).collect();
        assert!(
            missing.is_empty() && unexpected.is_empty(),
            "{label} in a stale session should be {}; missing {missing:#?}, unexpected \
             {unexpected:#?}",
            if applied { "applied" } else { "skipped" }
        );
    }
}

// ---------------------------------------------------------------------------
// Nothing a guild's events cached outlives the guild

/// The key prefixes of guild data (`constants::*_KEY`).
const CACHED_PREFIXES: [&str; 9] = [
    "channel", "emoji", "guild", "member", "message", "presence", "role", "thread", "voice",
];

/// `cache::keys::plan` indexes `<prefix>:<a>:<b>` under `guild_keys:<a>`
/// unless `<prefix>` is `message`. A new key type whose second segment is not
/// a guild id (`reaction:<channel>:...`) is never found by GUILD_DELETE.
#[tokio::test]
async fn leaving_a_guild_removes_everything_its_events_cached() {
    let Some(mut conn) = redis(1).await else {
        return;
    };
    let state = state_all();
    let fixtures = universe();
    let leave = find(&fixtures, "GUILD_DELETE");
    for seeded in [true, false] {
        flush(&mut conn).await;
        if seeded {
            seed_guild(&mut conn, &fixtures).await;
        }
        for fixture in &fixtures {
            if fixture.scope != Scope::Dm && fixture.label != leave.label {
                apply_as_runner(&mut conn, &state, fixture).await;
            }
        }
        // Every kind of key is cached when the guild is left.
        let cached = dump(&mut conn).await;
        for prefix in CACHED_PREFIXES {
            assert!(
                cached
                    .iter()
                    .any(|entry| entry.starts_with(&format!("{prefix}:"))),
                "nothing under {prefix}: (seeded: {seeded}): {cached:#?}"
            );
        }
        apply_as_runner(&mut conn, &state, leave).await;

        let left: Vec<String> = dump(&mut conn)
            .await
            .into_iter()
            // The bot user is no guild data.
            .filter(|entry| !entry.starts_with("bot_user "))
            .collect();
        assert!(left.is_empty(), "left behind (seeded: {seeded}): {left:#?}");
    }
}

// ---------------------------------------------------------------------------
// Every event is applied atomically

/// Commands that run a Lua script, which Redis runs atomically as well. Each
/// script that runs is an atomic write of its own, so two scripts for one
/// event are two writes.
const SCRIPTS: [&str; 2] = ["EVAL", "EVALSHA"];

/// Loads a script (for the EVALSHA redis-rs retries after NOSCRIPT) and
/// stores no data.
fn loads_script(command: &[String]) -> bool {
    matches!(command, [script, load, ..] if script == "SCRIPT" && load == "LOAD")
}

/// A connection error between two writes leaves keys that no index refers
/// to, so an event that changes the cache does so in exactly one atomic
/// write: one MULTI/EXEC (`cache::batch`) or one script. Whether it changed
/// anything is read from Redis, not from the recording, so a write the
/// recording does not count as one fails too. READY is the exception: one
/// transaction for the bot user and the guild stubs, then one per cached
/// guild of the shard that READY no longer lists.
#[tokio::test]
async fn every_event_is_one_transaction() {
    let Some(mut conn) = redis(3).await else {
        return;
    };
    let fixtures = universe();
    let minimal = StateConfig {
        enabled: true,
        ..StateConfig::default()
    };
    let mut failures = Vec::new();
    let mut stale_readies = 0;
    // Events other than READY that changed the cache, so were checked.
    let mut changes = 0;
    for (name, state) in [("all", state_all()), ("minimal", minimal)] {
        for seeded in [true, false] {
            flush(&mut conn).await;
            if seeded {
                seed_guild_and_dm(&mut conn, &fixtures).await;
                let d = guild(OTHER, "other");
                apply(&mut conn, &state_all(), "GUILD_CREATE", d).await;
            }
            for fixture in &fixtures {
                let Some(mut event) = runner_event(fixture, &state) else {
                    continue;
                };
                // READY's shard is shard 0 of 1: every guild is on it.
                let stale = if fixture.kind == "READY" {
                    members(&mut conn, "guild_keys")
                        .await
                        .iter()
                        .filter(|key| **key != format!("guild:{G}"))
                        .count()
                } else {
                    0
                };
                let before = dump(&mut conn).await;
                let mut recording = Recording::new(conn.clone());
                cache::update(&mut recording, &mut event, &update_context(&state))
                    .await
                    .unwrap_or_else(|err| panic!("{}: {err}", fixture.label));
                let changed = dump(&mut conn).await != before;

                let at = format!("{} with {name} (seeded: {seeded})", fixture.label);
                let mut transactions = Vec::new();
                let mut scripts = 0;
                for sent in &recording.sent {
                    match sent {
                        Sent::Command(command) if READS.contains(&command[0].as_str()) => {}
                        Sent::Command(command) if SCRIPTS.contains(&command[0].as_str()) => {
                            scripts += 1
                        }
                        Sent::Command(command) if loads_script(command) => {}
                        Sent::Command(command) => {
                            failures.push(format!("{at}: {command:?} outside a transaction"))
                        }
                        Sent::Pipeline {
                            atomic: false,
                            commands,
                        } => {
                            let names: Vec<&str> = commands.iter().map(|c| c[0].as_str()).collect();
                            failures.push(format!("{at}: {names:?} in a pipeline without MULTI"))
                        }
                        Sent::Pipeline { commands, .. } => transactions.push(commands),
                    }
                }
                let atomic_writes = transactions.len() + scripts - recording.scripts_not_run;
                if fixture.kind == "READY" {
                    let bot_user = transactions.first().is_some_and(|first| {
                        first.iter().any(|c| c[0] == "SET" && c[1] == "bot_user")
                    });
                    stale_readies += usize::from(stale > 0);
                    if !bot_user || atomic_writes != 1 + stale {
                        failures.push(format!(
                            "{at}: {atomic_writes} atomic writes for {stale} stale guilds (bot \
                             user first: {bot_user})"
                        ));
                    }
                } else if atomic_writes > 1 || (changed && atomic_writes == 0) {
                    // An update that changes nothing (a DEL of keys that are
                    // not cached) may still send its transaction.
                    failures.push(format!(
                        "{at}: {atomic_writes} atomic writes (changed the cache: {changed})"
                    ));
                } else {
                    changes += usize::from(changed);
                }
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
    assert_eq!(stale_readies, 2, "READY found a stale guild when seeded");
    assert!(
        changes > 0,
        "no event changed the cache: nothing was checked"
    );
}

// ---------------------------------------------------------------------------
// Routing: one guild's events stay in order

/// Guild events that do not go to their guild's worker (so they are not
/// ordered with the guild's other events).
const UNROUTED_GUILD_EVENTS: [(&str, &str); 1] = [("RATE_LIMITED", "its guild is in `meta`")];

/// Events of one guild are cached and published in order because they all go
/// to the guild's worker. An event whose guild id sits where routing does
/// not look for it overtakes (or lags) the guild's other events.
#[test]
fn guild_events_go_to_their_guilds_worker() {
    for fixture in universe() {
        let unrouted = UNROUTED_GUILD_EVENTS
            .iter()
            .any(|(kind, _)| *kind == fixture.kind);
        let expected = match fixture.scope {
            // How DM channel events are routed is not decided here.
            Scope::Dm => continue,
            Scope::Bot => Route::Unrouted,
            Scope::Guild if unrouted => Route::Unrouted,
            Scope::Guild => Route::Guild(G),
        };
        match prepare(&fixture.raw(1), &state_all()) {
            Prepared::Dispatch { route, .. } => assert_eq!(route, expected, "{}", fixture.label),
            other => panic!("{}: {other:?}", fixture.label),
        }
    }
}

// ---------------------------------------------------------------------------
// Broker topology

/// Deployed brokers keep the exchanges and queues every earlier version
/// declared, and refuse a declaration with other flags (PRECONDITION_FAILED):
/// the service then cannot start. CI starts with a fresh broker, where any
/// flags work, so the historical topology is declared first here.
#[tokio::test]
async fn the_topology_matches_what_existing_brokers_have() {
    let Some(url) = amqp_url() else {
        return;
    };
    let connection = Connection::connect(&url, ConnectionProperties::default())
        .await
        .unwrap();
    let channel = connection.create_channel().await.unwrap();
    let exchange = ExchangeDeclareOptions {
        passive: false,
        durable: true,
        auto_delete: false,
        internal: false,
        nowait: false,
    };
    let queue = QueueDeclareOptions {
        passive: false,
        durable: true,
        exclusive: false,
        auto_delete: false,
        nowait: false,
    };
    for (name, kind) in [
        ("gateway", ExchangeKind::Topic),
        ("gateway.send.shards", ExchangeKind::Direct),
    ] {
        channel
            .exchange_declare(name.into(), kind, exchange, FieldTable::default())
            .await
            .unwrap_or_else(|err| panic!("exchange {name}: {err}"));
    }
    for name in ["gateway.recv", "gateway.send"] {
        channel
            .queue_declare(name.into(), queue, FieldTable::default())
            .await
            .unwrap_or_else(|err| panic!("queue {name}: {err}"));
    }

    // The service's declarations on start: publisher, then command consumer.
    let amqp = Amqp::connect(url.clone(), true)
        .await
        .expect("the service declares the deployed topology");
    let consume = connection.create_channel().await.unwrap();
    declare_consume(&consume, [0])
        .await
        .expect("the command consumer declares the deployed topology");

    // The broker does refuse other flags, so the checks above mean something.
    let other = connection.create_channel().await.unwrap();
    let refused = other
        .exchange_declare(
            "gateway".into(),
            ExchangeKind::Topic,
            ExchangeDeclareOptions {
                durable: false,
                ..exchange
            },
            FieldTable::default(),
        )
        .await
        .expect_err("a non-durable gateway exchange is refused");
    assert!(refused.to_string().contains("PRECONDITION"), "{refused}");

    amqp.close().await;
    let _ = connection.close(200, "done".into()).await;
}

// ---------------------------------------------------------------------------
// Knowledge copied from twilight

/// FATAL_CLOSE_CODES repeats twilight's CloseCode::can_reconnect, which
/// decides when a shard stops for good (ShardState::FatallyClosed); codes
/// twilight does not know reconnect.
#[test]
fn fatal_close_codes_are_the_ones_twilight_does_not_reconnect() {
    for code in 0..=u16::MAX {
        let twilight = CloseCode::try_from(code).is_ok_and(|code| !code.can_reconnect());
        assert_eq!(is_fatal_close(code), twilight, "close code {code}");
    }
}

/// Heartbeat, Identify and Resume belong to the shard, and the client never
/// sends what it receives: injecting one from gateway.send closes or hijacks
/// the session.
#[test]
fn shard_owned_opcodes_are_never_allowed() {
    let owned: Vec<u64> = (0..=u8::MAX)
        .filter(|op| {
            OpCode::from(*op).is_some_and(|op| {
                op.is_received()
                    || matches!(op, OpCode::Heartbeat | OpCode::Identify | OpCode::Resume)
            })
        })
        .map(u64::from)
        .collect();
    assert_eq!(owned, [0, 1, 2, 6, 7, 9, 10, 11]);
    for op in owned {
        assert!(!ALLOWED_OPCODES.contains(&op), "opcode {op} is allowed");
    }
}

/// Upgrading twilight means copying a new release over vendor/ and
/// re-applying PATCHES.md. Cached flags must keep bits twilight does not know
/// (PATCHES.md item 3); a flags type the new release adds, or one whose patch
/// got lost, would silently drop them.
#[test]
fn vendored_flags_keep_unknown_bits() {
    let mut files = Vec::new();
    rust_files(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("vendor/twilight-model/src"),
        &mut files,
    );
    let mut checked = Vec::new();
    let mut truncating = Vec::new();
    for file in files {
        let source = std::fs::read_to_string(&file).unwrap();
        if !source.contains("bitflags!") {
            continue;
        }
        let types = source.lines().filter_map(|line| {
            let (name, bits) = line.trim().strip_prefix("pub struct ")?.split_once(':')?;
            bits.trim()
                .strip_suffix('{')?
                .trim()
                .starts_with('u')
                .then_some(name)
        });
        for name in types {
            // Only sent (IDENTIFY), never cached.
            if name == "Intents" {
                continue;
            }
            let deserialize = block_after(&source, &format!("Deserialize<'de> for {name} "))
                .unwrap_or_else(|| panic!("{}: no Deserialize for {name}", file.display()));
            // Including the visitor it hands the value to, if any.
            let mut code = deserialize.to_owned();
            for word in deserialize.split(|c: char| !c.is_alphanumeric() && c != '_') {
                if word.ends_with("Visitor") && word != "Visitor" {
                    code.extend(block_after(&source, &format!(" for {word} ")));
                }
            }
            if !code.contains("from_bits_retain")
                || code.contains("from_bits_truncate")
                || code.contains("from_bits(")
            {
                truncating.push(format!("{name} ({})", file.display()));
            }
            checked.push(name.to_owned());
        }
    }
    assert!(truncating.is_empty(), "{truncating:#?}");
    assert!(
        checked.len() >= 11,
        "only {checked:?}: scanner out of date?"
    );
}

/// twilight removed `Member::guild_id` and CachedMember adds it back with
/// `#[serde(flatten)]`. If twilight adds it again the stored JSON has the key
/// twice, which a `serde_json::Value` (and so a snapshot) does not show.
#[test]
fn cached_members_have_one_guild_id() {
    let cached: Member = serde_json::from_value(member(10, &[])).unwrap();
    let json = serde_json::to_string(&CachedMember {
        guild_id: Id::new(G),
        member: &cached,
    })
    .unwrap();
    assert_eq!(json.matches(r#""guild_id""#).count(), 1, "{json}");
}

/// Any JSON; fails on an object with a key twice.
struct UniqueKeys;

impl<'de> Deserialize<'de> for UniqueKeys {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(UniqueKeys)
    }
}

impl<'de> Visitor<'de> for UniqueKeys {
    type Value = UniqueKeys;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("JSON")
    }

    fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self, E> {
        Ok(self)
    }

    fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self, E> {
        Ok(self)
    }

    fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self, E> {
        Ok(self)
    }

    fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self, E> {
        Ok(self)
    }

    fn visit_str<E: de::Error>(self, _: &str) -> Result<Self, E> {
        Ok(self)
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self, E> {
        Ok(self)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self, A::Error> {
        while seq.next_element::<UniqueKeys>()?.is_some() {}
        Ok(self)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self, A::Error> {
        let mut keys = BTreeSet::new();
        while let Some(key) = map.next_key::<String>()? {
            map.next_value::<UniqueKeys>()?;
            if !keys.insert(key.clone()) {
                return Err(de::Error::custom(format!("{key:?} twice")));
            }
        }
        Ok(self)
    }
}

/// The same for everything the cache stores: values built by combining
/// structs (flatten) can repeat a key.
#[tokio::test]
async fn the_cache_stores_no_key_twice() {
    let mut checked = BTreeSet::new();
    for fixture in universe() {
        for cache in CACHES {
            let Some(sent) = cache_commands(&fixture, &state_all(), cache).await else {
                continue;
            };
            for (key, value) in stored_values(&writes(&sent)) {
                serde_json::from_str::<UniqueKeys>(&value)
                    .unwrap_or_else(|err| panic!("{}: {key} {err}: {value}", fixture.label));
                checked.extend(key.split(':').next().map(str::to_owned));
            }
        }
    }
    // A value stored some other way than SET / MSET is not checked at all.
    let unchecked: Vec<&str> = CACHED_PREFIXES
        .into_iter()
        .filter(|prefix| !checked.contains(*prefix))
        .collect();
    assert!(unchecked.is_empty(), "no value checked under {unchecked:?}");
}
