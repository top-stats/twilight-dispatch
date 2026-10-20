#![recursion_limit = "256"]

//! Property-based tests (proptest) for the pure parts of the service: cache key
//! planning, cache batches, JSON merges, gateway payload handling, sequence
//! tracking, configuration, gateway command validation and log throttling.
//! One model-based test replays random gateway event sequences through
//! `cache::update` against Redis (database 9; skipped without
//! `TEST_REDIS_URL`) and checks the whole cache after every event.
//!
//! Every property runs with a fixed RNG seed, so a run is deterministic and a
//! failure reproduces. To explore further, set `PROPTEST_RNG_SEED=<n>` and/or
//! `PROPTEST_CASES=<n>` (the latter applies to every property).

mod common;

use proptest::{prelude::*, test_runner::RngSeed};
use redis::aio::ConnectionManager;
use serde::Deserialize;
use serde_json::{json, value::RawValue, Map, Value};
use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use twilight_dispatch::{
    cache::{
        self,
        batch::{Batch, CHUNK},
        keys, merge, Outcome, UpdateContext, RECENT_JOIN_MS,
    },
    commands::{
        self, DeliveryInfo, DeliveryOpcode, InvalidCommand, Plan, ShardCommand, ALLOWED_OPCODES,
        MAX_PAYLOAD_BYTES,
    },
    config::{self, Config, ConfigError, ShardRange, StateConfig},
    constants::{self, EXPIRY_ZSET},
    gateway::{
        self,
        payload::{self, Envelope, Route},
        registry::{ShardHandle, STATUS_LABELS},
        runner::{self, Prepared},
        seq::{SeqToken, SeqTracker, FAILED_RETENTION},
    },
    models::{FormattedDateTime, SessionInfo, StatusInfo},
    util::Throttle,
};
use twilight_model::{
    gateway::{
        presence::{ActivityType, Status},
        Intents,
    },
    id::Id,
    util::Timestamp,
};

// ---------------------------------------------------------------------------
// Runner configuration and shared strategies

const SEED: u64 = 0x7d15_7a7c_0ffe_e5ed;

/// `cases` cases with a fixed seed and no regression files written into the
/// repository. `PROPTEST_CASES` / `PROPTEST_RNG_SEED` override both.
fn cases(cases: u32) -> ProptestConfig {
    let base = ProptestConfig::default();
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(cases);
    let rng_seed = if std::env::var_os("PROPTEST_RNG_SEED").is_some() {
        base.rng_seed
    } else {
        RngSeed::Fixed(SEED)
    };
    ProptestConfig {
        cases,
        rng_seed,
        failure_persistence: None,
        ..base
    }
}

/// Realistic snowflakes (a millisecond timestamp since the Discord epoch,
/// worker / process ids and an increment) plus edge values. Never 0.
fn snowflake() -> BoxedStrategy<u64> {
    prop_oneof![
        6 => (0u64..(1 << 42), 0u64..32, 0u64..32, 0u64..4096).prop_map(
            |(ms, worker, process, increment)| {
                (ms << 22) | (worker << 17) | (process << 12) | increment
            }
        ),
        1 => 1u64..10_000,
        1 => Just(u64::MAX),
        1 => any::<u64>(),
    ]
    .prop_map(|id| id.max(1))
    .boxed()
}

/// Field names Discord uses (and a few envelope keys), plus noise.
const FIELD_NAMES: &[&str] = &[
    "id",
    "guild_id",
    "channel_id",
    "user",
    "member",
    "roles",
    "type",
    "name",
    "flags",
    "permissions",
    "status",
    "activities",
    "application_id",
    "joined_at",
    "timestamp",
    "content",
    "author",
    "unavailable",
    "threads",
    "channels",
    "members",
    "parent_id",
    "nick",
    "member_count",
    "session_id",
    "resume_gateway_url",
    "t",
    "op",
    "s",
    "d",
];

fn field_name() -> BoxedStrategy<String> {
    prop_oneof![
        4 => prop::sample::select(FIELD_NAMES).prop_map(str::to_owned),
        1 => "[a-z_]{1,10}",
        1 => ".{0,6}",
    ]
    .boxed()
}

fn json_leaf() -> BoxedStrategy<Value> {
    prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(Value::from),
        any::<u64>().prop_map(Value::from),
        any::<f64>().prop_filter_map("finite", |f| serde_json::Number::from_f64(f)
            .map(Value::Number)),
        snowflake().prop_map(|id| Value::String(id.to_string())),
        "(?s).{0,12}".prop_map(Value::String),
    ]
    .boxed()
}

/// Arbitrary JSON, biased towards Discord-like keys.
fn json_value() -> impl Strategy<Value = Value> {
    json_leaf().prop_recursive(4, 48, 6, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
            prop::collection::vec((field_name(), inner), 0..6)
                .prop_map(|fields| Value::Object(fields.into_iter().collect())),
        ]
    })
}

fn json_object() -> BoxedStrategy<Map<String, Value>> {
    prop::collection::vec((field_name(), json_value()), 0..8)
        .prop_map(|fields| fields.into_iter().collect())
        .boxed()
}

/// `value` after a JSON round trip.
fn reparse(value: &Value) -> Value {
    serde_json::from_str(&value.to_string()).unwrap()
}

