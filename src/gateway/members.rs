//! Guild member requests (gateway opcode 8, Request Guild Members) that the
//! shards make themselves with `REQUEST_MEMBERS`.
//!
//! [`MemberRequester`] holds one shard's requests and does no I/O. The shard
//! runner hands it every dispatch ([`MemberRequester::observe`]), sends the
//! request [`MemberRequester::poll`] returns with the shard's command API, and
//! polls again by [`MemberRequester::next_wake`] at the latest. Times are
//! milliseconds since the Unix epoch ([`now_ms`]).
//!
//! - One request at a time: the next guild is requested only once the
//!   request in flight completed (its chunk with the last `chunk_index`
//!   arrived, or `chunk_count` of its chunks did), was rate limited, or timed
//!   out ([`CHUNK_TIMEOUT`] without a chunk). A request that timed out is
//!   retried once at the back of the queue, then the guild is given up until
//!   its next refresh.
//! - Requested are a guild whose `GUILD_CREATE` lists fewer members than its
//!   `member_count` (a join, or a large guild after a new session), and every
//!   guild of the shard once per refresh period, each at its own offset in
//!   the period (from its id), so the requests are spread evenly over it. The
//!   shard's guilds are READY's and those of every `GUILD_CREATE`, less those
//!   a `GUILD_DELETE` without `unavailable` removes. READY itself requests
//!   nothing: the `GUILD_CREATE`s after it do.
//! - A guild that cannot answer is not requested: one READY listed until its
//!   `GUILD_CREATE` came, and one in an outage (a `GUILD_DELETE` with
//!   `unavailable`) until it is back. Its refresh is skipped and a request
//!   waiting for it is dropped; the `GUILD_CREATE` that brings it back
//!   requests it when it leaves members out.
//! - Discord allows one request for a guild's whole member list per
//!   [`GUILD_REQUEST_INTERVAL`] and answers another with `RATE_LIMITED`
//!   instead of chunks. A guild is not requested within that interval of its
//!   last completed request, and one rate limited anyway is requested again
//!   after the `retry_after` Discord names and [`RATE_LIMIT_MARGIN`], which
//!   does not count as an attempt.
//! - Chunks and `RATE_LIMITED` belong to the request in flight when they
//!   carry its nonce (or, without a nonce, name its guild). Those answering
//!   other clients' requests change nothing here; the runner caches and
//!   publishes every one as before.
//! - A guild waits in one place at most (queued, held back or in flight).
//!   What is kept is bounded by the shard's guilds, and the completions by
//!   the last [`GUILD_REQUEST_INTERVAL`].

use super::payload::mix;
use crate::{metrics::MEMBER_REQUESTS, util::Throttle};

use serde::{
    de::{DeserializeOwned, IgnoredAny},
    Deserialize,
};
use std::{
    collections::{hash_map::Entry, BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    num::NonZeroU64,
    sync::LazyLock,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tracing::{info, warn};
use twilight_model::{
    gateway::{
        event::Event,
        payload::{incoming::GuildCreate, outgoing::RequestGuildMembers},
    },
    id::{marker::GuildMarker, Id},
};

/// How long a request may go without a chunk before it times out.
pub const CHUNK_TIMEOUT: Duration = Duration::from_millis(CHUNK_TIMEOUT_MS);
const CHUNK_TIMEOUT_MS: u64 = 30_000;

/// Discord allows one request for a guild's whole member list per guild and
/// bot in this long, and answers another with `RATE_LIMITED`.
pub const GUILD_REQUEST_INTERVAL: Duration = Duration::from_millis(GUILD_REQUEST_INTERVAL_MS);
const GUILD_REQUEST_INTERVAL_MS: u64 = 30_000;

/// Added to the `retry_after` of a `RATE_LIMITED`, so that the request goes
/// out after Discord's window reopened rather than on its edge.
pub const RATE_LIMIT_MARGIN: Duration = Duration::from_millis(RATE_LIMIT_MARGIN_MS);
const RATE_LIMIT_MARGIN_MS: u64 = 1_000;

/// Requests for a guild that time out before it is given up until its next
/// refresh: one that timed out is retried once.
pub const MAX_ATTEMPTS: u8 = 2;

/// At most one completed request is logged per this long and shard.
const DONE_LOG_INTERVAL: Duration = Duration::from_secs(60);

/// Milliseconds since the Unix epoch on a clock that never goes back: the
/// wall clock as this process first read it, advanced by the monotonic
/// clock. Refresh times follow Unix time, so a guild keeps its time in the
/// period across restarts, while a step of the wall clock disturbs no
/// timeout.
pub fn now_ms() -> u64 {
    static START: LazyLock<(Instant, u64)> = LazyLock::new(|| {
        let wall = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, millis);
        (Instant::now(), wall)
    });
    let (start, wall) = *START;
    wall.saturating_add(millis(start.elapsed()))
}

/// `duration` in whole milliseconds, rounded down and saturating.
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// `duration` in whole milliseconds, rounded up and saturating.
fn ceil_millis(duration: Duration) -> u64 {
    let whole = duration.as_nanos().div_ceil(1_000_000);
    u64::try_from(whole).unwrap_or(u64::MAX)
}

/// The first time after `now` that lies at the guild's offset in the refresh
/// `period`: `k * period + offset` in Unix time, with the offset from the
/// guild id. `None` without a period.
fn next_refresh(period: Option<NonZeroU64>, guild: Id<GuildMarker>, now: u64) -> Option<u64> {
    let period = period?;
    let offset = mix(guild.get()) % period;
    if now < offset {
        return Some(offset);
    }
    let periods = ((now - offset) / period).saturating_add(1);
    Some(offset.saturating_add(periods.saturating_mul(period.get())))
}

/// Whether a guild's `GUILD_CREATE` leaves members out: it lists fewer
/// (`listed`) than its `member_count` (Discord leaves out offline members
/// above `LARGE_THRESHOLD`, and nearly all above 75k members).
fn leaves_members_out(listed: usize, member_count: Option<u64>) -> bool {
    let listed = u64::try_from(listed).unwrap_or(u64::MAX);
    member_count.is_some_and(|count| listed < count)
}

/// A request to send: the whole member list of `guild`, answered with
/// `nonce`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemberRequest {
    /// The guild whose members are requested.
    pub guild: Id<GuildMarker>,
    /// What Discord echoes in every chunk of the answer: `<shard>.<number>`,
    /// at most 32 bytes (Discord drops a longer one).
    pub nonce: String,
}

impl MemberRequest {
    /// The gateway command: every member (`query` "" and `limit` 0), without
    /// presences.
    pub fn command(&self) -> RequestGuildMembers {
        RequestGuildMembers::builder(self.guild)
            .nonce(self.nonce.as_str())
            .presences(false)
            .query("", None)
    }
}

/// What became of a shard's requests, as `gateway_member_requests` counts
/// them.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MemberStats {
    /// Requests sent.
    pub sent: u64,
    /// Requests whose last chunk arrived.
    pub done: u64,
    /// Requests Discord answered with `RATE_LIMITED`.
    pub rate_limited: u64,
    /// Requests that went [`CHUNK_TIMEOUT`] without a chunk.
    pub timeout: u64,
    /// Guilds given up after [`MAX_ATTEMPTS`] requests that timed out.
    pub dropped: u64,
}

