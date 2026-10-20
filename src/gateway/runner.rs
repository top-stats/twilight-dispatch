//! Drives one shard: reads raw payloads, routes them to the ordered workers and
//! keeps the shard's snapshot in the registry up to date.
//!
//! twilight-gateway only heartbeats, identifies and reconnects while the shard
//! is being polled, so nothing slow is awaited here: cache updates and
//! publishing happen in the workers. Two exceptions, both bounded:
//! - READY's cache update (at most [`READY_TIMEOUT`]), which must land before
//!   the guilds that follow it so they are not mistaken for new joins;
//! - backpressure: when a worker queue is full the shard is first closed
//!   resumably (so no heartbeat is owed), then the runner waits for room and
//!   lets the shard reconnect; Discord replays what was not received.

use super::{
    payload::{self, Envelope, Route},
    registry::{status_label, ShardHandle},
    worker::Job,
};
use crate::{
    cache::{self, UpdateContext},
    config::StateConfig,
    constants::{CONNECT_COLOR, DISCONNECT_COLOR, READY_COLOR, RESUME_COLOR},
    discord_log::DiscordLog,
    metrics::{GATEWAY_EVENTS, GATEWAY_PARSE_ERRORS, SHARD_EVENTS},
    models::FormattedDateTime,
    util::Throttle,
};

use futures_util::StreamExt;
use redis::aio::ConnectionLike;
use serde::{de::DeserializeSeed, Deserialize};
use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{
    mpsc::{self, error::TrySendError},
    watch,
};
use tracing::{error, info, warn};
use twilight_gateway::{
    error::{ReceiveMessageError, ReceiveMessageErrorType},
    queue::InMemoryQueue,
    CloseFrame, Message, Shard, ShardState,
};
use twilight_model::{
    gateway::event::{Event, GatewayEventDeserializer},
    id::{marker::UserMarker, Id},
};

/// Close codes after which Discord does not allow reconnecting.
/// <https://discord.com/developers/docs/topics/opcodes-and-status-codes#gateway-gateway-close-event-codes>
pub const FATAL_CLOSE_CODES: [u16; 6] = [4004, 4010, 4011, 4012, 4013, 4014];

/// How long a resumable shutdown waits for the gateway to acknowledge the close.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);
/// Upper bound for READY's inline cache update, well below the ~41 s heartbeat.
pub const READY_TIMEOUT: Duration = Duration::from_secs(5);

pub struct RunnerContext<C> {
    pub state: StateConfig,
    pub workers: Vec<mpsc::Sender<Job>>,
    pub redis: C,
    pub discord: DiscordLog,
    pub bot_id: Id<UserMarker>,
    pub cache_timeout: Duration,
}

/// Whether the cache (or the guild join/leave log) needs the typed event.
pub fn wants_typed(kind: &str, state: &StateConfig) -> bool {
    if !state.enabled {
        return matches!(kind, "READY" | "GUILD_CREATE" | "GUILD_DELETE");
    }
    match kind {
        "CHANNEL_CREATE"
        | "CHANNEL_UPDATE"
        | "CHANNEL_DELETE"
        | "CHANNEL_PINS_UPDATE"
        | "THREAD_CREATE"
        | "THREAD_UPDATE"
        | "THREAD_DELETE"
        | "THREAD_LIST_SYNC"
        | "GUILD_CREATE"
        | "GUILD_DELETE"
        | "GUILD_UPDATE"
        | "GUILD_STICKERS_UPDATE"
        | "GUILD_ROLE_CREATE"
        | "GUILD_ROLE_UPDATE"
        | "GUILD_ROLE_DELETE"
        | "GUILD_MEMBER_ADD"
        | "GUILD_MEMBER_REMOVE"
        | "GUILD_MEMBER_UPDATE"
        | "USER_UPDATE"
        | "READY" => true,
        "GUILD_EMOJIS_UPDATE" => state.emoji,
        "GUILD_MEMBERS_CHUNK" => state.member || state.presence,
        "MESSAGE_CREATE" | "MESSAGE_UPDATE" | "MESSAGE_DELETE" | "MESSAGE_DELETE_BULK" => {
            state.message
        }
        "PRESENCE_UPDATE" => state.presence,
        "VOICE_STATE_UPDATE" => state.voice,
        _ => false,
    }
}