/// `json` with every non-ASCII character and every `/` written as an escape.
/// Only valid for compact JSON, where those characters only occur in strings.
fn ascii_escaped(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    for c in json.chars() {
        if c == '/' {
            out.push_str("\\/");
        } else if c.is_ascii() {
            out.push(c);
        } else {
            let mut units = [0u16; 2];
            for unit in c.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out
}

/// The raw text of a JSON value: compact, pretty or with escapes, as the
/// gateway might send it.
fn raw_json() -> BoxedStrategy<String> {
    prop_oneof![
        8 => (json_value(), 0u8..3).prop_map(|(value, style)| match style {
            0 => serde_json::to_string(&value).unwrap(),
            1 => serde_json::to_string_pretty(&value).unwrap(),
            _ => ascii_escaped(&serde_json::to_string(&value).unwrap()),
        }),
        1 => Just("true".to_owned()),
        1 => Just("false".to_owned()),
    ]
    .boxed()
}

/// Dispatch names the service handles, a few it does not, and noise.
const EVENT_NAMES: &[&str] = &[
    "READY",
    "RESUMED",
    "GUILD_CREATE",
    "GUILD_UPDATE",
    "GUILD_DELETE",
    "GUILD_ROLE_CREATE",
    "GUILD_ROLE_UPDATE",
    "GUILD_ROLE_DELETE",
    "GUILD_EMOJIS_UPDATE",
    "GUILD_STICKERS_UPDATE",
    "GUILD_MEMBER_ADD",
    "GUILD_MEMBER_REMOVE",
    "GUILD_MEMBER_UPDATE",
    "GUILD_MEMBERS_CHUNK",
    "CHANNEL_CREATE",
    "CHANNEL_UPDATE",
    "CHANNEL_DELETE",
    "CHANNEL_PINS_UPDATE",
    "THREAD_CREATE",
    "THREAD_UPDATE",
    "THREAD_DELETE",
    "THREAD_LIST_SYNC",
    "MESSAGE_CREATE",
    "MESSAGE_UPDATE",
    "MESSAGE_DELETE",
    "MESSAGE_DELETE_BULK",
    "PRESENCE_UPDATE",
    "VOICE_STATE_UPDATE",
    "USER_UPDATE",
    "TYPING_START",
    "MESSAGE_REACTION_ADD",
    "INTERACTION_CREATE",
];

fn event_name() -> BoxedStrategy<String> {
    prop_oneof![
        4 => prop::sample::select(EVENT_NAMES).prop_map(str::to_owned),
        1 => "[A-Z_]{1,24}",
        1 => "(?s).{0,12}",
    ]
    .boxed()
}

fn state_config() -> BoxedStrategy<StateConfig> {
    (any::<[bool; 7]>(), 0u64..100_000, 0u64..100_000)
        .prop_map(|(f, member_ttl, message_ttl)| {
            let enabled = f[0];
            StateConfig {
                enabled,
                member: enabled && f[1],
                member_ttl,
                message: enabled && f[2],
                message_ttl,
                presence: enabled && f[3],
                emoji: enabled && f[4],
                voice: enabled && f[5],
                old: enabled && f[6],
            }
        })
        .boxed()
}

// ---------------------------------------------------------------------------
// cache::keys: the key layout is a contract with the consumers of the cache

const CHILD_PREFIXES: &[&str] = &["role", "emoji", "member", "presence", "voice"];
const UNPREFIXED_KEYS: &[&str] = &["bot_user", "gateway_statuses", "gateway_started"];

/// The logical keys the service writes, built with the `constants` helpers.
#[derive(Clone, Debug)]
enum Shape {
    Guild(u64),
    GuildChannel(u64, u64),
    PrivateChannel(u64),
    Thread(u64, u64),
    Message(u64, u64),
    Child(&'static str, u64, u64),
    Unprefixed(&'static str),
}

fn shape() -> BoxedStrategy<Shape> {
    prop_oneof![
        snowflake().prop_map(Shape::Guild),
        (snowflake(), snowflake()).prop_map(|(g, c)| Shape::GuildChannel(g, c)),
        snowflake().prop_map(Shape::PrivateChannel),
        (snowflake(), snowflake()).prop_map(|(g, t)| Shape::Thread(g, t)),
        (snowflake(), snowflake()).prop_map(|(c, m)| Shape::Message(c, m)),
        (
            prop::sample::select(CHILD_PREFIXES),
            snowflake(),
            snowflake()
        )
            .prop_map(|(prefix, g, id)| Shape::Child(prefix, g, id)),
        prop::sample::select(UNPREFIXED_KEYS).prop_map(Shape::Unprefixed),
    ]
    .boxed()
}

impl Shape {
    fn logical(&self) -> String {
        match *self {
            Self::Guild(g) => constants::guild_key(Id::new(g)),
            Self::GuildChannel(g, c) => constants::channel_key(Some(Id::new(g)), Id::new(c)),
            Self::PrivateChannel(c) => constants::channel_key(None, Id::new(c)),
            Self::Thread(g, t) => constants::thread_key(Id::new(g), Id::new(t)),
            Self::Message(c, m) => constants::message_key(Id::new(c), Id::new(m)),
            Self::Child(prefix, g, id) => {
                let guild = Id::new(g);
                match prefix {
                    "role" => constants::role_key(guild, Id::new(id)),
                    "emoji" => constants::emoji_key(guild, Id::new(id)),
                    "member" => constants::member_key(guild, Id::new(id)),
                    "presence" => constants::presence_key(guild, Id::new(id)),
                    "voice" => constants::voice_key(guild, Id::new(id)),
                    other => unreachable!("{other}"),
                }
            }
            Self::Unprefixed(key) => key.to_owned(),
        }
    }

    /// The stored key and its index sets, as documented in the README and in
    /// `cache/keys.rs`.
    fn expected(&self) -> (String, BTreeSet<(String, String)>) {
        let (stored, sets): (String, Vec<String>) = match *self {
            Self::Guild(g) => (format!("guild:{g}"), vec!["guild_keys".into()]),
            Self::GuildChannel(g, c) => (
                format!("channel:{c}"),
                vec!["channel_keys".into(), format!("guild_keys:{g}")],
            ),
            Self::PrivateChannel(c) => (format!("channel:{c}"), vec!["channel_keys".into()]),
            Self::Thread(g, t) => (
                format!("thread:{g}:{t}"),
                vec![
                    "thread_keys".into(),
                    format!("guild_keys:{g}"),
                    format!("thread_keys:{g}"),
                ],
            ),
            Self::Message(c, m) => (
                format!("message:{c}:{m}"),
                vec!["message_keys".into(), format!("channel_keys:{c}")],
            ),
            Self::Child(prefix, g, id) => (
                format!("{prefix}:{g}:{id}"),
                vec![format!("{prefix}_keys"), format!("guild_keys:{g}")],
            ),
            Self::Unprefixed(key) => (key.to_owned(), Vec::new()),
        };
        let indexes = sets.into_iter().map(|set| (set, stored.clone())).collect();
        (stored, indexes)
    }
}

/// Key-like strings (prefixes and colons in odd places) and pure noise.
fn key_noise() -> BoxedStrategy<String> {
    prop_oneof![
        3 => "(guild|channel|thread|message|role|member|bot_user|[a-z_]{0,6})(:[0-9a-z]{0,6}){0,4}",
        1 => "(?s).{0,32}",
    ]
    .boxed()
}

proptest! {
    #![proptest_config(cases(512))]

    #[test]
    fn keys_follow_the_documented_layout(key_shape in shape()) {
        let key = key_shape.logical();
        let (stored, indexes) = key_shape.expected();
        let plan = keys::plan(&key);

        prop_assert_eq!(&plan.stored, &stored);
        prop_assert_eq!(keys::stored_key(&key), stored.clone());
        let planned: BTreeSet<(String, String)> = plan.indexes.iter().cloned().collect();
        prop_assert_eq!(planned.len(), plan.indexes.len(), "duplicate index entries");
        prop_assert_eq!(planned, indexes);
        // A stored key never moves again.
        prop_assert_eq!(keys::stored_key(&stored), stored.clone());

        // The index helpers name the same sets.
        match key_shape {
            Shape::Guild(_) => {
                prop_assert!(plan.indexes.contains(&(keys::type_index("guild"), stored.clone())));
            }
            Shape::GuildChannel(g, _) => {
                prop_assert!(plan.indexes.contains(&(keys::guild_index(g), stored.clone())));
            }
            Shape::Thread(g, _) => {
                prop_assert!(plan.indexes.contains(&(keys::guild_index(g), stored.clone())));
                prop_assert!(plan.indexes.contains(&(keys::thread_index(g), stored.clone())));
            }
            Shape::Message(c, _) => {
                prop_assert!(plan.indexes.contains(&(keys::channel_index(c), stored.clone())));
            }
            Shape::Child(prefix, g, _) => {
                prop_assert!(plan.indexes.contains(&(keys::type_index(prefix), stored.clone())));
                prop_assert!(plan.indexes.contains(&(keys::guild_index(g), stored.clone())));
            }
            Shape::PrivateChannel(_) | Shape::Unprefixed(_) => {}
        }
    }

    #[test]
    fn ids_are_read_back_from_stored_keys(key_shape in shape()) {
        let (stored, _) = key_shape.expected();
        let channel = match key_shape {
            Shape::GuildChannel(_, c) | Shape::PrivateChannel(c) => Some(c),
            _ => None,
        };
        let guild = match key_shape {
            Shape::Guild(g) => Some(g),
            _ => None,
        };
        let thread = match key_shape {
            Shape::Thread(_, t) => Some(t),
            _ => None,
        };
        prop_assert_eq!(keys::channel_id_of(&stored), channel);
        prop_assert_eq!(keys::guild_id_of(&stored), guild);
        prop_assert_eq!(keys::thread_id_of(&stored), thread);
    }

    #[test]
    fn planning_any_key_indexes_only_its_stored_key(key in key_noise()) {
        let plan = keys::plan(&key);
        prop_assert_eq!(&plan.stored, &keys::stored_key(&key));

        let colons = key.matches(':').count();
        let expected = match colons {
            0 => 0,
            1 => 1,
            _ if key.starts_with("thread:") => 3,
            _ => 2,
        };
        prop_assert_eq!(plan.indexes.len(), expected, "{:?}", plan);
        for (set, member) in &plan.indexes {
            prop_assert_eq!(member, &plan.stored);
            prop_assert!(set.ends_with("_keys") || set.contains("_keys:"), "{}", set);
        }
        // Only guild channels are stored under another name.
        if plan.stored != key {
            prop_assert!(key.starts_with("channel:") && colons >= 2, "{} -> {}", key, plan.stored);
        }
        if colons <= 2 {
            prop_assert_eq!(keys::stored_key(&plan.stored), plan.stored.clone());
        }
        // Never panics on anything.
        let _ = (keys::channel_id_of(&key), keys::guild_id_of(&key), keys::thread_id_of(&key));
    }

    #[test]
    fn id_helpers_only_accept_their_own_shape(
        prefix in "[a-z_]{0,8}",
        id in any::<u64>(),
        middle in "[0-9]{0,4}",
    ) {
        prop_assert_eq!(keys::channel_id_of(&format!("channel:{id}")), Some(id));
        prop_assert_eq!(keys::guild_id_of(&format!("guild:{id}")), Some(id));
        prop_assert_eq!(keys::thread_id_of(&format!("thread:{middle}:{id}")), Some(id));
        // Logical guild channel keys and child keys are not channel / guild keys.
        prop_assert_eq!(keys::channel_id_of(&format!("channel:{middle}:{id}")), None);
        prop_assert_eq!(keys::guild_id_of(&format!("guild:{middle}:{id}")), None);
        if prefix != "channel" {
            prop_assert_eq!(keys::channel_id_of(&format!("{prefix}:{id}")), None);
        }
        if prefix != "guild" {
            prop_assert_eq!(keys::guild_id_of(&format!("{prefix}:{id}")), None);
        }
        if prefix != "thread" {
            prop_assert_eq!(keys::thread_id_of(&format!("{prefix}:{middle}:{id}")), None);
        }
    }

    #[test]
    fn guilds_are_sharded_by_their_creation_time(
        guild in snowflake(),
        total in 1u32..=u32::MAX,
        low_bits in 0u64..(1 << 22),
    ) {
        let shard = keys::shard_for_guild(guild, total);
        prop_assert!(shard < total);
        // https://discord.com/developers/docs/events/gateway#sharding
        prop_assert_eq!(u64::from(shard), (guild >> 22) % u64::from(total));
        // Worker, process and increment bits never move a guild.
        let sibling = (guild & !((1u64 << 22) - 1)) | low_bits;
        prop_assert_eq!(keys::shard_for_guild(sibling, total), shard);
        prop_assert_eq!(keys::shard_for_guild(guild, 1), 0);
    }

    #[test]
    fn stale_guilds_are_the_unlisted_guilds_of_the_shard(
        cached in prop::collection::vec(snowflake(), 0..40),
        listed in prop::collection::vec(any::<bool>(), 40),
        other_ready in prop::collection::vec(snowflake(), 0..5),
        total in 1u32..8,
        pick in any::<u32>(),
    ) {
        let shard = pick % total;
        let ready: HashSet<u64> = cached
            .iter()
            .zip(&listed)
            .filter(|(_, listed)| **listed)
            .map(|(id, _)| *id)
            .chain(other_ready)
            .collect();
        // SMEMBERS order is arbitrary and a guild may be seen twice.
        let mut input = cached.clone();
        input.extend(cached.iter().rev().take(5));

        let stale = keys::stale_guilds(input, &ready, shard, total);
        prop_assert!(stale.windows(2).all(|w| w[0] < w[1]), "sorted and deduplicated: {:?}", stale);
        let expected: BTreeSet<u64> = cached
            .iter()
            .copied()
            .filter(|id| keys::shard_for_guild(*id, total) == shard && !ready.contains(id))
            .collect();
        prop_assert_eq!(stale, expected.into_iter().collect::<Vec<_>>());
    }

    #[test]
    fn deleting_a_key_undoes_exactly_what_writing_it_did(key in prop_oneof![
        3 => shape().prop_map(|key_shape| key_shape.logical()),
        1 => key_noise(),
    ]) {
        let mut set = Batch::new();
        set.set_json(&key, "{}".into());
        set.expire(&key, 1, 0);
        let mut del = Batch::new();
        del.del(&key);

        let written = collect_sent(&set.commands())?;
        let deleted = collect_sent(&del.commands())?;
        let stored: Vec<String> = written.mset.iter().map(|(k, _)| k.clone()).collect();
        let expiring: Vec<String> = written.zadd.iter().map(|(_, k)| k.clone()).collect();
        prop_assert_eq!(&deleted.del, &stored);
        prop_assert_eq!(&deleted.zrem, &expiring);
        prop_assert_eq!(sorted(deleted.srem), sorted(written.sadd));
    }
}

// ---------------------------------------------------------------------------
// cache::batch

#[derive(Clone, Debug)]
enum BatchOp {
    Set(String, Value),
    SetJson(String, String),
    Del(String),
    DelRaw(String),
    Expire(String, u64, i64),
    Persist(String),
}

/// A small universe of keys, so one batch often touches a key twice (and a
/// guild channel and a private channel can share a stored key).
fn batch_key() -> BoxedStrategy<String> {
    prop_oneof![
        8 => (0u8..9, 1u64..4, 1u64..6).prop_map(|(kind, a, b)| match kind {
            0 => format!("guild:{a}"),
            1 => format!("channel:{a}:{b}"),
            2 => format!("channel:{b}"),
            3 => format!("thread:{a}:{b}"),
            4 => format!("message:{a}:{b}"),
            5 => format!("member:{a}:{b}"),
            6 => format!("role:{a}:{b}"),
            7 => format!("presence:{a}:{b}"),
            _ => "bot_user".to_owned(),
        }),
        1 => shape().prop_map(|shape| shape.logical()),
    ]
    .boxed()
}

fn ttl_ms() -> BoxedStrategy<u64> {
    prop_oneof![Just(0u64), 1u64..100_000, Just(u64::MAX), any::<u64>()].boxed()
}

/// Unix milliseconds (never negative in practice).
fn now_ms() -> BoxedStrategy<i64> {
    prop_oneof![1_600_000_000_000i64..1_900_000_000_000, 0..=i64::MAX].boxed()
}

fn batch_op() -> BoxedStrategy<BatchOp> {
    prop_oneof![
        3 => (batch_key(), json_value()).prop_map(|(key, value)| BatchOp::Set(key, value)),
        1 => (batch_key(), "(?s).{0,16}").prop_map(|(key, json)| BatchOp::SetJson(key, json)),
        3 => batch_key().prop_map(BatchOp::Del),
        1 => prop_oneof![
            batch_key(),
            Just("guild_keys:1".to_owned()),
            Just("channel_keys:2".to_owned()),
            Just("thread_keys:3".to_owned()),
        ]
        .prop_map(BatchOp::DelRaw),
        2 => (batch_key(), ttl_ms(), now_ms())
            .prop_map(|(key, ttl, now)| BatchOp::Expire(key, ttl, now)),
        1 => batch_key().prop_map(BatchOp::Persist),
    ]
    .boxed()
}

fn build_batch(ops: &[BatchOp]) -> Batch {
    let mut batch = Batch::new();
    for op in ops {
        match op {
            BatchOp::Set(key, value) => batch.set(key, value).expect("serializable"),
            BatchOp::SetJson(key, json) => batch.set_json(key, json.clone()),
            BatchOp::Del(key) => batch.del(key),
            BatchOp::DelRaw(key) => batch.del_raw(key.clone()),
            BatchOp::Expire(key, ttl, now) => batch.expire(key, *ttl, *now),
            BatchOp::Persist(key) => batch.persist(key),
        }
    }
    batch
}

/// `now + ttl`, saturating, in unix milliseconds.
fn deadline(ttl_ms: u64, now_ms: i64) -> i64 {
    (i128::from(now_ms) + i128::from(ttl_ms)).min(i128::from(i64::MAX)) as i64
}

fn sorted<T: Ord>(mut items: Vec<T>) -> Vec<T> {
    items.sort();
    items
}

/// The arguments of a batch's commands, grouped by command.
#[derive(Debug, Default)]
struct Sent {
    del: Vec<String>,
    srem: Vec<(String, String)>,
    zrem: Vec<String>,
    mset: Vec<(String, String)>,
    sadd: Vec<(String, String)>,
    zadd: Vec<(String, String)>,
}

fn collect_sent(commands: &[Vec<String>]) -> Result<Sent, TestCaseError> {
    let mut sent = Sent::default();
    let mut wrote = false;
    for command in commands {
        let (name, args) = command.split_first().expect("command name");
        let destructive = matches!(name.as_str(), "DEL" | "SREM" | "ZREM");
        prop_assert!(
            !(destructive && wrote),
            "{} after a write in {:?}",
            name,
            commands
        );
        wrote |= !destructive;
        match name.as_str() {
            "DEL" => {
                prop_assert!(!args.is_empty());
                sent.del.extend(args.iter().cloned());
            }
            "SREM" | "SADD" => {
                prop_assert!(args.len() >= 2, "{:?}", command);
                let pairs = args[1..].iter().map(|m| (args[0].clone(), m.clone()));
                if name == "SADD" {
                    sent.sadd.extend(pairs);
                } else {
                    sent.srem.extend(pairs);
                }
            }
            "ZREM" => {
                prop_assert_eq!(args[0].as_str(), EXPIRY_ZSET);
                prop_assert!(args.len() >= 2, "{:?}", command);
                sent.zrem.extend(args[1..].iter().cloned());
            }
            "MSET" => {
                prop_assert!(!args.is_empty() && args.len() % 2 == 0, "{:?}", command);
                sent.mset
                    .extend(args.chunks(2).map(|kv| (kv[0].clone(), kv[1].clone())));
            }
            "ZADD" => {
                prop_assert_eq!(args[0].as_str(), EXPIRY_ZSET);
                prop_assert!(args.len() >= 3 && args.len() % 2 == 1, "{:?}", command);
                sent.zadd
                    .extend(args[1..].chunks(2).map(|p| (p[0].clone(), p[1].clone())));
            }
            other => prop_assert!(false, "unexpected command {}", other),
        }
    }
    Ok(sent)
}

/// Enough of Redis to replay a batch.
#[derive(Clone, Debug, Default, PartialEq)]
struct FakeRedis {
    strings: BTreeMap<String, String>,
    sets: BTreeMap<String, BTreeSet<String>>,
    expiry: BTreeMap<String, i64>,
}

impl FakeRedis {
    fn apply(&mut self, command: &[String]) {
        match command[0].as_str() {
            "DEL" => {
                for key in &command[1..] {
                    self.strings.remove(key);
                    self.sets.remove(key);
                }
            }
            "SREM" => {
                if let Some(set) = self.sets.get_mut(&command[1]) {
                    for member in &command[2..] {
                        set.remove(member);
                    }
                    if set.is_empty() {
                        self.sets.remove(&command[1]);
                    }
                }
            }
            "SADD" => {
                let set = self.sets.entry(command[1].clone()).or_default();
                set.extend(command[2..].iter().cloned());
            }
            "ZREM" => {
                for member in &command[2..] {
                    self.expiry.remove(member);
                }
            }
            "MSET" => {
                for kv in command[1..].chunks(2) {
                    self.strings.insert(kv[0].clone(), kv[1].clone());
                }
            }
            "ZADD" => {
                for pair in command[2..].chunks(2) {
                    self.expiry
                        .insert(pair[1].clone(), pair[0].parse().expect("integer score"));
                }
            }
            other => panic!("unexpected command {other}"),
        }
    }
}

fn fake_redis() -> BoxedStrategy<FakeRedis> {
    let pairs = prop::collection::vec((batch_key(), "[a-z]{0,4}"), 0..12);
    let members = prop::collection::vec((batch_key(), batch_key()), 0..12);
    let expiry = prop::collection::vec((batch_key(), now_ms()), 0..6);
    (pairs, members, expiry)
        .prop_map(|(pairs, members, expiry)| {
            let mut redis = FakeRedis::default();
            for (key, value) in pairs {
                redis.strings.insert(keys::stored_key(&key), value);
            }
            for (key, other) in members {
                // Index entries of real keys, plus a few unrelated ones.
                for (set, member) in keys::plan(&key).indexes {
                    redis.sets.entry(set).or_default().insert(member);
                }
                redis
                    .sets
                    .entry("guild_keys:1".into())
                    .or_default()
                    .insert(keys::stored_key(&other));
            }
            for (key, at) in expiry {
                redis.expiry.insert(keys::stored_key(&key), at);
            }
            redis
        })
        .boxed()
}

proptest! {
    #![proptest_config(cases(384))]

    #[test]
    fn batch_commands_are_exactly_the_planned_writes(
        ops in prop::collection::vec(batch_op(), 0..40),
    ) {
        let batch = build_batch(&ops);
        let commands = batch.commands();
        prop_assert_eq!(batch.is_empty(), commands.is_empty());
        let sent = collect_sent(&commands)?;

        let mut expected = Sent::default();
        for op in &ops {
            match op {
                BatchOp::Set(key, value) => {
                    let plan = keys::plan(key);
                    expected.mset.push((plan.stored, serde_json::to_string(value).unwrap()));
                    expected.sadd.extend(plan.indexes);
                }
                BatchOp::SetJson(key, json) => {
                    let plan = keys::plan(key);
                    expected.mset.push((plan.stored, json.clone()));
                    expected.sadd.extend(plan.indexes);
                }
                BatchOp::Del(key) => {
                    let plan = keys::plan(key);
                    expected.del.push(plan.stored.clone());
                    expected.zrem.push(plan.stored);
                    expected.srem.extend(plan.indexes);
                }
                BatchOp::DelRaw(key) => expected.del.push(key.clone()),
                BatchOp::Expire(key, ttl, now) => {
                    // A TTL of 0 means "never expires": no ZADD at all.
                    if *ttl > 0 {
                        let deadline = deadline(*ttl, *now).to_string();
                        expected.zadd.push((deadline, keys::stored_key(key)));
                    }
                }
                BatchOp::Persist(key) => expected.zrem.push(keys::stored_key(key)),
            }
        }
        // Values, deletes and expiries keep the order of the calls (the last
        // write of a key wins); index sets are only membership.
        prop_assert_eq!(sent.mset, expected.mset);
        prop_assert_eq!(sent.del, expected.del);
        prop_assert_eq!(sent.zrem, expected.zrem);
        prop_assert_eq!(sent.zadd, expected.zadd);
        prop_assert_eq!(sorted(sent.sadd), sorted(expected.sadd));
        prop_assert_eq!(sorted(sent.srem), sorted(expected.srem));
    }

    #[test]
    fn a_committed_batch_leaves_writes_in_place_and_deletes_gone(
        before in fake_redis(),
        ops in prop::collection::vec(batch_op(), 0..30),
    ) {
        let mut redis = before.clone();
        for command in build_batch(&ops).commands() {
            redis.apply(&command);
        }

        // Deletes of one batch run before its writes, so whatever is written
        // ends up written, even when the same batch also deleted it.
        let mut expected = before;
        for op in &ops {
            match op {
                BatchOp::Del(key) => {
                    let plan = keys::plan(key);
                    expected.strings.remove(&plan.stored);
                    expected.sets.remove(&plan.stored);
                    expected.expiry.remove(&plan.stored);
                    for (set, member) in plan.indexes {
                        if let Some(members) = expected.sets.get_mut(&set) {
                            members.remove(&member);
                            if members.is_empty() {
                                expected.sets.remove(&set);
                            }
                        }
                    }
                }
                BatchOp::DelRaw(key) => {
                    expected.strings.remove(key);
                    expected.sets.remove(key);
                }
                BatchOp::Persist(key) => {
                    expected.expiry.remove(&keys::stored_key(key));
                }
                _ => {}
            }
        }
        for op in &ops {
            match op {
                BatchOp::Set(key, value) => {
                    let plan = keys::plan(key);
                    expected.strings.insert(plan.stored, serde_json::to_string(value).unwrap());
                    for (set, member) in plan.indexes {
                        expected.sets.entry(set).or_default().insert(member);
                    }
                }
                BatchOp::SetJson(key, json) => {
                    let plan = keys::plan(key);
                    expected.strings.insert(plan.stored, json.clone());
                    for (set, member) in plan.indexes {
                        expected.sets.entry(set).or_default().insert(member);
                    }
                }
                BatchOp::Expire(key, ttl, now) if *ttl > 0 => {
                    expected.expiry.insert(keys::stored_key(key), deadline(*ttl, *now));
                }
                _ => {}
            }
        }
        prop_assert_eq!(redis, expected);
    }
}

proptest! {
    #![proptest_config(cases(12))]

    #[test]
    fn large_batches_are_chunked_without_losing_anything(
        sets in 0usize..2600,
        dels in 0usize..2600,
        expires in 0usize..2600,
        persists in 0usize..1200,
        guilds in 1u64..3,
    ) {
        let mut batch = Batch::new();
        for i in 0..sets {
            batch.set_json(&format!("member:{}:{i}", i as u64 % guilds + 1), "{}".into());
        }
        for i in 0..dels {
            batch.del(&format!("channel:{}:{i}", i as u64 % guilds + 1));
        }
        for i in 0..expires {
            batch.expire(&format!("member:1:{i}"), 1 + i as u64, 1_700_000_000_000);
        }
        for i in 0..persists {
            batch.persist(&format!("presence:1:{i}"));
        }
        let commands = batch.commands();

        // At most CHUNK keys, members or pairs per command (Redis <= 6.2 drops
        // the connection beyond 1M arguments). ZADD carries the zset key and
        // CHUNK score/member pairs, so it is the longest: 2 * CHUNK + 2.
        for command in &commands {
            let (min, max) = match command[0].as_str() {
                "DEL" => (2, 1 + CHUNK),
                "SREM" | "SADD" | "ZREM" => (3, 2 + CHUNK),
                "MSET" => (3, 1 + 2 * CHUNK),
                "ZADD" => (4, 2 + 2 * CHUNK),
                other => return Err(TestCaseError::fail(format!("unexpected {other}"))),
            };
            prop_assert!(
                (min..=max).contains(&command.len()),
                "{} with {} arguments",
                command[0],
                command.len()
            );
        }
        let items = |name: &str, skip: usize, per: usize| -> usize {
            commands
                .iter()
                .filter(|c| c[0] == name)
                .map(|c| (c.len() - 1 - skip) / per)
                .sum()
        };
        let count = |name: &str| commands.iter().filter(|c| c[0] == name).count();
        prop_assert_eq!(items("MSET", 0, 2), sets);
        prop_assert_eq!(items("DEL", 0, 1), dels);
        prop_assert_eq!(items("ZADD", 1, 2), expires);
        prop_assert_eq!(items("ZREM", 1, 1), dels + persists);
        // Every member and guild channel is in its type index and its guild index.
        prop_assert_eq!(items("SADD", 1, 1), sets * 2);
        prop_assert_eq!(items("SREM", 1, 1), dels * 2);
        // Chunks are full: n items take ceil(n / CHUNK) commands.
        prop_assert_eq!(count("MSET"), sets.div_ceil(CHUNK));
        prop_assert_eq!(count("DEL"), dels.div_ceil(CHUNK));
        prop_assert_eq!(count("ZADD"), expires.div_ceil(CHUNK));
        prop_assert_eq!(count("ZREM"), (dels + persists).div_ceil(CHUNK));
    }
}

// ---------------------------------------------------------------------------
// cache::merge

fn cached_guild() -> BoxedStrategy<Option<Value>> {
    prop_oneof![
        1 => Just(None),
        // A full guild blob as GUILD_CREATE stores it.
        3 => (json_object(), "[a-z]{1,8}", 0u64..10_000).prop_map(|(mut blob, name, count)| {
            blob.insert("name".into(), json!(name));
            blob.insert("member_count".into(), json!(count));
            for field in merge::SPLIT_GUILD_FIELDS {
                blob.insert(field.into(), json!([]));
            }
            blob.insert("threads".into(), json!([{"id": "9"}]));
            Some(Value::Object(blob))
        }),
        // READY / outage stubs.
        1 => snowflake().prop_map(|id| Some(json!({"id": id.to_string(), "unavailable": true}))),
        // Whatever an older version or another writer left behind.
        1 => json_value().prop_map(Some),
    ]
    .boxed()
}

fn guild_update() -> BoxedStrategy<Map<String, Value>> {
    (
        json_object(),
        prop::option::of("[a-z]{1,8}"),
        any::<[bool; 3]>(),
    )
        .prop_map(|(mut update, name, extra)| {
            if let Some(name) = name {
                update.insert("name".into(), json!(name));
            }
            if extra[0] {
                update.insert("roles".into(), json!([{"id": "2"}]));
            }
            if extra[1] {
                update.insert("unavailable".into(), json!(true));
            }
            if extra[2] {
                update.insert("emojis".into(), json!([{"id": "3"}]));
            }
            update
        })
        .boxed()
}

proptest! {
    #![proptest_config(cases(512))]

    #[test]
    fn guild_update_merges_into_the_cached_blob(
        cached in cached_guild(),
        update in guild_update(),
    ) {
        let blob = merge::merge_guild_update(cached.clone(), Value::Object(update.clone()));
        let merged = blob.as_object().expect("an object");
        let split = |key: &str| merge::SPLIT_GUILD_FIELDS.contains(&key);

        // Split-out lists are always empty in the blob, and the guild is available.
        for field in merge::SPLIT_GUILD_FIELDS {
            prop_assert_eq!(&blob[field], &json!([]), "{}", field);
        }
        prop_assert_eq!(&blob["unavailable"], &json!(false));
        // The update wins for everything it carries.
        for (key, value) in &update {
            if !split(key) && key != "unavailable" {
                prop_assert_eq!(&blob[key.as_str()], value, "{}", key);
            }
        }

        // A full cached blob keeps the GUILD_CREATE-only fields; anything else
        // (no blob, a stub, garbage) starts from an empty blob.
        let full = cached
            .as_ref()
            .and_then(Value::as_object)
            .filter(|blob| blob.contains_key("name"));
        let mut keys: BTreeSet<&str> = merge::SPLIT_GUILD_FIELDS.into_iter().collect();
        keys.insert("unavailable");
        keys.extend(update.keys().map(String::as_str));
        match full {
            Some(cached) => {
                for (key, value) in cached {
                    if !update.contains_key(key) && !split(key) && key != "unavailable" {
                        prop_assert_eq!(&blob[key.as_str()], value, "{}", key);
                    }
                }
                keys.extend(cached.keys().map(String::as_str));
            }
            None => {
                keys.insert("threads");
                if !update.contains_key("threads") {
                    prop_assert_eq!(&blob["threads"], &json!([]));
                }
            }
        }
        prop_assert_eq!(merged.keys().map(String::as_str).collect::<BTreeSet<_>>(), keys);
    }

    #[test]
    fn a_repeated_guild_update_changes_nothing(cached in cached_guild(), update in guild_update()) {
        let once = merge::merge_guild_update(cached, Value::Object(update.clone()));
        let twice = merge::merge_guild_update(Some(once.clone()), Value::Object(update));
        prop_assert_eq!(twice, once);
    }

    #[test]
    fn a_guild_update_that_is_not_an_object_changes_nothing(
        cached in cached_guild(),
        update in json_value().prop_filter("not an object", |v| !v.is_object()),
    ) {
        let merged = merge::merge_guild_update(cached.clone(), update.clone());
        prop_assert_eq!(merged, cached.unwrap_or(update));
    }

    #[test]
    fn an_outage_only_flags_the_guild(cached in cached_guild(), id in snowflake()) {
        let marked = merge::mark_unavailable(cached.clone(), id);
        match cached {
            Some(Value::Object(mut blob)) => {
                blob.insert("unavailable".into(), json!(true));
                prop_assert_eq!(&marked, &Value::Object(blob));
            }
            _ => prop_assert_eq!(&marked, &json!({"id": id.to_string(), "unavailable": true})),
        }
        prop_assert_eq!(merge::is_stub(&marked), marked.get("name").is_none());
    }

    #[test]
    fn member_counts_saturate(
        count in prop_oneof![any::<u64>(), 0u64..5, (u64::MAX - 5)..=u64::MAX],
        delta in prop_oneof![any::<i64>(), -5i64..5, Just(i64::MIN), Just(i64::MAX)],
        blob in json_object(),
    ) {
        let mut guild = blob;
        guild.insert("member_count".into(), json!(count));
        let adjusted = merge::adjust_member_count(Value::Object(guild.clone()), delta)
            .expect("a count to adjust");
        let expected = (i128::from(count) + i128::from(delta)).clamp(0, i128::from(u64::MAX));
        prop_assert_eq!(&adjusted["member_count"], &json!(expected as u64));
        // Nothing else changes.
        let mut adjusted = adjusted;
        adjusted.as_object_mut().unwrap().remove("member_count");
        guild.remove("member_count");
        prop_assert_eq!(adjusted, Value::Object(guild));
    }

    #[test]
    fn member_counts_need_a_cached_count(value in json_value(), delta in any::<i64>()) {
        let countable = value.get("member_count").and_then(Value::as_u64).is_some();
        prop_assert_eq!(merge::adjust_member_count(value, delta).is_some(), countable);
    }
}

// ---------------------------------------------------------------------------
// gateway::payload: the published message is `{"op", "t", "d", "old"?}` and
// `d` is passed through byte for byte

#[derive(Clone, Debug)]
enum Field<T> {
    Absent,
    Null,
    Value(T),
}

impl<T> Field<T> {
    fn value(&self) -> Option<&T> {
        match self {
            Self::Value(value) => Some(value),
            Self::Absent | Self::Null => None,
        }
    }
}

fn field<T: Clone + std::fmt::Debug + 'static>(
    value: impl Strategy<Value = T> + 'static,
) -> impl Strategy<Value = Field<T>> {
    prop_oneof![
        1 => Just(Field::Absent),
        1 => Just(Field::Null),
        6 => value.prop_map(Field::Value),
    ]
}

/// A gateway payload as text, with the envelope keys in any order, optional
/// whitespace and an unknown extra key.
#[derive(Clone, Debug)]
struct RawEnvelope {
    op: u8,
    s: Field<u64>,
    t: Field<String>,
    d: Field<String>,
    extra: Option<(String, String)>,
    order: Vec<usize>,
    pad: &'static str,
}

impl RawEnvelope {
    fn json(&self) -> String {
        let field = |key: &str, value: Option<String>| value.map(|value| (key.to_owned(), value));
        let raw = |field: &Field<String>| match field {
            Field::Absent => None,
            Field::Null => Some("null".to_owned()),
            Field::Value(raw) => Some(raw.clone()),
        };
        let slots = [
            field("op", Some(self.op.to_string())),
            field(
                "s",
                match self.s {
                    Field::Absent => None,
                    Field::Null => Some("null".into()),
                    Field::Value(s) => Some(s.to_string()),
                },
            ),
            field(
                "t",
                match &self.t {
                    Field::Absent => None,
                    Field::Null => Some("null".into()),
                    Field::Value(t) => Some(serde_json::to_string(t).unwrap()),
                },
            ),
            field("d", raw(&self.d)),
            self.extra.clone(),
        ];
        let pad = self.pad;
        let fields: Vec<String> = self
            .order
            .iter()
            .filter_map(|i| slots[*i].as_ref())
            .map(|(key, value)| format!("{pad}\"{key}\"{pad}:{pad}{value}{pad}"))
            .collect();
        format!("{{{}}}", fields.join(","))
    }

    fn kind(&self) -> Option<&str> {
        self.t.value().map(String::as_str)
    }

    /// `d` as the envelope exposes it: an explicit `null` is no data.
    fn data(&self) -> Option<&str> {
        self.d.value().map(String::as_str).filter(|d| *d != "null")
    }

    /// Whether twilight's text scan (`GatewayEventDeserializer::from_json`,
    /// which looks for the first `"op":` and `"t":`) finds this payload's own
    /// `op` and `t`: compact JSON with both before `d` and the extra key, as
    /// Discord sends it.
    fn scannable(&self) -> bool {
        let at = |slot: usize| self.order.iter().position(|i| *i == slot).unwrap();
        let nested = at(3).min(at(4));
        self.pad.is_empty() && at(0) < nested && at(2) < nested
    }
}

fn raw_envelope() -> BoxedStrategy<RawEnvelope> {
    let op = prop_oneof![
        3 => Just(0u8),
        1 => prop::sample::select(vec![1u8, 7, 9, 10, 11]),
        1 => any::<u8>(),
    ];
    let extra = prop::option::of((
        "[a-z]{1,6}".prop_filter("not an envelope key", |key| {
            !["op", "s", "t", "d"].contains(&key.as_str())
        }),
        raw_json(),
    ));
    (
        op,
        field(any::<u64>()),
        field(event_name()),
        field(raw_json()),
        extra,
        Just((0..5).collect::<Vec<usize>>()).prop_shuffle(),
        prop::sample::select(vec!["", " ", "\n  ", "\t"]),
    )
        .prop_map(|(op, s, t, d, extra, order, pad)| RawEnvelope {
            op,
            s,
            t,
            d,
            extra,
            order,
            pad,
        })
        .boxed()
}

#[derive(Deserialize)]
struct Published<'a> {
    #[serde(borrow)]
    d: &'a RawValue,
}

proptest! {
    #![proptest_config(cases(512))]

    #[test]
    fn envelopes_parse_and_republish_unchanged(
        envelope in raw_envelope(),
        old in prop::option::of(json_value()),
    ) {
        let json = envelope.json();
        let parsed = Envelope::parse(&json);
        prop_assert!(parsed.is_ok(), "{:?}: {}", parsed.err(), json);
        let parsed = parsed.unwrap();
        prop_assert_eq!(parsed.op, envelope.op);
        prop_assert_eq!(parsed.s, envelope.s.value().copied());
        prop_assert_eq!(parsed.kind(), envelope.kind());
        prop_assert_eq!(parsed.d.map(RawValue::get), envelope.data());

        let bytes = payload::outgoing(&parsed, old.as_ref()).expect("serializable");
        let text = String::from_utf8(bytes).expect("UTF-8");
        let value: Value = serde_json::from_str(&text).expect("valid JSON");
        let object = value.as_object().expect("an object");

        // Exactly {"op", "t", "d"} plus "old" when there is an old value.
        let mut expected_keys = vec!["d", "op", "t"];
        if old.is_some() {
            expected_keys.push("old");
        }
        let keys: Vec<&str> = object.keys().map(String::as_str).collect();
        prop_assert_eq!(keys, sorted(expected_keys));
        prop_assert_eq!(&object["op"], &json!(envelope.op));
        prop_assert_eq!(&object["t"], &json!(envelope.kind()));
        if let Some(old) = &old {
            // Compared after a round trip of its own: serde_json's float
            // parsing is not exact for every f64 (no `float_roundtrip`).
            prop_assert_eq!(&object["old"], &reparse(old));
        }
        // `d` byte for byte, escapes and whitespace included.
        let published: Published<'_> = serde_json::from_str(&text).unwrap();
        prop_assert_eq!(published.d.get(), envelope.data().unwrap_or("null"));
    }

    #[test]
    fn prepare_classifies_every_payload(envelope in raw_envelope(), state in state_config()) {
        let json = envelope.json();
        let prepared = runner::prepare(&json, &state);
        match (envelope.op, envelope.kind()) {
            (7, _) => prop_assert!(matches!(prepared, Prepared::Reconnect), "{:?}", prepared),
            (9, _) => {
                let resumable = envelope.data() == Some("true");
                prop_assert!(
                    matches!(prepared, Prepared::InvalidSession { resumable: r } if r == resumable),
                    "{:?} for {}",
                    prepared,
                    json
                );
            }
            (10, _) => prop_assert!(matches!(prepared, Prepared::Hello), "{:?}", prepared),
            (0, Some(expected_kind)) => match prepared {
                Prepared::Dispatch { kind, seq, route, event, ready, parse_error } => {
                    prop_assert_eq!(kind.as_str(), expected_kind);
                    prop_assert_eq!(seq, envelope.s.value().copied());
                    let d = envelope.data().map(|d| RawValue::from_string(d.to_owned()).unwrap());
                    prop_assert_eq!(route, payload::route(Some(expected_kind), d.as_deref()));
                    if !runner::wants_typed(expected_kind, &state) {
                        prop_assert!(event.is_none() && parse_error.is_none());
                    } else if envelope.scannable() {
                        prop_assert!(
                            event.is_some() != parse_error.is_some(),
                            "typed parsing either works or reports why: {:?} {:?}",
                            event,
                            parse_error
                        );
                        if let Some(event) = &event {
                            prop_assert_eq!(event.kind().name(), Some(expected_kind));
                        }
                    }
                    // READY's session is read from `d` even when the model rejects READY.
                    let expected_ready = (expected_kind == "READY")
                        .then(|| {
                            envelope
                                .data()
                                .and_then(|d| serde_json::from_str::<runner::ReadySession>(d).ok())
                        })
                        .flatten();
                    prop_assert_eq!(ready, expected_ready);
                }
                other => prop_assert!(false, "expected a dispatch, got {:?}", other),
            },
            _ => prop_assert!(matches!(prepared, Prepared::Other), "{:?}", prepared),
        }
    }

    #[test]
    fn arbitrary_text_never_panics(text in prop_oneof![
        "(?s).{0,200}",
        r#"\{"(op|t|s|d)":.{0,40}\}"#,
        json_value().prop_map(|v| v.to_string()),
    ]) {
        if let Ok(envelope) = Envelope::parse(&text) {
            let bytes = payload::outgoing(&envelope, None).expect("serializable");
            prop_assert!(serde_json::from_slice::<Value>(&bytes).is_ok());
            let _ = payload::route(envelope.kind(), envelope.d);
        }
        for state in [common::state_all(), StateConfig::default()] {
            let _ = runner::prepare(&text, &state);
        }
        let _ = runner::parse_event(&text);
    }

    #[test]
    fn near_envelopes_never_panic(
        fields in prop::collection::vec(
            (prop::sample::select(vec!["op", "s", "t", "d"]), json_value()),
            0..5,
        ),
    ) {
        let object: Map<String, Value> =
            fields.into_iter().map(|(k, v)| (k.to_owned(), v)).collect();
        let text = Value::Object(object).to_string();
        let _ = runner::prepare(&text, &common::state_all());
        let _ = runner::parse_event(&text);
        if let Ok(envelope) = Envelope::parse(&text) {
            let old = json!({"x": 1});
            prop_assert!(payload::outgoing(&envelope, Some(&old)).is_ok());
        }
    }
}

/// A routing hint field as it may appear in a dispatch's `d`.
#[derive(Clone, Debug)]
enum Hint {
    Absent,
    Null,
    Snowflake(u64),
    Text(String),
    Other(Value),
}

fn hint() -> BoxedStrategy<Hint> {
    prop_oneof![
        2 => Just(Hint::Absent),
        1 => Just(Hint::Null),
        4 => snowflake().prop_map(Hint::Snowflake),
        1 => "[a-z+-]{0,4}[0-9]{0,3}".prop_map(Hint::Text),
        1 => json_value()
            .prop_filter("not a string or null", |v| !v.is_string() && !v.is_null())
            .prop_map(Hint::Other),
    ]
    .boxed()
}

impl Hint {
    fn insert(&self, object: &mut Map<String, Value>, key: &str) {
        let value = match self {
            Self::Absent => return,
            Self::Null => Value::Null,
            Self::Snowflake(id) => Value::String(id.to_string()),
            Self::Text(text) => Value::String(text.clone()),
            Self::Other(value) => value.clone(),
        };
        object.insert(key.to_owned(), value);
    }

    fn id(&self) -> Option<u64> {
        match self {
            Self::Snowflake(id) => Some(*id),
            Self::Text(text) => text.parse().ok(),
            Self::Absent | Self::Null | Self::Other(_) => None,
        }
    }
}

proptest! {
    #![proptest_config(cases(512))]

    #[test]
    fn dispatches_route_by_guild_then_channel(
        kind in event_name(),
        guild_id in hint(),
        id in hint(),
        channel_id in hint(),
        rest in json_object(),
        workers in 0usize..=2048,
    ) {
        let mut d: Map<String, Value> = rest
            .into_iter()
            .filter(|(key, _)| !["guild_id", "id", "channel_id"].contains(&key.as_str()))
            .collect();
        guild_id.insert(&mut d, "guild_id");
        id.insert(&mut d, "id");
        channel_id.insert(&mut d, "channel_id");
        let raw = RawValue::from_string(Value::Object(d).to_string()).unwrap();

        // GUILD_CREATE / UPDATE / DELETE carry the guild itself; everything
        // else names its guild in `guild_id`, or else its (DM) channel.
        let own_id = matches!(kind.as_str(), "GUILD_CREATE" | "GUILD_UPDATE" | "GUILD_DELETE");
        let guild = if own_id { id.id() } else { guild_id.id() };
        let lenient = match (guild, channel_id.id()) {
            (Some(guild), _) => Route::Guild(guild),
            (None, Some(channel)) => Route::Channel(channel),
            (None, None) => Route::Unrouted,
        };
        let route = payload::route(Some(&kind), Some(&raw));
        let malformed = [&guild_id, &id, &channel_id].iter().any(|h| matches!(h, Hint::Other(_)));
        if malformed {
            // A hint of the wrong type may only lose the ordering, never
            // route the event somewhere else.
            prop_assert!(route == Route::Unrouted || route == lenient, "{:?}", route);
        } else {
            prop_assert_eq!(route, lenient);
        }

        let worker = route.worker(workers);
        prop_assert!(worker < workers.max(1));
        prop_assert_eq!(worker, route.worker(workers), "stable");
        if route == Route::Unrouted {
            prop_assert_eq!(worker, 0);
        }
    }

    #[test]
    fn workers_are_in_range_and_stable(id in any::<u64>(), workers in 0usize..=4096) {
        for route in [Route::Guild(id), Route::Channel(id)] {
            let worker = route.worker(workers);
            prop_assert!(worker < workers.max(1));
            prop_assert_eq!(worker, route.worker(workers));
            prop_assert_eq!(route.worker(1), 0);
        }
        prop_assert_eq!(Route::Unrouted.worker(workers), 0);
    }

    #[test]
    fn route_never_panics(
        kind in prop::option::of(event_name()),
        d in prop::option::of(raw_json()),
    ) {
        let raw = d.map(|d| RawValue::from_string(d).unwrap());
        let _ = payload::route(kind.as_deref(), raw.as_deref());
    }
}

// ---------------------------------------------------------------------------
// gateway::runner: typed parsing of real payloads, mutated

/// One payload per event type the cache handles, in Discord's shapes.
fn fixtures() -> Vec<(&'static str, Value)> {
    let g = 1000u64;
    let mut member_add = common::member(20, &[g + 1]);
    member_add["guild_id"] = json!(g.to_string());
    let mut member_update = common::member(10, &[g + 1]);
    member_update["guild_id"] = json!(g.to_string());
    let mut guild_update = common::guild(g, "renamed");
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
    let mut channel = common::channel(g + 3, Some(g), "general");
    channel["guild_id"] = json!(g.to_string());
    let mut archived = common::thread(g + 5, g, g + 3);
    archived["thread_metadata"]["archived"] = json!(true);
    vec![
        ("READY", common::ready([0, 1], &[g, g + 100])),
        ("GUILD_CREATE", common::guild(g, "g")),
        (
            "GUILD_CREATE",
            json!({"id": g.to_string(), "unavailable": true}),
        ),
        ("GUILD_UPDATE", guild_update),
        (
            "GUILD_DELETE",
            json!({"id": g.to_string(), "unavailable": true}),
        ),
        ("GUILD_DELETE", json!({"id": g.to_string()})),
        ("CHANNEL_CREATE", channel.clone()),
        ("CHANNEL_UPDATE", channel.clone()),
        ("CHANNEL_DELETE", channel),
        (
            "CHANNEL_PINS_UPDATE",
            json!({"guild_id": g.to_string(), "channel_id": (g + 3).to_string(),
                "last_pin_timestamp": "2024-01-01T00:00:00.000000+00:00"}),
        ),
        ("THREAD_CREATE", common::thread(g + 5, g, g + 3)),
        ("THREAD_UPDATE", archived),
        (
            "THREAD_DELETE",
            json!({"id": (g + 5).to_string(), "guild_id": g.to_string(),
                "parent_id": (g + 3).to_string(), "type": 11}),
        ),
        (
            "THREAD_LIST_SYNC",
            json!({"guild_id": g.to_string(), "channel_ids": [(g + 3).to_string()],
                "threads": [common::thread(g + 5, g, g + 3)], "members": []}),
        ),
        (
            "GUILD_ROLE_CREATE",
            json!({"guild_id": g.to_string(), "role": common::role(g + 1, "mods")}),
        ),
        (
            "GUILD_ROLE_UPDATE",
            json!({"guild_id": g.to_string(), "role": common::role(g + 1, "admins")}),
        ),
        (
            "GUILD_ROLE_DELETE",
            json!({"guild_id": g.to_string(), "role_id": (g + 1).to_string()}),
        ),
        (
            "GUILD_EMOJIS_UPDATE",
            json!({"guild_id": g.to_string(), "emojis": [common::emoji(g + 2, "pog")]}),
        ),
        (
            "GUILD_STICKERS_UPDATE",
            json!({"guild_id": g.to_string(), "stickers": []}),
        ),
        ("GUILD_MEMBER_ADD", member_add),
        ("GUILD_MEMBER_UPDATE", member_update),
        (
            "GUILD_MEMBER_REMOVE",
            json!({"guild_id": g.to_string(), "user": common::user(10, "user10")}),
        ),
        (
            "GUILD_MEMBERS_CHUNK",
            json!({"guild_id": g.to_string(), "members": [common::member(10, &[])],
                "chunk_index": 0, "chunk_count": 1,
                "presences": [common::presence(g, 10, "online")]}),
        ),
        ("MESSAGE_CREATE", common::message(7, g + 3, Some(g), "hi")),
        (
            "MESSAGE_UPDATE",
            common::message(7, g + 3, Some(g), "edited"),
        ),
        (
            "MESSAGE_DELETE",
            json!({"id": "7", "channel_id": (g + 3).to_string(), "guild_id": g.to_string()}),
        ),
        (
            "MESSAGE_DELETE_BULK",
            json!({"ids": ["7", "8"], "channel_id": (g + 3).to_string(),
                "guild_id": g.to_string()}),
        ),
        ("PRESENCE_UPDATE", common::presence(g, 10, "online")),
        (
            "VOICE_STATE_UPDATE",
            common::voice_state(Some(g), 10, Some(g + 3)),
        ),
        (
            "USER_UPDATE",
            json!({"id": common::BOT_ID.to_string(), "username": "bot", "discriminator": "0",
                "global_name": null, "avatar": null, "bot": true, "mfa_enabled": false,
                "verified": true, "flags": 0}),
        ),
    ]
}

#[derive(Clone, Debug)]
enum Mutation {
    Replace(Vec<usize>, Value),
    Remove(Vec<usize>),
    Insert(Vec<usize>, String, Value),
}

fn mutation() -> BoxedStrategy<Mutation> {
    let path = prop::collection::vec(any::<usize>(), 0..5);
    prop_oneof![
        4 => (path.clone(), json_value()).prop_map(|(path, value)| Mutation::Replace(path, value)),
        2 => path.clone().prop_map(Mutation::Remove),
        1 => (path, field_name(), json_value())
            .prop_map(|(path, key, value)| Mutation::Insert(path, key, value)),
    ]
    .boxed()
}

/// Walk `path` (indexes into objects and arrays, modulo their length) and
/// mutate what is found there.
fn mutate(value: &mut Value, path: &[usize], mutation: &Mutation) {
    if let Some((&first, rest)) = path.split_first() {
        match value {
            Value::Object(object) if !object.is_empty() => {
                let key = object.keys().nth(first % object.len()).unwrap().clone();
                if rest.is_empty() && matches!(mutation, Mutation::Remove(_)) {
                    object.remove(&key);
                } else {
                    mutate(object.get_mut(&key).unwrap(), rest, mutation);
                }
                return;
            }
            Value::Array(items) if !items.is_empty() => {
                let index = first % items.len();
                if rest.is_empty() && matches!(mutation, Mutation::Remove(_)) {
                    items.remove(index);
                } else {
                    mutate(&mut items[index], rest, mutation);
                }
                return;
            }
            _ => {}
        }
    }
    match mutation {
        Mutation::Replace(_, replacement) => *value = replacement.clone(),
        Mutation::Insert(_, key, inserted) => {
            if let Value::Object(object) = value {
                object.insert(key.clone(), inserted.clone());
            }
        }
        Mutation::Remove(_) => {}
    }
}

fn mutation_path(mutation: &Mutation) -> &[usize] {
    match mutation {
        Mutation::Replace(path, _) | Mutation::Remove(path) | Mutation::Insert(path, _, _) => path,
    }
}

/// The typed event of a dispatch, if any, is of the dispatch's own type.
fn typed_kind_matches(prepared: &Prepared) -> Result<(), TestCaseError> {
    if let Prepared::Dispatch {
        kind,
        event: Some(event),
        ..
    } = prepared
    {
        prop_assert_eq!(event.kind().name(), Some(kind.as_str()), "{:?}", event);
    }
    Ok(())
}

/// Every fixture parses as its own event type, so mutating them reaches deep
/// into the models.
#[test]
fn fixtures_parse_as_their_own_event_type() {
    for (kind, d) in fixtures() {
        let raw = common::raw(kind, 1, d);
        match runner::prepare(&raw, &common::state_all()) {
            Prepared::Dispatch {
                event: Some(event),
                parse_error: None,
                ..
            } => assert_eq!(event.kind().name(), Some(kind), "{raw}"),
            other => panic!("{kind} did not parse: {other:?}"),
        }
    }
}

proptest! {
    #![proptest_config(cases(768))]

    #[test]
    fn mutated_payloads_never_panic(
        fixture in prop::sample::select(fixtures()),
        mutations in prop::collection::vec(mutation(), 1..4),
        state in state_config(),
        seq in any::<u64>(),
    ) {
        let (kind, mut d) = fixture;
        for mutation in &mutations {
            mutate(&mut d, mutation_path(mutation), mutation);
        }
        let raw = common::raw(kind, seq, d);
        for state in [&state, &common::state_all()] {
            let prepared = runner::prepare(&raw, state);
            typed_kind_matches(&prepared)?;
            match prepared {
                Prepared::Dispatch { kind: k, seq: s, .. } => {
                    prop_assert_eq!(k.as_str(), kind);
                    prop_assert_eq!(s, Some(seq));
                }
                other => prop_assert!(false, "a dispatch is always a dispatch: {:?}", other),
            }
        }
        let envelope = Envelope::parse(&raw).expect("valid JSON");
        prop_assert!(payload::outgoing(&envelope, None).is_ok());
    }

    #[test]
    fn envelope_key_order_does_not_matter(
        fixture in prop::sample::select(fixtures()),
        order in Just(vec![0usize, 1, 2, 3]).prop_shuffle(),
        pad in prop::sample::select(vec!["", " ", "\n"]),
    ) {
        let (kind, d) = fixture;
        let fields = [
            format!("\"t\":{pad}\"{kind}\""),
            "\"s\":42".to_owned(),
            "\"op\":0".to_owned(),
            format!("\"d\":{pad}{d}"),
        ];
        let raw = format!(
            "{{{}}}",
            order.iter().map(|i| fields[*i].as_str()).collect::<Vec<_>>().join(",")
        );
        match runner::prepare(&raw, &common::state_all()) {
            Prepared::Dispatch { kind: k, seq, event, parse_error, .. } => {
                prop_assert_eq!(k.as_str(), kind);
                prop_assert_eq!(seq, Some(42));
                prop_assert!(parse_error.is_none(), "{:?} for {}", parse_error, raw);
                let event = event.expect("typed event");
                prop_assert_eq!(event.kind().name(), Some(kind), "{}", raw);
            }
            other => prop_assert!(false, "{:?}", other),
        }
    }
}

/// twilight's `GatewayEventDeserializer::from_json` finds the event type by
/// searching the text for the first `"t":`, while `prepare` takes the kind
/// (used for routing, metrics and the published routing key) from the parsed
/// envelope. When `d` comes before `t` and contains a `"t"` key, the two
/// disagree and the cache applies a different event than the one published.
/// Discord puts `t` first, so this needs a gateway (proxy) that reorders keys.
#[test]
#[ignore = "BUG (latent): prepare() trusts twilight's text scan for the event type; \
            a payload with `d` before `t` and a nested \"t\" key is cached as another event"]
fn typed_event_type_comes_from_the_envelope() {
    let raw = concat!(
        r#"{"op":0,"s":5,"d":{"t":"GUILD_DELETE","id":"1000","channel_id":"7"},"#,
        r#""t":"MESSAGE_CREATE"}"#,
    );
    let prepared = runner::prepare(raw, &common::state_all());
    if let Prepared::Dispatch {
        kind, event, route, ..
    } = &prepared
    {
        assert_eq!(kind, "MESSAGE_CREATE");
        assert_eq!(*route, Route::Channel(7));
        if let Some(event) = event {
            assert_eq!(
                event.kind().name(),
                Some("MESSAGE_CREATE"),
                "published as MESSAGE_CREATE but cached as {:?}",
                event.kind()
            );
        }
    } else {
        panic!("{prepared:?}");
    }
}

// ---------------------------------------------------------------------------
// gateway::seq: the resume point gives at-least-once delivery across restarts

/// `FAILED_GRANULARITY` in seq.rs (private): failures within this interval
/// share one entry, which may expire up to this much earlier.
const GRANULARITY: Duration = Duration::from_secs(1);

#[derive(Clone, Debug)]
enum SeqOp {
    /// Dispatch a sequence. With increasing sequences (Discord's, with gaps
    /// for READY and untracked payloads) this is the gap to the next one;
    /// otherwise the sequence is `1 + argument`, so it repeats and goes back.
    Dispatch(u64),
    Complete(usize),
    Fail(usize),
    NewSession(u64),
    Advance(Duration),
}

fn seq_ops(max_gap: u64) -> impl Strategy<Value = Vec<SeqOp>> {
    let retention = FAILED_RETENTION.as_millis() as u64;
    let advance = prop_oneof![
        3 => 0u64..1500,
        1 => (retention - GRANULARITY.as_millis() as u64 - 2)..(retention + 2),
        1 => 0u64..(2 * retention),
    ]
    .prop_map(Duration::from_millis);
    let op = prop_oneof![
        6 => (0..=max_gap).prop_map(SeqOp::Dispatch),
        3 => any::<usize>().prop_map(SeqOp::Complete),
        2 => any::<usize>().prop_map(SeqOp::Fail),
        1 => (0u64..1000).prop_map(SeqOp::NewSession),
        2 => advance.prop_map(SeqOp::Advance),
    ];
    prop::collection::vec(op, 1..100)
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Progress {
    Pending,
    Done,
    /// Could not be published, at this offset from the start.
    Failed(Duration),
}

#[derive(Debug)]
struct Issued {
    token: SeqToken,
    epoch: u64,
    seq: u64,
    progress: Progress,
}

/// Drive a tracker and a reference model side by side and check, after every
/// step, that the resume point skips nothing unpublished (safety), is held
/// back by nothing else (progress), and, with `increasing` sequences, never
/// moves backwards within a session.
fn check_seq_tracker(ops: &[SeqOp], increasing: bool) -> Result<(), TestCaseError> {
    let start = Instant::now();
    let mut tracker = SeqTracker::new();
    let mut now = Duration::ZERO;
    let mut epoch = 0u64;
    let mut highest = 0u64;
    let mut next = 1u64;
    let mut issued: Vec<Issued> = Vec::new();
    let mut previous_resume: Option<u64> = None;

    for op in ops {
        match *op {
            SeqOp::Dispatch(arg) => {
                let seq = if increasing {
                    let seq = next;
                    next += arg + 1;
                    seq
                } else {
                    1 + arg
                };
                let token = tracker.dispatch(seq);
                highest = highest.max(seq);
                issued.push(Issued {
                    token,
                    epoch,
                    seq,
                    progress: Progress::Pending,
                });
            }
            SeqOp::Complete(pick) | SeqOp::Fail(pick) => {
                let pending: Vec<usize> = (0..issued.len())
                    .filter(|i| issued[*i].progress == Progress::Pending)
                    .collect();
                if pending.is_empty() {
                    continue;
                }
                let record = &mut issued[pending[pick % pending.len()]];
                if matches!(op, SeqOp::Complete(_)) {
                    tracker.complete(record.token);
                    record.progress = Progress::Done;
                } else {
                    tracker.fail(record.token, start + now);
                    record.progress = Progress::Failed(now);
                }
            }
            SeqOp::NewSession(ready_seq) => {
                tracker.new_session(ready_seq);
                epoch += 1;
                highest = ready_seq;
                next = ready_seq + 1;
                previous_resume = None;
            }
            SeqOp::Advance(by) => now += by,
        }

        let resume = tracker.resume_seq_at(start + now);
        let current: Vec<&Issued> = issued.iter().filter(|r| r.epoch == epoch).collect();
        let unfinished: Vec<u64> = current
            .iter()
            .filter(|r| r.progress == Progress::Pending)
            .map(|r| r.seq)
            .collect();
        let failed_within = |limit: Duration| -> Vec<u64> {
            current
                .iter()
                .filter_map(|r| match r.progress {
                    Progress::Failed(at) if now - at < limit => Some(r.seq),
                    _ => None,
                })
                .collect()
        };
        // Failures are certainly still held back this long...
        let surely_held = failed_within(FAILED_RETENTION - GRANULARITY);
        // ...and certainly released after this long.
        let maybe_held = failed_within(FAILED_RETENTION);

        // Safety: a restart replays every payload not yet published, and every
        // recent payload that could not be published.
        for seq in unfinished.iter().chain(&surely_held) {
            prop_assert!(
                resume < *seq,
                "resume {} would skip {} after {:?}",
                resume,
                seq,
                op
            );
        }
        // Progress: nothing else holds the resume point back.
        match unfinished.iter().chain(&maybe_held).min() {
            None => prop_assert_eq!(resume, highest, "idle after {:?}", op),
            Some(lowest) => prop_assert!(
                resume >= lowest - 1,
                "resume {} is behind {} after {:?}",
                resume,
                lowest,
                op
            ),
        }
        prop_assert_eq!(tracker.in_flight(), unfinished.len());
        for record in &issued {
            prop_assert_eq!(tracker.is_current(record.token), record.epoch == epoch);
        }
        if increasing {
            if let Some(previous) = previous_resume {
                prop_assert!(
                    resume >= previous,
                    "resume went back from {} to {}",
                    previous,
                    resume
                );
            }
            previous_resume = Some(resume);
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(cases(1024))]

    #[test]
    fn resume_point_with_increasing_sequences(ops in seq_ops(2)) {
        check_seq_tracker(&ops, true)?;
    }

    #[test]
    fn resume_point_with_repeated_sequences(ops in seq_ops(12)) {
        check_seq_tracker(&ops, false)?;
    }
}

// ---------------------------------------------------------------------------
// config

/// Every variable `Config::from_lookup` reads. Removing one breaks existing
/// deployments silently, so the list only ever grows.
const VARIABLES: &[&str] = &[
    "BOT_TOKEN",
    "SHARDS_START",
    "SHARDS_END",
    "SHARDS_TOTAL",
    "SHARDS_CONCURRENCY",
    "DEFAULT_QUEUE",
    "RESUME",
    "INTENTS",
    "LARGE_THRESHOLD",
    "STATUS",
    "ACTIVITY_TYPE",
    "ACTIVITY_NAME",
    "ACTIVITY_STATE",
    "ACTIVITY_URL",
    "LOG_CHANNEL",
    "LOG_GUILD_CHANNEL",
    "STATE_ENABLED",
    "STATE_MEMBER",
    "STATE_MEMBER_TTL",
    "STATE_MESSAGE",
    "STATE_MESSAGE_TTL",
    "STATE_PRESENCE",
    "STATE_EMOJI",
    "STATE_VOICE",
    "STATE_OLD",
    "AMQP_URL",
    "RABBIT_HOST",
    "RABBIT_PORT",
    "RABBIT_USERNAME",
    "RABBIT_PASSWORD",
    "RABBIT_VHOST",
    "REDIS_URL",
    "REDIS_HOST",
    "REDIS_PORT",
    "REDIS_DB",
    "REDIS_USERNAME",
    "REDIS_PASSWORD",
    "PROMETHEUS_HOST",
    "PROMETHEUS_PORT",
    "WORKERS",
    "CACHE_TIMEOUT_MS",
    "SHARD_SILENCE_SECS",
    "DISCORD_API_PROXY",
    "DISCORD_GATEWAY_URL",
];

/// Variables whose absence is reported as `Missing`.
const REQUIRED: &[&str] = &[
    "BOT_TOKEN",
    "INTENTS",
    "SHARDS_TOTAL",
    "RABBIT_HOST",
    "REDIS_HOST",
];

/// The variables of `.env.example`, which every deployment started from.
fn example_env() -> HashMap<String, String> {
    [
        ("RUST_LOG", "info"),
        ("BOT_TOKEN", "token"),
        ("SHARDS_START", "0"),
        ("SHARDS_END", "1"),
        ("SHARDS_TOTAL", "2"),
        ("SHARDS_CONCURRENCY", "1"),
        ("SHARDS_WAIT", "6"),
        ("CLUSTERS", "2"),
        ("DEFAULT_QUEUE", "true"),
        ("RESUME", "true"),
        ("INTENTS", "32767"),
        ("LARGE_THRESHOLD", "250"),
        ("STATUS", "online"),
        ("ACTIVITY_TYPE", "0"),
        ("ACTIVITY_NAME", "Testing"),
        ("LOG_CHANNEL", ""),
        ("LOG_GUILD_CHANNEL", ""),
        ("STATE_ENABLED", "true"),
        ("STATE_MEMBER", "true"),
        ("STATE_MEMBER_TTL", "60000"),
        ("STATE_MESSAGE", "true"),
        ("STATE_MESSAGE_TTL", "60000"),
        ("STATE_PRESENCE", "true"),
        ("STATE_EMOJI", "true"),
        ("STATE_VOICE", "true"),
        ("STATE_OLD", "false"),
        ("RABBIT_HOST", "127.0.0.1"),
        ("RABBIT_PORT", "5672"),
        ("RABBIT_USERNAME", "guest"),
        ("RABBIT_PASSWORD", "guest"),
        ("REDIS_HOST", "127.0.0.1"),
        ("REDIS_PORT", "6379"),
        ("PROMETHEUS_HOST", "127.0.0.1"),
        ("PROMETHEUS_PORT", "8005"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect()
}

fn load(env: &HashMap<String, String>) -> Result<Config, ConfigError> {
    Config::from_lookup(&|name: &str| env.get(name).cloned())
}

/// What any configuration that loads must satisfy.
fn check_loaded(config: &Config) -> Result<(), TestCaseError> {
    if let Some(range) = config.shards {
        prop_assert!(range.total > 0 && range.start <= range.end && range.end < range.total);
        prop_assert_eq!(range.len() as usize, range.ids().count());
    }
    prop_assert!((50..=250).contains(&config.large_threshold));
    prop_assert!((1..=1024).contains(&config.workers));
    prop_assert!(config.cache_timeout >= Duration::from_millis(100));
    prop_assert!(config.shard_silence >= Duration::from_secs(120));
    prop_assert!(config.prometheus_host.parse::<std::net::IpAddr>().is_ok());
    prop_assert_eq!(
        Intents::from_bits(config.intents.bits()),
        Some(config.intents)
    );
    let state = &config.state;
    if !state.enabled {
        prop_assert!(
            !state.member
                && !state.message
                && !state.presence
                && !state.emoji
                && !state.voice
                && !state.old
        );
    }
    if let Some(url) = &config.discord_gateway_url {
        prop_assert!(url.starts_with("ws:") || url.starts_with("wss:"), "{}", url);
        prop_assert!(!url.ends_with('/'), "{}", url);
    }
    if let Some(proxy) = &config.discord_api_proxy {
        prop_assert!(!proxy.address.is_empty() && !proxy.address.contains('/'));
    }
    prop_assert!(!config.bot_token.trim().is_empty());
    Ok(())
}

fn config_value() -> BoxedStrategy<Option<String>> {
    let value = prop_oneof![
        2 => "(?s).{0,24}",
        2 => any::<i128>().prop_map(|n| n.to_string()),
        2 => (0u64..2000).prop_map(|n| n.to_string()),
        1 => any::<u64>().prop_map(|n| n.to_string()),
        1 => prop::sample::select(vec![
            "true", "false", " true ", "TRUE", "1", "yes", "null", "[]", "{}", "\"x\"",
            "1e3", "1.5", "-0", "-1", " ", "", "online", "dnd", "\"online\"", "localhost",
            "::1", "0.0.0.0", "300.1.1.1", "ws://", "ws://gateway/", "wss://g:1",
            "http://proxy:1/", "https://", "http://a/b", "ftp://x", "%", "%41", "\u{0}",
        ])
        .prop_map(str::to_owned),
    ];
    prop_oneof![1 => Just(None), 6 => value.prop_map(Some)].boxed()
}

proptest! {
    #![proptest_config(cases(512))]

    #[test]
    fn any_environment_loads_or_is_rejected_without_panicking(
        changes in prop::collection::vec((prop::sample::select(VARIABLES), config_value()), 0..10),
        drop_hosts in any::<bool>(),
    ) {
        let mut env = example_env();
        if drop_hosts {
            env.remove("RABBIT_HOST");
            env.remove("REDIS_HOST");
        }
        for (name, value) in &changes {
            match value {
                Some(value) => env.insert((*name).to_owned(), value.clone()),
                None => env.remove(*name),
            };
        }
        match load(&env) {
            Ok(config) => check_loaded(&config)?,
            Err(ConfigError::Missing(name)) => {
                prop_assert!(REQUIRED.contains(&name), "{}", name);
            }
            Err(ConfigError::Invalid { name, value, reason }) => {
                prop_assert!(VARIABLES.contains(&name), "{}", name);
                prop_assert!(!reason.is_empty());
                let _ = value;
            }
        }
    }

    #[test]
    fn shard_ranges_are_validated(
        start in prop::option::of(0u32..40),
        end in prop::option::of(0u32..40),
        total in prop::option::of(0u32..40),
        probe in 0u32..45,
    ) {
        let mut env = example_env();
        let values = [("SHARDS_START", start), ("SHARDS_END", end), ("SHARDS_TOTAL", total)];
        for (name, value) in values {
            match value {
                Some(value) => env.insert(name.to_owned(), value.to_string()),
                None => env.remove(name),
            };
        }
        let result = load(&env);
        match (start, end, total) {
            (None, None, None) => prop_assert_eq!(result.unwrap().shards, None),
            (_, _, None) => {
                prop_assert_eq!(result.unwrap_err(), ConfigError::Missing("SHARDS_TOTAL"));
            }
            (start, end, Some(total)) => {
                let start = start.unwrap_or(0);
                let end = end.unwrap_or(total.saturating_sub(1));
                if total > 0 && start <= end && end < total {
                    let range = result.unwrap().shards.unwrap();
                    prop_assert_eq!(range, ShardRange { start, end, total });
                    prop_assert_eq!(range.contains(probe), (start..=end).contains(&probe));
                    prop_assert_eq!(range.len(), end - start + 1);
                } else {
                    prop_assert!(
                        matches!(result, Err(ConfigError::Invalid { name: "SHARDS_END", .. })),
                        "{:?}",
                        result
                    );
                }
            }
        }
    }

    #[test]
    fn numeric_settings_are_range_checked(
        workers in 0usize..1500,
        threshold in 0u64..400,
        cache_ms in prop_oneof![0u64..300, any::<u64>()],
        silence in prop_oneof![0u64..300, any::<u64>()],
        port in 0u32..70_000,
        intents in prop_oneof![
            any::<u64>().prop_map(|bits| bits & Intents::all().bits()),
            any::<u64>(),
        ],
    ) {
        type Checked = Result<Option<Config>, TestCaseError>;
        let check = |name: &'static str, value: String, valid: bool| -> Checked {
            let mut env = example_env();
            env.insert(name.to_owned(), value.clone());
            let result = load(&env);
            if valid {
                prop_assert!(result.is_ok(), "{}={}: {:?}", name, value, result.err());
                Ok(result.ok())
            } else {
                prop_assert!(
                    matches!(&result, Err(ConfigError::Invalid { name: n, .. }) if *n == name),
                    "{}={}: {:?}",
                    name,
                    value,
                    result.map(|_| ())
                );
                Ok(None)
            }
        };
        let valid = (1..=1024).contains(&workers);
        if let Some(config) = check("WORKERS", workers.to_string(), valid)? {
            prop_assert_eq!(config.workers, workers);
        }
        let valid = (50..=250).contains(&threshold);
        if let Some(config) = check("LARGE_THRESHOLD", threshold.to_string(), valid)? {
            prop_assert_eq!(config.large_threshold, threshold);
        }
        if let Some(config) = check("CACHE_TIMEOUT_MS", cache_ms.to_string(), cache_ms >= 100)? {
            prop_assert_eq!(config.cache_timeout, Duration::from_millis(cache_ms));
        }
        if let Some(config) = check("SHARD_SILENCE_SECS", silence.to_string(), silence >= 120)? {
            prop_assert_eq!(config.shard_silence, Duration::from_secs(silence));
        }
        if let Some(config) = check("PROMETHEUS_PORT", port.to_string(), port <= 65_535)? {
            prop_assert_eq!(u32::from(config.prometheus_port), port);
        }
        let known = intents & !Intents::all().bits() == 0;
        if let Some(config) = check("INTENTS", intents.to_string(), known)? {
            prop_assert_eq!(config.intents.bits(), intents);
        }
    }

    #[test]
    fn state_flags_need_state_enabled(flags in any::<[Option<bool>; 7]>()) {
        let names = [
            "STATE_ENABLED",
            "STATE_MEMBER",
            "STATE_MESSAGE",
            "STATE_PRESENCE",
            "STATE_EMOJI",
            "STATE_VOICE",
            "STATE_OLD",
        ];
        let mut env = example_env();
        for (name, flag) in names.iter().zip(flags) {
            match flag {
                Some(flag) => env.insert((*name).to_owned(), flag.to_string()),
                None => env.remove(*name),
            };
        }
        let state = load(&env).unwrap().state;
        let enabled = flags[0].unwrap_or(true);
        let on = |i: usize| enabled && flags[i].unwrap_or(false);
        prop_assert_eq!(state.enabled, enabled);
        prop_assert_eq!(
            [state.member, state.message, state.presence, state.emoji, state.voice, state.old],
            [on(1), on(2), on(3), on(4), on(5), on(6)]
        );
    }
}

/// Undo percent-encoding.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            out.push(u8::from_str_radix(&text[i + 1..i + 3], 16).expect("hex escape"));
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).expect("UTF-8")
}

/// Percent-encode every byte that is not unreserved, with `upper` or lower
/// case hex, as an operator might have done by hand.
fn percent_encode(text: &str, upper: bool) -> String {
    text.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else if upper {
                format!("%{b:02X}")
            } else {
                format!("%{b:02x}")
            }
        })
        .collect()
}

