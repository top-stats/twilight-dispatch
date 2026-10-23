//! Drives one shard: reads raw payloads, routes them to the ordered workers and
//! keeps the shard's snapshot in the registry up to date.
//!
//! twilight-gateway only heartbeats, identifies and reconnects while the shard
//! is being polled, so nothing slow is awaited here: cache updates and
//! publishing happen in the workers. Two exceptions, both cut short by a
//! shutdown (which then has [`CLOSE_TIMEOUT`] in all):
//! - READY's cache update, which must land before the guilds that follow it
//!   so they are not mistaken for new joins; it is bounded by the smaller of
//!   `CACHE_TIMEOUT_MS` and [`READY_TIMEOUT`];
//! - backpressure, as a last resort: when a worker queue is full the runner
//!   holds the shard's later events in memory, in order, and keeps reading
//!   (so twilight keeps heartbeating) while it queues them as room frees up.
//!   Only once more than [`RunnerContext::max_held_bytes`] are held is the
//!   shard closed resumably (so no heartbeat is owed), then the runner waits
//!   for room for as long as that takes (this wait has no time bound) and
//!   lets the shard reconnect; Discord replays what was not received.
//!
//! With member requests on (`REQUEST_MEMBERS`, by default with the
//! `GUILD_MEMBERS` intent) the runner also requests the members of every one
//! of its guilds: when it starts, before the shard connects, it queues the
//! guilds the cache has for the shard (reading them up to three times, a
//! second apart), so that each start requests them all, also when the shard
//! resumes its session (no READY or `GUILD_CREATE` then); it hands every
//! dispatch to the shard's [`MemberRequester`], which queues every available
//! `GUILD_CREATE`'s guild; it sends the request that is due with the shard's
//! command API (only while the shard is connected, holds no events back and
//! has a command permit to spare) and wakes up when the next one is (see
//! [`super::members`]).

use super::{
    members::{self, MemberRequester},
    payload::{self, Envelope, Route},
    registry::{status_label, ShardHandle},
    worker::Job,
};
use crate::{
    cache::{self, UpdateContext},
    config::{MemberConfig, StateConfig},
    constants::{CONNECT_COLOR, DISCONNECT_COLOR, READY_COLOR, RESUME_COLOR},
    discord_log::DiscordLog,
    metrics::{GATEWAY_EVENTS, GATEWAY_PARSE_ERRORS, SHARD_EVENTS},
    models::FormattedDateTime,
    util::{ErrorChain, Throttle},
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
    queue::Queue,
    CloseFrame, CommandRatelimiter, Message, Shard, ShardState,
};
use twilight_model::{
    gateway::{
        event::{Event, GatewayEventDeserializer},
        OpCode,
    },
    id::{marker::UserMarker, Id},
};

/// How many bytes of payloads a shard holds in memory while a worker queue is
/// full before it stops reading ([`RunnerContext::max_held_bytes`] in
/// production): 128 MiB. Closing the connection to stop reading costs a
/// RESUME, and resuming while Discord sends the `GUILD_CREATE`s of a new
/// session can fail, which forces a new session and every `GUILD_CREATE` again.
pub const MAX_HELD_BYTES: usize = 128 * 1024 * 1024;

/// Close codes after which Discord does not allow reconnecting.
/// <https://discord.com/developers/docs/topics/opcodes-and-status-codes#gateway-gateway-close-event-codes>
pub const FATAL_CLOSE_CODES: [u16; 6] = [4004, 4010, 4011, 4012, 4013, 4014];

/// How long a resumable shutdown may take in all, from when the runner sees
/// it (also in a backpressure pause or during READY's cache update): waiting
/// for the gateway to answer the close, and for room in the worker queues for
/// what arrived until then. `app` waits a little longer for the runners
/// before aborting them.
pub const CLOSE_TIMEOUT: Duration = Duration::from_secs(4);
/// Upper bound for READY's inline cache update, well below the ~41 s heartbeat.
pub const READY_TIMEOUT: Duration = Duration::from_secs(5);
/// How many times a shard reads the guilds the cache has for it, for its
/// member requests, before it connects without them.
const SEED_ATTEMPTS: u32 = 3;
/// How long a shard waits after a failed read of its cached guilds before it
/// reads them again.
const SEED_RETRY_DELAY: Duration = Duration::from_secs(1);
/// How many characters of a message twilight rejected are logged.
const MAX_LOGGED_PAYLOAD: usize = 512;

