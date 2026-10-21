//! Optional status messages posted to Discord channels (LOG_CHANNEL and
//! LOG_GUILD_CHANNEL).
//!
//! Messages go through one bounded queue that a single task drains, so a
//! reconnect storm cannot spawn an unbounded number of REST calls.
//!
//! Repeated invalid requests (401/403) count towards Discord's Cloudflare ban
//! limit, so a channel that rejects posts is disabled or paused:
//! - A channel that answers 401/403/404 with Discord's JSON error is disabled.
//!   After a 401 twilight refuses every request with the token, so the other
//!   channel is disabled on its next message without another request.
//! - twilight-http drops the status of an error response whose body is not
//!   Discord's JSON error (an HTML page from Cloudflare or a proxy), so such a
//!   response may be a 403 as well as a 502. Disabling the channel would lose
//!   it for good after a passing outage, so it is paused instead: its messages
//!   are dropped for a minute, and each further such response before a post
//!   succeeds again doubles the pause, up to an hour.
//!
//! Any other failure (a 5xx with a JSON body, a timeout) only loses that
//! message.

use crate::constants::{JOIN_COLOR, LEAVE_COLOR};

use std::{
    ops::RangeInclusive,
    sync::Arc,
    time::{Duration, Instant},
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
/// The first pause of a channel after an error response without a status.
const PAUSE_MIN: Duration = Duration::from_secs(60);
/// The longest pause. A channel that answers every post with a 403 page then
/// costs one request an hour, far from the ban limit (10,000 per 10 minutes).
const PAUSE_MAX: Duration = Duration::from_secs(60 * 60);

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

/// What the posting task knows about one log channel.
#[derive(Debug, Default)]
struct ChannelState {
    /// Set after a permanent error: nothing is posted any more.
    disabled: bool,
    /// Messages are dropped until then.
    paused_until: Option<Instant>,
    /// The next pause; a successful post resets it.
    backoff: Duration,
}

impl ChannelState {
    /// Whether a message may be posted now.
    fn accepts(&self, now: Instant) -> bool {
        !self.disabled && self.paused_until.is_none_or(|until| now >= until)
    }

    fn posted(&mut self) {
        self.paused_until = None;
        self.backoff = Duration::ZERO;
    }

    /// Pause after an error response without a status; returns for how long.
    fn pause(&mut self, now: Instant, pauses: &RangeInclusive<Duration>) -> Duration {
        let pause = self.backoff.clamp(*pauses.start(), *pauses.end());
        self.backoff = pause * 2;
        self.paused_until = Some(now + pause);
        pause
    }
}

/// How a failed post affects its channel.
#[derive(Debug, Eq, PartialEq)]
enum Failure {
    /// Will not go away by retrying the same channel: disable it.
    Permanent,
    /// An error response whose status is unknown: pause the channel.
    Unknown,
    /// Only this message is lost.
    Transient,
}

impl DiscordLog {
    /// A logger that drops everything (used when both channels are unset, and in tests).
    pub fn disabled() -> Self {
        Self { tx: None }
    }

    pub fn new(client: Arc<Client>, log_channel: u64, log_guild_channel: u64) -> Self {
        Self::with_pauses(
            client,
            log_channel,
            log_guild_channel,
            PAUSE_MIN..=PAUSE_MAX,
        )
    }

    /// [`DiscordLog::new`] with other bounds for the pause after an error
    /// response without a status, so a test can see a pause end without
    /// waiting a minute. Only for tests.
    #[doc(hidden)]
    pub fn with_pauses(
        client: Arc<Client>,
        log_channel: u64,
        log_guild_channel: u64,
        pauses: RangeInclusive<Duration>,
    ) -> Self {
        let shards = Id::<ChannelMarker>::new_checked(log_channel);
        let guilds = Id::<ChannelMarker>::new_checked(log_guild_channel);
        if shards.is_none() && guilds.is_none() {
            return Self::disabled();
        }

        let (tx, mut rx) = mpsc::channel::<Post>(QUEUE);
        tokio::spawn(async move {
            let mut shards = shards.map(|id| (id, ChannelState::default()));
            let mut guilds = guilds.map(|id| (id, ChannelState::default()));
            while let Some(post) = rx.recv().await {
                let target = match post.target {
                    Target::Shards => &mut shards,
                    Target::Guilds => &mut guilds,
                };
                let Some((channel, state)) = target else {
                    continue;
                };
                if !state.accepts(Instant::now()) {
                    continue;
                }
                let embeds = [post.embed];
                let err = match client.create_message(*channel).embeds(&embeds).await {
                    Ok(_) => {
                        state.posted();
                        continue;
                    }
                    Err(err) => err,
                };
                match classify(err.kind()) {
                    Failure::Permanent => {
                        error!(
                            "Disabling Discord log channel {} after a permanent error: {}",
                            channel, err
                        );
                        state.disabled = true;
                    }
                    Failure::Unknown => {
                        let pause = state.pause(Instant::now(), &pauses);
                        warn!(
                            "Pausing Discord log channel {} for {:?} after an error: {}",
                            channel, pause, err
                        );
                    }
                    Failure::Transient => warn!("Failed to post message to Discord: {}", err),
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

fn classify(kind: &ErrorType) -> Failure {
    match kind {
        ErrorType::Unauthorized => Failure::Permanent,
        ErrorType::Response { status, .. } if matches!(status.get(), 401 | 403 | 404) => {
            Failure::Permanent
        }
        // Only an error response (neither 2xx nor 429) whose body is not
        // Discord's JSON error: twilight keeps its body but not its status.
        ErrorType::Parsing { .. } => Failure::Unknown,
        _ => Failure::Transient,
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
        assert_eq!(classify(&ErrorType::Unauthorized), Failure::Permanent);
        assert_eq!(classify(&ErrorType::RequestTimedOut), Failure::Transient);
        assert_eq!(classify(&ErrorType::RequestError), Failure::Transient);
    }

    #[test]
    fn error_responses_without_a_json_body_have_no_status() {
        let html = ErrorType::Parsing {
            body: b"<html>Access denied</html>".to_vec(),
        };
        assert_eq!(classify(&html), Failure::Unknown);
    }

    #[test]
    fn pauses_double_up_to_an_hour_until_a_post_succeeds() {
        let pauses = PAUSE_MIN..=PAUSE_MAX;
        let start = Instant::now();
        let mut state = ChannelState::default();
        assert!(state.accepts(start));

        assert_eq!(state.pause(start, &pauses), PAUSE_MIN);
        assert!(!state.accepts(start));
        assert!(!state.accepts(start + PAUSE_MIN - Duration::from_millis(1)));
        assert!(state.accepts(start + PAUSE_MIN));

        let mut now = start + PAUSE_MIN;
        let mut minutes = Vec::new();
        for _ in 0..8 {
            let pause = state.pause(now, &pauses);
            minutes.push(pause.as_secs() / 60);
            assert!(!state.accepts(now));
            now += pause;
            assert!(state.accepts(now));
        }
        assert_eq!(minutes, [2, 4, 8, 16, 32, 60, 60, 60]);

        state.posted();
        assert!(state.accepts(now));
        assert_eq!(
            state.pause(now, &pauses),
            PAUSE_MIN,
            "a successful post resets it"
        );
    }

    #[test]
    fn a_disabled_channel_never_accepts() {
        let now = Instant::now();
        let state = ChannelState {
            disabled: true,
            ..ChannelState::default()
        };
        assert!(!state.accepts(now));
        assert!(!state.accepts(now + PAUSE_MAX * 24));
    }
}