/// One of the outcomes [`MemberStats`] counts.
#[derive(Clone, Copy)]
enum Outcome {
    Sent,
    Done,
    RateLimited,
    Timeout,
    Dropped,
}

/// A guild waiting for its turn, with how many of its requests timed out.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Waiting {
    guild: Id<GuildMarker>,
    timeouts: u8,
}

/// Where a guild that waits for its turn is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Pending {
    /// In the queue.
    Queued,
    /// Held back until this time, then queued.
    Deferred(u64),
}

/// The request in flight.
#[derive(Debug)]
struct InFlight {
    guild: Id<GuildMarker>,
    nonce: String,
    /// How many earlier requests for the guild timed out.
    timeouts: u8,
    /// When it was sent.
    sent_at: u64,
    /// When it was sent or its last chunk arrived.
    progress_at: u64,
    /// Its chunks that arrived.
    chunks: u32,
}

impl InFlight {
    /// Whether a chunk or `RATE_LIMITED` with this nonce and guild answers
    /// this request: by its nonce, or by its guild when it has no nonce.
    fn answers(&self, nonce: Option<&str>, guild: Option<Id<GuildMarker>>) -> bool {
        match nonce {
            Some(nonce) => nonce == self.nonce && guild.is_none_or(|guild| guild == self.guild),
            None => guild == Some(self.guild),
        }
    }
}

/// The `d` of a dispatch, read without the rest of it.
#[derive(Deserialize)]
struct Dispatch<T> {
    d: T,
}

/// The `d` of the dispatch `json` as a `T`, if it is one.
fn dispatch_data<T: DeserializeOwned>(json: &str) -> Option<T> {
    serde_json::from_str::<Dispatch<T>>(json)
        .ok()
        .map(|dispatch| dispatch.d)
}

/// The guilds of a READY the runner could not parse.
#[derive(Deserialize)]
struct ReadyMeta {
    guilds: Vec<GuildMeta>,
}

/// What the requests need of a guild in a READY, or of a `GUILD_CREATE`, the
/// runner could not parse (its members are only counted).
#[derive(Deserialize)]
struct GuildMeta {
    id: Id<GuildMarker>,
    unavailable: Option<bool>,
    member_count: Option<u64>,
    #[serde(default)]
    members: Vec<IgnoredAny>,
}

/// What paces the requests in a `GUILD_MEMBERS_CHUNK` (its members are
/// skipped).
#[derive(Debug, Deserialize)]
struct ChunkMeta {
    guild_id: Id<GuildMarker>,
    chunk_index: u32,
    chunk_count: u32,
    nonce: Option<String>,
}

/// The `d` of a `RATE_LIMITED`.
#[derive(Debug, Deserialize)]
struct RateLimited {
    /// The opcode of the command Discord did not run.
    opcode: u64,
    /// Seconds to wait before asking again.
    retry_after: Option<f64>,
    /// The request it answers.
    meta: Option<RateLimitMeta>,
}

/// The request a `RATE_LIMITED` for opcode 8 answers.
#[derive(Debug, Deserialize)]
struct RateLimitMeta {
    guild_id: Option<Id<GuildMarker>>,
    nonce: Option<String>,
}

impl RateLimited {
    /// How long to wait before asking again: `retry_after` in whole
    /// milliseconds, rounded up ([`GUILD_REQUEST_INTERVAL`] when it is
    /// missing, negative or not a number), plus [`RATE_LIMIT_MARGIN`].
    fn delay_ms(&self) -> u64 {
        let wait = match self.retry_after.map(Duration::try_from_secs_f64) {
            Some(Ok(after)) => ceil_millis(after),
            _ => GUILD_REQUEST_INTERVAL_MS,
        };
        wait.saturating_add(RATE_LIMIT_MARGIN_MS)
    }
}

/// One shard's member requests: its guilds and when each is refreshed next,
/// the guilds waiting for a request and in what order, the request in
/// flight, and when guilds' last requests completed.
pub struct MemberRequester {
    shard: u32,
    /// The refresh period in milliseconds; `None`: no periodic refresh.
    period: Option<NonZeroU64>,
    /// The shard's guilds, each with the time of its next refresh.
    guilds: HashMap<Id<GuildMarker>, Option<u64>>,
    /// The shard's guilds that cannot answer a request now: listed by READY
    /// and still without their `GUILD_CREATE`, or in an outage.
    unavailable: HashSet<Id<GuildMarker>>,
    /// Every next refresh, earliest first.
    refreshes: BTreeSet<(u64, Id<GuildMarker>)>,
    /// The guilds whose turn comes next, in order.
    queue: VecDeque<Waiting>,
    /// Guilds held back until a time, then queued, with how many of their
    /// requests timed out.
    deferred: BTreeMap<(u64, Id<GuildMarker>), u8>,
    /// Where every queued or held back guild is, so that none waits twice.
    pending: HashMap<Id<GuildMarker>, Pending>,
    in_flight: Option<InFlight>,
    /// When guilds' requests completed, oldest first, within the last
    /// `GUILD_REQUEST_INTERVAL`.
    completions: VecDeque<(u64, Id<GuildMarker>)>,
    /// Each guild's last completion in `completions`.
    completed: HashMap<Id<GuildMarker>, u64>,
    /// The number in the next nonce.
    next_nonce: u64,
    stats: MemberStats,
    /// Throttle the logs of each kind.
    done_logs: Throttle,
    timeout_logs: Throttle,
    drop_logs: Throttle,
    rate_limit_logs: Throttle,
    ignored_logs: Throttle,
}

impl MemberRequester {
    /// The requests of shard `shard`, which requests every guild again once
    /// per `refresh` (never when it is under a millisecond). The nonces count
    /// on from `now`, so that they differ from those of an earlier run.
    pub fn new(shard: u32, refresh: Duration, now: u64) -> Self {
        Self {
            shard,
            period: NonZeroU64::new(millis(refresh)),
            guilds: HashMap::new(),
            unavailable: HashSet::new(),
            refreshes: BTreeSet::new(),
            queue: VecDeque::new(),
            deferred: BTreeMap::new(),
            pending: HashMap::new(),
            in_flight: None,
            completions: VecDeque::new(),
            completed: HashMap::new(),
            next_nonce: now,
            stats: MemberStats::default(),
            done_logs: Throttle::new(DONE_LOG_INTERVAL),
            timeout_logs: Throttle::default(),
            drop_logs: Throttle::default(),
            rate_limit_logs: Throttle::default(),
            ignored_logs: Throttle::default(),
        }
    }

    /// Make `guilds` the shard's, each refreshed at its time, without
    /// requesting any now: the guilds cached before a restart that resumed
    /// the session, which brings no READY or `GUILD_CREATE` to learn them
    /// from.
    pub fn seed(&mut self, guilds: impl IntoIterator<Item = Id<GuildMarker>>, now: u64) {
        for guild in guilds {
            self.add(guild, now);
        }
    }

    /// What became of the requests so far.
    pub fn stats(&self) -> MemberStats {
        self.stats
    }

