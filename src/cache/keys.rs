//! Pure key planning for the Redis state cache.
//!
//! Logical keys are what the rest of the code builds with the helpers in
//! [`crate::constants`] (for example `channel:<guild>:<channel>`). Stored keys
//! are what actually lives in Redis. The two only differ for guild channels,
//! which are stored as `channel:<channel>` so consumers can look a channel up
//! without knowing its guild. Every read, write and delete must go through
//! [`plan`] / [`stored_key`] so the two never drift apart again.

use crate::constants::{
    BOT_USER_KEY, CHANNEL_KEY, EMOJI_KEY, GUILD_KEY, KEYS_SUFFIX, MEMBER_KEY, MESSAGE_KEY,
    PRESENCE_KEY, ROLE_KEY, THREAD_KEY, VOICE_KEY,
};

use std::fmt::Display;

/// What the keys of a prefix belong to, which decides the index sets that
/// list them besides `<prefix>_keys`.
///
/// Removing a guild or a channel only walks its own index set, so a key
/// listed under the wrong parent is never removed (with a TTL of 0, never at
/// all). That is why the parent is spelled out per prefix in [`scope`]
/// instead of being guessed from the key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Scope {
    /// `guild:<id>` and `bot_user`: no parent.
    Global,
    /// `<prefix>:<guild>:<id>`: also in `guild_keys:<guild>`. Private
    /// channels (`channel:<id>`) have no guild segment and no parent.
    Guild,
    /// `message:<channel>:<id>`: also in `channel_keys:<channel>`.
    Channel,
    /// `thread:<guild>:<thread>`: also in `guild_keys:<guild>` and
    /// `thread_keys:<guild>`.
    Thread,
}

/// The scope of every key prefix the cache writes, `None` for anything else.
/// A new prefix has to be added here; a unit test checks that every `*_KEY`
/// constant is.
pub fn scope(prefix: &str) -> Option<Scope> {
    match prefix {
        GUILD_KEY | BOT_USER_KEY => Some(Scope::Global),
        CHANNEL_KEY | ROLE_KEY | EMOJI_KEY | MEMBER_KEY | PRESENCE_KEY | VOICE_KEY => {
            Some(Scope::Guild)
        }
        MESSAGE_KEY => Some(Scope::Channel),
        THREAD_KEY => Some(Scope::Thread),
        _ => None,
    }
}

/// Where a logical key is stored, and which index sets reference it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KeyPlan {
    pub stored: String,
    /// `(set, member)` pairs; the member is always the stored key.
    pub indexes: Vec<(String, String)>,
}

/// The key a logical key is stored under.
pub fn stored_key(key: &str) -> String {
    let mut parts = key.splitn(3, ':');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(CHANNEL_KEY), Some(_guild), Some(channel)) => format!("{CHANNEL_KEY}:{channel}"),
        _ => key.to_owned(),
    }
}

/// Compute the stored key and index-set memberships of a logical key.
///
/// - `<prefix>:...` is indexed in `<prefix>_keys`.
/// - `<prefix>:<parent>:<id>` is also indexed under its parent, as its
///   [`Scope`] says.
/// - Keys without a `:` (for example `bot_user`) are not indexed.
pub fn plan(key: &str) -> KeyPlan {
    let stored = stored_key(key);
    let parts: Vec<&str> = key.split(':').collect();
    let mut indexes = Vec::with_capacity(3);

    if parts.len() > 1 {
        indexes.push((type_index(parts[0]), stored.clone()));
    }

    if parts.len() > 2 {
        let parent = parts[1];
        match scope(parts[0]) {
            Some(Scope::Channel) => indexes.push((channel_index(parent), stored.clone())),
            Some(Scope::Thread) => {
                indexes.push((guild_index(parent), stored.clone()));
                // Also on their own, so finding a guild's threads does not
                // mean reading every key of the guild.
                indexes.push((thread_index(parent), stored.clone()));
            }
            Some(Scope::Guild) => indexes.push((guild_index(parent), stored.clone())),
            // The cache writes neither: global keys have no parent segment,
            // and every prefix it writes has a scope. Planning stays total
            // with the rule of earlier versions (the segment is a guild).
            Some(Scope::Global) | None => indexes.push((guild_index(parent), stored.clone())),
        }
    }

    KeyPlan { stored, indexes }
}

