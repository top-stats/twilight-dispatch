//! Shared view of every shard of this process.
//!
//! A `Shard` is driven by exactly one task and cannot be shared, so the runner
//! publishes a snapshot of its state here after every message. The metrics,
//! status and session jobs and the command consumer read it from here.

use super::seq::{SeqToken, SeqTracker};
use crate::models::{FormattedDateTime, SessionInfo, StatusInfo};

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};
use twilight_gateway::{MessageSender, ShardState};

/// Status labels, kept compatible with earlier versions' stage names.
pub const STATUS_LABELS: [&str; 6] = [
    "Connected",
    "Disconnected",
    "Handshaking",
    "Identifying",
    "Resuming",
    "FatallyClosed",
];

/// The status label of a shard state, as written to `gateway_statuses` and the
/// metrics. Always one of [`STATUS_LABELS`]; no state maps to `Handshaking`,
/// which only earlier versions reported.
pub fn status_label(state: ShardState) -> &'static str {
    match state {
        ShardState::Active => "Connected",
        ShardState::Disconnected { .. } => "Disconnected",
        ShardState::FatallyClosed => "FatallyClosed",
        ShardState::Identifying => "Identifying",
        ShardState::Resuming => "Resuming",
    }
}

/// The state of one shard, as last published by its runner.
#[derive(Clone, Debug)]
pub struct ShardSnapshot {
    /// Status label, one of [`STATUS_LABELS`].
    pub status: &'static str,
    /// Whether the shard is connected and identified (twilight's
    /// `ShardState::Active`).
    pub active: bool,
    /// Whether Discord closed the shard with a code that does not allow
    /// reconnecting. The next READY clears it.
    pub fatal: bool,
    /// Code of the last close frame the shard reported; `None` when the
    /// connection ended without one.
    pub close_code: Option<u16>,
    /// The latest heartbeat round trip, in milliseconds; 0 until the first ACK.
    pub latency_ms: u64,
    /// When the last heartbeat ACK arrived.
    pub last_ack: Option<Instant>,
    /// When the shard last received a message (the runner publishes the
    /// snapshot after every one).
    pub last_message: Option<Instant>,
    /// The id of the shard's current session, if it has one.
    pub session_id: Option<String>,
    /// READY's `resume_gateway_url`, kept for as long as the session lives.
    pub resume_url: Option<String>,
    /// Set while the runner waits for a connection it closed to finish.
    pub close_wait_since: Option<Instant>,
}

impl Default for ShardSnapshot {
    fn default() -> Self {
        Self {
            status: "Disconnected",
            active: false,
            fatal: false,
            close_code: None,
            latency_ms: 0,
            last_ack: None,
            last_message: None,
            session_id: None,
            resume_url: None,
            close_wait_since: None,
        }
    }
}

impl ShardSnapshot {
    /// How long the shard has received nothing; `None` before its first
    /// message.
    pub fn silence(&self) -> Option<Duration> {
        self.silence_at(Instant::now())
    }

    /// How long the shard had received nothing at `now`.
    pub fn silence_at(&self, now: Instant) -> Option<Duration> {
        self.last_message
            .map(|at| now.saturating_duration_since(at))
    }
}

/// One shard's entry in the [`Registry`]: the snapshot its runner publishes,
/// its command sender and the tracker of its resume point.
pub struct ShardHandle {
    /// The shard id.
    pub id: u32,
    /// Sends gateway commands over the shard's connection. `None` only in
    /// tests, where no real shard exists.
    pub sender: Option<MessageSender>,
    snapshot: Mutex<ShardSnapshot>,
    seq: Mutex<SeqTracker>,
}

