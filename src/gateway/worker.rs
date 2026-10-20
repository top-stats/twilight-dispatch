//! Ordered workers: apply events to the cache, then publish them.
//!
//! Every event of one guild is sent to the same worker (see
//! [`super::payload::Route`]), so cache updates and publishes of a guild happen
//! in gateway order, and a consumer that reads the cache after receiving an
//! event sees that event applied. Different guilds are processed in parallel.

use super::{payload, registry::Registry, seq::SeqToken};
use crate::{
    cache::{self, Outcome, UpdateContext},
    config::StateConfig,
    discord_log::DiscordLog,
    metrics::{GUILD_EVENTS, STATE_UPDATE_ERRORS},
    models::{ApiError, ApiResult, FormattedDateTime},
    util::Throttle,
};

use redis::aio::ConnectionLike;
use serde_json::Value;
use std::{
    future::Future,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{sync::mpsc, task::JoinHandle};
use tracing::warn;
use twilight_model::{
    gateway::{event::Event, payload::incoming::GuildCreate},
    id::{marker::UserMarker, Id},
};

/// Per-worker queue length. A full queue makes the shard runner wait.
pub const QUEUE: usize = 1024;
/// How long cache updates are skipped after one failed.
pub const BREAKER_COOLDOWN: Duration = Duration::from_secs(1);

/// One dispatch payload to cache and publish.
#[derive(Debug)]
pub struct Job {
    pub shard: u32,
    pub seq: Option<u64>,
    pub token: Option<SeqToken>,
    /// The gateway session the payload belongs to.
    pub session: Option<Arc<str>>,
    /// The raw payload text as received from Discord.
    pub json: String,
    /// The typed event, when the cache (or the guild log) needs it.
    pub event: Option<Event>,
}

/// Where workers publish to (the broker in production, a recorder in tests).
pub trait Publisher: Send + Sync + 'static {
    fn publish(
        &self,
        routing_key: &str,
        payload: Vec<u8>,
        message_id: Option<String>,
    ) -> impl Future<Output = ApiResult<()>> + Send;
}

impl Publisher for crate::amqp::Amqp {
    async fn publish(
        &self,
        routing_key: &str,
        payload: Vec<u8>,
        message_id: Option<String>,
    ) -> ApiResult<()> {
        let mut properties = lapin::BasicProperties::default();
        if let Some(id) = message_id {
            properties = properties.with_message_id(id.into());
        }
        crate::amqp::Amqp::publish(self, routing_key, &payload, properties).await
    }
}

/// Skips cache updates for a short time after one failed, so a Redis outage
/// does not make every event wait for a timeout.
#[derive(Debug, Default)]
pub struct Breaker {
    open_until: Mutex<Option<Instant>>,
}

impl Breaker {
    pub fn allow(&self) -> bool {
        let Ok(open_until) = self.open_until.lock() else {
            return true;
        };
        open_until.is_none_or(|until| Instant::now() >= until)
    }

    pub fn failure(&self) {
        if let Ok(mut open_until) = self.open_until.lock() {
            *open_until = Some(Instant::now() + BREAKER_COOLDOWN);
        }
    }

    pub fn success(&self) {
        if let Ok(mut open_until) = self.open_until.lock() {
            *open_until = None;
        }
    }
}

pub struct WorkerContext<C, P> {
    pub redis: C,
    pub publisher: Arc<P>,
    pub registry: Arc<Registry>,
    pub state: StateConfig,
    pub bot_id: Id<UserMarker>,
    pub cache_timeout: Duration,
    pub discord: DiscordLog,
    pub breaker: Breaker,
    pub cache_errors: Throttle,
    pub publish_errors: Throttle,
}

/// Start `count` workers. Returns their queues and join handles; the workers
/// stop once every sender is dropped and their queues are drained.
pub fn spawn<C, P>(
    count: usize,
    ctx: Arc<WorkerContext<C, P>>,
) -> (Vec<mpsc::Sender<Job>>, Vec<JoinHandle<()>>)
where
    C: ConnectionLike + Clone + Send + Sync + 'static,
    P: Publisher,
{
    (0..count.max(1))
        .map(|_| {
            let (tx, mut rx) = mpsc::channel::<Job>(QUEUE);
            let ctx = ctx.clone();
            let handle = tokio::spawn(async move {
                let mut conn = ctx.redis.clone();
                while let Some(job) = rx.recv().await {
                    process(&ctx, &mut conn, job).await;
                }
            });
            (tx, handle)
        })
        .unzip()
}

/// Cache and publish one job.
pub async fn process<C, P>(ctx: &WorkerContext<C, P>, conn: &mut C, job: Job)
where
    C: ConnectionLike + Send,
    P: Publisher,
{
    let Job {
        shard,
        seq,
        token,
        session,
        json,
        event,
    } = job;

    // Jobs of a previous session still in the queue would undo what READY
    // of the new session reconciled; that session re-sends the current state.
    let current = match (token, ctx.registry.get(shard)) {
        (Some(token), Some(handle)) => handle.is_current(token),
        _ => true,
    };
    let mut outcome = None;
    if let Some(mut event) = event {
        if ctx.state.enabled && current {
            outcome = update_cache(ctx, conn, &mut event).await;
        }
        log_guild_changes(ctx, &event, outcome.as_ref());
    }

    let message_id = seq.map(|seq| match &session {
        Some(session) => format!("{shard}:{session}:{seq}"),
        None => format!("{shard}::{seq}"),
    });
    let published = publish(ctx, shard, message_id, &json, outcome.and_then(|o| o.old)).await;

    if let (Some(token), Some(handle)) = (token, ctx.registry.get(shard)) {
        if published {
            handle.complete(token);
        } else {
            // Keep the resume point before it so a restart replays it.
            handle.fail(token);
        }
    }
}