fn url_safe(part: &str) -> bool {
    part.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"-._~%".contains(&b))
}

/// A credential without `%` (pre-encoded values are tested separately);
/// whitespace-only values mean "unset".
fn credential() -> BoxedStrategy<String> {
    "[^%]{1,16}"
        .prop_filter("not blank", |value| !value.trim().is_empty())
        .boxed()
}

proptest! {
    #![proptest_config(cases(512))]

    #[test]
    fn broker_credentials_survive_url_encoding(
        user in credential(),
        password in credential(),
        vhost in credential(),
    ) {
        let mut env = example_env();
        env.insert("RABBIT_USERNAME".into(), user.clone());
        env.insert("RABBIT_PASSWORD".into(), password.clone());
        env.insert("RABBIT_VHOST".into(), vhost.clone());
        let url = load(&env).unwrap().amqp_url;

        let rest = url.strip_prefix("amqp://").unwrap();
        let (userinfo, rest) = rest.split_once('@').unwrap();
        let (u, p) = userinfo.split_once(':').unwrap();
        let (address, path) = rest.split_once('/').unwrap();
        let (v, query) = path.split_once('?').unwrap();
        prop_assert_eq!(address, "127.0.0.1:5672");
        prop_assert_eq!(query, "heartbeat=30");
        for part in [u, p, v] {
            prop_assert!(url_safe(part), "{}", part);
        }
        prop_assert_eq!(percent_decode(u), user);
        prop_assert_eq!(percent_decode(p), password);
        prop_assert_eq!(percent_decode(v), vhost);
    }

    #[test]
    fn redis_credentials_survive_url_encoding(
        user in prop::option::of(credential()),
        password in prop::option::of(credential()),
        db in prop::option::of(0u32..16),
    ) {
        let mut env = example_env();
        if let Some(user) = &user {
            env.insert("REDIS_USERNAME".into(), user.clone());
        }
        if let Some(password) = &password {
            env.insert("REDIS_PASSWORD".into(), password.clone());
        }
        if let Some(db) = db {
            env.insert("REDIS_DB".into(), db.to_string());
        }
        let url = load(&env).unwrap().redis_url;
        let rest = url.strip_prefix("redis://").unwrap();
        let address = match rest.split_once('@') {
            Some((userinfo, address)) => {
                prop_assert!(user.is_some() || password.is_some(), "{}", url);
                let (u, p) = userinfo.split_once(':').unwrap();
                prop_assert!(url_safe(u) && url_safe(p), "{}", url);
                prop_assert_eq!(percent_decode(u), user.unwrap_or_default());
                prop_assert_eq!(percent_decode(p), password.unwrap_or_default());
                address
            }
            None => {
                prop_assert!(user.is_none() && password.is_none(), "{}", url);
                rest
            }
        };
        prop_assert_eq!(address.to_owned(), format!("127.0.0.1:6379/{}", db.unwrap_or(0)));
    }

    #[test]
    fn pre_encoded_credentials_are_kept(raw in "(?s).{1,12}", upper in any::<bool>()) {
        // Operators of earlier versions put encoded credentials in the URL
        // themselves; those must not be encoded a second time.
        let encoded = percent_encode(&raw, upper);
        let mut env = example_env();
        env.insert("RABBIT_PASSWORD".into(), encoded.clone());
        env.insert("REDIS_PASSWORD".into(), encoded.clone());
        let config = load(&env).unwrap();
        let expected_amqp = format!("amqp://guest:{encoded}@127.0.0.1:5672/%2F?heartbeat=30");
        prop_assert_eq!(config.amqp_url, expected_amqp);
        prop_assert_eq!(config.redis_url, format!("redis://:{encoded}@127.0.0.1:6379/0"));
    }

    #[test]
    fn numbers_parse_with_surrounding_whitespace(
        n in any::<u64>(),
        i in any::<i64>(),
        left in prop::sample::select(vec!["", " ", "\t", "\n", "  "]),
        right in prop::sample::select(vec!["", " ", "\t", "\r\n"]),
    ) {
        prop_assert_eq!(config::parse_value::<u64>(&format!("{left}{n}{right}")), Some(n));
        prop_assert_eq!(config::parse_value::<i64>(&format!("{left}{i}{right}")), Some(i));
        prop_assert_eq!(config::parse_value::<u16>(&n.to_string()), u16::try_from(n).ok());
        prop_assert_eq!(config::parse_value::<u32>(&n.to_string()), u32::try_from(n).ok());
        prop_assert_eq!(config::parse_value::<u64>(&i.to_string()), u64::try_from(i).ok());
    }

    #[test]
    fn only_true_and_false_are_booleans(text in prop_oneof![
        prop::sample::select(vec![
            "true", "false", " true", "false\n", "True", "FALSE", "1", "0", "yes", "no",
            "on", "off", "\"true\"", "t", "",
        ])
        .prop_map(str::to_owned),
        "(?s).{0,8}",
    ]) {
        let expected = match text.trim() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        };
        prop_assert_eq!(config::parse_value::<bool>(&text), expected);
    }

    #[test]
    fn strings_parse_as_json_strings_or_verbatim(text in "(?s).{0,40}") {
        let trimmed = text.trim();
        if !trimmed.starts_with('"') {
            prop_assert_eq!(config::parse_value::<String>(&text), Some(trimmed.to_owned()));
        }
        let quoted = serde_json::to_string(&text).unwrap();
        prop_assert_eq!(config::parse_value::<String>(&quoted), Some(text.clone()));
    }

    #[test]
    fn statuses_and_activity_types_parse(
        status in prop::sample::select(vec![
            Status::Online,
            Status::DoNotDisturb,
            Status::Idle,
            Status::Invisible,
            Status::Offline,
        ]),
        activity in any::<u8>(),
    ) {
        let name = serde_json::to_value(status).unwrap();
        let name = name.as_str().unwrap();
        prop_assert_eq!(config::parse_value::<Status>(name), Some(status));
        prop_assert_eq!(config::parse_value::<Status>(&format!("\"{name}\"")), Some(status));
        prop_assert_eq!(
            config::parse_value::<ActivityType>(&activity.to_string()),
            Some(ActivityType::from(activity))
        );
    }
}