/// Parse a raw gateway payload into a twilight event without copying it, the
/// way [`prepare`] does. `Ok(None)` when it is not a gateway payload at all.
pub fn parse_event(json: &str) -> Result<Option<Event>, String> {
    let Ok(envelope) = Envelope::parse(json) else {
        return Ok(None);
    };
    parse_typed(json, &envelope).map(Some)
}

/// Parse a payload as the event its parsed envelope says it is. Not with
/// `GatewayEventDeserializer::from_json`: it searches the text for the first
/// `"op":` and `"t":`, which can be a key inside `d` (key order is not
/// guaranteed behind a gateway proxy), so the cache would apply another event
/// than the one published; and it misses a key with a space before the colon.
fn parse_typed(json: &str, envelope: &Envelope<'_>) -> Result<Event, String> {
    let deserializer = GatewayEventDeserializer::new(envelope.op, envelope.kind());
    let mut json_deserializer = serde_json::Deserializer::from_str(json);
    deserializer
        .deserialize(&mut json_deserializer)
        .map(Event::from)
        .map_err(|err| err.to_string())
}

pub fn is_fatal_close(code: u16) -> bool {
    FATAL_CLOSE_CODES.contains(&code)
}

/// The session fields of READY, read without the full model so a READY the
/// model cannot parse still starts a new session.
#[derive(Debug, Deserialize, Eq, PartialEq)]
pub struct ReadySession {
    pub session_id: String,
    pub resume_gateway_url: String,
}

#[derive(Deserialize)]
struct ReadyEnvelope {
    d: ReadySession,
}

/// What a raw gateway message is, before anything is done with it.
// Short-lived (one per message), so the size difference does not matter.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Prepared {
    /// A dispatch to cache and publish.
    Dispatch {
        kind: String,
        seq: Option<u64>,
        route: Route,
        event: Option<Event>,
        /// For READY: the new session.
        ready: Option<ReadySession>,
        parse_error: Option<String>,
    },
    Hello,
    Reconnect,
    InvalidSession {
        resumable: bool,
    },
    /// Heartbeats, ACKs and anything else the shard handles itself.
    Other,
    Malformed(String),
}

/// Classify a raw gateway message and parse what the cache needs.
pub fn prepare(json: &str, state: &StateConfig) -> Prepared {
    let envelope = match Envelope::parse(json) {
        Ok(envelope) => envelope,
        Err(err) => return Prepared::Malformed(err.to_string()),
    };
    match envelope.op {
        0 => {}
        7 => return Prepared::Reconnect,
        9 => {
            let resumable = envelope.d.is_some_and(|d| d.get() == "true");
            return Prepared::InvalidSession { resumable };
        }
        10 => return Prepared::Hello,
        _ => return Prepared::Other,
    }
    let Some(kind) = envelope.kind().map(ToOwned::to_owned) else {
        return Prepared::Other;
    };
    let route = payload::route(envelope.kind(), envelope.d);
    let ready = if kind == "READY" {
        serde_json::from_str::<ReadyEnvelope>(json)
            .ok()
            .map(|r| r.d)
    } else {
        None
    };
    let (event, parse_error) = if wants_typed(&kind, state) {
        match parse_typed(json, &envelope) {
            Ok(event) => (Some(event), None),
            Err(err) => (None, Some(err)),
        }
    } else {
        (None, None)
    };
    Prepared::Dispatch {
        kind,
        seq: envelope.s,
        route,
        event,
        ready,
        parse_error,
    }
}

struct Runner<C> {
    shard: Shard<InMemoryQueue>,
    handle: Arc<ShardHandle>,
    ctx: Arc<RunnerContext<C>>,
    id: u32,
    last_status: &'static str,
    session: Option<Arc<str>>,
    /// Jobs waiting for room in a full worker queue, in order.
    held: VecDeque<(usize, Job)>,
    /// When the shard was closed because of backpressure.
    paused_at: Option<Instant>,
    parse_errors: Throttle,
    receive_errors: Throttle,
}

