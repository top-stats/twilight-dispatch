//! Optional status messages posted to Discord channels (LOG_CHANNEL and
//! LOG_GUILD_CHANNEL).
//!
//! Messages go through one bounded queue that a single task drains, so a
//! reconnect storm cannot spawn an unbounded number of REST calls. A channel
//! that answers 401/403/404 is disabled, because repeated invalid requests
//! count towards Discord's Cloudflare ban limit.

use crate::constants::{JOIN_COLOR, LEAVE_COLOR};

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tracing::{debug, error, warn};
use twilight_http::{error::ErrorType, Client};
use twilight_model::{
    channel::message::Embed,
    id::{marker::ChannelMarker, Id},
    util::Timestamp,
};

const QUEUE: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Target {
    Shards,
    Guilds,
}

struct Post {
    target: Target,
    embed: Embed,
}

#[derive(Clone)]
pub struct DiscordLog {
    tx: Option<mpsc::Sender<Post>>,
}

struct Channels {
    shards: Option<Id<ChannelMarker>>,
    guilds: Option<Id<ChannelMarker>>,
    shards_disabled: AtomicBool,
    guilds_disabled: AtomicBool,
}

impl DiscordLog {
    /// A logger that drops everything (used when both channels are unset, and in tests).
    pub fn disabled() -> Self {
        Self { tx: None }
    }

    pub fn new(client: Arc<Client>, log_channel: u64, log_guild_channel: u64) -> Self {
        let channels = Arc::new(Channels {
            shards: Id::new_checked(log_channel),
            guilds: Id::new_checked(log_guild_channel),
            shards_disabled: AtomicBool::new(false),
            guilds_disabled: AtomicBool::new(false),
        });
        if channels.shards.is_none() && channels.guilds.is_none() {
            return Self::disabled();
        }

        let (tx, mut rx) = mpsc::channel::<Post>(QUEUE);
        tokio::spawn(async move {
            while let Some(post) = rx.recv().await {
                let (channel, disabled) = match post.target {
                    Target::Shards => (channels.shards, &channels.shards_disabled),
                    Target::Guilds => (channels.guilds, &channels.guilds_disabled),
                };
                let Some(channel) = channel else { continue };
                if disabled.load(Ordering::Relaxed) {
                    continue;
                }
                let embeds = [post.embed];
                if let Err(err) = client.create_message(channel).embeds(&embeds).await {
                    if is_permanent(err.kind()) {
                        error!(
                            "Disabling Discord log channel {} after a permanent error: {}",
                            channel, err
                        );
                        disabled.store(true, Ordering::Relaxed);
                    } else {
                        warn!("Failed to post message to Discord: {}", err);
                    }
                }
            }
        });

        Self { tx: Some(tx) }
    }

    fn send(&self, target: Target, embed: Embed) {
        if let Some(tx) = &self.tx {
            if tx.try_send(Post { target, embed }).is_err() {
                debug!("Discord log queue is full, dropping a message");
            }
        }
    }

    /// A shard lifecycle message (Ready, Resumed, Disconnected, ...).
    pub fn shard(&self, color: u32, title: impl Into<String>) {
        self.send(
            Target::Shards,
            log_embed(color, Some(title.into()), None, now()),
        );
    }

    pub fn guild_join(&self, name: &str, id: u64) {
        self.send(
            Target::Guilds,
            log_embed(
                JOIN_COLOR,
                Some("Guild Join".into()),
                Some(format!("{name} ({id})")),
                now(),
            ),
        );
    }

    pub fn guild_leave(&self, name: &str, id: u64) {
        self.send(
            Target::Guilds,
            log_embed(
                LEAVE_COLOR,
                Some("Guild Leave".into()),
                Some(format!("{name} ({id})")),
                now(),
            ),
        );
    }
}

fn now() -> Option<Timestamp> {
    Timestamp::from_secs(OffsetDateTime::now_utc().unix_timestamp()).ok()
}

/// Errors that will not go away by retrying the same channel.
fn is_permanent(kind: &ErrorType) -> bool {
    match kind {
        ErrorType::Unauthorized => true,
        ErrorType::Response { status, .. } => matches!(status.get(), 401 | 403 | 404),
        _ => false,
    }
}

/// Build a log embed. Bot embeds are always of type "rich".
pub fn log_embed(
    color: u32,
    title: Option<String>,
    description: Option<String>,
    timestamp: Option<Timestamp>,
) -> Embed {
    Embed {
        author: None,
        color: Some(color),
        description,
        fields: Vec::new(),
        footer: None,
        image: None,
        kind: "rich".to_owned(),
        provider: None,
        thumbnail: None,
        timestamp,
        title,
        url: None,
        video: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embeds_are_rich() {
        let embed = log_embed(0xFF0000, Some("t".into()), Some("d".into()), None);
        let json = serde_json::to_value(&embed).unwrap();
        assert_eq!(json["type"], "rich");
        assert_eq!(json["color"], 0xFF0000);
        assert_eq!(json["title"], "t");
        assert_eq!(json["description"], "d");
    }

    #[test]
    fn disabled_logger_accepts_messages() {
        let log = DiscordLog::disabled();
        log.shard(0, "x");
        log.guild_join("g", 1);
        log.guild_leave("g", 1);
    }

    #[test]
    fn unauthorized_is_permanent() {
        assert!(is_permanent(&ErrorType::Unauthorized));
        assert!(!is_permanent(&ErrorType::RequestTimedOut));
    }
}