    /// Learn from a dispatch of the shard: its name (`t`), its typed event
    /// when the runner parsed one, and the raw payload, from which a READY,
    /// `GUILD_CREATE` or `GUILD_MEMBERS_CHUNK` the runner did not parse and a
    /// `RATE_LIMITED` are read.
    pub fn observe(&mut self, kind: &str, event: Option<&Event>, json: &str, now: u64) {
        match event {
            Some(Event::Ready(ready)) => {
                self.ready(ready.guilds.iter().map(|guild| guild.id), now);
            }
            Some(Event::GuildCreate(guild)) => match guild.as_ref() {
                GuildCreate::Available(guild) => {
                    let partial = leaves_members_out(guild.members.len(), guild.member_count);
                    self.guild_create(guild.id, partial, now);
                }
                GuildCreate::Unavailable(guild) => self.outage(guild.id, now),
            },
            Some(Event::GuildDelete(guild)) => {
                // An outage (`unavailable`) keeps the guild.
                if guild.unavailable == Some(true) {
                    self.outage(guild.id, now);
                } else {
                    self.remove(guild.id);
                }
            }
            Some(Event::MemberChunk(chunk)) => {
                let chunk = ChunkMeta {
                    guild_id: chunk.guild_id,
                    chunk_index: chunk.chunk_index,
                    chunk_count: chunk.chunk_count,
                    nonce: chunk.nonce.clone(),
                };
                self.chunk(&chunk, now);
            }
            _ => match kind {
                "READY" => {
                    if let Some(ready) = dispatch_data::<ReadyMeta>(json) {
                        self.ready(ready.guilds.into_iter().map(|guild| guild.id), now);
                    }
                }
                "GUILD_CREATE" => {
                    if let Some(guild) = dispatch_data::<GuildMeta>(json) {
                        self.guild_meta(&guild, now);
                    }
                }
                "GUILD_MEMBERS_CHUNK" => {
                    if let Some(chunk) = dispatch_data::<ChunkMeta>(json) {
                        self.chunk(&chunk, now);
                    }
                }
                "RATE_LIMITED" => {
                    if let Some(limit) = dispatch_data::<RateLimited>(json) {
                        self.rate_limited(&limit, now);
                    }
                }
                _ => {}
            },
        }
    }

    /// Bring the requests up to `now`: time out the request in flight, queue
    /// the guilds whose hold ended or whose refresh is due, and, when nothing
    /// is in flight and the shard can send (`ready`), start the request for
    /// the guild at the front of the queue and return it to be sent.
    pub fn poll(&mut self, now: u64, ready: bool) -> Option<MemberRequest> {
        self.time_out(now);
        self.prune(now);
        while let Some(entry) = self.deferred.first_entry() {
            if entry.key().0 > now {
                break;
            }
            let ((_, guild), timeouts) = entry.remove_entry();
            self.pending.remove(&guild);
            self.push(guild, timeouts);
        }
        while let Some(&(due, guild)) = self.refreshes.first() {
            if due > now {
                break;
            }
            self.refreshes.remove(&(due, guild));
            let next = next_refresh(self.period, guild, now);
            if let Some(next) = next {
                self.refreshes.insert((next, guild));
            }
            self.guilds.insert(guild, next);
            // One that cannot answer skips its turn.
            if !self.unavailable.contains(&guild) {
                self.enqueue(guild, 0, now);
            }
        }
        if !ready || self.in_flight.is_some() {
            return None;
        }
        // One that went unavailable while it waited is dropped.
        let waiting = loop {
            let waiting = self.queue.pop_front()?;
            self.pending.remove(&waiting.guild);
            if !self.unavailable.contains(&waiting.guild) {
                break waiting;
            }
        };
        let nonce = format!("{}.{}", self.shard, self.next_nonce);
        self.next_nonce = self.next_nonce.wrapping_add(1);
        self.in_flight = Some(InFlight {
            guild: waiting.guild,
            nonce: nonce.clone(),
            timeouts: waiting.timeouts,
            sent_at: now,
            progress_at: now,
            chunks: 0,
        });
        self.count(Outcome::Sent);
        Some(MemberRequest {
            guild: waiting.guild,
            nonce,
        })
    }

    /// When [`MemberRequester::poll`] next has something to do that no
    /// dispatch brings: the request in flight times out, a held back guild
    /// joins the queue, or a refresh is due. A queue waiting for the shard to
    /// be ready needs no wake: the dispatch that makes it ready polls.
    pub fn next_wake(&self) -> Option<u64> {
        let held = self.deferred.keys().next().map(|&(due, _)| due);
        let refresh = self.refreshes.first().map(|&(due, _)| due);
        let deadline = self.deadline();
        [deadline, held, refresh].into_iter().flatten().min()
    }

    /// READY lists the shard's guilds: those it does not list are gone, the
    /// others get their refresh times and cannot answer until their
    /// `GUILD_CREATE`. Nothing is requested.
    fn ready(&mut self, guilds: impl Iterator<Item = Id<GuildMarker>>, now: u64) {
        let listed: HashSet<Id<GuildMarker>> = guilds.collect();
        let gone: Vec<Id<GuildMarker>> = self
            .guilds
            .keys()
            .filter(|guild| !listed.contains(*guild))
            .copied()
            .collect();
        for guild in gone {
            self.remove(guild);
        }
        for guild in listed {
            self.outage(guild, now);
        }
    }

    /// An available guild's `GUILD_CREATE`: the guild is the shard's and can
    /// answer, and one that leaves members out (`partial`) is requested.
    fn guild_create(&mut self, guild: Id<GuildMarker>, partial: bool, now: u64) {
        self.add(guild, now);
        self.unavailable.remove(&guild);
        if partial {
            self.enqueue(guild, 0, now);
        }
    }

    /// `guild` is the shard's but cannot answer until its next
    /// `GUILD_CREATE`.
    fn outage(&mut self, guild: Id<GuildMarker>, now: u64) {
        self.add(guild, now);
        self.unavailable.insert(guild);
    }

    /// A `GUILD_CREATE` the runner could not parse, read raw.
    fn guild_meta(&mut self, guild: &GuildMeta, now: u64) {
        if guild.unavailable == Some(true) {
            self.outage(guild.id, now);
        } else {
            let partial = leaves_members_out(guild.members.len(), guild.member_count);
            self.guild_create(guild.id, partial, now);
        }
    }

    /// Make `guild` one of the shard's guilds, refreshed first after `now`,
    /// unless it is one already.
    fn add(&mut self, guild: Id<GuildMarker>, now: u64) {
        if let Entry::Vacant(entry) = self.guilds.entry(guild) {
            let refresh = next_refresh(self.period, guild, now);
            entry.insert(refresh);
            if let Some(due) = refresh {
                self.refreshes.insert((due, guild));
            }
        }
    }

    /// `guild` is no longer the shard's: forget it, its refresh, its wait and
    /// its request in flight.
    fn remove(&mut self, guild: Id<GuildMarker>) {
        if let Some(Some(due)) = self.guilds.remove(&guild) {
            self.refreshes.remove(&(due, guild));
        }
        self.unavailable.remove(&guild);
        match self.pending.remove(&guild) {
            Some(Pending::Queued) => self.queue.retain(|waiting| waiting.guild != guild),
            Some(Pending::Deferred(due)) => {
                self.deferred.remove(&(due, guild));
            }
            None => {}
        }
        if self.in_flight_for(guild) {
            self.in_flight = None;
        }
    }

