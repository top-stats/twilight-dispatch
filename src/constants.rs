use twilight_model::id::{
    marker::{ChannelMarker, EmojiMarker, GuildMarker, MessageMarker, RoleMarker, UserMarker},
    Id,
};

pub const EXCHANGE: &str = "gateway";
pub const QUEUE_RECV: &str = "gateway.recv";
pub const QUEUE_SEND: &str = "gateway.send";
/// Direct exchange that routes gateway commands to the process running the
/// shard (routing key: the shard id), for deployments with several processes.
pub const SHARD_EXCHANGE: &str = "gateway.send.shards";

/// Legacy blob of every shard's session, written by older versions. Read once
/// at startup as a fallback; new versions store one key per shard instead.
pub const SESSIONS_KEY: &str = "gateway_sessions";
pub const SESSION_KEY: &str = "gateway_session";
pub const STATUSES_KEY: &str = "gateway_statuses";
pub const STARTED_KEY: &str = "gateway_started";
pub const SHARDS_KEY: &str = "gateway_shards";
/// Identify turns shared by every process of the bot: one key per Discord
/// identify bucket (`shard % max_concurrency`), taken for one identify
/// interval by the process whose shard identifies (`gateway::queue`).
pub const IDENTIFY_KEY: &str = "gateway_identify";

pub const BOT_USER_KEY: &str = "bot_user";
pub const GUILD_KEY: &str = "guild";
pub const CHANNEL_KEY: &str = "channel";
pub const MESSAGE_KEY: &str = "message";
pub const ROLE_KEY: &str = "role";
pub const EMOJI_KEY: &str = "emoji";
pub const MEMBER_KEY: &str = "member";
pub const PRESENCE_KEY: &str = "presence";
pub const VOICE_KEY: &str = "voice";
/// Active threads. Earlier versions only had them inside the guild blob, so
/// they get their own prefix instead of `channel:` (consumers that build
/// channel objects from `channel:` keys do not expect thread types).
pub const THREAD_KEY: &str = "thread";

pub const KEYS_SUFFIX: &str = "_keys";
/// Legacy expiry hash (key -> formatted timestamp), migrated on startup.
pub const EXPIRY_KEYS: &str = "expiry_keys";
/// Expiry deadlines: sorted set of stored keys scored by unix milliseconds.
pub const EXPIRY_ZSET: &str = "expiry_zset";

pub const CACHE_JOB_INTERVAL_MS: u64 = 1000;
pub const METRICS_JOB_INTERVAL_MS: u64 = 1000;
/// Maximum number of expired keys deleted per Redis call.
pub const EXPIRY_BATCH: usize = 500;

pub const CONNECT_COLOR: u32 = 0x00FF00;
pub const DISCONNECT_COLOR: u32 = 0xFF0000;
pub const READY_COLOR: u32 = 0x00FF00;
pub const RESUME_COLOR: u32 = 0x1E90FF;
pub const JOIN_COLOR: u32 = 0x00FF00;
pub const LEAVE_COLOR: u32 = 0xFF0000;

pub fn session_key(shard: u32) -> String {
    format!("{SESSION_KEY}:{shard}")
}

pub fn identify_key(bucket: u32) -> String {
    format!("{IDENTIFY_KEY}:{bucket}")
}

pub fn guild_key(guild: Id<GuildMarker>) -> String {
    format!("{GUILD_KEY}:{guild}")
}

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

pub fn thread_key(guild: Id<GuildMarker>, thread: Id<ChannelMarker>) -> String {
    format!("{THREAD_KEY}:{guild}:{thread}")
}

pub fn message_key(channel: Id<ChannelMarker>, message: Id<MessageMarker>) -> String {
    format!("{MESSAGE_KEY}:{channel}:{message}")
}

pub fn role_key(guild: Id<GuildMarker>, role: Id<RoleMarker>) -> String {
    format!("{ROLE_KEY}:{guild}:{role}")
}

pub fn emoji_key(guild: Id<GuildMarker>, emoji: Id<EmojiMarker>) -> String {
    format!("{EMOJI_KEY}:{guild}:{emoji}")
}

pub fn member_key(guild: Id<GuildMarker>, member: Id<UserMarker>) -> String {
    format!("{MEMBER_KEY}:{guild}:{member}")
}

pub fn presence_key(guild: Id<GuildMarker>, member: Id<UserMarker>) -> String {
    format!("{PRESENCE_KEY}:{guild}:{member}")
}

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
