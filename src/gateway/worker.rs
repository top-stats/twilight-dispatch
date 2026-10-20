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
    sync::{Arc, Mutex, MutexGuard, PoisonError},
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
///
/// After the cooldown one update probes Redis while the others keep skipping
/// until it ends. Every worker shares the breaker: if all of them probed, a
/// stalled Redis would block each one for the full timeout after every
/// cooldown, and with it every event queued behind it.
#[derive(Debug, Default)]
pub struct Breaker {
    state: Mutex<BreakerState>,
}

#[derive(Debug, Default)]
struct BreakerState {
    open_until: Option<Instant>,
    probing: bool,
}

impl BreakerState {
    /// `Some(probe)` when an update may start at `now`: every update while
    /// the breaker is closed, one probe once its cooldown is over.
    fn admits(&self, now: Instant) -> Option<bool> {
        match self.open_until {
            None => Some(false),
            Some(until) if now < until || self.probing => None,
            Some(_) => Some(true),
        }
    }
}

impl Breaker {
    /// Whether an update started now would go through.
    pub fn allow(&self) -> bool {
        self.state().admits(Instant::now()).is_some()
    }

    /// Let one update through, or `None` to skip it.
    pub fn permit(&self) -> Option<Permit<'_>> {
        self.permit_at(Instant::now())
    }

    fn permit_at(&self, now: Instant) -> Option<Permit<'_>> {
        let mut state = self.state();
        let probe = state.admits(now)?;
        state.probing |= probe;
        Some(Permit {
            breaker: self,
            probe,
        })
    }

    pub fn failure(&self) {
        self.state().open_until = Some(Instant::now() + BREAKER_COOLDOWN);
    }

    pub fn success(&self) {
        self.state().open_until = None;
    }

    fn state(&self) -> MutexGuard<'_, BreakerState> {
        // Nothing panics while the lock is held, so the state is consistent.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// An update the breaker let through. Dropping it ends a probe however the
/// update ended (an error that says nothing about the connection, or the
/// update being cancelled), so the next update probes again instead of the
/// breaker skipping every update from then on.
#[must_use]
pub struct Permit<'a> {
    breaker: &'a Breaker,
    probe: bool,
}