/// Whether a cache error means Redis itself is unreachable or too slow (so
/// skipping updates for a moment helps) rather than a problem with one event.
fn is_connection_error(err: &ApiError) -> bool {
    match err {
        ApiError::Redis(err) => {
            err.is_io_error()
                || err.is_timeout()
                || err.is_connection_dropped()
                || err.is_connection_refusal()
        }
        _ => false,
    }
}

async fn update_cache<C, P>(
    ctx: &WorkerContext<C, P>,
    conn: &mut C,
    event: &mut Event,
) -> Option<Outcome>
where
    C: ConnectionLike + Send,
    P: Publisher,
{
    if !ctx.breaker.allow() {
        STATE_UPDATE_ERRORS.with_label_values(&["skipped"]).inc();
        return None;
    }

    let update_ctx = UpdateContext {
        state: &ctx.state,
        bot_id: Some(ctx.bot_id),
        now_ms: FormattedDateTime::now().unix_millis(),
    };
    match tokio::time::timeout(ctx.cache_timeout, cache::update(conn, event, &update_ctx)).await {
        Ok(Ok(outcome)) => {
            ctx.breaker.success();
            Some(outcome)
        }
        Ok(Err(err)) => {
            if is_connection_error(&err) {
                ctx.breaker.failure();
            }
            STATE_UPDATE_ERRORS.with_label_values(&["error"]).inc();
            if let Some(suppressed) = ctx.cache_errors.check() {
                warn!(
                    "Failed to update state for {:?}: {} ({} similar errors suppressed)",
                    event.kind(),
                    err,
                    suppressed
                );
            }
            None
        }
        Err(_) => {
            ctx.breaker.failure();
            STATE_UPDATE_ERRORS.with_label_values(&["timeout"]).inc();
            if let Some(suppressed) = ctx.cache_errors.check() {
                warn!(
                    "State update for {:?} timed out after {:?} ({} similar errors suppressed)",
                    event.kind(),
                    ctx.cache_timeout,
                    suppressed
                );
            }
            None
        }
    }
}

fn guild_name(value: Option<&Value>) -> Option<&str> {
    value?.get("name")?.as_str()
}

/// Guild join / leave logs and metrics.
///
/// A join is reported when the cache update succeeded and the guild was not
/// known before. Without the cache (STATE_ENABLED=false) a GUILD_CREATE is a
/// join when the bot's `joined_at` is recent. An outage (GUILD_DELETE
/// unavailable) is not a leave.
fn log_guild_changes<C, P>(ctx: &WorkerContext<C, P>, event: &Event, outcome: Option<&Outcome>) {
    match event {
        Event::GuildCreate(guild) => {
            let GuildCreate::Available(guild) = guild.as_ref() else {
                return;
            };
            let joined = match outcome {
                Some(outcome) => !outcome.guild_known,
                None if !ctx.state.enabled => {
                    cache::joined_recently(guild.joined_at, FormattedDateTime::now().unix_millis())
                }
                None => false,
            };
            if joined {
                GUILD_EVENTS.with_label_values(&["Join"]).inc();
                ctx.discord.guild_join(&guild.name, guild.id.get());
            }
        }
        Event::GuildDelete(data) if data.unavailable != Some(true) => {
            if ctx.state.enabled && outcome.is_none() {
                return;
            }
            GUILD_EVENTS.with_label_values(&["Leave"]).inc();
            let name =
                guild_name(outcome.and_then(|o| o.previous_guild.as_ref())).unwrap_or("Unknown");
            ctx.discord.guild_leave(name, data.id.get());
        }
        _ => {}
    }
}

/// Publish a payload. Returns false only when the broker did not take it
/// (payloads that cannot be published at all are logged and skipped).
async fn publish<C, P>(
    ctx: &WorkerContext<C, P>,
    shard: u32,
    message_id: Option<String>,
    json: &str,
    old: Option<Value>,
) -> bool
where
    P: Publisher,
{
    let envelope = match payload::Envelope::parse(json) {
        Ok(envelope) => envelope,
        Err(err) => {
            warn!("[Shard {}] Could not decode payload: {}", shard, err);
            return true;
        }
    };
    let Some(kind) = envelope.kind().map(ToOwned::to_owned) else {
        return true;
    };
    let body = match payload::outgoing(&envelope, old.as_ref()) {
        Ok(body) => body,
        Err(err) => {
            warn!("[Shard {}] Failed to serialize payload: {}", shard, err);
            return true;
        }
    };
    match ctx.publisher.publish(&kind, body, message_id).await {
        Ok(()) => true,
        Err(err) => {
            if let Some(suppressed) = ctx.publish_errors.check() {
                warn!(
                    "[Shard {}] Failed to publish event: {} ({} similar errors suppressed)",
                    shard, err, suppressed
                );
            }
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breaker_opens_after_failure_and_closes_on_success() {
        let breaker = Breaker::default();
        assert!(breaker.allow());
        breaker.failure();
        assert!(!breaker.allow());
        breaker.success();
        assert!(breaker.allow());
    }

    #[test]
    fn guild_names() {
        let value = serde_json::json!({"name": "g"});
        assert_eq!(guild_name(Some(&value)), Some("g"));
        assert_eq!(guild_name(Some(&serde_json::json!({"id": "1"}))), None);
        assert_eq!(guild_name(None), None);
    }
}