/// Every variable that was read before is still read (with no URL given, all
/// of them are looked up).
#[test]
fn every_documented_variable_is_still_read() {
    let env = example_env();
    let seen = RefCell::new(BTreeSet::new());
    let config = Config::from_lookup(&|name: &str| {
        seen.borrow_mut().insert(name.to_owned());
        env.get(name).cloned()
    });
    assert!(config.is_ok(), "{config:?}");
    let seen = seen.into_inner();
    let missing: Vec<&str> = VARIABLES
        .iter()
        .copied()
        .filter(|name| !seen.contains(*name))
        .collect();
    assert!(missing.is_empty(), "no longer read: {missing:?}");
}

/// `DISCORD_GATEWAY_URL=ws://` (no host) is accepted and trimmed to `ws:`,
/// which only fails later, when the shards try to connect.
#[test]
#[ignore = "BUG (minor): DISCORD_GATEWAY_URL without a host (ws://, wss:///) is accepted \
            and becomes \"ws:\" instead of a config error"]
fn gateway_url_needs_a_host() {
    for value in ["ws://", "wss://", "ws:///"] {
        let mut env = example_env();
        env.insert("DISCORD_GATEWAY_URL".into(), value.into());
        let result = load(&env).map(|config| config.discord_gateway_url);
        assert!(
            matches!(
                result,
                Err(ConfigError::Invalid {
                    name: "DISCORD_GATEWAY_URL",
                    ..
                })
            ),
            "{value} -> {result:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// commands: anything sent to a shard must be something Discord accepts

fn gateway_opcode() -> BoxedStrategy<Value> {
    prop_oneof![
        4 => prop::sample::select(ALLOWED_OPCODES.to_vec()).prop_map(Value::from),
        3 => (0u64..64).prop_map(Value::from),
        1 => prop::sample::select(vec![
            json!(3.0),
            json!("3"),
            json!(-3),
            Value::Null,
            json!(u64::MAX),
        ]),
    ]
    .boxed()
}

/// The `data` of a send delivery, sized around Discord's payload limit.
fn command_data() -> BoxedStrategy<Option<Value>> {
    let padded = (
        gateway_opcode(),
        0usize..4200,
        prop::sample::select(vec!["x", "é", "\u{1F600}", "\""]),
        json_value(),
        any::<bool>(),
    )
        .prop_map(|(op, len, unit, extra, has_op)| {
            let fill = unit.repeat(len / unit.len().max(1));
            let mut data = json!({"d": {"guild_id": "1", "query": fill, "extra": extra}});
            if has_op {
                data["op"] = op;
            }
            data
        });
    prop_oneof![
        1 => Just(None),
        1 => Just(Some(Value::Null)),
        6 => padded.prop_map(Some),
        2 => json_value().prop_map(Some),
    ]
    .boxed()
}

fn delivery_json() -> BoxedStrategy<Value> {
    let op = prop_oneof![
        4 => Just(json!(0)),
        2 => Just(json!(1)),
        1 => prop::sample::select(vec![json!(2), json!(-1), json!("0"), Value::Null, json!(0.5)]),
    ];
    let shard = prop_oneof![
        6 => (0u32..80).prop_map(Value::from),
        1 => prop::sample::select(vec![
            json!(u32::MAX),
            json!(u64::from(u32::MAX) + 1),
            json!(-1),
            json!("1"),
            Value::Null,
        ]),
    ];
    (op, prop::option::of(shard), command_data())
        .prop_map(|(op, shard, data)| {
            let mut body = json!({"op": op});
            if let Some(shard) = shard {
                body["shard"] = shard;
            }
            if let Some(data) = data {
                body["data"] = data;
            }
            body
        })
        .boxed()
}

proptest! {
    #![proptest_config(cases(512))]

    #[test]
    fn only_allowed_commands_within_the_size_limit_are_sent(body in delivery_json()) {
        let Ok(delivery) = commands::parse(body.to_string().as_bytes()) else {
            return Ok(());
        };
        let result = commands::validate(&delivery);
        match delivery.op {
            DeliveryOpcode::Reconnect => prop_assert_eq!(result, Ok(ShardCommand::Reconnect)),
            DeliveryOpcode::Send => {
                let data = delivery.data.as_ref();
                let object = data.and_then(Value::as_object);
                let op = object.and_then(|o| o.get("op")).and_then(Value::as_u64);
                let allowed = op.is_some_and(|op| ALLOWED_OPCODES.contains(&op));
                let compact = data.map(|d| serde_json::to_string(d).unwrap());
                match result {
                    Ok(ShardCommand::Send(json)) => {
                        prop_assert!(allowed);
                        prop_assert!(json.len() <= MAX_PAYLOAD_BYTES, "{} bytes", json.len());
                        // Sent as compact JSON, exactly what was received.
                        prop_assert_eq!(Some(&json), compact.as_ref());
                    }
                    Ok(ShardCommand::Reconnect) => prop_assert!(false, "a send became a reconnect"),
                    Err(InvalidCommand::MissingData) => prop_assert!(data.is_none()),
                    Err(InvalidCommand::NotAnObject) => {
                        prop_assert!(data.is_some() && object.is_none());
                    }
                    Err(InvalidCommand::OpcodeNotAllowed(rejected)) => {
                        prop_assert!(object.is_some() && !allowed);
                        prop_assert_eq!(rejected, op);
                    }
                    Err(InvalidCommand::TooLarge(len)) => {
                        prop_assert!(allowed);
                        prop_assert!(len > MAX_PAYLOAD_BYTES);
                        prop_assert_eq!(Some(len), compact.as_ref().map(String::len));
                    }
                    Err(InvalidCommand::Malformed(err)) => prop_assert!(false, "{}", err),
                }
                // Completeness: an allowed command within the limit is sent.
                if allowed && compact.as_ref().is_some_and(|c| c.len() <= MAX_PAYLOAD_BYTES) {
                    let command = commands::validate(&delivery);
                    prop_assert!(matches!(command, Ok(ShardCommand::Send(_))), "{:?}", command);
                }
            }
        }
    }

    #[test]
    fn deliveries_are_run_forwarded_or_dropped(
        body in prop_oneof![
            8 => delivery_json().prop_map(|v| v.to_string().into_bytes()),
            1 => any::<Vec<u8>>(),
            1 => "(?s).{0,40}".prop_map(String::into_bytes),
        ],
        owned in prop::collection::btree_set(0u32..80, 0..20),
        total in 1u32..64,
    ) {
        let plan = commands::plan(&body, |shard| owned.contains(&shard), total);
        let expected = match commands::parse(&body) {
            Err(_) => Plan::Drop,
            Ok(delivery) if delivery.shard >= total => Plan::Drop,
            // Only the process that runs the shard validates the command.
            Ok(delivery) if !owned.contains(&delivery.shard) => Plan::Forward(delivery.shard),
            Ok(delivery) => match commands::validate(&delivery) {
                Ok(command) => Plan::Run(delivery.shard, command),
                Err(_) => Plan::Drop,
            },
        };
        prop_assert_eq!(plan, expected);
    }

    #[test]
    fn delivery_parsing_round_trips(
        reconnect in any::<bool>(),
        shard in any::<u32>(),
        data in prop::option::of(json_value()),
    ) {
        let delivery = DeliveryInfo {
            op: if reconnect { DeliveryOpcode::Reconnect } else { DeliveryOpcode::Send },
            shard,
            data: data.filter(|d| !d.is_null()),
        };
        let body = serde_json::to_vec(&delivery).unwrap();
        let parsed = commands::parse(&body).unwrap();
        prop_assert_eq!(parsed.op, delivery.op);
        prop_assert_eq!(parsed.shard, delivery.shard);
        prop_assert_eq!(parsed.data, delivery.data.as_ref().map(reparse));
        // The wire format: {"op": 0 | 1, "shard": N, "data": ...}.
        let value: Value = serde_json::from_slice(&body).unwrap();
        prop_assert_eq!(&value["op"], &json!(if reconnect { 1 } else { 0 }));
        prop_assert_eq!(&value["shard"], &json!(shard));
    }
}

// ---------------------------------------------------------------------------
// util::Throttle

proptest! {
    #![proptest_config(cases(64))]

    #[test]
    fn throttle_logs_once_per_interval(checks in 1usize..200) {
        let quiet = Throttle::new(Duration::from_secs(3600));
        let results: Vec<Option<u64>> = (0..checks).map(|_| quiet.check()).collect();
        prop_assert_eq!(results[0], Some(0));
        prop_assert!(results[1..].iter().all(Option::is_none));

        let open = Throttle::new(Duration::ZERO);
        prop_assert!((0..checks).all(|_| open.check() == Some(0)));
    }

    #[test]
    fn throttle_is_shared_safely_between_threads(threads in 1usize..8, per_thread in 1usize..200) {
        let throttle = Arc::new(Throttle::new(Duration::from_secs(3600)));
        let logged = Arc::new(AtomicUsize::new(0));
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let (throttle, logged) = (throttle.clone(), logged.clone());
                std::thread::spawn(move || {
                    for _ in 0..per_thread {
                        if throttle.check().is_some() {
                            logged.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        prop_assert_eq!(logged.load(Ordering::SeqCst), 1);
    }
}

/// The count of suppressed messages is reported with the next logged one.
#[test]
fn throttle_reports_what_it_suppressed() {
    let interval = Duration::from_millis(300);
    let throttle = Throttle::new(interval);
    let before_first = Instant::now();
    assert_eq!(throttle.check(), Some(0));
    let mut suppressed = 0;
    for _ in 0..50 {
        match throttle.check() {
            None => suppressed += 1,
            Some(_) => {
                // Only possible once a whole interval passed (a stalled machine).
                assert!(
                    before_first.elapsed() >= interval,
                    "logged within the interval"
                );
                return;
            }
        }
    }
    std::thread::sleep(interval + Duration::from_millis(20));
    assert_eq!(throttle.check(), Some(suppressed));
}

// ---------------------------------------------------------------------------
// cache::update (Redis): random event sequences keep the cache consistent
//
// A small universe (two guilds, a few channels, threads, roles, users and
// messages) so events collide often. After every event the whole database is
// read back and checked against the README contract: every index entry
// points at a key, every key is in its indexes, `channel:` never holds a
// thread, removals remove everything below them, outages remove nothing, the
// `old` value is the previous cached value, and nothing outside the event's
// scope changes.

/// The clock of every cache update here: 2026-10-20T12:00:00Z.
const NOW_MS: i64 = 1_792_497_600_000;
/// One minute before [`NOW_MS`]: a new join.
const JUST_JOINED: &str = "2026-10-20T11:59:00.000000+00:00";
const PIN_TIME: &str = "2026-10-20T11:30:00.000000+00:00";
const GUILDS: [u64; 2] = [1000, 2000];
const DM_CHANNEL: u64 = 500;
/// Channel ids are `guild + offset`; 3 and 4 are in the GUILD_CREATE fixture.
const CHANNELS: [u64; 3] = [3, 4, 6];
/// Threads as `(offset, parent offset)`; 5 is in the GUILD_CREATE fixture.
const THREADS: [(u64, u64); 2] = [(5, 3), (7, 4)];
const ROLES: [u64; 3] = [0, 1, 8];
const EMOJIS: [u64; 2] = [2, 9];
const USERS: [u64; 4] = [10, 11, 12, common::BOT_ID];
/// TTL of members and messages in `common::state_all()`.
const TTL_MS: i64 = 60_000;

/// Every key of the database, its type and its value, in one round trip.
const SNAPSHOT_SCRIPT: &str = r"
local out = {}
for _, key in ipairs(redis.call('KEYS', '*')) do
  local kind = redis.call('TYPE', key)['ok']
  if kind == 'string' then
    out[key] = {kind, redis.call('GET', key)}
  elseif kind == 'set' then
    out[key] = {kind, redis.call('SMEMBERS', key)}
  elseif kind == 'zset' then
    out[key] = {kind, redis.call('ZRANGE', key, 0, -1, 'WITHSCORES')}
  else
    out[key] = {kind}
  end
end
return cjson.encode(out)
";

#[derive(Clone, Debug, Default)]
struct Snapshot {
    strings: BTreeMap<String, Value>,
    sets: BTreeMap<String, BTreeSet<String>>,
    expiry: BTreeMap<String, f64>,
}

async fn snapshot(conn: &mut ConnectionManager) -> Snapshot {
    let raw: String = redis::cmd("EVAL")
        .arg(SNAPSHOT_SCRIPT)
        .arg(0)
        .query_async(conn)
        .await
        .expect("snapshot");
    let entries: BTreeMap<String, Vec<Value>> = serde_json::from_str(&raw).expect("snapshot JSON");
    let mut snapshot = Snapshot::default();
    for (key, entry) in entries {
        let text = |value: &Value| value.as_str().expect("a string").to_owned();
        match entry[0].as_str() {
            Some("string") => {
                let value = serde_json::from_str(&text(&entry[1]))
                    .unwrap_or_else(|err| panic!("{key} is not JSON: {err}"));
                snapshot.strings.insert(key, value);
            }
            Some("set") => {
                let members = entry[1].as_array().expect("members").iter().map(text);
                snapshot.sets.insert(key, members.collect());
            }
            Some("zset") => {
                assert_eq!(key, EXPIRY_ZSET, "unexpected sorted set");
                for pair in entry[1].as_array().expect("members").chunks(2) {
                    let score = text(&pair[1]).parse().expect("a score");
                    snapshot.expiry.insert(text(&pair[0]), score);
                }
            }
            other => panic!("{key} has unexpected type {other:?}"),
        }
    }
    snapshot
}

impl Snapshot {
    fn get(&self, key: &str) -> Option<&Value> {
        self.strings.get(key)
    }

    fn has(&self, key: &str) -> bool {
        self.strings.contains_key(key)
    }

    fn messages_of(&self, channel: u64) -> Vec<&String> {
        let prefix = format!("message:{channel}:");
        self.strings
            .keys()
            .filter(|key| key.starts_with(&prefix))
            .collect()
    }

    /// Whether a channel or thread of `guild` is cached.
    fn has_channel(&self, guild: u64, id: u64) -> bool {
        self.has(&format!("channel:{id}")) || self.has(&format!("thread:{guild}:{id}"))
    }
}

#[derive(Clone, Copy, Debug)]
enum Place {
    Channel(u64, usize),
    Thread(u64, usize),
    Dm,
}

impl Place {
    fn channel(self) -> u64 {
        match self {
            Self::Channel(g, i) => g + CHANNELS[i],
            Self::Thread(g, i) => g + THREADS[i].0,
            Self::Dm => DM_CHANNEL,
        }
    }

    fn guild(self) -> Option<u64> {
        match self {
            Self::Channel(g, _) | Self::Thread(g, _) => Some(g),
            Self::Dm => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum MessageAction {
    Create,
    Update,
    Delete,
    Bulk,
}

#[derive(Clone, Copy, Debug)]
enum MemberAction {
    Add { recent: bool },
    Update,
    Remove,
}

#[derive(Clone, Copy, Debug)]
enum RoleAction {
    Create,
    Update,
    Delete,
}

#[derive(Clone, Debug)]
enum CacheEvent {
    /// The GUILD_CREATE fixture, optionally without its second channel, its
    /// thread, its second role or its emoji.
    GuildCreate {
        g: u64,
        drop: [bool; 4],
    },
    /// GUILD_CREATE or GUILD_DELETE with `unavailable: true`.
    GuildOutage {
        g: u64,
        via_create: bool,
    },
    GuildDelete {
        g: u64,
    },
    GuildUpdate {
        g: u64,
    },
    ChannelUpsert {
        g: u64,
        c: usize,
        update: bool,
    },
    ChannelDelete {
        g: u64,
        c: usize,
    },
    ThreadUpsert {
        g: u64,
        t: usize,
        archived: bool,
        update: bool,
    },
    ThreadDelete {
        g: u64,
        t: usize,
    },
    ThreadListSync {
        g: u64,
        parents: Vec<usize>,
        listed: Vec<usize>,
    },
    Message {
        place: Place,
        m: u64,
        action: MessageAction,
    },
    Member {
        g: u64,
        u: usize,
        action: MemberAction,
    },
    Role {
        g: u64,
        r: usize,
        action: RoleAction,
    },
    Emojis {
        g: u64,
        keep: [bool; 2],
    },
    Presence {
        g: u64,
        u: usize,
        online: bool,
    },
    Voice {
        g: u64,
        u: usize,
        channel: Option<usize>,
    },
    Chunk {
        g: u64,
        users: [bool; 4],
    },
    Pins {
        place: Place,
    },
    Ready {
        listed: [bool; 2],
    },
}

fn guild_id() -> BoxedStrategy<u64> {
    prop::sample::select(GUILDS.to_vec()).boxed()
}

fn place() -> BoxedStrategy<Place> {
    prop_oneof![
        3 => (guild_id(), 0..CHANNELS.len()).prop_map(|(g, i)| Place::Channel(g, i)),
        2 => (guild_id(), 0..THREADS.len()).prop_map(|(g, i)| Place::Thread(g, i)),
        1 => Just(Place::Dm),
    ]
    .boxed()
}

fn cache_event() -> BoxedStrategy<CacheEvent> {
    use CacheEvent::*;
    let guilds = prop_oneof![
        3 => (guild_id(), any::<[bool; 4]>()).prop_map(|(g, drop)| GuildCreate { g, drop }),
        1 => (guild_id(), any::<bool>())
            .prop_map(|(g, via_create)| GuildOutage { g, via_create }),
        1 => guild_id().prop_map(|g| GuildDelete { g }),
        1 => guild_id().prop_map(|g| GuildUpdate { g }),
        1 => any::<[bool; 2]>().prop_map(|listed| Ready { listed }),
    ];
    let channels = prop_oneof![
        3 => (guild_id(), 0..CHANNELS.len(), any::<bool>())
            .prop_map(|(g, c, update)| ChannelUpsert { g, c, update }),
        1 => (guild_id(), 0..CHANNELS.len()).prop_map(|(g, c)| ChannelDelete { g, c }),
        3 => (guild_id(), 0..THREADS.len(), any::<bool>(), any::<bool>()).prop_map(
            |(g, t, archived, update)| ThreadUpsert { g, t, archived: archived && update, update }
        ),
        1 => (guild_id(), 0..THREADS.len()).prop_map(|(g, t)| ThreadDelete { g, t }),
        1 => (
            guild_id(),
            prop::collection::vec(0..2usize, 0..3),
            prop::collection::vec(0..THREADS.len(), 0..3),
        )
            .prop_map(|(g, parents, listed)| ThreadListSync { g, parents, listed }),
        1 => place().prop_map(|place| Pins { place }),
    ];
    let messages = (
        place(),
        1u64..4,
        prop::sample::select(vec![
            MessageAction::Create,
            MessageAction::Create,
            MessageAction::Update,
            MessageAction::Delete,
            MessageAction::Bulk,
        ]),
    )
        .prop_map(|(place, m, action)| Message { place, m, action });
    let member_action = prop_oneof![
        2 => any::<bool>().prop_map(|recent| MemberAction::Add { recent }),
        1 => Just(MemberAction::Update),
        1 => Just(MemberAction::Remove),
    ];
    let role_action = prop::sample::select(vec![
        RoleAction::Create,
        RoleAction::Update,
        RoleAction::Delete,
    ]);
    let members = prop_oneof![
        3 => (guild_id(), 0..USERS.len(), member_action)
            .prop_map(|(g, u, action)| Member { g, u, action }),
        2 => (guild_id(), 0..USERS.len(), any::<bool>())
            .prop_map(|(g, u, online)| Presence { g, u, online }),
        1 => (guild_id(), 0..USERS.len(), prop::option::of(0..CHANNELS.len()))
            .prop_map(|(g, u, channel)| Voice { g, u, channel }),
        1 => (guild_id(), any::<[bool; 4]>()).prop_map(|(g, users)| Chunk { g, users }),
        2 => (guild_id(), 0..ROLES.len(), role_action)
            .prop_map(|(g, r, action)| Role { g, r, action }),
        1 => (guild_id(), any::<[bool; 2]>()).prop_map(|(g, keep)| Emojis { g, keep }),
    ];
    prop_oneof![3 => guilds, 4 => channels, 3 => messages, 4 => members].boxed()
}

/// GUILD_UPDATE's `d`: a guild without the GUILD_CREATE extra fields.
fn guild_update_payload(g: u64) -> Value {
    let mut update = common::guild(g, "renamed");
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

fn guild_event(g: u64, drop: [bool; 4]) -> (&'static str, Value) {
    let mut d = common::guild(g, "g");
    for (dropped, list, id) in [
        (drop[0], "channels", g + 4),
        (drop[1], "threads", g + 5),
        (drop[2], "roles", g + 1),
        (drop[3], "emojis", g + 2),
    ] {
        if dropped {
            let id = json!(id.to_string());
            d[list]
                .as_array_mut()
                .unwrap()
                .retain(|item| item["id"] != id);
        }
    }
    ("GUILD_CREATE", d)
}

fn message_event(place: Place, m: u64, action: MessageAction) -> (&'static str, Value) {
    let channel = place.channel();
    let guild = place.guild();
    let (kind, mut d) = match action {
        MessageAction::Create => ("MESSAGE_CREATE", common::message(m, channel, guild, "hi")),
        MessageAction::Update => (
            "MESSAGE_UPDATE",
            common::message(m, channel, guild, "edited"),
        ),
        MessageAction::Delete => (
            "MESSAGE_DELETE",
            json!({"id": m.to_string(), "channel_id": channel.to_string()}),
        ),
        MessageAction::Bulk => (
            "MESSAGE_DELETE_BULK",
            json!({"ids": [m.to_string(), (m + 1).to_string()], "channel_id": channel.to_string()}),
        ),
    };
    if let Some(guild) = guild {
        d["guild_id"] = json!(guild.to_string());
    }
    (kind, d)
}

fn member_event(g: u64, user: u64, action: MemberAction) -> (&'static str, Value) {
    match action {
        MemberAction::Add { recent } => {
            let mut d = common::member(user, &[g + 1]);
            d["guild_id"] = json!(g.to_string());
            if recent {
                d["joined_at"] = json!(JUST_JOINED);
            }
            ("GUILD_MEMBER_ADD", d)
        }
        MemberAction::Update => {
            let mut d = common::member(user, &[g + 1, g]);
            d["guild_id"] = json!(g.to_string());
            d["nick"] = json!("nick");
            ("GUILD_MEMBER_UPDATE", d)
        }
        MemberAction::Remove => (
            "GUILD_MEMBER_REMOVE",
            json!({"guild_id": g.to_string(), "user": common::user(user, "u")}),
        ),
    }
}

/// The gateway event (type and `d`) for a [`CacheEvent`].
fn payload_of(event: &CacheEvent) -> (&'static str, Value) {
    use CacheEvent::*;
    let pick = |flag: bool, yes: &'static str, no: &'static str| if flag { yes } else { no };
    match *event {
        GuildCreate { g, drop } => guild_event(g, drop),
        GuildOutage { g, via_create } => (
            pick(via_create, "GUILD_CREATE", "GUILD_DELETE"),
            json!({"id": g.to_string(), "unavailable": true}),
        ),
        GuildDelete { g } => ("GUILD_DELETE", json!({"id": g.to_string()})),
        GuildUpdate { g } => ("GUILD_UPDATE", guild_update_payload(g)),
        ChannelUpsert { g, c, update } => (
            pick(update, "CHANNEL_UPDATE", "CHANNEL_CREATE"),
            common::channel(g + CHANNELS[c], Some(g), pick(update, "renamed", "created")),
        ),
        ChannelDelete { g, c } => (
            "CHANNEL_DELETE",
            common::channel(g + CHANNELS[c], Some(g), "gone"),
        ),
        ThreadUpsert {
            g,
            t,
            archived,
            update,
        } => {
            let (offset, parent) = THREADS[t];
            let mut d = common::thread(g + offset, g, g + parent);
            d["thread_metadata"]["archived"] = json!(archived);
            (pick(update, "THREAD_UPDATE", "THREAD_CREATE"), d)
        }
        ThreadDelete { g, t } => {
            let (offset, parent) = THREADS[t];
            (
                "THREAD_DELETE",
                json!({"id": (g + offset).to_string(), "guild_id": g.to_string(),
                    "parent_id": (g + parent).to_string(), "type": 11}),
            )
        }
        ThreadListSync {
            g,
            ref parents,
            ref listed,
        } => {
            let (parents, listed) = thread_list_scope(parents, listed);
            let threads: Vec<Value> = listed
                .iter()
                .map(|i| {
                    let (offset, parent) = THREADS[*i];
                    // Threads in THREAD_LIST_SYNC may come without `guild_id`.
                    let mut thread = common::thread(g + offset, g, g + parent);
                    thread.as_object_mut().unwrap().remove("guild_id");
                    thread
                })
                .collect();
            let channel_ids: Vec<String> = parents.iter().map(|p| (g + p).to_string()).collect();
            (
                "THREAD_LIST_SYNC",
                json!({"guild_id": g.to_string(), "channel_ids": channel_ids,
                    "threads": threads, "members": []}),
            )
        }
        Message { place, m, action } => message_event(place, m, action),
        Member { g, u, action } => member_event(g, USERS[u], action),
        Role { g, r, action } => {
            let id = g + ROLES[r];
            match action {
                RoleAction::Delete => (
                    "GUILD_ROLE_DELETE",
                    json!({"guild_id": g.to_string(), "role_id": id.to_string()}),
                ),
                RoleAction::Create | RoleAction::Update => (
                    pick(
                        matches!(action, RoleAction::Create),
                        "GUILD_ROLE_CREATE",
                        "GUILD_ROLE_UPDATE",
                    ),
                    json!({"guild_id": g.to_string(), "role": common::role(id, "r")}),
                ),
            }
        }
        Emojis { g, keep } => {
            let emojis: Vec<Value> = EMOJIS
                .iter()
                .zip(keep)
                .filter(|(_, keep)| *keep)
                .map(|(offset, _)| common::emoji(g + offset, "e"))
                .collect();
            (
                "GUILD_EMOJIS_UPDATE",
                json!({"guild_id": g.to_string(), "emojis": emojis}),
            )
        }
        Presence { g, u, online } => (
            "PRESENCE_UPDATE",
            common::presence(g, USERS[u], pick(online, "online", "offline")),
        ),
        Voice { g, u, channel } => (
            "VOICE_STATE_UPDATE",
            common::voice_state(Some(g), USERS[u], channel.map(|c| g + CHANNELS[c])),
        ),
        Chunk { g, users } => {
            let chosen: Vec<u64> = USERS
                .iter()
                .zip(users)
                .filter(|(_, chosen)| *chosen)
                .map(|(user, _)| *user)
                .collect();
            let members: Vec<Value> = chosen.iter().map(|u| common::member(*u, &[])).collect();
            let presences: Vec<Value> = chosen
                .iter()
                .map(|u| common::presence(g, *u, "online"))
                .collect();
            (
                "GUILD_MEMBERS_CHUNK",
                json!({"guild_id": g.to_string(), "members": members, "presences": presences,
                    "chunk_index": 0, "chunk_count": 1}),
            )
        }
        Pins { place } => {
            let mut d = json!({"channel_id": place.channel().to_string(),
                "last_pin_timestamp": PIN_TIME});
            if let Some(guild) = place.guild() {
                d["guild_id"] = json!(guild.to_string());
            }
            ("CHANNEL_PINS_UPDATE", d)
        }
        Ready { listed } => {
            let guilds: Vec<u64> = GUILDS
                .iter()
                .zip(listed)
                .filter(|(_, listed)| *listed)
                .map(|(g, _)| *g)
                .collect();
            ("READY", common::ready([0, 1], &guilds))
        }
    }
}

/// THREAD_LIST_SYNC's channel offsets and listed threads: Discord only lists
/// active threads of the synced channels (of every channel when none is given).
fn thread_list_scope(parents: &[usize], listed: &[usize]) -> (BTreeSet<u64>, BTreeSet<usize>) {
    let parents: BTreeSet<u64> = parents.iter().map(|i| CHANNELS[*i]).collect();
    let listed = listed
        .iter()
        .copied()
        .filter(|i| parents.is_empty() || parents.contains(&THREADS[*i].1))
        .collect();
    (parents, listed)
}

/// Channel and thread ids of a guild in the universe.
fn guild_channel_ids(g: u64) -> Vec<u64> {
    CHANNELS
        .iter()
        .map(|c| g + c)
        .chain(THREADS.iter().map(|(t, _)| g + t))
        .collect()
}

/// Whether a stored key is one of `g`'s (messages aside): the guild, a
/// `<prefix>:<g>:<id>` child or a `channel:<id>` whose value names `g`.
fn belongs_to_guild(key: &str, value: Option<&Value>, g: u64) -> bool {
    let g = g.to_string();
    match key.split(':').collect::<Vec<_>>().as_slice() {
        ["guild", id] => *id == g,
        ["channel", _] => value.and_then(|v| v["guild_id"].as_str()) == Some(g.as_str()),
        [prefix, id, _] => *prefix != "message" && *id == g,
        _ => false,
    }
}

/// Every member of `set` starts with `prefix`.
fn only_prefixed(set: &str, members: &BTreeSet<String>, prefix: &str) -> Result<(), TestCaseError> {
    for member in members {
        prop_assert!(member.starts_with(prefix), "{} lists {}", set, member);
    }
    Ok(())
}

/// Index sets point at existing keys of their own kind.
fn check_indexes(after: &Snapshot) -> Result<(), TestCaseError> {
    for (set, members) in &after.sets {
        prop_assert!(
            set.ends_with("_keys") || set.contains("_keys:"),
            "unexpected set {}",
            set
        );
        for member in members {
            prop_assert!(after.has(member), "{} lists missing key {}", set, member);
        }
        if let Some(g) = set.strip_prefix("guild_keys:") {
            let g: u64 = g.parse().expect("guild id");
            for member in members {
                prop_assert!(
                    belongs_to_guild(member, after.get(member), g),
                    "{} lists {}",
                    set,
                    member
                );
            }
        } else if let Some(g) = set.strip_prefix("thread_keys:") {
            only_prefixed(set, members, &format!("thread:{g}:"))?;
        } else if let Some(c) = set.strip_prefix("channel_keys:") {
            only_prefixed(set, members, &format!("message:{c}:"))?;
        } else if let Some(prefix) = set.strip_suffix("_keys") {
            only_prefixed(set, members, &format!("{prefix}:"))?;
        }
    }
    for member in after.expiry.keys() {
        prop_assert!(
            after.has(member),
            "expiry_zset lists missing key {}",
            member
        );
    }
    Ok(())
}

/// Every key is in its indexes and holds what its name says.
fn check_values(after: &Snapshot) -> Result<(), TestCaseError> {
    let id = |value: &Value| value.as_str().map(str::to_owned);
    for (key, value) in &after.strings {
        if !key.contains(':') {
            prop_assert_eq!(key.as_str(), "bot_user");
            continue;
        }
        for (set, member) in keys::plan(key).indexes {
            prop_assert!(
                after.sets.get(&set).is_some_and(|s| s.contains(&member)),
                "{} is not in {}",
                key,
                set
            );
        }
        let parts: Vec<String> = key.split(':').map(str::to_owned).collect();
        let part = |i: usize| Some(parts[i].clone());
        let thread_type = matches!(value["type"].as_u64(), Some(10..=12));
        match parts[0].as_str() {
            "guild" => {
                prop_assert_eq!(id(&value["id"]), part(1), "{}", key);
                if value.get("name").is_some() {
                    for field in merge::SPLIT_GUILD_FIELDS {
                        prop_assert_eq!(&value[field], &json!([]), "{}.{}", key, field);
                    }
                    prop_assert!(value["unavailable"].is_boolean(), "{}: {}", key, value);
                    prop_assert!(value["threads"].is_array(), "{}: {}", key, value);
                } else {
                    prop_assert_eq!(&value["unavailable"], &json!(true), "stub {}", key);
                }
            }
            "channel" => {
                prop_assert!(!thread_type, "a thread under {}", key);
                prop_assert_eq!(id(&value["id"]), part(1), "{}", key);
                if let Some(g) = value["guild_id"].as_str() {
                    let index = format!("guild_keys:{g}");
                    prop_assert!(
                        after.sets.get(&index).is_some_and(|s| s.contains(key)),
                        "{} is not in {}",
                        key,
                        index
                    );
                }
            }
            "thread" => {
                prop_assert!(thread_type, "{} is not a thread: {}", key, value);
                prop_assert_eq!(id(&value["guild_id"]), part(1), "{}", key);
                prop_assert_eq!(id(&value["id"]), part(2), "{}", key);
            }
            "member" => {
                prop_assert_eq!(id(&value["guild_id"]), part(1), "{}", key);
                prop_assert_eq!(id(&value["user"]["id"]), part(2), "{}", key);
            }
            "presence" => {
                prop_assert_eq!(id(&value["guild_id"]), part(1), "{}", key);
                prop_assert_eq!(id(&value["user"]["id"]), part(2), "{}", key);
                prop_assert_ne!(&value["status"], &json!("offline"), "{}", key);
            }
            "voice" => {
                prop_assert_eq!(id(&value["guild_id"]), part(1), "{}", key);
                prop_assert_eq!(id(&value["user_id"]), part(2), "{}", key);
                prop_assert!(value["channel_id"].is_string(), "{}", key);
            }
            "role" | "emoji" => prop_assert_eq!(id(&value["id"]), part(2), "{}", key),
            "message" => {
                prop_assert_eq!(id(&value["channel_id"]), part(1), "{}", key);
                prop_assert_eq!(id(&value["id"]), part(2), "{}", key);
            }
            other => prop_assert!(false, "unexpected key type {} ({})", other, key),
        }
    }
    Ok(())
}

/// A channel or thread is gone, with its message index and its messages.
fn check_channel_gone(after: &Snapshot, key: &str, id: u64) -> Result<(), TestCaseError> {
    prop_assert!(!after.has(key), "{} survived", key);
    prop_assert!(
        !after.sets.contains_key(&keys::channel_index(id)),
        "channel_keys:{} survived",
        id
    );
    prop_assert!(
        after.messages_of(id).is_empty(),
        "messages of {} survived: {:?}",
        id,
        after.messages_of(id)
    );
    Ok(())
}

/// Nothing of `g` is left (messages of channels that were cached included).
fn check_guild_gone(before: &Snapshot, after: &Snapshot, g: u64) -> Result<(), TestCaseError> {
    for (key, value) in &after.strings {
        prop_assert!(!belongs_to_guild(key, Some(value), g), "{} survived", key);
    }
    for set in [keys::guild_index(g), keys::thread_index(g)] {
        prop_assert!(!after.sets.contains_key(&set), "{} survived", set);
    }
    for id in guild_channel_ids(g) {
        if before.has_channel(g, id) {
            prop_assert!(
                after.messages_of(id).is_empty(),
                "messages of {} survived",
                id
            );
        }
    }
    Ok(())
}

fn expiry_of(snapshot: &Snapshot, key: &str) -> Option<i64> {
    snapshot.expiry.get(key).map(|score| *score as i64)
}

/// GUILD_CREATE replaces everything cached for the guild, except the messages
/// of channels it still has.
fn check_guild_create(
    g: u64,
    drop: [bool; 4],
    before: &Snapshot,
    after: &Snapshot,
    outcome: &Outcome,
) -> Result<(), TestCaseError> {
    let mut expected: BTreeSet<String> = [
        format!("guild:{g}"),
        format!("channel:{}", g + 3),
        format!("role:{g}:{g}"),
        format!("member:{g}:{}", common::BOT_ID),
        format!("member:{g}:10"),
        format!("presence:{g}:10"),
        format!("voice:{g}:10"),
    ]
    .into_iter()
    .collect();
    let mut kept = vec![g + 3];
    if !drop[0] {
        expected.insert(format!("channel:{}", g + 4));
        kept.push(g + 4);
    }
    if !drop[1] {
        expected.insert(format!("thread:{g}:{}", g + 5));
        kept.push(g + 5);
    }
    if !drop[2] {
        expected.insert(format!("role:{g}:{}", g + 1));
    }
    if !drop[3] {
        expected.insert(format!("emoji:{g}:{}", g + 2));
    }
    let cached: BTreeSet<String> = after
        .strings
        .iter()
        .filter(|(key, value)| belongs_to_guild(key, Some(value), g))
        .map(|(key, _)| key.clone())
        .collect();
    prop_assert_eq!(cached, expected);
    let blob = &after.strings[&format!("guild:{g}")];
    prop_assert_eq!(&blob["name"], &json!("g"));
    prop_assert_eq!(&blob["member_count"], &json!(2));
    prop_assert_eq!(&blob["unavailable"], &json!(false));
    for id in guild_channel_ids(g) {
        if kept.contains(&id) {
            for message in before.messages_of(id) {
                prop_assert_eq!(after.get(message), before.get(message), "{}", message);
            }
        } else if before.has_channel(g, id) {
            prop_assert!(after.messages_of(id).is_empty(), "messages of {}", id);
        }
    }
    // Members expire, the bot never does.
    let member = format!("member:{g}:10");
    prop_assert_eq!(expiry_of(after, &member), Some(NOW_MS + TTL_MS));
    let bot = format!("member:{g}:{}", common::BOT_ID);
    prop_assert_eq!(expiry_of(after, &bot), None);
    let previous = before.get(&format!("guild:{g}")).cloned();
    prop_assert_eq!(outcome.guild_known, previous.is_some());
    prop_assert_eq!(&outcome.previous_guild, &previous);
    Ok(())
}

fn check_thread_list_sync(
    g: u64,
    parents: &[usize],
    listed: &[usize],
    before: &Snapshot,
    after: &Snapshot,
) -> Result<(), TestCaseError> {
    let (parents, listed) = thread_list_scope(parents, listed);
    for (i, (offset, _)) in THREADS.iter().enumerate() {
        let id = g + offset;
        let key = format!("thread:{g}:{id}");
        let parent = before
            .get(&key)
            .and_then(|t| t["parent_id"].as_str()?.parse::<u64>().ok());
        let in_scope = parent.is_some_and(|p| parents.is_empty() || parents.contains(&(p - g)));
        if listed.contains(&i) {
            let value = after.get(&key).expect("listed thread written");
            prop_assert_eq!(&value["guild_id"], &json!(g.to_string()));
        } else if in_scope {
            // Cached in a synced channel but not listed: no longer active.
            check_channel_gone(after, &key, id)?;
        } else {
            prop_assert_eq!(after.get(&key), before.get(&key), "{} is out of scope", key);
        }
    }
    Ok(())
}

fn check_member(
    g: u64,
    user: u64,
    action: MemberAction,
    before: &Snapshot,
    after: &Snapshot,
) -> Result<(), TestCaseError> {
    let key = format!("member:{g}:{user}");
    let count = |snapshot: &Snapshot| {
        snapshot
            .get(&format!("guild:{g}"))
            .and_then(|guild| guild["member_count"].as_u64())
    };
    match action {
        MemberAction::Add { recent } => {
            prop_assert!(after.has(&key));
            // Only a recent join of a member that was not cached counts.
            let new = recent && !before.has(&key);
            prop_assert_eq!(count(after), count(before).map(|n| n + u64::from(new)));
        }
        MemberAction::Update => {
            let value = after.get(&key).expect("member written");
            prop_assert_eq!(&value["nick"], &json!("nick"));
            prop_assert_eq!(count(after), count(before));
        }
        MemberAction::Remove => {
            prop_assert!(!after.has(&key));
            prop_assert!(
                !after.has(&format!("presence:{g}:{user}")),
                "{} survived",
                format!("presence:{g}:{user}")
            );
            prop_assert_eq!(count(after), count(before).map(|n| n.saturating_sub(1)));
        }
    }
    if after.has(&key) {
        let expected = (user != common::BOT_ID).then_some(NOW_MS + TTL_MS);
        prop_assert_eq!(expiry_of(after, &key), expected, "{}", key);
    }
    Ok(())
}

fn check_message(
    place: Place,
    m: u64,
    action: MessageAction,
    before: &Snapshot,
    after: &Snapshot,
) -> Result<(), TestCaseError> {
    let channel = place.channel();
    let key = format!("message:{channel}:{m}");
    match action {
        MessageAction::Create => {
            prop_assert!(after.has(&key));
            prop_assert_eq!(expiry_of(after, &key), Some(NOW_MS + TTL_MS));
        }
        // Only cached messages are updated.
        MessageAction::Update => {
            prop_assert_eq!(after.has(&key), before.has(&key));
            if let Some(value) = after.get(&key) {
                prop_assert_eq!(&value["content"], &json!("edited"));
            }
        }
        MessageAction::Delete => prop_assert!(!after.has(&key)),
        MessageAction::Bulk => {
            prop_assert!(!after.has(&key));
            prop_assert!(
                !after.has(&format!("message:{channel}:{}", m + 1)),
                "{} survived",
                format!("message:{channel}:{}", m + 1)
            );
        }
    }
    Ok(())
}

/// The postconditions of one event.
fn check_event(
    event: &CacheEvent,
    before: &Snapshot,
    after: &Snapshot,
    outcome: &Outcome,
) -> Result<(), TestCaseError> {
    use CacheEvent::*;
    match *event {
        GuildCreate { g, drop } => check_guild_create(g, drop, before, after, outcome)?,
        GuildOutage { g, via_create } => {
            // An outage keeps the guild and everything below it, flagged.
            let key = format!("guild:{g}");
            let expected = match before.get(&key) {
                Some(Value::Object(blob)) => {
                    let mut blob = blob.clone();
                    blob.insert("unavailable".into(), json!(true));
                    Value::Object(blob)
                }
                _ => json!({"id": g.to_string(), "unavailable": true}),
            };
            prop_assert_eq!(after.get(&key), Some(&expected));
            prop_assert_eq!(&outcome.previous_guild, &before.get(&key).cloned());
            if via_create {
                prop_assert_eq!(outcome.guild_known, before.has(&key));
            }
        }
        GuildDelete { g } => {
            check_guild_gone(before, after, g)?;
            let previous = before.get(&format!("guild:{g}")).cloned();
            prop_assert_eq!(&outcome.previous_guild, &previous);
        }
        GuildUpdate { g } => {
            let key = format!("guild:{g}");
            let blob = after.get(&key).expect("guild written");
            prop_assert_eq!(&blob["name"], &json!("renamed"));
            prop_assert_eq!(&blob["unavailable"], &json!(false));
            if let Some(previous) = before.get(&key).filter(|v| v.get("name").is_some()) {
                // GUILD_CREATE-only fields survive.
                for field in ["member_count", "joined_at", "large", "threads"] {
                    prop_assert_eq!(&blob[field], &previous[field], "{}", field);
                }
            }
            for role in [g, g + 1] {
                prop_assert!(
                    after.has(&format!("role:{g}:{role}")),
                    "{} missing",
                    format!("role:{g}:{role}")
                );
            }
        }
        ChannelUpsert { g, c, update } => {
            let id = g + CHANNELS[c];
            let value = after
                .get(&format!("channel:{id}"))
                .expect("channel written");
            prop_assert_eq!(&value["guild_id"], &json!(g.to_string()));
            let name = if update { "renamed" } else { "created" };
            prop_assert_eq!(&value["name"], &json!(name));
            prop_assert_eq!(after.messages_of(id), before.messages_of(id));
        }
        ChannelDelete { g, c } => {
            let id = g + CHANNELS[c];
            check_channel_gone(after, &format!("channel:{id}"), id)?;
            // Its threads go with it.
            for (offset, _) in THREADS {
                let key = format!("thread:{g}:{}", g + offset);
                let parent = before.get(&key).and_then(|t| t["parent_id"].as_str());
                if parent == Some(id.to_string().as_str()) {
                    check_channel_gone(after, &key, g + offset)?;
                }
            }
        }
        ThreadUpsert { g, t, archived, .. } => {
            let (offset, parent) = THREADS[t];
            let key = format!("thread:{g}:{}", g + offset);
            if archived {
                // Only active threads are cached.
                check_channel_gone(after, &key, g + offset)?;
            } else {
                let value = after.get(&key).expect("thread written");
                prop_assert_eq!(&value["parent_id"], &json!((g + parent).to_string()));
                prop_assert!(
                    !after.has(&format!("channel:{}", g + offset)),
                    "{} survived",
                    format!("channel:{}", g + offset)
                );
            }
        }
        ThreadDelete { g, t } => {
            let id = g + THREADS[t].0;
            check_channel_gone(after, &format!("thread:{g}:{id}"), id)?;
        }
        ThreadListSync {
            g,
            ref parents,
            ref listed,
        } => check_thread_list_sync(g, parents, listed, before, after)?,
        Message { place, m, action } => check_message(place, m, action, before, after)?,
        Member { g, u, action } => check_member(g, USERS[u], action, before, after)?,
        Role { g, r, action } => {
            let key = format!("role:{g}:{}", g + ROLES[r]);
            prop_assert_eq!(after.has(&key), !matches!(action, RoleAction::Delete));
        }
        Emojis { g, keep } => {
            let expected: BTreeSet<String> = EMOJIS
                .iter()
                .zip(keep)
                .filter(|(_, keep)| *keep)
                .map(|(offset, _)| format!("emoji:{g}:{}", g + offset))
                .collect();
            let prefix = format!("emoji:{g}:");
            let cached: BTreeSet<String> = after
                .strings
                .keys()
                .filter(|key| key.starts_with(&prefix))
                .cloned()
                .collect();
            prop_assert_eq!(cached, expected);
        }
        Presence { g, u, online } => {
            prop_assert_eq!(after.has(&format!("presence:{g}:{}", USERS[u])), online);
        }
        Voice { g, u, channel } => {
            let value = after.get(&format!("voice:{g}:{}", USERS[u]));
            let expected = channel.map(|c| json!((g + CHANNELS[c]).to_string()));
            prop_assert_eq!(value.map(|v| v["channel_id"].clone()), expected);
        }
        Chunk { g, users } => {
            for (user, chosen) in USERS.iter().zip(users) {
                if chosen {
                    prop_assert!(
                        after.has(&format!("member:{g}:{user}")),
                        "{} missing",
                        format!("member:{g}:{user}")
                    );
                    prop_assert!(
                        after.has(&format!("presence:{g}:{user}")),
                        "{} missing",
                        format!("presence:{g}:{user}")
                    );
                }
            }
        }
        Pins { place } => {
            let id = place.channel();
            let mut keys = vec![format!("channel:{id}")];
            if let Some(g) = place.guild() {
                keys.push(format!("thread:{g}:{id}"));
            }
            for key in keys {
                prop_assert_eq!(after.has(&key), before.has(&key));
                if let Some(value) = after.get(&key) {
                    prop_assert_eq!(&value["last_pin_timestamp"], &json!(PIN_TIME), "{}", key);
                }
            }
        }
        Ready { listed } => {
            prop_assert!(after.has("bot_user"));
            for (g, listed) in GUILDS.iter().zip(listed) {
                let key = format!("guild:{g}");
                if listed {
                    // A stub only where nothing was cached yet.
                    let stub = json!({"id": g.to_string(), "unavailable": true});
                    let expected = before.get(&key).cloned().unwrap_or(stub);
                    prop_assert_eq!(after.get(&key), Some(&expected));
                } else if before.has(&key) {
                    // Left while we were disconnected.
                    check_guild_gone(before, after, *g)?;
                }
            }
        }
    }
    Ok(())
}

/// With STATE_OLD, `old` is exactly the previous cached value (None where the
/// event has no previous value, like a create).
fn check_old(
    event: &CacheEvent,
    before: &Snapshot,
    outcome: &Outcome,
) -> Result<(), TestCaseError> {
    use CacheEvent::*;
    let old = |key: String| Some(before.get(&key).cloned());
    let expected = match *event {
        GuildCreate { g, .. } | GuildOutage { g, .. } | GuildDelete { g } | GuildUpdate { g } => {
            old(format!("guild:{g}"))
        }
        ChannelUpsert { update: false, .. } | ThreadUpsert { update: false, .. } => Some(None),
        ChannelUpsert { g, c, .. } | ChannelDelete { g, c } => {
            old(format!("channel:{}", g + CHANNELS[c]))
        }
        ThreadUpsert { g, t, .. } | ThreadDelete { g, t } => {
            old(format!("thread:{g}:{}", g + THREADS[t].0))
        }
        Message { place, m, action } => match action {
            MessageAction::Create => Some(None),
            MessageAction::Update | MessageAction::Delete => {
                old(format!("message:{}:{m}", place.channel()))
            }
            // All cached messages of the batch, in no particular order.
            MessageAction::Bulk => None,
        },
        Member { g, u, action } => match action {
            MemberAction::Add { .. } => Some(None),
            MemberAction::Update | MemberAction::Remove => old(format!("member:{g}:{}", USERS[u])),
        },
        Role { g, r, action } => match action {
            RoleAction::Create => Some(None),
            RoleAction::Update | RoleAction::Delete => old(format!("role:{g}:{}", g + ROLES[r])),
        },
        Presence { g, u, .. } => old(format!("presence:{g}:{}", USERS[u])),
        Voice { g, u, .. } => old(format!("voice:{g}:{}", USERS[u])),
        Pins { .. } | Chunk { .. } | Ready { .. } | ThreadListSync { .. } => Some(None),
        // Every cached emoji of the guild, in no particular order.
        Emojis { .. } => None,
    };
    if let Some(expected) = expected {
        prop_assert_eq!(&outcome.old, &expected, "old of {:?}", event);
    }
    Ok(())
}

/// Which stored keys an event may write or delete.
fn may_change(event: &CacheEvent, key: &str, value: Option<&Value>) -> bool {
    use CacheEvent::*;
    let messages_in = |channels: &[u64]| {
        channels
            .iter()
            .any(|c| key.starts_with(&format!("message:{c}:")))
    };
    let threads_of = |g: u64| -> Vec<u64> { THREADS.iter().map(|(t, _)| g + t).collect() };
    match *event {
        Ready { .. } => true,
        GuildCreate { g, .. } | GuildDelete { g } => {
            belongs_to_guild(key, value, g) || messages_in(&guild_channel_ids(g))
        }
        GuildOutage { g, .. } => key == format!("guild:{g}"),
        GuildUpdate { g } => {
            key == format!("guild:{g}")
                || key.starts_with(&format!("role:{g}:"))
                || key.starts_with(&format!("emoji:{g}:"))
        }
        ChannelUpsert { g, c, .. } => key == format!("channel:{}", g + CHANNELS[c]),
        ChannelDelete { g, c } => {
            let id = g + CHANNELS[c];
            let child_thread = key.starts_with(&format!("thread:{g}:"))
                && value.is_some_and(|v| v["parent_id"] == json!(id.to_string()));
            key == format!("channel:{id}")
                || child_thread
                || messages_in(&[id])
                || messages_in(&threads_of(g))
        }
        ThreadUpsert { g, t, .. } | ThreadDelete { g, t } => {
            let id = g + THREADS[t].0;
            key == format!("thread:{g}:{id}") || messages_in(&[id])
        }
        ThreadListSync { g, .. } => {
            key.starts_with(&format!("thread:{g}:")) || messages_in(&threads_of(g))
        }
        Message { place, .. } => messages_in(&[place.channel()]),
        Member { g, u, .. } => {
            let user = USERS[u];
            key == format!("member:{g}:{user}")
                || key == format!("presence:{g}:{user}")
                || key == format!("guild:{g}")
        }
        Role { g, r, .. } => key == format!("role:{g}:{}", g + ROLES[r]),
        Emojis { g, .. } => key.starts_with(&format!("emoji:{g}:")),
        Presence { g, u, .. } => key == format!("presence:{g}:{}", USERS[u]),
        Voice { g, u, .. } => key == format!("voice:{g}:{}", USERS[u]),
        Chunk { g, .. } => {
            key.starts_with(&format!("member:{g}:")) || key.starts_with(&format!("presence:{g}:"))
        }
        Pins { place } => {
            let id = place.channel();
            key == format!("channel:{id}")
                || place
                    .guild()
                    .is_some_and(|g| key == format!("thread:{g}:{id}"))
        }
    }
}

/// Nothing outside the event's scope changes (values or expiries).
fn check_frame(
    event: &CacheEvent,
    before: &Snapshot,
    after: &Snapshot,
) -> Result<(), TestCaseError> {
    let keys: BTreeSet<&String> = before
        .strings
        .keys()
        .chain(after.strings.keys())
        .chain(before.expiry.keys())
        .chain(after.expiry.keys())
        .collect();
    for key in keys {
        let changed =
            before.get(key) != after.get(key) || before.expiry.get(key) != after.expiry.get(key);
        if changed {
            let value = before.get(key).or_else(|| after.get(key));
            prop_assert!(
                may_change(event, key, value),
                "{:?} changed {}: {:?} -> {:?}",
                event,
                key,
                before.get(key),
                after.get(key)
            );
        }
    }
    Ok(())
}

async fn replay(conn: &mut ConnectionManager, events: &[CacheEvent]) -> Result<(), TestCaseError> {
    let state = common::state_all();
    let ctx = UpdateContext {
        state: &state,
        bot_id: Some(common::bot_id()),
        now_ms: NOW_MS,
    };
    let mut before = snapshot(conn).await;
    for (step, event) in events.iter().enumerate() {
        let (kind, d) = payload_of(event);
        let mut typed = common::event(kind, d);
        let outcome = cache::update(conn, &mut typed, &ctx)
            .await
            .map_err(|err| TestCaseError::fail(format!("step {step}: {event:?}: {err}")))?;
        let after = snapshot(conn).await;
        let context = |err: TestCaseError| TestCaseError::fail(format!("step {step}: {err}"));
        check_indexes(&after).map_err(context)?;
        check_values(&after).map_err(context)?;
        check_event(event, &before, &after, &outcome).map_err(context)?;
        check_old(event, &before, &outcome).map_err(context)?;
        check_frame(event, &before, &after).map_err(context)?;
        before = after;
    }
    Ok(())
}

#[test]
fn the_cache_stays_consistent_under_random_events() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let Some(conn) = runtime.block_on(common::redis(9)) else {
        return;
    };
    let mut runner = proptest::test_runner::TestRunner::new(cases(48));
    let events = prop::collection::vec(cache_event(), 1..30);
    let result = runner.run(&events, |events| {
        runtime.block_on(async {
            let mut conn = conn.clone();
            redis::cmd("FLUSHDB")
                .query_async::<()>(&mut conn)
                .await
                .expect("flush");
            replay(&mut conn, &events).await
        })
    });
    if let Err(err) = result {
        panic!("{err}");
    }
}

// ---------------------------------------------------------------------------
// models, sessions, registry: what is stored in Redis for operators and for
// the next start

/// 1970-01-01 .. 9999-12-31T23:59:59.999999999 (mostly around now).
fn datetime() -> BoxedStrategy<time::OffsetDateTime> {
    prop_oneof![
        3 => 1_600_000_000_000_000_000i128..1_900_000_000_000_000_000,
        1 => 0i128..=253_402_300_799_999_999_999,
        1 => (0i128..=253_402_300_799).prop_map(|secs| secs * 1_000_000_000),
    ]
    .prop_map(|nanos| time::OffsetDateTime::from_unix_timestamp_nanos(nanos).unwrap())
    .boxed()
}

/// `YYYY-MM-DDTHH:MM:SS.f` with 1 to 9 fraction digits.
fn is_formatted_timestamp(text: &str) -> bool {
    let bytes = text.as_bytes();
    let digit =
        |range: std::ops::Range<usize>| range.into_iter().all(|i| bytes[i].is_ascii_digit());
    bytes.len() >= 21
        && bytes.len() <= 29
        && digit(0..4)
        && bytes[4] == b'-'
        && digit(5..7)
        && bytes[7] == b'-'
        && digit(8..10)
        && bytes[10] == b'T'
        && digit(11..13)
        && bytes[13] == b':'
        && digit(14..16)
        && bytes[16] == b':'
        && digit(17..19)
        && bytes[19] == b'.'
        && digit(20..bytes.len())
}

fn session_info() -> BoxedStrategy<SessionInfo> {
    (
        prop_oneof![1 => Just(String::new()), 4 => "[a-f0-9]{1,32}"],
        prop_oneof![1 => Just(0u64), 4 => any::<u64>()],
        prop::option::of("wss://[a-z0-9-]{1,20}\\.discord\\.gg"),
    )
        .prop_map(|(session_id, sequence, resume_url)| SessionInfo {
            session_id,
            sequence,
            resume_url,
        })
        .boxed()
}

proptest! {
    #![proptest_config(cases(512))]

    #[test]
    fn timestamps_keep_their_stored_format(at in datetime(), other in datetime()) {
        let stamp = FormattedDateTime::from_offset(at);
        let text = stamp.format();
        prop_assert!(is_formatted_timestamp(&text), "{}", text);
        prop_assert_eq!(FormattedDateTime::parse(&text), Some(stamp));
        prop_assert_eq!(
            i128::from(stamp.unix_millis()),
            at.unix_timestamp_nanos().div_euclid(1_000_000)
        );
        // As JSON it is that string.
        let json = serde_json::to_string(&stamp).unwrap();
        prop_assert_eq!(&json, &format!("\"{text}\""));
        prop_assert_eq!(serde_json::from_str::<FormattedDateTime>(&json).unwrap(), stamp);
        // Sorting the stored strings sorts by time.
        let other = FormattedDateTime::from_offset(other);
        prop_assert_eq!(text.cmp(&other.format()), stamp.cmp(&other));
    }

    #[test]
    fn timestamp_parsing_never_panics(text in prop_oneof![
        "(?s).{0,32}",
        "[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}\\.[0-9]{0,12}",
    ]) {
        if let Some(stamp) = FormattedDateTime::parse(&text) {
            prop_assert_eq!(FormattedDateTime::parse(&stamp.format()), Some(stamp));
        }
        let _ = serde_json::from_value::<FormattedDateTime>(Value::String(text));
    }

    #[test]
    fn sessions_round_trip_as_json(session in session_info()) {
        let json = serde_json::to_value(&session).unwrap();
        let mut keys = vec!["sequence", "session_id"];
        if session.resume_url.is_some() {
            keys.push("resume_url");
        }
        let stored: Vec<&str> = json.as_object().unwrap().keys().map(String::as_str).collect();
        prop_assert_eq!(stored, sorted(keys));
        prop_assert_eq!(&json["sequence"], &json!(session.sequence));
        prop_assert_eq!(serde_json::from_value::<SessionInfo>(json).unwrap(), session.clone());
        prop_assert_eq!(
            session.is_resumable(),
            !session.session_id.is_empty() && session.sequence > 0
        );
    }

    #[test]
    fn legacy_session_blobs_keep_every_resumable_shard(
        sessions in prop::collection::btree_map(any::<u32>(), session_info(), 0..8),
        junk in prop::collection::btree_map(
            "[a-z-][a-z0-9]{0,4}|-[0-9]{1,3}",
            session_info(),
            0..3,
        ),
    ) {
        let mut blob = Map::new();
        for (shard, session) in &sessions {
            blob.insert(shard.to_string(), serde_json::to_value(session).unwrap());
        }
        for (key, session) in &junk {
            blob.insert(key.clone(), serde_json::to_value(session).unwrap());
        }
        let parsed = gateway::sessions::parse_legacy(&Value::Object(blob).to_string());
        let expected: HashMap<u32, SessionInfo> = sessions
            .into_iter()
            .filter(|(_, session)| session.is_resumable())
            .collect();
        prop_assert_eq!(parsed, expected);
    }

    #[test]
    fn legacy_session_parsing_never_panics(text in prop_oneof![
        "(?s).{0,64}",
        json_value().prop_map(|value| value.to_string()),
    ]) {
        for session in gateway::sessions::parse_legacy(&text).values() {
            prop_assert!(session.is_resumable());
        }
    }

    #[test]
    fn statuses_round_trip_as_json(
        shard in any::<u32>(),
        status in prop::sample::select(STATUS_LABELS.to_vec()),
        latency in any::<u64>(),
        at in datetime(),
    ) {
        let info = StatusInfo {
            shard,
            status: status.to_owned(),
            latency,
            last_ack: FormattedDateTime::from_offset(at),
        };
        let json = serde_json::to_value(&info).unwrap();
        let keys: Vec<&str> = json.as_object().unwrap().keys().map(String::as_str).collect();
        prop_assert_eq!(keys, vec!["last_ack", "latency", "shard", "status"]);
        prop_assert_eq!(&json["last_ack"], &json!(info.last_ack.format()));
        let back: StatusInfo = serde_json::from_value(json).unwrap();
        prop_assert_eq!(
            (back.shard, back.status, back.latency, back.last_ack),
            (info.shard, info.status, info.latency, info.last_ack)
        );
    }

    /// The session saved for the next start resumes before the first payload
    /// that was received but not yet published.
    #[test]
    fn saved_sessions_resume_before_unpublished_payloads(
        start in 0u64..1000,
        session in prop::option::of("[a-z0-9]{0,8}"),
        url in prop::option::of("wss://[a-z]{1,8}"),
        published in prop::collection::vec(any::<bool>(), 0..24),
    ) {
        let handle = ShardHandle::new(3, None, Some(start));
        handle.update(|s| {
            s.session_id = session.clone();
            s.resume_url = url.clone();
        });
        let tokens: Vec<SeqToken> = (0..published.len())
            .map(|i| handle.dispatch(start + 1 + i as u64).expect("token"))
            .collect();
        for (token, published) in tokens.iter().zip(&published) {
            if *published {
                handle.complete(*token);
            }
        }
        let sequence = match published.iter().position(|published| !published) {
            Some(first_pending) => start + first_pending as u64,
            None => start + published.len() as u64,
        };
        let expected = session
            .filter(|id| !id.is_empty() && sequence > 0)
            .map(|session_id| SessionInfo { session_id, sequence, resume_url: url });
        prop_assert_eq!(handle.session_info(), expected);
        prop_assert_eq!(handle.in_flight(), published.iter().filter(|p| !**p).count());
    }

    #[test]
    fn member_adds_count_as_joins_for_fifteen_minutes(
        joined_ms in 1_400_000_000_000i64..1_900_000_000_000,
        age_ms in prop_oneof![
            -60_000i64..RECENT_JOIN_MS * 2,
            Just(RECENT_JOIN_MS),
            Just(RECENT_JOIN_MS - 1),
        ],
    ) {
        let joined = Timestamp::from_micros(joined_ms * 1000).unwrap();
        let now_ms = joined_ms + age_ms;
        prop_assert_eq!(cache::joined_recently(Some(joined), now_ms), age_ms < RECENT_JOIN_MS);
        prop_assert!(!cache::joined_recently(None, now_ms));
    }

    #[test]
    fn typed_parsing_only_grows_with_the_state_config(
        kind in event_name(),
        low in state_config(),
        extra in any::<[bool; 7]>(),
    ) {
        let enabled = low.enabled || extra[0];
        let high = StateConfig {
            enabled,
            member: enabled && (low.member || extra[1]),
            message: enabled && (low.message || extra[2]),
            presence: enabled && (low.presence || extra[3]),
            emoji: enabled && (low.emoji || extra[4]),
            voice: enabled && (low.voice || extra[5]),
            old: enabled && (low.old || extra[6]),
            ..low.clone()
        };
        if runner::wants_typed(&kind, &low) {
            prop_assert!(runner::wants_typed(&kind, &high), "{} with {:?}", kind, high);
        }
        // Without the cache only READY and the guild join/leave log parse.
        let off = StateConfig::default();
        let minimal = matches!(kind.as_str(), "READY" | "GUILD_CREATE" | "GUILD_DELETE");
        prop_assert_eq!(runner::wants_typed(&kind, &off), minimal);
    }

    #[test]
    fn only_documented_close_codes_are_fatal(code in any::<u16>()) {
        // Discord's non-reconnectable gateway close codes:
        // https://discord.com/developers/docs/topics/opcodes-and-status-codes
        let fatal = [4004, 4010, 4011, 4012, 4013, 4014].contains(&code);
        prop_assert_eq!(runner::is_fatal_close(code), fatal);
    }
}
