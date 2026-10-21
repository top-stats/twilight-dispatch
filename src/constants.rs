//! Names shared with the other services: the AMQP exchanges and queues, the
//! Redis keys and key prefixes, and the embed colours of the Discord log
//! channels.
//!
//! Every value here is a contract with the consumers of the broker and the
//! cache, so changing one breaks them. Cache keys are only built with the
//! helpers below (`cache::keys` derives the stored keys and index sets from
//! them), so the key layout is defined in one place.

use twilight_model::id::{
    marker::{ChannelMarker, EmojiMarker, GuildMarker, MessageMarker, RoleMarker, UserMarker},
    Id,
};

/// Durable topic exchange every dispatch event is published to, with the
/// event name (for example `MESSAGE_CREATE`) as the routing key.
pub const EXCHANGE: &str = "gateway";
/// Durable queue bound to [`EXCHANGE`] with `#` (every event). Declared only
/// with `DEFAULT_QUEUE=true` (the default).
pub const QUEUE_RECV: &str = "gateway.recv";
/// Durable queue of gateway commands (send a payload to a shard, or reconnect
/// it), consumed by every process of the bot.
pub const QUEUE_SEND: &str = "gateway.send";
/// Direct exchange that routes gateway commands to the process running the
/// shard (routing key: the shard id), for deployments with several processes.
pub const SHARD_EXCHANGE: &str = "gateway.send.shards";

/// Every shard's session in one blob, the format older versions read and
/// wrote. This version still merges its own shards into it
/// (`gateway::sessions::save`, every job tick and on shutdown) for rollbacks
/// and existing consumers, and reads it at startup as a fallback for the
/// per-shard [`SESSION_KEY`] keys.
pub const SESSIONS_KEY: &str = "gateway_sessions";
/// Prefix of the per-shard session keys, `gateway_session:<shard>` (see
/// [`session_key`]): the shard's resumable session as JSON, rewritten every
/// job tick and on shutdown with a TTL, and removed while the shard has none.
pub const SESSION_KEY: &str = "gateway_session";
/// JSON array with the status of each of this process' shards (state,
/// latency, last heartbeat ACK), rewritten every job tick.
pub const STATUSES_KEY: &str = "gateway_statuses";
/// When this process last started, as a JSON string (a formatted UTC
/// timestamp).
pub const STARTED_KEY: &str = "gateway_started";
/// The shard total of the last start, as a decimal string. Saved sessions are
/// only resumed when it equals the configured `SHARDS_TOTAL`, because a
/// session is only valid for the shard count it was created with.
pub const SHARDS_KEY: &str = "gateway_shards";
/// Identify turns shared by every process of the bot: one key per Discord
/// identify bucket (`shard % max_concurrency`), taken for one identify
/// interval by the process whose shard identifies (`gateway::queue`).
pub const IDENTIFY_KEY: &str = "gateway_identify";

/// The bot's own user, a single key without an id segment.
pub const BOT_USER_KEY: &str = "bot_user";
/// Prefix of guild keys, `guild:<guild>` (see [`guild_key`]).
pub const GUILD_KEY: &str = "guild";
/// Prefix of channel keys (see [`channel_key`]). Guild and private channels
/// are both stored as `channel:<channel>`.
pub const CHANNEL_KEY: &str = "channel";
/// Prefix of message keys, `message:<channel>:<message>` (see
/// [`message_key`]).
pub const MESSAGE_KEY: &str = "message";
/// Prefix of role keys, `role:<guild>:<role>` (see [`role_key`]).
pub const ROLE_KEY: &str = "role";
/// Prefix of emoji keys, `emoji:<guild>:<emoji>` (see [`emoji_key`]).
pub const EMOJI_KEY: &str = "emoji";
/// Prefix of member keys, `member:<guild>:<user>` (see [`member_key`]).
pub const MEMBER_KEY: &str = "member";
/// Prefix of presence keys, `presence:<guild>:<user>` (see
/// [`presence_key`]).
pub const PRESENCE_KEY: &str = "presence";
/// Prefix of voice state keys, `voice:<guild>:<user>` (see [`voice_key`]).
pub const VOICE_KEY: &str = "voice";
/// Active threads. Earlier versions only had them inside the guild blob, so
/// they get their own prefix instead of `channel:` (consumers that build
/// channel objects from `channel:` keys do not expect thread types).
pub const THREAD_KEY: &str = "thread";

/// Suffix of the index sets: `<prefix>_keys` lists every key of a prefix, and
/// `guild_keys:<guild>`, `channel_keys:<channel>` and `thread_keys:<guild>`
/// list the keys under a parent.
pub const KEYS_SUFFIX: &str = "_keys";
/// Legacy expiry hash (key -> formatted timestamp) of older versions,
/// migrated into [`EXPIRY_ZSET`] on startup and again about every minute
/// (`app::expiry_job`), while an older version may still be writing it.
pub const EXPIRY_KEYS: &str = "expiry_keys";
/// Expiry deadlines: sorted set of stored keys scored by unix milliseconds.
pub const EXPIRY_ZSET: &str = "expiry_zset";