/// Run a shard until it is fatally closed or `shutdown` flips to true. On
/// shutdown the connection is closed resumably so the session survives.
pub async fn run<C>(
    shard: Shard<InMemoryQueue>,
    handle: Arc<ShardHandle>,
    ctx: Arc<RunnerContext<C>>,
    mut shutdown: watch::Receiver<bool>,
) where
    C: ConnectionLike + Clone + Send + Sync + 'static,
{
    let id = shard.id().number();
    let session = handle.snapshot().session_id.map(Arc::from);
    let mut runner = Runner {
        shard,
        handle,
        ctx,
        id,
        last_status: "Disconnected",
        session,
        held: VecDeque::new(),
        paused_at: None,
        parse_errors: Throttle::default(),
        receive_errors: Throttle::default(),
    };

    loop {
        // After the backpressure close went out no heartbeat is owed, so it
        // is safe to wait for the workers before reading again.
        if runner.paused_at.is_some() {
            if runner.is_disconnected() {
                runner.flush_held().await;
                // From now on the runner waits for the old connection to
                // finish closing; /healthcheck reports it if that never ends.
                runner
                    .handle
                    .update(|s| s.close_wait_since = Some(Instant::now()));
            } else if !runner.try_flush_held() {
                // Still connected (e.g. the connection dropped without a
                // close echo and twilight reconnected on its own) and the
                // workers are still full: close again rather than block
                // while a heartbeat is owed.
                runner.shard.close(CloseFrame::RESUME);
            }
        }

        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    runner.close().await;
                    return;
                }
            }
            item = runner.shard.next() => {
                if !runner.on_item(item).await {
                    runner.flush_held().await;
                    return;
                }
            }
        }
    }
}