/// Lock `mutex`, recovering the data if a panic poisoned it. The critical
/// sections here only assign fields (of the snapshot or of the tracker), so
/// the data is still usable, while treating it as lost would, for example,
/// drop the shard's saved session.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl ShardHandle {
    /// A handle for shard `id` with a default (disconnected) snapshot. With
    /// `resumed_seq`, the sequence of a saved session the shard resumes, the
    /// resume point starts there instead of at 0.
    pub fn new(id: u32, sender: Option<MessageSender>, resumed_seq: Option<u64>) -> Self {
        let mut seq = SeqTracker::new();
        if let Some(resumed) = resumed_seq {
            seq.new_session(resumed);
        }
        Self {
            id,
            sender,
            snapshot: Mutex::new(ShardSnapshot::default()),
            seq: Mutex::new(seq),
        }
    }

    /// A copy of the shard's latest snapshot.
    pub fn snapshot(&self) -> ShardSnapshot {
        lock(&self.snapshot).clone()
    }

    /// Change the snapshot in place, holding its lock while `f` runs.
    pub fn update(&self, f: impl FnOnce(&mut ShardSnapshot)) {
        let mut snapshot = lock(&self.snapshot);
        f(&mut snapshot);
    }

    /// Start tracking a new session at READY's sequence `ready_seq`; payloads
    /// of the previous session no longer hold the resume point back.
    pub fn new_session(&self, ready_seq: u64) {
        lock(&self.seq).new_session(ready_seq);
    }

    /// Record that the payload with sequence `seq` was handed to a worker.
    /// Hand the token back to [`Self::complete`] or [`Self::fail`]. Always
    /// returns `Some`.
    pub fn dispatch(&self, seq: u64) -> Option<SeqToken> {
        Some(lock(&self.seq).dispatch(seq))
    }

    /// Record that a dispatched payload was published, so it no longer holds
    /// the resume point back.
    pub fn complete(&self, token: SeqToken) {
        lock(&self.seq).complete(token);
    }

    /// Whether a job was dispatched in the shard's current session.
    pub fn is_current(&self, token: SeqToken) -> bool {
        lock(&self.seq).is_current(token)
    }

    /// The payload could not be published; keep the resume point before it.
    pub fn fail(&self, token: SeqToken) {
        lock(&self.seq).fail(token, Instant::now());
    }

    /// How many payloads of the current session were dispatched but neither
    /// completed nor failed yet.
    pub fn in_flight(&self) -> usize {
        lock(&self.seq).in_flight()
    }

    /// The session to persist for a resume after a restart, if any.
    pub fn session_info(&self) -> Option<SessionInfo> {
        let snapshot = self.snapshot();
        let sequence = lock(&self.seq).resume_seq();
        let info = SessionInfo {
            session_id: snapshot.session_id?,
            sequence,
            resume_url: snapshot.resume_url,
        };
        info.is_resumable().then_some(info)
    }

    /// The shard's `gateway_statuses` entry, written at `now`.
    pub fn status_info(&self, now: FormattedDateTime) -> StatusInfo {
        self.status_info_at(now, Instant::now())
    }

    /// [`Self::status_info`] with `instant` as the monotonic clock at `now`,
    /// so a test can put `now` ahead of the last ACK instead of putting the
    /// ACK in the past (which `Instant::checked_sub` refuses on a host that
    /// booted more recently than that).
    pub fn status_info_at(&self, now: FormattedDateTime, instant: Instant) -> StatusInfo {
        let snapshot = self.snapshot();
        let last_ack = snapshot.last_ack.map_or(now, |at| {
            let ago = instant.saturating_duration_since(at).as_millis();
            // A span between two `Instant`s never comes near `i64::MAX` ms.
            now - time::Duration::milliseconds(i64::try_from(ago).unwrap_or(i64::MAX))
        });
        StatusInfo {
            shard: self.id,
            status: snapshot.status.to_owned(),
            latency: snapshot.latency_ms,
            last_ack,
        }
    }
}

/// Every shard of this process, by shard id.
#[derive(Default)]
pub struct Registry {
    shards: BTreeMap<u32, Arc<ShardHandle>>,
}

impl Registry {
    /// A registry of `handles`; a later handle with the same id replaces an
    /// earlier one.
    pub fn new(handles: impl IntoIterator<Item = Arc<ShardHandle>>) -> Self {
        Self {
            shards: handles.into_iter().map(|h| (h.id, h)).collect(),
        }
    }

    /// The handle of `shard`, if this process runs it.
    pub fn get(&self, shard: u32) -> Option<&Arc<ShardHandle>> {
        self.shards.get(&shard)
    }

    /// Every handle, in shard id order.
    pub fn handles(&self) -> impl Iterator<Item = &Arc<ShardHandle>> {
        self.shards.values()
    }

    /// A snapshot of every shard, in shard id order.
    pub fn snapshots(&self) -> Vec<(u32, ShardSnapshot)> {
        self.shards
            .iter()
            .map(|(id, h)| (*id, h.snapshot()))
            .collect()
    }