/// `guild_keys:<guild>`: every child key of a guild.
pub fn guild_index(guild_id: impl Display) -> String {
    format!("{GUILD_KEY}{KEYS_SUFFIX}:{guild_id}")
}

/// `thread_keys:<guild>`: every cached thread of a guild.
pub fn thread_index(guild_id: impl Display) -> String {
    format!("{THREAD_KEY}{KEYS_SUFFIX}:{guild_id}")
}

/// The thread id of a stored `thread:<guild>:<thread>` key.
pub fn thread_id_of(stored: &str) -> Option<u64> {
    let mut parts = stored.splitn(3, ':');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(THREAD_KEY), Some(_), Some(id)) => id.parse().ok(),
        _ => None,
    }
}

/// `channel_keys:<channel>`: every cached message of a channel.
pub fn channel_index(channel_id: impl Display) -> String {
    format!("{CHANNEL_KEY}{KEYS_SUFFIX}:{channel_id}")
}

/// `<prefix>_keys`: every key of one type.
pub fn type_index(prefix: &str) -> String {
    format!("{prefix}{KEYS_SUFFIX}")
}

/// Whether a stored key has the given prefix (`emoji` for `emoji:1:2`).
pub fn has_prefix(stored: &str, prefix: &str) -> bool {
    stored
        .split_once(':')
        .is_some_and(|(first, _)| first == prefix)
}

/// The channel id of a stored `channel:<id>` key.
pub fn channel_id_of(stored: &str) -> Option<u64> {
    let (prefix, id) = stored.split_once(':')?;
    if prefix != CHANNEL_KEY || id.contains(':') {
        return None;
    }
    id.parse().ok()
}

/// The guild id of a stored `guild:<id>` key.
pub fn guild_id_of(stored: &str) -> Option<u64> {
    let (prefix, id) = stored.split_once(':')?;
    if prefix != GUILD_KEY || id.contains(':') {
        return None;
    }
    id.parse().ok()
}

/// Which shard a guild lives on (Discord's sharding formula).
pub const fn shard_for_guild(guild_id: u64, shard_total: u32) -> u32 {
    ((guild_id >> 22) % shard_total as u64) as u32
}

