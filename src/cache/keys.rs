//! Pure key planning for the Redis state cache.
//!
//! Logical keys are what the rest of the code builds with the helpers in
//! [`crate::constants`] (for example `channel:<guild>:<channel>`). Stored keys
//! are what actually lives in Redis. The two only differ for guild channels,
//! which are stored as `channel:<channel>` so consumers can look a channel up
//! without knowing its guild. Every read, write and delete must go through
//! [`plan`] / [`stored_key`] so the two never drift apart again.

use crate::constants::{CHANNEL_KEY, GUILD_KEY, KEYS_SUFFIX, MESSAGE_KEY, THREAD_KEY};

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
/// - `<prefix>:<guild>:<id>` is also indexed in `guild_keys:<guild>`, except
///   messages, which are indexed in `channel_keys:<channel>`.
/// - Keys without a `:` (for example `bot_user`) are not indexed.
pub fn plan(key: &str) -> KeyPlan {
    let stored = stored_key(key);
    let parts: Vec<&str> = key.split(':').collect();
    let mut indexes = Vec::with_capacity(2);

    if parts.len() > 1 {
        indexes.push((format!("{}{}", parts[0], KEYS_SUFFIX), stored.clone()));
    }

    if parts.len() > 2 {
        let parent = if parts[0] == MESSAGE_KEY {
            CHANNEL_KEY
        } else {
            GUILD_KEY
        };
        indexes.push((
            format!("{parent}{KEYS_SUFFIX}:{}", parts[1]),
            stored.clone(),
        ));
        // Threads are also indexed per guild on their own, so finding a
        // guild's threads does not mean reading every key of the guild.
        if parts[0] == THREAD_KEY {
            indexes.push((thread_index(parts[1]), stored.clone()));
        }
    }

    KeyPlan { stored, indexes }
}

/// `guild_keys:<guild>`: every child key of a guild.
pub fn guild_index(guild_id: u64) -> String {
    format!("{GUILD_KEY}{KEYS_SUFFIX}:{guild_id}")
}

/// `thread_keys:<guild>`: every cached thread of a guild.
pub fn thread_index(guild_id: impl std::fmt::Display) -> String {
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
pub fn channel_index(channel_id: u64) -> String {
    format!("{CHANNEL_KEY}{KEYS_SUFFIX}:{channel_id}")
}

/// `<prefix>_keys`: every key of one type.
pub fn type_index(prefix: &str) -> String {
    format!("{prefix}{KEYS_SUFFIX}")
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