    /// How many shards this process runs.
    pub fn len(&self) -> usize {
        self.shards.len()
    }

    /// Whether this process runs no shards.
    pub fn is_empty(&self) -> bool {
        self.shards.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_match_previous_stage_names() {
        assert_eq!(status_label(ShardState::Active), "Connected");
        assert_eq!(
            status_label(ShardState::Disconnected {
                reconnect_attempts: 3
            }),
            "Disconnected"
        );
        assert_eq!(status_label(ShardState::Identifying), "Identifying");
        assert_eq!(status_label(ShardState::Resuming), "Resuming");
        assert_eq!(status_label(ShardState::FatallyClosed), "FatallyClosed");
        for state in [
            ShardState::Active,
            ShardState::Disconnected {
                reconnect_attempts: 0,
            },
            ShardState::FatallyClosed,
            ShardState::Identifying,
            ShardState::Resuming,
        ] {
            assert!(STATUS_LABELS.contains(&status_label(state)));
        }
    }

    #[test]
    fn times_are_measured_from_the_given_instant() {
        let start = Instant::now();
        let later = start + Duration::from_millis(2_500);
        let handle = ShardHandle::new(4, None, None);
        let now = FormattedDateTime::now();
        assert_eq!(
            handle.status_info_at(now, later).last_ack,
            now,
            "no ACK yet"
        );

        handle.update(|s| {
            s.last_ack = Some(start);
            s.last_message = Some(start);
        });
        let info = handle.status_info_at(now, later);
        assert_eq!((now - info.last_ack).whole_milliseconds(), 2_500);
        assert_eq!(
            handle.snapshot().silence_at(later),
            Some(Duration::from_millis(2_500))
        );
        // An instant before the last message is no silence, not a panic.
        handle.update(|s| s.last_message = Some(later));
        assert_eq!(handle.snapshot().silence_at(start), Some(Duration::ZERO));
        assert_eq!(ShardSnapshot::default().silence_at(later), None);
    }
}

#[cfg(test)]
mod poison_tests {
    use super::*;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    /// Run `f`, which panics while it holds a lock, so the lock is poisoned.
    fn poison(f: impl FnOnce()) {
        assert!(catch_unwind(AssertUnwindSafe(f)).is_err(), "no panic");
    }

    /// Shard 0 with a session, resumed at sequence 5.
    fn with_session() -> ShardHandle {
        let shard = ShardHandle::new(0, None, Some(5));
        shard.update(|s| s.session_id = Some("abc".to_owned()));
        shard
    }

    #[test]
    fn a_poisoned_snapshot_keeps_its_data() {
        let shard = with_session();
        poison(|| {
            shard.update(|s| {
                s.status = "Connected";
                panic!("poison the snapshot");
            });
        });
        assert!(shard.snapshot.is_poisoned());

        // The change made before the panic is kept, and so is the session, so
        // the session job still saves it.
        assert_eq!(shard.snapshot().status, "Connected");
        let session = shard.session_info().expect("the session is kept");
        assert_eq!(session.session_id, "abc");
        assert_eq!(session.sequence, 5);
        let status = shard.status_info(FormattedDateTime::now());
        assert_eq!(status.status, "Connected");

        // Later changes still apply.
        shard.update(|s| s.latency_ms = 9);
        assert_eq!(shard.snapshot().latency_ms, 9);
    }

    #[test]
    fn a_poisoned_resume_point_keeps_tracking() {
        let shard = with_session();
        let first = shard.dispatch(6).expect("tracked");
        poison(|| {
            let _tracker = shard.seq.lock().unwrap();
            panic!("poison the resume point");
        });
        assert!(shard.seq.is_poisoned());

        assert_eq!(shard.in_flight(), 1);
        assert!(shard.is_current(first));
        let sequence = || shard.session_info().map(|session| session.sequence);
        assert_eq!(sequence(), Some(5), "6 is still in flight");

        let second = shard.dispatch(7).expect("still tracked");
        assert_eq!(shard.in_flight(), 2);
        shard.complete(first);
        shard.fail(second);
        assert_eq!(shard.in_flight(), 0);
        assert_eq!(sequence(), Some(6), "a restart replays 7");

        shard.new_session(10);
        assert!(!shard.is_current(second));
        assert_eq!(sequence(), Some(10));
    }
}