/// Cached guilds that belong to `shard` but are absent from its READY.
///
/// After a fresh IDENTIFY, READY lists every guild of the shard, so anything
/// else cached for that shard was left while we were disconnected.
pub fn stale_guilds(
    cached: impl IntoIterator<Item = u64>,
    ready: &std::collections::HashSet<u64>,
    shard: u32,
    shard_total: u32,
) -> Vec<u64> {
    let mut stale: Vec<u64> = cached
        .into_iter()
        .filter(|id| shard_for_guild(*id, shard_total) == shard && !ready.contains(id))
        .collect();
    stale.sort_unstable();
    stale.dedup();
    stale
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn idx(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
            .collect()
    }

    #[test]
    fn guild_channel_is_stored_without_guild() {
        assert_eq!(stored_key("channel:1:2"), "channel:2");
        assert_eq!(
            plan("channel:1:2"),
            KeyPlan {
                stored: "channel:2".into(),
                indexes: idx(&[("channel_keys", "channel:2"), ("guild_keys:1", "channel:2")]),
            }
        );
    }

    #[test]
    fn private_channel_is_only_type_indexed() {
        assert_eq!(stored_key("channel:2"), "channel:2");
        assert_eq!(
            plan("channel:2").indexes,
            idx(&[("channel_keys", "channel:2")])
        );
    }

    #[test]
    fn message_is_indexed_under_its_channel() {
        assert_eq!(
            plan("message:5:6"),
            KeyPlan {
                stored: "message:5:6".into(),
                indexes: idx(&[
                    ("message_keys", "message:5:6"),
                    ("channel_keys:5", "message:5:6")
                ]),
            }
        );
    }

    #[test]
    fn guild_children_are_indexed_under_their_guild() {
        for prefix in ["role", "emoji", "member", "presence", "voice"] {
            let key = format!("{prefix}:1:3");
            assert_eq!(
                plan(&key).indexes,
                vec![
                    (format!("{prefix}_keys"), key.clone()),
                    ("guild_keys:1".to_owned(), key.clone()),
                ]
            );
        }
    }

    #[test]
    fn threads_are_indexed_per_guild_twice() {
        assert_eq!(
            plan("thread:1:9").indexes,
            idx(&[
                ("thread_keys", "thread:1:9"),
                ("guild_keys:1", "thread:1:9"),
                ("thread_keys:1", "thread:1:9"),
            ])
        );
        assert_eq!(thread_id_of("thread:1:9"), Some(9));
        assert_eq!(thread_id_of("channel:9"), None);
    }

    #[test]
    fn guild_is_only_type_indexed() {
        assert_eq!(plan("guild:1").indexes, idx(&[("guild_keys", "guild:1")]));
    }

    #[test]
    fn unprefixed_keys_are_not_indexed() {
        for key in ["bot_user", "gateway_statuses", "gateway_started"] {
            let plan = plan(key);
            assert_eq!(plan.stored, key);
            assert!(plan.indexes.is_empty());
        }
    }

    #[test]
    fn id_extraction() {
        assert_eq!(channel_id_of("channel:42"), Some(42));
        assert_eq!(channel_id_of("channel:1:42"), None);
        assert_eq!(channel_id_of("role:1:2"), None);
        assert_eq!(guild_id_of("guild:7"), Some(7));
        assert_eq!(guild_id_of("guild_keys:7"), None);
        assert_eq!(guild_id_of("guild:x"), None);
        assert!(has_prefix("emoji:1:2", "emoji"));
        assert!(!has_prefix("emoji_keys:1", "emoji"));
        assert!(!has_prefix("emoji", "emoji"));
    }

    /// Every `pub const <NAME>_KEY: &str` of `constants.rs` as (name, value).
    fn key_constants() -> Vec<(&'static str, &'static str)> {
        include_str!("../constants.rs")
            .lines()
            .filter_map(|line| {
                let (name, value) = line.strip_prefix("pub const ")?.split_once(": &str = ")?;
                let value = value.strip_prefix('"')?.strip_suffix("\";")?;
                name.ends_with("_KEY").then_some((name, value))
            })
            .collect()
    }

    /// Gateway bookkeeping keys, written with plain commands, never planned.
    const NOT_CACHED: [&str; 5] = [
        crate::constants::SESSIONS_KEY,
        crate::constants::SESSION_KEY,
        crate::constants::STATUSES_KEY,
        crate::constants::STARTED_KEY,
        crate::constants::SHARDS_KEY,
    ];

    /// A new cache prefix must say what it belongs to (and be counted), or
    /// its keys would be indexed under a guild that is not theirs and leak.
    #[test]
    fn every_cache_key_prefix_has_a_scope_and_a_gauge() {
        let constants = key_constants();
        assert!(constants.len() > NOT_CACHED.len(), "{constants:?}");
        for (name, prefix) in constants {
            if NOT_CACHED.contains(&prefix) {
                continue;
            }
            let Some(kind) = scope(prefix) else {
                panic!("{name} has no cache::keys::scope");
            };
            let (key, sets) = match kind {
                Scope::Global => (format!("{prefix}:1"), vec![format!("{prefix}_keys")]),
                Scope::Guild => (
                    format!("{prefix}:1:2"),
                    vec![format!("{prefix}_keys"), "guild_keys:1".into()],
                ),
                Scope::Channel => (
                    format!("{prefix}:1:2"),
                    vec![format!("{prefix}_keys"), "channel_keys:1".into()],
                ),
                Scope::Thread => (
                    format!("{prefix}:1:2"),
                    vec![
                        format!("{prefix}_keys"),
                        "guild_keys:1".into(),
                        "thread_keys:1".into(),
                    ],
                ),
            };
            let planned: Vec<String> = plan(&key).indexes.into_iter().map(|(set, _)| set).collect();
            assert_eq!(planned, sets, "{name}");

            // bot_user is a single key; every other type has a state gauge.
            let counted = crate::metrics::STATE_GAUGES
                .iter()
                .any(|(counted, _)| *counted == prefix);
            assert_eq!(
                counted,
                prefix != BOT_USER_KEY,
                "{name} and metrics::STATE_GAUGES"
            );
        }
    }

    #[test]
    fn unknown_prefixes_keep_the_old_guild_rule() {
        assert_eq!(scope("reaction"), None);
        assert_eq!(
            plan("reaction:1:2").indexes,
            idx(&[
                ("reaction_keys", "reaction:1:2"),
                ("guild_keys:1", "reaction:1:2")
            ])
        );
    }

    /// Keys are built with the helpers in `constants` (and stored keys and
    /// index names here), so the layout is defined in one place. Unit tests
    /// may spell keys out: they pin the format.
    #[test]
    fn keys_are_not_built_by_hand() {
        fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    rust_files(&path, out);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    out.push(path);
                }
            }
        }
        // `"<prefix>:` for a cache prefix, `"<word>:{` for any other key-like
        // format string and `{<NAME>_KEY}:` for a key built with a constant.
        let looks_like_key = |line: &str| {
            line.contains("_KEY}:")
                || line.split('"').skip(1).step_by(2).any(|literal| {
                    let Some((word, rest)) = literal.split_once(':') else {
                        return false;
                    };
                    !word.is_empty()
                        && word.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
                        && (scope(word).is_some() || rest.starts_with('{'))
                })
        };

        let mut files = Vec::new();
        rust_files(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut files,
        );
        let mut found = Vec::new();
        for file in files {
            if file.ends_with("constants.rs") || file.ends_with("keys.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&file).unwrap();
            let code = text.split("#[cfg(test)]").next().unwrap_or_default();
            for (number, line) in code.lines().enumerate() {
                if !line.trim_start().starts_with("//") && looks_like_key(line) {
                    found.push(format!(
                        "{}:{}: {}",
                        file.display(),
                        number + 1,
                        line.trim()
                    ));
                }
            }
        }
        assert!(found.is_empty(), "{found:#?}");
    }

    #[test]
    fn sharding_formula_matches_discord_docs() {
        // Example from https://discord.com/developers/docs/events/gateway#sharding
        let guild = 197_038_439_483_310_086;
        assert_eq!(shard_for_guild(guild, 1), 0);
        assert_eq!(shard_for_guild(guild, 16), ((guild >> 22) % 16) as u32);
    }

    #[test]
    fn stale_guilds_only_considers_this_shard() {
        let total = 4;
        let on = |shard: u32| -> Vec<u64> {
            (1u64..)
                .map(|n| n << 22)
                .filter(|id| shard_for_guild(*id, total) == shard)
                .take(3)
                .collect()
        };
        let mine = on(1);
        let other = on(2);
        let ready: HashSet<u64> = [mine[0]].into_iter().collect();

        let cached = mine.iter().chain(other.iter()).copied().chain([mine[1]]);
        assert_eq!(
            stale_guilds(cached, &ready, 1, total),
            vec![mine[1], mine[2]]
        );
    }
}