/// Period of the cache and session jobs, in milliseconds: expiring keys, and
/// writing [`STATUSES_KEY`] and the shards' sessions.
pub const CACHE_JOB_INTERVAL_MS: u64 = 1000;
/// Period of the metrics job (gauges and the health state), in milliseconds.
pub const METRICS_JOB_INTERVAL_MS: u64 = 1000;
/// Maximum number of expired keys deleted per Redis call. Also the HSCAN
/// `COUNT` hint of the legacy expiry migration.
pub const EXPIRY_BATCH: usize = 500;

/// Embed colour (`0xRRGGBB`) of a shard's "Connected" log message: green.
pub const CONNECT_COLOR: u32 = 0x00_FF_00;
/// Embed colour (`0xRRGGBB`) of a shard's "Disconnected" and "Fatally
/// closed" log messages: red.
pub const DISCONNECT_COLOR: u32 = 0xFF_00_00;
/// Embed colour (`0xRRGGBB`) of a shard's "Ready" log message: green.
pub const READY_COLOR: u32 = 0x00_FF_00;
/// Embed colour (`0xRRGGBB`) of a shard's "Resumed" log message: blue.
pub const RESUME_COLOR: u32 = 0x1E_90_FF;
/// Embed colour (`0xRRGGBB`) of the "Guild Join" log message: green.
pub const JOIN_COLOR: u32 = 0x00_FF_00;
/// Embed colour (`0xRRGGBB`) of the "Guild Leave" log message: red.
pub const LEAVE_COLOR: u32 = 0xFF_00_00;

/// The session key of a shard, `gateway_session:<shard>`.
pub fn session_key(shard: u32) -> String {
    format!("{SESSION_KEY}:{shard}")
}

/// The identify turn key of a bucket, `gateway_identify:<bucket>`.
pub fn identify_key(bucket: u32) -> String {
    format!("{IDENTIFY_KEY}:{bucket}")
}

/// The key of a guild, `guild:<guild>`.
pub fn guild_key(guild: Id<GuildMarker>) -> String {
    format!("{GUILD_KEY}:{guild}")
}

/// The logical key of a channel: `channel:<guild>:<channel>` for a guild
/// channel, `channel:<channel>` for a private one (`guild` is `None`). Guild
/// channels are stored as `channel:<channel>` (`cache::keys::stored_key`).
pub fn channel_key(guild: Option<Id<GuildMarker>>, channel: Id<ChannelMarker>) -> String {
    if let Some(guild) = guild {
        guild_channel_key(guild, channel)
    } else {
        private_channel_key(channel)
    }
}

fn guild_channel_key(guild: Id<GuildMarker>, channel: Id<ChannelMarker>) -> String {
    format!("{CHANNEL_KEY}:{guild}:{channel}")
}

fn private_channel_key(channel: Id<ChannelMarker>) -> String {
    format!("{CHANNEL_KEY}:{channel}")
}

/// The key of an active thread, `thread:<guild>:<thread>`.
pub fn thread_key(guild: Id<GuildMarker>, thread: Id<ChannelMarker>) -> String {
    format!("{THREAD_KEY}:{guild}:{thread}")
}

/// The key of a message, `message:<channel>:<message>`.
pub fn message_key(channel: Id<ChannelMarker>, message: Id<MessageMarker>) -> String {
    format!("{MESSAGE_KEY}:{channel}:{message}")
}

/// The key of a role, `role:<guild>:<role>`.
pub fn role_key(guild: Id<GuildMarker>, role: Id<RoleMarker>) -> String {
    format!("{ROLE_KEY}:{guild}:{role}")
}

/// The key of an emoji, `emoji:<guild>:<emoji>`.
pub fn emoji_key(guild: Id<GuildMarker>, emoji: Id<EmojiMarker>) -> String {
    format!("{EMOJI_KEY}:{guild}:{emoji}")
}

/// The key of a guild member, `member:<guild>:<user>`.
pub fn member_key(guild: Id<GuildMarker>, member: Id<UserMarker>) -> String {
    format!("{MEMBER_KEY}:{guild}:{member}")
}

/// The key of a member's presence in a guild, `presence:<guild>:<user>`.
pub fn presence_key(guild: Id<GuildMarker>, member: Id<UserMarker>) -> String {
    format!("{PRESENCE_KEY}:{guild}:{member}")
}

/// The key of a member's voice state in a guild, `voice:<guild>:<user>`.
pub fn voice_key(guild: Id<GuildMarker>, member: Id<UserMarker>) -> String {
    format!("{VOICE_KEY}:{guild}:{member}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_formats_are_stable() {
        // These strings are a contract with every consumer of the cache.
        let (g, c, m, r, e, u) = (
            Id::new(1),
            Id::new(2),
            Id::new(3),
            Id::new(4),
            Id::new(5),
            Id::new(6),
        );
        assert_eq!(guild_key(g), "guild:1");
        assert_eq!(channel_key(Some(g), c), "channel:1:2");
        assert_eq!(channel_key(None, c), "channel:2");
        assert_eq!(message_key(c, m), "message:2:3");
        assert_eq!(role_key(g, r), "role:1:4");
        assert_eq!(emoji_key(g, e), "emoji:1:5");
        assert_eq!(member_key(g, u), "member:1:6");
        assert_eq!(presence_key(g, u), "presence:1:6");
        assert_eq!(voice_key(g, u), "voice:1:6");
        assert_eq!(session_key(7), "gateway_session:7");
        assert_eq!(identify_key(3), "gateway_identify:3");
        assert_eq!(thread_key(g, c), "thread:1:2");
    }
}