/// What the shard runners of a process share.
pub struct RunnerContext<C> {
    /// Which events are cached (the `STATE_*` settings).
    pub state: StateConfig,
    /// The worker queues (`WORKERS` of them); a payload goes to the one its
    /// [`Route`] picks, so the events of one guild stay in order.
    pub workers: Vec<mpsc::Sender<Job>>,
    /// Redis, for READY's cache update, which the runner applies itself.
    pub redis: C,
    /// Where shard lifecycle messages are posted (`LOG_CHANNEL`).
    pub discord: DiscordLog,
    /// The bot's user id, for the context of READY's cache update.
    pub bot_id: Id<UserMarker>,
    /// The time limit of a cache update (`CACHE_TIMEOUT_MS`); READY's is
    /// also capped at [`READY_TIMEOUT`].
    pub cache_timeout: Duration,
    /// How many bytes of payloads a shard may hold while a worker queue is
    /// full before it stops reading ([`MAX_HELD_BYTES`] in production; 0
    /// stops reading at the first held event).
    pub max_held_bytes: usize,
    /// Whether the shards request the members of their guilds, and how
    /// often they refresh them (`REQUEST_MEMBERS`, `MEMBER_REFRESH_HOURS`).
    pub members: MemberConfig,
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
/// way [`prepare`] does. A payload `prepare` calls malformed is an error too,
/// so a caller cannot mistake broken input for a payload without an event;
/// every payload that parses is an event, so it is never `Ok(None)`.
///
/// # Errors
///
/// Why the payload did not parse: its envelope does not (it is not JSON or
/// has no `op`, for example), or it is not a valid event of the kind its `op`
/// and `t` name.
pub fn parse_event(json: &str) -> Result<Option<Event>, String> {
    let envelope = Envelope::parse(json).map_err(|err| err.to_string())?;
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
        .map_err(|err| match (envelope.kind(), envelope.d) {
            // GuildCreate is an untagged enum, whose error ("did not match any
            // variant") hides which field failed; parse `d` as the guild
            // again, only on this path, to say.
            (Some("GUILD_CREATE"), Some(d)) => {
                match serde_json::from_str::<twilight_model::guild::Guild>(d.get()) {
                    Err(inner) => format!("{err}: {inner}"),
                    Ok(_) => err.to_string(),
                }
            }
            _ => err.to_string(),
        })
}

/// Whether a gateway close `code` is one of [`FATAL_CLOSE_CODES`], after which
/// the shard must not reconnect.
pub fn is_fatal_close(code: u16) -> bool {
    FATAL_CLOSE_CODES.contains(&code)
}

/// The session fields of READY, read without the full model so a READY the
/// model cannot parse still starts a new session.
#[derive(Debug, Deserialize, Eq, PartialEq)]
pub struct ReadySession {
    /// The id of the new session, which a resume names.
    pub session_id: String,
    /// The gateway URL to resume this session on.
    pub resume_gateway_url: String,
}

/// What a raw gateway message is, before anything is done with it.
#[allow(
    clippy::large_enum_variant,
    reason = "short-lived (one per message), so the size difference does not matter"
)]
#[derive(Debug)]
pub enum Prepared {
    /// A dispatch to cache and publish.
    Dispatch {
        /// The event name (`t`), such as `GUILD_CREATE`.
        kind: String,
        /// The sequence number (`s`).
        seq: Option<u64>,
        /// What orders the payload: it picks the worker.
        route: Route,
        /// The typed event, when [`wants_typed`] asks for it and it parsed.
        event: Option<Event>,
        /// For READY: the new session.
        ready: Option<ReadySession>,
        /// Why the typed event did not parse; the payload is published anyway.
        parse_error: Option<String>,
    },
    /// Op 10: a connection started.
    Hello,
    /// Op 7: Discord asks the shard to reconnect.
    Reconnect,
    /// Op 9: Discord invalidated the session.
    InvalidSession {
        /// Whether the session may be resumed (`d` is `true`).
        resumable: bool,
    },
    /// Heartbeats, ACKs and anything else the shard handles itself, and a
    /// dispatch without an event name (`t`).
    Other,
    /// A message whose envelope (`op`, `s`, `t`, `d`) does not parse, such as
    /// one that is not JSON or has no `op`: why.
    Malformed(String),
}