    /// Whether the request in flight is for `guild`.
    fn in_flight_for(&self, guild: Id<GuildMarker>) -> bool {
        matches!(&self.in_flight, Some(flight) if flight.guild == guild)
    }

    /// Queue a request for `guild`, of which `timeouts` timed out so far,
    /// unless it is not the shard's, already waits or is in flight. A guild
    /// whose request completed less than [`GUILD_REQUEST_INTERVAL`] ago is
    /// held back until that long after it completed.
    fn enqueue(&mut self, guild: Id<GuildMarker>, timeouts: u8, now: u64) {
        if !self.guilds.contains_key(&guild)
            || self.pending.contains_key(&guild)
            || self.in_flight_for(guild)
        {
            return;
        }
        self.prune(now);
        let completed = self.completed.get(&guild).copied();
        match completed {
            Some(at) => {
                let due = at.saturating_add(GUILD_REQUEST_INTERVAL_MS);
                self.defer(guild, timeouts, due);
            }
            None => self.push(guild, timeouts),
        }
    }

    /// Queue `guild` at the back.
    fn push(&mut self, guild: Id<GuildMarker>, timeouts: u8) {
        self.pending.insert(guild, Pending::Queued);
        self.queue.push_back(Waiting { guild, timeouts });
    }

    /// Hold `guild` back until `due`, then queue it.
    fn defer(&mut self, guild: Id<GuildMarker>, timeouts: u8, due: u64) {
        self.pending.insert(guild, Pending::Deferred(due));
        self.deferred.insert((due, guild), timeouts);
    }

    /// Forget the completions older than [`GUILD_REQUEST_INTERVAL`].
    fn prune(&mut self, now: u64) {
        while let Some(&(at, guild)) = self.completions.front() {
            if at.saturating_add(GUILD_REQUEST_INTERVAL_MS) > now {
                break;
            }
            self.completions.pop_front();
            if self.completed.get(&guild) == Some(&at) {
                self.completed.remove(&guild);
            }
        }
    }

    /// When the request in flight times out.
    fn deadline(&self) -> Option<u64> {
        let flight = self.in_flight.as_ref()?;
        Some(flight.progress_at.saturating_add(CHUNK_TIMEOUT_MS))
    }

    /// Stop waiting for a request in flight that went [`CHUNK_TIMEOUT`]
    /// without a chunk: retry it at the back of the queue, or give the guild
    /// up after [`MAX_ATTEMPTS`].
    fn time_out(&mut self, now: u64) {
        if self.deadline().is_none_or(|deadline| now < deadline) {
            return;
        }
        if let Some(flight) = self.in_flight.take() {
            self.count(Outcome::Timeout);
            let timeouts = flight.timeouts.saturating_add(1);
            if timeouts >= MAX_ATTEMPTS {
                self.count(Outcome::Dropped);
                if let Some(suppressed) = self.drop_logs.check() {
                    warn!(
                        "[Shard {}] Giving up on the members of guild {} after {} requests without an answer until its next refresh ({} similar suppressed)",
                        self.shard, flight.guild, timeouts, suppressed
                    );
                }
            } else {
                if let Some(suppressed) = self.timeout_logs.check() {
                    warn!(
                        "[Shard {}] No members chunk of guild {} for {:?} after {} chunks; asking again later ({} similar suppressed)",
                        self.shard, flight.guild, CHUNK_TIMEOUT, flight.chunks, suppressed
                    );
                }
                self.push(flight.guild, timeouts);
            }
        }
    }

    /// A `GUILD_MEMBERS_CHUNK`: one answering the request in flight moves
    /// its timeout on, and its last one (by `chunk_index`, or the
    /// `chunk_count`th to arrive) completes it.
    fn chunk(&mut self, chunk: &ChunkMeta, now: u64) {
        let guild = Some(chunk.guild_id);
        let last = match self.in_flight.as_mut() {
            Some(flight) if flight.answers(chunk.nonce.as_deref(), guild) => {
                flight.chunks = flight.chunks.saturating_add(1);
                flight.progress_at = now;
                chunk.chunk_index.saturating_add(1) >= chunk.chunk_count
                    || flight.chunks >= chunk.chunk_count
            }
            _ => false,
        };
        if let Some(flight) = self.in_flight.take_if(|_| last) {
            self.complete(&flight, now);
        }
    }

    /// The last chunk of `flight` arrived at `now`.
    fn complete(&mut self, flight: &InFlight, now: u64) {
        self.count(Outcome::Done);
        self.prune(now);
        self.completed.insert(flight.guild, now);
        self.completions.push_back((now, flight.guild));
        if let Some(suppressed) = self.done_logs.check() {
            let took = now.saturating_sub(flight.sent_at);
            let waiting = self.pending.len();
            info!(
                "[Shard {}] Received the members of guild {} in {} chunks after {} ms; {} guilds waiting ({} similar suppressed)",
                self.shard, flight.guild, flight.chunks, took, waiting, suppressed
            );
        }
    }

    /// A `RATE_LIMITED`. One for the request in flight frees its slot, and
    /// its guild is requested again after Discord's `retry_after` and
    /// [`RATE_LIMIT_MARGIN`], without counting an attempt. Others (another
    /// command, another client's request) change nothing.
    fn rate_limited(&mut self, limit: &RateLimited, now: u64) {
        if limit.opcode != 8 {
            return;
        }
        let meta = limit.meta.as_ref();
        let nonce = meta.and_then(|meta| meta.nonce.as_deref());
        let guild = meta.and_then(|meta| meta.guild_id);
        match self.in_flight.take_if(|f| f.answers(nonce, guild)) {
            Some(flight) => {
                self.count(Outcome::RateLimited);
                let delay = limit.delay_ms();
                if let Some(suppressed) = self.rate_limit_logs.check() {
                    warn!(
                        "[Shard {}] Discord rate limited the members request of guild {}; asking again in {} ms ({} similar suppressed)",
                        self.shard, flight.guild, delay, suppressed
                    );
                }
                let due = now.saturating_add(delay);
                self.defer(flight.guild, flight.timeouts, due);
            }
            None => {
                if let Some(suppressed) = self.ignored_logs.check() {
                    let guild = guild.map_or(0, Id::get);
                    let nonce = nonce.unwrap_or("none");
                    info!(
                        "[Shard {}] Ignoring RATE_LIMITED for a members request of guild {} that is not in flight here (nonce {}) ({} similar suppressed)",
                        self.shard, guild, nonce, suppressed
                    );
                }
            }
        }
    }