impl Permit<'_> {
    /// Redis answered: close the breaker.
    pub fn success(self) {
        self.breaker.success();
    }

    /// Redis is unreachable or too slow: skip updates for the cooldown. The
    /// breaker is reopened before the probe ends, so no other update can
    /// start a probe in between.
    pub fn failure(self) {
        self.breaker.failure();
    }
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        if self.probe {
            self.breaker.state().probing = false;
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
        if ctx.state.enabled && (current || only_removes(&event)) {
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

/// Events that only remove cached state. Nothing replays them after a new
/// session (GUILD_CREATE keeps the messages of channels it lists), and they
/// cannot bring back anything READY removed, so they are applied even when
/// they belong to a previous session.
///
/// MESSAGE_UPDATE is not applied across sessions although nothing replays it
/// either, so such an edit is missing from the cache until the message
/// expires (a known limitation). It reads the message and writes it back
/// later, while READY's reconcile runs in the shard runner, not in this
/// worker (`Runner::ready_cache` -> `cache::ready` -> `clear_guild`). If that
/// removes the guild in between, the write brings the message back, and with
/// STATE_MESSAGE_TTL=0 it never expires. Deletes have no such race.
pub fn only_removes(event: &Event) -> bool {
    match event {
        Event::MessageDelete(_)
        | Event::MessageDeleteBulk(_)
        | Event::ChannelDelete(_)
        | Event::ThreadDelete(_)
        | Event::RoleDelete(_) => true,
        // An outage marks the guild unavailable, which writes a stub.
        Event::GuildDelete(data) => data.unavailable != Some(true),
        _ => false,
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
    let Some(permit) = ctx.breaker.permit() else {
        STATE_UPDATE_ERRORS.with_label_values(&["skipped"]).inc();
        return None;
    };

    let update_ctx = UpdateContext {
        state: &ctx.state,
        bot_id: Some(ctx.bot_id),
        now_ms: FormattedDateTime::now().unix_millis(),
    };
    match tokio::time::timeout(ctx.cache_timeout, cache::update(conn, event, &update_ctx)).await {
        Ok(Ok(outcome)) => {
            permit.success();
            Some(outcome)
        }
        Ok(Err(err)) => {
            if is_connection_error(&err) {
                permit.failure();
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
            permit.failure();
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
    use std::task::Poll;
    use twilight_model::gateway::payload::incoming::MessageDelete;

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
    fn one_update_probes_after_the_cooldown() {
        let breaker = Breaker::default();
        breaker.failure();
        let until = breaker.state().open_until.unwrap();
        assert!(breaker
            .permit_at(until - Duration::from_millis(1))
            .is_none());

        let probe = breaker.permit_at(until).expect("one probe");
        assert!(breaker.permit_at(until).is_none(), "the others skip");
        // It ended without a verdict (an error about the event, or it was
        // cancelled): the next update probes.
        drop(probe);
        let probe = breaker.permit_at(until).expect("the next probe");
        assert!(breaker.permit_at(until).is_none());

        probe.success();
        let permits: Vec<_> = (0..3).map(|_| breaker.permit_at(until)).collect();
        assert!(permits.iter().all(Option::is_some), "closed: every update");
        drop(permits);

        breaker.failure();
        let until = breaker.state().open_until.unwrap();
        breaker.permit_at(until).expect("a probe").failure();
        assert!(!breaker.allow(), "a failed probe reopens the breaker");
        assert!(!breaker.state().probing, "and ends");
    }

    /// A Redis connection that fails every command in one way.
    #[derive(Clone, Copy, Debug)]
    enum Fake {
        /// Never answers, like a stalled or paused Redis.
        Hang,
        BrokenPipe,
        /// An error about the command, not the connection (like WRONGTYPE).
        Rejected,
    }

    impl Fake {
        fn reply<'a, T: Send + 'a>(self) -> redis::RedisFuture<'a, T> {
            Box::pin(async move {
                Err(match self {
                    Self::Hang => std::future::pending().await,
                    Self::BrokenPipe => std::io::Error::from(std::io::ErrorKind::BrokenPipe).into(),
                    Self::Rejected => (redis::ErrorKind::UnexpectedReturnType, "WRONGTYPE").into(),
                })
            })
        }
    }

    impl ConnectionLike for Fake {
        fn req_packed_command<'a>(
            &'a mut self,
            _cmd: &'a redis::Cmd,
        ) -> redis::RedisFuture<'a, redis::Value> {
            self.reply()
        }

        fn req_packed_commands<'a>(
            &'a mut self,
            _pipeline: &'a redis::Pipeline,
            _offset: usize,
            _count: usize,
        ) -> redis::RedisFuture<'a, Vec<redis::Value>> {
            self.reply()
        }

        fn get_db(&self) -> i64 {
            0
        }
    }

    struct Discard;

    impl Publisher for Discard {
        async fn publish(&self, _: &str, _: Vec<u8>, _: Option<String>) -> ApiResult<()> {
            Ok(())
        }
    }

    /// A context whose breaker's cooldown is over, so the next update probes.
    fn half_open(redis: Fake, cache_timeout: Duration) -> WorkerContext<Fake, Discard> {
        let ctx = WorkerContext {
            redis,
            publisher: Arc::new(Discard),
            registry: Arc::new(Registry::new(std::iter::empty())),
            state: StateConfig {
                enabled: true,
                message: true,
                ..StateConfig::default()
            },
            bot_id: Id::new(1),
            cache_timeout,
            discord: DiscordLog::disabled(),
            breaker: Breaker::default(),
            cache_errors: Throttle::default(),
            publish_errors: Throttle::default(),
        };
        ctx.breaker.state().open_until = Some(Instant::now());
        ctx
    }

    /// An event whose update always writes to Redis.
    fn message_delete() -> Event {
        Event::MessageDelete(MessageDelete {
            channel_id: Id::new(1),
            guild_id: None,
            id: Id::new(2),
        })
    }

    #[tokio::test]
    async fn every_way_a_probe_ends_releases_it() {
        let short = Duration::from_millis(10);
        for (mut conn, reopens) in [
            (Fake::BrokenPipe, true),
            (Fake::Hang, true),
            (Fake::Rejected, false),
        ] {
            let ctx = half_open(conn, short);
            let outcome = update_cache(&ctx, &mut conn, &mut message_delete()).await;
            assert!(outcome.is_none(), "{conn:?}");
            let state = ctx.breaker.state();
            assert!(!state.probing, "{conn:?} ends the probe");
            assert_eq!(
                state.open_until.unwrap() > Instant::now(),
                reopens,
                "{conn:?}"
            );
        }

        // Dropped while it waits for Redis (the worker task was aborted).
        let ctx = half_open(Fake::Hang, Duration::from_secs(60));
        let (mut conn, mut event) = (Fake::Hang, message_delete());
        let mut update = Box::pin(update_cache(&ctx, &mut conn, &mut event));
        let pending =
            std::future::poll_fn(|cx| Poll::Ready(update.as_mut().poll(cx).is_pending())).await;
        assert!(pending);
        assert!(!ctx.breaker.allow(), "the update is the probe");
        drop(update);
        assert!(ctx.breaker.allow(), "dropping it ends the probe");
    }

    #[test]
    fn guild_names() {
        let value = serde_json::json!({"name": "g"});
        assert_eq!(guild_name(Some(&value)), Some("g"));
        assert_eq!(guild_name(Some(&serde_json::json!({"id": "1"}))), None);
        assert_eq!(guild_name(None), None);
    }
}