/// Classify a raw gateway message and parse what the cache needs.
pub fn prepare(json: &str, state: &StateConfig) -> Prepared {
    let envelope = match Envelope::parse(json) {
        Ok(envelope) => envelope,
        Err(err) => return Prepared::Malformed(err.to_string()),
    };
    match OpCode::from(envelope.op) {
        Some(OpCode::Dispatch) => {}
        Some(OpCode::Reconnect) => return Prepared::Reconnect,
        Some(OpCode::InvalidSession) => {
            let resumable = envelope.d.is_some_and(|d| d.get() == "true");
            return Prepared::InvalidSession { resumable };
        }
        Some(OpCode::Hello) => return Prepared::Hello,
        _ => return Prepared::Other,
    }
    let Some(kind) = envelope.kind().map(ToOwned::to_owned) else {
        return Prepared::Other;
    };
    let route = payload::route(envelope.kind(), envelope.d);
    // Only the session fields of `d`, so a READY the model rejects has them.
    let ready = match envelope.d {
        Some(d) if kind == "READY" => serde_json::from_str::<ReadySession>(d.get()).ok(),
        _ => None,
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

/// The state of one shard's [`run`].
struct Runner<C, Q> {
    shard: Shard<Q>,
    handle: Arc<ShardHandle>,
    ctx: Arc<RunnerContext<C>>,
    id: u32,
    /// The status last recorded by [`Runner::refresh`].
    last_status: &'static str,
    /// The current gateway session, handed to every job.
    session: Option<Arc<str>>,
    /// Jobs waiting for room in a full worker queue, in order.
    held: VecDeque<(usize, Job)>,
    /// The payload bytes of the `held` jobs.
    held_bytes: usize,
    /// Since when jobs are held because a worker queue was full. Past
    /// [`RunnerContext::max_held_bytes`] the shard is closed resumably,
    /// unless it was disconnected already.
    paused_at: Option<Instant>,
    /// Whether the start of the current hold was logged, so its end is too.
    hold_logged: bool,
    /// Throttles the warnings about full worker queues.
    hold_warnings: Throttle,
    /// While shutting down: when [`Runner::close`] has to be done.
    close_deadline: Option<tokio::time::Instant>,
    /// The shutdown signal, for the one wait inside handling a message
    /// (READY's cache update); `run` waits on its own receiver.
    shutdown: watch::Receiver<bool>,
    /// Discord asked to reconnect (op 7 or 9) and that was counted then, so
    /// the close that ends the connection is not counted again.
    reconnect_counted: bool,
    /// A connection started (HELLO) whose end was not counted yet.
    connection_open: bool,
    /// Throttles the warning about dispatches the cache could not parse.
    parse_errors: Throttle,
    /// Throttles the warnings about messages twilight or [`prepare`] rejected.
    receive_errors: Throttle,
    /// The shard's guild member requests; `None` when members are not
    /// requested ([`MemberConfig::request`]).
    members: Option<MemberRequester>,
}

/// Run a shard until it is fatally closed or `shutdown` flips to true. On
/// shutdown the connection is closed resumably so the session survives.
pub async fn run<C, Q>(
    shard: Shard<Q>,
    handle: Arc<ShardHandle>,
    ctx: Arc<RunnerContext<C>>,
    mut shutdown: watch::Receiver<bool>,
) where
    C: ConnectionLike + Clone + Send + Sync + 'static,
    Q: Queue + Unpin + Send + Sync + 'static,
{
    let id = shard.id().number();
    let session = handle.snapshot().session_id.map(Arc::from);
    let requests = if ctx.members.request {
        Some(MemberRequester::new(
            id,
            ctx.members.refresh(),
            members::now_ms(),
        ))
    } else {
        None
    };
    let mut runner = Runner {
        shard,
        handle,
        ctx,
        id,
        last_status: "Disconnected",
        session,
        held: VecDeque::new(),
        held_bytes: 0,
        paused_at: None,
        hold_logged: false,
        hold_warnings: Throttle::default(),
        close_deadline: None,
        shutdown: shutdown.clone(),
        reconnect_counted: false,
        connection_open: false,
        parse_errors: Throttle::default(),
        receive_errors: Throttle::default(),
        members: requests,
    };
    runner.seed_members().await;

    loop {
        // After the backpressure close went out no heartbeat is owed, so it
        // is safe to wait for the workers before reading again.
        if runner.held_over_limit() {
            if runner.is_disconnected() {
                // But not past a shutdown, which would then wait until `app`
                // aborts the runner: `flush_held` keeps what it did not queue
                // yet, and `close` gives that until its deadline.
                tokio::select! {
                    () = runner.flush_held() => {}
                    () = shutdown_requested(&mut shutdown) => {
                        runner.close().await;
                        return;
                    }
                }
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

        // Send the member request that is due, and learn when one is next.
        let wake = runner.drive_members();

        // Below the limit the shard keeps reading while held jobs wait for
        // room in the queue of the oldest one.
        let waiting = if runner.held_over_limit() {
            None
        } else {
            let oldest = runner.held.front();
            oldest.map(|&(worker, _)| runner.ctx.workers[worker].clone())
        };
        tokio::select! {
            () = shutdown_requested(&mut shutdown) => {
                runner.close().await;
                return;
            }
            permit = room_in(waiting) => runner.queue_oldest_held(permit),
            item = runner.shard.next() => {
                if !runner.on_item(item).await {
                    runner.flush_held().await;
                    return;
                }
            }
            () = wake_at(wake) => {}
        }
    }
}

/// Resolves at `at`, and never without a time.
async fn wake_at(at: Option<tokio::time::Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Resolves with room in `queue` (`None` once its worker is gone), and never
/// without a queue to wait for.
async fn room_in(queue: Option<mpsc::Sender<Job>>) -> Option<mpsc::OwnedPermit<Job>> {
    match queue {
        Some(queue) => queue.reserve_owned().await.ok(),
        None => std::future::pending().await,
    }
}

/// Resolves once a shutdown is requested, or can no longer be (`app` is gone).
async fn shutdown_requested(shutdown: &mut watch::Receiver<bool>) {
    let _ = shutdown.wait_for(|stop| *stop).await;
}

/// The first `chars` characters of `text`, or all of it when it is shorter.
fn prefix(text: &str, chars: usize) -> &str {
    text.char_indices()
        .nth(chars)
        .map_or(text, |(end, _)| &text[..end])
}

impl<C, Q> Runner<C, Q>
where
    C: ConnectionLike + Clone + Send + Sync + 'static,
    Q: Queue + Unpin + Send + Sync + 'static,
{
    /// Handle one stream item. Returns false once the shard is finished.
    async fn on_item(&mut self, item: Option<Result<Message, ReceiveMessageError>>) -> bool {
        self.handle.update(|s| s.close_wait_since = None);
        match item {
            None => {
                self.fatally_closed();
                return false;
            }
            Some(Err(err)) => {
                if let ReceiveMessageErrorType::Deserializing { .. } = err.kind() {
                    GATEWAY_PARSE_ERRORS.with_label_values(&["SHARD"]).inc();
                }
                if let Some(suppressed) = self.receive_errors.check() {
                    self.log_receive_error(&err, suppressed);
                }
            }
            Some(Ok(Message::Close(frame))) => {
                self.on_close(frame.map(|f| (f.code, f.reason.into_owned())));
            }
            Some(Ok(Message::Text(json))) => self.on_text(json).await,
        }
        self.refresh();
        true
    }

    /// Log an error twilight yielded instead of a message, with the cause its
    /// `Display` leaves out. A message twilight rejected is shown cut to
    /// [`MAX_LOGGED_PAYLOAD`] characters rather than in full, as its `Display`
    /// has it (a READY can be megabytes).
    fn log_receive_error(&self, err: &ReceiveMessageError, suppressed: u64) {
        if let ReceiveMessageErrorType::Deserializing { event } = err.kind() {
            let bytes = event.len();
            let cause = std::error::Error::source(err)
                .map_or_else(String::new, |source| format!(": {}", ErrorChain(source)));
            let start = prefix(event, MAX_LOGGED_PAYLOAD);
            warn!(
                "[Shard {}] Gateway error: could not parse a {}-byte message{} ({} similar suppressed): {}",
                self.id, bytes, cause, suppressed, start
            );
        } else {
            let err = ErrorChain(err);
            warn!(
                "[Shard {}] Gateway error: {} ({} similar suppressed)",
                self.id, err, suppressed
            );
        }
    }

    /// Record the end of the connection by a close, with the frame's code and
    /// reason when it had one.
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
        self.disconnected(code);
    }

    /// Count and log the end of a connection: at its close, or at the next
    /// HELLO when it ended without one (see [`Prepared::Hello`]). twilight
    /// reconnects unless the gateway closed it with a fatal `code`.
    ///
    /// Never counted twice, but not exact either: a connection that ends
    /// without a close is not counted when no HELLO follows (the shard shuts
    /// down first), and when the next connection is closed before its HELLO
    /// both ends are counted as one.
    fn disconnected(&mut self, code: Option<u16>) {
        self.connection_open = false;
        SHARD_EVENTS.with_label_values(&["Disconnected"]).inc();
        let counted = std::mem::take(&mut self.reconnect_counted);
        match code {
            Some(code) if is_fatal_close(code) => {
                error!(
                    "[Shard {}] Close code {} is not reconnectable",
                    self.id, code
                );
            }
            // The close answering Discord's request (`reconnect_requested`).
            _ if counted => {}
            _ => {
                SHARD_EVENTS.with_label_values(&["Reconnecting"]).inc();
            }
        }
        self.ctx.discord.shard(
            DISCONNECT_COLOR,
            format!("[Shard {}] Disconnected", self.id),
        );
    }

    /// The shard stopped for good: the gateway closed it with a code that
    /// does not allow reconnecting.
    fn fatally_closed(&self) {
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
    }

    /// Sync the registry snapshot with the shard and record state changes.
    fn refresh(&mut self) {
        let status = status_label(self.shard.state());
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
        self.sync();
    }

    /// Mirror the shard's state, latency and session in the registry snapshot.
    fn sync(&mut self) {
        let state = self.shard.state();
        let status = status_label(state);
        let latency = self.shard.latency();
        let latency_ms = latency.recent().first().map_or(0, Duration::as_millis);
        let latency_ms = u64::try_from(latency_ms).unwrap_or(u64::MAX);
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

    /// Handle a text message from the gateway.
    async fn on_text(&mut self, json: String) {
        let prepared = prepare(&json, &self.ctx.state);
        self.on_prepared(prepared, json).await;
    }

    /// Act on a classified message: record connection changes, and hand a
    /// dispatch (`json`, the raw payload) to its worker.
    async fn on_prepared(&mut self, prepared: Prepared, json: String) {
        match prepared {
            // twilight yields a dispatch whose `op`, `s` and `t` its text scan
            // finds even when it is not valid JSON, and its session sequence
            // moves past it: the payload is lost, so it is counted.
            Prepared::Malformed(err) => {
                GATEWAY_PARSE_ERRORS.with_label_values(&["SHARD"]).inc();
                if let Some(suppressed) = self.receive_errors.check() {
                    warn!(
                        "[Shard {}] Could not decode payload: {} ({} similar suppressed)",
                        self.id, err, suppressed
                    );
                }
            }
            Prepared::Hello => {
                if self.connection_open {
                    // twilight yields nothing for a connection that ends
                    // without a close frame and reconnects on its own, so
                    // this HELLO is the first sign that the last one ended.
                    // A close clears the flag, so no end is counted twice.
                    info!(
                        "[Shard {}] Disconnected (the connection ended without a close)",
                        self.id
                    );
                    self.disconnected(None);
                }
                self.connection_open = true;
                info!("[Shard {}] Hello", self.id);
                SHARD_EVENTS.with_label_values(&["Connecting"]).inc();
                // A new connection; the last one's end is settled.
                self.reconnect_counted = false;
            }
            Prepared::Reconnect => {
                info!("[Shard {}] Reconnect requested by Discord", self.id);
                self.reconnect_requested();
            }
            Prepared::InvalidSession { resumable } => {
                info!(
                    "[Shard {}] Invalid Session (resumable: {})",
                    self.id, resumable
                );
                self.reconnect_requested();
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
                if let Some(requests) = self.members.as_mut() {
                    requests.observe(&kind, event.as_ref(), &json, members::now_ms());
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

    /// Queue a member request for every guild the cache has for this shard
    /// ([`MemberRequester::seed`]), so that every start requests them all: a
    /// shard that resumes its session gets no READY or `GUILD_CREATE` to
    /// request them by. After a new session READY drops those it does not
    /// list and holds the others back until their `GUILD_CREATE`. The cache
    /// is read before the shard first connects, so no heartbeat is owed
    /// meanwhile, up to [`SEED_ATTEMPTS`] times [`SEED_RETRY_DELAY`] apart,
    /// and a shutdown stops the reads. Without the cache, or when every read
    /// failed, a resumed shard requests a guild only at its next
    /// `GUILD_CREATE`.
    async fn seed_members(&mut self) {
        if self.members.is_none() || !self.ctx.state.enabled {
            return;
        }
        let mut shutdown = self.shutdown.clone();
        let guilds = tokio::select! {
            guilds = self.cached_guilds() => guilds,
            () = shutdown_requested(&mut shutdown) => None,
        };
        if let (Some(requests), Some(guilds)) = (self.members.as_mut(), guilds) {
            let guilds = guilds.into_iter().filter_map(Id::new_checked);
            requests.seed(guilds, members::now_ms());
        }
    }

    /// The ids of the guilds the cache has for this shard, read up to
    /// [`SEED_ATTEMPTS`] times [`SEED_RETRY_DELAY`] apart, each read within
    /// the smaller of `CACHE_TIMEOUT_MS` and [`READY_TIMEOUT`]; `None` when
    /// every read failed.
    async fn cached_guilds(&mut self) -> Option<Vec<u64>> {
        let total = self.shard.id().total();
        let limit = self.ctx.cache_timeout.min(READY_TIMEOUT);
        let mut redis = self.ctx.redis.clone();
        for attempt in 1..=SEED_ATTEMPTS {
            let read = cache::shard_guilds(&mut redis, self.id, total);
            let failure = match tokio::time::timeout(limit, read).await {
                Ok(Ok(guilds)) => return Some(guilds),
                Ok(Err(err)) => err.to_string(),
                Err(_) => format!("timed out after {limit:?}"),
            };
            if attempt < SEED_ATTEMPTS {
                warn!(
                    "[Shard {}] Could not read the cached guilds for member requests (attempt {} of {}), reading them again in {:?}: {}",
                    self.id, attempt, SEED_ATTEMPTS, SEED_RETRY_DELAY, failure
                );
                tokio::time::sleep(SEED_RETRY_DELAY).await;
            } else {
                warn!(
                    "[Shard {}] Could not read the cached guilds for member requests in {} attempts, so this start requests only the guilds of a READY or GUILD_CREATE: {}",
                    self.id, SEED_ATTEMPTS, failure
                );
            }
        }
        None
    }

    /// Send the member request that is due, if any, and return when the
    /// member requests next need a look ([`MemberRequester::next_wake`]). A
    /// request goes out only while the shard is connected, holds no events
    /// back (its chunks would only add to them) and has a permit of its
    /// command ratelimiter to spare: twilight then sends it at once, so its
    /// timeout measures Discord's answer rather than the wait for a permit.
    /// Without a permit the requests look again once the next is released.
    fn drive_members(&mut self) -> Option<tokio::time::Instant> {
        let requests = self.members.as_mut()?;
        let now = members::now_ms();
        let limiter = self.shard.ratelimiter();
        let permit = limiter.is_none_or(|limiter| limiter.available() > 0);
        let active = self.shard.state() == ShardState::Active;
        let ready = active && permit && self.held.is_empty();
        if let Some(request) = requests.poll(now, ready) {
            self.shard.command(&request.command());
        }
        let due = requests.next_wake().map(|at| at.saturating_sub(now));
        let due = due.map(Duration::from_millis);
        let release = limiter.filter(|_| !permit);
        let release = release.map(CommandRatelimiter::next_available);
        let wait = [due, release].into_iter().flatten().min()?;
        tokio::time::Instant::now().checked_add(wait)
    }

    /// Count the reconnect Discord asked for when it asks. twilight answers
    /// with a close of its own, and the gateway may drop the connection
    /// instead of echoing it, which yields no close to count it by.
    fn reconnect_requested(&mut self) {
        // A shard that is shutting down does not reconnect.
        if self.close_deadline.is_none() {
            SHARD_EVENTS.with_label_values(&["Reconnecting"]).inc();
            self.reconnect_counted = true;
        }
    }

    /// Start the session READY opens and apply READY to the cache before
    /// anything that follows it is handled.
    async fn on_ready(
        &mut self,
        seq: Option<u64>,
        ready: Option<ReadySession>,
        event: Option<&mut Event>,
    ) {
        // Start the new session even if the full READY did not parse.
        self.handle.new_session(seq.unwrap_or(0));
        if let Some(ready) = ready {
            info!("[Shard {}] Ready (session: {})", self.id, ready.session_id);
            self.session = Some(Arc::from(ready.session_id.as_str()));
            self.handle.update(|s| {
                s.session_id = Some(ready.session_id);
                s.resume_url = Some(ready.resume_gateway_url);
                s.fatal = false;
            });
        } else {
            info!("[Shard {}] Ready", self.id);
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

    /// Apply READY to the cache, waiting at most the smaller of the cache
    /// timeout and [`READY_TIMEOUT`], and never past the close's deadline
    /// once a shutdown started.
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
        let limit = tokio::time::Instant::now() + self.ctx.cache_timeout.min(READY_TIMEOUT);
        let update = cache::update(&mut conn, event, &update_ctx);
        tokio::pin!(update);
        // Neither a READY that arrives while closing nor a shutdown requested
        // while it is applied may hold up the shutdown: the update then has
        // until the close's deadline, which `close` keeps.
        let result = if let Some(deadline) = self.close_deadline {
            tokio::time::timeout_at(deadline.min(limit), update).await
        } else {
            tokio::select! {
                result = tokio::time::timeout_at(limit, &mut update) => result,
                () = shutdown_requested(&mut self.shutdown) => {
                    let deadline = tokio::time::Instant::now() + CLOSE_TIMEOUT;
                    self.close_deadline = Some(deadline);
                    tokio::time::timeout_at(deadline.min(limit), update).await
                }
            }
        };
        match result {
            Ok(Ok(_)) => {}
            Ok(Err(err)) => warn!(
                "[Shard {}] Failed to update state for READY: {}",
                self.id, err
            ),
            Err(_) => warn!("[Shard {}] State update for READY timed out", self.id),
        }
    }

    /// Hand a job to its worker without waiting. When the queue is full the
    /// job (and everything after it) is held, in order, and queued as room
    /// frees up. Past [`RunnerContext::max_held_bytes`] the shard is closed
    /// resumably, so it stops receiving until the workers caught up.
    fn enqueue(&mut self, worker: usize, job: Job) {
        if !self.held.is_empty() {
            self.hold(worker, job);
            return;
        }
        match self.ctx.workers[worker].try_send(job) {
            Ok(()) => {}
            Err(TrySendError::Full(job)) => {
                self.paused_at = Some(Instant::now());
                if let Some(suppressed) = self.hold_warnings.check() {
                    self.hold_logged = true;
                    warn!(
                        "[Shard {}] Event queue is full; holding events until it drains ({} similar suppressed)",
                        self.id, suppressed
                    );
                }
                self.hold(worker, job);
            }
            Err(TrySendError::Closed(_)) => {
                warn!(
                    "[Shard {}] Worker {} is gone, dropping event",
                    self.id, worker
                );
            }
        }
    }

    /// Hold a job behind the ones already held. Once more than
    /// [`RunnerContext::max_held_bytes`] are held the shard is closed
    /// resumably.
    fn hold(&mut self, worker: usize, job: Job) {
        let was_over = self.held_over_limit();
        self.held_bytes = self.held_bytes.saturating_add(job.json.len());
        self.held.push_back((worker, job));
        // A disconnected shard owes no heartbeat; a close queued now would
        // only kill its next connection.
        if !was_over && self.held_over_limit() && !self.is_disconnected() {
            warn!(
                "[Shard {}] {} bytes of events held; pausing the shard until the queue drains",
                self.id, self.held_bytes
            );
            self.shard.close(CloseFrame::RESUME);
        }
    }

    /// Whether more is held than [`RunnerContext::max_held_bytes`] allows, so
    /// the shard has to stop reading.
    fn held_over_limit(&self) -> bool {
        !self.held.is_empty() && self.held_bytes >= self.ctx.max_held_bytes
    }

    /// Queue the oldest held job with `permit`, room in its worker's queue
    /// (`None`: the worker is gone and the job is dropped), then as many of
    /// the others as fit.
    fn queue_oldest_held(&mut self, permit: Option<mpsc::OwnedPermit<Job>>) {
        if let Some((worker, job)) = self.held.pop_front() {
            self.held_bytes = self.held_bytes.saturating_sub(job.json.len());
            if let Some(permit) = permit {
                // It hands back a clone of the worker's sender.
                let _ = permit.send(job);
            } else {
                warn!(
                    "[Shard {}] Worker {} is gone, dropping event",
                    self.id, worker
                );
            }
        }
        self.try_flush_held();
    }

    /// Whether the shard has no connection, so no heartbeat is owed.
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
            let bytes = job.json.len();
            match self.ctx.workers[worker].try_send(job) {
                // A gone worker's job is dropped: no queue would ever take it.
                Ok(()) | Err(TrySendError::Closed(_)) => {
                    self.held_bytes = self.held_bytes.saturating_sub(bytes);
                }
                Err(TrySendError::Full(job)) => {
                    self.held.push_front((worker, job));
                    return false;
                }
            }
        }
        self.end_pause();
        true
    }

    /// Wait until every held job is queued. Only called once no heartbeat is
    /// owed (the shard is disconnected) or when the shard is done.
    async fn flush_held(&mut self) {
        while let Some(&(worker, _)) = self.held.front() {
            // A job leaves `held` only with its queue slot, so a wait that is
            // cut short (shutdown, deadline) keeps every job not queued yet,
            // in order, and queues none twice.
            let permit = self.ctx.workers[worker].reserve().await;
            let Some((_, job)) = self.held.pop_front() else {
                break;
            };
            self.held_bytes = self.held_bytes.saturating_sub(job.json.len());
            if let Ok(permit) = permit {
                permit.send(job);
            } else {
                warn!(
                    "[Shard {}] Worker {} is gone, dropping event",
                    self.id, worker
                );
            }
        }
        self.end_pause();
    }

    /// End the hold, if there is one, and log how long it took when its start
    /// was logged.
    fn end_pause(&mut self) {
        self.held_bytes = 0;
        if let Some(since) = self.paused_at.take() {
            if std::mem::take(&mut self.hold_logged) {
                info!(
                    "[Shard {}] Event queue drained after {:?}; resuming",
                    self.id,
                    since.elapsed()
                );
            }
        }
    }

    /// Close resumably and keep handling payloads until the gateway confirms,
    /// all within [`CLOSE_TIMEOUT`].
    async fn close(&mut self) {
        info!("[Shard {}] Shutting down", self.id);
        // Set already when READY's cache update saw the shutdown first.
        let deadline = *self
            .close_deadline
            .get_or_insert_with(|| tokio::time::Instant::now() + CLOSE_TIMEOUT);
        // Fatally closed already: `run` handled the gateway's close
        // (`on_close`), but saw the shutdown before the end of the stream,
        // which is where the shard is marked fatal. That is still owed.
        let mut fatal = self.shard.state() == ShardState::FatallyClosed;
        // A disconnected shard has nothing to close; a close queued now would
        // open a new connection just to close it. Whatever it did not
        // receive is replayed after the restart.
        if !self.is_disconnected() {
            self.shard.close(CloseFrame::RESUME);
            loop {
                let json = match tokio::time::timeout_at(deadline, self.shard.next()).await {
                    // No echo of the shard's 4000 but the gateway ending the
                    // session for good (twilight ignores the code, as it
                    // closed first): recorded as it is without a shutdown.
                    Ok(Some(Ok(Message::Close(Some(frame))))) if is_fatal_close(frame.code) => {
                        self.on_close(Some((frame.code, frame.reason.into_owned())));
                        fatal = true;
                        break;
                    }
                    // twilight ends the stream only after yielding the fatal
                    // close above, so `None` does not get here first.
                    Ok(Some(Ok(Message::Close(_))) | None) | Err(_) => break,
                    Ok(Some(Ok(Message::Text(json)))) => json,
                    // The connection is gone (it dropped instead of
                    // answering the close) and twilight failed to reconnect.
                    Ok(Some(Err(err)))
                        if matches!(err.kind(), ReceiveMessageErrorType::Reconnect) =>
                    {
                        break
                    }
                    Ok(Some(Err(_))) => continue,
                };
                let prepared = prepare(&json, &self.ctx.state);
                if matches!(prepared, Prepared::Hello) {
                    // HELLO starts a connection, normally one twilight opened
                    // on its own after the old one dropped instead of
                    // answering. twilight sends RESUME (or IDENTIFY) on the
                    // next poll, which would take the session over only to
                    // drop it again.
                    info!(
                        "[Shard {}] Reconnected while shutting down; not resuming",
                        self.id
                    );
                    break;
                }
                self.on_prepared(prepared, json).await;
            }
        }
        // Everything received has to reach the workers before the session is
        // saved; the shard no longer needs polling.
        if tokio::time::timeout_at(deadline, self.flush_held())
            .await
            .is_err()
        {
            warn!(
                "[Shard {}] Could not queue every event before shutting down",
                self.id
            );
        }
        // Mirror the final session: Discord may have invalidated it while
        // closing, and then it must not be saved. Not a state change to
        // record: a connection twilight opened meanwhile was never used.
        self.sync();
        self.handle.update(|s| s.active = false);
        if fatal {
            // After `sync`, which mirrors twilight's view of the close.
            self.fatally_closed();
        }
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
        // What `prepare` calls malformed: not JSON, cut off, without an op.
        for json in [
            "not json",
            r#"{"t":"GUILD_ROLE_DELETE","s":5,"op":0,"d":{"guild_id":"1","role_id":"2""#,
            r#"{"t":"GUILD_ROLE_DELETE","s":5,"d":{"guild_id":"1","role_id":"2"}}"#,
        ] {
            assert!(
                matches!(prepare(json, &all_on()), Prepared::Malformed(_)),
                "{json}"
            );
            assert!(parse_event(json).is_err(), "{json}");
        }
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

#[cfg(test)]
mod runner_tests {
    use super::*;
    use crate::models::SessionInfo;
    use twilight_gateway::{queue::InMemoryQueue, Intents, ShardId};

    /// A Redis that never answers, or (`down`) fails every command at once;
    /// only the reads of the cached guilds for member requests get as far as
    /// using it.
    #[derive(Clone, Default)]
    struct NoRedis {
        down: bool,
    }

    impl NoRedis {
        /// What every command gets: no answer, or an error when `down`.
        fn reply<'a, T: Send + 'a>(&self) -> redis::RedisFuture<'a, T> {
            let down = self.down;
            Box::pin(async move {
                if down {
                    Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe).into())
                } else {
                    std::future::pending().await
                }
            })
        }
    }

    impl ConnectionLike for NoRedis {
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

    /// A job with sequence `seq`.
    fn job(seq: u64) -> Job {
        Job {
            shard: 0,
            seq: Some(seq),
            token: None,
            session: None,
            json: String::new(),
            event: None,
        }
    }

    /// A runner with these worker queues whose shard never connected: it owes
    /// no heartbeat, so a full queue holds jobs without queueing a close.
    /// `Shard::new` spawns its identify queue, so the tests run in a runtime.
    fn runner(workers: Vec<mpsc::Sender<Job>>) -> Runner<NoRedis, InMemoryQueue> {
        // twilight's TLS connector needs a crypto provider; main() installs it.
        let _ = rustls::crypto::ring::default_provider().install_default();
        Runner {
            shard: Shard::new(ShardId::ONE, "t".to_owned(), Intents::empty()),
            handle: Arc::new(ShardHandle::new(0, None, None)),
            ctx: Arc::new(RunnerContext {
                state: StateConfig::default(),
                workers,
                redis: NoRedis::default(),
                discord: DiscordLog::disabled(),
                bot_id: Id::new(1),
                cache_timeout: Duration::from_secs(1),
                max_held_bytes: 0,
                members: MemberConfig::default(),
            }),
            id: 0,
            last_status: "Disconnected",
            session: None,
            held: VecDeque::new(),
            held_bytes: 0,
            paused_at: None,
            hold_logged: false,
            hold_warnings: Throttle::default(),
            close_deadline: None,
            shutdown: watch::channel(false).1,
            reconnect_counted: false,
            connection_open: false,
            parse_errors: Throttle::default(),
            receive_errors: Throttle::default(),
            members: None,
        }
    }

    /// The worker and the sequence of every held job, in order.
    fn held(r: &Runner<NoRedis, InMemoryQueue>) -> Vec<(usize, Option<u64>)> {
        r.held
            .iter()
            .map(|(worker, job)| (*worker, job.seq))
            .collect()
    }

    /// Once one worker's queue is full, a job for another worker that has
    /// room is held behind it instead of overtaking it, and the held jobs are
    /// queued in order once there is room, which ends the pause.
    #[tokio::test]
    async fn held_jobs_keep_their_order_across_workers() {
        let (tx0, mut rx0) = mpsc::channel(1);
        let (tx1, mut rx1) = mpsc::channel(1);
        assert!(tx0.try_send(job(1)).is_ok());
        let mut r = runner(vec![tx0, tx1]);

        r.enqueue(0, job(2));
        r.enqueue(1, job(3));
        assert!(rx1.try_recv().is_err(), "overtook the held job");
        assert!(r.paused_at.is_some());
        assert!(!r.try_flush_held(), "worker 0 is still full");
        assert_eq!(held(&r), [(0, Some(2)), (1, Some(3))]);
        assert!(r.paused_at.is_some());

        assert_eq!(rx0.try_recv().unwrap().seq, Some(1));
        assert!(r.try_flush_held());
        assert!(r.held.is_empty());
        assert!(r.paused_at.is_none());
        assert_eq!(rx0.try_recv().unwrap().seq, Some(2));
        assert_eq!(rx1.try_recv().unwrap().seq, Some(3));
    }

    /// A job for a worker that is gone is dropped: holding it would pause the
    /// shard for good, as no queue would ever take it.
    #[tokio::test]
    async fn a_gone_worker_drops_its_job_without_pausing() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let mut r = runner(vec![tx]);
        r.enqueue(0, job(1));
        assert!(r.held.is_empty());
        assert!(r.paused_at.is_none());
    }

    /// A held job whose worker went away meanwhile is dropped by
    /// `try_flush_held`, which goes on with the jobs behind it.
    #[tokio::test]
    async fn try_flush_held_drops_the_jobs_of_a_gone_worker() {
        let (tx0, rx0) = mpsc::channel(1);
        let (tx1, mut rx1) = mpsc::channel(1);
        assert!(tx0.try_send(job(1)).is_ok());
        let mut r = runner(vec![tx0, tx1]);
        r.enqueue(0, job(2));
        r.enqueue(1, job(3));
        drop(rx0);

        assert!(r.try_flush_held());
        assert!(r.held.is_empty());
        assert!(r.paused_at.is_none());
        assert_eq!(rx1.try_recv().unwrap().seq, Some(3));
    }

    /// As above for `flush_held`: it does not wait for room in the queue of
    /// a worker that is gone.
    #[tokio::test]
    async fn flush_held_drops_the_jobs_of_a_gone_worker() {
        let (tx0, rx0) = mpsc::channel(1);
        let (tx1, mut rx1) = mpsc::channel(1);
        assert!(tx0.try_send(job(1)).is_ok());
        let mut r = runner(vec![tx0, tx1]);
        r.enqueue(0, job(2));
        r.enqueue(1, job(3));
        drop(rx0);

        let flushed = tokio::time::timeout(Duration::from_secs(5), r.flush_held()).await;
        assert!(flushed.is_ok(), "waited for a worker that is gone");
        assert!(r.held.is_empty());
        assert!(r.paused_at.is_none());
        assert_eq!(rx1.try_recv().unwrap().seq, Some(3));
    }

    /// A close frame without a status code ends the connection like any
    /// resumable close: the snapshot drops the last close code and the end
    /// of the connection is recorded.
    #[tokio::test]
    async fn a_close_without_a_code_ends_the_connection() {
        let mut r = runner(Vec::new());
        r.handle.update(|s| s.close_code = Some(4000));
        r.connection_open = true;

        r.on_close(None);
        assert_eq!(r.handle.snapshot().close_code, None);
        assert!(!r.connection_open);
    }

    /// A READY whose session fields cannot be read still starts a new session
    /// at its sequence, so payloads of the previous one no longer hold the
    /// resume point back; the session id and resume URL stay as they were.
    #[tokio::test]
    async fn a_ready_without_its_session_fields_still_starts_a_session() {
        let mut r = runner(Vec::new());
        r.handle.update(|s| {
            s.session_id = Some("OLD".to_owned());
            s.resume_url = Some("wss://old".to_owned());
        });
        let previous = r.handle.dispatch(9).expect("a token");

        r.on_ready(Some(3), None, None).await;
        assert!(!r.handle.is_current(previous));
        assert!(r.session.is_none());
        assert_eq!(
            r.handle.session_info(),
            Some(SessionInfo {
                session_id: "OLD".to_owned(),
                sequence: 3,
                resume_url: Some("wss://old".to_owned()),
            })
        );
    }

    /// A runner like `runner`'s that requests members and reads the guilds
    /// they start from in the cache of `redis`, each read within 10 ms.
    fn seeding(redis: NoRedis) -> Runner<NoRedis, InMemoryQueue> {
        let mut r = runner(Vec::new());
        r.ctx = Arc::new(RunnerContext {
            state: StateConfig {
                enabled: true,
                ..StateConfig::default()
            },
            workers: Vec::new(),
            redis,
            discord: DiscordLog::disabled(),
            bot_id: Id::new(1),
            cache_timeout: Duration::from_millis(10),
            max_held_bytes: 0,
            members: MemberConfig::default(),
        });
        r.members = Some(MemberRequester::new(0, Duration::ZERO, 0));
        r
    }

    /// A read of the cached guilds that fails is tried again a second later,
    /// three times in all; then the shard starts with nothing to request.
    #[tokio::test]
    async fn failed_reads_of_the_cached_guilds_are_tried_again() {
        let mut r = seeding(NoRedis { down: true });
        let (_stop, shutdown) = watch::channel(false);
        r.shutdown = shutdown;
        let started = Instant::now();
        r.seed_members().await;
        let took = started.elapsed();
        assert!(took >= SEED_RETRY_DELAY * (SEED_ATTEMPTS - 1), "{took:?}");
        let requests = r.members.as_mut().expect("member requests");
        assert_eq!(requests.poll(0, true), None, "nothing seeded");
    }

    /// A read that times out is tried again too, and a shutdown stops the
    /// reads at once, also while they wait to try again.
    #[tokio::test]
    async fn a_shutdown_stops_the_reads_of_the_cached_guilds() {
        let mut r = seeding(NoRedis::default());
        let (stop, shutdown) = watch::channel(false);
        r.shutdown = shutdown;
        let stopping = async {
            // The first read timed out by then, after 10 ms.
            tokio::time::sleep(Duration::from_millis(200)).await;
            stop.send_replace(true);
        };
        let started = Instant::now();
        tokio::join!(r.seed_members(), stopping);
        let took = started.elapsed();
        assert!(took < SEED_RETRY_DELAY, "{took:?}");
        let requests = r.members.as_mut().expect("member requests");
        assert_eq!(requests.poll(0, true), None, "nothing seeded");
    }
}

#[cfg(test)]
mod prepare_tests {
    use super::*;

    /// What is logged of a message twilight rejected is cut by characters,
    /// never inside one.
    #[test]
    fn prefix_cuts_whole_characters() {
        assert_eq!(prefix("abc", 5), "abc");
        assert_eq!(prefix("abc", 3), "abc");
        assert_eq!(prefix("abc", 2), "ab");
        assert_eq!(prefix("abc", 0), "");
        assert_eq!(prefix("", 3), "");
        assert_eq!(prefix("h\u{e9}llo", 2), "h\u{e9}");
        assert_eq!(prefix("\u{1f600}\u{1f600}", 1), "\u{1f600}");
        let long = "\u{e9}".repeat(MAX_LOGGED_PAYLOAD * 2);
        let cut = prefix(&long, MAX_LOGGED_PAYLOAD);
        assert_eq!(cut.chars().count(), MAX_LOGGED_PAYLOAD);
        assert_eq!(cut.len(), MAX_LOGGED_PAYLOAD * 2);
    }

    /// Op codes go through twilight's `OpCode`: what the shard handles itself
    /// (heartbeat requests, ACKs), what only clients send, unknown op codes
    /// and a dispatch without `t` are `Other`; an `op` that is no `u8` makes
    /// the message malformed.
    #[test]
    fn op_codes_map_through_twilight() {
        for json in [
            r#"{"op":1,"d":null}"#,
            r#"{"op":11}"#,
            r#"{"op":2,"d":{}}"#,
            r#"{"op":6,"d":{}}"#,
            r#"{"op":200,"d":null}"#,
            r#"{"op":0,"s":3,"t":null,"d":{}}"#,
        ] {
            assert!(
                matches!(prepare(json, &StateConfig::default()), Prepared::Other),
                "{json}"
            );
        }
        assert!(matches!(
            prepare(r#"{"op":256}"#, &StateConfig::default()),
            Prepared::Malformed(_)
        ));
    }

    /// The session `prepare` reads from a READY with this `d`.
    fn ready_session(d: &str) -> Option<ReadySession> {
        let json = format!(r#"{{"t":"READY","s":1,"op":0,"d":{d}}}"#);
        match prepare(&json, &StateConfig::default()) {
            Prepared::Dispatch { ready, .. } => ready,
            other => panic!("{json}: {other:?}"),
        }
    }

    /// READY's session is read from its `d` alone: a `d` without
    /// `resume_gateway_url`, a `d` of null and session fields nested deeper
    /// give none, and escapes in it are decoded.
    #[test]
    fn readys_session_is_read_from_its_d() {
        assert_eq!(ready_session(r#"{"session_id":"abc"}"#), None);
        assert_eq!(ready_session("null"), None);
        let nested = r#"{"user":{"session_id":"x","resume_gateway_url":"wss://x"}}"#;
        assert_eq!(ready_session(nested), None);
        assert_eq!(
            ready_session(r#"{"session_id":"a\u0062c","resume_gateway_url":"wss://r"}"#),
            Some(ReadySession {
                session_id: "abc".to_owned(),
                resume_gateway_url: "wss://r".to_owned(),
            })
        );
    }
}