    /// Count an outcome, here and in `gateway_member_requests`.
    fn count(&mut self, outcome: Outcome) {
        let (label, stat) = match outcome {
            Outcome::Sent => ("sent", &mut self.stats.sent),
            Outcome::Done => ("done", &mut self.stats.done),
            Outcome::RateLimited => ("rate_limited", &mut self.stats.rate_limited),
            Outcome::Timeout => ("timeout", &mut self.stats.timeout),
            Outcome::Dropped => ("dropped", &mut self.stats.dropped),
        };
        *stat = stat.saturating_add(1);
        MEMBER_REQUESTS.with_label_values(&[label]).inc();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::runner::parse_event;
    use serde_json::{json, Value};

    const SHARD: u32 = 3;
    /// When the tests start, in Unix milliseconds.
    const T0: u64 = 1_700_000_000_000;
    const SEC: u64 = 1_000;

    fn id(guild: u64) -> Id<GuildMarker> {
        Id::new(guild)
    }

    /// Requests without periodic refreshes.
    fn requester() -> MemberRequester {
        MemberRequester::new(SHARD, Duration::ZERO, T0)
    }

    /// The guild of the request `poll` starts at `now`, if any.
    fn next(r: &mut MemberRequester, now: u64) -> Option<u64> {
        r.poll(now, true).map(|request| request.guild.get())
    }

    /// The request `poll` starts at `now`, which there must be.
    fn send(r: &mut MemberRequester, now: u64) -> MemberRequest {
        r.poll(now, true).expect("a request")
    }

    fn chunk(guild: u64, index: u32, count: u32, nonce: Option<&str>) -> ChunkMeta {
        ChunkMeta {
            guild_id: id(guild),
            chunk_index: index,
            chunk_count: count,
            nonce: nonce.map(str::to_owned),
        }
    }

    /// Answer `request` with a single chunk at `now`.
    fn answer(r: &mut MemberRequester, request: &MemberRequest, now: u64) {
        let only = chunk(request.guild.get(), 0, 1, Some(&request.nonce));
        r.chunk(&only, now);
    }

    fn limited(retry_after: Option<f64>, guild: Option<u64>, nonce: Option<&str>) -> RateLimited {
        RateLimited {
            opcode: 8,
            retry_after,
            meta: Some(RateLimitMeta {
                guild_id: guild.map(id),
                nonce: nonce.map(str::to_owned),
            }),
        }
    }

    /// Sent, done, rate limited, timed out and dropped.
    fn counts(r: &MemberRequester) -> [u64; 5] {
        let s = r.stats();
        [s.sent, s.done, s.rate_limited, s.timeout, s.dropped]
    }

    /// Hand `r` the dispatch `kind` with this `d` at `now`, typed when
    /// `typed` as the runner parses it, raw otherwise.
    fn observe(r: &mut MemberRequester, kind: &str, d: &Value, typed: bool, now: u64) {
        let json = format!(r#"{{"t":"{kind}","s":1,"op":0,"d":{d}}}"#);
        let event = if typed {
            parse_event(&json).unwrap()
        } else {
            None
        };
        r.observe(kind, event.as_ref(), &json, now);
    }

    fn member(user: u64) -> Value {
        json!({
            "user": {
                "id": user.to_string(), "username": "u", "global_name": null,
                "discriminator": "0", "avatar": null, "bot": false, "public_flags": 0,
            },
            "nick": null, "avatar": null, "banner": null, "roles": [],
            "joined_at": "2024-01-01T00:00:00.000000+00:00", "premium_since": null,
            "deaf": false, "mute": false, "flags": 0, "pending": false,
            "communication_disabled_until": null,
        })
    }

    /// The `d` of a GUILD_CREATE (as tests/common builds one, in parts for
    /// the macro's recursion limit) that lists `listed` of its `count`
    /// members.
    fn guild(guild_id: u64, count: u64, listed: u64) -> Value {
        let mut d = json!({
            "id": guild_id.to_string(), "name": "g", "icon": null, "splash": null,
            "discovery_splash": null, "owner_id": "1", "afk_channel_id": null,
            "afk_timeout": 300, "verification_level": 1,
            "default_message_notifications": 0, "explicit_content_filter": 0,
            "features": [], "mfa_level": 0, "application_id": null,
        });
        let more = json!({
            "system_channel_id": null, "system_channel_flags": 0,
            "rules_channel_id": null, "vanity_url_code": null, "description": null,
            "banner": null, "premium_tier": 0, "premium_subscription_count": 0,
            "preferred_locale": "en-US", "public_updates_channel_id": null,
            "nsfw_level": 0, "premium_progress_bar_enabled": false,
            "safety_alerts_channel_id": null,
        });
        let lists = json!({
            "joined_at": "2024-01-01T00:00:00.000000+00:00", "large": false,
            "unavailable": false, "member_count": count, "roles": [], "emojis": [],
            "channels": [], "threads": [], "presences": [], "voice_states": [],
            "stickers": [], "stage_instances": [], "guild_scheduled_events": [],
            "soundboard_sounds": [],
        });
        for part in [more, lists] {
            if let (Some(d), Value::Object(part)) = (d.as_object_mut(), part) {
                d.extend(part);
            }
        }
        d["members"] = (1..=listed).map(member).collect();
        d
    }

    /// The `d` of a READY listing these guilds.
    fn ready(guilds: &[u64]) -> Value {
        let guilds: Vec<Value> = guilds
            .iter()
            .map(|guild| json!({"id": guild.to_string(), "unavailable": true}))
            .collect();
        json!({
            "v": 10,
            "user": {
                "id": "900", "username": "bot", "discriminator": "0", "global_name": null,
                "avatar": null, "bot": true, "mfa_enabled": false, "verified": true,
                "flags": 0,
            },
            "guilds": guilds,
            "session_id": "s", "resume_gateway_url": "wss://r", "shard": [SHARD, 16],
            "application": {"id": "900", "flags": 0},
        })
    }

    /// Run `r` from `T0` to the end of its `periods`th refresh period,
    /// answering every request at once. How often each guild was requested.
    fn run_periods(r: &mut MemberRequester, period: u64, periods: u64) -> HashMap<u64, u64> {
        let end = T0 + periods * period;
        let mut requested = HashMap::new();
        while let Some(wake) = r.next_wake().filter(|&wake| wake <= end) {
            while let Some(request) = r.poll(wake, true) {
                *requested.entry(request.guild.get()).or_default() += 1;
                answer(r, &request, wake);
            }
        }
        requested
    }

    /// Guilds are requested one at a time, in the order they came, each once
    /// the last chunk of the one before arrived.
    #[test]
    fn one_request_at_a_time_in_order() {
        let mut r = requester();
        for guild in [1, 2, 3] {
            r.guild_create(id(guild), true, T0);
        }
        let first = send(&mut r, T0);
        assert_eq!(first.guild, id(1));
        assert_eq!(next(&mut r, T0), None, "one in flight");
        // A chunk that is not the last moves nothing on.
        r.chunk(&chunk(1, 0, 2, Some(&first.nonce)), T0 + SEC);
        assert_eq!(next(&mut r, T0 + SEC), None);
        r.chunk(&chunk(1, 1, 2, Some(&first.nonce)), T0 + SEC);
        let second = send(&mut r, T0 + SEC);
        assert_eq!(second.guild, id(2));
        assert_ne!(second.nonce, first.nonce);
        answer(&mut r, &second, T0 + SEC);
        assert_eq!(next(&mut r, T0 + SEC), Some(3));
        assert_eq!(counts(&r), [3, 2, 0, 0, 0]);
        assert!(r.queue.is_empty());
        assert!(r.pending.is_empty());
    }

    /// The command asks for every member without presences, with a nonce
    /// that counts on from the start time and never exceeds 32 bytes.
    #[test]
    fn requests_ask_for_every_member_with_a_short_nonce() {
        let mut r = requester();
        r.guild_create(id(5), true, T0);
        let request = send(&mut r, T0);
        assert_eq!(request.nonce, format!("{SHARD}.{T0}"));
        let command = serde_json::to_value(request.command()).unwrap();
        let expected = json!({
            "op": 8,
            "d": {
                "guild_id": "5", "query": "", "limit": 0, "presences": false,
                "nonce": request.nonce,
            },
        });
        assert_eq!(command, expected);
        let longest = MemberRequest {
            guild: id(5),
            nonce: format!("{}.{}", u32::MAX, u64::MAX),
        };
        assert!(longest.nonce.len() <= 32, "{}", longest.nonce);
    }

    /// READY only tells the shard's guilds, and a GUILD_CREATE that lists
    /// every member needs no request: neither sends one.
    #[test]
    fn ready_and_complete_guilds_request_nothing() {
        let mut r = MemberRequester::new(SHARD, Duration::from_secs(3600), T0);
        r.ready([1, 2].into_iter().map(id), T0);
        r.guild_create(id(1), false, T0);
        r.guild_create(id(3), false, T0);
        assert_eq!(next(&mut r, T0), None);
        assert_eq!(r.guilds.len(), 3);
        // Each is refreshed within the hour.
        assert_eq!(r.refreshes.len(), 3);
        let wake = r.next_wake().unwrap();
        assert!((T0 + 1..=T0 + 3600 * SEC).contains(&wake), "{wake}");
    }

    /// Nothing is sent while the shard cannot send, and only a dispatch
    /// wakes a queue that waits for that; it goes out once the shard can.
    #[test]
    fn nothing_is_sent_until_the_shard_is_ready() {
        let mut r = requester();
        r.guild_create(id(1), true, T0);
        assert_eq!(r.poll(T0, false), None);
        assert_eq!(r.next_wake(), None);
        assert_eq!(next(&mut r, T0 + SEC), Some(1));
    }

    /// A request without a chunk for 30 s is retried once at the back of the
    /// queue (a chunk moves the timeout on), and the second timeout gives the
    /// guild up until something requests it again.
    #[test]
    fn a_request_that_times_out_is_retried_once_then_dropped() {
        let mut r = requester();
        r.guild_create(id(1), true, T0);
        r.guild_create(id(2), true, T0);
        let first = send(&mut r, T0);
        assert_eq!(first.guild, id(1));
        assert_eq!(r.next_wake(), Some(T0 + 30 * SEC));
        r.chunk(&chunk(1, 0, 3, Some(&first.nonce)), T0 + 20 * SEC);
        assert_eq!(next(&mut r, T0 + 30 * SEC), None, "a chunk came");
        assert_eq!(r.next_wake(), Some(T0 + 50 * SEC));

        // Timed out: guild 2 goes first, guild 1 queues behind it.
        let second = send(&mut r, T0 + 50 * SEC);
        assert_eq!(second.guild, id(2));
        answer(&mut r, &second, T0 + 51 * SEC);
        let retry = send(&mut r, T0 + 51 * SEC);
        assert_eq!(retry.guild, id(1));
        assert_ne!(retry.nonce, first.nonce);
        // A late chunk of the first request is not the retry's.
        r.chunk(&chunk(1, 2, 3, Some(&first.nonce)), T0 + 52 * SEC);
        assert_eq!(r.in_flight.as_ref().map(|f| f.chunks), Some(0));

        // The second timeout gives it up.
        assert_eq!(next(&mut r, T0 + 81 * SEC), None);
        assert_eq!(counts(&r), [3, 1, 0, 2, 1]);
        assert!(r.in_flight.is_none());
        assert!(r.pending.is_empty());
        assert_eq!(r.next_wake(), None);
        // Until a GUILD_CREATE requests it again.
        r.guild_create(id(1), true, T0 + 90 * SEC);
        assert_eq!(next(&mut r, T0 + 90 * SEC), Some(1));
    }

    /// RATE_LIMITED for the request in flight frees the slot at once and
    /// requests the guild again after retry_after and the margin, without
    /// counting an attempt.
    #[test]
    fn a_rate_limited_request_is_retried_after_retry_after() {
        let mut r = requester();
        r.guild_create(id(1), true, T0);
        r.guild_create(id(2), true, T0);
        let first = send(&mut r, T0);
        r.rate_limited(&limited(Some(2.0001), Some(1), Some(&first.nonce)), T0);
        let second = send(&mut r, T0);
        assert_eq!(second.guild, id(2), "the slot is free at once");
        answer(&mut r, &second, T0 + SEC);
        let due = T0 + 2_001 + 1_000;
        assert_eq!(r.next_wake(), Some(due));
        assert_eq!(next(&mut r, due - 1), None);
        let retry = send(&mut r, due);
        assert_eq!(retry.guild, id(1));
        assert_eq!(counts(&r), [3, 1, 1, 0, 0]);
        // No attempt was spent: a timeout still leaves it one retry.
        assert_eq!(next(&mut r, due + 30 * SEC), Some(1));
        assert_eq!(r.stats().dropped, 0);
    }

    /// retry_after in whole milliseconds, rounded up, plus the margin; 30 s
    /// when it is missing or unusable.
    #[test]
    fn retry_after_is_rounded_up_to_the_millisecond() {
        let delay = |retry_after| limited(retry_after, None, None).delay_ms();
        assert_eq!(delay(Some(0.5)), 1_500);
        assert_eq!(delay(Some(0.0005)), 1_001);
        assert_eq!(delay(Some(0.0)), 1_000);
        assert_eq!(delay(Some(29.8731)), 30_874);
        for unusable in [None, Some(-1.0), Some(f64::NAN), Some(f64::INFINITY)] {
            assert_eq!(delay(unusable), 31_000);
        }
    }

    /// Only a RATE_LIMITED for opcode 8 that answers the request in flight
    /// counts: by its nonce, or by its guild when it has none.
    #[test]
    fn rate_limits_of_other_requests_change_nothing() {
        let mut r = requester();
        r.guild_create(id(1), true, T0);
        let request = send(&mut r, T0);
        let nonce = Some(request.nonce.as_str());
        let mut other_opcode = limited(Some(1.0), Some(1), nonce);
        other_opcode.opcode = 3;
        r.rate_limited(&other_opcode, T0);
        r.rate_limited(&limited(Some(1.0), Some(1), Some("bot.1")), T0);
        r.rate_limited(&limited(Some(1.0), Some(2), nonce), T0);
        r.rate_limited(&limited(Some(1.0), Some(2), None), T0);
        let no_meta = RateLimited {
            opcode: 8,
            retry_after: Some(1.0),
            meta: None,
        };
        r.rate_limited(&no_meta, T0);
        assert!(r.in_flight_for(id(1)));
        assert_eq!(r.stats().rate_limited, 0);
        // Without a nonce, the guild tells.
        r.rate_limited(&limited(None, Some(1), None), T0);
        assert!(r.in_flight.is_none());
        assert_eq!(
            r.pending.get(&id(1)),
            Some(&Pending::Deferred(T0 + 31 * SEC))
        );
    }

    /// A chunk counts for the request in flight when it carries its nonce
    /// (and names its guild), or names its guild without a nonce; chunks of
    /// other requests (another client's) leave it alone.
    #[test]
    fn chunks_of_other_requests_are_ignored() {
        let mut r = requester();
        r.guild_create(id(1), true, T0);
        r.guild_create(id(2), true, T0);
        let request = send(&mut r, T0);
        let nonce = Some(request.nonce.as_str());
        r.chunk(&chunk(1, 0, 1, Some("bot.7")), T0);
        r.chunk(&chunk(2, 0, 1, nonce), T0);
        r.chunk(&chunk(2, 0, 1, None), T0);
        assert_eq!(next(&mut r, T0), None, "still in flight");
        assert_eq!(r.in_flight.as_ref().map(|f| f.chunks), Some(0));
        r.chunk(&chunk(1, 0, 1, None), T0);
        assert_eq!(next(&mut r, T0), Some(2));
    }

    /// The chunk with the last index completes a request, and so does the
    /// `chunk_count`th chunk to arrive, whatever their order.
    #[test]
    fn the_last_chunk_completes_a_request() {
        let mut r = requester();
        for guild in [1, 2, 3] {
            r.guild_create(id(guild), true, T0);
        }
        let first = send(&mut r, T0);
        r.chunk(&chunk(1, 2, 3, Some(&first.nonce)), T0);
        let second = send(&mut r, T0);
        assert_eq!(second.guild, id(2));
        r.chunk(&chunk(2, 0, 2, Some(&second.nonce)), T0);
        assert_eq!(next(&mut r, T0), None);
        r.chunk(&chunk(2, 0, 2, Some(&second.nonce)), T0);
        assert_eq!(next(&mut r, T0), Some(3));
        assert_eq!(r.stats().done, 2);
    }

    /// A guild is not requested again within 30 s of its last completed
    /// request: it is held back until then, once.
    #[test]
    fn a_guild_waits_30_s_after_its_last_request() {
        let mut r = requester();
        r.guild_create(id(1), true, T0);
        let request = send(&mut r, T0);
        answer(&mut r, &request, T0 + SEC);
        r.guild_create(id(1), true, T0 + 5 * SEC);
        r.guild_create(id(1), true, T0 + 6 * SEC);
        assert_eq!(r.deferred.len(), 1);
        assert_eq!(r.next_wake(), Some(T0 + 31 * SEC));
        assert_eq!(next(&mut r, T0 + 31 * SEC - 1), None);
        assert_eq!(next(&mut r, T0 + 31 * SEC), Some(1));
        // Pruned by then: only the last 30 s are kept.
        assert!(r.completed.is_empty());
        assert!(r.completions.is_empty());
    }

    /// A guild waits in one place at most: requesting it again while it is
    /// queued, held back or in flight changes nothing, and a guild that is
    /// not the shard's is never requested.
    #[test]
    fn a_guild_waits_once() {
        let mut r = requester();
        for _ in 0..3 {
            r.guild_create(id(1), true, T0);
            r.guild_create(id(2), true, T0);
        }
        assert_eq!(r.queue.len(), 2);
        let request = send(&mut r, T0);
        // In flight.
        r.guild_create(id(1), true, T0);
        assert_eq!(r.queue.len(), 1);
        r.rate_limited(&limited(Some(5.0), None, Some(&request.nonce)), T0);
        // Held back.
        r.guild_create(id(1), true, T0);
        assert_eq!(r.queue.len(), 1);
        assert_eq!(r.deferred.len(), 1);
        assert_eq!(r.pending.len(), 2);
        r.enqueue(id(9), 0, T0);
        assert_eq!(r.pending.len(), 2);
    }

    /// The shard's guilds are READY's and those of GUILD_CREATE; one that
    /// goes leaves the queue, its hold, its refresh and the request in
    /// flight too.
    #[test]
    fn the_guild_set_follows_ready_and_guild_create() {
        let mut r = MemberRequester::new(SHARD, Duration::from_secs(60), T0);
        r.ready([1, 2, 3, 4].into_iter().map(id), T0);
        for guild in [1, 2, 3] {
            r.guild_create(id(guild), true, T0);
        }
        let request = send(&mut r, T0);
        r.rate_limited(&limited(Some(5.0), None, Some(&request.nonce)), T0);
        let request = send(&mut r, T0);
        assert_eq!(request.guild, id(2));
        // 1 is held back, 2 in flight and 3 queued: a READY without them
        // forgets all three.
        r.ready([4, 5].into_iter().map(id), T0);
        assert!(r.in_flight.is_none());
        assert!(r.queue.is_empty());
        assert!(r.deferred.is_empty());
        assert!(r.pending.is_empty());
        let mut guilds: Vec<u64> = r.guilds.keys().map(|guild| guild.get()).collect();
        guilds.sort_unstable();
        assert_eq!(guilds, [4, 5]);
        assert_eq!(r.refreshes.len(), 2);
        // Neither had its GUILD_CREATE yet.
        assert_eq!(r.unavailable.len(), 2);
        // A join, then leaving it; then leaving one that never came up.
        r.guild_create(id(6), false, T0);
        assert_eq!(r.guilds.len(), 3);
        r.remove(id(6));
        assert_eq!((r.guilds.len(), r.refreshes.len()), (2, 2));
        r.remove(id(5));
        assert_eq!((r.guilds.len(), r.unavailable.len()), (1, 1));
    }

    /// What `observe` reads from the dispatches, typed as the runner hands
    /// them over or raw: READY's guilds, GUILD_CREATE, GUILD_DELETE, chunks
    /// either way and RATE_LIMITED.
    #[test]
    fn observe_reads_what_the_requests_need() {
        let mut r = requester();
        observe(&mut r, "READY", &ready(&[1, 2]), true, T0);
        assert_eq!(r.guilds.len(), 2);
        // Every member listed, an unavailable guild, then members left out.
        observe(&mut r, "GUILD_CREATE", &guild(1, 2, 2), true, T0);
        let unavailable = json!({"id": "3", "unavailable": true});
        observe(&mut r, "GUILD_CREATE", &unavailable, true, T0);
        assert_eq!(next(&mut r, T0), None);
        assert_eq!(r.guilds.len(), 3);
        observe(&mut r, "GUILD_CREATE", &guild(2, 300, 1), true, T0);
        let request = send(&mut r, T0);
        assert_eq!(request.guild, id(2));

        let first = json!({
            "guild_id": "2", "chunk_index": 0, "chunk_count": 2, "nonce": request.nonce,
            "members": [member(5)], "not_found": [],
        });
        observe(&mut r, "GUILD_MEMBERS_CHUNK", &first, true, T0 + SEC);
        assert_eq!(r.in_flight.as_ref().map(|f| f.chunks), Some(1));
        let last = json!({
            "guild_id": "2", "chunk_index": 1, "chunk_count": 2, "nonce": request.nonce,
            "members": [member(6)], "not_found": [],
        });
        observe(&mut r, "GUILD_MEMBERS_CHUNK", &last, false, T0 + SEC);
        assert_eq!(r.stats().done, 1);

        let now = T0 + 2 * SEC;
        let partial = guild(1, 300, 1);
        observe(&mut r, "GUILD_CREATE", &partial, true, now);
        let request = send(&mut r, now);
        let limit = json!({
            "opcode": 8, "retry_after": 1.5,
            "meta": {"guild_id": "1", "nonce": request.nonce},
        });
        observe(&mut r, "RATE_LIMITED", &limit, false, now);
        assert_eq!(r.stats().rate_limited, 1);
        assert_eq!(r.next_wake(), Some(now + 2_500));

        // An outage keeps the guild; leaving it removes it.
        let now = T0 + 3 * SEC;
        let outage = json!({"id": "1", "unavailable": true});
        observe(&mut r, "GUILD_DELETE", &outage, true, now);
        assert!(r.guilds.contains_key(&id(1)));
        assert!(r.pending.contains_key(&id(1)));
        assert!(r.unavailable.contains(&id(1)));
        let left = json!({"id": "1"});
        observe(&mut r, "GUILD_DELETE", &left, true, now);
        assert!(!r.guilds.contains_key(&id(1)));
        assert!(r.pending.is_empty());
        // Of READY's 1 and 2 and the unavailable 3, only 3 is still down.
        assert_eq!(r.unavailable.len(), 1);
        assert!(r.unavailable.contains(&id(3)));

        // Other dispatches, and payloads that are not what they say, change
        // nothing.
        let empty = json!({});
        for kind in ["TYPING_START", "READY", "GUILD_CREATE", "RATE_LIMITED"] {
            observe(&mut r, kind, &empty, false, now);
        }
        observe(&mut r, "GUILD_MEMBERS_CHUNK", &empty, false, now);
        assert_eq!(counts(&r), [2, 1, 1, 0, 0]);
        assert_eq!(r.guilds.len(), 2);
    }

    /// Completions are kept for 30 s only, so they never outnumber the
    /// requests of the last 30 s.
    #[test]
    fn old_completions_are_pruned() {
        let mut r = requester();
        for guild in 1..=100 {
            let now = T0 + guild * SEC;
            r.guild_create(id(guild), true, now);
            let request = send(&mut r, now);
            answer(&mut r, &request, now);
            assert!(r.completed.len() <= 30);
            assert!(r.completions.len() <= 30);
        }
        assert_eq!(r.completed.len(), 30);
    }

    /// Each guild has its own time in the refresh period, from its id, and
    /// the times of many guilds spread evenly over the period.
    #[test]
    fn refresh_times_spread_evenly_over_the_period() {
        let period = NonZeroU64::new(1_000_000);
        let mut buckets = [0u32; 10];
        for n in 1..=5_000u64 {
            // Snowflakes: a timestamp over 22 bits that are mostly zero.
            let guild = id((n * 7_919) << 22);
            let at = next_refresh(period, guild, 0).unwrap();
            assert!((1..=1_000_000).contains(&at), "{at}");
            assert_eq!(next_refresh(period, guild, at), Some(at + 1_000_000));
            buckets[usize::try_from(at % 1_000_000 / 100_000).unwrap()] += 1;
        }
        for count in buckets {
            assert!((400..=600).contains(&count), "{buckets:?}");
        }
        assert_eq!(next_refresh(None, id(1), 0), None);
    }

    /// With a refresh period every guild is requested once per period, each
    /// at its own time, while READY (and the GUILD_CREATEs that list every
    /// member) request none of them at once.
    #[test]
    fn every_guild_is_refreshed_once_per_period() {
        let period = 600 * SEC;
        let mut r = MemberRequester::new(SHARD, Duration::from_millis(period), T0);
        let guilds: Vec<Id<GuildMarker>> = (1..=300).map(|n| id(n << 22)).collect();
        r.ready(guilds.iter().copied(), T0);
        for &guild in &guilds {
            r.guild_create(guild, false, T0);
        }
        assert_eq!(next(&mut r, T0), None);
        assert!(r.unavailable.is_empty());
        for periods in [1, 2] {
            let requested = run_periods(&mut r, period, periods);
            assert_eq!(requested.len(), 300, "period {periods}");
            for count in requested.values() {
                assert_eq!(*count, 1, "period {periods}");
            }
        }
        assert_eq!(r.stats().sent, 600);
    }

    /// A guild that cannot answer (listed by READY until its GUILD_CREATE,
    /// or in an outage) is not requested: its refresh is skipped and a
    /// request waiting for it is dropped, until a GUILD_CREATE brings it
    /// back.
    #[test]
    fn unavailable_guilds_are_not_requested() {
        let period = 600 * SEC;
        let mut r = MemberRequester::new(SHARD, Duration::from_millis(period), T0);
        r.ready([1, 2, 3].into_iter().map(id), T0);
        assert_eq!(r.unavailable.len(), 3);
        r.guild_create(id(1), true, T0);
        r.guild_create(id(2), true, T0);
        let first = send(&mut r, T0);
        assert_eq!(first.guild, id(1));
        // 2 goes down while it waits; 3 never came up.
        r.outage(id(2), T0);
        answer(&mut r, &first, T0 + SEC);
        assert_eq!(next(&mut r, T0 + SEC), None);
        assert!(!r.pending.contains_key(&id(2)));
        // Over a period only 1 is refreshed.
        let requested = run_periods(&mut r, period, 1);
        assert_eq!(requested, HashMap::from([(1, 1)]));
        assert_eq!(r.unavailable.len(), 2);
        // 3 comes up leaving members out: requested.
        let end = T0 + period;
        r.guild_create(id(3), true, end);
        assert_eq!(next(&mut r, end), Some(3));
        assert_eq!(r.unavailable.len(), 1);
    }

    /// A READY or GUILD_CREATE the runner could not parse is read raw: the
    /// guilds READY lists, and a GUILD_CREATE's availability and how many
    /// members it lists of its member_count.
    #[test]
    fn unparsed_ready_and_guild_create_are_read_raw() {
        let mut r = requester();
        let listed = json!({"guilds": [{"id": "7", "unavailable": true}, {"id": "8"}]});
        observe(&mut r, "READY", &listed, false, T0);
        assert_eq!((r.guilds.len(), r.unavailable.len()), (2, 2));
        let partial = json!({"id": "7", "member_count": 3, "members": [{}, {}]});
        observe(&mut r, "GUILD_CREATE", &partial, false, T0);
        let down = json!({"id": "8", "unavailable": true});
        observe(&mut r, "GUILD_CREATE", &down, false, T0);
        let whole = json!({"id": "9", "member_count": 1, "members": [{}]});
        observe(&mut r, "GUILD_CREATE", &whole, false, T0);
        assert_eq!(next(&mut r, T0), Some(7));
        assert_eq!(next(&mut r, T0), None);
        assert_eq!(r.guilds.len(), 3);
        assert!(r.queue.is_empty());
        let unavailable: Vec<u64> = r.unavailable.iter().map(|guild| guild.get()).collect();
        assert_eq!(unavailable, [8]);
    }

    /// `now_ms` is Unix time that never goes back.
    #[test]
    fn now_ms_follows_the_wall_clock_and_never_goes_back() {
        let wall = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let first = now_ms();
        let second = now_ms();
        assert!(second >= first);
        let drift = u128::from(first).abs_diff(wall);
        assert!(drift < 5_000, "{first} vs {wall}");
    }
}

#[cfg(test)]
mod seed_tests {
    use super::*;

    /// Seeded guilds (the cache's, after a restart that resumed the session)
    /// get their refresh time, and none is requested at once.
    #[test]
    fn seeded_guilds_get_a_refresh_and_no_request() {
        let mut requests = MemberRequester::new(0, Duration::from_secs(3600), 0);
        requests.seed([Id::new(1 << 22), Id::new(2 << 22)], 0);
        assert_eq!(requests.guilds.len(), 2);
        assert!(requests.queue.is_empty(), "nothing is queued");
        assert!(requests.next_wake().is_some(), "their refresh is due later");
    }
}