impl<C> Runner<C>
where
    C: ConnectionLike + Clone + Send + Sync + 'static,
{
    /// Handle one stream item. Returns false once the shard is finished.
    async fn on_item(&mut self, item: Option<Result<Message, ReceiveMessageError>>) -> bool {
        self.handle.update(|s| s.close_wait_since = None);
        match item {
            None => {
                error!(
                    "[Shard {}] Closed by Discord with a non-reconnectable code; it will not reconnect",
                    self.id
                );
                self.handle.update(|s| {
                    s.fatal = true;
                    s.active = false;
                    s.status = "FatallyClosed";
                });
                SHARD_EVENTS.with_label_values(&["FatallyClosed"]).inc();
                self.ctx.discord.shard(
                    DISCONNECT_COLOR,
                    format!("[Shard {}] Fatally closed", self.id),
                );
                return false;
            }
            Some(Err(err)) => {
                if let ReceiveMessageErrorType::Deserializing { .. } = err.kind() {
                    GATEWAY_PARSE_ERRORS.with_label_values(&["SHARD"]).inc();
                }
                if let Some(suppressed) = self.receive_errors.check() {
                    warn!(
                        "[Shard {}] Gateway error: {} ({} similar suppressed)",
                        self.id, err, suppressed
                    );
                }
            }
            Some(Ok(Message::Close(frame))) => {
                self.on_close(frame.as_ref().map(|f| (f.code, f.reason.to_string())))
            }
            Some(Ok(Message::Text(json))) => self.on_text(json).await,
        }
        self.refresh();
        true
    }

    fn on_close(&mut self, frame: Option<(u16, String)>) {
        match &frame {
            Some((code, reason)) if !reason.is_empty() => info!(
                "[Shard {}] Disconnected (code: {}, reason: {})",
                self.id, code, reason
            ),
            Some((code, _)) => info!("[Shard {}] Disconnected (code: {})", self.id, code),
            None => info!("[Shard {}] Disconnected", self.id),
        }
        let code = frame.map(|(code, _)| code);
        self.handle.update(|s| s.close_code = code);
        SHARD_EVENTS.with_label_values(&["Disconnected"]).inc();
        match code {
            Some(code) if is_fatal_close(code) => {
                error!(
                    "[Shard {}] Close code {} is not reconnectable",
                    self.id, code
                )
            }
            _ => {
                SHARD_EVENTS.with_label_values(&["Reconnecting"]).inc();
            }
        }
        self.ctx.discord.shard(
            DISCONNECT_COLOR,
            format!("[Shard {}] Disconnected", self.id),
        );
    }

    /// Sync the registry snapshot with the shard and record state changes.
    fn refresh(&mut self) {
        let state = self.shard.state();
        let status = status_label(state);
        if status != self.last_status {
            match status {
                "Identifying" => {
                    info!("[Shard {}] Identifying", self.id);
                    SHARD_EVENTS.with_label_values(&["Identifying"]).inc();
                }
                "Resuming" => {
                    info!("[Shard {}] Resuming", self.id);
                    SHARD_EVENTS.with_label_values(&["Resuming"]).inc();
                }
                _ => {}
            }
            self.last_status = status;
        }

        let latency = self.shard.latency();
        let latency_ms = latency.recent().first().map_or(0, |d| d.as_millis() as u64);
        let last_ack = latency.received();
        let session_id = self.shard.session().map(|s| s.id().to_owned());
        let resume_url = self.shard.resume_url().map(ToOwned::to_owned);
        if session_id.as_deref() != self.session.as_deref() {
            self.session = session_id.as_deref().map(Arc::from);
        }
        self.handle.update(|s| {
            s.status = status;
            s.active = state == ShardState::Active;
            s.latency_ms = latency_ms;
            s.last_ack = last_ack;
            s.last_message = Some(Instant::now());
            // twilight forgets the resume URL after a failed connect but keeps
            // the session; keep READY's URL for as long as the session lives.
            if session_id != s.session_id || resume_url.is_some() {
                s.resume_url = resume_url;
            }
            s.session_id = session_id;
        });
    }

    async fn on_text(&mut self, json: String) {
        match prepare(&json, &self.ctx.state) {
            Prepared::Malformed(err) => {
                warn!("[Shard {}] Could not decode payload: {}", self.id, err)
            }
            Prepared::Hello => {
                info!("[Shard {}] Hello", self.id);
                SHARD_EVENTS.with_label_values(&["Connecting"]).inc();
            }
            Prepared::Reconnect => {
                info!("[Shard {}] Reconnect requested by Discord", self.id);
                SHARD_EVENTS.with_label_values(&["Reconnecting"]).inc();
            }
            Prepared::InvalidSession { resumable } => {
                info!(
                    "[Shard {}] Invalid Session (resumable: {})",
                    self.id, resumable
                )
            }
            Prepared::Other => {}
            Prepared::Dispatch {
                kind,
                seq,
                route,
                mut event,
                ready,
                parse_error,
            } => {
                GATEWAY_EVENTS
                    .with_label_values(&[kind.as_str(), self.id.to_string().as_str()])
                    .inc();
                if let Some(err) = parse_error {
                    GATEWAY_PARSE_ERRORS
                        .with_label_values(&[kind.as_str()])
                        .inc();
                    if let Some(suppressed) = self.parse_errors.check() {
                        warn!(
                            "[Shard {}] Could not parse {} for the cache, publishing it anyway: {} ({} similar suppressed)",
                            self.id, kind, err, suppressed
                        );
                    }
                }

                match kind.as_str() {
                    "READY" => {
                        self.on_ready(seq, ready, event.as_mut()).await;
                        // READY was applied to the cache here; the worker only publishes it.
                        event = None;
                    }
                    "RESUMED" => {
                        info!("[Shard {}] Resumed", self.id);
                        SHARD_EVENTS.with_label_values(&["Resumed"]).inc();
                        SHARD_EVENTS.with_label_values(&["Connected"]).inc();
                        self.ctx
                            .discord
                            .shard(RESUME_COLOR, format!("[Shard {}] Resumed", self.id));
                        self.ctx
                            .discord
                            .shard(CONNECT_COLOR, format!("[Shard {}] Connected", self.id));
                    }
                    _ => {}
                }

                // READY starts the session, so it is not part of its resume
                // point (a failed READY publish must not block saving it).
                let token = if kind == "READY" {
                    None
                } else {
                    seq.and_then(|seq| self.handle.dispatch(seq))
                };
                let worker = route.worker(self.ctx.workers.len());
                let job = Job {
                    shard: self.id,
                    seq,
                    token,
                    session: self.session.clone(),
                    json,
                    event,
                };
                self.enqueue(worker, job);
            }
        }
    }

    async fn on_ready(
        &mut self,
        seq: Option<u64>,
        ready: Option<ReadySession>,
        event: Option<&mut Event>,
    ) {
        // Start the new session even if the full READY did not parse.
        self.handle.new_session(seq.unwrap_or(0));
        match ready {
            Some(ready) => {
                info!("[Shard {}] Ready (session: {})", self.id, ready.session_id);
                self.session = Some(Arc::from(ready.session_id.as_str()));
                self.handle.update(|s| {
                    s.session_id = Some(ready.session_id);
                    s.resume_url = Some(ready.resume_gateway_url);
                    s.fatal = false;
                });
            }
            None => info!("[Shard {}] Ready", self.id),
        }
        if let Some(event) = event {
            self.ready_cache(event).await;
        }
        SHARD_EVENTS.with_label_values(&["Ready"]).inc();
        SHARD_EVENTS.with_label_values(&["Connected"]).inc();
        self.ctx
            .discord
            .shard(READY_COLOR, format!("[Shard {}] Ready", self.id));
    }

    async fn ready_cache(&mut self, event: &mut Event) {
        if !self.ctx.state.enabled {
            return;
        }
        let update_ctx = UpdateContext {
            state: &self.ctx.state,
            bot_id: Some(self.ctx.bot_id),
            now_ms: FormattedDateTime::now().unix_millis(),
        };
        let mut conn = self.ctx.redis.clone();
        let limit = self.ctx.cache_timeout.min(READY_TIMEOUT);
        match tokio::time::timeout(limit, cache::update(&mut conn, event, &update_ctx)).await {
            Ok(Ok(_)) => {}
            Ok(Err(err)) => warn!(
                "[Shard {}] Failed to update state for READY: {}",
                self.id, err
            ),
            Err(_) => warn!("[Shard {}] State update for READY timed out", self.id),
        }
    }

    /// Hand a job to its worker without waiting. When the queue is full the
    /// job (and everything after it) is held and the shard is closed
    /// resumably, so it stops receiving until the workers caught up.
    fn enqueue(&mut self, worker: usize, job: Job) {
        if !self.held.is_empty() {
            self.held.push_back((worker, job));
            return;
        }
        match self.ctx.workers[worker].try_send(job) {
            Ok(()) => {}
            Err(TrySendError::Full(job)) => {
                self.held.push_back((worker, job));
                if self.paused_at.is_none() {
                    self.paused_at = Some(Instant::now());
                    // A disconnected shard owes no heartbeat; a close queued
                    // now would only kill its next connection.
                    if !self.is_disconnected() {
                        warn!(
                            "[Shard {}] Event queue is full; pausing the shard until it drains",
                            self.id
                        );
                        self.shard.close(CloseFrame::RESUME);
                    }
                }
            }
            Err(TrySendError::Closed(_)) => {
                warn!(
                    "[Shard {}] Worker {} is gone, dropping event",
                    self.id, worker
                );
            }
        }
    }

    fn is_disconnected(&self) -> bool {
        matches!(
            self.shard.state(),
            ShardState::Disconnected { .. } | ShardState::FatallyClosed
        )
    }

    /// Queue held jobs without waiting, in order, stopping at the first full
    /// queue. Returns true once nothing is held any more (the pause is over).
    fn try_flush_held(&mut self) -> bool {
        while let Some((worker, job)) = self.held.pop_front() {
            match self.ctx.workers[worker].try_send(job) {
                Ok(()) => {}
                Err(TrySendError::Full(job)) => {
                    self.held.push_front((worker, job));
                    return false;
                }
                Err(TrySendError::Closed(_)) => {}
            }
        }
        if self.paused_at.take().is_some() {
            info!("[Shard {}] Event queue drained; resuming", self.id);
        }
        true
    }

    /// Wait until every held job is queued. Only called once no heartbeat is
    /// owed (the shard is disconnected) or when the shard is done.
    async fn flush_held(&mut self) {
        for (worker, job) in std::mem::take(&mut self.held) {
            if self.ctx.workers[worker].send(job).await.is_err() {
                warn!(
                    "[Shard {}] Worker {} is gone, dropping event",
                    self.id, worker
                );
            }
        }
        if self.paused_at.take().is_some() {
            info!("[Shard {}] Event queue drained; resuming", self.id);
        }
    }

    /// Close resumably and keep handling payloads until the gateway confirms.
    async fn close(&mut self) {
        info!("[Shard {}] Shutting down", self.id);
        // A disconnected shard has nothing to close; a close queued now would
        // open a new connection just to close it. Whatever it did not
        // receive is replayed after the restart.
        if !self.is_disconnected() {
            self.shard.close(CloseFrame::RESUME);
            let deadline = tokio::time::Instant::now() + CLOSE_TIMEOUT;
            loop {
                match tokio::time::timeout_at(deadline, self.shard.next()).await {
                    Ok(Some(Ok(Message::Close(_)))) | Ok(None) | Err(_) => break,
                    Ok(Some(Ok(Message::Text(json)))) => self.on_text(json).await,
                    Ok(Some(Err(_))) => {}
                }
            }
        }
        // Everything received has to reach the workers before the session is
        // saved; the shard no longer needs polling.
        let flush = tokio::time::timeout(CLOSE_TIMEOUT, self.flush_held()).await;
        if flush.is_err() {
            warn!(
                "[Shard {}] Could not queue every event before shutting down",
                self.id
            );
        }
        // Mirror the final session: Discord may have invalidated it while
        // closing, and then it must not be saved.
        self.refresh();
        self.handle.update(|s| s.active = false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_on() -> StateConfig {
        StateConfig {
            enabled: true,
            member: true,
            member_ttl: 1,
            message: true,
            message_ttl: 1,
            presence: true,
            emoji: true,
            voice: true,
            old: true,
        }
    }

    #[test]
    fn typed_parsing_follows_the_state_config() {
        let on = all_on();
        for kind in [
            "GUILD_CREATE",
            "MESSAGE_CREATE",
            "PRESENCE_UPDATE",
            "VOICE_STATE_UPDATE",
            "READY",
        ] {
            assert!(wants_typed(kind, &on), "{kind}");
        }
        assert!(!wants_typed("TYPING_START", &on));
        assert!(!wants_typed("MESSAGE_REACTION_ADD", &on));

        let minimal = StateConfig {
            enabled: true,
            ..StateConfig::default()
        };
        assert!(!wants_typed("PRESENCE_UPDATE", &minimal));
        assert!(!wants_typed("MESSAGE_CREATE", &minimal));
        assert!(!wants_typed("GUILD_MEMBERS_CHUNK", &minimal));
        // Member counts and the bot's own member are kept without STATE_MEMBER.
        assert!(wants_typed("GUILD_MEMBER_ADD", &minimal));
        assert!(wants_typed("GUILD_MEMBER_UPDATE", &minimal));

        // Without the cache only what the join/leave log and READY need.
        let off = StateConfig::default();
        for kind in ["READY", "GUILD_CREATE", "GUILD_DELETE"] {
            assert!(wants_typed(kind, &off), "{kind}");
        }
        assert!(!wants_typed("MESSAGE_CREATE", &off));
    }

    #[test]
    fn parses_dispatch_payloads() {
        let json = r#"{"t":"GUILD_ROLE_DELETE","s":5,"op":0,"d":{"guild_id":"1","role_id":"2"}}"#;
        match parse_event(json).unwrap() {
            Some(Event::RoleDelete(data)) => {
                assert_eq!(data.guild_id.get(), 1);
                assert_eq!(data.role_id.get(), 2);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn bad_payloads_are_errors_not_panics() {
        let json = r#"{"t":"GUILD_ROLE_DELETE","s":5,"op":0,"d":{"guild_id":"x"}}"#;
        assert!(parse_event(json).is_err());
        assert_eq!(parse_event("not json").unwrap_or(None), None);
    }

    /// The typed event is the one the envelope names, which is also the one
    /// that is routed and published, however the payload is laid out.
    #[test]
    fn typed_events_follow_the_envelope() {
        let role_delete = [
            // `t` after a `d` that has a `t` of its own (a key-reordering
            // proxy); a text scan finds GUILD_DELETE and would wipe guild 1000.
            r#"{"op":0,"s":5,"d":{"t":"GUILD_DELETE","id":"1000","guild_id":"1","role_id":"2"},"t":"GUILD_ROLE_DELETE"}"#,
            // Spaces before the colons: a text scan finds no `"op":`.
            r#"{"t" : "GUILD_ROLE_DELETE", "s" : 5, "op" : 0, "d" : {"guild_id" : "1", "role_id" : "2"}}"#,
            // An escaped event type.
            r#"{"t":"GUILD_ROLE\u005fDELETE","s":5,"op":0,"d":{"guild_id":"1","role_id":"2"}}"#,
        ];
        for json in role_delete {
            match prepare(json, &all_on()) {
                Prepared::Dispatch {
                    kind,
                    route,
                    event: Some(Event::RoleDelete(data)),
                    parse_error: None,
                    ..
                } => {
                    assert_eq!(kind, "GUILD_ROLE_DELETE");
                    assert_eq!(route, Route::Guild(1));
                    assert_eq!((data.guild_id.get(), data.role_id.get()), (1, 2));
                }
                other => panic!("{json}: {other:?}"),
            }
            assert!(
                matches!(parse_event(json), Ok(Some(Event::RoleDelete(_)))),
                "{json}"
            );
        }
        // `op` comes from the envelope too: a nested one is not the payload's.
        let hello = r#"{"t":null,"s":null,"d":{"op":0,"heartbeat_interval":41250},"op":10}"#;
        assert!(matches!(prepare(hello, &all_on()), Prepared::Hello));
        assert!(matches!(
            parse_event(hello),
            Ok(Some(Event::GatewayHello(_)))
        ));
    }

    #[test]
    fn prepare_classifies_gateway_messages() {
        let state = all_on();
        assert!(matches!(
            prepare(
                r#"{"t":null,"s":null,"op":10,"d":{"heartbeat_interval":41250}}"#,
                &state
            ),
            Prepared::Hello
        ));
        assert!(matches!(prepare(r#"{"op":11}"#, &state), Prepared::Other));
        assert!(matches!(
            prepare(r#"{"t":null,"s":null,"op":7,"d":null}"#, &state),
            Prepared::Reconnect
        ));
        assert!(matches!(
            prepare(r#"{"t":null,"s":null,"op":9,"d":true}"#, &state),
            Prepared::InvalidSession { resumable: true }
        ));
        assert!(matches!(
            prepare(r#"{"t":null,"s":null,"op":9,"d":false}"#, &state),
            Prepared::InvalidSession { resumable: false }
        ));
        assert!(matches!(prepare("garbage", &state), Prepared::Malformed(_)));
    }

    #[test]
    fn prepare_routes_and_parses_dispatches() {
        let json = r#"{"t":"GUILD_ROLE_DELETE","s":5,"op":0,"d":{"guild_id":"1","role_id":"2"}}"#;
        match prepare(json, &all_on()) {
            Prepared::Dispatch {
                kind,
                seq,
                route,
                event,
                ready,
                parse_error,
            } => {
                assert_eq!(kind, "GUILD_ROLE_DELETE");
                assert_eq!(seq, Some(5));
                assert_eq!(route, Route::Guild(1));
                assert!(matches!(event, Some(Event::RoleDelete(_))));
                assert!(ready.is_none() && parse_error.is_none());
            }
            other => panic!("unexpected {other:?}"),
        }

        // Unparseable for the cache: still a dispatch to publish.
        let broken = r#"{"t":"GUILD_ROLE_DELETE","s":6,"op":0,"d":{"guild_id":"1"}}"#;
        match prepare(broken, &all_on()) {
            Prepared::Dispatch {
                event, parse_error, ..
            } => {
                assert!(event.is_none());
                assert!(parse_error.is_some());
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn ready_session_is_read_even_when_the_model_rejects_ready() {
        // Missing fields the model requires, but session fields present.
        let json =
            r#"{"t":"READY","s":1,"op":0,"d":{"session_id":"abc","resume_gateway_url":"wss://r"}}"#;
        for state in [all_on(), StateConfig::default()] {
            match prepare(json, &state) {
                Prepared::Dispatch {
                    kind, ready, event, ..
                } => {
                    assert_eq!(kind, "READY");
                    assert!(event.is_none());
                    assert_eq!(
                        ready,
                        Some(ReadySession {
                            session_id: "abc".into(),
                            resume_gateway_url: "wss://r".into(),
                        })
                    );
                }
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    #[test]
    fn fatal_close_codes() {
        for code in [4004, 4010, 4011, 4012, 4013, 4014] {
            assert!(is_fatal_close(code));
        }
        for code in [1000, 1001, 4000, 4007, 4009] {
            assert!(!is_fatal_close(code));
        }
    }
}
